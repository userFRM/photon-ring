# Hardware-Level Alternatives to the Seqlock Pattern

**Date:** 2026-03-19
**Scope:** Hardware design exploration for photon-ring
**Baseline:** ~3ns publish, ~48ns cross-thread one-way latency (i7-10700KF, seqlock with `write_volatile`/`read_volatile`)
**Problem:** `write_volatile`/`read_volatile` on non-atomic memory is a data race under Rust's abstract machine. We want the same performance without the formal UB.

---

## Framing

The fundamental constraint: Rust's largest atomic type is `AtomicU128` (16 bytes, nightly). For payloads of 20-64 bytes, multiple loads are required. If the writer is concurrent with any of those loads, the Rust abstract machine declares undefined behavior -- regardless of whether the value is ultimately used.

The seqlock "solution" is: do the loads anyway, re-check a sequence stamp, discard torn reads. The value is never *used* when torn, and `T: Pod` guarantees no bit pattern is invalid. But the abstract machine does not care about "used" -- the data race itself is UB.

We are looking for hardware mechanisms that provide atomicity guarantees wide enough to eliminate the data race entirely, or that can be wrapped in `asm!` blocks where the data race does not exist (inline assembly operates outside the Rust abstract machine on the hardware ISA directly).

For each mechanism: what it is, whether it provides true atomicity, how it maps to Rust, estimated latency impact, hardware requirements, and a final verdict.

---

## x86-64 Mechanisms

### 1. MOVDIR64B (Ice Lake+)

**What it is:** A single instruction that writes 64 bytes from a source memory operand to a 64-byte-aligned destination, using a non-temporal direct store protocol. Introduced in Ice Lake client / Sapphire Rapids server. Documented in Intel SDM Vol. 2, Chapter 4.

**How it would work:** Publisher writes the payload to a staging buffer (stack-local, no aliasing), then uses `MOVDIR64B` to atomically blast 64 bytes into the slot.

**Atomicity guarantee:** Intel SDM states MOVDIR64B provides "64-byte write atomicity at the destination" -- the entire 64-byte write is visible to other cores as a single atomic unit. This is a genuine hardware atomicity guarantee, confirmed in the Intel Architecture Instruction Set Extensions Programming Reference (rev 046, March 2024), Section 7.3.

**The problem:** There is no corresponding 64-byte atomic LOAD instruction. MOVDIR64B solves the write side only. The reader still needs to read 64 bytes, which decomposes into multiple cache-line-width loads (or multiple 8-byte MOVs). If the writer fires MOVDIR64B concurrently with the reader's multi-MOV sequence, the reader can see a partial update -- exactly the same torn-read problem we already have.

You might think: "but the write is atomic, so the reader either sees the old value or the new value, never a mix." That is true IF the read is also atomic. But a multi-MOV read is not atomic. The reader could load bytes 0-7 (old), then the writer's MOVDIR64B fires and atomically replaces all 64 bytes, then the reader loads bytes 8-15 (new). Torn read.

**Rust soundness:** Even if we wrapped MOVDIR64B in `asm!`, we still need a 64-byte atomic read, which does not exist. The read side remains a data race under the abstract machine.

**Latency impact:** MOVDIR64B is a non-temporal store -- it bypasses the cache hierarchy and writes directly to memory (or the write-combining buffer). This means it is NOT cache-coherent in the traditional sense. On the write side, it avoids the RFO (Read For Ownership) stall, potentially saving 20-40ns. But the non-temporal nature means the reader must do an SFENCE + re-read from memory rather than L3 -- adding 50-100ns of latency. Net effect: likely SLOWER than the current seqlock for the cross-thread pub/sub use case, where L3 snooping is the fast path.

**Required hardware:** Intel Ice Lake+ (client), Sapphire Rapids+ (server). Not available on AMD as of Zen 5.

**VERDICT: NOT VIABLE.** Solves the write side only. No 64-byte atomic read exists. Non-temporal semantics make cross-core latency worse, not better.

---

### 2. AVX-512 VMOVDQA64 (Skylake-X+)

**What it is:** A 512-bit (64-byte) aligned SIMD load/store instruction. Moves an entire ZMM register (64 bytes) to/from a 64-byte-aligned memory location in a single instruction.

**How it would work:** Publisher stores the payload into a ZMM register, then uses `VMOVDQA64` to write 64 bytes to the slot in one instruction. Reader uses `VMOVDQA64` to load 64 bytes from the slot in one instruction. If both are single-instruction operations, perhaps the hardware provides atomicity?

**Atomicity guarantee:** Intel SDM Volume 3, Section 9.1.1 ("Guaranteed Atomic Operations"):
- Aligned 8-byte loads/stores are guaranteed atomic.
- Aligned 16-byte loads/stores are guaranteed atomic on processors that support SSE2 (MOVDQA/MOVDQU).
- **There is NO guarantee for 32-byte or 64-byte loads/stores.** The SDM explicitly does NOT extend atomicity guarantees to AVX/AVX-512 widths.

The microarchitectural reality: on Skylake-X and Ice Lake, a 64-byte `VMOVDQA64` store decomposes internally into two 32-byte halves hitting the store buffer as separate entries. A 64-byte `VMOVDQA64` load decomposes into two 32-byte load micro-ops. The cache protocol handles each 32-byte half independently. There is no architectural guarantee that another core observes both halves atomically.

This was confirmed by Travis Downs' testing (2019) and Intel's own responses on the Intel Developer Zone: AVX-512 loads and stores are NOT atomic at the 64-byte width. They appear atomic in MOST cases because the cache line is the unit of coherence and both halves hit the same line, but this is a microarchitectural coincidence, not an ISA guarantee. A concurrent writer can interleave between the two 32-byte halves.

Even the 32-byte AVX2 case (`VMOVDQA256`) lacks a formal atomicity guarantee in the SDM, though it is widely believed to be atomic on all shipping implementations. "Widely believed" is not a spec guarantee and is not sufficient to eliminate formal UB.

**Rust soundness:** Even wrapped in `asm!`, the lack of an ISA-level atomicity guarantee means we cannot claim the operation is data-race-free. The inline assembly would be correct on every shipping microarchitecture, but a future microarchitecture could legally split the operation differently. This is the same "works in practice, not guaranteed by spec" situation as the current seqlock.

