# Published Stack Exchange compatibility smoke

Two live campaigns on local ARM macOS / PostgreSQL 18.6 passed against pinned
Lead `bd95c7e51b6afce81396790852ee2f2c169570ad`. Stannum and the harness were
built from `94674ac` (the PR72 implementation). Both used raw, unmodified prefixes
of the verified prepared Stack Exchange CSV and all 1,254 confirmed base samples,
expanded into 3,762 AND/OR/phrase forms. Text IDs were preserved.

| Documents | Completed forms | Differences | Verification seconds | Nonempty AND | Nonempty OR | Nonempty phrase |
|---:|---:|---:|---:|---:|---:|---:|
| 100 | 3,762 | 0 | 240.84 | 41 | 1,213 | 20 |
| 1,000 | 3,762 | 0 | 421.29 | 184 | 1,238 | 49 |

Each style has 1,254 forms. Every form compares exact membership and full-score
bits against Lead, plus Stannum top-ten membership/scores with ties permitted.
Empty results are checked too, but nonempty counts expose their limited coverage.
These are one-run correctness campaign runtimes, including verification of the
84.5 GB CSV, setup and client calls. Build time is excluded. They are not search
latencies, throughput benchmarks, or comparisons against closed-source TIN.

The native-test lock serialized both campaigns and builds. Unique temporary
local databases were removed after each run. No cloud resource was created.
Installed library hashes, query/input hashes, settings, source identity and
per-form matched-row counts were captured. Raw compressed observations preserve
all compared result rows. The reference revision is pinned, not a claim about
latest Lead or the managed TIN service.

## Recommendation and next gate

Use 1,000 rows / all 3,762 forms as an initial separate Stack Exchange gate with
a 900-second budget. The observed seven-minute duration has headroom, but is not
a repeated runtime guarantee. Preserve the existing Wikipedia campaign.
Only 49 phrase forms matched this prefix; add positive phrase/analyzer witnesses
before treating this as broad raw-text coverage. It does not exercise the full
150-million-row corpus, mutations, or memory pressure.

The live published-corpus differential path is now validated. The timed harness
still needs a tokenizer-aware sampled membership check before lifting its Stack
Exchange guard. This result does not justify replacing that check with regex
matching or treating GIN ranking as a BM25 oracle.

## Evidence and reproduction

Follow the published-corpus invocation in [published-datasets.md](published-datasets.md)
with `--rows 1000`. Use installed Stannum and the pinned Lead build on local PG18,
separate fresh databases, and hold `/tmp/stannum-pgrx.lock` for native installation
and the campaign. The oracle cleans its tables; the owner removes its databases.

[Compact evidence](stackexchange-lead-smoke-results.json) records both successful
runs and raw-file hashes. The local archive is
`benchmarks/results/stackexchange-lead-smoke-evidence.tar.gz`, SHA-256
`dbb24105b7ce039bae99a95b865fca8b66dafa0b7acd4d0a35b15b96b13d0bc7`.
Raw logs and observations are ignored by git; retain/export this archive before
removing the worktree. No passing result was inferred from a partial campaign.
