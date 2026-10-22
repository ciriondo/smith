//! The image: an embedded base, a generated Containerfile and one build per image config.
//!
//! The base image ships inside the `smith` binary as a gzip-compressed docker archive. The
//! first sandbox start loads it into the private store; the final image builds from the local
//! base tag. The tag is a hash of the base, the user package list and both binary digests, so
//! sessions with the same resolved settings reuse one image.

use std::io;
use std::path::Path;

use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::error::SandboxError;
use crate::supervisor::Supervisor;

/// The local tag carried by [`BASE_ARCHIVE`]. The archive is generated from Debian 13.7
/// (glibc 2.41) and pinned by digest; `assets/README.md` records the digest and the command.
pub const BASE_TAG: &str = "localhost/smith-base:debian-13.7";

/// The compressed docker archive of the base image. It is generated with `podman save` and
/// embedded, so the sandbox start does not pull a base image from a registry.
pub const BASE_ARCHIVE: &[u8] = include_bytes!("../assets/debian-slim.tar.gz");

/// The Containerfile and inventory format. Bump it when either changes, so an existing image
/// is not reused after the build logic changes.
const IMAGE_FORMAT: u32 = 1;

/// The child's system config tier. It is the only provider a sandboxed `smith` can build: the
/// worker's listener, with the per-container token as its key.
///
/// `default_model` is a placeholder because the config loader requires a new provider to set
/// one. The host proxy replaces the model with the parent's before the request leaves the
/// host, so the value never reaches a provider.
pub const CHILD_CONFIG: &str = "\
[providers.sandbox]
kind = \"openai_compat\"
base_url = \"http://127.0.0.1:3129/v1\"
api_key_env = \"SMITH_SANDBOX_TOKEN\"
default_model = \"proxied\"
models = []
";

/// The image tag: a hash of the base, the resolved package list and both binary digests.
///
/// The package list is sorted and deduplicated here as well as in the config, so two sessions
/// with the same packages in a different order reuse one image. The format constant is part of
/// the hash.
pub fn tag_for(base: &str, packages: &[String], worker_digest: &str, smith_digest: &str) -> String {
    let mut packages = packages.to_vec();
    packages.sort();
    packages.dedup();

    let mut hasher = Sha256::new();
    hasher.update(IMAGE_FORMAT.to_be_bytes());
    hash_field(&mut hasher, base.as_bytes());
    hash_field(&mut hasher, &(packages.len() as u64).to_be_bytes());
    for package in &packages {
        hash_field(&mut hasher, package.as_bytes());
    }
    hash_field(&mut hasher, worker_digest.as_bytes());
    hash_field(&mut hasher, smith_digest.as_bytes());
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("smith-sandbox:{}", &hex[..16])
}

fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

