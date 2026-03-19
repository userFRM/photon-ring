// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::pod::Pod;
use crate::slot::Slot;
use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::AtomicU64;
use spin::Mutex;

/// Cache-line padding to prevent false sharing between hot atomics.
///
/// Wraps a value with `#[repr(align(64))]` to ensure it occupies its
/// own cache line. Used internally for cursor trackers and the ring
/// cursor.
///
/// Exposed publicly so that [`DependencyBarrier::new`](crate::DependencyBarrier::new)
/// and [`Subscriber::tracker`](crate::Subscriber::tracker) can refer to
/// `Arc<Padded<AtomicU64>>` in their signatures.
#[repr(align(64))]
pub struct Padded<T>(pub T);

// ---------------------------------------------------------------------------
// RingIndex — encapsulates slot indexing for both pow2 and arbitrary capacity
// ---------------------------------------------------------------------------

/// Precomputed indexing constants for mapping sequence numbers to ring slots.
///
/// For power-of-two capacities, uses bitwise AND (single-cycle, ~0.3 ns).
/// For arbitrary capacities, uses Lemire's fastmod algorithm (~1.5 ns):
/// two 64-bit multiplies with no division instruction.
///
/// Reference: Daniel Lemire, "Faster Remainder by Direct Computation" (2019),
/// <https://arxiv.org/abs/1902.01961>
#[derive(Clone, Copy)]
pub(crate) struct RingIndex {
    /// Ring capacity.
    pub(crate) capacity: u64,
    /// For power-of-two: `capacity - 1`. For arbitrary: unused but harmless.
    pub(crate) mask: u64,
    /// Precomputed reciprocal for fast modulo: `floor(2^64 / capacity)`.
    /// Used to approximate `n / capacity` via `mulhi(n, reciprocal)`.
    pub(crate) reciprocal: u64,
    /// True if capacity is a power of two (use AND instead of fastmod).
    pub(crate) is_pow2: bool,
}

impl RingIndex {
    /// Create a new `RingIndex` for the given capacity.
    ///
    /// # Panics
    ///
    /// Panics if `capacity < 2`.
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(capacity >= 2, "capacity must be at least 2");
        let cap = capacity as u64;
        let is_pow2 = capacity.is_power_of_two();
        let mask = if is_pow2 { cap - 1 } else { 0 };
        // Reciprocal: floor(2^64 / d). Used with mulhi to approximate n/d.
        let reciprocal = ((1u128 << 64) / cap as u128) as u64;
        RingIndex {
            capacity: cap,
            mask,
            reciprocal,
            is_pow2,
        }
    }

    /// Map a sequence number to a slot index.
    ///
    /// Sub-1 ns for power-of-two (bitwise AND), ~1.5 ns for arbitrary
    /// capacity (Lemire fastmod -- two `MUL` instructions, no division).
    ///
    /// Note: hot-path structs (Publisher, Subscriber, etc.) inline their
    /// own copy of this logic to avoid the indirection through RingIndex.
    /// This method is the canonical reference, used by tests.
    #[cfg(test)]
    #[inline(always)]
    pub(crate) fn slot(&self, seq: u64) -> usize {
        if self.is_pow2 {
            (seq & self.mask) as usize
        } else {
            self.fastmod(seq) as usize
        }
    }

    /// Fast modulo: compute `n % capacity` without division.
    ///
    /// Algorithm: approximate quotient via `q = mulhi(n, M)`, then
    /// `r = n - q * d`, with a single conditional correction for the
    /// off-by-one case. Correct for all `n` in `[0, u64::MAX]`.
    #[cfg(test)]
    #[inline(always)]
    fn fastmod(&self, n: u64) -> u64 {
        let q = ((n as u128 * self.reciprocal as u128) >> 64) as u64;
        let mut r = n - q.wrapping_mul(self.capacity);
        if r >= self.capacity {
            r -= self.capacity;
        }
        r
    }
}

/// Backpressure state attached to a [`SharedRing`] when created via
/// [`channel_bounded`](crate::channel::channel_bounded).
pub(crate) struct BackpressureState {
    /// How many slots of headroom to leave between the publisher and the
    /// slowest subscriber.
    pub(crate) watermark: u64,
    /// Per-subscriber cursor trackers (weak references). The publisher scans
    /// these to find the minimum (slowest) cursor when it is close to lapping.
    /// Weak references prevent a panicked subscriber (that fails to drop) from
    /// blocking the publisher forever.
    pub(crate) trackers: Mutex<Vec<Weak<Padded<AtomicU64>>>>,
}

