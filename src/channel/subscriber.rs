// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use super::errors::TryRecvError;
use crate::barrier::DependencyBarrier;
use crate::pod::Pod;
use crate::ring::{Padded, SharedRing};
use crate::slot::Slot;
use crate::wait::WaitStrategy;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

/// The read side of a Photon SPMC channel.
///
/// Each subscriber has its own cursor — no contention between consumers.
pub struct Subscriber<T: Pod> {
    pub(super) ring: Arc<SharedRing<T>>,
    /// Cached raw pointer to the slot array. Avoids Arc + Box deref on the
    /// hot path. Valid for the lifetime of `ring` (the Arc keeps it alive).
    pub(super) slots_ptr: *const Slot<T>,
    /// Cached ring mask (`capacity - 1`). Immutable after construction.
    pub(super) mask: u64,
    pub(super) cursor: u64,
    /// Per-subscriber cursor tracker for backpressure. `None` on regular
    /// (lossy) channels — zero overhead.
    pub(super) tracker: Option<Arc<Padded<AtomicU64>>>,
    /// Cumulative messages skipped due to lag.
    pub(super) total_lagged: u64,
    /// Cumulative messages successfully received.
    pub(super) total_received: u64,
}

unsafe impl<T: Pod> Send for Subscriber<T> {}

