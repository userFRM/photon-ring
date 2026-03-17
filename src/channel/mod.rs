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

/// Prefetch the next slot's cache line with write intent.
///
/// On **x86/x86_64**: emits `PREFETCHT0` — universally supported (SSE).
/// Brings the line into L1, hiding the RFO stall on the next publish.
///
/// On **aarch64**: emits `PRFM PSTL1KEEP` — prefetch for store, L1, temporal.
///
/// On other platforms: no-op.
#[inline(always)]
fn prefetch_write_next<T>(slots_ptr: *const Slot<T>, next_idx: u64) {
    let ptr = unsafe { slots_ptr.add(next_idx as usize) as *const u8 };
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_mm_prefetch(ptr as *const i8, core::arch::x86_64::_MM_HINT_T0);
    }
    #[cfg(target_arch = "x86")]
    unsafe {
        core::arch::x86::_mm_prefetch(ptr as *const i8, core::arch::x86::_MM_HINT_T0);
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("prfm pstl1keep, [{ptr}]", ptr = in(reg) ptr, options(nostack, preserves_flags));
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64")))]
    {
        let _ = ptr;
    }
}
