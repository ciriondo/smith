//! Why the sandbox could not start, or could not continue.

/// A failure in the sandbox layer. The messages are written for the terminal: they name the
/// component and what the reader can do.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("{0}")]
    Config(String),
    #[error("cannot run {command}: {source}")]
    Spawn {
        command: String,
        source: std::io::Error,
    },
    #[error("podman failed: {0}")]
    Podman(String),
    #[error("the sandbox channel failed: {0}")]
    Channel(String),
    #[error("the sandbox channel failed: {0}")]
    ChannelIo(#[from] smith_channel::ChannelError),
    #[error("the sandbox worker did not answer within {0:?}")]
    Timeout(std::time::Duration),
    #[error("the sandbox worker speaks protocol {found}; this build speaks {wanted}")]
    Protocol { found: u16, wanted: u16 },
    #[error("the sandbox worker is version {found}; this build is {wanted}")]
    VersionMismatch { found: String, wanted: String },
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    FdMapping(#[from] command_fds::FdMappingCollision),
}
