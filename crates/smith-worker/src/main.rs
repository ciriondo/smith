//! Entry point of the process that runs as pid 1 inside the sandbox.
//!
//! The host places one end of the channel socketpair on descriptor 3 with `--preserve-fds=1`.
//! The worker opens no other socket until the host asks for a tunnel.

use std::fs;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

/// The descriptor the host reserves for the channel.
const CHANNEL_FD: i32 = 3;

fn main() -> ExitCode {
    let stream = match channel_stream() {
        Ok(stream) => stream,
        Err(message) => {
            eprintln!("smith-worker: {message}");
            return ExitCode::FAILURE;
        }
    };

    match smith_worker::run(stream) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("smith-worker: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Adopt the inherited channel descriptor.
///
/// The descriptor is validated before it is adopted, and the adoption duplicates it rather
/// than claiming ownership of descriptor 3. The original number then stays open and unowned
/// for the life of the process, so it cannot be handed to a later file and closed by mistake.
#[allow(unsafe_code)]
fn channel_stream() -> Result<UnixStream, String> {
    let path = format!("/proc/self/fd/{CHANNEL_FD}");
    let metadata = fs::metadata(&path)
        .map_err(|e| format!("descriptor {CHANNEL_FD} is not available: {e}"))?;
    if !metadata.file_type().is_socket() {
        return Err(format!("descriptor {CHANNEL_FD} is not a socket"));
    }

    // SAFETY: the metadata check above proves descriptor 3 is open and is a socket. The
    // borrow ends with this statement, and `dup` returns a separate owned descriptor, so the
    // process never owns descriptor 3 and no drop can close a recycled number. The host
    // places the socket there with `--preserve-fds=1`, and `main` is the first code to run.
    let borrowed = unsafe { BorrowedFd::borrow_raw(CHANNEL_FD) };
    let duplicate = nix::unistd::dup(borrowed)
        .map_err(|e| format!("cannot duplicate descriptor {CHANNEL_FD}: {e}"))?;
    let stream = UnixStream::from(duplicate);

    // A tool process must not inherit the channel.
    nix::fcntl::fcntl(
        &stream,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    )
    .map_err(|e| format!("cannot mark the channel close-on-exec: {e}"))?;

    Ok(stream)
}
