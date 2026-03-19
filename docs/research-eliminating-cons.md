# Research: Eliminating photon-ring Limitations

**Date:** 2026-03-19
**Scope:** Five architectural limitations of photon-ring and whether each can be eliminated, with concrete analysis grounded in the actual codebase.

---

## Con 1: No async/await Support

### Current State

photon-ring is `#![no_std]` with blocking receive paths:

- `Subscriber::try_recv()` -- non-blocking, returns `Err(TryRecvError::Empty)` immediately
- `Subscriber::recv()` -- hard-coded two-phase spin (64 bare spins, then PAUSE/WFE loop)
- `Subscriber::recv_with(WaitStrategy)` -- spin with configurable strategy
- `SubscriberGroup` has identical methods

The publisher never blocks on lossy channels. On bounded channels, `publish()` spin-waits for backpressure clearance.

There is no `Future` implementation, no waker registration, and no integration with any async runtime.

### Analysis

**Can `try_recv()` be wrapped in a `Future` that polls and registers a waker?**

Yes, but the wakeup mechanism is the fundamental problem. A naive `Future` would look like:

```rust
impl Future for RecvFuture<'_, T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        match self.sub.try_recv() {
            Ok(val) => Poll::Ready(val),
            Err(TryRecvError::Empty) => {
                // WHO calls cx.waker().wake()?
                Poll::Pending
            }
            Err(TryRecvError::Lagged { .. }) => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}
```

The problem: the publisher has no concept of wakers. It writes to a slot, stores the stamp, and advances the cursor. It does not know about, and cannot notify, any async consumers.

**Wakeup mechanism options:**

**(a) Poll-in-a-loop with yield.** The simplest approach: after returning `Pending`, immediately re-schedule via `cx.waker().wake_by_ref()`. This is equivalent to `tokio::task::yield_now()` between polls. Latency: one executor scheduling round-trip per empty poll (~200-500 ns on tokio). CPU: still burns a core if the task is the only one polling. This defeats the purpose of async -- you get no cooperative scheduling benefit.

**(b) Condvar/eventfd signaling.** Add an `Option<Arc<Notify>>` (or raw `eventfd` on Linux) to `SharedRing`. The publisher stores the waker/signals the eventfd after each `cursor.store()`. Latency cost: one `futex(FUTEX_WAKE)` syscall per publish (~50-100 ns on Linux). This is unacceptable for the core SPMC hot path where publish is currently ~3 ns. However, it could be a separate channel type (`channel_async`) or gated behind a feature flag so the core path is untouched.

**(c) io_uring for event notification.** Use `io_uring` `IORING_OP_POLL_ADD` on an eventfd. The publisher writes to the eventfd; io_uring delivers the completion to the async runtime. Latency: comparable to (b) for the publisher side (still needs the eventfd write), but the consumer side avoids a blocking `read()` -- the completion is delivered inline by the runtime's io_uring driver. This only works with io_uring-native runtimes (tokio-uring, glommio, monoio). Not portable.

**(d) Hybrid: eventfd for wakeup, try_recv for data.** The consumer registers interest in an eventfd. The publisher writes 1 to the eventfd after publishing (coalesced -- only if the eventfd counter is 0). The consumer's `Future` reads the eventfd to clear it, then drains via `try_recv()` in a loop. This amortizes the syscall cost: one eventfd write can wake up a consumer that then batch-reads many messages. Latency: first message pays the eventfd cost (~100 ns); subsequent messages in the same batch are at native try_recv speed.

**Latency cost of async vs raw spin:**

| Path | Latency |
|---|---|
| `recv()` bare spin | ~3-10 ns (already waiting when message arrives) |
| `recv_with(BusySpin)` | ~3-10 ns |
| Async poll loop (yield) | ~200-500 ns per scheduling round |
| Async eventfd wake | ~100-200 ns first message, ~3 ns subsequent batch |
| Async io_uring wake | ~50-150 ns first message |

