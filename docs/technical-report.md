<!--
  Copyright 2026 Photon Ring Contributors
  SPDX-License-Identifier: Apache-2.0
-->

# Photon Ring: Seqlock-Stamped Ring Buffers for Sub-100ns Inter-Thread Messaging

## Abstract

We present Photon Ring, a single-producer multi-consumer (SPMC) message
passing library for Rust that achieves sub-100 nanosecond one-way
inter-thread latency on commodity x86_64 hardware. The design co-locates
a seqlock stamp with its payload in a single cache line, eliminating the
extra cache miss that plagues traditional sequence-barrier designs. We
describe the stamp-in-slot protocol, its safety properties under the Rust
memory model, and show that per-slot seqlocks combined with per-consumer
cursors yield constant-time, zero-allocation publish and receive
operations. Benchmarks on an Intel i7-10700KF demonstrate 48 ns p50
one-way latency and 0.2 ns per-subscriber fanout cost with batched
subscriber groups -- a 5.5x improvement over independent consumers.

## 1. Introduction

### 1.1 The inter-thread communication bottleneck

[Discuss how inter-thread messaging latency is the critical path in
low-latency systems: high-frequency trading, real-time audio/video
processing, game engines. The limiting factor is cache-coherence protocol
round-trip time, not software overhead.]

### 1.2 The LMAX Disruptor and sequence barriers

[Review the LMAX Disruptor (Trotter 2011), which popularised ring
buffers with sequence barriers for inter-thread messaging. Discuss
its influence on Aeron, Chronicle Queue, and the broader
mechanical-sympathy movement. Note the fundamental overhead: consumers
must load both the shared cursor and the slot data, requiring two
cache-line transfers in the worst case.]

### 1.3 Our contribution

[State the key insight: by co-locating the seqlock stamp and the
payload in the same cache line, the consumer can validate slot ownership
and read data in a single cache-line transfer. This eliminates the
sequence-barrier load on the hot path, saving one L3-to-L1 snoop per
message. Summarise the results: 48 ns one-way latency, 0.2 ns/sub
fanout slope.]

## 2. Background

### 2.1 Cache coherence and the MESI protocol

[Review the MESI (Modified, Exclusive, Shared, Invalid) coherence
protocol. Explain how a store on core A triggers an invalidation on
core B, and the subsequent load on B requires a cache-line transfer via
L3 snooping (~40-55 ns on Intel Comet Lake) or QPI/UPI for cross-socket
(~100-200 ns). This latency is the physical lower bound for any
inter-thread communication scheme.]

### 2.2 Seqlocks in the Linux kernel

[Describe the Linux kernel seqlock (seqlock_t): a writer increments a
sequence counter to odd before writing, then to even after. Readers
check the counter before and after reading; if the values differ or
are odd, the read is invalid and must be retried. Cite Bovet & Cesati
(Understanding the Linux Kernel) and the kernel documentation. Note
the restriction to plain-old-data types (no pointers, no destructors)
-- directly analogous to our `T: Copy` constraint.]

### 2.3 The Disruptor pattern

[Describe the Disruptor ring buffer: a pre-allocated array of slots
indexed by a monotonically increasing sequence number via bitmask.
Publishers write to the next slot and advance a shared cursor.
Consumers track their own cursor and poll the shared cursor to
discover new messages. Discuss the sequence-barrier abstraction and
its role in dependency graphs (diamond topologies, pipeline stages).]

## 3. Design

### 3.1 Slot layout and cache-line co-location

[Describe the `Slot<T>` struct: `#[repr(C, align(64))]` with an
`AtomicU64` stamp followed by `UnsafeCell<MaybeUninit<T>>`. For
`T` up to 56 bytes, both stamp and payload reside in a single 64-byte
cache line. The stamp encoding: `seq*2+1` = writing, `seq*2+2` = done,
`0` = never written. Discuss why `align(64)` is critical: prevents
false sharing between adjacent slots.]

### 3.2 Seqlock write/read protocol

