// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use super::group::SubscriberGroup;
use super::subscriber::Subscriber;
use crate::pod::Pod;
use crate::ring::{Padded, SharedRing};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

/// Clone-able handle for spawning [`Subscriber`]s.
///
/// Send this to other threads and call [`subscribe`](Subscribable::subscribe)
/// to create independent consumers.
pub struct Subscribable<T: Pod> {
    pub(super) ring: Arc<SharedRing<T>>,
}

impl<T: Pod> Clone for Subscribable<T> {
    fn clone(&self) -> Self {
        Subscribable {
            ring: self.ring.clone(),
        }
    }
}

unsafe impl<T: Pod> Send for Subscribable<T> {}
unsafe impl<T: Pod> Sync for Subscribable<T> {}

impl<T: Pod> Subscribable<T> {
    /// Create a subscriber that will see only **future** messages.
    pub fn subscribe(&self) -> Subscriber<T> {
        let head = self.ring.cursor.0.load(Ordering::Acquire);
        let start = if head == u64::MAX { 0 } else { head + 1 };
        let tracker = self.ring.register_tracker(start);
        let slots_ptr = self.ring.slots_ptr();
        let mask = self.ring.mask;
        Subscriber {
            ring: self.ring.clone(),
            slots_ptr,
            mask,
            cursor: start,
            tracker,
            total_lagged: 0,
            total_received: 0,
        }
    }

    /// Create a [`SubscriberGroup`] of `N` subscribers starting from the next
    /// message. All `N` logical subscribers share a single ring read — the
    /// seqlock is checked once and all cursors are advanced together.
    ///
    /// This is dramatically faster than `N` independent [`Subscriber`]s when
    /// polled in a loop on the same thread.
    ///
    /// # Panics
    ///
    /// Panics if `N` is 0.
    pub fn subscribe_group<const N: usize>(&self) -> SubscriberGroup<T, N> {
        assert!(N > 0, "SubscriberGroup requires at least 1 subscriber");
        let head = self.ring.cursor.0.load(Ordering::Acquire);
        let start = if head == u64::MAX { 0 } else { head + 1 };
        let tracker = self.ring.register_tracker(start);
        let slots_ptr = self.ring.slots_ptr();
        let mask = self.ring.mask;
        SubscriberGroup {
            ring: self.ring.clone(),
            slots_ptr,
            mask,
            cursor: start,
            total_lagged: 0,
            total_received: 0,
            tracker,
        }
    }

    /// Create a subscriber starting from the **oldest available** message
    /// still in the ring (or 0 if nothing published yet).
    pub fn subscribe_from_oldest(&self) -> Subscriber<T> {
        let head = self.ring.cursor.0.load(Ordering::Acquire);
        let cap = self.ring.capacity();
        let start = if head == u64::MAX {
            0
        } else if head >= cap {
            head - cap + 1
        } else {
            0
        };
        let tracker = self.ring.register_tracker(start);
        let slots_ptr = self.ring.slots_ptr();
        let mask = self.ring.mask;
        Subscriber {
            ring: self.ring.clone(),
            slots_ptr,
            mask,
            cursor: start,
            tracker,
            total_lagged: 0,
            total_received: 0,
        }
    }

    /// Create a subscriber with an active cursor tracker.
    ///
    /// Use this when the subscriber will participate in a
    /// [`DependencyBarrier`] as an upstream consumer.
    ///
    /// On **bounded** channels, this behaves identically to
    /// [`subscribe()`](Self::subscribe) — those subscribers already have
    /// trackers.
    ///
    /// On **lossy** channels, [`subscribe()`](Self::subscribe) omits the
    /// tracker (zero overhead for the common case). This method creates a
    /// standalone tracker so that a [`DependencyBarrier`] can read the
    /// subscriber's cursor position. The tracker is **not** registered
    /// with the ring's backpressure system — it is purely for dependency
    /// graph coordination.
    pub fn subscribe_tracked(&self) -> Subscriber<T> {
        let head = self.ring.cursor.0.load(Ordering::Acquire);
        let start = if head == u64::MAX { 0 } else { head + 1 };
        // On bounded channels, register_tracker returns Some (backpressure-aware).
        // On lossy channels, it returns None — so we create a standalone tracker.
        let tracker = self
            .ring
            .register_tracker(start)
            .or_else(|| Some(Arc::new(Padded(AtomicU64::new(start)))));
        let slots_ptr = self.ring.slots_ptr();
        let mask = self.ring.mask;
        Subscriber {
            ring: self.ring.clone(),
            slots_ptr,
            mask,
            cursor: start,
            tracker,
            total_lagged: 0,
            total_received: 0,
        }
    }
}
