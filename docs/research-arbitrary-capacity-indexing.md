# Arbitrary-Capacity Ring Indexing: Eliminating the Power-of-Two Constraint

## Problem Statement

photon-ring requires ring capacity to be a power of two so it can compute slot
indices via `seq & mask` (bitwise AND with `capacity - 1`). This compiles to a
single-cycle AND instruction (~0.3ns). The alternative, `seq % capacity`
(modulo by a runtime variable), requires a 64-bit integer division (`DIV r64`)
which has 35-90 cycle latency on modern x86 (~9-24ns at 3.8 GHz), not the
3-5ns sometimes cited (that figure applies to 32-bit division or compiler-
optimized constant division).

The `& mask` pattern appears on **every** publish and **every** receive in all
four hot paths:

- `Publisher::publish_unchecked` (line 79): `self.slots_ptr.add((self.seq & self.mask) as usize)`
- `Subscriber::read_slot` (line 373): `self.slots_ptr.add((self.cursor & self.mask) as usize)`
- `Subscriber::recv` (line 52): `self.slots_ptr.add((self.cursor & self.mask) as usize)`
- `MpPublisher::publish` (line 73): `self.slots_ptr.add((seq & self.mask) as usize)`

Plus the prefetch path: `(self.seq + 1) & self.mask`

That is 2 AND operations per publish, 1 per receive. At photon-ring's measured
~40-55ns one-way latency, replacing a 0.3ns AND with a 9-24ns DIV would be
a **16-43% regression** in one-way latency. This is catastrophic.

### Banned Solutions

1. **Bare modulo** (`seq % capacity` with runtime `capacity`) -- 35-90 cycle DIV. BANNED.
2. **Round up to next power of two** -- wastes up to 50% memory. BANNED as primary.
3. **"Users can deal with power-of-two"** -- status quo. BANNED.

---

## Technique 1: Reciprocal Multiplication (Fixed-Point Division)

### Mechanism

When the compiler sees `x % C` where `C` is a compile-time constant, it
replaces the division with a multiply-high + shift sequence. We can do the same
at runtime by precomputing the magic constants at ring construction time.

For a divisor `d`, compute:
```
m = ceil(2^(64+s) / d)    // magic multiplier
s = ceil(log2(d))          // shift amount
```

Then: `x / d = mulhi(x, m) >> s`
And:  `x % d = x - (x / d) * d`

On x86-64, `MULQ` (64x64->128 multiply) has 3-cycle latency on modern Intel
(since Skylake). The full sequence is:

```asm
mov    rax, m        ; load magic constant (from struct field, L1 hit)
mul    rdi           ; 64x64->128, result high in rdx (3 cycles)
shr    rdx, s        ; shift (1 cycle)
; rdx now holds x / d
; for modulo: imul rdx, d; sub rdi, rdx  (3+1 more cycles)
```

### For photon-ring specifically

We don't actually need the modulo -- we need the *index*. The index IS
`seq % capacity`. But we can restructure: since `seq` is monotonically
increasing and we only need `seq mod capacity`, we could instead:

**Option A: Full reciprocal modulo.**
Precompute `(magic, shift)` at construction. Store alongside `mask` in
Publisher/Subscriber. Hot path becomes:
```rust
let idx = fastmod(self.seq, self.magic, self.shift, self.capacity);
```
where `fastmod` is ~8 cycles (MUL + SHR + IMUL + SUB).

**Option B: Lemire's fastmod (2019).**
Daniel Lemire published a faster variant using 128-bit multiplication:
```
fastmod(a, d) = mulhi(lowbits(a * M), d)
```
where `M = ceil(2^64 / d)`. This requires only:
1. `MUL` (3 cycles) to get `a * M`
2. Take low 64 bits
3. `MUL` again with `d` (3 cycles)
4. Take high 64 bits

Total: ~7 cycles for the modulo. No shift, no subtract.

### Latency Estimate

| Variant | Cycles | ns @ 4.7 GHz | Overhead vs AND |
|---------|--------|--------------|-----------------|
| AND (status quo) | 1 | 0.21 | baseline |
| Classic reciprocal (mul+shr+imul+sub) | ~8 | 1.70 | +1.49ns |
| Lemire fastmod (2x mul + extract) | ~7 | 1.49 | +1.28ns |

