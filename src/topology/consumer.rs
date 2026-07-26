// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A managed terminal consumer: a dedicated thread that runs a handler over
//! every message, with shutdown, drain, and panic capture handled for you.
//!
//! [`topology::Pipeline`](super::Pipeline) covers stages that transform and
//! publish onward. This covers the end of the line — the stage that only
//! consumes — which otherwise means hand-rolling a thread and a receive loop.

extern crate std;

use crate::channel::{Subscriber, TryRecvError};
use crate::pod::Pod;
use crate::wait::WaitStrategy;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::{self, JoinHandle};

use super::{STAGE_COMPLETED, STAGE_PANICKED, STAGE_RUNNING};

/// What a consumer does with messages still buffered when it is asked to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainPolicy {
    /// Process everything already published before exiting. Matches the
    /// shutdown behaviour of the LMAX Disruptor and its Rust port, where a
    /// consumer drains to the last published sequence.
    Drain,
    /// Stop at the next poll, leaving buffered messages unprocessed.
    Immediate,
}

/// A handler thread reading from one [`Subscriber`].
///
/// Created by [`Consumer::spawn`]. The handler receives
/// `(message, sequence, end_of_batch)`, where `end_of_batch` is true for the
/// last message of each drained batch — the signal to flush anything the
/// handler has been accumulating.
///
/// A panic in the handler is captured: the thread exits, the consumer reports
/// [`panicked`](Consumer::panicked), and the subscriber is dropped, which
/// releases the publisher rather than wedging it.
pub struct Consumer {
    handle: Option<JoinHandle<()>>,
    status: Arc<AtomicU8>,
    shutdown: Arc<AtomicBool>,
}

impl Consumer {
    /// Spawn a thread running `f` over every message from `sub`.
    ///
    /// ```
    /// use photon_ring::{channel_bounded, WaitStrategy};
    /// use photon_ring::topology::{Consumer, DrainPolicy};
    /// use std::sync::atomic::{AtomicU64, Ordering};
    /// use std::sync::Arc;
    ///
    /// let (mut p, s) = channel_bounded::<u64>(64, 0);
    /// let total = Arc::new(AtomicU64::new(0));
    /// let t = total.clone();
    ///
    /// let consumer = Consumer::spawn(
    ///     s.subscribe(),
    ///     WaitStrategy::default(),
    ///     DrainPolicy::Drain,
    ///     move |value: u64, _seq, _end_of_batch| {
    ///         t.fetch_add(value, Ordering::Relaxed);
    ///     },
    /// );
    ///
    /// for i in 1..=10 {
    ///     p.publish(i);
    /// }
    /// consumer.shutdown();
    /// consumer.join();
    /// assert_eq!(total.load(Ordering::Relaxed), 55);
    /// ```
    pub fn spawn<T, F>(
        mut sub: Subscriber<T>,
        strategy: WaitStrategy,
        drain: DrainPolicy,
        mut f: F,
    ) -> Consumer
    where
        T: Pod,
        F: FnMut(T, u64, bool) + Send + 'static,
    {
        let status = Arc::new(AtomicU8::new(STAGE_RUNNING));
        let status_inner = status.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_inner = shutdown.clone();

        let handle = thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut idle: u32 = 0;
                loop {
                    if shutdown_inner.load(Ordering::Acquire) {
                        if drain == DrainPolicy::Drain {
                            drain_batch(&mut sub, &mut f);
                        }
                        return;
                    }
                    if drain_batch(&mut sub, &mut f) == 0 {
                        strategy.wait(idle);
                        idle = idle.saturating_add(1);
                    } else {
                        idle = 0;
                    }
                }
            }));
            match result {
                Ok(()) => status_inner.store(STAGE_COMPLETED, Ordering::Release),
                Err(_) => status_inner.store(STAGE_PANICKED, Ordering::Release),
            }
        });

        Consumer {
            handle: Some(handle),
            status,
            shutdown,
        }
    }

    /// Ask the consumer to stop after its current batch.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    /// Wait for the consumer thread to finish. Call [`shutdown`](Self::shutdown)
    /// first, or it will run indefinitely.
    pub fn join(mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// Whether the handler panicked.
    pub fn panicked(&self) -> bool {
        self.status.load(Ordering::Acquire) == STAGE_PANICKED
    }

    /// Whether the consumer is still processing.
    pub fn is_healthy(&self) -> bool {
        self.status.load(Ordering::Acquire) == STAGE_RUNNING
    }
}

/// Signals shutdown on drop, so a dropped handle does not leave a thread
/// spinning forever.
impl Drop for Consumer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

/// Drain everything currently available, calling `f` with `end_of_batch` set on
/// the final message. Returns how many were processed.
///
/// The batch size is read once from `pending()`, so `end_of_batch` refers to the
/// messages available when the batch started — the same meaning as the
/// Disruptor's `available == sequence` test.
#[inline]
fn drain_batch<T: Pod>(sub: &mut Subscriber<T>, f: &mut impl FnMut(T, u64, bool)) -> u64 {
    let batch = sub.pending();
    if batch == 0 {
        return 0;
    }
    let mut done = 0;
    for i in 0..batch {
        let seq = sub.cursor();
        match sub.try_recv() {
            Ok(value) => {
                f(value, seq, i + 1 == batch);
                done += 1;
            }
            // Lagged: the cursor already jumped to the oldest live message, so
            // keep going — the batch bound still limits the work.
            Err(TryRecvError::Lagged { .. }) => {}
            Err(TryRecvError::Empty) => break,
        }
    }
    done
}
