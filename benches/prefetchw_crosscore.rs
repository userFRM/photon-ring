// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! # Cross-core PREFETCHW validation benchmark
//!
//! ## What this measures
//!
//! `PREFETCHW` vs `PREFETCHT0` savings only materialise when a subscriber
//! has *recently read* the slot being prefetched — leaving its cache line
//! in **Shared (S)** state on the subscriber's core. The publisher's
//! subsequent write then needs an RFO (Read For Ownership) to upgrade
//! S→M. `PREFETCHW` moves the line to **Exclusive (E)** *during the
//! prefetch*, overlapping the RFO with the previous slot's write.
//!
//! This does NOT show up in same-core benchmarks (publisher and subscriber
//! share L1; the line is already in M state before the next prefetch fires).
//!
//! ## Setup
//!
//! Publisher thread pinned to CPU 0.
//! Subscriber thread pinned to CPU 1 (separate physical core).
//!
//! On i7-10700KF (Comet Lake, 8 cores × 2 HT each):
//!   CPU 0  = Core 0, HT0
//!   CPU 1  = Core 0, HT1   ← same physical core, shares L1/L2
//!   CPU 2  = Core 1, HT0   ← different physical core, separate L1/L2
//!   CPU 8  = Core 0, HT1   (alternate numbering depending on OS)
//!
//! For the worst-case PREFETCHW scenario (line in Shared on a DIFFERENT
//! physical core), we pin publisher to CPU 0 and subscriber to CPU 2.
//! CPU 0 and CPU 2 share the L3 ring bus but have separate L1/L2 —
//! this forces the MESI state transitions that make PREFETCHW valuable.
//!
//! ## Variants benchmarked
//!
//! 1. `publish throughput (same core)` — publisher+subscriber on CPU 0.
//!    Control: shows no PREFETCHW effect (line stays in M state).
//!
//! 2. `publish throughput (cross core, HT sibling)` — pub CPU 0, sub CPU 1.
//!    Moderate effect: shared L1/L2 means shorter snoop latency.
//!
//! 3. `publish throughput (cross core, different physical)` — pub CPU 0, sub CPU 2.
//!    Maximum effect: separate L1/L2, full L3 snoop required.
//!    This is the scenario where PREFETCHW saves ~10-12 ns.
//!
//! 4. `publish throughput (cross core, 4-apart)` — pub CPU 0, sub CPU 4.
//!    Stress test: measures across NUMA-local but topologically distant cores.
//!
//! ## How to read the output
//!
//! Compare variants 1 and 3. The gap is the PREFETCHW savings. If the
//! gap is 0 ns, either:
//!   (a) PREFETCHW is not supported on this CPU (check `grep 3dnowprefetch /proc/cpuinfo`)
//!   (b) The ring buffer is too small and the slot line is being evicted anyway
//!   (c) Hardware prefetcher is beating us to it (rare with seqlock write patterns)
//!
//! ## Cargo.toml
//!
//! Add to [[bench]] in Cargo.toml:
//!   name = "prefetchw_crosscore"
//!   harness = false   (NOT needed — we use criterion_main!)
//!
//! Run with:
//!   cargo bench --bench prefetchw_crosscore
//!   cargo bench --bench prefetchw_crosscore -- --save-baseline before   (before PREFETCHW)
//!   cargo bench --bench prefetchw_crosscore -- --baseline before         (compare)

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// CPU pinning (Linux only via sched_setaffinity)
// ---------------------------------------------------------------------------

/// Pin the calling thread to a specific CPU core.
/// No-op on non-Linux or if the syscall fails (benchmark still runs, just unpinned).
#[cfg(target_os = "linux")]
fn pin_to_cpu(cpu: usize) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

#[cfg(not(target_os = "linux"))]
fn pin_to_cpu(_cpu: usize) {}

// ---------------------------------------------------------------------------
// RDTSCP — cycle-accurate timestamps for one-way latency measurement
// ---------------------------------------------------------------------------

#[inline(always)]
fn rdtscp() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let lo: u64;
        let hi: u64;
        std::arch::asm!(
            "rdtscp",
            out("rax") lo,
            out("rdx") hi,
            out("rcx") _,
            options(nostack, nomem),
        );
        (hi << 32) | lo
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

