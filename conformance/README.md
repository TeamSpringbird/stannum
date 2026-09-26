# TIN interface conformance suite

An engine-agnostic correctness suite for the TIN full-text search interface:
the `==>` operator, the TINQL query language, the index access method and
the scoring and highlighting functions. It runs the same declarative cases
against any engine that implements the interface: today PlanetScale TIN
(`tin`) and Stannum (`stannum`), the open-source PostgreSQL extension. It
records one engine's answers and checks another engine's live answers
against them.

The suite lives in this repository, next to Stannum. The runner imports
nothing from the rest of the repository, so it can check any engine that
implements the interface.

```
conformance/
  README.md
  run.py                       the runner
  cases/<area>.yaml            declarative cases, grouped by area
  cases/catalog.*.yaml         the 140 cases of docs/tin-behavior-catalog.md §9
  expected/<engine>-<version>/<area>.json
                               recorded answers of one engine version
  divergences/<engine>.yaml    where an engine knowingly answers differently
```

## Running

Requirements: Python 3.9+, `pip install pyyaml psycopg`, and a PostgreSQL
database where the engine's extension is installed (or may be created by the
connecting role).

The connection string is read **only** from an environment variable, by
default `CONFORMANCE_DSN` (choose another with `--dsn-env NAME`). Never pass
it on the command line or commit it anywhere.

```sh
export CONFORMANCE_DSN='host=localhost port=5432 dbname=postgres'

# Check Stannum against TIN 1.0.3's recorded answers
python3 conformance/run.py --engine stannum --check conformance/expected/tin-1.0.3 --skip-crash

# Record an engine's answers into conformance/expected/<engine>-<version>/
python3 conformance/run.py --engine tin --record conformance/expected --host-note "PlanetScale, us-east-1"

# Only some cases
python3 conformance/run.py --engine stannum --check conformance/expected/tin-1.0.3 --area spans
python3 conformance/run.py --engine stannum --check conformance/expected/tin-1.0.3 --case 'bm25.k1.*'
```

With neither `--record` nor `--check` the runner only runs the cases and
reports which ones raised an ERROR.

For Stannum, `script/test-all conformance` runs the check against the
installed build in a throwaway cluster of its own; CI runs it on
PostgreSQL 18 after installing the release build (see `docs/testing.md`).

Every run creates a scratch schema `conformance_<random>`, builds each corpus
the selected cases use once (a table and an index in that schema), runs the
cases and drops the schema at the end, also when the run fails. Each capture
runs in its own transaction, which is rolled back, so cases cannot affect one
another. Settings are applied with `SET LOCAL`; the default statement timeout
is 60 s.

### Check mode

`--check EXPECTED_DIR` reads every `*.json` in the directory (all must come
from the same engine version), prints the engine version and source it
compares against, runs the cases that have a recorded answer (the rest report SKIP without running) and prints one line per case:

| status | meaning |
|--------|---------|
| `PASS` | every recorded capture equals the live answer |
| `DIFF` | compatible but not identical: both engines raised an ERROR with the same SQLSTATE but different message text (and the case requires no `message_prefix`). Reported, not a failure |
| `FAIL` | a capture differs, an ERROR has another SQLSTATE, the engine under test crashed the server, its connection was lost, or it could not build a corpus the recorded engine built |
| `SKIP` | no answer is recorded for the case (no expectation), or `--skip-crash` skipped it |
| `IMPROVED` | a documented improvement: the recorded engine refused with an ERROR and the engine under test answered (see Divergences) |
| `GAP` | a documented gap: the engine under test lacks what the case exercises (see Divergences) |

The run exits 1 if any case FAILs. Answers are compared exactly: id lists in
order, counts, float4 bit patterns, highlight text.

### Divergences

`divergences/<engine>.yaml` lists the cases where the engine under test
knowingly answers differently from a recorded engine version (`against`,
e.g. `tin-1.0.3`), naming the captures that differ:

- `kind: improvement`: the recorded engine raised an ERROR on each listed
  capture and this engine answers. Reported as `IMPROVED`, with a
  `question` for the recorded engine's authors (why is this not supported?).
- `kind: gap`: this engine lacks what the case exercises. Reported as `GAP`.

The entries cannot hide a regression: the case FAILs if a capture that is
not listed differs, a listed capture matches the recorded answer again, a
listed case matches entirely, or an improvement raises an ERROR where it
should answer. Whether a divergence is an improvement is a judgement the
entry records; the runner only checks its mechanics.

### Crashes

A case may be tagged `crashes: [tin-1.0.3, stannum-0.1.0]`: known to crash
the server with that engine version (a bare engine name, e.g. `stannum`, tags
every version). With `--skip-crash`, the runner skips cases tagged for the
engine and version under test; always use it on a server other sessions
share. Without it, tagged cases run as risky. Whether a case crashes can
depend on the server's environment (a stack overflow, for example, depends
on the postmaster's stack limit), so tag a case as soon as it crashes any
server, and run untagged-but-risky areas such as `query_size` on a
throwaway server first.

