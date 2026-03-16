# Roadmap

## v0.4.0 — Production Hardening

### Core Affinity Helpers
- [ ] `PinnedPublisher<T>` / `PinnedSubscriber<T>` wrappers that pin the calling
  thread to a specific CPU core via `core_affinity`
- [ ] NUMA-aware ring allocation — allocate the ring buffer on the local NUMA node
  of the publisher core using `libnuma` / `set_mempolicy`
- [ ] Benchmark with pinning enabled vs disabled on multi-socket Xeon

### Backpressure Mode
- [ ] `channel_bounded()` — publisher blocks when the slowest consumer is within
  `N` slots of being lapped (configurable watermark)
- [ ] `PublishError::Full` return type for non-blocking backpressure
- [ ] Preserves the default lossy `channel()` for market-data use cases
- [ ] Formal proof that backpressure mode cannot deadlock (single-producer guarantee)

### Wait Strategies
- [ ] `WaitStrategy` enum: `BusySpin`, `Yield`, `Park`, `Adaptive`
- [ ] `BusySpin` — bare spin (current `recv()` phase 1), lowest latency
- [ ] `Yield` — `thread::yield_now()` between spins, SMT-friendly
- [ ] `Park` — `thread::park()` / `unpark()`, near-zero CPU when idle
- [ ] `Adaptive` — spin → yield → park escalation with configurable thresholds
- [ ] Apply to both `Subscriber::recv()` and `SubscriberGroup::recv()`

## v0.5.0 — HFT-Grade Infrastructure

### Kernel-Bypass Integration
- [ ] Optional `io_uring` eventfd notification for `Park` wait strategy
- [ ] Example: DPDK → Photon Ring → strategy threads pipeline
- [ ] Example: Solarflare ef_vi → Photon Ring fan-out

### Memory Control
- [ ] Huge page allocation (2MB / 1GB) via `mmap` + `MAP_HUGETLB`
- [ ] `mlock` / `mlockall` for ring buffer pages (prevent page faults on hot path)
- [ ] Cache line padding verification at compile time (`static_assert` on `Slot` size)
- [ ] Pre-fault all ring pages on construction

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
