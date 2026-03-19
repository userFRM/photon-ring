// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use super::errors::TryRecvError;
use crate::pod::Pod;
use crate::ring::{Padded, SharedRing};
use crate::slot::Slot;
use crate::wait::WaitStrategy;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

/// A group of `N` logical subscribers backed by a single ring read.
///
/// All `N` logical subscribers share one cursor —
/// [`try_recv`](SubscriberGroup::try_recv) performs **one** seqlock read
/// and a single cursor increment, eliminating the N-element sweep loop.
///
/// ```
/// let (mut p, subs) = photon_ring::channel::<u64>(64);
/// let mut group = subs.subscribe_group::<4>();
/// p.publish(42);
/// assert_eq!(group.try_recv(), Ok(42));
/// ```
pub struct SubscriberGroup<T: Pod, const N: usize> {
    pub(super) ring: Arc<SharedRing<T>>,
    /// Cached raw pointer to the slot array. Avoids Arc + Box deref on the
    /// hot path. Valid for the lifetime of `ring` (the Arc keeps it alive).
    pub(super) slots_ptr: *const Slot<T>,
    /// Cached ring mask (`capacity - 1`). Immutable after construction.
    pub(super) mask: u64,
    /// Single cursor shared by all `N` logical subscribers.
    pub(super) cursor: u64,
    /// Cumulative messages skipped due to lag.
    pub(super) total_lagged: u64,
    /// Cumulative messages successfully received.
    pub(super) total_received: u64,
    /// Per-group cursor tracker for backpressure. `None` on regular
    /// (lossy) channels — zero overhead.
    pub(super) tracker: Option<Arc<Padded<AtomicU64>>>,
}

unsafe impl<T: Pod, const N: usize> Send for SubscriberGroup<T, N> {}

impl<T: Pod, const N: usize> SubscriberGroup<T, N> {
    /// Try to receive the next message for the group.
    ///
    /// Performs a single seqlock read and one cursor increment — no
    /// N-element sweep needed since all logical subscribers share one cursor.
    #[inline]
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let cur = self.cursor;
        // SAFETY: slots_ptr is valid for the lifetime of self.ring (Arc-owned).
        let slot = unsafe { &*self.slots_ptr.add((cur & self.mask) as usize) };
        let expected = cur * 2 + 2;

