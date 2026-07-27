// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cross-thread fanout: one producer, N consumer threads, every consumer sees
//! every message.
//!
//! The single-threaded `fanout_scaling` bench isolates protocol overhead. This
//! one measures the thing you actually deploy: consumers on their own cores,
//! paying real cache-coherence traffic. It is also the only shape in which some
//! designs can be measured at all — a builder that owns its consumer threads has
//! no single-threaded mode.
//!
//! Each case publishes `MSGS` messages and has all N consumers account for all
//! of them before the timer stops.
//!
//! Delivery guarantees differ, and the comparison is only meaningful if that is
//! stated: photon (on a bounded ring), the barrier ring, and the per-consumer
//! queues are **lossless** — a slow consumer applies backpressure. The
//! shared-ring broadcast is **lossy** and cannot be otherwise; a slow receiver is
//! told what it missed instead of holding the producer back. Compare it against
//! photon's lossy `channel()` rather than against the lossless rows.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const MSGS: u64 = 100_000;
const RING: usize = 8192;

/// photon: a bounded ring, so delivery is lossless like the others.
fn photon(n: usize) {
    let (mut p, s) = photon_ring::channel_bounded::<u64>(RING, 0);
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let mut sub = s.subscribe();
            std::thread::spawn(move || {
                let mut seen = 0u64;
                while seen < MSGS {
                    if sub.try_recv().is_ok() {
                        seen += 1;
                    }
                }
                seen
            })
        })
        .collect();
    drop(s);
    for i in 0..MSGS {
        p.publish(i);
    }
    for h in handles {
        assert_eq!(h.join().unwrap(), MSGS);
    }
}

/// A shared-ring broadcast: each receiver gets its own clone of the value.
///
/// **This shape is lossy and cannot be otherwise.** A receiver that falls behind
/// is told how many messages it missed (`Lagged`) rather than holding the
/// producer back, so it is not comparable to the lossless rows — it is closer to
/// photon's own lossy `channel()`. Skipped messages are counted as observed so
/// the loop terminates; the run reports how many were actually delivered.
fn shared_ring(n: usize) {
    let (tx, _) = tokio::sync::broadcast::channel::<u64>(RING);
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let mut rx = tx.subscribe();
            std::thread::spawn(move || {
                let mut seen = 0u64;
                while seen < MSGS {
                    match rx.try_recv() {
                        Ok(_) => seen += 1,
                        Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                            std::hint::spin_loop()
                        }
                        // Lossy by design: count the gap so the run terminates.
                        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(k)) => seen += k,
                        Err(e) => panic!("{e:?}"),
                    }
                }
            })
        })
        .collect();
    for i in 0..MSGS {
        // `send` only fails when there are no receivers; it never blocks, which
        // is exactly why this shape drops instead of applying backpressure.
        let _ = tx.send(i);
    }
    for h in handles {
        h.join().unwrap();
    }
}

/// A point-to-point queue is not broadcast: to fan out, the producer sends once
/// per consumer. This is the cost of that, not a like-for-like rival.
fn per_consumer_queue(n: usize) {
    let pairs: Vec<_> = (0..n)
        .map(|_| crossbeam_channel::bounded::<u64>(RING))
        .collect();
    let (txs, rxs): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
    let handles: Vec<_> = rxs
        .into_iter()
        .map(|rx| {
            std::thread::spawn(move || {
                let mut seen = 0u64;
                while seen < MSGS {
                    if rx.recv().is_ok() {
                        seen += 1;
                    }
                }
            })
        })
        .collect();
    for i in 0..MSGS {
        for tx in txs.iter() {
            tx.send(i).unwrap();
        }
    }
    for h in handles {
        h.join().unwrap();
    }
}

/// A sequence-barrier ring. Its builder owns the consumer threads and fixes the
/// handler count at build time, so each arity is written out. There is no
/// single-threaded mode for this shape — which is why this benchmark exists.
macro_rules! handler {
    ($b:expr, $c:expr) => {{
        let c = $c.clone();
        $b.handle_events_with(move |_: &u64, _, _| {
            c.fetch_add(1, Ordering::Relaxed);
        })
    }};
}

fn barrier_ring(n: usize) {
    use disruptor::*;
    let c = Arc::new(AtomicU64::new(0));
    {
        let b = build_single_producer(RING, || 0u64, BusySpin);
        // Each arity is spelled out because the builder's type changes per handler.
        match n {
            1 => {
                let mut p = handler!(b, c).build();
                for i in 0..MSGS {
                    p.publish(|slot| *slot = i);
                }
            }
            2 => {
                let mut p = handler!(handler!(b, c), c).build();
                for i in 0..MSGS {
                    p.publish(|slot| *slot = i);
                }
            }
            4 => {
                let mut p = handler!(handler!(handler!(handler!(b, c), c), c), c).build();
                for i in 0..MSGS {
                    p.publish(|slot| *slot = i);
                }
            }
            8 => {
                let mut p = handler!(
                    handler!(
                        handler!(
                            handler!(handler!(handler!(handler!(handler!(b, c), c), c), c), c),
                            c
                        ),
                        c
                    ),
                    c
                )
                .build();
                for i in 0..MSGS {
                    p.publish(|slot| *slot = i);
                }
            }
            _ => unreachable!(),
        }
    } // drop drains and joins every handler
    assert_eq!(c.load(Ordering::Relaxed), MSGS * n as u64);
}

fn threaded(c: &mut Criterion) {
    let mut g = c.benchmark_group("cross-thread fanout, 100k messages");
    g.sample_size(10);

    for &n in [1usize, 2, 4, 8].iter() {
        g.throughput(Throughput::Elements(MSGS * n as u64));
        g.bench_with_input(BenchmarkId::new("photon", n), &n, |b, &n| {
            b.iter(|| photon(n))
        });
        g.bench_with_input(BenchmarkId::new("disruptor-rs", n), &n, |b, &n| {
            b.iter(|| barrier_ring(n))
        });
        g.bench_with_input(
            BenchmarkId::new("tokio::sync::broadcast", n),
            &n,
            |b, &n| b.iter(|| shared_ring(n)),
        );
        g.bench_with_input(
            BenchmarkId::new("crossbeam-channel (one per consumer)", n),
            &n,
            |b, &n| b.iter(|| per_consumer_queue(n)),
        );
    }
    g.finish();
}

criterion_group!(benches, threaded);
criterion_main!(benches);
