//! The worker's provider listener.
//!
//! A sandboxed `smith` child has no network and no provider key. Its provider configuration
//! points at `http://127.0.0.1:3129/v1` and its key is the per-container token. This listener
//! turns one `POST /v1/chat/completions` into a [`ProviderRequest`] frame and streams the
//! host's answer back as HTTP. It holds no credential and never dials a provider.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use smith_channel::{Message, ProviderCancel, ProviderRequest};

use crate::protocol::send;

/// Where the child's provider configuration points.
pub const LISTEN: &str = "127.0.0.1:3129";

/// The environment variable holding the per-container request token.
pub const TOKEN_ENV: &str = "SMITH_SANDBOX_TOKEN";

/// The largest request body the listener accepts. The channel frame limit covers the body
/// plus the eight-byte request id, so a larger conversation is refused with 413 rather than
/// failing when the frame is written.
pub const MAX_BODY: usize = smith_channel::MAX_PAYLOAD - 8;

/// How long a client may take to send its headers.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// What the main loop delivers to the client thread waiting on one request.
#[derive(Debug)]
pub enum Event {
    Head {
        status: u16,
        content_type: Option<String>,
    },
    Data(Vec<u8>),
    End,
    Failure(String),
    /// The child closed its connection before the response finished.
    Disconnect,
}

/// The listener and the per-request senders the main loop routes frames to.
pub struct Provider {
    writer: Arc<Mutex<std::os::unix::net::UnixStream>>,
    token: String,
    next_id: AtomicU64,
    requests: Mutex<HashMap<u64, Sender<Event>>>,
}

