//! Tool arguments and results, shared by the host and the worker.
//!
//! The host validates arguments against a JSON Schema in `smith-core::tools` before sending a
//! call. The worker parses the same bytes into the structs here. Results travel back as JSON
//! in [`crate::ToolResult::content`].

use serde::{Deserialize, Serialize};

/// Arguments of `read_file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFileArgs {
    pub path: String,
    /// First line to return, 1-based. Defaults to the first line.
    pub start_line: Option<u32>,
    /// Last line to return, inclusive. Defaults to the end of the file.
    pub end_line: Option<u32>,
}

/// Result of `read_file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFileResult {
    pub path: String,
    /// First line present in `content`, 1-based. Zero when `content` is empty.
    pub start_line: u32,
    /// Last line present in `content`, inclusive. Zero when `content` is empty.
    pub end_line: u32,
    pub total_lines: u32,
    pub truncated: bool,
    pub content: String,
}

/// Arguments of `write_file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteFileArgs {
    pub path: String,
    pub content: String,
}

/// Result of `write_file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteFileResult {
    pub path: String,
    pub bytes: u64,
}

/// Arguments of `edit_file`. `old_string` must occur exactly once unless `replace_all` is
/// true.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditFileArgs {
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

/// Result of `edit_file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditFileResult {
    pub path: String,
    pub replacements: u32,
}

/// Arguments of `run_shell`. `cwd` is resolved against the workspace and defaults to its
/// root. `timeout_secs` defaults to 120 and is capped at 600.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunShellArgs {
    pub command: String,
    pub cwd: Option<String>,
    pub timeout_secs: Option<u32>,
}

/// Result of `run_shell`. `exit_code` is `None` when no code was produced, for example when
/// the command timed out or was cancelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunShellResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// True when either stream hit its size cap.
    pub truncated: bool,
    pub timed_out: bool,
    pub cancelled: bool,
}
