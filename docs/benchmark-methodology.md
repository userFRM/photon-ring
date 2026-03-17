<!--
  Copyright 2026 Photon Ring Contributors
  SPDX-License-Identifier: Apache-2.0
-->

# Benchmark Methodology

This document describes how the Photon Ring benchmarks are structured,
what they measure, what they do not control, and how to reproduce them.

## Hardware

### Machine A (primary benchmark machine)

| Property | Value |
|---|---|
| CPU | Intel Core i7-10700KF (Comet Lake) |
| Base frequency | 3.80 GHz |
| Turbo frequency | Up to 5.10 GHz (single-core) |
| Cores / Threads | 8 cores / 16 threads (SMT enabled) |
| L1d cache | 32 KB per core, 8-way |
| L2 cache | 256 KB per core, 4-way |
| L3 cache | 16 MB shared, ring bus interconnect |
| Architecture | x86_64, Coffee Lake successor (14 nm) |

### Machine B (secondary)

| Property | Value |
|---|---|
| CPU | Apple M1 Pro |
| Cores | 8 (6 performance + 2 efficiency) |
| Architecture | aarch64 (ARMv8.5-A) |
| L1d cache | 128 KB per P-core, 64 KB per E-core |
| L2 cache | 12 MB P-cluster, 4 MB E-cluster |

## OS and Kernel

| Machine | OS | Kernel |
|---|---|---|
| A | Ubuntu (Linux 6.8) | 6.8.x (default distribution kernel) |
| B | macOS 26.3 | Darwin / XNU |

## Rust Toolchain

| Machine | Rust version |
|---|---|
| A | 1.93.1 (stable) |
| B | 1.92.0 (stable) |

## Compiler Flags

All benchmarks are compiled with `--release`, which uses Cargo's default
release profile:

```toml
[profile.release]
opt-level = 3
```

No custom `RUSTFLAGS` are set. No LTO, PGO, or `target-cpu=native` flags
are applied. The benchmarks use whatever code generation the default
stable toolchain produces.

## Criterion Configuration

