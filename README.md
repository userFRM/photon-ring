# Photon Ring

[![Crates.io](https://img.shields.io/crates/v/photon-ring.svg)](https://crates.io/crates/photon-ring)
[![docs.rs](https://docs.rs/photon-ring/badge.svg)](https://docs.rs/photon-ring)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE-APACHE)
[![no_std](https://img.shields.io/badge/no__std-compatible-brightgreen.svg)](https://docs.rs/photon-ring)
[![CI](https://github.com/userFRM/photon-ring/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/userFRM/photon-ring/actions/workflows/ci.yml)

**Ultra-low-latency SPMC/MPMC pub/sub using seqlock-stamped ring buffers.**

Photon Ring is a pub/sub messaging library for Rust that achieves ~95 ns
cross-thread roundtrip latency (48 ns one-way) with zero allocation on the
hot path. `no_std` compatible, zero dependencies on the core channel path.

```rust
use photon_ring::{channel, channel_mpmc, Photon};

// SPMC channel (single producer, multiple consumers)
let (mut pub_, subs) = channel::<u64>(1024);
let mut sub = subs.subscribe();
pub_.publish(42);
assert_eq!(sub.try_recv(), Ok(42));

// MPMC channel (multiple producers)
let (mp_pub, subs) = channel_mpmc::<u64>(1024);
let mp_pub2 = mp_pub.clone(); // Clone + Send + Sync

// Named-topic bus
let bus = Photon::<u64>::new(1024);
let mut p = bus.publisher("prices");
let mut s = bus.subscribe("prices");
p.publish(100);
assert_eq!(s.try_recv(), Ok(100));
```

## The Problem

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

## The Solution: Seqlock-Stamped Slots

Photon Ring takes a different approach. Instead of sequence barriers, each slot in the
ring buffer carries its own **seqlock stamp** co-located with the payload:

```
                        64 bytes (one cache line)
    +-----------------------------------------------------+
    |  stamp: AtomicU64  |  value: T                      |
    |  (seqlock)         |  (Copy, no Drop)                |
    +-----------------------------------------------------+
    For T <= 56 bytes, stamp and value share one cache line.
    Larger T spills to additional lines (still correct, slightly slower).
```

### Write Protocol (Publisher)

```
1. stamp = seq * 2 + 1     (odd = write in progress)
2. fence(Release)           (stamp visible before data)
3. memcpy(slot.value, data) (direct write, no allocation)
4. stamp = seq * 2 + 2     (even = write complete, Release)
5. cursor = seq             (Release -- consumers can proceed)
```

### Read Protocol (Subscriber)

```
1. s1 = stamp.load(Acquire)
2. if odd -> spin              (writer active)
3. if s1 < expected -> Empty   (not yet published)
4. if s1 > expected -> Lagged  (slot reused, consult head cursor)
5. value = memcpy(slot)        (direct read, T: Copy)
6. s2 = stamp.load(Acquire)
7. if s1 == s2 -> return       (consistent read)
8. else -> retry               (torn read detected)
```

### Why This Is Fast

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

4. **`T: Copy` enables torn-read detection without resource leaks.** Because `T` has
   no destructor, a torn read never causes double-free or resource leaks. The stamp
   check detects the inconsistency and the read is retried.

5. **Single-producer by type system.** `Publisher::publish` takes `&mut self`, enforced
   by the Rust borrow checker. No CAS, no lock, no sequence claiming on the write side.
   For multi-producer, `MpPublisher` uses CAS-based sequence claiming.

## Benchmark Results

### Benchmark Machines

| Machine | CPU | Cores | OS | Rust |
|---|---|---|---|---|
| **A** | Intel Core i7-10700KF @ 3.80 GHz | 8C / 16T | Linux 6.8 (Ubuntu) | 1.93.1 |
| **B** | Apple M1 Pro | 8C | macOS 26.3 | 1.92.0 |

All runs: Criterion, 100 samples, 3-second warmup, `--release`, no core pinning.
Numbers are medians. **Your results will vary** -- run `cargo bench` on your own
hardware for authoritative numbers.

### Head-to-Head vs disruptor-rs (v4.0.0)

| Benchmark | Photon Ring (A) | disruptor 4.0 (A) | Photon Ring (B) | disruptor 4.0 (B) |
|---|---|---|---|---|
| Publish only | **2.9 ns** | 25.8 ns | **2.0 ns** | 12 ns |
| Cross-thread roundtrip | **95 ns** | 132 ns | **103 ns** | 174 ns |

### Detailed Benchmarks

| Operation | A | B | Notes |
|---|---|---|---|
| `publish` (write only) | 2.9 ns | 2.0 ns | Single slot seqlock write |
| `publish` + `try_recv` (1 sub) | 2.6 ns | -- | Stamp-only fast path |
| Fanout: 10 independent subs | 15.9 ns | -- | ~1.3 ns per additional sub |
| **SubscriberGroup (any N)** | **2.6 ns** | -- | **O(1) -- single cursor, single seqlock read** |
| **MPMC 1 pub, 1 sub** | **12.2 ns** | -- | CAS sequence claiming overhead |
| `try_recv` (empty) | 0.85 ns | -- | Single atomic load |
| Batch 64 + drain | 156 ns | -- | 2.4 ns/msg amortized |
| Struct roundtrip (24B) | 4.8 ns | -- | Realistic payload size |
| Cross-thread latency | 95 ns | 103 ns | Inter-core cache transfer |
| One-way latency (RDTSC) | 48 ns p50 | -- | Single cache line transfer |

### Payload Scaling

Benchmarked from 8B to 4KB. Photon Ring outperforms the Disruptor at all tested
payload sizes. See [`docs/payload-scaling.md`](docs/payload-scaling.md) for the
full analysis and chart.

## Comparison with Existing Work

| | Photon Ring | disruptor-rs (v4) | crossbeam | bus |
|---|---|---|---|---|
| **Pattern** | SPMC/MPMC broadcast | SP/MP sequence barriers | MPMC queue | SPMC broadcast |
| **Publish cost** | 2.9 ns (SPMC) / 12.2 ns (MPMC) | 25.8 ns | -- | -- |
| **Cross-thread** | 95 ns | 132 ns | -- | -- |
| **Topology builder** | `Pipeline::builder().then()` | `handleEventsWith().then()` | No | No |
| **Batch APIs** | `recv_batch`, `drain`, `publish_batch` | Batch publishing | Iterator drain | No |
| **Named-topic bus** | `Photon<T>`, `TypedBus` | No | No | No |
| **Backpressure** | `channel_bounded` (SPMC) | Default | Default | Default |
| **Overflow** | Lossy (default) or bounded | Backpressure | Backpressure | Backpressure |
| **`no_std`** | Yes | No | No | No |
| **Affinity / NUMA** | Yes | No | No | No |
| **Multi-producer** | Yes (`MpPublisher`) | Yes | Yes | No |

`crossbeam-channel` is a queue (each message consumed by one receiver), not a broadcast
primitive. Use crossbeam when you need point-to-point; use Photon Ring when every
subscriber should see every message.

## API

### Channels

| Constructor | Producer type | Use case |
|---|---|---|
| `channel::<T>(capacity)` | `Publisher<T>` (`&mut self`) | Single producer, lowest latency |
| `channel_mpmc::<T>(capacity)` | `MpPublisher<T>` (`&self`, Clone) | Multiple producers |
| `channel_bounded::<T>(capacity, watermark)` | `Publisher<T>` with `try_publish` | Lossless delivery |

### Consumer Types

| Type | Use case |
|---|---|
| `Subscriber<T>` | Independent consumer with `try_recv`, `recv`, `recv_with`, `latest`, `recv_batch`, `drain` |
| `SubscriberGroup<T, N>` | O(1) batched fanout -- single seqlock read for N logical consumers |

### Topic Buses

| Type | Use case |
|---|---|
| `Photon<T>` | Named topics, all sharing one message type |
| `TypedBus` | Named topics, each with its own message type |

### Pipeline Topology

```rust
use photon_ring::topology::Pipeline;

let (mut input, stages) = Pipeline::builder()
    .capacity(64)
    .input::<u64>();

let (mut output, pipeline) = stages
    .then(|x| x * 2)
    .then(|x| x + 1)
    .build();

input.publish(10);
assert_eq!(output.recv(), 21);

pipeline.shutdown();
pipeline.join();
```

Supports `then()` for chained stages, `fan_out()` for diamond topologies,
`is_healthy()` and `panicked_stages()` for monitoring.

### Wait Strategies

| Strategy | Latency | CPU | Best for |
|---|---|---|---|
| `BusySpin` | Lowest | 100% core | Dedicated, pinned cores |
| `YieldSpin` | Low | High | Shared cores, SMT |
| `BackoffSpin` | Medium | Decreasing | Background consumers |
| `Adaptive` (default) | Auto-scaling | Varies | General purpose |

### Additional Features

- **Core affinity:** `affinity::pin_to_core_id(0)` on Linux, macOS, Windows, FreeBSD, Android
- **Memory control** (`hugepages` feature, Linux): `mlock()`, `prefault()`, huge pages, NUMA placement
- **Observability:** `total_received()`, `total_lagged()`, `receive_ratio()` on all consumers
- **Batch receive:** `recv_batch(&mut [T])`, `drain()` iterator
- **Shutdown:** `Shutdown::new()` / `trigger()` / `is_shutdown()`
- **In-place publish:** `publish_with(|slot| { ... })` for write-side copy elision

## Design Constraints

| Constraint | Rationale |
|---|---|
| `T: Copy` | Torn-read safety; no `Drop`/double-free on partial reads |
| Power-of-two capacity | Bitmask modulo (`seq & mask`) instead of `%` division |
| Single producer (SPMC default) | Seqlock invariant via `&mut self`; MPMC available via `channel_mpmc` |
| Lossy on overflow (default) | Producer never blocks; consumers detect via `Lagged` |
| 64-bit atomics required | Excludes 32-bit ARM Cortex-M |

## Platform Support

| Platform | Core ring | Affinity | Topology | Hugepages | Notes |
|---|---|---|---|---|---|
| x86_64 Linux | Yes | Yes | Yes | Yes | Full support |
| x86_64 macOS | Yes | Yes | Yes | No | |
| x86_64 Windows | Yes | Yes | Yes | No | |
| aarch64 Linux | Yes | Yes | Yes | Yes | |
| aarch64 macOS (Apple Silicon) | Yes | Yes | Yes | No | M1/M2/M3/M4 |
| wasm32 | Yes | No | No | No | Core channel only |
| FreeBSD / NetBSD / Android | Yes | Yes | Yes | No | |
| 32-bit ARM (Cortex-M) | No | No | No | No | Requires AtomicU64 |

## Soundness

The seqlock read protocol involves an optimistic non-atomic read that may race with
the writer. The stamp re-check detects torn reads and discards them. This is the same
pattern used by the Linux kernel (`seqlock_t`) and Facebook's Folly library. Under the
Rust/C++ abstract memory model, this concurrent access is formally a data race, but it
is correct on all real hardware for `T: Copy` types without validity invariants.

Recommended payloads: `u64`, `f64`, `[u8; N]`, `#[repr(C)]` structs of plain numerics.
Avoid: `bool`, `char`, `NonZero*`, references.

## Running

```bash
cargo bench                                    # Full benchmark suite
cargo bench --bench payload_scaling            # Payload size scaling
cargo test                                     # 117 tests
cargo +nightly miri test --test correctness -- --test-threads=1  # MIRI
cargo run --release --example market_data      # Throughput demo
cargo run --release --example pipeline         # Pipeline topology
cargo run --release --example backpressure     # Lossless delivery
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE-APACHE).
