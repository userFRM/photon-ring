// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! Observability wrappers for photon-ring channels.
//!
//! Provides framework-agnostic metric snapshots from photon-ring's built-in
//! counters ([`total_received`], [`total_lagged`], [`receive_ratio`],
//! [`published`], [`pending`]).
//!
//! [`total_received`]: photon_ring::Subscriber::total_received
//! [`total_lagged`]: photon_ring::Subscriber::total_lagged
//! [`receive_ratio`]: photon_ring::Subscriber::receive_ratio
//! [`published`]: photon_ring::Publisher::published
//! [`pending`]: photon_ring::Subscriber::pending
//!
//! # Example
//!
//! ```
//! use photon_ring_metrics::SubscriberMetrics;
//!
//! let (mut pub_, subs) = photon_ring::channel::<u64>(1024);
//! let mut sub = subs.subscribe();
//! let metrics = SubscriberMetrics::new(&sub);
//!
//! pub_.publish(42);
//! sub.try_recv().unwrap();
//!
//! let snapshot = metrics.snapshot(&sub);
//! assert_eq!(snapshot.total_received, 1);
//! assert_eq!(snapshot.total_lagged, 0);
//! ```

use photon_ring::Pod;

// ---------------------------------------------------------------------------
// Subscriber snapshot
// ---------------------------------------------------------------------------

/// Point-in-time metrics snapshot from a subscriber.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubscriberSnapshot {
    /// Cumulative messages successfully received.
    pub total_received: u64,
    /// Cumulative messages lost due to lag (consumer fell behind the ring).
    pub total_lagged: u64,
    /// Ratio of received to total (received + lagged). 0.0 if no messages processed.
    pub receive_ratio: f64,
    /// Messages currently available to read (capped at ring capacity).
    pub pending: u64,
}

impl core::fmt::Display for SubscriberSnapshot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "recv={} lag={} ratio={:.2}% pending={}",
            self.total_received,
            self.total_lagged,
            self.receive_ratio * 100.0,
            self.pending,
        )
    }
}

// ---------------------------------------------------------------------------
// Publisher snapshot
// ---------------------------------------------------------------------------

/// Point-in-time metrics snapshot from a publisher.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PublisherSnapshot {
    /// Total messages published so far.
    pub published: u64,
    /// Ring capacity (power of two).
    pub capacity: u64,
}

impl core::fmt::Display for PublisherSnapshot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "published={} capacity={}", self.published, self.capacity,)
    }
}

// ---------------------------------------------------------------------------
// SubscriberMetrics
// ---------------------------------------------------------------------------

/// Metrics wrapper for a [`photon_ring::Subscriber`].
///
/// Tracks cumulative counters and supports both absolute snapshots and
/// delta snapshots (changes since the last `delta()` call).
pub struct SubscriberMetrics {
    prev_received: u64,
    prev_lagged: u64,
}

impl SubscriberMetrics {
    /// Create a new metrics wrapper, capturing the subscriber's current
    /// counters as the baseline for future delta computations.
    pub fn new<T: Pod>(sub: &photon_ring::Subscriber<T>) -> Self {
        Self {
            prev_received: sub.total_received(),
            prev_lagged: sub.total_lagged(),
        }
    }

    /// Take an absolute snapshot of current subscriber metrics.
    pub fn snapshot<T: Pod>(&self, sub: &photon_ring::Subscriber<T>) -> SubscriberSnapshot {
        SubscriberSnapshot {
            total_received: sub.total_received(),
            total_lagged: sub.total_lagged(),
            receive_ratio: sub.receive_ratio(),
            pending: sub.pending(),
        }
    }

    /// Take a delta snapshot: changes since the last `delta()` call
    /// (or since construction if `delta()` has not been called yet).
    ///
    /// `receive_ratio` and `pending` are absolute (not deltas) because
    /// ratios and instantaneous counts are not meaningful as differences.
    pub fn delta<T: Pod>(&mut self, sub: &photon_ring::Subscriber<T>) -> SubscriberSnapshot {
        let current = self.snapshot(sub);
        let delta = SubscriberSnapshot {
            total_received: current.total_received - self.prev_received,
            total_lagged: current.total_lagged - self.prev_lagged,
            receive_ratio: current.receive_ratio,
            pending: current.pending,
        };
        self.prev_received = current.total_received;
        self.prev_lagged = current.total_lagged;
        delta
    }
}

// ---------------------------------------------------------------------------
// PublisherMetrics
// ---------------------------------------------------------------------------

