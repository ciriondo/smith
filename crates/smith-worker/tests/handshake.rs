use std::collections::BTreeMap;
use std::fs;
use std::os::unix::net::UnixStream;
use std::thread::{self, JoinHandle};

use serde_json::json;
use smith_channel::{
    read_frame, write_frame, HelloAck, Message, ToolCall, ToolResult, PROTOCOL_VERSION,
};
use smith_worker::run_with_inventory;

fn next_result(stream: &mut UnixStream) -> Option<ToolResult> {
    loop {
        match read_frame(stream).unwrap() {
            Some(Message::ToolResult(result)) => return Some(result),
            Some(Message::Log(_)) => continue,
            Some(other) => panic!("unexpected {} frame", other.kind()),
            None => return None,
        }
    }
}

#[test]
fn handshake_reports_the_inventory_and_runs_tools() {
    let dir = tempfile::tempdir().unwrap();
    let inventory = dir.path().join("inventory.txt");
    fs::write(&inventory, "# image\nbash\ngit\n").unwrap();
    fs::write(dir.path().join("hello.txt"), "hi\n").unwrap();

    let (server, mut client) = UnixStream::pair().unwrap();
    let mut worker: Option<JoinHandle<Result<(), smith_worker::protocol::Error>>> =
        Some(thread::spawn(move || {
            run_with_inventory(server, &inventory)
        }));

    let hello = match read_frame(&mut client).unwrap() {
        Some(Message::Hello(hello)) => hello,
        other => panic!("expected hello, got {other:?}"),
    };
    assert_eq!(hello.protocol, PROTOCOL_VERSION);
    assert_eq!(hello.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(hello.inventory, vec!["bash".to_string(), "git".to_string()]);

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
    write_frame(
        &mut client,
        &Message::HelloAck(HelloAck {
            workspace: dir.path().display().to_string(),
            env: env.clone(),
        }),
    )
    .unwrap();

    write_frame(
        &mut client,
        &Message::ToolCall(ToolCall {
            call_id: "c1".to_string(),
            name: "read_file".to_string(),
            args: json!({"path": "hello.txt"}),
        }),
    )
    .unwrap();

    let result = match next_result(&mut client) {
        Some(result) => result,
        None => panic!(
            "worker closed the channel: {:?}",
            worker.take().unwrap().join()
        ),
    };
    assert!(result.ok, "{}", result.content);
    let value: serde_json::Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(value["content"], "hi\n");
    assert_eq!(value["total_lines"], 1);

    write_frame(
        &mut client,
        &Message::ToolCall(ToolCall {
            call_id: "c2".to_string(),
            name: "run_shell".to_string(),
            args: json!({"command": "printf %s \"$HOME\""}),
        }),
    )
    .unwrap();

    let result = match next_result(&mut client) {
        Some(result) => result,
        None => panic!(
            "worker closed the channel: {:?}",
            worker.take().unwrap().join()
        ),
    };
    assert!(result.ok, "{}", result.content);
    let value: serde_json::Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(value["stdout"], env["HOME"]);
    assert_eq!(value["exit_code"], 0);

    write_frame(&mut client, &Message::Shutdown).unwrap();
    worker.take().unwrap().join().unwrap().unwrap();
}