[Detail the write protocol:
  1. `stamp.store(seq*2+1, Relaxed)` -- mark slot as writing
  2. `fence(Release)` -- ensure odd stamp is visible before data
  3. `ptr::write(value)` -- copy payload into slot
  4. `stamp.store(seq*2+2, Release)` -- mark slot as done

Detail the read protocol:
  1. `s1 = stamp.load(Acquire)`
  2. If `s1` is odd, return None (write in progress, spin)
  3. If `s1 != seq*2+2`, wrong sequence (empty or lagged)
  4. `value = ptr::read(slot.value)`
  5. `s2 = stamp.load(Acquire)`
  6. If `s1 == s2`, return Some(value) (consistent read)
  7. Else return None (torn read, retry)]

### 3.3 Per-consumer cursors

[Explain that each `Subscriber` holds a private `cursor: u64` tracking
the next sequence to read. No atomic operations on the cursor in the
common case -- it is a plain local variable. This eliminates all
consumer-to-consumer contention. The shared `ring.cursor` is only read
on the lag-detection slow path, not the hot path (since v0.2.0).]

### 3.4 Stamp-only fast path (v0.2.0)

[Describe the key optimisation: instead of loading the shared cursor to
check for new messages, the consumer goes directly to the expected slot
and checks its stamp. If `stamp == seq*2+2`, the message is ready and
can be read without touching the shared cursor. The shared cursor is
only consulted when the stamp indicates lag (stamp > expected), which
triggers the slow path in `Subscriber::read_slot`. This reduces the
per-message cache traffic from two cache-line loads to one.]

### 3.5 SubscriberGroup batched fanout

[Describe `SubscriberGroup<T, N>`: a const-generic type that holds
N cursors and performs a single seqlock read when all cursors are
aligned (the common case). The stamp check and value read happen once;
the N cursor increments are a compiler-unrolled loop. This reduces
fanout cost from O(N) seqlock reads to O(1) seqlock read + O(N) cursor
increments. Measured: 0.2 ns/sub vs 1.1 ns/sub for independent
subscribers.]

### 3.6 Backpressure and bounded channels

[Describe `channel_bounded(capacity, watermark)`: per-subscriber cursor
tracking via `Arc<Padded<AtomicU64>>` in a `Mutex<Vec<...>>`.
Publisher scans all trackers to find the slowest consumer. Fast path:
cached `slowest` cursor avoids re-scanning on every publish. The
lossy `channel()` has zero overhead -- no tracker allocation, no
backpressure check.]

### 3.7 Multi-producer extension

[Describe `channel_mpmc()` and `MpPublisher`: sequence numbers are
claimed via `fetch_add` on a shared atomic counter. Cursor advancement
uses a CAS spin loop to ensure strict ordering -- each producer waits
until all prior sequences have committed before advancing the cursor.
This is the standard Disruptor multi-producer protocol adapted for
the seqlock stamp encoding.]

## 4. Implementation

### 4.1 Rust memory model considerations

[Discuss the mapping of the seqlock protocol to Rust atomics:
- `Relaxed` store for the odd stamp (paired with a subsequent `Release`
  fence, forming a store-release)
- `Release` store for the even stamp (ensures data write is visible
  before the done stamp)
- `Acquire` load for both stamp reads (ensures stamp is loaded before
  data read, and data read before second stamp check)
- Why `fence(Release)` between the odd stamp and the data write:
  a `Relaxed` store + `Release` fence is equivalent to a `Release`
  store, but separates the stamp write from the fence for clarity.

Discuss the memory model concern: reading `T` via `ptr::read` while
another thread may be concurrently writing via `ptr::write` is, strictly
speaking, a data race under the C++20/Rust memory model. The seqlock
detects this after the fact by re-checking the stamp, but the read
itself is technically undefined behaviour. This is a known limitation
shared with the Linux kernel's seqlock implementation. In practice,
no compiler or hardware reorders in a way that breaks this pattern
for `Copy` types, and MIRI (under `-Zmiri-disable-data-race-check`)
does not flag it.]

### 4.2 T: Copy constraint and torn-read safety

