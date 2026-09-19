# Matched mutation/count baseline against GIN

The `mutation-count` profile runs identical term, Boolean and phrase membership
shapes for Stannum and PostgreSQL GIN alongside the same insert/delete/update
scripts. It does not run ranked queries: native ranking and BM25 are different
computations. Every successful mutation changes exactly one row under the
`live-key-wrap-v2` protocol.

The campaign accepts `--engines stannum,gin,gin-no-fastupdate`. GIN uses an
expression index on `to_tsvector('simple', body)`; the latter two variants set
`fastupdate=on` and `off` explicitly. They are not a comparison with a stored
`tsvector` column. The text input is identical. Corpus token normalization and
matching are checked against independent regex scans, not against either index.
The count-only oracle also checks exact membership, not just equal counts.

All variants share PostgreSQL settings, reader/write rates, client counts, fresh
databases, warmup, corpus, mutation scripts, and scheduled VACUUM policy. The
campaign reverses the full case/engine order on alternating repetitions. The
controlled benchmark disables table autovacuum and forces index access using
`enable_seqscan=off`; results describe this index-access workload rather than the
best plan PostgreSQL might choose with sequential scans allowed. Stannum storage
settings apply only to Stannum and are recorded separately. The campaign rejects on-disk harness source changes between windows. The comparison guard
permits engine-specific settings while rejecting changed reader rates, workload
profiles or PostgreSQL settings.

GIN pending pages/tuples are sampled using
[`pgstatginindex`](https://www.postgresql.org/docs/18/pgstattuple.html), including
before/after VACUUM and before/after final cleanup. These are pending-list counts,
not counts of all dead entries or a bound on maintenance debt. GIN cleanup can
also occur in foreground writes. Samples may miss short-lived peaks. The local
campaign requires the `pgstattuple` extension and its sampling privileges.

`campaign.json` records reader/writer scheduled latency, execution latency and
lag, actual affected rows, maintenance coverage, index size, and separate WAL
byte deltas for traffic/observer completion versus final drain. WAL is
cluster-wide; the private cluster isolates this campaign but includes all its
heap, primary-key and maintenance work. It is not search-index-only WAL.
Two cleanup passes after traffic are reported separately and do not establish
that maintenance kept pace during traffic.

## Run

```sh
python3 /tmp/stannum-pgrx-lock.py python3 benchmarks/sustained.py \
  --artifact /absolute/retained/stannum.dylib \
  --output benchmarks/results/gin-matched \
  --engines stannum,gin,gin-no-fastupdate --profile mutation-count \
  --rows 50000 --seconds 60 --rounds 3 --writer-counts 2 \
  --write-rates 2000,8000 --read-rate 500
```

Use `--writer-counts` (the connection counts), independent of `--write-rates`
(total offered transactions/sec). Use `--dataset /verified/corpus` with matching
`--rows` for the representative corpus. Compare one matching pair with:

```sh
python3 benchmarks/run.py compare /stannum/window /gin/window --cross-engine
```

The three-engine native smoke passed locally. CI runs this smoke on PG17/18 and
ARM/x86. No timing threshold is imposed on shared CI runners. [Local measurements](gin-mutation-results.md) cover repeated synthetic and
verified Wikipedia windows; short synthetic
results must not be described as a production capacity limit or universal
speedup over GIN.
