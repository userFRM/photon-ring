// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: Apache-2.0

//! Wait strategies for blocking receive operations.
//!
//! [`WaitStrategy`] controls how a consumer thread waits when no message is
//! available. All strategies are `no_std` compatible.
//!
//! | Strategy | Latency | CPU usage | Best for |
//! |---|---|---|---|
//! | `BusySpin` | Lowest (~0 ns wakeup) | 100% core | Dedicated, pinned cores |
//! | `YieldSpin` | Low (~30 ns on x86) | High | Shared cores, SMT |
//! | `BackoffSpin` | Medium (exponential) | Decreasing | Background consumers |
//! | `Adaptive` | Auto-scaling | Varies | General purpose |
//!
//! # Platform-specific optimizations
//!
//! On **aarch64**, `YieldSpin` and `BackoffSpin` use the `WFE` (Wait For
//! Event) instruction instead of `core::hint::spin_loop()` (which maps to
//! `YIELD`). `WFE` puts the core into a low-power state until an event —
//! such as a cache line invalidation from the publisher's store — wakes it.
//! The `SEVL` + `WFE` pattern is used: `SEVL` sets the local event register
//! so the first `WFE` doesn't block unconditionally.
//!
//! On **x86/x86_64**, `core::hint::spin_loop()` emits `PAUSE`, which is the
//! standard spin-wait hint (~140 cycles on Skylake+).
//!
// NOTE: Intel Tremont+ CPUs support UMWAIT/TPAUSE instructions for
// user-mode cache line monitoring. These would allow near-zero latency
// wakeup without burning CPU. Not yet implemented — requires CPUID
// feature detection (WAITPKG) and is only available on recent Intel.

/// Strategy for blocking `recv()` and `SubscriberGroup::recv()`.
///
/// All variants are `no_std` compatible — no OS thread primitives required.
///
/// | Strategy | Latency | CPU usage | Best for |
/// |---|---|---|---|
/// | `BusySpin` | Lowest (~0 ns wakeup) | 100% core | Dedicated, pinned cores |
/// | `YieldSpin` | Low (~30 ns on x86) | High | Shared cores, SMT |
/// | `BackoffSpin` | Medium (exponential) | Decreasing | Background consumers |
/// | `Adaptive` | Auto-scaling | Varies | General purpose |
/// | `MonitorWait` | Near-zero (~30 ns on Intel) | Near-zero | Intel Alder Lake+ |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStrategy {
    /// Pure busy-spin with no PAUSE instruction. Minimum wakeup latency
    /// but consumes 100% of one CPU core. Use on dedicated, pinned cores.
    BusySpin,

    /// Spin with `core::hint::spin_loop()` (PAUSE on x86, YIELD on ARM)
    /// between iterations. Yields the CPU pipeline to the SMT sibling
    /// and reduces power consumption vs `BusySpin`.
    YieldSpin,

    /// Exponential backoff spin. Starts with bare spins, then escalates
    /// to PAUSE-based spins with increasing delays. Good for consumers
    /// that may be idle for extended periods without burning a full core.
    BackoffSpin,

    /// Three-phase escalation: bare spin for `spin_iters` iterations,
    /// then PAUSE-spin for `yield_iters`, then repeated PAUSE bursts.
    Adaptive {
        /// Number of bare-spin iterations before escalating to PAUSE.
        spin_iters: u32,
        /// Number of PAUSE iterations before entering deep backoff.
        yield_iters: u32,
    },

    /// UMONITOR/UMWAIT on Intel (Tremont+, Alder Lake+) or WFE on ARM.
    ///
    /// On x86_64 with WAITPKG support: `UMONITOR` sets up a monitored
    /// address range, `UMWAIT` puts the core into an optimized C0.1/C0.2
    /// state until a write to the monitored cache line wakes it. Near-zero
    /// power consumption with ~30 ns wakeup latency.
    ///
    /// Falls back to `YieldSpin` on x86 CPUs without WAITPKG support.
    /// On aarch64: uses SEVL+WFE (identical to `YieldSpin`).
    ///
    /// `addr` must point to the stamp field of the slot being waited on.
    /// Use [`WaitStrategy::monitor_wait`] to construct safely from an
    /// `AtomicU64` reference. For callers that cannot provide an address,
    /// use [`MonitorWaitFallback`](WaitStrategy::MonitorWaitFallback).
    ///
    /// # Safety (of direct construction)
    ///
    /// The pointer must remain valid for the duration of the wait.
    /// Constructing this variant directly with a pointer to stack memory
    /// or memory that may be deallocated is undefined behavior. Always
    /// prefer the safe constructor [`WaitStrategy::monitor_wait()`].
    MonitorWait {
        /// Pointer to the memory location to monitor. Must remain valid
        /// for the duration of the wait.
        addr: *const u8,
    },

    /// Like `MonitorWait` but without an explicit address.
    ///
    /// On x86_64 with WAITPKG: falls back to `TPAUSE` (timed wait in
    /// C0.1 state) for low-power waiting without address monitoring.
    /// On aarch64: SEVL+WFE. On other x86: PAUSE.
    MonitorWaitFallback,
}

