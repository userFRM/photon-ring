// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::pod::Pod;
use crate::ring::SharedRing;
use crate::slot::Slot;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use super::prefetch_write_next;

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
pub struct MpPublisher<T: Pod> {
    pub(super) ring: Arc<SharedRing<T>>,
    /// Cached raw pointer to the slot array. Avoids Arc + Box deref on the
    /// hot path. Valid for the lifetime of `ring` (the Arc keeps it alive).
    pub(super) slots_ptr: *const Slot<T>,
    /// Cached ring capacity. Immutable after construction.
    pub(super) capacity: u64,
    /// Cached ring mask (`capacity - 1`). Used for pow2 fast path.
    pub(super) mask: u64,
    /// Precomputed Lemire reciprocal for arbitrary-capacity fastmod.
    pub(super) reciprocal: u64,
    /// True if capacity is a power of two (AND instead of fastmod).
    pub(super) is_pow2: bool,
    /// Cached raw pointer to `ring.cursor.0`. Avoids Arc deref on hot path.
    pub(super) cursor_ptr: *const AtomicU64,
    /// Cached raw pointer to `ring.next_seq`. Avoids Arc deref + Option
    /// unwrap on hot path.
    pub(super) next_seq_ptr: *const AtomicU64,
}

impl<T: Pod> Clone for MpPublisher<T> {
    fn clone(&self) -> Self {
        MpPublisher {
            ring: self.ring.clone(),
            slots_ptr: self.slots_ptr,
            capacity: self.capacity,
            mask: self.mask,
            reciprocal: self.reciprocal,
            is_pow2: self.is_pow2,
            cursor_ptr: self.cursor_ptr,
            next_seq_ptr: self.next_seq_ptr,
        }
    }
}

// Safety: MpPublisher uses atomic CAS for all shared state.
// No mutable fields — all coordination is via atomics on SharedRing.
unsafe impl<T: Pod> Send for MpPublisher<T> {}
unsafe impl<T: Pod> Sync for MpPublisher<T> {}

impl<T: Pod> MpPublisher<T> {
    /// Map a sequence number to a slot index.
    #[inline(always)]
    fn slot_index(&self, seq: u64) -> usize {
        if self.is_pow2 {
            (seq & self.mask) as usize
        } else {
            let q = ((seq as u128 * self.reciprocal as u128) >> 64) as u64;
            let mut r = seq - q.wrapping_mul(self.capacity);
            if r >= self.capacity {
                r -= self.capacity;
            }
            r as usize
        }
    }

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
        let seq = next_seq_atomic.fetch_add(1, Ordering::AcqRel);
        // SAFETY: slots_ptr is valid for the lifetime of self.ring (Arc-owned).
        let slot = unsafe { &*self.slots_ptr.add(self.slot_index(seq)) };
        prefetch_write_next(self.slots_ptr, self.slot_index(seq + 1) as u64);
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
        let seq = next_seq_atomic.fetch_add(1, Ordering::AcqRel);
        // SAFETY: slots_ptr is valid for the lifetime of self.ring (Arc-owned).
        let slot = unsafe { &*self.slots_ptr.add(self.slot_index(seq)) };
        prefetch_write_next(self.slots_ptr, self.slot_index(seq + 1) as u64);
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

    /// Ring capacity.
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
            let pred_slot = unsafe { &*self.slots_ptr.add(self.slot_index(seq - 1)) };
            let pred_done = (seq - 1) * 2 + 2;
            // Check stamp >= pred_done to handle rare ring-wrap case where
            // a later sequence already overwrote the predecessor's slot.
            //
            // On aarch64: SEVL before the loop sets the event register so the
            // first WFE returns immediately (avoids unconditional block).
            // Subsequent WFE calls sleep until a cache-line invalidation
            // (the predecessor's stamp store) wakes the core.
            #[cfg(target_arch = "aarch64")]
            unsafe {
                core::arch::asm!("sevl", options(nomem, nostack));
            }
            while pred_slot.stamp_load() < pred_done {
                #[cfg(target_arch = "aarch64")]
                unsafe {
                    core::arch::asm!("wfe", options(nomem, nostack));
                }
                #[cfg(not(target_arch = "aarch64"))]
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
            let slot = unsafe { &*self.slots_ptr.add(self.slot_index(next)) };
            if slot.stamp_load() < done_stamp {
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
