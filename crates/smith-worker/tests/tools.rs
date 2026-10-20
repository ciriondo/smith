use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use smith_channel::{ReadFileResult, RunShellResult, WriteFileResult};
use smith_worker::tools::{execute, ToolContext};
use smith_worker::{CancelFlag, Workspace};

struct Fixture {
    dir: tempfile::TempDir,
    workspace: Workspace,
    env: BTreeMap<String, String>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let env = BTreeMap::from([
        (
            "HOME".to_string(),
            dir.path().join("home").display().to_string(),
        ),
        (
            "PATH".to_string(),
            "/usr/local/bin:/usr/bin:/bin".to_string(),
        ),
    ]);
    fs::create_dir_all(dir.path().join("home")).unwrap();
    Fixture {
        dir,
        workspace,
        env,
    }
}

fn with_ctx<T>(fixture: &Fixture, f: impl FnOnce(&ToolContext<'_>) -> T) -> T {
    let cancel = CancelFlag::new();
    let ctx = ToolContext {
        workspace: &fixture.workspace,
        env: &fixture.env,
        cancel: &cancel,
    };
    f(&ctx)
}

fn read_file(ctx: &ToolContext<'_>, args: Value) -> Result<Value, String> {
    execute("read_file", args, ctx).map(|json| serde_json::from_str(&json).unwrap())
}

fn shell_result(ctx: &ToolContext<'_>, args: Value) -> Result<RunShellResult, String> {
    execute("run_shell", args, ctx).map(|json| serde_json::from_str(&json).unwrap())
}

#[test]
fn read_file_returns_the_whole_file() {
    let fixture = fixture();
    fs::write(fixture.dir.path().join("a.txt"), "one\ntwo\n").unwrap();
    let result: ReadFileResult = serde_json::from_str(&with_ctx(&fixture, |ctx| {
        execute("read_file", json!({"path": "a.txt"}), ctx).unwrap()
    }))
    .unwrap();
    assert_eq!(result.content, "one\ntwo\n");
    assert_eq!(result.total_lines, 2);
    assert_eq!(result.start_line, 1);
    assert_eq!(result.end_line, 2);
    assert!(!result.truncated);
}

#[test]
fn read_file_returns_a_requested_range() {
    let fixture = fixture();
    fs::write(fixture.dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let result = with_ctx(&fixture, |ctx| {
        read_file(
            ctx,
            json!({"path": "a.txt", "start_line": 2, "end_line": 3}),
        )
        .unwrap()
    });
    assert_eq!(result["content"], "two\nthree\n");
    assert_eq!(result["start_line"], 2);
    assert_eq!(result["end_line"], 3);
    assert_eq!(result["truncated"], false);
}

#[test]
fn read_file_truncates_at_two_thousand_lines() {
    let fixture = fixture();
    let mut text = String::new();
    for index in 1..=2500 {
        text.push_str(&format!("line {index}\n"));
    }
    fs::write(fixture.dir.path().join("big.txt"), text).unwrap();

    let result: ReadFileResult = serde_json::from_str(&with_ctx(&fixture, |ctx| {
        execute("read_file", json!({"path": "big.txt"}), ctx).unwrap()
    }))
    .unwrap();
    assert_eq!(result.total_lines, 2500);
    assert_eq!(result.end_line, 2000);
    assert!(result.truncated);
    assert!(result.content.contains("line 2000"));
    assert!(!result.content.contains("line 2001"));
}

#[test]
fn read_file_caps_a_single_long_line() {
    let fixture = fixture();
    fs::write(fixture.dir.path().join("long.txt"), "a".repeat(70_000)).unwrap();
    let result: ReadFileResult = serde_json::from_str(&with_ctx(&fixture, |ctx| {
        execute("read_file", json!({"path": "long.txt"}), ctx).unwrap()
    }))
    .unwrap();
    assert_eq!(result.content.len(), 64 * 1024);
    assert!(result.truncated);
}

#[test]
fn read_file_rejects_binary_content() {
    let fixture = fixture();
    fs::write(fixture.dir.path().join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
    let err = with_ctx(&fixture, |ctx| {
        execute("read_file", json!({"path": "blob.bin"}), ctx).unwrap_err()
    });
    assert!(err.contains("binary"), "{err}");
}

#[test]
fn read_file_rejects_paths_outside_the_workspace() {
    let fixture = fixture();
    let err = with_ctx(&fixture, |ctx| {
        execute("read_file", json!({"path": "/etc/passwd"}), ctx).unwrap_err()
    });
    assert!(err.contains("outside the workspace"), "{err}");

    let err = with_ctx(&fixture, |ctx| {
        execute("read_file", json!({"path": "../escape"}), ctx).unwrap_err()
    });
    assert!(err.contains(".."), "{err}");
}

#[test]
fn write_file_creates_parent_directories() {
    let fixture = fixture();
    let result: WriteFileResult = serde_json::from_str(&with_ctx(&fixture, |ctx| {
        execute(
            "write_file",
            json!({"path": "nested/dir/a.txt", "content": "hello\n"}),
            ctx,
        )
        .unwrap()
    }))
    .unwrap();
    assert_eq!(result.bytes, 6);
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("nested/dir/a.txt")).unwrap(),
        "hello\n"
    );
}

#[test]
fn write_file_replaces_existing_content() {
    let fixture = fixture();
    fs::write(fixture.dir.path().join("a.txt"), "old\n").unwrap();
    with_ctx(&fixture, |ctx| {
        execute(
            "write_file",
            json!({"path": "a.txt", "content": "new\n"}),
            ctx,
        )
        .unwrap()
    });
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("a.txt")).unwrap(),
        "new\n"
    );
}

