// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::pod::Pod;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr;
use core::sync::atomic::{fence, AtomicU64, Ordering};

/// A cache-line-aligned slot holding a seqlock stamp and a payload.
///
/// The stamp co-locates with the value in the same cache line (for T ≤ 56 bytes),
/// eliminating an extra cache miss on reads. The encoding:
///
/// - `stamp = seq * 2 + 1` — write in progress for sequence `seq`
/// - `stamp = seq * 2 + 2` — write complete for sequence `seq`
/// - `stamp = 0`           — never written
#[repr(C, align(64))]
pub(crate) struct Slot<T> {
    stamp: AtomicU64,
    value: UnsafeCell<MaybeUninit<T>>,
}

// Safety: Access is coordinated by the seqlock protocol.
// Only one writer (Publisher) writes to a slot at a time.
// Readers (Subscribers) use the stamp to detect torn reads.
unsafe impl<T: Send> Sync for Slot<T> {}
unsafe impl<T: Send> Send for Slot<T> {}

// Compile-time verification that Slot<u64> is cache-line aligned.
// This is guaranteed by #[repr(C, align(64))], but we assert it as a safety
// net — any future layout change will trigger a build failure.
const _: () = assert!(core::mem::align_of::<Slot<u64>>() == 64);

impl<T> Slot<T> {
    pub(crate) fn new() -> Self {
        Slot {
            stamp: AtomicU64::new(0),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Load the stamp with Acquire ordering. Used by MPMC catch-up to
    /// check whether a successor slot has been committed.
    #[inline]
    pub(crate) fn stamp_load(&self) -> u64 {
        self.stamp.load(Ordering::Acquire)
    }
}

impl<T: Pod> Slot<T> {
    /// Seqlock write protocol. Single-writer only.
    ///
    /// 1. Store odd stamp (writing)
    /// 2. Release fence — odd stamp visible before data write
    /// 3. Write value
    /// 4. Release store of even stamp — data visible before done stamp
    #[inline]
    pub(crate) fn write(&self, seq: u64, value: T) {
        let writing = seq * 2 + 1;
        let done = seq * 2 + 2;

        self.stamp.store(writing, Ordering::Relaxed);
        fence(Ordering::Release);

        unsafe { ptr::write(self.value.get() as *mut T, value) };

        self.stamp.store(done, Ordering::Release);
    }

    /// Seqlock write via closure — enables in-place construction.
    #[inline]
    pub(crate) fn write_with(&self, seq: u64, f: impl FnOnce(&mut MaybeUninit<T>)) {
        let writing = seq * 2 + 1;
        let done = seq * 2 + 2;

        self.stamp.store(writing, Ordering::Relaxed);
        fence(Ordering::Release);

        f(unsafe { &mut *self.value.get() });

        self.stamp.store(done, Ordering::Release);
    }

    /// Seqlock read protocol. Returns `None` on torn read (caller should retry).
    ///
    /// Returns `Err(actual_stamp)` if the slot holds a different sequence.
    #[inline]
    pub(crate) fn try_read(&self, seq: u64) -> Result<Option<T>, u64> {
        let expected = seq * 2 + 2;

        let s1 = self.stamp.load(Ordering::Acquire);

        // Happy path first — stamp matches expected sequence
        if s1 == expected {
            let value = unsafe { ptr::read((*self.value.get()).as_ptr()) };
            let s2 = self.stamp.load(Ordering::Acquire);
            if s1 == s2 {
                return Ok(Some(value));
            }
            return Ok(None); // torn read
        }

        // Write in progress — caller should spin
        if s1 & 1 != 0 {
            return Ok(None);
        }

        // Wrong sequence (not written yet, or overwritten by a later sequence)
        Err(s1)
    }
}
