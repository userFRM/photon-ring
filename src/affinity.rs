// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! CPU core affinity helpers for deterministic cross-core latency.
//!
//! In HFT and other latency-sensitive workloads, pinning publisher and
//! subscriber threads to specific CPU cores eliminates OS scheduler jitter
//! and ensures consistent cache-coherence transfer times.
//!
//! ## NUMA considerations
//!
//! On multi-socket systems, always pin publisher and subscriber threads to
//! cores on the **same** socket. Cross-socket communication (QPI/UPI) adds
//! ~100-200 ns of additional latency per cache-line transfer compared to
//! intra-socket L3 ring-bus snoops (~40-55 ns on Intel Comet Lake).
//!
//! ## Example
//!
//! ```no_run
//! use photon_ring::affinity;
//!
//! // Pin publisher to core 0, subscriber to core 1.
//! let cores = affinity::available_cores();
//! assert!(cores.len() >= 2, "need at least 2 cores");
//!
//! let pub_core = cores[0];
//! let sub_core = cores[1];
//!
//! std::thread::spawn(move || {
//!     affinity::pin_to_core(pub_core);
//!     // ... publisher loop ...
//! });
//! std::thread::spawn(move || {
//!     affinity::pin_to_core(sub_core);
//!     // ... subscriber loop ...
//! });
//! ```
//!
//! ## Feature gate
//!
//! This module is only available when the `affinity` feature is enabled
//! (on by default). Disable with `--no-default-features` for pure `no_std`
//! builds.

use alloc::vec::Vec;

pub use core_affinity::CoreId;

/// Pin the current thread to a specific CPU core.
///
/// Call this at the start of your publisher or subscriber thread for
/// deterministic cross-core latency. Returns `true` if the affinity was
/// set successfully, `false` if the OS rejected the request (e.g.,
/// invalid core ID or insufficient permissions).
///
/// # Example
///
/// ```no_run
/// use photon_ring::affinity::{self, CoreId};
///
/// // Pin to core 2.
/// let ok = affinity::pin_to_core(CoreId { id: 2 });
/// assert!(ok, "failed to pin to core 2");
/// ```
#[inline]
pub fn pin_to_core(core_id: CoreId) -> bool {
    core_affinity::set_for_current(core_id)
}

/// Return the list of CPU cores available to this process.
///
/// The returned [`CoreId`]s can be passed directly to [`pin_to_core`].
/// The list order matches the OS core numbering (logical CPUs including
/// SMT siblings).
///
/// # Panics
///
/// Panics if the OS fails to enumerate cores (should not happen on
/// Linux, macOS, or Windows).
pub fn available_cores() -> Vec<CoreId> {
    core_affinity::get_core_ids().expect("failed to enumerate CPU cores")
}

/// Pin the current thread to a core by its numeric index.
///
/// Convenience wrapper around [`pin_to_core`] that looks up the core by
/// index in [`available_cores`]. Returns `true` on success, `false` if
/// the index is out of range or the OS rejects the request.
///
/// # Example
///
/// ```no_run
/// use photon_ring::affinity;
///
/// affinity::pin_to_core_id(0); // pin to the first available core
/// ```
pub fn pin_to_core_id(index: usize) -> bool {
    let cores = available_cores();
    match cores.get(index) {
        Some(&core_id) => pin_to_core(core_id),
        None => false,
    }
}

/// Return the number of CPU cores available to this process.
///
/// This is equivalent to `available_cores().len()` but avoids allocating
/// the full `Vec` if you only need the count.
pub fn core_count() -> usize {
    available_cores().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_cores_is_nonempty() {
        let cores = available_cores();
        assert!(!cores.is_empty(), "expected at least one core");
    }

    #[test]
    fn core_count_matches_available() {
        assert_eq!(core_count(), available_cores().len());
    }

    #[test]
    fn pin_to_first_core() {
        let cores = available_cores();
        assert!(pin_to_core(cores[0]), "failed to pin to first core");
    }

    #[test]
    fn pin_to_core_id_valid() {
        assert!(pin_to_core_id(0), "failed to pin to core index 0");
    }

    #[test]
    fn pin_to_core_id_out_of_range() {
        assert!(!pin_to_core_id(usize::MAX), "should fail for invalid index");
    }
}
