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
    /// 3. Write value via `write_volatile` — prevents compiler elision and
    ///    is formally sound even when a concurrent reader observes a
    ///    partially-written value (the reader's `read_volatile` + stamp
    ///    re-check discards torn reads).
    /// 4. Release store of even stamp — data visible before done stamp
    #[inline]
    pub(crate) fn write(&self, seq: u64, value: T) {
        let writing = seq * 2 + 1;
        let done = seq * 2 + 2;

        self.stamp.store(writing, Ordering::Relaxed);
        fence(Ordering::Release);

        // SAFETY: single-writer guarantee — no concurrent writes to this slot.
        // write_volatile prevents the compiler from eliding or reordering the
        // store, and avoids formal UB from ptr::write on potentially-aliased
        // memory (a concurrent reader may be mid-read_volatile on the same bytes).
        unsafe { ptr::write_volatile(self.value.get() as *mut T, value) };

        self.stamp.store(done, Ordering::Release);
    }

    /// Seqlock write via closure — construct a value on the stack, then
    /// `write_volatile` it into the slot.
    ///
    /// The closure receives a `&mut MaybeUninit<T>` pointing to a **stack
    /// temporary**, not the slot itself. This avoids creating a `&mut`
    /// reference that aliases concurrent readers (which would be UB under
    /// the Rust abstract machine). After the closure returns, the fully
    /// initialized value is written into the slot with `write_volatile`,
    /// matching the soundness approach used by [`write()`](Self::write).
    ///
    /// For `T: Pod` types (typically register-sized), the extra stack copy
    /// is negligible and often optimized away.
    #[inline]
    pub(crate) fn write_with(&self, seq: u64, f: impl FnOnce(&mut MaybeUninit<T>)) {
        let mut tmp = MaybeUninit::<T>::uninit();
        f(&mut tmp);

        let writing = seq * 2 + 1;
        let done = seq * 2 + 2;

        self.stamp.store(writing, Ordering::Relaxed);
        fence(Ordering::Release);

        // SAFETY: single-writer guarantee — no concurrent writes to this slot.
        // write_volatile avoids formal UB from concurrent readers (same as write()).
        // tmp was initialized by the closure (caller contract).
        unsafe { ptr::write_volatile(self.value.get() as *mut T, tmp.assume_init()) };

        self.stamp.store(done, Ordering::Release);
    }

    /// Seqlock read protocol. Returns `None` on torn read (caller should retry).
    ///
    /// Returns `Err(actual_stamp)` if the slot holds a different sequence.
    ///
    /// Uses `read_volatile` instead of `ptr::read` to avoid formal UB when the
    /// slot is mid-write. The volatile read prevents the compiler from assuming
    /// the memory is in a valid state, and the subsequent stamp re-check gates
    /// whether the value is actually used.
    #[inline]
    pub(crate) fn try_read(&self, seq: u64) -> Result<Option<T>, u64> {
        let expected = seq * 2 + 2;

        let s1 = self.stamp.load(Ordering::Acquire);

        // Happy path first — stamp matches expected sequence
        if s1 == expected {
            // SAFETY: read_volatile is sound even if the writer is mid-store on
            // another core — the stamp re-check below gates whether we use the
            // value. This eliminates the formal UB of ptr::read on torn data.
            let value = unsafe { ptr::read_volatile((*self.value.get()).as_ptr()) };
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
