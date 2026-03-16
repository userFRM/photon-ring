// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use photon_ring::{channel, channel_bounded, channel_mpmc, Photon, PublishError, TryRecvError};

// -------------------------------------------------------------------------
// Basic publish / receive
// -------------------------------------------------------------------------

#[test]
fn single_message() {
    let (mut p, s) = channel::<u64>(4);
    let mut sub = s.subscribe();
    p.publish(42);
    assert_eq!(sub.try_recv(), Ok(42));
}

#[test]
fn empty_recv() {
    let (_p, s) = channel::<u64>(4);
    let mut sub = s.subscribe();
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn sequential_messages() {
    let (mut p, s) = channel::<u64>(8);
    let mut sub = s.subscribe();
    for i in 0..8 {
        p.publish(i);
    }
    for i in 0..8 {
        assert_eq!(sub.try_recv(), Ok(i));
    }
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn interleaved_publish_recv() {
    let (mut p, s) = channel::<u64>(4);
    let mut sub = s.subscribe();

    p.publish(1);
    assert_eq!(sub.try_recv(), Ok(1));

    p.publish(2);
    p.publish(3);
    assert_eq!(sub.try_recv(), Ok(2));
    assert_eq!(sub.try_recv(), Ok(3));
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));
}

// -------------------------------------------------------------------------
// Multi-subscriber fanout
// -------------------------------------------------------------------------

#[test]
fn two_subscribers() {
    let (mut p, s) = channel::<u64>(8);
    let mut s1 = s.subscribe();
    let mut s2 = s.subscribe();

    p.publish(10);
    p.publish(20);

    assert_eq!(s1.try_recv(), Ok(10));
    assert_eq!(s1.try_recv(), Ok(20));
    assert_eq!(s2.try_recv(), Ok(10));
    assert_eq!(s2.try_recv(), Ok(20));
}

#[test]
fn five_subscribers_independent_cursors() {
    let (mut p, s) = channel::<u32>(16);
    let mut subs: Vec<_> = (0..5).map(|_| s.subscribe()).collect();

    for i in 0..10 {
        p.publish(i);
    }

    // Read different amounts from each
    for (idx, sub) in subs.iter_mut().enumerate() {
        for i in 0..(idx + 1) as u32 {
            assert_eq!(sub.try_recv(), Ok(i));
        }
    }
}

// -------------------------------------------------------------------------
// Ring overflow / lag detection
// -------------------------------------------------------------------------

