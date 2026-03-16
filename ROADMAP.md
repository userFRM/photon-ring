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

## v0.5.0 — HFT-Grade Infrastructure

### Kernel-Bypass Integration
- [ ] Example: DPDK → Photon Ring → strategy threads pipeline
- [ ] Example: Solarflare ef_vi → Photon Ring fan-out

### Memory Control
- [ ] Huge page allocation (2MB / 1GB) via `mmap` + `MAP_HUGETLB`
- [ ] `mlock` / `mlockall` for ring buffer pages (prevent page faults on hot path)
- [ ] Cache line padding verification at compile time (`static_assert` on `Slot` size)
- [ ] Pre-fault all ring pages on construction
- [ ] NUMA-aware allocation via `set_mempolicy`

### Observability
- [ ] Per-subscriber lag counters (total drops, max lag depth)
- [ ] Publisher sequence watermark (exportable to Prometheus / StatsD)
- [ ] Optional RDTSC-stamped latency histogram (compile-time feature gate)
- [ ] `SubscriberGroup::aligned_count()` already exists — extend to per-cursor stats

## v0.6.0 — Advanced Patterns

### Multi-Producer Support
- [ ] `channel_mpmc()` — CAS-based sequence claiming for multi-producer
- [ ] Benchmark MPMC vs SPMC to quantify the CAS overhead
- [ ] Document when SPMC (current) vs MPMC is appropriate

### Consumer Topologies
- [ ] Pipeline: `A → B → C` (consumer B publishes to a second ring)
- [ ] Diamond: `A → {B, C} → D` (fan-out then fan-in)
- [ ] Provide topology builder or document the pattern with examples

### Typed Topic Bus
- [ ] `Photon::topic::<T>(name)` — heterogeneous topics with type-erased storage
- [ ] Compile-time topic registration via const generics or inventory

## Future / Research

### Platform-Specific Optimizations
- [ ] `UMWAIT` / `TPAUSE` support on Intel Alder Lake+ (user-mode cache line monitor)
- [ ] ARM `WFE` / `SEV` for efficient cross-core notification on Apple Silicon
- [ ] RISC-V `WRS` (wait-on-reservation-set) when available

### Formal Verification
- [ ] TLA+ model of the seqlock-stamped ring protocol
- [ ] Loom-based concurrency testing (when loom supports seqlock patterns)
- [ ] Property-based testing with proptest for lag detection edge cases

### Academic Publication
- [ ] Technical report documenting the stamp-in-slot design and benchmark methodology
- [ ] Comparison with LMAX Disruptor, Aeron, Chronicle Queue
- [ ] Submit to a systems venue (OSDI, ATC, EuroSys workshops)
