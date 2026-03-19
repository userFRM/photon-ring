# photon-ring-async

Runtime-agnostic async wrappers for [photon-ring](https://github.com/userFRM/photon-ring) pub/sub channels.

## Approach

photon-ring's publisher has no waker infrastructure -- it writes to a slot, stores the stamp, and advances the cursor. There is no notification mechanism for async consumers.

This crate bridges the gap with **yield-based polling**: the async subscriber tries `spin_budget` synchronous `try_recv()` calls per poll, then yields to the executor. This is cooperative spin-polling, not event-driven wakeup. The trade-off: one executor scheduling round-trip (~200-500 ns) per empty yield, but other tasks can run between polls.

No tokio, async-std, or any runtime dependency. Uses only `core::task` and `core::future::poll_fn`.

## Usage

```rust
let (mut pub_, subs) = photon_ring::channel::<u64>(64);
let mut async_sub = photon_ring_async::AsyncSubscriber::new(subs.subscribe());

pub_.publish(42);
let value = async_sub.recv().await;
assert_eq!(value, 42);
```

### Custom spin budget

```rust
// Low budget: yields quickly, good for mixed workloads
let mut sub = photon_ring_async::AsyncSubscriber::with_spin_budget(subs.subscribe(), 4);

// High budget: lower latency, holds executor thread longer
let mut sub = photon_ring_async::AsyncSubscriber::with_spin_budget(subs.subscribe(), 1024);
```

### Subscriber groups

```rust
let mut group = photon_ring_async::AsyncSubscriberGroup::<u64, 4>::new(
    subs.subscribe_group::<4>(),
);
let value = group.recv().await;
```

### Batch receive

```rust
let mut buf = [0u64; 256];
let count = async_sub.recv_batch(&mut buf).await;
// buf[..count] contains the received messages
```

## License

Apache-2.0
