// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Loom model of the bounded-channel backpressure protocol.
//!
//! The guarantee under test: **a registered subscriber never has an unread slot
//! overwritten.** A bounded channel promises exactly that, and the protocol
//! defending it is subtle enough that reading the code is not sufficient
//! evidence — a registration race survived careful review of this crate until a
//! model of this shape was written down.
//!
//! What is modelled, matching `Publisher::has_room`, `SharedRing::slowest_cursor`
//! and `SharedRing::register_tracker_at_head`:
//!
//! 1. The publisher does **not** consult trackers on every publish. It caches the
//!    slowest cursor and only rescans once that cached value says it is close to
//!    lapping. The cache is the interesting part: it is what allows a publisher to
//!    run ahead of a subscriber it has not noticed yet.
//! 2. A subscriber joins by choosing a start position and registering a tracker.
//!    Both steps happen under the tracker lock, so they are atomic with respect
//!    to a rescan.
//! 3. A subscriber advances its tracker only after consuming.
//!
//! Run with: `RUSTFLAGS="--cfg loom" cargo test --test loom_backpressure --release`

#![cfg(loom)]

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;

/// Ring capacity. Kept tiny to bound loom's state space; the invariant does not
/// depend on the value.
const CAPACITY: usize = 2;
/// Headroom. Zero is the tightest setting and the one the docs and examples use.
const WATERMARK: usize = 0;

/// Sentinel for "nothing published yet", mirroring the real cursor.
const NOTHING: usize = usize::MAX;

struct RingModel {
    /// Sequence of the last published message.
    cursor: AtomicUsize,
    /// Registered subscriber cursors: the next sequence each will read.
    trackers: Mutex<Vec<Arc<AtomicUsize>>>,
}

impl RingModel {
    fn new() -> Self {
        RingModel {
            cursor: AtomicUsize::new(NOTHING),
            trackers: Mutex::new(Vec::new()),
        }
    }

    /// Model of `SharedRing::register_tracker_at_head`: choose the start position
    /// and publish the tracker under one lock, so a concurrent rescan either sees
    /// this subscriber or ran when the head was no further along than `start`.
    fn register(&self) -> Arc<AtomicUsize> {
        let mut trackers = self.trackers.lock().unwrap();
        let head = self.cursor.load(Ordering::Acquire);
        let start = if head == NOTHING { 0 } else { head + 1 };
        let tracker = Arc::new(AtomicUsize::new(start));
        trackers.push(tracker.clone());
        tracker
    }

    /// Model of `SharedRing::slowest_cursor`.
    fn slowest(&self) -> Option<usize> {
        let trackers = self.trackers.lock().unwrap();
        trackers.iter().map(|t| t.load(Ordering::Acquire)).min()
    }

    /// Ground truth, used only for assertions: would publishing `seq` overwrite a
    /// slot some registered subscriber has not read? Slot reuse means sequence
    /// `seq` lands on the slot last holding `seq - CAPACITY`.
    fn would_clobber(&self, seq: usize) -> bool {
        let trackers = self.trackers.lock().unwrap();
        trackers
            .iter()
            .map(|t| t.load(Ordering::Acquire))
            .any(|t| seq >= t + CAPACITY)
    }
}

/// Model of `Publisher`, including the cached-slowest fast path.
struct PublisherModel {
    ring: Arc<RingModel>,
    seq: usize,
    cached_slowest: usize,
}

impl PublisherModel {
    fn new(ring: Arc<RingModel>) -> Self {
        PublisherModel {
            ring,
            seq: 0,
            cached_slowest: 0,
        }
    }

    /// Model of `Publisher::has_room`.
    fn has_room(&mut self) -> bool {
        let effective = CAPACITY - WATERMARK;
        if self.seq >= self.cached_slowest + effective {
            if let Some(slowest) = self.ring.slowest() {
                self.cached_slowest = slowest;
                if self.seq >= slowest + effective {
                    return false;
                }
            }
        }
        true
    }

    /// Publish one message if there is room. Asserts the guarantee before writing.
    fn try_publish(&mut self) -> bool {
        if !self.has_room() {
            return false;
        }
        assert!(
            !self.ring.would_clobber(self.seq),
            "published seq {} over a slot a registered subscriber had not read \
             (cached_slowest = {})",
            self.seq,
            self.cached_slowest
        );
        self.ring.cursor.store(self.seq, Ordering::Release);
        self.seq += 1;
        true
    }
}

/// A subscriber that joins a ring already in flight, then consumes.
///
/// This is the interleaving that matters: registration racing an in-progress
/// publisher whose cached slowest cursor predates the new subscriber.
#[test]
fn subscriber_joining_a_live_ring_is_never_lapped() {
    loom::model(|| {
        let ring = Arc::new(RingModel::new());

        let producer_ring = ring.clone();
        let producer = thread::spawn(move || {
            let mut p = PublisherModel::new(producer_ring);
            // Bounded work: enough publishes to wrap the ring more than once.
            for _ in 0..(CAPACITY + 1) {
                if !p.try_publish() {
                    break;
                }
            }
        });

        let joiner_ring = ring.clone();
        let joiner = thread::spawn(move || {
            let tracker = joiner_ring.register();
            // Consume one message if the publisher has produced one for us.
            let mine = tracker.load(Ordering::Relaxed);
            let head = joiner_ring.cursor.load(Ordering::Acquire);
            if head != NOTHING && head >= mine {
                tracker.store(mine + 1, Ordering::Release);
            }
        });

        producer.join().unwrap();
        joiner.join().unwrap();
    });
}

/// Two subscribers already registered, one consuming and one idle. The publisher
/// must be held by the idle one.
#[test]
fn an_idle_subscriber_holds_the_publisher() {
    loom::model(|| {
        let ring = Arc::new(RingModel::new());
        let _idle = ring.register();
        let active = ring.register();

        let producer_ring = ring.clone();
        let producer = thread::spawn(move || {
            let mut p = PublisherModel::new(producer_ring);
            for _ in 0..(CAPACITY + 1) {
                if !p.try_publish() {
                    break;
                }
            }
        });

        let consumer_ring = ring.clone();
        let consumer = thread::spawn(move || {
            let mine = active.load(Ordering::Relaxed);
            let head = consumer_ring.cursor.load(Ordering::Acquire);
            if head != NOTHING && head >= mine {
                active.store(mine + 1, Ordering::Release);
            }
        });

        producer.join().unwrap();
        consumer.join().unwrap();
    });
}
