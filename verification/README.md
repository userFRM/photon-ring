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

- The spec models sequentially consistent memory (TLA+ default). The
  Rust implementation uses `Acquire`/`Release` orderings, which are
  weaker. The TLA+ spec therefore over-approximates correctness: if a
  bug exists under SC, it exists under weaker models too. The converse
  is not guaranteed -- memory-model-specific bugs (e.g., missing fences
  on ARM) require tools like LKMM or CDSChecker.
- The multi-producer (`MpPublisher`) CAS protocol is not modelled. This
  spec covers the SPMC case only.
- The `SubscriberGroup` batched fanout is not modelled separately; it
  uses the same `Slot::try_read` protocol as individual subscribers.
