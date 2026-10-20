use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::process::{Command, Output, Stdio};

use command_fds::{CommandFdExt, FdMapping};

use smith_channel::{read_frame, write_frame, HelloAck, Message, PROTOCOL_VERSION};

const WORKER: &str = env!("CARGO_BIN_EXE_smith-worker");

fn run_shell(script: &str) -> Output {
    Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .arg(WORKER)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn a_closed_descriptor_is_an_error() {
    // Opening and closing the descriptor first keeps the shell's `exec` from failing on a
    // descriptor that was already closed.
    let output = run_shell("exec 3</dev/null; exec 3<&-; exec \"$@\"");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("descriptor 3"), "{stderr}");
}

#[test]
fn a_descriptor_that_is_not_a_socket_is_an_error() {
    let output = run_shell("exec 3</dev/null; exec \"$@\"");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not a socket"), "{stderr}");
}

#[test]
fn a_socket_on_the_descriptor_starts_the_handshake() {
    let dir = tempfile::tempdir().unwrap();
    let (server, mut client) = UnixStream::pair().unwrap();

    let mut command = Command::new(WORKER);
    command
        .fd_mappings(vec![FdMapping {
            parent_fd: server.into(),
            child_fd: 3,
        }])
        .unwrap()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();

    let hello = match read_frame(&mut client).unwrap() {
        Some(Message::Hello(hello)) => hello,
        other => {
            let mut stderr = child.stderr.take().unwrap();
            let mut text = String::new();
            stderr.read_to_string(&mut text).unwrap();
            panic!("expected hello, got {other:?}; stderr: {text}");
        }
    };
    assert_eq!(hello.protocol, PROTOCOL_VERSION);

    let env = BTreeMap::from([(
        "HOME".to_string(),
        dir.path().join("home").display().to_string(),
    )]);
    write_frame(
        &mut client,
        &Message::HelloAck(HelloAck {
            workspace: dir.path().display().to_string(),
            env,
        }),
    )
    .unwrap();
    write_frame(&mut client, &Message::Shutdown).unwrap();

    let mut stderr = child.stderr.take().unwrap();
    let status = child.wait().unwrap();
    let mut text = String::new();
    stderr.read_to_string(&mut text).unwrap();
    assert!(status.success(), "stderr: {text}");
}
