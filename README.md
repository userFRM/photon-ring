<p align="center">
  <img src="docs/images/banner.jpg" alt="Photon Ring" width="100%">
</p>

# Photon Ring

[![Crates.io](https://img.shields.io/crates/v/photon-ring.svg)](https://crates.io/crates/photon-ring)
[![docs.rs](https://docs.rs/photon-ring/badge.svg)](https://docs.rs/photon-ring)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE-APACHE)
[![no_std](https://img.shields.io/badge/no__std-compatible-brightgreen.svg)](https://docs.rs/photon-ring)
[![CI](https://github.com/userFRM/photon-ring/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/userFRM/photon-ring/actions/workflows/ci.yml)

**Ultra-low-latency SPMC/MPMC pub/sub using stamped ring buffers.**

Photon Ring is a zero-allocation pub/sub crate for Rust built around pre-allocated ring buffers, per-slot stamp validation, and `T: Pod` payloads. It targets the part of concurrent systems where queueing overhead dominates: market data, telemetry fanout, staged pipelines, and other hot-path broadcast workloads where every subscriber should see every message.

By default, slots use a volatile-based seqlock for maximum performance. With the `atomic-slots` feature, the same stamp protocol operates over `AtomicU64` stripes — **formally sound under the Rust abstract machine** with zero performance regression on x86-64.

It is `no_std` compatible with `alloc`, supports named-topic buses and typed buses, and includes a pipeline builder for multi-stage thread topologies on supported desktop/server platforms.

> [!IMPORTANT]
> The default `channel()` is lossy on overflow: the publisher never blocks, and slow subscribers detect drops via `TryRecvError::Lagged`. If lossless delivery matters more than raw latency, use `channel_bounded()`.

## Quick start

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
mp_pub.publish(1);
mp_pub2.publish(2);

// Named-topic bus
let bus = Photon::<u64>::new(1024);
let mut p = bus.publisher("prices");
let mut s = bus.subscribe("prices");
p.publish(100);
assert_eq!(s.try_recv(), Ok(100));
```

## Installation

```toml
[dependencies]
photon-ring = "2.4.0"
```

Optional features:

- `derive`: enables `#[derive(photon_ring::DerivePod)]` for user-defined `Pod` types.
- `hugepages`: enables Linux memory controls such as `mlock`, `prefault`, and NUMA helpers.
- `atomic-slots`: enables formally sound slot implementation using `AtomicU64` stripes instead of `write_volatile`/`read_volatile`. Zero performance cost on x86-64; ~5-10ns reader overhead on ARM64 due to acquire fence. Eliminates formal undefined behavior under the Rust abstract machine. Passes Miri.

Rust 1.94+ is supported. For best performance, compile with `-C target-cpu=native` to enable `PREFETCHW` and other CPU-specific optimizations.

## The problem

Inter-thread communication is often the dominant cost in concurrent systems. Traditional messaging designs usually pay for at least one expensive property on the hot path:

| Approach | Write cost | Read cost | Allocation |
|---|---|---|---|
| `std::sync::mpsc` | Lock + CAS | Lock + CAS | Per-message |
| `Mutex<VecDeque>` | Lock acquisition | Lock acquisition | Dynamic growth |
| `crossbeam-channel` | CAS on head | CAS on tail | None |
| LMAX Disruptor pattern | Sequence claim + barrier | Sequence barrier spin | None |

The Disruptor showed that pre-allocated rings can remove allocator overhead and drive very low latency, but its shared sequence barriers still create cache-line contention between producers and consumers.

## The solution: seqlock-stamped slots

Photon Ring moves synchronization into each slot. Every slot carries its own seqlock stamp beside the payload, so readers validate the data they just loaded instead of bouncing a shared barrier cache line.

```text
                        64 bytes (one cache line)
    +-----------------------------------------------------+
    |  stamp: AtomicU64  |  value: T                      |
    |  (seqlock)         |  (Pod - all bit patterns valid)|
    +-----------------------------------------------------+
    For T <= 56 bytes, stamp and value share one cache line.
    Larger T spills to additional lines (still correct, slightly slower).
```

### Write protocol

```text
1. stamp = seq * 2 + 1     (odd = write in progress)
2. fence(Release)          (stamp visible before data)
3. write_volatile(slot.value, data)
4. stamp = seq * 2 + 2     (even = write complete, Release)
5. cursor = seq            (Release - consumers can proceed)
```

### Read protocol

```text
1. s1 = stamp.load(Acquire)
2. if odd -> spin
3. if s1 < expected -> Empty
4. if s1 > expected -> Lagged
5. value = read_volatile(slot)    (direct read, T: Pod)
6. s2 = stamp.load(Acquire)
7. if s1 == s2 -> return
8. else -> retry
```

## Why this is fast

1. **No shared mutable state on the read path.** Each subscriber keeps its own local cursor. Readers do not publish progress into a shared hot cache line unless bounded backpressure tracking is enabled.
2. **Stamp and payload are co-located.** For `T <= 56` bytes, the stamp check and payload read hit the same cache line.
3. **No allocation on publish or receive.** The ring is fixed at construction time, and hot-path operations are direct slot reads and writes.
4. **`T: Pod` makes torn reads safe to reject.** Every bit pattern is valid, so an optimistic torn read is harmless and discarded by the stamp re-check.
5. **Single-producer SPMC avoids write-side contention.** `Publisher::publish` takes `&mut self`, so the type system enforces one producer without CAS. `MpPublisher` adds an MPMC path when you need multiple concurrent writers.

## Benchmarks

Measured with Criterion on an **Intel i7-10700KF** (8C/16T, 3.80 GHz, Linux 6.8, Rust 1.93.1) and **Apple M1 Pro** (8C, macOS 26.3, Rust 1.92.0), `--release`, 100 samples, no core pinning unless stated.

> [!NOTE]
> These numbers use `Pod` payloads and compare concrete implementations, not abstract algorithms. Scheduler noise, pinning, CPU generation, and payload layout all matter, so treat them as reproducible snapshots rather than universal constants.

### Against `disruptor-rs`

- **Publish:** 2.8 ns (Intel) / 2.4 ns (M1 Pro), versus 30.6 ns / 15.3 ns for `disruptor-rs`
- **Cross-thread roundtrip:** 95 ns (Intel) / 130 ns (M1 Pro), versus 138 ns / 186 ns for `disruptor-rs`

### Core operations

```
                                    i7-10700KF     M1 Pro
                                    ──────────     ──────
  Publish only                        2.8 ns      2.4 ns
  Roundtrip (1 sub, same thread)      2.7 ns      8.8 ns
  Fanout (10 independent subs)       17.0 ns     27.7 ns
  SubscriberGroup (any N, O(1))       2.6 ns      8.8 ns
  MPMC (1 pub, 1 sub)                12.1 ns     10.6 ns
  Empty poll                          0.9 ns      1.1 ns
  Batch 64 + drain                    158 ns      282 ns
  Struct roundtrip (24B Pod)          4.8 ns      9.3 ns
  Cross-thread roundtrip               95 ns      130 ns
  One-way latency (RDTSC)             48 ns p50     —
```

### Throughput

- **Sustained throughput:** about 300M msg/s on Intel and 88M msg/s on M1 Pro
- **Payload scaling:** Photon Ring outperformed `disruptor-rs` across 8-byte to 4 KiB `Pod` payloads; see [`docs/payload-scaling.md`](docs/payload-scaling.md)

## Comparison

| | Photon Ring | disruptor-rs (v4) | crossbeam-channel | bus |
|---|---|---|---|---|
| **Delivery** | Broadcast | Broadcast | Point-to-point queue | Broadcast |
| **Publish cost** | 2.8 ns / 12.1 ns (MPMC) | 30.6 ns | - | - |
| **Cross-thread** | 95 ns | 138 ns | - | - |
| **Throughput** | ~300M msg/s | - | - | - |
| **Topology builder** | Yes | Yes | No | No |
| **Batch APIs** | Yes | Yes | Iterator | No |
| **Topic bus** | Yes | No | No | No |
| **Backpressure** | Optional | Default | Default | Default |
| **`no_std`** | Yes | No | No | No |
| **Multi-producer** | Yes | Yes | Yes | No |

Use `crossbeam-channel` when one receiver should own each message. Use Photon Ring when each subscriber should observe the same stream with minimal coordination overhead.

## API overview

Channels are the lowest-level interface. `channel::<T>(capacity)` creates the fastest single-producer path and returns a `Publisher<T>` plus a cloneable `Subscribable<T>`. `channel_bounded::<T>(capacity, watermark)` adds optional backpressure; `Publisher::try_publish` returns `PublishError::Full(value)` instead of overwriting unread slots. `channel_mpmc::<T>(capacity)` returns `MpPublisher<T>`, which is `Clone + Send + Sync` and uses atomic sequence claiming for concurrent producers. On the write side, the important APIs are `publish`, `publish_with` for in-place construction, `publish_batch` on `Publisher`, and `published`/`capacity` for lightweight counters.

Subscribers are independent and contention-free by default. `Subscribable::subscribe()` starts from future messages only, while `subscribe_from_oldest()` starts at the oldest message still retained in the ring. `Subscriber<T>` exposes `try_recv`, `recv`, `recv_with`, `latest`, `pending`, `recv_batch`, and `drain`, plus observability counters through `total_received`, `total_lagged`, and `receive_ratio`. If multiple logical consumers are polled on the same thread, `subscribe_group::<N>()` creates a `SubscriberGroup<T, N>` that performs one ring read for the whole group instead of one per logical subscriber.

For topic routing, `Photon<T>` provides a string-keyed bus where all topics share the same payload type, and `TypedBus` allows a different `T: Pod` per topic. Both lazily create topics and expose `publisher`, `try_publisher`, `subscribe`, and `subscribable`. `publisher()` will panic if the publisher for that topic was already taken, and `TypedBus` also panics on type mismatches for an existing topic.

Pipelines build dedicated-thread processing graphs on supported OS targets. `topology::Pipeline::builder().capacity(...).input::<T>()` returns an input publisher plus a typed builder; `.then(...)` chains stages, `.fan_out(...)` creates a diamond, `.then_a(...)` and `.then_b(...)` extend either branch, and `.build()` returns the final subscriber plus a `Pipeline` handle. `then_with(f, WaitStrategy)` (and `then_a_with`, `then_b_with`) lets you configure the wait strategy for each pipeline stage. The handle supports `shutdown`, `join`, `panicked_stages`, `is_healthy`, and `stage_count`. For manual shutdown outside topology, use `Shutdown`.

The `#[derive(photon_ring::DeriveMessage)]` macro supports a `#[photon(as_enum)]` attribute for fields whose types are `#[repr(u8)]` enums. Unrecognized types without this attribute now produce a compile error instead of being silently assumed to be enums.

Ring capacity accepts any integer >= 2. Power-of-two capacities use bitwise `seq & mask` for zero-overhead indexing; arbitrary capacities use Lemire reciprocal-multiply fastmod (~1.5 ns).

Wait behavior is explicit. `recv_with` accepts `WaitStrategy::BusySpin`, `YieldSpin`, `BackoffSpin`, `Adaptive`, `MonitorWait`, or `MonitorWaitFallback` depending on whether you want the absolute lowest wakeup latency or better core sharing. `MonitorWait` uses Intel UMONITOR/UMWAIT (Alder Lake+) for near-zero power wakeup (~30 ns), with automatic fallback to PAUSE on older x86 or WFE on ARM; construct it safely via `WaitStrategy::monitor_wait(&stamp)`. `MonitorWaitFallback` uses TPAUSE without requiring an address. On supported platforms, the crate also includes `affinity` helpers for CPU pinning; with the `hugepages` feature on Linux, you can use `Publisher::mlock`, `Publisher::prefault`, and `mem::{set_numa_preferred, reset_numa_policy}` to reduce page-fault and NUMA noise.

### Companion crates

- **[`photon-ring-async`](photon-ring-async/)** — Runtime-agnostic async wrappers. `AsyncSubscriber` and `AsyncSubscriberGroup` with yield-based polling and configurable spin budget. Works with tokio, smol, embassy, or any executor.
- **[`photon-ring-metrics`](photon-ring-metrics/)** — Observability wrappers with `SubscriberMetrics` (snapshot/delta tracking) and `PublisherMetrics`. Framework-agnostic — bring your own prometheus/opentelemetry.

## Design constraints

| Constraint | Rationale |
|---|---|
| `T: Pod` | Every bit pattern must be valid, which makes optimistic torn reads safe to reject. |
| Capacity >= 2 | Any capacity works. Power-of-two uses `seq & mask`; arbitrary capacity uses Lemire fastmod (~1.5 ns, zero-division). |
| Single producer by default | The fastest path relies on `&mut self` rather than write-side atomics. |
| Lossy overflow by default | The publisher never blocks; subscribers detect drops through `Lagged`. |
| 64-bit atomics required | The core algorithm depends on `AtomicU64`. |
| 64-bit sequence numbers | Stamp encoding `seq * 2 + 2` overflows at `u64::MAX / 2` (~9.2 × 10^18 messages). At 1 billion msg/s this would take ~292 years. |

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

## Soundness and `Pod`

### The `Pod` trait

The `Pod` trait means more than `Copy`: every possible bit pattern of the payload must be valid. This is required because the stamp-based read protocol may speculatively read bytes from a slot while a writer is updating it. If a torn bit pattern could be invalid for `T`, the read would be undefined behavior before the stamp check could discard it.

Primitive numerics, arrays of `Pod`, and tuples of `Pod` are already supported. For your own structs, use `#[repr(C)]`, stick to `Pod` fields, and implement `Pod` manually or via the `derive` feature when appropriate.

| Type | Why it is not `Pod` | Use instead |
|---|---|---|
| `bool` | Only `0` and `1` are valid | `u8` |
| `char` | Must be a valid Unicode scalar | `u32` |
| `NonZero<u32>` | `0` is invalid | `u32` |
| `Option<T>` | The discriminant has invalid patterns | Sentinel integer |
| Rust `enum` | Only declared variants are valid | `u8` or `u32` |
| `&T`, `&str` | Pointers must be valid | Value types only |
| `String`, `Vec<_>` | Heap-owning, has `Drop` | Fixed `[u8; N]` buffer |

### Formal soundness

Photon Ring offers two slot implementations, selectable at compile time:

| | Default (volatile) | `atomic-slots` feature |
|---|---|---|
| **Mechanism** | `write_volatile` / `read_volatile` | `AtomicU64::store/load(Relaxed)` stripes |
| **Formal status** | Data race under Rust abstract machine (practical UB) | **Formally sound** — no data races |
| **Miri** | Flags multi-threaded tests | **Passes** |
| **x86-64 cost** | Baseline | **Zero** — identical `MOV` instructions |
| **ARM64 cost** | Baseline | **+5-10 ns** reader (one `DMB ISHLD` fence) |
| **Precedent** | Same pattern as Linux kernel seqlocks (20+ years) | Novel: first formally-sound seqlock in Rust |

> [!NOTE]
> The default volatile-based implementation is **correct on all real hardware** (x86, ARM). The "UB" is purely under Rust's abstract machine — no compiler has ever miscompiled this pattern, and the Linux kernel relies on identical semantics. Enable `atomic-slots` if you need formal soundness, Miri compliance, or defense against hypothetical future compiler optimizations.

> [!TIP]
> Keep rich domain types at the edges and publish compact `Pod` messages in the middle. Convert enums, `Option`, booleans, and strings into explicit numeric fields or fixed-size buffers before calling `publish`.

Examples of safe boundary conversions:

- `bool` -> `u8` (`0 = false`, `1 = true`)
- `enum Side { Buy, Sell }` -> `u8` (`0 = Buy`, `1 = Sell`)
- `Option<u32>` -> `u32` (`0 = None`, nonzero = `Some`)
- `String` / `&str` -> `[u8; N]`

## Running

```bash
cargo test
cargo bench
cargo bench --bench payload_scaling
cargo +nightly miri test --test correctness -- --test-threads=1
cargo test --test loom_mpmc --release  # Loom exhaustive MPMC concurrency tests
cargo run --release --example market_data
cargo run --release --example pipeline
cargo run --release --example backpressure
```

## License

Licensed under Apache-2.0. See [LICENSE-APACHE](LICENSE-APACHE).
