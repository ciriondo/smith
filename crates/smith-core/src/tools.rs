//! Tool definitions and the seam between the session loop and a tool implementation.
//!
//! The loop calls [`Tool::prepare`] once per turn and [`Tool::call`] once per model tool
//! call. Implementations live outside this crate: `smith-sandbox` forwards calls over the
//! channel to the worker, and `smith-core::mock` provides a deterministic tool for tests.

use std::path::PathBuf;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
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

/// A bounded, terminal-independent preview of one tool call.
///
/// The agent computes it from the call arguments before the call and from the result after it.
/// Frontends render it without parsing the model-facing result JSON. The variants carry whole
/// rows, so the terminal can print them and the web UI can wrap them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolDisplay {
    Collapsed,
    Head {
        lines: Vec<String>,
        omitted: usize,
    },
    HeadTail {
        head: Vec<String>,
        tail: Vec<String>,
        omitted: usize,
    },
    Diff {
        lines: Vec<String>,
        omitted: usize,
    },
}

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

    /// The preview derived from the call arguments, before the call runs. Only the tools whose
    /// preview needs the arguments return one; the rest use [`Tool::display_result`].
    fn display_args(&self, args: &Value) -> Option<ToolDisplay> {
        display_for_args(&self.spec().name, args)
    }

    /// The preview derived from the model-facing result.
    fn display_result(&self, result: &str) -> ToolDisplay {
        display_for_result(&self.spec().name, result)
    }

    /// True from the start of a turn until its terminal event. The sandbox uses it to refuse
    /// an idle stop while a turn is running.
    fn set_busy(&self, busy: bool) {
        let _ = busy;
    }

    /// The session working directory, when this tool owns one.
    ///
    /// The sandbox and local tools share one value per session; the agent reads it to compose
    /// the next provider request. Tools without a directory return `None`.
    fn cwd(&self) -> Option<PathBuf> {
        None
    }
}

/// The line a tool contributes to the sandbox section.
///
/// The inventory lists the binaries the image provides. The replacement guidance follows hax:
/// it is added only when both commands are present, so the model is not told to prefer a tool
/// the image lacks.
pub fn inventory_sentence(inventory: &[String]) -> Option<String> {
    if inventory.is_empty() {
        return None;
    }
    let mut sentence = format!("Available commands: {}.", inventory.join(", "));
    let replacements = [("rg", "grep -r"), ("fd", "find"), ("python3", "python")];
    let preferences: Vec<String> = replacements
        .iter()
        .filter(|(available, _)| inventory.iter().any(|name| name == available))
        .filter(|(_, replaced)| {
            let command = replaced.split_whitespace().next().unwrap_or_default();
            inventory.iter().any(|name| name == command)
        })
        .map(|(available, replaced)| format!("`{available}` to `{replaced}`"))
        .collect();
    if !preferences.is_empty() {
        sentence.push_str(&format!("\nPrefer {}.", preferences.join(", ")));
    }
    Some(sentence)
}

/// The specs of all five tools, in a stable order.
pub fn specs() -> Vec<ToolSpec> {
    TOOL_NAMES.iter().filter_map(|name| spec(name)).collect()
}

