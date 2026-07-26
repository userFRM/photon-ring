<!--
  Copyright 2026 Photon Ring Contributors
  SPDX-License-Identifier: MIT OR Apache-2.0
-->

# Migrating from `disruptor-rs`

A side-by-side port guide, including the places where the two libraries genuinely
differ. Read the [semantic differences](#semantic-differences) before porting
anything you depend on — some of them will change your shutdown tests.

## The mechanical part

```rust
// disruptor-rs
let mut producer = build_single_producer(1024, || Event::default(), BusySpin)
    .handle_events_with(|e: &Event, seq: Sequence, end_of_batch: bool| {
        handle(e, seq, end_of_batch);
    })
    .build();

producer.publish(|slot| { slot.price = 100; });
// dropping the producer drains and joins the consumer
```

```rust
// photon-ring
use photon_ring::{channel_bounded, WaitStrategy};
use photon_ring::topology::{Consumer, DrainPolicy};

let (mut producer, subs) = channel_bounded::<Event>(1024, 0);

let consumer = Consumer::spawn(
    subs.subscribe(),
    WaitStrategy::BusySpin,
    DrainPolicy::Drain,
    |e: Event, seq: u64, end_of_batch: bool| {
        handle(&e, seq, end_of_batch);
    },
);

producer.publish(Event { price: 100, ..Default::default() });

consumer.shutdown();
consumer.join();
```

| `disruptor-rs` | photon-ring |
|---|---|
| `build_single_producer(n, factory, W)` | `channel_bounded::<T>(n, 0)` |
| `.handle_events_with(\|e, seq, eob\|)` | `Consumer::spawn(subs.subscribe(), W, DrainPolicy::Drain, \|e, seq, eob\|)` |
| `.build()` | not needed — `Consumer::spawn` starts the thread |
| `producer.publish(\|slot\| ...)` | `producer.publish(value)` or `publish_with(\|slot\| ...)` |
| `drop(producer)` (drains, joins) | `consumer.shutdown(); consumer.join();` |
| `BusySpin` | `WaitStrategy::BusySpin` |
| a second `.handle_events_with(...)` | a second `subs.subscribe()` + `Consumer::spawn` |
| `.and_then(...)` dependency chain | `DependencyBarrier` + `try_recv_gated`, or `topology::Pipeline` |

Capacity does not need to be a power of two here. Watermark `0` is the closest
match to the Disruptor's behaviour: the publisher blocks only when it would
overwrite a message a registered consumer has not read.

## Semantic differences

**Payloads must be `Pod`.** Every bit pattern must be valid and the type must
carry no padding, which rules out `String`, `Vec`, `bool`, `enum`, and `Option`.
The Disruptor can hold those because its barrier guarantees no consumer reads a
slot being written; photon's per-slot stamps let consumers run uncoordinated
instead, and that is what the bound pays for. Use `#[repr(C)]` structs of plain
numerics with explicit padding fields, or `#[derive(photon_ring::DeriveMessage)]`
to generate a wire struct from a domain type. **This is the largest porting cost
and it is not going away.**

**Handlers receive the message by value.** The Disruptor hands you `&E` into the
ring; photon copies the message out. For payloads at or below a cache line the
copy is a few percent of the end-to-end latency; it grows with payload size.

**A panicking handler does not wedge the publisher.** In `disruptor-rs` a
panicked handler leaves its cursor in the barrier's minimum forever, so the
producer stops permanently — their own consumer joins with
`.expect("Consumer should not panic.")`. In photon the handler's panic is
captured, the subscriber is dropped, its tracker leaves the backpressure set, and
the publisher continues. Check `consumer.panicked()`. If your design relied on a
panic halting the world, you must now check for it explicitly.

**Multi-producer has no backpressure.** `channel_mpmc` is lossy: it has no
backpressure state at all, so there is no multi-producer equivalent of the
Disruptor's blocking multi-producer. Single-producer bounded is the configuration
that matches.

**`subscribe_from_oldest` on a bounded channel blocks the publisher.** It
registers a consumer a full ring behind, which legitimately leaves the publisher
no room until that consumer drains. Use `subscribe()` unless you specifically
want the retained history.

## What you gain

**Observers that cannot stall you.** `subs.subscribe_lossy()` returns a consumer
the publisher's backpressure scan ignores. Telemetry, logging and debug taps can
share the production ring without being able to stop it, and they report exactly
what they missed through `Lagged { skipped }` and `receive_ratio()`. Every
consumer in a Disruptor sits on the gating sequence, so this is not expressible
there.

**Consumers you can attach and detach while running.** The consumer set is fixed
at `build()` in `disruptor-rs`. Here `Subscribable` is `Clone` and
`subscribe()`/`subscribe_lossy()` work at any time, so a debug tap can be
attached to a live ring and dropped again.

**Broadcast without a shared barrier.** Each subscriber holds a private cursor,
so adding consumers does not add contention on a shared sequence.

**`no_std`** (with `alloc`), and an `atomic-slots` feature whose hot path is free
of data races under the Rust memory model, verified under Miri in CI.

## Performance

Measured on one machine with both libraries in the same binary, same Criterion
invocation, `BusySpin`, 4096-slot rings. Treat them as a reproducible snapshot
rather than universal constants, and re-run `cargo bench` on your own hardware.

| | photon-ring | `disruptor-rs` |
|---|---|---|
| publish, live consumer, matched lossless semantics | 7.96 ns | 20.46 ns |
| cross-thread roundtrip | 97.6 ns | 132.2 ns |

The publish figure compares like for like: both with a consumer thread running,
and photon on a bounded channel so both are lossless. Photon's lossy `channel()`
publishes in 6.16 ns, but that is a weaker delivery guarantee and not a fair
comparison.
