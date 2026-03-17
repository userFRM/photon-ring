# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.1.1] - 2026-03-17

### Fixed
- Removed unnecessary `unsafe` from `__cpuid_count` (safe since Rust 1.94).
- Fixed clippy `manual-is-multiple-of` lint in backpressure example.

### Changed
- MSRV bumped to 1.94.
- CI now auto-publishes to crates.io on tagged releases via `CARGO_REGISTRY_TOKEN`.

## [2.1.0] - 2026-03-17

### Fixed
- **MPMC `catch_up_cursor` deadlock:** After ring wraparound, a late producer
  could strand the shared cursor permanently because `catch_up_cursor` used
  exact stamp equality (`!=`) instead of `<`. Once a successor slot was reused
  by a later sequence, the cursor would never advance, and all subscribers would
  spin on `Empty` forever. Changed to `stamp < done_stamp` to match
  `advance_cursor`'s existing wraparound-safe check.

### Added
- **`MonitorWait` wait strategy:** UMONITOR/UMWAIT on Intel Tremont+/Alder Lake+
  with runtime CPUID WAITPKG detection. Near-zero power, ~30 ns wakeup latency.
  Falls back to PAUSE on unsupported x86 CPUs, SEVL+WFE on aarch64.
  Safe constructor: `WaitStrategy::monitor_wait(&AtomicU64)`.
- **`MonitorWaitFallback` wait strategy:** TPAUSE (timed C0.1 pause) without
  address monitoring. Same CPUID gating and platform fallbacks.
- **Prefetch on all publish paths:** `PREFETCHT0` (x86) / `PRFM PSTL1KEEP` (ARM)
  prefetches the next slot's cache line before writing the current slot, hiding
  the Read-For-Ownership stall. Applied to SPMC and MPMC publish, publish_with.

### Changed
- **Seqlock uses `write_volatile`/`read_volatile`:** Replaces `ptr::write`/
  `ptr::read` in `Slot::write` and `Slot::try_read`. Eliminates formal UB when
  a reader observes a partially-written slot. Zero measurable runtime cost.
- **Cached `has_backpressure` on Publisher:** Avoids Arc deref + Option check on
  every `publish()` for lossy channels.
- **`recv_with()` direct slot access:** Pre-computes slot pointer and expected
  stamp outside the spin loop, eliminating per-iteration `try_recv()` overhead.
  Applied to both `Subscriber` and `SubscriberGroup`.
- **Removed dead `count` field from `SubscriberGroup`:** Was always equal to
  the const generic `N`. `aligned_count()` now returns `N` directly.
- **WFE in MPMC predecessor spin:** On aarch64, the contended `advance_cursor`
  path now uses WFE (low-power sleep until cache-line event) instead of YIELD.

## [2.0.0] - 2026-03-17

### Breaking Changes
- **`T: Copy` replaced with `unsafe trait Pod`** across the entire public API.
  `Pod` requires every bit pattern to be valid, excluding `bool`, `char`,
  `NonZero*`, and references. Pre-implemented for all numeric primitives,
  arrays of `Pod`, and tuples up to 12 elements. User structs require
  `unsafe impl photon_ring::Pod for MyStruct {}`.

### Added
- **`Pod` marker trait** (`src/pod.rs`): enforces seqlock-safe payloads at the
  type level, not just in documentation.
- **`photon-ring-derive` crate** (optional `derive` feature):
  `#[derive(photon_ring::DerivePod)]` generates `unsafe impl Pod` with
  compile-time field verification.
- **`try_publisher()`** on `Photon<T>` and `TypedBus`: returns `Option`
  instead of panicking when the publisher was already taken.
- **`docs/benchmark-methodology.md`**: full benchmark reproducibility
  documentation (hardware, OS, toolchain, Criterion config, caveats).
- **Verification README strengthened**: explicit SPMC-only, SC-only, no-MPMC
  limitations documented.
- Tuple `Pod` impls extended to arity 12 (matching `std`).

## [1.0.1] - 2026-03-17

### Changed
- Updated benchmark numbers for both machines (Intel + M1 Pro).
- Added project banner.
- Throughput reported with variance ranges.

