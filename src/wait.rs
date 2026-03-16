// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Wait strategies for blocking receive operations.
//!
//! [`WaitStrategy`] controls how a consumer thread waits when no message is
//! available. Choose based on your latency vs CPU usage requirements:
//!
//! | Strategy | Latency | CPU usage | Best for |
//! |---|---|---|---|
//! | `BusySpin` | Lowest (~0 ns wakeup) | 100% core | HFT, dedicated cores |
//! | `YieldSpin` | Low (~1-5 us wakeup) | High | Shared cores, SMT |
//! | `Park` | Medium (~10-50 us wakeup) | Near zero | Background consumers |
//! | `Adaptive` | Auto-scaling | Varies | General purpose |

/// Strategy for blocking `recv()` and `SubscriberGroup::recv()`.
///
/// Controls how the consumer thread waits when no message is available.
/// Choose based on your latency vs CPU usage requirements:
///
/// | Strategy | Latency | CPU usage | Best for |
/// |---|---|---|---|
/// | `BusySpin` | Lowest (~0 ns wakeup) | 100% core | HFT, dedicated cores |
/// | `YieldSpin` | Low (~1-5 us wakeup) | High | Shared cores, SMT |
/// | `Park` | Medium (~10-50 us wakeup) | Near zero | Background consumers |
/// | `Adaptive` | Auto-scaling | Varies | General purpose |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStrategy {
    /// Pure busy-spin with no PAUSE instruction. Minimum wakeup latency
    /// but consumes 100% of one CPU core. Use on dedicated, pinned cores.
    BusySpin,

    /// Spin with `thread::yield_now()` between iterations. Yields the
    /// OS time slice to other threads on the same core. Good for SMT.
    #[cfg(feature = "std")]
    YieldSpin,

    /// `thread::park()` / `unpark()` based waiting. Near-zero CPU usage
    /// when idle but higher wakeup latency (~10-50 us depending on OS).
    #[cfg(feature = "std")]
    Park,

    /// Three-phase escalation: busy-spin for `spin_iters` iterations,
    /// then yield for `yield_iters`, then park. Balances latency and CPU.
    ///
    /// On `no_std` (without the `std` feature), the yield and park phases
    /// fall back to `core::hint::spin_loop()`.
    Adaptive {
        /// Number of bare-spin iterations before escalating.
        spin_iters: u32,
        /// Number of yield iterations before parking (or PAUSE-spinning on `no_std`).
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
    /// receive — it drives the phase transitions in `Adaptive`.
    #[inline]
    pub(crate) fn wait(&self, iter: u32) {
        match self {
            WaitStrategy::BusySpin => {
                // No hint, no yield — pure busy loop.
            }
            #[cfg(feature = "std")]
            WaitStrategy::YieldSpin => {
                std::thread::yield_now();
            }
            #[cfg(feature = "std")]
            WaitStrategy::Park => {
                std::thread::park();
            }
            WaitStrategy::Adaptive {
                spin_iters,
                yield_iters,
            } => {
                if iter < *spin_iters {
                    // Phase 1: bare spin
                } else if iter < spin_iters + yield_iters {
                    // Phase 2: yield (or spin_loop on no_std)
                    #[cfg(feature = "std")]
                    {
                        std::thread::yield_now();
                    }
                    #[cfg(not(feature = "std"))]
                    {
                        core::hint::spin_loop();
                    }
                } else {
                    // Phase 3: park (or spin_loop on no_std)
                    #[cfg(feature = "std")]
                    {
                        std::thread::park();
                    }
                    #[cfg(not(feature = "std"))]
                    {
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
        // Should not block — just verify it completes.
        for i in 0..1000 {
            ws.wait(i);
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn yield_spin_returns() {
        let ws = WaitStrategy::YieldSpin;
        for i in 0..100 {
            ws.wait(i);
        }
    }

    #[test]
    fn adaptive_phases() {
        let ws = WaitStrategy::Adaptive {
            spin_iters: 4,
            yield_iters: 4,
        };
        // Phase 1 (spin): iters 0..4
        for i in 0..4 {
            ws.wait(i);
        }
        // Phase 2 (yield): iters 4..8
        for i in 4..8 {
            ws.wait(i);
        }
        // Phase 3 (park/spin_loop): iter 8+
        // On std this would park, but since no one unparks we only test
        // non-park path here. The park path is tested via recv_with integration.
        #[cfg(not(feature = "std"))]
        {
            ws.wait(8);
            ws.wait(100);
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