/// The sha256 of a binary, as lowercase hex.
pub fn digest(path: &Path) -> Result<String, SandboxError> {
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The Containerfile for one package list.
///
/// The default image is the base plus the two binaries. The optional install line is the only
/// place a package reaches the image. The final RUN generates the inventory from the actual
/// image and removes the package manager from the base.
pub fn containerfile(packages: &[String]) -> String {
    let install = if packages.is_empty() {
        String::new()
    } else {
        format!(
            "RUN apt-get update \\\n\
             \x20&& apt-get install -y --no-install-recommends {} \\\n\
             \x20&& rm -rf /var/lib/apt/lists /var/cache/apt\n",
            packages.join(" ")
        )
    };
    let package_loop = if packages.is_empty() {
        String::new()
    } else {
        format!(
            "for p in {}; do dpkg-query -L \"$p\" 2>/dev/null | while read -r f; do \
             case \"$f\" in /usr/bin/*|/bin/*) b=${{f##*/}}; [ -x \"$f\" ] && \
             printf '%s\\n' \"$b\";; esac; done; done; ",
            packages.join(" ")
        )
    };

    format!(
        "FROM {BASE_TAG}\n\
         {install}\
         RUN mkdir -p /etc/smith\n\
         COPY smith-worker /usr/local/bin/smith-worker\n\
         COPY smith /usr/local/bin/smith\n\
         COPY smith-config.toml /etc/smith/config.toml\n\
         RUN {{ for c in bash sh cat cp mv rm mkdir ls grep sed awk tar gzip find head tail wc sort uniq cut tr env sleep true false printf du perl git gcc ld; do command -v \"$c\" >/dev/null 2>&1 && printf '%s\\n' \"$c\"; done; \
         {package_loop}printf '%s\\n' smith; }} | sort -u > /etc/smith/inventory.txt \\\n\
         \x20&& rm -rf /var/lib/apt/lists /var/cache/apt /etc/apt /var/lib/dpkg \\\n\
         \x20&& rm -f /usr/bin/apt /usr/bin/apt-get /usr/bin/dpkg /usr/bin/dpkg-deb\n"
    )
}

/// Build the image when it does not exist. The base is loaded from the embedded archive when
/// the private store does not contain it; the final build never pulls an image.
pub async fn ensure(
    supervisor: &Supervisor,
    worker: &Path,
    smith: &Path,
    packages: &[String],
    tag: &str,
) -> Result<(), SandboxError> {
    if exists(supervisor, tag).await? {
        return Ok(());
    }

    ensure_base(supervisor).await?;

    eprintln!("smith: sandbox: building the image from {BASE_TAG}");
    let context = tempfile::tempdir()?;
    std::fs::write(
        context.path().join("Containerfile"),
        containerfile(packages),
    )?;
    copy_binary(worker, &context.path().join("smith-worker"))?;
    copy_binary(smith, &context.path().join("smith"))?;
    std::fs::write(context.path().join("smith-config.toml"), CHILD_CONFIG)?;

    let output = Command::new("podman")
        .args(supervisor.global_args())
        .args(["build", "--pull=never", "-t", tag])
        .arg(context.path())
        .output()
        .await
        .map_err(|source| SandboxError::Spawn {
            command: "podman build".to_string(),
            source,
        })?;

    if !output.status.success() {
        return Err(SandboxError::Podman(format!(
            "image build failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Load the embedded base archive when the private store does not have it.
async fn ensure_base(supervisor: &Supervisor) -> Result<(), SandboxError> {
    if exists(supervisor, BASE_TAG).await? {
        return Ok(());
    }

    eprintln!("smith: sandbox: loading the embedded base image {BASE_TAG}");
    let directory = tempfile::tempdir()?;
    let archive = directory.path().join("base.tar.gz");
    std::fs::write(&archive, BASE_ARCHIVE)?;

    let output = Command::new("podman")
        .args(supervisor.global_args())
        .args(["load", "-i"])
        .arg(&archive)
        .output()
        .await
        .map_err(|source| SandboxError::Spawn {
            command: "podman load".to_string(),
            source,
        })?;

    if !output.status.success() {
        return Err(SandboxError::Podman(format!(
            "base image load failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

async fn exists(supervisor: &Supervisor, tag: &str) -> Result<bool, SandboxError> {
    let status = Command::new("podman")
        .args(supervisor.global_args())
        .args(["image", "exists", tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|source| SandboxError::Spawn {
            command: "podman image exists".to_string(),
            source,
        })?;
    Ok(status.success())
}

/// Copy a binary into the build context and make it executable, so `podman build` sees a
/// plain file rather than a symlink or a path outside the context.
fn copy_binary(binary: &Path, destination: &Path) -> io::Result<()> {
    std::fs::copy(binary, destination)?;
    let mut permissions = std::fs::metadata(destination)?.permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o755);
    std::fs::set_permissions(destination, permissions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_containerfile_has_the_base_binaries_and_no_packages() {
        let text = containerfile(&[]);
        assert!(text.contains(&format!("FROM {BASE_TAG}")));
        assert!(!text.contains("apt-get install"));
        assert!(text.contains("inventory.txt"));
        assert!(text.contains("rm -f /usr/bin/apt"));
        assert!(text.contains("COPY smith-worker /usr/local/bin/smith-worker"));
        assert!(text.contains("COPY smith /usr/local/bin/smith"));
        assert!(text.contains("COPY smith-config.toml /etc/smith/config.toml"));
        assert!(!text.contains("chmod 755"));
    }

    #[test]
    fn the_containerfile_installs_the_configured_packages() {
        let text = containerfile(&["git".to_string(), "gcc".to_string()]);
        assert!(text.contains("apt-get install -y --no-install-recommends git gcc"));
        assert!(text.contains("for p in git gcc;"));
        assert!(text.contains("dpkg-query -L"));
    }

    #[test]
    fn the_base_archive_is_embedded_and_compressed() {
        assert!(BASE_ARCHIVE.len() > 1024);
        assert_eq!(&BASE_ARCHIVE[..2], &[0x1f, 0x8b]);
    }

    #[test]
    fn the_tag_is_stable_and_package_order_does_not_matter() {
        let a = tag_for(
            BASE_TAG,
            &["gcc".to_string(), "git".to_string()],
            "0123456789abcdef0123",
            "fedcba98765432100123",
        );
        let b = tag_for(
            BASE_TAG,
            &["git".to_string(), "gcc".to_string(), "git".to_string()],
            "0123456789abcdef0123",
            "fedcba98765432100123",
        );
        assert_eq!(a, b);
        assert!(a.starts_with("smith-sandbox:"));
        assert_eq!(a.len(), "smith-sandbox:".len() + 16);
    }

    #[test]
    fn the_tag_changes_with_packages_and_binaries() {
        let base = tag_for(
            BASE_TAG,
            &[],
            "0123456789abcdef0123",
            "fedcba98765432100123",
        );
        assert_ne!(
            base,
            tag_for(
                BASE_TAG,
                &["git".to_string()],
                "0123456789abcdef0123",
                "fedcba98765432100123"
            )
        );
        assert_ne!(
            base,
            tag_for(
                BASE_TAG,
                &[],
                "ffffffffffffffffffff",
                "fedcba98765432100123"
            )
        );
    }

    /// The child config has to load through the same tier the container uses. A new provider
    /// with an empty `default_model` or a missing `api_key_env` fails here, not at runtime.
    #[test]
    fn the_child_config_parses_as_the_system_tier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, CHILD_CONFIG).unwrap();

        let config =
            smith_core::config::load_tiers(&[(smith_core::config::Tier::System, path)]).unwrap();
        let provider = config.provider("sandbox").unwrap();
        assert_eq!(provider.base_url, "http://127.0.0.1:3129/v1");
        assert_eq!(provider.api_key_env, "SMITH_SANDBOX_TOKEN");
        assert_eq!(
            provider.kind,
            smith_core::config::ProviderKind::OpenAiCompat
        );
    }
}
