//! Path resolution for the file tools.
//!
//! Every path the model supplies resolves against the workspace and must stay inside it.
//! Paths containing `..` are rejected rather than normalized, because a normalization that
//! disagrees with the kernel is where confinement bugs appear.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// The workspace root, canonicalized once at startup.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, String> {
        let root = root.into();
        let canonical = root
            .canonicalize()
            .map_err(|e| format!("cannot resolve the workspace {}: {e}", root.display()))?;
        if !canonical.is_dir() {
            return Err(format!(
                "the workspace {} is not a directory",
                canonical.display()
            ));
        }
        Ok(Self { root: canonical })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a path that must already exist. A directory is returned as it is; callers
    /// decide whether that is an error.
    pub fn existing(&self, path: &str) -> Result<PathBuf, String> {
        let candidate = self.candidate(path)?;
        let resolved = self.confine(&candidate)?;
        if !resolved.exists() {
            return Err(format!("{} does not exist", resolved.display()));
        }
        Ok(resolved)
    }

    /// Resolve a path that may not exist yet. The caller creates the parent directory.
    pub fn writable(&self, path: &str) -> Result<PathBuf, String> {
        let candidate = self.candidate(path)?;
        self.confine(&candidate)
    }

    fn candidate(&self, path: &str) -> Result<PathBuf, String> {
        if path.is_empty() {
            return Err("path is empty".to_string());
        }
        let raw = Path::new(path);
        if raw
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(format!("{path}: '..' is not allowed in a tool path"));
        }
        Ok(if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.root.join(raw)
        })
    }

    /// Canonicalize the longest existing ancestor, append the rest, and require the result to
    /// stay under the root. A dangling symlink is an error rather than something to step
    /// over, because stepping over it would let a later write follow it out of the workspace.
    fn confine(&self, candidate: &Path) -> Result<PathBuf, String> {
        let mut existing = candidate.to_path_buf();
        let mut suffix: Vec<OsString> = Vec::new();
        loop {
            if existing.exists() {
                let canonical = existing
                    .canonicalize()
                    .map_err(|e| format!("cannot resolve {}: {e}", existing.display()))?;
                let mut resolved = canonical;
                for part in suffix.iter().rev() {
                    resolved.push(part);
                }
                if !resolved.starts_with(&self.root) {
                    return Err(format!("{} is outside the workspace", candidate.display()));
                }
                return Ok(resolved);
            }
            if existing.symlink_metadata().is_ok() {
                return Err(format!("{} is a dangling symlink", existing.display()));
            }
            match existing.file_name() {
                Some(name) => {
                    suffix.push(name.to_os_string());
                    existing.pop();
                }
                None => return Err(format!("{} is outside the workspace", candidate.display())),
            }
        }
    }
}