The fundamental tension: async exists to multiplex many tasks on few threads. photon-ring exists to minimize per-message latency on dedicated threads. These are opposing goals. Any async bridge adds at least one scheduling hop.

**Is there a zero-cost way to bridge sync photon-ring with async consumers?**

No. The publisher must somehow notify the consumer, and that notification has nonzero cost. The closest to zero-cost is option (a) -- poll loop with yield -- but that still burns CPU and adds scheduling latency.

However, the overhead can be made *amortized zero-cost* via option (d): batch-drain after a single eventfd wakeup. If the consumer processes 100 messages per wakeup, the per-message async overhead is 1-2 ns.

**Should this be a separate crate or feature flag?**

Separate crate (`photon-ring-async`). Reasons:
1. The core crate is `#![no_std]`. Adding async would require `std` (for eventfd/futex) or an external runtime dependency.
2. The async bridge needs to own the wakeup infrastructure (eventfd, `Arc<Notify>`, etc.) without polluting the core `SharedRing`.
3. Different async runtimes have different completion models. A separate crate can provide `tokio` and `smol` feature flags.
4. The `Subscriber` type is already `Send` -- the async crate can wrap it.

### Verdict

| Attribute | Value |
|---|---|
| Feasibility | Medium |
| Effort | ~2-3 weeks for a solid tokio bridge |
| Approach | Separate `photon-ring-async` crate. Hybrid eventfd + batch-drain. Feature flags for tokio/smol. |
| Core change needed | Minimal: add an optional `Arc<AtomicBool>` "has_waiters" flag to `SharedRing` so the publisher can skip the eventfd write when no async consumers exist. This is zero-cost when no async consumers are present (branch on a cached bool). |
| Latency impact on sync path | Zero if async is not used. ~1-2 ns per-message amortized if async consumers exist (eventfd write cost spread across batch). |

---

## Con 2: Pipeline Stages Busy-Spin with No Configurable WaitStrategy

### Current State

The `spawn_stage` function in `src/topology/mod.rs` (lines 122-163) uses a hard-coded spin loop:

```rust
loop {
    if shutdown.load(Ordering::Acquire) { return; }
    match input.try_recv() {
        Ok(value) => {
            let out = f(value);
            output.publish(out);
        }
        Err(TryRecvError::Empty) => {
            core::hint::spin_loop();     // <-- hard-coded
        }
        Err(TryRecvError::Lagged { .. }) => {
            core::hint::spin_loop();     // <-- hard-coded
        }
    }
}
```

This is effectively `WaitStrategy::YieldSpin` behavior, but without using the `WaitStrategy` infrastructure that already exists and is already wired into `Subscriber::recv_with()`.

### Analysis

**Can `WaitStrategy` be added as a parameter to `StageBuilder::then()`?**

Yes, trivially. The infrastructure already exists:

1. `WaitStrategy` is `Copy + Send + Sync` (lines 251-252 of `wait.rs`).
2. `Subscriber::recv_with(strategy)` already implements all strategies correctly, including the slow path for lag recovery.
3. The `spawn_stage` function just needs to accept a `WaitStrategy` parameter and replace the inner loop with `input.recv_with(strategy)`.

The refactored `spawn_stage` would be:

```rust
fn spawn_stage<T, U>(
    mut input: Subscriber<T>,
    mut output: Publisher<U>,
    shutdown: Arc<AtomicBool>,
    f: impl Fn(T) -> U + Send + 'static,
    strategy: WaitStrategy,  // new parameter
) -> (Arc<AtomicU8>, JoinHandle<()>)
```

And the inner loop reduces to:

```rust
loop {
    if shutdown.load(Ordering::Acquire) { return; }
    let value = input.recv_with(strategy);
    output.publish(f(value));
}
```

Wait -- there is a subtlety. `recv_with()` blocks indefinitely. If the shutdown flag is set while `recv_with` is spinning, the stage will not exit until a message arrives. The current code checks `shutdown` on every `try_recv()` iteration. The fix: use `try_recv()` + `strategy.wait()` manually, preserving the shutdown check:

