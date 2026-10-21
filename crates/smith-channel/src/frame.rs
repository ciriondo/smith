//! Frame encoding for the sandbox channel.
//!
//! A frame is a five-byte header and a payload: a `u32` big-endian payload length, a kind
//! byte, then that many payload bytes. Control messages are JSON. `Data` and `Eof` carry a
//! `u32` tunnel id, and `Data` appends raw bytes, so proxied traffic is not encoded a second
//! time. The provider frames use a `u64` request id in the same way: `ProviderRequest` and
//! `ProviderData` put the id in front of raw bytes, `ProviderEnd` and `ProviderCancel` carry
//! only the id, and `ProviderHead` and `ProviderFailure` are JSON because they carry text.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol version carried in [`Hello`]. The host and the worker reject a mismatch.
///
/// Version 2 added the provider proxy frames.
pub const PROTOCOL_VERSION: u16 = 2;

/// The largest payload either side accepts. A frame that advertises more closes the
/// connection before its payload is allocated.
pub const MAX_PAYLOAD: usize = 1024 * 1024;

/// A `u32` length and a kind byte.
const HEADER_LEN: usize = 5;

/// The discriminant of a frame, one per message type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Hello = 1,
    HelloAck = 2,
    ToolCall = 3,
    ToolResult = 4,
    Connect = 5,
    ConnectResult = 6,
    Data = 7,
    Eof = 8,
    Log = 9,
    Cancel = 10,
    Shutdown = 11,
    ProviderRequest = 12,
    ProviderHead = 13,
    ProviderData = 14,
    ProviderEnd = 15,
    ProviderFailure = 16,
    ProviderCancel = 17,
}

impl FrameKind {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FrameKind::Hello => "hello",
            FrameKind::HelloAck => "hello_ack",
            FrameKind::ToolCall => "tool_call",
            FrameKind::ToolResult => "tool_result",
            FrameKind::Connect => "connect",
            FrameKind::ConnectResult => "connect_result",
            FrameKind::Data => "data",
            FrameKind::Eof => "eof",
            FrameKind::Log => "log",
            FrameKind::Cancel => "cancel",
            FrameKind::Shutdown => "shutdown",
            FrameKind::ProviderRequest => "provider_request",
            FrameKind::ProviderHead => "provider_head",
            FrameKind::ProviderData => "provider_data",
            FrameKind::ProviderEnd => "provider_end",
            FrameKind::ProviderFailure => "provider_failure",
            FrameKind::ProviderCancel => "provider_cancel",
        }
    }

    fn from_u8(byte: u8) -> Result<Self, ChannelError> {
        let kind = match byte {
            1 => FrameKind::Hello,
            2 => FrameKind::HelloAck,
            3 => FrameKind::ToolCall,
            4 => FrameKind::ToolResult,
            5 => FrameKind::Connect,
            6 => FrameKind::ConnectResult,
            7 => FrameKind::Data,
            8 => FrameKind::Eof,
            9 => FrameKind::Log,
            10 => FrameKind::Cancel,
            11 => FrameKind::Shutdown,
            12 => FrameKind::ProviderRequest,
            13 => FrameKind::ProviderHead,
            14 => FrameKind::ProviderData,
            15 => FrameKind::ProviderEnd,
            16 => FrameKind::ProviderFailure,
            17 => FrameKind::ProviderCancel,
            other => {
                return Err(ChannelError::Protocol(format!(
                    "unknown frame kind {other}"
                )))
            }
        };
        Ok(kind)
    }
}

impl fmt::Display for FrameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a frame could not be written or read.
#[derive(Debug)]
pub enum ChannelError {
    /// The stream failed. `UnexpectedEof` here means the peer closed in the middle of a
    /// frame.
    Io(io::Error),
    /// The bytes were readable but did not form a valid frame.
    Protocol(String),
}

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChannelError::Io(e) => write!(f, "{e}"),
            ChannelError::Protocol(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ChannelError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ChannelError::Io(e) => Some(e),
            ChannelError::Protocol(_) => None,
        }
    }
}

impl From<io::Error> for ChannelError {
    fn from(e: io::Error) -> Self {
        ChannelError::Io(e)
    }
}

/// The worker's first frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    /// [`PROTOCOL_VERSION`] of the sender.
    pub protocol: u16,
    /// The smith version, which must match the host's exactly.
    pub version: String,
    /// The target the worker was built for, for logs and diagnostics.
    pub target: String,
    /// Binary names the image provides, one per entry.
    pub inventory: Vec<String>,
}

