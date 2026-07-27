# Roadmap

Shipped work lives in [CHANGELOG.md](CHANGELOG.md). This file tracks what is
**planned but not yet shipped**. Nothing here is a commitment or a date — it is
the current wish list, roughly in priority order.

## Performance & platform

- NUMA-aware ring allocation via `set_mempolicy` at construction time, and a
  multi-socket Xeon benchmark quantifying pinned vs unpinned placement.
- RISC-V `WRS` (wait-on-reservation-set) as a `WaitStrategy` backend where available.

## Async

- Event-driven wakeup for `photon-ring-async`, so an idle `AsyncSubscriber`
  parks instead of holding a core. Needs a waker registry on the ring and a
  publish-side check (one relaxed load when nobody waits), plus a loom model
  of the register-versus-publish race. Until then the crate is honest about
  being a polling adapter.

## Observability

- Optional RDTSC-stamped latency histogram on the receive path.
- A Prometheus / StatsD export example built on `photon-ring-metrics`
  (the crate stays framework-agnostic; the exporter lives in an example).

## Verification

- Loom model that drives the real ring types via `cfg(loom)`, not just the
  standalone MPMC cursor model.
- Property-based testing with `proptest` / `cargo-fuzz`.

## Examples

- DPDK → Photon Ring ingest pipeline.
- Solarflare `ef_vi` → Photon Ring fan-out.

## Writing

- Complete the technical report (`docs/technical-report.md`) into a full paper
  and submit to a systems venue.
