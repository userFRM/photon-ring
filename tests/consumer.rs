// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Managed consumer behaviour: ordering, sequence numbering, batch signalling,
//! drain-on-shutdown, and panic containment.

use photon_ring::topology::{Consumer, DrainPolicy};
use photon_ring::{channel_bounded, WaitStrategy};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
fn processes_every_message_in_order() {
    let (mut p, s) = channel_bounded::<u64>(64, 0);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();

    let c = Consumer::spawn(
        s.subscribe(),
        WaitStrategy::default(),
        DrainPolicy::Drain,
        move |v, _seq, _eob| sink.lock().unwrap().push(v),
    );

    for i in 0..1_000u64 {
        p.publish(i);
    }
    c.shutdown();
    c.join();

    let got = seen.lock().unwrap().clone();
    assert_eq!(got, (0..1_000).collect::<Vec<u64>>());
}

#[test]
fn sequence_matches_the_message() {
    let (mut p, s) = channel_bounded::<u64>(64, 0);
    let bad = Arc::new(AtomicU64::new(0));
    let flag = bad.clone();

    // Published value == its sequence number, so any mismatch is detectable.
    let c = Consumer::spawn(
        s.subscribe(),
        WaitStrategy::default(),
        DrainPolicy::Drain,
        move |v, seq, _eob| {
            if v != seq {
                flag.fetch_add(1, Ordering::Relaxed);
            }
        },
    );
    for i in 0..500u64 {
        p.publish(i);
    }
    c.shutdown();
    c.join();
    assert_eq!(bad.load(Ordering::Relaxed), 0);
}

#[test]
fn end_of_batch_is_set_exactly_once_per_batch() {
    let (mut p, s) = channel_bounded::<u64>(64, 0);
    let marks = Arc::new(Mutex::new(Vec::new()));
    let sink = marks.clone();

    let c = Consumer::spawn(
        s.subscribe(),
        WaitStrategy::default(),
        DrainPolicy::Drain,
        move |v, _seq, eob| sink.lock().unwrap().push((v, eob)),
    );

    for i in 0..200u64 {
        p.publish(i);
    }
    c.shutdown();
    c.join();

    let got = marks.lock().unwrap().clone();
    assert_eq!(got.len(), 200, "every message delivered");
    // The final message must always close a batch, and at least one flush
    // must occur, otherwise a handler that buffers would never emit.
    assert!(got.last().unwrap().1, "last message must end a batch");
    assert!(got.iter().filter(|(_, eob)| *eob).count() >= 1);
}

#[test]
fn drain_policy_processes_buffered_messages_on_shutdown() {
    let (mut p, s) = channel_bounded::<u64>(256, 0);
    let count = Arc::new(AtomicU64::new(0));
    let n = count.clone();
    let gate = Arc::new(AtomicBool::new(false));
    let wait = gate.clone();

    // Hold the handler until after shutdown is signalled, so the messages are
    // definitely still buffered when it observes the stop.
    let c = Consumer::spawn(
        s.subscribe(),
        WaitStrategy::default(),
        DrainPolicy::Drain,
        move |_v, _seq, _eob| {
            while !wait.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            n.fetch_add(1, Ordering::Relaxed);
        },
    );

    for i in 0..100u64 {
        p.publish(i);
    }
    c.shutdown();
    gate.store(true, Ordering::Release);
    c.join();

    assert_eq!(
        count.load(Ordering::Relaxed),
        100,
        "DrainPolicy::Drain must process buffered messages before exiting"
    );
}

#[test]
fn a_panicking_handler_is_captured_and_releases_the_publisher() {
    let (mut p, s) = channel_bounded::<u64>(16, 0);
    let c = Consumer::spawn(
        s.subscribe(),
        WaitStrategy::default(),
        DrainPolicy::Immediate,
        |v, _seq, _eob| {
            if v >= 4 {
                panic!("handler died");
            }
        },
    );

    // The publisher must not be wedged by the dead consumer.
    let done = Arc::new(AtomicBool::new(false));
    let d = done.clone();
    let writer = std::thread::spawn(move || {
        for i in 0..5_000u64 {
            p.publish(i);
        }
        d.store(true, Ordering::Release);
    });

    let start = Instant::now();
    while !done.load(Ordering::Acquire) && start.elapsed() < Duration::from_secs(30) {
        std::thread::yield_now();
    }
    assert!(
        done.load(Ordering::Acquire),
        "publisher wedged by a dead consumer"
    );
    writer.join().unwrap();

    assert!(c.panicked(), "panic should be reported");
    assert!(!c.is_healthy());
    c.join();
}