impl Provider {
    /// Read the token from `/dev/urandom`. It is per container and never written to disk.
    pub fn new(writer: Arc<Mutex<std::os::unix::net::UnixStream>>) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            writer,
            token: generate_token()?,
            next_id: AtomicU64::new(1),
            requests: Mutex::new(HashMap::new()),
        }))
    }

    /// The token the child sends as its bearer credential.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Start the listener on [`LISTEN`]. A bind failure is reported over the channel and
    /// leaves the child with a refused connection.
    pub fn start(self: &Arc<Self>) {
        let provider = self.clone();
        thread::spawn(move || match TcpListener::bind(LISTEN) {
            Ok(listener) => provider.listen(listener),
            Err(e) => {
                let _ = send(
                    &provider.writer,
                    Message::Log(smith_channel::Log {
                        level: smith_channel::LogLevel::Error,
                        message: format!("cannot listen on {LISTEN}: {e}"),
                    }),
                );
            }
        });
    }

    /// Accept on `listener` until it fails. Tests bind port 0 and call this directly.
    pub fn listen(self: &Arc<Self>, listener: TcpListener) {
        for client in listener.incoming() {
            match client {
                Ok(client) => {
                    let provider = self.clone();
                    thread::spawn(move || provider.handle(client));
                }
                Err(_) => break,
            }
        }
    }

    /// The host's response head.
    pub fn head(&self, id: u64, status: u16, content_type: Option<String>) {
        self.deliver(
            id,
            Event::Head {
                status,
                content_type,
            },
        );
    }

    /// One chunk of the response body.
    pub fn data(&self, id: u64, bytes: Vec<u8>) {
        self.deliver(id, Event::Data(bytes));
    }

    /// The response is complete.
    pub fn end(&self, id: u64) {
        if let Some(sender) = self.requests.lock().unwrap().remove(&id) {
            let _ = sender.send(Event::End);
        }
    }

    /// The host could not produce a response.
    pub fn failure(&self, id: u64, message: String) {
        if let Some(sender) = self.requests.lock().unwrap().remove(&id) {
            let _ = sender.send(Event::Failure(message));
        }
    }

    fn deliver(&self, id: u64, event: Event) {
        let sender = self.requests.lock().unwrap().get(&id).cloned();
        if let Some(sender) = sender {
            let _ = sender.send(event);
        }
    }

    fn handle(self: Arc<Self>, client: TcpStream) {
        let mut reader = match client.try_clone() {
            Ok(stream) => BufReader::new(stream),
            Err(_) => return,
        };

        let head = match read_head(&mut reader) {
            Ok(head) => head,
            Err(_) => {
                let _ = respond(&client, 400, "text/plain", b"bad request\n");
                return;
            }
        };
        let Some(length) = head.content_length else {
            let _ = respond(&client, 411, "text/plain", b"content-length is required\n");
            return;
        };
        if length > MAX_BODY {
            let _ = respond(&client, 413, "text/plain", b"request body too large\n");
            return;
        }

        // The body is read before the body-independent checks, so a refusal does not leave
        // bytes in the socket and turn the close into a reset.
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).is_err() {
            let _ = respond(&client, 400, "text/plain", b"bad request\n");
            return;
        }
        let _ = reader.get_ref().set_read_timeout(None);

        if head.method != "POST" || head.path != "/v1/chat/completions" {
            let _ = respond(&client, 404, "text/plain", b"not found\n");
            return;
        }
        if head
            .authorization
            .as_deref()
            .and_then(|value| value.strip_prefix("Bearer "))
            != Some(self.token.as_str())
        {
            let _ = respond(&client, 401, "text/plain", b"unauthorized\n");
            return;
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel();
        self.requests.lock().unwrap().insert(id, sender.clone());
        self.watch_hangup(&client, sender);
        if send(
            &self.writer,
            Message::ProviderRequest(ProviderRequest { id, body }),
        )
        .is_err()
        {
            self.requests.lock().unwrap().remove(&id);
            let _ = respond(&client, 502, "text/plain", b"the host channel is closed\n");
            // The watcher holds a clone of the socket and waits for a close.
            let _ = client.shutdown(Shutdown::Both);
            return;
        }

        if let Err(e) = self.stream(&client, receiver) {
            // The child stopped reading before the end. The host request has no reader left.
            let _ = send(&self.writer, Message::ProviderCancel(ProviderCancel { id }));
            eprintln!("smith-worker: provider request {id} ended early: {e}");
        }
        self.requests.lock().unwrap().remove(&id);
        // Releases the watcher thread and lets the child see the end of the body.
        let _ = client.shutdown(Shutdown::Both);
    }

    /// Report a hang-up on a channel of its own. The child sends no more bytes after its
    /// request, so a read returning zero means it closed.
    fn watch_hangup(&self, client: &TcpStream, events: Sender<Event>) {
        let Ok(mut socket) = client.try_clone() else {
            let _ = events.send(Event::Disconnect);
            return;
        };
        thread::spawn(move || {
            let mut byte = [0u8; 1];
            loop {
                match socket.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = events.send(Event::Disconnect);
        });
    }

    /// Write the response head after the first event, then body chunks as they arrive. The
    /// body has no length; `Connection: close` marks its end.
    fn stream(&self, client: &TcpStream, receiver: mpsc::Receiver<Event>) -> io::Result<()> {
        // The head is written lazily: a failure before it becomes a 502, which the child's
        // adapter reports as a request error rather than a truncated stream.
        let mut out: Option<TcpStream> = None;
        while let Ok(event) = receiver.recv() {
            match event {
                Event::Head {
                    status,
                    content_type,
                } => {
                    let mut stream = client.try_clone()?;
                    let content_type =
                        content_type.unwrap_or_else(|| "application/octet-stream".to_string());
                    write!(
                        stream,
                        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\n\
                         Connection: close\r\n\r\n",
                        reason(status)
                    )?;
                    stream.flush()?;
                    out = Some(stream);
                }
                Event::Data(bytes) => {
                    let Some(stream) = out.as_mut() else {
                        return Err(io::Error::other("the host sent data before the head"));
                    };
                    stream.write_all(&bytes)?;
                    stream.flush()?;
                }
                Event::End => {
                    if let Some(mut stream) = out.take() {
                        stream.flush()?;
                    }
                    return Ok(());
                }
                Event::Failure(message) => {
                    return match out {
                        None => respond(client, 502, "text/plain", message.as_bytes()),
                        // The status line is already on the wire. Closing without the end of
                        // the body is how the child's stream parser sees a truncation.
                        Some(_) => Ok(()),
                    };
                }
                Event::Disconnect => return Err(io::Error::other("the child disconnected")),
            }
        }
        Err(io::Error::other("the host channel closed"))
    }
}

struct Head {
    method: String,
    path: String,
    authorization: Option<String>,
    content_length: Option<usize>,
}

/// Read the request line and headers, leaving the body in the reader's buffer.
fn read_head(reader: &mut BufReader<TcpStream>) -> io::Result<Head> {
    reader.get_ref().set_read_timeout(Some(HEAD_TIMEOUT))?;

    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let version = parts.next().unwrap_or_default();
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(io::Error::other("not an HTTP request"));
    }

    let mut authorization = None;
    let mut content_length = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header == "\r\n" || header == "\n" {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "authorization" => authorization = Some(value.trim().to_string()),
            "content-length" => content_length = value.trim().parse().ok(),
            _ => {}
        }
    }
    reader.get_ref().set_read_timeout(None)?;
    Ok(Head {
        method,
        path,
        authorization,
        content_length,
    })
}

