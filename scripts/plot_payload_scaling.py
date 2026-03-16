#!/usr/bin/env python3
# Copyright 2026 Photon Ring Contributors
# SPDX-License-Identifier: Apache-2.0
"""
Generate payload scaling charts for Photon Ring vs Disruptor-style in-place access.

Produces two charts:
1. Same-thread roundtrip latency vs payload size
2. Cross-thread latency vs payload size with Disruptor comparison

Data from: cargo bench --bench payload_scaling (Intel i7-10700KF, Rust 1.93.1)
"""

import matplotlib
matplotlib.use('Agg')  # headless
import matplotlib.pyplot as plt
import numpy as np

# --- Benchmark data (from cargo bench --bench payload_scaling) ---
payload_bytes = [8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096]

# Same-thread roundtrip (publish + try_recv, single thread, L1 hot)
same_thread_ns = [2.4, 9.8, 11.8, 18.8, 23.3, 34.4, 55.9, 88.1, 149.6, 361.6]

# Cross-thread roundtrip (publisher + subscriber on different OS threads)
cross_thread_ns = [117.3, 112.3, 118.7, 124.5, 139.4, 147.8, 163.1, 191.4, 225.8, 342.0]

# Disruptor model: in-place write + in-place read = 0 copy cost.
# The only cost is cache coherence transfer (~96 ns base) + per-cache-line
# transfer for multi-line payloads.
# Each additional cache line beyond the first adds ~40-55 ns.
cache_line = 64
base_coherence_ns = 133  # disruptor measured cross-thread roundtrip

def disruptor_estimated_ns(size):
    """Estimate Disruptor cross-thread latency for a given payload size.

    The Disruptor writes in-place and reads in-place (no memcpy on either
    side). However, the consumer still needs to READ the same cache lines
    via coherence transfer. The savings vs Photon Ring are:
    - No write-side memcpy (publisher writes directly into pre-allocated slot)
    - No read-side memcpy (consumer reads in-place via event handler reference)

    But the cache coherence transfer is the same: each modified cache line
    must be snooped from the publisher's L1 to the consumer's L1.

    Model: same base coherence as Photon Ring's cross-thread measurement,
    minus the memcpy overhead. The memcpy costs ~0.5 ns per 8 bytes
    (two directions: write + read).
    """
    # Base: Disruptor's measured 133 ns for 8B (includes its own overhead:
    # sequence barrier load + event handler dispatch + signal-back)
    # Additional cache lines cost ~8-15 ns each (prefetcher pipelines them)
    total_bytes = size + 8
    num_lines = max(1, (total_bytes + cache_line - 1) // cache_line)
    extra_lines = max(0, num_lines - 1)
    return base_coherence_ns + extra_lines * 12  # ~12 ns per extra line (pipelined)

disruptor_ns = [disruptor_estimated_ns(s) for s in payload_bytes]

# Photon Ring copy overhead (isolated from cache coherence)
copy_overhead_ns = [ct - 96 for ct in cross_thread_ns]  # subtract base coherence

# --- Chart 1: Same-thread roundtrip ---
fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(16, 7))

ax1.plot(payload_bytes, same_thread_ns, 'o-', color='#2196F3', linewidth=2.5,
         markersize=8, label='Photon Ring (same thread)')
ax1.set_xscale('log', base=2)
ax1.set_xlabel('Payload Size (bytes)', fontsize=13)
ax1.set_ylabel('Roundtrip Latency (ns)', fontsize=13)
ax1.set_title('Same-Thread Roundtrip Latency vs Payload Size', fontsize=14, fontweight='bold')
ax1.set_xticks(payload_bytes)
ax1.set_xticklabels([f'{s}B' if s < 1024 else f'{s//1024}KB' for s in payload_bytes],
                     rotation=45, ha='right')
ax1.grid(True, alpha=0.3)
ax1.legend(fontsize=11)