```rust
loop {
    if shutdown.load(Ordering::Acquire) { return; }
    match input.try_recv() {
        Ok(value) => {
            output.publish(f(value));
            iter = 0;
        }
        Err(TryRecvError::Empty) => {
            strategy.wait(iter);
            iter = iter.saturating_add(1);
        }
        Err(TryRecvError::Lagged { .. }) => {
            iter = 0;
        }
    }
}
```

**API design:**

Option A -- method on `StageBuilder`:
```rust
stages.then_with(|x| x * 2, WaitStrategy::Adaptive { spin_iters: 64, yield_iters: 64 })
```

Option B -- builder pattern:
```rust
stages.then(|x| x * 2).wait_strategy(WaitStrategy::BackoffSpin)
```

Option B does not compose well because `then()` currently consumes `self` and returns a new `StageBuilder<U>` -- the stage is already spawned inside `then()`. To support Option B, spawning would need to be deferred to `build()`, which is a larger refactor.

Option A is simpler and matches how `recv_with()` works on subscribers. Recommended approach:

```rust
impl<T: Pod> StageBuilder<T> {
    pub fn then<U: Pod>(self, f: impl Fn(T) -> U + Send + 'static) -> StageBuilder<U> {
        self.then_with(f, WaitStrategy::default())
    }

    pub fn then_with<U: Pod>(
        self, f: impl Fn(T) -> U + Send + 'static, strategy: WaitStrategy
    ) -> StageBuilder<U> { ... }
}
```

Similarly for `fan_out` -> `fan_out_with`.

**Backward compatibility:**

Zero breakage. `then()` keeps its existing signature and defaults to `WaitStrategy::default()` (Adaptive). `then_with()` is purely additive. Same for `fan_out()` / `fan_out_with()`.

### Verdict

| Attribute | Value |
|---|---|
| Feasibility | Easy |
| Effort | ~2-4 hours |
| Approach | Add `then_with()` and `fan_out_with()` methods that accept a `WaitStrategy`. Keep `then()` and `fan_out()` as convenience wrappers using `WaitStrategy::default()`. |
| Core change needed | Modify `spawn_stage()` to accept `WaitStrategy`. Add `then_with()` / `fan_out_with()` to `StageBuilder` and `FanOutBuilder`. |
| Breaking changes | None. Purely additive API. |

---

## Con 3: Message Derive Sharp Edges (No Generics, Enum Fallback)

### Current State

The `DeriveMessage` proc macro in `photon-ring-derive/src/lib.rs` has three limitations:

1. **No generics.** The macro extracts `input.data` as a struct with named fields but does not propagate generic parameters to the generated `{Name}Wire` struct. A generic struct like `Msg<T> { data: T }` would fail because the generated `MsgWire` would reference `T` without declaring it.

2. **Enum fallback.** The `classify()` function (line 185) falls through to `FieldKind::Enum` for any unrecognized type name. This means a field of type `MyStruct` (a nested struct, not an enum) silently gets treated as a `#[repr(u8)]` enum, generating `pub field: u8` in the wire struct and `transmute::<u8, MyStruct>` in the back-conversion. This compiles only if `MyStruct` is 1 byte (the size assertion catches larger types), but it is semantically wrong for structs.

3. **No nested struct support.** There is no way to include a nested struct that itself has a wire representation. You cannot write `#[photon(flatten)] inner: InnerStruct` to inline `InnerStructWire`'s fields.

### Analysis

**Can generic struct support be added?**

Partially. The challenges:

- The generated wire struct needs the same type parameters: `struct MsgWire<T: Pod> { data: T }`. The macro already extracts `input.generics` for the `Pod` derive but does not use it for `Message`.
- The wire struct's fields are computed by `classify()`, which pattern-matches on type names. A generic parameter `T` would hit the `Enum` fallback. The macro would need a way to know that `T` is passthrough (Pod) vs. needs conversion.
- A `#[photon(passthrough)]` attribute on the field would solve this: `#[photon(passthrough)] data: T` means "copy the field as-is into the wire struct." The macro would then propagate `T` to the wire struct's generic parameters.

