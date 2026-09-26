#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Fit a deliberately small diagnostic rule on training queries only.

Candidate rule: force pages if estimated postings per heap page >= density and
unique terms >= width; otherwise preserve default selection. Timing simulation
excludes selector overhead and does not constitute an implemented speedup.
"""
import argparse
import json
from pathlib import Path

DENSITIES = (.01,.025,.05,.1,.25,.5,1,2,4,8,16,32,64)
WIDTHS = (1,2,3,4,6,8,12)


def use_pages(row, rule):
    # Forcing an already-selected page path cannot be credited as an optimization.
    default = [r['strategy'] for r in row.get('runs', []) if r['mode']=='off']
    if default and all(strategy=='page bitmaps' for strategy in default):
        return False
    return (rule is not None and row['features']['postings_per_heap_page'] >= rule['density']
            and row['features']['unique_terms'] >= rule['width'])


def material_regression(before, after):
    return after-before > max(.05,.1*before)


def fit(phases):
    rows = [r for name in ('vacuumed','mutated') for r in phases[name] if r['partition']=='train']
    if not rows: raise ValueError('no training rows')
    best = sum(r['median_ms']['off'] for r in rows)
    selected = None
    for density in DENSITIES:
        for width in WIDTHS:
            rule = dict(density=density,width=width)
            predictions = [(r['median_ms']['off'], r['median_ms']['on' if use_pages(r,rule) else 'off']) for r in rows]
            if any(material_regression(a,b) for a,b in predictions): continue
            cost=sum(b for a,b in predictions)
            if cost < best:
                best,selected=cost,rule
    return selected


def evaluate(rows, rule):
    baseline = sum(r['median_ms']['off'] for r in rows)
    selected = sum(r['median_ms']['on' if use_pages(r,rule) else 'off'] for r in rows)
    regressions=[]
    for r in rows:
        a=r['median_ms']['off'];b=r['median_ms']['on' if use_pages(r,rule) else 'off']
        if material_regression(a,b): regressions.append(dict(id=r['id'],text=r['text'],default_ms=a,selected_ms=b))
    return dict(queries=len(rows), forced_queries=sum(use_pages(r,rule) for r in rows),
                summed_default_medians_ms=baseline,summed_selected_medians_ms=selected,
                simulated_ratio=baseline/selected if selected else None,
                material_regressions=regressions,
                oracle_summed_medians_ms=sum(min(r['median_ms'].values()) for r in rows))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('root',type=Path)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--fixed-rule',type=Path,help='Evaluate a previously frozen selection.json without fitting')
    args=parser.parse_args()
    manifest=json.loads((args.root/'manifest.json').read_text())
    if manifest['status']!='complete': raise ValueError('campaign incomplete')
    phases={name:json.loads((args.root/name/'comparison.json').read_text()) for name in
            ('vacuumed','vacuumed-repeat','mutated','revacuumed')}
    expected=None
    for name,rows in phases.items():
        ids={r['id']:r['partition'] for r in rows}
        if len(rows)!=302 or len(ids)!=302: raise ValueError('missing or duplicate queries')
        if expected is not None and expected!=ids: raise ValueError('query partitions changed')
        expected=ids
        if any(len(r['runs'])!=18 for r in rows): raise ValueError('missing repetitions')
    rule=json.loads(args.fixed_rule.read_text())['rule'] if args.fixed_rule else fit(phases)
    result=dict(rule=rule,training_phases=[] if args.fixed_rule else ['vacuumed','mutated'],
                fixed_rule_source=str(args.fixed_rule.resolve()) if args.fixed_rule else None,
                regression_guard='slower by more than both 10% and 0.05ms',
                caveat='Offline simulation, equal weight per query; not measured selector throughput. Features use existing planner estimates; no actual candidate counts used.',
                evaluations={name:{part:evaluate([r for r in rows if r['partition']==part],rule) for part in ('train','heldout')} for name,rows in phases.items()})
    args.output.mkdir(parents=True,exist_ok=False)
    (args.output/'selection.json').write_text(json.dumps(result,indent=2)+'\n')
    lines=['# COUNT strategy crossover map','','Frozen rule evaluated without retuning.' if args.fixed_rule else 'Rule fitted using training queries from vacuumed and mutated phases only.',
           'Held-out query IDs were assigned before measurement. Prior six examples are training-only.',
           '',f'Candidate rule: `{rule}`. If no rule survives the guard, retain the existing selector.',
           '',result['caveat'],'',
           '| Phase | Set | Queries | Forced | Simulated ratio | Material regressions |',
           '| --- | --- | ---: | ---: | ---: | ---: |']
    for phase,parts in result['evaluations'].items():
        for part,r in parts.items():
            lines.append(f"| {phase} | {part} | {r['queries']} | {r['forced_queries']} | {r['simulated_ratio']:.3f}x | {len(r['material_regressions'])} |")
    (args.output/'report.md').write_text('\n'.join(lines)+'\n')
    print('\n'.join(lines))


if __name__=='__main__':
    main()