**Latency impact:** If we assume it IS atomic (on current hardware): a single `VMOVDQA64` load is ~5-7 cycles (L1 hit), roughly equivalent to the current `read_volatile` multi-MOV sequence which LLVM already vectorizes on most payloads. No latency benefit. On the write side, same story. The seqlock overhead is the stamp check (2 atomic loads, ~2ns), not the data copy.

**Additional cost:** AVX-512 instructions cause frequency throttling on many Intel processors. Heavy AVX-512 use drops the core from Turbo Tier 0 to Tier 2, reducing clock speed by 200-400 MHz. This would INCREASE latency on the rest of the hot path. Intel has removed AVX-512 from E-cores on Alder Lake+ and restricted it significantly on P-cores.

**Required hardware:** Intel Skylake-X / Cannon Lake+ (with AVX-512). AMD Zen 4+ (limited AVX-512 support at 256-bit execution width, doubled internally -- even worse for atomicity).

**VERDICT: NOT VIABLE.** No ISA-level atomicity guarantee for 64-byte or even 32-byte operations. Frequency throttling on Intel makes it actively harmful. Does not solve the formal problem.

---

### 3. Intel TSX (HLE/RTM) -- Hardware Transactional Memory

**What it is:** Intel Transactional Synchronization Extensions, introduced in Haswell. `XBEGIN` starts a hardware transaction, `XEND` commits it. All memory operations between XBEGIN and XEND appear atomic to other cores. If a conflict is detected (another core wrote to the same cache line), the transaction aborts and control transfers to a fallback path.

**How it would work (writer):**
```
XBEGIN fallback
  write 64 bytes to slot
  write stamp (done)
XEND
fallback:
  ; use regular seqlock as fallback
```

**How it would work (reader):**
```
XBEGIN fallback
  load stamp
  load 64 bytes from slot
  load stamp (re-check)
  if stamps don't match: XABORT
XEND
fallback:
  ; use regular seqlock as fallback
```

Within an RTM transaction, the hardware tracks the read-set and write-set at cache-line granularity. If the writer modifies the slot's cache line while a reader's transaction is in-flight, the reader's transaction aborts (conflict detected via cache coherence protocol). The reader retries. If no conflict, the reader's transaction commits and the snapshot is guaranteed consistent -- the hardware enforced that no writer touched the cache line during the read.

**Atomicity guarantee:** STRONG. RTM provides true hardware-enforced multi-word atomicity. The Intel SDM (Vol. 1, Chapter 16) guarantees that a committed transaction's memory operations are serializable with respect to all other cores. This is not "appears atomic" -- it IS atomic at the hardware level, enforced by the L1 cache coherence protocol.

**Rust soundness:** This is where it gets interesting. If the load/store sequence is entirely within an `asm!` block (inline assembly), the operations are outside the Rust abstract machine. The Rust compiler does not reason about the memory operations inside `asm!` -- it treats them as opaque. The `asm!` block would need `~{memory}` clobbers to prevent the compiler from reordering Rust-level operations across it, but the actual loads/stores inside the transaction are ISA-level operations, not Rust-level operations. There is no "data race" inside an `asm!` block because Rust's data race definition applies to Rust's memory model, not to inline assembly.

However: the fallback path IS Rust code. If the transaction aborts (which happens routinely -- TSX abort rates of 5-20% are normal), the fallback must use the existing seqlock, which HAS the data race. So TSX only eliminates the UB on the commit path, not the abort path. You still need a formally-sound fallback.

If the fallback is a regular `Mutex` or `RwLock`, you are formally sound but the abort path has 50-200ns overhead (mutex acquisition). The happy path (transaction commits) has ~10-20ns overhead for XBEGIN/XEND.

**But there is a bigger problem:** Intel has effectively abandoned TSX.
- **Haswell/Broadwell:** TSX had a hardware bug (HLE was broken, patched via microcode to NOP). RTM was disabled on many steppings.
- **Skylake/Kaby Lake/Coffee Lake:** TSX was re-enabled but still had errata (TAA side-channel vulnerability, CVE-2019-11135). Many cloud providers and Linux distros disable TSX via microcode/kernel parameter (`tsx=off`).
- **Ice Lake+:** TSX is present but deprecated. Intel's "Restricted Transactional Memory" (RTM) is still there but marked as "may be removed in future processors" in the SDM.
- **Alder Lake+ (hybrid):** TSX is completely disabled (E-cores do not support it). The `CPUID` flag is cleared.
- **Sapphire Rapids / Emerald Rapids server:** TSX is present but must be explicitly enabled. Many data center deployments keep it disabled for security.
- **Arrow Lake / Lunar Lake / Panther Lake:** TSX has been fully removed from the ISA.

As of 2026, TSX is essentially dead on Intel client parts and dying on server parts. AMD never implemented TSX.

**Latency impact:** On hardware where TSX is available and enabled:
- Writer: XBEGIN costs ~8-15 cycles, XEND costs ~8-12 cycles. Total overhead: ~20-27 cycles (~5-7ns at 4 GHz). This is SIGNIFICANT -- the current seqlock publish path is ~3ns.
- Reader: XBEGIN + XEND: same ~20-27 cycle overhead. Plus the abort penalty when the writer is concurrent: an abort costs ~50-100 cycles, then the fallback path runs.
- Net: ~2x slower on the publish path, ~2x slower on the read path, with occasional abort spikes. Cross-thread latency goes from ~48ns to ~60-70ns on the happy path.

**Required hardware:** Intel Skylake through Ice Lake (with TSX enabled via BIOS/microcode). NOT available on Alder Lake+, Arrow Lake+, any AMD processor, or any ARM processor.

**VERDICT: NOT VIABLE (hardware is dead).** TSX provides the cleanest theoretical solution -- true hardware atomicity for arbitrary-width operations -- but Intel has killed it. Building a critical path dependency on a deprecated, widely-disabled feature that has been removed from new hardware is not a sound engineering decision. If TSX had survived, this would be the answer.

---

### 4. LOCK CMPXCHG16B Chaining (x86-64, all processors)

**What it is:** `LOCK CMPXCHG16B` (compare-and-swap 16 bytes) is the widest atomic RMW operation on x86-64. Available on all x86-64 processors with the `cx16` CPUID flag (which is all of them since AMD K8 rev F / Intel Core 2). Provides a 128-bit (16-byte) atomic compare-and-swap.

