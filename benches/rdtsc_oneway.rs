// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

// RDTSC-based one-way latency measurement.
//
// Measures TRUE one-way publisher->consumer latency by embedding the
// publisher's TSC stamp in the message payload. No signal-back needed,
// so we measure exactly one cache line transfer (slot line) rather than
// a roundtrip over two lines (slot + seen).
//
// Expected result on i7-10700KF (Comet Lake, ring bus): ~40-55 ns.
//
// Usage:
//   cargo bench --bench rdtsc_oneway
//
// NOTE: Requires x86_64. Both cores must be on the same socket (same
// TSC domain). TSC offset between cores is assumed negligible — on
// modern Intel with invariant TSC this holds if you don't cross
// sockets.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Read the Time Stamp Counter (RDTSC) with an RDTSCP serialising read.
/// RDTSCP waits for all prior instructions to retire before reading TSC,
/// giving us a lower-bound timestamp that isn't speculated past.
#[inline(always)]
fn rdtscp() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let lo: u64;
        let hi: u64;
        // RDTSCP: serialises prior instructions, reads TSC into EDX:EAX,
        // and core ID into ECX (which we discard).
        std::arch::asm!(
            "rdtscp",
            out("rax") lo,
            out("rdx") hi,
            out("rcx") _, // IA32_TSC_AUX — core ID, unused
            options(nostack, nomem),
        );
        (hi << 32) | lo
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        // Fallback: use std::time (not cycle-accurate)
        std::time::Instant::now()
            .duration_since(std::time::Instant::now())
            .as_nanos() as u64
    }
}

/// LFENCE; RDTSC — use LFENCE to serialize on the read side so we get
/// the earliest possible "arrival" timestamp. RDTSCP would also work
/// but adds a micro-op for the IA32_TSC_AUX read we don't need.
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

/// Message carrying a TSC timestamp.
#[derive(Clone, Copy)]
#[repr(C)]
struct TscMsg {
    tsc: u64, // publisher's RDTSCP stamp
    seq: u64, // sequence for ordering verification
}

fn main() {
    const WARMUP: u64 = 10_000;
    const SAMPLES: u64 = 100_000;

    let (mut pub_, subs) = photon_ring::channel::<TscMsg>(4096);
    let mut sub = subs.subscribe();

    let done = Arc::new(AtomicBool::new(false));
    let done2 = done.clone();

    // --- Consumer thread ---
    // Collects (delta_tsc) samples. No signal back to producer.
    let reader = std::thread::spawn(move || {
        let mut deltas: Vec<u64> = Vec::with_capacity(SAMPLES as usize);
        let mut received = 0u64;
        let mut warmup_done = false;

        loop {
            if deltas.len() as u64 >= SAMPLES {
                break;
            }
            if done2.load(Ordering::Relaxed) && received >= WARMUP + SAMPLES {
                break;
            }
            match sub.try_recv() {
                Ok(msg) => {
                    // LFENCE+RDTSC immediately on receipt — this is our
                    // "arrival" timestamp, before any other work.
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

                    if deltas.len() as u64 >= SAMPLES {
                        break;
                    }
                }
                Err(_) => {
                    core::hint::spin_loop();
                }
            }
        }

        deltas
    });

    // --- Publisher thread (this thread) ---
    // Warm up, then publish SAMPLES messages with RDTSCP stamps.
    // We throttle slightly to avoid lapping the consumer — the consumer
    // must keep up (it does no signal-back, so it's fast).
    for i in 0..(WARMUP + SAMPLES + 1000) {
        let tsc = rdtscp();
        pub_.publish(TscMsg { tsc, seq: i });
        // Brief spin to avoid lapping the consumer's 4096-slot ring.
        for _ in 0..32 {
            core::hint::spin_loop();
        }
    }

    done.store(true, Ordering::Relaxed);
    let mut deltas = reader.join().expect("reader thread panicked");

    if deltas.is_empty() {
        eprintln!("ERROR: no samples collected");
        return;
    }

    // --- Statistics ---
    deltas.sort_unstable();

    let n = deltas.len();
    let min = deltas[0];
    let p50 = deltas[n / 2];
    let p90 = deltas[n * 9 / 10];
    let p99 = deltas[n * 99 / 100];
    let p999 = deltas[n * 999 / 1000];
    let max = deltas[n - 1];
    let mean: f64 = deltas.iter().map(|&d| d as f64).sum::<f64>() / n as f64;

    // Convert cycles to nanoseconds.
    // On a 3.8 GHz base (i7-10700KF), 1 cycle = ~0.263 ns.
    // Turbo can reach 5.1 GHz (1 cycle = ~0.196 ns).
    // We print both raw cycles and estimated ns at different frequencies.
    let freq_ghz_base = 3.8_f64;
    let freq_ghz_turbo = 5.1_f64;

    println!("=== RDTSC One-Way Latency (publisher -> consumer) ===");
    println!("Samples: {n}");
    println!();
    println!(
        "         {:>10} {:>10} {:>10}",
        "cycles", "ns@3.8GHz", "ns@5.1GHz"
    );
    println!(
        "  min:   {:>10} {:>10.1} {:>10.1}",
        min,
        min as f64 / freq_ghz_base,
        min as f64 / freq_ghz_turbo
    );
    println!(
        "  mean:  {:>10.0} {:>10.1} {:>10.1}",
        mean,
        mean / freq_ghz_base,
        mean / freq_ghz_turbo
    );
    println!(
        "  p50:   {:>10} {:>10.1} {:>10.1}",
        p50,
        p50 as f64 / freq_ghz_base,
        p50 as f64 / freq_ghz_turbo
    );
    println!(
        "  p90:   {:>10} {:>10.1} {:>10.1}",
        p90,
        p90 as f64 / freq_ghz_base,
        p90 as f64 / freq_ghz_turbo
    );
    println!(
        "  p99:   {:>10} {:>10.1} {:>10.1}",
        p99,
        p99 as f64 / freq_ghz_base,
        p99 as f64 / freq_ghz_turbo
    );
    println!(
        "  p999:  {:>10} {:>10.1} {:>10.1}",
        p999,
        p999 as f64 / freq_ghz_base,
        p999 as f64 / freq_ghz_turbo
    );
    println!(
        "  max:   {:>10} {:>10.1} {:>10.1}",
        max,
        max as f64 / freq_ghz_base,
        max as f64 / freq_ghz_turbo
    );
    println!();
    println!("Interpretation:");
    println!("  The p50 in cycles / your_turbo_GHz = one-way latency in ns.");
    println!("  On i7-10700KF at ~4.7 GHz all-core turbo:");
    println!("  If p50 ~ 200 cycles => ~42 ns one-way");
    println!("  This represents ONE cache line transfer (slot line: MESI");
    println!("  Modified->Shared snoop through L3 ring bus).");
}
