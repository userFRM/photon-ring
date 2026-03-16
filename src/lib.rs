//! # Photon
//!
//! Ultra-low-latency SPMC pub/sub using seqlock-stamped ring buffers.
//!
//! ## Key design
//!
//! - **Seqlock per slot** — stamp and payload share a cache line; readers never
//!   take a lock, writers never allocate.
//! - **`T: Copy`** — enables safe `memcpy` reads; torn reads are detected and
//!   retried (no `Drop` / double-free concerns).
//! - **Per-consumer cursor** — zero contention between subscribers.
//! - **Single-producer** — no write-side synchronisation; the seqlock invariant
//!   is upheld by `&mut self` on [`Publisher::publish`].
//!
//! ## Quick start
//!
//! ```
//! // Low-level SPMC channel
//! let (mut pub_, subs) = photon::channel::<u64>(64);
//! let mut sub = subs.subscribe();
//! pub_.publish(42);
//! assert_eq!(sub.try_recv(), Ok(42));
//!
//! // Named-topic bus
//! let bus = photon::Photon::<u64>::new(64);
//! let mut p = bus.publisher("topic-a");
//! let mut s = bus.subscribe("topic-a");
//! p.publish(7);
//! assert_eq!(s.try_recv(), Ok(7));
//! ```

mod bus;
pub mod channel;
pub(crate) mod ring;
pub(crate) mod slot;

pub use bus::Photon;
pub use channel::{channel, Publisher, Subscribable, Subscriber, TryRecvError};