**How it would work:** Decompose a 32-byte payload into two 16-byte halves. Writer does:
1. `LOCK CMPXCHG16B` on bytes 0-15 (first half)
2. `LOCK CMPXCHG16B` on bytes 16-31 (second half)

Reader does:
1. `LOCK CMPXCHG16B` (compare with expected, swap with same value -- effectively an atomic 16-byte load) on bytes 0-15
2. `LOCK CMPXCHG16B` on bytes 16-31

For 64 bytes: four `LOCK CMPXCHG16B` operations.

**Atomicity guarantee:** Each individual `LOCK CMPXCHG16B` is fully atomic (Intel SDM Vol. 2, CMPXCHG16B entry). But the PAIR of operations is NOT atomic. Between the writer's first and second CMPXCHG16B, a reader can observe the first half updated but the second half stale. This is the classic multi-word CAS problem.

To make the pair atomic, you need a meta-protocol:
- **Double-check seqlock:** Wrap the multi-CMPXCHG16B sequence in a seqlock. But then you still have the seqlock's data race problem (the stamp check is fine, but the actual payload reads between the stamp loads are data races if the writer is mid-write). Wait -- no. If every access is via `LOCK CMPXCHG16B`, every access IS atomic. The question is whether the composition is consistent.

Actually, there is a subtlety here. If the reader uses `LOCK CMPXCHG16B` (expected=0, new=0) as an atomic 128-bit load on each half, and the writer uses `LOCK CMPXCHG16B` to write each half, then EACH HALF is read and written atomically. But the reader can see half-A-new and half-B-old. You still need a consistency check.

The seqlock + multi-atomic-load pattern works like this:
1. Reader loads stamp (AtomicU64, Acquire). Stamp is even = complete.
2. Reader loads payload half A via `LOCK CMPXCHG16B` (atomic 128-bit load).
3. Reader loads payload half B via `LOCK CMPXCHG16B` (atomic 128-bit load).
4. Reader re-loads stamp (AtomicU64, Acquire). If stamp unchanged, snapshot is consistent.

The key insight: each individual load in steps 2-3 is an atomic operation (not a data race). The question is whether steps 2-3 racing with a concurrent writer is a data race.

**It is NOT a data race** if each individual access is atomic. The Rust/C++ memory model defines a data race as two conflicting accesses (to the same location, at least one a write) where neither happens-before the other AND at least one is non-atomic. If ALL accesses are atomic, there is no data race -- there is only a question of ordering. The seqlock stamp provides the consistency guarantee: if both stamp reads return the same even value, no writer intervened, so the snapshot is consistent.

**Rust soundness:** This is formally sound IF:
- The stamp is `AtomicU64` (already is).
- Each 16-byte chunk of the payload is `AtomicU128` (or accessed via `LOCK CMPXCHG16B` in `asm!`).
- All operations are atomic. No data race exists.

The problem: `AtomicU128` is nightly-only in Rust. On stable Rust, you cannot do a 128-bit atomic load. You CAN use inline assembly with `LOCK CMPXCHG16B`, which sidesteps the Rust abstract machine entirely.

For 64 bytes: 4 x `AtomicU128` (or 4 x `LOCK CMPXCHG16B`) + 2 stamp loads = 6 atomic operations per read, 4 atomic operations + 2 stamp stores per write.