The fundamental issue: `classify()` uses the *string name* of the type to decide the conversion. It cannot introspect whether a type parameter implements Pod at macro expansion time. An attribute-based approach is the only viable path.

Effort: Medium. Requires:
1. Parse `#[photon(...)]` attributes on fields.
2. Propagate `input.generics` to the wire struct declaration.
3. Add `where T: Pod` (or `where T: Copy`) bounds to the wire struct's `Pod` impl.
4. Handle the `From` impls with generic bounds.

**Can the enum fallback be replaced with attribute-based opt-in?**

Yes, and this should be done regardless of generic support. The current behavior is a footgun: any unrecognized type silently becomes `u8`. Two improvements:

1. **Emit a compile error for unrecognized types** instead of silently treating them as enums. Change the `_ => FieldKind::Enum` fallback to `_ => FieldKind::Unknown(name)`, and emit `compile_error!("field `X` has unrecognized type `Y`. Use #[photon(as_enum)] for repr(u8) enums or #[photon(passthrough)] for Pod types.")`.

2. **Add `#[photon(as_enum)]` attribute** for explicit enum opt-in. The current `FieldKind::Enum` logic (cast to u8, transmute back) only applies when the attribute is present.

This is a **breaking change** for existing `DeriveMessage` users who have enum fields without the attribute. Migration path: add `#[photon(as_enum)]` to every enum field. The compile error message would tell them exactly what to do.

**Can nested struct support be added?**

Yes, via `#[photon(flatten)]`. The macro would:
1. Detect the attribute on a field.
2. Require that the field's type has a corresponding `{Type}Wire` struct (by naming convention).
3. Inline `{Type}Wire`'s fields into the parent wire struct (prefixed with the field name to avoid collisions).
4. Generate conversion code that constructs the inner wire struct and destructures it.

This is hard. The macro cannot inspect the inner type's wire struct at expansion time (it may not be expanded yet, or it may be in a different crate). Two approaches:

- **Naming convention + trait:** Define a `WireConvertible` trait with `type Wire: Pod; fn to_wire(self) -> Self::Wire; fn from_wire(w: Self::Wire) -> Self;`. The macro generates an impl. Nested fields use the trait instead of inline expansion. This avoids the cross-crate visibility problem but does not flatten -- the wire struct would have `inner: InnerStructWire` as a single field.
- **True flattening:** Not feasible in a proc macro without full type resolution. The macro would need to know `InnerStructWire`'s fields, which requires cross-crate expansion.

The trait-based approach (non-flattened nesting) is practical and covers most use cases. True flattening would require a build script or IDE plugin.

### Verdict

| Attribute | Value |
|---|---|
| Feasibility | Medium (attribute opt-in), Hard (generics + flatten) |
| Effort | 1-2 days for attribute-based enum opt-in + error on unknown types. 1 week for generic passthrough. 2+ weeks for nested struct trait. |
| Recommended approach (phase 1) | Replace the silent enum fallback with a compile error. Add `#[photon(as_enum)]` for explicit enum fields. This is a breaking change but prevents silent miscompilation. |
| Recommended approach (phase 2) | Add `#[photon(passthrough)]` for fields whose types are already Pod (including generic parameters). Propagate generics to wire struct. |
| Recommended approach (phase 3) | Add a `WireConvertible` trait for nested struct composition (non-flattened). |
| Breaking changes | Phase 1 breaks existing `DeriveMessage` users with enum fields. |

---

## Con 4: No Metrics/Observability Integration

### Current State

photon-ring exposes raw counters on subscribers:

- `Subscriber::total_received() -> u64`
- `Subscriber::total_lagged() -> u64`
- `Subscriber::receive_ratio() -> f64`
- `Subscriber::pending() -> u64`
- `Publisher::sequence() -> u64` / `published() -> u64`
- `Publisher::capacity() -> u64`
- `SubscriberGroup` has the same counter methods

