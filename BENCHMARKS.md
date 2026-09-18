# Benchmarking Stanum

Our question is whether Stanum can provide exact Boolean/phrase search, BM25
ranking, and counts at useful throughput while documents are being updated. We
compare behavior before speed, retain failures, and keep raw evidence for each
build. This page is the current plan and results summary; older investigation
notes retain their original Lead/TIN names and describe earlier implementations.

## Current evidence: 100,000 Wikipedia articles

On September 17, 2026, the original Lead/competitor campaign attempted 35 trials:
five count trials for each of four engines and five mixed count/ranked trials for
Lead, ParadeDB, and pg_textsearch. All trials ended; 26 passed and nine failed.
The overall campaign is incomplete, although each count group and ParadeDB mixed
has all five successful repetitions.

A frozen development build from `t3code/postgres-full-text-index-design-1`, commit
`3906c288ec2142133439cec4b31364d89b2515a3` **plus uncommitted changes**, then passed
one count trial and one mixed trial under the same workload and resource settings.
This build still used the old `tin` extension identity. The Stanum rename itself
has not been measured in these results. A commit alone cannot reproduce that
working-tree build: its source snapshot, per-file hashes, patch, and immutable
image ID were retained.

| Engine/build | Count read QPS | Successful count trials | Mixed read QPS | Successful mixed trials |
| --- | ---: | ---: | ---: | ---: |
| Original Lead | 0.39 | 5/5 | unavailable | 0/5 |
| PostgreSQL GIN | 3.25 | 5/5 | not tested | — |
| ParadeDB | 80.33 | 5/5 | 80.93 | 5/5 |
| pg_textsearch | 0.21 | 5/5 | incomplete | 1/5 |
| Stanum development build, before rename | **1,458.45** | **1/1** | **644.79** | **1/1** |

Baseline values are medians of five trials. Stanum values are single trials, not
five-run estimates. These are earlier-versus-later measurements on one host, not
interleaved pairs. Large differences justify further testing; they do not establish
an engine-wide performance ranking, significance, or production capacity.

Stanum sustained about 20.24 updates/second in both trials. Index build time was
17.73 s for count and 18.47 s for mixed; total index size after traffic was about
175.29 MiB, including the primary-key index. Peak container memory was below
1.9 GiB. Both trials passed before/after exact-membership checks; mixed also passed
ranked cardinality, membership, finite-score, and score-order checks. No transaction
failures or memory-limit events were observed, and timed CPU throttling was zero.
The ranked checks do not prove globally optimal top-k selection or relevance.

All five original Lead mixed trials experienced an OOM kill at the 4-GiB limit.
The four failed pg_textsearch mixed trials did not sample every query type within
the measurement window. Its one passing trial is not a complete baseline. Failed
runs never become zero-latency samples or disappear through trimming.

### Evidence identity

Original campaign: `wikipedia-v1-20260917/wikipedia-100000`.
Candidate: `lead-current-100k-20260917-230423/campaign`.
These names deliberately preserve the historical artifact identity.

```text
Original image:
sha256:028b8e940aa33846aeb537e9097532e76e42b58dc3a03b32b2f8cba453728532
Candidate image:
sha256:1f12be2ffd865eb18a94fb21a91ece6b0ca82530e95cdd1bb57fe8f18a8dfd8c
```

The development machine retains these folders beneath
`~/Library/Application Support/LeadBenchmarks/campaigns/`, including manifests,
source snapshots, plans, logs, resource counters, and comparison reports. They are
local evidence, not downloadable public artifacts. Generated corpora and raw
results are excluded from Git. Keep legacy folders and frozen protocols unchanged;
new Stanum builds use new output folders and image tags.

## Workload and resource envelope

The frozen corpus samples the English November 2023 Wikimedia Wikipedia dataset,
pinned to revision `b04c8d1ceb2f5cd4588862100d08de323dccfbaa`, with seed 1729.
The normalized 100k bodies contain 278,979,934 bytes before the mutable suffix.
Normalization concatenates title/text, keeps lowercase ASCII letter sequences,
and caps each document at 8,192 tokens. It is not a multilingual workload.
Original article attribution and dataset checksums are retained with the corpus.
The historical absent-term token is part of the fixture, not a product name to
rewrite; changing it would change the benchmark.

Queries include common/medium/rare terms, AND/OR, three phrase frequencies, and
term/phrase misses. Count runs have ten query shapes. Mixed runs have twenty:
each count plus its ranked top ten. Query streams use the same seed. GIN is tested
for counts only; its ranking is not treated as BM25. Engine-specific adapters are
in `benchmarks/run.py`.

| Setting | Value |
| --- | --- |
| Host/runtime | Local Mac Studio, native ARM64 Docker/OrbStack, PostgreSQL 18.6 |
| Server budget | 4 VM CPUs, cpuset 0–3, 4 GiB memory, no swap |
| PostgreSQL | 1 GiB shared buffers, 16 MiB work_mem, 512 MiB maintenance_work_mem |
| Readers / writer | 2 closed-loop readers; 1 writer requested at 20 updates/s |
| Warmup / measurement | 30 seconds / 300 seconds per trial |
| Cache policy | Warm workload; host/VM caches are not forcibly evicted |
| Durability | fsync, synchronous commit, full-page writes, and autovacuum enabled |
| Isolation | Fresh database volume/container per trial; one measured engine at a time |

