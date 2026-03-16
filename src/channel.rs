// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::ring::SharedRing;
use alloc::sync::Arc;
use core::sync::atomic::Ordering;

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
    seq: u64,
}

unsafe impl<T: Copy + Send> Send for Publisher<T> {}

impl<T: Copy> Publisher<T> {
    /// Publish a single value. Zero-allocation, O(1).
    #[inline]
    pub fn publish(&mut self, value: T) {
        self.ring.slot(self.seq).write(self.seq, value);
        self.ring.cursor.0.store(self.seq, Ordering::Release);
        self.seq += 1;
    }

    /// Publish a batch of values with a single cursor update.
    ///
    /// Each slot is written atomically (seqlock), but the cursor advances only
    /// once at the end — consumers see the entire batch appear at once, and
    /// cache-line bouncing on the shared cursor is reduced to one store.
    #[inline]
    pub fn publish_batch(&mut self, values: &[T]) {
        if values.is_empty() {
            return;
        }
        for (i, &v) in values.iter().enumerate() {
            let seq = self.seq + i as u64;
            self.ring.slot(seq).write(seq, v);
        }
        let last = self.seq + values.len() as u64 - 1;
        self.ring.cursor.0.store(last, Ordering::Release);
        self.seq += values.len() as u64;
    }

    /// Number of messages published so far.
    #[inline]
    pub fn published(&self) -> u64 {
        self.seq
    }

    /// Ring capacity (power of two).
    #[inline]
    pub fn capacity(&self) -> u64 {
        self.ring.capacity()
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
        Subscriber {
            ring: self.ring.clone(),
            cursor: start,
        }
    }

    /// Create a [`SubscriberGroup`] of `N` subscribers starting from the next
    /// message. All `N` logical subscribers share a single ring read — the
    /// seqlock is checked once and all cursors are advanced together.
    ///
    /// This is dramatically faster than `N` independent [`Subscriber`]s when
    /// polled in a loop on the same thread.
    pub fn subscribe_group<const N: usize>(&self) -> SubscriberGroup<T, N> {
        let head = self.ring.cursor.0.load(Ordering::Acquire);
        let start = if head == u64::MAX { 0 } else { head + 1 };
        SubscriberGroup {
            ring: self.ring.clone(),
            cursors: [start; N],
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
        Subscriber {
            ring: self.ring.clone(),
            cursor: start,
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
    cursor: u64,
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
        let slot = self.ring.slot(self.cursor);
        let expected = self.cursor * 2 + 2;
        // Phase 1: bare spin — no PAUSE, minimum wakeup latency
        for _ in 0..64 {
            match slot.try_read(self.cursor) {
                Ok(Some(value)) => {
                    self.cursor += 1;
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

    /// Stamp-only fast-path read. The consumer's local `self.cursor` tells us
    /// which slot and expected stamp to check — no shared cursor load needed
    /// on the hot path.
    #[inline]
    fn read_slot(&mut self) -> Result<T, TryRecvError> {
        let slot = self.ring.slot(self.cursor);
        let expected = self.cursor * 2 + 2;

        match slot.try_read(self.cursor) {
            Ok(Some(value)) => {
                self.cursor += 1;
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

// ---------------------------------------------------------------------------
// SubscriberGroup (batched multi-consumer read)
// ---------------------------------------------------------------------------

/// A group of `N` logical subscribers backed by a single ring read.
///
/// When all `N` cursors are at the same position (the common case),
/// [`try_recv`](SubscriberGroup::try_recv) performs **one** seqlock read
/// and advances all `N` cursors — reducing per-subscriber overhead from
/// ~1.1 ns to ~0.15 ns.
///
/// ```
/// let (mut p, subs) = photon_ring::channel::<u64>(64);
/// let mut group = subs.subscribe_group::<4>();
/// p.publish(42);
/// assert_eq!(group.try_recv(), Ok(42));
/// ```
pub struct SubscriberGroup<T: Copy, const N: usize> {
    ring: Arc<SharedRing<T>>,
    cursors: [u64; N],
}

unsafe impl<T: Copy + Send, const N: usize> Send for SubscriberGroup<T, N> {}

impl<T: Copy, const N: usize> SubscriberGroup<T, N> {
    /// Try to receive the next message for the group.
    ///
    /// On the fast path (all cursors aligned), this does a single seqlock
    /// read and sweeps all `N` cursors — the compiler unrolls the cursor
    /// increment loop for small `N`.
    #[inline]
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        // Fast path: all cursors at the same position (common case).
        let first = self.cursors[0];
        let slot = self.ring.slot(first);
        let expected = first * 2 + 2;

        match slot.try_read(first) {
            Ok(Some(value)) => {
                // Single seqlock read succeeded — advance all aligned cursors.
                for c in self.cursors.iter_mut() {
                    if *c == first {
                        *c = first + 1;
                    }
                }
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
                if head == u64::MAX || first > head {
                    return Err(TryRecvError::Empty);
                }
                if head >= cap {
                    let oldest = head - cap + 1;
                    if first < oldest {
                        let skipped = oldest - first;
                        for c in self.cursors.iter_mut() {
                            if *c < oldest {
                                *c = oldest;
                            }
                        }
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

    /// How many of the `N` cursors are at the minimum (aligned) position.
    pub fn aligned_count(&self) -> usize {
        let min = self.cursors.iter().copied().min().unwrap_or(0);
        self.cursors.iter().filter(|&&c| c == min).count()
    }

    /// Number of messages available (based on the slowest cursor).
    pub fn pending(&self) -> u64 {
        let head = self.ring.cursor.0.load(Ordering::Acquire);
        let min = self.cursors.iter().copied().min().unwrap_or(0);
        if head == u64::MAX || min > head {
            0
        } else {
            let raw = head - min + 1;
            raw.min(self.ring.capacity())
        }
    }
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

/// Create a Photon SPMC channel.
///
/// `capacity` must be a power of two (≥ 2). Returns the single-producer
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
    (
        Publisher {
            ring: ring.clone(),
            seq: 0,
        },
        Subscribable { ring },
    )
}
