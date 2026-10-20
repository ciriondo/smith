//! Bare-Esc detection while a turn runs.
//!
//! Between turns the REPL's line reader owns stdin in canonical mode. During a turn this
//! watcher takes stdin in raw mode, sends `Pause` on the first bare Esc and `Cancel` on the
//! second, then restores the terminal on drop. It reads keys only; output is untouched.
//!
//! The watcher runs on a thread that polls stdin and a stop pipe. It cannot share stdin with
//! the line reader: the guard is dropped and joined before the REPL reads the next line.

use std::io::{self, IsTerminal};
use std::os::fd::{AsFd, OwnedFd};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::termios::{self, LocalFlags, SetArg, SpecialCharacterIndices, Termios};
use nix::unistd::{pipe, read, write};
use smith_core::session::SessionHandle;

/// How long a lone Esc waits for the next byte before it is classified as bare. This is short
/// enough for a key press and long enough for a terminal to deliver the rest of a sequence.
const ESCAPE_TIMEOUT: Duration = Duration::from_millis(50);

/// The terminal watcher for one turn. Dropping it stops the thread and restores termios.
pub struct KeyWatch {
    stop: Option<OwnedFd>,
    thread: Option<JoinHandle<()>>,
    saved: Option<Termios>,
}

impl KeyWatch {
    /// Start watching when stdin and stdout are terminals. Returns `None` otherwise, or when
    /// the terminal cannot be put into raw mode.
    pub fn start(session: &SessionHandle) -> Option<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return None;
        }

        let saved = termios::tcgetattr(io::stdin()).ok()?;
        let mut raw = saved.clone();
        raw.local_flags
            .remove(LocalFlags::ICANON | LocalFlags::ECHO);
        raw.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
        raw.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
        if termios::tcsetattr(io::stdin(), SetArg::TCSANOW, &raw).is_err() {
            return None;
        }

        let (stop_read, stop_write) = match pipe() {
            Ok(pair) => pair,
            Err(_) => {
                let _ = termios::tcsetattr(io::stdin(), SetArg::TCSANOW, &saved);
                return None;
            }
        };

        let handle = session.clone();
        let thread = match thread::Builder::new()
            .name("smith-keywatch".to_string())
            .spawn(move || watch_loop(&handle, stop_read))
        {
            Ok(thread) => thread,
            Err(_) => {
                let _ = termios::tcsetattr(io::stdin(), SetArg::TCSANOW, &saved);
                return None;
            }
        };

        Some(Self {
            stop: Some(stop_write),
            thread: Some(thread),
            saved: Some(saved),
        })
    }
}

impl Drop for KeyWatch {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = write(stop.as_fd(), b"x");
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(saved) = self.saved.take() {
            let _ = termios::tcsetattr(io::stdin(), SetArg::TCSANOW, &saved);
        }
    }
}

fn watch_loop(session: &SessionHandle, stop: OwnedFd) {
    let stdin = io::stdin();
    let mut classifier = Classifier::new();
    let mut pause_sent = false;

    loop {
        let timeout = match classifier.timeout() {
            Some(remaining) => PollTimeout::try_from(remaining).unwrap_or(PollTimeout::MAX),
            None => PollTimeout::NONE,
        };
        let mut fds = [
            PollFd::new(stdin.as_fd(), PollFlags::POLLIN),
            PollFd::new(stop.as_fd(), PollFlags::POLLIN),
        ];

        match poll(&mut fds, timeout) {
            Ok(0) => {
                if classifier.on_timeout() == Some(KeyEvent::BareEscape) {
                    fire(session, &mut pause_sent);
                }
            }
            Ok(_) => {
                if fds[1]
                    .revents()
                    .is_some_and(|events| events.contains(PollFlags::POLLIN))
                {
                    return;
                }
                if !fds[0]
                    .revents()
                    .is_some_and(|events| events.contains(PollFlags::POLLIN))
                {
                    return;
                }

                let mut byte = [0u8; 1];
                match read(stdin.as_fd(), &mut byte) {
                    Ok(1) => {
                        if classifier.feed(byte[0]) == Some(KeyEvent::BareEscape) {
                            fire(session, &mut pause_sent);
                        }
                    }
                    Ok(_) => {}
                    Err(Errno::EINTR) => {}
                    Err(_) => return,
                }
            }
            Err(Errno::EINTR) => {}
            Err(_) => return,
        }
    }
}