/// The host's answer to [`Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloAck {
    /// The canonical workspace root, which is also the container's working directory.
    pub workspace: String,
    /// The resolved environment for tool processes. Values travel here rather than in the
    /// container's argv.
    pub env: BTreeMap<String, String>,
}

/// One tool invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub args: Value,
}

/// The answer to one [`ToolCall`]. `content` is the result JSON or an error message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub call_id: String,
    pub ok: bool,
    pub content: String,
}

/// A request to open a tunnel to `host:port`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connect {
    pub tunnel: u32,
    pub host: String,
    pub port: u16,
}

/// The host's decision on a [`Connect`]. A refusal carries the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectResult {
    pub tunnel: u32,
    pub ok: bool,
    pub reason: Option<String>,
}

/// Severity of a [`Log`] frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// A line for the host's stderr. Worker logs do not go through the podman log driver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Log {
    pub level: LogLevel,
    pub message: String,
}

/// Cancel the tool call named by `call_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cancel {
    pub call_id: String,
}

/// One proxied provider request body from the worker. The host answers with
/// [`Message::ProviderHead`], any number of [`Message::ProviderData`] and
/// [`Message::ProviderEnd`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequest {
    pub id: u64,
    /// The child's request body, forwarded unchanged except for the host-side replacements.
    pub body: Vec<u8>,
}

/// The status line and content type of a proxied provider response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderHead {
    pub id: u64,
    pub status: u16,
    pub content_type: Option<String>,
}

/// A chunk of a proxied provider response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderData {
    pub id: u64,
    pub bytes: Vec<u8>,
}

/// The end of a proxied provider response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderEnd {
    pub id: u64,
}

/// A proxied provider request failed on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderFailure {
    pub id: u64,
    pub message: String,
}

/// The child closed the connection before the response finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCancel {
    pub id: u64,
}

/// One message in either direction.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Hello(Hello),
    HelloAck(HelloAck),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    Connect(Connect),
    ConnectResult(ConnectResult),
    /// Raw bytes for a tunnel. The tunnel id is part of the payload, not the JSON.
    Data {
        tunnel: u32,
        bytes: Vec<u8>,
    },
    /// One direction of a tunnel closed. The other side may still carry bytes.
    Eof {
        tunnel: u32,
    },
    Log(Log),
    Cancel(Cancel),
    Shutdown,
    ProviderRequest(ProviderRequest),
    ProviderHead(ProviderHead),
    ProviderData(ProviderData),
    ProviderEnd(ProviderEnd),
    ProviderFailure(ProviderFailure),
    ProviderCancel(ProviderCancel),
}

impl Message {
    pub fn kind(&self) -> FrameKind {
        match self {
            Message::Hello(_) => FrameKind::Hello,
            Message::HelloAck(_) => FrameKind::HelloAck,
            Message::ToolCall(_) => FrameKind::ToolCall,
            Message::ToolResult(_) => FrameKind::ToolResult,
            Message::Connect(_) => FrameKind::Connect,
            Message::ConnectResult(_) => FrameKind::ConnectResult,
            Message::Data { .. } => FrameKind::Data,
            Message::Eof { .. } => FrameKind::Eof,
            Message::Log(_) => FrameKind::Log,
            Message::Cancel(_) => FrameKind::Cancel,
            Message::Shutdown => FrameKind::Shutdown,
            Message::ProviderRequest(_) => FrameKind::ProviderRequest,
            Message::ProviderHead(_) => FrameKind::ProviderHead,
            Message::ProviderData(_) => FrameKind::ProviderData,
            Message::ProviderEnd(_) => FrameKind::ProviderEnd,
            Message::ProviderFailure(_) => FrameKind::ProviderFailure,
            Message::ProviderCancel(_) => FrameKind::ProviderCancel,
        }
    }
}

/// Write one frame and flush the writer.
pub fn write_frame<W: Write>(writer: &mut W, message: &Message) -> Result<(), ChannelError> {
    let kind = message.kind();
    let payload = encode(message)?;
    if payload.len() > MAX_PAYLOAD {
        return Err(ChannelError::Protocol(format!(
            "{kind} payload of {} bytes exceeds the {MAX_PAYLOAD}-byte limit",
            payload.len()
        )));
    }

    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    header[4] = kind.as_u8();
    writer.write_all(&header)?;
    writer.write_all(&payload)?;
    // Tool output and proxy traffic are interactive. A buffered writer must not hold a frame
    // until the next one arrives.
    writer.flush()?;
    Ok(())
}

