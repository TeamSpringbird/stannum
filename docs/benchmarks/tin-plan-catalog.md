# Observing TIN's plan choices

On September 19, 2026, we collected 170 EXPLAIN ANALYZE observations from a
small PlanetScale TIN instance, using owned synthetic fixtures of 1,000 and
10,000 rows. All probe schemas were removed afterward. An earlier 136-case
pilot used the same frequency fixture; the checked-in results are the later,
broader sweep, not independent repetitions.

The [catalog](tin-plan-catalog-results.json) retains the case definitions,
server settings, work estimates and counters. Eight [complete example plans](tin-plan-examples.json)
retain the unfiltered EXPLAIN output. Local raw SQL, all complete plans and the
exact collector snapshot remain in `benchmarks/results/tin-catalog-sweep`.
Connection details and credentials are not recorded.

## Environment and limits

The server reported PostgreSQL 18.6 on ARM64 and TIN 1.0.2, with 67 MiB
shared buffers, 2 MiB work_mem, 16 MiB maintenance_work_mem,
effective_cache_size 203 MiB, random_page_cost 1.1, JIT off and I/O timing off.
These are PostgreSQL settings, not a measurement of physical RAM or CPU quota.
The user identified this as a low-spec deployment. The synthetic index at
10,000 rows was 1,171,456 bytes and the complete relation 2,490,368 bytes.
This is a small, cache-friendly probe, not a capacity or disk-pressure test.

Each case ran once, sequentially, with `EXPLAIN (ANALYZE, BUFFERS, SETTINGS,
FORMAT JSON, TIMING OFF)`. Overall execution time is recorded, but there are
ordered warm-cache effects and instrumentation overhead; no speedup claims or
performance confidence intervals are inferred. Prepared cases compare forced
custom, forced generic and auto plans. Auto uses six EXPLAIN-only executions
of the same value before the measured execution, not a production request mix.

## Observed choices

| Probe | Observation on this fixture |
| --- | --- |
| Count, including phrase and prefix expansion | `Tin Count` custom node |
| Ranked search | `Projector` above `Text Search Scan`, with a `Top K` field |
| Generic prepared ranking | Text-search custom node retained `$1` and Top K 10 for rare, broad OR and phrase queries at both sizes |
| Dense scoring at 10% versus 10.1% document frequency | `p100` was retained; `p101` was listed under `Elided Terms`, at both sizes |
| `full_score` on those same queries | No dense-term-elision annotation |
| Indexed ID filters selecting 0.1%, 1% or 10% of 10,000 rows | `Conjunction Scan` joined text search with `Index TID Probe`, followed by sorting; tested LIMIT 10 and 100 |
| ID filters selecting 50% or 90% | Ranked text path with additional projection, without the TID-probe conjunction |
| Tenant filter, LIMIT 1000, work_mem 64 KiB | Same conjunction/sort shape, with 15 temporary blocks written |
| Same tenant query, work_mem 2 or 8 MiB | No temporary blocks written |
| OR lengths 2, 4, 8, 9 and LIMIT values 1, 10, 100, 1000 | Ranked text node remained; no visible algorithm switch in these tested cases |

The fixtures make the frequencies exact: `common` occurs everywhere, `rare`
in 0.1%, `half` and `opposite` are disjoint halves, and `p010` through `p200`
have nested frequencies set by `id % 1000`. Adjacent `alpha beta` occurs in
10%, while `alpha spacer beta` occurs in the remaining rows. These correlations
are intentional; they are not representative of every natural-language corpus.

The filter-path transition is **bracketed between the tested 10% and 50% points**
for those queries/settings. It is not a recovered universal cutoff. Cost changes,
correlation, row width, index layout, LIMIT, cache estimates and hardware may
move it. The unchanged visible node for longer OR queries does not prove the
same internal posting algorithm was used.

## What EXPLAIN exposes

TIN provides a `Predicted Work` breakdown containing posting, position,
document-length, heap and startup pages; driver rows, mutable rows, candidate
blocks, filter TIDs, exact batches, hash rows and sort rows. The `Page Touches`
section distinguishes metadata, term map, postings footer/payload/TF tail,
positions, document-length sidecar and liveness bitmap accesses.

Those labels give us useful hypotheses about selective access to posting data.
They do not reveal an exact compression codec, SIMD instruction sequence,
on-disk format, or every runtime decision. Page touches are logical accesses;
they are not distinct pages or physical storage bytes. Fields named rows under
`Predicted Work` are estimates, not the actual number of matches. In particular,
a count aggregate's `Actual Rows: 1` means one aggregate result.

## Implications for Stannum

1. **Complete generic ranked plans.** The TIN probes confirm this behavior is
   useful and observable; PR #32 independently implements execution-time
   binding using Stannum's existing scorer.
2. **Measure filtered ranking before optimizing it.** TIN's explicit TID-probe
   conjunction makes a concrete alternative to ranking candidates and then
   applying a selective SQL filter. Compare strategies on Stannum's own data
   structure and costs before choosing or copying any threshold.
3. **Improve work accounting.** Similar counters for posting decoding, positions,
   scoring, heap fetches and discarded candidates would make our own choices
   diagnosable. Distinguish estimates from actual counters.
4. **Expand one dimension at a time.** Additional sizes, row widths, mutations,
   joins and resource settings can test whether the observed transitions hold.
   Keep Lead as the semantic reference. Observing a TIN plan does not establish
   correctness or that the same plan is optimal for Stannum.

## Replay

Supply standard libpq environment variables through your normal secret manager;
never commit them or place credentials in the command line. The existing
benchmark entry point accepts:

```sh
python3 benchmarks/tin.py catalog --rows 1000 10000 \
  --output benchmarks/results/tin-catalog-new
```

It creates uniquely named schemas on the selected existing TIN server, caps
fixture sizes at 50,000 rows, executes one query at a time with a 30-second
statement timeout and 3-second lock timeout, and drops only schemas it created.
It does not provision remote infrastructure or modify server-wide settings.
The three work_mem probes change only their own short-lived sessions.
