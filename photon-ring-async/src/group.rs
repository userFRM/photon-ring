// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::DEFAULT_SPIN_BUDGET;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use photon_ring::{Pod, SubscriberGroup, TryRecvError};

/// Async wrapper around [`photon_ring::SubscriberGroup`].
///
/// Same yield-based polling strategy as [`AsyncSubscriber`](crate::AsyncSubscriber),
/// but wraps a [`SubscriberGroup<T, N>`] — `N` logical subscribers sharing
/// a single ring read and cursor.
pub struct AsyncSubscriberGroup<T: Pod, const N: usize> {
    inner: SubscriberGroup<T, N>,
    spin_budget: u32,
}

impl<T: Pod, const N: usize> AsyncSubscriberGroup<T, N> {
    /// Wrap a synchronous subscriber group with the default spin budget (64).
    pub fn new(group: SubscriberGroup<T, N>) -> Self {
        Self {
            inner: group,
            spin_budget: DEFAULT_SPIN_BUDGET,
        }
    }

    /// Wrap a synchronous subscriber group with a custom spin budget.
    pub fn with_spin_budget(group: SubscriberGroup<T, N>, budget: u32) -> Self {
        Self {
            inner: group,
            spin_budget: budget,
        }
    }

    /// Receive the next message asynchronously.
    ///
    /// Tries `spin_budget` synchronous `try_recv()` calls. If a message
    /// arrives, returns immediately. Otherwise, yields to the async runtime
    /// and tries again on the next poll.
    pub async fn recv(&mut self) -> T {
        core::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Poll-based receive for manual `Future` composition.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<T> {
        for _ in 0..self.spin_budget {
            match self.inner.try_recv() {
                Ok(value) => return Poll::Ready(value),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Lagged { .. }) => {
                    // Cursor was advanced — retry from oldest available.
                }
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    /// Receive a batch of messages asynchronously.
    ///
    /// Fills the provided buffer with up to `buf.len()` messages. Returns
    /// the number of messages written. If no messages are currently
    /// available, yields to the async runtime and retries.
    pub async fn recv_batch(&mut self, buf: &mut [T]) -> usize {
        core::future::poll_fn(|cx| self.poll_recv_batch(buf, cx)).await
    }

    fn poll_recv_batch(&mut self, buf: &mut [T], cx: &mut Context<'_>) -> Poll<usize> {
        let count = self.inner.recv_batch(buf);
        if count > 0 {
            Poll::Ready(count)
        } else {
            for _ in 0..self.spin_budget {
                match self.inner.try_recv() {
                    Ok(value) => {
                        buf[0] = value;
                        let rest = self.inner.recv_batch(&mut buf[1..]);
                        return Poll::Ready(1 + rest);
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Lagged { .. }) => {}
                }
            }
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    /// How many messages are available to read (capped at ring capacity).
    #[inline]
    pub fn pending(&self) -> u64 {
        self.inner.pending()
    }

    /// Total messages successfully received by this group.
    #[inline]
    pub fn total_received(&self) -> u64 {
        self.inner.total_received()
    }

    /// Total messages lost due to lag.
    #[inline]
    pub fn total_lagged(&self) -> u64 {
        self.inner.total_lagged()
    }

    /// Ratio of received to total (received + lagged).
    #[inline]
    pub fn receive_ratio(&self) -> f64 {
        self.inner.receive_ratio()
    }

    /// Get the current spin budget.
    #[inline]
    pub fn spin_budget(&self) -> u32 {
        self.spin_budget
    }

    /// Set the spin budget.
    #[inline]
    pub fn set_spin_budget(&mut self, budget: u32) {
        self.spin_budget = budget;
    }

    /// Get a reference to the inner synchronous subscriber group.
    #[inline]
    pub fn inner(&self) -> &SubscriberGroup<T, N> {
        &self.inner
    }

    /// Get a mutable reference to the inner synchronous subscriber group.
    #[inline]
    pub fn inner_mut(&mut self) -> &mut SubscriberGroup<T, N> {
        &mut self.inner
    }

    /// Unwrap into the inner synchronous subscriber group.
    #[inline]
    pub fn into_inner(self) -> SubscriberGroup<T, N> {
        self.inner
    }
}

/// A `Future` that resolves to the next message from an
/// [`AsyncSubscriberGroup`].
pub struct GroupRecvFuture<'a, T: Pod, const N: usize> {
    group: &'a mut AsyncSubscriberGroup<T, N>,
}

impl<'a, T: Pod, const N: usize> GroupRecvFuture<'a, T, N> {
    /// Create a new receive future from a mutable reference to an async
    /// subscriber group.
    pub fn new(group: &'a mut AsyncSubscriberGroup<T, N>) -> Self {
        Self { group }
    }
}

impl<'a, T: Pod, const N: usize> Future for GroupRecvFuture<'a, T, N> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        self.group.poll_recv(cx)
    }
}

// SAFETY: AsyncSubscriberGroup<T, N> is Send because SubscriberGroup<T, N>
// is Send and spin_budget is a plain u32.
unsafe impl<T: Pod, const N: usize> Send for AsyncSubscriberGroup<T, N> {}