The writer toggles a reserved suffix. This rewrites and indexes document content
without changing the tested query memberships. Inserts, deletes, changing result
sets, and long-running maintenance are separate workloads still needed. CPU and
memory limits are ceilings, not reserved hardware; background host activity is
recorded and can affect results. Client CPU is outside the server budget.

## Reproduce a Stanum campaign

Install Docker with native ARM64 support, Python 3, and PostgreSQL 18 client tools.
The Docker recipe builds Stanum and pins the comparator distribution/sources.
Use an existing checksummed 100k corpus, or prepare one using `benchmarks/dataset.py`
as described in [LOCAL.md](benchmarks/LOCAL.md). Corpus preparation currently also
materializes a nested million-document sample; that does not authorize or launch
a million-document evaluation. The series runner defaults to 100k only.

```sh
export PATH="$(brew --prefix postgresql@18)/bin:$PATH"
DATASETS="$HOME/Library/Application Support/LeadBenchmarks/datasets"
RESULTS="$HOME/Library/Application Support/StanumBenchmarks/campaigns"
RUN_ID="$(date +%Y%m%d-%H%M%S)"

python3 benchmarks/campaign.py --build \
  --image "stanum-bench:$RUN_ID" \
  --output "$RESULTS/stanum-100k-$RUN_ID" \
  --dataset "$DATASETS/wikipedia-100000" --rows 100000 \
  --engines stanum --profiles count mixed --repetitions 5 \
  --seconds 300 --warmup 30 --clients 2 --write-rate 20 \
  --statement-timeout-ms 1800000
```

This is ten five-minute measurement windows plus loading, building, and checking.
Use `--repetitions 1` for a preliminary two-trial check, labeling it as such. Never
reuse a results directory. The runner freezes its protocol and records source
fingerprints including the `segment` crate. Pause other benchmark traffic and
heavy builds before measured trials. A campaign builds its image before traffic.

For a new all-engine local campaign select
`--engines stanum gin paradedb pg_textsearch`. TIN is an explicit external adapter;
it is not bundled in the Docker image or included in the local defaults.

## Comparing with PlanetScale TIN

The earlier remote TIN measurements use PlanetScale hardware and a network path
that differ from local Stanum. Their QPS is not comparable to the table above.
Server-side execution probes suggest similar orders of magnitude for some shapes,
but they also use different hardware/settings. We have not demonstrated that
Stanum is faster than TIN on equal resources.

The compatibility oracle previously agreed on 205 query/state pairs (41 queries
across five mutation states), including document sets and score bits. After this
rename it selects each extension explicitly:

```sh
python3 benchmarks/oracle.py \
  --left stanum.env --left-engine stanum \
  --right tin.env --right-engine tin \
  --rows 5000 --output benchmarks/results/stanum-vs-tin-oracle-01
```

Use dedicated test databases and libpq environment files. The two databases must
be distinct and have no existing `oracle_docs` table. The oracle creates its own
fixture and removes it on successful completion unless `--keep` is selected.

For a direct performance comparison, install both libraries on the same instance
and use separate databases, with identical corpus, settings, SQL shapes, clients,
and write schedules. Only extension/schema names differ. Alternate engine order
for five repetitions, run one workload at a time, and compare per-query latency,
QPS, achieved writes, build time, index bytes, and resource pressure. Verify plans,
match sets, and scoring behavior before accepting performance claims.

Our current PlanetScale role cannot install server binaries, and Stanum is not an
available extension on the checked instance. PlanetScale support would need to
provide custom-extension installation, or we would need access to a TIN build on
a host we control. See [PlanetScale's extension policy](https://planetscale.com/docs/postgres/extensions#need-additional-extensions).
Renaming the extension does not remove this deployment requirement.

## Next steps and interpretation

1. Repeat the renamed Stanum build under the frozen 100k protocol; retain all runs.
2. Obtain same-instance TIN/Stanum comparisons with alternating trials.
3. Extend correctness and latency coverage to sustained inserts/deletes, changing
   matches, folding/merging, VACUUM, restart, and crash recovery.
4. Investigate long-tail latency, broad ranked queries, cold sessions, and
   maintenance-induced stalls before making release-readiness claims.

The million-document evaluation is intentionally out of scope for now. A larger
static corpus is not the next proof we need.

For five or more valid repetitions, reports retain median, range, dispersion, and
a trimmed mean that removes one minimum and maximum **per metric**, never whole
runs. Incomplete groups withhold aggregate comparisons. Do not pool transactions
from different runs as independent experiments or average p99s into a workload p99.
The harness reference is [benchmarks/README.md](benchmarks/README.md).
