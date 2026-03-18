// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

mod constructors;
mod errors;
mod group;
mod mp_publisher;
mod publisher;
mod subscribable;
mod subscriber;

pub use constructors::{channel, channel_bounded, channel_mpmc};
pub use errors::{PublishError, TryRecvError};
pub use group::SubscriberGroup;
pub use mp_publisher::MpPublisher;
pub use publisher::Publisher;
pub use subscribable::Subscribable;
pub use subscriber::{Drain, Subscriber};

use crate::slot::Slot;

/// Prefetch the next slot's cache line(s) with **write intent**.
///
/// Covers all cache lines occupied by `Slot<T>`. For `T ≤ 56` bytes the slot
/// fits in a single 64-byte line; larger types automatically get multi-line
/// prefetches (the loop bound is a compile-time constant, so LLVM fully
/// unrolls it — zero overhead for the common single-line case).
///
/// On **x86/x86_64** with the `prfchw` target feature (Intel Broadwell+ / all
/// AMD x86-64): emits `PREFETCHW`, which brings the cache line into
/// **Exclusive** state, avoiding the RFO (Read For Ownership) stall that
/// `PREFETCHT0` causes when a subscriber has the line cached in Shared state.
/// Without `prfchw` (generic x86-64 builds): emits `PREFETCHT0` as a
/// universal fallback. Compile with `-C target-cpu=native` to enable
/// `PREFETCHW` on supported hardware.
///
/// On **aarch64**: emits `PRFM PSTL1KEEP` — write-intent prefetch to L1.
///
/// On other platforms: no-op.
#[inline(always)]
fn prefetch_write_next<T>(slots_ptr: *const Slot<T>, next_idx: u64) {
    let ptr = unsafe { slots_ptr.add(next_idx as usize) as *const u8 };

    #[cfg(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64"))]
    let lines = core::mem::size_of::<Slot<T>>().div_ceil(64);

    #[cfg(target_arch = "x86_64")]
    for i in 0..lines {
        let line_ptr = unsafe { ptr.add(i * 64) };
        #[cfg(target_feature = "prfchw")]
        unsafe {
            core::arch::asm!(
                "prefetchw [{ptr}]",
                ptr = in(reg) line_ptr,
                options(nostack, preserves_flags),
            );
        }
        #[cfg(not(target_feature = "prfchw"))]
        unsafe {
            core::arch::x86_64::_mm_prefetch(
                line_ptr as *const i8,
                core::arch::x86_64::_MM_HINT_T0,
            );
        }
    }

    #[cfg(target_arch = "x86")]
    for i in 0..lines {
        let line_ptr = unsafe { ptr.add(i * 64) };
        #[cfg(target_feature = "prfchw")]
        unsafe {
            core::arch::asm!(
                "prefetchw [{ptr}]",
                ptr = in(reg) line_ptr,
                options(nostack, preserves_flags),
            );
        }
        #[cfg(not(target_feature = "prfchw"))]
        unsafe {
            core::arch::x86::_mm_prefetch(line_ptr as *const i8, core::arch::x86::_MM_HINT_T0);
        }
    }

    #[cfg(target_arch = "aarch64")]
    for i in 0..lines {
        let line_ptr = unsafe { ptr.add(i * 64) };
        unsafe {
            core::arch::asm!(
                "prfm pstl1keep, [{ptr}]",
                ptr = in(reg) line_ptr,
                options(nostack, preserves_flags),
            );
        }
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64")))]
    {
        let _ = ptr;
    }
}
