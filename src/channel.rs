// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::ring::{Padded, SharedRing};
use crate::slot::Slot;
use crate::wait::WaitStrategy;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Error from [`Subscriber::try_recv`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TryRecvError {
    /// No new messages available.
    Empty,
    /// Consumer fell behind the ring. `skipped` messages were lost.
    Lagged { skipped: u64 },
}

/// Error returned by [`Publisher::try_publish`] when the ring is full
/// and backpressure is enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError<T> {
    /// The slowest consumer is within the backpressure watermark.
    /// Contains the value that was not published.
    Full(T),
}

// ---------------------------------------------------------------------------
// Publisher (single-producer write side)
// ---------------------------------------------------------------------------

/// The write side of a Photon SPMC channel.
///
/// There is exactly one `Publisher` per channel. It is `Send` but not `Sync` —
/// only one thread may publish at a time (single-producer guarantee enforced
/// by `&mut self`).
pub struct Publisher<T: Copy> {
    ring: Arc<SharedRing<T>>,
    /// Cached raw pointer to the slot array. Avoids Arc + Box deref on the
    /// hot path. Valid for the lifetime of `ring` (the Arc keeps it alive).
    slots_ptr: *const Slot<T>,
    /// Cached ring mask (`capacity - 1`). Immutable after construction.
    mask: u64,
    /// Cached raw pointer to `ring.cursor.0`. Avoids Arc deref on hot path.
    cursor_ptr: *const AtomicU64,
    seq: u64,
    /// Cached minimum cursor from the last tracker scan. Used as a fast-path
    /// check to avoid scanning on every `try_publish` call.
    cached_slowest: u64,
}

unsafe impl<T: Copy + Send> Send for Publisher<T> {}

impl<T: Copy> Publisher<T> {
    /// Write a single value to the ring without any backpressure check.
    /// This is the raw publish path used by both `publish()` (lossy) and
    /// `try_publish()` (after backpressure check passes).
    #[inline]
    fn publish_unchecked(&mut self, value: T) {
        // SAFETY: slots_ptr is valid for the lifetime of self.ring (Arc-owned).
        // Index is masked to stay within the allocated slot array.
        let slot = unsafe { &*self.slots_ptr.add((self.seq & self.mask) as usize) };
        slot.write(self.seq, value);
        // SAFETY: cursor_ptr points to ring.cursor.0, kept alive by self.ring.
        unsafe { &*self.cursor_ptr }.store(self.seq, Ordering::Release);
        self.seq += 1;
    }

    /// Publish by writing directly into the slot via a closure.
    ///
    /// The closure receives a `&mut MaybeUninit<T>`, allowing in-place
    /// construction that can eliminate the write-side `memcpy` when the
    /// compiler constructs the value directly in slot memory.
    ///
    /// This is the lossy (no backpressure) path. For bounded channels,
    /// prefer [`publish()`](Self::publish) with a pre-built value.
    ///
    /// # Example
    ///
    /// ```
    /// use std::mem::MaybeUninit;
    /// let (mut p, s) = photon_ring::channel::<u64>(64);
    /// let mut sub = s.subscribe();
    /// p.publish_with(|slot| { slot.write(42u64); });
    /// assert_eq!(sub.try_recv(), Ok(42));
    /// ```
    #[inline]
    pub fn publish_with(&mut self, f: impl FnOnce(&mut core::mem::MaybeUninit<T>)) {
        // SAFETY: see publish_unchecked.
        let slot = unsafe { &*self.slots_ptr.add((self.seq & self.mask) as usize) };
        slot.write_with(self.seq, f);
        unsafe { &*self.cursor_ptr }.store(self.seq, Ordering::Release);
        self.seq += 1;
    }

    /// Publish a single value. Zero-allocation, O(1).
    ///
    /// On a bounded channel (created with [`channel_bounded()`]), this method
    /// spin-waits until there is room in the ring, ensuring no message loss.
    /// On a regular (lossy) channel, this publishes immediately without any
    /// backpressure check.
    #[inline]
    pub fn publish(&mut self, value: T) {
        if self.ring.backpressure.is_some() {
            let mut v = value;
            loop {
                match self.try_publish(v) {
                    Ok(()) => return,
                    Err(PublishError::Full(returned)) => {
                        v = returned;
                        core::hint::spin_loop();
                    }
                }
            }
        }
        self.publish_unchecked(value);
    }

