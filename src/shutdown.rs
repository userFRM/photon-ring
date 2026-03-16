// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! A shared shutdown signal for coordinating graceful termination of
//! consumer loops.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

/// A shared shutdown signal for coordinating graceful termination.
///
/// ```
/// use photon_ring::Shutdown;
///
/// let shutdown = Shutdown::new();
/// let flag = shutdown.clone();
///
/// // In consumer thread:
/// // while !flag.is_shutdown() {
/// //     match sub.try_recv() {
/// //         Ok(v) => { /* process */ }
/// //         Err(_) => core::hint::spin_loop(),
/// //     }
/// // }
///
/// // In main thread:
/// shutdown.trigger();
/// assert!(flag.is_shutdown());
/// ```
pub struct Shutdown {
    flag: Arc<AtomicBool>,
}

impl Shutdown {
    /// Create a new shutdown signal (not yet triggered).
    pub fn new() -> Self {
        Shutdown {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Trigger the shutdown signal. All clones will observe `is_shutdown() == true`.
    pub fn trigger(&self) {
        self.flag.store(true, Ordering::Release);
    }

    /// Returns `true` if [`trigger`](Self::trigger) has been called on any clone.
    pub fn is_shutdown(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for Shutdown {
    fn clone(&self) -> Self {
        Shutdown {
            flag: self.flag.clone(),
        }
    }
}