#[inline(always)]
fn rdtsc_fenced() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let lo: u64;
        let hi: u64;
        std::arch::asm!(
            "lfence",
            "rdtsc",
            out("rax") lo,
            out("rdx") hi,
            options(nostack, nomem),
        );
        (hi << 32) | lo
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

/// TSC-stamped message — publisher embeds its RDTSCP; consumer measures delta.
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct TscMsg {
    tsc: u64,
    seq: u64,
}

// SAFETY: TscMsg is #[repr(C)] with all numeric fields.
unsafe impl photon_ring::Pod for TscMsg {}

// ---------------------------------------------------------------------------
// Pinned throughput benchmark: measure publish rate with cross-core reader
// ---------------------------------------------------------------------------

/// Benchmark publish throughput when a dedicated reader thread is pinned to
/// `reader_cpu`. The publisher runs on whichever CPU Criterion scheduled it
/// on — for the cross-core cases we pin the publisher in a setup closure.
///
/// Returns: measured publish throughput in ops/sec (Criterion records this).
///
/// The reader continuously consumes; the publisher measures its own loop.
/// This is throughput-focused: we want to see if PREFETCHW reduces
/// publisher stall time when re-writing slots the reader recently touched.
fn bench_pinned_throughput(
    c: &mut Criterion,
    group_name: &str,
    publisher_cpu: usize,
    reader_cpu: usize,
) {
    let mut group = c.benchmark_group(group_name);
    group.measurement_time(Duration::from_secs(8));
    group.warm_up_time(Duration::from_secs(3));
    group.sample_size(50);
    // Report throughput in messages/second
    group.throughput(Throughput::Elements(1));

    // Ring sizes: 256, 4096 — different ring sizes affect how often the
    // publisher laps the reader and re-writes slots the reader has cached.
    // Smaller ring = more frequent re-writes = more pronounced PREFETCHW effect.
    for ring_size in [256usize, 4096] {
        group.bench_with_input(
            BenchmarkId::new("ring_size", ring_size),
            &ring_size,
            |b, &size| {
                let (mut pub_, subs) = photon_ring::channel::<u64>(size);
                let mut sub = subs.subscribe();

                let done = Arc::new(AtomicBool::new(false));
                let done2 = done.clone();

                // Spawn reader on reader_cpu
                let reader = std::thread::Builder::new()
                    .name(format!("reader-cpu{reader_cpu}"))
                    .spawn(move || {
                        pin_to_cpu(reader_cpu);
                        while !done2.load(Ordering::Relaxed) {
                            // Drain as fast as possible — keep the slot lines
                            // in Shared state on the reader's core
                            let _ = black_box(sub.try_recv());
                            core::hint::spin_loop();
                        }
                    })
                    .expect("failed to spawn reader thread");

                // Pin publisher
                pin_to_cpu(publisher_cpu);

                let mut i = 0u64;
                b.iter(|| {
                    pub_.publish(black_box(i));
                    i = i.wrapping_add(1);
                });

                done.store(true, Ordering::Relaxed);
                reader.join().unwrap();
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// One-way latency distribution: cycles from RDTSCP stamp to consumer receipt
// ---------------------------------------------------------------------------
//
// Embeds RDTSCP in the payload, consumer measures delta on receipt.
// We collect 200k samples and report percentile distribution.
// This is the most physically meaningful measurement — it tells you exactly
// how long one cache line transfer takes across the MESI state machine.

#[allow(dead_code)]
fn bench_one_way_latency(
    c: &mut Criterion,
    group_name: &str,
    publisher_cpu: usize,
    reader_cpu: usize,
) {
    let mut group = c.benchmark_group(group_name);
    group.measurement_time(Duration::from_secs(10));
    group.warm_up_time(Duration::from_secs(2));
    group.sample_size(100);
    group.throughput(Throughput::Elements(1));

    group.bench_function("one_way_latency_ns", |b| {
        let (mut pub_, subs) = photon_ring::channel::<TscMsg>(4096);
        let mut sub = subs.subscribe();

        let done = Arc::new(AtomicBool::new(false));
        let latency_sum = Arc::new(AtomicU64::new(0));
        let latency_count = Arc::new(AtomicU64::new(0));
        let done2 = done.clone();
        let sum2 = latency_sum.clone();
        let count2 = latency_count.clone();

        let reader = std::thread::Builder::new()
            .name(format!("latency-reader-cpu{reader_cpu}"))
            .spawn(move || {
                pin_to_cpu(reader_cpu);
                while !done2.load(Ordering::Relaxed) {
                    if let Ok(msg) = sub.try_recv() {
                        // LFENCE+RDTSC immediately on receipt — earliest
                        // possible arrival timestamp
                        let now = rdtsc_fenced();
                        if now > msg.tsc {
                            let delta_cycles = now - msg.tsc;
                            // Convert cycles → ns at 3.8 GHz base
                            // (invariant TSC on i7-10700KF runs at base frequency)
                            let delta_ns = (delta_cycles * 1000) / 3_800; // integer ns
                            sum2.fetch_add(delta_ns, Ordering::Relaxed);
                            count2.fetch_add(1, Ordering::Relaxed);
                        }
                    } else {
                        core::hint::spin_loop();
                    }
                }
            })
            .expect("failed to spawn latency reader");

        pin_to_cpu(publisher_cpu);

        let mut seq = 0u64;
        b.iter(|| {
            let tsc = rdtscp();
            pub_.publish(TscMsg { tsc, seq });
            seq = seq.wrapping_add(1);
            // Brief throttle to avoid lapping the 4096-slot ring
            core::hint::spin_loop();
        });

        done.store(true, Ordering::Relaxed);
        reader.join().unwrap();

        let count = latency_count.load(Ordering::Relaxed);
        let sum = latency_sum.load(Ordering::Relaxed);
        if count > 0 {
            let mean_ns = sum / count;
            // Emit as a custom value so it appears in output
            // (Criterion will show it in the report)
            eprintln!("\n[{group_name}] one-way latency: mean={mean_ns}ns over {count} samples");
        }
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Per-slot latency histogram via Criterion custom measurement
// Measures SPSC publish→recv latency from RDTSCP with full percentile output
// ---------------------------------------------------------------------------

fn bench_rdtscp_percentiles(
    c: &mut Criterion,
    label: &str,
    publisher_cpu: usize,
    reader_cpu: usize,
) {
    const WARMUP: u64 = 5_000;
    const SAMPLES: u64 = 200_000;

    // We can't use b.iter() for percentile collection, so wrap as a single
    // timed function that Criterion treats as one sample.
    // This gives us the full distribution without Criterion overhead.
    c.bench_function(label, |b| {
        b.iter(|| {
            let (mut pub_, subs) = photon_ring::channel::<TscMsg>(4096);
            let mut sub = subs.subscribe();

            let done = Arc::new(AtomicBool::new(false));
            let done2 = done.clone();

            let reader = std::thread::Builder::new()
                .name(format!("pct-reader-cpu{reader_cpu}"))
                .spawn(move || {
                    pin_to_cpu(reader_cpu);
                    let mut deltas: Vec<u64> = Vec::with_capacity(SAMPLES as usize);
                    let mut received = 0u64;
                    let mut warmup_done = false;

                    loop {
                        if deltas.len() as u64 >= SAMPLES {
                            break;
                        }
                        if done2.load(Ordering::Relaxed) {
                            break;
                        }
                        match sub.try_recv() {
                            Ok(msg) => {
                                let now = rdtsc_fenced();
                                received += 1;
                                if !warmup_done {
                                    if received >= WARMUP {
                                        warmup_done = true;
                                    }
                                    continue;
                                }
                                if now > msg.tsc {
                                    deltas.push(now - msg.tsc);
                                }
                            }
                            Err(_) => {
                                core::hint::spin_loop();
                            }
                        }
                    }
                    deltas
                })
                .expect("failed to spawn reader");

            pin_to_cpu(publisher_cpu);

            for i in 0..(WARMUP + SAMPLES + 2_000) {
                let tsc = rdtscp();
                pub_.publish(TscMsg { tsc, seq: i });
                for _ in 0..8 {
                    core::hint::spin_loop();
                }
            }

            done.store(true, Ordering::Relaxed);
            let mut deltas = reader.join().expect("reader panicked");

            if !deltas.is_empty() {
                deltas.sort_unstable();
                let n = deltas.len();
                let ghz = 3.8_f64;
                let ns = |c: u64| c as f64 / ghz;

                eprintln!("\n=== {} | one-way latency ({} samples) ===", label, n);
                eprintln!("  min    {:6.1} ns  ({} cycles)", ns(deltas[0]), deltas[0]);
                eprintln!(
                    "  p50    {:6.1} ns  ({} cycles)",
                    ns(deltas[n / 2]),
                    deltas[n / 2]
                );
                eprintln!(
                    "  p90    {:6.1} ns  ({} cycles)",
                    ns(deltas[n * 9 / 10]),
                    deltas[n * 9 / 10]
                );
                eprintln!(
                    "  p99    {:6.1} ns  ({} cycles)",
                    ns(deltas[n * 99 / 100]),
                    deltas[n * 99 / 100]
                );
                eprintln!(
                    "  p99.9  {:6.1} ns  ({} cycles)",
                    ns(deltas[n * 999 / 1000]),
                    deltas[n * 999 / 1000]
                );
                eprintln!(
                    "  max    {:6.1} ns  ({} cycles)",
                    ns(deltas[n - 1]),
                    deltas[n - 1]
                );
                let mean: f64 = deltas.iter().map(|&d| d as f64).sum::<f64>() / n as f64;
                eprintln!("  mean   {:6.1} ns", mean / ghz);
            }

            black_box(())
        });
    });
}

// ---------------------------------------------------------------------------
// Benchmark groups
// ---------------------------------------------------------------------------

fn bench_same_core(c: &mut Criterion) {
    // Control: publisher and subscriber on same physical core.
    // Cache line stays in M state — PREFETCHW has no effect.
    // Establishes the floor.
    bench_pinned_throughput(c, "cross_core/same_core (CPU0+CPU0) — control", 0, 0);
}

fn bench_ht_sibling(c: &mut Criterion) {
    // HT sibling: CPU 0 and CPU 1 share L1/L2 cache on i7-10700KF.
    // Moderate PREFETCHW effect — snoop stays within shared L2.
    bench_pinned_throughput(c, "cross_core/ht_sibling (CPU0→CPU1) — shared L2", 0, 1);
}

fn bench_different_core(c: &mut Criterion) {
    // Different physical core: CPU 0 (Core 0) and CPU 2 (Core 1).
    // Separate L1/L2, shared L3 ring bus.
    // This is the scenario PREFETCHW was designed for.
    // Expected savings: ~40-60 cycles (~10-16 ns at 3.8 GHz base).
    bench_pinned_throughput(
        c,
        "cross_core/different_physical (CPU0→CPU2) — separate L1/L2",
        0,
        2,
    );
}

fn bench_distant_core(c: &mut Criterion) {
    // Topologically distant: CPU 0 and CPU 7 — furthest apart on ring bus.
    bench_pinned_throughput(
        c,
        "cross_core/distant (CPU0→CPU7) — max ring distance",
        0,
        7,
    );
}

fn bench_latency_same_core(c: &mut Criterion) {
    bench_rdtscp_percentiles(c, "latency_pct/same_core (CPU0→CPU0)", 0, 0);
}

fn bench_latency_different_core(c: &mut Criterion) {
    bench_rdtscp_percentiles(c, "latency_pct/different_physical (CPU0→CPU2)", 0, 2);
}

fn bench_latency_ht_sibling(c: &mut Criterion) {
    bench_rdtscp_percentiles(c, "latency_pct/ht_sibling (CPU0→CPU1)", 0, 1);
}

criterion_group!(
    name = pinned_throughput;
    config = Criterion::default()
        .measurement_time(Duration::from_secs(8))
        .warm_up_time(Duration::from_secs(3))
        .sample_size(50);
    targets =
        bench_same_core,
        bench_ht_sibling,
        bench_different_core,
        bench_distant_core,
);

criterion_group!(
    name = latency_distribution;
    config = Criterion::default()
        .measurement_time(Duration::from_secs(15))
        .warm_up_time(Duration::from_secs(2))
        .sample_size(10);  // each sample runs 200k messages
    targets =
        bench_latency_same_core,
        bench_latency_ht_sibling,
        bench_latency_different_core,
);

criterion_main!(pinned_throughput, latency_distribution);