Per publish (2 indexing ops): +2.56 to +2.98ns overhead.
Per receive (1 indexing op): +1.28 to +1.49ns overhead.

### Implementation Complexity

Low-medium. Add two u64 fields (`magic`, `shift` or just `magic` for Lemire)
to `Publisher`, `Subscriber`, `MpPublisher`, and `SubscriberGroup`. Compute at
construction in `SharedRing::new()`. Replace `& self.mask` with an `#[inline]`
`fastmod()` call. Approximately 50-80 lines of new code.

### Tradeoffs

- (+) Arbitrary capacity. Zero wasted memory.
- (+) Still sub-2ns per indexing operation. Well within the sub-1ns-per-op
  target if we count the total publish overhead (the AND itself is negligible
  next to the stamp store + cursor store which dominate).
- (-) Not sub-1ns *per indexing operation* -- it's ~1.3-1.7ns per operation.
  But the AND was 0.21ns, so the delta is ~1.1-1.5ns on the publish hot path.
- (-) Two extra fields per Publisher/Subscriber (16 bytes). Negligible.
- (-) The `MUL` instruction may compete for the integer multiplier with other
  hot-path arithmetic (the `seq * 2 + 1` stamp computation). On Intel since
  Skylake, there is 1 multiplier port, so back-to-back MULs may pipeline but
  not execute in parallel.

### Verdict: VIABLE -- best general-purpose solution

This is what compilers already do. The 1.3-1.7ns overhead per operation is
acceptable when the total publish path is ~40-55ns. It's a ~3% regression on
end-to-end latency, not the 16-43% regression that raw `DIV` would cause.

Lemire's fastmod is the preferred variant (fewer instructions, no branch).

---

## Technique 2: Barrett Reduction

### Mechanism

Barrett reduction is used in cryptography to compute `a mod n` without
division, given a precomputed approximation of `1/n`. The algorithm:

```
q = floor(a * m / 2^k)     // m = floor(2^k / n), k chosen for precision
r = a - q * n
if r >= n: r -= n           // at most one correction
```

This is functionally identical to reciprocal multiplication (Technique 1).
The "Barrett" name comes from its application in modular arithmetic for
multi-precision integers in RSA/ECC, but for single-precision 64-bit values,
it reduces to the same multiply-shift-subtract sequence.

### Latency Estimate

Same as Technique 1: ~7-8 cycles. The correction branch (`if r >= n`) adds a
conditional move (CMOV, 1 cycle) or a branch that is almost always not-taken
(free with branch prediction).

### Tradeoffs

Identical to reciprocal multiplication. Barrett reduction IS reciprocal
multiplication for single-precision integers. The terminology difference is
historical (Barrett's 1986 paper targeted multi-word integers).

### Verdict: IDENTICAL TO TECHNIQUE 1

Not a separate technique. Use Lemire's fastmod (which is the modern refinement
of this family of algorithms).

---

## Technique 3: Compile-Time Capacity (Const Generic)

### Mechanism

Make the ring capacity a const generic parameter:

```rust
struct SharedRing<T, const N: usize> { ... }
```

When `N` is known at compile time, the compiler can:
- If `N` is a power of two: emit `AND` (1 cycle).
- If `N` is not a power of two: emit the reciprocal-multiplication sequence
  (~4-5 cycles, tighter than runtime because the multiplier is an immediate).

The compiler does this automatically for `seq % N` when `N` is a const generic.

### Latency Estimate

| N | Compiled to | Cycles | ns @ 4.7 GHz |
|---|-------------|--------|--------------|
| Power of two | `AND mask` | 1 | 0.21 |
| Non-power-of-two | `IMUL + SHR` (constant folded) | ~4 | 0.85 |

The non-power-of-two case is **faster than runtime reciprocal** because the
compiler can embed the magic multiplier as an immediate operand, avoiding a
memory load. This saves 1 cache access (~4 cycles L1 hit) that the runtime
version pays.

### Implementation Complexity

**Very high. This is the problem.**

Const generics infect every type in the API:

