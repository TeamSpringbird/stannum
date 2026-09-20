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
deliberately separate from our existing Hugging Face sample format. The current
`tin.py run --dataset` expects that older format: do not point it at these raw
upstream directories yet. Next, add a loader preserving their text IDs and exact
body bytes, make Wikipedia/Stack Exchange trace selection explicit, and separate
bounded correctness validation from the full-scale performance run.

For memory pressure, start with full Wikipedia while varying container memory
and collecting database/index sizes, memory peaks, reads and latency. Then move
to Stack Exchange with explicit build/query resource limits. Acquisition alone
is not a successful index build or a memory-pressure performance result. The
published trace identity discrepancy remains unresolved; see the
[source audit](tin-benchmark-source-audit.md).

The historical 573-record Stack Exchange trace was recovered from commit
`4b065a745d14fa34569f3c4afee5a3889f0c7e7c` and retained locally as
`queries.shortened.historical.json` (SHA-256
`e2bf0eb5d262134fa0241c5a12de6c998b72224c5ec70e5e6245d80c22053a8c`).
It expands to the article's 1,719 query forms, but its deletion commit says it
was never used. Keep it separate from the current 1,254-record trace; neither
should silently replace the other. The standard downloader acquires the current
pinned trace, not this historical candidate.

Both full CSVs completed size and SHA-256 verification on September 20, 2026.
[Verification receipts](published-datasets-verification.json) retain the exact
pinned revision and CSV identities. This verifies upstream file identity; it
does not prove which query trace generated the article's charts.