Crash detection uses an idle sentinel connection opened next to the working
one. When PostgreSQL reinitializes after a backend crash, it ends every
backend, so the sentinel dies too. Before and after each case marked
`risky: true` (and after any case whose connection drops), the runner checks
the sentinel; if it died, the case's result is `server_crashed`, and the
runner waits for crash recovery, reconnects and continues. A dropped working
connection with a live sentinel is reported as a lost connection instead.

If the recorded result of a case is `server_crashed` (the recorded engine
crashed on it), the engine under test must not crash: an ERROR passes, and an
answer passes if it equals the case's `expect_if_answered` (when the case
gives none, the answer is reported as unchecked and passes).

### Record mode

`--record OUTDIR` writes `OUTDIR/<engine>-<version>/<area>.json`, where the
version is the extension's `extversion` in `pg_extension`. To record a new
engine version into the suite, record into `conformance/expected` and commit
the new directory:

```sh
python3 conformance/run.py --engine tin --record conformance/expected --host-note "PlanetScale Postgres 18, us-east-1"
```

Each file starts with a `source` header, then the answers:

```json
{
 "source": {
  "engine": "tin",
  "extension_version": "1.0.4",
  "server_version": "PostgreSQL 18.6 ...",
  "host": "PlanetScale Postgres 18, us-east-1",
  "date": "2026-10-01T15:00:00Z",
  "suite_commit": "<git rev-parse HEAD of the suite>",
  "runner_version": "1"
 },
 "cases": [
  {"id": "span.then_terms.1", "captures": {"ids": [1, 2], "count": 2, "ranked": [2, 1]}},
  {"id": "query_size.nested.5000", "server_crashed": true, "detail": "..."}
 ]
}
```

A capture that raised an ERROR is recorded as
`{"error": {"sqlstate": ..., "message": ...}}`; a case whose corpus could not
be built (for example an index option the engine rejects) is recorded as
`{"corpus_error": {...}}`. With `--area` or `--case`,
the recorded cases are merged into existing files instead of replacing them.
Record with `--skip-crash` only if the recorded engine's crashes should stay
unrecorded; to record that an engine crashes, run without it on a server you
may restart.

## Provenance rules

- A directory under `expected/` holds answers of exactly one engine version,
  named `<engine>-<version>`, and every file in it names the engine, the
  extension version, the server version, the host, the date and how the
  answers were obtained (suite commit and runner version, or, for imported
  answers, `imported_from` and `measured_by`).
- Recorded answers are measurements, never edited by hand. To follow a new
  release, record a new directory; do not change the values of an existing
  one. A recorded answer that looks wrong stays as it is; the disagreement
  is the finding.
- `expected/tin-*` hold answers of PlanetScale TIN only, never answers
  produced by Stannum or derived from Stannum's code.
- `expected/tin-1.0.3/` holds TIN 1.0.3's answers for every area, recorded
  live by `run.py` at `64b7f8e` on 2026-09-26 against PostgreSQL 18.6 on
  PlanetScale (us-east-1): 186 of the 189 cases answered. The spans, BM25,
  query-size and minimal-interval answers were first measured the same day
  by `benchmarks/tin_behavior_probe.py` and a one-off psql probe; the live
  run reproduced them identically. `query_size.json` keeps its imported
  header (`imported_from`, `measured_by`): the live run skipped the three
  sizes that crash TIN's server, and their `server_crashed` records come
  from that first probe.
- A case without recorded answers for an engine is not a failure; it is
  reported as SKIP until someone records that engine.

## Case format

A case file is named after its area, `cases/<area>.yaml` (for example
`catalog.count.yaml`), the name of the recorded
`expected/<engine>-<version>/<area>.json`; the runner rejects a file whose
`area` differs from its name.

A case file has an `area`, an optional `description`, optional `defaults`
merged into every case (`settings` are merged key by key), named `corpora`,
and `cases`. Corpus names are global across files, so one area may use a
corpus another defines. Keys the runner does not know (for example the
`stannum_observations` of `query_size.yaml`) are kept as documentation.

```yaml
area: spans
defaults:
  corpus: spans4
  settings:
    enable_seqscan: "off"
  variants:                       # these captures must agree under every variant
    settings:
      - {"{engine}.enable_custom_scan": "on"}
      - {"{engine}.enable_custom_scan": "off"}
    captures: [ids, count]
  capture: [ids, count, ranked]

corpora:
  spans4:
    rows:                         # [id, body] or {id: ..., body: ...}
      - [1, alpha x beta gamma]
      - [2, alpha beta gamma]
    index_options: {}             # optional: CREATE INDEX ... WITH (key = value, ...)
    setup_sql: []                 # optional: statements run before the index is built

cases:
  - id: span.then_phrase_operand.1          # stable: never renumber or reuse
    description: A term then a phrase at most one position apart.
    query: 'alpha THEN/1 "beta gamma"'
```