## [1.0.0] - 2026-03-16

### Added
- **`topology::Pipeline` builder:** First-class pipeline topology API with
  `Pipeline::builder().input::<T>().then(|x| transform(x)).build()`.
  Supports chained stages on dedicated threads, fan-out via `.fan_out()`,
  and graceful shutdown. Gated to platforms with OS thread support.
- **`recv_batch(&mut self, buf: &mut [T]) -> usize`:** Batch receive for
  `Subscriber` and `SubscriberGroup`. Handles lag transparently (retries
  after cursor advancement).
- **`drain()` iterator:** Yields all currently available messages. Handles
  lag by retrying instead of stopping.
- **`Shutdown` signal type:** `Arc<AtomicBool>` wrapper for coordinating
  graceful termination of consumer loops. Clone-able, `no_std` compatible.
- **`publish_with` closure API:** Enables in-place construction in the slot,
  letting the compiler elide the write-side memcpy.
- **Payload scaling benchmark** (`benches/payload_scaling.rs`) with matplotlib
  chart (`docs/images/payload-scaling.png`) comparing Photon Ring vs Disruptor
  across 8B to 4KB payloads.
- **Raw pointer caching** in Publisher, Subscriber, SubscriberGroup, MpPublisher
  for ~2 ns savings on hot path (eliminates Arc → Box pointer chain).

### Performance
- SubscriberGroup: O(1) fanout (2.8 ns regardless of N, was 5.3 ns for N=10)
- try_read: happy-path-first branch order (~0.5-1.5 ns improvement)
- Backpressure tracker: Relaxed atomics (saves ~1 ns on ARM)
- Bus topic lookup: no String allocation on hit (~50-300 ns savings)

## [0.9.0] - 2026-03-16

### Performance (Codex-recommended optimizations)
- **SubscriberGroup: O(1) fanout.** Replaced `[u64; N]` cursor array with single
  `u64` cursor. Group fanout is now 2.8 ns regardless of N (was 5.3 ns for N=10).
  Per-subscriber marginal cost: **0 ns** (was 0.2 ns).
- **try_read happy-path-first branch order.** Stamp-match check is now the first
  branch, improving branch prediction by ~0.5-1.5 ns/recv.
- **Backpressure tracker: Relaxed atomics.** Changed tracker loads from Acquire to
  Relaxed (sufficient for min-computation). Saves ~1 ns on ARM.
- **Bus topic lookup: no String allocation on hit.** `publisher()`/`subscribe()`
  now use `&str` lookup via hashbrown's `Equivalent` trait. String allocation
  only on first topic creation. Saves ~50-300 ns/call.
- **`publish_with` closure API.** Enables in-place construction in the slot,
  letting the compiler elide the write-side memcpy. Zero overhead.
- **`Slot::write_with` internal method** for closure-based seqlock writes.

## [0.8.0] - 2026-03-16

### Added
- **`TypedBus`:** Heterogeneous topic bus — different `T: Copy + Send + 'static`
  per topic. `bus.publisher::<f64>("prices")`, `bus.subscribe::<u32>("volumes")`.
  Panics on type mismatch for safety.
- **SPMC vs MPMC benchmark:** Side-by-side comparison in `benches/throughput.rs`.
  SPMC: 2.8 ns, MPMC: 11.7 ns (4.2x CAS overhead — consistent with Disruptor).
- **ARM `WFE` instruction:** `BackoffSpin` and `YieldSpin` now use `WFE` on
  aarch64 for lower-power spin-wait (vs `YIELD`/`PAUSE` on x86).
- **UMWAIT/TPAUSE documentation:** Documented as future optimization for
  Intel Tremont+ in `wait.rs`.
- **TLA+ formal specification:** `verification/seqlock.tla` models the seqlock
  protocol with `NoTornRead` safety property. Includes `MC.tla` model config
  and README with instructions for running TLC model checker.
- **Technical report outline:** `docs/technical-report.md` — structured outline
  for an academic paper covering design, evaluation, and comparison with
  Disruptor, Aeron, and Chronicle Queue.

## [0.7.0] - 2026-03-16

