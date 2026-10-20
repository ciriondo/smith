//! The worker's main loop: handshake, frame dispatch and tool execution.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use smith_channel::{
    read_frame, write_frame, Cancel, Hello, Log, LogLevel, Message, ToolCall, ToolResult,
    PROTOCOL_VERSION,
};

use crate::cancel::CancelFlag;
use crate::proxy::Proxy;
use crate::tools::{self, ToolContext};
use crate::workspace::Workspace;

/// Where the image writes the binary inventory.
pub const INVENTORY_PATH: &str = "/etc/smith/inventory.txt";

pub type Error = Box<dyn std::error::Error + Send + Sync>;

struct Running {
    call_id: String,
    cancel: Arc<CancelFlag>,
    handle: JoinHandle<()>,
}

/// Serve a socketpair, which is how the tests drive the worker.
pub fn run(stream: UnixStream) -> Result<(), Error> {
    run_with_inventory(stream, Path::new(INVENTORY_PATH))
}

/// [`run`] with an explicit inventory file, which lets tests avoid the image layout.
pub fn run_with_inventory(stream: UnixStream, inventory_path: &Path) -> Result<(), Error> {
    let writer = stream.try_clone()?;
    serve(stream, writer, inventory_path)
}

/// Send the hello, wait for the workspace and the environment, then answer calls until the
/// host sends `Shutdown` or closes the channel.
fn serve(mut reader: UnixStream, writer: UnixStream, inventory_path: &Path) -> Result<(), Error> {
    let writer = Arc::new(Mutex::new(writer));

    let (inventory, inventory_error) = match read_inventory(inventory_path) {
        Ok(list) => (list, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };
    send(
        &writer,
        Message::Hello(Hello {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
            target: target(),
            inventory,
        }),
    )?;
    if let Some(message) = inventory_error {
        send(
            &writer,
            Message::Log(Log {
                level: LogLevel::Warn,
                message: format!("cannot read {}: {message}", inventory_path.display()),
            }),
        )?;
    }

    let ack = match read_frame(&mut reader)? {
        Some(Message::HelloAck(ack)) => ack,
        Some(other) => return Err(format!("expected hello_ack, got {}", other.kind()).into()),
        None => return Ok(()),
    };
    let workspace = Workspace::new(ack.workspace)?;
    let env = ack.env;
    if let Err(e) = prepare_home(&env) {
        send(
            &writer,
            Message::Log(Log {
                level: LogLevel::Warn,
                message: format!("cannot prepare HOME: {e}"),
            }),
        )?;
    }

    let proxy = Proxy::new(writer.clone());
    proxy.start();

    let mut running: Option<Running> = None;
    loop {
        if let Some(current) = &running {
            if current.handle.is_finished() {
                running = None;
            }
        }

        match read_frame(&mut reader)? {
            None | Some(Message::Shutdown) => {
                if let Some(current) = running.take() {
                    current.cancel.cancel();
                    let _ = current.handle.join();
                }
                return Ok(());
            }
            Some(Message::ToolCall(call)) => {
                if let Some(current) = &running {
                    if current.handle.is_finished() {
                        running = None;
                    }
                }
                if running.is_some() {
                    send(
                        &writer,
                        Message::Log(Log {
                            level: LogLevel::Error,
                            message: format!(
                                "tool call {} arrived before the previous result",
                                call.call_id
                            ),
                        }),
                    )?;
                    return Err("a second tool call arrived before the first result".into());
                }
                running = Some(spawn_tool(call, &writer, &workspace, &env));
            }
            Some(Message::Cancel(Cancel { call_id })) => {
                if let Some(current) = &running {
                    if current.call_id == call_id {
                        current.cancel.cancel();
                    }
                }
            }
            Some(Message::ConnectResult(result)) => {
                proxy.decision(result.tunnel, result.ok, result.reason);
            }
            Some(Message::Data { tunnel, bytes }) => {
                proxy.data(tunnel, bytes);
            }
            Some(Message::Eof { tunnel }) => {
                proxy.eof(tunnel);
            }
            Some(other) => {
                let kind = other.kind();
                send(
                    &writer,
                    Message::Log(Log {
                        level: LogLevel::Error,
                        message: format!("unexpected {kind} frame"),
                    }),
                )?;
                return Err(format!("unexpected {kind} frame").into());
            }
        }
    }
}

fn spawn_tool(
    call: ToolCall,
    writer: &Arc<Mutex<UnixStream>>,
    workspace: &Workspace,
    env: &BTreeMap<String, String>,
) -> Running {
    let ToolCall {
        call_id,
        name,
        args,
    } = call;
    let cancel = Arc::new(CancelFlag::new());
    let workspace = workspace.clone();
    let env = env.clone();
    let flag = cancel.clone();
    let out = writer.clone();
    let id = call_id.clone();

    let handle = thread::spawn(move || {
        let ctx = ToolContext {
            workspace: &workspace,
            env: &env,
            cancel: &flag,
        };
        let message = match tools::execute(&name, args, &ctx) {
            Ok(content) => ToolResult {
                call_id: id,
                ok: true,
                content,
            },
            Err(content) => ToolResult {
                call_id: id,
                ok: false,
                content,
            },
        };
        // A failed send means the host is gone. The main loop sees the closed stream on its
        // next read.
        let _ = send(&out, Message::ToolResult(message));
    });

    Running {
        call_id,
        cancel,
        handle,
    }
}

pub(crate) fn send(writer: &Mutex<UnixStream>, message: Message) -> Result<(), Error> {
    let mut guard = writer
        .lock()
        .map_err(|_| io::Error::other("the channel writer lock is poisoned"))?;
    write_frame(&mut *guard, &message)?;
    Ok(())
}

/// Read the image inventory: one binary name per line, blank lines and `#` comments ignored.
pub fn read_inventory(path: &Path) -> io::Result<Vec<String>> {
    let text = fs::read_to_string(path)?;
    Ok(parse_inventory(&text))
}

fn parse_inventory(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

fn prepare_home(env: &BTreeMap<String, String>) -> io::Result<()> {
    if let Some(home) = env.get("HOME") {
        fs::create_dir_all(home)
            .map_err(|e| io::Error::new(e.kind(), format!("cannot create HOME={home}: {e}")))?;
    }
    Ok(())
}

/// The target the worker was built for, in `arch-os` form.
pub fn target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_skips_comments_and_blank_lines() {
        let text = "# provided by the image\nbash\n\n  git  \n# cargo\nrustc\n";
        assert_eq!(
            parse_inventory(text),
            vec!["bash".to_string(), "git".to_string(), "rustc".to_string()]
        );
    }

    #[test]
    fn target_names_the_architecture_and_os() {
        let target = target();
        assert!(target.contains('-'), "{target}");
    }
}