These are local counters with no export mechanism. No histograms, no timestamps, no integration with prometheus, opentelemetry, or tracing.

### Analysis

**What metrics matter for a ring buffer?**

| Metric | Type | Source | Why |
|---|---|---|---|
| `photon_messages_published_total` | Counter | `Publisher::sequence()` | Throughput |
| `photon_messages_received_total` | Counter | `Subscriber::total_received()` | Per-consumer throughput |
| `photon_messages_lagged_total` | Counter | `Subscriber::total_lagged()` | Lag pressure |
| `photon_receive_ratio` | Gauge | `Subscriber::receive_ratio()` | Health indicator |
| `photon_pending_messages` | Gauge | `Subscriber::pending()` | Queue depth |
| `photon_ring_utilization` | Gauge | `pending() / capacity()` | Saturation |
| `photon_publish_latency_ns` | Histogram | RDTSC around `publish()` | Hot path timing |
| `photon_recv_latency_ns` | Histogram | RDTSC around `recv()` | Consumer latency |
| `photon_backpressure_stalls_total` | Counter | New counter in publisher | Backpressure events |

**Should this be a feature flag or a separate crate?**

Separate crate (`photon-ring-metrics`). Reasons:

1. **no_std compatibility.** The core crate is `#![no_std]`. Prometheus and OpenTelemetry require `std` and pull in significant dependency trees (hyper, tonic, prost for OTLP).
2. **Dependency weight.** `prometheus-client` adds ~15 crates. `opentelemetry` adds ~30+. These should not be optional deps of a latency-critical `no_std` crate.
3. **Zero-cost when disabled.** If the metrics crate is not in the dependency graph, there is literally no code generated. A feature flag would still require conditional compilation throughout the core crate.

Architecture of `photon-ring-metrics`:

```rust
// Wraps a Subscriber and periodically exports counters
pub struct MeteredSubscriber<T: Pod> {
    inner: Subscriber<T>,
    received_counter: prometheus::IntCounter,
    lagged_counter: prometheus::IntCounter,
    // ...
}

impl<T: Pod> MeteredSubscriber<T> {
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let result = self.inner.try_recv();
        if result.is_ok() {
            self.received_counter.inc();
        }
        result
    }
}
```

This adds one atomic increment per message (~1 ns). For truly zero-cost when metrics are not needed, the wrapper is simply not used.

For RDTSC-based latency histograms, the publisher could stamp each message with a TSC value (requires the message type to include a timestamp field -- application-level concern, not library-level).

**Alternative: callback-based approach.**

Instead of a wrapper crate, add an optional callback hook in the core crate:

```rust
pub struct Subscriber<T: Pod> {
    // ... existing fields ...
    #[cfg(feature = "metrics-callback")]
    on_recv: Option<fn(u64)>,  // called with sequence number
    #[cfg(feature = "metrics-callback")]
    on_lag: Option<fn(u64)>,   // called with skipped count
}
```

This is a feature flag on the core crate, adding a branch + indirect call on the hot path (~2-3 ns when enabled, 0 ns when compiled out). It provides a generic hook that any metrics system can plug into.

### Verdict

| Attribute | Value |
|---|---|
| Feasibility | Easy |
| Effort | 1 week for a solid metrics crate with prometheus + OTLP export |
| Approach | Separate `photon-ring-metrics` crate with wrapper types. Optionally, a `metrics-callback` feature flag on the core crate for lightweight hooks. |
| Core change needed | None for the separate crate approach. A single `#[cfg(feature)]` callback field if hooks are desired. |
| Zero-cost when disabled | Yes (separate crate: not in dep graph = no code. Feature flag: compiled out entirely). |

---

## Con 5: MPMC Not Formally Verified (No Loom Testing)

### Current State

