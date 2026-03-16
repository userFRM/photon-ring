// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! # Photon Ring
//!
//! Ultra-low-latency SPMC pub/sub using seqlock-stamped ring buffers.
//!
//! `no_std` compatible (requires `alloc`). The [`topology`] module uses
//! OS threads and is available on Linux, macOS, Windows, and other
//! supported platforms.
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

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "android",
))]
pub mod affinity;
mod bus;
pub mod channel;
#[cfg(all(target_os = "linux", feature = "hugepages"))]
pub mod mem;
pub(crate) mod ring;
mod shutdown;
pub(crate) mod slot;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "android",
))]
pub mod topology;
mod typed_bus;
pub mod wait;

pub use bus::Photon;
pub use channel::{
    channel, channel_bounded, channel_mpmc, Drain, MpPublisher, PublishError, Publisher,
    Subscribable, Subscriber, SubscriberGroup, TryRecvError,
};
pub use shutdown::Shutdown;
pub use typed_bus::TypedBus;
pub use wait::WaitStrategy;