        match slot.try_read(cur) {
            Ok(Some(value)) => {
                self.cursor = cur + 1;
                self.total_received += 1;
                self.update_tracker();
                Ok(value)
            }
            Ok(None) => Err(TryRecvError::Empty),
            Err(actual_stamp) => {
                if actual_stamp & 1 != 0 || actual_stamp < expected {
                    return Err(TryRecvError::Empty);
                }
                // Lagged — recompute from head cursor
                let head = self.ring.cursor.0.load(Ordering::Acquire);
                let cap = self.ring.capacity();
                if head == u64::MAX || cur > head {
                    return Err(TryRecvError::Empty);
                }
                if head >= cap {
                    let oldest = head - cap + 1;
                    if cur < oldest {
                        let skipped = oldest - cur;
                        self.cursor = oldest;
                        self.total_lagged += skipped;
                        self.update_tracker();
                        return Err(TryRecvError::Lagged { skipped });
                    }
                }
                Err(TryRecvError::Empty)
            }
        }
    }

    /// Spin until the next message is available.
    ///
    /// On aarch64: uses SEVL + WFE for near-zero-power cache-line-event
    /// wakeup. On x86: uses PAUSE (spin_loop hint).
    #[inline]
    pub fn recv(&mut self) -> T {
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("sevl", options(nomem, nostack));
        }
        loop {
            match self.try_recv() {
                Ok(val) => return val,
                Err(TryRecvError::Empty) => {
                    #[cfg(target_arch = "aarch64")]
                    unsafe {
                        core::arch::asm!("wfe", options(nomem, nostack));
                    }
                    #[cfg(not(target_arch = "aarch64"))]
                    core::hint::spin_loop();
                }
                Err(TryRecvError::Lagged { .. }) => {}
            }
        }
    }

    /// Block until the next message using the given [`WaitStrategy`].
    ///
    /// Like [`Subscriber::recv_with`], but for the grouped fast path.
    ///
    /// # Example
    /// ```
    /// use photon_ring::{channel, WaitStrategy};
    ///
    /// let (mut p, s) = channel::<u64>(64);
    /// let mut group = s.subscribe_group::<2>();
    /// p.publish(42);
    /// assert_eq!(group.recv_with(WaitStrategy::BusySpin), 42);
    /// ```
    #[inline]
    pub fn recv_with(&mut self, strategy: WaitStrategy) -> T {
        let cur = self.cursor;
        let slot = unsafe { &*self.slots_ptr.add((cur & self.mask) as usize) };
        let expected = cur * 2 + 2;
        let mut iter: u32 = 0;
        loop {
            match slot.try_read(cur) {
                Ok(Some(value)) => {
                    self.cursor = cur + 1;
                    self.total_received += 1;
                    self.update_tracker();
                    return value;
                }
                Ok(None) => {
                    strategy.wait(iter);
                    iter = iter.saturating_add(1);
                }
                Err(stamp) => {
                    if stamp >= expected {
                        return self.recv_with_slow(strategy);
                    }
                    strategy.wait(iter);
                    iter = iter.saturating_add(1);
                }
            }
        }
    }

    #[cold]
    #[inline(never)]
    fn recv_with_slow(&mut self, strategy: WaitStrategy) -> T {
        let mut iter: u32 = 0;
        loop {
            match self.try_recv() {
                Ok(val) => return val,
                Err(TryRecvError::Empty) => {
                    strategy.wait(iter);
                    iter = iter.saturating_add(1);
                }
                Err(TryRecvError::Lagged { .. }) => {
                    iter = 0;
                }
            }
        }
    }

    /// How many of the `N` logical subscribers are aligned.
    ///
    /// With the single-cursor design all subscribers are always aligned,
    /// so this trivially returns `N`.
    #[inline]
    pub fn aligned_count(&self) -> usize {
        N
    }

    /// Number of messages available to read (capped at ring capacity).
    #[inline]
    pub fn pending(&self) -> u64 {
        let head = self.ring.cursor.0.load(Ordering::Acquire);
        if head == u64::MAX || self.cursor > head {
            0
        } else {
            let raw = head - self.cursor + 1;
            raw.min(self.ring.capacity())
        }
    }

    /// Total messages successfully received by this group.
    #[inline]
    pub fn total_received(&self) -> u64 {
        self.total_received
    }

    /// Total messages lost due to lag (group fell behind the ring).
    #[inline]
    pub fn total_lagged(&self) -> u64 {
        self.total_lagged
    }

    /// Ratio of received to total (received + lagged). Returns 0.0 if no
    /// messages have been processed.
    #[inline]
    pub fn receive_ratio(&self) -> f64 {
        let total = self.total_received + self.total_lagged;
        if total == 0 {
            0.0
        } else {
            self.total_received as f64 / total as f64
        }
    }

    /// Receive up to `buf.len()` messages in a single call.
    ///
    /// Messages are written into the provided slice starting at index 0.
    /// Returns the number of messages received. On lag, the cursor is
    /// advanced and filling continues from the oldest available message.
    #[inline]
    pub fn recv_batch(&mut self, buf: &mut [T]) -> usize {
        let mut count = 0;
        for slot in buf.iter_mut() {
            match self.try_recv() {
                Ok(value) => {
                    *slot = value;
                    count += 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Lagged { .. }) => {
                    // Cursor was advanced — retry from oldest available.
                    match self.try_recv() {
                        Ok(value) => {
                            *slot = value;
                            count += 1;
                        }
                        Err(_) => break,
                    }
                }
            }
        }
        count
    }

    /// Update the backpressure tracker to reflect the current cursor position.
    /// No-op on regular (lossy) channels.
    #[inline]
    fn update_tracker(&self) {
        if let Some(ref tracker) = self.tracker {
            tracker.0.store(self.cursor, Ordering::Relaxed);
        }
    }
}

impl<T: Pod, const N: usize> Drop for SubscriberGroup<T, N> {
    fn drop(&mut self) {
        if let Some(ref tracker) = self.tracker {
            if let Some(ref bp) = self.ring.backpressure {
                let weak = Arc::downgrade(tracker);
                let mut trackers = bp.trackers.lock();
                trackers.retain(|t| !t.ptr_eq(&weak));
            }
        }
    }
}
