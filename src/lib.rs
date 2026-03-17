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
//! - **`T: Pod`** — restricts payloads to plain-old-data types where every bit
//!   pattern is valid, making torn seqlock reads harmless (no UB).
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
pub mod barrier;
mod bus;
pub mod channel;
#[cfg(all(target_os = "linux", feature = "hugepages"))]
pub mod mem;
mod pod;
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

pub use barrier::DependencyBarrier;
pub use bus::Photon;
pub use channel::{
    channel, channel_bounded, channel_mpmc, Drain, MpPublisher, PublishError, Publisher,
    Subscribable, Subscriber, SubscriberGroup, TryRecvError,
};
pub use pod::Pod;
pub use ring::Padded;

/// Derive macro for the [`Pod`] trait. Requires the `derive` feature.
///
/// ```ignore
/// #[derive(photon_ring::DerivePod, Clone, Copy)]
/// #[repr(C)]
/// struct Quote { price: f64, volume: u32 }
/// ```
#[cfg(feature = "derive")]
pub use photon_ring_derive::Pod as DerivePod;

/// Derive macro that generates a Pod-compatible wire struct from a domain struct.
///
/// Given a struct with `bool`, `Option<numeric>`, `usize`/`isize`, and
/// `#[repr(u8)]` enum fields, generates `{Name}Wire` plus `From` conversions
/// in both directions. Requires the `derive` feature.
///
/// ```ignore
/// #[derive(photon_ring::DeriveMessage)]
/// struct Order { price: f64, side: Side, filled: bool, tag: Option<u32> }
/// // Generates: OrderWire, From<Order> for OrderWire, From<OrderWire> for Order
/// ```
#[cfg(feature = "derive")]
pub use photon_ring_derive::Message as DeriveMessage;

pub use shutdown::Shutdown;
pub use typed_bus::TypedBus;
pub use wait::WaitStrategy;