/// The spec of one tool.
pub fn spec(name: &str) -> Option<ToolSpec> {
    let (description, schema) = match name {
        "read_file" => (
            "Read a text file inside the workspace. A relative path starts at the current \
             working directory. Returns at most 2000 lines and 64 KiB per call; `truncated` \
             reports that more remains. Binary files are rejected. Lines are 1-based and the \
             range is inclusive.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["path"],
                "properties": {
                    "path": {"type": "string", "description": "File to read, relative to the current working directory or absolute inside the workspace."},
                    "start_line": {"type": "integer", "minimum": 1, "description": "First line, 1-based."},
                    "end_line": {"type": "integer", "minimum": 1, "description": "Last line, inclusive."}
                }
            }),
        ),
        "write_file" => (
            "Write a text file inside the workspace, creating parent directories. A relative \
             path starts at the current working directory. The write is atomic: the previous \
             content survives a failed write.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["path", "content"],
                "properties": {
                    "path": {"type": "string", "description": "File to write, relative to the current working directory or absolute inside the workspace."},
                    "content": {"type": "string", "description": "Full replacement content."}
                }
            }),
        ),
        "edit_file" => (
            "Replace an exact string in a text file inside the workspace. A relative path \
             starts at the current working directory. `old_string` must occur exactly once \
             unless `replace_all` is true.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["path", "old_string", "new_string"],
                "properties": {
                    "path": {"type": "string", "description": "File to edit, relative to the current working directory or absolute inside the workspace."},
                    "old_string": {"type": "string", "description": "Exact text to replace."},
                    "new_string": {"type": "string", "description": "Replacement text."},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence. Default false."}
                }
            }),
        ),
        "run_shell" => (
            "Run a command with sh -c inside the sandbox. The working directory defaults to the \
             current working directory. The directory a foreground command ends in becomes the \
             working directory for later tool calls, so `cd` persists. A background command, a \
             command that ends by replacing the shell with `exec`, and a command stopped by the \
             timeout do not report a new directory. Output is captured per stream and capped at \
             32 KiB each. The default timeout is 120 seconds and the maximum is 600. With \
             `background: true` the call returns a task id immediately and the command runs \
             without a timeout; use `tool_wait` to collect its output, check its status or stop \
             it.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["command"],
                "properties": {
                    "command": {"type": "string", "description": "Shell command line."},
                    "cwd": {"type": "string", "description": "Working directory inside the workspace, relative to the current working directory."},
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

/// The preview limits. They follow hax: a short head, a short head plus tail, and a per-line
/// cap. The frontends wrap the rows to their own width.
const DISPLAY_HEAD_LINES: usize = 8;
const DISPLAY_HEAD_TAIL_HEAD: usize = 4;
const DISPLAY_HEAD_TAIL_TAIL: usize = 4;
const DISPLAY_LINE_CHARS: usize = 400;

/// The preview from the call arguments, when the arguments carry the useful part.
pub fn display_for_args(name: &str, args: &Value) -> Option<ToolDisplay> {
    match name {
        "write_file" => {
            let content = args.get("content").and_then(Value::as_str).unwrap_or("");
            Some(head_display(&display_lines(content), DISPLAY_HEAD_LINES))
        }
        "edit_file" => {
            let old = args.get("old_string").and_then(Value::as_str).unwrap_or("");
            let new = args.get("new_string").and_then(Value::as_str).unwrap_or("");
            Some(diff_display(old, new))
        }
        _ => None,
    }
}

/// The preview from the model-facing result.
pub fn display_for_result(name: &str, result: &str) -> ToolDisplay {
    match name {
        "run_shell" => shell_display(result),
        "tool_wait" => wait_display(result),
        _ => ToolDisplay::Collapsed,
    }
}

/// A preview for a failed call. The error message is the only output.
pub fn failure_display(message: &str) -> ToolDisplay {
    head_display(&display_lines(message), DISPLAY_HEAD_LINES)
}

fn display_value(result: &str) -> Option<Value> {
    serde_json::from_str(result).ok()
}

fn shell_display(result: &str) -> ToolDisplay {
    let Some(value) = display_value(result) else {
        return ToolDisplay::Collapsed;
    };
    if value["task_id"].is_string() || value["running"].as_bool() == Some(true) {
        return ToolDisplay::Collapsed;
    }
    let mut lines = display_lines(value["stdout"].as_str().unwrap_or(""));
    for line in display_lines(value["stderr"].as_str().unwrap_or("")) {
        lines.push(format!("! {line}"));
    }
    head_tail_display(lines)
}

fn wait_display(result: &str) -> ToolDisplay {
    let Some(value) = display_value(result) else {
        return ToolDisplay::Collapsed;
    };
    let mut lines = display_lines(value["stdout"].as_str().unwrap_or(""));
    for line in display_lines(value["stderr"].as_str().unwrap_or("")) {
        lines.push(format!("! {line}"));
    }
    head_tail_display(lines)
}

fn head_display(lines: &[String], limit: usize) -> ToolDisplay {
    if lines.is_empty() {
        return ToolDisplay::Collapsed;
    }
    let shown = lines.len().min(limit);
    ToolDisplay::Head {
        lines: lines[..shown].to_vec(),
        omitted: lines.len() - shown,
    }
}

fn head_tail_display(lines: Vec<String>) -> ToolDisplay {
    if lines.is_empty() {
        return ToolDisplay::Collapsed;
    }
    let head_count = lines.len().min(DISPLAY_HEAD_TAIL_HEAD);
    let tail_count = lines
        .len()
        .saturating_sub(head_count)
        .min(DISPLAY_HEAD_TAIL_TAIL);
    let head = lines[..head_count].to_vec();
    let tail = lines[lines.len() - tail_count..].to_vec();
    ToolDisplay::HeadTail {
        head,
        tail,
        omitted: lines.len() - head_count - tail_count,
    }
}

fn diff_display(old: &str, new: &str) -> ToolDisplay {
    let old_lines = display_lines(old);
    let new_lines = display_lines(new);
    let old_taken = old_lines.len().min(DISPLAY_HEAD_TAIL_HEAD);
    let new_taken = new_lines.len().min(DISPLAY_HEAD_TAIL_TAIL);
    let mut lines: Vec<String> = old_lines[..old_taken]
        .iter()
        .map(|line| format!("-{line}"))
        .collect();
    lines.extend(new_lines[..new_taken].iter().map(|line| format!("+{line}")));
    let omitted = old_lines.len() - old_taken + new_lines.len() - new_taken;
    if lines.is_empty() {
        ToolDisplay::Collapsed
    } else {
        ToolDisplay::Diff { lines, omitted }
    }
}

fn display_lines(text: &str) -> Vec<String> {
    text.lines().map(sanitize_line).collect()
}

/// Remove terminal control sequences, expand tabs and cap the row.
fn sanitize_line(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    let mut length = 0;
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            skip_escape(&mut chars);
            continue;
        }
        if c == '\t' {
            let spaces = 4 - (length % 4);
            for _ in 0..spaces {
                if length >= DISPLAY_LINE_CHARS {
                    break;
                }
                out.push(' ');
                length += 1;
            }
            continue;
        }
        if c.is_control() {
            continue;
        }
        if length >= DISPLAY_LINE_CHARS {
            break;
        }
        out.push(c);
        length += 1;
    }
    out
}

fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => {
            for c in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&c) {
                    break;
                }
            }
        }
        Some(']') => {
            for c in chars.by_ref() {
                if c == '\x07' || c == '\x1b' {
                    break;
                }
            }
        }
        _ => {}
    }
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
    fn the_inventory_prefers_replacements_only_when_both_exist() {
        let inventory: Vec<String> = ["bash", "grep", "rg", "find", "fd", "python", "python3"]
            .iter()
            .map(|name| name.to_string())
            .collect();
        let sentence = inventory_sentence(&inventory).unwrap();
        assert!(
            sentence.starts_with("Available commands: bash, grep, rg, find, fd, python, python3."),
            "{sentence}"
        );
        assert!(
            sentence.contains("Prefer `rg` to `grep -r`, `fd` to `find`, `python3` to `python`."),
            "{sentence}"
        );

        let partial = inventory_sentence(&["rg".to_string()]).unwrap();
        assert!(!partial.contains("Prefer"), "{partial}");
        assert_eq!(inventory_sentence(&[]), None);
    }

    #[test]
    fn the_shell_preview_keeps_a_head_and_tail() {
        let content = json!({
            "exit_code": 0,
            "stdout": "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n",
            "stderr": "bad\n",
            "truncated": false, "timed_out": false, "cancelled": false, "running": false
        })
        .to_string();
        match display_for_result("run_shell", &content) {
            ToolDisplay::HeadTail {
                head,
                tail,
                omitted,
            } => {
                assert_eq!(head, vec!["one", "two", "three", "four"]);
                assert_eq!(tail, vec!["seven", "eight", "nine", "! bad"]);
                assert_eq!(omitted, 2);
            }
            other => panic!("unexpected display: {other:?}"),
        }
    }

    #[test]
    fn previews_strip_control_sequences_and_expand_tabs() {
        let content = json!({
            "exit_code": 0,
            "stdout": "\u{001b}[31mred\u{001b}[0m\tend\n",
            "stderr": "", "truncated": false, "timed_out": false,
            "cancelled": false, "running": false
        })
        .to_string();
        match display_for_result("run_shell", &content) {
            ToolDisplay::HeadTail { head, .. } => assert_eq!(head, vec!["red end"]),
            other => panic!("unexpected display: {other:?}"),
        }
    }

    #[test]
    fn a_background_start_and_a_read_are_collapsed() {
        let background = json!({
            "exit_code": null, "stdout": "", "stderr": "",
            "truncated": false, "timed_out": false, "cancelled": false,
            "task_id": "t1", "running": true
        })
        .to_string();
        assert_eq!(
            display_for_result("run_shell", &background),
            ToolDisplay::Collapsed
        );
        assert_eq!(
            display_for_result("read_file", "{\"content\":\"secret\"}"),
            ToolDisplay::Collapsed
        );
    }

    #[test]
    fn an_edit_preview_is_a_diff_and_a_write_is_a_head() {
        let edit = display_for_args(
            "edit_file",
            &json!({"path": "a", "old_string": "old\nline2", "new_string": "new"}),
        )
        .unwrap();
        assert_eq!(
            edit,
            ToolDisplay::Diff {
                lines: vec!["-old".to_string(), "-line2".to_string(), "+new".to_string()],
                omitted: 0,
            }
        );

        let content = (1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        match display_for_args("write_file", &json!({"path": "a", "content": content})).unwrap() {
            ToolDisplay::Head { lines, omitted } => {
                assert_eq!(lines.len(), 8);
                assert_eq!(omitted, 2);
                assert_eq!(lines[0], "line 1");
            }
            other => panic!("unexpected display: {other:?}"),
        }
        assert_eq!(display_for_args("run_shell", &json!({})), None);
    }

    #[test]
    fn a_failure_preview_is_the_error_head() {
        assert_eq!(
            failure_display("boom"),
            ToolDisplay::Head {
                lines: vec!["boom".to_string()],
                omitted: 0,
            }
        );
    }

    #[test]
    fn displays_round_trip_through_json() {
        let displays = [
            ToolDisplay::Collapsed,
            ToolDisplay::Head {
                lines: vec!["a".to_string()],
                omitted: 2,
            },
            ToolDisplay::HeadTail {
                head: vec!["a".to_string()],
                tail: vec!["b".to_string()],
                omitted: 3,
            },
            ToolDisplay::Diff {
                lines: vec!["-a".to_string(), "+b".to_string()],
                omitted: 0,
            },
        ];
        for display in displays {
            let text = serde_json::to_string(&display).unwrap();
            assert_eq!(serde_json::from_str::<ToolDisplay>(&text).unwrap(), display);
        }
        assert_eq!(
            serde_json::to_value(ToolDisplay::Collapsed).unwrap(),
            json!({"kind": "collapsed"})
        );
    }

    #[test]
    fn the_tool_defaults_use_the_shared_display() {
        use crate::mock::MockTool;
        let tool = MockTool::new("edit_file", "{}");
        assert!(tool
            .display_args(&json!({"old_string": "a", "new_string": "b"}))
            .is_some());
        let result = json!({
            "exit_code": 0, "stdout": "hi\n", "stderr": "",
            "truncated": false, "timed_out": false, "cancelled": false, "running": false
        })
        .to_string();
        let tool = MockTool::new("run_shell", "{}");
        match tool.display_result(&result) {
            ToolDisplay::HeadTail { head, .. } => assert_eq!(head, vec!["hi"]),
            other => panic!("unexpected display: {other:?}"),
        }
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
