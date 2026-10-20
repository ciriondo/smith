//! Host-side control of the sandbox.
//!
//! [`Supervisor`] owns the image, the container store and the caches. [`tools`] builds the
//! four [`smith_core::Tool`] values that forward calls over the channel to the worker. The
//! worker holds no policy; the allowlist and the mounts are decided here.

pub mod command;
pub mod error;
pub mod image;
pub mod supervisor;
pub mod tool;

pub use error::SandboxError;
pub use supervisor::{Sandbox, SandboxConfig, Supervisor};
pub use tool::{sandbox_tools, standard_env};