impl<T: Pod> Subscriber<T> {
    /// Try to receive the next message without blocking.
    #[inline]
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.read_slot()
    }

    /// Spin until the next message is available and return it.
    ///
    /// Uses a two-phase spin strategy: bare spin for the first 64 iterations
    /// (minimum wakeup latency, ~0 ns reaction time), then `PAUSE`-based spin
    /// (saves power, yields to SMT sibling). On Skylake+, `PAUSE` adds ~140
    /// cycles of delay per iteration — the bare-spin phase avoids this penalty
    /// when the message arrives quickly (typical for cross-thread pub/sub).
    #[inline]
    pub fn recv(&mut self) -> T {
        // SAFETY: slots_ptr is valid for the lifetime of self.ring (Arc-owned).
        let slot = unsafe { &*self.slots_ptr.add((self.cursor & self.mask) as usize) };
        let expected = self.cursor * 2 + 2;
        // Phase 1: bare spin — no PAUSE, minimum wakeup latency
        for _ in 0..64 {
            match slot.try_read(self.cursor) {
                Ok(Some(value)) => {
                    self.cursor += 1;
                    self.update_tracker();
                    self.total_received += 1;
                    return value;
                }
                Ok(None) => {}
                Err(stamp) => {
                    if stamp >= expected {
                        return self.recv_slow();
                    }
                }
            }
        }
        // Phase 2: PAUSE-based spin — power efficient
        loop {
            match slot.try_read(self.cursor) {
                Ok(Some(value)) => {
                    self.cursor += 1;
                    self.update_tracker();
                    self.total_received += 1;
                    return value;
                }
                Ok(None) => core::hint::spin_loop(),
                Err(stamp) => {
                    if stamp < expected {
                        core::hint::spin_loop();
                    } else {
                        return self.recv_slow();
                    }
                }
            }
        }
    }

    /// Slow path for lag recovery in recv().
    #[cold]
    #[inline(never)]
    fn recv_slow(&mut self) -> T {
        loop {
            match self.try_recv() {
                Ok(val) => return val,
                Err(TryRecvError::Empty) => core::hint::spin_loop(),
                Err(TryRecvError::Lagged { .. }) => {}
            }
        }
    }

    /// Block until the next message using the given [`WaitStrategy`].
    ///
    /// Unlike [`recv()`](Self::recv), which hard-codes a two-phase spin,
    /// this method delegates idle behaviour to the strategy — enabling
    /// yield-based, park-based, or adaptive waiting.
    ///
    /// # Example
    /// ```
    /// use photon_ring::{channel, WaitStrategy};
    ///
    /// let (mut p, s) = channel::<u64>(64);
    /// let mut sub = s.subscribe();
    /// p.publish(7);
    /// assert_eq!(sub.recv_with(WaitStrategy::BusySpin), 7);
    /// ```
    #[inline]
    pub fn recv_with(&mut self, strategy: WaitStrategy) -> T {
        let slot = unsafe { &*self.slots_ptr.add((self.cursor & self.mask) as usize) };
        let expected = self.cursor * 2 + 2;
        let mut iter: u32 = 0;
        loop {
            match slot.try_read(self.cursor) {
                Ok(Some(value)) => {
                    self.cursor += 1;
                    self.update_tracker();
                    self.total_received += 1;
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

    /// Skip to the **latest** published message (discards intermediate ones).
    ///
    /// Returns `None` only if nothing has been published yet. Under heavy
    /// producer load, retries internally if the target slot is mid-write.
    pub fn latest(&mut self) -> Option<T> {
        loop {
            let head = self.ring.cursor.0.load(Ordering::Acquire);
            if head == u64::MAX {
                return None;
            }
            self.cursor = head;
            match self.read_slot() {
                Ok(v) => return Some(v),
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Lagged { .. }) => {
                    // Producer lapped us between cursor read and slot read.
                    // Retry with updated head.
                }
            }
        }
    }

    /// How many messages are available to read (capped at ring capacity).
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

    /// Total messages successfully received by this subscriber.
    #[inline]
    pub fn total_received(&self) -> u64 {
        self.total_received
    }

    /// Total messages lost due to lag (consumer fell behind the ring).
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

    /// Returns an iterator that drains all currently available messages.
    /// Stops when no more messages are available. Handles lag transparently
    /// by retrying after cursor advancement.
    pub fn drain(&mut self) -> Drain<'_, T> {
        Drain { sub: self }
    }

    /// Get this subscriber's cursor tracker for use in a
    /// [`DependencyBarrier`].
    ///
    /// Returns `None` if the subscriber was created on a lossy channel
    /// without [`subscribe_tracked()`](crate::Subscribable::subscribe_tracked).
    /// Use `subscribe_tracked()` to ensure a tracker is always present.
    #[inline]
    pub fn tracker(&self) -> Option<Arc<Padded<AtomicU64>>> {
        self.tracker.clone()
    }

    /// Try to receive the next message, but only if all upstream
    /// subscribers in the barrier have already processed it.
    ///
    /// Returns [`TryRecvError::Empty`] if the upstream barrier has not
    /// yet advanced past this subscriber's cursor, or if no new message
    /// is available from the ring.
    ///
    /// # Example
    ///
    /// ```
    /// use photon_ring::{channel, DependencyBarrier, TryRecvError};
    ///
    /// let (mut pub_, subs) = channel::<u64>(64);
    /// let mut upstream = subs.subscribe_tracked();
    /// let barrier = DependencyBarrier::from_subscribers(&[&upstream]);
    /// let mut downstream = subs.subscribe();
    ///
    /// pub_.publish(42);
    ///
    /// // Downstream can't read — upstream hasn't consumed it yet
    /// assert_eq!(downstream.try_recv_gated(&barrier), Err(TryRecvError::Empty));
    ///
    /// upstream.try_recv().unwrap();
    ///
    /// // Now downstream can proceed
    /// assert_eq!(downstream.try_recv_gated(&barrier), Ok(42));
    /// ```
    #[inline]
    pub fn try_recv_gated(&mut self, barrier: &DependencyBarrier) -> Result<T, TryRecvError> {
        // The barrier's slowest() returns the minimum tracker value among
        // upstreams, which is the *next sequence to read* for the slowest
        // upstream. If slowest() <= self.cursor, the slowest upstream hasn't
        // finished reading self.cursor yet.
        if barrier.slowest() <= self.cursor {
            return Err(TryRecvError::Empty);
        }
        self.try_recv()
    }

    /// Blocking receive gated by a dependency barrier.
    ///
    /// Spins until all upstream subscribers in the barrier have processed
    /// the next message, then reads and returns it. On lag, the cursor is
    /// advanced and the method retries.
    ///
    /// # Example
    ///
    /// ```
    /// use photon_ring::{channel, DependencyBarrier};
    ///
    /// let (mut pub_, subs) = channel::<u64>(64);
    /// let mut upstream = subs.subscribe_tracked();
    /// let barrier = DependencyBarrier::from_subscribers(&[&upstream]);
    /// let mut downstream = subs.subscribe();
    ///
    /// pub_.publish(99);
    /// upstream.try_recv().unwrap();
    ///
    /// assert_eq!(downstream.recv_gated(&barrier), 99);
    /// ```
    #[inline]
    pub fn recv_gated(&mut self, barrier: &DependencyBarrier) -> T {
        loop {
            match self.try_recv_gated(barrier) {
                Ok(val) => return val,
                Err(TryRecvError::Empty) => core::hint::spin_loop(),
                Err(TryRecvError::Lagged { .. }) => {}
            }
        }
    }

    /// Update the backpressure tracker to reflect the current cursor position.
    /// No-op on regular (lossy) channels.
    #[inline]
    fn update_tracker(&self) {
        if let Some(ref tracker) = self.tracker {
            tracker.0.store(self.cursor, Ordering::Relaxed);
        }
    }

    /// Stamp-only fast-path read. The consumer's local `self.cursor` tells us
    /// which slot and expected stamp to check — no shared cursor load needed
    /// on the hot path.
    #[inline]
    fn read_slot(&mut self) -> Result<T, TryRecvError> {
        // SAFETY: slots_ptr is valid for the lifetime of self.ring (Arc-owned).
        let slot = unsafe { &*self.slots_ptr.add((self.cursor & self.mask) as usize) };
        let expected = self.cursor * 2 + 2;

        match slot.try_read(self.cursor) {
            Ok(Some(value)) => {
                self.cursor += 1;
                self.update_tracker();
                self.total_received += 1;
                Ok(value)
            }
            Ok(None) => {
                // Torn read or write-in-progress — treat as empty for try_recv
                Err(TryRecvError::Empty)
            }
            Err(actual_stamp) => {
                // Odd stamp means write-in-progress — not ready yet
                if actual_stamp & 1 != 0 {
                    return Err(TryRecvError::Empty);
                }
                if actual_stamp < expected {
                    // Slot holds an older (or no) sequence — not published yet
                    Err(TryRecvError::Empty)
                } else {
                    // stamp > expected: slot was overwritten — slow path.
                    // Read head cursor to compute exact lag.
                    let head = self.ring.cursor.0.load(Ordering::Acquire);
                    let cap = self.ring.capacity();
                    if head == u64::MAX || self.cursor > head {
                        // Rare race: stamp updated but cursor not yet visible
                        return Err(TryRecvError::Empty);
                    }
                    if head >= cap {
                        let oldest = head - cap + 1;
                        if self.cursor < oldest {
                            let skipped = oldest - self.cursor;
                            self.cursor = oldest;
                            self.update_tracker();
                            self.total_lagged += skipped;
                            return Err(TryRecvError::Lagged { skipped });
                        }
                    }
                    // Head hasn't caught up yet (rare timing race)
                    Err(TryRecvError::Empty)
                }
            }
        }
    }
}

impl<T: Pod> Drop for Subscriber<T> {
    fn drop(&mut self) {
        if let Some(ref tracker) = self.tracker {
            if let Some(ref bp) = self.ring.backpressure {
                let mut trackers = bp.trackers.lock();
                trackers.retain(|t| !Arc::ptr_eq(t, tracker));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Drain iterator
// ---------------------------------------------------------------------------

/// An iterator that drains all currently available messages from a
/// [`Subscriber`]. Stops when no more messages are available. Handles lag transparently
/// by retrying after cursor advancement.
///
/// Created by [`Subscriber::drain`].
pub struct Drain<'a, T: Pod> {
    pub(super) sub: &'a mut Subscriber<T>,
}

impl<'a, T: Pod> Iterator for Drain<'a, T> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        loop {
            match self.sub.try_recv() {
                Ok(v) => return Some(v),
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Lagged { .. }) => {
                    // Cursor was advanced — retry from oldest available.
                }
            }
        }
    }
}