/// Read one frame. `Ok(None)` means the peer closed cleanly at a frame boundary.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Message>, ChannelError> {
    let mut header = [0u8; HEADER_LEN];

    // A dedicated read for the first byte separates a clean close from a truncated header.
    let mut first = [0u8; 1];
    loop {
        match reader.read(&mut first) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    header[0] = first[0];
    reader.read_exact(&mut header[1..])?;

    let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if length > MAX_PAYLOAD {
        return Err(ChannelError::Protocol(format!(
            "frame length {length} exceeds the {MAX_PAYLOAD}-byte limit"
        )));
    }

    let kind = FrameKind::from_u8(header[4])?;
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload)?;
    decode(kind, payload).map(Some)
}

fn encode(message: &Message) -> Result<Vec<u8>, ChannelError> {
    fn json<T: Serialize>(value: &T, kind: FrameKind) -> Result<Vec<u8>, ChannelError> {
        serde_json::to_vec(value)
            .map_err(|e| ChannelError::Protocol(format!("cannot encode a {kind} frame: {e}")))
    }

    let payload = match message {
        Message::Hello(m) => json(m, FrameKind::Hello)?,
        Message::HelloAck(m) => json(m, FrameKind::HelloAck)?,
        Message::ToolCall(m) => json(m, FrameKind::ToolCall)?,
        Message::ToolResult(m) => json(m, FrameKind::ToolResult)?,
        Message::Connect(m) => json(m, FrameKind::Connect)?,
        Message::ConnectResult(m) => json(m, FrameKind::ConnectResult)?,
        Message::Data { tunnel, bytes } => {
            let mut payload = Vec::with_capacity(4 + bytes.len());
            payload.extend_from_slice(&tunnel.to_be_bytes());
            payload.extend_from_slice(bytes);
            payload
        }
        Message::Eof { tunnel } => tunnel.to_be_bytes().to_vec(),
        Message::Log(m) => json(m, FrameKind::Log)?,
        Message::Cancel(m) => json(m, FrameKind::Cancel)?,
        Message::Shutdown => Vec::new(),
        Message::ProviderRequest(m) => with_id(m.id, &m.body),
        Message::ProviderHead(m) => json(m, FrameKind::ProviderHead)?,
        Message::ProviderData(m) => with_id(m.id, &m.bytes),
        Message::ProviderEnd(m) => m.id.to_be_bytes().to_vec(),
        Message::ProviderFailure(m) => json(m, FrameKind::ProviderFailure)?,
        Message::ProviderCancel(m) => m.id.to_be_bytes().to_vec(),
    };
    Ok(payload)
}

/// A `u64` provider request id followed by raw bytes.
fn with_id(id: u64, bytes: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8 + bytes.len());
    payload.extend_from_slice(&id.to_be_bytes());
    payload.extend_from_slice(bytes);
    payload
}

/// Split a provider payload into its `u64` id and the bytes that follow.
fn split_id(kind: FrameKind, payload: Vec<u8>) -> Result<(u64, Vec<u8>), ChannelError> {
    if payload.len() < 8 {
        return Err(ChannelError::Protocol(format!(
            "{kind} payload of {} bytes has no request id",
            payload.len()
        )));
    }
    let mut id = [0u8; 8];
    id.copy_from_slice(&payload[..8]);
    Ok((u64::from_be_bytes(id), payload[8..].to_vec()))
}

fn decode_end(kind: FrameKind, payload: Vec<u8>) -> Result<u64, ChannelError> {
    if payload.len() != 8 {
        return Err(ChannelError::Protocol(format!(
            "{kind} payload is {} bytes, expected 8",
            payload.len()
        )));
    }
    let (id, _) = split_id(kind, payload)?;
    Ok(id)
}

