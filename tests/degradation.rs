// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Degradation semantics: what a bounded ring does when a consumer misbehaves.
//!
//! Each test corresponds to a claim made in the README. They exist so those
//! claims are falsifiable rather than asserted.

use photon_ring::{channel_bounded, TryRecvError};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A lossy subscriber must never gate the publisher, even on a bounded ring
/// whose capacity it has long since fallen behind.
#[test]
fn lossy_subscriber_never_gates_publisher() {
    let (mut p, s) = channel_bounded::<u64>(16, 0);
    // Present but never read from: on a bounded ring a *tracked* subscriber
    // here would stop the publisher after 16 messages.
    let _idle = s.subscribe_lossy();

    for i in 0..10_000u64 {
        p.try_publish(i)
            .expect("lossy subscriber must not exert backpressure");
    }
    assert_eq!(p.published(), 10_000);
}

/// The inverse, so the test above is meaningful: a tracked subscriber *does*
/// gate the publisher.
#[test]
fn tracked_subscriber_does_gate_publisher() {
    let (mut p, s) = channel_bounded::<u64>(16, 0);
    let _idle = s.subscribe();

    for i in 0..16u64 {
        p.try_publish(i).expect("ring has room");
    }
    assert!(
        p.try_publish(99).is_err(),
        "a tracked subscriber that has read nothing must stop the publisher"
    );
}

/// Mixed criticality: both contracts on one ring. The tracked consumer loses
/// nothing; the lossy one absorbs the loss and reports it exactly.
#[test]
fn mixed_criticality_on_one_ring() {
    let (mut p, s) = channel_bounded::<u64>(8, 0);
    let mut critical = s.subscribe();
    let mut telemetry = s.subscribe_lossy();

    // Fill past capacity. The publisher is gated only by `critical`, so drain
    // it as we go while `telemetry` deliberately falls behind.
    for i in 0..64u64 {
        p.publish(i);
        assert_eq!(
            critical.try_recv(),
            Ok(i),
            "tracked consumer must lose nothing"
        );
    }

    assert_eq!(critical.total_lagged(), 0);

    // Telemetry fell behind and reports it precisely.
    let mut lagged = false;
    loop {
        match telemetry.try_recv() {
            Ok(_) => {}
            Err(TryRecvError::Lagged { skipped }) => {
                assert!(skipped > 0);
                lagged = true;
            }
            Err(TryRecvError::Empty) => break,
        }
    }
    assert!(lagged, "lossy consumer should have observed lag");
    assert!(telemetry.total_lagged() > 0);
    assert!(telemetry.receive_ratio() < 1.0);
}

/// Subscribers share one sequence space, so a lossy tap and a gating consumer
/// can correlate positions on the same message.
#[test]
fn subscribers_share_a_sequence_space() {
    let (mut p, s) = channel_bounded::<u64>(16, 0);
    let mut critical = s.subscribe();
    let mut tap = s.subscribe_lossy();

    p.publish(100);
    p.publish(200);

    assert_eq!(critical.cursor(), tap.cursor());
    assert_eq!(critical.try_recv(), Ok(100));
    assert_eq!(tap.try_recv(), Ok(100));
    assert_eq!(
        critical.cursor(),
        tap.cursor(),
        "same message, same sequence"
    );
}

/// A *dead* consumer must not wedge the publisher forever. Its `Subscriber` is
/// dropped while the thread unwinds, which prunes its tracker from the
/// backpressure set and releases the publisher.
#[test]
fn dead_consumer_releases_the_publisher() {
    let (mut p, s) = channel_bounded::<u64>(16, 0);
    let mut sub = s.subscribe(); // tracked: gates the publisher

    let consumer = std::thread::spawn(move || {
        loop {
            if let Ok(v) = sub.try_recv() {
                if v >= 4 {
                    panic!("consumer died"); // `sub` dropped during unwind
                }
            }
        }
    });

    let done = Arc::new(AtomicBool::new(false));
    let done_w = done.clone();
    let count = Arc::new(AtomicU64::new(0));
    let count_w = count.clone();

    let writer = std::thread::spawn(move || {
        for i in 0..5_000u64 {
            p.publish(i);
            count_w.store(i + 1, Ordering::Relaxed);
        }
        done_w.store(true, Ordering::Relaxed);
    });

    let start = Instant::now();
    while !done.load(Ordering::Relaxed) && start.elapsed() < Duration::from_secs(30) {
        std::thread::yield_now();
    }

    assert!(
        done.load(Ordering::Relaxed),
        "publisher wedged after the consumer died: only {} of 5000 published",
        count.load(Ordering::Relaxed)
    );

    let _ = consumer.join(); // panicked by design
    writer.join().expect("writer should finish");
}

/// A subscriber can be attached to a ring that is already running, and a lossy
/// one perturbs nothing when it arrives or leaves.
#[test]
fn lossy_subscriber_hot_attaches_and_detaches() {
    let (mut p, s) = channel_bounded::<u64>(16, 0);
    let mut critical = s.subscribe();

    for i in 0..8u64 {
        p.publish(i);
        assert_eq!(critical.try_recv(), Ok(i));
    }

    {
        // Attach mid-stream; sees only future messages.
        let mut tap = s.subscribe_lossy();
        p.publish(99);
        assert_eq!(critical.try_recv(), Ok(99));
        assert_eq!(tap.try_recv(), Ok(99));
    } // detached

    // Publisher and the critical consumer carry on untouched.
    for i in 100..200u64 {
        p.publish(i);
        assert_eq!(critical.try_recv(), Ok(i));
    }
    assert_eq!(critical.total_lagged(), 0);
}

/// The no-loss guarantee belongs to a tracked subscriber's lifetime, not to the
/// ring. Once the last tracked subscriber is gone, nothing gates the publisher
/// and a bounded ring behaves like a lossy one.
#[test]
fn losing_the_last_tracked_subscriber_unbounds_the_ring() {
    let (mut p, s) = channel_bounded::<u64>(8, 0);
    let mut lossy = s.subscribe_lossy();

    {
        let _critical = s.subscribe();
        for i in 0..8u64 {
            p.try_publish(i).expect("ring has room");
        }
        assert!(p.try_publish(99).is_err(), "tracked subscriber gates here");
    } // dropped

    // With no tracked subscriber left, the publisher runs unbounded.
    for i in 0..1_000u64 {
        p.try_publish(i)
            .expect("nothing gates the publisher any more");
    }
    // The lossy subscriber absorbs that as lag, as documented.
    let mut lagged = false;
    while let Err(TryRecvError::Lagged { .. }) | Ok(_) = lossy.try_recv() {
        if lossy.total_lagged() > 0 {
            lagged = true;
            break;
        }
    }
    assert!(lagged, "lossy subscriber should observe the lap as lag");
}
