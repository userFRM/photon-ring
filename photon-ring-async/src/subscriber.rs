// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::DEFAULT_SPIN_BUDGET;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use photon_ring::{Pod, Subscriber, TryRecvError};

/// Async wrapper around [`photon_ring::Subscriber`].
///
/// Uses yield-based polling: tries up to `spin_budget` synchronous
/// [`try_recv()`](Subscriber::try_recv) calls per poll, then yields to the
/// async runtime by re-registering the waker and returning `Pending`.
///
/// # Spin budget
///
/// The `spin_budget` controls the trade-off between latency and CPU
/// sharing:
///
/// - **High budget** (e.g. 1024): lower latency (more chances to catch a
///   message before yielding), but holds the executor thread longer per poll.
/// - **Low budget** (e.g. 1): yields quickly, giving other tasks more
///   scheduling time, but each empty poll adds one executor round-trip
///   (~200-500 ns).
/// - **Default** (64): matches the bare-spin phase of
///   [`Subscriber::recv()`](Subscriber::recv).
pub struct AsyncSubscriber<T: Pod> {
    inner: Subscriber<T>,
    spin_budget: u32,
}

impl<T: Pod> AsyncSubscriber<T> {
    /// Wrap a synchronous subscriber with the default spin budget (64).
    pub fn new(sub: Subscriber<T>) -> Self {
        Self {
            inner: sub,
            spin_budget: DEFAULT_SPIN_BUDGET,
        }
    }

    /// Wrap a synchronous subscriber with a custom spin budget.
    pub fn with_spin_budget(sub: Subscriber<T>, budget: u32) -> Self {
        Self {
            inner: sub,
            spin_budget: budget,
        }
    }

    /// Receive the next message asynchronously.
    ///
    /// Tries `spin_budget` synchronous `try_recv()` calls. If a message
    /// arrives, returns immediately. Otherwise, yields to the async runtime
    /// and tries again on the next poll.
    ///
    /// Lag is handled transparently: when the subscriber falls behind the
    /// ring, the cursor is advanced and polling continues from the oldest
    /// available message.
    pub async fn recv(&mut self) -> T {
        core::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Poll-based receive for manual `Future` composition.
    ///
    /// Returns `Poll::Ready(value)` if a message was received within the
    /// spin budget, or `Poll::Pending` after exhausting the budget (with
    /// the waker re-registered for immediate re-scheduling).
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<T> {
        for _ in 0..self.spin_budget {
            match self.inner.try_recv() {
                Ok(value) => return Poll::Ready(value),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Lagged { .. }) => {
                    // Cursor was advanced by try_recv — retry immediately
                    // from the oldest available slot.
                }
            }
        }
        // Budget exhausted. Re-register the waker so the executor polls
        // us again. This is cooperative yielding, not event-driven wakeup.
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    /// Receive a batch of messages asynchronously.
    ///
    /// Fills the provided buffer with up to `buf.len()` messages. Returns
    /// the number of messages written. If no messages are currently
    /// available, yields to the async runtime and retries.
    ///
    /// This is more efficient than calling `recv()` in a loop when you can
    /// process messages in bulk, because it drains everything available
    /// before yielding.
    pub async fn recv_batch(&mut self, buf: &mut [T]) -> usize {
        core::future::poll_fn(|cx| self.poll_recv_batch(buf, cx)).await
    }

    fn poll_recv_batch(&mut self, buf: &mut [T], cx: &mut Context<'_>) -> Poll<usize> {
        let count = self.inner.recv_batch(buf);
        if count > 0 {
            Poll::Ready(count)
        } else {
            // Nothing available — spin a few times before yielding
            for _ in 0..self.spin_budget {
                match self.inner.try_recv() {
                    Ok(value) => {
                        buf[0] = value;
                        // Got one — now drain the rest without spinning
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

    /// Total messages successfully received by this subscriber.
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

    /// Get a reference to the inner synchronous subscriber.
    #[inline]
    pub fn inner(&self) -> &Subscriber<T> {
        &self.inner
    }

    /// Get a mutable reference to the inner synchronous subscriber.
    #[inline]
    pub fn inner_mut(&mut self) -> &mut Subscriber<T> {
        &mut self.inner
    }

    /// Unwrap into the inner synchronous subscriber.
    #[inline]
    pub fn into_inner(self) -> Subscriber<T> {
        self.inner
    }
}

// ---------------------------------------------------------------------------
// RecvFuture — a named Future for use in select!/join! combinators
// ---------------------------------------------------------------------------

/// A `Future` that resolves to the next message from an [`AsyncSubscriber`].
///
/// Created by holding a mutable reference to the subscriber. Useful when
/// you need a named future for `select!` or `join!` combinators.
///
/// ```rust,no_run
/// # async fn example() {
/// let (mut p, subs) = photon_ring::channel::<u64>(64);
/// let mut sub = photon_ring_async::AsyncSubscriber::new(subs.subscribe());
/// p.publish(1);
/// let fut = photon_ring_async::RecvFuture::new(&mut sub);
/// let value = fut.await;
/// assert_eq!(value, 1);
/// # }
/// ```
pub struct RecvFuture<'a, T: Pod> {
    sub: &'a mut AsyncSubscriber<T>,
}

impl<'a, T: Pod> RecvFuture<'a, T> {
    /// Create a new receive future from a mutable reference to an async
    /// subscriber.
    pub fn new(sub: &'a mut AsyncSubscriber<T>) -> Self {
        Self { sub }
    }
}

impl<'a, T: Pod> Future for RecvFuture<'a, T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        self.sub.poll_recv(cx)
    }
}

// SAFETY: AsyncSubscriber<T> is Send because Subscriber<T> is Send and
// spin_budget is a plain u32.
unsafe impl<T: Pod> Send for AsyncSubscriber<T> {}
