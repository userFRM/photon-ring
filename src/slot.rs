// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::pod::Pod;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{fence, AtomicU64, Ordering};

/// Number of `AtomicU64` stripes needed to hold a value of type `T`.
#[cfg(feature = "atomic-slots")]
#[inline(always)]
const fn stripe_count<T>() -> usize {
    core::mem::size_of::<T>().div_ceil(8)
}

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

// ---------------------------------------------------------------------------
// Default implementation: volatile-based seqlock (fastest, practical UB)
// ---------------------------------------------------------------------------
#[cfg(not(feature = "atomic-slots"))]
impl<T: Pod> Slot<T> {
    /// Seqlock write protocol. Single-writer only.
    ///
    /// Uses `write_volatile` for the payload store. This avoids practical UB
    /// on all target architectures (x86, ARM) but is formally a data race
    /// under the Rust abstract machine. Enable the `atomic-slots` feature
    /// for a formally sound implementation.
    #[inline]
    pub(crate) fn write(&self, seq: u64, value: T) {
        let writing = seq * 2 + 1;
        let done = seq * 2 + 2;

        self.stamp.store(writing, Ordering::Relaxed);
        fence(Ordering::Release);

        // SAFETY: single-writer guarantee — no concurrent writes to this slot.
        // write_volatile avoids practical UB on all target architectures (x86, ARM);
        // formally still a data race under the Rust abstract machine, as volatile
        // does not establish a happens-before relationship. Sound because T: Pod
        // makes all bit patterns valid and the stamp re-check gates usage.
        unsafe { core::ptr::write_volatile(self.value.get() as *mut T, value) };

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

        // SAFETY: single-writer guarantee + tmp initialized by closure.
        unsafe { core::ptr::write_volatile(self.value.get() as *mut T, tmp.assume_init()) };

        self.stamp.store(done, Ordering::Release);
    }

    /// Seqlock read protocol. Returns `None` on torn read (caller should retry).
    ///
    /// Returns `Err(actual_stamp)` if the slot holds a different sequence.
    ///
    /// Uses `read_volatile` for the payload load. Formally a data race under
    /// the Rust abstract machine. Enable `atomic-slots` for formal soundness.
    #[inline]
    pub(crate) fn try_read(&self, seq: u64) -> Result<Option<T>, u64> {
        let expected = seq * 2 + 2;

        let s1 = self.stamp.load(Ordering::Acquire);

        if s1 == expected {
            // SAFETY: read_volatile avoids practical UB on all target architectures.
            // Formally a data race. T: Pod makes all bit patterns valid; stamp
            // re-check gates usage.
            let value = unsafe { core::ptr::read_volatile((*self.value.get()).as_ptr()) };
            let s2 = self.stamp.load(Ordering::Acquire);
            if s1 == s2 {
                return Ok(Some(value));
            }
            return Ok(None); // torn read
        }

        if s1 & 1 != 0 {
            return Ok(None);
        }

        Err(s1)
    }
}

// ---------------------------------------------------------------------------
// atomic-slots: formally sound implementation using AtomicU64 stripes
// ---------------------------------------------------------------------------
//
// Replaces write_volatile/read_volatile with per-u64 atomic stores/loads.
// On x86-64, AtomicU64::store/load(Relaxed) compiles to identical MOV
// instructions — zero performance cost. On ARM64, one extra DMB ISHLD
// barrier in the reader path (~5-10ns).
//
// All memory accesses go through Atomic* types, so there are no data races
// under the Rust abstract machine. Miri-passable. Formally sound.
#[cfg(feature = "atomic-slots")]
impl<T: Pod> Slot<T> {
    /// Seqlock write protocol using atomic stripes. Single-writer only.
    ///
    /// Decomposes `T` into `ceil(size_of::<T>() / 8)` u64 chunks and
    /// stores each one atomically with `Relaxed` ordering. The `Release`
    /// stamp store at the end ensures all stripe stores are visible to
    /// any reader that observes the "done" stamp.
    ///
    /// On x86-64, `AtomicU64::store(Relaxed)` compiles to plain `MOV` —
    /// identical machine code to the volatile-based implementation.
    #[inline]
    pub(crate) fn write(&self, seq: u64, value: T) {
        let writing = seq * 2 + 1;
        let done = seq * 2 + 2;

        self.stamp.store(writing, Ordering::Relaxed);
        fence(Ordering::Release);

        let n = stripe_count::<T>();
        let src = &value as *const T as *const u8;
        let dst = self.value.get() as *mut u64;

        for i in 0..n {
            let chunk = unsafe { read_stripe(src, i, core::mem::size_of::<T>()) };
            // SAFETY: dst is within the Slot's UnsafeCell, properly aligned
            // (Slot is align(64)). AtomicU64::from_ptr requires *mut u64
            // with proper alignment and no mixed atomic/non-atomic access.
            // Under atomic-slots, all accesses to this memory go through
            // AtomicU64, satisfying the no-mixing requirement.
            unsafe { AtomicU64::from_ptr(dst.add(i)) }.store(chunk, Ordering::Relaxed);
        }

        self.stamp.store(done, Ordering::Release);
    }