The SPMC seqlock protocol is verified via TLA+ (`verification/seqlock.tla`): NoTornRead, ResultIsValid, and ReaderProgress are model-checked with TLC (2 readers, ring size 2, 4 sequences).

The MPMC cursor advancement protocol (`MpPublisher::advance_cursor` and `catch_up_cursor` in `src/channel/mp_publisher.rs`) has **no formal verification**. It relies on:

1. `fetch_add(1, AcqRel)` on `next_seq` to claim sequence numbers.
2. A single CAS on `cursor` to advance it (`expected_cursor -> seq`).
3. Stamp-based predecessor waiting: if the CAS fails, poll the predecessor slot's stamp until it shows committed (`stamp >= pred_done`).
4. A catch-up loop that advances the cursor past any successors whose stamps are already committed.

This protocol has subtle interleavings that are not covered by the TLA+ spec (which only models SPMC).

The existing Rust tests (`correctness.rs` lines 1064-1329) exercise MPMC with 2-4 publishers and verify per-stream ordering, but these are randomized schedule tests that only cover a fraction of possible interleavings.

### Analysis

**How hard is it to add loom tests?**

Loom is designed for testing Rust atomics-based concurrent code by exhaustively exploring thread interleavings. The challenges specific to photon-ring:

1. **`#![no_std]` incompatibility.** Loom requires `std` (it wraps `std::sync::atomic::*`, `std::thread::*`). photon-ring's atomics are `core::sync::atomic::*`. To use loom, the crate would need conditional compilation: `#[cfg(loom)] use loom::sync::atomic::*; #[cfg(not(loom))] use core::sync::atomic::*;`. This is a well-established pattern (tokio, crossbeam use it).

2. **UnsafeCell access.** Loom tracks `UnsafeCell` accesses to detect data races. The `Slot<T>` struct uses `UnsafeCell<MaybeUninit<T>>` with `write_volatile`/`read_volatile` (default mode) or `AtomicU64::from_ptr` (atomic-slots mode). Under loom, the volatile path would need to be replaced with loom-trackable operations. The `atomic-slots` feature is naturally loom-compatible because it already uses atomic types.

3. **State space explosion.** Loom exhaustively explores all interleavings. With 2 producers publishing 2 messages each into a ring of size 2, the state space is already enormous. Practical loom tests for MPMC need:
   - Very small parameters: 2 producers, 1 consumer, ring size 2, 2-3 messages per producer.
   - Focused scope: test only `advance_cursor` + `catch_up_cursor`, not the entire publish path.

4. **`spin_loop()` and `PAUSE` instructions.** Loom's scheduler must be aware of spin loops. The `core::hint::spin_loop()` calls in `advance_cursor` need to be replaced with `loom::thread::yield_now()` under loom.

**What specific interleavings should be tested?**

| Test | Description | What it catches |
|---|---|---|
| Two producers, sequential slots | P1 claims seq=0, P2 claims seq=1. P2 commits first. Test that cursor advances past both. | Catch-up loop correctness. |
| Two producers, same-wrap slot | P1 claims seq=0, P2 claims seq=2 (same slot index in ring size 2). P1 is slow. Test that P2's commit does not corrupt P1's pending write. | Ring-wrap safety. |
| Predecessor stall | P1 claims seq=0 but stalls before writing. P2 claims seq=1, writes, and waits on P1's stamp. Test that P2 eventually advances. | Stamp-based predecessor waiting liveness. |
| Cursor CAS contention | Both producers finish writing simultaneously and race on the cursor CAS. Test that the cursor ends up at the correct value. | CAS + catch-up loop ordering. |
| Consumer reads during MPMC gap | Consumer tries to read seq=1 while P1 (seq=0) is still writing and P2 (seq=1) is committed. Cursor is at 0 (not yet advanced). | Consumer sees Empty, not a torn or out-of-order value. |
| Three producers, middle stall | P1=seq0, P2=seq1, P3=seq2. P2 stalls. P1 and P3 commit. Test cursor advancement. | Catch-up loop does not skip over uncommitted slots. |

