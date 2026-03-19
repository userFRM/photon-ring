// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! Loom-based exhaustive concurrency tests for the MPMC cursor advancement protocol.
//!
//! These tests model the `advance_cursor` + `catch_up_cursor` algorithm from
//! `src/channel/mp_publisher.rs` using loom atomics. This is a standalone model
//! of the protocol — it does not modify or import from the main crate source,
//! but it faithfully reproduces the algorithm and verifies correctness under
//! all possible thread interleavings.
//!
//! # What is tested
//!
//! The MPMC cursor advancement protocol guarantees:
//!
//! 1. **NoGap**: If cursor == N, then stamps 0..=N are all committed.
//!    The cursor never advances past an uncommitted slot.
//! 2. **Stamp safety**: All stamps reach committed state after producers finish.
//! 3. **Consumer safety**: Reading up to the cursor yields only committed stamps.
//! 4. **Minimum progress**: At least the first sequence (cursor >= 0) becomes visible.
//!
//! The cursor is **best-effort** — it may lag behind the highest committed
//! sequence. This is by design: the protocol prioritizes throughput (no cursor
//! CAS spin loop) over cursor precision. Consumers use stamp-based reading, so
//! they can read any committed slot regardless of the cursor position.
//!
//! # Running
//!
//! ```sh
//! RUSTFLAGS="--cfg loom" cargo test --test loom_mpmc --release
//! ```
//!
//! Loom tests are extremely slow in debug mode. Always use `--release`.
//!
//! # Protocol summary (from mp_publisher.rs)
//!
//! 1. Producer claims seq via `fetch_add(1, AcqRel)` on `next_seq`.
//! 2. Producer writes slot: stamp = `seq*2+1` (writing), then stamp = `seq*2+2` (done).
//! 3. Fast-path CAS: `cursor: expected -> seq` (expected = u64::MAX for seq=0, else seq-1).
//! 4. If fast CAS succeeds: run `catch_up_cursor(seq)`.
//! 5. If fast CAS fails: wait on predecessor slot stamp >= `(seq-1)*2+2`.
//! 6. Retry CAS after predecessor confirmed done.
//! 7. If cursor == seq after retry: run `catch_up_cursor(seq)`.
//! 8. `catch_up_cursor`: loop advancing cursor past already-committed successors.

// Only compile when the `loom` cfg is set.
#![cfg(loom)]

use loom::sync::atomic::{fence, AtomicU64, Ordering};
use loom::sync::Arc;
use loom::thread;

/// Ring capacity for all tests. Must be a power of two.
/// Keep this small to bound loom's state space.
const CAPACITY: u64 = 4;
const MASK: u64 = CAPACITY - 1;

/// Shared state modelling the MPMC ring.
struct RingModel {
    next_seq: AtomicU64,
    cursor: AtomicU64,
    stamps: [AtomicU64; CAPACITY as usize],
}

