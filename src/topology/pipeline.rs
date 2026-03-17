// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

extern crate std;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::JoinHandle;

use super::{STAGE_PANICKED, STAGE_RUNNING};

/// A multi-stage processing pipeline.
///
/// Each stage reads from one Photon Ring channel, applies a transformation,
/// and publishes to the next channel. Stages run on dedicated threads.
///
/// Created via [`Pipeline::builder`] -> [`PipelineBuilder::input`] ->
/// [`StageBuilder::then`] / [`StageBuilder::fan_out`] -> [`StageBuilder::build`].
pub struct Pipeline {
    pub(super) handles: Vec<JoinHandle<()>>,
    pub(super) shutdown: Arc<AtomicBool>,
    pub(super) statuses: Vec<Arc<AtomicU8>>,
}

impl Pipeline {
    /// Create a new pipeline builder.
    pub fn builder() -> super::builder::PipelineBuilder {
        super::builder::PipelineBuilder::new()
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
