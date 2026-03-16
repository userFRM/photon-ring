// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

// ---------------------------------------------------------------------------
// Photon benchmarks
// ---------------------------------------------------------------------------

fn publish_single(c: &mut Criterion) {
    c.bench_function("photon: publish only", |b| {
        let (mut p, _s) = photon_ring::channel::<u64>(4096);
        let mut i = 0u64;
        b.iter(|| {
            p.publish(black_box(i));
            i = i.wrapping_add(1);
        });
    });
}

fn publish_recv_roundtrip(c: &mut Criterion) {
    c.bench_function("photon: publish+recv 1 sub", |b| {
        let (mut p, s) = photon_ring::channel::<u64>(4096);
        let mut sub = s.subscribe();
        let mut i = 0u64;
        b.iter(|| {
            p.publish(i);
            let v = sub.try_recv().unwrap();
            black_box(v);
            i = i.wrapping_add(1);
        });
    });
}

fn fanout(c: &mut Criterion) {
    for n in [1, 2, 5, 10] {
        c.bench_function(&format!("photon: fanout {n} subs"), |b| {
            let (mut p, s) = photon_ring::channel::<u64>(4096);
            let mut subs: Vec<_> = (0..n).map(|_| s.subscribe()).collect();
            let mut i = 0u64;
            b.iter(|| {
                p.publish(i);
                for sub in subs.iter_mut() {
                    black_box(sub.try_recv().unwrap());
                }
                i = i.wrapping_add(1);
            });
        });
    }
}

fn fanout_group(c: &mut Criterion) {
    for n in [1, 2, 5, 10] {
        c.bench_function(&format!("photon: group fanout {n} subs"), |b| match n {
            1 => {
                let (mut p, s) = photon_ring::channel::<u64>(4096);
                let mut g = s.subscribe_group::<1>();
                let mut i = 0u64;
                b.iter(|| {
                    p.publish(i);
                    black_box(g.try_recv().unwrap());
                    i = i.wrapping_add(1);
                });
            }
            2 => {
                let (mut p, s) = photon_ring::channel::<u64>(4096);
                let mut g = s.subscribe_group::<2>();
                let mut i = 0u64;
                b.iter(|| {
                    p.publish(i);
                    black_box(g.try_recv().unwrap());
                    i = i.wrapping_add(1);
                });
            }
            5 => {
                let (mut p, s) = photon_ring::channel::<u64>(4096);
                let mut g = s.subscribe_group::<5>();
                let mut i = 0u64;
                b.iter(|| {
                    p.publish(i);
                    black_box(g.try_recv().unwrap());
                    i = i.wrapping_add(1);
                });
            }
            10 => {
                let (mut p, s) = photon_ring::channel::<u64>(4096);
                let mut g = s.subscribe_group::<10>();
                let mut i = 0u64;
                b.iter(|| {
                    p.publish(i);
                    black_box(g.try_recv().unwrap());
                    i = i.wrapping_add(1);
                });
            }
            _ => unreachable!(),
        });
    }
}

fn try_recv_empty(c: &mut Criterion) {
    c.bench_function("photon: try_recv (empty)", |b| {
        let (_p, s) = photon_ring::channel::<u64>(64);
        let mut sub = s.subscribe();
        b.iter(|| {
            let _ = black_box(sub.try_recv());
        });
    });
}

fn latest_skip(c: &mut Criterion) {
    c.bench_function("photon: latest (skip to newest)", |b| {
        let (mut p, s) = photon_ring::channel::<u64>(4096);
        let mut sub = s.subscribe();
        let mut i = 0u64;
        b.iter(|| {
            for _ in 0..10 {
                p.publish(i);
                i = i.wrapping_add(1);
            }
            black_box(sub.latest());
        });
    });
}

fn batch_publish_recv(c: &mut Criterion) {
    c.bench_function("photon: batch 64 + drain", |b| {
        let (mut p, s) = photon_ring::channel::<u64>(4096);
        let mut sub = s.subscribe();
        let batch: Vec<u64> = (0..64).collect();
        b.iter(|| {
            p.publish_batch(&batch);
            for _ in 0..64 {
                black_box(sub.try_recv().unwrap());
            }
        });
    });
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
struct Quote {
    price: f64,
    volume: u64,
    ts: u64,
}

fn struct_roundtrip(c: &mut Criterion) {
    c.bench_function("photon: struct roundtrip (24B)", |b| {
        let (mut p, s) = photon_ring::channel::<Quote>(4096);
        let mut sub = s.subscribe();
        let mut i = 0u64;
        b.iter(|| {
            p.publish(Quote {
                price: 1.0,
                volume: i,
                ts: i,
            });
            black_box(sub.try_recv().unwrap());
            i = i.wrapping_add(1);
        });
    });
}

// ---------------------------------------------------------------------------
// Disruptor comparison benchmarks (apple-to-apple single-threaded roundtrip)
// ---------------------------------------------------------------------------

fn disruptor_publish_only(c: &mut Criterion) {
    use disruptor::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    c.bench_function("disruptor: publish only", |b| {
        let counter = std::sync::Arc::new(AtomicU64::new(0));
        let c_ref = counter.clone();
        let mut producer = build_single_producer(4096, || 0u64, BusySpin)
            .handle_events_with(move |e: &u64, _, _| {
                c_ref.store(*e, Ordering::Relaxed);
            })
            .build();

        let mut i = 0u64;
        b.iter(|| {
            producer.publish(|slot| {
                *slot = black_box(i);
            });
            i = i.wrapping_add(1);
        });
    });
}

fn disruptor_roundtrip(c: &mut Criterion) {
    use disruptor::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    c.bench_function("disruptor: publish+recv 1 consumer", |b| {
        let counter = std::sync::Arc::new(AtomicU64::new(0));
        let c2 = counter.clone();
        let mut producer = build_single_producer(4096, || 0u64, BusySpin)
            .handle_events_with(move |e: &u64, _, _| {
                c2.store(*e, Ordering::Release);
            })
            .build();

        let mut i = 0u64;
        b.iter(|| {
            producer.publish(|slot| {
                *slot = i;
            });
            // Spin until consumer processes the event
            while counter.load(Ordering::Acquire) != i {
                core::hint::spin_loop();
            }
            black_box(i);
            i = i.wrapping_add(1);
        });
    });
}

// ---------------------------------------------------------------------------
// Cross-thread latency (Photon)
// ---------------------------------------------------------------------------

fn cross_thread_latency(c: &mut Criterion) {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    c.bench_function("photon: cross-thread latency", |b| {
        let (mut p, s) = photon_ring::channel::<u64>(4096);
        let mut sub = s.subscribe();

        let done = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(AtomicU64::new(u64::MAX));
        let done2 = done.clone();
        let seen2 = seen.clone();

        let reader = std::thread::spawn(move || {
            while !done2.load(Ordering::Relaxed) {
                if let Ok(v) = sub.try_recv() {
                    seen2.store(v, Ordering::Release);
                }
                core::hint::spin_loop();
            }
        });

        let mut i = 0u64;
        b.iter(|| {
            p.publish(i);
            while seen.load(Ordering::Acquire) != i {
                core::hint::spin_loop();
            }
            i = i.wrapping_add(1);
        });

        done.store(true, Ordering::Relaxed);
        reader.join().unwrap();
    });
}

criterion_group!(
    benches,
    publish_single,
    publish_recv_roundtrip,
    fanout,
    fanout_group,
    try_recv_empty,
    latest_skip,
    batch_publish_recv,
    struct_roundtrip,
    disruptor_publish_only,
    disruptor_roundtrip,
    cross_thread_latency,
);
criterion_main!(benches);
