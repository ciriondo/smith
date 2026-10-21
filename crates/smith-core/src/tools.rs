//! Tool definitions and the seam between the session loop and a tool implementation.
//!
//! The loop calls [`Tool::prepare`] once per turn and [`Tool::call`] once per model tool
//! call. Implementations live outside this crate: `smith-sandbox` forwards calls over the
//! channel to the worker, and `smith-core::mock` provides a deterministic tool for tests.

use std::path::PathBuf;

use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::provider::ToolSpec;

/// The five tool names this milestone implements.
pub const TOOL_NAMES: [&str; 5] = [
    "read_file",
    "write_file",
    "edit_file",
    "run_shell",
    "tool_wait",
];

/// Why a tool call did not produce a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    /// A message for the model. The turn continues and the message becomes a failed
    /// `ToolResult`.
    Message(String),
    /// The channel to the sandbox is gone. The turn fails, because no result can arrive.
    SandboxGone,
}

impl ToolError {
    pub fn message(text: impl Into<String>) -> Self {
        ToolError::Message(text.into())
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::Message(message) => f.write_str(message),
            ToolError::SandboxGone => f.write_str("the sandbox is gone"),
        }
    }
}

/// What a tool call needs from the session.
pub struct ToolContext {
    pub workspace: PathBuf,
    pub cancel: CancellationToken,
}

/// One tool the model can call.
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    /// Called once at the start of a turn. A returned sentence is appended to the system
    /// prompt of that turn; the sandbox tool returns the image inventory.
    fn prepare<'a>(&'a self) -> BoxFuture<'a, Result<Option<String>, ToolError>> {
        Box::pin(async { Ok(None) })
    }

    fn call<'a>(
        &'a self,
        args: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<String, ToolError>>;

    /// True from the start of a turn until its terminal event. The sandbox uses it to refuse
    /// an idle stop while a turn is running.
    fn set_busy(&self, busy: bool) {
        let _ = busy;
    }
}

/// The specs of all five tools, in a stable order.
pub fn specs() -> Vec<ToolSpec> {
    TOOL_NAMES.iter().filter_map(|name| spec(name)).collect()
}

/// The spec of one tool.
pub fn spec(name: &str) -> Option<ToolSpec> {
    let (description, schema) = match name {
        "read_file" => (
            "Read a text file inside the workspace. Returns at most 2000 lines and 64 KiB per \
             call; `truncated` reports that more remains. Binary files are rejected. Lines are \
             1-based and the range is inclusive.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["path"],
                "properties": {
                    "path": {"type": "string", "description": "File to read, relative to the workspace or absolute inside it."},
                    "start_line": {"type": "integer", "minimum": 1, "description": "First line, 1-based."},
                    "end_line": {"type": "integer", "minimum": 1, "description": "Last line, inclusive."}
                }
            }),
        ),
        "write_file" => (
            "Write a text file inside the workspace, creating parent directories. The write is \
             atomic: the previous content survives a failed write.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["path", "content"],
                "properties": {
                    "path": {"type": "string", "description": "File to write, relative to the workspace or absolute inside it."},
                    "content": {"type": "string", "description": "Full replacement content."}
                }
            }),
        ),
        "edit_file" => (
            "Replace an exact string in a text file inside the workspace. `old_string` must \
             occur exactly once unless `replace_all` is true.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["path", "old_string", "new_string"],
                "properties": {
                    "path": {"type": "string", "description": "File to edit, relative to the workspace or absolute inside it."},
                    "old_string": {"type": "string", "description": "Exact text to replace."},
                    "new_string": {"type": "string", "description": "Replacement text."},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence. Default false."}
                }
            }),
        ),
        "run_shell" => (
            "Run a command with sh -c inside the sandbox. Working directory defaults to the \
             workspace root. Output is captured per stream and capped at 32 KiB each. The \
             default timeout is 120 seconds and the maximum is 600. With `background: true` \
             the call returns a task id immediately and the command runs without a timeout; \
             use `tool_wait` to collect its output, check its status or stop it.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["command"],
                "properties": {
                    "command": {"type": "string", "description": "Shell command line."},
                    "cwd": {"type": "string", "description": "Working directory inside the workspace."},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600, "description": "Timeout in seconds. Ignored for a background run."},
                    "background": {"type": "boolean", "description": "Start the command as a background task and return its id. Default false."}
                }
            }),
        ),
        "tool_wait" => (
            "Wait on a background shell run started by `run_shell` with `background: true`. \
             Returns the output produced since the previous wait for that run, plus its status. \
             `timeout_secs` defaults to 120 and is capped at 600; the run keeps going when the \
             wait times out. `kill` stops the run with SIGTERM, then SIGKILL after a grace period.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": {"type": "string", "description": "Task id returned by a background run_shell call."},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600, "description": "Seconds to wait. Default 120."},
                    "kill": {"type": "boolean", "description": "Stop the run and return its final output. Default false."}
                }
            }),
        ),
        _ => return None,
    };

    Some(ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        schema,
    })
}

