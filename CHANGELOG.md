# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **`event_channel` — a ring for payloads that are not `Pod`.** `String`, `Vec`,
  enums, `Option` and `bool` are ordinary payloads here. Slots own their values,
  built once by a factory and mutated in place rather than overwritten, so
  nothing is copied into or out of the ring and steady-state publishing of
  heap-owning payloads allocates nothing — a `String` field reuses the capacity
  it had the previous time round the ring.

  The `Pod` bound exists so that a reader racing a writer sees a harmless torn
  value instead of undefined behaviour. That race is only possible when the
  publisher may overwrite a slot a subscriber has not read, which cannot happen
  on a bounded ring where every subscriber is registered for backpressure. With
  that established, the ring needs no seqlock either: the cursor's
  `Release`/`Acquire` pair is the whole publication edge, so there are no stamps,
  no torn-read retry and no fence on this path.

  The cost model differs from the `Pod` ring in a way worth knowing: an event
  ring costs what the publisher and subscriber actually touch, while the `Pod`
  ring costs `size_of::<T>()` on every publish and every receive regardless.
  Updating a few fields of a 4 KiB payload measures 7.9 ns against 207.7 ns for
  the copying ring; at 8 B, where there is no copy worth avoiding, the `Pod` ring
  is slightly ahead (3.8 ns against 4.1 ns). Rewriting an entire large payload
  every message collects no copy advantage.

  The trade is that every subscriber gates the publisher — there are no lossy
  observers on an event ring, because a reader that can be lapped is exactly the
  reader this design excludes. `channel()` remains for broadcast with lossy taps.
- **`topology::Consumer`** — a managed terminal consumer. Spawns a thread that
  runs a handler over every message with `(message, sequence, end_of_batch)`,
  handling shutdown, batch signalling and panic capture. `Pipeline` covers stages
  that transform and publish onward; this covers the end of the line, which
  previously meant hand-rolling a thread and a receive loop. `DrainPolicy::Drain`
  processes messages still buffered at shutdown so a clean stop loses nothing.
- **`Subscribable::subscribe_lossy()`** — a subscriber that never gates the
  publisher, even on a bounded channel. Registered subscribers keep their
  no-loss guarantee while an observer (telemetry, logging, a debug tap) shares
  the same ring without being able to stall it; when it falls behind it reports
  `Lagged { skipped }` and `receive_ratio()` shows what it sampled. Both read the
  same sequence numbers, so observations correlate with the messages a gating
  consumer processed. Previously every subscriber on a bounded ring applied
  backpressure, so mixing the two contracts required two rings and a second
  publish.
- **`Subscriber::cursor()`** — the sequence number this subscriber will read
  next, for correlating positions across subscribers on a ring.
- **`tests/loom_backpressure.rs`** — a loom model of the bounded-channel
  backpressure guarantee, covering the publisher's cached-slowest fast path and
  a subscriber registering against a live publisher. Run in CI. Reverting the
  registration fix makes it fail with the offending interleaving, so the model is
  demonstrably load-bearing rather than decorative.
- **`examples/degradation.rs` and `tests/degradation.rs`** — the slow-observer
  and dead-consumer scenarios, run and asserted rather than described.

### Removed
- **Multi-field tuple `Pod` impls.** `(A, B)` through the 12-element tuple used
  `repr(Rust)` layout, so the compiler could insert padding — `(u8, u64)` carries
  7 padding bytes. That made `channel::<(u8, u64)>()` undefined behaviour from
  entirely safe code under `atomic-slots`, using impls the crate itself provided.
  `()`, `(A,)`, arrays and primitives remain, none of which can carry padding.
  Replace a multi-field tuple payload with a `#[repr(C)]` struct with explicit
  padding fields. **Breaking.**
- **`Publisher::sequence()`** — returned exactly what `published()` returns. Use
  `published()`, whose docs now carry the lag-computation note. **Breaking**;
  this makes the next release semver-major.
- **`SubscriberGroup`, `subscribe_group`, and `AsyncSubscriberGroup`.** All `N`
  logical subscribers shared one cursor, so the type had become a newtype over
  `Subscriber` whose const `N` did nothing but echo back from `aligned_count()`
  — an API surface implying a capability that no longer existed. Replace
  `subscribe_group::<N>()` with `subscribe()`: it is the same object with the
  same cost, since a group already performed exactly one ring read regardless of
  `N`. **Breaking.**

