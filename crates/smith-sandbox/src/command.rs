//! The podman command line, built as data so it can be asserted in tests.

use std::path::{Path, PathBuf};

/// Everything a container needs beyond the store and the caches.
pub struct ContainerSpec<'a> {
    pub workspace: &'a Path,
    pub session: &'a str,
    pub cidfile: &'a Path,
    pub ro_mounts: &'a [PathBuf],
    pub image: &'a str,
    pub worker: &'a Path,
    pub version: &'a str,
}

/// The global arguments that choose a private store.
pub fn global_args(store: &Path, runroot: &Path) -> Vec<String> {
    vec![
        "--root".to_string(),
        store.display().to_string(),
        "--runroot".to_string(),
        runroot.display().to_string(),
    ]
}

/// The arguments after the global ones, up to but not including the image.
pub fn run_args(spec: &ContainerSpec<'_>, caches: &Path) -> Vec<String> {
    let workspace = spec.workspace.display().to_string();
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--network=none".to_string(),
        "--read-only".to_string(),
        "--read-only-tmpfs".to_string(),
        "--userns=keep-id".to_string(),
        "--cap-drop=all".to_string(),
        "--security-opt".to_string(),
        "no-new-privileges".to_string(),
        "--security-opt".to_string(),
        "label=disable".to_string(),
        "--preserve-fds=1".to_string(),
        "--cidfile".to_string(),
        spec.cidfile.display().to_string(),
        "--label".to_string(),
        "smith.managed=true".to_string(),
        "--label".to_string(),
        format!("smith.session={}", spec.session),
        "--label".to_string(),
        format!("smith.version={}", spec.version),
        "--ulimit".to_string(),
        "core=0".to_string(),
        "--pull=never".to_string(),
        "-v".to_string(),
        format!("{workspace}:{workspace}"),
        "-v".to_string(),
        format!("{}:/var/cache/smith/cargo", caches.join("cargo").display()),
        "-v".to_string(),
        format!("{}:/var/cache/smith/npm", caches.join("npm").display()),
        "-v".to_string(),
        format!("{}:/var/cache/smith/pip", caches.join("pip").display()),
    ];
    for mount in spec.ro_mounts {
        let path = mount.display().to_string();
        args.push("-v".to_string());
        args.push(format!("{path}:{path}:ro"));
    }
    args.push("-w".to_string());
    args.push(workspace);
    args.push(spec.image.to_string());
    args.push(spec.worker.display().to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ContainerSpec<'static> {
        ContainerSpec {
            workspace: Path::new("/home/chris/projects/smith"),
            session: "s1",
            cidfile: Path::new("/tmp/cid"),
            ro_mounts: &[],
            image: "smith-sandbox:0.1.0",
            worker: Path::new("/usr/local/bin/smith-worker"),
            version: "0.1.0",
        }
    }

    #[test]
    fn a_read_only_mount_covers_a_file_and_a_directory() {
        let mounts = [
            PathBuf::from("/home/chris/projects/smith/AGENTS.md"),
            PathBuf::from("/home/chris/projects/smith/.agents"),
        ];
        let spec = ContainerSpec {
            workspace: Path::new("/home/chris/projects/smith"),
            session: "s1",
            cidfile: Path::new("/tmp/cid"),
            ro_mounts: &mounts,
            image: "smith-sandbox:0.1.0",
            worker: Path::new("/usr/local/bin/smith-worker"),
            version: "0.1.0",
        };
        let args = run_args(&spec, Path::new("/home/chris/.local/share/smith/cache"));
        for path in mounts {
            let pair = format!("{}:{}:ro", path.display(), path.display());
            assert!(args.iter().any(|arg| arg == &pair), "{pair} is missing");
        }
    }

    #[test]
    fn every_security_flag_is_present() {
        let args = run_args(&spec(), Path::new("/home/chris/.local/share/smith/cache"));
        for flag in [
            "--network=none",
            "--read-only",
            "--read-only-tmpfs",
            "--userns=keep-id",
            "--cap-drop=all",
            "--preserve-fds=1",
            "--ulimit",
            "--pull=never",
        ] {
            assert!(args.iter().any(|arg| arg == flag), "{flag} is missing");
        }
    }

    #[test]
    fn the_workspace_is_mounted_at_its_own_path() {
        let args = run_args(&spec(), Path::new("/cache"));
        let workspace = "/home/chris/projects/smith";
        assert!(args
            .iter()
            .any(|arg| arg == &format!("{workspace}:{workspace}")));
        assert!(args.windows(2).any(|pair| pair == ["-w", workspace]));
    }

    #[test]
    fn no_environment_is_passed_on_the_command_line() {
        let args = run_args(&spec(), Path::new("/cache"));
        assert!(
            !args.iter().any(|arg| arg == "--env" || arg == "-e"),
            "environment values travel over the channel"
        );
    }

    #[test]
    fn read_only_mounts_keep_their_path_and_are_ro() {
        let mounts = vec![PathBuf::from("/home/chris/.rustup")];
        let mut spec = spec();
        spec.ro_mounts = &mounts;
        let args = run_args(&spec, Path::new("/cache"));
        assert!(args
            .iter()
            .any(|arg| arg == "/home/chris/.rustup:/home/chris/.rustup:ro"));
    }
}
