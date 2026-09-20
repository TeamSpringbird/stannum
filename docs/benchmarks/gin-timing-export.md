# GIN timing export correction

The September 20 AWS Wikipedia COUNT run completed 1,030 measured queries, but
its dashboard export recorded only 268 milliseconds. The dashboard's two-second
inactivity heuristic set `EndTime` after an early gap; later query samples updated
`LastUpdateTime` without reopening the run. Slow queries therefore froze the
exported duration and inflated QPS. This is a measurement bug, not a GIN execution
improvement, and can affect any backend with gaps between completions.

The adapter now treats idle closure as provisional. Later activity reopens the
run, and an already-old batch closes at its latest sample. Out-of-order delivery
cannot shrink that boundary. The report also rejects query samples outside the
exported window, allowing the export's millisecond truncation. Native Linux
dashboard tests run before driver compilation, including when cross-compiling.

## Recovered AWS result

The original files remain unchanged. Local derived evidence is under
`benchmarks/results/gin-timing-recovery/`; `recovery.json` records source hashes
and the recovery method. `comparison.json` is the authoritative derived report;
the intermediate dashboard JSON is only a timing-corrected input, not a rebuilt
dashboard with all aggregate fields recalculated.

The two raw `scenario_started` samples confirm the original start timestamp,
1789939574737 ms. The maximum query completion timestamp is 1789940174687 ms.
Using that boundary follows the driver's last-activity convention, rather than
substituting the requested test duration or using the first query completion.

| Metric | Recovered value |
| --- | ---: |
| Duration | 599.950 seconds |
| Completed queries | 1,030 |
| Throughput | 1.716810 queries/second |
| p50 | 157.199 ms |
| p95 | 4,638.605 ms |
| p99 | 5,665.103 ms |

The resource summary was recomputed for the corrected interval as well. This
recovers a previous run; it is not a new benchmark. Running AWS jobs retain their
original binaries and require the same export audit when they finish.

Validation: a synthetic dashboard replay reproduced the frozen 268 ms window
before the fix and passed afterward, including a late out-of-order sample. The
full dashboard suite and 29 Python benchmark tests pass. The report regression
rejects the stale boundary and accepts a corrected boundary with submillisecond
sample timestamps.