/// Metrics wrapper for a [`photon_ring::Publisher`].
///
/// Since publisher counters are monotonic and rarely need delta tracking,
/// this provides only a static `snapshot()` method.
pub struct PublisherMetrics;

impl PublisherMetrics {
    /// Take an absolute snapshot of current publisher metrics.
    pub fn snapshot<T: Pod>(pub_: &photon_ring::Publisher<T>) -> PublisherSnapshot {
        PublisherSnapshot {
            published: pub_.published(),
            capacity: pub_.capacity(),
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
    fn subscriber_snapshot_initial() {
        let (_, subs) = photon_ring::channel::<u64>(64);
        let sub = subs.subscribe();
        let metrics = SubscriberMetrics::new(&sub);
        let snap = metrics.snapshot(&sub);

        assert_eq!(snap.total_received, 0);
        assert_eq!(snap.total_lagged, 0);
        assert_eq!(snap.receive_ratio, 0.0);
        assert_eq!(snap.pending, 0);
    }

    #[test]
    fn subscriber_snapshot_after_recv() {
        let (mut pub_, subs) = photon_ring::channel::<u64>(64);
        let mut sub = subs.subscribe();
        let metrics = SubscriberMetrics::new(&sub);

        pub_.publish(1);
        pub_.publish(2);
        pub_.publish(3);
        assert_eq!(sub.try_recv(), Ok(1));
        assert_eq!(sub.try_recv(), Ok(2));

        let snap = metrics.snapshot(&sub);
        assert_eq!(snap.total_received, 2);
        assert_eq!(snap.total_lagged, 0);
        assert_eq!(snap.receive_ratio, 1.0);
        assert_eq!(snap.pending, 1); // message 3 still pending
    }

    #[test]
    fn subscriber_delta() {
        let (mut pub_, subs) = photon_ring::channel::<u64>(64);
        let mut sub = subs.subscribe();
        let mut metrics = SubscriberMetrics::new(&sub);

        // Phase 1: receive 2 messages
        pub_.publish(10);
        pub_.publish(20);
        assert_eq!(sub.try_recv(), Ok(10));
        assert_eq!(sub.try_recv(), Ok(20));

        let d1 = metrics.delta(&sub);
        assert_eq!(d1.total_received, 2);
        assert_eq!(d1.total_lagged, 0);

        // Phase 2: receive 1 more
        pub_.publish(30);
        assert_eq!(sub.try_recv(), Ok(30));

        let d2 = metrics.delta(&sub);
        assert_eq!(d2.total_received, 1);
        assert_eq!(d2.total_lagged, 0);
    }

    #[test]
    fn subscriber_delta_with_lag() {
        // Tiny ring: capacity 4. Publishing 6 messages before the subscriber
        // reads will cause lag.
        let (mut pub_, subs) = photon_ring::channel::<u64>(4);
        let mut sub = subs.subscribe();
        let mut metrics = SubscriberMetrics::new(&sub);

        for i in 0..6 {
            pub_.publish(i);
        }

        // First try_recv should report Lagged, second should succeed.
        let _ = sub.try_recv(); // Lagged — cursor advanced
        let _ = sub.try_recv(); // Ok

        let d = metrics.delta(&sub);
        // At least some lag should have been recorded.
        assert!(d.total_lagged > 0 || d.total_received > 0);
    }

    #[test]
    fn publisher_snapshot() {
        let (mut pub_, _subs) = photon_ring::channel::<u64>(128);

        let snap0 = PublisherMetrics::snapshot(&pub_);
        assert_eq!(snap0.published, 0);
        assert_eq!(snap0.capacity, 128);

        pub_.publish(1);
        pub_.publish(2);
        pub_.publish(3);

        let snap1 = PublisherMetrics::snapshot(&pub_);
        assert_eq!(snap1.published, 3);
        assert_eq!(snap1.capacity, 128);
    }

    #[test]
    fn subscriber_snapshot_display() {
        let snap = SubscriberSnapshot {
            total_received: 100,
            total_lagged: 5,
            receive_ratio: 0.9524,
            pending: 3,
        };
        let s = format!("{snap}");
        assert_eq!(s, "recv=100 lag=5 ratio=95.24% pending=3");
    }

    #[test]
    fn publisher_snapshot_display() {
        let snap = PublisherSnapshot {
            published: 42,
            capacity: 1024,
        };
        let s = format!("{snap}");
        assert_eq!(s, "published=42 capacity=1024");
    }
}