fn decode(kind: FrameKind, payload: Vec<u8>) -> Result<Message, ChannelError> {
    fn json<T: for<'de> Deserialize<'de>>(
        payload: Vec<u8>,
        kind: FrameKind,
    ) -> Result<T, ChannelError> {
        serde_json::from_slice(&payload)
            .map_err(|e| ChannelError::Protocol(format!("malformed {kind} payload: {e}")))
    }

    let message = match kind {
        FrameKind::Hello => Message::Hello(json(payload, kind)?),
        FrameKind::HelloAck => Message::HelloAck(json(payload, kind)?),
        FrameKind::ToolCall => Message::ToolCall(json(payload, kind)?),
        FrameKind::ToolResult => Message::ToolResult(json(payload, kind)?),
        FrameKind::Connect => Message::Connect(json(payload, kind)?),
        FrameKind::ConnectResult => Message::ConnectResult(json(payload, kind)?),
        FrameKind::Log => Message::Log(json(payload, kind)?),
        FrameKind::Cancel => Message::Cancel(json(payload, kind)?),
        FrameKind::Data => {
            if payload.len() < 4 {
                return Err(ChannelError::Protocol(format!(
                    "data payload of {} bytes has no tunnel id",
                    payload.len()
                )));
            }
            let tunnel = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            Message::Data {
                tunnel,
                bytes: payload[4..].to_vec(),
            }
        }
        FrameKind::Eof => {
            if payload.len() != 4 {
                return Err(ChannelError::Protocol(format!(
                    "eof payload is {} bytes, expected 4",
                    payload.len()
                )));
            }
            Message::Eof {
                tunnel: u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
            }
        }
        FrameKind::Shutdown => {
            if !payload.is_empty() {
                return Err(ChannelError::Protocol(format!(
                    "shutdown payload is {} bytes, expected 0",
                    payload.len()
                )));
            }
            Message::Shutdown
        }
        FrameKind::ProviderRequest => {
            let (id, body) = split_id(kind, payload)?;
            Message::ProviderRequest(ProviderRequest { id, body })
        }
        FrameKind::ProviderHead => Message::ProviderHead(json(payload, kind)?),
        FrameKind::ProviderData => {
            let (id, bytes) = split_id(kind, payload)?;
            Message::ProviderData(ProviderData { id, bytes })
        }
        FrameKind::ProviderEnd => Message::ProviderEnd(ProviderEnd {
            id: decode_end(kind, payload)?,
        }),
        FrameKind::ProviderFailure => Message::ProviderFailure(json(payload, kind)?),
        FrameKind::ProviderCancel => Message::ProviderCancel(ProviderCancel {
            id: decode_end(kind, payload)?,
        }),
    };
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(message: Message) {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &message).unwrap();
        let decoded = read_frame(&mut &bytes[..]).unwrap().unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn provider_frames_round_trip() {
        round_trip(Message::ProviderRequest(ProviderRequest {
            id: 7,
            body: br#"{"model":"m","messages":[]}"#.to_vec(),
        }));
        round_trip(Message::ProviderRequest(ProviderRequest {
            id: u64::MAX,
            body: Vec::new(),
        }));
        round_trip(Message::ProviderHead(ProviderHead {
            id: 7,
            status: 200,
            content_type: Some("text/event-stream".to_string()),
        }));
        round_trip(Message::ProviderHead(ProviderHead {
            id: 7,
            status: 502,
            content_type: None,
        }));
        round_trip(Message::ProviderData(ProviderData {
            id: 7,
            bytes: b"data: {}\n\n".to_vec(),
        }));
        round_trip(Message::ProviderEnd(ProviderEnd { id: 7 }));
        round_trip(Message::ProviderFailure(ProviderFailure {
            id: 7,
            message: "no proxy".to_string(),
        }));
        round_trip(Message::ProviderCancel(ProviderCancel { id: 7 }));
    }

    #[test]
    fn a_provider_frame_without_its_id_is_rejected() {
        let mut bytes = Vec::new();
        // A length of 4, kind `provider_end`, and four bytes that are not an id.
        bytes.extend_from_slice(&4u32.to_be_bytes());
        bytes.push(FrameKind::ProviderEnd.as_u8());
        bytes.extend_from_slice(&[0, 0, 0, 1]);
        let error = read_frame(&mut &bytes[..]).unwrap_err().to_string();
        assert!(error.contains("expected 8"), "{error}");
    }

    #[test]
    fn a_payload_over_the_limit_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&((MAX_PAYLOAD + 1) as u32).to_be_bytes());
        bytes.push(FrameKind::ProviderData.as_u8());
        let error = read_frame(&mut &bytes[..]).unwrap_err().to_string();
        assert!(error.contains("exceeds"), "{error}");
    }
}
