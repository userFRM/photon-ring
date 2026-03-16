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
}
