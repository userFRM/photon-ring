// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A ring for values that are not `Pod`.
//!
//! The [`Pod`](crate::Pod) bound on [`channel`](crate::channel()) exists so that a
//! reader racing a writer observes a harmless torn value rather than undefined
//! behaviour. That race is only possible when the publisher is allowed to
//! overwrite a slot a subscriber has not read — which on a bounded ring, with
//! every subscriber registered for backpressure, cannot happen.
//!
//! So this ring drops the bound. Slots own their values, created once by a
//! factory and **mutated in place** rather than overwritten, so nothing is ever
//! copied into or out of the ring and `String`, `Vec`, enums and `Option` are
//! all ordinary payloads. Because no reader can ever observe a slot mid-write,
//! there is no seqlock either: the cursor's `Release`/`Acquire` pair is the
//! entire publication edge.
//!
//! Because nothing is copied, the cost is proportional to what you actually
//! touch rather than to `size_of::<T>()`. Updating a few fields of a large,
//! mostly-stable message is dramatically cheaper than a ring that must copy the
//! whole value in and out; rewriting the entire payload every message is not.
//!
//! The trade is that **every** subscriber gates the publisher. There are no
//! lossy observers here — a reader that could be lapped is exactly the reader
//! this design excludes. Use [`channel`](crate::channel()) when you want those.
//!
//! ```
//! use photon_ring::event_channel;
//!
//! # #[derive(Default)]
//! struct Order { symbol: String, qty: u32 }
//!
//! let (mut tx, rx) = event_channel(64, || Order { symbol: String::new(), qty: 0 });
//! let mut sub = rx.subscribe();
//!
//! tx.publish(|o| {
//!     o.symbol.clear();
//!     o.symbol.push_str("ETHUSD");   // reuses the existing allocation
//!     o.qty = 5;
//! });
//!
//! let qty = sub.process(|o| o.qty).unwrap();
//! assert_eq!(qty, 5);
//! ```

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

use crate::ring::{Padded, RingIndex};

/// Shared state: slots that own their values, plus the publication cursor and
/// the subscriber trackers the publisher is gated by.
struct EventRing<T> {
    slots: Box<[UnsafeCell<T>]>,
    index: RingIndex,
    /// Sequence of the last published message; `u64::MAX` before the first.
    cursor: Padded<AtomicU64>,
    trackers: Mutex<Vec<Weak<Padded<AtomicU64>>>>,
}

// SAFETY: a slot is only ever accessed by the publisher while no subscriber can
// reach it (guaranteed by the backpressure invariant: the publisher will not
// advance past `slowest + capacity`), and only ever by subscribers once the
// cursor has been released past it. `T: Send` is required to move values across
// the threads that touch them, and `T: Sync` because several subscribers can
// hold `&T` into the same slot at once — without it a payload with interior
// mutability would let safe code race through those shared references.
unsafe impl<T: Send + Sync> Send for EventRing<T> {}
unsafe impl<T: Send + Sync> Sync for EventRing<T> {}

impl<T> EventRing<T> {
    /// The lowest sequence any live subscriber still needs, or `None` if there
    /// are none. Prunes dropped subscribers, which is what lets a dead consumer
    /// release the publisher instead of wedging it.
    fn slowest(&self) -> Option<u64> {
        let mut trackers = self.trackers.lock();
        let mut min = u64::MAX;
        let mut any = false;
        trackers.retain(|weak| match weak.upgrade() {
            Some(t) => {
                min = min.min(t.0.load(Ordering::Acquire));
                any = true;
                true
            }
            None => false,
        });
        any.then_some(min)
    }
}

/// The write side. Single producer: `&mut self` enforces it without atomics.
pub struct EventPublisher<T> {
    ring: Arc<EventRing<T>>,
    seq: u64,
    cached_slowest: u64,
}

// SAFETY: see EventRing.
unsafe impl<T: Send + Sync> Send for EventPublisher<T> {}

impl<T> EventPublisher<T> {
    /// Whether sequence `self.seq` can be written without overtaking a
    /// subscriber. Mirrors the bounded channel's cached fast path.
    fn has_room(&mut self) -> bool {
        let capacity = self.ring.index.capacity;
        if self.seq >= self.cached_slowest + capacity {
            match self.ring.slowest() {
                Some(slowest) => {
                    self.cached_slowest = slowest;
                    if self.seq >= slowest + capacity {
                        return false;
                    }
                }
                // No subscribers: nothing to protect.
                None => return true,
            }
        }
        true
    }

    /// Fill the next slot, blocking until a subscriber frees one.
    ///
    /// The closure receives the slot's existing value to mutate. Nothing is
    /// allocated or copied, so a payload that owns a `String` or `Vec` reuses
    /// the capacity it had from the previous time round the ring.
    ///
    /// If the closure panics the value is left as the closure altered it —
    /// still a valid `T` — and the message is not published.
    pub fn publish(&mut self, f: impl FnOnce(&mut T)) {
        while !self.has_room() {
            core::hint::spin_loop();
        }
        self.write(f);
    }

    /// Fill the next slot, or return `false` if that would overtake a subscriber.
    pub fn try_publish(&mut self, f: impl FnOnce(&mut T)) -> bool {
        if !self.has_room() {
            return false;
        }
        self.write(f);
        true
    }

