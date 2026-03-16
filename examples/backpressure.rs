// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Backpressure example: reliable order fill pipeline.
//!
//! Demonstrates channel_bounded() for scenarios where message loss
//! is unacceptable (e.g., order management systems).
//!
//! The publisher sends 10,000 "order fill" messages. The consumer
//! processes each one with a simulated 1us delay. Backpressure
//! prevents the publisher from overwriting unprocessed fills.
//!
//! Run with: cargo run --release --example backpressure

use photon_ring::{channel_bounded, PublishError, TryRecvError};
use std::thread;
use std::time::Instant;

const NUM_FILLS: u64 = 10_000;
const RING_SIZE: usize = 256;

/// An order fill event — must be `Copy` for Photon Ring.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct Fill {
    order_id: u64,
    price: f64,
    quantity: u32,
    side: Side,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum Side {
    Buy,
    Sell,
}

fn main() {
    println!("Backpressure demo: reliable order fill pipeline");
    println!("  Ring size:   {RING_SIZE} slots");
    println!("  Fills:       {NUM_FILLS}");
    println!("  Watermark:   0 (block as soon as ring is full)");
    println!();

    let (mut publisher, subscribable) = channel_bounded::<Fill>(RING_SIZE, 0);
    let mut subscriber = subscribable.subscribe();

    let start = Instant::now();

    // --- Consumer thread ---
    let consumer = thread::spawn(move || {
        let mut received: u64 = 0;
        let mut total_value = 0.0f64;

        loop {
            match subscriber.try_recv() {
                Ok(fill) => {
                    // Validate ordering: fills must arrive in sequence.
                    assert_eq!(
                        fill.order_id, received,
                        "out-of-order fill: expected {received}, got {}",
                        fill.order_id
                    );
                    total_value += fill.price * fill.quantity as f64;
                    received += 1;

                    // Simulate 1us processing delay per fill.
                    busy_wait_ns(1_000);

                    if received == NUM_FILLS {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => {
                    core::hint::spin_loop();
                }
                Err(TryRecvError::Lagged { skipped }) => {
                    // This should NEVER happen with a bounded channel.
                    panic!("unexpected lag: {skipped} fills lost — bounded channel violated");
                }
            }
        }

        (received, total_value)
    });

    // --- Publisher: send fills as fast as possible ---
    let mut published: u64 = 0;
    let mut backpressure_count: u64 = 0;

    while published < NUM_FILLS {
        let fill = Fill {
            order_id: published,
            price: 100.0 + (published as f64) * 0.01,
            quantity: 100 + (published % 500) as u32,
            side: if published % 2 == 0 {
                Side::Buy
            } else {
                Side::Sell
            },
        };

        match publisher.try_publish(fill) {
            Ok(()) => {
                published += 1;
            }
            Err(PublishError::Full(_)) => {
                // Backpressure: ring is full, consumer hasn't caught up.
                // Retry after yielding.
                backpressure_count += 1;
                core::hint::spin_loop();
            }
        }
    }

    let (received, total_value) = consumer.join().expect("consumer thread panicked");
    let elapsed = start.elapsed();

    // --- Verify zero-loss delivery ---
    assert_eq!(
        published, received,
        "MISMATCH: published {published} but received {received}"
    );

    println!("--- Results ---");
    println!("  Published:          {published}");
    println!("  Received:           {received}");
    println!("  Dropped:            0 (guaranteed by backpressure)");
    println!("  Backpressure stalls:{backpressure_count:>8}");
    println!("  Total notional:     ${total_value:>14.2}");
    println!("  Elapsed:            {elapsed:.2?}");
    println!(
        "  Throughput:         {:.1}K fills/s",
        published as f64 / elapsed.as_secs_f64() / 1_000.0
    );
    println!();
    println!("All {published} fills delivered. Zero drops.");
}

/// Busy-wait for approximately `ns` nanoseconds.
/// More precise than thread::sleep for sub-microsecond delays.
#[inline(never)]
fn busy_wait_ns(ns: u64) {
    let start = Instant::now();
    let target = std::time::Duration::from_nanos(ns);
    while start.elapsed() < target {
        core::hint::spin_loop();
    }
}