### Changed
- **Dual-licensed under `MIT OR Apache-2.0`** (was Apache-2.0 only). `LICENSE-MIT`
  added; existing Apache-2.0 terms are unchanged and remain available.
- **`Pipeline::try_join`** now reports failures. Stage panics are captured at the
  thread boundary, so the old `Err(payload)` arm was unreachable and the method
  always returned `Ok(())` even when a stage had panicked. It now returns
  `Err(payload)` where `payload` downcasts to a `Vec<usize>` of panicked stage
  indices; the original panic payload is not recoverable by design.
  `Pipeline::join` documented accordingly — it never re-panics.
- Crate packages exclude `docs/`, `verification/`, and `scripts/`.

### Fixed
- **A newly registered subscriber could be lapped on a bounded channel.**
  `subscribe()` read the head cursor and registered its tracker as two separate
  steps. The publisher only rescans trackers when its cached slowest cursor says
  it is close to lapping, so a subscriber that registered in the gap was invisible
  to a publisher whose cached value came from a faster consumer — and could be
  overwritten before the next rescan, losing messages the bounded channel had
  promised it. Both steps now happen under the tracker lock, which orders them
  against the scan. `subscribe_from_oldest` is inherently exempt: it starts at a
  sequence the publisher was already entitled to overwrite, which registration
  cannot retroactively reserve, and this is now documented on the method.
- **Missing read-side `Acquire` fence in the default (volatile) slot read.**
  `try_read` loaded the payload, then re-checked the stamp with an acquire *load*.
  An acquire load is a one-way barrier: it stops later accesses from moving
  earlier, but leaves the hardware free to satisfy the payload read *after* the
  re-check has validated. On a weakly ordered CPU — aarch64, which this crate
  supports and benchmarks — a reader could therefore return data from a later
  overwrite as a valid read. Fixed by placing an `Acquire` fence between the
  payload read and the re-check, mirroring both the `atomic-slots` path and the
  `smp_rmb()` in the Linux kernel's `read_seqcount_retry()`. On x86 it emits no
  instruction, but it is still a compiler barrier: the same-thread roundtrip
  microbenchmark moves from ~3.0 ns to ~4.2 ns because the optimiser may no
  longer reorder across it. Correctness on weakly ordered hardware is worth it,
  and the cross-thread path — where the crate is actually used — moves ~2.8%.
- **Out-of-bounds atomic access in the `atomic-slots` payload copy.** The striped
  copy rounded the payload up to whole `AtomicU64` stripes, so for any `T` whose
  size is not a multiple of 8 — `u8`, `u16`, `u32`, and most structs — the final
  stripe read and wrote up to 7 bytes past the payload's provenance. The bytes are
  real (a slot is padded to its 64-byte alignment) so no miscompilation or
  corruption was observed in practice, but it is undefined behavior under the Rust
  abstract machine, which is precisely what this feature exists to avoid. The tail
  now steps down through `AtomicU32`/`AtomicU16`/`AtomicU8`, keeping every access
  in bounds with no change to slot layout and no extra operations in the common
  cases. Found by running Miri against the feature for the first time.
- **Documented the padding requirement for `atomic-slots`.** A payload with
  implicit padding leaves those bytes uninitialized, and reading them as part of
  an atomic word is undefined regardless of the fix above. `Pod`'s safety contract
  now states the no-padding requirement explicitly. The reachable-from-safe-code
  case is closed by removing the multi-field tuple impls (see Removed); a
  hand-written `unsafe impl Pod` for a padded struct remains the implementor's
  responsibility, which the `miri (atomic-slots)` job is there to catch.
- **`atomic-slots` is now covered by Miri in CI** (`miri (atomic-slots)` job). The
  existing `miri` job runs the default volatile slots single-threaded with all
  cross-thread tests skipped, so it could never have caught the above; the
  soundness claim was documented but ungated. Iteration counts scale down under
  `cfg(miri)`, and one lossy-ring liveness assertion is scoped to non-Miri runs
  where the scheduler makes it meaningful.