Corpus fields, all optional:

- `rows`: `[id, body]` pairs (a body may be `null`).
- `pad: N`: adds N rows `(1000 + n, 'pad' || n)`, each with a unique term, so
  a small corpus has enough documents that its terms are not dense.
- `columns`: the table's columns (default `id int PRIMARY KEY, body text`).
- `setup_sql`: statements run after the rows are inserted, before the index.
- `index_options`: `WITH (...)` options of the default index
  `CREATE INDEX {index} ON {table} USING {engine}(body)`.
- `index_sql`: statements that replace the default index (an empty list
  builds no index).
- `after_index_sql`: statements run after the index is built (rows inserted
  here live in the index's write buffer).

If a corpus cannot be built, every case using it records a `corpus_error`
and the run continues.

Case fields:

- `id`: dotted words, `<area>.<topic>.<n>` or `catalog.<catalog id>`
  (e.g. `catalog.F-02`); stable forever.
- `description`: required. `corpus`: the case's corpus; a case that only
  calls functions (`value`, `error`) may omit it.
- `source`: where the expected behaviour is documented (the catalog cases
  cite `docs/tin-behavior-catalog.md` §9 and the behaviour row);
  `priority`: `conflict` for the §8 documentation conflicts, `edge` for
  error and edge cases. Both are documentation; the runner does not use
  them.
- `query`: a TINQL string, bound as a SQL literal to `{query}`; or
  `query_sql`: a SQL expression that computes the query (e.g.
  `repeat('a ', 1000)`).
- `capture`: a list of captures, each a name or a one-key mapping from the
  name to parameters (`as` renames the result, so one case can hold two
  captures of the same kind). Any capture may give its own `sql`, its own
  `query` or `query_sql`, its own `settings` (merged over the case's), and
  its own `corpus` (so one case can compare two index configurations).
  - `ids`: ids of `body ==> query`, ordered by id.
  - `count`: `count(*)` of the matches.
  - `ranked`: the top `k` (default 10) ids by `score` (default
    `{engine}.full_score(ctid)`) through `ORDER BY score DESC LIMIT k`,
    listed by score descending, ties by id.
  - `scores`: `encode(float4send(score), 'hex')` per matching id.
  - `highlight`: rows of `(id, {engine}.highlight(body))` for the matches, or
    the rows of the case's `sql`.
  - `value`: the rows of `sql` (a scalar when one row of one column).
  - `error`: runs `sql` (default: the `ids` query) and records the ERROR's
    SQLSTATE and message; with `message_prefix`, check mode also requires the
    live message to start with it.
  - `script`: ordered `steps`, each `{sql, session: a, capture: true}`, run
    in autocommit mode on fresh connections named by `session` (so a step may
    `BEGIN` in one session while another session reads), with the case's
    settings as session settings. The result lists the captured steps'
    rows or ERRORs. Use it for mutations (DELETE, VACUUM, REINDEX, ALTER
    INDEX) and for multi-session visibility; give such a case its own corpus,
    since the changes are not rolled back.
- `score`, `k`: defaults for the ranked and scores captures.
- `settings`: GUCs set with `SET LOCAL` for every capture; names may use
  `{engine}`.
- `variants`: see the example above.
- `risky: true`: check the sentinel connection before and after the case.
- `crashes: [<engine>-<version>, ...]`: known to crash that engine version.
- `expect_if_answered`: the correct captures if an engine answers a case
  the recorded engine crashed on.

SQL placeholders: `{engine}` (the engine's name, which is both its function
schema and its access method: `{engine}.full_score(ctid)`,
`USING {engine}(body)`), `{table}` (the corpus table), `{index}` (its
default index, `{table}_idx`), `{query}`, `{score}`, `{k}` and `{schema}`. Other braces, such as `'{}'` array literals, are left
alone.

Compare scores bit for bit only when both engines see identical corpus
statistics. Beware dense-term elision in `score()`: a term in at least 10%
of the documents scores 0, which turns a ranking into an id tiebreak; use
`full_score()` or `pad` when a case is about order.

### Adding a case

1. Add it to the area's file in `cases/` (or a new `cases/<area>.yaml`),
   with a new stable `id`, reusing a corpus where one fits.
2. Run it without recording to see that it works:
   `python3 conformance/run.py --engine stannum --case <id>`.
3. Record answers for the engines you have access to (see Record mode);
   engines without recorded answers report SKIP for it.
