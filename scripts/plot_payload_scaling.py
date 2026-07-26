#!/usr/bin/env python3
# Copyright 2026 Photon Ring Contributors
# SPDX-License-Identifier: MIT OR Apache-2.0
"""
Generate the payload-scaling chart for Photon Ring.

Produces one figure with two panels, both from measured data:
1. Same-thread roundtrip latency vs payload size
2. Cross-thread roundtrip latency vs payload size

Data from: cargo bench --bench payload_scaling (Intel i7-10700KF, Rust 1.93.1).
Only measured Photon Ring numbers are plotted — no modeled competitor curves.
"""

import matplotlib
matplotlib.use('Agg')  # headless
import matplotlib.pyplot as plt

# --- Benchmark data (from cargo bench --bench payload_scaling) ---
payload_bytes = [8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096]

# Same-thread roundtrip (publish + try_recv, single thread, L1 hot)
same_thread_ns = [2.4, 9.8, 11.8, 18.8, 23.3, 34.4, 55.9, 88.1, 149.6, 361.6]

# Cross-thread roundtrip (publisher + subscriber on different OS threads)
cross_thread_ns = [117.3, 112.3, 118.7, 124.5, 139.4, 147.8, 163.1, 191.4, 225.8, 342.0]

PHOTON_BLUE = '#2196F3'


def _style_axis(ax, ylabel, title):
    ax.set_xscale('log', base=2)
    ax.set_xlabel('Payload Size (bytes)', fontsize=13)
    ax.set_ylabel(ylabel, fontsize=13)
    ax.set_title(title, fontsize=14, fontweight='bold')
    ax.set_xticks(payload_bytes)
    ax.set_xticklabels([f'{s}B' if s < 1024 else f'{s // 1024}KB' for s in payload_bytes],
                       rotation=45, ha='right')
    ax.grid(True, alpha=0.3)
    ax.legend(fontsize=11)


fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(16, 7))

# --- Panel 1: Same-thread roundtrip ---
ax1.plot(payload_bytes, same_thread_ns, 'o-', color=PHOTON_BLUE, linewidth=2.5,
         markersize=8, label='Photon Ring (same thread)')
_style_axis(ax1, 'Roundtrip Latency (ns)', 'Same-Thread Roundtrip Latency vs Payload Size')
ax1.axvline(x=56, color='red', linestyle='--', alpha=0.5)
ax1.annotate('56B = 1 cache line\n(with 8B stamp)', xy=(56, max(same_thread_ns) * 0.5),
             fontsize=9, color='red', ha='right')

# --- Panel 2: Cross-thread roundtrip ---
ax2.plot(payload_bytes, cross_thread_ns, 'o-', color=PHOTON_BLUE, linewidth=2.5,
         markersize=8, label='Photon Ring (cross thread)')
_style_axis(ax2, 'Cross-Thread Roundtrip Latency (ns)', 'Cross-Thread Roundtrip Latency vs Payload Size')

plt.tight_layout()
plt.savefig('docs/images/payload-scaling.png', dpi=150, bbox_inches='tight')
print('Saved: docs/images/payload-scaling.png')

# --- Also print the data table ---
print('\n## Payload Scaling Results\n')
print('| Payload | Same-Thread | Cross-Thread |')
print('|---------|-------------|--------------|')
for i, size in enumerate(payload_bytes):
    s = f'{size}B' if size < 1024 else f'{size // 1024}KB'
    print(f'| {s:>5} | {same_thread_ns[i]:>8.1f} ns | {cross_thread_ns[i]:>9.1f} ns |')
