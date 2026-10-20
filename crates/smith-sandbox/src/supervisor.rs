//! The supervisor: the private store, the image and one container per session.
//!
//! The channel is a socketpair. One end is mapped to descriptor 3 for the podman client by
//! `command-fds`, which applies the mapping between fork and exec, so smith's own descriptor
//! table is never touched. The other end is driven by two blocking threads; the async side
//! exchanges [`Message`]s and oneshot replies.

use std::collections::{BTreeMap, HashMap};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use command_fds::{CommandFdExt, FdMapping};
use smith_channel::{
    read_frame, write_frame, Connect, ConnectResult, Hello, HelloAck, Log, LogLevel, Message,
    ToolCall, ToolResult, PROTOCOL_VERSION,
};
use smith_core::config::NetworkEntry;
use smith_core::ToolError;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::command::{self, ContainerSpec};
use crate::error::SandboxError;
use crate::image;

/// Where the image places the worker.
pub const WORKER_PATH: &str = "/usr/local/bin/smith-worker";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<String, String>>>>>;
type Tunnels = Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>;

/// Everything one session needs from the supervisor.
pub struct SandboxConfig {
    pub workspace: PathBuf,
    pub session: String,
    pub allow: Vec<NetworkEntry>,
    pub ro_mounts: Vec<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub idle_timeout: Duration,
}

/// Owns the store, the image and the caches.
pub struct Supervisor {
    pub(crate) store: PathBuf,
    pub(crate) runroot: PathBuf,
    pub(crate) cache: PathBuf,
    pub(crate) podman: String,
}

impl Supervisor {
    /// Resolve the store, the runroot, the caches and the worker binary. Directories are
    /// created; nothing is started.
    pub fn new() -> Result<Self, SandboxError> {
        let data = data_home();
        let store = data.join("smith/containers/storage");
        let runroot = runtime_home().join("smith/containers");
        let cache = data.join("smith/cache");
        for dir in [
            &store,
            &runroot,
            &cache.join("cargo"),
            &cache.join("npm"),
            &cache.join("pip"),
        ] {
            std::fs::create_dir_all(dir)?;
        }

        Ok(Self {
            store,
            runroot,
            cache,
            podman: "podman".to_string(),
        })
    }

    pub(crate) fn global_args(&self) -> Vec<String> {
        command::global_args(&self.store, &self.runroot)
    }

    /// Start one container for `config` and complete the handshake.
    pub async fn start(&self, config: &SandboxConfig) -> Result<Arc<Sandbox>, SandboxError> {
        let worker = worker_binary()?;
        let tag = image::tag_for(&image::digest(&worker)?);
        image::ensure(self, &worker, &tag).await?;

        let (host, worker) = UnixStream::pair()?;
        host.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;

        let directory = tempfile::tempdir()?;
        let cidfile = directory.path().join("cid");
        let spec = ContainerSpec {
            workspace: &config.workspace,
            session: &config.session,
            cidfile: &cidfile,
            ro_mounts: &config.ro_mounts,
            image: &tag,
            worker: Path::new(WORKER_PATH),
            version: env!("CARGO_PKG_VERSION"),
        };

        let mut command = Command::new(&self.podman);
        command.args(self.global_args());
        command.args(command::run_args(&spec, &self.cache));
        command.fd_mappings(vec![FdMapping {
            parent_fd: worker.into(),
            child_fd: 3,
        }])?;
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());

        let output = command
            .output()
            .await
            .map_err(|source| SandboxError::Spawn {
                command: "podman run".to_string(),
                source,
            })?;
        // smith's copy of the worker end closes here; the child has its own.
        drop(command);