impl WaitStrategy {
    /// Safely construct a [`MonitorWait`](WaitStrategy::MonitorWait) from
    /// an `AtomicU64` reference (typically a slot's stamp field).
    #[inline]
    pub fn monitor_wait(stamp: &core::sync::atomic::AtomicU64) -> Self {
        WaitStrategy::MonitorWait {
            addr: stamp as *const core::sync::atomic::AtomicU64 as *const u8,
        }
    }
}

impl Default for WaitStrategy {
    fn default() -> Self {
        WaitStrategy::Adaptive {
            spin_iters: 64,
            yield_iters: 64,
        }
    }
}

/// Check at runtime whether the CPU supports WAITPKG (UMONITOR/UMWAIT/TPAUSE).
///
/// CPUID leaf 7, sub-leaf 0, ECX bit 5.
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[inline]
fn has_waitpkg() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        let result = core::arch::x86_64::__cpuid_count(7, 0);
        result.ecx & (1 << 5) != 0
    }
    #[cfg(target_arch = "x86")]
    {
        let result = core::arch::x86::__cpuid_count(7, 0);
        result.ecx & (1 << 5) != 0
    }
}

/// Cached WAITPKG support flag. Evaluated once via a racy init pattern
/// (benign data race — worst case is redundant CPUID calls on first access).
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
static WAITPKG_SUPPORT: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// 0 = unknown, 1 = not supported, 2 = supported.
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[inline]
fn waitpkg_supported() -> bool {
    let cached = WAITPKG_SUPPORT.load(core::sync::atomic::Ordering::Relaxed);
    if cached != 0 {
        return cached == 2;
    }
    let supported = has_waitpkg();
    WAITPKG_SUPPORT.store(
        if supported { 2 } else { 1 },
        core::sync::atomic::Ordering::Relaxed,
    );
    supported
}

// SAFETY wrappers for UMONITOR/UMWAIT/TPAUSE instructions.
// These are encoded via raw bytes because stable Rust doesn't expose them
// as intrinsics yet.
//
// UMONITOR: sets up address monitoring (F3 0F AE /6)
// UMWAIT:   wait until store to monitored line or timeout (F2 0F AE /6)
// TPAUSE:   timed pause without address monitoring (66 0F AE /6)
//
// EDX:EAX = absolute TSC deadline. The instruction exits when either:
//   (a) a store hits the monitored cache line (UMWAIT only), or
//   (b) TSC >= deadline, or
//   (c) an OS-configured timeout (IA32_UMWAIT_CONTROL MSR) fires.
//
// We set the deadline ~100µs in the future — long enough to actually
// enter a low-power state, short enough to bound worst-case latency
// if the wakeup event is missed (e.g., the store happened between
// UMONITOR and UMWAIT).
#[cfg(target_arch = "x86_64")]
mod umwait {
    /// Read the TSC and return a deadline ~100µs in the future.
    /// On a 3 GHz CPU, 100µs ≈ 300,000 cycles.
    ///
    /// Note: The 300,000 cycle offset assumes ~3 GHz TSC frequency. On slower
    /// CPUs (1 GHz), this becomes ~300 µs; on faster CPUs (5 GHz), ~60 µs.
    /// The deadline is a safety bound, not a precision target.
    #[inline(always)]
    fn deadline_100us() -> (u32, u32) {
        // Use _rdtsc() directly. The previous version had a dead inline-asm
        // `rdtsc` block whose eax/edx outputs were discarded (bound to `_`),
        // immediately followed by a second _rdtsc() call for the actual value.
        // That wasted ~20–25 cycles per UMWAIT/TPAUSE call. Removed.
        let tsc = unsafe { core::arch::x86_64::_rdtsc() };
        let deadline = tsc.wrapping_add(300_000); // ~100µs at 3 GHz
        (deadline as u32, (deadline >> 32) as u32) // (eax, edx)
    }