- **Two MPMC publishers could write the same slot concurrently.** Sequence
  claiming via `fetch_add` is unbounded, so with more publishes in flight than
  the ring has slots — more than `capacity` threads inside `publish` at once —
  two producers hold sequences exactly one lap apart, which is the same slot.
  The seqlock stamp only detects reader-versus-writer races, so the two writes
  could interleave into a mixture that ends up carrying a valid stamp, and a
  reader would accept it; on the default volatile path the concurrent writes are
  also a data race, reachable from safe code as
  `channel_mpmc::<u64>(2)` plus three publishing threads. A publisher now waits
  for the previous lap's write to its slot to complete before writing (one
  Acquire load of the stamp it is about to overwrite; the wait itself never
  triggers with fewer concurrent publishers than slots). Modelled exhaustively
  under loom in `tests/loom_mpmc.rs` (`writers_one_lap_apart_are_exclusive`,
  which fails on the ungated protocol), and exercised by a stress test with
  more publishers than slots.
- **`AsyncSubscriber::recv_batch` / `AsyncSubscriberGroup::recv_batch` panicked on
  a zero-length buffer** ("index out of bounds: the len is 0 but the index is 0"),
  and consumed a message it could not store. They now return `0`.
- Documented loom command was missing `RUSTFLAGS="--cfg loom"` and so ran zero
  tests.
- Payload-scaling chart and surrounding prose presented a modeled competitor curve
  as measured data; the chart now plots only measured Photon Ring numbers.
- Stale docs corrected: the `wait.rs` "UMWAIT not yet implemented" note (it is
  implemented), `photon-ring-metrics`' "power of two" capacity comment, and the
  `Pod`/`Message` derive examples missing `#[repr(C)]`.

### Internal
- Deduplicated the Lemire fastmod slot index into `RingIndex::slot`, the publisher
  construction and watermark checks, and the pipeline stage-spawn plumbing.
- Collapsed nine near-identical `Option<T>` arms in the `Message` derive.
- Publish workflow no longer masks real `cargo publish` failures with `|| true`.
- Removed an orphaned script, a dead benchmark function, and redundant
  `unsafe impl Send`s.
- Benchmarks: `photon: publish only` had no consumer attached while
  `disruptor: publish only` always runs one, so the two were not comparable.
  Added `publish, live consumer` variants (lossy and bounded) for a like-for-like
  measurement and relabeled the original.

## [2.5.0] - 2026-03-19

### Added
- **Arbitrary ring capacity:** Ring capacity no longer requires power-of-two.
  Any capacity >= 2 is supported. Power-of-two uses bitwise AND (zero regression);
  arbitrary capacity uses Lemire reciprocal-multiply fastmod (~1.5 ns).
  15 new tests including exhaustive fastmod verification.
- **Pipeline `then_with()` API:** `StageBuilder::then_with(f, WaitStrategy)`,
  `FanOutBuilder::then_a_with()`, `then_b_with()` for configurable stage wait
  behavior. Existing `then()`/`then_a()`/`then_b()` unchanged (delegate with default).
- **`#[photon(as_enum)]` derive attribute:** The Message derive macro no longer
  silently assumes unknown types are `#[repr(u8)]` enums. Unrecognized types now
  produce a compile error. Use `#[photon(as_enum)]` to explicitly mark enum fields.
  **Breaking change** for Message derive users with enum fields.
- **`photon-ring-async` crate:** Runtime-agnostic async wrappers for photon-ring
  channels. `AsyncSubscriber`, `AsyncSubscriberGroup` with yield-based polling.
  Named `RecvFuture`/`GroupRecvFuture` for `select!`/`join!` combinators.
  Configurable spin budget. No tokio dependency. 8 tests.
- **`photon-ring-metrics` crate:** Observability wrappers with `SubscriberMetrics`
  (snapshot/delta tracking), `PublisherMetrics`. Framework-agnostic. 7 tests.
- **Loom MPMC model tests:** Standalone loom model of the MPMC cursor advancement
  protocol. 4 scenarios covering 2-producer basic, contention, consumer reads,
  and cursor catch-up. Run with `RUSTFLAGS="--cfg loom" cargo test --test loom_mpmc --release`.

### Changed
- `RingIndex` struct encapsulates capacity, mask, reciprocal, and is_pow2 flag.
  Internal to the crate; no public API change.

### Breaking
- `#[derive(photon_ring::DeriveMessage)]` enum fields now require `#[photon(as_enum)]`.

