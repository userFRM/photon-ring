// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! Tests that specifically exercise the `atomic-slots` feature — AtomicU64
//! stripe-based seqlock reads/writes. Covers partial stripes, multi-stripe
//! payloads, cache-line-boundary payloads, and concurrent stress scenarios.

#![cfg(feature = "atomic-slots")]

use photon_ring::{channel, channel_bounded, channel_mpmc, Pod, PublishError, TryRecvError};

// -------------------------------------------------------------------------
// Helper Pod types
// -------------------------------------------------------------------------

/// 7 bytes — NOT a multiple of 8. Tests partial-stripe handling.
/// `repr(C, packed)` suppresses trailing padding so size is truly 7.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Odd {
    a: u32,
    b: u16,
    c: u8,
}

// SAFETY: Odd is #[repr(C)] with all numeric fields;
// every bit pattern is a valid Odd.
unsafe impl Pod for Odd {}

/// 48 bytes — 6 full u64 stripes. Tests multi-stripe read/write.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Large {
    f0: u64,
    f1: u64,
    f2: u64,
    f3: u64,
    f4: u64,
    f5: u64,
}

unsafe impl Pod for Large {}

/// 56 bytes — 7 u64 stripes, the maximum payload that fits in a single
/// 64-byte cache line alongside the 8-byte stamp.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct CacheLine {
    f0: u64,
    f1: u64,
    f2: u64,
    f3: u64,
    f4: u64,
    f5: u64,
    f6: u64,
}

unsafe impl Pod for CacheLine {}

// -------------------------------------------------------------------------
// 1. Small payload (u8) — 1-byte partial stripe
// -------------------------------------------------------------------------

#[test]
fn test_small_payload_u8() {
    let (mut p, s) = channel::<u8>(4);
    let mut sub = s.subscribe();

    p.publish(0xAB);
    assert_eq!(sub.try_recv(), Ok(0xAB));

    // Multiple values through the ring.
    for i in 0u8..=255 {
        p.publish(i);
    }
    // Read them all back (ring size 4, so only the last 4 are available
    // without lag if we read fast enough — but we published synchronously
    // before subscribing reads, so lag will occur).
    let err = sub.try_recv().unwrap_err();
    match err {
        TryRecvError::Lagged { skipped } => {
            assert!(skipped > 0, "expected lag after overflow");
        }
        other => panic!("expected Lagged, got {other:?}"),
    }

    // Read remaining values — they should be valid u8s (no corruption).
    let mut count = 0;
    loop {
        match sub.try_recv() {
            Ok(_v) => count += 1,
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Lagged { .. }) => {}
        }
    }
    assert!(count > 0, "should have received at least some u8 values");
}

// -------------------------------------------------------------------------
// 2. Odd-size payload — partial stripe handling
// -------------------------------------------------------------------------

#[test]
fn test_odd_size_payload() {
    // Verify the type is indeed not a multiple of 8 (7 bytes, packed).
    assert_eq!(core::mem::size_of::<Odd>(), 7);

    let (mut p, s) = channel::<Odd>(8);
    let mut sub = s.subscribe();

    let val = Odd {
        a: 0xDEAD_BEEF,
        b: 0xCAFE,
        c: 0x42,
    };
    p.publish(val);

    let received = sub.try_recv().unwrap();
    // Copy fields out to avoid unaligned references on the packed struct.
    let (ra, rb, rc) = (received.a, received.b, received.c);
    assert_eq!(ra, 0xDEAD_BEEF);
    assert_eq!(rb, 0xCAFE);
    assert_eq!(rc, 0x42);
    assert_eq!(received, val);

    // Multiple round-trips.
    for i in 0..100u32 {
        let v = Odd {
            a: i,
            b: (i & 0xFFFF) as u16,
            c: (i & 0xFF) as u8,
        };
        p.publish(v);
        assert_eq!(sub.try_recv(), Ok(v));
    }
}

// -------------------------------------------------------------------------
// 3. Large payload — multi-stripe (48 bytes = 6 stripes)
// -------------------------------------------------------------------------

#[test]
fn test_large_payload() {
    assert_eq!(core::mem::size_of::<Large>(), 48);

    let (mut p, s) = channel::<Large>(8);
    let mut sub = s.subscribe();

    let val = Large {
        f0: 0x0011_2233_4455_6677,
        f1: 0x8899_AABB_CCDD_EEFF,
        f2: u64::MAX,
        f3: 0,
        f4: 42,
        f5: 0xDEAD_BEEF_CAFE_BABE,
    };
    p.publish(val);

    let received = sub.try_recv().unwrap();
    assert_eq!(received, val);
    assert_eq!(received.f0, 0x0011_2233_4455_6677);
    assert_eq!(received.f5, 0xDEAD_BEEF_CAFE_BABE);

    // Cycle the ring multiple times.
    for i in 0..50u64 {
        let v = Large {
            f0: i,
            f1: i.wrapping_mul(3),
            f2: i.wrapping_mul(7),
            f3: i.wrapping_mul(11),
            f4: i.wrapping_mul(13),
            f5: i.wrapping_mul(17),
        };
        p.publish(v);
        assert_eq!(sub.try_recv(), Ok(v));
    }
}

