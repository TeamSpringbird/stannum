# Published-trace benchmark

`benchmarks/tin.py` is the local entry point for the pinned PlanetScale fork
of ParadeDB Benchmarker. The upstream Go/k6 driver owns query traversal,
warmup, phase deadlines, paced updates, latency samples, and index/WAL counters.
Our adapter adds Stannum SQL and access-method accounting. The Python wrapper
owns isolated containers, dataset loading, correctness gates, provenance,
and a compact report. This is not a local TIN binary or an exact reproduction
of PlanetScale's published performance numbers.

## Prepare and run

Requirements: Docker, Python 3, and PostgreSQL client tools (`psql`,
`pg_isready`) on PATH. Node is needed only for the query-adapter tests.
Go/k6 are built in Docker for the host architecture; no global installation.

```sh
python3 benchmarks/tin.py prepare
python3 benchmarks/tin.py build
python3 benchmarks/tin.py build-image \
  --image stannum-bench:published-trace \
  --output benchmarks/results/published-image-01

python3 benchmarks/tin.py run \
  --dataset "$HOME/Library/Application Support/LeadBenchmarks/datasets/wikipedia-100000" \
  --image stannum-bench:published-trace \
  --output benchmarks/results/published-count-01 \
  --rows 1000 --workload count --style mixed --seconds 60
```

Use a new output directory each time. The dataset argument expects our
checksummed `dataset.py` format, not upstream's headered CSV. This first
integration reuses the public Wikipedia **trace** against our existing corpus.
The exact public 5-million-document corpus is supported with
`--published-corpus wikipedia`; see [dataset loading](published-datasets.md).
`--query-file` selects an explicit trace, which is copied and hashed into the run.

Use `--workload topk` for ranked queries, `--style conjunction`, `disjunction`,
or `phrase` to isolate a family, and `--updates 100` to offer 100 whitespace
updates per second. The updater uses one serial connection and skips
missed slots; its actual affected-row count can be lower. Our
`random-page-live-wrap-v1` patch preserves the initial random-page choice but
continues forward to a live tuple, then wraps, if that page is empty. The
unpatched driver failed the 1k write test with 69 empty-page attempts; its
exactly-one-row assertion remains enabled. A real-server regression reproduced
the failure and now covers both forward and wrapped selection. This departure
from the upstream picker is recorded in the adapter manifest. This is not the
mixed insert/update/delete workload in `sustained.py`.

`--rows 100000 --seconds 60 --clients 2 --memory 4g --shared-buffers 1GB`
is an initial resident-corpus diagnostic. Increase corpus size and independently
vary memory and clients before making capacity claims. Only one engine runs at
a time, in a fresh named volume, with the same extension image and PostgreSQL
settings. GIN uses a stored generated vector and its normal planner.
`--build-memory` optionally sets a separate construction cap, switched to
`--memory` before validation and measurement. Set `--maintenance-work-mem`
explicitly when shrinking container limits; it defaults to 512MB.
`--build-segment-docs` overrides Stannum's construction batch size for controlled
memory experiments. Omit it to use the extension default. The override is
recorded in the configuration and PostgreSQL settings; it applies only to Stannum,
and paired image comparisons reject trials with different build batch settings.
Changing the batch size may change the final segment layout and query cost.
`--engines postgres stannum` reverses order for another repetition.

Build and run commands acquire `/tmp/stannum-pgrx.lock`, shared with native
installation/timing work. **Do not wrap them in another acquisition of that
lock.** They remove only their own randomly named containers and volumes.
Unrelated local services remain running; background load is not eliminated.

## Correctness and semantics

All 302 source records expand to 906 AND/OR/phrase forms, including intentional
repetitions. By default, before timing each form compares exact result sets on a bounded
validation sample (`--validation-rows`, default 1000). The indexed candidate
set is materialized from the actual benchmark index before restricting it to
the sampled IDs. This checks both false positives and false negatives on that
sample; it does not prove membership for every row of a larger corpus. Full
counts are also retained for all checked forms and compared between engines.
For larger corpora, `--validation-queries N` explicitly samples N evenly spaced
forms for these untimed checks, recording their IDs in the manifest. The timed
trace remains complete; unchecked forms are not correctness-verified.

Stannum's reference uses raw normalized text with word-boundary predicates.
GIN's reference uses an unindexed PostgreSQL `tsvector`. Ranked runs also
compare the checked forms' top-10 score multisets with exhaustive same-engine scoring,
allowing arbitrary selection/order among tied documents. This can be expensive
on large corpora. It proves top-k agreement with that engine's scoring path,
not the BM25 formula or cross-engine relevance equivalence.

