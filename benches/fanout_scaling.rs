// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! How the cost of delivering one message to N consumers scales with N.
//!
//! This is the measurement that distinguishes broadcast designs. Delivering a
//! message to N consumers is O(N) work somewhere; the question is *where*, and
//! whether the producer pays it.
//!
//! Each case does the same logical work: publish one message, then have all N
//! consumers observe it. Single-threaded and in-cache, so what is measured is
//! protocol cost rather than scheduling.
//!
//! - **photon**: one write, N independent cursor reads. Subscribers share no
//!   state, so the producer's cost should not depend on N.
//! - **tokio::sync::broadcast**: a shared ring; the value is cloned per receiver.
//! - **point-to-point queues**: not broadcast. To fan out, the producer sends
//!   once per consumer, so the producer's own cost grows with N. Included to
//!   show the shape of that trade, not as a like-for-like rival — they solve
//!   the "exactly one receiver owns each message" problem instead.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;

const SUBS: [usize; 6] = [1, 2, 4, 8, 16, 32];
const RING: usize = 4096;

fn fanout(c: &mut Criterion) {
    let mut g = c.benchmark_group("deliver 1 message to N consumers");

    for &n in SUBS.iter() {
        g.throughput(Throughput::Elements(n as u64));

        g.bench_with_input(BenchmarkId::new("photon", n), &n, |b, &n| {
            let (mut p, s) = photon_ring::channel::<u64>(RING);
            let mut subs: Vec<_> = (0..n).map(|_| s.subscribe()).collect();
            let mut i = 0u64;
            b.iter(|| {
                p.publish(black_box(i));
                for sub in subs.iter_mut() {
                    black_box(sub.try_recv().unwrap());
                }
                i = i.wrapping_add(1);
            });
        });

        g.bench_with_input(
            BenchmarkId::new("tokio::sync::broadcast", n),
            &n,
            |b, &n| {
                let (tx, _) = tokio::sync::broadcast::channel::<u64>(RING);
                let mut rxs: Vec<_> = (0..n).map(|_| tx.subscribe()).collect();
                let mut i = 0u64;
                b.iter(|| {
                    tx.send(black_box(i)).unwrap();
                    for rx in rxs.iter_mut() {
                        black_box(rx.try_recv().unwrap());
                    }
                    i = i.wrapping_add(1);
                });
            },
        );

        g.bench_with_input(
            BenchmarkId::new("crossbeam-channel (one per consumer)", n),
            &n,
            |b, &n| {
                let pairs: Vec<_> = (0..n)
                    .map(|_| crossbeam_channel::bounded::<u64>(RING))
                    .collect();
                let mut i = 0u64;
                b.iter(|| {
                    for (tx, _) in pairs.iter() {
                        tx.send(black_box(i)).unwrap();
                    }
                    for (_, rx) in pairs.iter() {
                        black_box(rx.try_recv().unwrap());
                    }
                    i = i.wrapping_add(1);
                });
            },
        );

        g.bench_with_input(
            BenchmarkId::new("flume (one per consumer)", n),
            &n,
            |b, &n| {
                let pairs: Vec<_> = (0..n).map(|_| flume::bounded::<u64>(RING)).collect();
                let mut i = 0u64;
                b.iter(|| {
                    for (tx, _) in pairs.iter() {
                        tx.send(black_box(i)).unwrap();
                    }
                    for (_, rx) in pairs.iter() {
                        black_box(rx.try_recv().unwrap());
                    }
                    i = i.wrapping_add(1);
                });
            },
        );
    }
    g.finish();
}

criterion_group!(benches, fanout);
criterion_main!(benches);