/// One line for `ToolStarted`, built from the arguments.
pub fn summarize_call(name: &str, args: &Value) -> String {
    if args.is_null() {
        return format!("{name} (arguments did not parse)");
    }
    let target = args
        .get("path")
        .or_else(|| args.get("command"))
        .or_else(|| args.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let first_line = target.lines().next().unwrap_or("");
    let trimmed = truncate(first_line, 60);
    if trimmed.is_empty() {
        name.to_string()
    } else {
        format!("{name} {trimmed}")
    }
}

/// One line for `ToolFinished`, built from the result.
pub fn summarize_result(name: &str, ok: bool, truncated: bool, content: &str) -> String {
    if !ok {
        return "failed".to_string();
    }
    if name == "run_shell" {
        if let Ok(value) = serde_json::from_str::<Value>(content) {
            if let Some(id) = value["task_id"].as_str() {
                return format!("started {id}");
            }
            if value["cancelled"].as_bool() == Some(true) {
                return "cancelled".to_string();
            }
            if value["timed_out"].as_bool() == Some(true) {
                return "timed out".to_string();
            }
            if let Some(code) = value["exit_code"].as_i64() {
                let suffix = if truncated { ", output truncated" } else { "" };
                return format!("exit {code}{suffix}");
            }
        }
    }
    if name == "tool_wait" {
        if let Ok(value) = serde_json::from_str::<Value>(content) {
            let id = value["id"].as_str().unwrap_or("?");
            if value["killed"].as_bool() == Some(true) {
                return format!("{id} killed");
            }
            if value["running"].as_bool() == Some(true) {
                let suffix = if value["timed_out"].as_bool() == Some(true) {
                    "still running"
                } else {
                    "running"
                };
                return format!("{id} {suffix}");
            }
            if let Some(signal) = value["signal"].as_i64() {
                return format!("{id} signal {signal}");
            }
            if let Some(code) = value["exit_code"].as_i64() {
                let suffix = if truncated { ", output truncated" } else { "" };
                return format!("{id} exit {code}{suffix}");
            }
            return format!("{id} finished");
        }
    }
    if truncated {
        "ok (truncated)".to_string()
    } else {
        "ok".to_string()
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_spec_with_a_closed_schema() {
        for name in TOOL_NAMES {
            let spec = spec(name).expect(name);
            assert_eq!(spec.name, name);
            assert_eq!(spec.schema["additionalProperties"], false);
            assert!(!spec.description.is_empty());
        }
        assert_eq!(specs().len(), TOOL_NAMES.len());
        assert!(spec("rm_rf").is_none());

        let wait = spec("tool_wait").expect("tool_wait");
        assert_eq!(wait.schema["required"], json!(["id"]));
        let shell = spec("run_shell").expect("run_shell");
        assert_eq!(shell.schema["properties"]["background"]["type"], "boolean");
    }

    #[test]
    fn call_summaries_name_the_target() {
        assert_eq!(
            summarize_call("read_file", &json!({"path": "src/main.rs"})),
            "read_file src/main.rs"
        );
        assert_eq!(
            summarize_call("run_shell", &json!({"command": "cargo test\n--all"})),
            "run_shell cargo test"
        );
        assert_eq!(
            summarize_call("read_file", &Value::Null),
            "read_file (arguments did not parse)"
        );
        assert_eq!(
            summarize_call("tool_wait", &json!({"id": "t1"})),
            "tool_wait t1"
        );
    }

    #[test]
    fn result_summaries_use_the_exit_status() {
        let content = json!({
            "exit_code": 0, "stdout": "", "stderr": "",
            "truncated": false, "timed_out": false, "cancelled": false
        })
        .to_string();
        assert_eq!(
            summarize_result("run_shell", true, false, &content),
            "exit 0"
        );
        assert_eq!(
            summarize_result("read_file", true, true, "{}"),
            "ok (truncated)"
        );
        assert_eq!(
            summarize_result("read_file", false, false, "boom"),
            "failed"
        );
    }

    #[test]
    fn background_summaries_name_the_task_and_status() {
        let started = json!({
            "exit_code": null, "stdout": "", "stderr": "", "truncated": false,
            "timed_out": false, "cancelled": false, "task_id": "t1", "running": true
        })
        .to_string();
        assert_eq!(
            summarize_result("run_shell", true, false, &started),
            "started t1"
        );

        let running = json!({
            "id": "t1", "running": true, "exit_code": null, "signal": null,
            "stdout": "", "stderr": "", "truncated": false, "has_more": false,
            "timed_out": true, "killed": false
        })
        .to_string();
        assert_eq!(
            summarize_result("tool_wait", true, false, &running),
            "t1 still running"
        );

        let exited = json!({
            "id": "t1", "running": false, "exit_code": 0, "signal": null,
            "stdout": "", "stderr": "", "truncated": false, "has_more": false,
            "timed_out": false, "killed": false
        })
        .to_string();
        assert_eq!(
            summarize_result("tool_wait", true, false, &exited),
            "t1 exit 0"
        );

        let signalled = json!({
            "id": "t1", "running": false, "exit_code": null, "signal": 15,
            "stdout": "", "stderr": "", "truncated": false, "has_more": false,
            "timed_out": false, "killed": true
        })
        .to_string();
        assert_eq!(
            summarize_result("tool_wait", true, false, &signalled),
            "t1 killed"
        );
    }
}