    fn write(&mut self, f: impl FnOnce(&mut T)) {
        let idx = self.ring.index.slot(self.seq);
        // SAFETY: `has_room` established that no subscriber can still be reading
        // this slot, and `&mut self` means no other publisher exists.
        f(unsafe { &mut *self.ring.slots[idx].get() });
        // Release: the mutation above is visible to any subscriber that acquires
        // this cursor value. This is the entire publication edge.
        self.ring.cursor.0.store(self.seq, Ordering::Release);
        self.seq += 1;
    }

    /// Messages published so far.
    pub fn published(&self) -> u64 {
        self.seq
    }

    /// Ring capacity.
    pub fn capacity(&self) -> u64 {
        self.ring.index.capacity
    }
}

/// Clone-able handle for creating subscribers.
pub struct EventSubscribable<T> {
    ring: Arc<EventRing<T>>,
}

// SAFETY: see EventRing.
unsafe impl<T: Send + Sync> Send for EventSubscribable<T> {}
unsafe impl<T: Send + Sync> Sync for EventSubscribable<T> {}

impl<T> Clone for EventSubscribable<T> {
    fn clone(&self) -> Self {
        EventSubscribable {
            ring: self.ring.clone(),
        }
    }
}

impl<T> EventSubscribable<T> {
    /// Create a subscriber, starting from the next message published.
    ///
    /// Every subscriber gates the publisher, so one that stops reading will
    /// stop the ring. Dropping it releases the publisher again.
    pub fn subscribe(&self) -> EventSubscriber<T> {
        // The start position is chosen under the tracker lock so that it is
        // atomic with respect to the publisher's scan; see
        // `SharedRing::register_tracker_at_head` for why that matters.
        let mut trackers = self.ring.trackers.lock();
        let head = self.ring.cursor.0.load(Ordering::Acquire);
        let start = if head == u64::MAX { 0 } else { head + 1 };
        let tracker = Arc::new(Padded(AtomicU64::new(start)));
        trackers.push(Arc::downgrade(&tracker));
        drop(trackers);
        EventSubscriber {
            ring: self.ring.clone(),
            cursor: start,
            tracker,
        }
    }
}

/// The read side. Reads borrow the value in place; nothing is copied out.
pub struct EventSubscriber<T> {
    ring: Arc<EventRing<T>>,
    cursor: u64,
    tracker: Arc<Padded<AtomicU64>>,
}

// SAFETY: see EventRing.
unsafe impl<T: Send + Sync> Send for EventSubscriber<T> {}

impl<T> EventSubscriber<T> {
    /// Run `f` on the next message, if one is available.
    ///
    /// The value is borrowed from the ring, not copied out of it. The slot is
    /// released for reuse when `f` returns; if `f` panics the message is not
    /// consumed and will be seen again.
    pub fn process<R>(&mut self, f: impl FnOnce(&T) -> R) -> Option<R> {
        // Acquire pairs with the publisher's Release store, making its mutation
        // of this slot visible.
        let head = self.ring.cursor.0.load(Ordering::Acquire);
        if head == u64::MAX || self.cursor > head {
            return None;
        }
        let idx = self.ring.index.slot(self.cursor);
        // SAFETY: the publisher cannot reach this slot while our tracker sits at
        // `self.cursor`, and the cursor load above established the value is
        // fully written.
        let out = f(unsafe { &*self.ring.slots[idx].get() });
        self.cursor += 1;
        // Release: everything we read above happens-before the publisher's
        // Acquire load of this tracker, so it cannot overwrite the slot early.
        self.tracker.0.store(self.cursor, Ordering::Release);
        Some(out)
    }

    /// Sequence this subscriber will read next.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Messages published but not yet processed by this subscriber.
    pub fn pending(&self) -> u64 {
        let head = self.ring.cursor.0.load(Ordering::Acquire);
        if head == u64::MAX || self.cursor > head {
            0
        } else {
            head - self.cursor + 1
        }
    }
}

/// Create a ring of `capacity` values built by `factory`.
///
/// `T` must be `Sync` as well as `Send`: several subscribers can hold a
/// reference into the same slot at once, so a payload with interior mutability
/// would let them race.
///
/// Values are created once, up front, and reused for the life of the ring, so
/// steady-state publishing allocates nothing even for payloads that own heap
/// data.
///
/// # Panics
///
/// Panics if `capacity < 2`.
pub fn event_channel<T: Send + Sync + 'static>(
    capacity: usize,
    mut factory: impl FnMut() -> T,
) -> (EventPublisher<T>, EventSubscribable<T>) {
    let index = RingIndex::new(capacity);
    let slots: Vec<UnsafeCell<T>> = (0..capacity).map(|_| UnsafeCell::new(factory())).collect();
    let ring = Arc::new(EventRing {
        slots: slots.into_boxed_slice(),
        index,
        cursor: Padded(AtomicU64::new(u64::MAX)),
        trackers: Mutex::new(Vec::new()),
    });
    (
        EventPublisher {
            ring: ring.clone(),
            seq: 0,
            cached_slowest: 0,
        },
        EventSubscribable { ring },
    )
}
