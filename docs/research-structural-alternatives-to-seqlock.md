# Structural Alternatives to the Seqlock Pattern for Ultra-Low-Latency Broadcast Pub/Sub

**Date:** 2026-03-19
**Context:** Algorithmic design exploration for photon-ring. The current seqlock-stamped slot protocol has a formal data race (UB under Rust's abstract machine). This document evaluates structural alternatives that are formally sound under Rust's memory model while targeting the same performance envelope.

**Baseline (current photon-ring seqlock):**
- Publish: ~2.8 ns
- Cross-thread one-way: ~48 ns (p50)
- Payload: 20-64 bytes (`T: Pod`)
- Per-read cost: 1 stamp load (Acquire) + 1 `read_volatile` + 1 stamp re-check (Acquire) = 3 memory operations
- Zero allocations on hot path
- `no_std` compatible (requires `alloc`)

**Banned approaches:** seqlocks/volatile, locks of any kind, double-buffer/left-right/epoch-based, per-subscriber copies O(N).

---

## Design 1: All-Atomic Seqlock (AtomicU64 Array Payload)

### Description

Replace `UnsafeCell<MaybeUninit<T>>` with `[AtomicU64; N]` where `N = ceil(size_of::<T>() / 8)`. The stamp occupies `slot.stamp` (an `AtomicU64`). The payload is stored as N `AtomicU64` values with `Release` ordering on writes and `Acquire` ordering on reads. The standard seqlock protocol (odd stamp = writing, even stamp = done, re-check after read) detects torn sequences.

**Write protocol:**
1. `stamp.store(odd, Relaxed)` + `fence(Release)` -- identical to current
2. For i in 0..N: `payload[i].store(chunk_i, Release)`
3. `stamp.store(even, Release)`

**Read protocol:**
1. `s1 = stamp.load(Acquire)`
2. For i in 0..N: `chunk_i = payload[i].load(Acquire)`
3. `s2 = stamp.load(Acquire)`
4. If s1 == s2 == expected_even: transmute chunks to T

### Formal Soundness Analysis

**Is this a data race?** No. Every access to shared memory is through `AtomicU64` operations. Atomic operations are excluded from the data-race definition in both C++20 ([intro.races]/2) and Rust's memory model. Two concurrent atomic operations on the same location are never a data race, regardless of ordering.

**Is the happens-before chain correct?** This requires careful analysis:

- **Writer side:** `stamp.store(odd, Relaxed)` followed by `fence(Release)`. The fence synchronizes-with any subsequent `Acquire` load that observes the odd stamp. The payload stores are `Release`, so they are ordered after the fence. The final `stamp.store(even, Release)` is ordered after all payload stores (Release-Release ordering: on x86 this is free; on ARM, each Release store emits a `DMB ISH` before the store, or uses `STLR`, which orders with respect to prior stores).

- **Reader side:** `stamp.load(Acquire)` loads s1. If s1 == expected_even, the Acquire establishes a happens-before with the writer's final `Release` store of the even stamp. However, does this happens-before extend backward through the payload stores?

**Critical ordering question:** If the reader's first stamp load (Acquire) observes the even stamp, does the reader see the payload stores that preceded it?

The answer depends on whether the individual `Acquire` loads on the payload elements synchronize with the corresponding `Release` stores. Under C++20 / Rust atomics:

- `payload[i].store(val, Release)` in the writer
- `payload[i].load(Acquire)` in the reader

These form a synchronizes-with pair **if** the load observes the value written by the store. But can the reader's `payload[i].load(Acquire)` see a **stale** value from a previous write rather than the current one? Yes, in principle -- atomics guarantee no data race, but `Acquire` on one atomic does not order loads from *different* atomic variables.

**The stamp re-check is essential.** If the reader loads s1 (Acquire) = even, then loads all payload chunks (Acquire each), then loads s2 (Acquire) = even, and s1 == s2, then:
- The writer must not have started a new write between s1 and s2 (because s2 is still even).
- But could the payload loads return stale values from an *earlier* write?

On x86 (TSO): No. x86's Total Store Order guarantees that loads are not reordered with other loads. The `Acquire` on s1 prevents the payload loads from being reordered before s1. The `Acquire` on s2 prevents s2 from being reordered before the payload loads. Since s1 == s2 == even, the writer did not modify the slot during the read window. Since the writer's stores are also ordered (Release), and the reader's loads are ordered (Acquire), the reader must see the writer's stores. **Sound on x86.**

On ARM/weakly-ordered: The situation is more subtle. `Acquire` loads emit `LDAR` on AArch64, which prevents subsequent loads from being reordered before the `LDAR`. So:
- `LDAR` stamp (s1) -> all payload loads are guaranteed to occur after s1 in program order
- All payload loads -> `LDAR` stamp (s2) is guaranteed to occur after payload loads

But the question is: does `payload[i].load(Acquire)` necessarily see the value from the writer's `payload[i].store(val, Release)` that preceded `stamp.store(even, Release)`?

The writer's sequence is:
```
  fence(Release)           // orders prior stores before subsequent stores
  payload[0].store(Release)  // S_p0
  payload[1].store(Release)  // S_p1
  ...
  stamp.store(even, Release) // S_stamp
```

The reader observes `S_stamp` via `stamp.load(Acquire)`. By the Release-Acquire rule, the reader's load synchronizes-with the writer's `stamp.store(even, Release)`. This establishes: everything sequenced-before `S_stamp` in the writer *happens-before* everything sequenced-after the `stamp.load(Acquire)` in the reader. The payload stores `S_p0`, `S_p1`, ... are sequenced-before `S_stamp`. The payload loads are sequenced-after the stamp load. Therefore the payload loads **must** see at least the values from `S_p0`, `S_p1`, etc.

**Wait -- the reader loads s1 first (Acquire), then payload, then s2 (Acquire).** The synchronizes-with is established by whichever load actually observes the writer's Release store. If s1 observes the even stamp, the happens-before chain is already established at s1. The payload loads, being sequenced-after s1, must see the payload stores.

**BUT:** There is a subtle problem. The Acquire on s1 synchronizes with the writer's `stamp.store(even, Release)` for the *specific sequence* s1 encodes. But what if between s1 and the payload loads, the writer starts a NEW write (stores odd stamp, begins writing new payload)? Then the payload loads might see a mix of old and new values. The s2 re-check catches this: if s2 != s1, the read is torn and discarded.

**If s1 == s2 == expected_even:** The writer did not modify the stamp during the read window. Since the writer always stores odd *before* modifying payload and even *after*, the payload was not being modified during the read window. The reader's Acquire loads are guaranteed to see the values from the matching Release stores.

**Verdict: FORMALLY SOUND** -- no data race, correct happens-before chain. The only assumption is that `AtomicU64` is lock-free (true on all 64-bit targets).

### Latency Estimate

**Per-read atomic operation count:** 2 stamp loads + N payload loads = N + 2

| Payload size | N (AtomicU64 chunks) | Total atomic loads | Estimated read cost |
|---|---|---|---|
| 20 bytes | 3 | 5 | ~5 ns |
| 24 bytes | 3 | 5 | ~5 ns |
| 32 bytes | 4 | 6 | ~6 ns |
| 48 bytes | 6 | 8 | ~8 ns |
| 64 bytes | 8 | 10 | ~10 ns |

**Cost model for atomic loads:**

On x86-64, `AtomicU64::load(Acquire)` compiles to a plain `MOV`. The `Acquire` ordering does not add a fence instruction on x86 because TSO already provides load-load and load-store ordering. Therefore, N + 2 `Acquire` loads on x86 compile to N + 2 `MOV` instructions with **zero fence overhead**. The cost is purely the data dependency and instruction throughput.

**However:** The cost is NOT simply N * (cost of one MOV). When all N + 2 values are on the same cache line (which they are, since the slot is `align(64)` and payloads <= 56 bytes fit in one line), the cache line is fetched once and subsequent loads hit L1. The incremental cost of each additional `MOV` from the same cache line is approximately 0.25-0.5 ns (one load port cycle at 4-5 GHz).

**Estimated x86-64 read latency for 48-byte payload (hot path, same-thread):**
- 1 cache-line fetch: ~4-5 ns (L1 hit) or ~48 ns (cross-thread, L3 snoop)
- 8 MOV instructions from same cache line: ~2 ns (8 * 0.25 ns at pipeline throughput)
- Total same-thread: ~6-7 ns
- Total cross-thread: ~50 ns (dominated by cache-line transfer)

**On ARM (AArch64):** `Acquire` loads emit `LDAR`, which has additional ordering cost. Each `LDAR` is approximately 5-15 ns more than a plain `LDR` depending on microarchitecture, **but** consecutive `LDAR` instructions from the same cache line can be optimized by the core's load-store unit. Realistic estimate: ~2-4 ns per additional `LDAR` from the same line. Total for 8 loads: ~16-32 ns overhead above the cache-line transfer.

**Optimization:** Replace per-element `Acquire` with a single `fence(Acquire)` after the first stamp load, then use `Relaxed` loads for the payload, then `Acquire` for the final stamp. This is valid because:
- The first stamp `Acquire` establishes the happens-before
- Payload loads with `Relaxed` are still ordered after the `Acquire` load on the same thread (program order + acquire semantics prevent reordering of subsequent loads)
- The final stamp `Acquire` provides the re-check

**Wait -- is this correct?** On x86: yes, TSO prevents load reordering regardless. On ARM: `LDAR` prevents subsequent loads from being reordered before it, but `Relaxed` loads after an `LDAR` are... actually ordered correctly. The ARM architecture reference manual states that `LDAR` establishes a one-way barrier: no subsequent load or store can be reordered before the `LDAR`. So `Relaxed` loads after `stamp.load(Acquire)` are correctly ordered.

**Refined read protocol (optimal):**
1. `s1 = stamp.load(Acquire)` -- prevents subsequent loads from executing before this
2. For i in 0..N: `chunk_i = payload[i].load(Relaxed)` -- safe because of s1's Acquire
3. `s2 = stamp.load(Acquire)` -- prevents reordering of payload loads past this point

**This is still formally sound.** The happens-before from s1 (Acquire) synchronizing with the writer's final stamp store (Release) guarantees visibility of all payload stores. The Relaxed loads are sequenced-after the Acquire load, so they must observe at least the values that were visible at the point of the Acquire.

**Actually, there is a subtlety.** The Acquire on s1 synchronizes-with the Release on the writer's stamp store. This gives us: writer's payload stores *happen-before* reader's s1 load *sequenced-before* reader's payload loads. By transitivity, the writer's payload stores happen-before the reader's payload loads. The reader's payload loads must see at least the values written by the writer. **Correct.**

**With this optimization, the ARM overhead drops to 2 LDAR + N LDR = approximately the same as x86.**

**Estimated cross-thread read latency (optimized):**

| Payload | Ops | x86-64 | AArch64 |
|---|---|---|---|
| 20 B | 2 LDAR + 3 LDR | ~50 ns | ~52 ns |
| 48 B | 2 LDAR + 6 LDR | ~52 ns | ~54 ns |
| 64 B | 2 LDAR + 8 LDR | ~53 ns | ~56 ns |

These are within **10%** of the current seqlock's 48 ns because the cross-thread latency is dominated by the cache-line transfer, not the load instruction count.

### Write-Side Cost

**Per-write:** 1 stamp store (Relaxed) + 1 fence(Release) + N payload stores (Release, or Relaxed + trailing fence) + 1 stamp store (Release)

On x86: All Release stores compile to plain `MOV`. The fence compiles to nothing (TSO). Total: N + 2 MOV instructions. For 48-byte payload: 8 MOVs. Estimated: ~4-5 ns (vs current 2.8 ns).

The write-side overhead increase is ~1.5-2x because the current `write_volatile` does a single `memcpy`-equivalent operation that the compiler can optimize into 1-2 wide stores (e.g., 2x MOVDQA for 48 bytes on AVX2). The atomic version must do 6 individual 8-byte stores. On x86 with store coalescing in the store buffer, this may partially overlap, but the instruction count is higher.

**Optimization for writes:** The writer could use `Relaxed` stores for the payload (since the final stamp `Release` store provides the ordering) and a `fence(Release)` before the first payload store.

```
stamp.store(odd, Relaxed)
fence(Release)        // everything before this is visible before payload stores
payload[0].store(Relaxed)
payload[1].store(Relaxed)
...
stamp.store(even, Release)  // payload stores are visible before even stamp
```

This is valid: the `fence(Release)` + subsequent `stamp.store(even, Release)` provides a *release sequence* that makes all payload stores visible to readers who Acquire the even stamp.

**With Relaxed payload stores on x86:** Cost is identical (MOVs either way). On ARM: avoids STLR per payload element, uses plain STR instead. Significant win on ARM.

### Hardware Requirements

- 64-bit atomics (AtomicU64): all 64-bit targets
- Lock-free AtomicU64: all modern x86-64, AArch64

### Tradeoffs vs Current Seqlock

| Dimension | Current seqlock | All-atomic seqlock |
|---|---|---|
| Formal soundness | UB (data race) | Sound (all atomic) |
| x86 read latency (cross-thread) | ~48 ns | ~50-53 ns (+4-10%) |
| x86 write latency | ~2.8 ns | ~4-5 ns (+50-80%) |
| ARM read latency | similar | +2-4 ns with optimization |
| Code complexity | Simple (memcpy) | Moderate (transmute dance) |
| Compiler optimization | memcpy can be vectorized | Individual stores, no autovect |
| Slot size | `align(64)`, stamp + T | `align(64)`, stamp + [AtomicU64; N] |
| `T: Pod` still required | Yes | Yes (for transmute safety) |
| Miri passes | No | **Yes** |
| `no_std` compatible | Yes | Yes |

### Verdict: **VIABLE**

This is the strongest candidate. The cross-thread latency increase is dominated by the cache-line transfer (~48 ns), so the additional load instructions add only 2-5 ns (~5-10%). The write-side increase of ~50-80% is noticeable but publish is not the bottleneck (readers are). Formally sound, no special hardware, no `std` dependency.

**Key insight:** On x86-64, `Acquire` loads and `Release` stores compile to plain `MOV` instructions with zero fence overhead. The "atomic tax" is the inability to use wide SIMD stores/loads, costing at most a few nanoseconds of instruction throughput on the same cache line.

---

## Design 2: Ring of AtomicU64 Arrays (Stamp Embedded in Payload Array)

### Description

A variant of Design 1 where the stamp is embedded as `payload[0]` rather than a separate `stamp` field. Each slot is `[AtomicU64; N+1]` where `slot[0]` is the stamp and `slot[1..=N]` is the payload.

**Write protocol:**
1. `slot[0].store(odd, Release)` -- marks write in progress
2. For i in 1..=N: `slot[i].store(chunk_i, Relaxed)`
3. `slot[0].store(even, Release)` -- marks write complete

**Read protocol:**
1. `s1 = slot[0].load(Acquire)`
2. For i in 1..=N: `chunk_i = slot[i].load(Relaxed)`
3. `s2 = slot[0].load(Acquire)`
4. If s1 == s2 == expected_even: valid

### Soundness Analysis

Identical to Design 1. The only difference is layout: stamp is `slot[0]` instead of a separate field. The ordering constraints are the same:
- `slot[0].store(even, Release)` synchronizes-with `slot[0].load(Acquire)` on the reader
- Payload stores (Relaxed) are sequenced-after the odd stamp store and before the even stamp store
- Payload loads (Relaxed) are sequenced-after the first `Acquire` load

**Formally sound.** Same proof as Design 1.

### Latency

Identical to Design 1 modulo layout effects.

**Potential layout concern:** If the stamp and first payload element share the same 8-byte-aligned position at offset 0, there is no additional cache-line penalty. The array is contiguous, so for payloads <= 56 bytes, the entire `[AtomicU64; N+1]` fits in one 64-byte cache line (since N+1 <= 8 for 56-byte payloads). Identical to the current Slot layout.

### Tradeoff vs Design 1

The main difference is ergonomic: Design 1 preserves the separate `stamp` field (matching the existing Slot structure), while Design 2 forces the stamp into the array. Design 1 is easier to implement as a drop-in replacement.

### Verdict: **VIABLE** (identical to Design 1, prefer Design 1 for ergonomics)

---

## Design 3: Batched Atomic Copy via SIMD-Width Intrinsics

### Description

Instead of N individual `AtomicU64` loads, use platform SIMD intrinsics to perform wider loads:

- x86-64 SSE2: `_mm_load_si128` -- 16-byte aligned load
- x86-64 AVX2: `_mm256_load_si256` -- 32-byte aligned load
- AArch64: `LDP` (Load Pair) -- 16-byte load

For a 48-byte payload: 3 x 16-byte SSE2 loads = 3 operations, or 1 x 32-byte + 1 x 16-byte = 2 operations with AVX.

### Formal Soundness Analysis

**This is NOT formally sound under Rust's/C++'s memory model.**

The C++ and Rust memory models define atomicity only for operations on `atomic<T>` / `AtomicT` types. SIMD load/store intrinsics like `_mm_load_si128` operate on `__m128i` / `*const __m128i`, which are NOT atomic types. A concurrent write to memory being read by `_mm_load_si128` is a data race, exactly like `read_volatile`.

**Hardware atomicity is not the same as language-level atomicity:**

- Intel's architecture manual guarantees that aligned 16-byte loads via `MOVDQA` are atomic on processors with SSE2 since Nehalem (i.e., the load returns either the old value or the new value, never a mix). However, this is a **hardware** guarantee, not a **language-level** guarantee.
- The Rust abstract machine does not know about `MOVDQA`. It sees a non-atomic access to non-atomic memory. Data race. UB.
- Furthermore, `_mm256_load_si256` (AVX2, 32 bytes) is **NOT guaranteed atomic** even at the hardware level. Intel's manuals are ambiguous on 32-byte atomicity; it depends on whether the store crosses a cache-line boundary and the specific microarchitecture. No reliable 32-byte atomicity guarantee exists.

**Even on x86, with 16-byte aligned loads being hardware-atomic:** The compiler is free to assume no data race and optimize accordingly. LLVM could in principle split a 128-bit load into two 64-bit loads, reorder them, or cache a stale value. In practice, intrinsics are opaque to the optimizer and are emitted as-is. But "in practice" is not "formally sound."

**Comparison to the current `read_volatile`:** The SIMD approach has the *exact same* formal-soundness problem as the current seqlock. You are performing a non-atomic read of memory that may be concurrently written. The fact that the hardware load is wider does not change the abstract-machine analysis. You have merely traded `read_volatile` for `_mm_load_si128` -- both are data races under Rust.

### The 128-bit atomic alternative

If the payload is <= 16 bytes, Rust nightly provides `AtomicU128` (feature `integer_atomics`). On x86-64 this compiles to `CMPXCHG16B` for stores and `CMPXCHG16B` for loads (or `VMOVDQA` on newer compilers that recognize the pattern). This IS formally sound but:

- Only 16 bytes. Not enough for 20-64 byte payloads.
- `CMPXCHG16B` is a read-modify-write operation even for loads, costing ~20 ns per load (vs ~1 ns for a plain `MOV`). Unusable for low-latency.
- On AArch64, `AtomicU128` uses `LDXP`/`STXP` (load-exclusive/store-exclusive pair), which is ~5-10 ns per load. Better, but still expensive.

### Latency Estimate (if we ignore soundness)

| Method | Payload | Ops | Estimated read ns |
|---|---|---|---|
| SSE2 `_mm_load_si128` | 48 B | 3 loads + 2 stamp | ~49 ns cross-thread |
| AVX2 `_mm256_load_si256` | 48 B | 2 loads + 2 stamp | ~49 ns cross-thread |
| Current `read_volatile` | 48 B | 1 memcpy + 2 stamp | ~48 ns cross-thread |
| Design 1 atomics | 48 B | 6 loads + 2 stamp | ~52 ns cross-thread |

The SIMD approach is marginally faster than Design 1 but the difference is negligible (dominated by cache-line transfer) and it provides **no formal-soundness improvement** over the current `read_volatile`.

### Hardware Requirements

- SSE2: all x86-64 processors (universal)
- AVX2: Intel Haswell+ / AMD Excavator+ (2013+)
- 16-byte hardware atomicity: Intel Nehalem+ / AMD Bulldozer+ (specific models, not guaranteed by ISA)
- 32-byte hardware atomicity: **NOT guaranteed by any ISA**

### Verdict: **NOT VIABLE**

This approach solves nothing. It has the same formal UB as the current seqlock (`non-atomic access to concurrently-written memory = data race`). The only difference is using wider non-atomic loads. It does not eliminate the data race; it just changes which instruction performs the racy read.

If the goal is formal soundness, SIMD intrinsics do not help. If the goal is performance, Design 1 (per-u64 atomics) achieves within 5-10% of the SIMD approach while being formally sound.

---

## Design 4: Message Versioning (Append-Only Log)

### Description

Instead of overwriting ring slots, the publisher appends to a monotonically-increasing log. Each message has a sequence number. Readers scan forward from their cursor. No overwrites means no concurrent writes means no data races.

### Analysis

**Attempt 1: Unbounded log**

A simple `Vec`-like buffer where the publisher pushes messages and readers read by index. No overwrites, no data races. Sound.

Problem: Unbounded memory growth. A publisher at 10M messages/sec consumes ~480 MB/sec for 48-byte payloads. This is not viable without garbage collection.

**Attempt 2: Circular log with generation numbers**

Use a ring buffer but instead of overwriting slot N, write to slot (N % capacity) only when all readers have passed it. This is essentially a bounded SPMC queue with backpressure.

Problem: This is the Disruptor pattern. The reader must check a shared cursor or barrier to know if the slot is safe to read. This re-introduces the two-cache-line problem (barrier + slot), which photon-ring was designed to eliminate.

**Attempt 3: Arena with epoch reclamation**

Allocate messages from a bump allocator. Publish an atomic pointer/index to the latest message. Readers load the pointer (Acquire), read the message (no concurrent writes since the writer has moved on to a new arena region), re-check the pointer.

This is an interesting variant:
- Writer bumps an arena pointer and writes the message
- Writer atomically publishes the new index (Release)
- Reader loads the index (Acquire), reads message from the arena
- Since the writer never modifies published messages, the read is not concurrent with any write
- **No data race!** Sound.

**Problem 1:** Arena growth. Without reclamation, the arena grows unboundedly. With reclamation, you need to know when all readers have passed a region. This requires either:
- Per-reader cursors with epoch tracking (like `crossbeam-epoch`): requires `std`
- A fixed-size arena where old regions are known unreachable: requires backpressure guarantees

**Problem 2:** If the arena is fixed-size and circular, the writer eventually wraps around and overwrites old messages. A reader that is still in the old region sees a concurrent write. Back to square one.

**Problem 3:** If we guarantee no wraparound overlap (i.e., the ring is large enough that no reader is ever reading a slot the writer is overwriting), this IS formally sound. But this requires either:
- Very large rings (capacity >> subscriber count * processing time * publish rate)
- Backpressure (writer blocks until slowest reader advances)

Photon-ring already supports bounded channels with backpressure (`channel_bounded`). For bounded channels, the writer never overwrites a slot a reader is reading (the watermark prevents this). **For bounded channels, the current code is ALREADY free of data races on the read path!**

Wait -- is this actually true? Let me re-examine:

In `channel_bounded`, the writer checks `self.seq >= slowest_cursor + effective_capacity` before writing. If the condition holds, it blocks. This means the writer never advances more than `effective_capacity` slots past the slowest reader. As long as `effective_capacity < ring_capacity`, the writer never wraps around to a slot the slowest reader is currently reading.

But the reader's `try_read` still uses `read_volatile`, and the reader could be at slot X while the writer is writing to slot X + effective_capacity. These are DIFFERENT slots. The data race only occurs if the writer and reader access the SAME memory location concurrently. With backpressure, they never do (the writer is always at least 1 slot ahead and at most `effective_capacity` slots ahead, and `effective_capacity < ring_capacity` ensures no overlap).

**However:** The `read_volatile` access is to `self.value.get()`, which is an `UnsafeCell`. Even with backpressure, the reader is accessing non-atomic memory that was previously written by another thread. The question is whether the happens-before chain correctly establishes visibility.

The writer does: `stamp.store(done, Release)` after writing the payload.
The reader does: `stamp.load(Acquire)` before reading the payload.

If the reader observes `stamp == done`, the Release-Acquire pair establishes happens-before. The writer's `write_volatile` is sequenced-before the `stamp.store(done, Release)`. The reader's `read_volatile` is sequenced-after the `stamp.load(Acquire)`. By transitivity, the write happens-before the read.

**But `write_volatile` / `read_volatile` are not atomic operations.** The happens-before chain only applies to atomic operations and operations sequenced around them. The question is: does the happens-before relationship established by the stamp's Release-Acquire pair extend to the non-atomic `read_volatile` / `write_volatile`?

Under C++20 ([intro.races]/10): "An evaluation A *happens before* an evaluation B if A is sequenced before B, or A *inter-thread happens before* B." And ([intro.races]/21): "A visible side effect A on a scalar object M with respect to a value computation B of M satisfies... A happens before B."

The key: `write_volatile` is a *side effect* on memory location M. `read_volatile` is a *value computation* on M. If the write happens-before the read (via the stamp Release-Acquire), **and** there is no other write to M that also happens-before the read, then the read must see the value written by the write.

**Under C++20, this IS well-defined.** The happens-before chain established by the stamp atomics makes the non-atomic accesses well-ordered. The data race condition is: two conflicting accesses where neither happens-before the other. With the stamp, the write happens-before the read. No data race.

**CRITICAL REALIZATION:** The current seqlock in photon-ring is ALREADY sound for the success path (s1 == s2 == expected). The data race only exists for the **torn read** path: when the reader reads the payload while the writer is concurrently modifying it (s1 != s2). In this case, the Release-Acquire chain is broken (the reader does NOT observe the even stamp), so there is no happens-before, and the concurrent access IS a data race.

The UB is specifically: "I read non-atomic memory that is concurrently being written, but I intend to throw away the result." Rust's abstract machine says this is UB regardless of whether you use the result. The read itself is the UB, not the use of the read value.

**This means Design 4 (append-only with backpressure) does NOT help.** The issue is not wraparound; the issue is that `read_volatile` during a concurrent write is UB even if the result is discarded. And the current seqlock performs this speculative read before knowing whether the stamp will match.

### Verdict: **NOT VIABLE**

The append-only approach conflates two different problems. The data race in the seqlock is not caused by slot reuse -- it is caused by the speculative non-atomic read during a potential concurrent write. Design 1 (all-atomic) solves this by making every access atomic. The append-only log does not solve the speculative-read problem.

However, the analysis above reveals an important insight that benefits Design 1: **for the non-torn-read path, the current seqlock's happens-before chain is actually correct.** The UB is localized to the torn-read speculation, which Design 1 eliminates entirely.

---

## Design 5: Cache-Line Ownership Transfer (Pointer-Publishing)

### Description

The writer does not write to a shared slot. Instead:
1. Writer writes the message to a **private** slot in a pool of pre-allocated slots.
2. Writer atomically publishes a pointer (or index) to the written slot via an `AtomicU64`.
3. Reader loads the pointer (Acquire), then reads from the pointed-to slot.
4. Since the writer completed writing before publishing the pointer, and the Acquire load synchronizes with the Release store of the pointer, the read is not concurrent with the write.

The Release-Acquire on the pointer establishes the happens-before chain, making all prior writes (including the non-atomic message write) visible to the reader.

### Formal Soundness Analysis

**Is there a data race?** Let's trace the accesses:

Writer:
1. Writes message to `slots[idx]` -- non-atomic write to private (currently un-published) memory
2. `published_index.store(idx, Release)` -- atomic, synchronizes-with reader's Acquire

Reader:
1. `idx = published_index.load(Acquire)` -- atomic, synchronizes-with writer's Release
2. Reads message from `slots[idx]` -- non-atomic read

For a data race, we need two conflicting accesses to the same location where neither happens-before the other. The writer's write (step 1) is sequenced-before the writer's atomic store (step 2). The reader's atomic load (step 1) synchronizes-with the writer's atomic store (both on `published_index`). The reader's read (step 2) is sequenced-after the reader's atomic load (step 1).

Chain: writer's write -> writer's Release store -> (synchronizes-with) -> reader's Acquire load -> reader's read.

**The writer's write happens-before the reader's read.** No data race. **Formally sound.**

But there is a critical caveat: the writer must not **reuse** `slots[idx]` for a new message while any reader might still be reading it.

### The Reuse Problem

In a ring buffer, the writer will eventually cycle back and overwrite `slots[idx]` with a new message. If a reader is still reading the old message at `slots[idx]`, this is a concurrent write + read = data race.

**How to prevent reuse conflicts:**

**Option A: Double the ring.** Use 2x the number of slots as the current ring. The writer publishes to slot `seq % (2 * capacity)`. Readers are guaranteed to be at most `capacity` slots behind (in a lossy channel, the slot is overwritten and the reader detects lag). With 2x slots, the writer's current position and the readers' positions never overlap.

Problem: This does NOT work for lossy channels. In a lossy channel, a slow reader can be arbitrarily far behind. When the writer wraps around, it will overwrite a slot the reader might still be reading.

**Option B: Backpressure guarantee.** With bounded channels, the writer never advances more than `effective_capacity` past the slowest reader. If the ring has `capacity` slots and `effective_capacity < capacity`, then the writer never writes to a slot any reader is currently reading. **Sound for bounded channels.**

But for lossy channels (the primary use case), this approach fails. A reader might be reading slot K while the writer, having lapped the reader, writes to slot K + capacity which maps to the same physical slot.

**Option C: Per-slot generation counter.** Each slot has an AtomicU64 generation. The writer writes the message, then stores (generation, Release). The reader loads generation (Acquire) before reading, re-checks after. If generation changed, discard.

This IS the seqlock. We have come full circle. The non-atomic message read is still a data race under the torn-read path.

**Option D: No physical slot reuse -- use a pool with publish/reclaim.**

Slots are allocated from a pool. Writer takes a free slot, writes message, publishes index. Readers read from published index. When a reader advances past the index, the slot becomes reclaimable. A garbage collection pass returns old slots to the pool.

This works but requires:
- A concurrent free-list or pool allocator
- A mechanism to determine when all readers have passed a slot (epoch tracking)
- This is essentially `crossbeam-epoch` + a ring of indices

Cost: Pool allocation/deallocation on every publish. Even a fast bump allocator adds ~5-10 ns. Epoch tracking adds ~5-15 ns per read. Total overhead: ~10-25 ns, bringing cross-thread latency to ~60-75 ns.

And this requires `std` for epoch tracking (thread-local storage).

### Lossy Channel Variant: Overwrite + Detect

For lossy channels, accept that readers might occasionally read during a concurrent write. Use an AtomicU64 generation stamp (same as the seqlock) to detect this. But now we're back to the original data race on the torn-read path.

The only way to avoid this for lossy channels is to make the reads atomic (Design 1).

### Verdict: **CONDITIONALLY VIABLE**

**Condition:** Bounded (backpressure) channels only, where the writer is guaranteed never to overwrite a slot being read.

For **bounded channels**, pointer-publishing is formally sound and adds approximately zero overhead beyond the current design (the pointer store replaces the cursor store; reads are identical). In fact, the current photon-ring bounded channel ALREADY has this property -- the only change needed is to replace `read_volatile` / `write_volatile` with non-atomic reads/writes that are correctly ordered by the stamp Release-Acquire. But `read_volatile` / `write_volatile` compile to the same instructions as normal reads/writes on x86. The UB is theoretical, not practical.

For **lossy channels**, this approach requires either (a) making reads atomic (Design 1) or (b) accepting the formal UB.

---

## Design 6: Fat Atomic Publish (Per-Field AtomicU64)

### Description

Identical to Design 1 but framed at the field level: each field of the struct is stored/loaded as an individual `AtomicU64`. For a struct like:

```rust
struct Quote { price: f64, volume: u32, ts: u64, _pad: u32 }  // 24 bytes
```

Store as 3 `AtomicU64` values: `[price_bits, volume_ts_low, ts_high_pad]`.

### Analysis

This is literally Design 1 with a different framing. The same `[AtomicU64; N]` representation, the same ordering constraints, the same soundness proof. The only question is transmute safety, which `T: Pod` already guarantees (all bit patterns valid, `repr(C)`, no padding constraints).

### Verdict: **VIABLE** (identical to Design 1; merged into Design 1 analysis)

---

## Design 7: Segment + Atomic Handoff (Header/Body Split)

### Description

Split the payload into a "header" (first 8 bytes, published as AtomicU64) and a "body" (remaining bytes). Writer writes body to a staging area, then atomically publishes the header which contains both data and a sequence number.

Protocol:
1. Writer writes body bytes to the slot (non-atomic)
2. Writer stores header (AtomicU64, Release) containing sequence + first 8 bytes of payload
3. Reader loads header (AtomicU64, Acquire) -- gets sequence and first 8 bytes
4. Reader reads remaining body bytes (non-atomic)
5. Reader re-checks header (AtomicU64, Acquire)

### Formal Soundness Analysis

**The body read (step 4) is non-atomic.** Is it concurrent with any write?

If the reader observes the header with the expected sequence (step 3, Acquire), then:
- The writer's body write (step 1) is sequenced-before the writer's header store (step 2, Release)
- The reader's header load (step 3, Acquire) synchronizes-with the writer's header store
- The reader's body read (step 4) is sequenced-after the reader's header load

Therefore: writer's body write happens-before reader's body read. **No data race for the current sequence.**

**BUT:** What about the NEXT writer iteration? If the ring wraps around and the writer begins writing a new message to the same slot:
1. Writer stores new header (odd sequence, Release) -- marks write in progress
2. Writer writes new body bytes (non-atomic)

If a reader is still at step 4 (reading the old body) when step 2 begins, the reader's read and the writer's write are concurrent on the same memory location, and neither happens-before the other. **DATA RACE.**

**Mitigation via re-check (step 5):** If the reader re-checks the header after reading the body and the header still matches, then the writer has NOT started a new write during the read window. But the abstract-machine UB analysis is the same as the seqlock: the speculative non-atomic read of the body is UB if it happens to be concurrent with a write, even if the re-check subsequently detects the conflict and discards the result.

### Key Difference from Design 1

Design 1 solves this by making ALL accesses (including the body/payload) atomic. Design 7 only makes the header atomic. The body access is still non-atomic and subject to the same torn-read UB as the current seqlock.

Design 7 reduces the number of atomic operations (1 atomic header + non-atomic body) but retains the fundamental soundness problem for the body.

**Marginal improvement:** If the header contains enough information for the reader to make decisions (e.g., "is this message relevant to me?"), the reader might skip the body read entirely for irrelevant messages. This reduces the frequency of the racy body read but does not eliminate it.

### Latency Estimate

For messages where the reader reads the body:
- 2 atomic loads (header x2) + non-atomic body read
- Approximately the same as the current seqlock

For messages where the reader skips the body:
- 1 atomic load (header check) + branch
- Very fast: ~1-2 ns if the header is already cached

### Verdict: **NOT VIABLE** (for soundness)

This is a minor optimization of the existing seqlock, not a replacement. It retains the formal data race on the body access. The only scenario where it helps is if the reader can make decisions based on the 8-byte atomic header alone, which is application-specific and not general.

---

## Summary Matrix

| Design | Formally Sound? | x86 Cross-Thread Latency | Write Latency | `no_std` | Hardware Reqs |
|---|---|---|---|---|---|
| **Current seqlock** | NO (data race) | ~48 ns | ~2.8 ns | Yes | 64-bit |
| **1. All-atomic seqlock** | **YES** | ~50-53 ns (+5-10%) | ~4-5 ns (+50-80%) | Yes | 64-bit AtomicU64 |
| **2. AtomicU64 array** | **YES** | ~50-53 ns (+5-10%) | ~4-5 ns (+50-80%) | Yes | 64-bit AtomicU64 |
| **3. SIMD intrinsics** | NO (same as seqlock) | ~49 ns (~0%) | ~2.8 ns (~0%) | Yes | SSE2/AVX2 |
| **4. Append-only log** | NO (torn-read path) | N/A | N/A | N/A | N/A |
| **5. Pointer-publishing** | **Bounded only** | ~48 ns (~0%) | ~2.8 ns (~0%) | Yes | 64-bit |
| **6. Fat atomic** | **YES** (= Design 1) | ~50-53 ns | ~4-5 ns | Yes | 64-bit AtomicU64 |
| **7. Header/body split** | NO (body race) | ~48 ns (~0%) | ~2.8 ns (~0%) | Yes | 64-bit |

---

## Recommendation

### Primary: Design 1 -- All-Atomic Seqlock

**This is the clear winner.** It is:
- **Formally sound** under Rust's memory model (no data race -- all accesses are atomic)
- **Within 5-10% of the current seqlock's cross-thread latency** (dominated by cache-line transfer)
- **`no_std` compatible** (uses only `core::sync::atomic`)
- **Passes Miri** (no UB under the abstract machine)
- **Zero special hardware requirements** beyond 64-bit atomics
- **Drop-in replacement** for the current `Slot<T>` implementation

The write-side penalty (~50-80% slower) is the main cost, but publish is already ~2.8 ns and is rarely the bottleneck. A 4-5 ns publish cost is still negligible compared to cross-thread transfer latency (~48 ns).

### Implementation Sketch

```rust
#[repr(C, align(64))]
pub(crate) struct AtomicSlot<const N: usize> {
    stamp: AtomicU64,
    payload: [AtomicU64; N],
}

impl<const N: usize> AtomicSlot<N> {
    fn write(&self, seq: u64, data: &[u64; N]) {
        let writing = seq * 2 + 1;
        let done = seq * 2 + 2;
        self.stamp.store(writing, Ordering::Relaxed);
        fence(Ordering::Release);
        for i in 0..N {
            self.payload[i].store(data[i], Ordering::Relaxed);
        }
        self.stamp.store(done, Ordering::Release);
    }

    fn try_read(&self, seq: u64) -> Option<[u64; N]> {
        let expected = seq * 2 + 2;
        let s1 = self.stamp.load(Ordering::Acquire);
        if s1 != expected { return None; }
        let mut data = [0u64; N];
        for i in 0..N {
            data[i] = self.payload[i].load(Ordering::Relaxed);
        }
        let s2 = self.stamp.load(Ordering::Acquire);
        if s1 == s2 { Some(data) } else { None }
    }
}
```

The `T: Pod` transmute from `[u64; N]` to `T` (and vice versa) is safe because:
- `T: Pod` guarantees all bit patterns are valid
- `T` is `repr(C)` (Pod requirement implies this for user types)
- `size_of::<T>() <= N * 8` (with possible trailing padding zeroed)

### Secondary: Design 5 for Bounded Channels

For users of `channel_bounded` who want zero overhead, pointer-publishing with backpressure is formally sound and matches the current seqlock's exact performance. The implementation change is minimal: the existing code is already correct under the abstract machine for bounded channels where the writer never overwrites a slot a reader is accessing. The only formal concern is that `read_volatile` / `write_volatile` are used instead of normal reads/writes (volatile is not needed when happens-before is established by the stamp atomics). Replacing `read_volatile` with `ptr::read` and `write_volatile` with `ptr::write` would make the bounded-channel code formally sound with zero performance change.

### Future: Wait for `atomic_memcpy`

When Rust's `atomic_memcpy` RFC lands, the current `read_volatile` / `write_volatile` approach will become formally sound for ALL channel types (lossy and bounded). At that point, the all-atomic seqlock can be removed in favor of the original, slightly faster seqlock pattern. Design 1 is therefore a sound bridge to the future language-level fix.

---

## Appendix A: x86 Atomic Ordering Cost Model

On x86-64 (TSO), the actual machine instructions generated by Rust atomics:

| Operation | Instruction | Fence overhead |
|---|---|---|
| `AtomicU64::load(Relaxed)` | `MOV` | 0 |
| `AtomicU64::load(Acquire)` | `MOV` | 0 (TSO provides load-load order) |
| `AtomicU64::load(SeqCst)` | `MOV` | 0 (TSO provides load-load order) |
| `AtomicU64::store(Relaxed)` | `MOV` | 0 |
| `AtomicU64::store(Release)` | `MOV` | 0 (TSO provides store-store order) |
| `AtomicU64::store(SeqCst)` | `MOV` + `MFENCE` (or `XCHG`) | ~20-33 ns |
| `fence(Acquire)` | no-op (compiler fence only) | 0 |
| `fence(Release)` | no-op (compiler fence only) | 0 |
| `fence(SeqCst)` | `MFENCE` | ~20-33 ns |

**Key takeaway:** On x86-64, `Acquire` loads and `Release` stores are free. The "cost" of atomics is zero above the cost of the memory access itself. The only expensive atomic operation is `SeqCst` store (which requires `MFENCE` or `XCHG`). Design 1 uses no `SeqCst` operations.

## Appendix B: AArch64 Atomic Ordering Cost Model

On AArch64, the situation is different:

| Operation | Instruction | Overhead vs plain LDR/STR |
|---|---|---|
| `AtomicU64::load(Relaxed)` | `LDR` | 0 |
| `AtomicU64::load(Acquire)` | `LDAR` | ~1-5 ns (pipeline stall) |
| `AtomicU64::store(Relaxed)` | `STR` | 0 |
| `AtomicU64::store(Release)` | `STLR` | ~1-5 ns |
| `fence(Acquire)` | `DMB ISHLD` | ~5-15 ns |
| `fence(Release)` | `DMB ISH` | ~5-15 ns |

The optimized Design 1 read protocol (2 x `LDAR` + N x `LDR`) minimizes the ARM ordering overhead to exactly 2 barrier-inducing instructions per read, regardless of payload size.

## Appendix C: Happens-Before Proof for Relaxed Payload Loads

**Claim:** In the optimized Design 1 read protocol, `Relaxed` payload loads between two `Acquire` stamp loads are guaranteed to observe the writer's payload stores.

**Proof:**

Let W be the writer, R the reader.

W's execution:
```
W1: stamp.store(odd, Relaxed)
W2: fence(Release)
W3: payload[i].store(val_i, Relaxed)  for each i
W4: stamp.store(even, Release)
```

R's execution:
```
R1: s1 = stamp.load(Acquire)          // observes W4's even value
R2: chunk_i = payload[i].load(Relaxed) for each i
R3: s2 = stamp.load(Acquire)          // observes W4's even value
```

R1 synchronizes-with W4 (Acquire load observes Release store on same atomic).
Therefore: everything sequenced-before W4 *happens-before* everything sequenced-after R1.

W3 (all payload stores) are sequenced-before W4. Therefore W3 happens-before R1.
R2 (all payload loads) are sequenced-after R1. Therefore R1 happens-before R2.
By transitivity: W3 happens-before R2.

Since W3 (the payload stores) happen-before R2 (the payload loads), R2 must observe at least the values written by W3. If no intervening write exists (confirmed by R3 == R1), R2 observes exactly W3's values.

**The Relaxed ordering on R2 is sufficient** because the happens-before relationship is established by the stamp Acquire/Release pair, not by the payload loads themselves. QED.
