// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

/// Error from [`Subscriber::try_recv`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TryRecvError {
    /// No new messages available.
    Empty,
    /// Consumer fell behind the ring. `skipped` messages were lost.
    Lagged { skipped: u64 },
}

/// Error returned by [`Publisher::try_publish`] when the ring is full
/// and backpressure is enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError<T> {
    /// The slowest consumer is within the backpressure watermark.
    /// Contains the value that was not published.
    Full(T),
}
