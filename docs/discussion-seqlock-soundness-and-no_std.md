# Discussion: Seqlock Soundness, Alternatives, and the no_std Question

**Date:** 2026-03-19
**Context:** Post multi-model audit (Gemini 2.5 Pro, Claude Opus 4.6, Kimi k2.5). This document captures the design discussion around the seqlock's formal UB status, what alternatives exist, and whether `no_std` is the right constraint for this project.

---

## 1. The Seqlock Problem

Photon Ring's core performance comes from the seqlock-stamped slot protocol. A reader speculatively copies a slot via `read_volatile` while a writer may be concurrently modifying it via `write_volatile`. The stamp re-check discards torn reads. `T: Pod` guarantees every bit pattern is valid, so a torn read is harmless — it's just garbage that gets thrown away.

**On real hardware (x86, ARM), this is well-defined.** `write_volatile` and `read_volatile` compile to plain `MOV` instructions. The Linux kernel's seqlock has done exactly this for 20+ years.

**Under Rust's abstract machine, this is undefined behavior.** Any concurrent access to non-atomic memory where at least one access is a write is a data race, regardless of `volatile`. Rust intentionally has no concept of "data race that produces a valid-but-unspecified value" — every data race is instant UB, full stop.

This is not a bug in photon-ring. It is an inherent limitation of implementing seqlocks in Rust. The safety comments now state this honestly:

> "Avoids practical UB on all target architectures (x86, ARM); formally still a data race under the Rust abstract machine, as volatile does not establish a happens-before relationship. Sound because T: Pod makes all bit patterns valid and the stamp re-check gates usage."

---

## 2. Can We Get Seqlock Performance Without the Formal UB?

### Kill the Assumption: "The payload is too large for atomics"

| Payload size | Hardware-atomic solution | Formal UB? |
|---|---|---|
| T ≤ 8 bytes | `AtomicU64` — plain atomic load/store | **None. Zero UB.** |
| T ≤ 16 bytes | `AtomicU128` (nightly) or inline asm `CMPXCHG16B` (x86) / `CASP` (ARM) | None with inline asm |
| T ≤ 64 bytes | Intel `MOVDIR64B` (Sapphire Rapids+) — 64-byte atomic store | No atomic 64B *load* exists |
| T > 64 bytes | **Nothing.** No hardware provides atomic loads wider than 16 bytes. | Seqlock or bust |

For the most common use case — market data ticks (price: f64, volume: u32, timestamp: u64 = 20 bytes) — you're already past `AtomicU128`. You *need* the seqlock pattern. There is no hardware escape hatch for 20+ byte payloads.

**Takeaway:** If your payload fits in 8 bytes, use `AtomicU64` and skip the seqlock. If it fits in 16 bytes, `AtomicU128` or inline asm works. Beyond that, there is no formally-sound alternative at this performance level.

### Kill the Assumption: "Reader and writer share the same memory"

**Double-buffer (flip-buffer):** Writer writes to slot A while readers read from slot B. Atomic flag flips which is "current." Readers always see a consistent snapshot.

- The `left-right` crate implements this with epoch-based reclamation.
- Cost: ~15-25ns overhead per read for the epoch check.
- Requires `std` (thread-local storage for epoch tracking).
- Designed for read-heavy shared data structures, not message streams.
- Loses the ring buffer — not suitable for broadcast pub/sub.

**Slot-per-subscriber (private copies):** Writer copies the message to each subscriber's private buffer. No concurrent access at all.

- O(N) writes instead of O(1). With 10 subscribers, 10x the writes.
- Cache pollution destroys performance.
- Defeats the entire purpose of SPMC broadcast.

**Sequence + staging area:** Writer writes to a private staging slot, then atomically publishes the sequence number. Reader checks sequence, copies from staging, re-checks sequence.

- This IS a seqlock. The "staging area" is the slot. The concurrent copy is still a data race. Renaming it doesn't fix it.

**Takeaway:** For T > 16 bytes, broadcast pub/sub, there is no architecture in any language that avoids the seqlock data race pattern while matching its performance.

### Kill the Assumption: "Rust's abstract machine defines the rules"

The formal UB exists only in Rust's memory model. On actual hardware:

- x86: Total Store Order (TSO) — stores are visible in order, loads see the latest store. `MOV` is well-defined even during concurrent access.
- ARM: Weaker ordering but `DMB`/`DSB` barriers (emitted by `Ordering::Release`/`Acquire`) provide the necessary guarantees. A torn read on ARM produces an architecturally-valid (if meaningless) value.

There is active work in the Rust community to close this gap:

- **`atomic_memcpy` RFC** — proposed `atomic_load_bytes` / `atomic_store_bytes` that would do element-wise atomic operations on a byte range, making seqlocks formally sound in Rust.
- **`Freeze` trait** — would formalize "types where all bit patterns are valid" at the language level (what `Pod` already does informally).

Neither exists yet. Until they do, **every seqlock in Rust has this gap.** This includes any Disruptor-pattern implementation, any shared-memory IPC, and any lock-free data structure that does optimistic reads of multi-byte payloads.

---

## 3. The Assumption Map

| Assumption | Performance locked behind it | Can it be killed? |
|---|---|---|
| "Rust's memory model is the right model" | The entire seqlock pattern | **No** — until `atomic_memcpy` lands in Rust |
| "Payload must be > 16 bytes" | Need for seqlock (vs raw atomics) | **Yes** — if you can pack your message into ≤16 bytes, use `AtomicU128` and skip the seqlock entirely |
| "One write must serve N readers" | SPMC broadcast architecture | **Yes** — but then you're not building pub/sub anymore |
| "Reader must see the latest value" | Per-slot stamp checking | **Partially** — if you tolerate stale reads, relaxed atomics on a double-buffer work |
| "Must be no_std" | Can't use `left-right`, epoch reclamation, or thread-local storage | **Yes** — if you drop `no_std`, `left-right` is a formally-sound alternative at ~15-25ns overhead |

