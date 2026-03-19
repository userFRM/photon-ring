// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! Builder-pattern topology for multi-stage processing pipelines.
//!
//! Inspired by LMAX Disruptor's `handleEventsWith(A).then(B)` pattern,
//! but idiomatic Rust: each stage has concrete input/output types and
//! runs on a dedicated thread.
//!
//! # Linear pipeline
//!
//! ```
//! use photon_ring::topology::Pipeline;
//!
//! let (mut publisher, stages) = Pipeline::builder()
//!     .capacity(64)
//!     .input::<u64>();
//!
//! let (mut output, pipeline) = stages
//!     .then(|x: u64| x * 2)
//!     .then(|x: u64| x + 1)
//!     .build();
//!
//! publisher.publish(10);
//! let result = output.recv();
//! assert_eq!(result, 21);
//!
//! pipeline.shutdown();
//! pipeline.join();
//! ```
//!
//! # Fan-out (diamond) topology
//!
//! ```
//! use photon_ring::topology::Pipeline;
//!
//! let (mut publisher, stages) = Pipeline::builder()
//!     .capacity(64)
//!     .input::<u64>();
//!
//! let (mut outputs, pipeline) = stages
//!     .fan_out(|x: u64| x * 2, |x: u64| x + 100)
//!     .build();
//!
//! publisher.publish(5);
//!
//! let val_a = outputs.0.recv();
//! let val_b = outputs.1.recv();
//! assert_eq!(val_a, 10);
//! assert_eq!(val_b, 105);
//!
//! pipeline.shutdown();
//! pipeline.join();
//! ```
//!
//! # Panic handling
//!
//! If a stage closure panics, the panic is captured. Call
//! [`Pipeline::panicked_stages`] to inspect which stages failed.
//!
//! # Platform availability
//!
//! This module spawns OS threads for each processing stage and is
//! available on Linux, macOS, Windows, FreeBSD, NetBSD, and Android.

extern crate std;

mod builder;
mod fan_out;
mod pipeline;

pub use builder::{PipelineBuilder, StageBuilder};
pub use fan_out::FanOutBuilder;
pub use pipeline::Pipeline;

use crate::channel::{Publisher, Subscriber};
use crate::pod::Pod;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::{self, JoinHandle};

/// Default ring capacity when none is specified.
const DEFAULT_CAPACITY: usize = 1024;

// ---------------------------------------------------------------------------
// Stage status constants
// ---------------------------------------------------------------------------

const STAGE_RUNNING: u8 = 0;
const STAGE_COMPLETED: u8 = 1;
const STAGE_PANICKED: u8 = 2;

// ---------------------------------------------------------------------------
// Shared internals carried through the builder chain
// ---------------------------------------------------------------------------

/// Shared state accumulated during pipeline construction.
///
/// Uses `Arc` so the same instance is shared between the builder chain
/// and the spawned stage threads. The `Mutex` is only held briefly
/// during `push` (build-time) and `iter` (query-time).
struct SharedState {
    shutdown: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    statuses: Vec<Arc<AtomicU8>>,
}

impl SharedState {
    fn new() -> Self {
        SharedState {
            shutdown: Arc::new(AtomicBool::new(false)),
            handles: Vec::new(),
            statuses: Vec::new(),
        }
    }
}

