//! The working directory shared by one session's tools.
//!
//! The directory is confined to the workspace. A foreground shell reports the directory it
//! ended in, but the report is untrusted: a model can write the report file directly. The
//! worker canonicalizes the report inside the container. The host walks the components without
//! following a symlink, so a workspace directory replaced by a link to a host path cannot make
//! the host resolve that path. Only a plain directory inside the workspace is stored. A path
//! that no longer exists, or that gained a symlink, falls back to the workspace root.
//!
//! Relative file paths and the default `run_shell` directory are resolved against the stored
//! value before the call reaches the worker.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};

/// The current directory of one session.
#[derive(Debug)]
pub struct WorkingDir {
    root: PathBuf,
    cwd: Mutex<PathBuf>,
}

impl WorkingDir {
    /// Create a directory rooted at `root`, canonicalized when possible.
    pub fn new(root: PathBuf) -> Self {
        let root = fs::canonicalize(&root).unwrap_or(root);
        Self {
            cwd: Mutex::new(root.clone()),
            root,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The current directory, reset to the workspace root when it has disappeared or a
    /// component was replaced by a symlink.
    pub fn current(&self) -> PathBuf {
        let mut cwd = self.cwd.lock().unwrap_or_else(|e| e.into_inner());
        if !self.plain_directory(&cwd) {
            *cwd = self.root.clone();
        }
        cwd.clone()
    }

    /// Resolve relative paths in the call arguments against the current directory.
    ///
    /// `read_file`, `write_file` and `edit_file` carry a `path`; `run_shell` carries an optional
    /// `cwd`. Absolute paths are left alone and are still checked by the worker.
    pub fn rewrite(&self, name: &str, args: &mut Value) {
        let Some(object) = args.as_object_mut() else {
            return;
        };
        match name {
            "read_file" | "write_file" | "edit_file" => {
                let Some(Value::String(path)) = object.get_mut("path") else {
                    return;
                };
                if !Path::new(path).is_absolute() {
                    *path = self.current().join(&*path).display().to_string();
                }
            }
            "run_shell" => {
                let current = self.current();
                match object.get_mut("cwd") {
                    Some(Value::String(path)) if !Path::new(path).is_absolute() => {
                        *path = current.join(&*path).display().to_string();
                    }
                    Some(Value::Null) | None => {
                        object.insert("cwd".to_string(), json!(current.display().to_string()));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Apply the directory a `run_shell` result reports.
    ///
    /// A rejected report keeps the current directory and adds `cwd_error`, so the model sees
    /// that the change did not take effect. Non-shell results are returned unchanged.
    pub fn observe(&self, name: &str, content: String) -> String {
        if name != "run_shell" {
            return content;
        }
        let Ok(mut value) = serde_json::from_str::<Value>(&content) else {
            return content;
        };
        let Some(object) = value.as_object_mut() else {
            return content;
        };

        let inert = object.get("running").and_then(Value::as_bool) == Some(true)
            || object.get("timed_out").and_then(Value::as_bool) == Some(true)
            || object.get("cancelled").and_then(Value::as_bool) == Some(true);
        if inert {
            let current = self.current();
            object.insert("cwd".to_string(), json!(current.display().to_string()));
            return serde_json::to_string(&value).unwrap_or(content);
        }

        if let Some(error) = object
            .get("cwd_error")
            .and_then(Value::as_str)
            .filter(|error| !error.is_empty())
            .map(str::to_string)
        {
            let current = self.current();
            object.insert("cwd".to_string(), json!(current.display().to_string()));
            object.insert("cwd_error".to_string(), json!(error));
            return serde_json::to_string(&value).unwrap_or(content);
        }

        let Some(reported) = object
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return content;
        };

        match self.accept(&reported) {
            Ok(path) => {
                *self.cwd.lock().unwrap_or_else(|e| e.into_inner()) = path.clone();
                object.insert("cwd".to_string(), json!(path.display().to_string()));
            }
            Err(error) => {
                let current = self.current();
                object.insert("cwd".to_string(), json!(current.display().to_string()));
                object.insert("cwd_error".to_string(), json!(error));
            }
        }
        serde_json::to_string(&value).unwrap_or(content)
    }

    /// Resolve and validate a reported directory without following a symlink.
    ///
    /// The worker canonicalizes the shell's report inside the container. The host repeats the
    /// check, but it must not resolve a host path outside the workspace: a model can replace a
    /// workspace directory with a symlink after the worker accepted it. Walking the components
    /// with `symlink_metadata` leaves every such target unread.
    fn accept(&self, reported: &str) -> Result<PathBuf, String> {
        let candidate = Path::new(reported);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.current().join(candidate)
        };
        if !joined.starts_with(&self.root) {
            return Err("the reported directory is outside the workspace".to_string());
        }
        if !self.plain_directory(&joined) {
            return Err(
                "the reported directory is not a plain directory inside the workspace".to_string(),
            );
        }
        Ok(joined)
    }

    /// True when `path` is a real directory under the root and no component is a symlink.
    fn plain_directory(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        if !fs::symlink_metadata(&self.root)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
        {
            return false;
        }
        let mut current = self.root.clone();
        for component in relative.components() {
            match component {
                std::path::Component::Normal(part) => current.push(part),
                std::path::Component::CurDir => continue,
                _ => return false,
            }
            let Ok(metadata) = fs::symlink_metadata(&current) else {
                return false;
            };
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::SeqCst);
            let path =
                std::env::temp_dir().join(format!("smith-working-dir-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_directory_starts_at_the_root_and_resets_when_removed() {
        let dir = TestDir::new();
        let sub = dir.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        let working = WorkingDir::new(dir.path().to_path_buf());
        assert_eq!(working.current(), dir.path().canonicalize().unwrap());

        let accepted = working.observe(
            "run_shell",
            json!({"cwd": sub.display().to_string()}).to_string(),
        );
        assert!(accepted.contains("sub"), "{accepted}");
        assert_eq!(working.current(), sub.canonicalize().unwrap());

        fs::remove_dir(&sub).unwrap();
        assert_eq!(working.current(), dir.path().canonicalize().unwrap());
    }

    #[test]
    fn relative_paths_follow_the_current_directory() {
        let dir = TestDir::new();
        let sub = dir.path().join("sub");
        fs::create_dir_all(sub.join("nested")).unwrap();
        let working = WorkingDir::new(dir.path().to_path_buf());
        working.observe(
            "run_shell",
            json!({"cwd": sub.display().to_string()}).to_string(),
        );

        let mut args = json!({"path": "a.txt"});
        working.rewrite("read_file", &mut args);
        assert_eq!(args["path"], json!(sub.join("a.txt").display().to_string()));

        let mut shell = json!({"command": "pwd"});
        working.rewrite("run_shell", &mut shell);
        assert_eq!(
            shell["cwd"],
            json!(sub.canonicalize().unwrap().display().to_string())
        );

        let mut relative = json!({"command": "pwd", "cwd": "nested"});
        working.rewrite("run_shell", &mut relative);
        assert_eq!(
            relative["cwd"],
            json!(sub
                .canonicalize()
                .unwrap()
                .join("nested")
                .display()
                .to_string())
        );
    }

    #[test]
    fn an_outside_report_is_rejected_and_reported() {
        let dir = TestDir::new();
        let sub = dir.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        let working = WorkingDir::new(dir.path().to_path_buf());

        let content = working.observe("run_shell", json!({"cwd": "/tmp"}).to_string());
        let value: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            value["cwd"],
            json!(dir.path().canonicalize().unwrap().display().to_string())
        );
        assert!(value["cwd_error"].as_str().unwrap().contains("outside"));
        assert_eq!(working.current(), dir.path().canonicalize().unwrap());
    }

    #[test]
    fn a_symlinked_directory_is_rejected_and_resets_the_current_one() {
        let dir = TestDir::new();
        let sub = dir.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        let working = WorkingDir::new(dir.path().to_path_buf());
        working.observe(
            "run_shell",
            json!({"cwd": sub.display().to_string()}).to_string(),
        );
        assert_eq!(working.current(), sub.canonicalize().unwrap());

        fs::remove_dir(&sub).unwrap();
        std::os::unix::fs::symlink("/tmp", &sub).unwrap();
        assert_eq!(working.current(), dir.path().canonicalize().unwrap());

        let content = working.observe(
            "run_shell",
            json!({"cwd": sub.display().to_string()}).to_string(),
        );
        let value: Value = serde_json::from_str(&content).unwrap();
        assert!(value["cwd_error"].as_str().unwrap().contains("plain"));
    }

    #[test]
    fn a_background_or_timed_out_result_does_not_move_the_directory() {
        let dir = TestDir::new();
        let sub = dir.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        let working = WorkingDir::new(dir.path().to_path_buf());

        for result in [
            json!({"cwd": sub.display().to_string(), "running": true}),
            json!({"cwd": sub.display().to_string(), "timed_out": true}),
            json!({"cwd": sub.display().to_string(), "cancelled": true}),
        ] {
            let content = working.observe("run_shell", result.to_string());
            let value: Value = serde_json::from_str(&content).unwrap();
            assert_eq!(
                value["cwd"],
                json!(dir.path().canonicalize().unwrap().display().to_string())
            );
            assert_eq!(working.current(), dir.path().canonicalize().unwrap());
        }
    }

    #[test]
    fn a_worker_error_survives_the_host_check() {
        let dir = TestDir::new();
        let working = WorkingDir::new(dir.path().to_path_buf());
        let content = working.observe(
            "run_shell",
            json!({"cwd": dir.path().display().to_string(), "cwd_error": "outside"}).to_string(),
        );
        let value: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["cwd_error"], "outside");
        assert_eq!(
            value["cwd"],
            json!(dir.path().canonicalize().unwrap().display().to_string())
        );
    }
}
