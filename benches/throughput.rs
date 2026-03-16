use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

fn publish_single(c: &mut Criterion) {
    c.bench_function("publish_single", |b| {
        let (mut p, _s) = photon::channel::<u64>(4096);
        let mut i = 0u64;
        b.iter(|| {
            p.publish(black_box(i));
            i = i.wrapping_add(1);
        });
    });
}

fn publish_recv_roundtrip(c: &mut Criterion) {
    c.bench_function("publish+recv roundtrip (1 sub)", |b| {
        let (mut p, s) = photon::channel::<u64>(4096);
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
        c.bench_function(&format!("fanout_{n}_subs"), |b| {
            let (mut p, s) = photon::channel::<u64>(4096);
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

fn try_recv_empty(c: &mut Criterion) {
    c.bench_function("try_recv_empty", |b| {
        let (_p, s) = photon::channel::<u64>(64);
        let mut sub = s.subscribe();
        b.iter(|| {
            let _ = black_box(sub.try_recv());
        });
    });
}

fn latest_skip(c: &mut Criterion) {
    c.bench_function("latest (skip to newest)", |b| {
        let (mut p, s) = photon::channel::<u64>(4096);
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
    c.bench_function("batch_publish 64 + drain", |b| {
        let (mut p, s) = photon::channel::<u64>(4096);
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
    c.bench_function("struct roundtrip (24B Quote)", |b| {
        let (mut p, s) = photon::channel::<Quote>(4096);
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

criterion_group!(
    benches,
    publish_single,
    publish_recv_roundtrip,
    fanout,
    try_recv_empty,
    latest_skip,
    batch_publish_recv,
    struct_roundtrip,
);
criterion_main!(benches);
