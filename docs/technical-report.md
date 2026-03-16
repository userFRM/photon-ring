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

In concurrent systems -- from high-frequency trading engines and real-time audio pipelines to game simulation loops -- inter-thread message passing lies on the critical path of nearly every latency-sensitive operation. A pervasive misconception holds that software complexity (lock acquisition, memory allocation, context switching) is the primary obstacle to low-latency messaging. In reality, for well-designed lock-free data structures, the dominant cost is the cache-coherence protocol round-trip imposed by the hardware itself.

Modern multicore processors maintain the illusion of shared memory through coherence protocols, most commonly MESI (Modified, Exclusive, Shared, Invalid) and its extensions [1]. When a producer thread on core A writes a message, the cache line containing that message transitions to the Modified state in A's private L1 cache. Before a consumer thread on core B can read that message, the coherence protocol must transfer the cache line from A's cache hierarchy to B's. On Intel processors using a ring-bus L3 interconnect (e.g., Comet Lake), this transfer requires a snoop request to traverse the L3 ring, locate the modified line, write it back, and deliver a copy to the requesting core -- a process that takes approximately 40--55 nanoseconds for intra-socket transfers [2, 3]. On multi-socket systems connected via QPI or UPI, the penalty rises to 100--200 nanoseconds per transfer.

This coherence latency represents a hard physical floor. No software optimization -- however clever the data structure or however relaxed the memory ordering -- can deliver an inter-thread message faster than the time required for a single cache-line transfer between cores. For a naive messaging scheme that touches two cache lines per message (one for the data, one for a shared control variable), the floor doubles. The central challenge in low-latency inter-thread communication, therefore, is not to eliminate synchronization overhead in the abstract, but to minimize the number of cache-line transfers on the critical path.

### 1.2 The LMAX Disruptor and its limitations

The LMAX Disruptor [4], introduced by Thompson, Farley, and Barker in 2011, represented a landmark in the mechanical-sympathy approach to concurrent systems design. By replacing traditional bounded queues with a pre-allocated ring buffer indexed by monotonically increasing sequence numbers via bitmask, the Disruptor eliminated per-message allocation and reduced memory access patterns to a predictable, cache-friendly stride. The design has profoundly influenced the broader ecosystem: Real Logic's Aeron [5] adopted similar principles for network IPC, OpenHFT's Chronicle Queue [6] extended the pattern to persistent, memory-mapped messaging, and the mechanical-sympathy philosophy permeated the design of low-latency systems across the industry.

The Disruptor's core abstraction is the *sequence barrier*: a shared atomic cursor that the producer advances after writing each slot, and that consumers poll to discover new messages. Consumers maintain their own cursors and can be arranged in dependency graphs (pipeline stages, diamond topologies) via barrier chaining. Batching emerges naturally -- a consumer that falls briefly behind discovers multiple new messages in a single barrier read and processes them in a burst, amortizing the cost of the barrier poll.

Despite its elegance, the Disruptor's reliance on sequence barriers introduces a structural overhead that cannot be eliminated within its design framework. On the consumer's hot path, receiving a single message requires *two* cache-line transfers in the worst case: first, the consumer loads the shared sequence barrier to determine that a new message is available; second, it loads the slot containing the message payload. If the barrier and the slot reside on different cache lines -- which they almost always do, since the barrier is a single atomic variable shared among all consumers while slots are distributed across the ring -- the consumer pays two L3 snoop latencies per message. On a Comet Lake processor, this amounts to approximately 80--110 nanoseconds of irreducible coherence traffic for a single receive operation.

Furthermore, the shared cursor itself becomes a point of contention. Every consumer reads the same cache line containing the producer's sequence number. While read-shared cache lines do not generate invalidation traffic under MESI (they remain in the Shared state), the initial transition from Modified to Shared -- triggered by the producer's write -- invalidates the line in every consumer's cache simultaneously. For N consumers, this produces N snoop responses on the L3 ring bus, serializing the cache-line distribution and adding latency proportional to the consumer count.

### 1.3 Our contribution

We present Photon Ring, a Rust library for single-producer multi-consumer (SPMC) inter-thread messaging that eliminates the sequence-barrier load from the consumer hot path. The key insight is *stamp-in-slot co-location*: by embedding a seqlock sequence stamp directly in the same `#[repr(C, align(64))]` slot structure as the message payload, both the ownership metadata and the data itself reside within a single 64-byte cache line (for payloads up to 56 bytes). The consumer validates slot ownership and reads the message in one cache-line transfer, reducing the per-message coherence traffic from two L3 snoops to one.

The design makes three further contributions beyond co-location:

**Per-consumer cursors with no shared state on the read path.** Each subscriber holds a private, non-atomic `u64` cursor tracking the next expected sequence number. Unlike the Disruptor, where consumers must load a shared barrier to detect new messages, Photon Ring consumers go directly to the expected slot and check its stamp. The shared producer cursor is consulted only on the lag-detection slow path -- when the stamp indicates that the slot has been overwritten -- not on the common-case fast path. This eliminates all consumer-to-consumer and consumer-to-producer contention on the hot path.

**The `T: Copy` constraint as a safety invariant.** Photon Ring requires that message types implement `Copy`, which precludes heap-allocated types (`String`, `Vec`, `Box`) but enables a critical safety property: torn reads -- where the consumer reads a partially overwritten slot -- produce a bit pattern with no destructor, no pointer dereference, and no validity-invariant violation (for recommended payload types). The seqlock stamp check detects the inconsistency, and the torn value is silently discarded. This is the same principle underlying the Linux kernel's `seqlock_t`, where the restriction to plain-old-data types ensures that speculative reads cannot corrupt kernel state.

**Full `no_std` compatibility.** The entire library, including the named-topic bus (`Photon<T>`), heterogeneous-type bus (`TypedBus`), batched subscriber groups (`SubscriberGroup<T, N>`), and all wait strategies, operates without the Rust standard library. The implementation depends only on `alloc` (for `Arc`, `Box`, and `Vec` at channel construction time) and two lightweight dependencies: `hashbrown` for the topic bus hash map and `spin` for internal mutexes. Wait strategies use `core::hint::spin_loop()` and inline assembly (`WFE` on aarch64), never OS primitives such as `futex` or thread parking.

Benchmarks on an Intel Core i7-10700KF (Comet Lake, 8 cores, 3.8 GHz base) demonstrate 48 ns median one-way latency (measured via RDTSC timestamps embedded in the message payload), 96 ns cross-thread roundtrip latency, and a publish cost of approximately 2.8 ns per message. The 48 ns one-way figure is within 20% of the bare L3 snoop latency on this microarchitecture, indicating near-zero software overhead above the cache-coherence floor. The `SubscriberGroup<T, N>` batched fanout mechanism further reduces per-subscriber overhead from 1.1 ns (independent subscribers) to 0.2 ns (grouped subscribers), a 5.5x improvement achieved by performing a single seqlock read and sweeping N cursor increments in a compiler-unrolled loop.

## 2. Background

### 2.1 Cache coherence protocols

Modern symmetric multiprocessors maintain the abstraction of a single shared address space across multiple cores, each with private L1 and L2 caches and a shared last-level cache (typically L3). The *coherence protocol* ensures that no core observes stale data: every store by one core must eventually become visible to loads by all other cores, and no two cores may simultaneously hold a cache line in states that permit conflicting accesses.

The MESI protocol [7, 8], used by Intel processors, assigns each cache line one of four states. **Modified**: the line is dirty and present only in this core's cache; the core may read and write it without bus traffic. **Exclusive**: the line is clean and present only in this core's cache; a write transitions it to Modified without invalidation. **Shared**: the line is clean and may be present in multiple caches; a write requires an invalidation broadcast. **Invalid**: the line is not present in this cache. AMD processors extend MESI to MOESI, adding an **Owned** state that allows a core to supply dirty data directly to a requesting core without writing back to L3, reducing certain transfer latencies.

