// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! # photon-ring-async
//!
//! Runtime-agnostic async wrappers for [`photon_ring`]'s synchronous pub/sub
//! channels.
//!
//! photon-ring is a `#![no_std]` ultra-low-latency ring buffer. Its publisher
//! has no concept of wakers -- there is no notification mechanism when a new
//! message is published. This crate bridges that gap with a **yield-based
//! polling** strategy: the async subscriber tries `spin_budget` synchronous
//! `try_recv()` calls, then yields back to the async runtime so other tasks
//! can make progress. On the next poll, it tries again.
//!
//! This is *not* event-driven wakeup. It is cooperative spin-polling with
//! periodic yields. The trade-off: slightly higher latency than bare spin
//! (one async scheduling round-trip per yield, ~200-500 ns on tokio), and
//! other tasks get to run between polls — but the waiting task re-registers
//! its waker on every empty poll, so it never goes to sleep.
//!
//! **Idle cost.** A task awaiting an idle channel stays permanently runnable:
//! the executor re-polls it in a loop, keeping one core at 100% for as long
//! as no message arrives. Other tasks on the runtime still make progress
//! (each poll yields), but the CPU never idles. This is the right trade for
//! streams that are rarely quiet — market data, telemetry under load — and
//! the wrong one for workloads that sit idle. For those, run the subscriber
//! on a dedicated thread (`photon_ring::topology::Consumer` with a parking
//! [`WaitStrategy`](photon_ring::WaitStrategy)) and hand results to your
//! runtime through its own notification-based channel.
//!
//! **No runtime dependency.** This crate uses only `core::task::{Context,
//! Poll, Waker}` and `core::future::poll_fn`. It works with tokio, smol,
//! async-std, embassy, or any other executor.
//!
//! ## Quick start
//!
//! ```rust,no_run
//! # async fn example() {
//! let (mut pub_, subs) = photon_ring::channel::<u64>(64);
//! let mut async_sub = photon_ring_async::AsyncSubscriber::new(subs.subscribe());
//!
//! pub_.publish(42);
//! let value = async_sub.recv().await;
//! assert_eq!(value, 42);
//! # }
//! ```

mod group;
mod subscriber;

pub use group::{AsyncSubscriberGroup, GroupRecvFuture};
pub use subscriber::{AsyncSubscriber, RecvFuture};

/// Default number of synchronous `try_recv()` attempts before yielding to
/// the async runtime.
///
/// 64 matches the bare-spin phase of `Subscriber::recv()`, giving the same
/// fast-path latency when messages arrive quickly.
pub const DEFAULT_SPIN_BUDGET: u32 = 64;