/// Shared ring buffer: a pre-allocated array of seqlock-stamped slots
/// plus the producer cursor.
///
/// The cursor stores the sequence number of the last published message
/// (`u64::MAX` means nothing published yet).
pub(crate) struct SharedRing<T> {
    slots: Box<[Slot<T>]>,
    pub(crate) index: RingIndex,
    pub(crate) cursor: Padded<AtomicU64>,
    /// Present only for bounded (backpressure-capable) channels.
    pub(crate) backpressure: Option<BackpressureState>,
    /// Shared sequence counter for multi-producer channels.
    /// `None` for SPMC channels, `Some` for MPMC channels.
    pub(crate) next_seq: Option<Padded<AtomicU64>>,
}

impl<T: Pod> SharedRing<T> {
    pub(crate) fn new(capacity: usize) -> Self {
        let index = RingIndex::new(capacity);

        let slots: Vec<Slot<T>> = (0..capacity).map(|_| Slot::new()).collect();

        SharedRing {
            slots: slots.into_boxed_slice(),
            index,
            cursor: Padded(AtomicU64::new(u64::MAX)),
            backpressure: None,
            next_seq: None,
        }
    }

    pub(crate) fn new_bounded(capacity: usize, watermark: usize) -> Self {
        assert!(capacity >= 2, "capacity must be at least 2");
        assert!(watermark < capacity, "watermark must be less than capacity");

        let index = RingIndex::new(capacity);
        let slots: Vec<Slot<T>> = (0..capacity).map(|_| Slot::new()).collect();

        SharedRing {
            slots: slots.into_boxed_slice(),
            index,
            cursor: Padded(AtomicU64::new(u64::MAX)),
            backpressure: Some(BackpressureState {
                watermark: watermark as u64,
                trackers: Mutex::new(Vec::new()),
            }),
            next_seq: None,
        }
    }

    pub(crate) fn new_mpmc(capacity: usize) -> Self {
        let index = RingIndex::new(capacity);

        let slots: Vec<Slot<T>> = (0..capacity).map(|_| Slot::new()).collect();

        SharedRing {
            slots: slots.into_boxed_slice(),
            index,
            cursor: Padded(AtomicU64::new(u64::MAX)),
            backpressure: None,
            next_seq: Some(Padded(AtomicU64::new(0))),
        }
    }

    /// Raw pointer to the start of the slot array.
    #[inline]
    pub(crate) fn slots_ptr(&self) -> *const Slot<T> {
        self.slots.as_ptr()
    }

    /// Raw pointer to the cursor atomic.
    #[inline]
    pub(crate) fn cursor_ptr(&self) -> *const AtomicU64 {
        &self.cursor.0 as *const AtomicU64
    }

    /// Total byte length of the slot array.
    #[cfg(all(target_os = "linux", feature = "hugepages"))]
    #[inline]
    pub(crate) fn slots_byte_len(&self) -> usize {
        self.slots.len() * core::mem::size_of::<Slot<T>>()
    }

    #[inline]
    pub(crate) fn capacity(&self) -> u64 {
        self.index.capacity
    }

    /// Register a new subscriber tracker and return it.
    /// Only meaningful when backpressure is enabled; returns `None` otherwise.
    pub(crate) fn register_tracker(&self, initial: u64) -> Option<Arc<Padded<AtomicU64>>> {
        let bp = self.backpressure.as_ref()?;
        let tracker = Arc::new(Padded(AtomicU64::new(initial)));
        bp.trackers.lock().push(Arc::downgrade(&tracker));
        Some(tracker)
    }

    /// Scan all subscriber trackers and return the minimum cursor.
    /// Returns `None` if there are no live subscribers. Dead (dropped)
    /// trackers are pruned during the scan.
    #[inline]
    pub(crate) fn slowest_cursor(&self) -> Option<u64> {
        let bp = self.backpressure.as_ref()?;
        let mut trackers = bp.trackers.lock();
        let mut min = u64::MAX;
        let mut has_live = false;
        trackers.retain(|weak| {
            if let Some(arc) = weak.upgrade() {
                let val = arc.0.load(core::sync::atomic::Ordering::Relaxed);
                if val < min {
                    min = val;
                }
                has_live = true;
                true // retain live tracker
            } else {
                false // prune dead tracker
            }
        });
        if has_live {
            Some(min)
        } else {
            None
        }
    }
}
