// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Event ring (in-place, no seqlock) against the Pod ring (copy + seqlock).
use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

macro_rules! sized {
    ($name:ident, $n:expr) => {
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct $name([u64; $n / 8]);
        // Arrays only derive Default up to 32 elements; the large payloads need it.
        impl Default for $name {
            fn default() -> Self {
                $name([0; $n / 8])
            }
        }
        unsafe impl photon_ring::Pod for $name {}
    };
}
sized!(P8, 8);
sized!(P64, 64);
sized!(P1K, 1024);
sized!(P4K, 4096);

macro_rules! pair {
    ($c:expr, $t:ty, $label:literal) => {{
        let n = core::mem::size_of::<$t>();
        $c.bench_function(&format!("pod ring   {} ({}B)", $label, n), |b| {
            let (mut p, s) = photon_ring::channel_bounded::<$t>(1024, 0);
            let mut sub = s.subscribe();
            b.iter(|| {
                p.publish(black_box(<$t>::default()));
                black_box(sub.try_recv().unwrap().0[0]);
            });
        });
        // Touching one field: the case where a large message is mostly stable
        // and the publisher updates a little of it.
        $c.bench_function(&format!("event ring {} ({}B) 1 field", $label, n), |b| {
            let (mut tx, rx) = photon_ring::event_channel(1024, <$t>::default);
            let mut sub = rx.subscribe();
            b.iter(|| {
                tx.publish(|v| v.0[0] = black_box(1));
                black_box(sub.process(|v| v.0[0]).unwrap());
            });
        });
        // Rewriting the whole payload: the fair comparison against a ring that
        // must copy the entire value in and out regardless.
        $c.bench_function(&format!("event ring {} ({}B) full write", $label, n), |b| {
            let (mut tx, rx) = photon_ring::event_channel(1024, <$t>::default);
            let mut sub = rx.subscribe();
            b.iter(|| {
                tx.publish(|v| {
                    for w in v.0.iter_mut() {
                        *w = black_box(1);
                    }
                });
                black_box(
                    sub.process(|v| v.0.iter().fold(0u64, |a, &w| a ^ w))
                        .unwrap(),
                );
            });
        });
    }};
}

fn rings(c: &mut Criterion) {
    pair!(c, P8, "8B");
    pair!(c, P64, "64B");
    pair!(c, P1K, "1K");
    pair!(c, P4K, "4K");
}

criterion_group!(benches, rings);
criterion_main!(benches);