```rust
struct Publisher<T: Pod, const N: usize> { ... }
struct Subscriber<T: Pod, const N: usize> { ... }
struct Subscribable<T: Pod, const N: usize> { ... }
struct MpPublisher<T: Pod, const N: usize> { ... }
struct SubscriberGroup<T: Pod, const N: usize> { ... }
struct SharedRing<T, const N: usize> { ... }
struct Photon<T: Pod, const N: usize> { ... }
struct DependencyBarrier<const N: usize> { ... } // or not, since it's capacity-agnostic
```

Every user of the library must carry the const generic everywhere:
```rust
fn process<T: Pod, const N: usize>(sub: &mut Subscriber<T, N>) { ... }
```

This also means:
- A `Subscriber<u64, 1024>` and `Subscriber<u64, 2048>` are **different types**.
  They cannot be stored in the same Vec, passed to the same function without
  generics, or used with `dyn Trait` dispatch.
- The `Photon` bus cannot mix topics with different capacities (or must be
  generic over capacity, losing the ability to have per-topic sizing).
- Binary bloat from monomorphization (every capacity value generates a separate
  copy of every function).
- Cannot change capacity at runtime (e.g., based on config file input).

### Tradeoffs

- (+) Optimal codegen: 0.85ns for non-power-of-two, 0.21ns for power-of-two.
  The ONLY technique that gets close to sub-1ns for arbitrary capacity.
- (-) API ergonomics catastrophe. The const generic propagates everywhere.
- (-) No runtime capacity. Users who read capacity from config are blocked.
- (-) Type incompatibility between different capacities.
- (-) Monomorphization bloat.

### Possible Mitigation: Hybrid Approach

Offer const-generic types as an opt-in "turbo mode" alongside the existing
runtime-capacity types:

```rust
// Runtime capacity (status quo API, uses reciprocal multiplication)
let (pub_, subs) = photon_ring::channel::<u64>(1000);

// Compile-time capacity (new, optimal codegen)
let (pub_, subs) = photon_ring::channel_static::<u64, 1000>();
```

This doubles the API surface but preserves ergonomics for the common case while
giving HFT users the absolute best codegen.

### Verdict: VIABLE only as opt-in secondary API

The const-generic approach has the best possible codegen but destroys API
ergonomics. Not viable as the primary API. Viable as a `channel_static::<T, N>()`
opt-in for users who need the last 0.5ns.

---

## Technique 4: Double-Mapping (Virtual Memory Trick)

### Mechanism

Map the ring buffer's physical memory twice in contiguous virtual address space:

```
Virtual address space:
[  Ring (N slots)  ][  Ring (N slots)  ]
 ^                   ^
 Same physical pages backing both regions
```

With this layout, any pointer access `base + offset` where
`0 <= offset < 2*N` automatically wraps via the virtual memory mapping.
You never need to compute `seq % N` or `seq & mask` -- you just use
`base + (seq % something_big)` and the MMU handles the wrap.

**But wait** -- you still need to convert `seq` to a byte offset. The trick
is that you can use a mask that is larger than the ring. If the ring has `N`
slots and you double-map, any access to `base + (seq & (2*N - 1)) * slot_size`
will land on the correct physical slot... but only if `2*N` is a power of two.
This just pushes the power-of-two requirement to `2*N`.

