# Published Wikipedia loader and memory-pressure pilot

The runner now accepts the exact prepared Wikipedia corpus and a selectable,
hashed query trace. These runs use an ordered 500,000-document prefix for the
memory experiment, not the full 5,032,104-document dataset used upstream.
Both successful query-memory runs use the same build image, input prefix,
4 Docker CPUs, 2 readers, 128 MiB shared buffers, 64 MiB maintenance_work_mem,
10-second warmup and 30-second mixed ranked top-10 measurement. Index builds
receive 2 GiB; query memory changes only after CREATE INDEX.

| Query memory | Read QPS | p95 ms | p99 ms | Timed forms | Query sampled peak MiB | Guest block reads MiB |
|---|---:|---:|---:|---:|---:|---:|
| 2g | 630.6 | 5.92 | 16.85 | 906 | 1792.3 | 1.8 |
| 512m | 560.3 | 6.91 | 18.02 | 906 | 512.0 | 5945.4 |

These are single diagnostic windows, not repeated capacity estimates or a TIN
comparison. Client timing includes local transport. Docker runs on the existing
ARM host with unrelated services; guest block reads can be served by host caches.
The resource sampler covers interior intervals and reports boundary gaps. Build
and lifetime memory peaks must not be mistaken for the query-window peak.

## Correctness and the failed construction run

The 1,000-document smoke checked all 906 query forms on Stannum and GIN. A
separate ranked Stannum smoke checked all 906 score multisets against exhaustive
same-engine scoring. Larger runs explicitly select 18 evenly spaced forms,
checking membership on 1,000 sampled rows and exhaustive top-10 scores/counts
over the imported corpus. This is not a full-corpus independent correctness
proof, nor an upstream Lead compatibility run. Timed traffic still uses all
906 forms; observed timed coverage is shown above.

An initial whole-lifecycle 512 MiB run was OOM-killed during CREATE INDEX.
The server logged a signal-9 termination and Docker recorded OOMKilled=true.
That failed run is retained, with no fabricated query measurements. A 2 GiB
whole-lifecycle control completed. This motivated the separate `--build-memory`
option: bulk construction and search have different memory requirements.

[Machine-readable results](published-memory-results.json) include the failed
pilot, successful smokes, image/corpus/trace/prefix identities, per-family
metrics, explicit validation IDs, and resource counters. Full raw logs, plans,
query-level samples and resource snapshots remain under
`benchmarks/results/published-{wikipedia-smoke,wikipedia-ranked-smoke,memory-pilot,query-memory}`.

## Next work

Repeat the query-memory matrix in alternating order before quantifying a
regression or speedup; profile construction allocations separately. Scale to
full Wikipedia after those controls. Stack Exchange is downloaded but its raw
mixed-case/punctuation text needs a tokenizer-aware membership oracle before
we can use it with this runner. The public 573-versus-1,254-record trace question
does not block these Wikipedia experiments.