fn respond(client: &TcpStream, status: u16, content_type: &str, body: &[u8]) -> io::Result<()> {
    let mut stream = client.try_clone()?;
    write!(
        stream,
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        reason(status),
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        411 => "Length Required",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

fn generate_token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::os::unix::net::UnixStream;

    /// A listener on an ephemeral port. The returned stream is the host end of the channel.
    fn start() -> (SocketAddr, Arc<Provider>, UnixStream) {
        let (server, client) = UnixStream::pair().unwrap();
        let provider = Provider::new(Arc::new(Mutex::new(server))).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let service = provider.clone();
        thread::spawn(move || service.listen(listener));
        (address, provider, client)
    }

    fn request(
        address: SocketAddr,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &str,
    ) -> String {
        let mut stream = TcpStream::connect(address).unwrap();
        let bearer = token
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{bearer}\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn next_frame(client: &mut UnixStream) -> Message {
        loop {
            match smith_channel::read_frame(client).unwrap().unwrap() {
                Message::Log(_) => continue,
                message => return message,
            }
        }
    }

    #[test]
    fn an_unknown_path_and_a_wrong_token_are_refused() {
        let (address, provider, _client) = start();
        let response = request(address, "GET", "/v1/models", Some(provider.token()), "");
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");

        let response = request(address, "POST", "/v1/chat/completions", Some("wrong"), "{}");
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    }

    #[test]
    fn a_request_becomes_a_frame_and_the_response_streams_back() {
        let (address, provider, mut client) = start();
        let token = provider.token().to_string();

        let join = thread::spawn(move || {
            request(
                address,
                "POST",
                "/v1/chat/completions",
                Some(&token),
                r#"{"model":"child"}"#,
            )
        });

        let id = match next_frame(&mut client) {
            Message::ProviderRequest(request) => {
                assert_eq!(request.body, br#"{"model":"child"}"#);
                request.id
            }
            other => panic!("expected a provider request, got {}", other.kind()),
        };

        provider.head(id, 200, Some("text/event-stream".to_string()));
        provider.data(id, b"data: one\n\n".to_vec());
        provider.data(id, b"data: two\n\n".to_vec());
        provider.end(id);

        let response = join.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            response.contains("Content-Type: text/event-stream"),
            "{response}"
        );
        assert!(
            response.ends_with("data: one\n\ndata: two\n\n"),
            "{response}"
        );
    }

    #[test]
    fn a_host_failure_before_the_head_is_a_502() {
        let (address, provider, mut client) = start();
        let token = provider.token().to_string();

        let join = thread::spawn(move || {
            request(address, "POST", "/v1/chat/completions", Some(&token), "{}")
        });

        let id = match next_frame(&mut client) {
            Message::ProviderRequest(request) => request.id,
            other => panic!("expected a provider request, got {}", other.kind()),
        };
        provider.failure(id, "no proxy configured".to_string());

        let response = join.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 502"), "{response}");
        assert!(response.ends_with("no proxy configured"), "{response}");
    }

    #[test]
    fn a_body_over_the_limit_is_refused_without_reading_it() {
        let (address, provider, _client) = start();
        let mut stream = TcpStream::connect(address).unwrap();
        write!(
            stream,
            "POST /v1/chat/completions HTTP/1.1\r\nAuthorization: Bearer {}\r\n\
             Content-Length: {}\r\n\r\n",
            provider.token(),
            MAX_BODY + 1
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 413"), "{response}");
    }

    #[test]
    fn a_client_hang_up_cancels_the_host_request() {
        let (address, provider, mut client) = start();
        let token = provider.token().to_string();

        let mut stream = TcpStream::connect(address).unwrap();
        write!(
            stream,
            "POST /v1/chat/completions HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\
             Content-Length: 2\r\n\r\n{{}}"
        )
        .unwrap();

        let id = match next_frame(&mut client) {
            Message::ProviderRequest(request) => request.id,
            other => panic!("expected a provider request, got {}", other.kind()),
        };
        provider.head(id, 200, None);
        provider.data(id, b"partial".to_vec());
        // The client goes away before the response ends.
        drop(stream);

        let cancel = loop {
            match next_frame(&mut client) {
                Message::ProviderCancel(cancel) => break cancel,
                Message::ProviderRequest(_) => continue,
                other => panic!("expected a cancel, got {}", other.kind()),
            }
        };
        assert_eq!(cancel.id, id);
    }
}