    /// Set up monitoring on the cache line containing `addr`.
    /// The CPU will track writes to this line until UMWAIT is called.
    #[inline(always)]
    pub(super) unsafe fn umonitor(addr: *const u8) {
        // UMONITOR rax: F3 0F AE /6 (with rax)
        core::arch::asm!(
            ".byte 0xf3, 0x0f, 0xae, 0xf0", // UMONITOR rax
            in("rax") addr,
            options(nostack, preserves_flags),
        );
    }

    /// Wait for a write to the monitored address or timeout.
    /// `ctrl` = 0 for C0.2 (deeper sleep), 1 for C0.1 (lighter sleep).
    /// Returns quickly (~30 ns) when the monitored cache line is written.
    /// Deadline is set ~100µs in the future as a safety bound.
    #[inline(always)]
    pub(super) unsafe fn umwait(ctrl: u32) {
        let (lo, hi) = deadline_100us();
        // UMWAIT ecx: F2 0F AE /6 (with ecx for control)
        // edx:eax = absolute TSC deadline
        core::arch::asm!(
            ".byte 0xf2, 0x0f, 0xae, 0xf1", // UMWAIT ecx
            in("ecx") ctrl,
            in("edx") hi,
            in("eax") lo,
            options(nostack, preserves_flags),
        );
    }

    /// Timed pause without address monitoring. Enters C0.1 state
    /// until the deadline (~100µs from now).
    /// `ctrl` = 0 for C0.2, 1 for C0.1.
    #[inline(always)]
    pub(super) unsafe fn tpause(ctrl: u32) {
        let (lo, hi) = deadline_100us();
        // TPAUSE ecx: 66 0F AE /6 (with ecx for control)
        core::arch::asm!(
            ".byte 0x66, 0x0f, 0xae, 0xf1", // TPAUSE ecx
            in("ecx") ctrl,
            in("edx") hi,
            in("eax") lo,
            options(nostack, preserves_flags),
        );
    }
}

// SAFETY: MonitorWait stores a raw pointer for the monitored address.
// The pointer is only used during wait() and must remain valid for
// the duration of the wait. Since WaitStrategy is passed by value to
// recv_with() and wait() is called synchronously, this is safe as long
// as the caller ensures the pointed-to memory outlives the wait.
unsafe impl Send for WaitStrategy {}
unsafe impl Sync for WaitStrategy {}