**Can the existing tests be adapted for loom?**

Partially. The cross-thread MPMC tests in `correctness.rs` use `std::thread::spawn` and large message counts (10K-40K). These cannot run under loom (state space would be infinite). They would need to be rewritten as minimal-parameter loom tests.

The single-threaded MPMC ordering test (`mpmc_ordering`, line 1148) tests sequential publishes on one thread and does not exercise concurrent interleavings at all.

New purpose-built loom tests are needed. The existing tests serve as smoke tests; loom tests would provide exhaustive coverage of the critical interleavings.

**Implementation plan:**

1. Add `loom` as a dev-dependency with a `loom` cfg flag.
2. Add `#[cfg(loom)]` / `#[cfg(not(loom))]` imports for atomic types in `ring.rs`, `slot.rs`, `mp_publisher.rs`.
3. The `atomic-slots` feature is the natural target for loom testing (already uses `AtomicU64` for all data access -- no UB under loom's model).
4. Write 5-6 focused loom tests in a new `tests/loom_mpmc.rs` file.
5. Run with `RUSTFLAGS="--cfg loom" cargo test --test loom_mpmc --features atomic-slots`.

**Alternative: extend the TLA+ spec.**

The existing `seqlock.tla` models SPMC. A separate `mpmc.tla` could model:
- Multiple writers with `fetch_add` sequence claiming.
- The CAS-based cursor advancement.
- The stamp-based predecessor wait.
- The catch-up loop.

This would provide a formal proof of the algorithm's correctness (NoGap: cursor never skips an uncommitted slot; NoReorder: consumers see messages in sequence order; Liveness: cursor eventually advances past all committed slots). TLA+/TLC can handle small parameter spaces (2-3 producers, ring size 2, 3-4 sequences) and would complement loom testing.

### Verdict

| Attribute | Value |
|---|---|
| Feasibility | Medium |
| Effort | 1-2 weeks (loom tests). 3-5 days (TLA+ MPMC spec). |
| Approach | Both loom AND TLA+. Loom catches Rust-specific memory ordering bugs (Acquire/Release misuse). TLA+ catches algorithmic bugs (cursor advancement logic). |
| Core change needed | Add `#[cfg(loom)]` conditional imports in `ring.rs`, `slot.rs`, `mp_publisher.rs`, `wait.rs`. Replace `spin_loop()` with `loom::thread::yield_now()` under loom. Requires `atomic-slots` feature for loom compatibility. |
| Breaking changes | None. All changes are behind `#[cfg(loom)]` and only affect the test path. |
| Prerequisite | The `atomic-slots` feature must be enabled for loom testing, since the default volatile-based seqlock path has data races that loom would (correctly) flag. |

---

## Summary

| Con | Feasibility | Effort | Delivery | Core Impact |
|---|---|---|---|---|
| 1. No async/await | Medium | 2-3 weeks | Separate crate | Near-zero (optional flag on SharedRing) |
| 2. No WaitStrategy in pipeline | **Easy** | 2-4 hours | Core change | None (additive API) |
| 3. Derive sharp edges | Medium | 1-2 days (phase 1) | Core change (breaking) | Phase 1 is breaking; phases 2-3 additive |
| 4. No metrics | Easy | 1 week | Separate crate | None |
| 5. No loom testing | Medium | 1-2 weeks | Dev-only (cfg loom) | None (test infrastructure only) |

**Recommended priority order:**

1. **Con 2** (pipeline WaitStrategy) -- trivial, fixes a real usability gap, zero risk.
2. **Con 3 phase 1** (enum fallback error) -- fixes a safety footgun, small scope.
3. **Con 5** (loom + TLA+ for MPMC) -- correctness is existential; MPMC is the newest and least-verified code path.
4. **Con 4** (metrics crate) -- valuable but not blocking.
5. **Con 1** (async bridge) -- highest effort, most architectural questions, least aligned with photon-ring's core identity.
