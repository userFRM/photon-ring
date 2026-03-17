<p align="center">
  <img src="docs/images/banner.jpg" alt="Photon Ring" width="100%">
</p>

# Photon Ring

[![Crates.io](https://img.shields.io/crates/v/photon-ring.svg)](https://crates.io/crates/photon-ring)
[![docs.rs](https://docs.rs/photon-ring/badge.svg)](https://docs.rs/photon-ring)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE-APACHE)
[![no_std](https://img.shields.io/badge/no__std-compatible-brightgreen.svg)](https://docs.rs/photon-ring)
[![CI](https://github.com/userFRM/photon-ring/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/userFRM/photon-ring/actions/workflows/ci.yml)

**Ultra-low-latency SPMC/MPMC pub/sub using seqlock-stamped ring buffers.**

Photon Ring is a pub/sub messaging library for Rust that achieves ~95 ns
cross-thread roundtrip latency (48 ns one-way), ~300M msg/s throughput, with
zero allocation on the hot path. `no_std` compatible.

```rust
use photon_ring::{channel, channel_mpmc, Photon};

// SPMC channel (single producer, multiple consumers)
let (mut pub_, subs) = channel::<u64>(1024);
let mut sub = subs.subscribe();
pub_.publish(42);
assert_eq!(sub.try_recv(), Ok(42));

// MPMC channel (multiple producers, clone-able)
let (mp_pub, subs) = channel_mpmc::<u64>(1024);
let mp_pub2 = mp_pub.clone();

// Named-topic bus
let bus = Photon::<u64>::new(1024);
let mut p = bus.publisher("prices");
let mut s = bus.subscribe("prices");
p.publish(100);
assert_eq!(s.try_recv(), Ok(100));
```

## The problem

Inter-thread communication is the dominant cost in concurrent systems. Traditional
approaches pay for at least one of:

| Approach | Write cost | Read cost | Allocation |
|---|---|---|---|
| `std::sync::mpsc` | Lock + CAS | Lock + CAS | Per-message |
| `Mutex<VecDeque>` | Lock acquisition | Lock acquisition | Dynamic ring growth |
| Crossbeam bounded channel | CAS on head | CAS on tail | None (pre-allocated) |
| LMAX Disruptor | Sequence claim + barrier | Sequence barrier spin | None (pre-allocated) |

The Disruptor eliminated allocation overhead and demonstrated that pre-allocated ring
buffers with sequence barriers could achieve 8-32 ns latency. But it still relies on
sequence barriers (shared atomic cursors) that create cache-line contention between
producer and consumers.

## The solution: seqlock-stamped slots

Photon Ring takes a different approach. Instead of sequence barriers, each slot in the
ring buffer carries its own **seqlock stamp** co-located with the payload:

```
                        64 bytes (one cache line)
    +-----------------------------------------------------+
    |  stamp: AtomicU64  |  value: T                      |
    |  (seqlock)         |  (Pod — all bit patterns valid) |
    +-----------------------------------------------------+
    For T <= 56 bytes, stamp and value share one cache line.
    Larger T spills to additional lines (still correct, slightly slower).
```

### Write protocol

```
1. stamp = seq * 2 + 1     (odd = write in progress)
2. fence(Release)           (stamp visible before data)
3. memcpy(slot.value, data) (direct write, no allocation)
4. stamp = seq * 2 + 2     (even = write complete, Release)
5. cursor = seq             (Release — consumers can proceed)
```

### Read protocol

```
1. s1 = stamp.load(Acquire)
2. if odd → spin              (writer active)
3. if s1 < expected → Empty   (not yet published)
4. if s1 > expected → Lagged  (slot reused, consult head cursor)
5. value = memcpy(slot)       (direct read, T: Pod)
6. s2 = stamp.load(Acquire)
7. if s1 == s2 → return       (consistent read)
8. else → retry               (torn read detected)
```

### Why this is fast

1. **No shared mutable state on the read path.** Each subscriber has its own cursor
   (a local `u64`, not an atomic). Subscribers never write to memory that anyone else
   reads. Zero cache-line bouncing between consumers.

2. **Stamp-in-slot co-location.** For payloads up to 56 bytes, the seqlock stamp and
   payload share the same cache line. A reader loads the stamp and the data in a single
   cache-line fetch. The Disruptor pattern requires reading a separate sequence barrier
   (different cache line) before accessing the slot.

3. **No allocation, ever.** The ring is pre-allocated at construction. Publish is a
   `memcpy` into a pre-existing slot. No `Arc`, no `Box`, no heap allocation on the
   hot path.

4. **The `Pod` trait prevents torn-read UB.** Message types must implement `Pod`
   (plain old data), guaranteeing every bit pattern is valid. A torn read produces
   a meaningless-but-safe value that the stamp check detects and discards.

5. **Single-producer by type system.** `Publisher::publish` takes `&mut self`, enforced
   by the Rust borrow checker. No CAS, no lock, no sequence claiming on the write side.
   For multi-producer, `MpPublisher` uses CAS-based sequence claiming.

## Benchmarks

> Measured on **Intel i7-10700KF** (8C/16T, 3.80 GHz, Linux 6.8, Rust 1.93.1) and
> **Apple M1 Pro** (8C, macOS 26.3, Rust 1.92.0). Criterion, 100 samples,
> `--release`, no core pinning. **Your results will vary.**

### vs disruptor-rs (v4.0.0)

|  | Photon Ring | disruptor-rs |
|---|---|---|
| Publish only (Intel) | **2.8 ns** | 30.6 ns |
| Publish only (M1 Pro) | **2.4 ns** | 15.3 ns |
| Cross-thread (Intel) | **95 ns** | 138 ns |
| Cross-thread (M1 Pro) | **130 ns** | 186 ns |

### Detailed operations (Intel / M1 Pro)

- **Publish:** 2.8 / 2.4 ns
- **Roundtrip (1 sub, same thread):** 2.7 / 8.8 ns
- **Fanout 10 independent subscribers:** 17 / 27.7 ns
- **SubscriberGroup (any N, O(1)):** 2.6 / 8.8 ns
- **MPMC (1 publisher, 1 subscriber):** 12.1 / 10.6 ns
- **Empty poll:** 0.85 / 1.1 ns
- **Batch 64 + drain:** 158 / 282 ns
- **Struct roundtrip (24B):** 4.8 / 9.3 ns
- **Cross-thread roundtrip:** 95 / 130 ns
- **One-way latency (RDTSC, Intel only):** 48 ns p50

### Throughput

~300M msg/s on Intel (range: 200-389M), ~88M msg/s on M1 Pro (range: 50-106M).
Varies with OS thread scheduling, especially on Apple Silicon without core pinning.

### Payload scaling

Benchmarked 8B to 4KB — Photon Ring outperforms the Disruptor at all tested sizes.
See [`docs/payload-scaling.md`](docs/payload-scaling.md) for full analysis and chart.

**Note:** All Disruptor numbers are measured against
[`disruptor-rs`](https://crates.io/crates/disruptor) v4.0.0 (the Rust port), not the
original Java LMAX Disruptor.

## Comparison

| | Photon Ring | disruptor-rs (v4) | crossbeam | bus |
|---|---|---|---|---|
| **Delivery** | Broadcast | Broadcast | Point-to-point queue | Broadcast |
| **Publish cost** | 2.8 ns / 12.1 ns (MPMC) | 30.6 ns | — | — |
| **Cross-thread** | 95 ns | 138 ns | — | — |
| **Throughput** | ~300M msg/s | — | — | — |
| **Topology builder** | Yes | Yes | No | No |
| **Batch APIs** | Yes | Yes | Iterator | No |
| **Topic bus** | Yes | No | No | No |
| **Backpressure** | Optional | Default | Default | Default |
| **`no_std`** | Yes | No | No | No |
| **Multi-producer** | Yes | Yes | Yes | No |

`crossbeam-channel` is a queue (each message consumed by one receiver). Use crossbeam
for point-to-point; use Photon Ring when every subscriber should see every message.

## API

**Channels** — `channel::<T>(cap)` creates a single-producer channel (lowest latency).
`channel_mpmc::<T>(cap)` creates a multi-producer channel where `MpPublisher` is
`Clone + Send + Sync`. `channel_bounded::<T>(cap, watermark)` creates a lossless
channel where `try_publish()` returns `Full` instead of overwriting.

**Consumers** — `Subscriber<T>` provides `try_recv`, `recv`, `recv_with`, `latest`,
`recv_batch`, and `drain`. `SubscriberGroup<T, N>` reads the ring once for N logical
subscribers (O(1) fanout regardless of N).

**Topic buses** — `Photon<T>` routes messages by topic name (all topics share one type).
`TypedBus` allows different types per topic. Both support `try_publisher()` (returns
`Option`) and `publisher()` (panics if already taken).

**Pipeline topology** — `Pipeline::builder().input::<T>().then(|x| f(x)).build()`
chains stages on dedicated threads. Supports `fan_out()` for diamond topologies,
`shutdown()` for graceful termination, and `panicked_stages()` for health monitoring.

**Wait strategies** — `BusySpin` (lowest latency, 100% CPU), `YieldSpin` (PAUSE/WFE),
`BackoffSpin` (exponential), `Adaptive` (default, three-phase escalation). Use
`recv_with(strategy)` on any consumer.

**Additional** — core affinity (`pin_to_core_id`), memory control (`mlock`, `prefault`,
huge pages, NUMA), observability (`total_received`, `total_lagged`, `receive_ratio`),
`Shutdown` signal, `publish_with` for in-place construction.

## Design constraints

| Constraint | Rationale |
|---|---|
| `T: Pod` | Every bit pattern must be valid — prevents torn-read UB. See [`Pod` docs](https://docs.rs/photon-ring/latest/photon_ring/trait.Pod.html). |
| Power-of-two capacity | Bitmask modulo (`seq & mask`) instead of `%` division |
| Single producer (default) | Seqlock invariant via `&mut self`; MPMC available via `channel_mpmc` |
| Lossy on overflow (default) | Producer never blocks; consumers detect via `Lagged`. Use `channel_bounded` for lossless. |
| 64-bit atomics required | Excludes 32-bit ARM Cortex-M |

## Platform support

| Platform | Core ring | Affinity | Topology | Hugepages |
|---|---|---|---|---|
| x86_64 Linux | Yes | Yes | Yes | Yes |
| x86_64 macOS / Windows | Yes | Yes | Yes | No |
| aarch64 Linux | Yes | Yes | Yes | Yes |
| aarch64 macOS (Apple Silicon) | Yes | Yes | Yes | No |
| wasm32 | Yes | No | No | No |
| FreeBSD / NetBSD / Android | Yes | Yes | Yes | No |
| 32-bit ARM (Cortex-M) | No | No | No | No |

## Soundness

The seqlock read protocol involves an optimistic non-atomic read that may race with
the writer. The stamp re-check detects torn reads and discards them. This is the same
pattern used by the Linux kernel (`seqlock_t`) and Facebook's Folly library. Under the
Rust/C++ abstract memory model, this concurrent access is formally a data race, but it
is correct on all real hardware for types satisfying `Pod`.

The `Pod` trait enforces that every bit pattern is valid, preventing types like `bool`,
`char`, `NonZero*`, enums, and references from being used as payloads. For domain types
that use these, convert at the boundary:

- `bool` → `u8` (0 = false, 1 = true)
- `enum { A, B, C }` → `u8` (0, 1, 2)
- `Option<u32>` → `u32` (0 = None, nonzero = Some)
- `String` / `&str` → `[u8; N]` fixed buffer

Parse at the boundary, publish as a struct of plain numerics. See
[`Pod` trait docs](https://docs.rs/photon-ring/latest/photon_ring/trait.Pod.html).

## Running

```bash
cargo bench                                    # Full benchmark suite
cargo bench --bench payload_scaling            # Payload size scaling
cargo test                                     # All tests
cargo +nightly miri test --test correctness -- --test-threads=1  # MIRI
cargo run --release --example market_data      # Throughput demo
cargo run --release --example pipeline         # Pipeline topology
cargo run --release --example backpressure     # Lossless delivery
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE-APACHE).