impl WaitStrategy {
    /// Execute one wait iteration. Called by `recv_with` on each loop when
    /// `try_recv` returns `Empty`.
    ///
    /// `iter` is the zero-based iteration count since the last successful
    /// receive — it drives phase transitions in `Adaptive` and `BackoffSpin`.
    #[inline]
    pub(crate) fn wait(&self, iter: u32) {
        match self {
            WaitStrategy::BusySpin => {
                // No hint — pure busy loop. Fastest wakeup, highest power.
            }
            WaitStrategy::YieldSpin => {
                // On aarch64: SEVL + WFE puts the core into a low-power
                // state until a cache-line event wakes it. SEVL sets the
                // local event register so the first WFE returns immediately
                // (avoids unconditional blocking).
                // On x86: PAUSE yields the pipeline to the SMT sibling.
                #[cfg(target_arch = "aarch64")]
                unsafe {
                    core::arch::asm!("sevl", options(nomem, nostack));
                    core::arch::asm!("wfe", options(nomem, nostack));
                }
                #[cfg(not(target_arch = "aarch64"))]
                core::hint::spin_loop();
            }
            WaitStrategy::BackoffSpin => {
                // Exponential backoff: more iterations as we wait longer.
                // On aarch64: WFE sleeps until a cache-line event, making
                // each iteration near-zero power. On x86: PAUSE yields the
                // pipeline with ~140 cycle delay per iteration.
                let pauses = 1u32.wrapping_shl(iter.min(6)); // 1, 2, 4, 8, 16, 32, 64
                for _ in 0..pauses {
                    #[cfg(target_arch = "aarch64")]
                    unsafe {
                        core::arch::asm!("wfe", options(nomem, nostack));
                    }
                    #[cfg(not(target_arch = "aarch64"))]
                    core::hint::spin_loop();
                }
            }
            WaitStrategy::Adaptive {
                spin_iters,
                yield_iters,
            } => {
                if iter < *spin_iters {
                    // Phase 1: bare spin — fastest wakeup.
                } else if iter < spin_iters + yield_iters {
                    // Phase 2: PAUSE-spin — yields pipeline.
                    core::hint::spin_loop();
                } else {
                    // Phase 3: deep backoff — multiple PAUSE per iteration.
                    for _ in 0..8 {
                        core::hint::spin_loop();
                    }
                }
            }
            WaitStrategy::MonitorWait { addr } => {
                // On x86_64 with WAITPKG: UMONITOR + UMWAIT for near-zero
                // power wakeup on cache-line write (~30 ns wakeup latency).
                // Falls back to PAUSE on CPUs without WAITPKG.
                #[cfg(target_arch = "x86_64")]
                {
                    if waitpkg_supported() {
                        unsafe {
                            umwait::umonitor(*addr);
                            umwait::umwait(1); // C0.1 — lighter sleep, faster wakeup
                        }
                    } else {
                        core::hint::spin_loop();
                    }
                }
                // On aarch64: SEVL + WFE (same as YieldSpin — WFE already
                // monitors cache-line invalidation events).
                #[cfg(target_arch = "aarch64")]
                {
                    let _ = addr;
                    unsafe {
                        core::arch::asm!("sevl", options(nomem, nostack));
                        core::arch::asm!("wfe", options(nomem, nostack));
                    }
                }
                #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
                {
                    let _ = addr;
                    core::hint::spin_loop();
                }
            }
            WaitStrategy::MonitorWaitFallback => {
                // On x86_64 with WAITPKG: TPAUSE enters C0.1 without
                // address monitoring — still saves power vs PAUSE.
                // On aarch64: SEVL + WFE.
                // Elsewhere: PAUSE.
                #[cfg(target_arch = "x86_64")]
                {
                    if waitpkg_supported() {
                        unsafe {
                            umwait::tpause(1); // C0.1
                        }
                    } else {
                        core::hint::spin_loop();
                    }
                }
                #[cfg(target_arch = "aarch64")]
                unsafe {
                    core::arch::asm!("sevl", options(nomem, nostack));
                    core::arch::asm!("wfe", options(nomem, nostack));
                }
                #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
                core::hint::spin_loop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_adaptive() {
        let ws = WaitStrategy::default();
        assert_eq!(
            ws,
            WaitStrategy::Adaptive {
                spin_iters: 64,
                yield_iters: 64,
            }
        );
    }

    #[test]
    fn busy_spin_returns_immediately() {
        let ws = WaitStrategy::BusySpin;
        for i in 0..1000 {
            ws.wait(i);
        }
    }

    #[test]
    fn yield_spin_returns() {
        let ws = WaitStrategy::YieldSpin;
        for i in 0..100 {
            ws.wait(i);
        }
    }

    #[test]
    fn backoff_spin_returns() {
        let ws = WaitStrategy::BackoffSpin;
        for i in 0..20 {
            ws.wait(i);
        }
    }

    #[test]
    fn adaptive_phases() {
        let ws = WaitStrategy::Adaptive {
            spin_iters: 4,
            yield_iters: 4,
        };
        for i in 0..20 {
            ws.wait(i);
        }
    }

    #[test]
    fn clone_and_copy() {
        let ws = WaitStrategy::BusySpin;
        let ws2 = ws;
        #[allow(clippy::clone_on_copy)]
        let ws3 = ws.clone();
        assert_eq!(ws, ws2);
        assert_eq!(ws, ws3);
    }

    #[test]
    fn debug_format() {
        use alloc::format;
        let ws = WaitStrategy::BusySpin;
        let s = format!("{ws:?}");
        assert!(s.contains("BusySpin"));
    }
}