---

## 4. The `no_std` Question

### What `no_std` buys

| Target | Reality |
|---|---|
| wasm32 | Single-threaded. A ring buffer without threads is a queue. |
| Cortex-M (32-bit ARM) | Already unsupported — no 64-bit atomics. |
| Bare-metal x86-64 | Real use case (hypervisors, unikernels), but extremely niche. |
| Custom RTOS | Maybe, but these typically bring their own sync primitives. |

### What `no_std` costs

1. **`spin::Mutex` instead of `std::sync::Mutex`** — priority inversion on every bus/backpressure operation. `spin::Mutex` wastes CPU spinning and can starve lower-priority threads. `std::sync::Mutex` parks the thread and lets the OS schedule fairly.

2. **`hashbrown` dependency** — exists solely because `std::collections::HashMap` isn't available in `no_std`. Identical functionality, extra dependency in the tree.

3. **Racy WAITPKG caching** — using `AtomicU8` with a benign data race for the CPUID feature detection cache, instead of `std::sync::OnceLock` which is clean, correct, and zero-cost after init.

4. **Can't use `left-right` or epoch-based patterns** — these would eliminate the formal seqlock UB entirely for users who need formal soundness, but they require thread-local storage which is `std`-only.

5. **The topology module already requires `std`** — it has `extern crate std` and spawns OS threads. Same for affinity (OS APIs) and hugepages (Linux syscalls). So `no_std` only applies to the core ring/channel/bus layer. The most useful features already violate the constraint.

### The contradiction

The target users (market data, telemetry, real-time pipelines) all run on Linux/macOS/Windows servers with a full OS. The features that make photon-ring actually useful (pipelines, affinity pinning, hugepages, UMWAIT) all require OS support. The `no_std` core is carrying a constraint that the rest of the crate already violates.

### Recommendation: Hybrid approach

Keep `no_std` on the core ring/channel layer (it's already working and costs nothing on the hot path), but stop letting it constrain cold-path decisions.

Use `cfg(feature = "std")` to gate better implementations:

- Default `std` feature **on** — most users get `std::sync::Mutex`, `OnceLock`, standard `HashMap`.
- `no_std` users opt in by disabling default features — they accept `spin::Mutex` and `hashbrown`.
- Hot path is identical either way — it uses no `std` primitives (all raw atomics and volatile).
- Zero performance regression for anyone.

This keeps the door open for bare-metal x86-64 (hypervisors, unikernels) without taxing every Linux user with `spin::Mutex` priority inversion and extra dependencies.

---

## 5. What Would Make Photon Ring "Battle-Tested"

The multi-model audit and fixes addressed code-level correctness. What remains for production confidence:

| Gap | What it would take |
|---|---|
| MPMC formal verification | Extend the TLA+ spec to cover the CAS + predecessor stamp + catch-up protocol, or use `loom`/`shuttle` for exhaustive concurrency testing |
| Fuzz testing | `proptest` or `cargo-fuzz` on the ring protocol with random timing, capacities, subscribe/unsubscribe patterns |
| Production usage data | Months of real-world deployment under varied load patterns — latency spikes, memory pressure, NUMA effects, rapid subscriber churn |
| Seqlock formal soundness | Wait for Rust's `atomic_memcpy` RFC to land, then adopt it. Until then, the UB is an inherent language limitation shared by every seqlock implementation in Rust. |
| `std` feature gate migration | Move cold-path primitives behind `cfg(feature = "std")` per the recommendation above |

---

## 6. Conclusion

`T: Pod` is necessary but not sufficient for photon-ring's performance. The speed comes from seven design decisions stacking multiplicatively:

1. **Seqlock-in-slot co-location** — one cache miss instead of two (the biggest win)
2. **Per-consumer cursors** — zero read-side contention
3. **`T: Pod`** — enables the seqlock pattern by making torn reads harmless
4. **Raw pointer caching** — eliminates Arc deref chains on hot path
5. **Single-producer `&mut self`** — zero write-side atomics
6. **PREFETCHW** — hides Read-For-Ownership stalls
7. **Platform-native wait instructions** — WFE (ARM), UMWAIT (Intel)

The seqlock's formal UB under Rust's abstract machine is not fixable today. It is not a photon-ring problem — it is a Rust language problem. Every seqlock, Disruptor, and shared-memory IPC implementation in Rust has the same gap. The hardware semantics are well-defined. The language spec will catch up (`atomic_memcpy`). Until then, photon-ring documents the gap honestly and relies on the same hardware guarantees that the Linux kernel has relied on for two decades.

The `no_std` constraint is defensible for the core ring layer but should not constrain cold-path decisions. A hybrid `std`-default approach would give most users better primitives while preserving bare-metal compatibility for the niche that needs it.

---

## 7. Update: `atomic-slots` Feature Implemented

The ASCL design proposed by the constraint-anchored analysis has been implemented
as the `atomic-slots` feature flag. Benchmarks confirm the research prediction:
zero performance regression on x86-64 (identical MOV instructions). On ARM64,
one extra DMB barrier in the reader (~5-10ns).

Users who need formal soundness can opt in via:
```toml
photon-ring = { version = "2.4.0", features = ["atomic-slots"] }
```
