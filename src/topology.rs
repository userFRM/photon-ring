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

use crate::channel::{self, Publisher, Subscribable, Subscriber};
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
    T: Copy + Send + 'static,
    U: Copy + Send + 'static,
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
                        // Upstream overrun — skip ahead.
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
// Pipeline (the finalized handle)
// ---------------------------------------------------------------------------

/// A multi-stage processing pipeline.
///
/// Each stage reads from one Photon Ring channel, applies a transformation,
/// and publishes to the next channel. Stages run on dedicated threads.
///
/// Created via [`Pipeline::builder`] -> [`PipelineBuilder::input`] ->
/// [`StageBuilder::then`] / [`StageBuilder::fan_out`] -> [`StageBuilder::build`].
pub struct Pipeline {
    handles: Vec<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    statuses: Vec<Arc<AtomicU8>>,
}

impl Pipeline {
    /// Create a new pipeline builder.
    pub fn builder() -> PipelineBuilder {
        PipelineBuilder::new()
    }

    /// Signal all stages to shut down gracefully.
    ///
    /// Stages will finish processing their current item and then exit.
    /// Call [`join`](Pipeline::join) to wait for them to complete.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    /// Wait for all stage threads to finish.
    ///
    /// This consumes the pipeline. Call [`shutdown`](Pipeline::shutdown)
    /// first, or the threads may run indefinitely.
    pub fn join(self) {
        for h in self.handles {
            let _ = h.join();
        }
    }

    /// Return the indices of stages that have panicked.
    ///
    /// Returns an empty vec if all stages are healthy.
    pub fn panicked_stages(&self) -> Vec<usize> {
        self.statuses
            .iter()
            .enumerate()
            .filter(|(_, s)| s.load(Ordering::Acquire) == STAGE_PANICKED)
            .map(|(i, _)| i)
            .collect()
    }

    /// Returns `true` if all stages are still running (none have panicked
    /// or completed).
    pub fn is_healthy(&self) -> bool {
        self.statuses
            .iter()
            .all(|s| s.load(Ordering::Acquire) == STAGE_RUNNING)
    }

    /// Number of stages in this pipeline.
    pub fn stage_count(&self) -> usize {
        self.statuses.len()
    }
}

// ---------------------------------------------------------------------------
// PipelineBuilder
// ---------------------------------------------------------------------------

/// Builder for constructing a [`Pipeline`].
///
/// # Example
///
/// ```
/// use photon_ring::topology::Pipeline;
///
/// let (mut pub_, stages) = Pipeline::builder()
///     .capacity(128)
///     .input::<u32>();
///
/// let (mut output, pipeline) = stages
///     .then(|x| x + 1)
///     .build();
///
/// pub_.publish(41);
/// assert_eq!(output.recv(), 42);
/// pipeline.shutdown();
/// pipeline.join();
/// ```
pub struct PipelineBuilder {
    capacity: usize,
}

impl Default for PipelineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PipelineBuilder {
    /// Create a new builder with default capacity (1024).
    pub fn new() -> Self {
        PipelineBuilder {
            capacity: DEFAULT_CAPACITY,
        }
    }

    /// Set the ring capacity for all inter-stage channels.
    ///
    /// Must be a power of two and at least 2.
    pub fn capacity(mut self, cap: usize) -> Self {
        self.capacity = cap;
        self
    }

    /// Declare the input type and create the first channel.
    ///
    /// Returns the publisher for writing into the pipeline and a
    /// [`StageBuilder`] for attaching processing stages.
    pub fn input<T: Copy + Send + 'static>(self) -> (Publisher<T>, StageBuilder<T>) {
        let (pub_, subs) = channel::channel::<T>(self.capacity);
        let subscriber = subs.subscribe();
        (
            pub_,
            StageBuilder {
                subscriber,
                subscribable: subs,
                capacity: self.capacity,
                state: SharedState::new(),
            },
        )
    }
}

// ---------------------------------------------------------------------------
// StageBuilder<T> — typed builder for chaining stages
// ---------------------------------------------------------------------------

/// Intermediate builder for adding processing stages to a pipeline.
///
/// The type parameter `T` is the output type of the most recently added
/// stage (or the input type, if no stages have been added yet).
pub struct StageBuilder<T: Copy + Send + 'static> {
    subscriber: Subscriber<T>,
    subscribable: Subscribable<T>,
    capacity: usize,
    state: SharedState,
}

