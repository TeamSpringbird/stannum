# Short local performance experiment loop

Start with [the comparison board](experiment-board/index.html), its
[Markdown table](experiment-board/report.md), and the pinned
[branch/PR inventory](../../benchmarks/experiments/inventory.json).
Results are diagnostics on shared local hardware, not published TIN comparisons.

## First batch

| Candidate | Workload | Status / decision |
|---|---|---|
| Count strategy, 54c2d9e | 302 OR-count queries; 100k and 1m; clean and mutated snapshots | 100k/1m clean complete. Million-row mutated comparison running. Forced bitmaps improve the upper tail but regress many queries; do not enable universally. |
| PR45, ad646405 | Filtered ranked top-K; 100k clean snapshot; three queries × 1/10/50% hash filters × K=10/100 | Queued after the million-row run. Four alternating rounds, nine timed executions per case. Compare feature off/on in the exact same PR binary to isolate the feature from its older branch base. |
| PR61 | Merge-heavy writes with concurrent reads | Next maintenance experiment. Measure write/read p95, peak memory and spill bytes. A read-only count benchmark does not exercise its changed path. |
| PR63 / PR69 | Rollback, temporary-file cleanup and crash recovery | Correctness gates for PR61, not independent speed candidates. |
| PR79 | Visibility and count correctness | Reuse its coverage as a gate. |
| PR80 | Sparse/dense posting decode microbenchmark | Mechanism evidence only; pair any future decoder change with SQL measurements. |

Other branches remain unclassified until patch equivalence is reviewed. A branch
head not being an ancestor of main does not prove its changes are absent: many
PRs were squash-merged. Do not benchmark obsolete branches as new optimizations.
Do not merge experimental PRs based only on a favorable aggregate.

## Repeatable protocol

1. State one hypothesis and the query/maintenance path it should affect. Select
   a control and candidate commit, flag values, workload hash and snapshot state.
2. Build release artifacts separately, retain hashes, and check SQL/index-format
   compatibility. Current native runners assume identical extension SQL interfaces.
3. Restore a fresh physical snapshot for each trial. Bind only the owned cluster
   to a private library copy and restart before using the candidate. Run result
   and execution-plan gates before timing. A fallback plan is not an optimization.
4. Use alternating or balanced order, fixed resource settings and seeds. Serialize
   builds and benchmarks with the local lock. Retain every trial, including errors.
5. Report per-query changes and p50/p95/p99 with spread across repetitions. Show
   clean/mutated and dataset sizes separately. Highlight regressions exceeding both
   10% and 0.05ms; thresholds are screening rules, not statistical significance.
6. Decide keep/investigate/reject for that hypothesis. Validate promising policies
   on unseen queries/data and under concurrent load before AWS comparisons.

The count board reports nearest-rank percentiles over 302 per-query medians, not
request-weighted concurrent p95. Three rounds characterize noise but do not prove
statistical significance. The frontier experiment reports each shape separately
and uses tie-aware membership/score/order checks against exhaustive same-engine
ranking with the optimized custom scan disabled. That is not an independent Lead
oracle. Its plans must demonstrate that the enabled frontier actually ran.

## Tools and retained evidence

`benchmarks/experiment_queue.py --manifest queue.json --output <new-dir>` runs
explicit commands serially after prerequisite manifests complete. Each step has
`name`, `argv`, `cwd`, optional `lock`, and optional `source_commit`. Source-pinned
steps reject a changed/dirty worktree. A step failure stops dependent work and
retains exit status/logs. Runners that own the local lock must not be wrapped in
another lock. Freeze scripts into the campaign directory before launching.

`benchmarks/experiment_report.py <snapshot-latency-run> ... --output <report-dir>`
generates standalone HTML, Markdown, JSON and CSV. It withholds metrics from
unfinished runs and rejects missing round/query coverage in completed runs.

The active queue, frozen scripts, launcher PID, logs and status are retained in
`benchmarks/results/experiment-loop-r1/`. It waits for the million-row mutated
run, builds pinned PR45 in an isolated worktree/target directory, executes
`frontier_snapshot_experiment.py`, and refreshes the count board. PR45 produces
its own per-shape `frontier/report.md`, raw plans and failure/completion manifest.
An unsuccessful test stays unsuccessful; a previous good result must not hide it.

Snapshots, SQL interfaces, platform, PostgreSQL version and binary identities
remain explicit constraints. Native macOS results screen ideas; AWS checks the
published PlanetScale workload/resource target with remaining hardware and
methodology differences disclosed. Local results are never relabeled as TIN
comparisons.