        if !output.status.success() {
            return Err(SandboxError::Podman(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let cid = std::fs::read_to_string(&cidfile)?.trim().to_string();
        if cid.is_empty() {
            let _ = self.remove(&cid).await;
            return Err(SandboxError::Channel(
                "podman did not write the container id".to_string(),
            ));
        }

        let workspace = config.workspace.display().to_string();
        let env = config.env.clone();
        let handshake =
            tokio::task::spawn_blocking(move || -> Result<(UnixStream, Hello), SandboxError> {
                let mut host = host;
                let hello = match read_frame(&mut host)? {
                    Some(Message::Hello(hello)) => hello,
                    Some(other) => {
                        return Err(SandboxError::Channel(format!(
                            "expected hello, received {}",
                            other.kind()
                        )))
                    }
                    None => {
                        return Err(SandboxError::Channel(
                            "the worker closed the channel before hello".to_string(),
                        ))
                    }
                };
                if hello.protocol != PROTOCOL_VERSION {
                    return Err(SandboxError::Protocol {
                        found: hello.protocol,
                        wanted: PROTOCOL_VERSION,
                    });
                }
                if hello.version != env!("CARGO_PKG_VERSION") {
                    return Err(SandboxError::VersionMismatch {
                        found: hello.version,
                        wanted: env!("CARGO_PKG_VERSION").to_string(),
                    });
                }
                write_frame(&mut host, &Message::HelloAck(HelloAck { workspace, env }))?;
                host.set_read_timeout(None)?;
                Ok((host, hello))
            })
            .await
            .map_err(|e| SandboxError::Channel(e.to_string()))?;

        let (host, hello) = match handshake {
            Ok(pair) => pair,
            Err(e) => {
                let _ = self.remove(&cid).await;
                return Err(e);
            }
        };

        let (writer, writer_rx) = mpsc::channel::<Message>(64);
        let writer_for_reader = writer.clone();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let tunnels: Tunnels = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let allow = Arc::new(config.allow.clone());

        let write_half = host.try_clone()?;
        thread::spawn(move || write_loop(write_half, writer_rx));
        thread::spawn({
            let pending = pending.clone();
            let tunnels = tunnels.clone();
            let closed = closed.clone();
            move || read_loop(host, writer_for_reader, pending, tunnels, closed, allow)
        });

        Ok(Arc::new(Sandbox {
            cid,
            inventory: hello.inventory,
            writer,
            pending,
            closed,
            supervisor: self.global_args(),
        }))
    }

    /// Remove a container by id. Used for cleanup and by `smith sandbox rm`.
    pub async fn remove(&self, cid: &str) -> Result<(), SandboxError> {
        let output = Command::new(&self.podman)
            .args(self.global_args())
            .args(["rm", "-f", "--ignore", cid])
            .output()
            .await
            .map_err(|source| SandboxError::Spawn {
                command: "podman rm".to_string(),
                source,
            })?;
        if !output.status.success() {
            return Err(SandboxError::Podman(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        Ok(())
    }

    /// Stop a running container by id or name.
    pub async fn stop(&self, cid: &str) -> Result<(), SandboxError> {
        let output = Command::new(&self.podman)
            .args(self.global_args())
            .args(["stop", cid])
            .output()
            .await
            .map_err(|source| SandboxError::Spawn {
                command: "podman stop".to_string(),
                source,
            })?;
        if !output.status.success() {
            return Err(SandboxError::Podman(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        Ok(())
    }

    /// Containers left in smith's store, one line each.
    pub async fn list(&self) -> Result<String, SandboxError> {
        let output = Command::new(&self.podman)
            .args(self.global_args())
            .args([
                "ps",
                "-a",
                "--filter",
                "label=smith.managed=true",
                "--format",
                "{{.ID}} {{.Names}} {{.Status}}",
            ])
            .output()
            .await
            .map_err(|source| SandboxError::Spawn {
                command: "podman ps".to_string(),
                source,
            })?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// The store path, for `smith sandbox ps`.
    pub fn store(&self) -> &Path {
        &self.store
    }
}

/// One running container and its channel.
pub struct Sandbox {
    cid: String,
    inventory: Vec<String>,
    writer: mpsc::Sender<Message>,
    pending: Pending,
    closed: Arc<AtomicBool>,
    supervisor: Vec<String>,
}

impl Sandbox {
    pub fn cid(&self) -> &str {
        &self.cid
    }

    pub fn inventory(&self) -> &[String] {
        &self.inventory
    }

    /// One tool call. A cancel sends `Cancel` and waits up to five seconds for the worker to
    /// kill the process group and answer.
    pub async fn call(
        &self,
        call_id: String,
        name: String,
        args: serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<String, ToolError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ToolError::SandboxGone);
        }

        let (sender, mut receiver) = oneshot::channel();
        self.pending.lock().unwrap().insert(call_id.clone(), sender);

        if self
            .writer
            .send(Message::ToolCall(ToolCall {
                call_id: call_id.clone(),
                name,
                args,
            }))
            .await
            .is_err()
        {
            return Err(ToolError::SandboxGone);
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let _ = self.writer.send(Message::Cancel(smith_channel::Cancel { call_id })).await;
                let _ = tokio::time::timeout(Duration::from_secs(5), &mut receiver).await;
                Err(ToolError::Message("cancelled".to_string()))
            }
            result = &mut receiver => match result {
                Ok(Ok(content)) => Ok(content),
                Ok(Err(message)) => Err(ToolError::Message(message)),
                Err(_) => Err(ToolError::SandboxGone),
            },
        }
    }

    /// Ask the worker to exit and remove the container.
    pub async fn stop(&self) {
        let _ = self.writer.try_send(Message::Shutdown);
        self.closed.store(true, Ordering::SeqCst);
        let _ = Command::new("podman")
            .args(&self.supervisor)
            .args(["rm", "-f", "--ignore", &self.cid])
            .output()
            .await;
    }
}

fn write_loop(mut writer: UnixStream, mut rx: mpsc::Receiver<Message>) {
    while let Some(message) = rx.blocking_recv() {
        if write_frame(&mut writer, &message).is_err() {
            break;
        }
    }
}

fn read_loop(
    mut reader: UnixStream,
    out: mpsc::Sender<Message>,
    pending: Pending,
    tunnels: Tunnels,
    closed: Arc<AtomicBool>,
    allow: Arc<Vec<NetworkEntry>>,
) {
    loop {
        match read_frame(&mut reader) {
            Ok(Some(Message::ToolResult(ToolResult {
                call_id,
                ok,
                content,
            }))) => {
                if let Some(sender) = pending.lock().unwrap().remove(&call_id) {
                    let _ = sender.send(if ok { Ok(content) } else { Err(content) });
                }
            }
            Ok(Some(Message::Connect(Connect { tunnel, host, port }))) => {
                let out = out.clone();
                let tunnels = tunnels.clone();
                let allow = allow.clone();
                thread::spawn(move || dial(tunnel, host, port, allow, out, tunnels));
            }
            Ok(Some(Message::Data { tunnel, bytes })) => {
                let sender = tunnels.lock().unwrap().get(&tunnel).cloned();
                if let Some(sender) = sender {
                    let _ = sender.blocking_send(bytes);
                }
            }
            Ok(Some(Message::Eof { tunnel })) => {
                tunnels.lock().unwrap().remove(&tunnel);
            }
            Ok(Some(Message::Log(Log { level, message }))) => {
                let level = match level {
                    LogLevel::Debug => "debug",
                    LogLevel::Info => "info",
                    LogLevel::Warn => "warn",
                    LogLevel::Error => "error",
                };
                eprintln!("smith: sandbox: {level}: {message}");
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }

    closed.store(true, Ordering::SeqCst);
    // Dropping the senders makes every waiting call observe a closed channel, which the tool
    // maps to `SandboxGone`.
    pending.lock().unwrap().clear();
    tunnels.lock().unwrap().clear();
}

fn dial(
    tunnel: u32,
    host: String,
    port: u16,
    allow: Arc<Vec<NetworkEntry>>,
    out: mpsc::Sender<Message>,
    tunnels: Tunnels,
) {
    let mut failure = None;
    let mut stream = None;

    if !allow.iter().any(|entry| entry.matches(&host, port)) {
        failure = Some(format!("{host}:{port} is not in the sandbox allowlist"));
    } else {
        match (host.as_str(), port).to_socket_addrs() {
            Ok(addrs) => {
                let addresses: Vec<_> = addrs.collect();
                if addresses.is_empty() {
                    failure = Some(format!("{host} does not resolve"));
                } else if addresses.iter().any(|addr| addr.ip().is_loopback()) {
                    // smith itself, and every other local service, is on loopback.
                    failure = Some(format!("{host} resolves to a loopback address"));
                } else {
                    let mut last = None;
                    for address in addresses {
                        match TcpStream::connect_timeout(&address, DIAL_TIMEOUT) {
                            Ok(connected) => {
                                stream = Some(connected);
                                break;
                            }
                            Err(e) => last = Some(e),
                        }
                    }
                    if stream.is_none() {
                        failure = Some(match last {
                            Some(e) => format!("cannot connect to {host}:{port}: {e}"),
                            None => format!("cannot connect to {host}:{port}"),
                        });
                    }
                }
            }
            Err(e) => failure = Some(format!("cannot resolve {host}: {e}")),
        }
    }

    let Some(mut stream) = stream else {
        let _ = out.blocking_send(Message::ConnectResult(ConnectResult {
            tunnel,
            ok: false,
            reason: failure,
        }));
        return;
    };

    if out
        .blocking_send(Message::ConnectResult(ConnectResult {
            tunnel,
            ok: true,
            reason: None,
        }))
        .is_err()
    {
        return;
    }

    let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(64);
    tunnels.lock().unwrap().insert(tunnel, sender);

    match stream.try_clone() {
        Ok(mut reader) => {
            let out = out.clone();
            thread::spawn(move || {
                let mut buffer = [0u8; 8192];
                loop {
                    match std::io::Read::read(&mut reader, &mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            if out
                                .blocking_send(Message::Data {
                                    tunnel,
                                    bytes: buffer[..n].to_vec(),
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                let _ = out.blocking_send(Message::Eof { tunnel });
            });
        }
        Err(e) => {
            let _ = out.blocking_send(Message::Eof { tunnel });
            eprintln!("smith: sandbox: cannot split a tunnel stream: {e}");
            return;
        }
    }

    while let Some(bytes) = receiver.blocking_recv() {
        if std::io::Write::write_all(&mut stream, &bytes).is_err() {
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Write);
}

fn data_home() -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(path);
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
}

fn runtime_home() -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(path);
    }
    let uid = nix::unistd::getuid();
    PathBuf::from(format!("/tmp/smith-runroot/{uid}"))
}

pub(crate) fn worker_binary() -> Result<PathBuf, SandboxError> {
    if let Some(path) = std::env::var_os("SMITH_WORKER_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(SandboxError::Config(format!(
            "SMITH_WORKER_BIN is set to {}, which is not a file",
            path.display()
        )));
    }

    let executable = std::env::current_exe()?;
    let candidate = executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("smith-worker");
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(SandboxError::Config(format!(
            "cannot find the worker binary at {}; run `cargo build -p smith-worker` or set \
             SMITH_WORKER_BIN",
            candidate.display()
        )))
    }
}