## [2.4.0] - 2026-03-19

### Performance
- **PREFETCHW on x86 publisher write path:** Replaces `PREFETCHT0` (Shared
  state) with `PREFETCHW` (Exclusive state) when compiled with
  `-C target-cpu=native` on Intel Broadwell+ / all AMD x86-64. Eliminates
  the RFO stall when subscribers have the slot cache line in Shared state.
  Falls back to `PREFETCHT0` on generic builds (zero regression).
  Measured: **-10.7% fanout 1 sub, -14.6% fanout 2 subs**.
- **Multi-cache-line prefetch for large T:** `prefetch_write_next` now
  prefetches all cache lines of the next slot. The loop bound is a
  compile-time constant; LLVM fully unrolls it. Zero overhead for T ≤ 56B
  (single cache line).
- **WFE in `recv()` Phase 2 on AArch64:** `Subscriber::recv()` and
  `SubscriberGroup::recv()` now use SEVL+WFE instead of YIELD (spin_loop)
  in the power-efficient phase. Near-zero power, ~12 ns cache-line-event
  wakeup.
- **Dead `rdtsc` asm block removed:** `deadline_100us()` had a dead
  inline-asm `rdtsc` whose outputs were discarded, followed by a second
  `_rdtsc()` call. Removed (~20-25 cycles saved per UMWAIT/TPAUSE call).

### Added
- **Cross-core PREFETCHW benchmark** (`benches/prefetchw_crosscore.rs`):
  Pinned Criterion benchmark measuring publish throughput and RDTSCP
  one-way latency across same-core, HT-sibling, and cross-physical-core
  topologies.
- **`atomic-slots` feature:** Formally sound slot implementation using `AtomicU64`
  stripes instead of `write_volatile`/`read_volatile`. Eliminates the seqlock data
  race (formal UB under the Rust abstract machine) by decomposing `T: Pod` payloads
  into per-u64 atomic stores/loads. On x86-64, `AtomicU64::store/load(Relaxed)`
  compiles to identical `MOV` instructions — zero performance regression. On ARM64,
  one extra `DMB ISHLD` barrier in the reader path (~5-10ns). Miri-passable.
  `no_std` compatible. 8 new tests covering partial stripes, odd-sized payloads,
  cross-thread stress, MPMC, and bounded backpressure under atomic-slots.
- **Seqlock alternatives analysis** exploring sound alternatives to the
  seqlock-stamped slot via constraint-anchored analysis (prohibition +
  impossibility proofs), which informed the `atomic-slots` design.

## [2.3.0] - 2026-03-18

### Added
- **`#[derive(Message)]` proc macro:** Automatic Pod-compatible wire struct
  generation from normal Rust structs. Handles `bool` → `u8`, `Option<T>` →
  `{value, has}` pair (preserves `Some(0)`, float precision, u128/i128),
  `#[repr(u8)]` enums → `u8`, `usize` → `u64` transparently. Generates
  `{Name}Wire` struct + `From` conversions + `unsafe impl Pod`. Structs with
  enum fields use `unsafe fn into_domain()` instead of safe `From`.
- 17 tests for Message derive (roundtrip, bool, option zero/none/max, enum,
  arrays, publish, float precision, u128/i128/usize/isize options).

### Changed
- Refactored `channel.rs` (1451 lines) into `channel/` module directory (7 files).
- Refactored `topology.rs` (817 lines) into `topology/` module directory (4 files).
- No API or behavior changes from the refactor.

## [2.2.0] - 2026-03-17

### Added
- **`DependencyBarrier`:** Consumer dependency graphs — downstream subscribers
  can gate their reads behind upstream subscribers, enabling ordered
  multi-stage processing pipelines.
  - `DependencyBarrier::from_subscribers(&[&Subscriber])` — creates a barrier
  - `Subscriber::try_recv_gated(&barrier)` — non-blocking gated receive
  - `Subscriber::recv_gated(&barrier)` — blocking gated receive
  - `Subscribable::subscribe_tracked()` — creates a subscriber with cursor
    tracker on lossy channels (needed for dependency graphs)
  - `Subscriber::tracker()` — exposes the cursor tracker for barrier construction
- 10 new tests covering single/multi upstream, cross-thread, bounded channels

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
