\* Copyright 2026 Photon Ring Contributors
\* SPDX-License-Identifier: Apache-2.0

---------------------------- MODULE MC ----------------------------
(*
 * Model-checking wrapper for the seqlock specification.
 *
 * This module extends the base spec and provides concrete constant
 * values for TLC. The MC.cfg file binds the constants and selects
 * the specification, invariants, and liveness properties.
 *
 * Default configuration:
 *   - 2 readers  (sufficient to expose concurrency bugs)
 *   - Ring of 2 slots (minimum valid ring; maximises wrap-around)
 *   - 4 sequence numbers (enough for 2 full wraps; triggers lag)
 *
 * Larger configurations (3 readers, ring=4, MaxSeq=8) are feasible
 * but increase state-space exploration time significantly.
 *
 * Usage:
 *   tlc MC -config MC.cfg -workers auto
 *)

EXTENDS seqlock

========================================================================
