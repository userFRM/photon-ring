// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! CPU core affinity helpers for deterministic cross-core latency.
//!
//! Pinning publisher and subscriber threads to specific CPU cores
//! eliminates OS scheduler jitter and ensures consistent cache-coherence
//! transfer times.
//!
//! ## NUMA considerations
//!
//! On multi-socket systems, pin publisher and subscriber threads to
//! cores on the **same** socket. Cross-socket communication (QPI/UPI)
//! adds ~100-200 ns of additional latency per cache-line transfer
//! compared to intra-socket L3 snoops (~40-55 ns on Intel Comet Lake).
//!
//! ## Example
//!
//! ```no_run
//! use photon_ring::affinity;
//!
//! let cores = affinity::available_cores();
//! assert!(cores.len() >= 2, "need at least 2 cores");
//!
//! // Pin to the first core
//! assert!(affinity::pin_to_core(cores[0]));
//! ```

use alloc::vec::Vec;

pub use core_affinity2::CoreId;

/// Pin the current thread to a specific CPU core.
///
/// Returns `true` on success, `false` if the OS rejected the request
/// (e.g., invalid core ID, insufficient permissions, or unsupported
/// platform).
///
/// # Example
///
/// ```no_run
/// use photon_ring::affinity;
///
/// let cores = affinity::available_cores();
/// assert!(affinity::pin_to_core(cores[0]), "failed to pin");
/// ```
#[inline]
pub fn pin_to_core(core_id: CoreId) -> bool {
    core_id.set_affinity().is_ok()
}

/// Return the list of CPU cores available to this process.
///
/// The returned [`CoreId`]s can be passed directly to [`pin_to_core`].
/// The list order matches the OS core numbering (logical CPUs including
/// SMT siblings).
pub fn available_cores() -> Vec<CoreId> {
    core_affinity2::get_core_ids().unwrap_or_default()
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
/// assert!(affinity::pin_to_core_id(0), "failed to pin to core 0");
/// ```
pub fn pin_to_core_id(index: usize) -> bool {
    let cores = available_cores();
    match cores.get(index) {
        Some(&core_id) => pin_to_core(core_id),
        None => false,
    }
}

/// Return the number of CPU cores available to this process.
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
