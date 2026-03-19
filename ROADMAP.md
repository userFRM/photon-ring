# Roadmap

## v0.4.0 — Production Hardening (DONE)

### Core Affinity Helpers (DONE)
- [x] `affinity::pin_to_core()`, `pin_to_core_id()`, `available_cores()`, `core_count()`
- [x] `no_std` compatible via `core_affinity2` crate (no `std` dependency)
- [x] Re-exports `CoreId` for direct use
- [ ] NUMA-aware ring allocation (v0.5.0)
- [ ] Benchmark with pinning enabled vs disabled on multi-socket Xeon (v0.5.0)

### Backpressure Mode (DONE)
- [x] `channel_bounded(capacity, watermark)` — non-lossy bounded channel
- [x] `Publisher::try_publish()` → `Err(PublishError::Full(value))` when ring is full
- [x] Per-subscriber cursor tracking with publisher-side min-scan on slow path
- [x] Zero overhead on default lossy `channel()` (backpressure is `Option::None`)
- [x] 9 backpressure-specific tests including cross-thread correctness

### Wait Strategies (DONE)
- [x] `WaitStrategy` enum — fully `no_std`, no OS primitives
- [x] `BusySpin` — bare spin, zero wakeup latency
- [x] `YieldSpin` — `core::hint::spin_loop()` (PAUSE on x86, YIELD on ARM)
- [x] `BackoffSpin` — exponential backoff with increasing PAUSE bursts
- [x] `Adaptive { spin_iters, yield_iters }` — three-phase escalation (default)
- [x] `recv_with(strategy)` on both `Subscriber` and `SubscriberGroup`

## v0.5.0 — HFT-Grade Infrastructure (DONE)

### Memory Control (DONE)
- [x] Huge page allocation (2MB) via `mmap` + `MAP_HUGETLB` (`mem::mmap_huge_pages`)
- [x] `Publisher::mlock()` — lock ring pages in RAM (prevent swap/page faults)
- [x] `Publisher::prefault()` — pre-fault all ring pages on demand
- [x] Cache line alignment verified at compile time (`const_assert` on `Slot`)
- [ ] NUMA-aware allocation via `set_mempolicy` (v0.6.0)

### Observability (DONE)
- [x] `Subscriber::total_received()` / `total_lagged()` / `receive_ratio()`
- [x] `SubscriberGroup::total_received()` / `total_lagged()` / `receive_ratio()`
- [x] `Publisher::sequence()` — current sequence for lag computation
- [ ] Optional RDTSC-stamped latency histogram (v0.6.0)
- [ ] Prometheus / StatsD export (v0.6.0)

### Examples (DONE)
- [x] `examples/pinned_latency.rs` — core-pinned RDTSC latency measurement
- [x] `examples/backpressure.rs` — reliable order fill pipeline
- [ ] Example: DPDK → Photon Ring pipeline (v0.6.0)
- [ ] Example: Solarflare ef_vi → Photon Ring fan-out (v0.6.0)

## v0.7.0 — Advanced Patterns (DONE)

### Multi-Producer Support (DONE)
- [x] `channel_mpmc()` — CAS-based sequence claiming for multi-producer
- [x] `MpPublisher<T>` — Clone + Send + Sync, `&self` publish
- [x] Ordered cursor advancement (consumers see messages in sequence)
- [ ] Benchmark MPMC vs SPMC to quantify the CAS overhead (future)

### Consumer Topologies (DONE)
- [x] Pipeline: `A → B → C` — `examples/pipeline.rs`
- [x] Diamond: `A → {B, C} → D` — `examples/diamond.rs`

### NUMA-Aware Allocation (DONE)
- [x] `mem::set_numa_preferred(node)` — set memory policy for ring allocation
- [x] `mem::reset_numa_policy()` — reset to system default

### Typed Topic Bus (DONE)
- [x] `TypedBus` with `publisher::<T>(name)` / `subscribe::<T>(name)`
- [x] Type-erased storage via `Box<dyn Any>`, panics on type mismatch

## v0.8.0 — Research & Formal Methods (DONE)

### Platform-Specific Optimizations (DONE)
- [x] ARM `WFE` instruction in `BackoffSpin` and `YieldSpin` (aarch64)
- [x] UMWAIT/TPAUSE implemented as `MonitorWait`/`MonitorWaitFallback` with runtime CPUID detection
- [x] SPMC vs MPMC benchmark comparison (2.8 ns vs 11.7 ns, 4.2x CAS overhead)
- [ ] RISC-V `WRS` (wait-on-reservation-set) when available

### Formal Verification (DONE)
- [x] TLA+ model of the seqlock-stamped ring protocol (`verification/seqlock.tla`)
- [x] `NoTornRead` safety property verified in spec
- [x] MC.tla model config + README with TLC instructions
- [x] **`atomic-slots` feature:** Formal soundness gap closed. Seqlock data race
  (UB under Rust abstract machine) eliminated by decomposing `T: Pod` payloads into
  `[AtomicU64; N]` stripes. Zero performance regression on x86-64 (identical MOV
  instructions). Miri-passable. See `docs/research-seqlock-alternatives.md` for the
  constraint-anchored analysis that produced this design.
- [ ] Loom-based concurrency testing (when loom supports seqlock patterns)
- [ ] Property-based testing with proptest

### Academic Publication (DONE)
- [x] Technical report outline (`docs/technical-report.md`)
- [x] Structured for comparison with Disruptor, Aeron, Chronicle Queue
- [ ] Full paper text (future)
- [ ] Submit to a systems venue (future)
