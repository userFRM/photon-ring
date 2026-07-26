// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What happens to the publisher when a consumer misbehaves.
//!
//! A bounded ring gives every registered consumer a no-loss guarantee, which
//! means a slow consumer applies backpressure — that is the feature working.
//! But two failure modes should NOT stop the world:
//!
//!   1. a consumer that is merely an observer (telemetry, logging, a debug tap)
//!   2. a consumer that has died
//!
//! Run with: cargo run --release --example degradation

use photon_ring::{channel_bounded, TryRecvError};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RING: usize = 64;
const N: u64 = 200_000;

fn main() {
    println!("ring capacity {RING}, publishing {N} messages\n");
    slow_observer();
    dead_consumer();
}

/// A deliberately slow observer on the same ring as a critical consumer.
/// The critical consumer keeps its no-loss guarantee; the observer absorbs
/// the loss and reports exactly how much it missed.
fn slow_observer() {
    println!("-- slow observer, sharing a ring with a critical consumer --");

    let (mut p, s) = channel_bounded::<u64>(RING, 0);
    let mut critical = s.subscribe(); // gates the publisher
    let mut telemetry = s.subscribe_lossy(); // never gates it

    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let missed = Arc::new(AtomicU64::new(0));
    let missed_t = missed.clone();
    let seen = Arc::new(AtomicU64::new(0));
    let seen_t = seen.clone();

    let observer = std::thread::spawn(move || {
        while !stop_t.load(Ordering::Relaxed) {
            match telemetry.try_recv() {
                Ok(_) => {
                    seen_t.fetch_add(1, Ordering::Relaxed);
                    // Simulate export work: serialising, an HTTP post, a disk write.
                    for _ in 0..50 {
                        std::hint::spin_loop();
                    }
                }
                Err(TryRecvError::Lagged { skipped }) => {
                    missed_t.fetch_add(skipped, Ordering::Relaxed);
                }
                Err(TryRecvError::Empty) => std::hint::spin_loop(),
            }
        }
    });

    let start = Instant::now();
    let mut received = 0u64;
    for i in 0..N {
        p.publish(i);
        // The critical consumer keeps up, so the publisher is never gated.
        while critical.try_recv().is_ok() {
            received += 1;
        }
    }
    let elapsed = start.elapsed();
    stop.store(true, Ordering::Relaxed);
    observer.join().unwrap();

    println!("  publisher    : {N} messages in {elapsed:.3?}");
    println!(
        "  critical     : {received} received, {} lost",
        critical.total_lagged()
    );
    println!(
        "  telemetry    : {} received, {} dropped ({:.1}% delivered)",
        seen.load(Ordering::Relaxed),
        missed.load(Ordering::Relaxed),
        telemetry_ratio(&seen, &missed)
    );
    println!("  -> the slow observer never stalled the publisher, and never cost the");
    println!("     critical consumer a single message. How much the observer sees is");
    println!("     purely a function of its own speed against the stream rate, and it");
    println!("     always knows: receive_ratio() reports exactly what it sampled.\n");
}

fn telemetry_ratio(seen: &AtomicU64, missed: &AtomicU64) -> f64 {
    let s = seen.load(Ordering::Relaxed) as f64;
    let m = missed.load(Ordering::Relaxed) as f64;
    if s + m == 0.0 {
        0.0
    } else {
        100.0 * s / (s + m)
    }
}

/// A registered consumer that panics. Its `Subscriber` is dropped as the thread
/// unwinds, which removes it from the backpressure set — so the publisher
/// resumes instead of waiting on a cursor that will never advance again.
fn dead_consumer() {
    println!("-- consumer dies mid-stream --");

    let (mut p, s) = channel_bounded::<u64>(RING, 0);
    let mut sub = s.subscribe(); // tracked: this one gates the publisher

    let consumer = std::thread::spawn(move || loop {
        if let Ok(v) = sub.try_recv() {
            if v >= 10 {
                panic!("consumer died");
            }
        }
    });

    let done = Arc::new(AtomicBool::new(false));
    let done_w = done.clone();
    let published = Arc::new(AtomicU64::new(0));
    let published_w = published.clone();

    let start = Instant::now();
    let writer = std::thread::spawn(move || {
        for i in 0..N {
            p.publish(i);
            published_w.store(i + 1, Ordering::Relaxed);
        }
        done_w.store(true, Ordering::Relaxed);
    });

    while !done.load(Ordering::Relaxed) && start.elapsed() < Duration::from_secs(10) {
        std::thread::yield_now();
    }
    let elapsed = start.elapsed();
    let ok = done.load(Ordering::Relaxed);
    let _ = consumer.join(); // panicked by design

    if ok {
        writer.join().unwrap();
        println!("  publisher    : {N} messages in {elapsed:.3?} — recovered");
        println!("  -> the dead consumer's slot in the backpressure set was released when");
        println!("     its Subscriber dropped, so the publisher was never wedged.");
    } else {
        println!(
            "  publisher    : STALLED at {} of {N}",
            published.load(Ordering::Relaxed)
        );
    }
}
