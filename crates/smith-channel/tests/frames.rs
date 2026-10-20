use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use smith_channel::{
    read_frame, write_frame, Cancel, ChannelError, Connect, ConnectResult, FrameKind, Hello,
    HelloAck, Log, LogLevel, Message, MAX_PAYLOAD, PROTOCOL_VERSION,
};

fn sample_messages() -> Vec<Message> {
    vec![
        Message::Hello(Hello {
            protocol: PROTOCOL_VERSION,
            version: "0.1.0".to_string(),
            target: "x86_64-unknown-linux-gnu".to_string(),
            inventory: vec!["git".to_string(), "sh".to_string()],
        }),
        Message::HelloAck(HelloAck {
            workspace: "/work".to_string(),
            env: BTreeMap::from([("PATH".to_string(), "/usr/bin".to_string())]),
        }),
        Message::ToolCall(smith_channel::ToolCall {
            call_id: "c1".to_string(),
            name: "run_shell".to_string(),
            args: serde_json::json!({"command": "true"}),
        }),
        Message::ToolResult(smith_channel::ToolResult {
            call_id: "c1".to_string(),
            ok: true,
            content: "{}".to_string(),
        }),
        Message::Connect(Connect {
            tunnel: 7,
            host: "index.crates.io".to_string(),
            port: 443,
        }),
        Message::ConnectResult(ConnectResult {
            tunnel: 7,
            ok: false,
            reason: Some("not in the allowlist".to_string()),
        }),
        Message::Data {
            tunnel: 7,
            bytes: vec![0, 1, 2, 255],
        },
        Message::Eof { tunnel: 7 },
        Message::Log(Log {
            level: LogLevel::Warn,
            message: "inventory missing".to_string(),
        }),
        Message::Cancel(Cancel {
            call_id: "c1".to_string(),
        }),
        Message::Shutdown,
    ]
}

fn encode_all(messages: &[Message]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for message in messages {
        write_frame(&mut bytes, message).expect("write");
    }
    bytes
}

fn read_all(bytes: &[u8]) -> Vec<Message> {
    let mut reader = bytes;
    let mut messages = Vec::new();
    while let Some(message) = read_frame(&mut reader).expect("read") {
        messages.push(message);
    }
    messages
}

fn protocol_error(err: ChannelError) -> String {
    match err {
        ChannelError::Protocol(message) => message,
        ChannelError::Io(e) => panic!("expected a protocol error, got io: {e}"),
    }
}

#[test]
fn every_kind_round_trips() {
    let messages = sample_messages();
    let bytes = encode_all(&messages);
    assert_eq!(read_all(&bytes), messages);
}

#[test]
fn clean_eof_is_none() {
    let mut empty = &[][..];
    assert_eq!(read_frame(&mut empty).unwrap(), None);
}

#[test]
fn short_header_is_an_io_error() {
    let mut bytes = &[0u8, 0, 0][..];
    let err = read_frame(&mut bytes).unwrap_err();
    match err {
        ChannelError::Io(e) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
        ChannelError::Protocol(message) => panic!("unexpected protocol error: {message}"),
    }
}

#[test]
fn truncated_payload_is_an_io_error() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&10u32.to_be_bytes());
    bytes.push(FrameKind::ToolResult.as_u8());
    bytes.extend_from_slice(b"abc");

    let err = read_frame(&mut &bytes[..]).unwrap_err();
    match err {
        ChannelError::Io(e) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
        ChannelError::Protocol(message) => panic!("unexpected protocol error: {message}"),
    }
}

#[test]
fn oversize_length_is_rejected_without_reading_the_payload() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&((MAX_PAYLOAD + 1) as u32).to_be_bytes());
    bytes.push(FrameKind::Shutdown.as_u8());

    let message = protocol_error(read_frame(&mut &bytes[..]).unwrap_err());
    assert!(
        message.contains(&(MAX_PAYLOAD + 1).to_string()),
        "{message}"
    );
}

#[test]
fn oversize_write_is_rejected() {
    let message = Message::Data {
        tunnel: 1,
        bytes: vec![0u8; MAX_PAYLOAD - 3],
    };
    let mut sink = Vec::new();
    let err = write_frame(&mut sink, &message).unwrap_err();
    assert!(protocol_error(err).contains("exceeds"));
    assert!(sink.is_empty());
}

#[test]
fn payload_at_the_limit_is_accepted() {
    let message = Message::Data {
        tunnel: 1,
        bytes: vec![7u8; MAX_PAYLOAD - 4],
    };
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &message).unwrap();
    assert_eq!(bytes.len(), MAX_PAYLOAD + 5);
    assert_eq!(read_all(&bytes), vec![message]);
}

#[test]
fn unknown_kind_is_a_protocol_error() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.push(99);

    let message = protocol_error(read_frame(&mut &bytes[..]).unwrap_err());
    assert!(message.contains("unknown frame kind 99"), "{message}");
}

#[test]
fn data_without_tunnel_id_is_a_protocol_error() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&2u32.to_be_bytes());
    bytes.push(FrameKind::Data.as_u8());
    bytes.extend_from_slice(&[0, 1]);

    let message = protocol_error(read_frame(&mut &bytes[..]).unwrap_err());
    assert!(message.contains("no tunnel id"), "{message}");
}

#[test]
fn eof_with_the_wrong_length_is_a_protocol_error() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.push(FrameKind::Eof.as_u8());

    let message = protocol_error(read_frame(&mut &bytes[..]).unwrap_err());
    assert!(message.contains("expected 4"), "{message}");
}

#[test]
fn shutdown_with_a_payload_is_a_protocol_error() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.push(FrameKind::Shutdown.as_u8());
    bytes.push(0);

    let message = protocol_error(read_frame(&mut &bytes[..]).unwrap_err());
    assert!(message.contains("expected 0"), "{message}");
}

#[test]
fn unknown_json_fields_are_rejected() {
    let payload = br#"{"protocol":1,"version":"v","target":"t","inventory":[],"extra":true}"#;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.push(FrameKind::Hello.as_u8());
    bytes.extend_from_slice(payload);

    let message = protocol_error(read_frame(&mut &bytes[..]).unwrap_err());
    assert!(message.contains("malformed hello payload"), "{message}");
}

#[test]
fn unknown_fields_in_a_control_message_are_rejected() {
    let payload = br#"{"call_id":"c1","extra":1}"#;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.push(FrameKind::Cancel.as_u8());
    bytes.extend_from_slice(payload);

    let message = protocol_error(read_frame(&mut &bytes[..]).unwrap_err());
    assert!(message.contains("malformed cancel payload"), "{message}");
}

struct Dribble<'a> {
    inner: &'a [u8],
}

impl Read for Dribble<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.inner.is_empty() || buf.is_empty() {
            return Ok(0);
        }
        buf[0] = self.inner[0];
        self.inner = &self.inner[1..];
        Ok(1)
    }
}

#[test]
fn a_reader_that_returns_one_byte_at_a_time_still_produces_frames() {
    let messages = sample_messages();
    let bytes = encode_all(&messages);

    let mut reader = Dribble { inner: &bytes };
    let mut read = Vec::new();
    while let Some(message) = read_frame(&mut reader).unwrap() {
        read.push(message);
    }
    assert_eq!(read, messages);
}

#[derive(Default)]
struct Recorder {
    bytes: Vec<u8>,
    flushes: usize,
}

impl Write for Recorder {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}

#[test]
fn every_frame_is_flushed() {
    let mut recorder = Recorder::default();
    write_frame(&mut recorder, &Message::Shutdown).unwrap();
    assert_eq!(recorder.flushes, 1);
}
