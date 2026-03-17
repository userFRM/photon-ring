// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::channel::{self, Subscribable, Subscriber};
use crate::pod::Pod;

use super::pipeline::Pipeline;
use super::{spawn_stage, SharedState};

/// Builder for a fan-out (diamond) topology with two output branches.
///
/// Created by [`StageBuilder::fan_out`]. Call [`.build()`](FanOutBuilder::build)
/// to finalize, or chain additional stages on each branch with
/// [`.then_a()`](FanOutBuilder::then_a) and
/// [`.then_b()`](FanOutBuilder::then_b).
pub struct FanOutBuilder<A: Pod, B: Pod> {
    pub(super) sub_a: Subscriber<A>,
    pub(super) subs_a: Subscribable<A>,
    pub(super) sub_b: Subscriber<B>,
    pub(super) subs_b: Subscribable<B>,
    pub(super) capacity: usize,
    pub(super) state: SharedState,
}

impl<A: Pod, B: Pod> FanOutBuilder<A, B> {
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
    pub fn then_a<A2: Pod>(mut self, f: impl Fn(A) -> A2 + Send + 'static) -> FanOutBuilder<A2, B> {
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
    pub fn then_b<B2: Pod>(mut self, f: impl Fn(B) -> B2 + Send + 'static) -> FanOutBuilder<A, B2> {
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
