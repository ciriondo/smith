//! The sandbox worker, which runs as pid 1 inside the container.
//!
//! The worker sends a [`smith_channel::Hello`] with the protocol version and the image
//! inventory, receives the workspace and the environment from the host, and answers tool
//! calls. It holds no policy: the host decides the allowlist, the mounts and the environment.

pub mod cancel;
pub mod protocol;
pub mod provider;
pub mod proxy;
pub mod tasks;
pub mod tools;
pub mod workspace;

pub use cancel::CancelFlag;
pub use protocol::{run, run_with_inventory};
pub use provider::Provider;
pub use tasks::TaskRegistry;
pub use workspace::Workspace;