    /// Seqlock write via closure using atomic stripes.
    #[inline]
    pub(crate) fn write_with(&self, seq: u64, f: impl FnOnce(&mut MaybeUninit<T>)) {
        let mut tmp = MaybeUninit::<T>::uninit();
        f(&mut tmp);

        // SAFETY: closure contract — tmp is initialized.
        let value = unsafe { tmp.assume_init() };
        self.write(seq, value);
    }

    /// Seqlock read protocol using atomic stripes. Formally sound.
    ///
    /// Loads each u64 stripe atomically with `Relaxed` ordering. An
    /// `Acquire` fence after all stripe loads ensures they complete
    /// before the second stamp check (required on ARM; no-op on x86).
    ///
    /// All memory accesses are through `Atomic*` types — no data races
    /// exist under the Rust abstract machine.
    #[inline]
    pub(crate) fn try_read(&self, seq: u64) -> Result<Option<T>, u64> {
        let expected = seq * 2 + 2;

        // Acquire: orders all subsequent loads (stripe loads) after this.
        let s1 = self.stamp.load(Ordering::Acquire);

        if s1 == expected {
            let n = stripe_count::<T>();
            let src = self.value.get() as *mut u64;
            let mut buf = MaybeUninit::<T>::uninit();
            let dst = buf.as_mut_ptr() as *mut u8;

            for i in 0..n {
                // SAFETY: src is within the Slot's UnsafeCell, properly aligned.
                // Under atomic-slots, all accesses go through AtomicU64.
                let chunk = unsafe { AtomicU64::from_ptr(src.add(i)) }.load(Ordering::Relaxed);
                unsafe { write_stripe(dst, i, chunk, core::mem::size_of::<T>()) };
            }

            // Acquire fence: ensures all stripe loads above complete before
            // the second stamp load below. Required on ARM (DMB ISHLD);
            // no-op on x86 (TSO provides load-load ordering).
            fence(Ordering::Acquire);

            // Relaxed is sufficient here because the fence above orders it.
            let s2 = self.stamp.load(Ordering::Relaxed);
            if s1 == s2 {
                // SAFETY: all stripes loaded atomically between matching stamps.
                // T: Pod guarantees every bit pattern is valid.
                return Ok(Some(unsafe { buf.assume_init() }));
            }
            return Ok(None); // torn read (stamps diverged)
        }

        if s1 & 1 != 0 {
            return Ok(None);
        }

        Err(s1)
    }
}

// ---------------------------------------------------------------------------
// Stripe read/write helpers (used by atomic-slots)
// ---------------------------------------------------------------------------

/// Read 8 bytes from `src` at stripe offset `i`, handling the last partial
/// stripe by reading only the remaining bytes and zero-padding.
#[cfg(feature = "atomic-slots")]
#[inline(always)]
unsafe fn read_stripe(src: *const u8, i: usize, size: usize) -> u64 {
    let offset = i * 8;
    if offset + 8 <= size {
        // Full 8-byte stripe — read directly.
        (src.add(offset) as *const u64).read_unaligned()
    } else {
        // Partial last stripe — zero-pad.
        let remaining = size - offset;
        let mut buf = 0u64;
        core::ptr::copy_nonoverlapping(src.add(offset), &mut buf as *mut u64 as *mut u8, remaining);
        buf
    }
}

/// Write 8 bytes of a stripe to `dst` at stripe offset `i`, handling the
/// last partial stripe by writing only the remaining bytes.
#[cfg(feature = "atomic-slots")]
#[inline(always)]
unsafe fn write_stripe(dst: *mut u8, i: usize, chunk: u64, size: usize) {
    let offset = i * 8;
    if offset + 8 <= size {
        // Full 8-byte stripe — write directly.
        (dst.add(offset) as *mut u64).write_unaligned(chunk);
    } else {
        // Partial last stripe — write only remaining bytes.
        let remaining = size - offset;
        core::ptr::copy_nonoverlapping(
            &chunk as *const u64 as *const u8,
            dst.add(offset),
            remaining,
        );
    }
}