[Explain why `T: Copy` is a hard requirement:
- A torn read produces a bit pattern that may not be a valid `T`. For
  `Copy` types, this is harmless: the invalid value is detected by the
  stamp re-check and discarded. No destructor runs on the torn value.
- For non-Copy types (e.g., `String`, `Vec`), a torn read could produce
  an invalid pointer that, when dropped, causes a double-free or
  use-after-free. The `Copy` bound eliminates this entire class of bugs.
- `MaybeUninit<T>` is used for the slot value to avoid requiring `Default`
  and to defer initialisation until the first write.]

### 4.3 no_std compatibility

[Describe the `no_std` + `alloc` design:
- `hashbrown` for the topic bus hash map (no `std::collections`)
- `spin::Mutex` for internal locks (no `std::sync::Mutex`)
- `core_affinity2` for CPU pinning (no `std::thread`)
- All wait strategies use `core::hint::spin_loop()` and inline assembly
  (`WFE` on aarch64), never OS primitives like `futex` or `park/unpark`
- The only optional `std` dependency is `libc` for the `hugepages`
  feature (mmap, mlock, set_mempolicy)]

## 5. Evaluation

### 5.1 Benchmark methodology

[Describe the benchmarking setup:
- Hardware: Intel i7-10700KF (Comet Lake, 8C/16T, 3.8 GHz base)
- OS: Linux 6.x, isolcpus for publisher and subscriber cores
- Core pinning via `affinity::pin_to_core_id()`
- Criterion.rs for microbenchmarks, custom RDTSC harness for one-way
  latency
- Message type: `u64` (8 bytes, fits in cache line with stamp)
- Ring size: 64 slots (4 KB, fits in L1d)]

### 5.2 Single-threaded microbenchmarks

[Present same-thread publish+recv roundtrip latency:
- v0.1.0: 3.2 ns
- v0.2.0 (stamp-only fast path): 2.5 ns (-22%)
- Comparison: `crossbeam-channel` ~25 ns, `bus` ~15 ns]

### 5.3 Cross-thread latency (RDTSC)

[Present one-way publisher-to-subscriber latency measured via RDTSC
timestamps embedded in the message payload:
- p50: 48 ns
- min: 34 ns
- p99: 66 ns
- p999: ~120 ns
- Roundtrip (2 x one-way): 96 ns
- Discuss that 34 ns approaches the L3 snoop latency floor (~40 ns on
  Comet Lake), indicating near-zero software overhead.]

### 5.4 Comparison with related libraries

[Present comparative benchmarks:
- `disruptor-rs` v4.0.0: cross-thread roundtrip comparison
- `crossbeam-channel`: bounded MPMC comparison
- `bus`: SPMC broadcast comparison
- Discuss methodology: same hardware, same core pinning, same message
  type, Criterion.rs for all]

### 5.5 Fanout scaling analysis

[Present per-subscriber overhead as a function of subscriber count:
- Independent subscribers: 1.1 ns/sub (linear scaling)
- SubscriberGroup: 0.2 ns/sub (5.5x improvement)
- Fanout 10 subs: 14 ns (independent) vs 4.3 ns (group)
- Discuss why: group amortises the seqlock read over N cursors,
  reducing cache traffic from O(N) to O(1)]

## 6. Related Work

### 6.1 LMAX Disruptor (Java)

[The original Disruptor pattern (Trotter 2011). Sequence barriers,
event processors, ring buffer. Influence on the entire field. Photon
Ring borrows the ring buffer + bitmask indexing but replaces sequence
barriers with per-slot seqlocks.]

### 6.2 Aeron (Java/C++)

[Real Logic's Aeron: IPC and network messaging. Uses term buffers
(log-structured ring buffers) rather than slot-based rings. Designed
for network I/O, not pure inter-thread messaging.]

### 6.3 Chronicle Queue (Java)

[OpenHFT's Chronicle Queue: memory-mapped, persistent message queue.
Designed for replay and audit trails. Higher latency than in-memory
ring buffers but supports disk-backed persistence.]

### 6.4 disruptor-rs (Rust)

[Rust port of the Disruptor pattern. Uses sequence barriers and
busy-spin waiting. Photon Ring's stamp-in-slot design eliminates the
separate barrier load, reducing per-message latency.]