#[test]
fn lag_detection_fast_path() {
    let (mut p, s) = channel::<u64>(4);
    let mut sub = s.subscribe();

    // Fill ring (4 slots) and overflow by 2
    for i in 0..6 {
        p.publish(i);
    }

    // Consumer should detect lag — messages 0, 1 are gone
    let err = sub.try_recv().unwrap_err();
    assert_eq!(err, TryRecvError::Lagged { skipped: 2 });

    // After lag, cursor advanced — should read oldest available (2)
    assert_eq!(sub.try_recv(), Ok(2));
    assert_eq!(sub.try_recv(), Ok(3));
    assert_eq!(sub.try_recv(), Ok(4));
    assert_eq!(sub.try_recv(), Ok(5));
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn heavy_overflow() {
    let (mut p, s) = channel::<u64>(4);
    let mut sub = s.subscribe();

    // Publish 100 messages into a 4-slot ring
    for i in 0..100 {
        p.publish(i);
    }

    // Consumer should detect massive lag
    let err = sub.try_recv().unwrap_err();
    match err {
        TryRecvError::Lagged { skipped } => assert_eq!(skipped, 96),
        other => panic!("expected Lagged, got {other:?}"),
    }

    // Should read the last 4 messages
    assert_eq!(sub.try_recv(), Ok(96));
    assert_eq!(sub.try_recv(), Ok(97));
    assert_eq!(sub.try_recv(), Ok(98));
    assert_eq!(sub.try_recv(), Ok(99));
}

// -------------------------------------------------------------------------
// Latest / pending
// -------------------------------------------------------------------------

#[test]
fn latest_skips_to_newest() {
    let (mut p, s) = channel::<u64>(8);
    let mut sub = s.subscribe();

    for i in 0..5 {
        p.publish(i);
    }

    // Skip to latest
    assert_eq!(sub.latest(), Some(4));
    // After latest, cursor advanced past it
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn latest_returns_none_when_empty() {
    let (_p, s) = channel::<u64>(4);
    let mut sub = s.subscribe();
    assert_eq!(sub.latest(), None);
}

#[test]
fn pending_count() {
    let (mut p, s) = channel::<u64>(8);
    let mut sub = s.subscribe();

    assert_eq!(sub.pending(), 0);

    p.publish(1);
    assert_eq!(sub.pending(), 1);

    p.publish(2);
    p.publish(3);
    assert_eq!(sub.pending(), 3);

    sub.try_recv().unwrap();
    assert_eq!(sub.pending(), 2);
}

// -------------------------------------------------------------------------
// Batch publish
// -------------------------------------------------------------------------

#[test]
fn batch_publish() {
    let (mut p, s) = channel::<u64>(8);
    let mut sub = s.subscribe();

    let batch: Vec<u64> = (10..15).collect();
    p.publish_batch(&batch);

    for i in 10..15 {
        assert_eq!(sub.try_recv(), Ok(i));
    }
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn batch_publish_empty() {
    let (mut p, _s) = channel::<u64>(4);
    p.publish_batch(&[]);
    assert_eq!(p.published(), 0);
}

// -------------------------------------------------------------------------
// Subscribe timing (future only vs from_oldest)
// -------------------------------------------------------------------------

#[test]
fn subscribe_sees_only_future() {
    let (mut p, s) = channel::<u64>(8);
    p.publish(1);
    p.publish(2);

    let mut sub = s.subscribe(); // subscribed AFTER 1, 2
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));

    p.publish(3);
    assert_eq!(sub.try_recv(), Ok(3));
}

#[test]
fn subscribe_from_oldest() {
    let (mut p, s) = channel::<u64>(8);
    p.publish(1);
    p.publish(2);
    p.publish(3);

    let mut sub = s.subscribe_from_oldest();
    assert_eq!(sub.try_recv(), Ok(1));
    assert_eq!(sub.try_recv(), Ok(2));
    assert_eq!(sub.try_recv(), Ok(3));
}

#[test]
fn subscribe_from_oldest_after_overflow() {
    let (mut p, s) = channel::<u64>(4);
    for i in 0..10 {
        p.publish(i);
    }

    let mut sub = s.subscribe_from_oldest();
    // Oldest in ring: 10 - 4 = 6
    assert_eq!(sub.try_recv(), Ok(6));
    assert_eq!(sub.try_recv(), Ok(7));
    assert_eq!(sub.try_recv(), Ok(8));
    assert_eq!(sub.try_recv(), Ok(9));
}

// -------------------------------------------------------------------------
// Struct payload (verifies Copy + cache-line semantics)
// -------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
struct Quote {
    price: f64,
    volume: u64,
    ts: u64,
}

#[test]
fn struct_payload() {
    let (mut p, s) = channel::<Quote>(8);
    let mut sub = s.subscribe();

    let q = Quote {
        price: 123.45,
        volume: 1000,
        ts: 999,
    };
    p.publish(q);

    assert_eq!(sub.try_recv(), Ok(q));
}

// -------------------------------------------------------------------------
// Concurrent publish + subscribe
// -------------------------------------------------------------------------

#[test]
fn cross_thread_spmc() {
    let (mut p, s) = channel::<u64>(1024);
    let mut s1 = s.subscribe();
    let mut s2 = s.subscribe();

    let n = 100_000u64;

    let writer = std::thread::spawn(move || {
        for i in 0..n {
            p.publish(i);
        }
    });

    let reader1 = std::thread::spawn(move || {
        let mut last = None;
        let mut count = 0u64;
        loop {
            match s1.try_recv() {
                Ok(v) => {
                    if let Some(prev) = last {
                        assert!(v > prev, "out of order: {prev} -> {v}");
                    }
                    last = Some(v);
                    count += 1;
                    if v == n - 1 {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => core::hint::spin_loop(),
                Err(TryRecvError::Lagged { .. }) => {}
            }
        }
        count
    });

    let reader2 = std::thread::spawn(move || {
        let mut last = None;
        let mut count = 0u64;
        loop {
            match s2.try_recv() {
                Ok(v) => {
                    if let Some(prev) = last {
                        assert!(v > prev, "out of order: {prev} -> {v}");
                    }
                    last = Some(v);
                    count += 1;
                    if v == n - 1 {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => core::hint::spin_loop(),
                Err(TryRecvError::Lagged { .. }) => {}
            }
        }
        count
    });

    writer.join().unwrap();
    let c1 = reader1.join().unwrap();
    let c2 = reader2.join().unwrap();

    // Both readers must see at least some messages (may lag)
    assert!(c1 > 0);
    assert!(c2 > 0);
}

#[test]
fn blocking_recv_cross_thread() {
    let (mut p, s) = channel::<u64>(64);
    let mut sub = s.subscribe();

    let writer = std::thread::spawn(move || {
        for i in 0..10 {
            std::thread::sleep(std::time::Duration::from_micros(100));
            p.publish(i);
        }
    });

    for i in 0..10 {
        let v = sub.recv();
        assert_eq!(v, i);
    }

    writer.join().unwrap();
}

// -------------------------------------------------------------------------
// Stress: 1M messages, verify no corruption
// -------------------------------------------------------------------------

#[test]
fn stress_1m_messages() {
    let (mut p, s) = channel::<u64>(4096);
    let mut sub = s.subscribe();
    let n = 1_000_000u64;

    let writer = std::thread::spawn(move || {
        for i in 0..n {
            p.publish(i);
        }
    });

    let reader = std::thread::spawn(move || {
        let mut expected = 0u64;
        let mut lag_total = 0u64;
        loop {
            match sub.try_recv() {
                Ok(v) => {
                    assert_eq!(v, expected, "corruption: expected {expected}, got {v}");
                    expected += 1;
                    if expected == n {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => core::hint::spin_loop(),
                Err(TryRecvError::Lagged { skipped }) => {
                    lag_total += skipped;
                    expected += skipped;
                }
            }
        }
        lag_total
    });

    writer.join().unwrap();
    let lags = reader.join().unwrap();
    // With ring 4096, a single reader should keep up (lag = 0 on most machines)
    eprintln!("stress_1m: lags = {lags}");
}

// -------------------------------------------------------------------------
// Bus (Photon<T>) tests
// -------------------------------------------------------------------------

#[test]
fn bus_basic() {
    let bus = Photon::<u64>::new(64);
    let mut p = bus.publisher("quotes");
    let mut sub = bus.subscribe("quotes");

    p.publish(100);
    assert_eq!(sub.try_recv(), Ok(100));
}

#[test]
fn bus_multi_topic() {
    let bus = Photon::<u64>::new(64);
    let mut p1 = bus.publisher("A");
    let mut p2 = bus.publisher("B");
    let mut s1 = bus.subscribe("A");
    let mut s2 = bus.subscribe("B");

    p1.publish(1);
    p2.publish(2);

    assert_eq!(s1.try_recv(), Ok(1));
    assert_eq!(s2.try_recv(), Ok(2));

    // Cross-topic isolation
    assert_eq!(s1.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(s2.try_recv(), Err(TryRecvError::Empty));
}

#[test]
#[should_panic(expected = "publisher already taken")]
fn bus_double_publisher_panics() {
    let bus = Photon::<u64>::new(64);
    let _p1 = bus.publisher("X");
    let _p2 = bus.publisher("X"); // should panic
}

#[test]
fn bus_subscribe_before_publisher() {
    let bus = Photon::<u64>::new(64);
    let mut sub = bus.subscribe("late");
    let mut p = bus.publisher("late");

    p.publish(99);
    assert_eq!(sub.try_recv(), Ok(99));
}

// -------------------------------------------------------------------------
// Publisher metadata
// -------------------------------------------------------------------------

#[test]
fn published_count() {
    let (mut p, _s) = channel::<u64>(8);
    assert_eq!(p.published(), 0);
    p.publish(1);
    assert_eq!(p.published(), 1);
    p.publish_batch(&[2, 3, 4]);
    assert_eq!(p.published(), 4);
}

#[test]
fn capacity_query() {
    let (p, _s) = channel::<u64>(128);
    assert_eq!(p.capacity(), 128);
}

// -------------------------------------------------------------------------
// Bounded channel (backpressure)
// -------------------------------------------------------------------------

#[test]
fn bounded_basic_publish_recv() {
    let (mut p, s) = channel_bounded::<u64>(8, 0);
    let mut sub = s.subscribe();

    // Publish and receive a few messages.
    for i in 0..5 {
        p.try_publish(i).unwrap();
    }
    for i in 0..5 {
        assert_eq!(sub.try_recv(), Ok(i));
    }
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn bounded_try_publish_returns_full() {
    let (mut p, s) = channel_bounded::<u64>(4, 0);
    let mut sub = s.subscribe();

    // Fill all 4 slots.
    for i in 0..4 {
        p.try_publish(i).unwrap();
    }

    // Ring is full — backpressure should kick in.
    assert_eq!(p.try_publish(99), Err(PublishError::Full(99)));

    // Value was not consumed — verify the ring still holds 0..4.
    for i in 0..4 {
        assert_eq!(sub.try_recv(), Ok(i));
    }
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn bounded_backpressure_releases_when_consumer_catches_up() {
    let (mut p, s) = channel_bounded::<u64>(4, 0);
    let mut sub = s.subscribe();

    // Fill ring.
    for i in 0..4 {
        p.try_publish(i).unwrap();
    }
    assert_eq!(p.try_publish(100), Err(PublishError::Full(100)));

    // Drain one slot — frees capacity for one more.
    assert_eq!(sub.try_recv(), Ok(0));

    // Now publisher can write again.
    p.try_publish(100).unwrap();

    // But not two in a row.
    assert_eq!(p.try_publish(200), Err(PublishError::Full(200)));

    // Drain all remaining.
    assert_eq!(sub.try_recv(), Ok(1));
    assert_eq!(sub.try_recv(), Ok(2));
    assert_eq!(sub.try_recv(), Ok(3));
    assert_eq!(sub.try_recv(), Ok(100));
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn bounded_watermark_provides_headroom() {
    // capacity=8, watermark=2 means effective capacity = 6.
    let (mut p, s) = channel_bounded::<u64>(8, 2);
    let mut _sub = s.subscribe();

    // Should be able to publish exactly 6 messages.
    for i in 0..6 {
        p.try_publish(i).unwrap();
    }
    // 7th should fail (watermark reserves 2 slots).
    assert_eq!(p.try_publish(99), Err(PublishError::Full(99)));
}

#[test]
fn bounded_multiple_subscribers_slowest_controls_backpressure() {
    let (mut p, s) = channel_bounded::<u64>(4, 0);
    let mut fast = s.subscribe();
    let mut slow = s.subscribe();

    // Fill ring.
    for i in 0..4 {
        p.try_publish(i).unwrap();
    }
    assert_eq!(p.try_publish(99), Err(PublishError::Full(99)));

    // Fast reader drains everything.
    for i in 0..4 {
        assert_eq!(fast.try_recv(), Ok(i));
    }

    // Ring is still full from slow reader's perspective.
    assert_eq!(p.try_publish(99), Err(PublishError::Full(99)));

    // Slow reader reads one message — frees one slot.
    assert_eq!(slow.try_recv(), Ok(0));
    p.try_publish(99).unwrap();

    // Still blocked because slow reader is still behind.
    assert_eq!(p.try_publish(200), Err(PublishError::Full(200)));
}

#[test]
fn bounded_no_subscribers_allows_unlimited_publish() {
    // If no subscribers have been created, there is no slowest cursor
    // to block on — the ring behaves like an unbounded lossy channel.
    let (mut p, _s) = channel_bounded::<u64>(4, 0);

    for i in 0..100 {
        p.try_publish(i).unwrap();
    }
    assert_eq!(p.published(), 100);
}

#[test]
fn regular_channel_try_publish_always_succeeds() {
    // Regular (lossy) channel — try_publish is just publish + Ok.
    let (mut p, s) = channel::<u64>(4);
    let mut _sub = s.subscribe();

    // Publish way more than capacity — no backpressure, all succeed.
    for i in 0..100 {
        p.try_publish(i).unwrap();
    }
    assert_eq!(p.published(), 100);
}

#[test]
fn bounded_full_cycle_stress() {
    // Publish and drain in lockstep, cycling the ring many times.
    let (mut p, s) = channel_bounded::<u64>(4, 0);
    let mut sub = s.subscribe();

    for cycle in 0..1000u64 {
        for slot in 0..4u64 {
            let val = cycle * 4 + slot;
            p.try_publish(val).unwrap();
        }
        assert_eq!(p.try_publish(9999), Err(PublishError::Full(9999)));
        for slot in 0..4u64 {
            let val = cycle * 4 + slot;
            assert_eq!(sub.try_recv(), Ok(val));
        }
    }
}

#[test]
fn bounded_cross_thread() {
    let (mut p, s) = channel_bounded::<u64>(64, 0);
    let mut sub = s.subscribe();
    let n = 100_000u64;

    let writer = std::thread::spawn(move || {
        for i in 0..n {
            loop {
                match p.try_publish(i) {
                    Ok(()) => break,
                    Err(PublishError::Full(_)) => core::hint::spin_loop(),
                }
            }
        }
    });

    let reader = std::thread::spawn(move || {
        for expected in 0..n {
            loop {
                match sub.try_recv() {
                    Ok(v) => {
                        assert_eq!(v, expected, "corruption at seq {expected}");
                        break;
                    }
                    Err(TryRecvError::Empty) => core::hint::spin_loop(),
                    Err(TryRecvError::Lagged { .. }) => {
                        panic!("bounded channel should never lag");
                    }
                }
            }
        }
    });

    writer.join().unwrap();
    reader.join().unwrap();
}

// -------------------------------------------------------------------------
// Observability counters
// -------------------------------------------------------------------------

#[test]
fn subscriber_counters_basic() {
    let (mut p, s) = channel::<u64>(64);
    let mut sub = s.subscribe();

    assert_eq!(sub.total_received(), 0);
    assert_eq!(sub.total_lagged(), 0);

    for i in 0..10 {
        p.publish(i);
    }
    for _ in 0..10 {
        sub.try_recv().unwrap();
    }

    assert_eq!(sub.total_received(), 10);
    assert_eq!(sub.total_lagged(), 0);
}

#[test]
fn subscriber_counters_with_lag() {
    let (mut p, s) = channel::<u64>(4);
    let mut sub = s.subscribe();

    // Publish 8 messages into a 4-slot ring — subscriber hasn't read any,
    // so 4 messages will be overwritten.
    for i in 0..8 {
        p.publish(i);
    }

    // First try_recv should report lag.
    let err = sub.try_recv().unwrap_err();
    match err {
        TryRecvError::Lagged { skipped } => assert_eq!(skipped, 4),
        other => panic!("expected Lagged, got {other:?}"),
    }
    assert_eq!(sub.total_lagged(), 4);
    assert_eq!(sub.total_received(), 0);

    // Now read the remaining 4 messages.
    for _ in 0..4 {
        sub.try_recv().unwrap();
    }
    assert_eq!(sub.total_received(), 4);
    assert_eq!(sub.total_lagged(), 4);
}

#[test]
fn receive_ratio() {
    let (mut p, s) = channel::<u64>(4);
    let mut sub = s.subscribe();

    // No messages processed — ratio should be 0.0.
    assert_eq!(sub.receive_ratio(), 0.0);

    // Publish 8 into a 4-slot ring: 4 lagged, then 4 received.
    for i in 0..8 {
        p.publish(i);
    }

    // Trigger lag detection.
    let _ = sub.try_recv(); // Lagged { skipped: 4 }

    // Read the remaining 4.
    for _ in 0..4 {
        sub.try_recv().unwrap();
    }

    // received = 4, lagged = 4 => ratio = 0.5
    assert_eq!(sub.total_received(), 4);
    assert_eq!(sub.total_lagged(), 4);
    assert!((sub.receive_ratio() - 0.5).abs() < f64::EPSILON);
}

#[test]
fn group_counters() {
    let (mut p, s) = channel::<u64>(4);
    let mut group = s.subscribe_group::<2>();

    assert_eq!(group.total_received(), 0);
    assert_eq!(group.total_lagged(), 0);
    assert_eq!(group.receive_ratio(), 0.0);

    // Normal receives — no lag.
    for i in 0..4 {
        p.publish(i);
    }
    for _ in 0..4 {
        group.try_recv().unwrap();
    }
    assert_eq!(group.total_received(), 4);
    assert_eq!(group.total_lagged(), 0);
    assert!((group.receive_ratio() - 1.0).abs() < f64::EPSILON);

    // Cause lag: publish 8 more into a 4-slot ring without reading.
    for i in 10..18 {
        p.publish(i);
    }

    // First try_recv should detect lag.
    let err = group.try_recv().unwrap_err();
    match err {
        TryRecvError::Lagged { skipped } => assert!(skipped > 0),
        other => panic!("expected Lagged, got {other:?}"),
    }
    assert!(group.total_lagged() > 0);
}

#[test]
fn publisher_sequence() {
    let (mut p, _s) = channel::<u64>(8);
    assert_eq!(p.sequence(), 0);
    p.publish(1);
    assert_eq!(p.sequence(), 1);
    p.publish_batch(&[2, 3, 4]);
    assert_eq!(p.sequence(), 4);
    // sequence() == published()
    assert_eq!(p.sequence(), p.published());
}

// -------------------------------------------------------------------------
// Bug fix: publish() respects backpressure on bounded channels
// -------------------------------------------------------------------------

#[test]
fn bounded_publish_blocks_until_consumer_catches_up() {
    // publish() (not just try_publish) must respect backpressure.
    let (mut p, s) = channel_bounded::<u64>(4, 0);
    let mut sub = s.subscribe();
    let n = 100u64;

    let writer = std::thread::spawn(move || {
        for i in 0..n {
            // Uses publish() — must block when ring is full.
            p.publish(i);
        }
    });

    let reader = std::thread::spawn(move || {
        for expected in 0..n {
            loop {
                match sub.try_recv() {
                    Ok(v) => {
                        assert_eq!(
                            v, expected,
                            "bounded publish() corruption at seq {expected}"
                        );
                        break;
                    }
                    Err(TryRecvError::Empty) => core::hint::spin_loop(),
                    Err(TryRecvError::Lagged { .. }) => {
                        panic!("bounded channel publish() should never cause lag");
                    }
                }
            }
        }
    });

    writer.join().unwrap();
    reader.join().unwrap();
}

#[test]
fn bounded_publish_batch_blocks_until_consumer_catches_up() {
    // publish_batch() must also respect backpressure.
    let (mut p, s) = channel_bounded::<u64>(4, 0);
    let mut sub = s.subscribe();

    let writer = std::thread::spawn(move || {
        // Publish 10 batches of 4 values each through a 4-slot ring.
        for batch in 0..10u64 {
            let values: Vec<u64> = (batch * 4..batch * 4 + 4).collect();
            p.publish_batch(&values);
        }
    });

    let reader = std::thread::spawn(move || {
        for expected in 0..40u64 {
            loop {
                match sub.try_recv() {
                    Ok(v) => {
                        assert_eq!(
                            v, expected,
                            "bounded publish_batch() corruption at seq {expected}"
                        );
                        break;
                    }
                    Err(TryRecvError::Empty) => core::hint::spin_loop(),
                    Err(TryRecvError::Lagged { .. }) => {
                        panic!("bounded channel publish_batch() should never cause lag");
                    }
                }
            }
        }
    });

    writer.join().unwrap();
    reader.join().unwrap();
}

// -------------------------------------------------------------------------
// Bug fix: Subscriber drop releases backpressure tracker
// -------------------------------------------------------------------------

#[test]
fn subscriber_drop_releases_backpressure() {
    let (mut p, s) = channel_bounded::<u64>(4, 0);

    // Create a subscriber that will be the only tracker.
    let sub = s.subscribe();

    // Fill the ring.
    for i in 0..4 {
        p.try_publish(i).unwrap();
    }

    // Ring is full — publisher can't publish.
    assert_eq!(p.try_publish(99), Err(PublishError::Full(99)));

    // Drop the subscriber — its tracker should be deregistered.
    drop(sub);

    // Now the publisher should be able to publish freely (no trackers = unbounded).
    p.try_publish(99).unwrap();
    p.try_publish(100).unwrap();
}

#[test]
fn subscriber_drop_unblocks_other_subscribers() {
    let (mut p, s) = channel_bounded::<u64>(4, 0);
    let slow = s.subscribe(); // slow reader, will be dropped
    let mut fast = s.subscribe();

    // Fill the ring.
    for i in 0..4 {
        p.try_publish(i).unwrap();
    }

    // Both trackers exist — slowest (slow) blocks the publisher.
    assert_eq!(p.try_publish(99), Err(PublishError::Full(99)));

    // Fast reader catches up.
    for _ in 0..4 {
        fast.try_recv().unwrap();
    }

    // Still blocked because slow reader hasn't read anything.
    assert_eq!(p.try_publish(99), Err(PublishError::Full(99)));

    // Drop the slow reader — removes its tracker.
    drop(slow);

    // Now the publisher is gated only by the fast reader.
    p.try_publish(99).unwrap();
}

// -------------------------------------------------------------------------
// Bug fix: SubscriberGroup participates in backpressure
// -------------------------------------------------------------------------

#[test]
fn subscriber_group_bounded_channel_basic() {
    let (mut p, s) = channel_bounded::<u64>(4, 0);
    let mut group = s.subscribe_group::<2>();

    // Publish and receive normally.
    for i in 0..4 {
        p.try_publish(i).unwrap();
    }

    // Ring is full — group tracker should block the publisher.
    assert_eq!(p.try_publish(99), Err(PublishError::Full(99)));

    // Drain one — frees one slot.
    assert_eq!(group.try_recv(), Ok(0));
    p.try_publish(99).unwrap();

    // Drain the rest.
    assert_eq!(group.try_recv(), Ok(1));
    assert_eq!(group.try_recv(), Ok(2));
    assert_eq!(group.try_recv(), Ok(3));
    assert_eq!(group.try_recv(), Ok(99));
    assert_eq!(group.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn subscriber_group_drop_releases_backpressure() {
    let (mut p, s) = channel_bounded::<u64>(4, 0);
    let group = s.subscribe_group::<2>();

    // Fill the ring.
    for i in 0..4 {
        p.try_publish(i).unwrap();
    }

    // Group tracker should block.
    assert_eq!(p.try_publish(99), Err(PublishError::Full(99)));

    // Drop the group — tracker should be deregistered.
    drop(group);

    // Publisher should be free now.
    p.try_publish(99).unwrap();
}

#[test]
fn subscriber_group_bounded_cross_thread() {
    let (mut p, s) = channel_bounded::<u64>(64, 0);
    let mut group = s.subscribe_group::<3>();
    let n = 10_000u64;

    let writer = std::thread::spawn(move || {
        for i in 0..n {
            p.publish(i);
        }
    });

    let reader = std::thread::spawn(move || {
        for expected in 0..n {
            loop {
                match group.try_recv() {
                    Ok(v) => {
                        assert_eq!(
                            v, expected,
                            "subscriber group bounded corruption at seq {expected}"
                        );
                        break;
                    }
                    Err(TryRecvError::Empty) => core::hint::spin_loop(),
                    Err(TryRecvError::Lagged { .. }) => {
                        panic!("bounded channel subscriber group should never lag");
                    }
                }
            }
        }
    });

    writer.join().unwrap();
    reader.join().unwrap();
}

// -------------------------------------------------------------------------
// Bug fix: subscribe_group::<0>() must panic
// -------------------------------------------------------------------------

#[test]
#[should_panic(expected = "SubscriberGroup requires at least 1 subscriber")]
fn subscribe_group_zero_panics() {
    let (_p, s) = channel::<u64>(4);
    let _group = s.subscribe_group::<0>();
}

// -------------------------------------------------------------------------
// MPMC (multi-producer, multi-consumer) channel
// -------------------------------------------------------------------------

#[test]
fn mpmc_basic() {
    let (pub1, subs) = channel_mpmc::<u64>(64);
    let pub2 = pub1.clone();
    let mut sub = subs.subscribe();

    pub1.publish(1);
    pub2.publish(2);

    assert_eq!(sub.try_recv(), Ok(1));
    assert_eq!(sub.try_recv(), Ok(2));
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn mpmc_two_publishers_one_subscriber() {
    let (pub1, subs) = channel_mpmc::<u64>(4096);
    let pub2 = pub1.clone();
    let mut sub = subs.subscribe();
    let n = 10_000u64;

    let writer1 = std::thread::spawn(move || {
        for i in 0..n {
            pub1.publish(i * 2); // even numbers
        }
    });

    let writer2 = std::thread::spawn(move || {
        for i in 0..n {
            pub2.publish(i * 2 + 1); // odd numbers
        }
    });

    writer1.join().unwrap();
    writer2.join().unwrap();

    // Read all available messages.
    let mut received = Vec::new();
    loop {
        match sub.try_recv() {
            Ok(v) => received.push(v),
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Lagged { .. }) => {}
        }
    }

    // Verify: we received some messages and no values are corrupted.
    // Values are interleaved (even from writer1, odd from writer2), so we
    // cannot assert monotonic *value* order. Instead, verify each value is
    // in the expected range and separate the streams.
    let mut evens = Vec::new();
    let mut odds = Vec::new();
    for &v in &received {
        assert!(v < 2 * n, "value {v} out of range");
        if v % 2 == 0 {
            evens.push(v / 2);
        } else {
            odds.push((v - 1) / 2);
        }
    }

    // Within each producer's stream, values must appear in order
    // (may have gaps from lag).
    for window in evens.windows(2) {
        assert!(
            window[1] > window[0],
            "even stream out of order: {} -> {}",
            window[0],
            window[1]
        );
    }
    for window in odds.windows(2) {
        assert!(
            window[1] > window[0],
            "odd stream out of order: {} -> {}",
            window[0],
            window[1]
        );
    }

    assert!(!received.is_empty(), "should have received some messages");
}

#[test]
fn mpmc_ordering() {
    // Verify that messages from a single MPMC channel arrive in the order
    // their sequence numbers were claimed, not in the order values happen
    // to be written.
    let (pub_, subs) = channel_mpmc::<u64>(64);
    let mut sub = subs.subscribe();

    // Sequential publishes — sequence order matches publish order.
    for i in 0..20 {
        pub_.publish(i);
    }

    for i in 0..20 {
        assert_eq!(sub.try_recv(), Ok(i));
    }
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn mpmc_stress() {
    // 4 publishers, 2 subscribers, 10K messages each publisher.
    // (Kept modest because the CAS-based cursor is serialised — debug mode
    //  would be very slow with higher counts.)
    let (pub_, subs) = channel_mpmc::<u64>(4096);
    let n_per_pub = 10_000u64;
    let n_pubs = 4u64;
    let total = n_per_pub * n_pubs;

    let mut sub1 = subs.subscribe();
    let mut sub2 = subs.subscribe();

    let mut writers = Vec::new();
    for pid in 0..n_pubs {
        let p = pub_.clone();
        writers.push(std::thread::spawn(move || {
            for i in 0..n_per_pub {
                // Encode publisher ID and sequence in the value.
                p.publish(pid * n_per_pub + i);
            }
        }));
    }

    let reader1 = std::thread::spawn(move || {
        let mut count = 0u64;
        loop {
            match sub1.try_recv() {
                Ok(_v) => {
                    count += 1;
                    if count >= total {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => {
                    if count >= total {
                        break;
                    }
                    core::hint::spin_loop();
                }
                Err(TryRecvError::Lagged { skipped }) => {
                    count += skipped;
                }
            }
        }
        count
    });

    let reader2 = std::thread::spawn(move || {
        let mut count = 0u64;
        loop {
            match sub2.try_recv() {
                Ok(_v) => {
                    count += 1;
                    if count >= total {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => {
                    if count >= total {
                        break;
                    }
                    core::hint::spin_loop();
                }
                Err(TryRecvError::Lagged { skipped }) => {
                    count += skipped;
                }
            }
        }
        count
    });

    for w in writers {
        w.join().unwrap();
    }

    let c1 = reader1.join().unwrap();
    let c2 = reader2.join().unwrap();

    // Both readers should see all messages (received + lagged = total).
    assert_eq!(c1, total, "reader1 saw {c1} of {total}");
    assert_eq!(c2, total, "reader2 saw {c2} of {total}");
}

#[test]
fn mpmc_published_count() {
    let (pub_, _subs) = channel_mpmc::<u64>(64);
    assert_eq!(pub_.published(), 0);

    pub_.publish(1);
    assert_eq!(pub_.published(), 1);

    pub_.publish(2);
    pub_.publish(3);
    assert_eq!(pub_.published(), 3);
}

#[test]
fn mpmc_capacity() {
    let (pub_, _subs) = channel_mpmc::<u64>(128);
    assert_eq!(pub_.capacity(), 128);
}

#[test]
fn mpmc_subscribe_sees_only_future() {
    let (pub_, subs) = channel_mpmc::<u64>(64);
    pub_.publish(1);
    pub_.publish(2);

    let mut sub = subs.subscribe();
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));

    pub_.publish(3);
    assert_eq!(sub.try_recv(), Ok(3));
}

#[test]
fn mpmc_latest() {
    let (pub_, subs) = channel_mpmc::<u64>(64);
    let mut sub = subs.subscribe();

    for i in 0..10 {
        pub_.publish(i);
    }

    assert_eq!(sub.latest(), Some(9));
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn mpmc_pending() {
    let (pub_, subs) = channel_mpmc::<u64>(64);
    let mut sub = subs.subscribe();

    assert_eq!(sub.pending(), 0);
    pub_.publish(1);
    assert_eq!(sub.pending(), 1);
    pub_.publish(2);
    pub_.publish(3);
    assert_eq!(sub.pending(), 3);

    sub.try_recv().unwrap();
    assert_eq!(sub.pending(), 2);
}

#[test]
fn mpmc_lag_detection() {
    let (pub_, subs) = channel_mpmc::<u64>(4);
    let mut sub = subs.subscribe();

    // Publish 6 messages into a 4-slot ring — overflow by 2.
    for i in 0..6 {
        pub_.publish(i);
    }

    let err = sub.try_recv().unwrap_err();
    assert_eq!(err, TryRecvError::Lagged { skipped: 2 });

    assert_eq!(sub.try_recv(), Ok(2));
    assert_eq!(sub.try_recv(), Ok(3));
    assert_eq!(sub.try_recv(), Ok(4));
    assert_eq!(sub.try_recv(), Ok(5));
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn mpmc_blocking_recv() {
    let (pub_, subs) = channel_mpmc::<u64>(64);
    let mut sub = subs.subscribe();

    let writer = std::thread::spawn(move || {
        for i in 0..10 {
            std::thread::sleep(std::time::Duration::from_micros(100));
            pub_.publish(i);
        }
    });

    for i in 0..10 {
        let v = sub.recv();
        assert_eq!(v, i);
    }

    writer.join().unwrap();
}

#[test]
fn mpmc_clone_is_independent() {
    let (pub_, subs) = channel_mpmc::<u64>(64);
    let pub2 = pub_.clone();
    let pub3 = pub_.clone();

    // All three clones should be able to publish.
    pub_.publish(10);
    pub2.publish(20);
    pub3.publish(30);

    let mut sub = subs.subscribe_from_oldest();
    assert_eq!(sub.try_recv(), Ok(10));
    assert_eq!(sub.try_recv(), Ok(20));
    assert_eq!(sub.try_recv(), Ok(30));
}