### Added
- **Multi-producer support:** `channel_mpmc()` returns `MpPublisher<T>` (Clone + Send +
  Sync) that uses CAS-based sequence claiming. Multiple threads can publish concurrently.
  Ordered cursor advancement ensures consumers see messages in sequence order.
  `MpPublisher::publish()` takes `&self` (not `&mut self`).
- **Pipeline example** (`examples/pipeline.rs`): Three-stage processing chain
  (raw ticks → enrichment → signal generation) demonstrating how to chain
  multiple Photon Ring channels.
- **Diamond example** (`examples/diamond.rs`): Fan-out to two filter stages,
  fan-in to an aggregator, demonstrating parallel processing topologies.
- **NUMA-aware allocation** (`hugepages` feature, Linux): `mem::set_numa_preferred(node)`
  and `mem::reset_numa_policy()` via `set_mempolicy` syscall. Call before `channel()`
  to place the ring on the publisher's NUMA node.
- License changed to Apache-2.0 only.

## [0.6.0] - 2026-03-16

### Fixed (Codex-reported critical issues)
- **`publish()` now enforces backpressure** on bounded channels. Previously only
  `try_publish()` checked — `publish()` and `publish_batch()` bypassed it silently.
  Now `publish()` spin-waits for room on bounded channels.
- **Subscriber Drop deregisters backpressure tracker.** Dropping a subscriber on a
  bounded channel no longer leaves a stale cursor that blocks the publisher forever.
- **`SubscriberGroup` participates in backpressure.** Groups now register a tracker
  and update it with the minimum cursor on each `try_recv()`.
- **`prefault()` is now `unsafe`** with documented precondition (must be called before
  any publish/subscribe operations).
- **`subscribe_group::<0>()` now panics** with a clear message instead of silently
  breaking.

### Changed
- Stale README test counts updated (40 integration + 12 unit + 10 doc-tests = 70).
- Removed invalid `--features affinity` references (no longer a feature gate).
- Fixed `affinity::pin_to_core(0)` → `affinity::pin_to_core_id(0)` in README.
- Added `rust-version = "1.70"`, `docs.rs` metadata, `exclude = [".github/"]`.

## [0.5.1] - 2026-03-16

### Added
- **GitHub Actions CI** (`.github/workflows/ci.yml`): 9 jobs covering check,
  test, clippy, fmt, miri, cross-platform (Linux/macOS/Windows), wasm32,
  no-default-features, and hugepages feature gate.
- **Platform support matrix** in README (x86_64, aarch64, wasm32, Cortex-M).

### Changed
- Removed unnecessary `#[allow(dead_code)]` annotations from `ring.rs`.
- All docstrings verified objective (no domain-specific jargon).
- All 11 `.rs` source files have SPDX license headers.

## [0.5.0] - 2026-03-16

### Added
- **Memory control** (`hugepages` feature, Linux): `Publisher::mlock()` locks ring
  pages in RAM, `Publisher::prefault()` pre-faults all pages. `mem::mmap_huge_pages()`
  for 2MB huge page allocation. Compile-time `Slot` alignment assertion.
- **Observability counters:** `Subscriber::total_received()`, `total_lagged()`,
  `receive_ratio()` on both `Subscriber` and `SubscriberGroup`. Zero-cost — plain
  `u64` fields incremented on the fast path.
- **`Publisher::sequence()`** alias for computing subscriber lag.
- **`examples/pinned_latency.rs`** — core-pinned RDTSC latency measurement demo.
- **`examples/backpressure.rs`** — reliable order fill pipeline with `channel_bounded`.

## [0.4.1] - 2026-03-16

### Changed
- Kill `std` feature entirely. All wait strategies pure `no_std`.
- Replace `Park` with `BackoffSpin` (exponential PAUSE backoff).
- Switch `core_affinity` to `core_affinity2` (no_std compatible).

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
- 40 integration tests including cross-thread SPMC and 1M-message stress test
- MIRI verification (single-threaded tests)
- Criterion benchmarks with `disruptor` v4.0.0 comparison
- Market data example (4-topic fan-out, ~160M msg/s)
- SPDX license headers on all source files
- License: Apache-2.0