The first attempt using PostgreSQL as Stannum's phrase oracle failed on two
documents for `"the movement"`. A five-row reproduction established the cause:
late repeated-word positions are discarded from `tsvector`, whereas Stannum
and direct text matching retain the phrase. See PostgreSQL's
[text-search limits](https://www.postgresql.org/docs/18/textsearch-limitations.html).
The integration retains this regression in `benchmarks/tin/position-limit.sql`.

Cross-engine membership differences on the validation sample are recorded in
`semantics.txt` and the manifest, separately from each engine's correctness
gate. Mixed/phrase results with these differences are **not equivalent-result
speed comparisons**. BM25 and GIN's `ts_rank_cd` are also different rankings.
The documents are not shortened or filtered to hide either distinction.

Update runs restart the stopped container after measurement and repeat the
membership checks. This validation is outside the measured interval. The
update-only phase must also preserve the heap row count and all 906 full-corpus
query counts. Driver regressions, including query cancellation and live-tuple
selection, run against the isolated server before each engine's workload.

## Artifacts and interpretation

Each run retains:

- Pinned upstream revision, complete source-file fingerprints, adapter patch,
  resolved Go dependencies, driver binary hash, image identity, and extension
  source fingerprint.
- Corpus identity and exact imported CSV hash, rows, settings, source snapshots,
  import/vector preparation/index-build timings, and relation sizes.
- Correctness outputs, semantic differences, representative plans, upstream
  dashboard JSON, raw compressed k6 samples with query IDs, logs, and Docker
  resource configuration.
- `comparison.json` and `report.md`, with overall and per-family/per-query
  latency distributions, completed updates, and observed query-form coverage.

Representative plans execute the actual parameterized count or top-10
projection for the first six forms in the selected style, under both
`force_custom_plan` and `force_generic_plan`. Each plan retains its SQL and
JSON. These diagnostics run in separate sessions before measurement, with
explicit deallocation and reset; timed connections keep their normal planner
settings. Including `id`, `body`, and the score expression in ranked plans is
necessary to expose parameter-sensitive planner-support failures.

Per-query p99 is omitted below 1000 samples. A short run can finish without
traversing the whole trace, especially for slow ranked queries. Consult
`measured_query_forms`; all forms passing a correctness check does not mean
all forms were timed. Warmup samples are absent from `query_duration` metrics.

Index read/hit byte counters measure block accesses, not physical disk traffic.
Read-only and write-active runs have different accounting interference.
Docker resource ceilings are enforced, but the host may cache the VM disk.
The wrapper samples cgroup counters every two seconds across setup, import,
index construction, validation, and the driver phase in `resources.jsonl`.
Each record includes the wall-clock sampling interval and phase. It records
CPU throttling, memory use/events/reclaim, and Linux VM block-device I/O;
pressure-stall counters are null when the Docker kernel does not expose them.
Sampling itself incurs a short Docker exec and its small resource cost is
included in the observations.

`before-build-cgroup.json` and `after-build-cgroup.json` bracket index creation.
`memory.peak` is the lifetime container high-water mark, including import; the
largest sampled `memory.current` in the build phase is only a lower bound on
that phase's peak. The upstream driver stops its container at phase end, so
the last periodic sample is **not** an exact final counter. Use samples wholly
inside the exported measurement interval to compute deltas, and report their
covered duration and gaps to the interval boundaries. Do not label a delta
covering warmup or validation as measurement-only I/O.

Import, vector preparation, index creation and VACUUM have a separate
`--setup-timeout-seconds` limit (default 1800); query correctness and plan
statements retain the 120-second limit. This permits larger index builds
without weakening query timeouts.

The wrapper checks source drift and retains failed attempts. Its report does
not infer a cross-engine speedup or declare a winner. Native ARM results do
not measure AVX-512 behavior.

## Consolidation target

New broad search measurements should use this entry point. Do not add another
campaign launcher. The retirement order is:

| Existing component | Replacement / remaining gate |
| --- | --- |
| `baselines.py` (79 lines) | Dataset-size and repeated-run matrix in the new entry point; retain historical reports |
| `campaign.py` (313 lines) | New container runner plus repetitions, pressure deltas, and remaining engine adapters |
| `paired.py` (193 lines) | Alternate two immutable image identities with matching input/driver manifests |
| Read-only portions of `run.py` | Upstream query driver/reporting; extract the provenance/cgroup helpers still shared with mutation tests |
| `server_times.py` | Generated representative and slow-query plans once those cover its inspection workflow |
| `sustained.py`, `mutation.py`, `oracle.py`, lifecycle/VACUUM probes | Keep until replacement checks the same mutations, visibility, locking and maintenance invariants |

The first three wrappers total 585 lines, before their tests and the replaced
read-only runner code. They are deletion candidates, not yet redundant: this
first slice does not cover all their engines, repetitions, paired-build
comparisons or mutation contracts. Delete each with its caller/documentation
migration in the same PR once those gates pass. Preserve measurement history
and unique correctness regressions; retaining an old report does not require
retaining its executable harness forever.

## Repeated before/after comparisons

Use `compare` for two Stannum builds. Build each image from its own checkout
with `tin.py build-image`, retaining its output directory and `source.json`.
The baseline can come from an older checkout: unlike a normal `run`, `compare`
checks each image against its explicit build provenance. The source-file
fingerprints must hash to the recorded source identity, which must match the
image label. It retains both manifests and any accompanying `source.patch`;
these are provenance records, not a complete source archive.

```sh
python3 benchmarks/tin.py --driver benchmarks/results/tin-driver compare \
  --dataset /path/to/wikipedia-100000 --rows 1000 \
  --baseline-image stannum-bench:baseline \
  --baseline-source /path/to/baseline-build/source.json \
  --candidate-image stannum-bench:candidate \
  --candidate-source /path/to/candidate-build/source.json \
  --workload topk --style mixed --seconds 60 --warmup 10 \
  --repetitions 5 --output benchmarks/results/ranked-comparison-01
```

Image tags resolve once to immutable IDs before any trials. Each pair runs
baseline then candidate; the next runs candidate then baseline. Each trial
uses a fresh container and volume. The whole comparison holds the existing
machine timing lock, so do not wrap the command in another acquisition of
that lock. All trials use the same corpus prefix, driver, query trace, random
seed, offered update rate, client count and server settings. Two repetitions
are the minimum; five is the default. An A/A comparison using the same image
on both sides can help establish machine noise.

`paired.json` records every planned trial, its image and source identity, and
completion status. Each trial retains the usual raw samples, correctness
outputs and report. `aggregate.json` reports median, mean, sample standard
deviation, coefficient of variation, and range across trials, plus paired
QPS and latency ratios. Query latency ratios use baseline/candidate;
throughput ratios use candidate/baseline, so values above one favor the
candidate. These describe observed variation; they are not significance
claims. Inspect individual-query ratios as well as the overall mix.

All planned trials must complete and share actual PostgreSQL settings,
extension versions, imported CSV bytes, corpus manifest, driver and harness
fingerprints, and full-corpus query counts. Every query form in the requested
timed style must appear in every trial's samples. A trial that is too short
to traverse the trace invalidates the comparison; increase `--seconds` and
start a fresh output directory. Failed, interrupted, incompatible or partially
covered comparisons retain evidence but publish **no aggregate ratios**.
Update workloads must complete at least one update with no failed attempts;
the offered rate is not asserted as achieved throughput. Ranked runs also
require the exhaustive same-engine top-10 oracle to pass.
This does not establish full cross-version ranked-result equivalence.

Regenerate the aggregate without Docker using:

```sh
python3 benchmarks/tin.py report --output benchmarks/results/ranked-comparison-01
```

This supplies the repeated published-trace workflow that will replace
`paired.py`. Keep that older harness until an end-to-end comparison has passed
and its remaining callers and synthetic mutation coverage are migrated.

## Ten-minute local article rehearsal

`--style conjunction-phrase` runs both AND and phrase forms, excluding OR,
with the same seeded shuffle as the other styles. This matches the family mix
of the article's second chart; it is not an AND-only surrogate.

The four local article workloads use 100,000-document prefixes, two clients,
four CPUs, 2 GiB container memory, 128 MiB shared buffers and 64 MiB maintenance
memory. Each engine runs sequentially for 600 measured seconds after ten seconds
warmup. Ranked workloads use the raw Stack Exchange trace; disjunction counts
use the published Wikipedia trace. The disjunction ranked/write workload targets
1,000 updates per second and reports completed updates and errors separately.
These local subsets and resource limits are not the published full-corpus AWS
configuration. Preserve the original GIN and all other published series.

This iteration validates 90 explicitly selected forms against a 1,000-row
membership sample and exhaustive same-engine ranking, where applicable. All
forms remain in the timed trace; retain actual timed coverage and post-update
validation. Earlier full-trace correctness receipts remain separate evidence.
The marketing importer only accepts complete campaigns with at least 599 seconds
of recorded measurement per engine (the upstream stop boundary can be fractional).
One-second points are not extended to fill unmeasured time; run summaries come
from the retained samples and reports. There is no GIN-based hardware multiplier.