/// Spawn a stage thread that reads from `input`, applies `f`, and
/// publishes to `output`. Returns the `Arc<AtomicU8>` status handle
/// and the `JoinHandle`.
fn spawn_stage<T, U>(
    mut input: Subscriber<T>,
    mut output: Publisher<U>,
    shutdown: Arc<AtomicBool>,
    f: impl Fn(T) -> U + Send + 'static,
) -> (Arc<AtomicU8>, JoinHandle<()>)
where
    T: Pod,
    U: Pod,
{
    let status = Arc::new(AtomicU8::new(STAGE_RUNNING));
    let status_inner = status.clone();

    let handle = thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            loop {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                match input.try_recv() {
                    Ok(value) => {
                        let out = f(value);
                        output.publish(out);
                    }
                    Err(crate::channel::TryRecvError::Empty) => {
                        core::hint::spin_loop();
                    }
                    Err(crate::channel::TryRecvError::Lagged { .. }) => {
                        // Cursor was advanced by try_recv, retry immediately.
                        core::hint::spin_loop();
                    }
                }
            }
        }));
        match result {
            Ok(()) => status_inner.store(STAGE_COMPLETED, Ordering::Release),
            Err(_) => status_inner.store(STAGE_PANICKED, Ordering::Release),
        }
    });

    (status, handle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_stage_pipeline() {
        let (mut pub_, stages) = Pipeline::builder().capacity(64).input::<u64>();

        let (mut output, pipeline) = stages.then(|x: u64| x * 3).build();

        pub_.publish(7);
        assert_eq!(output.recv(), 21);

        pipeline.shutdown();
        pipeline.join();
    }

    #[test]
    fn two_stage_pipeline() {
        let (mut pub_, stages) = Pipeline::builder().capacity(64).input::<u64>();

        let (mut output, pipeline) = stages.then(|x: u64| x * 2).then(|x: u64| x + 1).build();

        pub_.publish(10);
        assert_eq!(output.recv(), 21);

        pipeline.shutdown();
        pipeline.join();
    }

    #[test]
    fn three_stage_pipeline() {
        let (mut pub_, stages) = Pipeline::builder().capacity(64).input::<i64>();

        let (mut output, pipeline) = stages
            .then(|x: i64| x + 10)
            .then(|x: i64| x * 2)
            .then(|x: i64| x - 5)
            .build();

        pub_.publish(5);
        // (5 + 10) * 2 - 5 = 25
        assert_eq!(output.recv(), 25);

        pipeline.shutdown();
        pipeline.join();
    }

    #[test]
    fn pipeline_multiple_messages() {
        // Capacity must exceed message count to avoid lossy ring drops.
        let (mut pub_, stages) = Pipeline::builder().capacity(256).input::<u64>();

        let (mut output, pipeline) = stages.then(|x: u64| x + 1).build();

        for i in 0..100u64 {
            pub_.publish(i);
        }
        for i in 0..100u64 {
            assert_eq!(output.recv(), i + 1);
        }

        pipeline.shutdown();
        pipeline.join();
    }

    #[test]
    fn pipeline_type_transform() {
        #[derive(Clone, Copy)]
        #[repr(C)]
        struct Input {
            value: f64,
        }
        // SAFETY: Input is #[repr(C)] with a single f64 field;
        // every bit pattern is a valid f64.
        unsafe impl crate::Pod for Input {}

        #[derive(Clone, Copy, Debug, PartialEq)]
        #[repr(C)]
        struct Output {
            doubled: f64,
            positive: u8,
        }
        // SAFETY: Output is #[repr(C)] with f64 and u8 fields;
        // every bit pattern is valid.
        unsafe impl crate::Pod for Output {}

        let (mut pub_, stages) = Pipeline::builder().capacity(64).input::<Input>();

        let (mut output, pipeline) = stages
            .then(|inp: Input| Output {
                doubled: inp.value * 2.0,
                positive: if inp.value > 0.0 { 1 } else { 0 },
            })
            .build();

        pub_.publish(Input { value: 3.5 });
        let out = output.recv();
        assert_eq!(out.doubled, 7.0);
        assert_eq!(out.positive, 1);

        pub_.publish(Input { value: -1.0 });
        let out = output.recv();
        assert_eq!(out.doubled, -2.0);
        assert_eq!(out.positive, 0);

        pipeline.shutdown();
        pipeline.join();
    }

    #[test]
    fn fan_out_basic() {
        let (mut pub_, stages) = Pipeline::builder().capacity(64).input::<u64>();

        let ((mut out_a, mut out_b), pipeline) =
            stages.fan_out(|x: u64| x * 2, |x: u64| x + 100).build();

        pub_.publish(5);
        assert_eq!(out_a.recv(), 10);
        assert_eq!(out_b.recv(), 105);

        pipeline.shutdown();
        pipeline.join();
    }

    #[test]
    fn fan_out_multiple_messages() {
        let (mut pub_, stages) = Pipeline::builder().capacity(64).input::<u64>();

        let ((mut out_a, mut out_b), pipeline) =
            stages.fan_out(|x: u64| x * 10, |x: u64| x + 1).build();

        for i in 0..50u64 {
            pub_.publish(i);
        }
        for i in 0..50u64 {
            assert_eq!(out_a.recv(), i * 10);
            assert_eq!(out_b.recv(), i + 1);
        }

        pipeline.shutdown();
        pipeline.join();
    }

    #[test]
    fn fan_out_then_a() {
        let (mut pub_, stages) = Pipeline::builder().capacity(64).input::<u64>();

        let ((mut out_a, mut out_b), pipeline) = stages
            .fan_out(|x: u64| x * 2, |x: u64| x + 100)
            .then_a(|x: u64| x + 1)
            .build();

        pub_.publish(5);
        assert_eq!(out_a.recv(), 11); // 5 * 2 + 1
        assert_eq!(out_b.recv(), 105); // 5 + 100

        pipeline.shutdown();
        pipeline.join();
    }

    #[test]
    fn fan_out_then_b() {
        let (mut pub_, stages) = Pipeline::builder().capacity(64).input::<u64>();

        let ((mut out_a, mut out_b), pipeline) = stages
            .fan_out(|x: u64| x * 2, |x: u64| x + 100)
            .then_b(|x: u64| x * 3)
            .build();

        pub_.publish(5);
        assert_eq!(out_a.recv(), 10); // 5 * 2
        assert_eq!(out_b.recv(), 315); // (5 + 100) * 3

        pipeline.shutdown();
        pipeline.join();
    }

    #[test]
    fn fan_out_then_both() {
        let (mut pub_, stages) = Pipeline::builder().capacity(64).input::<u64>();

        let ((mut out_a, mut out_b), pipeline) = stages
            .fan_out(|x: u64| x * 2, |x: u64| x + 100)
            .then_a(|x: u64| x + 1)
            .then_b(|x: u64| x * 3)
            .build();

        pub_.publish(5);
        assert_eq!(out_a.recv(), 11); // 5 * 2 + 1
        assert_eq!(out_b.recv(), 315); // (5 + 100) * 3

        pipeline.shutdown();
        pipeline.join();
    }

    #[test]
    fn pipeline_stage_count() {
        let (_, stages) = Pipeline::builder().capacity(64).input::<u64>();

        let (_, pipeline) = stages.then(|x: u64| x).then(|x: u64| x).build();

        assert_eq!(pipeline.stage_count(), 2);

        pipeline.shutdown();
        pipeline.join();
    }

    #[test]
    fn pipeline_is_healthy() {
        let (_, stages) = Pipeline::builder().capacity(64).input::<u64>();

        let (_, pipeline) = stages.then(|x: u64| x).build();

        assert!(pipeline.is_healthy());
        assert!(pipeline.panicked_stages().is_empty());

        pipeline.shutdown();
        pipeline.join();
    }

    #[test]
    fn pipeline_detects_panic() {
        let (mut pub_, stages) = Pipeline::builder().capacity(64).input::<u64>();

        let (_, pipeline) = stages
            .then(|x: u64| -> u64 {
                if x == 42 {
                    panic!("test panic");
                }
                x
            })
            .build();

        // Send the panic-inducing value.
        pub_.publish(42);

        // Wait for the stage to detect the panic. Use a generous timeout
        // for slow CI environments (e.g., Windows GitHub Actions runners).
        let mut panicked = false;
        for _ in 0..100 {
            if !pipeline.panicked_stages().is_empty() {
                panicked = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(panicked, "expected stage to detect panic");
        assert_eq!(pipeline.panicked_stages(), alloc::vec![0]);

        pipeline.shutdown();
        pipeline.join();
    }

    #[test]
    fn pipeline_default_builder() {
        let builder = PipelineBuilder::default();
        let (mut pub_, stages) = builder.input::<u64>();
        let (mut output, pipeline) = stages.then(|x: u64| x + 1).build();

        pub_.publish(9);
        assert_eq!(output.recv(), 10);

        pipeline.shutdown();
        pipeline.join();
    }

    #[test]
    fn pipeline_linear_then_fan_out() {
        let (mut pub_, stages) = Pipeline::builder().capacity(64).input::<u64>();

        let ((mut out_a, mut out_b), pipeline) = stages
            .then(|x: u64| x + 10)
            .fan_out(|x: u64| x * 2, |x: u64| x * 3)
            .build();

        pub_.publish(5);
        // (5 + 10) * 2 = 30
        assert_eq!(out_a.recv(), 30);
        // (5 + 10) * 3 = 45
        assert_eq!(out_b.recv(), 45);

        assert_eq!(pipeline.stage_count(), 3);

        pipeline.shutdown();
        pipeline.join();
    }

    #[test]
    fn zero_stage_pipeline() {
        let (mut pub_, stages) = Pipeline::builder().capacity(64).input::<u64>();

        let (mut output, pipeline) = stages.build();

        pub_.publish(42);
        assert_eq!(output.recv(), 42);

        assert_eq!(pipeline.stage_count(), 0);

        pipeline.shutdown();
        pipeline.join();
    }
}
