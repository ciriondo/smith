//! The wire protocol between the host and the sandbox worker, and the tool argument and
//! result types both sides parse.
//!
//! The crate reads and writes only the streams it is given. It has no async runtime and no
//! networking dependency, so `smith-worker` can link it into a static binary.

pub mod frame;
pub mod tools;

pub use frame::{
    read_frame, write_frame, Cancel, ChannelError, Connect, ConnectResult, FrameKind, Hello,
    HelloAck, Log, LogLevel, Message, ToolCall, ToolResult, MAX_PAYLOAD, PROTOCOL_VERSION,
};
pub use tools::{
    EditFileArgs, EditFileResult, ReadFileArgs, ReadFileResult, RunShellArgs, RunShellResult,
    WriteFileArgs, WriteFileResult,
};