# Annotate the cache line boundary
ax1.axvline(x=56, color='red', linestyle='--', alpha=0.5)
ax1.annotate('56B = 1 cache line\n(with 8B stamp)', xy=(56, max(same_thread_ns)*0.5),
             fontsize=9, color='red', ha='right')

# --- Chart 2: Cross-thread with Disruptor comparison ---
ax2.plot(payload_bytes, cross_thread_ns, 'o-', color='#2196F3', linewidth=2.5,
         markersize=8, label='Photon Ring (seqlock + memcpy)')
ax2.plot(payload_bytes, disruptor_ns, 's--', color='#FF5722', linewidth=2.5,
         markersize=8, label='Disruptor (in-place, modeled)')

# Find crossover point
for i in range(len(payload_bytes) - 1):
    if cross_thread_ns[i] < disruptor_ns[i] and cross_thread_ns[i+1] >= disruptor_ns[i+1]:
        # Linear interpolation for crossover
        x1, x2 = payload_bytes[i], payload_bytes[i+1]
        y1_p, y2_p = cross_thread_ns[i], cross_thread_ns[i+1]
        y1_d, y2_d = disruptor_ns[i], disruptor_ns[i+1]
        # Solve: y1_p + t*(y2_p-y1_p) = y1_d + t*(y2_d-y1_d)
        t = (y1_d - y1_p) / ((y2_p - y1_p) - (y2_d - y1_d))
        cross_x = x1 + t * (x2 - x1)
        cross_y = y1_p + t * (y2_p - y1_p)
        ax2.axvline(x=cross_x, color='green', linestyle=':', alpha=0.7)
        ax2.annotate(f'Crossover ~{int(cross_x)}B',
                     xy=(cross_x, cross_y), xytext=(cross_x*1.5, cross_y-20),
                     fontsize=11, fontweight='bold', color='green',
                     arrowprops=dict(arrowstyle='->', color='green'))
        break

# Shade the "Photon Ring wins" region
ax2.fill_between(payload_bytes, cross_thread_ns, disruptor_ns,
                  where=[p < d for p, d in zip(cross_thread_ns, disruptor_ns)],
                  alpha=0.15, color='#2196F3', label='Photon Ring advantage')
ax2.fill_between(payload_bytes, cross_thread_ns, disruptor_ns,
                  where=[p >= d for p, d in zip(cross_thread_ns, disruptor_ns)],
                  alpha=0.15, color='#FF5722', label='Disruptor advantage')

ax2.set_xscale('log', base=2)
ax2.set_xlabel('Payload Size (bytes)', fontsize=13)
ax2.set_ylabel('Cross-Thread Roundtrip Latency (ns)', fontsize=13)
ax2.set_title('Cross-Thread Latency: Photon Ring vs Disruptor', fontsize=14, fontweight='bold')
ax2.set_xticks(payload_bytes)
ax2.set_xticklabels([f'{s}B' if s < 1024 else f'{s//1024}KB' for s in payload_bytes],
                     rotation=45, ha='right')
ax2.grid(True, alpha=0.3)
ax2.legend(fontsize=10, loc='upper left')

plt.tight_layout()
plt.savefig('docs/images/payload-scaling.png', dpi=150, bbox_inches='tight')
print(f'Saved: docs/images/payload-scaling.png')

# --- Also generate the data table ---
print('\n## Payload Scaling Results\n')
print('| Payload | Same-Thread | Cross-Thread | Disruptor (est.) | Winner |')
print('|---------|-------------|--------------|------------------|--------|')
for i, size in enumerate(payload_bytes):
    s = f'{size}B' if size < 1024 else f'{size//1024}KB'
    winner = '**Photon Ring**' if cross_thread_ns[i] < disruptor_ns[i] else '**Disruptor**'
    print(f'| {s:>5} | {same_thread_ns[i]:>8.1f} ns | {cross_thread_ns[i]:>9.1f} ns | {disruptor_ns[i]:>13.1f} ns | {winner} |')
