use photon_ring::{channel, Photon, TryRecvError};

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
