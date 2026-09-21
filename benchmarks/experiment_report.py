#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Render completed snapshot latency trials as JSON, CSV, Markdown and HTML."""
import argparse
import csv
import html
import json
from pathlib import Path
import statistics


def summarize(root):
    m=json.loads((root/'manifest.json').read_text())
    if m['status']!='complete':return dict(path=str(root),status=m['status'],rows=[])
    results=json.loads((root/'results.json').read_text())
    expected={(round_id,variant) for round_id in (1,2,3) for variant in ('main','candidate-default','candidate-bitmaps')}
    if {(x['round'],x['variant']) for x in m['rounds']}!=expected:raise ValueError('Incomplete round coverage')
    for round_id,variant in expected:
        ids=[r['id'] for r in results if r['round']==round_id and r['variant']==variant]
        if len(ids)!=302 or len(set(ids))!=302:raise ValueError('Incomplete query coverage')
    rows=[]
    by={(qid,variant):statistics.median(r['median_ms'] for r in results if r['id']==qid and r['variant']==variant) for qid in {r['id'] for r in results} for variant in ('main','candidate-default','candidate-bitmaps')}
    for variant in ('main','candidate-default','candidate-bitmaps'):
        rounds=[r for r in m['rounds'] if r['variant']==variant]
        regressions=sum(by[q,variant]>by[q,'main']*1.1 and by[q,variant]-by[q,'main']>.05 for q in {r['id'] for r in results})
        row=dict(variant=variant,regressions_vs_main=regressions)
        for metric in ('p50_ms','p95_ms','p99_ms'):
            values=[r[metric] for r in rounds];row[metric]=statistics.median(values);row[metric+'_range']=[min(values),max(values)]
        rows.append(row)
    return dict(path=str(root),status='complete',dataset_rows=m['source_manifest'].get('expected_rows',100000),state=m.get('snapshot_state','clean'),rows=rows,contract=m['percentile'])


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('runs',nargs='+',type=Path);p.add_argument('--output',type=Path,required=True);a=p.parse_args()
    a.output.mkdir(parents=True,exist_ok=True);runs=[summarize(r) for r in a.runs]
    (a.output/'comparison.json').write_text(json.dumps(runs,indent=2)+'\n')
    lines=['# Local experiment comparison','','Single-client server timings. Percentiles are over 302 per-query medians; not production request p95. Three balanced rounds per completed experiment. No cross-machine speedup claims.','','| Documents / state | Variant | p50 ms | p95 ms (range) | p99 ms | Regressed queries vs main |','|---|---|---:|---:|---:|---:|']
    flat=[]
    for run in runs:
        if run['status']!='complete':lines.append(f"\nPending/failed: {run['path']} ({run['status']}); no aggregate published.\n");continue
        for row in run['rows']:
            low,high=row['p95_ms_range'];label=f"{run['dataset_rows']:,} / {run['state']}"
            cells=[label,row['variant'],f"{row['p50_ms']:.3f}",f"{row['p95_ms']:.3f} ({low:.3f}–{high:.3f})",f"{row['p99_ms']:.3f}",str(row['regressions_vs_main'])]
            lines.append('| '+' | '.join(cells)+' |');flat.append(cells)
    lines+=['','Regression threshold: median across rounds is slower by both 10% and 0.05 ms. Count default and forced paths separately; forced bitmaps are not an enabled selector.','', 'Use the inventory to select workloads: ranked frontier → filtered top-K; spill output → read/write contention and memory; recovery/test PRs → correctness gates; decoding → microbenchmarks.']
    (a.output/'report.md').write_text('\n'.join(lines)+'\n')
    with (a.output/'comparison.csv').open('w') as f:
        writer=csv.writer(f,lineterminator="\n");writer.writerow(['Dataset/state','Variant','p50 ms','p95 ms (range)','p99 ms','Regressions vs main']);writer.writerows(flat)
    body=''.join('<tr>'+''.join('<td>'+html.escape(c)+'</td>' for c in row)+'</tr>' for row in flat)
    pending=''.join('<p>'+html.escape(r['path']+': '+r['status'])+'</p>' for r in runs if r['status']!='complete')
    (a.output/'index.html').write_text('''<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Stannum experiment comparison</title><style>body{font:16px system-ui;max-width:1100px;margin:40px auto;padding:0 20px;color:#17212b}table{border-collapse:collapse;width:100%}th,td{text-align:left;padding:12px;border-bottom:1px solid #d9e0e7}th{background:#eef3f7}td:nth-child(n+3){font-variant-numeric:tabular-nums}p{line-height:1.5;color:#475567}h1{font-size:28px}</style><h1>Stannum local experiment comparison</h1><p>Three balanced rounds · 302 queries · single client · server execution time.<br>Percentiles describe per-query medians, not concurrent production latency.</p><table><thead><tr><th>Documents / state</th><th>Variant</th><th>p50 ms</th><th>p95 ms (range)</th><th>p99 ms</th><th>Regressions vs main</th></tr></thead><tbody>'''+body+'''</tbody></table>'''+pending+'''<p>Regressions exceed both 10% and 0.05 ms. Shared local hardware introduces noise. Forced bitmap results do not represent an enabled selector.</p><p><a href="comparison.json">JSON</a> · <a href="comparison.csv">CSV</a> · <a href="report.md">Method and table</a></p></html>''')


if __name__=='__main__':main()
