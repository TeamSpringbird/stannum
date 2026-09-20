#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Render matched AWS measurements, using only the committed result JSON."""
import json
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.lines import Line2D

ROOT = Path(__file__).resolve().parent
DATA = json.loads((ROOT / 'aws-comparable-results.json').read_text())
SUMMARY = DATA['summary']
BG, INK, MUTED = '#f5f7fb', '#17243b', '#526078'
COLORS = {'stannum': '#008779', 'tin': '#c57524', 'gin': '#5375b5'}
LABELS = [('common', 'history'), ('and', 'history AND war'),
          ('or', 'history OR war'), ('selective_or', 'telescope OR astronomy'),
          ('rare', 'quasar')]
plt.rcParams.update({'font.family': 'DejaVu Sans', 'font.size': 11,
                     'svg.fonttype': 'none', 'svg.hashsalt': 'stannum-aws-20260920'})


def style(ax):
    ax.set_facecolor(BG)
    ax.set_axisbelow(True)
    ax.grid(axis='x', alpha=.18)
    for spine in ax.spines.values():
        spine.set_visible(False)
    ax.tick_params(length=0, pad=7)
    ax.set_xlabel('Server execution (ms) · lower is better', color=MUTED)


def heading(fig, title, subtitle):
    fig.set_facecolor(BG)
    fig.text(.055, .952, title, fontsize=23, weight='bold', color=INK)
    fig.text(.055, .914, subtitle, fontsize=11, color=MUTED)


def footer(fig, sampling):
    fig.text(.055, .087, sampling, fontsize=10, color=MUTED)
    fig.text(.055, .061, 'EC2: Graviton4, 2 vCPU / 16 GiB, EBS · PlanetScale: requested PS-160 ARM / EBS, exact CPU unverified.', fontsize=10, color=MUTED)
    fig.text(.055, .035, '2026-09-20 UTC · Stannum ab1e6db · TIN 1.0.2 · Source: docs/benchmarks/aws-comparable-results.json', fontsize=10, color=MUTED)


def save(fig, name):
    for suffix in ('png', 'svg'):
        fig.savefig(ROOT / f'{name}.{suffix}', dpi=180,
                    facecolor=fig.get_facecolor(), metadata={'Date': None} if suffix == 'svg' else {})
    p = ROOT / f'{name}.svg'
    p.write_text('\n'.join(line.rstrip() for line in p.read_text().splitlines()) + '\n')
    plt.close(fig)


# Ranked comparisons: identical scale and order across the three query groups.
fig, axes = plt.subplots(1, 3, figsize=(17, 8.5), sharex=True, sharey=True)
fig.subplots_adjust(left=.19, right=.97, bottom=.21, top=.765, wspace=.13)
heading(fig, 'Ranked top ten: where Stannum stands against TIN',
        '100,000 Wikipedia articles · warm buffers · matching query results and score bits · no OFFSET · network excluded')
handles = [Line2D([0], [0], color=COLORS[e], lw=9, label=label)
           for e, label in [('stannum', 'Stannum / EC2'), ('tin', 'TIN / PlanetScale')]]
fig.legend(handles=handles, loc='upper left', bbox_to_anchor=(.18, .895), ncol=2, frameon=False)
fig.text(.19, .838, 'Cross-host observations: substantial GIN controls were 1.3–1.6× faster on EC2. Small differences are not definitive engine wins.', fontsize=10, color=MUTED)
for ax, (cutoff, title) in zip(axes, [('None', 'No SQL filter'), ('25000', 'id <= 25,000'), ('1000', 'id <= 1,000')]):
    for engine, offset in [('stannum', -.17), ('tin', .17)]:
        for i, (key, _) in enumerate(LABELS):
            row = SUMMARY[engine][f'{engine}:rank:{key}:{cutoff}']
            value = row['median_ms']
            y = i + offset
            ax.barh(y, value, height=.28, color=COLORS[engine])
            rounds = list(row['round_median_ms'].values())
            # Range of round medians, deliberately not presented as a confidence interval.
            ax.plot([min(rounds), max(rounds)], [y, y], color=INK, lw=1.5, marker='|', markersize=7)
            ax.text(max(value, max(rounds)) + .13, y, f'{value:.3f}', va='center', fontsize=10, color=INK)
    ax.set_title(title, fontsize=14, color=INK, pad=16)
    ax.set_xlim(0, 11.4)
    ax.set_xticks([0, 3, 6, 9])
    style(ax)
