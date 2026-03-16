use crate::ring::SharedRing;
use std::sync::atomic::Ordering;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Error from [`Subscriber::try_recv`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TryRecvError {
    /// No new messages available.
    Empty,
    /// Consumer fell behind the ring. `skipped` messages were lost.
    Lagged { skipped: u64 },
}

// ---------------------------------------------------------------------------
// Publisher (single-producer write side)
// ---------------------------------------------------------------------------

/// The write side of a Photon SPMC channel.
///
/// There is exactly one `Publisher` per channel. It is `Send` but not `Sync` —
/// only one thread may publish at a time (single-producer guarantee enforced
/// by `&mut self`).
pub struct Publisher<T: Copy> {
    ring: Arc<SharedRing<T>>,
    seq: u64,
}

unsafe impl<T: Copy + Send> Send for Publisher<T> {}

impl<T: Copy> Publisher<T> {
    /// Publish a single value. Zero-allocation, O(1).
    #[inline]
    pub fn publish(&mut self, value: T) {
        self.ring.slot(self.seq).write(self.seq, value);
        self.ring.cursor.0.store(self.seq, Ordering::Release);
        self.seq += 1;
    }

    /// Publish a batch of values with a single cursor update.
    ///
    /// Each slot is written atomically (seqlock), but the cursor advances only
    /// once at the end — consumers see the entire batch appear at once, and
    /// cache-line bouncing on the shared cursor is reduced to one store.
    #[inline]
    pub fn publish_batch(&mut self, values: &[T]) {
        if values.is_empty() {
            return;
        }
        for (i, &v) in values.iter().enumerate() {
            let seq = self.seq + i as u64;
            self.ring.slot(seq).write(seq, v);
        }
        let last = self.seq + values.len() as u64 - 1;
        self.ring.cursor.0.store(last, Ordering::Release);
        self.seq += values.len() as u64;
    }

    /// Number of messages published so far.
    #[inline]
    pub fn published(&self) -> u64 {
        self.seq
    }

    /// Ring capacity (power of two).
    #[inline]
    pub fn capacity(&self) -> u64 {
        self.ring.capacity()
    }
}

// ---------------------------------------------------------------------------
// Subscribable (factory for subscribers)
// ---------------------------------------------------------------------------

/// Clone-able handle for spawning [`Subscriber`]s.
///
/// Send this to other threads and call [`subscribe`](Subscribable::subscribe)
/// to create independent consumers.
pub struct Subscribable<T: Copy> {
    ring: Arc<SharedRing<T>>,
}

impl<T: Copy> Clone for Subscribable<T> {
    fn clone(&self) -> Self {
        Subscribable {
            ring: self.ring.clone(),
        }
    }
}

unsafe impl<T: Copy + Send> Send for Subscribable<T> {}
unsafe impl<T: Copy + Send> Sync for Subscribable<T> {}

impl<T: Copy> Subscribable<T> {
    /// Create a subscriber that will see only **future** messages.
    pub fn subscribe(&self) -> Subscriber<T> {
        let head = self.ring.cursor.0.load(Ordering::Acquire);
        let start = if head == u64::MAX { 0 } else { head + 1 };
        Subscriber {
            ring: self.ring.clone(),
            cursor: start,
        }
    }

    /// Create a subscriber starting from the **oldest available** message
    /// still in the ring (or 0 if nothing published yet).
    pub fn subscribe_from_oldest(&self) -> Subscriber<T> {
        let head = self.ring.cursor.0.load(Ordering::Acquire);
        let cap = self.ring.capacity();
        let start = if head == u64::MAX {
            0
        } else if head >= cap {
            head - cap + 1
        } else {
            0
        };
        Subscriber {
            ring: self.ring.clone(),
            cursor: start,
        }
    }
}

// ---------------------------------------------------------------------------
// Subscriber (consumer read side)
// ---------------------------------------------------------------------------

/// The read side of a Photon SPMC channel.
///
/// Each subscriber has its own cursor — no contention between consumers.
pub struct Subscriber<T: Copy> {
    ring: Arc<SharedRing<T>>,
    cursor: u64,
}

unsafe impl<T: Copy + Send> Send for Subscriber<T> {}

impl<T: Copy> Subscriber<T> {
    /// Try to receive the next message without blocking.
    #[inline]
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let head = self.ring.cursor.0.load(Ordering::Acquire);

        if head == u64::MAX || self.cursor > head {
            return Err(TryRecvError::Empty);
        }

        // Fast-path lag check (avoids touching the slot cache line)
        let cap = self.ring.capacity();
        if head >= cap {
            let oldest = head - cap + 1;
            if self.cursor < oldest {
                let skipped = oldest - self.cursor;
                self.cursor = oldest;
                return Err(TryRecvError::Lagged { skipped });
            }
        }

        self.read_slot()
    }

    /// Spin until the next message is available and return it.
    #[inline]
    pub fn recv(&mut self) -> T {
        loop {
            match self.try_recv() {
                Ok(val) => return val,
                Err(TryRecvError::Empty) => core::hint::spin_loop(),
                Err(TryRecvError::Lagged { .. }) => {
                    // Cursor already advanced past the gap — retry immediately.
                }
            }
        }
    }

    /// Skip to the **latest** published message (discards intermediate ones).
    ///
    /// Returns `None` only if nothing has been published yet. Under heavy
    /// producer load, retries internally if the target slot is mid-write.
    pub fn latest(&mut self) -> Option<T> {
        loop {
            let head = self.ring.cursor.0.load(Ordering::Acquire);
            if head == u64::MAX {
                return None;
            }
            self.cursor = head;
            match self.read_slot() {
                Ok(v) => return Some(v),
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Lagged { .. }) => {
                    // Producer lapped us between cursor read and slot read.
                    // Retry with updated head.
                }
            }
        }
    }

    /// How many messages are available to read (capped at ring capacity).
    #[inline]
    pub fn pending(&self) -> u64 {
        let head = self.ring.cursor.0.load(Ordering::Acquire);
        if head == u64::MAX || self.cursor > head {
            0
        } else {
            let raw = head - self.cursor + 1;
            raw.min(self.ring.capacity())
        }
    }

    /// Internal seqlock read with retry.
    #[inline]
    fn read_slot(&mut self) -> Result<T, TryRecvError> {
        let slot = self.ring.slot(self.cursor);

        loop {
            match slot.try_read(self.cursor) {
                Ok(Some(value)) => {
                    self.cursor += 1;
                    return Ok(value);
                }
                Ok(None) => {
                    // Torn read or write-in-progress — spin and retry.
                    core::hint::spin_loop();
                }
                Err(actual_stamp) => {
                    let expected_stamp = self.cursor * 2 + 2;
                    if actual_stamp < expected_stamp {
                        return Err(TryRecvError::Empty);
                    }
                    // Slot was overwritten — recompute oldest from head cursor
                    // (authoritative source) rather than inferring from the
                    // slot stamp, which only tells us about this one slot.
                    let head = self.ring.cursor.0.load(Ordering::Acquire);
                    let cap = self.ring.capacity();
                    if head >= cap {
                        let oldest = head - cap + 1;
                        if self.cursor < oldest {
                            let skipped = oldest - self.cursor;
                            self.cursor = oldest;
                            return Err(TryRecvError::Lagged { skipped });
                        }
                    }
                    // Head hasn't caught up yet (rare timing race) — spin
                    core::hint::spin_loop();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

/// Create a Photon SPMC channel.
///
/// `capacity` must be a power of two (≥ 2). Returns the single-producer
/// write end and a clone-able factory for creating consumers.
///
/// # Example
/// ```
/// let (mut pub_, subs) = photon_ring::channel::<u64>(64);
/// let mut sub = subs.subscribe();
/// pub_.publish(42);
/// assert_eq!(sub.try_recv(), Ok(42));
/// ```
pub fn channel<T: Copy + Send>(capacity: usize) -> (Publisher<T>, Subscribable<T>) {
    let ring = Arc::new(SharedRing::new(capacity));
    (
        Publisher {
            ring: ring.clone(),
            seq: 0,
        },
        Subscribable { ring },
    )
}