impl<T: Copy + Send + 'static> StageBuilder<T> {
    /// Add a processing stage that transforms `T -> U`.
    ///
    /// Spawns a dedicated thread that reads from the current stage's
    /// output channel, applies `f`, and publishes to a new channel.
    ///
    /// # Example
    ///
    /// ```
    /// use photon_ring::topology::Pipeline;
    ///
    /// let (mut pub_, stages) = Pipeline::builder()
    ///     .capacity(64)
    ///     .input::<i32>();
    ///
    /// let (mut out, pipe) = stages
    ///     .then(|x| x * 2)
    ///     .then(|x| x + 1)
    ///     .build();
    ///
    /// pub_.publish(5);
    /// assert_eq!(out.recv(), 11);
    /// pipe.shutdown();
    /// pipe.join();
    /// ```
    pub fn then<U: Copy + Send + 'static>(
        mut self,
        f: impl Fn(T) -> U + Send + 'static,
    ) -> StageBuilder<U> {
        let (next_pub, next_subs) = channel::channel::<U>(self.capacity);
        let next_sub = next_subs.subscribe();

        let (status, handle) =
            spawn_stage(self.subscriber, next_pub, self.state.shutdown.clone(), f);
        self.state.handles.push(handle);
        self.state.statuses.push(status);

        StageBuilder {
            subscriber: next_sub,
            subscribable: next_subs,
            capacity: self.capacity,
            state: self.state,
        }
    }

    /// Fan out to two parallel processing stages.
    ///
    /// Both stages receive the same input (via two subscribers on the
    /// same ring). Each applies its own transformation and publishes
    /// to its own output channel.
    ///
    /// Returns a [`FanOutBuilder`] that can be finalized with `.build()`.
    ///
    /// # Example
    ///
    /// ```
    /// use photon_ring::topology::Pipeline;
    ///
    /// let (mut pub_, stages) = Pipeline::builder()
    ///     .capacity(64)
    ///     .input::<u64>();
    ///
    /// let ((mut out_a, mut out_b), pipe) = stages
    ///     .fan_out(|x: u64| x * 2, |x: u64| x + 100)
    ///     .build();
    ///
    /// pub_.publish(5);
    /// let a = out_a.recv();
    /// let b = out_b.recv();
    /// assert_eq!(a, 10);
    /// assert_eq!(b, 105);
    /// pipe.shutdown();
    /// pipe.join();
    /// ```
    pub fn fan_out<A, B>(
        mut self,
        fa: impl Fn(T) -> A + Send + 'static,
        fb: impl Fn(T) -> B + Send + 'static,
    ) -> FanOutBuilder<A, B>
    where
        A: Copy + Send + 'static,
        B: Copy + Send + 'static,
    {
        let (pub_a, subs_a) = channel::channel::<A>(self.capacity);
        let (pub_b, subs_b) = channel::channel::<B>(self.capacity);
        let sub_a_out = subs_a.subscribe();
        let sub_b_out = subs_b.subscribe();

        // Branch A uses the existing subscriber.
        let input_a = self.subscriber;
        // Branch B gets a fresh subscriber from the same source ring.
        let input_b = self.subscribable.subscribe();

        let (status_a, handle_a) = spawn_stage(input_a, pub_a, self.state.shutdown.clone(), fa);
        let (status_b, handle_b) = spawn_stage(input_b, pub_b, self.state.shutdown.clone(), fb);

        self.state.handles.push(handle_a);
        self.state.handles.push(handle_b);
        self.state.statuses.push(status_a);
        self.state.statuses.push(status_b);

        FanOutBuilder {
            sub_a: sub_a_out,
            subs_a,
            sub_b: sub_b_out,
            subs_b,
            capacity: self.capacity,
            state: self.state,
        }
    }

    /// Finalize the pipeline, returning the output subscriber and the
    /// [`Pipeline`] handle.
    ///
    /// The subscriber reads from the last stage's output channel (or
    /// directly from the input channel if no stages were added).
    /// The pipeline handle is used for shutdown and health monitoring.
    pub fn build(self) -> (Subscriber<T>, Pipeline) {
        (
            self.subscriber,
            Pipeline {
                handles: self.state.handles,
                shutdown: self.state.shutdown,
                statuses: self.state.statuses,
            },
        )
    }
}

