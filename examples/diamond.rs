// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! Diamond topology: publisher → {filter_a, filter_b} → aggregator
//!
//! Publisher fans out to two filter stages (via two subscribers on the
//! same ring). Each filter publishes to its own output ring. The
//! aggregator reads from both output rings and merges the results.
//!
//! ```text
//!              ┌──── filter_a (large orders) ──── ring_a ────┐
//!  publisher ──┤                                              ├── aggregator
//!              └──── filter_b (small orders) ──── ring_b ────┘
//! ```
//!
//! Since Photon Ring is SPMC (single-producer), fan-in to a single ring
//! would require multiple producers — which is not supported. Instead,
//! each filter writes to its own ring and the aggregator polls both.
//!
//! Run with: cargo run --release --example diamond

use photon_ring::{channel, TryRecvError};
use std::thread;

const NUM_ORDERS: u32 = 20_000;
const LARGE_ORDER_THRESHOLD: u32 = 500;

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct Order {
    id: u32,
    price: f64,
    quantity: u32,
}

// SAFETY: Order is #[repr(C)] with all numeric fields;
// every bit pattern is a valid Order.
unsafe impl photon_ring::Pod for Order {}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
#[allow(dead_code)]
struct TaggedOrder {
    id: u32,
    price: f64,
    quantity: u32,
    /// 0 = Large, 1 = Small (enums are NOT Pod)
    tag: u32,
}

// SAFETY: TaggedOrder is #[repr(C)] with all numeric fields;
// every bit pattern is a valid TaggedOrder.
unsafe impl photon_ring::Pod for TaggedOrder {}

const TAG_LARGE: u32 = 0;
const TAG_SMALL: u32 = 1;

fn main() {
    // Source ring: publisher → {filter_a, filter_b}
    let (mut publisher, source_subs) = channel::<Order>(2048);

    // Output rings: filter_a → aggregator, filter_b → aggregator
    let (mut pub_a, subs_a) = channel::<TaggedOrder>(1024);
    let (mut pub_b, subs_b) = channel::<TaggedOrder>(1024);

    // Filter A: passes only large orders (quantity >= threshold)
    let mut sub_a = source_subs.subscribe();
    let filter_a = thread::spawn(move || loop {
        match sub_a.try_recv() {
            Ok(order) => {
                if order.quantity >= LARGE_ORDER_THRESHOLD {
                    pub_a.publish(TaggedOrder {
                        id: order.id,
                        price: order.price,
                        quantity: order.quantity,
                        tag: TAG_LARGE,
                    });
                }
            }
            Err(TryRecvError::Empty) => core::hint::spin_loop(),
            Err(TryRecvError::Lagged { .. }) => {}
        }
    });

    // Filter B: passes only small orders (quantity < threshold)
    let mut sub_b = source_subs.subscribe();
    let filter_b = thread::spawn(move || loop {
        match sub_b.try_recv() {
            Ok(order) => {
                if order.quantity < LARGE_ORDER_THRESHOLD {
                    pub_b.publish(TaggedOrder {
                        id: order.id,
                        price: order.price,
                        quantity: order.quantity,
                        tag: TAG_SMALL,
                    });
                }
            }
            Err(TryRecvError::Empty) => core::hint::spin_loop(),
            Err(TryRecvError::Lagged { .. }) => {}
        }
    });

    // Aggregator: reads from both output rings, merges results
    let mut agg_sub_a = subs_a.subscribe();
    let mut agg_sub_b = subs_b.subscribe();
    let aggregator = thread::spawn(move || {
        let mut large_count = 0u64;
        let mut small_count = 0u64;
        let mut large_notional = 0.0f64;
        let mut small_notional = 0.0f64;
        let target = NUM_ORDERS as u64;

        loop {
            // Poll ring A (large orders)
            match agg_sub_a.try_recv() {
                Ok(order) => {
                    large_count += 1;
                    large_notional += order.price * order.quantity as f64;
                    if large_count <= 3 {
                        println!(
                            "  [aggregator] large order #{}: qty={}, price={:.2}",
                            order.id, order.quantity, order.price
                        );
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Lagged { .. }) => {}
            }

            // Poll ring B (small orders)
            match agg_sub_b.try_recv() {
                Ok(order) => {
                    small_count += 1;
                    small_notional += order.price * order.quantity as f64;
                    if small_count <= 3 {
                        println!(
                            "  [aggregator] small order #{}: qty={}, price={:.2}",
                            order.id, order.quantity, order.price
                        );
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Lagged { .. }) => {}
            }

            if large_count + small_count >= target {
                break;
            }

            // Yield when both rings are empty to avoid burning CPU
            if large_count + small_count > 0 {
                // Active processing — tight spin
            } else {
                core::hint::spin_loop();
            }
        }

        (large_count, small_count, large_notional, small_notional)
    });

    // Publisher: send orders with varying quantities
    println!(
        "Diamond topology: {NUM_ORDERS} orders -> {{large_filter, small_filter}} -> aggregator\n"
    );
    for i in 0..NUM_ORDERS {
        publisher.publish(Order {
            id: i,
            price: 50.0 + (i as f64 * 0.005),
            quantity: 100 + (i % 1000), // ranges from 100 to 1099
        });
    }

    let (large, small, large_val, small_val) = aggregator.join().unwrap();
    println!();
    println!("--- Results ---");
    println!("  Total published:  {NUM_ORDERS}");
    println!("  Large orders:     {large} (notional: ${large_val:.2})");
    println!("  Small orders:     {small} (notional: ${small_val:.2})");
    println!("  Accounted for:    {} / {NUM_ORDERS}", large + small);

    // Note: filter threads are still running. In production,
    // use a shutdown signal (AtomicBool) to stop them gracefully.
    drop(filter_a);
    drop(filter_b);
}
