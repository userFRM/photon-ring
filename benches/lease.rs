// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Zero-copy lease vs copying try_recv, across payload sizes.
use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

macro_rules! sized {
    ($name:ident, $n:expr) => {
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct $name([u64; $n / 8]);
        unsafe impl photon_ring::Pod for $name {}
    };
}
sized!(P8, 8);
sized!(P64, 64);
sized!(P256, 256);
sized!(P1K, 1024);
sized!(P4K, 4096);

macro_rules! bench_pair {
    ($c:expr, $t:ty, $label:literal) => {{
        let n = core::mem::size_of::<$t>();
        $c.bench_function(&format!("copy try_recv {} ({}B)", $label, n), |b| {
            let (mut p, s) = photon_ring::channel_bounded::<$t>(1024, 0);
            let mut sub = s.subscribe();
            b.iter(|| {
                p.publish(black_box(unsafe { core::mem::zeroed() }));
                let v = sub.try_recv().unwrap();
                black_box(v.0[0]);
            });
        });
        $c.bench_function(&format!("process      {} ({}B)", $label, n), |b| {
            let (mut p, s) = photon_ring::channel_bounded::<$t>(1024, 0);
            let mut sub = s.subscribe();
            b.iter(|| {
                p.publish(black_box(unsafe { core::mem::zeroed() }));
                let v = sub.try_process(|e: &$t| e.0[0]).unwrap();
                black_box(v);
            });
        });
        $c.bench_function(&format!("lease        {} ({}B)", $label, n), |b| {
            let (mut p, s) = photon_ring::channel_bounded::<$t>(1024, 0);
            let mut sub = s.subscribe();
            b.iter(|| {
                p.publish(black_box(unsafe { core::mem::zeroed() }));
                let l = sub.try_lease().unwrap();
                black_box(l.0[0]);
            });
        });
    }};
}

fn lease_vs_copy(c: &mut Criterion) {
    bench_pair!(c, P8, "8B");
    bench_pair!(c, P64, "64B");
    bench_pair!(c, P256, "256B");
    bench_pair!(c, P1K, "1K");
    bench_pair!(c, P4K, "4K");
}

criterion_group!(benches, lease_vs_copy);
criterion_main!(benches);