// ---------------------------------------------------------------------------
// FanOutBuilder — two-branch fan-out terminator
// ---------------------------------------------------------------------------

/// Builder for a fan-out (diamond) topology with two output branches.
///
/// Created by [`StageBuilder::fan_out`]. Call [`.build()`](FanOutBuilder::build)
/// to finalize, or chain additional stages on each branch with
/// [`.then_a()`](FanOutBuilder::then_a) and
/// [`.then_b()`](FanOutBuilder::then_b).
pub struct FanOutBuilder<A: Copy + Send + 'static, B: Copy + Send + 'static> {
    sub_a: Subscriber<A>,
    subs_a: Subscribable<A>,
    sub_b: Subscriber<B>,
    subs_b: Subscribable<B>,
    capacity: usize,
    state: SharedState,
}

impl<A: Copy + Send + 'static, B: Copy + Send + 'static> FanOutBuilder<A, B> {
    /// Finalize the fan-out pipeline.
    ///
    /// Returns a tuple of `(branch_a_subscriber, branch_b_subscriber)` and
    /// the [`Pipeline`] handle.
    pub fn build(self) -> ((Subscriber<A>, Subscriber<B>), Pipeline) {
        (
            (self.sub_a, self.sub_b),
            Pipeline {
                handles: self.state.handles,
                shutdown: self.state.shutdown,
                statuses: self.state.statuses,
            },
        )
    }

    /// Add a processing stage after branch A.
    ///
    /// Transforms `A -> A2` on a dedicated thread. Branch B is unchanged.
    pub fn then_a<A2: Copy + Send + 'static>(
        mut self,
        f: impl Fn(A) -> A2 + Send + 'static,
    ) -> FanOutBuilder<A2, B> {
        let (next_pub, next_subs) = channel::channel::<A2>(self.capacity);
        let next_sub = next_subs.subscribe();

        let (status, handle) = spawn_stage(self.sub_a, next_pub, self.state.shutdown.clone(), f);
        self.state.handles.push(handle);
        self.state.statuses.push(status);

        FanOutBuilder {
            sub_a: next_sub,
            subs_a: next_subs,
            sub_b: self.sub_b,
            subs_b: self.subs_b,
            capacity: self.capacity,
            state: self.state,
        }
    }

    /// Add a processing stage after branch B.
    ///
    /// Transforms `B -> B2` on a dedicated thread. Branch A is unchanged.
    pub fn then_b<B2: Copy + Send + 'static>(
        mut self,
        f: impl Fn(B) -> B2 + Send + 'static,
    ) -> FanOutBuilder<A, B2> {
        let (next_pub, next_subs) = channel::channel::<B2>(self.capacity);
        let next_sub = next_subs.subscribe();

        let (status, handle) = spawn_stage(self.sub_b, next_pub, self.state.shutdown.clone(), f);
        self.state.handles.push(handle);
        self.state.statuses.push(status);

        FanOutBuilder {
            sub_a: self.sub_a,
            subs_a: self.subs_a,
            sub_b: next_sub,
            subs_b: next_subs,
            capacity: self.capacity,
            state: self.state,
        }
    }
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
        struct Input {
            value: f64,
        }

        #[derive(Clone, Copy, Debug, PartialEq)]
        struct Output {
            doubled: f64,
            positive: bool,
        }

        let (mut pub_, stages) = Pipeline::builder().capacity(64).input::<Input>();

        let (mut output, pipeline) = stages
            .then(|inp: Input| Output {
                doubled: inp.value * 2.0,
                positive: inp.value > 0.0,
            })
            .build();

        pub_.publish(Input { value: 3.5 });
        let out = output.recv();
        assert_eq!(out.doubled, 7.0);
        assert!(out.positive);

        pub_.publish(Input { value: -1.0 });
        let out = output.recv();
        assert_eq!(out.doubled, -2.0);
        assert!(!out.positive);

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

        // Spin-check until the stage reports panicked.
        let mut panicked = false;
        for _ in 0..1_000_000 {
            if !pipeline.panicked_stages().is_empty() {
                panicked = true;
                break;
            }
            core::hint::spin_loop();
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
