<p align="center">
  <img src="docs/images/banner.jpg" alt="Photon Ring" width="100%">
</p>

# Photon Ring

[![Crates.io](https://img.shields.io/crates/v/photon-ring.svg)](https://crates.io/crates/photon-ring)
[![docs.rs](https://docs.rs/photon-ring/badge.svg)](https://docs.rs/photon-ring)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![no_std](https://img.shields.io/badge/no__std-compatible-brightgreen.svg)](https://docs.rs/photon-ring)
[![CI](https://github.com/userFRM/photon-ring/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/userFRM/photon-ring/actions/workflows/ci.yml)

**Broadcast messaging for Rust, where every subscriber sees every message and no
subscriber can slow another one down.**

## Who this is for

You have one producer (or a few) and several consumers that must each see the
whole stream — market data fanned out to a pricing engine, a risk check and a
recorder; telemetry to several sinks; a staged pipeline. The messages are small
and fixed-shape, they arrive faster than a lock-based channel can move them, and
you care about the nanoseconds.

If instead each message should be handled by exactly one of N workers, you want a
work queue, not this. If your consumers are async tasks that idle most of the
time, use your runtime's own channel.

## What you get that you would otherwise build yourself

**Consumers with different delivery contracts, on the same ring.** Backpressure
exists so a consumer that must not lose messages can stop the producer. But a
consumer that is only *observing* should never be able to do that:

```rust
let (mut tx, subs) = channel_bounded::<Order>(1024, 0);

let mut risk      = subs.subscribe();        // gates the publisher, loses nothing
let mut telemetry = subs.subscribe_lossy();  // never gates it, drops when behind
```

`risk` keeps its no-loss guarantee. `telemetry` is invisible to the publisher's
backpressure scan, so however slow it gets it cannot stall order flow — and when
it falls behind it reports `Lagged { skipped }` with an exact count rather than
silently missing data. Both read the same sequence numbers, so an observation can
be tied to the message the risk engine processed.

**A consumer that dies releases the ring** instead of wedging it: its subscriber
drops as the thread unwinds, and the publisher continues. **Consumers can attach
to a ring that is already running,** so a debug tap goes on and comes off a live
system without perturbing it.

Both properties fall out of the design — each slot carries its own validation
stamp, so subscribers never share a barrier and therefore never contend.

**Two payload models.** `channel()` and friends take `T: Pod` — fixed-shape
plain data, validated optimistically, no copying beyond the message itself.
[`event_channel()`](#choosing-a-ring) takes any `Send` type, including `String`,
`Vec`, enums and `Option`: slots own their values and are mutated in place, so
steady-state publishing allocates nothing.

It is `no_std` compatible with `alloc`, and the concurrency protocols are gated
in CI by Miri, loom and a TLA+ model.

> [!IMPORTANT]
> The default `channel()` is lossy on overflow: the publisher never blocks, and
> slow subscribers detect drops via `TryRecvError::Lagged`. If lossless delivery
> matters more than raw latency, use `channel_bounded()`.

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

## Choosing a ring

Every ring broadcasts: each subscriber sees each message, with a private cursor and no shared read barrier. What varies is the payload family and the delivery contract.

| Constructor | Payload | Delivery contract |
|---|---|---|
| `channel` | `T: Pod` | Lossy. The publisher never blocks; a subscriber that falls behind observes `Lagged { skipped }`. The fastest path. |
| `channel_bounded` | `T: Pod` | Per consumer. `subscribe()` gates the publisher and loses nothing; `subscribe_lossy()` taps the same ring and can never stall it. |
| `channel_mpmc` | `T: Pod` | Lossy, many producing threads. There is no bounded MPMC ring — see design constraints. |
| `event_channel` | Any `T: Send` (`String`, `Vec`, enums) | Every subscriber gates the publisher. Slots are factory-built once and mutated in place; steady state copies and allocates nothing. |
| `Photon<T>` / `TypedBus` | `T: Pod` | String-keyed topics, each an independent lossy ring. `Photon` fixes one payload type for the whole bus; `TypedBus` allows one per topic. |
| `topology::Pipeline` / `topology::Consumer` | `T: Pod` | Dedicated OS threads wired with lossy rings between stages; shutdown, drain, and panic capture handled. |

Sequence numbers are shared by every subscriber on a ring, so consumers with different contracts — a gating risk engine, a lossy telemetry tap — can correlate observations of the same message.

## Installation

```toml
[dependencies]
photon-ring = "2.5.0"
```

Optional features:

- `derive`: enables `#[derive(photon_ring::DerivePod)]` for user-defined `Pod` types.
- `hugepages`: enables Linux memory controls such as `mlock`, `prefault`, and NUMA helpers.
- `atomic-slots`: enables a data-race-free slot implementation that decomposes the payload into `AtomicU64` stripes (stepping down through `AtomicU32`/`U16`/`U8` for a trailing partial stripe) instead of `write_volatile`/`read_volatile`. Zero performance cost on x86-64. On ARM64 both paths pay the same reader-side acquire fence, so `atomic-slots` costs nothing extra there either. Eliminates the formal undefined behavior the default path carries under the Rust abstract machine, **for payloads with no padding bytes**. Its multi-threaded tests run under Miri in CI (`miri (atomic-slots)`), so the claim is machine-checked rather than asserted. Padding is the remaining gap: a type such as `(u8, u64)` has 7 uninitialized bytes, and reading those as part of an atomic word is itself undefined. Use `#[repr(C)]` with explicit padding fields (as the examples do) so every byte is initialized.

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
6. fence(Acquire)                 (payload read completes before re-check)
7. s2 = stamp.load(Relaxed)
8. if s1 == s2 -> return
9. else -> retry
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

### Fanout scaling

Delivering one message to N consumers is O(N) work somewhere. The question is
where it lands, and whether the producer pays it.

Time to publish one message and have all N consumers observe it, in nanoseconds.
Single-threaded and in-cache, so this measures protocol overhead rather than
real cross-core fanout latency:

| N consumers | 1 | 2 | 4 | 8 | 16 | 32 | marginal |
|---|---|---|---|---|---|---|---|
| **Photon Ring** | **3.9** | **5.9** | **8.5** | **14.9** | **25.9** | **50.2** | **1.5 ns/consumer** |
| Shared-ring broadcast | 47.7 | 65.5 | 99.2 | 167.3 | 306.9 | 579.6 | 17.2 ns/consumer |
| Point-to-point queue, one per consumer | 22.3 | 44.3 | 87.8 | 176.5 | 351.3 | 701.9 | 21.9 ns/consumer |
| Point-to-point queue, one per consumer (alt) | 27.4 | 53.9 | 108.7 | 217.6 | 428.4 | 859.3 | 26.8 ns/consumer |

The marginal figure is what matters. A subscriber here costs about one cursor
read and one stamp check, because subscribers share no state — so the producer's
work does not grow with the audience. The other shapes pay per-consumer
coordination: a shared-ring broadcast clones through common state, and a
point-to-point queue is not broadcast at all, so fanning out means the producer
sends once per consumer.

Read that last row as *what broadcast costs on a queue that does not do
broadcast*, not as a race those libraries lost — they solve the different and
equally real problem of exactly one receiver owning each message. Reproduce with
`cargo bench --bench fanout_scaling`.

### Core operations

```
                                    i7-10700KF     M1 Pro
                                    ──────────     ──────
  Publish only                        2.8 ns      2.4 ns
  Roundtrip (1 sub, same thread)      2.7 ns      8.8 ns
  Fanout (10 independent subs)       17.0 ns     27.7 ns
  MPMC (1 pub, 1 sub)                12.1 ns     10.6 ns
  Empty poll                          0.9 ns      1.1 ns
  Batch 64 + drain                    158 ns      282 ns
  Struct roundtrip (24B Pod)          4.8 ns      9.3 ns
  Cross-thread roundtrip               95 ns      130 ns
  One-way latency (RDTSC)             48 ns p50     —
```

### Throughput

- **Sustained throughput:** about 300M msg/s on Intel and 88M msg/s on M1 Pro
- **Payload scaling:** at cache-line-sized payloads the copy is a few percent of latency — cross-core cache-coherence transfer dominates. The copy only becomes co-dominant in the KiB range; see [`docs/payload-scaling.md`](docs/payload-scaling.md)

## Delivery contracts in detail

The guarantees sketched above have edges worth knowing before you rely on them.

A dying consumer releases the ring **provided its `Subscriber` drops with it** —
automatic when the consumer thread owns the subscriber. One parked in long-lived
shared state (an `Arc`'d registry, a supervisor struct) outlives its consumer and
keeps gating the publisher, so don't do that with a tracked subscriber. A merely
*wedged* consumer still applies backpressure; that is the guarantee working.

The no-loss guarantee belongs to each tracked subscriber's lifetime, not to the
ring. If the last tracked subscriber goes away while lossy ones remain, nothing
gates the publisher any more and the bounded ring behaves like a lossy one.

`subscribe_from_oldest()` is the exception on a bounded ring: it starts at a
sequence the publisher was already entitled to overwrite, so its history may be
lapped, and it registers a full ring behind — which gates the publisher until it
drains. Use `subscribe()` unless you specifically want the retained history.

`cargo run --release --example degradation` demonstrates the slow-observer and
dead-consumer scenarios; `tests/degradation.rs` asserts them.

## At a glance

| Capability | Photon Ring |
|---|---|
| **Delivery** | Broadcast — every subscriber sees every message |
| **Publish** | 2.8 ns lossy, 7.96 ns bounded with a live consumer |
| **Cross-thread roundtrip** | 95 ns |
| **Throughput** | ~300M msg/s sustained |
| **Per-consumer contracts** | Gating and non-gating subscribers on one ring |
| **Consumer failure** | A dead consumer releases the publisher |
| **Hot attach** | Subscribe to and detach from a running ring |
| **Topologies** | Pipelines, fan-out, managed terminal consumers |
| **Topic bus** | Named topics, and a typed bus for per-topic payload types |
| **Backpressure** | Optional, per subscriber |
| **`no_std`** | Yes, with `alloc` |
| **Multi-producer** | Yes |

Photon Ring is for streams where every subscriber should observe every message with minimal coordination between them. If instead each message should be owned by exactly one receiver, a work queue is the better shape.

## API overview

Channels are the lowest-level interface. `channel::<T>(capacity)` creates the fastest single-producer path and returns a `Publisher<T>` plus a cloneable `Subscribable<T>`. `channel_bounded::<T>(capacity, watermark)` adds optional backpressure; `Publisher::try_publish` returns `PublishError::Full(value)` instead of overwriting unread slots. `channel_mpmc::<T>(capacity)` returns `MpPublisher<T>`, which is `Clone + Send + Sync` and uses atomic sequence claiming for concurrent producers. On the write side, the important APIs are `publish`, `publish_with` for in-place construction, `publish_batch` on `Publisher`, and `published`/`capacity` for lightweight counters.

Subscribers are independent and contention-free by default. `Subscribable::subscribe()` starts from future messages only, while `subscribe_from_oldest()` starts at the oldest message still retained in the ring. `Subscriber<T>` exposes `try_recv`, `recv`, `recv_with`, `latest`, `pending`, `recv_batch`, and `drain`, plus observability counters through `total_received`, `total_lagged`, and `receive_ratio`.

For topic routing, `Photon<T>` provides a string-keyed bus where all topics share the same payload type, and `TypedBus` allows a different `T: Pod` per topic. Both lazily create topics and expose `publisher`, `try_publisher`, `subscribe`, and `subscribable`. `publisher()` will panic if the publisher for that topic was already taken, and `TypedBus` also panics on type mismatches for an existing topic.

Pipelines build dedicated-thread processing graphs on supported OS targets. `topology::Pipeline::builder().capacity(...).input::<T>()` returns an input publisher plus a typed builder; `.then(...)` chains stages, `.fan_out(...)` creates a diamond, `.then_a(...)` and `.then_b(...)` extend either branch, and `.build()` returns the final subscriber plus a `Pipeline` handle. `then_with(f, WaitStrategy)` (and `then_a_with`, `then_b_with`) lets you configure the wait strategy for each pipeline stage. The handle supports `shutdown`, `join`, `panicked_stages`, `is_healthy`, and `stage_count`. For manual shutdown outside topology, use `Shutdown`.

The `#[derive(photon_ring::DeriveMessage)]` macro supports a `#[photon(as_enum)]` attribute for fields whose types are `#[repr(u8)]` enums. Unrecognized types without this attribute now produce a compile error instead of being silently assumed to be enums.

Ring capacity accepts any integer >= 2. Power-of-two capacities use bitwise `seq & mask` for zero-overhead indexing; arbitrary capacities use Lemire reciprocal-multiply fastmod (~1.5 ns).

Wait behavior is explicit. `recv_with` accepts `WaitStrategy::BusySpin`, `YieldSpin`, `BackoffSpin`, `Adaptive`, `MonitorWait`, or `MonitorWaitFallback` depending on whether you want the absolute lowest wakeup latency or better core sharing. `MonitorWait` uses Intel UMONITOR/UMWAIT (Alder Lake+) for near-zero power wakeup (~30 ns), with automatic fallback to PAUSE on older x86 or WFE on ARM; construct it safely via `WaitStrategy::monitor_wait(&stamp)`. `MonitorWaitFallback` uses TPAUSE without requiring an address. On supported platforms, the crate also includes `affinity` helpers for CPU pinning; with the `hugepages` feature on Linux, you can use `Publisher::mlock`, `Publisher::prefault`, and `mem::{set_numa_preferred, reset_numa_policy}` to reduce page-fault and NUMA noise.

### Companion crates

- **[`photon-ring-async`](photon-ring-async/)** — Runtime-agnostic async wrappers. `AsyncSubscriber` with yield-based polling and configurable spin budget. Works with tokio, smol, embassy, or any executor.
- **[`photon-ring-metrics`](photon-ring-metrics/)** — Observability wrappers with `SubscriberMetrics` (snapshot/delta tracking) and `PublisherMetrics`. Framework-agnostic — bring your own prometheus/opentelemetry.

## Design constraints

| Constraint | Rationale |
|---|---|
| `T: Pod` | Every bit pattern must be valid, which makes optimistic torn reads safe to reject. |
| Capacity >= 2 | Any capacity works. Power-of-two uses `seq & mask`; arbitrary capacity uses Lemire fastmod (~1.5 ns, zero-division). |
| Single producer by default | The fastest path relies on `&mut self` rather than write-side atomics. |
| Lossy overflow by default | The publisher never blocks; subscribers detect drops through `Lagged`. |
| MPMC is lossy-only | Backpressure gates the publisher on per-subscriber trackers, which the multi-producer claim path does not consult. For lossless delivery use `channel_bounded` (single producer). |
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
| **Miri** | Flags multi-threaded tests | **Passes, enforced in CI** |
| **x86-64 cost** | Baseline | **Zero** — identical `MOV` instructions |
| **ARM64 cost** | One `DMB ISHLD` reader fence (both paths) | Same fence — no additional cost |
| **Precedent** | Same pattern as Linux kernel seqlocks (20+ years) | Per-word atomic decomposition, as in `atomic-memcpy` |

> [!NOTE]
> Both implementations place an `Acquire` fence between the payload read and the
> stamp re-check — the same barrier the Linux kernel's `read_seqcount_retry()`
> carries as `smp_rmb()`. Without it an acquire *load* is one-way and a weakly
> ordered CPU may satisfy the payload read after the re-check has validated,
> which would return data from a later overwrite. On x86 the fence emits no
> instruction — TSO already orders load-load — but it is still a compiler
> barrier, and measured at roughly +1.2 ns on the same-thread roundtrip
> microbenchmark because it forbids reordering the optimiser was otherwise
> free to do. That is the price of the guarantee, on every architecture.
>
> With that fence in place, the default volatile implementation produces correct
> results on real hardware; what remains is that it is a data race under Rust's
> abstract machine, which Miri reports and no compiler has yet exploited. Enable
> `atomic-slots` for a build free of that race — machine-checked in CI, and free
> on x86-64.

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
RUSTFLAGS="--cfg loom" cargo test --release --test loom_mpmc --test loom_backpressure  # exhaustive interleaving checks
cargo run --release --example market_data
cargo run --release --example pipeline
cargo run --release --example backpressure
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
