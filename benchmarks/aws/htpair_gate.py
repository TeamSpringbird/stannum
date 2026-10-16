# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""The paired-core keep gate over a hyperthread-paired replay run (benchmarks/aws/devbox-htpair-ab.sh).

    python3 benchmarks/aws/htpair_gate.py OUT --compare NAME:BASE,... [--target NAME=STYLE/STYLE,...]
        [--keep 1.5] [--style-floor -0.5]

Reads the same OUT/pinned-r<round>-<arm>.txt files as htpair_summary.py and,
for each NAME:BASE pair, prints per style the change in queries per
gigacycle of the paired core (2 threads) and of one thread alone (1), each
as the median of the per-round ratios (rounds are rotated, so a round's arm
and base ran close together) with the per-round range, and the change in
instructions per query. Then, over every compared pair and style, how far
the paired change follows the single-thread change and the instruction
change (least-squares slope and correlation), and each pair's verdict:

    KEEP   answers equal, no style's paired change below --style-floor %, and
           either the geomean over styles >= --keep % with every round's
           geomean > 0, or (a change aimed at some styles, --target) one
           target style >= +2.0% with every round >= +1.0% and the geomean
           >= +0.5%;
    NOISE  otherwise if |geomean| < --keep %;
    DROP   otherwise.
"""

import argparse
import math
import statistics as st

from htpair_summary import get, parse, styles_of


def ratio_rounds(runs, name, base, threads, style, key):
    rounds = sorted(set(runs.get(name, {})) & set(runs.get(base, {})))
    out = []
    for r in rounds:
        a, b = get(runs, base, r, threads, style, key), get(runs, name, r, threads, style, key)
        if a and b:
            out.append(b / a - 1)
    return out


def geomean(changes):
    return math.exp(st.mean(math.log1p(c) for c in changes)) - 1


def fit(xs, ys):
    mx, my = st.mean(xs), st.mean(ys)
    sxx = sum((x - mx) ** 2 for x in xs)
    sxy = sum((x - mx) * (y - my) for x, y in zip(xs, ys))
    syy = sum((y - my) ** 2 for y in ys)
    return sxy / sxx, sxy / math.sqrt(sxx * syy)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('out')
    ap.add_argument('--compare', required=True, help='NAME:BASE,...')
    ap.add_argument('--keep', type=float, default=1.5, help='geomean paired gain to keep, %% (default 1.5)')
    ap.add_argument('--style-floor', type=float, default=-0.5, help='worst style allowed, %% (default -0.5)')
    ap.add_argument('--target', default='', help='NAME=STYLE/STYLE,...: the styles a change aims at')
    args = ap.parse_args()
    targets = {k: v.split('/') for k, v in (t.split('=') for t in filter(None, args.target.split(',')))}
    runs = parse(args.out)
    styles = styles_of(runs)
    points, verdicts = [], []
    for pair in filter(None, args.compare.split(',')):
        name, base = pair.split(':')
        print(f'== {name} vs {base}: change per style, median of per-round ratios [round range]; + is better')
        print(f'  {"style":<18} {"pair q/Gcyc":>22} {"single q/Gcyc":>22} {"ins/q":>7} {"pair-single":>11}')
        pair_by_round, pair_med, same, aimed = [], [], True, False
        for style in styles:
            p = ratio_rounds(runs, name, base, 2, style, 'rate')
            s = ratio_rounds(runs, name, base, 1, style, 'rate')
            i = ratio_rounds(runs, name, base, 1, style, 'ins')
            if not p:
                continue
            same &= all(get(runs, name, r, t, style, 'digest') == get(runs, base, r, t, style, 'digest')
                        for r in set(runs[name]) & set(runs[base]) for t in (1, 2))
            pm, sm, im = st.median(p), st.median(s), st.median(i)
            pair_med.append(pm)
            pair_by_round.append(p)
            aimed |= style in targets.get(name, []) and pm >= 0.020 and min(p) >= 0.010
            points.append((pm, sm, im))
            print(f'  {style:<18} {100 * pm:+6.2f}% [{100 * min(p):+5.1f},{100 * max(p):+5.1f}] '
                  f'{100 * sm:+6.2f}% [{100 * min(s):+5.1f},{100 * max(s):+5.1f}] {100 * im:+6.2f}% '
                  f'{100 * (pm - sm):+10.2f}')
        g = geomean(pair_med)
        round_g = [geomean(list(col)) for col in zip(*pair_by_round)]
        sg = geomean([pt[1] for pt in points[-len(pair_med):]])
        worst = min(pair_med)
        broad = g * 100 >= args.keep and min(round_g) > 0
        if same and worst * 100 >= args.style_floor and (broad or (aimed and g >= 0.005)):
            verdict = 'KEEP' if broad else 'KEEP (target)'
        elif abs(g * 100) < args.keep:
            verdict = 'NOISE'
        else:
            verdict = 'DROP'
        verdicts.append((name, base, g, sg, worst, round_g, verdict))
        print(f'  {"geomean":<18} {100 * g:+6.2f}%  rounds {" ".join(f"{100 * x:+.1f}" for x in round_g)}  '
              f'single {100 * sg:+.2f}%  answers {"same" if same else "DIFFERENT"}\n')
    if len(points) > 2:
        pm, sm, im = zip(*points)
        b1, r1 = fit(sm, pm)
        b2, r2 = fit([-x for x in im], pm)
        print(f'== paired change vs single-thread change: slope {b1:.2f}, r {r1:.2f}; '
              f'vs instruction cut: slope {b2:.2f}, r {r2:.2f} ({len(points)} pair-styles)')
        print(f'   mean paired - single: {100 * st.mean(p - s for p, s, _ in points):+.2f} pt\n')
    print(f'== verdicts (keep: every style >= {args.style_floor}% and geomean >= +{args.keep}% every round > 0, '
          f'or a target style >= +2.0% every round >= +1.0% with geomean >= +0.5%)')
    for name, base, g, sg, worst, round_g, verdict in verdicts:
        print(f'  {name:>6} vs {base:<6} paired {100 * g:+6.2f}%  single {100 * sg:+6.2f}%  '
              f'worst style {100 * worst:+6.2f}%  {verdict}')


if __name__ == '__main__':
    main()