/// The first bare Esc pauses; a second terminates.
fn fire(session: &SessionHandle, pause_sent: &mut bool) {
    if *pause_sent {
        session.cancel_now();
    } else if session.pause_now() {
        *pause_sent = true;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyEvent {
    BareEscape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Escape,
    Csi,
    Ss3,
}

/// Classifies a byte stream into bare Escape presses. A sequence that starts with `[` or `O`
/// is an arrow key, function key or paste marker, not a pause.
struct Classifier {
    state: State,
    deadline: Option<Instant>,
}

impl Classifier {
    fn new() -> Self {
        Self {
            state: State::Idle,
            deadline: None,
        }
    }

    fn feed(&mut self, byte: u8) -> Option<KeyEvent> {
        match self.state {
            State::Idle => {
                if byte == 0x1b {
                    self.enter(State::Escape);
                }
                None
            }
            State::Escape => match byte {
                0x1b => {
                    // The first Esc is bare; the second starts its own candidate.
                    self.enter(State::Escape);
                    Some(KeyEvent::BareEscape)
                }
                b'[' => {
                    self.enter(State::Csi);
                    None
                }
                b'O' => {
                    self.enter(State::Ss3);
                    None
                }
                _ => {
                    self.reset();
                    Some(KeyEvent::BareEscape)
                }
            },
            State::Csi => {
                if (0x40..=0x7e).contains(&byte) || !(0x20..=0x7e).contains(&byte) {
                    self.reset();
                } else {
                    self.enter(State::Csi);
                }
                None
            }
            State::Ss3 => {
                self.reset();
                None
            }
        }
    }

    fn enter(&mut self, state: State) {
        self.state = state;
        self.deadline = Some(Instant::now() + ESCAPE_TIMEOUT);
    }

    fn reset(&mut self) {
        self.state = State::Idle;
        self.deadline = None;
    }

    fn timeout(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    fn on_timeout(&mut self) -> Option<KeyEvent> {
        let expired = self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline);
        if !expired {
            return None;
        }
        let was_escape = self.state == State::Escape;
        self.reset();
        if was_escape {
            Some(KeyEvent::BareEscape)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(classifier: &mut Classifier, bytes: &[u8]) -> Vec<KeyEvent> {
        let mut events = Vec::new();
        for byte in bytes {
            if let Some(event) = classifier.feed(*byte) {
                events.push(event);
            }
        }
        events
    }

    fn expire(classifier: &mut Classifier) {
        classifier.deadline = Some(Instant::now() - ESCAPE_TIMEOUT);
    }

    #[test]
    fn a_lone_escape_becomes_bare_after_the_timeout() {
        let mut classifier = Classifier::new();
        assert!(feed(&mut classifier, b"\x1b").is_empty());
        assert_eq!(classifier.on_timeout(), None, "the deadline is still ahead");
        expire(&mut classifier);
        assert_eq!(classifier.on_timeout(), Some(KeyEvent::BareEscape));
        assert_eq!(classifier.state, State::Idle);
    }

    #[test]
    fn a_csi_sequence_is_not_a_pause() {
        let mut classifier = Classifier::new();
        assert!(feed(&mut classifier, b"\x1b[A").is_empty());
        assert_eq!(classifier.state, State::Idle);
        expire(&mut classifier);
        assert_eq!(classifier.on_timeout(), None);
    }

    #[test]
    fn an_ss3_sequence_is_not_a_pause() {
        let mut classifier = Classifier::new();
        assert!(feed(&mut classifier, b"\x1bOP").is_empty());
        assert_eq!(classifier.state, State::Idle);
    }

    #[test]
    fn a_double_escape_is_two_bare_presses() {
        let mut classifier = Classifier::new();
        assert_eq!(
            feed(&mut classifier, b"\x1b\x1b"),
            vec![KeyEvent::BareEscape],
            "the second Esc confirms the first"
        );
        expire(&mut classifier);
        assert_eq!(
            classifier.on_timeout(),
            Some(KeyEvent::BareEscape),
            "the second Esc times out as bare"
        );
    }

    #[test]
    fn a_normal_byte_after_escape_confirms_the_escape() {
        let mut classifier = Classifier::new();
        assert_eq!(feed(&mut classifier, b"\x1bx"), vec![KeyEvent::BareEscape]);
        assert_eq!(classifier.state, State::Idle);
    }

    #[test]
    fn an_unfinished_sequence_times_out_without_a_pause() {
        let mut classifier = Classifier::new();
        assert!(feed(&mut classifier, b"\x1b[").is_empty());
        expire(&mut classifier);
        assert_eq!(classifier.on_timeout(), None);
        assert_eq!(classifier.state, State::Idle);
    }
}
