// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! Pipeline topology: publisher → stage1 → stage2 → final consumer
//!
//! Demonstrates chaining multiple Photon Ring channels to build
//! a multi-stage processing pipeline. Each stage reads from one
//! ring and publishes to the next.
//!
//! Run with: cargo run --release --example pipeline

use photon_ring::{channel, TryRecvError};
use std::thread;

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct RawTick {
    price: f64,
    volume: u32,
}

// SAFETY: RawTick is #[repr(C)] with all numeric fields;
// every bit pattern is a valid RawTick.
unsafe impl photon_ring::Pod for RawTick {}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
#[allow(dead_code)]
struct EnrichedTick {
    price: f64,
    volume: u32,
    vwap: f64,
}

// SAFETY: EnrichedTick is #[repr(C)] with all numeric fields;
// every bit pattern is a valid EnrichedTick.
unsafe impl photon_ring::Pod for EnrichedTick {}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct Signal {
    /// 0 = sell, 1 = buy (bool is NOT Pod)
    buy: u8,
    strength: f64,
}

// SAFETY: Signal is #[repr(C)] with all numeric fields;
// every bit pattern is a valid Signal.
unsafe impl photon_ring::Pod for Signal {}

fn main() {
    // Stage 0 → Stage 1: raw ticks
    let (mut pub0, subs0) = channel::<RawTick>(1024);
    // Stage 1 → Stage 2: enriched ticks
    let (mut pub1, subs1) = channel::<EnrichedTick>(1024);
    // Stage 2 → Final: signals
    let (mut pub2, subs2) = channel::<Signal>(1024);

    // Stage 1: enrich raw ticks with running VWAP
    let mut sub0 = subs0.subscribe();
    let stage1 = thread::spawn(move || {
        let mut vwap_sum = 0.0;
        let mut vol_sum = 0u64;
        loop {
            match sub0.try_recv() {
                Ok(tick) => {
                    vwap_sum += tick.price * tick.volume as f64;
                    vol_sum += tick.volume as u64;
                    let vwap = if vol_sum > 0 {
                        vwap_sum / vol_sum as f64
                    } else {
                        tick.price
                    };
                    pub1.publish(EnrichedTick {
                        price: tick.price,
                        volume: tick.volume,
                        vwap,
                    });
                }
                Err(TryRecvError::Empty) => core::hint::spin_loop(),
                Err(TryRecvError::Lagged { .. }) => {}
            }
        }
    });

    // Stage 2: generate trading signals from enriched ticks
    let mut sub1 = subs1.subscribe();
    let stage2 = thread::spawn(move || loop {
        match sub1.try_recv() {
            Ok(tick) => {
                let buy = if tick.price < tick.vwap { 1u8 } else { 0u8 };
                let strength = (tick.vwap - tick.price).abs() / tick.vwap;
                pub2.publish(Signal { buy, strength });
            }
            Err(TryRecvError::Empty) => core::hint::spin_loop(),
            Err(TryRecvError::Lagged { .. }) => {}
        }
    });

    // Final consumer: collect and print signals
    let mut sub2 = subs2.subscribe();
    let consumer = thread::spawn(move || {
        let mut count = 0u64;
        loop {
            match sub2.try_recv() {
                Ok(sig) => {
                    count += 1;
                    if count <= 5 {
                        println!(
                            "Signal #{count}: buy={}, strength={:.4}",
                            sig.buy, sig.strength
                        );
                    }
                    if count == 10_000 {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => core::hint::spin_loop(),
                Err(TryRecvError::Lagged { .. }) => {}
            }
        }
        count
    });

    // Publisher: send raw ticks
    for i in 0..10_000u32 {
        pub0.publish(RawTick {
            price: 100.0 + (i as f64 * 0.01),
            volume: 100 + (i % 50),
        });
    }

    let received = consumer.join().unwrap();
    println!("\nPipeline: 10,000 ticks -> enrich -> signal -> {received} signals received");

    // Note: stage1 and stage2 threads are still running. In production,
    // use a shutdown signal (AtomicBool) to stop them gracefully.
    drop(stage1);
    drop(stage2);
}
