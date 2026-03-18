// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use super::errors::PublishError;
use crate::pod::Pod;
use crate::ring::SharedRing;
use crate::slot::Slot;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use super::prefetch_write_next;

/// The write side of a Photon SPMC channel.
///
/// There is exactly one `Publisher` per channel. It is `Send` but not `Sync` —
/// only one thread may publish at a time (single-producer guarantee enforced
/// by `&mut self`).
pub struct Publisher<T: Pod> {
    pub(super) ring: Arc<SharedRing<T>>,
    /// Cached raw pointer to the slot array. Avoids Arc + Box deref on the
    /// hot path. Valid for the lifetime of `ring` (the Arc keeps it alive).
    pub(super) slots_ptr: *const Slot<T>,
    /// Cached ring mask (`capacity - 1`). Immutable after construction.
    pub(super) mask: u64,
    /// Cached raw pointer to `ring.cursor.0`. Avoids Arc deref on hot path.
    pub(super) cursor_ptr: *const AtomicU64,
    pub(super) seq: u64,
    /// Cached minimum cursor from the last tracker scan. Used as a fast-path
    /// check to avoid scanning on every `try_publish` call.
    pub(super) cached_slowest: u64,
    /// Cached backpressure flag. Avoids Arc deref + Option check on every
    /// publish() for lossy channels. Immutable after construction.
    pub(super) has_backpressure: bool,
}

unsafe impl<T: Pod> Send for Publisher<T> {}

impl<T: Pod> Publisher<T> {
    /// Spin-wait until backpressure allows publishing.
    ///
    /// On a bounded channel, this blocks until the slowest subscriber has
    /// advanced far enough. On a lossy channel (no backpressure), this is
    /// a no-op.
    #[inline]
    fn wait_for_backpressure(&mut self) {
        if !self.has_backpressure {
            return;
        }
        loop {
            if let Some(bp) = self.ring.backpressure.as_ref() {
                let capacity = self.ring.capacity();
                let effective = capacity - bp.watermark;
                if self.seq >= self.cached_slowest + effective {
                    match self.ring.slowest_cursor() {
                        Some(slowest) => {
                            self.cached_slowest = slowest;
                            if self.seq >= slowest + effective {
                                core::hint::spin_loop();
                                continue;
                            }
                        }
                        None => {
                            // No subscribers registered yet — ring is unbounded.
                        }
                    }
                }
            }
            break;
        }
    }

    /// Write a single value to the ring without any backpressure check.
    /// This is the raw publish path used by both `publish()` (lossy) and
    /// `try_publish()` (after backpressure check passes).
    #[inline]
    fn publish_unchecked(&mut self, value: T) {
        // SAFETY: slots_ptr is valid for the lifetime of self.ring (Arc-owned).
        // Index is masked to stay within the allocated slot array.
        let slot = unsafe { &*self.slots_ptr.add((self.seq & self.mask) as usize) };
        prefetch_write_next(self.slots_ptr, (self.seq + 1) & self.mask);
        slot.write(self.seq, value);
        // SAFETY: cursor_ptr points to ring.cursor.0, kept alive by self.ring.
        unsafe { &*self.cursor_ptr }.store(self.seq, Ordering::Release);
        self.seq += 1;
    }

    /// Publish by writing directly into the slot via a closure.
    ///
    /// The closure receives a `&mut MaybeUninit<T>`, allowing construction
    /// of the value into a stack temporary which is then written to the slot.
    ///
    /// On a bounded channel (created with [`channel_bounded()`]), this method
    /// spin-waits until there is room in the ring, ensuring no message loss
    /// (same backpressure semantics as [`publish()`](Self::publish)).
    /// On a regular (lossy) channel, this publishes immediately.
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
        self.wait_for_backpressure();
        // SAFETY: see publish_unchecked.
        let slot = unsafe { &*self.slots_ptr.add((self.seq & self.mask) as usize) };
        prefetch_write_next(self.slots_ptr, (self.seq + 1) & self.mask);
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
        if self.has_backpressure {
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
        if self.has_backpressure {
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
