# Exact prepared PlanetScale datasets

`benchmarks/published_dataset.py` acquires the public prepared datasets from
PlanetScale's benchmarker at pinned revision
`f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86`. It retains source descriptions,
query files, manifests, compressed parts and the exact decompressed CSV.

```sh
python3 benchmarks/published_dataset.py --corpus wikipedia \
  --output /path/to/datasets/planetscale-wikipedia
python3 benchmarks/published_dataset.py --corpus stackexchange \
  --output /path/to/datasets/planetscale-stackexchange
```

Run only one acquisition per output directory. Downloads resume partial files
and skip checksum-verified parts. Verification checks each part, the combined
gzip stream, and the full decompressed CSV against upstream sizes and SHA-256s.
Parts are slices of one gzip stream, not independent gzip archives. CSV becomes
`data.csv` only after verification. Interrupted decompression restarts from the
retained compressed parts. A corrupt partial download fails rather than being
accepted; remove that named partial file before retrying.

| Dataset | Upstream document count | Compressed bytes | CSV bytes |
|---|---:|---:|---:|
| Wikipedia | 5,032,104 | 2,685,744,356 | 8,093,810,896 |
| Stack Exchange | 150,000,000 | 28,137,896,404 | 84,521,226,768 |

During extraction, budget space for the CSV and two compressed copies; afterward
only the verified CSV and original compressed parts remain. Both corpora together
retain approximately 123 GB, before database heaps, indexes, WAL or build scratch.
The initial host had approximately 3.1 TiB free. Local dataset directories are
under `~/Library/Application Support/LeadBenchmarks/datasets/` and are not committed.

No truncation, normalization, ID remapping or row reordering is applied. This is
deliberately separate from our existing Hugging Face sample format. Use the explicit `--published-corpus wikipedia` option described below for the
raw Wikipedia directory. The default `--dataset` format remains the older sample
format. Stack Exchange uses tokenizer-aware heap membership checks; keep the separate
Lead differential gate for independent semantic validation.

For memory pressure, start with full Wikipedia while varying container memory
and collecting database/index sizes, memory peaks, reads and latency. Then move
to Stack Exchange with explicit build/query resource limits. Acquisition alone
is not a successful index build or a memory-pressure performance result. The
maintainer has confirmed the full Stack Exchange trace; see the
[source audit](tin-benchmark-source-audit.md).

The historical 573-record Stack Exchange trace was recovered from commit
`4b065a745d14fa34569f3c4afee5a3889f0c7e7c` and retained locally as
`queries.shortened.historical.json` (SHA-256
`e2bf0eb5d262134fa0241c5a12de6c998b72224c5ec70e5e6245d80c22053a8c`).
It expands to the article's erroneous 1,719 query forms. The maintainer has
confirmed that the full 1,254-record trace (3,762 forms) was used instead.
Keep the shortened file only as historical evidence. The standard downloader acquires the current
pinned trace, not this historical candidate.

Both full CSVs completed size and SHA-256 verification on September 20, 2026.
[Verification receipts](published-datasets-verification.json) retain the exact
pinned revision and CSV identities. This verifies upstream file identity; it
does not pin every run setting behind the article's charts; the maintainer has
separately confirmed the full Stack Exchange query file.

## Load the exact Wikipedia corpus through the benchmark

The published Wikipedia format is now supported directly:

```sh
python3 benchmarks/tin.py --driver /path/to/prepared-driver run \
  --published-corpus wikipedia --dataset /path/to/datasets/planetscale-wikipedia \
  --image stannum-bench:published-corpus --output benchmarks/results/published-01 \
  --rows 500000 --workload topk --style mixed --seconds 30 \
  --validation-rows 1000 --validation-queries 18 \
  --memory 2g --shared-buffers 128MB --maintenance-work-mem 64MB
```

The loader verifies the full CSV against the manifest in the pinned, verified
upstream driver. It imports the requested prefix in original order using text
IDs, without a primary-key index, matching the upstream table layout. CSV quoting
may change in the bounded import file; decoded field contents, embedded newlines,
Unicode and empty strings do not. Both full-source and imported-prefix hashes are
recorded. Duplicate IDs fail the membership-check precondition.

`--query-file /path/to/queries.json` selects an explicit trace. The runner snapshots
and hashes that JSON and uses the same snapshot for validation and timed traffic.
IDs must be unique. Wikipedia records require normalized ASCII query text; raw
Stack Exchange records preserve punctuation and case. Both require TIN
and PostgreSQL forms. The full pinned Wikipedia trace remains the default.

`--validation-queries 0` (default) checks every query form. A positive limit selects
evenly spaced forms for untimed membership, full-corpus counts and exhaustive
ranked checks. Selected IDs are saved in the manifest. It does not shrink the timed
trace, and it does not establish correctness for the unchecked forms. The small
smoke test still checks every form. Keep the independent Lead oracle separate.

`--published-corpus stackexchange` selects its own 1,254-sample query file by
default and preserves raw bodies and query text. Its sampled membership check
compares indexed results with the same engine's operator on an unindexed table,
using real tokenizer semantics. This detects index/evaluator disagreement but
is not independent of the engine: run the separate Lead differential gate too.
Cross-engine membership diagnostics use the Stannum operator versus GIN's
`simple` analyzer and retain disagreements; do not report those forms as
same-result performance comparisons. Wikipedia keeps its independent normalized
lexical check. Explicit `--query-file` overrides remain recorded and hashed.

`--build-memory 2g --memory 512m` separates index construction from query memory:
the isolated container starts with the build cap, then switches to the query cap
after CREATE INDEX, before VACUUM/validation/prewarm/timed traffic. Both limits
and the transition are recorded. Without `--build-memory`, the same cap applies
to the whole lifecycle. `--maintenance-work-mem` is independent of those caps;
PostgreSQL's setting is not a bound on all extension allocations.

## Published-corpus Lead compatibility gate

The separate differential oracle now accepts either prepared corpus without
normalizing bodies or remapping IDs. This uses the real Lead tokenizer and
compares membership, exact full-score bits and candidate top-ten results:

```sh
python3 benchmarks/oracle.py --left stannum.env --right lead.env \
  --dataset /path/to/planetscale-stackexchange \
  --published-corpus stackexchange --published-source /path/to/pinned-benchmarker \
  --trace /path/to/planetscale-stackexchange/queries.json \
  --reference-source /path/to/clean-lead-checkout --rows 100 \
  --budget-seconds 900 --output benchmarks/results/stackexchange-lead-100
```

The benchmarker checkout must contain the pinned Git revision; manifest and
trace are read from that immutable commit. The full CSV is checksum-verified
before extracting a prefix. Verification time includes that read, which can be
substantial for the 84.5 GB Stack Exchange CSV. Start with a small prefix to
measure the cost of all 3,762 forms before choosing a routine corpus size.
The existing 906-form Wikipedia campaign is unchanged.

Live checks now pass on 100 and 1,000 raw Stack Exchange documents across all
3,762 forms; the 1,000-row run took 421 seconds with zero differences. See the
[results and nonempty coverage](stackexchange-lead-smoke.md). Passing empty
result sets alone does not prove representative coverage. The timed Stack
Exchange guard remains until its membership checks are ready.