**Latency impact:** `LOCK CMPXCHG16B` has a latency of ~15-25 cycles on modern Intel (Agner Fog's instruction tables: Skylake = 15 cycles, Ice Lake = 18 cycles). This is because the `LOCK` prefix acquires exclusive cache-line ownership (same as a full memory fence for that line).

For 64-byte payload:
- Writer: 4 x LOCK CMPXCHG16B + 2 stamp stores = 4 * 18 + 2 * 1 = ~74 cycles = ~19ns at 4 GHz. Current seqlock write: ~12 cycles (~3ns). **6x slower on write.**
- Reader: 4 x LOCK CMPXCHG16B + 2 stamp loads = ~74 cycles. But wait -- the reader uses LOCK CMPXCHG16B as a "load" by doing CAS(expected=current, new=current). This still acquires exclusive ownership of the cache line, which means:
  - Multiple readers CONTEND on the same cache line. Each reader's LOCK CMPXCHG16B bounces the line between cores. The entire SPMC broadcast benefit is destroyed.
  - With N readers, each read requires N-1 cache-line transfers. For 4 subscribers: 4 * 3 * 50ns = 600ns of coherence traffic per message.

**This is catastrophic for broadcast pub/sub.** The LOCK prefix on the read side turns every reader into a writer (from the cache coherence perspective). SPMC becomes MPMC contention. The fundamental advantage of photon-ring -- zero read-side contention -- is obliterated.

**Mitigation:** Use `LOCK CMPXCHG16B` only on the write side and non-locked `MOVDQA` (128-bit aligned load) on the read side. `MOVDQA` is guaranteed atomic for aligned 16-byte loads on all SSE2+ processors (Intel SDM Vol. 3, Section 9.1.1). This gives atomic 16-byte loads without cache-line locking. But `MOVDQA` is not a Rust atomic operation -- it is a SIMD operation. To make it formally atomic in Rust, you would need `AtomicU128::load()` which compiles to `MOVDQA` on x86 (it does on nightly Rust, verified in Godbolt).

**Revised approach (AtomicU128 loads, no LOCK on read side):**
- Reader: 4 x `AtomicU128::load(Acquire)` (compiles to `MOVDQA` -- no lock prefix, no contention) + 2 x `AtomicU64::load(Acquire)`.
- Writer: 4 x `AtomicU128::store(Release)` (compiles to `MOVDQA` + `SFENCE` or `XCHG`; but `MOVDQA` store is also atomic for aligned 16-byte) + 2 x `AtomicU64::store(Release)`.
- No LOCK prefix needed on either side. No contention. Each individual access is atomic.

**Latency for the revised approach (4 x AtomicU128):**
- Write: 4 x MOVDQA store (4 cycles each on L1) + stamp stores = ~18 cycles = ~4.5ns. vs current ~3ns. **1.5x overhead.**
- Read: 4 x MOVDQA load (4 cycles each) + stamp loads + branch = ~20 cycles = ~5ns of local work. Cross-thread latency dominated by cache coherence (~40-50ns), same as current.
- Net cross-thread latency: ~50-55ns vs current ~48ns. **Marginal overhead (~10-15%).**

**Required hardware:** All x86-64 processors with SSE2 (all of them). `AtomicU128` additionally requires nightly Rust.

**VERDICT: VIABLE (with caveats).** The 4 x `AtomicU128` seqlock pattern eliminates ALL formal data races. Every memory access is atomic. The seqlock stamp provides consistency. The overhead is ~10-15% on read latency and ~50% on publish latency compared to the current `write_volatile`/`read_volatile` approach. This is the most promising approach for 32-64 byte payloads on x86-64. The caveats: (1) requires nightly Rust for `AtomicU128`, (2) payloads must be restructured as arrays of `AtomicU128`, (3) not available on stable Rust without inline `asm!`. See Mechanism #12 for the full cross-platform analysis.

---

### 5. Cache Line Locking (x86-64)

**What it is:** The hypothesis that x86 guarantees an aligned store to a 64-byte region (one cache line) is visible atomically to readers, because the L1 cache handles the entire line as a unit.

**Atomicity guarantee:** This is FALSE. Intel SDM Vol. 3, Section 9.1.1 is explicit:
> "The Intel-64 memory ordering model guarantees that, for each of the following memory-access instructions, the constituent memory operation appears to execute as a single memory access regardless of memory type:
> - Instructions that read or write a single byte.
> - Instructions that read or write a word (2 bytes) whose address is aligned on a 2-byte boundary.
> - Instructions that read or write a doubleword (4 bytes) whose address is aligned on a 4-byte boundary.
> - Instructions that read or write a quadword (8 bytes) whose address is aligned on an 8-byte boundary.
> - Instructions that read or write a double quadword (16 bytes) whose address is aligned on a 16-byte boundary (i.e., SSE instructions that access a 16-byte aligned memory location)."

Note: 32-byte and 64-byte are NOT on this list. The guarantee stops at 16 bytes.

The cache line IS the unit of coherence in the MESI protocol, but that means the TRANSFER of the line between cores is atomic -- the entire line is invalidated and refetched as a unit. It does NOT mean that a 64-byte STORE is atomic with respect to a concurrent 64-byte LOAD on another core. A 64-byte store decomposes into multiple store-buffer entries, and a concurrent load on another core can observe a partial subset.

In practice, on current Intel microarchitectures, a 64-byte aligned store within a single cache line APPEARS atomic because the store buffer commits to L1 as a single unit for naturally-aligned operations within a line. But this is a microarchitectural implementation detail, not an ISA guarantee. Intel explicitly reserves the right to break this in future implementations.

**Rust soundness:** Even if we could prove it works on current hardware, it is not formally guaranteed by the ISA and therefore cannot eliminate formal UB.

**VERDICT: NOT VIABLE.** The ISA does not guarantee cache-line-width atomicity. Building on an undocumented microarchitectural coincidence is exactly the same class of problem as the current seqlock -- "works in practice, not guaranteed by spec."

---

### 6. PREFETCHW + CLDEMOTE

**What it is:** `PREFETCHW` brings a cache line into Exclusive state (enabling a subsequent write without an RFO stall). `CLDEMOTE` (Tremont+, Alder Lake+) demotes a cache line from the local L1/L2 to the L3 (shared) level, making it available to other cores faster.

**How it would work:** Publisher writes the slot, then uses `CLDEMOTE` to push the dirty line to L3, reducing the time until the reader's snoop finds it. Combined with `PREFETCHW` on the NEXT slot (which photon-ring already does), this creates a pipelined "publish and make visible" primitive.

**Atomicity guarantee:** NONE. `PREFETCHW` and `CLDEMOTE` are cache management hints. They affect WHERE a cache line lives in the hierarchy, not the ATOMICITY of operations on it. A reader can still observe a partially-written line, regardless of prefetch/demote state.

`CLDEMOTE` might reduce cross-core latency by 5-15ns (the line is already in L3 when the reader snoops, avoiding the full Modified-to-Shared transition from L1). But this is a latency optimization, not an atomicity fix.

**Rust soundness:** No impact on soundness. The data race remains.

**Latency impact:** Potentially useful as an OPTIMIZATION layered on top of whatever atomicity solution is chosen. `CLDEMOTE` could shave 5-15ns off cross-core latency by preemptively pushing the completed slot to the shared cache level. However, `CLDEMOTE` is a hint -- the processor may ignore it -- and it is available only on very recent hardware (Tremont E-cores, Alder Lake P-cores, Sapphire Rapids).

**Required hardware:** CLDEMOTE: Intel Tremont+, Alder Lake+, Sapphire Rapids+. PREFETCHW: already in use by photon-ring on Broadwell+.

**VERDICT: NOT VIABLE (for atomicity).** Useful latency optimization, orthogonal to the soundness problem. Could be combined with Mechanism #12 to reduce the cross-thread overhead of the atomic seqlock pattern.

---

## AArch64 Mechanisms

### 7. LDXP/STXP (ARMv8.0+)

**What it is:** Load-Exclusive Pair / Store-Exclusive Pair. `LDXP` atomically loads 128 bits (two 64-bit registers) from memory and marks the cache line as exclusive. `STXP` stores 128 bits conditionally -- succeeds only if no other core has written to the exclusive-marked line since the `LDXP`. This is ARM's equivalent of Load-Linked/Store-Conditional (LL/SC) for 128-bit values.

**How it would work for wider payloads:** Chain multiple LDXP/STXP pairs:
1. `LDXP` bytes 0-15, mark exclusive.
2. `LDXP` bytes 16-31... but this clears the exclusive monitor from step 1.

**Atomicity guarantee for single 128-bit:** Full. `LDXP`/`STXP` provides true 128-bit atomicity. The ARM Architecture Reference Manual (ARM DDI 0487, Section B2.9) guarantees that a successful `STXP` following an `LDXP` to the same address is atomic with respect to all observers.

**Chaining for wider payloads:** NOT possible. The ARM exclusive monitor tracks a SINGLE exclusive access at a time. A second `LDXP` clears the monitor set by the first `LDXP`. You CANNOT chain LDXP/STXP to get atomicity wider than 128 bits. The ARM ARM (Section B2.9.5) states: "An `LDXP` to a different address clears any existing exclusive monitor."

You could try:
1. LDXP 0-15
2. STXP 0-15 (to "commit" the first pair)
3. LDXP 16-31
4. STXP 16-31
But steps 2 and 4 are separate atomic operations -- the pair is NOT atomic. This is exactly the same multi-word CAS problem as x86 CMPXCHG16B chaining.

**Rust soundness:** For payloads up to 16 bytes, LDXP/STXP is formally sound (true 128-bit atomicity). For wider payloads, you need a seqlock or equivalent -- same problem as x86.

**Latency impact:** LDXP/STXP for 128-bit: comparable to `AtomicU128` load/store on ARM (compiles to LDXP/STXP via LLVM). No additional overhead vs the current approach for 16-byte payloads.

**Required hardware:** All ARMv8.0+ processors (essentially all AArch64).

**VERDICT: NOT VIABLE for payloads > 16 bytes.** Single LDXP/STXP is limited to 128 bits and cannot be chained. Same as `AtomicU128` on ARM. For wider payloads, combine with the seqlock-over-atomics pattern (Mechanism #12).

---

### 8. LSE2 (ARMv8.4+, FEAT_LSE2)

**What it is:** Large System Extensions v2, introduced in ARMv8.4. The key feature for us: single-copy atomicity for naturally-aligned 128-bit loads and stores using plain `LDP`/`STP` instructions (no need for LDXP/STXP). Before LSE2, an aligned `LDP` of two 64-bit registers was NOT guaranteed atomic at 128 bits -- it could tear between the two halves.

**Atomicity guarantee:** ARM Architecture Reference Manual (Section B2.2.1, ARMv8.4+): "A pair of memory accesses generated by an `LDP` or `STP` instruction to a naturally-aligned 128-bit address is single-copy atomic." This means `LDP x0, x1, [addr]` where `addr` is 16-byte aligned is a single 128-bit atomic load. No exclusive monitors, no CAS overhead.

**Does it extend to wider than 128 bits?** NO. LSE2 only guarantees atomicity for single `LDP`/`STP` pairs (128-bit). There is no `LDP` variant that loads 256 or 512 bits atomically. Multiple `LDP` instructions are independent and can be interleaved with writes from other cores.

**Rust soundness:** For 128-bit payloads, LSE2 makes `AtomicU128` load/store compile to simple `LDP`/`STP` (already happens in LLVM on ARMv8.4+ targets), which is formally sound and fast. For wider payloads, same situation as everything else.

**Latency impact:** `LDP`/`STP` is faster than LDXP/STXP for atomic 128-bit access (~4 cycles vs ~8-10 cycles). For 128-bit payloads on ARMv8.4+, this is strictly better.

**Required hardware:** ARMv8.4+. Apple M1+ (all Apple Silicon), Cortex-A77+, Neoverse N2+, Graviton 3+. NOT available on Cortex-A76 (Graviton 2) or earlier.

**VERDICT: NOT VIABLE for payloads > 16 bytes.** Excellent for 128-bit atomicity (faster than LDXP/STXP) but does not extend wider. Combine with Mechanism #12 for wider payloads.

---

### 9. SVE/SVE2 Scatter-Gather (ARMv8.2+)

**What it is:** ARM's Scalable Vector Extension. SVE registers are 128-2048 bits wide (implementation-defined, in multiples of 128). A single `LD1D` instruction can load up to 2048 bits (256 bytes) from contiguous memory into a Z register.

**Atomicity guarantee:** NONE. The ARM ARM (Section B2.2) does not extend single-copy atomicity guarantees to SVE load/store instructions. SVE loads decompose into element-wise accesses internally. Each element access (8/16/32/64-bit) has its own atomicity guarantee, but the aggregate load is NOT atomic.

The ARM ARM explicitly states (Section B2.2.2): "For SVE load and store instructions, each element access is single-copy atomic when the element size and alignment requirements are met. Multiple element accesses within a single SVE instruction are not guaranteed to be atomic with respect to each other."

So an SVE `LD1D` loading 512 bits (on a hardware with 512-bit SVE) decomposes into 8 x 64-bit element loads, each individually atomic, but the octet is not atomic as a whole. Torn reads are possible between elements.

Additionally, SVE register widths are implementation-defined and discovered at runtime. A portable SVE implementation cannot assume any specific width. Apple Silicon uses 128-bit SVE (NEON-equivalent). Graviton 3 uses 256-bit SVE. Neoverse V2 uses 128-bit. The width varies dramatically across implementations.

**Rust soundness:** No improvement. Individual element accesses are atomic but the aggregate is not.

**Latency impact:** Not applicable -- no atomicity benefit.

**VERDICT: NOT VIABLE.** SVE provides NO multi-element atomicity guarantee. Each element is atomic individually, but the whole vector load is not. Same torn-read problem.

---

### 10. TME (Transactional Memory Extension, FEAT_TME)

**What it is:** ARM's answer to Intel TSX. `TSTART` begins a transaction, `TCOMMIT` commits it, `TCANCEL` aborts it. All memory operations between TSTART and TCOMMIT are atomic with respect to all other observers. If a conflict is detected, the transaction aborts and control transfers to the address recorded by `TSTART`.

**How it would work:** Identical to the TSX approach (Mechanism #3) but on ARM:
- Writer: TSTART, write payload + stamp, TCOMMIT. Fallback on abort.
- Reader: TSTART, load stamp + payload + re-check stamp, TCOMMIT. Fallback on abort.

**Atomicity guarantee:** The ARM ARM (Section D1.4) guarantees that a committed transaction's memory operations are observed atomically by all other PEs (Processing Elements). This is the same strong guarantee as Intel RTM.

**The problem: NOBODY HAS IMPLEMENTED IT.**

TME was specified in the ARM Architecture Reference Manual as an optional extension (FEAT_TME) starting in ARMv9.0. As of March 2026:
- **Apple Silicon (M1-M5):** Does NOT implement FEAT_TME.
- **ARM Cortex-A (Cortex-A720, Cortex-A725, Cortex-X4, Cortex-X925):** Does NOT implement FEAT_TME.
- **ARM Neoverse (N2, N3, V2, V3):** Does NOT implement FEAT_TME.
- **AWS Graviton (2, 3, 4):** Does NOT implement FEAT_TME.
- **Qualcomm (Oryon, Nuvia):** Does NOT implement FEAT_TME.
- **Samsung (Exynos):** Does NOT implement FEAT_TME.
- **Fujitsu A64FX (HPC):** Does NOT implement FEAT_TME.

I am not aware of ANY shipping silicon that implements FEAT_TME. It exists only in the ARM specification as an optional feature that no vendor has chosen to implement. The likely reason: Intel's TSX experience showed that hardware transactional memory has severe practical problems (high abort rates under contention, security vulnerabilities like TAA/MDS, silicon area cost). ARM vendors appear to have concluded it is not worth the transistor budget.

Even Linux kernel support for TME is minimal -- there are patches for TSTART/TCOMMIT/TCANCEL detection but no real workload-level support, because no hardware exists to test on.

**Rust soundness:** Theoretically identical to TSX -- formally sound if the entire load/store is in `asm!`. But there is no hardware to run it on.

**Latency impact:** Unknown. No hardware exists to benchmark.

**VERDICT: NOT VIABLE (no hardware exists).** TME is a paper specification. No shipping processor implements it. Even if it were available, the Intel TSX experience suggests abort rates would make it unsuitable for a hot-path primitive. Do not design around TME.

---

## Cross-Platform Mechanisms

### 11. Padding to Power-of-Two + Multi-Atomic Pattern

**What it is:** Decompose a 64-byte payload into 4 x `AtomicU128` fields (or 8 x `AtomicU64` fields). Every access to every field is atomic. No data race exists by construction.

**How it would work (no seqlock, just ordering):**

Writer:
```rust
self.field_0.store(val.part0, Ordering::Relaxed);
self.field_1.store(val.part1, Ordering::Relaxed);
self.field_2.store(val.part2, Ordering::Relaxed);
self.field_3.store(val.part3, Ordering::Release); // last store is Release
```

Reader:
```rust
let part3 = self.field_3.load(Ordering::Acquire); // first load is Acquire
let part2 = self.field_2.load(Ordering::Relaxed);
let part1 = self.field_1.load(Ordering::Relaxed);
let part0 = self.field_0.load(Ordering::Relaxed);
```

**Consistency guarantee:** The Acquire-Release pair on field_3 creates a happens-before edge. If the reader's Acquire load of field_3 sees the value from the writer's Release store, then all preceding Relaxed stores (fields 0-2) are visible. But "visible" does not mean "consistent snapshot." It means the reader sees values AT LEAST AS NEW as the writer's stores. A concurrent SECOND write could update field_0 but not yet field_3, and the reader could see field_0 from write #2 but field_3 from write #1.

In a single-writer (SPMC) scenario: if there is only one writer, and the reader reads field_3 with Acquire, and then reads fields 0-2 with Relaxed, the reader might see:
- field_3 from write N
- field_2 from write N (happens-before guarantees it)
- field_1 from write N
- field_0 from write N (happens-before guarantees it)

Wait -- actually, in a single-writer scenario with Release on the last store and Acquire on the first load, happens-before DOES give you a consistent snapshot. If the reader sees write N's field_3, it sees ALL of write N's preceding stores (field_0, field_1, field_2). This is the definition of Release/Acquire: all stores before the Release are visible after the Acquire that synchronizes with it.

**BUT:** What if the writer is MID-WRITE when the reader loads? The reader might load:
- field_3 from write N (Acquire) -- writer has finished write N
- field_2 from write N+1 (writer started N+1, already updated field_2)
- field_1 from write N+1
- field_0 from write N+1

This is a torn read across two writes: field_3 is from write N, fields 0-2 are from write N+1.

The Acquire on field_3 only creates a happens-before with write N's Release. It says nothing about write N+1, which is concurrent.

**Conclusion:** Without a seqlock, multi-atomic loads do NOT provide a consistent snapshot in the presence of a concurrent writer, even with Release/Acquire ordering. You need the stamp check.

**VERDICT: NOT VIABLE on its own.** Multi-atomic accesses eliminate the data race per-field but do not provide cross-field consistency. Must be combined with a seqlock (Mechanism #12) for a consistent snapshot.

---

### 12. Atomic Seqlock: Sequence Number + Payload in Multiple Atomics

**What it is:** The exact same seqlock protocol photon-ring already uses, but with ALL accesses being atomic operations. The stamp is `AtomicU64` (already is). The payload is decomposed into multiple `AtomicU64` (or `AtomicU128`) fields. Every single memory access in the protocol is an atomic operation. The data race is eliminated by construction.

**Protocol:**

Writer (single-writer, `&mut self`):
```rust
// 1. Store odd stamp (writing)
slot.stamp.store(writing, Ordering::Relaxed);
fence(Ordering::Release);

// 2. Store payload fields (all atomic, Relaxed)
slot.data[0].store(val_part_0, Ordering::Relaxed);
slot.data[1].store(val_part_1, Ordering::Relaxed);
slot.data[2].store(val_part_2, Ordering::Relaxed);
slot.data[3].store(val_part_3, Ordering::Relaxed);
// ... up to data[6] for 56 bytes via AtomicU64, or data[3] for 64 bytes via AtomicU128

// 3. Store even stamp (done)
slot.stamp.store(done, Ordering::Release);
```

Reader:
```rust
// 1. Load stamp (Acquire)
let s1 = slot.stamp.load(Ordering::Acquire);
if s1 != expected || s1 & 1 != 0 { return None; }

// 2. Load payload fields (all atomic, Relaxed -- sequenced after Acquire)
let p0 = slot.data[0].load(Ordering::Relaxed);
let p1 = slot.data[1].load(Ordering::Relaxed);
let p2 = slot.data[2].load(Ordering::Relaxed);
let p3 = slot.data[3].load(Ordering::Relaxed);

// 3. Re-check stamp (Acquire acts as fence for the preceding loads)
let s2 = slot.stamp.load(Ordering::Acquire);
if s1 == s2 { return Some(reconstruct(p0, p1, p2, p3)); }
else { return None; } // torn -- retry
```

**Formal soundness analysis:**

Is there a data race? A data race requires two conflicting accesses to the same memory location where at least one is non-atomic. Here, EVERY access (stamp loads/stores, data loads/stores) is an atomic operation. Therefore, by definition, there is NO data race. The C++/Rust memory model explicitly states that conflicting atomic accesses are NOT data races (C++ [intro.races]/21, which Rust inherits).

Is the snapshot consistent? If `s1 == s2` (both equal to `seq * 2 + 2`):
- The writer's `stamp.store(done, Release)` at the end of write `seq` happens-before the reader's `stamp.load(Acquire)` that returns `done` (first stamp load, s1).
- By the Release-Acquire ordering: all stores before the writer's Release (the data stores) are visible to the reader after the Acquire.
- The reader then loads the data fields with Relaxed ordering. Since these are sequenced-after the Acquire, they see values at least as new as the writer's stores.
- The second stamp load `s2` with Acquire creates another synchronization point. If `s2 == s1`, no new write has started (no new odd stamp has been stored), meaning no data store from a newer write has occurred. Since the reader's loads are between two Acquire fences that both observed the SAME completed stamp, the data loads must reflect exactly the stores from that one write.

**This is formally sound.** No data races, consistent snapshots, same seqlock semantics. The proof follows directly from the C++/Rust memory model's definition of happens-before and release-acquire synchronization.

**Latency analysis (x86-64):**

On x86-64, `AtomicU64::load(Relaxed)` compiles to a plain `MOV` instruction (x86-64's Total Store Order makes all loads implicitly Acquire, so Relaxed loads have zero overhead vs non-atomic loads). `AtomicU64::store(Relaxed)` also compiles to a plain `MOV`. The only overhead vs the current approach:
- The compiler MAY NOT optimize across atomic accesses as aggressively. With `read_volatile`, the compiler is told "this memory might change, don't cache it" but CAN still reorder non-volatile operations around it. With atomic Relaxed, the compiler is told "this is an atomic operation" and must not introduce data races involving it, but Relaxed ordering allows the compiler to reorder atomics with respect to each other. In practice, on x86, the generated assembly is identical.

Let me be precise about what changes at the assembly level:

| Operation | Current (volatile) | Proposed (atomic) | x86-64 assembly |
|---|---|---|---|
| Stamp load (Acquire) | `AtomicU64::load(Acquire)` | `AtomicU64::load(Acquire)` | `mov rax, [stamp]` (same) |
| Data load (8 bytes) | `read_volatile` | `AtomicU64::load(Relaxed)` | `mov rax, [addr]` (same) |
| Data load (16 bytes) | `read_volatile` | `AtomicU128::load(Relaxed)` | `movdqa xmm0, [addr]` or `mov rax/rdx` (same) |
| Data store (8 bytes) | `write_volatile` | `AtomicU64::store(Relaxed)` | `mov [addr], rax` (same) |
| Data store (16 bytes) | `write_volatile` | `AtomicU128::store(Relaxed)` | `movdqa [addr], xmm0` or `mov` pair (same) |
| Stamp store (Release) | `AtomicU64::store(Release)` | `AtomicU64::store(Release)` | `mov [stamp], rax` (same, x86 stores are ordered) |

**On x86-64, the generated assembly is IDENTICAL.** The `volatile` and `Relaxed` atomic paths produce the same `MOV` instructions. The only difference is at the Rust abstract machine level: the atomic version is formally sound, the volatile version is formally UB.

**Latency analysis (AArch64):**

On AArch64, `Relaxed` atomic loads/stores compile to plain `LDR`/`STR` (same as volatile). `Acquire` loads compile to `LDAR` (same as current). `Release` stores compile to `STLR` (same as current). No overhead.

For `AtomicU128` loads on AArch64:
- Without LSE2: compiles to `LDXP` (load-exclusive pair), which has ~2-3 cycle overhead vs plain `LDP`.
- With LSE2 (ARMv8.4+): compiles to plain `LDP` with 128-bit atomicity guarantee. Zero overhead.

**For 8 x AtomicU64 (no AtomicU128 needed):**
Each 64-bit field is loaded/stored with `AtomicU64`. On all platforms, `AtomicU64::load(Relaxed)` compiles to the same instruction as `read_volatile` on a `u64`. Zero overhead on any architecture.

**Payload decomposition:**

A 56-byte payload (fits in one cache line alongside the 8-byte stamp):
```
Slot<T> layout (current):
  [0..7]   stamp: AtomicU64
  [8..63]  value: UnsafeCell<MaybeUninit<T>>  (56 bytes payload)

Slot layout (proposed):
  [0..7]   stamp: AtomicU64
  [8..15]  data_0: AtomicU64
  [16..23] data_1: AtomicU64
  [24..31] data_2: AtomicU64
  [32..39] data_3: AtomicU64
  [40..47] data_4: AtomicU64
  [48..55] data_5: AtomicU64
  [56..63] data_6: AtomicU64
```

7 x `AtomicU64` = 56 bytes of payload. On the read path: 7 Relaxed loads + 2 Acquire loads = 9 atomic operations. On x86: 9 `MOV` instructions. On ARM: 7 `LDR` + 2 `LDAR`. Same as what the compiler generates today for `read_volatile` on a 56-byte struct.

**For larger payloads (64-128 bytes, spanning 2 cache lines):**
Use `AtomicU128` to reduce the number of operations:
- 64 bytes: 4 x `AtomicU128` = 4 loads + 2 stamp loads = 6 operations.
- 128 bytes: 8 x `AtomicU128` = 8 loads + 2 stamp loads = 10 operations.

Or stay with `AtomicU64` for stable Rust compatibility:
- 64 bytes: 8 x `AtomicU64` = 8 loads + 2 stamp loads = 10 operations.

**API impact:**

The payload can no longer be a generic `T: Pod`. It must be decomposed into atomic fields. Two approaches:

**Approach A: Internal decomposition (transparent to user)**
```rust
#[repr(C, align(64))]
struct Slot<T: Pod> {
    stamp: AtomicU64,
    data: [AtomicU64; N],  // N = ceil(size_of::<T>() / 8)
}
```
Writer: `memcpy` T into a `[u64; N]` buffer, then store each element atomically.
Reader: Load each element atomically into a `[u64; N]` buffer, then `memcpy` into T.

The `memcpy` to/from the atomic array adds ~2-5ns for 56 bytes (cache-local copies). This is the main overhead vs the current approach, which does a single `write_volatile`/`read_volatile` of the entire struct.

**Approach B: Require T to be an array of AtomicU64 (user-facing change)**
This would break the existing `T: Pod` API. Not recommended unless the performance difference of Approach A is unacceptable.

**Approach A estimated overhead:**
- Write: marshal T into `[u64; 7]` (~2ns), store 7 x AtomicU64 Relaxed (~7 `MOV` = ~2ns), stamp stores (~1ns). Total: ~5ns vs current ~3ns. **~67% overhead on write.**
- Read: 7 x AtomicU64 Relaxed load (~2ns), stamp loads (~1ns), unmarshal to T (~2ns). Total: ~5ns vs current ~3ns locally. Cross-thread: still dominated by cache coherence (~48ns). **~4% overhead on cross-thread latency.**
- Torn-read retry: identical to current approach. No additional cost.

**Required hardware:** All platforms. AtomicU64 is universally available. AtomicU128 requires nightly Rust or inline asm.

**VERDICT: VIABLE. This is the answer.** The atomic seqlock eliminates ALL formal UB while producing IDENTICAL assembly on x86-64 (for the AtomicU64 variant) and near-identical assembly on AArch64. The overhead is ~2-4ns on the publish path (marshaling cost) and negligible on the cross-thread read path. The seqlock consistency protocol is unchanged. The `T: Pod` API can be preserved with an internal decomposition layer. This approach requires ZERO special hardware, works on stable Rust (with AtomicU64), and is formally sound under the Rust/C++ memory model.

---

## Summary Table

| # | Mechanism | Atomicity Width | True Atomicity? | Rust-Sound? | Latency vs Seqlock | Hardware Req | Verdict |
|---|---|---|---|---|---|---|---|
| 1 | MOVDIR64B | 64B write only | Yes (write) | No (no atomic read) | Worse (non-temporal) | Ice Lake+ | NOT VIABLE |
| 2 | AVX-512 VMOVDQA64 | 64B | No (no ISA guarantee) | No | Same (freq throttling) | Skylake-X+ | NOT VIABLE |
| 3 | Intel TSX (RTM) | Arbitrary | Yes | Yes (in asm!) | +5-7ns publish | Skylake-Ice Lake | NOT VIABLE (dead HW) |
| 4 | LOCK CMPXCHG16B chain | 16B per op | Per-op only | Yes (per-op) | 6x slower (LOCK contention) | All x86-64 | NOT VIABLE (alone) |
| 4b | AtomicU128 load/store | 16B per op | Yes | Yes | +10-15% cross-thread | All x86-64 + nightly | **VIABLE** (part of #12) |
| 5 | Cache line locking | 64B | No (not guaranteed) | No | Same | All x86-64 | NOT VIABLE |
| 6 | PREFETCHW + CLDEMOTE | N/A | N/A (hints only) | No impact | -5-15ns (optimization) | Broadwell+ / Tremont+ | OPTIMIZATION ONLY |
| 7 | LDXP/STXP chain | 16B per op | Per-op only | Yes (per-op) | Same | ARMv8.0+ | NOT VIABLE (alone) |
| 8 | LSE2 LDP/STP | 16B | Yes | Yes | Faster than LDXP | ARMv8.4+ | **VIABLE** (part of #12) |
| 9 | SVE scatter-gather | Per-element | No (per-element only) | No | N/A | ARMv8.2+ | NOT VIABLE |
| 10 | ARM TME | Arbitrary | Yes (spec) | Yes (in asm!) | Unknown | NO HARDWARE EXISTS | NOT VIABLE |
| 11 | Multi-atomic (no seqlock) | Per-field | Per-field | Yes (per-field) | Same | All platforms | NOT VIABLE (alone) |
| 12 | **Atomic seqlock** | Per-field + stamp | **Yes (composite)** | **YES** | **+2-4ns publish, ~0 cross-thread** | **All platforms, stable Rust** | **VIABLE -- THE ANSWER** |

---

## Recommendation

**Mechanism #12 (Atomic Seqlock) is the only viable path.** It:

1. **Eliminates all formal UB** -- every memory access is atomic, no data race exists.
2. **Produces identical assembly on x86-64** -- `AtomicU64::load/store(Relaxed)` compiles to the same `MOV` instructions as `read_volatile`/`write_volatile`.
3. **Works on all platforms** -- `AtomicU64` is universally available on all 64-bit targets.
4. **Works on stable Rust** -- no nightly features required (AtomicU64 is stable).
5. **Preserves the seqlock protocol** -- identical consistency semantics, identical retry logic.
6. **Preserves the T: Pod API** -- internal decomposition layer handles marshaling.
7. **Overhead is negligible on the read path** -- cross-thread latency unchanged (~48ns).
8. **Overhead is small on the write path** -- ~2-4ns for marshaling (67% regression on a 3ns baseline, but the absolute number is still sub-10ns).

The implementation path:
1. Change `Slot<T>` to store `[AtomicU64; N]` instead of `UnsafeCell<MaybeUninit<T>>`.
2. Writer marshals `T` into `[u64; N]` via `transmute_copy` (T: Pod guarantees this is safe), then stores each element with `AtomicU64::store(Relaxed)`.
3. Reader loads each element with `AtomicU64::load(Relaxed)` into `[u64; N]`, then unmarshals to `T` via `transmute_copy`.
4. Stamp protocol unchanged.
5. The `unsafe impl Sync for Slot<T>` safety comment can be updated to: "All payload accesses are atomic. No data race."

Optional enhancements:
- On nightly, use `AtomicU128` for 2x fewer operations on 16+ byte payloads.
- Layer `CLDEMOTE` (Mechanism #6) on top to reduce cross-core latency by 5-15ns on supported hardware.
- For payloads that fit in 16 bytes, skip the seqlock entirely and use a single `AtomicU128` (fastest possible path).

**Every other mechanism explored here either does not provide the necessary atomicity guarantee, requires dead/nonexistent hardware, or introduces unacceptable performance regression. The atomic seqlock is the only design that gives us formal soundness at near-zero cost.**

---

## Appendix: Why No Other Language Has Solved This Either

C++ has the same problem. `std::atomic` maxes out at the largest lock-free atomic the platform supports (typically 16 bytes). For wider payloads, C++ seqlocks use `std::atomic_thread_fence` + plain loads/stores, which is the same data race as Rust's approach. The C++ standard says this is UB too (conflicting non-atomic accesses without happens-before).

The Linux kernel "solves" this by defining its own memory model (LKMM) that IS aware of hardware semantics and explicitly allows torn reads of non-atomic memory when gated by a sequence counter. Rust has no equivalent escape hatch.

The `atomic_memcpy` RFC (rust-lang/rfcs#3301) would add `atomic_load_bytes`/`atomic_store_bytes` that perform element-wise atomic operations on a byte range. This would make the existing seqlock pattern formally sound without any code changes. Until it lands, Mechanism #12 (explicit decomposition into atomic fields) is the correct engineering solution.

---

## Implementation Status

Mechanism #12 (Atomic Seqlock) has been implemented as the `atomic-slots` feature.
Confirmed: zero performance regression on x86-64. See CHANGELOG.md.