#[test]
fn edit_file_replaces_a_unique_string() {
    let fixture = fixture();
    fs::write(fixture.dir.path().join("a.txt"), "let x = 1;\n").unwrap();
    let result = with_ctx(&fixture, |ctx| {
        execute(
            "edit_file",
            json!({"path": "a.txt", "old_string": "1", "new_string": "2"}),
            ctx,
        )
        .unwrap()
    });
    let value: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(value["replacements"], 1);
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("a.txt")).unwrap(),
        "let x = 2;\n"
    );
}

#[test]
fn edit_file_refuses_a_duplicate_string_without_replace_all() {
    let fixture = fixture();
    fs::write(fixture.dir.path().join("a.txt"), "a b a\n").unwrap();
    let err = with_ctx(&fixture, |ctx| {
        execute(
            "edit_file",
            json!({"path": "a.txt", "old_string": "a", "new_string": "c"}),
            ctx,
        )
        .unwrap_err()
    });
    assert!(err.contains("occurs 2 times"), "{err}");

    let result = with_ctx(&fixture, |ctx| {
        execute(
            "edit_file",
            json!({"path": "a.txt", "old_string": "a", "new_string": "c", "replace_all": true}),
            ctx,
        )
        .unwrap()
    });
    let value: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(value["replacements"], 2);
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("a.txt")).unwrap(),
        "c b c\n"
    );
}

#[test]
fn edit_file_reports_a_missing_string() {
    let fixture = fixture();
    fs::write(fixture.dir.path().join("a.txt"), "a\n").unwrap();
    let err = with_ctx(&fixture, |ctx| {
        execute(
            "edit_file",
            json!({"path": "a.txt", "old_string": "z", "new_string": "y"}),
            ctx,
        )
        .unwrap_err()
    });
    assert!(err.contains("does not occur"), "{err}");
}

#[test]
fn run_shell_returns_output_and_status() {
    let fixture = fixture();
    let result = with_ctx(&fixture, |ctx| {
        shell_result(ctx, json!({"command": "printf out; printf err >&2"})).unwrap()
    });
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.stdout, "out");
    assert_eq!(result.stderr, "err");
    assert!(!result.truncated);
    assert!(!result.timed_out);
    assert!(!result.cancelled);
}

#[test]
fn run_shell_reports_a_nonzero_exit() {
    let fixture = fixture();
    let result = with_ctx(&fixture, |ctx| {
        shell_result(ctx, json!({"command": "exit 3"})).unwrap()
    });
    assert_eq!(result.exit_code, Some(3));
}

#[test]
fn run_shell_honours_cwd() {
    let fixture = fixture();
    fs::create_dir_all(fixture.dir.path().join("sub")).unwrap();
    let result = with_ctx(&fixture, |ctx| {
        shell_result(ctx, json!({"command": "pwd", "cwd": "sub"})).unwrap()
    });
    let printed = result.stdout.trim_end();
    assert!(
        printed.ends_with("/sub"),
        "expected the sub directory, got {printed}"
    );
}

#[test]
fn run_shell_caps_its_output() {
    let fixture = fixture();
    let result = with_ctx(&fixture, |ctx| {
        shell_result(
            ctx,
            json!({"command": "head -c 100000 /dev/zero | tr '\\0' 'a'"}),
        )
        .unwrap()
    });
    assert!(result.truncated);
    assert_eq!(result.stdout.len(), 32 * 1024);
}

#[test]
fn run_shell_times_out_and_kills_the_group() {
    let fixture = fixture();
    let started = Instant::now();
    let result = with_ctx(&fixture, |ctx| {
        shell_result(ctx, json!({"command": "sleep 30", "timeout_secs": 1})).unwrap()
    });
    assert!(result.timed_out);
    assert!(!result.cancelled);
    assert_eq!(result.exit_code, None);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn run_shell_cancels_on_request() {
    let fixture = fixture();
    let cancel = Arc::new(CancelFlag::new());
    let trigger = cancel.clone();
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        trigger.cancel();
    });
    let ctx = ToolContext {
        workspace: &fixture.workspace,
        env: &fixture.env,
        cancel: &cancel,
    };
    let started = Instant::now();
    let result = shell_result(&ctx, json!({"command": "sleep 30"})).unwrap();
    canceller.join().unwrap();
    assert!(result.cancelled);
    assert!(!result.timed_out);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn an_unknown_tool_is_an_error() {
    let fixture = fixture();
    let err = with_ctx(&fixture, |ctx| {
        execute("rm_rf", json!({}), ctx).unwrap_err()
    });
    assert!(err.contains("unknown tool"), "{err}");
}

#[test]
fn invalid_arguments_are_reported() {
    let fixture = fixture();
    let err = with_ctx(&fixture, |ctx| {
        execute("read_file", json!({"path": "a.txt", "mode": "x"}), ctx).unwrap_err()
    });
    assert!(err.contains("invalid arguments"), "{err}");
}
