# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Summarizes a hyperthread-paired replay run (benchmarks/aws/devbox-htpair-ab.sh).

    python3 benchmarks/aws/htpair_summary.py OUT [--compare NAME:BASE,...]

Reads OUT/pinned-r<round>-<arm>.txt (tnsreplay --cpus lines) and prints:

- per arm and style, medians over rounds: single-thread cycles per query,
  the paired threads' cycles per query and their ratio to the single
  thread's (and the instructions' ratio), the single thread's IPC and the
  paired core's, each set's queries per gigacycle (an eight-thread set's
  too, if run) and what the sibling adds to the core;
- per NAME:BASE pair and style: the paired core's throughput change (its
  queries per gigacycle, median over rounds, and each round's), the paired
  threads' cycles per query, the single thread's cycles, instructions and
  branch misses per query, and whether the answers' digests agree in every
  round;
- per NAME:BASE pair and style, the tail: the mean cycles per query of the
  --tail (25) forms slowest in BASE (by their median over rounds, paired),
  each arm's against BASE's, beside the mean over every form, paired and
  single (median of the per-round changes; from tnsreplay's `pinq` lines).

A run without paired sets (`--cpus C` only, as on macOS, where the counts
are the process's) is read as its single thread in the paired columns.
"""

import argparse
import collections
import glob
import math
import os
import re
import statistics as st

ORDER = ['conjunction', 'phrase', 'disjunction', 'conjunction+full', 'phrase+full', 'disjunction+full']


TAIL = 25


def parse(out):
    """{arm: {round: {(threads, style): fields}}}, arms in name order.

    A query's own counts (`pinq` lines) are under (threads, style, 'forms'):
    {name: cycles per timed pass}."""
    runs = collections.defaultdict(dict)
    for path in sorted(glob.glob(os.path.join(out, 'pinned-r*-*.txt'))):
        m = re.fullmatch(r'pinned-r(\d+)-(.+)\.txt', os.path.basename(path))
        if not m:
            continue
        r, arm = int(m.group(1)), m.group(2)
        rows = {}
        for line in open(path, errors='replace'):
            f = line.split()
            if len(f) == 8 and f[0] == 'pinq':
                threads = f[1].count(',') + 1
                rows.setdefault((threads, f[2], 'forms'), {})[f[3]] = float(f[7])
                continue
            if not f or f[0] != 'pinned':
                continue
            row = dict(zip(f[3::2], f[4::2]))

            def num(key, row=row):
                return float(re.sub(r'[MK]$', '', row[key]))

            threads = int(row['threads'])
            rows[(threads, f[2])] = {
                'ins': num('ins/q'),
                'cyc': num('cyc/q'),
                'bmiss': num('bmiss/q'),
                'ipc': num('ipc'),
                'core_ipc': num('core-ipc'),
                'rate': num('q/Gcyc'),
                'qps': num('qps'),
                'digest': row['digest'],
            }
        if rows:
            runs[arm][r] = rows
    paired = any(k[0] == 2 for arm in runs.values() for rows in arm.values() for k in rows)
    if not paired:
        # Single-thread runs only: read them as the paired columns too.
        for arm in runs.values():
            for rows in arm.values():
                for k in [k for k in rows if k[0] == 1]:
                    rows[(2,) + k[1:]] = rows[k]
    return runs


def med(values):
    values = [v for v in values if v is not None]
    return st.median(values) if values else float('nan')


def styles_of(runs):
    seen = {k[1] for arm in runs.values() for rows in arm.values() for k in rows if len(k) == 2}
    return sorted(seen, key=lambda s: (ORDER.index(s) if s in ORDER else 99, s))


def get(runs, arm, r, threads, style, key):
    row = runs.get(arm, {}).get(r, {}).get((threads, style))
    return row[key] if row else None


def tail_rounds(runs, name, base, threads, style, n=TAIL):
    """Per common round, the change in mean cycles per query of the n forms
    slowest in BASE (paired, median over rounds) and of every form; ([], [])
    without per-query lines."""
    rounds = sorted(set(runs.get(name, {})) & set(runs.get(base, {})))
    forms = lambda arm, r, t: runs[arm][r].get((t, style, 'forms'), {})
    by_base = collections.defaultdict(list)
    for r in rounds:
        for q, c in forms(base, r, 2).items():
            by_base[q].append(c)
    if not by_base:
        return [], []
    slowest = sorted(by_base, key=lambda q: -st.median(by_base[q]))[:n]
    tail, every = [], []
    for r in rounds:
        a, b = forms(base, r, threads), forms(name, r, threads)
        common = [q for q in a if q in b]
        top = [q for q in slowest if q in a and q in b]
        if top:
            tail.append(sum(b[q] for q in top) / sum(a[q] for q in top) - 1)
            every.append(sum(b[q] for q in common) / sum(a[q] for q in common) - 1)
    return tail, every


def over(runs, arm, threads, style, key):
    return med([get(runs, arm, r, threads, style, key) for r in sorted(runs[arm])])


def validation(runs):
    print('== Per arm: one hyperthread (1) vs both of its core (2); medians over rounds')
    print(f'{"arm":<8} {"style":<18} {"cyc/q 1":>9} {"cyc/q 2":>9} {"2/1":>5} {"ins 2/1":>7} '
          f'{"IPC 1":>6} {"core IPC 2":>10} {"q/Gcyc 1":>9} {"q/Gcyc 2":>9} {"SMT gain":>8} {"q/Gcyc 8":>9}')
    for arm in runs:
        for style in styles_of(runs):
            c1, c2 = over(runs, arm, 1, style, 'cyc'), over(runs, arm, 2, style, 'cyc')
            i1, i2 = over(runs, arm, 1, style, 'ins'), over(runs, arm, 2, style, 'ins')
            q1, q2 = over(runs, arm, 1, style, 'rate'), over(runs, arm, 2, style, 'rate')
            q8 = over(runs, arm, 8, style, 'rate')
            q8 = f'{q8:9.2f}' if not math.isnan(q8) else f'{"-":>9}'
            print(f'{arm:<8} {style:<18} {c1:9.2f} {c2:9.2f} {c2 / c1:5.2f} {i2 / i1:7.3f} '
                  f'{over(runs, arm, 1, style, "ipc"):6.2f} {over(runs, arm, 2, style, "core_ipc"):10.2f} '
                  f'{q1:9.2f} {q2:9.2f} {100 * (q2 / q1 - 1):+7.1f}% {q8}')
    print()


def compare(runs, name, base):
    print(f'== {name} vs {base}: paired-core throughput (q/Gcyc; + is better); cycles, instructions and '
          f'branch misses per query (- is better)')
    rounds = sorted(set(runs.get(name, {})) & set(runs.get(base, {})))
    if not rounds:
        print('  no common rounds\n')
        return
    changes = []
    for style in styles_of(runs):
        def change(threads, key, pick=med):
            a = pick([get(runs, base, r, threads, style, key) for r in rounds])
            b = pick([get(runs, name, r, threads, style, key) for r in rounds])
            return b / a - 1 if a else float("nan")

        per = []
        for r in rounds:
            a, b = get(runs, base, r, 2, style, 'rate'), get(runs, name, r, 2, style, 'rate')
            per.append(f'{100 * (b / a - 1):+.1f}' if a and b else '?')
        q2 = change(2, 'rate')
        same = all(get(runs, name, r, t, style, 'digest') == get(runs, base, r, t, style, 'digest')
                   for r in rounds for t in (1, 2))
        if '@' not in style:  # a fixed set of forms (STYLE@SET) is shown, not averaged in
            changes.append(q2)
        print(f'  {style:<18} pair {100 * q2:+6.2f}% ({" ".join(per)})  pair cyc/q {100 * change(2, "cyc"):+6.2f}%  '
              f'single cyc/q {100 * change(1, "cyc"):+6.2f}% ins/q {100 * change(1, "ins", st.mean):+6.2f}% '
              f'bmiss/q {100 * change(1, "bmiss"):+6.1f}%  answers {"same" if same else "DIFFERENT"}')
    g = math.exp(st.mean(math.log1p(c) for c in changes)) - 1
    print(f'  {"geomean":<18} pair {100 * g:+6.2f}%')
    tails = [(style,) + tail_rounds(runs, name, base, 2, style) + tail_rounds(runs, name, base, 1, style)
             for style in styles_of(runs)]
    if any(t[1] for t in tails):
        print(f'  tail: mean cycles per query of the {TAIL} forms slowest in {base} (paired) beside every form\'s, '
              f'median of per-round changes (- is better)')
        for style, tp, ep, ts, es in tails:
            if tp:
                print(f'  {style:<18} tail{TAIL} pair {100 * med(tp):+6.2f}% ({" ".join(f"{100 * x:+.1f}" for x in tp)})  '
                      f'mean pair {100 * med(ep):+6.2f}%   tail{TAIL} single {100 * med(ts):+6.2f}%  '
                      f'mean single {100 * med(es):+6.2f}%')
    print()


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('out')
    ap.add_argument('--compare', default='', help='NAME:BASE,... (default: none)')
    args = ap.parse_args()
    runs = parse(args.out)
    if not runs:
        raise SystemExit(f'no pinned-r*-*.txt in {args.out}')
    validation(runs)
    for pair in filter(None, args.compare.split(',')):
        name, base = pair.split(':')
        compare(runs, name, base)


if __name__ == '__main__':
    main()