impl RingModel {
    fn new() -> Self {
        RingModel {
            next_seq: AtomicU64::new(0),
            cursor: AtomicU64::new(u64::MAX), // sentinel: nothing published yet
            stamps: core::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Model of `Slot::write` — the seqlock write protocol.
    fn slot_write(&self, seq: u64) {
        let idx = (seq & MASK) as usize;
        let writing = seq * 2 + 1;
        let done = seq * 2 + 2;

        self.stamps[idx].store(writing, Ordering::Relaxed);
        fence(Ordering::Release);
        // (Payload write would go here — elided since we only test the protocol.)
        self.stamps[idx].store(done, Ordering::Release);
    }

    /// Model of `Slot::stamp_load`.
    fn stamp_load(&self, seq: u64) -> u64 {
        let idx = (seq & MASK) as usize;
        self.stamps[idx].load(Ordering::Acquire)
    }

    /// Model of `MpPublisher::advance_cursor`.
    ///
    /// Faithfully reproduces the algorithm from mp_publisher.rs, including
    /// the Relaxed load after the second CAS (which is a known weak point
    /// under non-TSO memory models — see test comments).
    fn advance_cursor(&self, seq: u64) {
        let expected_cursor = if seq == 0 { u64::MAX } else { seq - 1 };

        // Fast path: single CAS.
        if self
            .cursor
            .compare_exchange(expected_cursor, seq, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            self.catch_up_cursor(seq);
            return;
        }

        // Contended path: wait on predecessor's stamp.
        if seq > 0 {
            let pred_done = (seq - 1) * 2 + 2;
            while self.stamp_load(seq - 1) < pred_done {
                loom::thread::yield_now(); // replaces core::hint::spin_loop()
            }
        }

        // Predecessor is done — retry CAS.
        let _ = self.cursor.compare_exchange(
            expected_cursor,
            seq,
            Ordering::Release,
            Ordering::Relaxed,
        );
        // If we won the CAS, absorb successors.
        if self.cursor.load(Ordering::Relaxed) == seq {
            self.catch_up_cursor(seq);
        }
    }

    /// Model of `MpPublisher::catch_up_cursor`.
    fn catch_up_cursor(&self, mut seq: u64) {
        loop {
            let next = seq + 1;
            // Don't advance past what has been claimed.
            if next >= self.next_seq.load(Ordering::Acquire) {
                break;
            }
            // Check if the next slot's stamp shows a completed write.
            let done_stamp = next * 2 + 2;
            if self.stamp_load(next) < done_stamp {
                break;
            }
            // Slot is committed — try to advance cursor.
            if self
                .cursor
                .compare_exchange(seq, next, Ordering::Release, Ordering::Relaxed)
                .is_err()
            {
                break;
            }
            seq = next;
        }
    }

    /// Full publish operation: claim seq, write slot, advance cursor.
    fn publish(&self) -> u64 {
        let seq = self.next_seq.fetch_add(1, Ordering::AcqRel);
        self.slot_write(seq);
        self.advance_cursor(seq);
        seq
    }
}

// ---------------------------------------------------------------------------
// Test 1: NoGap — cursor never advances past uncommitted slots
// ---------------------------------------------------------------------------

/// Two producers each publish one message. Verify that whatever value the
/// cursor ends at, all preceding slots are committed. The cursor is
/// best-effort and may not reach the highest committed sequence, but it
/// must never point past an uncommitted slot.
#[test]
fn two_producers_no_gap_invariant() {
    loom::model(|| {
        let ring = Arc::new(RingModel::new());

        let r1 = ring.clone();
        let r2 = ring.clone();

        let t1 = thread::spawn(move || r1.publish());
        let t2 = thread::spawn(move || r2.publish());

        t1.join().unwrap();
        t2.join().unwrap();

        let final_cursor = ring.cursor.load(Ordering::Acquire);

        // Cursor must have advanced past the sentinel.
        assert_ne!(
            final_cursor,
            u64::MAX,
            "cursor was never advanced from sentinel"
        );

        // NoGap: all slots 0..=cursor must have committed stamps.
        for seq in 0..=final_cursor {
            let done = seq * 2 + 2;
            let stamp = ring.stamp_load(seq);
            assert!(
                stamp >= done,
                "NoGap violated: cursor={final_cursor} but seq {seq} stamp={stamp} (need >= {done})"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Test 2: Stamp safety — all stamps committed after all producers finish
// ---------------------------------------------------------------------------

/// After both producers complete, every claimed slot must have a committed
/// stamp (even stamp value >= seq*2+2). No slot should be left in the
/// "writing" state.
#[test]
fn stamps_all_committed_after_completion() {
    loom::model(|| {
        let ring = Arc::new(RingModel::new());

        let r1 = ring.clone();
        let r2 = ring.clone();

        let t1 = thread::spawn(move || r1.publish());
        let t2 = thread::spawn(move || r2.publish());

        t1.join().unwrap();
        t2.join().unwrap();

        let claimed = ring.next_seq.load(Ordering::Acquire);
        assert_eq!(claimed, 2, "should have claimed 2 sequences");

        for seq in 0..claimed {
            let stamp = ring.stamp_load(seq);
            let done = seq * 2 + 2;
            assert!(
                stamp >= done,
                "seq {seq} stamp should be >= {done} (committed), got {stamp}"
            );
            // Stamp should not be odd (would mean write still in progress).
            assert!(
                stamp % 2 == 0,
                "seq {seq} stamp is odd ({stamp}), indicating incomplete write"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Test 3: Consumer safety at quiescence — cursor implies committed stamps
// ---------------------------------------------------------------------------

/// After both producers finish, snapshot the cursor and verify that all
/// slots up to the cursor have committed stamps. This models what a
/// consumer would do: trust the cursor as a lower bound for availability,
/// then read each slot's stamp to verify.
#[test]
fn consumer_safety_at_quiescence() {
    loom::model(|| {
        let ring = Arc::new(RingModel::new());

        let r1 = ring.clone();
        let r2 = ring.clone();

        let t1 = thread::spawn(move || r1.publish());
        let t2 = thread::spawn(move || r2.publish());

        t1.join().unwrap();
        t2.join().unwrap();

        // Simulate a consumer reading the cursor then checking stamps.
        let cursor_val = ring.cursor.load(Ordering::Acquire);
        if cursor_val != u64::MAX {
            for seq in 0..=cursor_val {
                let done_stamp = seq * 2 + 2;
                let stamp = ring.stamp_load(seq);
                assert!(
                    stamp >= done_stamp,
                    "cursor={cursor_val} but seq {seq} stamp={stamp} (expected >= {done_stamp})"
                );
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Test 4: Minimum progress — at least seq=0 becomes visible
// ---------------------------------------------------------------------------

/// After all producers finish, the cursor must be at least 0 (not stuck
/// at the u64::MAX sentinel). This verifies that at minimum the first
/// published message advances the cursor.
#[test]
fn minimum_cursor_progress() {
    loom::model(|| {
        let ring = Arc::new(RingModel::new());

        let r1 = ring.clone();
        let r2 = ring.clone();

        let t1 = thread::spawn(move || r1.publish());
        let t2 = thread::spawn(move || r2.publish());

        t1.join().unwrap();
        t2.join().unwrap();

        let final_cursor = ring.cursor.load(Ordering::Acquire);
        assert_ne!(
            final_cursor,
            u64::MAX,
            "cursor stuck at sentinel after 2 publishes"
        );
        // The producer that claimed seq=0 will always succeed its
        // fast-path CAS (MAX -> 0), so cursor >= 0 is guaranteed.
    });
}

// ---------------------------------------------------------------------------
// Test 5: Sequence uniqueness — fetch_add guarantees distinct sequences
// ---------------------------------------------------------------------------

/// Verify that two concurrent producers always get distinct sequence
/// numbers. This is trivially guaranteed by fetch_add, but worth
/// confirming under loom's model.
#[test]
fn sequence_numbers_are_unique() {
    loom::model(|| {
        let ring = Arc::new(RingModel::new());

        let r1 = ring.clone();
        let r2 = ring.clone();

        let t1 = thread::spawn(move || r1.publish());
        let t2 = thread::spawn(move || r2.publish());

        let s1 = t1.join().unwrap();
        let s2 = t2.join().unwrap();

        assert_ne!(s1, s2, "sequences must be distinct");
        assert!(s1 <= 1 && s2 <= 1, "sequences must be 0 or 1");
        assert_eq!(s1 + s2, 1, "sequences must be {{0, 1}}, got {s1} and {s2}");
    });
}

// ---------------------------------------------------------------------------
// Test 6: Catch-up with delayed writer
// ---------------------------------------------------------------------------

/// One producer delays its slot write (via yield) while the other proceeds
/// immediately. This specifically exercises the predecessor-waiting path
/// and the catch-up loop under all interleavings with a stall.
#[test]
fn catch_up_with_delayed_writer() {
    loom::model(|| {
        let ring = Arc::new(RingModel::new());

        let r1 = ring.clone();
        let r2 = ring.clone();

        // Thread 1: claim seq, yield (simulating slow write), then write.
        let t1 = thread::spawn(move || {
            let seq = r1.next_seq.fetch_add(1, Ordering::AcqRel);
            loom::thread::yield_now(); // delay before writing
            r1.slot_write(seq);
            r1.advance_cursor(seq);
            seq
        });

        // Thread 2: publish immediately.
        let t2 = thread::spawn(move || {
            let seq = r2.next_seq.fetch_add(1, Ordering::AcqRel);
            r2.slot_write(seq);
            r2.advance_cursor(seq);
            seq
        });

        let s1 = t1.join().unwrap();
        let s2 = t2.join().unwrap();

        assert_ne!(s1, s2);

        let final_cursor = ring.cursor.load(Ordering::Acquire);

        // Cursor must have advanced past sentinel.
        assert_ne!(final_cursor, u64::MAX, "cursor stuck at sentinel");

        // NoGap: all slots up to cursor are committed.
        for seq in 0..=final_cursor {
            let done = seq * 2 + 2;
            let stamp = ring.stamp_load(seq);
            assert!(
                stamp >= done,
                "NoGap violated: cursor={final_cursor}, seq {seq} stamp={stamp} (need >= {done})"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Test 7: Consumer single-check during publishing (3 threads, minimal)
// ---------------------------------------------------------------------------

/// Two producers publish while a consumer does one cursor load and one
/// stamp check. The consumer's work is minimized (2 atomic ops) to keep
/// the 3-thread state space tractable for loom.
///
/// Invariant: if cursor shows seq=N, then stamp[N] must be committed.
#[test]
fn consumer_single_check_during_publish() {
    loom::model(|| {
        let ring = Arc::new(RingModel::new());

        let r1 = ring.clone();
        let r2 = ring.clone();
        let rc = ring.clone();

        let t1 = thread::spawn(move || r1.publish());
        let t2 = thread::spawn(move || r2.publish());

        // Consumer: one Acquire load of cursor, one Acquire load of the
        // stamp at that cursor position. Only 2 atomic ops to minimise
        // loom's state space.
        let consumer = thread::spawn(move || {
            let cursor_val = rc.cursor.load(Ordering::Acquire);
            if cursor_val != u64::MAX {
                let done = cursor_val * 2 + 2;
                let stamp = rc.stamp_load(cursor_val);
                assert!(
                    stamp >= done,
                    "cursor={cursor_val} but its stamp={stamp} (expected >= {done})"
                );
            }
        });

        t1.join().unwrap();
        t2.join().unwrap();
        consumer.join().unwrap();
    });
}