    /// Try to publish a single value with backpressure awareness.
    ///
    /// - On a regular (lossy) channel created with [`channel()`], this always
    ///   succeeds — it publishes the value and returns `Ok(())`.
    /// - On a bounded channel created with [`channel_bounded()`], this checks
    ///   whether the slowest subscriber has fallen too far behind. If
    ///   `publisher_seq - slowest_cursor >= capacity - watermark`, it returns
    ///   `Err(PublishError::Full(value))` without writing.
    #[inline]
    pub fn try_publish(&mut self, value: T) -> Result<(), PublishError<T>> {
        if let Some(bp) = self.ring.backpressure.as_ref() {
            let capacity = self.ring.capacity();
            let effective = capacity - bp.watermark;

            // Fast path: use cached slowest cursor.
            if self.seq >= self.cached_slowest + effective {
                // Slow path: rescan all trackers.
                match self.ring.slowest_cursor() {
                    Some(slowest) => {
                        self.cached_slowest = slowest;
                        if self.seq >= slowest + effective {
                            return Err(PublishError::Full(value));
                        }
                    }
                    None => {
                        // No subscribers registered yet — ring is unbounded.
                    }
                }
            }
        }
        self.publish_unchecked(value);
        Ok(())
    }

    /// Publish a batch of values.
    ///
    /// On a **lossy** channel: writes all values with a single cursor update
    /// at the end — consumers see the entire batch appear at once, and
    /// cache-line bouncing on the shared cursor is reduced to one store.
    ///
    /// On a **bounded** channel: spin-waits for room before each value,
    /// ensuring no message loss. The cursor advances per-value (not batched),
    /// so consumers may observe a partial batch during publication.
    #[inline]
    pub fn publish_batch(&mut self, values: &[T]) {
        if values.is_empty() {
            return;
        }
        if self.ring.backpressure.is_some() {
            for &v in values.iter() {
                let mut val = v;
                loop {
                    match self.try_publish(val) {
                        Ok(()) => break,
                        Err(PublishError::Full(returned)) => {
                            val = returned;
                            core::hint::spin_loop();
                        }
                    }
                }
            }
            return;
        }
        for (i, &v) in values.iter().enumerate() {
            let seq = self.seq + i as u64;
            // SAFETY: see publish_unchecked.
            let slot = unsafe { &*self.slots_ptr.add((seq & self.mask) as usize) };
            slot.write(seq, v);
        }
        let last = self.seq + values.len() as u64 - 1;
        unsafe { &*self.cursor_ptr }.store(last, Ordering::Release);
        self.seq += values.len() as u64;
    }

    /// Number of messages published so far.
    #[inline]
    pub fn published(&self) -> u64 {
        self.seq
    }

    /// Current sequence number (same as `published()`).
    /// Useful for computing lag: `publisher.sequence() - subscriber.cursor`.
    #[inline]
    pub fn sequence(&self) -> u64 {
        self.seq
    }

    /// Ring capacity (power of two).
    #[inline]
    pub fn capacity(&self) -> u64 {
        self.ring.capacity()
    }

    /// Lock the ring buffer pages in RAM, preventing the OS from swapping
    /// them to disk. Reduces worst-case latency by eliminating page-fault
    /// stalls on the hot path.
    ///
    /// Returns `true` on success. Requires `CAP_IPC_LOCK` or sufficient
    /// `RLIMIT_MEMLOCK` on Linux. No-op on other platforms.
    #[cfg(all(target_os = "linux", feature = "hugepages"))]
    pub fn mlock(&self) -> bool {
        let ptr = self.ring.slots_ptr() as *const u8;
        let len = self.ring.slots_byte_len();
        unsafe { crate::mem::mlock_pages(ptr, len) }
    }

    /// Pre-fault all ring buffer pages by writing a zero byte to each 4 KiB
    /// page. Ensures the first publish does not trigger a page fault.
    ///
    /// # Safety
    ///
    /// Must be called before any publish/subscribe operations begin.
    /// Calling this while the ring is in active use is undefined behavior
    /// because it writes zero bytes to live ring memory via raw pointers,
    /// which can corrupt slot data and seqlock stamps.
    #[cfg(all(target_os = "linux", feature = "hugepages"))]
    pub unsafe fn prefault(&self) {
        let ptr = self.ring.slots_ptr() as *mut u8;
        let len = self.ring.slots_byte_len();
        crate::mem::prefault_pages(ptr, len)
    }
}