Actually, the real mechanism is different. In classic C ring buffer
implementations (like the Linux kernel's `kfifo`), double-mapping is used for
*contiguous reads* across the wrap boundary, not for index computation. The
typical use case:

```c
// Without double-mapping: need to handle wrap
memcpy(dst, ring + (tail % N), min(len, N - tail % N));
memcpy(dst + first_part, ring, remaining);

// With double-mapping: single memcpy (wraps via MMU)
memcpy(dst, ring + (tail % N), len);
```

This eliminates the **branching** around the wrap point for bulk copies, but
it does NOT eliminate the modulo/AND for index computation. You still need to
convert `seq` to an offset.

### For photon-ring specifically

photon-ring doesn't do bulk copies across the wrap boundary. Each publish and
receive operates on a single `Slot<T>`. The access pattern is:

```rust
let slot = &*self.slots_ptr.add((self.seq & self.mask) as usize);
```

With double-mapping, this becomes:

```rust
// If total_mapped = 2 * N slots, and N is arbitrary:
let slot = &*self.slots_ptr.add((self.seq % N) as usize);
// Still need modulo!
```

The double-mapping doesn't help because we're indexing individual slots,
not doing contiguous copies that cross the wrap point.

**Alternative interpretation**: use the double-mapping to make the mask
work for non-power-of-two sizes. If we map `N` physical pages but
`round_up_to_pow2(N)` virtual pages (with the extra virtual pages mapping
back to the start of the physical region), then `seq & (pow2 - 1)` always
gives a valid virtual address, even if the ring has `N < pow2` physical
slots. But this means we're back to the "round up to power of two"
approach -- we're just doing the rounding in virtual address space instead
of physical memory. We DO save physical memory (only `N` pages allocated),
but virtual address space is cheap, so this is a minor win.

### Latency Estimate

If we use the virtual-rounding trick: identical to the AND (0.21ns),
because the indexing is still `& mask`. Physical memory usage is optimal
(only `N` slots). Virtual address usage: up to `2*N` slots.

If we try to use it to avoid indexing entirely: not possible for
single-element access patterns.

### Implementation Complexity

**Very high for Rust, moderate for C.**

- Requires `mmap` with `MAP_FIXED` to place two mappings at known addresses.
  This is inherently unsafe and platform-specific.
- The current `SharedRing` allocates slots via `Vec<Slot<T>>` (heap). The
  double-mapping requires `mmap` (or `VirtualAlloc` on Windows, `vm_remap`
  on macOS). This replaces the allocator entirely.
- `mmap` has alignment constraints (page-aligned). Slots are 64 bytes;
  pages are 4096 bytes. This means capacity must be a multiple of
  `4096 / sizeof(Slot<T>)` = 64 (for 64-byte slots) to align with page
  boundaries.
- On Linux, the trick uses `memfd_create` + double `mmap`:
  ```c
  fd = memfd_create("ring", 0);
  ftruncate(fd, N * slot_size);
  base = mmap(NULL, 2 * pow2_slots * slot_size, NONE, PRIVATE|ANON, -1, 0);
  mmap(base, N * slot_size, RW, FIXED|SHARED, fd, 0);
  mmap(base + pow2_slots * slot_size, N * slot_size, FIXED|SHARED, fd, 0);
  ```
- Not portable: different syscalls on Linux vs macOS vs Windows.
- Breaks `no_std`. photon-ring is currently `#![no_std]` (with `alloc`).
  This would require `std` or raw syscall wrappers.
- TLB pressure: the double-mapping uses twice the virtual address space,
  which may cause additional TLB misses for large rings.

### Tradeoffs

- (+) AND-speed indexing (0.21ns) with non-power-of-two physical capacity.
- (+) Zero wasted physical memory.
- (-) Wasted virtual address space (up to 2x).
- (-) Platform-specific, breaks `no_std`, massively increases unsafe surface.
- (-) Capacity must be a multiple of `page_size / slot_size`.
- (-) Adds TLB pressure. For a ring that fits in one huge page (2 MiB),
  the double-map needs 2 huge-page TLB entries instead of 1.
- (-) Interacts poorly with the existing `hugepages` feature (which uses
  `MAP_HUGETLB`). Would need careful integration.
- (-) Does not actually eliminate the index computation for single-slot
  access patterns (photon-ring's use case). Only helps with the
  "round up physical memory" waste.

### Verdict: NOT VIABLE as a general solution

The complexity is enormous and the benefit is marginal. The only real
advantage over "round up to power of two" is saving physical memory for
non-power-of-two capacities. But the user asked for 1000 slots and gets
1000 physical slots + 24 virtual-only shadow slots. They could just as
easily accept 1024 slots. The implementation cost (platform-specific mmap,
breaking no_std, TLB pressure) far exceeds the benefit.

If photon-ring ever needs this for a different reason (e.g., zero-copy
bulk reads across the wrap point), it would be worth revisiting. But for
the index-computation problem alone, it is overkill.

---

## Technique 5: Lookup Table (LUT)

### Mechanism

Precompute an array mapping sequence numbers to slot indices:

```rust
let lut: [u16; LUT_SIZE] = core::array::from_fn(|i| (i % capacity) as u16);
```

Hot path: `let idx = lut[seq & (LUT_SIZE - 1)] as usize;`

For this to work, `LUT_SIZE` must be a power of two (so we can use AND to
index the LUT) AND a multiple of `capacity` (so the modulo pattern repeats
correctly). The smallest such value is `lcm(capacity, next_pow2(capacity))`.

### Analysis

If `capacity` = 1000:
- `next_pow2(1000)` = 1024
- `lcm(1000, 1024)` = 128000
- LUT size: 128000 entries * 2 bytes = 256 KB

This is absurd. The LUT is larger than most L1 caches (32-48 KB), so
accessing it would cause L1 misses on every access, adding ~4ns per
operation (L2 latency) or worse.

**Alternative: small LUT with capacity as a power of two of the LUT.**

This doesn't make mathematical sense. The LUT approach fundamentally
requires the LUT to cover one full period of the modulo pattern, which is
`lcm(capacity, lut_size_pow2)`.

**Another alternative: stride table.**

For capacity `N`, store `N` entries: `table[i] = i`. Then:
```rust
// Maintain a position counter that wraps
self.pos += 1;
if self.pos >= N { self.pos = 0; }
let idx = self.pos;
```

This replaces the modulo with a comparison + conditional reset. On x86:
```asm
inc     rax          ; 1 cycle
cmp     rax, rcx     ; 1 cycle (fused with jge)
jge     .wrap        ; branch, predicted taken ~once per N iterations
; ...
.wrap:
xor     eax, eax     ; 1 cycle (only taken once per ring revolution)
```

This is 2 cycles in the common case (increment + compare), 3 cycles on
wrap. With branch prediction, the wrap branch is predicted correctly
(N-1)/N of the time.

**But wait** -- photon-ring doesn't use a position counter. It uses a
monotonically increasing `seq` (or `cursor`) and computes the index from
that. The seq/cursor is also used for the seqlock stamp computation
(`seq * 2 + 1`, `seq * 2 + 2`). Switching to a wrapping position counter
would require either:
1. Maintaining both `seq` (for stamps) and `pos` (for indexing), doubling
   the bookkeeping.
2. Rederiving `seq` from `pos`, which requires tracking wrap count.

Option 1 is actually reasonable and very cheap: add a `pos: u64` field
that wraps modulo capacity. Increment it alongside `seq`. The comparison +
conditional is 2 cycles (~0.42ns), well within the sub-1ns target.

### Latency Estimate

| Approach | Cycles | ns @ 4.7 GHz |
|----------|--------|--------------|
| LUT (large) | ~12 (L2 miss) | 2.55 |
| Wrapping counter (cmp+jge) | 2 | 0.42 |

### Implementation Complexity

**Wrapping counter approach**: Very low. Add a `pos: u64` field to
Publisher, Subscriber, MpPublisher, SubscriberGroup. Increment and wrap
on each operation. ~20 lines of code.

**Complications**:
- `Subscriber::latest()` jumps the cursor to `head`. The wrapping counter
  must also be set: `self.pos = head % capacity`. This reintroduces a
  modulo on the cold path (acceptable -- it's a rare operation).
- `Subscriber::read_slot` lag recovery: jumps cursor to `oldest`. Same
  issue. Cold path, acceptable.
- `MpPublisher::publish`: `seq` comes from `fetch_add`, not from a local
  counter. There is no local `pos` to maintain -- each publish gets a
  different `seq`. The wrapping counter approach **does not work** for
  MPMC publishers, because there's no local monotonic counter.

**This is a critical limitation.** The wrapping counter works for SPMC
(where the publisher has a local `self.seq` and each subscriber has a local
`self.cursor`), but not for MPMC (where sequences are claimed atomically
from a shared counter).

For MPMC, we'd need to fall back to reciprocal multiplication.

### Verdict: VIABLE for SPMC only, as a micro-optimization

The wrapping counter approach achieves ~0.42ns per indexing operation,
within the sub-1ns target. But it only works for SPMC. MPMC requires
reciprocal multiplication. The added complexity of two different indexing
strategies (wrapping counter for SPMC, reciprocal for MPMC) may not be
worth the ~0.88ns savings over reciprocal multiplication alone.

---

## Technique 6: Other Approaches

### 6a: Conditional Subtract (Modular Counter)

Instead of `seq % N`, maintain a counter and use:
```rust
self.pos += 1;
self.pos -= N * (self.pos >= N) as u64;  // branchless
```

On x86 this compiles to:
```asm
inc     rax
cmp     rax, rcx
sbb     rdx, rdx       ; rdx = -1 if pos >= N, else 0 (1 cycle)
and     rdx, rcx        ; rdx = N if pos >= N, else 0 (1 cycle)
sub     rax, rdx        ; pos -= N or 0 (1 cycle)
```

Total: 4 cycles (0.85ns @ 4.7 GHz). Branchless, no branch prediction
dependency. Same limitation as Technique 5 (wrapping counter): does not
work for MPMC.

### 6b: Masking with Correction

For capacity `N`, find the smallest `mask >= N - 1` that is `(power of 2) - 1`.
Compute `idx = seq & mask`, then correct:
```rust
let idx = seq & mask;
let idx = if idx >= capacity { idx - capacity } else { idx };
```

This is branchless via CMOV:
```asm
and     rax, mask
mov     rdx, rax
sub     rdx, capacity
cmovge  rax, rdx
```

But **this does not produce correct modular arithmetic** for all seq values.
For example, with capacity=3 and mask=3 (binary 11):
- seq=0: `0 & 3 = 0`, correct (0 % 3 = 0)
- seq=1: `1 & 3 = 1`, correct
- seq=2: `2 & 3 = 2`, correct
- seq=3: `3 & 3 = 3`, 3 >= 3, so 3-3=0, correct
- seq=4: `4 & 3 = 0`, should be 1. **WRONG.**

This approach only works when `capacity > mask/2`, i.e., when the capacity
is more than half the next power of two. For capacity=1000 with mask=1023,
it works for `seq & 1023 < 2000`, but `seq & 1023` ranges 0-1023, and
values 1000-1023 would be corrected to 0-23. Let me re-check:

- seq=1000: `1000 & 1023 = 1000`, 1000 >= 1000, so 1000-1000=0. Correct (1000 % 1000 = 0).
- seq=1024: `1024 & 1023 = 0`, should be 24. **WRONG.**

No, this doesn't work. The AND operation loses information about how many
full cycles of `capacity` have passed. You can't recover that from just
`seq & mask`.

### Verdict on 6b: NOT VIABLE. Mathematically incorrect.

### 6c: Fixed-Point Fractional Counter

Instead of tracking `seq` and computing `seq % N`, track a fixed-point
accumulator that holds the fractional position within the ring:

```rust
// At construction: reciprocal = (1 << 32) / capacity (fixed-point)
// On each publish:
self.frac += self.reciprocal;  // fixed-point addition
let idx = (self.frac >> 32) ...
```

This is getting convoluted and doesn't straightforwardly give modular
indexing. Abandoned.

### 6d: Sequence Number Quantization

Restrict sequence numbers to multiples of capacity, mapping each
publication to a slot directly:

This destroys the seqlock stamp computation which relies on `seq * 2 + 1`
and `seq * 2 + 2` encoding. Not viable without redesigning the entire
seqlock protocol.

### 6e: SIMD-Accelerated Modulo

Use SIMD to compute multiple modulo operations in parallel. Not applicable
-- we compute one index per publish/receive, so there's no parallelism to
exploit.

---

## Comparative Summary

| Technique | Latency/op | Works for MPMC? | Complexity | Memory waste | `no_std`? |
|-----------|-----------|-----------------|------------|-------------|-----------|
| AND (status quo) | 0.21ns | Yes | None | Up to 100% | Yes |
| Reciprocal multiplication | 1.3-1.7ns | Yes | Low-Med | 0% | Yes |
| Barrett reduction | Same as above | Yes | Same | 0% | Yes |
| Const generic | 0.21-0.85ns | Yes | Very High | 0% | Yes |
| Double-mapping (VM) | 0.21ns | Yes | Extreme | 0% phys, 2x virt | **No** |
| Wrapping counter | 0.42ns | **No** | Low | 0% | Yes |
| Conditional subtract | 0.85ns | **No** | Low | 0% | Yes |
| Masking + correction | N/A | -- | -- | -- | -- |

---

## Recommendation

### Primary: Reciprocal Multiplication (Lemire fastmod)

This is the correct answer. It is:
- **Universal**: works for all capacity values, for SPMC and MPMC.
- **Fast enough**: ~1.5ns per operation. On a 40-55ns end-to-end latency
  path, 3ns total overhead (2 ops per publish) is a ~5-7% regression.
  Significant but not catastrophic.
- **Simple**: ~60 lines of code. Two new fields per Publisher/Subscriber.
  No API changes. No platform-specific code. Preserves `no_std`.
- **Well-understood**: this is exactly what compilers do. The algorithm is
  proven correct for all 64-bit inputs. Lemire's paper provides a
  correctness proof.

Implementation plan:
1. Add `struct FastMod { multiplier: u64, capacity: u64 }` (or inline
   the two fields).
2. At construction, compute `multiplier = u128::MAX / capacity as u128 + 1`.
3. Replace `seq & self.mask` with `fastmod(seq, self.multiplier, self.capacity)`.
4. Keep the `is_power_of_two()` assertion as a runtime check on a
   `channel()` constructor, and add a new `channel_arbitrary()` (or remove
   the assertion entirely and use fastmod always -- when capacity IS a
   power of two, the fastmod is still correct, just slower).

**Actually, the cleanest approach**: always use fastmod internally, but
detect power-of-two capacity at construction and store a flag. On the hot
path, branch on the flag:
```rust
if self.is_pow2 { seq & self.mask } else { fastmod(seq, ...) }
```
This branch is perfectly predicted (always the same direction) and costs
0 cycles after warmup. Power-of-two users get zero regression. Non-power-
of-two users get ~1.5ns/op overhead.

Even simpler: don't branch. Just always use fastmod. When capacity is a
power of two, fastmod still produces the correct result. The 1.3ns
regression for power-of-two users who previously got 0.21ns might be
unwelcome, so the branching approach is safer.

### Secondary (opt-in): Const Generic API

For users who want the absolute minimum indexing overhead and can tolerate
the ergonomic cost:

```rust
let (pub_, subs) = photon_ring::channel_static::<u64, 1000>();
```

This gives 0.85ns for non-power-of-two and 0.21ns for power-of-two, at
the cost of const-generic type propagation. Offer as a separate API
surface, not as a replacement for the runtime API.

### Not recommended

- Double-mapping: too complex, breaks `no_std`, marginal benefit.
- Wrapping counter: doesn't work for MPMC, marginal improvement over
  reciprocal multiplication.
- LUT: cache pollution, mathematically requires huge tables.

---

## Appendix: Lemire's fastmod Algorithm

Reference: Daniel Lemire, "Faster Remainder by Direct Computation" (2019),
https://arxiv.org/abs/1902.01961

For a 64-bit dividend `a` and divisor `d`:

```rust
/// Compute the multiplier M = ceil(2^64 / d).
/// For d > 1, this equals (u64::MAX / d) + 1.
#[inline]
const fn compute_multiplier(d: u64) -> u64 {
    // Equivalent to ceil(2^64 / d) for d >= 2.
    // Using 128-bit arithmetic to avoid overflow.
    ((1u128 << 64) / d as u128 + 1) as u64  // Note: for d=1, M=0 (wraps), but d=1 is degenerate
}

/// Fast modulo: a % d, given precomputed M = compute_multiplier(d).
#[inline]
fn fastmod(a: u64, m: u64, d: u64) -> u64 {
    // lowbits = a * M (mod 2^64)  -- take the low 64 bits of 128-bit product
    let lowbits = (a as u128).wrapping_mul(m as u128) as u64;
    // result = mulhi(lowbits, d)  -- high 64 bits of lowbits * d
    ((lowbits as u128 * d as u128) >> 64) as u64
}
```

On x86-64, `fastmod` compiles to:
```asm
mov     rax, rdi        ; a
mul     rsi             ; a * M -> RDX:RAX, we want RAX (low bits)
mov     rax, rax        ; (nop, low bits already in RAX)
mul     rcx             ; lowbits * d -> RDX:RAX, we want RDX (high bits)
; RDX = a % d
```

Two `MUL` instructions = 2 * 3 cycles = 6 cycles. Plus register moves,
~7-8 cycles total. At 4.7 GHz: ~1.5ns.

This is strictly faster than any form of integer division and correct for
all `a < 2^64` and `d >= 1`.
