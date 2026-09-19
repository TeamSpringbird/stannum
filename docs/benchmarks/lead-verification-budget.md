# A bounded Lead verification corpus

**Selected local default: 5,000 Wikipedia documents.** The complete 906-form
check passed in **851.2 seconds (14m 11s)**, with zero differences and about
49 seconds remaining in the 15-minute budget. This is the largest prefix
verified end-to-end in this calibration, not a universal hard limit for Lead.

The correctness reference is PlanetScale Lead, not PostgreSQL GIN. Large
performance corpora and routine differential checks have different size
budgets: Lead tokenizes and scores heap documents repeatedly rather than using
Stannum's searchable index.

This local calibration targets **900 seconds** for verification. Compilation
and PostgreSQL provisioning are separate; corpus checksum validation, prefix
loading, index creation, and query checking consume the verification budget.
Cleanup runs afterward. There is no universal maximum Lead row count: document
length, query shape, machine load and the checks performed change the limit.

## Verification contract

The read-only trace mode in `benchmarks/oracle.py` uses an immutable prefix of
our existing Wikipedia performance corpus. It preserves complete document
bodies from that corpus; it does not shorten them to make Lead finish faster.
The published 302 records expand into all 906 AND/OR/phrase forms, including
repeated text. Each form checks:

1. Every matching document ID, not just counts or the first ten matches.
2. Exact `full_score` float32 bits for every matching document.
3. Stannum's optimized top-10 projection against Lead's exhaustive scores,
   allowing different document IDs only at equal-score ties.

No scoring exceptions or numerical tolerances apply to this trace mode.
Errors, timeouts, nonfinite scores, duplicate rows, source/binary drift, and
incomplete query coverage cannot produce a passing result. Both databases get
the exact same CSV prefix. Compressed raw observations preserve each query's
results; the summary records identities, timings, counts and differences.

This supplements the existing 47-shape/five-state synthetic oracle, which also
covers mutations, dense scoring and highlighting. A read-only Wikipedia check
does not replace those contracts or prove agreement for arbitrary TINQL.

## Calibration environment

Local native ARM64 release builds, PostgreSQL 18.6, Apple M4 Max
(16 logical CPUs, 128 GiB RAM); one
query at a time, 1 GiB shared buffers, JIT and parallel query workers disabled.
No Docker CPU or memory ceiling applies to this native calibration. Other
local services remain running. Builds and measurements use the machine-wide
pgrx lock to avoid overlapping installation/timing work.

Lead is pinned at `bd95c7e51b6afce81396790852ee2f2c169570ad`. Stannum includes
PR #28's prepared-query fix. Both extensions are built in release mode and
installed before starting the isolated PostgreSQL cluster. The report records
installed library hashes and the actual server/extension versions.

The first pilot sampled 24 forms spanning common terms, rare combinations,
phrases, and the long queries at the end of the published trace. All match IDs
and exact score bits agreed at all five sizes:

| Documents | Pilot query time, both engines | Setup time |
| --- | ---: | ---: |
| 100 | 2.02 s | 0.21 s |
| 500 | 3.84 s | 0.30 s |
| 1,000 | 6.38 s | 0.45 s |
| 2,000 | 11.16 s | 0.80 s |
| 5,000 | 26.08 s | 1.50 s |

Pilot timings exclude the additional optimized top-10 check in the full suite.
They estimate a useful search range, not a verified whole-suite runtime or a
passing result for the remaining query forms. Raw artifacts are retained under
`benchmarks/results/lead-sizing-pilot`.

## Full-suite result and failure behavior

The fixed prefix contains **19,132,553 bytes of document text**. All 906 forms
passed; 629 had matches and 277 correctly returned none. The check compared
**338,513 matching-row score pairs** exactly and verified **4,577 returned
top-10 rows** (queries with fewer than ten matches return fewer rows).

| Stage | Observed wall time |
| --- | ---: |
| Setup, including checksum and loading | 1.94 s |
| Lead exhaustive projections, combined | 800.36 s |
| Stannum exhaustive projections, combined | 30.91 s |
| Stannum optimized top-10 projections, combined | 16.06 s |
| Entire verification, including recording and cleanup | 851.21 s |

Projection timings include client startup and result transfer. They are
correctness-check costs, not steady-state throughput measurements. Lead
accounts for about 94% of total verification time. The result justifies keeping
this reference corpus small while using the 100k/1m corpora for performance.
It does not establish a memory ceiling or a limit that transfers to CI runners.

The [compact results](lead-verification-results.json) retain identities, source
and installed-library hashes, measured settings, pilot observations and the
full-run summary. Raw artifacts are in `benchmarks/results/lead-trace-5000`.
The verifier source used for the timed run is preserved in its `protocol/`
directory; subsequent changes only added artifact/progress metadata and bounded
cleanup handling, without changing the verification SQL or comparisons.

A deliberate three-second run on 100 documents stopped after **23/906 forms**,
returned a nonzero exit code and recorded **`incomplete`**, despite zero observed
differences in completed forms. Its tables were cleaned up. The existing shell
entry point also passed a separate 100-row, nine-form integration smoke. Neither
small run is counted as an additional complete published-trace verification.

The five-state synthetic oracle remains in CI. This larger real-document mode
is opt-in and locally calibrated; it does not silently increase CI's workload.

## Run the check

Use the existing `script/reference-oracle` entry point against an isolated
PostgreSQL server you own, with `pg_config`, `psql`, and `cargo pgrx` on PATH.
The wrapper builds both extensions before the timed verification, creates its
usual two oracle databases, and removes them afterward. Keep native extension
installation and verification serialized on the host.

```sh
PGHOST=127.0.0.1 PGPORT=55441 \
LEAD_REF_DIR=/path/to/clean/pinned/lead \
DATASET="$HOME/Library/Application Support/LeadBenchmarks/datasets/wikipedia-100000" \
TRACE=benchmarks/results/tin-driver/datasets/wikipedia/queries.json \
ROWS=5000 BUDGET_SECONDS=900 \
script/reference-oracle benchmarks/results/lead-trace-check
```

An existing `LEAD_REF_DIR` is used as-is, so explicitly check out the intended
revision. Its actual revision is recorded; no claim that it automatically
tracks upstream main is made. The oracle records the installed local library
hashes; use the same local PostgreSQL installation for building and serving
these verification databases.

The result lives in `trace/oracle.json`, with raw observations in
`trace/observations.jsonl.gz` and the exact imported prefix in `trace/input.csv`.
Only `status: passed` means every query completed with no discrepancy. An
`incomplete` result is not a correctness pass, even if completed queries agreed.
The routine budget applies to the trace stage, not the build commands above.

The standalone `benchmarks/oracle.py --dataset ... --trace ...` mode also
accepts two environment files pointing at separate prepared databases. It
creates only its own `oracle_trace_docs` table and refuses to reuse an existing
one. The original invocation without `--dataset` retains its synthetic
mutation/highlight behavior.
