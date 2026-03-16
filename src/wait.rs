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
}

impl Default for WaitStrategy {
    fn default() -> Self {
        WaitStrategy::Adaptive {
            spin_iters: 64,
            yield_iters: 64,
        }
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
