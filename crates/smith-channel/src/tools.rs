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
/// root. `timeout_secs` defaults to 120 and is capped at 600 for a synchronous call. A
/// background call ignores it and runs until it finishes or is killed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunShellArgs {
    pub command: String,
    pub cwd: Option<String>,
    pub timeout_secs: Option<u32>,
    #[serde(default)]
    pub background: bool,
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
    /// Present when this result starts a background run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// True for a background start, false for a synchronous result.
    #[serde(default)]
    pub running: bool,
    /// The session working directory after the call. A foreground shell reports the directory
    /// it ended in; the worker and host check it before it is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Why a reported directory was rejected. The session keeps its previous directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_error: Option<String>,
}

/// Arguments of `tool_wait`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolWaitArgs {
    /// Task id returned by `run_shell` with `background: true`.
    pub id: String,
    /// Seconds to wait. Defaults to 120 and is capped at 600.
    pub timeout_secs: Option<u32>,
    /// Stop the task, then return its final output.
    #[serde(default)]
    pub kill: bool,
}

/// Result of `tool_wait`. Output fields carry only bytes produced since the previous wait.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolWaitResult {
    pub id: String,
    /// True when the task was still running when the wait returned.
    pub running: bool,
    pub exit_code: Option<i32>,
    /// Terminating signal, when the task died from one.
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// True when the 1 MiB spool cap discarded output.
    pub truncated: bool,
    /// True when the spool holds more than this wait returned.
    pub has_more: bool,
    /// True when the wait timeout expired before the task finished.
    pub timed_out: bool,
    /// True when this wait sent a signal to the task.
    pub killed: bool,
}
