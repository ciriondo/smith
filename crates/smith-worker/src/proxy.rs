//! The worker's half of the egress proxy.
//!
//! Tool processes see `HTTPS_PROXY=http://127.0.0.1:3128`. This listener accepts a `CONNECT
//! host:port`, asks the host for a tunnel over the channel and copies bytes in both
//! directions. It holds no policy: the host decides whether the target is allowed.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use smith_channel::{Connect, Message};

use crate::protocol::send;

/// Where tools are told the proxy listens.
pub const LISTEN: &str = "127.0.0.1:3128";
const DECISION_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared state between the listener, the tunnels and the main loop.
pub struct Proxy {
    writer: Arc<Mutex<std::os::unix::net::UnixStream>>,
    next_tunnel: AtomicU32,
    /// Tunnels waiting for the host's decision.
    pending: Mutex<HashMap<u32, Sender<Result<(), String>>>>,
    /// Tunnels whose bytes flow from the host to the client.
    tunnels: Mutex<HashMap<u32, Sender<Vec<u8>>>>,
}

impl Proxy {
    pub fn new(writer: Arc<Mutex<std::os::unix::net::UnixStream>>) -> Arc<Self> {
        Arc::new(Self {
            writer,
            next_tunnel: AtomicU32::new(1),
            pending: Mutex::new(HashMap::new()),
            tunnels: Mutex::new(HashMap::new()),
        })
    }

    /// Start the listener. A bind failure is reported over the channel and leaves tools with
    /// a proxy that refuses connections, so every egress attempt fails loudly.
    pub fn start(self: &Arc<Self>) {
        let proxy = self.clone();
        thread::spawn(move || match TcpListener::bind(LISTEN) {
            Ok(listener) => {
                for client in listener.incoming() {
                    match client {
                        Ok(client) => {
                            let proxy = proxy.clone();
                            thread::spawn(move || handle(proxy, client));
                        }
                        Err(_) => break,
                    }
                }
            }
            Err(e) => {
                let _ = send(
                    &proxy.writer,
                    Message::Log(smith_channel::Log {
                        level: smith_channel::LogLevel::Error,
                        message: format!("cannot listen on {LISTEN}: {e}"),
                    }),
                );
            }
        });
    }

    /// The host's decision on a tunnel.
    pub fn decision(&self, tunnel: u32, ok: bool, reason: Option<String>) {
        if let Some(sender) = self.pending.lock().unwrap().remove(&tunnel) {
            let _ = sender.send(if ok {
                Ok(())
            } else {
                Err(reason.unwrap_or_else(|| "refused".to_string()))
            });
        }
    }

    /// Bytes from the host for a tunnel. A missing tunnel is dropped: the client closed.
    pub fn data(&self, tunnel: u32, bytes: Vec<u8>) {
        if let Some(sender) = self.tunnels.lock().unwrap().get(&tunnel) {
            let _ = sender.send(bytes);
        }
    }

    /// The host closed its direction; drain the tunnel.
    pub fn eof(&self, tunnel: u32) {
        self.tunnels.lock().unwrap().remove(&tunnel);
    }
}

fn handle(proxy: Arc<Proxy>, mut client: TcpStream) {
    let reader = match client.try_clone() {
        Ok(reader) => reader,
        Err(_) => return,
    };
    let mut reader = BufReader::new(reader);

    let target = match read_connect(&mut reader) {
        Some(target) => target,
        None => {
            let _ = respond(client, "400 Bad Request");
            return;
        }
    };
    let (host, port) = match target.rsplit_once(':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(port) => (host.to_string(), port),
            Err(_) => {
                let _ = respond(client, "400 Bad Request");
                return;
            }
        },
        None => {
            let _ = respond(client, "400 Bad Request");
            return;
        }
    };

    let tunnel = proxy.next_tunnel.fetch_add(1, Ordering::SeqCst);
    let (decision_tx, decision_rx) = mpsc::channel();
    proxy.pending.lock().unwrap().insert(tunnel, decision_tx);
    if send(
        &proxy.writer,
        Message::Connect(Connect { tunnel, host, port }),
    )
    .is_err()
    {
        return;
    }

    match decision_rx.recv_timeout(DECISION_TIMEOUT) {
        Ok(Ok(())) => {}
        Ok(Err(reason)) => {
            let _ = respond(client, &format!("403 Forbidden: {reason}"));
            return;
        }
        Err(_) => {
            let _ = respond(client, "504 Gateway Timeout");
            return;
        }
    }

    if client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .is_err()
    {
        return;
    }

    let (data_tx, data_rx) = mpsc::channel::<Vec<u8>>();
    proxy.tunnels.lock().unwrap().insert(tunnel, data_tx);

    let mut downstream = match client.try_clone() {
        Ok(stream) => stream,
        Err(_) => return,
    };
    let drain = thread::spawn(move || {
        for bytes in data_rx {
            if downstream.write_all(&bytes).is_err() {
                break;
            }
        }
        let _ = downstream.shutdown(Shutdown::Write);
    });

    copy_to_host(&proxy, tunnel, &mut reader);
    let _ = send(&proxy.writer, Message::Eof { tunnel });
    let _ = drain.join();
    proxy.tunnels.lock().unwrap().remove(&tunnel);
}

/// Read the request line and headers. Only `CONNECT` is understood.
fn read_connect(reader: &mut BufReader<TcpStream>) -> Option<String> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    if parts.next()? != "CONNECT" {
        return None;
    }
    let target = parts.next()?.to_string();

    // Consume the headers so the first tunnelled byte is not mistaken for a header.
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 || header == "\r\n" {
            break;
        }
    }
    Some(target)
}

fn copy_to_host(proxy: &Proxy, tunnel: u32, reader: &mut BufReader<TcpStream>) {
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                if send(
                    &proxy.writer,
                    Message::Data {
                        tunnel,
                        bytes: buffer[..n].to_vec(),
                    },
                )
                .is_err()
                {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn respond(mut client: TcpStream, status: &str) -> std::io::Result<()> {
    client.write_all(format!("HTTP/1.1 {status}\r\n\r\n").as_bytes())?;
    client.flush()
}
