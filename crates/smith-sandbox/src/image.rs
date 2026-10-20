//! The image: an embedded Containerfile and one build per smith version.
//!
//! The base is pinned by digest, the build context holds only the Containerfile and the
//! worker binary, and the tag carries the smith version so an upgrade produces a new image.

use std::io;
use std::path::Path;

use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::error::SandboxError;
use crate::supervisor::Supervisor;

/// Debian 13.7, glibc 2.41. The digest was captured on 2026-10-19.
pub const BASE: &str =
    "debian:stable-slim@sha256:d405c1e1d9eecbf30022d23766a0eea235e7f499aa33e357dfd29271a839f82a";

/// The image tag: the smith version and a digest of the worker. Including the worker means a
/// rebuilt worker produces a new tag instead of silently reusing an older image.
pub fn tag_for(worker_digest: &str) -> String {
    let short = &worker_digest[..12.min(worker_digest.len())];
    format!("smith-sandbox:{}-{short}", env!("CARGO_PKG_VERSION"))
}

/// The sha256 of the worker binary, as lowercase hex.
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

/// The Containerfile. `gcc`, `binutils` and `libc6-dev` are present because `rustc` cannot
/// link without them; `git` and `ca-certificates` are absent from the base image.
pub fn containerfile() -> String {
    format!(
        "FROM {BASE}\n\
         RUN apt-get update \\\n\
         \x20&& apt-get install -y --no-install-recommends bash coreutils git gcc libc6-dev binutils ca-certificates \\\n\
         \x20&& rm -rf /var/lib/apt/lists /var/cache/apt /etc/apt /var/lib/dpkg \\\n\
         \x20&& rm -f /usr/bin/apt /usr/bin/apt-get /usr/bin/dpkg /usr/bin/dpkg-deb \\\n\
         \x20&& mkdir -p /etc/smith \\\n\
         \x20&& printf '%s\\n' bash sh cat cp mv rm mkdir ls grep sed awk git gcc ld tar gzip find head tail wc sort uniq cut tr env sleep true false printf > /etc/smith/inventory.txt\n\
         COPY smith-worker /usr/local/bin/smith-worker\n\
         RUN chmod 755 /usr/local/bin/smith-worker\n"
    )
}

/// Build the image when it does not exist. The first build pulls the base and the packages.
pub async fn ensure(supervisor: &Supervisor, worker: &Path, tag: &str) -> Result<(), SandboxError> {
    if exists(supervisor, tag).await? {
        return Ok(());
    }

    eprintln!("smith: sandbox: building the image, this pulls {BASE}");
    let context = tempfile::tempdir()?;
    std::fs::write(context.path().join("Containerfile"), containerfile())?;
    copy_worker(worker, &context.path().join("smith-worker"))?;

    let output = Command::new("podman")
        .args(supervisor.global_args())
        .args(["build", "--pull=missing", "-t", tag])
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

/// Copy the worker into the build context and make it executable, so `podman build` sees a
/// plain file rather than a symlink or a path outside the context.
fn copy_worker(worker: &Path, destination: &Path) -> io::Result<()> {
    std::fs::copy(worker, destination)?;
    let mut permissions = std::fs::metadata(destination)?.permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o755);
    std::fs::set_permissions(destination, permissions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_containerfile_pins_the_base_and_removes_the_package_manager() {
        let text = containerfile();
        assert!(text.contains(BASE));
        assert!(text.contains("libc6-dev"));
        assert!(text.contains("inventory.txt"));
        assert!(text.contains("rm -f /usr/bin/apt"));
        assert!(text.contains("COPY smith-worker /usr/local/bin/smith-worker"));
    }

    #[test]
    fn the_tag_carries_the_version_and_the_worker_digest() {
        assert_eq!(
            tag_for("0123456789abcdef0123"),
            format!("smith-sandbox:{}-0123456789ab", env!("CARGO_PKG_VERSION"))
        );
    }
}
