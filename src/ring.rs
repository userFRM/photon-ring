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
    pub(crate) mask: u64,
    pub(crate) cursor: Padded<AtomicU64>,
    /// Present only for bounded (backpressure-capable) channels.
    pub(crate) backpressure: Option<BackpressureState>,
    /// Shared sequence counter for multi-producer channels.
    /// `None` for SPMC channels, `Some` for MPMC channels.
    pub(crate) next_seq: Option<Padded<AtomicU64>>,
}

impl<T: Pod> SharedRing<T> {
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(
            capacity.is_power_of_two(),
            "capacity must be a power of two"
        );
        assert!(capacity >= 2, "capacity must be at least 2");

        let slots: Vec<Slot<T>> = (0..capacity).map(|_| Slot::new()).collect();

        SharedRing {
            slots: slots.into_boxed_slice(),
            mask: (capacity - 1) as u64,
            cursor: Padded(AtomicU64::new(u64::MAX)),
            backpressure: None,
            next_seq: None,
        }
    }

    pub(crate) fn new_bounded(capacity: usize, watermark: usize) -> Self {
        assert!(
            capacity.is_power_of_two(),
            "capacity must be a power of two"
        );
        assert!(capacity >= 2, "capacity must be at least 2");
        assert!(watermark < capacity, "watermark must be less than capacity");

        let slots: Vec<Slot<T>> = (0..capacity).map(|_| Slot::new()).collect();

        SharedRing {
            slots: slots.into_boxed_slice(),
            mask: (capacity - 1) as u64,
            cursor: Padded(AtomicU64::new(u64::MAX)),
            backpressure: Some(BackpressureState {
                watermark: watermark as u64,
                trackers: Mutex::new(Vec::new()),
            }),
            next_seq: None,
        }
    }

    pub(crate) fn new_mpmc(capacity: usize) -> Self {
        assert!(
            capacity.is_power_of_two(),
            "capacity must be a power of two"
        );
        assert!(capacity >= 2, "capacity must be at least 2");

        let slots: Vec<Slot<T>> = (0..capacity).map(|_| Slot::new()).collect();

        SharedRing {
            slots: slots.into_boxed_slice(),
            mask: (capacity - 1) as u64,
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
        self.mask + 1
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
