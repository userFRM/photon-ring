# Photon Ring

[![Crates.io](https://img.shields.io/crates/v/photon-ring.svg)](https://crates.io/crates/photon-ring)
[![docs.rs](https://docs.rs/photon-ring/badge.svg)](https://docs.rs/photon-ring)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE-APACHE)
[![no_std](https://img.shields.io/badge/no__std-compatible-brightgreen.svg)](https://docs.rs/photon-ring)

**Ultra-low-latency SPMC inter-thread messaging using seqlock-stamped ring buffers.**

Photon Ring is a single-producer, multi-consumer (SPMC) pub/sub library for Rust.
`no_std` compatible (requires `alloc`), zero-allocation hot path, ~96 ns cross-thread
latency (48 ns one-way), and ~3 ns publish cost.

```rust
use photon_ring::{channel, Photon};

// Low-level SPMC channel
let (mut publisher, subscribers) = channel::<u64>(1024);
let mut sub = subscribers.subscribe();
publisher.publish(42);
assert_eq!(sub.try_recv(), Ok(42));

// Named-topic bus
let bus = Photon::<u64>::new(1024);
let mut pub_ = bus.publisher("prices");
let mut sub  = bus.subscribe("prices");
pub_.publish(100);
assert_eq!(sub.try_recv(), Ok(100));
```

## The Problem

Inter-thread communication is the dominant cost in concurrent systems. Traditional approaches
pay for at least one of:

| Approach | Write cost | Read cost | Allocation |
|---|---|---|---|
| `std::sync::mpsc` | Lock + CAS | Lock + CAS | Per-message |
| `Mutex<VecDeque>` | Lock acquisition | Lock acquisition | Dynamic ring growth |
| Crossbeam bounded channel | CAS on head | CAS on tail | None (pre-allocated) |
| LMAX Disruptor | Sequence claim + barrier | Sequence barrier spin | None (pre-allocated) |

The Disruptor eliminated allocation overhead and demonstrated that pre-allocated ring buffers
with sequence barriers could achieve 8-32 ns latency. But it still relies on sequence barriers
(shared atomic cursors) that create cache-line contention between producer and consumers.

## The Solution: Seqlock-Stamped Slots

Photon Ring takes a different approach. Instead of sequence barriers, each slot in the ring
buffer carries its own **seqlock stamp** co-located with the payload:

```
                        64 bytes (one cache line)
    ┌─────────────────────────────────────────────────────┐
    │  stamp: AtomicU64  │  value: T                      │
    │  (seqlock)         │  (Copy, no Drop)               │
    └─────────────────────────────────────────────────────┘
    For T <= 56 bytes, stamp and value share one cache line.
    Larger T spills to additional lines (still correct, slightly slower).
```

### Write Protocol (Publisher)

```
1. stamp = seq * 2 + 1     (odd = write in progress)
2. fence(Release)           (stamp visible before data)
3. memcpy(slot.value, data) (direct write, no allocation)
4. stamp = seq * 2 + 2     (even = write complete, Release)
5. cursor = seq             (Release — consumers can proceed)
```

### Read Protocol (Subscriber)

```
1. s1 = stamp.load(Acquire)
2. if odd → spin              (writer active)
3. if s1 < expected → Empty   (not yet published)
4. if s1 > expected → Lagged  (slot reused, consult head cursor)
5. value = memcpy(slot)       (direct read, T: Copy)
6. fence(Acquire)
7. s2 = stamp.load()
8. if s1 == s2 → return       (consistent read)
9. else → retry               (torn read detected)
```

### Why This Is Fast

1. **No shared mutable state on the read path.** Each subscriber has its own cursor (a local
   `u64`, not an atomic). Subscribers never write to memory that anyone else reads. Zero
   cache-line bouncing between consumers.

2. **Stamp-in-slot co-location.** For payloads up to 56 bytes, the seqlock stamp and payload
   share the same cache line. A reader loads the stamp and the data in a single cache-line
   fetch. The Disruptor pattern requires reading a separate sequence barrier (different cache
   line) before accessing the slot.

3. **No allocation, ever.** The ring is pre-allocated at construction. Publish is a `memcpy`
   into a pre-existing slot. No `Arc`, no `Box`, no heap allocation on the hot path.

4. **`T: Copy` enables torn-read detection without resource leaks.** Because `T` has no
   destructor, a torn read (partial overwrite during read) never causes double-free or
   resource leaks. The stamp check detects the inconsistency and the read is retried.
   See [Soundness](#the-seqlock-memory-model-question) for the full discussion.

5. **Single-producer by type system.** `Publisher::publish` takes `&mut self`, enforced by
   the Rust borrow checker. No CAS, no lock, no sequence claiming on the write side.

## Benchmark Results

### Benchmark Machines

| Machine | CPU | Cores | OS | Rust |
|---|---|---|---|---|
| **A** | Intel Core i7-10700KF @ 3.80 GHz | 8C / 16T | Linux 6.8 (Ubuntu) | 1.93.1 |
| **B** | Apple M1 Pro | 8C | macOS 26.3 | 1.92.0 |

All runs: Criterion, 100 samples, 3-second warmup, `--release` (opt-level 3), no
core pinning. Numbers are medians. **Your results will vary** — run `cargo bench`
on your own hardware for authoritative numbers.

### Cross-Thread Latency (the metric that matters)

Both libraries measured with publisher and consumer on separate OS threads, busy-spin
wait strategy, ring size 4096. This is the apples-to-apples comparison.

| Benchmark | Photon Ring (A) | disruptor 4.0 (A) | Photon Ring (B) | disruptor 4.0 (B) |
|---|---|---|---|---|
| Cross-thread roundtrip | **96 ns** | 133 ns | **103 ns** | 174 ns |
| Publish only (write cost) | **3 ns** | 24 ns | **2 ns** | 12 ns |

Cross-thread latency is dominated by the CPU's cache coherence protocol (MESI/MOESI).
Both libraries are close to the hardware floor. The publish-only difference reflects
Photon Ring's simpler write path (one seqlock stamp vs sequence claim + barrier).

### Photon Ring Detailed Benchmarks

| Operation | A | B | Notes |
|---|---|---|---|
| `publish` (write only) | 3 ns | 2 ns | Single slot seqlock write |
| `publish` + `try_recv` (1 sub, same thread) | 2.5 ns | 7 ns | Stamp-only fast path |
| Fanout: 10 independent subs | 13 ns | 23 ns | ~1.1 ns per additional sub |
| **Fanout: 10 SubscriberGroup** | **4.3 ns** | — | **~0.2 ns per additional sub** |
| `try_recv` (empty channel) | < 1 ns | < 1 ns | Single atomic load |
| Batch publish 64 + drain | 155 ns | 206 ns | 2.4 ns/msg amortized |
| Struct roundtrip (24B payload) | 4.4 ns | 8 ns | Realistic payload size |
| Cross-thread latency | 96 ns | 103 ns | Inter-core cache transfer |
| One-way latency (RDTSC) | 48 ns p50 | — | Single cache line transfer |

### Throughput

The `market_data` example publishes 500,000 messages per topic across 4 independent
SPMC topics (4 publishers, 4 subscribers):

| Machine | Messages | Time | Throughput |
|---|---|---|---|
| **A** | 2,000,000 | 12.5 ms | 160M msg/s |
| **B** | 2,000,000 | 26.44 ms | 75.6M msg/s |

## Soundness

### Test Suite

- **58 correctness tests** (40 integration + 18 unit) covering basic pub/sub,
  multi-subscriber fanout, ring overflow with lag detection, `latest()` under
  contention, batch publish, cross-thread SPMC, bounded backpressure, core
  affinity, wait strategies, memory control, observability counters, and a
  1M-message stress test.
- **10 doc-tests** verifying all code examples compile and run.

### MIRI Verification

Single-threaded tests pass under [Miri](https://github.com/rust-lang/miri) with no
undefined behavior detected. Multi-threaded tests are excluded because Miri's thread
scheduling is non-deterministic and the tests contain spin loops.

MIRI verifies the single-threaded unsafe operations (pointer reads/writes, `MaybeUninit`
handling, `UnsafeCell` access patterns) but **does not verify the concurrent seqlock
protocol**, which relies on hardware memory ordering guarantees beyond what the abstract
memory model formalizes.

```bash
cargo +nightly miri test --test correctness -- --test-threads=1
```

### The Seqlock Memory Model Question

Seqlocks involve an optimistic read pattern: the reader copies data that may be concurrently
modified by the writer, then verifies consistency via the stamp. Under the C++20/Rust
abstract memory model, concurrent non-atomic reads and writes to the same memory location
constitute a data race, which is undefined behavior — even if the result is discarded
on mismatch.

**This is a known open problem in language-level memory models.** The pattern is
universally used in practice:

- **The Linux kernel** uses seqlocks pervasively (`seqlock_t`) for read-heavy data like
  `jiffies`, namespace counters, and filesystem metadata.
- **Facebook/Meta's Folly** library implements `folly::SharedMutex` using the same pattern.
- **The C++ standards committee** (WG21) has acknowledged this gap. Papers like
  [P1478R7](https://wg21.link/P1478R7) (`std::byte`-based seqlock support) and discussions
  around `std::start_lifetime_as` aim to formalize seqlock semantics.

**Why `T: Copy` is necessary but not sufficient:**

The `T: Copy` bound ensures no destructor runs on a torn read, preventing resource leaks
and double-free. However, certain `Copy` types have **validity invariants** — for example,
`bool` (must be 0 or 1), `NonZero<u32>` (must be non-zero), or reference types. A torn
read of these types could produce a value that violates the type's invariant, which is
undefined behavior regardless of whether the value is later discarded.

**Recommended payload types:** Use plain numeric types (`u8`..`u128`, `f32`, `f64`),
fixed-size arrays of numerics, or `#[repr(C)]` structs composed exclusively of such types.
These have no validity invariants beyond alignment and can safely tolerate torn reads.

In practice, on all mainstream architectures (x86, ARM, RISC-V), torn reads of
naturally-aligned types produce a valid-but-meaningless bit pattern that is always
detected and discarded by the stamp check. No undefined CPU state, trap, or signal
is produced.

## API

### Low-Level Channel

```rust
use photon_ring::{channel, TryRecvError};

let (mut pub_, subs) = channel::<u64>(1024); // capacity must be power of 2

// Subscribe (future messages only)
let mut sub = subs.subscribe();

// Or subscribe from oldest available message still in the ring
let mut sub_old = subs.subscribe_from_oldest();

// Publish
pub_.publish(42);
pub_.publish_batch(&[1, 2, 3, 4]);

// Receive (non-blocking)
match sub.try_recv() {
    Ok(value) => { /* process */ }
    Err(TryRecvError::Empty) => { /* no data yet */ }
    Err(TryRecvError::Lagged { skipped }) => { /* fell behind, skipped N messages */ }
}

// Blocking receive (busy-spins until data is available)
let value = sub.recv();

// Skip to latest (discard intermediate messages)
if let Some(latest) = sub.latest() { /* ... */ }

// Query state
let n = sub.pending();       // messages available (capped at capacity)
let n = pub_.published();    // total messages published
```

**Wait strategies:** `recv()` uses a two-phase spin by default. For control over
CPU usage vs latency, use `recv_with()`:

```rust
use photon_ring::WaitStrategy;

// Lowest latency — 100% CPU, use on dedicated pinned cores
let value = sub.recv_with(WaitStrategy::BusySpin);

// Balanced — spin 64 iters, yield 64, then park
let value = sub.recv_with(WaitStrategy::default());
```

### Backpressure (bounded channel)

When message loss is unacceptable (e.g., order fill notifications):

```rust
use photon_ring::{channel_bounded, PublishError};

let (mut pub_, subs) = channel_bounded::<u64>(1024, 0);
let mut sub = subs.subscribe();

// try_publish returns Full instead of overwriting
match pub_.try_publish(42u64) {
    Ok(()) => { /* published */ }
    Err(PublishError::Full(val)) => { /* ring full, val returned */ }
}
```

### Core Affinity

Pin threads to specific CPU cores for deterministic cache coherence latency.
Available automatically on Linux, macOS, Windows, FreeBSD, NetBSD, and Android
(via `core_affinity2` dependency).

```rust,no_run
use photon_ring::affinity;

let cores = affinity::available_cores();
// Pin publisher to core 0, subscriber to core 1
affinity::pin_to_core_id(0);
```

### SubscriberGroup (batched fanout)

When multiple subscribers are polled on the same thread, `SubscriberGroup` reads the
ring **once** and advances all cursors together — reducing per-subscriber cost from
~1.1 ns to ~0.2 ns.

```rust
use photon_ring::channel;

let (mut pub_, subs) = channel::<u64>(1024);
let mut group = subs.subscribe_group::<10>(); // 10 logical subscribers

pub_.publish(42);
let value = group.try_recv().unwrap(); // one seqlock read, 10 cursor advances
assert_eq!(value, 42);
```

### Named-Topic Bus

```rust
use photon_ring::Photon;

#[derive(Clone, Copy)]
struct Quote { price: f64, volume: u32 }

let bus = Photon::<Quote>::new(4096);

// Each topic is an independent SPMC ring.
// publisher() can only be called once per topic (panics on second call).
let mut prices_pub = bus.publisher("AAPL");
let mut prices_sub = bus.subscribe("AAPL");

// Multiple subscribers per topic
let mut logger_sub = bus.subscribe("AAPL");

prices_pub.publish(Quote { price: 150.0, volume: 100 });
```

## Design Constraints

| Constraint | Rationale |
|---|---|
| `T: Copy` | Enables torn-read detection without resource leaks; see [Soundness](#the-seqlock-memory-model-question) |
| Power-of-two capacity | Bitmask modulo (`seq & mask`) instead of expensive `%` division |
| Single producer | Seqlock invariant requires exclusive write access; enforced via `&mut self` |
| Lossy on overflow | When the ring wraps, oldest messages are silently overwritten; consumers detect via `Lagged` |
| Busy-spin `recv()` | Lowest latency; use `try_recv()` with your own backoff if CPU usage matters |

## Comparison with Existing Work

| | Photon Ring | disruptor-rs (v4) | bus (jonhoo) | crossbeam bounded |
|---|---|---|---|---|
| **Pattern** | SPMC seqlock ring | SP/MP sequence barriers | SPMC broadcast | MPMC bounded queue |
| **Cross-thread latency** | 96–103 ns | 133–174 ns | — | — |
| **Publish cost** | 2–3 ns | 12–24 ns | — | — |
| **Allocation** | None | None | None | None (bounded) |
| **Consumer model** | Poll (`try_recv`) | Callback + Poller API | Poll | Poll |
| **Overflow** | Lossy (Lagged) | Backpressure (blocks) | Backpressure | Backpressure |
| **Multi-producer** | No | Yes | No | Yes |
| **`no_std`** | Yes | No | No | No |
| **Dependencies** | 2 (hashbrown, spin) | 4 | 0 | 3 |

**Note:** Crossbeam bounded channels use backpressure (the sender blocks when the buffer is
full), which prevents message loss but adds latency under contention. Photon Ring uses lossy
semantics — the producer never blocks, but slow consumers miss messages.

## Running Benchmarks

```bash
# Full benchmark suite (includes disruptor comparison)
cargo bench

# Market data throughput example
cargo run --release --example market_data

# Run the test suite
cargo test

# MIRI soundness check (requires nightly)
cargo +nightly miri test --test correctness -- --test-threads=1
```

## Platform Support

| Platform | Core ring | Affinity | Hugepages | Notes |
|---|---|---|---|---|
| x86_64 Linux | Yes | Yes | Yes | Full support |
| x86_64 macOS | Yes | Yes | No | |
| x86_64 Windows | Yes | Yes | No | |
| aarch64 Linux | Yes | Yes | Yes | |
| aarch64 macOS (Apple Silicon) | Yes | Yes | No | M1/M2/M3/M4 |
| wasm32 | Yes | No | No | Core channel only |
| FreeBSD / NetBSD | Yes | Yes | No | |
| Android | Yes | Yes | No | |
| 32-bit ARM (Cortex-M) | No | No | No | Requires AtomicU64 |

## License

Licensed under the [Apache License, Version 2.0](LICENSE-APACHE).
