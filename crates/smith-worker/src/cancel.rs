//! A cancellation flag shared between the worker's main loop and a running tool.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the main loop when a [`smith_channel::Cancel`] frame arrives, and cleared before
/// the next call. `run_shell` polls it while the child runs.
#[derive(Debug, Default)]
pub struct CancelFlag {
    cancelled: AtomicBool,
}

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn reset(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}