The benchmarks use [Criterion.rs](https://github.com/bheisler/criterion.rs)
v0.8.x with default settings unless noted otherwise:

| Parameter | Value |
|---|---|
| Sample size | 100 (Criterion default) |
| Warm-up time | 3 seconds (Criterion default) |
| Measurement time | 5 seconds (Criterion default) |
| Reported statistic | Median (as stated in README) |
| Outlier detection | Criterion's built-in MAD-based classification |

No custom Criterion configuration is applied in `Criterion.toml` or via
the `Criterion::default().configure_from_args()` builder.

## What Is NOT Controlled

The following variables are **not** controlled or pinned during benchmark
runs. They can cause variance between runs and between machines:

- **CPU frequency governor.** The OS governor (e.g., `performance` vs
  `powersave` vs `schedutil` on Linux) is left at its default. Turbo
  boost / frequency scaling is not disabled. This means latency
  numbers reflect real-world conditions but are not deterministic.

- **Turbo boost.** Intel Turbo Boost 2.0 is enabled on Machine A. The
  actual clock frequency during a benchmark run depends on thermal
  state, number of active cores, and power limits. Single-threaded
  benchmarks may run at up to 5.1 GHz; cross-thread benchmarks
  typically settle around 4.5--4.7 GHz all-core turbo.

- **SMT (Hyper-Threading).** SMT is enabled on Machine A (16 logical
  CPUs on 8 physical cores). Cross-thread benchmarks may land on
  sibling hyperthreads (sharing a physical core) or on separate
  physical cores, which dramatically changes latency.

- **Core isolation.** No `isolcpus`, `nohz_full`, or `rcu_nocbs` kernel
  parameters are set. OS scheduler interrupts, timer ticks, and other
  threads may preempt benchmark threads.

- **Core pinning.** The Criterion-based benchmarks (`cargo bench`) do
  **not** pin threads to specific cores. The OS scheduler controls
  placement. The `rdtsc_oneway` bench and the `pinned_latency` example
  do use core pinning where noted.

- **NUMA topology.** Machine A is a single-socket system (no NUMA).
  Results on multi-socket systems would differ due to inter-socket
  cache coherence costs (QPI/UPI).

- **Background load.** Benchmarks are run on a developer workstation,
  not a dedicated benchmark machine. Background processes (desktop
  environment, browser, etc.) may be running.

## How to Reproduce

### Full benchmark suite (Criterion)

```bash
# Clone and enter the repository
git clone https://github.com/userFRM/photon-ring.git
cd photon-ring

# Run all Criterion benchmarks
cargo bench --release

# Run individual benchmark binaries
cargo bench --bench throughput
cargo bench --bench payload_scaling
```

Results are written to `target/criterion/` as JSON and HTML reports.

### One-way latency (RDTSC)

```bash
# x86_64 only — uses inline RDTSCP/LFENCE+RDTSC instructions
cargo bench --bench rdtsc_oneway
```

This is a standalone binary (not a Criterion harness). It prints
percentile statistics (p50, p90, p99, p999) in raw TSC cycles and
estimated nanoseconds at two reference frequencies.

### Throughput example

```bash
cargo run --release --example market_data
```

### Pinned-core latency example

```bash
cargo run --release --example pinned_latency
```

## Measurement Methodology

### Cross-thread roundtrip latency

**File:** `benches/throughput.rs`, function `cross_thread_latency`

The roundtrip benchmark measures the time for a message to travel from
the publisher thread to the subscriber thread and for the subscriber to
signal receipt back to the publisher:

1. The publisher writes a `u64` sequence number to the ring via
   `publish(i)`.
2. The subscriber thread busy-spins on `try_recv()`. When it receives
   the message, it stores the value into a shared `AtomicU64` (`seen`)
   with `Release` ordering.
3. The publisher busy-spins on `seen.load(Acquire)` until it equals `i`.
4. Criterion measures the time for step 1 through step 3.

This is a **roundtrip** measurement: it includes one cache line transfer
for the slot data (publisher -> subscriber) and one cache line transfer
for the `seen` atomic (subscriber -> publisher). The reported "95 ns
cross-thread latency" is therefore approximately 2x the true one-way
latency, plus the overhead of the `AtomicU64` signal-back store and
load.

The signal-back `AtomicU64` is a separate cache line from the ring slot.
This means the roundtrip crosses two cache lines, not one.

### One-way latency (RDTSC)

**File:** `benches/rdtsc_oneway.rs`

The one-way benchmark eliminates the signal-back overhead by embedding
the publisher's TSC (Time Stamp Counter) reading directly in the message
payload:

1. The publisher calls `RDTSCP` (serializing TSC read) immediately
   before `publish()`. The TSC value is stored in the message payload
   (`TscMsg.tsc`).
2. The subscriber calls `LFENCE; RDTSC` immediately after `try_recv()`
   returns `Ok`. The `LFENCE` serializes the read to get the earliest
   possible "arrival" timestamp.
3. The delta `(subscriber_tsc - publisher_tsc)` is recorded in raw
   cycles.
4. After collecting 100,000 samples (with 10,000 warmup discarded),
   percentiles are computed and converted to nanoseconds using the
   known CPU base and turbo frequencies.

**Assumptions:**

- Both threads run on the same socket (same TSC domain). On modern
  Intel with invariant TSC (`constant_tsc` + `nonstop_tsc` CPUID
  flags), the TSC is synchronized across cores within a socket.
- TSC offset between cores is negligible (< 1 cycle on same-socket
  Intel).
- The publisher throttles with a brief spin loop (32 iterations of
  `spin_loop()`) to avoid lapping the consumer in the 4096-slot ring.

**Why RDTSC and not `std::time::Instant`?**

`Instant::now()` typically calls `clock_gettime(CLOCK_MONOTONIC)`, which
on Linux goes through the vDSO and reads the TSC internally, but adds
overhead for timekeeping math and potential syscall fallback. Raw
`RDTSCP` / `LFENCE+RDTSC` gives cycle-accurate measurements with ~20
cycles of overhead instead of ~50--100.

### Disruptor comparison

**File:** `benches/throughput.rs`, functions `disruptor_publish_only` and
`disruptor_roundtrip`

The Disruptor benchmarks use the [`disruptor`](https://crates.io/crates/disruptor)
crate v4.0.0 (the Rust port of the LMAX Disruptor pattern):

- **Same binary:** Both Photon Ring and Disruptor benchmarks run in the
  same Criterion binary, compiled with the same flags, on the same
  machine, in the same `cargo bench` invocation.
- **Same ring size:** Both use 4096 slots.
- **Same wait strategy:** The Disruptor is configured with `BusySpin`
  (its lowest-latency wait strategy), matching Photon Ring's default
  busy-spin subscriber loop.
- **Publish-only:** The Disruptor `publish_only` benchmark publishes
  into a Disruptor ring with a single `BusySpin` consumer attached
  (required by the Disruptor API). The consumer stores received values
  into a `Relaxed` atomic, which the benchmark ignores.
- **Roundtrip:** The Disruptor `roundtrip` benchmark publishes a `u64`
  and spins until a `Release`/`Acquire` atomic confirms the consumer
  processed it -- the same signal-back pattern used for Photon Ring's
  cross-thread latency benchmark.

The comparison is **same-thread roundtrip** (publish + spin-until-consumed
in the Criterion loop), not cross-thread. Cross-thread Disruptor numbers
are not measured because the Disruptor's consumer thread is managed
internally by its builder API.

## Caveats

- **Self-benchmarks.** All benchmarks are authored and run by the Photon
  Ring maintainers. They have not been independently verified by a
  third party. Selection bias (choosing favorable hardware, favorable
  run, or favorable metric) is possible even if unintentional.

- **Hardware-dependent.** The absolute numbers (95 ns, 48 ns, 2.8 ns)
  are specific to the tested hardware. Different CPUs, cache
  hierarchies, and interconnects will produce different results. Intel
  Comet Lake's ring bus L3 interconnect has different latency
  characteristics than AMD Zen's CCX/CCD mesh or Apple Silicon's
  SLC-backed coherence fabric.

- **Not independently verified.** No external benchmarking organization
  or peer reviewer has reproduced these numbers. Users should run
  `cargo bench` on their own hardware and treat the published numbers
  as indicative, not authoritative.

- **Disruptor comparison is against the Rust port.** The `disruptor`
  crate (v4.0.0) is a Rust reimplementation of the LMAX Disruptor
  pattern. It shares the same algorithmic design (sequence barriers,
  pre-allocated ring, wait strategies) as the original Java LMAX
  Disruptor but differs in language runtime, JIT vs AOT compilation,
  and memory model. A direct comparison against the Java original on
  matched hardware has not been performed.

- **Median vs. tail latency.** The README reports median (p50) numbers.
  Tail latency (p99, p999) is higher and more variable due to OS
  scheduling, interrupts, and frequency scaling. The `rdtsc_oneway`
  benchmark reports full percentile distributions.

- **Single-socket only.** All benchmarks run on single-socket machines.
  Cross-socket (NUMA) latency would be significantly higher for both
  Photon Ring and the Disruptor.
