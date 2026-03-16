// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::slot::Slot;
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::AtomicU64;
use spin::Mutex;

/// Cache-line padding to prevent false sharing between hot atomics.
#[repr(align(64))]
pub(crate) struct Padded<T>(pub(crate) T);

/// Backpressure state attached to a [`SharedRing`] when created via
/// [`channel_bounded`](crate::channel::channel_bounded).
pub(crate) struct BackpressureState {
    /// How many slots of headroom to leave between the publisher and the
    /// slowest subscriber.
    pub(crate) watermark: u64,
    /// Per-subscriber cursor trackers. The publisher scans these to find the
    /// minimum (slowest) cursor when it is close to lapping.
    pub(crate) trackers: Mutex<Vec<Arc<Padded<AtomicU64>>>>,
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
}

impl<T: Copy> SharedRing<T> {
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
        }
    }

    #[inline]
    pub(crate) fn slot(&self, seq: u64) -> &Slot<T> {
        unsafe { self.slots.get_unchecked((seq & self.mask) as usize) }
    }

    /// Raw pointer to the start of the slot array.
    #[allow(dead_code)] // used only with `hugepages` feature
    #[inline]
    pub(crate) fn slots_ptr(&self) -> *const Slot<T> {
        self.slots.as_ptr()
    }

    /// Total byte length of the slot array.
    #[allow(dead_code)] // used only with `hugepages` feature
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
        bp.trackers.lock().push(tracker.clone());
        Some(tracker)
    }

    /// Scan all subscriber trackers and return the minimum cursor.
    /// Returns `None` if there are no subscribers.
    #[inline]
    pub(crate) fn slowest_cursor(&self) -> Option<u64> {
        let bp = self.backpressure.as_ref()?;
        let trackers = bp.trackers.lock();
        if trackers.is_empty() {
            return None;
        }
        let mut min = u64::MAX;
        for t in trackers.iter() {
            let val = t.0.load(core::sync::atomic::Ordering::Acquire);
            if val < min {
                min = val;
            }
        }
        Some(min)
    }
}
