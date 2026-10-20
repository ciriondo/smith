use serde_json::json;
use smith_channel::{
    EditFileArgs, ReadFileArgs, ReadFileResult, RunShellArgs, RunShellResult, WriteFileArgs,
};

#[test]
fn read_file_accepts_a_range() {
    let args: ReadFileArgs = serde_json::from_value(json!({
        "path": "src/main.rs",
        "start_line": 10,
        "end_line": 20
    }))
    .unwrap();
    assert_eq!(args.path, "src/main.rs");
    assert_eq!(args.start_line, Some(10));
    assert_eq!(args.end_line, Some(20));
}

#[test]
fn read_file_defaults_to_the_whole_file() {
    let args: ReadFileArgs = serde_json::from_value(json!({"path": "a.txt"})).unwrap();
    assert_eq!(args.start_line, None);
    assert_eq!(args.end_line, None);
}

#[test]
fn unknown_arguments_are_rejected() {
    let err = serde_json::from_value::<ReadFileArgs>(json!({
        "path": "a.txt",
        "mode": "binary"
    }))
    .unwrap_err();
    assert!(err.to_string().contains("unknown field"), "{err}");
}

#[test]
fn a_missing_required_argument_is_rejected() {
    let err = serde_json::from_value::<WriteFileArgs>(json!({"path": "a.txt"})).unwrap_err();
    assert!(err.to_string().contains("content"), "{err}");
}

#[test]
fn edit_file_defaults_replace_all_to_false() {
    let args: EditFileArgs = serde_json::from_value(json!({
        "path": "a.txt",
        "old_string": "a",
        "new_string": "b"
    }))
    .unwrap();
    assert!(!args.replace_all);

    let args: EditFileArgs = serde_json::from_value(json!({
        "path": "a.txt",
        "old_string": "a",
        "new_string": "b",
        "replace_all": true
    }))
    .unwrap();
    assert!(args.replace_all);
}

#[test]
fn run_shell_defaults_to_the_workspace_root_and_no_timeout() {
    let args: RunShellArgs = serde_json::from_value(json!({"command": "cargo test"})).unwrap();
    assert_eq!(args.command, "cargo test");
    assert_eq!(args.cwd, None);
    assert_eq!(args.timeout_secs, None);
}

#[test]
fn run_shell_accepts_a_cwd_and_a_timeout() {
    let args: RunShellArgs = serde_json::from_value(json!({
        "command": "cargo test",
        "cwd": "crates/smith-core",
        "timeout_secs": 300
    }))
    .unwrap();
    assert_eq!(args.cwd.as_deref(), Some("crates/smith-core"));
    assert_eq!(args.timeout_secs, Some(300));
}

#[test]
fn results_round_trip_through_json() {
    let read = ReadFileResult {
        path: "a.txt".to_string(),
        start_line: 1,
        end_line: 2,
        total_lines: 2,
        truncated: false,
        content: "one\ntwo\n".to_string(),
    };
    let value = serde_json::to_value(&read).unwrap();
    assert_eq!(value["total_lines"], 2);
    assert_eq!(
        serde_json::from_value::<ReadFileResult>(value).unwrap(),
        read
    );

    let shell = RunShellResult {
        exit_code: None,
        stdout: String::new(),
        stderr: "killed".to_string(),
        truncated: false,
        timed_out: true,
        cancelled: false,
    };
    let value = serde_json::to_value(&shell).unwrap();
    assert!(value["exit_code"].is_null());
    assert_eq!(
        serde_json::from_value::<RunShellResult>(value).unwrap(),
        shell
    );
}
