# Photon Ring

**Ultra-low-latency SPMC inter-thread messaging using seqlock-stamped ring buffers.**

Photon Ring is a single-producer, multi-consumer (SPMC) pub/sub library for Rust that achieves
sub-5 ns same-thread roundtrip latency with zero allocation on the hot path and zero
runtime dependencies.

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

### Benchmark Environment

- **CPU:** Intel Core i7-10700KF @ 3.80 GHz (8 cores / 16 threads)
- **OS:** Linux 6.8.0-101-generic (Ubuntu)
- **Rust:** 1.93.1 (stable), `--release` profile (opt-level 3)
- **Turbo Boost:** enabled; **SMT:** enabled; **Core pinning:** none
- **Framework:** Criterion 0.5.1, 100 samples, 3-second warmup

Numbers are medians from the machine above. **Your results will vary** — run
`cargo bench` on your own hardware for authoritative numbers.

### Methodology Note on disruptor Comparison

The `disruptor` crate (v4.0.0) uses a **managed-thread consumer model**: consumers run as
closures on threads owned by the Disruptor. There is no same-thread `publish` + `recv`
roundtrip equivalent, because the consumer always runs on a separate thread.

Therefore:

- **"Publish only"** compares just the cost of writing into each library's ring.
- **"Roundtrip"** for Photon Ring is same-thread (`publish` then `try_recv`). For `disruptor`,
  the consumer inherently runs on another thread, so its roundtrip includes inter-core
  cache transfer. These are **not equivalent** — Photon Ring's same-thread number reflects
  pure algorithmic overhead, while the `disruptor` number includes cross-thread synchronization.
- **"Cross-thread"** for Photon Ring is measured separately — a dedicated benchmark with publisher
  and subscriber on different OS threads, spinning until the value is observed.

| Benchmark | Photon Ring | disruptor 4.0 | Notes |
|---|---|---|---|
| Publish only | **3.0 ns** | 20 ns | Both single-producer, ring size 4096 |
| Same-thread roundtrip | **2.7 ns** | N/A | disruptor has no same-thread API |
| Cross-thread roundtrip | **131 ns** | 133 ns | disruptor consumer is always cross-thread |

### Photon Ring Detailed Benchmarks

| Operation | Latency | Notes |
|---|---|---|
| `publish` (write only) | 3.0 ns | Single slot seqlock write |
| `publish` + `try_recv` (1 sub) | 2.7 ns | Same-thread roundtrip |
| Fanout: 1 subscriber | 3.3 ns | Publish + 1 recv |
| Fanout: 2 subscribers | 5.1 ns | ~2 ns per additional sub |
| Fanout: 5 subscribers | 10.9 ns | Linear scaling |
| Fanout: 10 subscribers | 20.3 ns | Linear scaling |
| `try_recv` (empty channel) | 0.8 ns | Single atomic load |
| `latest` (skip to newest) | 26 ns | Includes 10 publishes |
| Batch publish 64 + drain | 190 ns | 3.0 ns/msg amortized |
| Struct roundtrip (24B payload) | 4.3 ns | Realistic payload size |
| Cross-thread latency | 131 ns | Inter-core cache transfer |

### Throughput

The `market_data` example publishes 500,000 messages per topic across 4 independent
SPMC topics (4 publishers, 4 subscribers):

```
2,000,000 messages in 12.5 ms = 160M msg/s
```

## Soundness

### Test Suite

- **26 correctness tests** covering basic pub/sub, multi-subscriber fanout, ring overflow
  with lag detection, `latest()` under contention, batch publish, cross-thread SPMC,
  and a 1M-message stress test verified across 5 consecutive runs.
- **3 doc-tests** verifying all README-facing code examples compile and run.

### MIRI Verification

22 single-threaded tests pass under [Miri](https://github.com/rust-lang/miri) with no
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

**Important:** `recv()` busy-spins (`core::hint::spin_loop`) and will consume 100% of one
CPU core while waiting. Use `try_recv()` with your own backoff strategy if this is
unacceptable.

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
| **Publish cost** | **3.0 ns** | 20 ns | — | — |
| **Cross-thread latency** | 131 ns | 133 ns | — | — |
| **Allocation** | None | None | None | None (bounded) |
| **Consumer model** | Poll (`try_recv`) | Callback + Poller API | Poll | Poll |
| **Overflow** | Lossy (Lagged) | Backpressure (blocks) | Backpressure | Backpressure |
| **Multi-producer** | No | Yes | No | Yes |
| **Dependencies** | **0** | 4 | 0 | 3 |

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

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT License](LICENSE-MIT) at your option.
