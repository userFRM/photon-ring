// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! Consumer dependency barriers for pipeline-style ordering.
//!
//! A [`DependencyBarrier`] gates a downstream consumer behind one or more
//! upstream consumers. The downstream subscriber will not advance past the
//! slowest upstream subscriber's cursor, ensuring that consumer C only
//! processes event N after consumers A and B have both finished it.
//!
//! This is the Disruptor's *sequence barrier* concept, adapted for Photon
//! Ring's cursor-tracker infrastructure.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::channel::Subscriber;
use crate::pod::Pod;
use crate::ring::Padded;

/// A barrier that gates a downstream consumer behind upstream consumers.
///
/// Reads the cursor trackers of upstream subscribers to determine the
/// highest safe sequence number. The downstream consumer should not
/// read beyond this point.
///
/// # Example
///
/// ```
/// use photon_ring::{channel, DependencyBarrier};
///
/// let (mut pub_, subs) = channel::<u64>(64);
/// let mut upstream_a = subs.subscribe_tracked();
/// let mut upstream_b = subs.subscribe_tracked();
/// let barrier = DependencyBarrier::from_subscribers(&[&upstream_a, &upstream_b]);
/// let mut downstream_c = subs.subscribe();
///
/// pub_.publish(42);
///
/// // C can't read yet — A and B haven't consumed it
/// assert_eq!(downstream_c.try_recv_gated(&barrier), Err(photon_ring::TryRecvError::Empty));
///
/// // A and B consume
/// upstream_a.try_recv().unwrap();
/// upstream_b.try_recv().unwrap();
///
/// // Now C can proceed
/// assert_eq!(downstream_c.try_recv_gated(&barrier), Ok(42));
/// ```
pub struct DependencyBarrier {
    upstreams: Vec<Arc<Padded<AtomicU64>>>,
}

impl DependencyBarrier {
    /// Create a barrier from a list of upstream subscriber trackers.
    ///
    /// Each tracker is an `Arc<Padded<AtomicU64>>` obtained from
    /// [`Subscriber::tracker()`].
    ///
    /// # Panics
    ///
    /// Panics if `trackers` is empty.
    pub fn new(trackers: Vec<Arc<Padded<AtomicU64>>>) -> Self {
        assert!(
            !trackers.is_empty(),
            "DependencyBarrier requires at least one upstream tracker"
        );
        DependencyBarrier {
            upstreams: trackers,
        }
    }

    /// Create a barrier from a slice of upstream subscribers.
    ///
    /// Extracts each subscriber's tracker. Use [`Subscribable::subscribe_tracked()`]
    /// to ensure every subscriber has a tracker, even on lossy channels.
    ///
    /// # Panics
    ///
    /// Panics if `subscribers` is empty or if any subscriber lacks a tracker
    /// (created on a lossy channel without [`subscribe_tracked()`]).
    ///
    /// [`Subscribable::subscribe_tracked()`]: crate::channel::Subscribable::subscribe_tracked
    /// [`subscribe_tracked()`]: crate::channel::Subscribable::subscribe_tracked
    pub fn from_subscribers<T: Pod>(subscribers: &[&Subscriber<T>]) -> Self {
        assert!(
            !subscribers.is_empty(),
            "DependencyBarrier requires at least one upstream subscriber"
        );
        let trackers: Vec<Arc<Padded<AtomicU64>>> = subscribers
            .iter()
            .map(|s| {
                s.tracker()
                    .expect("upstream subscriber has no tracker — use subscribe_tracked()")
            })
            .collect();
        DependencyBarrier {
            upstreams: trackers,
        }
    }

    /// The cursor position of the slowest upstream subscriber.
    ///
    /// This is the *next sequence to read* for the slowest upstream.
    /// A downstream subscriber at cursor `N` may proceed only if
    /// `slowest() > N` (meaning all upstreams have finished reading `N`).
    #[inline]
    pub fn slowest(&self) -> u64 {
        let mut min = u64::MAX;
        for t in &self.upstreams {
            let val = t.0.load(Ordering::Acquire);
            if val < min {
                min = val;
            }
        }
        min
    }

    /// Number of upstream subscribers tracked by this barrier.
    #[inline]
    pub fn upstream_count(&self) -> usize {
        self.upstreams.len()
    }
}