The critical path for inter-thread communication is the Modified-to-Shared transition. When a producer writes a value (placing the cache line in Modified state in core A's L1), and a consumer on core B subsequently loads from the same address, the following sequence occurs: (1) core B issues a load that misses in L1 and L2; (2) the L3 cache slice responsible for that address receives the request; (3) a snoop is sent to core A (the owner in Modified state); (4) core A writes the dirty line back to L3 and transitions its copy to Shared (under MESI) or Invalid (if exclusive transfer is requested); (5) L3 delivers the line to core B, which installs it in Shared state. On Intel desktop processors with a ring-bus L3 interconnect (Skylake through Comet Lake), the end-to-end latency for this sequence is approximately 40--55 nanoseconds, dominated by the ring-bus traversal time, which depends on the physical distance between the requesting core's L3 slice and the owner's slice [2, 3]. Intel's optimization manual reports unloaded core-to-core latency of 42 ns for a Modified-to-Shared snoop on Comet Lake [3]. Under load, contention on the ring bus can push this to 70 ns or higher.

On server-class processors with mesh interconnects (Skylake-SP and later Xeon), intra-socket coherence latency is similar (40--70 ns) but exhibits higher variance due to the non-deterministic routing of snoop requests through the mesh. Cross-socket transfers over UPI (Ultra Path Interconnect) add a further 60--150 ns, bringing the total to 100--200 ns per cache-line transfer [2]. This disparity makes NUMA-aware placement -- allocating the ring buffer on the same NUMA node as the publisher -- essential for cross-socket messaging.

For the design of inter-thread messaging primitives, the key implication is that each cache-line transfer on the hot path costs approximately 40--70 ns. A messaging scheme that requires the consumer to load two distinct cache lines per message (e.g., a shared cursor and a data slot) pays twice this cost, setting a floor of approximately 80--140 ns. Reducing the hot path to a single cache-line load halves the theoretical minimum.

### 2.2 Seqlocks in the Linux kernel

The seqlock, introduced in Linux 2.5.60 and formalized as `seqlock_t` in the kernel's synchronization primitives [9, 10], is a reader-writer synchronization mechanism optimized for workloads where reads vastly outnumber writes. Unlike reader-writer locks (`rwlock_t`), where readers acquire a shared lock that blocks writers, a seqlock allows readers to proceed without acquiring any lock at all. Readers instead perform an optimistic read-and-verify protocol that detects concurrent writes after the fact.

The mechanism centers on a sequence counter. A writer increments the counter to an odd value before modifying the protected data, performs the modification, then increments the counter to an even value. A reader samples the counter before reading, copies the protected data, then samples the counter again. If the two counter values are equal and even, the read is consistent -- no write occurred during the read window. If they differ, or the first value is odd (indicating a write in progress), the reader discards the copied data and retries. The protocol is defined in `include/linux/seqlock.h`:

```
Writer:                           Reader:
  write_seqlock(&seq);              do {
  // modify protected data            s = read_seqbegin(&seq);
  write_sequnlock(&seq);              // copy protected data
                                    } while (read_seqretry(&seq, s));
```

The crucial property of this protocol is that the reader never writes to shared memory. The sequence counter is only written by the writer; readers perform loads exclusively. This means multiple readers produce zero cache-line invalidation traffic among themselves -- a property that distinguishes seqlocks from reader-writer locks, where even read-lock acquisition writes to a shared counter and generates coherence traffic proportional to the reader count.

The restriction to plain-old-data types (POD, or trivially copyable types in C++ terminology) is fundamental to the correctness of the pattern. Because the reader copies data that may be concurrently modified by the writer, the read can produce a *torn* value -- a bit pattern that is neither the old value nor the new value, but an arbitrary interleaving of bytes from both. For POD types, this torn value is harmless: it is detected by the sequence counter check and discarded. No destructor runs, no pointer is dereferenced, and no resource is leaked. For non-POD types containing pointers or RAII handles, a torn read could produce an invalid pointer that, when subsequently dereferenced or freed, causes memory corruption. The Linux kernel's seqlock documentation [10] explicitly warns against protecting pointer-containing structures with seqlocks.

This POD restriction maps directly to Rust's `Copy` trait. Types implementing `Copy` are guaranteed to be bitwise-copyable, to have no `Drop` implementation, and to contain no references that could be invalidated by a torn read. Photon Ring's `T: Copy` constraint is the Rust-idiomatic expression of the same safety invariant that the Linux kernel enforces through convention and code review.

Under the C++20 and Rust memory models, the seqlock pattern occupies an uneasy position. The concurrent non-atomic read (by the reader) and non-atomic write (by the writer) to the same memory location constitute a *data race* as defined by the memory model, which is undefined behavior regardless of whether the result is subsequently discarded. The Linux kernel addresses this by operating outside the C abstract machine (relying on compiler barriers and hardware memory ordering guarantees rather than the language-level model). Proposals such as P1478R7 [11] to the C++ standards committee aim to provide language-level support for seqlock-safe reads, but as of this writing no such facility exists in either C++ or Rust. This tension between the formal model and the practical hardware behavior is a known limitation shared by all seqlock implementations, including Photon Ring.

### 2.3 The Disruptor pattern

The Disruptor [4], developed at LMAX Exchange for their foreign-exchange trading platform, introduced a ring-buffer-based messaging pattern that has become the de facto standard for low-latency inter-thread communication in Java and, subsequently, in other languages. The design's influence extends well beyond its original implementation: the concepts of mechanical sympathy, false-sharing avoidance, and pre-allocated ring buffers have become foundational principles in high-performance systems engineering.

The Disruptor ring buffer is a fixed-size array of slots, allocated once at construction time, indexed by a monotonically increasing 64-bit sequence number. The physical slot index is computed by masking the sequence number with `capacity - 1` (where capacity is a power of two), avoiding the latency of integer division. This produces a naturally wrapping ring: sequence 0 maps to slot 0, sequence N maps to slot N mod capacity, and the ring overwrites itself every `capacity` messages. Because the array is pre-allocated and slots are reused in place, no heap allocation occurs on the publish or consume paths -- a property that eliminates garbage collection pauses in Java and allocation overhead in native-code implementations.

The producer claims the next sequence number (via `fetch_add` in multi-producer mode, or a simple local increment in single-producer mode), writes the message payload into the corresponding slot, and then advances a shared *cursor* to signal that the message is available. Consumers track their own position in the ring via per-consumer sequence counters and poll the producer's cursor -- or a *sequence barrier* that aggregates dependency information from upstream consumers in a pipeline topology -- to determine which messages are ready to read.

The batching effect is a natural consequence of the design. A consumer that polls the sequence barrier and discovers that the producer has advanced by K positions since the last poll can process all K messages in a single batch, amortizing the barrier-read cost over K messages. Under sustained load, this batching behavior causes the consumer to alternate between short idle periods (waiting for the barrier to advance) and bursts of sequential slot reads, which are highly cache-friendly due to the contiguous memory layout.

The Disruptor's limitation, from the perspective of minimizing coherence traffic, is the barrier read on the consumer's hot path. Each call to `waitFor` (or its equivalent in Rust ports such as `disruptor-rs` [12]) loads the shared cursor to check for new messages, then loads the slot to read the payload. These are two distinct cache-line accesses. The shared cursor, written by the producer on every publish, resides in a cache line that oscillates between Modified (after the producer writes) and Shared (after consumers read), generating one invalidation per publish cycle. The slot's cache line undergoes a separate Modified-to-Shared transition. Together, these two transitions set the Disruptor's per-message floor at approximately two cache-line transfer latencies.

Photon Ring's stamp-in-slot design eliminates the first of these two transfers. By encoding the sequence ownership information directly in the slot's cache line, the consumer can determine both whether a message is available and what its contents are from a single cache-line load. The shared producer cursor is consulted only on the slow path, when the stamp indicates that the consumer has fallen behind and must compute its lag relative to the ring head. On the common-case fast path -- where the consumer is keeping up with the producer -- the cursor is never touched, and the per-message cost is exactly one cache-line transfer: the slot line.

## 3. Design

### 3.1 Slot layout and cache-line co-location

The fundamental data structure in Photon Ring is `Slot<T>`, a cache-line-aligned container that co-locates the seqlock stamp and the message payload within a single 64-byte cache line. The struct is declared as follows:

```rust
#[repr(C, align(64))]
pub(crate) struct Slot<T> {
    stamp: AtomicU64,
    value: UnsafeCell<MaybeUninit<T>>,
}
```

Three attributes govern the memory layout:

- **`repr(C)`** enforces a deterministic, C-compatible field ordering: `stamp` occupies bytes 0--7, and `value` begins at byte 8. Without `repr(C)`, the Rust compiler is free to reorder fields, which could place the stamp and value on different cache lines.

- **`align(64)`** forces each `Slot<T>` to begin on a 64-byte boundary, matching the cache line size on all current x86_64 and aarch64 microarchitectures. This prevents *false sharing*: adjacent slots in the ring array occupy distinct cache lines, so a write to slot *k* never triggers a spurious coherence invalidation for slot *k+1*.

- **`MaybeUninit<T>`** avoids requiring `T: Default`. The slot payload is left uninitialised until the first write, and reads are gated by the stamp protocol, so the uninitialised bytes are never observed.

For `T` up to 56 bytes (the 64-byte cache line minus the 8-byte `AtomicU64` stamp), the stamp and the payload reside in the same cache line. When a consumer core loads the stamp to check for a new message, the hardware coherence protocol fetches the entire 64-byte line into L1d. If the stamp indicates the message is ready, the subsequent payload read hits L1d with zero additional latency -- no second cache-line transfer is required. This is the key advantage over Disruptor-style designs, where the consumer must first load a shared cursor (one cache-line transfer) and then load the slot data (a second transfer).

The layout for a representative `Slot<u64>` is:

```
        0       8                                               64
        +-------+-----------------------------------------------+
        | stamp |   value: u64 (8 B)  |       padding           |
        | (8 B) |                     |       (48 B)            |
        +-------+---------------------+-------------------------+
        |<---------------- single 64-byte cache line ------------------>|
```

For a larger payload such as a 48-byte struct:

```
        0       8                                               64
        +-------+-----------------------------------------------+
        | stamp |           value: [u8; 48]  (48 B)             |
        | (8 B) |                                               |
        +-------+-----------------------------------------------+
        |<---------------- single 64-byte cache line ------------------>|
```

A compile-time assertion verifies the alignment invariant:

```rust
const _: () = assert!(core::mem::align_of::<Slot<u64>>() == 64);
```

If a future refactoring changes the layout, this assertion triggers a build failure rather than a silent performance regression.

The stamp encoding uses the full `u64` range with a simple scheme:

| Stamp value       | Meaning                                  |
|--------------------|------------------------------------------|
| `0`               | Slot has never been written              |
| `seq * 2 + 1`    | Write in progress for sequence `seq`     |
| `seq * 2 + 2`    | Write complete for sequence `seq`        |

The odd/even encoding serves double duty: the low bit distinguishes in-progress writes from completed writes (enabling the seqlock torn-read check), while the upper 63 bits encode the sequence number (enabling consumers to verify they are reading the expected generation of the slot, not a stale or overwritten value).

### 3.2 Seqlock write protocol

The publisher writes to a slot using a four-step seqlock protocol. The method `Slot::write(&self, seq: u64, value: T)` executes the following operations:

**Step 1: Store the odd (writing) stamp.**

```rust
self.stamp.store(writing, Ordering::Relaxed);  // writing = seq * 2 + 1
```

The odd stamp signals to any concurrent reader that a write is in progress. This store uses `Relaxed` ordering because it does not need to be ordered with respect to the *preceding* instructions -- it only needs to become visible *before* the data write. That ordering is established by the next step.

**Step 2: Release fence.**

```rust
fence(Ordering::Release);
```

The `Release` fence ensures that the odd stamp store (step 1) is visible to other cores before the data write (step 3) begins. A `Relaxed` store followed by a `Release` fence is semantically equivalent to a single `Release` store, but separating them makes the protocol's intent explicit: the fence is the barrier between "announce writing" and "perform the write."

On x86-TSO, this fence compiles to zero instructions because the Total Store Order guarantee already prevents store-store reordering. On ARM and RISC-V, the fence emits a `DMB ISH` or `fence rw,rw` respectively.

**Step 3: Write the payload.**

```rust
unsafe { ptr::write(self.value.get() as *mut T, value) };
```

The payload is copied into the slot via `ptr::write`, which performs a bitwise copy of the `T: Copy` value. This is an unsynchronised write -- it does not use atomic operations -- which is why the seqlock protocol is necessary to prevent consumers from observing a partially written value.

**Step 4: Store the even (done) stamp.**

```rust
self.stamp.store(done, Ordering::Release);  // done = seq * 2 + 2
```

The `Release` store ensures that the data write (step 3) is visible to all cores before the done stamp becomes visible. Any consumer that loads this even stamp with `Acquire` ordering is guaranteed to see the complete, consistent payload.

After the slot write, the publisher advances the shared cursor:

```rust
self.ring.cursor.0.store(self.seq, Ordering::Release);
self.seq += 1;
```

This fifth step -- cursor advancement -- is not part of the seqlock protocol itself, but it is the mechanism by which the slow path (lag detection) discovers new messages. Since v0.2.0, the cursor is not consulted on the fast path (see Section 3.5).

### 3.3 Seqlock read protocol

The consumer reads from a slot using `Slot::try_read(&self, seq: u64) -> Result<Option<T>, u64>`. The protocol proceeds in seven steps, grouped into three phases: validation, read, and verification.

**Phase 1: Stamp validation**

**Step 1: Load the stamp (Acquire).**

```rust
let s1 = self.stamp.load(Ordering::Acquire);
```

The `Acquire` load ensures that all subsequent memory operations (including the payload read) are ordered after the stamp load. If the stamp indicates "done" (`seq * 2 + 2`), the consumer is guaranteed to see the payload written by step 3 of the write protocol.

**Step 2: Check for write-in-progress.**

```rust
if s1 & 1 != 0 {
    return Ok(None);  // odd stamp → write in progress, spin
}
```

If the low bit is set, the stamp is odd, meaning a write is in progress. The consumer returns `None` and retries.

**Step 3: Check for correct sequence.**

```rust
if s1 != expected {  // expected = seq * 2 + 2
    return Err(s1);
}
```

If the stamp is even but does not match the expected value, the slot holds a different sequence. If the actual stamp is less than expected, the slot has not yet been written for this sequence (the consumer is ahead of the publisher). If the actual stamp is greater, the publisher has lapped the consumer -- the consumer has fallen behind and must enter the lag-recovery slow path.

**Phase 2: Payload read**

**Step 4: Read the value.**

```rust
let value = unsafe { ptr::read((*self.value.get()).as_ptr()) };
```

The consumer performs an unsynchronised `ptr::read` of the payload. If the publisher is concurrently writing to this slot (because the consumer's stamp check in step 1 raced with the publisher's stamp update in step 1 of the write protocol), this read may produce a torn -- partially old, partially new -- bit pattern. The verification phase (step 5) detects this case.

**Phase 3: Stamp verification**

**Step 5: Reload the stamp (Acquire).**

```rust
let s2 = self.stamp.load(Ordering::Acquire);
```

The second `Acquire` load ensures that the payload read (step 4) is not reordered past this stamp load by either the compiler or the hardware.

**Step 6: Compare stamps.**

```rust
if s1 == s2 {
    Ok(Some(value))   // stamps match → consistent read
} else {
    Ok(None)          // stamps differ → torn read, retry
}
```

If `s1 == s2`, the stamp did not change during the read, so the payload is consistent. If the stamps differ, a write occurred between steps 1 and 5, and the read value may be torn. The consumer discards it and retries.

**Step 7 (implicit): Retry.**

The caller (`read_slot` or `recv`) loops on `Ok(None)`, retrying the entire protocol until either a consistent read succeeds or the stamp indicates a different condition (lag, empty).

The full protocol ensures that a consumer never observes a partially written message. The key invariant is: *if both stamp loads return the same even value, no write occurred between them, and the payload is the one written by the publisher for that sequence.*

### 3.4 Per-consumer cursors

Each `Subscriber` holds a private cursor field:

```rust
pub struct Subscriber<T: Copy> {
    ring: Arc<SharedRing<T>>,
    cursor: u64,       // plain u64 -- not atomic
    tracker: Option<Arc<Padded<AtomicU64>>>,
    ...
}
```

The `cursor` field is a plain `u64`, not an atomic. It is local to the subscriber thread and is never read or written by any other thread. This design eliminates all consumer-to-consumer contention: ten subscribers reading from the same ring do not share any mutable cache lines between them on the read path.

When the subscriber successfully reads a message, it increments its local cursor:

```rust
self.cursor += 1;
```

This is a single-cycle register increment with no cache-coherence traffic. Compare this with Disruptor-style designs where each consumer must atomically update a shared cursor or sequence barrier, generating a cache-line invalidation visible to all other consumers and the producer.

The only shared state the subscriber accesses on the read path is the ring slot itself (read-only, fetched via the coherence protocol's Shared state) and, on the slow path only, the shared `ring.cursor` for lag detection. The `tracker` field -- an `Arc<Padded<AtomicU64>>` -- is only present when backpressure is enabled and is only *written* (not read) by the subscriber after each successful read, via `Release` store. On lossy channels, the tracker is `None` and the subscriber's hot path touches exactly one shared cache line: the slot being read.

### 3.5 Stamp-only fast path (v0.2.0)

Prior to v0.2.0, the consumer's `try_recv` implementation first loaded the shared `ring.cursor` to check whether a new message was available, then loaded the slot data. This required two cache-line transfers per message: one for the cursor and one for the slot.

The v0.2.0 optimisation eliminates the cursor load on the fast path. Instead of consulting the shared cursor, the consumer goes directly to the expected slot and checks its stamp:

```rust
fn read_slot(&mut self) -> Result<T, TryRecvError> {
    let slot = self.ring.slot(self.cursor);
    // No load of ring.cursor here -- go straight to the stamp.
    match slot.try_read(self.cursor) {
        Ok(Some(value)) => {
            self.cursor += 1;
            ...
            Ok(value)
        }
        ...
    }
}
```

The consumer computes the slot index from its local cursor (`self.cursor & self.ring.mask`) and the expected stamp (`self.cursor * 2 + 2`). If the slot's stamp matches, the message is ready and can be read without touching the shared cursor at all. The shared cursor is only consulted on the *slow path*, when the stamp indicates the consumer has been lapped:

```rust
Err(actual_stamp) => {
    ...
    if actual_stamp >= expected {
        // Lapped: load shared cursor to compute exact lag.
        let head = self.ring.cursor.0.load(Ordering::Acquire);
        ...
    }
}
```

This reduces per-message cache traffic from two cache-line loads to one. On Intel Comet Lake, each L3 snoop costs approximately 40--55 ns, so eliminating one snoop per message saves roughly 40 ns on the cross-thread path. The benchmark results confirm this: same-thread roundtrip latency dropped from 3.2 ns (v0.1.0) to 2.5 ns (v0.2.0), a 22% improvement attributable to removing the cursor load from the critical path.

### 3.6 SubscriberGroup batched fanout

`SubscriberGroup<T, const N: usize>` is a const-generic type that holds `N` logical subscriber cursors and performs a single seqlock read when all cursors are aligned -- the common case when all subscribers are polled in lockstep on the same thread.

```rust
pub struct SubscriberGroup<T: Copy, const N: usize> {
    ring: Arc<SharedRing<T>>,
    cursors: [u64; N],
    ...
}
```

The `try_recv` fast path checks whether the first cursor's slot is ready, and if so, sweeps all aligned cursors in a single pass:

```rust
pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
    let first = self.cursors[0];
    let slot = self.ring.slot(first);

    match slot.try_read(first) {
        Ok(Some(value)) => {
            // Single seqlock read succeeded -- advance all aligned cursors.
            for c in self.cursors.iter_mut() {
                if *c == first {
                    *c = first + 1;
                }
            }
            ...
            Ok(value)
        }
        ...
    }
}
```

The cost breakdown is:

- **One seqlock read**: two `Acquire` loads (stamp before and after) plus one `ptr::read` of the payload. This is the expensive part -- it involves a cache-line transfer from the publisher's core.
- **N cursor increments**: a compiler-unrolled loop over the `[u64; N]` array. For small `N` (constrained at compile time), LLVM unrolls this into `N` straight-line compare-and-increment operations that execute entirely in registers.

The result is that fanout cost scales as O(1) seqlock reads + O(N) register operations, rather than O(N) seqlock reads. Benchmarks show 0.2 ns per subscriber for a `SubscriberGroup`, versus 1.1 ns per subscriber for independent `Subscriber` instances -- a 5.5x improvement. For a fanout of 10 subscribers, this translates to 4.3 ns total (group) versus 14 ns total (independent).

The alignment check (`if *c == first`) handles the case where individual cursors have diverged due to per-subscriber lag recovery. When a lag event resets one cursor but not others, subsequent `try_recv` calls advance only the aligned subset, and the diverged cursors catch up independently.

### 3.7 Backpressure mode

By default, Photon Ring channels are *lossy*: the publisher overwrites the oldest slot without checking whether any subscriber has read it. This is the zero-overhead path -- no per-publish checks, no shared state beyond the slot and cursor.

For applications that require guaranteed delivery, `channel_bounded(capacity, watermark)` creates a backpressure-capable channel. The backpressure mechanism uses per-subscriber cursor trackers:

```rust
pub(crate) struct BackpressureState {
    pub(crate) watermark: u64,
    pub(crate) trackers: Mutex<Vec<Arc<Padded<AtomicU64>>>>,
}
```

When a subscriber is created on a bounded channel, a tracker is registered:

```rust
pub(crate) fn register_tracker(&self, initial: u64) -> Option<Arc<Padded<AtomicU64>>> {
    let bp = self.backpressure.as_ref()?;
    let tracker = Arc::new(Padded(AtomicU64::new(initial)));
    bp.trackers.lock().push(tracker.clone());
    Some(tracker)
}
```

Each `Padded<AtomicU64>` is aligned to 64 bytes (`#[repr(align(64))]`) to prevent false sharing between tracker updates from different subscribers.

On the publish path, the publisher checks whether it would overwrite unread data:

```rust
pub fn try_publish(&mut self, value: T) -> Result<(), PublishError<T>> {
    if let Some(bp) = self.ring.backpressure.as_ref() {
        let effective = capacity - bp.watermark;

        // Fast path: use cached slowest cursor.
        if self.seq >= self.cached_slowest + effective {
            // Slow path: rescan all trackers.
            match self.ring.slowest_cursor() {
                Some(slowest) => {
                    self.cached_slowest = slowest;
                    if self.seq >= slowest + effective {
                        return Err(PublishError::Full(value));
                    }
                }
                None => { /* no subscribers -- unbounded */ }
            }
        }
    }
    self.publish_unchecked(value);
    Ok(())
}
```

The publisher maintains a `cached_slowest` field to avoid scanning all trackers on every publish. The scan only occurs when the publisher's sequence number reaches the cached limit. The `slowest_cursor()` method acquires the `Mutex`, iterates the tracker vector, and returns the minimum cursor value.

The `watermark` parameter controls how many slots of headroom the publisher must maintain. A watermark of 0 means the publisher blocks as soon as all `capacity` slots are occupied. A higher watermark provides breathing room for bursty consumers.

When the publisher is full, `try_publish` returns `Err(PublishError::Full(value))`, returning the unsent value to the caller. The blocking `publish()` method spin-waits internally until room is available.

Subscribers update their tracker after each successful read:

```rust
fn update_tracker(&self) {
    if let Some(ref tracker) = self.tracker {
        tracker.0.store(self.cursor, Ordering::Release);
    }
}
```

On lossy channels (`tracker` is `None`), the `update_tracker` call compiles to nothing -- zero overhead on the default path.

### 3.8 Multi-producer extension

`channel_mpmc()` creates a multi-producer, multi-consumer channel using `MpPublisher`, which is `Clone + Send + Sync`. The multi-producer protocol adapts the standard Disruptor multi-producer algorithm for the seqlock stamp encoding.

**Step 1: Claim a sequence number.**

```rust
let seq = next_seq.0.fetch_add(1, Ordering::Relaxed);
```

Each producer atomically claims the next sequence number via `fetch_add` on a shared `AtomicU64` counter (`next_seq`). This is a single atomic RMW operation -- no CAS loop is needed for claiming. `Relaxed` ordering suffices because the sequence number is only used locally by the claiming producer until the cursor advancement step.

**Step 2: Write the slot.**

```rust
self.ring.slot(seq).write(seq, value);
```

The producer writes the claimed slot using the standard seqlock write protocol (Section 3.2). Multiple producers may be writing to *different* slots concurrently -- the seqlock per-slot design allows this without contention (each slot is on its own cache line).

**Step 3: Advance the cursor in strict order.**

```rust
let expected_cursor = if seq == 0 { u64::MAX } else { seq - 1 };
while self.ring.cursor.0
    .compare_exchange_weak(expected_cursor, seq, Ordering::Release, Ordering::Relaxed)
    .is_err()
{
    spin_loop();
}
```

The cursor must advance in strict sequence order: producer claiming sequence *k* must wait until the cursor equals *k - 1* (meaning all sequences 0 through *k - 1* have committed) before advancing it to *k*. This is enforced by a CAS spin loop: `compare_exchange_weak` atomically sets the cursor to `seq` only if it currently equals `expected_cursor`. If another producer with a lower sequence number has not yet committed, the CAS fails and the current producer spins.

The `Release` ordering on the successful CAS ensures that the slot write (step 2) is visible to consumers before the cursor update. The `Relaxed` ordering on the failure path avoids unnecessary fence overhead during the spin.

The special case `seq == 0` handles the initial state where the cursor is `u64::MAX` (the sentinel for "nothing published yet").

This protocol guarantees that consumers see messages in strict sequence order, even when multiple producers write concurrently. The trade-off is that the cursor cache line becomes a serialisation point: under high producer contention, the CAS spin loop becomes the bottleneck, as all producers contend on the same cache line. This is the standard cost of ordered multi-producer ring buffers and is discussed further in Section 7.3.

## 4. Implementation

### 4.1 Rust memory model considerations

Photon Ring maps the seqlock protocol onto Rust's atomic memory model using three ordering levels, chosen to minimise fence overhead while maintaining correctness on all target architectures.

**Write-side orderings:**

The write protocol uses `Relaxed` for the odd stamp store, paired with an explicit `Release` fence:

```rust
self.stamp.store(writing, Ordering::Relaxed);   // step 1
fence(Ordering::Release);                        // step 2
ptr::write(self.value.get() as *mut T, value);   // step 3
self.stamp.store(done, Ordering::Release);       // step 4
```

A `Relaxed` store followed by a `Release` fence is equivalent to a `Release` store in the C++20 memory model: the fence establishes the ordering constraint that the store in step 1 is visible before any store or non-atomic write that follows the fence. The reason for separating the store and the fence, rather than using a single `Release` store for step 1, is clarity: the fence explicitly marks the boundary between "announce writing" and "perform the write."

The `Release` store in step 4 is the critical ordering operation. It ensures that the data write (step 3) is ordered before the done stamp becomes visible. Any thread that loads the done stamp with `Acquire` ordering is guaranteed to see the complete payload.

**Read-side orderings:**

Both stamp loads use `Acquire`:

```rust
let s1 = self.stamp.load(Ordering::Acquire);   // step 1
// ... read payload ...
let s2 = self.stamp.load(Ordering::Acquire);   // step 5
```

The first `Acquire` load (step 1) ensures that the payload read is ordered after the stamp load. Without this, the compiler or hardware could speculatively execute the `ptr::read` before the stamp check, potentially reading stale data.

The second `Acquire` load (step 5) ensures that the payload read is ordered before the verification stamp load. Without this, the processor could reorder the second stamp load before the data read, defeating the torn-read detection.

**x86-TSO implications:**

On x86-64, the Total Store Order (TSO) memory model provides strong ordering guarantees: loads are not reordered with other loads, and stores are not reordered with other stores. The only reordering permitted is store-load reordering (a store can be observed by a subsequent load before it is visible to other cores). Consequently:

- The `Release` fence in write step 2 compiles to zero instructions -- TSO already prevents store-store reordering.
- The `Release` store in write step 4 compiles to a plain `mov` -- TSO guarantees store ordering.
- The `Acquire` loads on the read side compile to plain `mov` instructions -- TSO guarantees load ordering.

The only case where x86 requires an explicit fence is a `SeqCst` store (which uses `xchg` or `mfence`). Photon Ring avoids `SeqCst` entirely, so the seqlock protocol has zero fence overhead on x86-TSO. On ARM, the `Acquire` loads emit `ldar` instructions and the `Release` stores emit `stlr` instructions, which carry the necessary barrier semantics.

**The data-race question:**

Strictly speaking, the concurrent `ptr::read` (consumer) and `ptr::write` (publisher) on the same slot constitute a data race under both the C++20 and Rust memory models, because the accesses are not mediated by atomic operations. The seqlock protocol detects torn reads *after the fact* via the stamp re-check, but the read itself is technically undefined behaviour.

This is the same situation faced by the Linux kernel's `seqlock_t` implementation, and it is widely regarded as "benign UB" in practice: no known compiler optimisation or hardware behaviour on x86, ARM, or RISC-V produces incorrect results for `Copy` types under this pattern. The torn bits are detected and discarded, and no destructor runs on the torn value (because `T: Copy` implies no `Drop`). MIRI, Rust's undefined-behaviour sanitiser, flags the concurrent access but does not identify any actual miscompilation when run with `-Zmiri-disable-data-race-check`. A future solution may come from atomic memcpy (proposed for C++26), which would eliminate the UB without changing the protocol's semantics or performance.

### 4.2 The `T: Copy` constraint

The `T: Copy` bound on all Photon Ring message types is a hard safety requirement, not merely a performance optimisation. It serves three purposes:

**Torn-read safety.** When a consumer's `ptr::read` races with a publisher's `ptr::write`, the resulting bit pattern may be a chimera: some bytes from the old value, some from the new. For `Copy` types, this chimera is harmless. It is a valid bit pattern (all bit patterns are valid for `Copy` types that do not contain padding with validity constraints), and discarding it after the stamp re-check has no side effects. No destructor runs, no allocation is freed, no invariant is violated.

For non-`Copy` types, a torn read is catastrophic. Consider `String`: a torn read might produce a `String` whose internal pointer points to one allocation and whose length/capacity correspond to a different allocation. When this torn `String` is dropped, it would attempt to free an invalid pointer, causing a double-free or heap corruption. The `Copy` bound eliminates this entire class of bugs at compile time.

**No `Drop` interaction.** `Copy` types in Rust cannot implement `Drop`. This means the publisher can overwrite a slot without running a destructor on the previous value, and the consumer can discard a torn read without running a destructor on the torn value. The slot's `UnsafeCell<MaybeUninit<T>>` is never explicitly dropped -- slots are reused indefinitely, and the `MaybeUninit` wrapper ensures no implicit drop occurs.

**Validity invariant.** For most `Copy` types (integers, floats, arrays of `Copy` types, `#[repr(C)]` structs of `Copy` fields), every bit pattern is a valid value. This means a torn read, while logically incorrect, does not trigger undefined behaviour merely by existing as a Rust value. The exception is types like `bool` (which has only two valid bit patterns) or `char` (which must be a valid Unicode scalar value). For these types, a torn read could produce an invalid value. In practice, single-byte and four-byte types are written atomically on all supported architectures, so torn reads of `bool` or `char` do not occur, but the theoretical concern remains.

### 4.3 `no_std` compatibility

Photon Ring is declared `#![no_std]` with `extern crate alloc`, making it usable in embedded, kernel, and bare-metal contexts that provide a heap allocator. The crate avoids all `std`-only dependencies:

- **`hashbrown`** provides the `HashMap` used by the `Photon` and `TypedBus` topic buses. This is the same hash map implementation that backs `std::collections::HashMap`, but it is available as a standalone `no_std` crate.

- **`spin::Mutex`** replaces `std::sync::Mutex` for internal locking (the backpressure tracker vector, the topic bus hash map). `spin::Mutex` is a pure spin-lock with no OS primitive dependencies. It is suitable here because the critical sections are very short (vector push/scan, hash map lookup) and contention is rare (subscriber creation and publisher backpressure scanning are not hot-path operations).

- **`core_affinity2`** provides CPU core pinning via platform-specific APIs (`sched_setaffinity` on Linux, `SetThreadAffinityMask` on Windows). It is conditionally compiled for supported platforms only:

  ```rust
  #[cfg(any(target_os = "linux", target_os = "macos", ...))]
  pub mod affinity;
  ```

- **All wait strategies** use `core::hint::spin_loop()` (which maps to `PAUSE` on x86, `YIELD` on ARM) and inline assembly (`WFE` on aarch64). No OS threading primitives (`futex`, `park`, `unpark`, `condvar`) are used anywhere in the wait path. This means `recv()` and `recv_with()` work on bare-metal targets.

The only optional `std` dependency is `libc`, gated behind the `hugepages` Cargo feature. When enabled, it provides `mmap` (huge-page allocation), `mlock` (page-fault prevention), and `set_mempolicy` (NUMA placement). These are Linux-specific system calls that are inherently platform-dependent and are never required for basic operation.

### 4.4 Wait strategies

The `WaitStrategy` enum provides four waiting modes for blocking receive operations, all `no_std` compatible:

**`BusySpin`.** A pure busy loop with no hint instruction. The consumer thread executes a tight loop checking the slot stamp, consuming 100% of one CPU core. This provides the absolute minimum wakeup latency (effectively zero additional delay beyond the cache-coherence transfer time) but is only appropriate for dedicated, pinned cores in latency-critical paths.

```rust
WaitStrategy::BusySpin => {
    // No hint -- pure busy loop. Fastest wakeup, highest power.
}
```

**`YieldSpin`.** Inserts a platform-specific yield hint between iterations. On x86, this emits `PAUSE`, which yields the execution pipeline to the SMT (hyper-threading) sibling and introduces approximately 140 cycles of delay on Skylake and later microarchitectures. On aarch64, Photon Ring uses the `SEVL` + `WFE` instruction pair instead of `YIELD`:

```rust
#[cfg(target_arch = "aarch64")]
unsafe {
    core::arch::asm!("sevl", options(nomem, nostack));
    core::arch::asm!("wfe", options(nomem, nostack));
}
```

`WFE` (Wait For Event) puts the core into a low-power state until an event occurs -- such as a cache-line invalidation triggered by the publisher's stamp store. The preceding `SEVL` (Send Event Locally) sets the local event register so the first `WFE` returns immediately rather than blocking unconditionally. This pattern provides near-zero wakeup latency on ARM while consuming negligible power during the wait.

**`BackoffSpin`.** Exponential backoff with increasing numbers of pause iterations per wait call. The backoff doubles the number of pause iterations for each consecutive empty poll, up to a maximum of 64:

```rust
let pauses = 1u32.wrapping_shl(iter.min(6)); // 1, 2, 4, 8, 16, 32, 64
for _ in 0..pauses {
    spin_loop(); // PAUSE on x86, WFE on aarch64
}
```

This strategy is suitable for consumers that may be idle for extended periods: it starts responsive and progressively backs off to reduce CPU and power consumption. On aarch64, each `WFE` iteration is effectively free in terms of power, making the backoff curve less relevant than on x86 where each `PAUSE` burns ~140 cycles.

**`Adaptive`.** A three-phase escalation strategy parameterised by `spin_iters` and `yield_iters`:

1. **Phase 1** (iterations 0 to `spin_iters - 1`): bare spin with no hint, identical to `BusySpin`. Provides minimum wakeup latency for messages that arrive quickly.

2. **Phase 2** (iterations `spin_iters` to `spin_iters + yield_iters - 1`): single `PAUSE`/`YIELD` per iteration, identical to `YieldSpin`. Reduces power consumption while maintaining reasonable responsiveness.

3. **Phase 3** (iterations beyond `spin_iters + yield_iters`): eight `PAUSE`/`YIELD` instructions per iteration. Deep backoff for long idle periods.

The default configuration is `Adaptive { spin_iters: 64, yield_iters: 64 }`, which provides 64 bare-spin iterations (~0 ns reaction time for fast messages) followed by 64 yield-spin iterations before entering deep backoff. This is the default used by `Subscriber::recv()`, which hard-codes a similar two-phase strategy (64 bare spins, then `PAUSE`-based spin) for the common case.

The `recv()` method directly implements the two-phase spin without going through `WaitStrategy`, avoiding the per-iteration enum dispatch overhead on the hottest path. The `recv_with(strategy)` method provides the full configurable wait for users who need different trade-offs.

## 5. Evaluation

This section presents a systematic evaluation of Photon Ring's latency and throughput characteristics. We begin with the benchmarking methodology (Section 5.1), then present single-threaded microbenchmarks (Section 5.2), cross-thread latency measurements (Section 5.3), a comparison with the `disruptor-rs` Rust crate (Section 5.4), an analysis of SPMC versus MPMC overhead (Section 5.5), and a detailed fanout scaling study (Section 5.6). All reported figures are medians of Criterion sample distributions unless stated otherwise.

### 5.1 Benchmark methodology

**Hardware.** Two machines were used throughout the evaluation:

| Property | Machine A | Machine B |
|---|---|---|
| CPU | Intel Core i7-10700KF @ 3.80 GHz | Apple M1 Pro |
| Microarchitecture | Comet Lake (14 nm) | Firestorm/Icestorm (5 nm) |
| Cores / Threads | 8C / 16T | 8C (6P + 2E) |
| L1d / L2 / L3 | 32 KB / 256 KB / 16 MB | 128 KB / 4 MB (shared cluster) / 24 MB |
| Interconnect | Ring bus | Shared L2 cluster (P-cores) |
| OS | Linux 6.8 (Ubuntu) | macOS 26.3 |
| Rust toolchain | 1.93.1 | 1.92.0 |

Machine A represents a typical mid-range x86_64 workstation with a ring-bus L3 interconnect. Machine B represents a modern AArch64 laptop processor with a unified memory architecture. The Comet Lake ring bus is particularly relevant because its L3 snoop latency (~40--55 ns) establishes the physical floor for inter-core communication on this platform.

**Framework.** All microbenchmarks use Criterion 0.8 (`criterion = "0.8.2"`) with the following settings: 100 samples per benchmark, 3-second warmup period, and `--release` compilation (opt-level 3, LTO disabled). Ring buffer capacity is fixed at 4096 slots (256 KB for `u64` payload, fitting comfortably in L2 cache) for all benchmarks unless noted. The default message type is `u64` (8 bytes), which together with the 8-byte `AtomicU64` stamp occupies 16 bytes of a 64-byte cache-line-aligned slot. All benchmarks use `std::hint::black_box` to prevent dead-code elimination.

**Core pinning.** The default benchmarks do not employ explicit core pinning. The OS scheduler is free to migrate threads across cores, introducing non-deterministic variance from cache migration events. This choice reflects the expected deployment scenario for most users. Pinned-core results are discussed qualitatively where relevant; the `pinned_latency` example in the repository demonstrates explicit affinity assignment via `core_affinity2`.

**RDTSC methodology.** One-way latency is measured via a dedicated harness (`benches/rdtsc_oneway.rs`) that embeds the publisher's TSC timestamp directly in the message payload. The consumer reads `LFENCE; RDTSC` immediately upon receiving each message, yielding a cycle-accurate arrival timestamp without any signal-back to the publisher. This eliminates the second cache-line transfer (the `seen` signal in roundtrip benchmarks), measuring exactly one cache-line transfer: the slot line from the producer's L1d to the consumer's L1d via L3 snooping. The harness discards 10,000 warmup messages before collecting 100,000 measurement samples. Cycle counts are converted to nanoseconds using the base clock frequency (3.8 GHz) and the observed all-core turbo frequency (~4.7 GHz). The `RDTSCP` instruction is used on the publish side (serialising prior instructions before reading TSC), while `LFENCE; RDTSC` is used on the receive side (serialising prior loads to capture the earliest possible arrival time). Both cores must reside in the same TSC domain (same socket); the invariant TSC guarantee on Comet Lake ensures negligible inter-core TSC skew.

**Disruptor comparison.** The `disruptor` crate (v4.0.0) is benchmarked alongside Photon Ring in the same Criterion suite (`benches/throughput.rs`). Both libraries use ring size 4096, `u64` payload, and busy-spin wait strategy. The disruptor benchmarks use `build_single_producer` with a single `BusySpin` event handler. A methodological caveat applies: `disruptor-rs` uses a managed-thread consumer model in which the library spawns and manages the consumer thread internally, whereas Photon Ring's benchmark manually spawns a consumer thread with a spin loop. This architectural difference may account for a portion of the observed latency gap.

### 5.2 Single-threaded microbenchmarks

Single-threaded benchmarks isolate the instruction cost of publish and receive operations from cache-coherence effects. Because publisher and subscriber operate on the same thread, all data resides in L1d throughout the measurement; the reported times reflect pure instruction overhead.

**Table 1.** Single-threaded operation latencies (Machine A, Intel i7-10700KF).

| Operation | Latency | Notes |
|---|---|---|
| `publish` only | 2.9 ns | Seqlock write: odd stamp, fence, memcpy, even stamp, cursor advance |
| `publish` + `try_recv` (1 sub) | 2.8 ns | Full roundtrip; stamp-only fast path avoids shared cursor load |
| `try_recv` (empty channel) | 0.8 ns | Single `stamp.load(Acquire)` -- stamp mismatch, early return |
| Struct roundtrip (24 B payload) | 4.7 ns | `Quote { f64, u64, u64 }` -- 24 B memcpy within single cache line |
| Fanout: 1 independent sub | 2.8 ns | Baseline identical to single-sub roundtrip |
| Fanout: 10 independent subs | 13 ns | ~1.1 ns per additional subscriber |
| Fanout: 10 SubscriberGroup | 4.3 ns | ~0.2 ns per additional subscriber |

The `publish`-only benchmark measures the write path in isolation: the consumer is allocated but never polled, so the ring never fills within a benchmark iteration. At 2.9 ns, the write path compiles to approximately 6 `mov` instructions on x86_64 (two stamp stores, one 8-byte payload store, one cursor store, plus the release fence which compiles to no additional instructions on x86 TSO). The 2.8 ns publish+recv figure is marginally lower than publish-only due to Criterion measurement noise at this resolution; the two figures are statistically indistinguishable.

The empty `try_recv` cost of 0.8 ns confirms that the stamp-only fast path compiles to a single atomic load (`stamp.load(Acquire)`) followed by a comparison and early return. No shared cursor is consulted on the empty path. This is critical for polling patterns where consumers speculatively check multiple channels.

The 24-byte struct roundtrip (4.7 ns) demonstrates that payloads fitting within the 56-byte slot value region incur only the additional `memcpy` cost. The `Quote` struct (8 + 8 + 8 = 24 bytes) plus the 8-byte stamp totals 32 bytes, well within a single 64-byte cache line.

### 5.3 Cross-thread latency

Cross-thread latency is the metric of primary interest for inter-thread messaging systems, as it is dominated by the cache-coherence protocol rather than instruction count. The benchmark spawns a dedicated consumer thread that busy-spins on `try_recv()`, storing the received value into a shared `AtomicU64`. The publisher measures the roundtrip time from `publish()` to observing the consumer's acknowledgment.

**Table 2.** Cross-thread roundtrip latency (Criterion, 100 samples).

| Metric | Machine A (Intel) | Machine B (M1 Pro) |
|---|---|---|
| Roundtrip latency (median) | 96 ns | 103 ns |

**Table 3.** One-way latency distribution (Machine A, RDTSC harness, 100,000 samples).

| Percentile | Cycles | Nanoseconds (@ 3.8 GHz) | Nanoseconds (@ 4.7 GHz turbo) |
|---|---|---|---|
| min | ~130 | 34 ns | 28 ns |
| p50 | ~182 | 48 ns | 39 ns |
| p90 | ~220 | 58 ns | 47 ns |
| p99 | ~250 | 66 ns | 53 ns |

**Decomposition.** The 96 ns roundtrip decomposes cleanly into two cache-line transfers of approximately 48 ns each. The first transfer carries the slot cache line (stamp + payload) from the publisher's L1d to the consumer's L1d via L3 ring-bus snooping. The second transfer carries the signal-back cache line (the `seen: AtomicU64`) from the consumer's L1d back to the publisher's L1d. The one-way RDTSC measurement, which eliminates the signal-back transfer, confirms this decomposition: p50 = 48 ns corresponds precisely to half the roundtrip.

**Proximity to the hardware floor.** On Comet Lake, an L3 ring-bus snoop for a Modified-to-Shared transition requires approximately 40--55 ns depending on the relative positions of the requesting and owning cores on the ring bus. The measured minimum of 34 ns (at turbo frequency, where TSC cycles convert to fewer nanoseconds) is consistent with the best-case snoop latency when the two cores are adjacent on the ring. The p50 of 48 ns represents approximately 89% of the theoretical two-transfer floor (~54 ns x 2 = ~108 ns for roundtrip; actual roundtrip of 96 ns is 89% of this pessimistic estimate), indicating that software overhead contributes less than 5 ns per transfer on the hot path.

**M1 Pro comparison.** The 103 ns roundtrip on M1 Pro is 7% slower than the Intel result despite the M1 Pro's higher IPC and newer process node. This likely reflects the longer coherence latency for cross-cluster cache transfers on the M1 Pro's fabric when the OS schedules publisher and consumer on cores in different L2 clusters. Within the same L2 cluster, latencies as low as 60--70 ns have been observed in pinned configurations.

### 5.4 Comparison with disruptor-rs

We compare Photon Ring against `disruptor-rs` v4.0.0, the most prominent Rust implementation of the LMAX Disruptor pattern. Both libraries are benchmarked in the same Criterion process on the same hardware with identical ring sizes and message types.

**Table 4.** Photon Ring vs. disruptor-rs (Machine A, Intel i7-10700KF).

| Benchmark | Photon Ring | disruptor-rs 4.0 | Ratio |
|---|---|---|---|
| Publish only (write cost) | 2.9 ns | 20 ns | 6.5x faster |
| Cross-thread roundtrip | 96 ns | 133 ns | 1.39x faster (28% lower) |

**Table 5.** Photon Ring vs. disruptor-rs (Machine B, Apple M1 Pro).

| Benchmark | Photon Ring | disruptor-rs 4.0 | Ratio |
|---|---|---|---|
| Publish only (write cost) | 2 ns | 12 ns | 6x faster |
| Cross-thread roundtrip | 103 ns | 174 ns | 1.69x faster (41% lower) |

The 6.5x publish-only advantage reflects the structural difference in write paths. Photon Ring's publisher executes a seqlock stamp pair (two stores) and a payload copy. The Disruptor publisher must claim a sequence number (atomic load or CAS), write the payload, and advance a shared sequence barrier (atomic store with release semantics), touching at least two distinct cache lines. On the single-threaded publish-only benchmark, the barrier store to a separate cache line incurs additional store-buffer drain overhead even without cross-core traffic.

The cross-thread advantage is smaller (28--41%) because inter-core latency is dominated by cache-coherence transfer time, which both libraries pay equally. Photon Ring's advantage in this regime stems from eliminating one cache-line load on the consumer hot path: the stamp-in-slot design allows the consumer to verify message availability and read the payload from a single cache line, whereas the Disruptor consumer must first load the sequence barrier (one cache line) and then load the slot data (a second cache line).

**Methodology caveat.** The `disruptor-rs` crate uses a managed-thread model where the library internally spawns consumer threads and invokes user-provided event handlers. In our benchmark, we use `build_single_producer` with a `BusySpin` wait strategy and a single event handler that stores the received value into an `AtomicU64`. The Photon Ring benchmark manually spawns a consumer thread with an explicit spin loop on `try_recv()`. These architectural differences in thread management, handler dispatch, and internal bookkeeping may account for a portion of the observed latency gap. The comparison should be interpreted as a measure of end-to-end API latency rather than a pure algorithmic comparison.

### 5.5 SPMC vs. MPMC overhead

Photon Ring provides both a single-producer (`Publisher`) and a multi-producer (`MpPublisher`) mode. The single-producer variant enforces exclusive access via `&mut self` at the type level, requiring no atomic operations on the write path. The multi-producer variant uses `fetch_add` on a shared atomic counter to claim sequence numbers, followed by a compare-and-exchange (CAS) spin loop to advance the cursor in strict order.

**Table 6.** SPMC vs. MPMC single-threaded roundtrip (Machine A, 1 publisher, 1 subscriber).

| Mode | Roundtrip latency | Overhead vs. SPMC |
|---|---|---|
| SPMC (`Publisher`) | 2.8 ns | -- (baseline) |
| MPMC (`MpPublisher`, 1 publisher) | 11.7 ns | 4.2x |

The 4.2x overhead is attributable to two additional atomic operations on the MPMC publish path. First, `fetch_add(1, Relaxed)` on the shared sequence counter claims the next slot. Although `fetch_add` is a single instruction on x86_64 (`lock xadd`), the `lock` prefix forces the processor to acquire exclusive ownership of the counter's cache line and drain the store buffer, costing approximately 5--8 ns even without contention. Second, after writing the slot, the publisher must advance the shared cursor via a CAS spin loop (`compare_exchange` in a loop), waiting until all prior sequence numbers have committed their writes. In the uncontended single-publisher case, the CAS succeeds on the first attempt, but the `lock cmpxchg` instruction itself costs approximately 5--8 ns due to the same store-buffer drain.

This overhead is the expected cost of supporting multiple producers. In the SPMC mode, both operations are replaced by plain stores (non-atomic increment of a local counter, and a `Release` store to the cursor), which cost effectively zero additional cycles on x86 TSO.

### 5.6 Fanout scaling analysis

A key design goal of Photon Ring is efficient single-producer, multi-consumer fanout. We measure how per-message latency scales with the number of subscribers, comparing independent `Subscriber` instances against the batched `SubscriberGroup<T, N>` API.

**Table 7.** Fanout scaling (Machine A, same-thread, publish + recv all subs).

| Subscribers | Independent subs | SubscriberGroup | Speedup |
|---|---|---|---|
| 1 | 2.8 ns | 2.8 ns | 1.0x |
| 2 | 3.9 ns | 3.1 ns | 1.3x |
| 5 | 7.2 ns | 3.5 ns | 2.1x |
| 10 | 13.0 ns | 4.3 ns | 3.0x |

**Per-subscriber marginal cost:**

| Mode | Marginal cost per sub | Derived from |
|---|---|---|
| Independent subscribers | ~1.1 ns / sub | Linear regression on 1--10 subs |
| SubscriberGroup | ~0.2 ns / sub | Linear regression on 1--10 subs |
| Improvement factor | 5.5x | 1.1 / 0.2 |

**Root cause analysis.** Because these benchmarks execute on a single thread, all data resides in L1d throughout. There is no cache-coherence traffic; the scaling cost is pure instruction overhead. For independent subscribers, each `try_recv()` call executes the full seqlock read protocol: two `stamp.load(Acquire)` operations (pre-read and post-read validation), one `ptr::read` of the payload, one cursor increment, and associated branch instructions. The 1.1 ns marginal cost per subscriber reflects these ~4--5 instructions per seqlock read.

`SubscriberGroup<T, N>` eliminates N-1 redundant seqlock reads by exploiting the observation that when all N cursors are aligned (the common case for same-thread fanout), they all seek the same slot at the same sequence number. The group performs a single seqlock read (one stamp-load pair plus one payload copy), then advances all N cursors in a compiler-unrolled loop. Each cursor advance is a single `u64` increment (one `add` instruction), costing approximately 0.2 ns. The stamp loads and payload copy are performed exactly once regardless of N, making the seqlock overhead amortised to O(1) rather than O(N).

**Projection.** Extrapolating the linear trend, a fanout of 100 independent subscribers would cost approximately 112 ns per message, while a `SubscriberGroup<T, 100>` would cost approximately 23 ns -- a 4.9x improvement. At 1,000 subscribers, the projected costs are 1,103 ns vs. 203 ns (5.4x). These projections assume the cursor array remains L1-resident; for very large N, L1d capacity evictions would introduce additional latency not captured by the linear model.

### 5.7 Threats to validity

Several factors limit the generalisability of the reported results.

**Turbo frequency variability.** The i7-10700KF supports turbo frequencies up to 5.1 GHz (single-core) and approximately 4.7 GHz (all-core). Criterion benchmarks do not control for frequency scaling; the CPU's P-state governor may select different frequencies across runs. The RDTSC harness reports raw cycle counts alongside nanosecond estimates at base and turbo frequencies to bound this uncertainty.

**No core pinning in Criterion benchmarks.** The default benchmarks do not pin publisher and consumer threads to specific cores. OS-level thread migration introduces variance and may inflate reported latencies when threads are migrated to distant cores on the ring bus. The `pinned_latency` example provides pinned-core measurements for users requiring deterministic results.

**Single-socket topology.** All measurements are taken on single-socket systems. Cross-socket communication (via QPI/UPI on Intel, or cross-die interconnect on chiplet designs) would add 50--150 ns per transfer, significantly altering the absolute numbers while preserving the relative advantages.

**Measurement granularity.** At sub-10 ns resolution, Criterion's measurement loop overhead (timer calls, black_box barriers, iteration counting) becomes a non-negligible fraction of the measured quantity. The publish-only measurement of 2.9 ns is within 2--3x of Criterion's measurement floor on this hardware. The relative ordering and ratios between benchmarks are more reliable than absolute values.

**Comparison fairness.** The disruptor-rs comparison is not strictly apples-to-apples due to differing consumer models (polled vs. managed-thread). The disruptor's managed-thread model includes internal dispatch overhead that Photon Ring's manual spin loop does not. A fairer comparison would require either adapting Photon Ring to a callback model or using the disruptor's lower-level `Sequence` API directly, neither of which was attempted.

**Payload size sensitivity.** All primary benchmarks use `u64` (8 bytes). The 24-byte struct roundtrip demonstrates that larger payloads within the 56-byte single-cache-line budget incur modest additional cost. Payloads exceeding 56 bytes spill to multiple cache lines, and the seqlock torn-read probability increases with payload size. The scaling characteristics of multi-cache-line payloads are not systematically evaluated.

**Thermal throttling.** Extended benchmark runs (particularly the 100,000-sample RDTSC harness) may trigger thermal throttling on both platforms, reducing turbo frequency and inflating tail latencies. The p999 and max values in the RDTSC distribution should be interpreted with this caveat in mind.

## 6. Related Work

Photon Ring builds on a substantial body of prior work in lock-free inter-thread messaging. In this section we survey the most closely related systems, compare their design choices with ours, and identify the tradeoffs each makes.

### 6.1 LMAX Disruptor (Java)

The LMAX Disruptor [Thompson et al., 2011] is the seminal work on high-performance inter-thread messaging via pre-allocated ring buffers. The Disruptor introduced sequence barriers as the primary coordination mechanism: a shared cursor (the producer's sequence number) is polled by consumers, who advance their own dependent barriers in a directed acyclic graph of event processors. Events are pre-allocated in the ring and mutated in place, avoiding garbage collection pressure. Thompson reported approximately 52 ns mean latency in a three-stage pipeline on contemporary hardware.

Photon Ring borrows the Disruptor's core insight -- pre-allocated ring buffers with bitmask indexing -- but departs from it in three fundamental ways. First, the Disruptor uses separate sequence barriers to coordinate producers and consumers; Photon Ring co-locates a seqlock stamp with the payload in the same cache line, eliminating one cache-line transfer per message on the consumer hot path. Second, the Disruptor's events are mutable Java objects that are populated in place and passed through event handlers; Photon Ring requires `T: Copy` and uses value-copy semantics, which enables torn-read detection without resource-management hazards. Third, the Disruptor maintains shared consumer barriers that all downstream processors poll; Photon Ring uses per-consumer local cursors (plain `u64` values, not atomics), which eliminates all consumer-to-consumer contention.

The Disruptor's design is better suited to complex processing topologies (diamonds, pipelines with dependent stages) because its sequence-barrier abstraction naturally expresses inter-stage dependencies. Photon Ring's simpler model -- independent consumers without dependency graphs -- trades topological generality for lower per-message latency. The managed consumer thread model of the Disruptor also handles thread lifecycle concerns that Photon Ring deliberately leaves to the caller.

### 6.2 Aeron (Java/C++)

Aeron [Thompson, 2014] is a high-performance messaging library developed by Real Logic for financial systems. It supports both IPC (shared-memory) and network transport modes, using term buffers -- log-structured ring buffers with header-per-message framing -- rather than the fixed-slot ring buffers used by the Disruptor and Photon Ring. Aeron is designed as a complete messaging layer: it handles flow control, congestion avoidance, message fragmentation, and reliable delivery over UDP.

The scope difference is substantial. Aeron targets inter-process and cross-network communication, whereas Photon Ring is exclusively an in-process, inter-thread mechanism. Aeron's term-buffer design supports variable-length messages with per-message headers, whereas Photon Ring uses fixed-size slots parameterized by a single type `T`. Aeron's IPC mode achieves low-microsecond latencies via memory-mapped files, but the memory-mapping overhead and message framing cost make it significantly slower than a dedicated in-process ring buffer for the same-host, same-process case. Aeron also uses memory-mapped files for its log buffers, which provides crash recovery at the cost of file-system interaction on the critical path.

Both systems share an emphasis on mechanical sympathy [Thompson, 2011] -- designing data structures around CPU cache behavior rather than abstract algorithmic concerns. Aeron is licensed under Apache-2.0, as is Photon Ring.

### 6.3 Chronicle Queue (Java)

Chronicle Queue [Lawrey, 2013] is a persistent inter-process messaging system from OpenHFT (now Chronicle Software). It uses memory-mapped files to implement an append-only log that supports both real-time consumption and historical replay. Messages are persisted to disk as they are written, enabling audit trails and post-hoc analysis -- a requirement in regulated financial environments.

The design philosophy differs fundamentally from Photon Ring's. Chronicle Queue prioritizes durability and replay: messages survive process restarts, are indexed for random access, and can be consumed by multiple independent processes at different speeds. Photon Ring is ephemeral -- the ring buffer exists only in process memory, and slow consumers lose messages when the ring wraps (unless the bounded backpressure mode is used). Chronicle Queue's memory-mapped I/O path adds latency compared to purely in-memory ring buffers; the tradeoff is justified when persistence is a requirement but is unnecessary overhead for use cases that need only in-process fanout.

The two systems are complementary rather than competing. A common architecture in low-latency financial systems uses an in-process ring (such as Photon Ring) for the hot path -- distributing market data to strategy threads with sub-100 ns latency -- while a separate Chronicle Queue captures the same messages to disk for compliance logging and replay.

### 6.4 disruptor-rs (Rust)

The `disruptor` crate [Vestergaard, 2022] is a Rust port of the LMAX Disruptor pattern, providing sequence-barrier-based ring buffers with managed consumer threads. Version 4.0.0 implements the full Disruptor feature set: single-producer and multi-producer modes, busy-spin and yielding wait strategies, and a builder pattern for constructing processing topologies.

We benchmarked Photon Ring v0.8.0 against `disruptor` v4.0.0 on the same hardware (Intel i7-10700KF, Linux 6.8, Rust 1.93.1, Criterion, no core pinning). Cross-thread roundtrip latency was 96 ns for Photon Ring versus 133 ns for `disruptor` on x86_64, and 103 ns versus 174 ns on Apple M1 Pro. Publish-only cost was 3 ns versus 24 ns. The latency difference is attributable to two factors: the stamp-in-slot co-location eliminates the separate barrier load on the consumer hot path, and the single-producer path in Photon Ring avoids the sequence-claiming overhead that `disruptor` pays even in its single-producer mode.

`disruptor` v4.0.0 natively supports multi-producer multi-consumer (MPMC) topologies through its sequence-barrier framework. Photon Ring added MPMC support in v0.7.0 via a `fetch_add`-based sequence-claiming protocol with a CAS spin loop for cursor advancement, following the standard Disruptor multi-producer algorithm. Under the MPMC configuration, Photon Ring's CAS overhead narrows the performance gap (see Section 7.3).

`disruptor` provides managed consumer threads and callback-based event handling, which simplifies topology construction but reduces flexibility relative to Photon Ring's poll-based API. `disruptor` also depends on `std` (4 dependencies) and is not `no_std` compatible, whereas Photon Ring requires only `alloc` (2 dependencies: `hashbrown` and `spin`).

### 6.5 bus (Rust)

The `bus` crate [Gjengset, 2016] is a lock-free single-producer multi-consumer broadcast channel. It uses epoch-based synchronization to coordinate readers and writers, providing a simple `broadcast`/`recv` API without topic routing or subscriber grouping. The implementation is notable for its minimalism: zero dependencies, a single source file, and a straightforward epoch-flip protocol.

`bus` differs from Photon Ring in several respects. Its synchronization mechanism is epoch-based rather than seqlock-based: the producer writes to one buffer while consumers read from the other, then flips the epoch. This avoids torn reads entirely but requires double-buffering, which doubles memory usage. The `bus` documentation acknowledges busy-waiting as a limitation, noting that consumers spin-wait without any backoff or yield strategy. Photon Ring provides configurable wait strategies (`BusySpin`, `YieldSpin`, `BackoffSpin`, `Adaptive`) and uses architecture-specific instructions (`WFE` on aarch64, `PAUSE` on x86) to reduce power consumption during idle spins.

No published cross-thread latency benchmarks are available for `bus`, making direct performance comparison difficult. In single-threaded microbenchmarks, Photon Ring's stamp-only fast path achieves 2.5 ns publish-plus-receive compared to approximately 15 ns for `bus`, though the difference in measurement methodology limits the significance of this comparison. `bus` does not support backpressure, topic routing, subscriber groups, or `no_std` environments.

### 6.6 crossbeam (Rust)

The `crossbeam-channel` crate [Tokio contributors, 2017] provides general-purpose bounded and unbounded MPMC channels for Rust. It is the most widely used inter-thread communication primitive in the Rust ecosystem, with mature ergonomics (select macro, timeout support, iterator adapters) and a well-tested correctness record.

The fundamental paradigm difference is queue versus broadcast. `crossbeam-channel` implements point-to-point MPMC queues: each message is consumed by exactly one receiver. Photon Ring implements SPMC broadcast: each message is delivered to all subscribers. These are complementary patterns that serve different use cases. Work-stealing thread pools and task dispatchers are well-served by `crossbeam-channel`; market data fanout, event buses, and sensor-data distribution are better served by broadcast semantics.

Crossbeam's bounded channel achieves approximately 65 ns cross-thread latency on similar hardware [Stjepang, 2019], using a combination of `compare_exchange` operations on the head and tail pointers. The sender blocks when the buffer is full (backpressure), preventing message loss at the cost of potential producer stalls. Photon Ring's default mode is lossy -- the producer never blocks, and slow consumers miss messages -- which is appropriate when freshness matters more than completeness (e.g., market data ticks, sensor readings).

Crossbeam is the more mature and general-purpose library: it provides unbounded channels, zero-capacity rendezvous channels, the `select!` macro for multiplexing, and integration with the broader Rust async ecosystem. Photon Ring does not aim to replace crossbeam; rather, it occupies a narrower niche where broadcast semantics and sub-100 ns latency are the primary requirements.

## 7. Limitations and Future Work

### 7.1 Seqlock memory model undefined behavior

The most fundamental limitation of Photon Ring is that the seqlock protocol involves a data race under the C++20 and Rust abstract memory models. The consumer reads the payload via `ptr::read` while the producer may be concurrently writing to the same memory location via `ptr::write`. Although the seqlock stamp detects this situation after the fact -- the consumer re-checks the stamp and discards any torn read -- the concurrent non-atomic read itself constitutes undefined behavior according to the language specification, regardless of whether the result is used.

This is not a novel problem. The Linux kernel's `seqlock_t` mechanism [Bovet and Cesati, 2005] uses the same pattern pervasively for read-heavy data such as `jiffies`, namespace counters, and filesystem metadata. Facebook's Folly library implements `folly::SharedMutex` using identical semantics. The C++ standards committee (WG21) has acknowledged this formalization gap; proposals such as P1478R7 [Bos and Carter, 2023] aim to introduce `std::byte`-based seqlock support, and discussions around `std::start_lifetime_as` seek to define the semantics of optimistic concurrent reads.

In practice, no extant compiler or hardware architecture produces incorrect behavior for this pattern when applied to `Copy` types on naturally aligned memory. The undefined behavior is "benign" in the same sense as the Linux kernel's seqlock: the abstract machine permits arbitrary behavior, but all concrete implementations produce the expected outcome -- a potentially invalid bit pattern that is always detected and discarded by the stamp verification. MIRI (under `-Zmiri-disable-data-race-check`) does not flag the single-threaded seqlock operations, though it cannot verify the concurrent protocol because its thread scheduler does not model hardware memory ordering.

A future resolution may come from either the language side (atomic `memcpy` proposed for C++26, or a Rust equivalent) or the library side (per-byte atomics, which are correct but impractical for payloads larger than a few bytes). Until such facilities are standardized, Photon Ring accepts this gap -- as does every production seqlock implementation -- and mitigates it through the `T: Copy` bound and the recommendation that users restrict payloads to types with no validity invariants (plain numeric types, `#[repr(C)]` structs of numerics).

### 7.2 T: Copy restriction

The `T: Copy` bound on all Photon Ring types (`Publisher<T>`, `Subscriber<T>`, `Slot<T>`) excludes heap-allocated types such as `String`, `Vec`, `Box`, and reference-counted pointers (`Arc`, `Rc`). This restriction is not merely conservative -- it is load-bearing for safety. A torn read of a non-`Copy` type could produce an invalid pointer value; if a destructor subsequently runs on this value, the result is a double-free, use-after-free, or arbitrary memory corruption. The `Copy` bound guarantees that no destructor executes on torn values, confining the impact of a torn read to a harmless invalid bit pattern.

Even within the `Copy` universe, certain types have validity invariants that a torn read could violate: `bool` must be 0 or 1, `NonZero<u32>` must be nonzero, and reference types (`&T`) must point to valid memory. Photon Ring's documentation recommends restricting payloads to types without validity invariants -- plain numerics (`u8` through `u128`, `f32`, `f64`), fixed-size arrays thereof, and `#[repr(C)]` structs composed exclusively of such types.

A possible relaxation would introduce a marker trait -- tentatively `SafeForSeqlock` -- that users would implement for their payload types, attesting that the type has no validity invariants and is safe to read via `ptr::read` under a torn-read scenario. This would allow types like `#[repr(C)] struct Quote { price: f64, volume: u32, _pad: [u8; 4] }` to be used without requiring `Copy`, though in practice nearly all such types already implement `Copy`. A more ambitious direction would use `ManuallyDrop<T>` with epoch-based reclamation to support non-`Copy` types, at the cost of additional complexity on the read path and a fundamental change to the ownership model.

### 7.3 Multi-producer CAS overhead

The `MpPublisher` introduced in v0.7.0 uses a `fetch_add` on a shared atomic counter to claim sequence numbers and a `compare_exchange_weak` spin loop to advance the cursor in strict order. This is the standard Disruptor multi-producer protocol, adapted for the seqlock stamp encoding.

The CAS spin loop serializes all producers on the cursor cache line: each producer must wait until all prior sequences have committed before it can advance the cursor past its own sequence. Under contention with four producers on an i7-10700KF, the MPMC path exhibits approximately 4.2x higher latency than the SPMC single-producer path. This overhead is inherent to any CAS-based sequence-claiming protocol and is not specific to Photon Ring's implementation.

Possible mitigations include batched CAS (a producer claims N sequence numbers at once, amortizing the CAS cost over N messages), per-producer sub-rings with a merging consumer (eliminates cross-producer contention at the cost of consumer complexity), and FAA-based cursor advancement that relaxes strict ordering (allowing consumers to observe messages slightly out of order). Each of these trades correctness guarantees or API simplicity for throughput under contention; none has been implemented as of v0.8.0.

### 7.4 Platform-specific optimizations

Photon Ring already exploits platform-specific instructions on two architectures. On aarch64, the `WFE` (Wait For Event) instruction is used in the `YieldSpin` and `BackoffSpin` wait strategies: the `SEVL` + `WFE` pattern puts the core into a low-power state until a cache-line invalidation event (caused by the producer's store to the slot stamp) wakes the consumer. On x86, `core::hint::spin_loop()` emits `PAUSE`, which yields the pipeline to the SMT sibling with approximately 140 cycles of delay on Skylake and later microarchitectures.

Intel Tremont and later cores (Alder Lake, Raptor Lake, Lunar Lake) support the `UMONITOR`/`UMWAIT`/`TPAUSE` instruction family (CPUID feature flag WAITPKG). `UMONITOR` registers a cache-line address for monitoring; `UMWAIT` then places the core in a low-power state until the monitored cache line is written to or a timeout expires. This would enable near-zero-latency wakeup without burning CPU cycles -- the consumer would monitor the target slot's stamp cache line and wake on the producer's store, with no spin loop at all. Implementation requires runtime CPUID detection and falls back to `PAUSE`-based spinning on older hardware. This is a high-priority future optimization, as it would simultaneously reduce latency and power consumption.

On RISC-V, the ratified Zawrs extension introduces `WRS.STO` (Wait on Reservation Set, Short Timeout) and `WRS.NTO` (No Timeout), which provide similar cache-line monitoring semantics to ARM's `WFE` and Intel's `UMWAIT`. As RISC-V hardware with Zawrs support becomes available, Photon Ring can add a platform-specific wait strategy branch analogous to the existing aarch64 path.

### 7.5 Formal verification

Photon Ring includes a TLA+ specification (`verification/seqlock.tla`) that models the full seqlock-stamped ring buffer protocol: producer write (odd stamp, data write, even stamp, cursor advance) and consumer read (stamp check, value read, stamp re-check, lag detection). The specification verifies three properties under fair scheduling: safety (NoTornRead -- a consumer never commits a value that was partially written), result validity (ResultIsValid -- committed values are within the expected range), and liveness (ReaderProgress -- if the producer publishes, every idle consumer eventually reads a value). The TLC model checker exhaustively explores the state space for bounded configurations (e.g., 2 readers, ring size 4, 8 maximum sequences).

However, the TLA+ specification operates under sequential consistency -- it does not model the weak memory ordering semantics of real hardware (x86-TSO, ARM's relaxed model, RISC-V RVWMO). Extending the formal verification to weaker memory models would require tools such as the Linux Kernel Memory Model (LKMM), CDSChecker [Norris and Demsky, 2013], or GenMC [Kokologiannakis et al., 2019], which can explore the reorderings permitted by specific hardware memory models. The multi-producer CAS protocol added in v0.7.0 is not yet modeled in the TLA+ specification and represents an important verification gap.

On the dynamic testing side, Loom [Tokio, 2019] -- the Rust concurrency testing framework that exhaustively explores thread interleavings -- cannot be applied to the seqlock pattern because Loom intercepts `std::sync::atomic` operations but does not model non-atomic `ptr::read`/`ptr::write` accesses. The seqlock's optimistic read path, which deliberately performs a non-atomic read concurrent with a potential write, falls outside Loom's interception model. Property-based testing via `proptest` [Altarawneh, 2018] is a viable complement: randomized message sequences, ring sizes, and consumer counts can exercise edge cases (lag recovery, ring wrap-around, torn-read retry) that are difficult to reach with hand-written tests. A `proptest` harness is planned for a future release.

## 8. Conclusion

This paper presented Photon Ring, a seqlock-stamped ring buffer library for Rust that achieves 48 ns one-way inter-thread latency on commodity x86_64 hardware -- within approximately 20% of the cache-coherence protocol floor (34 ns minimum observed, compared to the approximately 40 ns L3 snoop latency on Intel Comet Lake).

The core contribution is the stamp-in-slot co-location: by placing the seqlock stamp and the message payload in a single 64-byte cache line (`#[repr(C, align(64))]`), the consumer validates slot ownership and reads the data in a single cache-line transfer. This eliminates the sequence-barrier load that the Disruptor pattern requires on the consumer hot path, saving one L3-to-L1 snoop per message. Per-consumer local cursors -- plain `u64` values rather than shared atomics -- further eliminate all consumer-to-consumer cache-line contention.

The `SubscriberGroup<T, N>` mechanism extends this efficiency to multi-consumer workloads: when N consumers are polled on the same thread, the seqlock is read once and all N cursors are advanced in a compiler-unrolled loop, reducing per-subscriber fanout cost from 1.1 ns (independent subscribers) to 0.2 ns (grouped), a 5.5x improvement.

These gains come with explicit tradeoffs. The `T: Copy` bound restricts payloads to plain-old-data types, excluding heap-allocated types such as `String` and `Vec`. The default mode is lossy: when the ring wraps, slow consumers miss messages rather than blocking the producer. The seqlock read protocol involves a concurrent non-atomic memory access that is technically undefined behavior under the C++20 and Rust abstract memory models, though it is universally relied upon in practice -- from the Linux kernel to Facebook's Folly library -- and functions correctly on all mainstream hardware for `Copy` types without validity invariants.

The crate is `no_std` compatible (requiring only `alloc`), with two runtime dependencies (`hashbrown` for the topic bus hash map and `spin` for internal locks). Platform-specific optimizations -- `WFE` on aarch64, `PAUSE` on x86 -- reduce idle power consumption without sacrificing wakeup latency. The TLA+ specification in `verification/seqlock.tla` formally verifies the safety property (no torn reads are ever committed), result validity, and liveness (progress under fair scheduling) for bounded configurations under sequential consistency.

Benchmarked against the `disruptor` crate v4.0.0 on two architectures (Intel i7-10700KF and Apple M1 Pro), Photon Ring achieves 96 ns versus 133 ns cross-thread roundtrip latency on x86_64 and 103 ns versus 174 ns on aarch64, with 3 ns versus 24 ns publish cost. These results suggest that the stamp-in-slot design is a practical improvement over the sequence-barrier approach for the single-producer broadcast use case, and that the Rust type system's `Copy` bound provides a disciplined mechanism for ensuring torn-read safety without runtime overhead.

## References

[1] Papamarcos, M. and Patel, J. "A Low-Overhead Coherence Solution for Multiprocessors with Private Cache Memories." *Proceedings of the 11th Annual International Symposium on Computer Architecture (ISCA)*, 1984.

[2] Molka, D. et al. "Memory Performance and Cache Coherency Effects on an Intel Nehalem Multiprocessor System." *Proceedings of the 18th International Conference on Parallel Architectures and Compilation Techniques (PACT)*, 2009.

[3] Intel Corporation. *Intel 64 and IA-32 Architectures Optimization Reference Manual*. Order Number: 248966-045, 2024. Chapter 2: Intel Microarchitecture Code Name Skylake, Section on Cache Latencies.

[4] Thompson, M., Farley, D., Barker, M., Gee, P., and Stewart, A. "Disruptor: High Performance Alternative to Bounded Queues for Exchanging Data Between Concurrent Threads." Technical paper, LMAX Exchange, 2011.

[5] Real Logic. *Aeron: Efficient Reliable UDP Unicast, Multicast, and IPC Message Transport*. https://github.com/real-logic/aeron.

[6] OpenHFT. *Chronicle Queue: Micro Second Messaging That Stores Everything to Disk*. https://github.com/OpenHFT/Chronicle-Queue.

[7] Hennessy, J. and Patterson, D. *Computer Architecture: A Quantitative Approach*. 6th Edition, Morgan Kaufmann, 2017. Chapter 5: Thread-Level Parallelism.

[8] Sorin, D., Hill, M., and Wood, D. *A Primer on Memory Consistency and Cache Coherence*. 2nd Edition, Morgan & Claypool, 2020.

[9] Bovet, D. and Cesati, M. *Understanding the Linux Kernel*. 3rd Edition, O'Reilly Media, 2005. Chapter 5: Kernel Synchronization.

[10] Linux Kernel Documentation. "Sequence counters and sequential locks." `Documentation/locking/seqlock.rst`.

[11] Maurer, H. and Wong, M. "Byte-wise Atomic Memcpy." ISO/IEC JTC1/SC22/WG21 Paper P1478R7, 2022.

[12] `disruptor-rs`: Rust port of the LMAX Disruptor pattern. https://crates.io/crates/disruptor, version 4.0.0.

[13] Lamport, L. *Specifying Systems: The TLA+ Language and Tools for Hardware and Software Engineers*. Addison-Wesley, 2002.

[14] The Rust Reference. "Atomics and Memory Ordering." https://doc.rust-lang.org/reference/memory-model.html.

[15] Norris, B. and Demsky, B. "CDSChecker: Checking Concurrent Data Structures Written with C/C++ Atomics." *Proceedings of the 2013 ACM SIGPLAN International Conference on Object-Oriented Programming, Systems, Languages, and Applications (OOPSLA)*, 2013.

[16] Kokologiannakis, M. et al. "Model Checking for Weakly Consistent Libraries." *Proceedings of the 40th ACM SIGPLAN Conference on Programming Language Design and Implementation (PLDI)*, 2019.

[17] Jonhoo (Gjengset, J.). `bus`: Lock-free, bounded, single-producer, multi-consumer, broadcast channel. https://crates.io/crates/bus.

[18] crossbeam-rs. `crossbeam-channel`: Multi-producer multi-consumer channels for message passing. https://crates.io/crates/crossbeam-channel.

[19] Facebook/Meta. *Folly: Facebook Open-Source Library*. `folly/synchronization/Rcu.h`, seqlock implementation. https://github.com/facebook/folly.
