// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::channel::{self, Publisher, Subscribable, Subscriber};
use crate::pod::Pod;

use super::fan_out::FanOutBuilder;
use super::pipeline::Pipeline;
use super::{spawn_stage, SharedState, DEFAULT_CAPACITY};

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
    pub fn input<T: Pod>(self) -> (Publisher<T>, StageBuilder<T>) {
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
pub struct StageBuilder<T: Pod> {
    pub(super) subscriber: Subscriber<T>,
    pub(super) subscribable: Subscribable<T>,
    pub(super) capacity: usize,
    pub(super) state: SharedState,
}

impl<T: Pod> StageBuilder<T> {
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
    pub fn then<U: Pod>(mut self, f: impl Fn(T) -> U + Send + 'static) -> StageBuilder<U> {
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
        A: Pod,
        B: Pod,
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