// -------------------------------------------------------------------------
// 4. Cache-line payload (56 bytes = 7 stripes, max single cache line)
// -------------------------------------------------------------------------

#[test]
fn test_cache_line_payload() {
    assert_eq!(core::mem::size_of::<CacheLine>(), 56);

    let (mut p, s) = channel::<CacheLine>(8);
    let mut sub = s.subscribe();

    let val = CacheLine {
        f0: 1,
        f1: 2,
        f2: 3,
        f3: 4,
        f4: 5,
        f5: 6,
        f6: 7,
    };
    p.publish(val);

    let received = sub.try_recv().unwrap();
    assert_eq!(received, val);

    // Verify each field individually.
    assert_eq!(received.f0, 1);
    assert_eq!(received.f1, 2);
    assert_eq!(received.f2, 3);
    assert_eq!(received.f3, 4);
    assert_eq!(received.f4, 5);
    assert_eq!(received.f5, 6);
    assert_eq!(received.f6, 7);

    // Edge: all bits set.
    let all_ones = CacheLine {
        f0: u64::MAX,
        f1: u64::MAX,
        f2: u64::MAX,
        f3: u64::MAX,
        f4: u64::MAX,
        f5: u64::MAX,
        f6: u64::MAX,
    };
    p.publish(all_ones);
    assert_eq!(sub.try_recv(), Ok(all_ones));
}

// -------------------------------------------------------------------------
// 5. Cross-thread SPMC under atomic-slots — 100K messages, 2 subscribers
// -------------------------------------------------------------------------

#[test]
fn test_cross_thread_atomic_slots() {
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
                        assert!(v > prev, "ordering violation: {prev} -> {v}");
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
                        assert!(v > prev, "ordering violation: {prev} -> {v}");
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

    assert!(c1 > 0, "reader1 should have received messages");
    assert!(c2 > 0, "reader2 should have received messages");
}

// -------------------------------------------------------------------------
// 6. Stress: 1M messages through a 4096-slot ring under atomic-slots
// -------------------------------------------------------------------------

#[test]
fn test_stress_1m_atomic_slots() {
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
    eprintln!("test_stress_1m_atomic_slots: lags = {lags}");
}

// -------------------------------------------------------------------------
// 7. MPMC under atomic-slots — 2 publishers, 1 subscriber, 10K each
// -------------------------------------------------------------------------

#[test]
fn test_mpmc_atomic_slots() {
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

    // Drain all available messages.
    let mut received = Vec::new();
    loop {
        match sub.try_recv() {
            Ok(v) => received.push(v),
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Lagged { .. }) => {}
        }
    }

    // Separate streams and verify per-stream ordering.
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

// -------------------------------------------------------------------------
// 8. Bounded channel under atomic-slots — backpressure verification
// -------------------------------------------------------------------------

#[test]
fn test_bounded_atomic_slots() {
    let (mut p, s) = channel_bounded::<u64>(4, 0);
    let mut sub = s.subscribe();

    // Fill the ring (4 slots).
    for i in 0..4u64 {
        p.try_publish(i).unwrap();
    }

    // Ring is full — backpressure must kick in.
    assert_eq!(p.try_publish(99), Err(PublishError::Full(99)));

    // Drain and verify values.
    for i in 0..4u64 {
        assert_eq!(sub.try_recv(), Ok(i));
    }
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));

    // After draining, publisher can write again.
    for i in 100..104u64 {
        p.try_publish(i).unwrap();
    }

    // Full again.
    assert_eq!(p.try_publish(999), Err(PublishError::Full(999)));

    // Drain second batch.
    for i in 100..104u64 {
        assert_eq!(sub.try_recv(), Ok(i));
    }
    assert_eq!(sub.try_recv(), Err(TryRecvError::Empty));

    // Cross-thread bounded under atomic-slots: verify no corruption.
    let (mut p2, s2) = channel_bounded::<u64>(64, 0);
    let mut sub2 = s2.subscribe();
    let n = 10_000u64;

    let writer = std::thread::spawn(move || {
        for i in 0..n {
            p2.publish(i);
        }
    });

    let reader = std::thread::spawn(move || {
        for expected in 0..n {
            loop {
                match sub2.try_recv() {
                    Ok(v) => {
                        assert_eq!(
                            v, expected,
                            "bounded atomic-slots corruption at seq {expected}"
                        );
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