// ---------------------------------------------------------------------------
// Subscribable (factory for subscribers)
// ---------------------------------------------------------------------------

/// Clone-able handle for spawning [`Subscriber`]s.
///
/// Send this to other threads and call [`subscribe`](Subscribable::subscribe)
/// to create independent consumers.
pub struct Subscribable<T: Copy> {
    ring: Arc<SharedRing<T>>,
}

impl<T: Copy> Clone for Subscribable<T> {
    fn clone(&self) -> Self {
        Subscribable {
            ring: self.ring.clone(),
        }
    }
}

unsafe impl<T: Copy + Send> Send for Subscribable<T> {}
unsafe impl<T: Copy + Send> Sync for Subscribable<T> {}

impl<T: Copy> Subscribable<T> {
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
            count: N,
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
}

// ---------------------------------------------------------------------------
// Subscriber (consumer read side)
// ---------------------------------------------------------------------------

/// The read side of a Photon SPMC channel.
///
/// Each subscriber has its own cursor — no contention between consumers.
pub struct Subscriber<T: Copy> {
    ring: Arc<SharedRing<T>>,
    /// Cached raw pointer to the slot array. Avoids Arc + Box deref on the
    /// hot path. Valid for the lifetime of `ring` (the Arc keeps it alive).
    slots_ptr: *const Slot<T>,
    /// Cached ring mask (`capacity - 1`). Immutable after construction.
    mask: u64,
    cursor: u64,
    /// Per-subscriber cursor tracker for backpressure. `None` on regular
    /// (lossy) channels — zero overhead.
    tracker: Option<Arc<Padded<AtomicU64>>>,
    /// Cumulative messages skipped due to lag.
    total_lagged: u64,
    /// Cumulative messages successfully received.
    total_received: u64,
}

unsafe impl<T: Copy + Send> Send for Subscriber<T> {}

