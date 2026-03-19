// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use super::mp_publisher::MpPublisher;
use super::publisher::Publisher;
use super::subscribable::Subscribable;
use crate::pod::Pod;
use crate::ring::SharedRing;
use alloc::sync::Arc;

/// Create a Photon SPMC channel.
///
/// `capacity` must be >= 2. Any positive integer is accepted; power-of-two
/// capacities use a single-cycle AND for slot indexing, while arbitrary
/// capacities use Lemire fastmod (~1.5 ns overhead per indexing operation).
///
/// # Example
/// ```
/// let (mut pub_, subs) = photon_ring::channel::<u64>(64);
/// let mut sub = subs.subscribe();
/// pub_.publish(42);
/// assert_eq!(sub.try_recv(), Ok(42));
/// ```
pub fn channel<T: Pod>(capacity: usize) -> (Publisher<T>, Subscribable<T>) {
    let ring = Arc::new(SharedRing::new(capacity));
    let slots_ptr = ring.slots_ptr();
    let idx = ring.index;
    let cursor_ptr = ring.cursor_ptr();
    (
        Publisher {
            has_backpressure: ring.backpressure.is_some(),
            ring: ring.clone(),
            slots_ptr,
            capacity: idx.capacity,
            mask: idx.mask,
            reciprocal: idx.reciprocal,
            is_pow2: idx.is_pow2,
            cursor_ptr,
            seq: 0,
            cached_slowest: 0,
        },
        Subscribable { ring },
    )
}

/// Create a backpressure-capable SPMC channel.
///
/// The publisher will refuse to publish (returning [`PublishError::Full`])
/// when it would overwrite a slot that the slowest subscriber hasn't
/// read yet, minus `watermark` slots of headroom.
///
/// Unlike the default lossy [`channel()`], no messages are ever dropped.
///
/// # Arguments
/// - `capacity` -- ring size, must be >= 2.
/// - `watermark` -- headroom slots; must be less than `capacity`.
///   A watermark of 0 means the publisher blocks as soon as all slots are
///   occupied. A watermark of `capacity - 1` means it blocks when only one
///   slot is free.
///
/// # Example
/// ```
/// use photon_ring::channel_bounded;
/// use photon_ring::PublishError;
///
/// let (mut p, s) = channel_bounded::<u64>(4, 0);
/// let mut sub = s.subscribe();
///
/// // Fill the ring (4 slots).
/// for i in 0u64..4 {
///     p.try_publish(i).unwrap();
/// }
///
/// // Ring is full — backpressure kicks in.
/// assert_eq!(p.try_publish(99u64), Err(PublishError::Full(99)));
///
/// // Drain one slot — publisher can continue.
/// assert_eq!(sub.try_recv(), Ok(0));
/// p.try_publish(99).unwrap();
/// ```
pub fn channel_bounded<T: Pod>(
    capacity: usize,
    watermark: usize,
) -> (Publisher<T>, Subscribable<T>) {
    let ring = Arc::new(SharedRing::new_bounded(capacity, watermark));
    let slots_ptr = ring.slots_ptr();
    let idx = ring.index;
    let cursor_ptr = ring.cursor_ptr();
    (
        Publisher {
            has_backpressure: ring.backpressure.is_some(),
            ring: ring.clone(),
            slots_ptr,
            capacity: idx.capacity,
            mask: idx.mask,
            reciprocal: idx.reciprocal,
            is_pow2: idx.is_pow2,
            cursor_ptr,
            seq: 0,
            cached_slowest: 0,
        },
        Subscribable { ring },
    )
}

/// Create a Photon MPMC (multi-producer, multi-consumer) channel.
///
/// `capacity` must be >= 2. Returns a clone-able [`MpPublisher`] and the
/// same [`Subscribable`] factory used by SPMC channels.
///
/// Multiple threads can clone the publisher and publish concurrently.
/// Subscribers work identically to the SPMC case.
///
/// # Example
/// ```
/// let (pub_, subs) = photon_ring::channel_mpmc::<u64>(64);
/// let mut sub = subs.subscribe();
///
/// let pub2 = pub_.clone();
/// pub_.publish(1);
/// pub2.publish(2);
///
/// assert_eq!(sub.try_recv(), Ok(1));
/// assert_eq!(sub.try_recv(), Ok(2));
/// ```
pub fn channel_mpmc<T: Pod>(capacity: usize) -> (MpPublisher<T>, Subscribable<T>) {
    use core::sync::atomic::AtomicU64;
    let ring = Arc::new(SharedRing::new_mpmc(capacity));
    let slots_ptr = ring.slots_ptr();
    let idx = ring.index;
    let cursor_ptr = ring.cursor_ptr();
    let next_seq_ptr = &ring
        .next_seq
        .as_ref()
        .expect("MPMC ring must have next_seq")
        .0 as *const AtomicU64;
    (
        MpPublisher {
            ring: ring.clone(),
            slots_ptr,
            capacity: idx.capacity,
            mask: idx.mask,
            reciprocal: idx.reciprocal,
            is_pow2: idx.is_pow2,
            cursor_ptr,
            next_seq_ptr,
        },
        Subscribable { ring },
    )
}
