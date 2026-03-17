<!--
  Copyright 2026 Photon Ring Contributors
  SPDX-License-Identifier: Apache-2.0
-->

# Formal Verification: Seqlock-Stamped Ring Buffer

This directory contains a TLA+ specification of the Photon Ring seqlock
protocol and a model-checking configuration for the TLC model checker.

## Files

| File | Purpose |
|---|---|
| `seqlock.tla` | TLA+ specification of the SPMC seqlock ring protocol |
| `MC.tla` | Model-checking module (binds constants for TLC) |
| `MC.cfg` | TLC configuration: constants, invariants, liveness properties |

## What is verified

**Safety (invariants)**

- `NoTornRead` -- A reader never commits a value that was partially written.
  If `readResult[r]` is set, it equals the sequence the reader consumed.
- `ResultIsValid` -- Every committed result is a valid sequence number
  (not a stale or garbage value).

**Liveness (temporal properties)**

- `ReaderProgress` -- If the writer publishes at least one message,
  every reader eventually reads a value (no starvation under fair
  scheduling).

**Lag correctness (structural)**

- The `ReaderStampMismatch` action detects when a slot has been
  overwritten (stamp > expected) and advances the reader cursor to the
  oldest available message, matching the `TryRecvError::Lagged` path in
  `src/channel.rs`.

## Prerequisites

Install the TLA+ tools. The easiest method is the VS Code extension:

1. Install [VS Code](https://code.visualstudio.com/).
2. Install the [TLA+ extension](https://marketplace.visualstudio.com/items?itemName=alygin.vscode-tlaplus).
3. The extension bundles TLC and TLAPS.

Alternatively, download the standalone TLA+ toolbox from
<https://github.com/tlaplus/tlaplus/releases> or install via:

```bash
# With Homebrew (macOS)
brew install tlaplus

# With SDKMAN (any platform with JVM)
sdk install tlaplus
```

TLC requires Java 11 or later.

## Running TLC

### From the command line

```bash
cd verification/

# Default configuration (2 readers, ring=2, MaxSeq=4)
# Typical runtime: < 30 seconds
java -jar /path/to/tla2tools.jar MC -config MC.cfg -workers auto

# Or if tla2tools is on PATH:
tlc MC -config MC.cfg -workers auto
```

The `-workers auto` flag uses all available CPU cores for parallel
state-space exploration.

### From VS Code

1. Open `MC.tla` in VS Code.
2. Press `Ctrl+Shift+P` and select **TLA+: Check Model with TLC**.
3. Results appear in the TLA+ output panel.

### Larger configurations

Edit `MC.cfg` to increase the parameters:

```
CONSTANTS
    NumReaders = 3
    RingSize = 4
    MaxSeq = 8
```

This explores more interleavings but takes longer (minutes to hours
depending on hardware). The state space grows exponentially with
`MaxSeq` and `NumReaders`.

## Interpreting results

A successful run prints:

```
Model checking completed. No error has been found.
  Finished in ...
  N states generated, M distinct states found, ...
```

If a property is violated, TLC prints a counterexample trace showing
the exact sequence of steps that leads to the violation. Each step
shows the action taken (e.g., `WriterBegin`, `ReaderLoadValue`) and the
values of all variables.

## Correspondence to Rust implementation

| TLA+ action | Rust code |
|---|---|
| `WriterBegin` | `Slot::write` -- `stamp.store(writing, Relaxed)` + release fence |
| `WriterData` | `Slot::write` -- `ptr::write(value)` |
| `WriterFinish` | `Slot::write` -- `stamp.store(done, Release)` + cursor store |
| `ReaderLoadStamp1` | `Slot::try_read` -- `stamp.load(Acquire)` |
| `ReaderLoadValue` | `Slot::try_read` -- `ptr::read(value)` |
| `ReaderVerify` | `Slot::try_read` -- second `stamp.load(Acquire)`, compare |
| `ReaderRetryOdd` | `Slot::try_read` -- `s1 & 1 != 0` branch |
| `ReaderStampMismatch` | `Subscriber::read_slot` -- lag detection slow path |

## Limitations

### SPMC only -- MPMC is not modeled

The TLA+ specification covers the single-producer, multi-consumer (SPMC)
protocol exclusively. The multi-producer path (`MpPublisher`) uses a
fundamentally different cursor advancement protocol based on
`compare_exchange` (CAS) for sequence claiming and stamp-based spin-waiting
for slot availability. None of this is present in the model. Bugs in the
MPMC CAS retry loop, ABA hazards, or producer-to-producer ordering would
not be caught by this specification.

### Sequential consistency, not Acquire/Release

TLA+ models execute under sequential consistency (SC) by default. The
Rust implementation uses weaker `Acquire`/`Release` orderings (and
`Relaxed` in some non-critical paths). Under SC, all memory operations
are globally ordered; under Acquire/Release, independent stores on
different threads may be observed in different orders by different
readers.

This means the model is **strictly stronger** than the implementation:
any safety violation found under SC would also exist under weaker
orderings. However, the converse does not hold -- the model **cannot
detect bugs that only manifest under weak memory**, such as:

- A missing `Release` fence allowing a reader to observe a partially
  written slot despite a valid stamp.
- Store-buffer forwarding on x86 masking a bug that surfaces on ARM.
- Compiler reorderings that move a data write past the stamp store.

### Weak memory reordering effects are not modeled

Hardware memory models introduce reorderings that TLA+ does not capture:

- **x86-TSO:** Store-store ordering is preserved, but store-load
  reordering is possible. The x86 memory model is close to SC for
  many patterns, which means bugs that would appear on ARM/RISC-V may
  be invisible on x86 and equally invisible in this TLA+ model.
- **ARM/AArch64:** Both store-load and store-store reordering are
  permitted. The seqlock pattern relies on acquire/release fences to
  prevent the reader from observing stale data after a valid stamp. A
  missing or misplaced barrier on ARM could cause a torn read that
  neither x86 testing nor this TLA+ model would detect.

Tools that model weak memory (CDSChecker, GenMC, LKMM/herd7) would be
needed to verify correctness under these hardware memory models.

### MPMC cursor advancement protocol is not verified

The `MpPublisher` uses a CAS-based sequence claiming protocol:

1. A producer atomically claims the next sequence number via
   `compare_exchange` on a shared cursor.
2. It writes the slot data and finalizes the stamp.
3. Other producers spin-wait on the slot stamp if they claimed a
   later sequence but an earlier producer has not yet finished writing.

This protocol introduces concerns (CAS retry fairness, livelock under
high contention, correct stamp-based waiting) that are entirely absent
from the single-producer model where the writer holds `&mut self` and
sequence advancement is a plain increment.

### SubscriberGroup

The `SubscriberGroup` batched fanout is not modeled separately. It
reuses the same `Slot::try_read` protocol as individual subscribers, so
the existing reader actions cover its correctness at the slot level.