impl<T: Copy> Subscriber<T> {
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
        let mut iter: u32 = 0;
        loop {
            match self.try_recv() {
                Ok(val) => return val,
                Err(TryRecvError::Empty) => {
                    strategy.wait(iter);
                    iter = iter.saturating_add(1);
                }
                Err(TryRecvError::Lagged { .. }) => {
                    // Cursor was advanced by try_recv — retry immediately.
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

impl<T: Copy> Drop for Subscriber<T> {
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
// SubscriberGroup (batched multi-consumer read)
// ---------------------------------------------------------------------------

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
pub struct SubscriberGroup<T: Copy, const N: usize> {
    ring: Arc<SharedRing<T>>,
    /// Cached raw pointer to the slot array. Avoids Arc + Box deref on the
    /// hot path. Valid for the lifetime of `ring` (the Arc keeps it alive).
    slots_ptr: *const Slot<T>,
    /// Cached ring mask (`capacity - 1`). Immutable after construction.
    mask: u64,
    /// Single cursor shared by all `N` logical subscribers.
    cursor: u64,
    /// Number of logical subscribers in this group (always `N`).
    count: usize,
    /// Cumulative messages skipped due to lag.
    total_lagged: u64,
    /// Cumulative messages successfully received.
    total_received: u64,
    /// Per-group cursor tracker for backpressure. `None` on regular
    /// (lossy) channels — zero overhead.
    tracker: Option<Arc<Padded<AtomicU64>>>,
}

unsafe impl<T: Copy + Send, const N: usize> Send for SubscriberGroup<T, N> {}

impl<T: Copy, const N: usize> SubscriberGroup<T, N> {
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
    #[inline]
    pub fn recv(&mut self) -> T {
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
        self.count
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

    /// Update the backpressure tracker to reflect the current cursor position.
    /// No-op on regular (lossy) channels.
    #[inline]
    fn update_tracker(&self) {
        if let Some(ref tracker) = self.tracker {
            tracker.0.store(self.cursor, Ordering::Relaxed);
        }
    }
}

impl<T: Copy, const N: usize> Drop for SubscriberGroup<T, N> {
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
// Constructors
// ---------------------------------------------------------------------------

/// Create a Photon SPMC channel.
///
/// `capacity` must be a power of two (>= 2). Returns the single-producer
/// write end and a clone-able factory for creating consumers.
///
/// # Example
/// ```
/// let (mut pub_, subs) = photon_ring::channel::<u64>(64);
/// let mut sub = subs.subscribe();
/// pub_.publish(42);
/// assert_eq!(sub.try_recv(), Ok(42));
/// ```
pub fn channel<T: Copy + Send>(capacity: usize) -> (Publisher<T>, Subscribable<T>) {
    let ring = Arc::new(SharedRing::new(capacity));
    let slots_ptr = ring.slots_ptr();
    let mask = ring.mask;
    let cursor_ptr = ring.cursor_ptr();
    (
        Publisher {
            ring: ring.clone(),
            slots_ptr,
            mask,
            cursor_ptr,
            seq: 0,
            cached_slowest: 0,
        },
        Subscribable { ring },
    )
}

/// Create a backpressure-capable SPMC channel.
///
/// The publisher will refuse to publish (returning [`PublishError::Full`])
/// when it would overwrite a slot that the slowest subscriber hasn't
/// read yet, minus `watermark` slots of headroom.
///
/// Unlike the default lossy [`channel()`], no messages are ever dropped.
///
/// # Arguments
/// - `capacity` — ring size, must be a power of two (>= 2).
/// - `watermark` — headroom slots; must be less than `capacity`.
///   A watermark of 0 means the publisher blocks as soon as all slots are
///   occupied. A watermark of `capacity - 1` means it blocks when only one
///   slot is free.
///
/// # Example
/// ```
/// use photon_ring::channel_bounded;
/// use photon_ring::PublishError;
///
/// let (mut p, s) = channel_bounded::<u64>(4, 0);
/// let mut sub = s.subscribe();
///
/// // Fill the ring (4 slots).
/// for i in 0u64..4 {
///     p.try_publish(i).unwrap();
/// }
///
/// // Ring is full — backpressure kicks in.
/// assert_eq!(p.try_publish(99u64), Err(PublishError::Full(99)));
///
/// // Drain one slot — publisher can continue.
/// assert_eq!(sub.try_recv(), Ok(0));
/// p.try_publish(99).unwrap();
/// ```
pub fn channel_bounded<T: Copy + Send>(
    capacity: usize,
    watermark: usize,
) -> (Publisher<T>, Subscribable<T>) {
    let ring = Arc::new(SharedRing::new_bounded(capacity, watermark));
    let slots_ptr = ring.slots_ptr();
    let mask = ring.mask;
    let cursor_ptr = ring.cursor_ptr();
    (
        Publisher {
            ring: ring.clone(),
            slots_ptr,
            mask,
            cursor_ptr,
            seq: 0,
            cached_slowest: 0,
        },
        Subscribable { ring },
    )
}

// ---------------------------------------------------------------------------
// MpPublisher (multi-producer write side)
// ---------------------------------------------------------------------------

/// The write side of a Photon MPMC channel.
///
/// Unlike [`Publisher`], `MpPublisher` is `Clone + Send + Sync` — multiple
/// threads can publish concurrently. Sequence numbers are claimed atomically
/// via `fetch_add` on a shared counter, and the cursor is advanced with a
/// single best-effort CAS (no spin loop). Consumers use stamp-based reading,
/// so the cursor only needs to be eventually consistent for `subscribe()`,
/// `latest()`, and `pending()`.
///
/// Created via [`channel_mpmc()`].
pub struct MpPublisher<T: Copy> {
    ring: Arc<SharedRing<T>>,
    /// Cached raw pointer to the slot array. Avoids Arc + Box deref on the
    /// hot path. Valid for the lifetime of `ring` (the Arc keeps it alive).
    slots_ptr: *const Slot<T>,
    /// Cached ring mask (`capacity - 1`). Immutable after construction.
    mask: u64,
    /// Cached raw pointer to `ring.cursor.0`. Avoids Arc deref on hot path.
    cursor_ptr: *const AtomicU64,
    /// Cached raw pointer to `ring.next_seq`. Avoids Arc deref + Option
    /// unwrap on hot path.
    next_seq_ptr: *const AtomicU64,
}

impl<T: Copy> Clone for MpPublisher<T> {
    fn clone(&self) -> Self {
        MpPublisher {
            ring: self.ring.clone(),
            slots_ptr: self.slots_ptr,
            mask: self.mask,
            cursor_ptr: self.cursor_ptr,
            next_seq_ptr: self.next_seq_ptr,
        }
    }
}

// Safety: MpPublisher uses atomic CAS for all shared state.
// No mutable fields — all coordination is via atomics on SharedRing.
unsafe impl<T: Copy + Send> Send for MpPublisher<T> {}
unsafe impl<T: Copy + Send> Sync for MpPublisher<T> {}

impl<T: Copy> MpPublisher<T> {
    /// Publish a single value. Zero-allocation, O(1) amortised.
    ///
    /// Multiple threads may call this concurrently. Each call atomically
    /// claims a sequence number, writes the slot using the seqlock protocol,
    /// then advances the shared cursor.
    ///
    /// Instead of spinning on the cursor CAS (which serializes all
    /// producers on one cache line), this implementation waits for the
    /// predecessor's **slot stamp** to become committed. Stamp checks
    /// distribute contention across per-slot cache lines, avoiding the
    /// single-point serialization bottleneck. Once the predecessor is
    /// confirmed done, a single CAS advances the cursor, followed by a
    /// catch-up loop to absorb any successors that are also done.
    #[inline]
    pub fn publish(&self, value: T) {
        // SAFETY: next_seq_ptr points to ring.next_seq (MPMC ring), kept alive by self.ring.
        let next_seq_atomic = unsafe { &*self.next_seq_ptr };
        let seq = next_seq_atomic.fetch_add(1, Ordering::Relaxed);
        // SAFETY: slots_ptr is valid for the lifetime of self.ring (Arc-owned).
        let slot = unsafe { &*self.slots_ptr.add((seq & self.mask) as usize) };
        slot.write(seq, value);
        self.advance_cursor(seq);
    }

    /// Publish by writing directly into the slot via a closure.
    ///
    /// Like [`publish`](Self::publish), but the closure receives a
    /// `&mut MaybeUninit<T>` for in-place construction, potentially
    /// eliminating a write-side `memcpy`.
    ///
    /// # Example
    ///
    /// ```
    /// use std::mem::MaybeUninit;
    /// let (p, subs) = photon_ring::channel_mpmc::<u64>(64);
    /// let mut sub = subs.subscribe();
    /// p.publish_with(|slot| { slot.write(42u64); });
    /// assert_eq!(sub.try_recv(), Ok(42));
    /// ```
    #[inline]
    pub fn publish_with(&self, f: impl FnOnce(&mut core::mem::MaybeUninit<T>)) {
        // SAFETY: next_seq_ptr points to ring.next_seq (MPMC ring), kept alive by self.ring.
        let next_seq_atomic = unsafe { &*self.next_seq_ptr };
        let seq = next_seq_atomic.fetch_add(1, Ordering::Relaxed);
        // SAFETY: slots_ptr is valid for the lifetime of self.ring (Arc-owned).
        let slot = unsafe { &*self.slots_ptr.add((seq & self.mask) as usize) };
        slot.write_with(seq, f);
        self.advance_cursor(seq);
    }

    /// Number of messages claimed so far (across all clones).
    ///
    /// This reads the shared atomic counter — the value may be slightly
    /// ahead of the cursor if some producers haven't committed yet.
    #[inline]
    pub fn published(&self) -> u64 {
        // SAFETY: next_seq_ptr points to ring.next_seq, kept alive by self.ring.
        unsafe { &*self.next_seq_ptr }.load(Ordering::Relaxed)
    }

    /// Ring capacity (power of two).
    #[inline]
    pub fn capacity(&self) -> u64 {
        self.ring.capacity()
    }

    /// Advance the shared cursor after writing seq.
    ///
    /// Fast path: single CAS attempt (`cursor: seq-1 -> seq`). In the
    /// uncontended case this succeeds immediately and has the same cost
    /// as the original implementation.
    ///
    /// Contended path: if the CAS fails (predecessor not done yet), we
    /// wait on the predecessor's **slot stamp** instead of retrying the
    /// cursor CAS. Stamp polling distributes contention across per-slot
    /// cache lines, avoiding the single-point serialization bottleneck
    /// of the cursor-CAS spin loop.
    #[inline]
    fn advance_cursor(&self, seq: u64) {
        // SAFETY: cursor_ptr points to ring.cursor.0, kept alive by self.ring.
        let cursor_atomic = unsafe { &*self.cursor_ptr };
        let expected_cursor = if seq == 0 { u64::MAX } else { seq - 1 };

        // Fast path: single CAS — succeeds immediately when uncontended.
        if cursor_atomic
            .compare_exchange(expected_cursor, seq, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            self.catch_up_cursor(seq);
            return;
        }

        // Contended path: predecessor hasn't committed yet.
        // Wait on predecessor's slot stamp (per-slot cache line) instead
        // of retrying the cursor CAS (shared cache line).
        if seq > 0 {
            // SAFETY: slots_ptr is valid for the lifetime of self.ring.
            let pred_slot = unsafe { &*self.slots_ptr.add(((seq - 1) & self.mask) as usize) };
            let pred_done = (seq - 1) * 2 + 2;
            // Check stamp >= pred_done to handle rare ring-wrap case where
            // a later sequence already overwrote the predecessor's slot.
            while pred_slot.stamp_load() < pred_done {
                core::hint::spin_loop();
            }
        }

        // Predecessor is done — advance cursor with a single CAS.
        let _ = cursor_atomic.compare_exchange(
            expected_cursor,
            seq,
            Ordering::Release,
            Ordering::Relaxed,
        );
        // If we won the CAS, absorb any successors that are also done.
        if cursor_atomic.load(Ordering::Relaxed) == seq {
            self.catch_up_cursor(seq);
        }
    }

    /// After successfully advancing the cursor to `seq`, check whether
    /// later producers (seq+1, seq+2, ...) have already committed their
    /// slots. If so, advance the cursor past them in one pass.
    ///
    /// In the common (uncontended) case the first stamp check fails
    /// immediately and the loop body never runs.
    #[inline]
    fn catch_up_cursor(&self, mut seq: u64) {
        // SAFETY: all cached pointers are valid for the lifetime of self.ring.
        let cursor_atomic = unsafe { &*self.cursor_ptr };
        let next_seq_atomic = unsafe { &*self.next_seq_ptr };
        loop {
            let next = seq + 1;
            // Don't advance past what has been claimed.
            if next >= next_seq_atomic.load(Ordering::Acquire) {
                break;
            }
            // Check if the next slot's stamp shows a completed write.
            let done_stamp = next * 2 + 2;
            let slot = unsafe { &*self.slots_ptr.add((next & self.mask) as usize) };
            if slot.stamp_load() != done_stamp {
                break;
            }
            // Slot is committed — try to advance cursor.
            if cursor_atomic
                .compare_exchange(seq, next, Ordering::Release, Ordering::Relaxed)
                .is_err()
            {
                break;
            }
            seq = next;
        }
    }
}

/// Create a Photon MPMC (multi-producer, multi-consumer) channel.
///
/// `capacity` must be a power of two (>= 2). Returns a clone-able
/// [`MpPublisher`] and the same [`Subscribable`] factory used by SPMC
/// channels.
///
/// Multiple threads can clone the publisher and publish concurrently.
/// Subscribers work identically to the SPMC case.
///
/// # Example
/// ```
/// let (pub_, subs) = photon_ring::channel_mpmc::<u64>(64);
/// let mut sub = subs.subscribe();
///
/// let pub2 = pub_.clone();
/// pub_.publish(1);
/// pub2.publish(2);
///
/// assert_eq!(sub.try_recv(), Ok(1));
/// assert_eq!(sub.try_recv(), Ok(2));
/// ```
pub fn channel_mpmc<T: Copy + Send>(capacity: usize) -> (MpPublisher<T>, Subscribable<T>) {
    let ring = Arc::new(SharedRing::new_mpmc(capacity));
    let slots_ptr = ring.slots_ptr();
    let mask = ring.mask;
    let cursor_ptr = ring.cursor_ptr();
    let next_seq_ptr = &ring
        .next_seq
        .as_ref()
        .expect("MPMC ring must have next_seq")
        .0 as *const AtomicU64;
    (
        MpPublisher {
            ring: ring.clone(),
            slots_ptr,
            mask,
            cursor_ptr,
            next_seq_ptr,
        },
        Subscribable { ring },
    )
}