### 6.5 bus (Rust)

[Jonhoo's `bus` crate: SPMC broadcast channel using epoch-based
synchronisation. Simple API but higher per-message overhead than
seqlock-based designs.]

### 6.6 crossbeam (Rust)

[crossbeam-channel: general-purpose MPMC channel. Excellent ergonomics
and correctness but not optimised for the SPMC broadcast case.
Significantly higher latency than dedicated ring buffers for the
single-producer fanout pattern.]

## 7. Limitations and Future Work

### 7.1 Seqlock memory model UB

[The fundamental limitation: concurrent `ptr::read` and `ptr::write`
on the same memory location is undefined behaviour under the C++20 and
Rust memory models, even though the seqlock detects torn reads after
the fact. This is the same situation as the Linux kernel's seqlock.
Possible mitigations:
- Atomic memcpy (proposed for C++26, not yet in Rust)
- Per-byte atomics (impractical for large T)
- Compiler barriers + volatile reads (current practical approach)
- Acceptance as "benign UB" (current status in Linux kernel)]

### 7.2 T: Copy restriction

[The `Copy` bound prevents using Photon Ring with heap-allocated types
(`String`, `Vec`, `Box`). Workarounds:
- Use indices or handles instead of owned values
- Use `ArrayString` / `ArrayVec` for bounded-size data
- Future: explore `ManuallyDrop<T>` + explicit epoch-based reclamation]

### 7.3 Multi-producer CAS overhead

[The `MpPublisher` uses a CAS spin loop for cursor advancement, which
serialises all producers on the cursor cache line. Under high contention,
this becomes the bottleneck. Possible improvements:
- Batched CAS (claim N sequences at once)
- Per-producer sub-rings with a merging consumer
- FAA-based cursor (relaxing strict ordering)]

### 7.4 UMWAIT/TPAUSE on future hardware

[Intel Tremont+ CPUs support `UMWAIT` and `TPAUSE` instructions for
user-mode cache-line monitoring. These would allow near-zero latency
wakeup without burning CPU -- the consumer monitors the stamp's cache
line and wakes on invalidation. Requires CPUID feature detection
(WAITPKG flag). Not yet implemented but would replace the two-phase
spin in `recv()` with a single `UMONITOR`/`UMWAIT` pair.]

### 7.5 Formal verification

[The TLA+ specification in `verification/seqlock.tla` verifies safety
(no torn reads) and liveness (no starvation) under sequential
consistency. Extending this to weaker memory models (TSO, ARM) would
require tools like LKMM, CDSChecker, or GenMC. Additionally, the
multi-producer CAS protocol is not yet modelled.]

## 8. Conclusion

[Summarise the contributions:
1. Stamp-in-slot co-location: eliminates one cache-line transfer per
   message on the consumer hot path.
2. SubscriberGroup batched fanout: O(1) seqlock reads for N consumers.
3. Full `no_std` implementation in safe Rust (modulo the seqlock
   `ptr::read`/`ptr::write` pattern).
4. Sub-100 ns one-way latency on commodity hardware, approaching the
   cache-coherence protocol floor.
5. TLA+ formal verification of the core safety properties.

Photon Ring demonstrates that careful cache-line-aware design, combined
with the Rust type system's `Copy` bound for torn-read safety, can
achieve inter-thread messaging latencies within a small constant factor
of the hardware limit.]

## References

[Placeholder for bibliography. Key references:
- Thompson, M. et al. "Disruptor: High performance alternative to
  bounded queues for exchanging data between concurrent threads." 2011.
- Bovet, D. and Cesati, M. "Understanding the Linux Kernel." O'Reilly.
- Intel. "Intel 64 and IA-32 Architectures Optimization Reference
  Manual." Chapter on UMWAIT/TPAUSE.
- Lamport, L. "Specifying Systems: The TLA+ Language and Tools for
  Hardware and Software Engineers." Addison-Wesley, 2002.
- The Rust Reference: Atomics and Memory Ordering.
- Linux kernel documentation: seqlock.rst.]
