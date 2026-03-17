// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! Payload-size scaling benchmark.
//!
//! Measures publish + try_recv roundtrip latency for payloads from 8 B to 4 KiB
//! to reveal the crossover point where memcpy cost overtakes cache-coherence
//! overhead.

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

// ---------------------------------------------------------------------------
// Generic payload wrapper
// ---------------------------------------------------------------------------

/// Fixed-size payload aligned to 8 bytes so it packs predictably into `Slot`.
#[derive(Clone, Copy)]
#[repr(C)]
struct Pad<const N: usize> {
    data: [u8; N],
}

// SAFETY: Pad<N> is #[repr(C)] containing only [u8; N];
// every bit pattern is valid.
unsafe impl<const N: usize> photon_ring::Pod for Pad<N> {}

// ---------------------------------------------------------------------------
// Helper: single-thread roundtrip for a given payload size
// ---------------------------------------------------------------------------

macro_rules! bench_roundtrip {
    ($group:expr, $n:literal) => {{
        $group.bench_function(concat!(stringify!($n), "B roundtrip"), |b| {
            let (mut p, s) = photon_ring::channel::<Pad<$n>>(4096);
            let mut sub = s.subscribe();
            b.iter(|| {
                p.publish(Pad { data: [42u8; $n] });
                black_box(sub.try_recv().unwrap());
            });
        });
    }};
}

// ---------------------------------------------------------------------------
// Helper: cross-thread roundtrip for a given payload size
// ---------------------------------------------------------------------------

macro_rules! bench_cross_thread {
    ($group:expr, $n:literal) => {{
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        $group.bench_function(concat!(stringify!($n), "B cross-thread"), |b| {
            let (mut p, s) = photon_ring::channel::<Pad<$n>>(4096);
            let mut sub = s.subscribe();

            let done = Arc::new(AtomicBool::new(false));
            let received = Arc::new(AtomicBool::new(false));
            let done2 = done.clone();
            let received2 = received.clone();

            let reader = std::thread::spawn(move || {
                while !done2.load(Ordering::Relaxed) {
                    if sub.try_recv().is_ok() {
                        received2.store(true, Ordering::Release);
                    }
                    core::hint::spin_loop();
                }
            });

            b.iter(|| {
                received.store(false, Ordering::Relaxed);
                p.publish(Pad { data: [42u8; $n] });
                while !received.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }
            });

            done.store(true, Ordering::Relaxed);
            reader.join().unwrap();
        });
    }};
}

// ---------------------------------------------------------------------------
// Benchmark group
// ---------------------------------------------------------------------------

fn payload_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("payload_scaling");

    // Single-thread roundtrip: publish + try_recv
    bench_roundtrip!(group, 8);
    bench_roundtrip!(group, 16);
    bench_roundtrip!(group, 32);
    bench_roundtrip!(group, 64);
    bench_roundtrip!(group, 128);
    bench_roundtrip!(group, 256);
    bench_roundtrip!(group, 512);
    bench_roundtrip!(group, 1024);
    bench_roundtrip!(group, 2048);
    bench_roundtrip!(group, 4096);

    group.finish();
}

fn payload_scaling_cross_thread(c: &mut Criterion) {
    let mut group = c.benchmark_group("payload_scaling_xthread");

    bench_cross_thread!(group, 8);
    bench_cross_thread!(group, 16);
    bench_cross_thread!(group, 32);
    bench_cross_thread!(group, 64);
    bench_cross_thread!(group, 128);
    bench_cross_thread!(group, 256);
    bench_cross_thread!(group, 512);
    bench_cross_thread!(group, 1024);
    bench_cross_thread!(group, 2048);
    bench_cross_thread!(group, 4096);

    group.finish();
}

criterion_group!(benches, payload_scaling, payload_scaling_cross_thread);
criterion_main!(benches);
