// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! Platform-specific memory control for ring buffer allocation.
//!
//! Provides huge-page-backed allocation via `mmap`, `mlock` for page-fault
//! prevention, and pre-faulting for deterministic first-access latency.
//!
//! Available on Linux with the `hugepages` feature.

/// Allocate `size` bytes backed by 2 MiB transparent huge pages.
///
/// Uses `mmap` with `MAP_HUGETLB` to request huge-page-backed anonymous
/// memory. Returns a null pointer on failure (caller must check).
///
/// # Safety
///
/// The returned pointer must eventually be freed with
/// `libc::munmap(ptr as *mut libc::c_void, size)`. The caller is
/// responsible for ensuring `size` is a multiple of the huge page size
/// (2 MiB) for optimal alignment.
pub unsafe fn mmap_huge_pages(size: usize) -> *mut u8 {
    let ptr = libc::mmap(
        core::ptr::null_mut(),
        size,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB,
        -1,
        0,
    );
    if ptr == libc::MAP_FAILED {
        core::ptr::null_mut()
    } else {
        ptr as *mut u8
    }
}

/// Lock `size` bytes starting at `ptr` into physical RAM, preventing the
/// kernel from swapping them to disk.
///
/// Returns `true` on success. Requires `CAP_IPC_LOCK` capability or
/// sufficient `RLIMIT_MEMLOCK` on Linux.
///
/// # Safety
///
/// `ptr` must point to a valid, mapped region of at least `size` bytes.
pub unsafe fn mlock_pages(ptr: *const u8, size: usize) -> bool {
    libc::mlock(ptr as *const libc::c_void, size) == 0
}

/// Touch every 4 KiB page in the region `[ptr, ptr + size)` to force the
/// kernel to back them with physical frames. Eliminates first-access page
/// faults on the hot path.
///
/// # Safety
///
/// `ptr` must point to a valid, writable region of at least `size` bytes.
pub unsafe fn prefault_pages(ptr: *mut u8, size: usize) {
    for offset in (0..size).step_by(4096) {
        core::ptr::write_volatile(ptr.add(offset), 0u8);
    }
}

/// Set memory policy to prefer allocations on a specific NUMA node.
/// Call before creating channels to place the ring on the publisher's node.
///
/// Uses `set_mempolicy(MPOL_PREFERRED, ...)` so that subsequent heap
/// allocations (including the ring buffer `Vec<Slot<T>>`) are placed on
/// the requested node when possible, falling back to other nodes if
/// memory is exhausted.
///
/// Returns `true` on success, `false` if NUMA is not available or the
/// node ID is invalid.
///
/// # Example
///
/// ```no_run
/// use photon_ring::mem;
///
/// // Pin to core 0, set NUMA preference for that core's node
/// photon_ring::affinity::pin_to_core_id(0);
/// mem::set_numa_preferred(0); // NUMA node 0
/// let (pub_, subs) = photon_ring::channel::<u64>(4096);
/// // Ring is now allocated on NUMA node 0
/// mem::reset_numa_policy();
/// ```
pub fn set_numa_preferred(node: usize) -> bool {
    // MPOL_PREFERRED = 1: prefer the specified node, fall back to others.
    let nodemask: libc::c_ulong = 1u64.wrapping_shl(node as u32) as libc::c_ulong;
    let maxnode = core::mem::size_of::<libc::c_ulong>() * 8;
    unsafe {
        libc::syscall(
            libc::SYS_set_mempolicy,
            1i32, // MPOL_PREFERRED
            &nodemask as *const libc::c_ulong,
            maxnode as libc::c_ulong,
        ) == 0
    }
}

/// Reset the memory allocation policy to the system default.
///
/// Call this after channel creation to avoid affecting unrelated
/// allocations on the same thread.
///
/// Returns `true` on success.
pub fn reset_numa_policy() -> bool {
    // MPOL_DEFAULT = 0: revert to the system-wide default policy.
    unsafe {
        libc::syscall(
            libc::SYS_set_mempolicy,
            0i32, // MPOL_DEFAULT
            core::ptr::null::<libc::c_ulong>(),
            0 as libc::c_ulong,
        ) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel;
    use crate::slot::Slot;

    #[test]
    fn slot_alignment_is_cache_line() {
        assert_eq!(core::mem::align_of::<Slot<u64>>(), 64);
    }

    #[test]
    fn test_prefault() {
        let (mut pub_, subs) = channel::<u64>(64);

        // SAFETY: Called before any publish/subscribe operations begin.
        unsafe {
            pub_.prefault();
        }

        let mut sub = subs.subscribe();

        // The ring should still work correctly after prefaulting.
        pub_.publish(42);
        pub_.publish(43);
        assert_eq!(sub.try_recv(), Ok(42));
        assert_eq!(sub.try_recv(), Ok(43));
    }

    #[test]
    fn test_mlock() {
        let (pub_, _subs) = channel::<u64>(64);

        // mlock may fail without sufficient permissions — that is expected.
        // We just verify the API is callable and returns a bool.
        let _result: bool = pub_.mlock();
    }

    #[test]
    fn test_mmap_huge_pages_returns_ptr_or_null() {
        // Huge pages may not be available on the test system, so we accept
        // either a valid pointer or null. We just verify the function is
        // callable and does not panic.
        let size = 2 * 1024 * 1024; // 2 MiB
        let ptr = unsafe { mmap_huge_pages(size) };
        if !ptr.is_null() {
            // Clean up on success.
            unsafe {
                libc::munmap(ptr as *mut libc::c_void, size);
            }
        }
    }

    #[test]
    fn test_prefault_pages_standalone() {
        // Allocate a small region with regular mmap and prefault it.
        let size = 8192; // 2 pages
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        let ptr = ptr as *mut u8;

        unsafe {
            prefault_pages(ptr, size);
            // Verify we can read back the zeroed values.
            assert_eq!(core::ptr::read_volatile(ptr), 0u8);
            assert_eq!(core::ptr::read_volatile(ptr.add(4096)), 0u8);
            libc::munmap(ptr as *mut libc::c_void, size);
        }
    }

    #[test]
    fn test_mlock_pages_standalone() {
        let size = 4096;
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        let ptr = ptr as *mut u8;

        // mlock may fail without sufficient RLIMIT_MEMLOCK — just test the call.
        let _result = unsafe { mlock_pages(ptr, size) };

        unsafe {
            libc::munmap(ptr as *mut libc::c_void, size);
        }
    }

    #[test]
    fn test_set_numa_preferred() {
        // On non-NUMA systems the syscall may return false — that is fine.
        // On NUMA systems node 0 always exists. Either way, this must not
        // panic or crash.
        let result = set_numa_preferred(0);
        // Accept both outcomes: true (NUMA available) or false (not available).
        let _ = result;

        // Always clean up to avoid affecting other tests on this thread.
        reset_numa_policy();
    }

    #[test]
    fn test_reset_numa_policy() {
        // Resetting to default should always succeed, even on non-NUMA
        // systems, because MPOL_DEFAULT with a null nodemask is valid.
        assert!(reset_numa_policy());
    }
}
