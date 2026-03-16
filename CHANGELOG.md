# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-03-16

### Added
- **`WaitStrategy` enum:** Fully `no_std` configurable consumer wait behavior —
  `BusySpin` (zero wakeup latency), `YieldSpin` (PAUSE/YIELD instruction),
  `BackoffSpin` (exponential backoff), `Adaptive { spin_iters, yield_iters }`
  (three-phase escalation, default). No OS primitives required.
  New methods `Subscriber::recv_with(strategy)` and
  `SubscriberGroup::recv_with(strategy)`.
- **`channel_bounded()` with backpressure:** `try_publish()` returns
  `Err(PublishError::Full(value))` when the ring is full instead of overwriting.
  Per-subscriber cursor tracking with publisher-side min-scan on the slow path.
  Zero overhead on the default lossy `channel()`.
- **Core affinity helpers** (`affinity` feature, default on, `no_std` via
  `core_affinity2`): `affinity::pin_to_core()`, `affinity::pin_to_core_id()`,
  `affinity::available_cores()`. Critical for HFT core placement.
- **ROADMAP.md** with v0.4.0–v0.6.0 plan and future research directions.

### Removed
- **`std` feature:** Eliminated entirely. All wait strategies, backpressure,
  and core affinity are pure `no_std` + `alloc`. Zero `std` dependency.

## [0.3.0] - 2026-03-16

### Added
- **`SubscriberGroup<T, N>`:** Const-generic batched multi-consumer type that reads
  the ring once and sweeps all `N` cursor increments in a compiler-unrolled loop.
  Reduces per-subscriber fanout cost from ~1.1 ns to ~0.2 ns (5.5x slope reduction).
  API: `Subscribable::subscribe_group::<N>()`, with `try_recv()`, `recv()`,
  `pending()`, and `aligned_count()` methods.
- **Two-phase spin in `recv()`:** 64 bare-spin iterations (zero wakeup latency),
  then `PAUSE`-based spin (power efficient). On Skylake+, `PAUSE` adds ~140 cycles
  per iteration — the bare-spin phase avoids this when the message arrives quickly.
- **RDTSC one-way latency benchmark** (`benches/rdtsc_oneway.rs`, x86_64 only):
  Embeds TSC timestamps in message payload, measures true publisher-to-consumer
  latency without signal-back overhead. Confirmed p50 = 48 ns one-way on i7-10700KF.

### Performance
- SubscriberGroup fanout 10 subs: **4.3 ns** (vs 13.3 ns independent = 3.1x faster)
- Fanout slope: **0.2 ns/sub** (vs 1.1 ns/sub = 5.5x improvement)
- One-way latency (RDTSC): **48 ns p50**, 34 ns min, 66 ns p99
- Cross-thread roundtrip: **96 ns** (confirmed = 2 × ~48 ns cache line transfers)

## [0.2.0] - 2026-03-16

### Changed
- **Stamp-only fast path:** `try_recv()` no longer reads the shared cursor on the
  hot path. The consumer goes directly to the slot stamp, eliminating one cache line
  transfer. The cursor is only consulted on the lag-detection slow path.
- **Simplified seqlock read:** replaced `fence(Acquire) + load(Relaxed)` with a single
  `load(Acquire)` for the torn-read verification stamp check. Equivalent on x86,
  gives the compiler more optimization freedom.
- **Tight spin in `recv()`:** `recv()` now spins directly on the target slot's stamp
  instead of calling `try_recv()` in a loop. Reduces per-iteration overhead.

### Performance
- Cross-thread latency: 110 ns → **98 ns** (-11%)
- Same-thread roundtrip: 3.2 ns → **2.5 ns** (-22%)
- Fanout 10 subs: 20 ns → **14 ns** (-32%)

## [0.1.0] - 2026-03-16

### Added
- Core SPMC channel: `channel()`, `Publisher<T>`, `Subscriber<T>`, `Subscribable<T>`
- Seqlock-stamped ring buffer with cache-line-aligned slots (`#[repr(C, align(64))]`)
- Per-subscriber cursor (zero contention between consumers)
- `try_recv()`, `recv()` (busy-spin), `latest()` (skip to newest)
- `publish_batch()` for amortized cursor updates
- `subscribe_from_oldest()` for replay from oldest available message
- `pending()` (capped at ring capacity) and `published()` queries
- Lag detection via `TryRecvError::Lagged { skipped }` with head-cursor-based computation
- Named-topic bus: `Photon<T>` with `publisher()`, `subscribe()`, `subscribable()`
- Full `no_std` support (requires `alloc`) using `hashbrown` and `spin`
- 26 correctness tests including cross-thread SPMC and 1M-message stress test
- MIRI verification (22 single-threaded tests)
- Criterion benchmarks with `disruptor` v4.0.0 comparison
- Market data example (4-topic fan-out, ~160M msg/s)
- SPDX license headers on all source files
- Dual license: MIT OR Apache-2.0
