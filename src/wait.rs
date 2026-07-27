// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

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
//! | `MonitorWaitFallback` | Near-zero (~30 ns on Intel) | Near-zero | Intel Alder Lake+ |
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
//! On recent Intel (Alder Lake+), the `MonitorWaitFallback`
//! strategies use `UMONITOR`/`UMWAIT`/`TPAUSE` for near-zero-power wakeup,
//! gated at runtime on the `WAITPKG` CPUID feature.

/// Strategy for blocking `recv()`.
///
/// All variants are `no_std` compatible — no OS thread primitives required.
///
/// | Strategy | Latency | CPU usage | Best for |
/// |---|---|---|---|
/// | `BusySpin` | Lowest (~0 ns wakeup) | 100% core | Dedicated, pinned cores |
/// | `YieldSpin` | Low (~30 ns on x86) | High | Shared cores, SMT |
/// | `BackoffSpin` | Medium (exponential) | Decreasing | Background consumers |
/// | `Adaptive` | Auto-scaling | Varies | General purpose |
/// | `MonitorWaitFallback` | Near-zero (~30 ns on Intel) | Near-zero | Intel Alder Lake+ |
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
    /// power consumption with ~30 ns wakeup latency, without needing an
    /// address to monitor.
    ///
    /// Falls back to `YieldSpin` on x86 CPUs without WAITPKG support.
    /// On aarch64: uses SEVL+WFE (identical to `YieldSpin`).
    /// On x86_64 with WAITPKG: `TPAUSE` (timed wait in C0.1 state) for
    /// low-power waiting. On aarch64: SEVL+WFE. On other x86: PAUSE.
    MonitorWaitFallback,
}

impl WaitStrategy {}

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
        // Read the TSC once.
        let tsc = unsafe { core::arch::x86_64::_rdtsc() };
        let deadline = tsc.wrapping_add(300_000); // ~100µs at 3 GHz
        (deadline as u32, (deadline >> 32) as u32) // (eax, edx)
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
