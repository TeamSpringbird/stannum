#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Render the synthetic shape diagnostic; never mix in external TIN timings."""
import argparse
import json
from pathlib import Path

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('results', type=Path)
    parser.add_argument('output', type=Path, help='Output basename, without extension')
    args = parser.parse_args()
    report = json.loads(args.results.read_text())
    rows = {r['query']: r for r in report['cases']}
    fig, axes = plt.subplots(1, 2, figsize=(10, 4.5))
    for ax, mode in zip(axes, ['count', 'ranked']):
        for op, color in [('and', '#217a78'), ('or', '#c96936')]:
            widths = [2, 8, 32, 128]
            values = [rows[f'{op}_{n}_{mode}']['median_ms'] for n in widths]
            ax.plot(widths, values, marker='o', label=op.upper(), color=color)
        ax.set_xscale('log', base=2)
        ax.set_xticks(widths, labels=widths)
        ax.set_xlabel('Distinct terms')
        ax.set_ylabel('Server execution time (ms)')
        ax.set_ylim(bottom=0)
        ax.set_title('COUNT(*)' if mode == 'count' else 'Top 10 by score')
        ax.grid(alpha=.2)
        ax.legend()
    failures = sum(r['status'] != 'passed' for r in report['correctness'])
    fig.suptitle(f"{report['engine'].title()}: query width diagnostic", fontsize=15)
    fig.text(.5, .025, f"{report['rows']:,} synthetic documents · warm, single client · temporary tables\n"
             f"{failures} other cases failed and are excluded from timings. Not a TIN comparison or capacity test.",
             ha='center', fontsize=9)
    fig.tight_layout(rect=(0,.12,1,.93))
    args.output.parent.mkdir(parents=True,exist_ok=True)
    for extension in ['png','svg']:
        fig.savefig(args.output.with_suffix('.'+extension),dpi=160)


if __name__ == '__main__':
    main()
