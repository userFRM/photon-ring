// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

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
        let idx = self.ring.index;
        Subscriber {
            ring: self.ring.clone(),
            slots_ptr,
            index: idx,
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
        SubscriberGroup(self.subscribe())
    }

    /// Create a subscriber that never gates the publisher.
    ///
    /// On a bounded channel, [`subscribe`](Self::subscribe) registers the
    /// subscriber for backpressure: the publisher refuses to overwrite a slot
    /// this subscriber has not read yet. A **lossy** subscriber opts out of
    /// that guarantee. The publisher ignores it entirely, and if it falls
    /// behind it observes [`TryRecvError::Lagged`](crate::TryRecvError::Lagged)
    /// with an exact skip count, exactly as on a lossy channel.
    ///
    /// This lets a single ring carry consumers with **different delivery
    /// contracts**: a risk engine that must see every message, and telemetry
    /// that must never stall the publisher, reading the same sequence numbers.
    ///
    /// ```
    /// use photon_ring::channel_bounded;
    ///
    /// let (mut p, s) = channel_bounded::<u64>(4, 0);
    /// let mut critical = s.subscribe();        // gates the publisher
    /// let mut telemetry = s.subscribe_lossy(); // never gates it
    ///
    /// p.publish(1);
    /// assert_eq!(critical.try_recv(), Ok(1));
    /// assert_eq!(telemetry.try_recv(), Ok(1));
    /// ```
    ///
    /// On a lossy channel this is identical to [`subscribe`](Self::subscribe),
    /// since no subscriber gates the publisher there.
    pub fn subscribe_lossy(&self) -> Subscriber<T> {
        let head = self.ring.cursor.0.load(Ordering::Acquire);
        let start = if head == u64::MAX { 0 } else { head + 1 };
        let slots_ptr = self.ring.slots_ptr();
        let idx = self.ring.index;
        Subscriber {
            ring: self.ring.clone(),
            slots_ptr,
            index: idx,
            cursor: start,
            // No tracker: the publisher's slowest-cursor scan never sees this
            // subscriber, so it can never be blocked by it.
            tracker: None,
            total_lagged: 0,
            total_received: 0,
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
        let idx = self.ring.index;
        Subscriber {
            ring: self.ring.clone(),
            slots_ptr,
            index: idx,
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
        let idx = self.ring.index;
        Subscriber {
            ring: self.ring.clone(),
            slots_ptr,
            index: idx,
            cursor: start,
            tracker,
            total_lagged: 0,
            total_received: 0,
        }
    }
}
