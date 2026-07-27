# Security Policy

## Supported versions

| Version | Supported |
|---------|-----------|
| 2.5.x   | ✅        |
| < 2.5   | ❌        |

## Reporting a vulnerability

Please report security issues privately through GitHub's
[**Report a vulnerability**](https://github.com/userFRM/photon-ring/security/advisories/new)
flow rather than opening a public issue. You can expect an initial response
within a few days.

## Scope

Photon Ring is an `unsafe`-heavy, lock-free crate. In scope:

- Memory-safety or data-race bugs in the ring, seqlock protocol, or `atomic-slots` stripes.
- Soundness holes in the `Pod` bound or the derive macros (e.g. a safe API that admits a non-`Pod` type).
- Undefined behavior reproducible under Miri on a default build.

Note that `default-features = false` selects the older volatile slot
implementation, which is a deliberate data race under the Rust abstract machine
— correct on real hardware, but flagged by Miri. That specific, documented
behavior is not a vulnerability; the default build is formally sound. Reports of
*additional* soundness gaps are welcome.
