// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The event ring carries payloads the `Pod` ring cannot: heap-owning types,
//! enums, `Option`, `bool` — and reuses their allocations.

use photon_ring::event_channel;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug, PartialEq)]
enum Side {
    Buy,
    Sell,
}

#[derive(Debug)]
struct Order {
    symbol: String,
    tags: Vec<u32>,
    side: Side,
    filled: bool,
    parent: Option<u64>,
}

impl Default for Order {
    fn default() -> Self {
        Order {
            symbol: String::new(),
            tags: Vec::new(),
            side: Side::Buy,
            filled: false,
            parent: None,
        }
    }
}

#[test]
fn carries_types_pod_forbids() {
    let (mut tx, rx) = event_channel(8, Order::default);
    let mut sub = rx.subscribe();

    tx.publish(|o| {
        o.symbol.clear();
        o.symbol.push_str("BTCUSD");
        o.tags.clear();
        o.tags.extend_from_slice(&[7, 8, 9]);
        o.side = Side::Sell;
        o.filled = true;
        o.parent = Some(42);
    });

    let got = sub
        .process(|o| {
            (
                o.symbol.clone(),
                o.tags.clone(),
                matches!(o.side, Side::Sell),
                o.filled,
                o.parent,
            )
        })
        .expect("a message was published");

    assert_eq!(got.0, "BTCUSD");
    assert_eq!(got.1, vec![7, 8, 9]);
    assert!(got.2);
    assert!(got.3);
    assert_eq!(got.4, Some(42));
}

#[test]
fn steady_state_reuses_allocations() {
    let (mut tx, rx) = event_channel(4, Order::default);
    let mut sub = rx.subscribe();

    // Warm up: one full lap so every slot has grown its buffers once.
    for i in 0..4u32 {
        tx.publish(|o| {
            o.symbol.clear();
            o.symbol.push_str("SYMBOL");
            o.tags.clear();
            o.tags.push(i);
        });
        sub.process(|_| ()).unwrap();
    }

    // Steady state: publishing the same shape must not reallocate again, so the
    // observed capacity is stable rather than merely large.
    let mut caps = Vec::new();
    for i in 0..32u32 {
        tx.publish(|o| {
            o.symbol.clear();
            o.symbol.push_str("SYMBOL");
            o.tags.clear();
            o.tags.push(i);
        });
        caps.push(
            sub.process(|o| (o.symbol.capacity(), o.tags.capacity()))
                .unwrap(),
        );
    }

    let first = caps[0];
    assert!(
        caps.iter().all(|&c| c == first),
        "steady-state publishing reallocated: {caps:?}"
    );
    assert!(first.0 >= 6 && first.1 >= 1, "buffers were actually used");
}

#[test]
fn values_are_dropped_exactly_once_when_the_ring_dies() {
    static ALIVE: AtomicUsize = AtomicUsize::new(0);

    struct Counted(#[allow(dead_code)] usize);
    impl Counted {
        fn new() -> Self {
            ALIVE.fetch_add(1, Ordering::SeqCst);
            Counted(0)
        }
    }
    impl Drop for Counted {
        fn drop(&mut self) {
            ALIVE.fetch_sub(1, Ordering::SeqCst);
        }
    }

    {
        let (mut tx, rx) = event_channel(8, Counted::new);
        assert_eq!(ALIVE.load(Ordering::SeqCst), 8, "factory fills every slot");
        let mut sub = rx.subscribe();
        tx.publish(|c| c.0 = 1);
        sub.process(|_| ()).unwrap();
    }

    assert_eq!(
        ALIVE.load(Ordering::SeqCst),
        0,
        "every slot value must be dropped when the ring is dropped"
    );
}

#[test]
fn every_subscriber_gates_the_publisher() {
    let (mut tx, rx) = event_channel(4, || 0u64);
    let _idle = rx.subscribe();

    for i in 0..4u64 {
        assert!(tx.try_publish(|v| *v = i), "ring has room");
    }
    assert!(
        !tx.try_publish(|v| *v = 99),
        "an idle subscriber must stop the publisher"
    );
}

#[test]
fn a_dropped_subscriber_releases_the_publisher() {
    let (mut tx, rx) = event_channel(4, || 0u64);
    {
        let _blocker = rx.subscribe();
        for i in 0..4u64 {
            assert!(tx.try_publish(|v| *v = i));
        }
        assert!(!tx.try_publish(|v| *v = 99), "blocked while it is alive");
    }
    assert!(
        tx.try_publish(|v| *v = 100),
        "dropping the subscriber must release the publisher"
    );
}

#[test]
fn messages_arrive_in_order_across_threads() {
    let (mut tx, rx) = event_channel(64, || 0u64);
    let mut sub = rx.subscribe();
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();

    let reader = std::thread::spawn(move || {
        let mut expect = 0u64;
        while expect < 10_000 {
            if let Some(v) = sub.process(|v| *v) {
                assert_eq!(v, expect, "out of order");
                expect += 1;
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    for i in 0..10_000u64 {
        tx.publish(|v| *v = i);
    }
    reader.join().unwrap();
    assert_eq!(seen.load(Ordering::Relaxed), 10_000);
}
