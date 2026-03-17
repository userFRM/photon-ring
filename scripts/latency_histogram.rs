// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! Collects cross-thread roundtrip latency samples for:
//! - Photon Ring
//! - disruptor-rs v4.0.0
//! - crossbeam-channel
//!
//! Outputs CSV for histogram generation.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

const WARMUP: u64 = 1_000_000;
const SAMPLES: u64 = 100_000_000;

fn measure_photon() -> Vec<u64> {
    eprintln!("Photon Ring: collecting {} samples...", SAMPLES);
    let (mut p, s) = photon_ring::channel::<u64>(4096);
    let mut sub = s.subscribe();
    let done = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(AtomicU64::new(u64::MAX));
    let d2 = done.clone();
    let s2 = seen.clone();

    let reader = thread::spawn(move || {
        while !d2.load(Ordering::Relaxed) {
            if let Ok(v) = sub.try_recv() {
                s2.store(v, Ordering::Release);
            }
        }
    });

    for i in 0..WARMUP {
        p.publish(i);
        while seen.load(Ordering::Acquire) != i { core::hint::spin_loop(); }
    }

    let mut lat = Vec::with_capacity(SAMPLES as usize);
    for i in WARMUP..(WARMUP + SAMPLES) {
        let t = Instant::now();
        p.publish(i);
        while seen.load(Ordering::Acquire) != i { core::hint::spin_loop(); }
        lat.push(t.elapsed().as_nanos() as u64);
    }

    done.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    lat
}

fn measure_crossbeam() -> Vec<u64> {
    eprintln!("crossbeam-channel: collecting {} samples...", SAMPLES);
    let (tx, rx) = crossbeam_channel::bounded::<u64>(4096);
    let done = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(AtomicU64::new(u64::MAX));
    let d2 = done.clone();
    let s2 = seen.clone();

    let reader = thread::spawn(move || {
        while !d2.load(Ordering::Relaxed) {
            if let Ok(v) = rx.try_recv() {
                s2.store(v, Ordering::Release);
            }
        }
    });

    for i in 0..WARMUP {
        tx.send(i).unwrap();
        while seen.load(Ordering::Acquire) != i { core::hint::spin_loop(); }
    }

    let mut lat = Vec::with_capacity(SAMPLES as usize);
    for i in WARMUP..(WARMUP + SAMPLES) {
        let t = Instant::now();
        tx.send(i).unwrap();
        while seen.load(Ordering::Acquire) != i { core::hint::spin_loop(); }
        lat.push(t.elapsed().as_nanos() as u64);
    }

    done.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    lat
}

fn measure_disruptor() -> Vec<u64> {
    use disruptor::*;

    eprintln!("disruptor-rs: collecting {} samples...", SAMPLES);
    let counter = Arc::new(AtomicU64::new(u64::MAX));
    let c2 = counter.clone();
    let mut producer = build_single_producer(4096, || 0u64, BusySpin)
        .handle_events_with(move |e: &u64, _, _| {
            c2.store(*e, Ordering::Release);
        })
        .build();

    for i in 0..WARMUP {
        producer.publish(|slot| { *slot = i; });
        while counter.load(Ordering::Acquire) != i { core::hint::spin_loop(); }
    }

    let mut lat = Vec::with_capacity(SAMPLES as usize);
    for i in WARMUP..(WARMUP + SAMPLES) {
        let t = Instant::now();
        producer.publish(|slot| { *slot = i; });
        while counter.load(Ordering::Acquire) != i { core::hint::spin_loop(); }
        lat.push(t.elapsed().as_nanos() as u64);
    }

    lat
}

fn to_histogram(data: &[u64], buckets: &[u64]) -> Vec<u64> {
    let mut counts = vec![0u64; buckets.len()];
    for &v in data {
        let mut placed = false;
        for i in (0..buckets.len()).rev() {
            if v >= buckets[i] {
                counts[i] += 1;
                placed = true;
                break;
            }
        }
        if !placed { counts[0] += 1; }
    }
    counts
}

fn main() {
    let photon = measure_photon();
    let crossbeam = measure_crossbeam();
    let disruptor = measure_disruptor();

    let buckets: Vec<u64> = vec![
        0, 16, 32, 64, 96, 128, 192, 256, 384, 512,
        1024, 2048, 4096, 8192, 16384, 32768, 65536,
        131072, 262144, 524288, 1048576, 4194304, 8388608,
    ];

    let ph = to_histogram(&photon, &buckets);
    let ch = to_histogram(&crossbeam, &buckets);
    let dh = to_histogram(&disruptor, &buckets);

    println!("bucket_start,photon_ring,crossbeam,disruptor");
    for (i, b) in buckets.iter().enumerate() {
        if ph[i] > 0 || ch[i] > 0 || dh[i] > 0 {
            println!("{},{},{},{}", b, ph[i], ch[i], dh[i]);
        }
    }

    let stats = |name: &str, d: &mut Vec<u64>| {
        d.sort_unstable();
        let n = d.len();
        eprintln!("{}: p50={} p99={} p999={} min={} max={}",
            name, d[n/2], d[n*99/100], d[n*999/1000], d[0], d[n-1]);
    };
    let mut p = photon; let mut c = crossbeam; let mut d = disruptor;
    stats("Photon Ring", &mut p);
    stats("crossbeam  ", &mut c);
    stats("disruptor  ", &mut d);
}
