// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::errors::TryRecvError;
use super::subscriber::Subscriber;
use crate::pod::Pod;
use crate::wait::WaitStrategy;

/// A group of `N` logical subscribers backed by a single ring read.
///
/// All `N` logical subscribers share one cursor —
/// [`try_recv`](SubscriberGroup::try_recv) performs **one** seqlock read
/// and a single cursor increment, eliminating the N-element sweep loop.
/// That makes the group behaviourally a single [`Subscriber`] carrying a
/// compile-time count, which is exactly how it is implemented; `N` is
/// reported by [`aligned_count`](SubscriberGroup::aligned_count).
///
/// ```
/// let (mut p, subs) = photon_ring::channel::<u64>(64);
/// let mut group = subs.subscribe_group::<4>();
/// p.publish(42);
/// assert_eq!(group.try_recv(), Ok(42));
/// ```
pub struct SubscriberGroup<T: Pod, const N: usize>(pub(super) Subscriber<T>);

impl<T: Pod, const N: usize> SubscriberGroup<T, N> {
    /// Try to receive the next message for the group.
    ///
    /// Performs a single seqlock read and one cursor increment — no
    /// N-element sweep needed since all logical subscribers share one cursor.
    #[inline]
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.0.try_recv()
    }

    /// Spin until the next message is available.
    ///
    /// Uses the same two-phase spin as [`Subscriber::recv`]: bare spin for
    /// the first 64 iterations, then a power-efficient wait (`PAUSE` on x86,
    /// `SEVL`/`WFE` on aarch64).
    #[inline]
    pub fn recv(&mut self) -> T {
        self.0.recv()
    }

    /// Block until the next message using the given [`WaitStrategy`].
    ///
    /// # Example
    /// ```
    /// use photon_ring::{channel, WaitStrategy};
    ///
    /// let (mut p, s) = channel::<u64>(64);
    /// let mut group = s.subscribe_group::<2>();
    /// p.publish(42);
    /// assert_eq!(group.recv_with(WaitStrategy::BusySpin), 42);
    /// ```
    #[inline]
    pub fn recv_with(&mut self, strategy: WaitStrategy) -> T {
        self.0.recv_with(strategy)
    }

    /// How many of the `N` logical subscribers are aligned.
    ///
    /// With the single-cursor design all subscribers are always aligned,
    /// so this trivially returns `N`.
    #[inline]
    pub fn aligned_count(&self) -> usize {
        N
    }

    /// Number of messages available to read (capped at ring capacity).
    #[inline]
    pub fn pending(&self) -> u64 {
        self.0.pending()
    }

    /// Total messages successfully received by this group.
    #[inline]
    pub fn total_received(&self) -> u64 {
        self.0.total_received()
    }

    /// Total messages lost due to lag (group fell behind the ring).
    #[inline]
    pub fn total_lagged(&self) -> u64 {
        self.0.total_lagged()
    }

    /// Ratio of received to total (received + lagged). Returns 0.0 if no
    /// messages have been processed.
    #[inline]
    pub fn receive_ratio(&self) -> f64 {
        self.0.receive_ratio()
    }

    /// Receive up to `buf.len()` messages in a single call.
    ///
    /// Messages are written into the provided slice starting at index 0.
    /// Returns the number of messages received. On lag, the cursor is
    /// advanced and filling continues from the oldest available message.
    #[inline]
    pub fn recv_batch(&mut self, buf: &mut [T]) -> usize {
        self.0.recv_batch(buf)
    }
}