axes[0].set_yticks(range(len(LABELS)), [x[1] for x in LABELS])
axes[0].invert_yaxis()
footer(fig, 'Medians of 60 retained samples per case across 3 rounds. Thin marks span round medians, not confidence intervals.')
save(fig, 'performance-snapshot')

# GIN controls stay on the same host as their respective search extension.
fig, axes = plt.subplots(2, 3, figsize=(17, 10))
fig.subplots_adjust(left=.14, right=.96, bottom=.21, top=.82, wspace=.55, hspace=.58)
heading(fig, 'Count queries: compare each engine with its local GIN control',
        '100,000 Wikipedia articles · warm buffers · exact matching document sets · server medians · each panel has its own scale')
series = [('stannum', 'stannum', 'Stannum / EC2'), ('stannum', 'gin', 'GIN / EC2'),
          ('tin', 'tin', 'TIN / PlanetScale'), ('tin', 'gin', 'GIN / PlanetScale')]
for ax, (key, title) in zip(axes.flat, LABELS + [('miss', 'Absent term')]):
    values = [SUMMARY[host][f'{engine}:count:{key}']['median_ms'] for host, engine, _ in series]
    bars = ax.barh(range(4), values, color=[COLORS[e] for _, e, _ in series], height=.6)
    bars[3].set_hatch('///')
    ax.bar_label(bars, labels=[f'{v:.3f}' for v in values], padding=5, fontsize=10, color=INK)
    ax.set_yticks(range(4), [label for _, _, label in series], fontsize=10)
    ax.invert_yaxis()
    ax.set_xlim(0, max(values) * 1.35)
    ax.set_title(title, fontsize=13, color=INK, pad=12)
    style(ax)
footer(fig, '60 retained samples per case. GIN is faster on EC2 too: these hosts are not performance-identical. Do not normalize by one ratio.')
save(fig, 'performance-counts')

# Same-host intervention explains the historical misleading ranking comparison.
fig, ax = plt.subplots(figsize=(15, 7.5))
fig.subplots_adjust(left=.24, right=.94, bottom=.27, top=.79)
heading(fig, 'OFFSET 0 explains the misleading historical comparison',
        'Same filtered history OR war query, id <= 25,000, top 10 · only the literal OFFSET 0 changes within each host')
for engine, offset in [('stannum', -.16), ('tin', .16)]:
    for i, form in enumerate(('no_offset', 'offset0')):
        row = DATA['offset_pairs'][engine][form]
        v = row['median_ms']
        ax.barh(i + offset, v, height=.27, color=COLORS[engine])
        ax.plot([row['min_ms'], row['max_ms']], [i + offset, i + offset], color=INK, lw=1.5, marker='|', markersize=7)
        ax.text(row['max_ms'] + .35, i + offset, f'{v:.2f} ms', va='center', color=INK)
ax.set_yticks([0, 1], ['LIMIT 10', 'LIMIT 10 OFFSET 0'])
ax.invert_yaxis()
ax.set_xlim(0, 35)
style(ax)
fig.legend(handles=handles, loc='upper left', bbox_to_anchor=(.23, .89), ncol=2, frameon=False)
fig.text(.055, .18, 'TIN: bounded top-k scan becomes a conjunction plus top-N sort. Equivalent returned IDs/scores; no correctness failure observed.', fontsize=11, color=INK)
footer(fig, '20 retained samples per form, alternating order. Thin marks show sample min–max. This is a paired plan sensitivity, not a general engine ranking.')
save(fig, 'performance-offset-zero')
