// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! # Photon Ring
//!
//! Ultra-low-latency SPMC pub/sub using seqlock-stamped ring buffers.
//!
//! Fully `no_std` compatible (requires `alloc`). Every type — including the
//! named-topic [`Photon`] bus — works without the standard library.
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
//! let (mut pub_, subs) = photon_ring::channel::<u64>(64);
//! let mut sub = subs.subscribe();
//! pub_.publish(42);
//! assert_eq!(sub.try_recv(), Ok(42));
//!
//! // Named-topic bus
//! let bus = photon_ring::Photon::<u64>::new(64);
//! let mut p = bus.publisher("topic-a");
//! let mut s = bus.subscribe("topic-a");
//! p.publish(7);
//! assert_eq!(s.try_recv(), Ok(7));
//! ```

#![no_std]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "affinity")]
pub mod affinity;
mod bus;
pub mod channel;
pub(crate) mod ring;
pub(crate) mod slot;
pub mod wait;

pub use bus::Photon;
pub use channel::{
    channel, channel_bounded, PublishError, Publisher, Subscribable, Subscriber, SubscriberGroup,
    TryRecvError,
};
pub use wait::WaitStrategy;
