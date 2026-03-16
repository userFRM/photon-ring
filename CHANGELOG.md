# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-03-16

### Added
- Core SPMC channel: `channel()`, `Publisher<T>`, `Subscriber<T>`, `Subscribable<T>`
- Seqlock-stamped ring buffer with cache-line-aligned slots (`#[repr(C, align(64))]`)
- Per-subscriber cursor (zero contention between consumers)
- `try_recv()`, `recv()` (busy-spin), `latest()` (skip to newest)
- `publish_batch()` for amortized cursor updates
- `subscribe_from_oldest()` for replay from oldest available message
- `pending()` (capped at ring capacity) and `published()` queries
- Lag detection via `TryRecvError::Lagged { skipped }` with head-cursor-based computation
- Named-topic bus: `Photon<T>` with `publisher()`, `subscribe()`, `subscribable()`
- Full `no_std` support (requires `alloc`) using `hashbrown` and `spin`
- 26 correctness tests including cross-thread SPMC and 1M-message stress test
- MIRI verification (22 single-threaded tests)
- Criterion benchmarks with `disruptor` v4.0.0 comparison
- Market data example (4-topic fan-out, ~160M msg/s)
- SPDX license headers on all source files
- Dual license: MIT OR Apache-2.0
