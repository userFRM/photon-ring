// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Core-pinned cross-thread latency measurement.
//!
//! Demonstrates the HFT-recommended setup:
//! 1. Pin publisher to core 0, subscriber to core 1
//! 2. Use BusySpin wait strategy for minimum wakeup latency
//! 3. Measure one-way latency with high-resolution timestamps
//!
//! Uses a bounded channel so the publisher cannot outrun the subscriber,
//! ensuring every message is received and measured.
//!
//! Run with: cargo run --release --example pinned_latency

use photon_ring::affinity;
use photon_ring::{channel_bounded, PublishError, WaitStrategy};
use std::thread;
use std::time::Instant;

const NUM_MESSAGES: u64 = 100_000;

/// Ring capacity — small to keep the pipeline tight.
/// Bounded backpressure ensures zero message loss.
const RING_CAPACITY: usize = 64;

/// A timestamped message for latency measurement.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct TimedMsg {
    seq: u64,
    send_ns: u64,
}

fn main() {
    let cores = affinity::available_cores();
    let core_count = cores.len();
    println!("Available cores: {core_count}");

    if core_count < 2 {
        eprintln!("WARNING: Need at least 2 cores for meaningful pinned latency measurement.");
        eprintln!(
            "         Running without core isolation — results will include scheduler jitter."
        );
    }

    let pub_core = 0;
    let sub_core = if core_count >= 2 { 1 } else { 0 };
    println!("Publisher pinned to core {pub_core}");
    println!("Subscriber pinned to core {sub_core}");
    println!("Messages: {NUM_MESSAGES}");
    println!("Wait strategy: BusySpin");
    println!("Ring: bounded({RING_CAPACITY}, watermark=0)");
    println!();

    // Bounded channel — publisher blocks when ring is full, guaranteeing
    // the subscriber sees every single message (no lag, no drops).
    let (mut publisher, subscribable) = channel_bounded::<TimedMsg>(RING_CAPACITY, 0);
    let mut subscriber = subscribable.subscribe();

    // Reference instant for nanosecond timestamps.
    let epoch = Instant::now();

    // --- Subscriber thread: core 1 ---
    let sub_epoch = epoch;
    let sub_handle = thread::spawn(move || {
        if !affinity::pin_to_core_id(sub_core) {
            eprintln!("WARNING: Failed to pin subscriber to core {sub_core}");
        }

        let mut latencies = Vec::with_capacity(NUM_MESSAGES as usize);

        for _ in 0..NUM_MESSAGES {
            let msg = subscriber.recv_with(WaitStrategy::BusySpin);
            let recv_ns = sub_epoch.elapsed().as_nanos() as u64;
            let latency_ns = recv_ns.saturating_sub(msg.send_ns);
            latencies.push(latency_ns);
        }

        latencies
    });

    // --- Publisher thread: core 0 ---
    let pub_epoch = epoch;
    let pub_handle = thread::spawn(move || {
        if !affinity::pin_to_core_id(pub_core) {
            eprintln!("WARNING: Failed to pin publisher to core {pub_core}");
        }

        for seq in 0..NUM_MESSAGES {
            let send_ns = pub_epoch.elapsed().as_nanos() as u64;
            let msg = TimedMsg { seq, send_ns };

            // Retry on backpressure — spin until a slot opens.
            loop {
                match publisher.try_publish(msg) {
                    Ok(()) => break,
                    Err(PublishError::Full(_)) => core::hint::spin_loop(),
                }
            }
        }
    });

    pub_handle.join().expect("publisher thread panicked");
    let mut latencies = sub_handle.join().expect("subscriber thread panicked");

    // --- Compute percentile stats ---
    latencies.sort_unstable();
    let len = latencies.len();

    let p50 = latencies[len / 2];
    let p99 = latencies[len * 99 / 100];
    let p999 = latencies[len * 999 / 1000];
    let max = latencies[len - 1];
    let min = latencies[0];
    let avg = latencies.iter().sum::<u64>() / len as u64;

    println!("--- One-way latency ({NUM_MESSAGES} messages) ---");
    println!("  min:   {min:>8} ns");
    println!("  avg:   {avg:>8} ns");
    println!("  p50:   {p50:>8} ns");
    println!("  p99:   {p99:>8} ns");
    println!("  p99.9: {p999:>8} ns");
    println!("  max:   {max:>8} ns");
}
