#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Run the same queries on TIN and on Stannum over the same rows, and set
their plans, timings, page counts and answers side by side.

    # 1. Load N rows into each engine and build its index (VACUUM ANALYZE after)
    python3 benchmarks/compare/compare.py load --engine tin --dataset stackexchange --rows 100000
    script/pgrx-lock.py -- python3 benchmarks/compare/compare.py load --engine stannum \\
        --dataset stackexchange --rows 100000

    # 2. Measure every query shape on each engine
    python3 benchmarks/compare/compare.py measure --engine tin --dataset stackexchange --rows 100000 \\
        --output benchmarks/results/compare/se-100k
    script/pgrx-lock.py -- python3 benchmarks/compare/compare.py measure --engine stannum \\
        --dataset stackexchange --rows 100000 --output benchmarks/results/compare/se-100k

    # 3. Compare: answers, and one Markdown table per query shape
    python3 benchmarks/compare/compare.py report --output benchmarks/results/compare/se-100k

Connections come only from environment variables: TIN_DSN for TIN and
STANNUM_DSN for Stannum (choose others with --dsn-env); --dbname replaces
the database of the connection string. Nothing here prints them.

Datasets: `stackexchange` reads the first N rows of the published Stack
Exchange CSV (id, body, with a header), `wikipedia` the first N rows of
wikipedia-1000000/documents.csv (id, body, no header), both under
$LEAD_DATASETS (default ~/Library/Application Support/LeadBenchmarks/datasets);
--csv names another file. The table is compare.<dataset>_<rows>
(id bigint PRIMARY KEY, body text) with one index USING <engine>(body) and
default options.

Queries come from the published trace (planetscale-stackexchange/queries.json,
or --queries), which gives each query in conjunction, disjunction and phrase
form. --per-shape queries (default 8) are taken evenly from the token buckets
--buckets (default 2-6); single-term queries are the trace's one-word queries,
then the first words of the sampled ones. The shapes:

    term_top10 / term_count       one term, top 10 by score, count(*)
    and_top10 / and_count         the conjunction
    or_top10 / or_count           the disjunction
    phrase_top10 / phrase_count   the phrase
    and_filtered_top10            the conjunction AND id <= K, top 10
    or_filtered_top10             the disjunction AND id <= K, top 10
    or_tiebreak_top10             the disjunction, ORDER BY score DESC, id LIMIT 10

K is the 10th percentile of the loaded ids. Top 10 is ORDER BY
<engine>.score(ctid) DESC LIMIT 10. Each query runs once plainly for its
answer (ids and float4 score bits, or the count), then --discard (2) + --keep
(5) times under EXPLAIN (ANALYZE, BUFFERS, VERBOSE, FORMAT JSON) in one
session. The report gives the median execution time of the kept runs and the
counters of the run that had it: planning time, shared hit and read blocks
of the whole plan, the plan's node names, and every property the engine's
custom scan nodes add to EXPLAIN (for TIN: Page Touches by area, Predicted
Work, Count Strategy, Execution Mode, Execution Strategy, Elided Terms; for
Stannum: its counters, and Disk Pages By Area and Bytes Fetched by area).

Stannum keeps decoded index data in a per-backend cache
(stannum.reader_cache_mb), which BUFFERS does not see, so a warm run's shared
blocks understate its reads. Each query therefore also runs once first in a
fresh session (only the engine's library loaded; shared buffers warm, backend
caches empty): its blocks, the bytes Stannum fetched by area and TIN's Page
Touches are reported as "fresh". --no-fresh skips it.

--merge replaces only the measured shapes' results in an existing measure
file, so a slow shape can be measured apart with fewer runs (--shape
or_tiebreak_top10 --discard 0 --keep 1 --no-fresh --merge).

Results (JSON per engine, comparison JSON and Markdown) go under --output,
by default benchmarks/results/compare/<dataset>-<rows>/, which git ignores.
The two engines usually run on different hardware: compare pages and work,
and read milliseconds only as a rough guide.
"""

import argparse
import csv
import datetime
import itertools
import json
import os
from pathlib import Path
import statistics
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
DATASETS = Path(os.environ.get("LEAD_DATASETS",
                               Path.home() / "Library/Application Support/LeadBenchmarks/datasets"))
CSV_FILES = {
    "stackexchange": ("planetscale-stackexchange/data.csv", True),
    "wikipedia": ("wikipedia-1000000/documents.csv", False),
}
DEFAULT_QUERIES = "planetscale-stackexchange/queries.json"
DSN_ENV = {"tin": "TIN_DSN", "stannum": "STANNUM_DSN"}
SHAPES = ("term_top10", "term_count", "and_top10", "and_count", "or_top10", "or_count",
          "phrase_top10", "phrase_count", "and_filtered_top10", "or_filtered_top10", "or_tiebreak_top10")
TIN_AREAS = ("Metadata", "Term Map", "Postings Footer", "Postings Payload", "Postings TF Tail",
             "DL Sidecar", "Positions", "Liveness Bitmap")
STANDARD_KEYS = {
    "Node Type", "Parent Relationship", "Custom Plan Provider", "Parallel Aware", "Async Capable",
    "Startup Cost", "Total Cost", "Plan Rows", "Plan Width", "Actual Startup Time", "Actual Total Time",
    "Actual Rows", "Actual Loops", "Disabled", "Output", "Relation Name", "Schema", "Alias", "Plans",
    "Workers", "Workers Planned", "Workers Launched", "Subplan Name", "Filter", "Rows Removed by Filter",
}


# ---------------------------------------------------------------- connections


def psycopg_modules():
    """psycopg is imported only by the commands that connect, so the unit tests run without it."""
    try:
        import psycopg
        from psycopg import sql
        from psycopg.conninfo import make_conninfo
    except ImportError as error:  # pragma: no cover - environment guidance
        sys.exit(f"{error}; install the dependencies with: pip install -r benchmarks/requirements.txt")
    return psycopg, sql, make_conninfo


def connect(engine, args):
    psycopg, _, make_conninfo = psycopg_modules()
    name = args.dsn_env or DSN_ENV[engine]
    dsn = os.environ.get(name)
    if not dsn:
        sys.exit(f"set the connection string for {engine} in the environment variable {name}")
    if args.dbname:
        dsn = make_conninfo(dsn, dbname=args.dbname)
    connection = psycopg.connect(dsn, autocommit=True, application_name="stannum-compare")
    row = connection.execute("SELECT extversion FROM pg_extension WHERE extname = %s", (engine,)).fetchone()
    if row is None:
        sys.exit(f"extension {engine} is not installed in the target database")
    return connection, row[0], connection.execute("SELECT version()").fetchone()[0]


def table_name(args):
    return f"{args.dataset}_{args.rows}"


def output_dir(args):
    return Path(args.output or ROOT / "benchmarks/results/compare" / f"{args.dataset}-{args.rows}")


def now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# ---------------------------------------------------------------- load


def read_rows(path, header, rows):
    csv.field_size_limit(1 << 30)
    with open(path, newline="", encoding="utf-8") as handle:
        reader = csv.reader(handle)
        if header:
            next(reader)
        for record in itertools.islice(reader, rows):
            yield int(record[0]), record[1]


def load(args):
    _, sql, _ = psycopg_modules()
    connection, version, server = connect(args.engine, args)
    relative, header = CSV_FILES[args.dataset]
    path = Path(args.csv) if args.csv else DATASETS / relative
    table = sql.Identifier("compare", table_name(args))
    index = sql.Identifier(f"{table_name(args)}_body")
    connection.execute(sql.SQL("SET statement_timeout = {}").format(sql.Literal(f"{args.timeout_minutes}min")))
    connection.execute("CREATE SCHEMA IF NOT EXISTS compare")
    connection.execute(sql.SQL("DROP TABLE IF EXISTS {}").format(table))
    connection.execute(sql.SQL("CREATE TABLE {} (id bigint, body text)").format(table))
    timings, started = {}, time.monotonic()
    count = 0
    with connection.cursor() as cursor:
        with cursor.copy(sql.SQL("COPY {} (id, body) FROM STDIN").format(table)) as copy:
            for row in read_rows(path, header, args.rows):
                copy.write_row(row)
                count += 1
    timings["copy_s"] = round(time.monotonic() - started, 1)
    print(f"copied {count} rows in {timings['copy_s']} s", flush=True)
    started = time.monotonic()
    connection.execute(sql.SQL("ALTER TABLE {} ADD PRIMARY KEY (id)").format(table))
    timings["primary_key_s"] = round(time.monotonic() - started, 1)
    started = time.monotonic()
    connection.execute(sql.SQL("CREATE INDEX {} ON {} USING {}(body)").format(
        index, table, sql.Identifier(args.engine)))
    timings["create_index_s"] = round(time.monotonic() - started, 1)
    print(f"built the {args.engine} index in {timings['create_index_s']} s", flush=True)
    started = time.monotonic()
    connection.execute(sql.SQL("VACUUM ANALYZE {}").format(table))
    timings["vacuum_analyze_s"] = round(time.monotonic() - started, 1)
    sizes = connection.execute(sql.SQL(
        "SELECT pg_table_size({t}), pg_relation_size({i}), pg_database_size(current_database())").format(
        t=sql.Literal(f"compare.{table_name(args)}"), i=sql.Literal(f"compare.{table_name(args)}_body"))).fetchone()
    segments = connection.execute(sql.SQL("SELECT count(*) FROM {}.segment_info({})").format(
        sql.Identifier(args.engine), sql.Literal(f"compare.{table_name(args)}_body"))).fetchone()[0]
    record = {"engine": args.engine, "extension_version": version, "server_version": server,
              "dataset": args.dataset, "csv": path.name, "rows": count, "date": now(), "timings": timings,
              "table_bytes": sizes[0], "index_bytes": sizes[1], "database_bytes": sizes[2], "segments": segments}
    out = output_dir(args)
    out.mkdir(parents=True, exist_ok=True)
    (out / f"load-{args.engine}.json").write_text(json.dumps(record, indent=1) + "\n")
    print(json.dumps(record, indent=1))


# ---------------------------------------------------------------- queries


def parse_buckets(text):
    low, _, high = text.partition("-")
    return set(range(int(low), int(high or low) + 1))


def select_queries(args):
    trace = json.loads(Path(args.queries or DATASETS / DEFAULT_QUERIES).read_text())["queries"]
    buckets = parse_buckets(args.buckets)
    pool = [q for q in sorted(trace, key=lambda q: q["source_id"]) if q["token_bucket"] in buckets]
    step = max(1, len(pool) // args.per_shape)
    sampled = pool[::step][:args.per_shape]
    singles = [q["conjunction"] for q in sorted(trace, key=lambda q: q["source_id"]) if q["token_bucket"] == 1]
    for q in sampled:
        first = q["text"].split()[0]
        if len(singles) >= args.per_shape:
            break
        if first not in singles:
            singles.append(first)
    chosen = {"term": [(f"t{n}", text) for n, text in enumerate(singles[:args.per_shape], 1)]}
    for style in ("conjunction", "disjunction", "phrase"):
        chosen[style] = [(f"q{q['source_id']}", q[style]) for q in sampled]
    return chosen


def statements(engine, table, shape, query, cutoff):
    """(answer SQL, measured SQL) for one shape and query; the query is inlined as a literal."""
    score = f"{engine}.score(ctid)"
    literal = "'" + query.replace("'", "''") + "'"
    where = f"body ==> {literal}"
    if "filtered" in shape:
        where += f" AND id <= {cutoff}"
    if shape.endswith("count"):
        measured = f"SELECT count(*) FROM compare.{table} WHERE {where}"
        return measured, measured
    order = f"{score} DESC, id" if "tiebreak" in shape else f"{score} DESC"
    measured = f"SELECT id, {score} AS score FROM compare.{table} WHERE {where} ORDER BY {order} LIMIT 10"
    answer = (f"SELECT id, encode(float4send(score), 'hex') FROM ({measured}) top "
              "ORDER BY score DESC, id")
    return answer, measured


def shape_queries(chosen, shape):
    style = {"term": "term", "and": "conjunction", "or": "disjunction", "phrase": "phrase"}[shape.split("_")[0]]
    return chosen[style]


# ---------------------------------------------------------------- EXPLAIN


def walk(node):
    yield node
    for child in node.get("Plans") or []:
        yield from walk(child)


def node_label(node):
    provider = node.get("Custom Plan Provider")
    return f"{node['Node Type']} ({provider})" if provider else node["Node Type"]


def engine_properties(node):
    return {key: value for key, value in node.items()
            if key not in STANDARD_KEYS and not key.endswith(" Blocks") and not key.startswith("I/O")
            and not key.startswith("WAL")}


def summarize_plan(document):
    plan = document["Plan"]
    nodes = list(walk(plan))
    custom = [{"node": node_label(n), **engine_properties(n)} for n in nodes if n["Node Type"] == "Custom Scan"]
    summary = {
        "execution_ms": document.get("Execution Time"),
        "planning_ms": document.get("Planning Time"),
        "shared_hit": plan.get("Shared Hit Blocks", 0),
        "shared_read": plan.get("Shared Read Blocks", 0),
        "planning_shared_hit": (document.get("Planning") or {}).get("Shared Hit Blocks", 0),
        "planning_shared_read": (document.get("Planning") or {}).get("Shared Read Blocks", 0),
        "nodes": [node_label(n) for n in nodes],
        "custom_nodes": custom,
    }
    touches = {}
    for props in custom:
        for key, value in (props.get("Page Touches") or {}).items():
            if isinstance(value, (int, float)):
                touches[key] = touches.get(key, 0) + value
    if touches:
        summary["page_touches"] = touches
    for key, name in (("Count Strategy", "count_strategy"), ("Execution Mode", "execution_mode"),
                      ("Elided Terms", "elided_terms")):
        values = [str(p[key]) for p in custom if key in p]
        if values:
            summary[name] = "; ".join(dict.fromkeys(values))
    strategies = ["/".join(str(p["Execution Strategy"].get(k, "")) for k in ("Strategy", "Postings", "Positions"))
                  for p in custom if isinstance(p.get("Execution Strategy"), dict)]
    if strategies:
        summary["execution_strategy"] = "; ".join(dict.fromkeys(strategies))
    predicted = {}
    for props in custom:
        if "Index" in props and isinstance(props.get("Predicted Work"), dict):
            for key, value in props["Predicted Work"].items():
                predicted[key] = round(predicted.get(key, 0) + value, 2)
    if predicted:
        summary["predicted_work"] = predicted
    # Stannum: index pages read into its backend cache, and bytes fetched, by area ("name n, name n").
    for key, name in (("Disk Pages By Area", "stannum_pages_by_area"), ("Bytes Fetched", "stannum_bytes_by_area")):
        areas = {}
        for props in custom:
            for part in filter(None, str(props.get(key, "")).split(", ")):
                area, _, value = part.rpartition(" ")
                if value.isdigit():
                    areas[area] = areas.get(area, 0) + int(value)
        if areas:
            summary[name] = areas
    numbers = {}
    for props in custom:
        for key, value in props.items():
            if isinstance(value, (int, float)) and not isinstance(value, bool):
                numbers[key] = numbers.get(key, 0) + value
    if numbers:
        summary["custom_counters"] = numbers
    return summary


def fresh_session_run(engine, args, measured_sql):
    """One EXPLAIN ANALYZE in a new session, after only loading the engine's
    library: the backend's own caches are empty, shared buffers are warm."""
    connection, _, _ = connect(engine, args)
    try:
        connection.execute("SET statement_timeout = '120s'")
        for setting in args.set or []:
            name, _, value = setting.partition("=")
            connection.execute("SELECT set_config(%s, %s, false)", (name, value))
        connection.execute(f"SELECT {engine}.maybe_quote('x')").fetchone()
        document = connection.execute(
            "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, FORMAT JSON) " + measured_sql).fetchone()[0][0]
        return summarize_plan(document)
    finally:
        connection.close()


def measure(args):
    psycopg, _, _ = psycopg_modules()
    connection, version, server = connect(args.engine, args)
    table = table_name(args)
    cutoff = connection.execute(
        f"SELECT percentile_disc(0.1) WITHIN GROUP (ORDER BY id) FROM compare.{table}").fetchone()[0]
    rows = connection.execute(f"SELECT count(*) FROM compare.{table}").fetchone()[0]
    connection.execute("SET statement_timeout = '120s'")
    for setting in args.set or []:
        name, _, value = setting.partition("=")
        connection.execute("SELECT set_config(%s, %s, false)", (name, value))
    # Load the library and warm the catalog caches before the first measurement.
    connection.execute(f"SELECT count(*) FROM compare.{table} WHERE body ==> 'warmup'").fetchone()
    chosen = select_queries(args)
    results = []
    shapes = [s for s in SHAPES if not args.shape or s in args.shape]
    for shape in shapes:
        for query_id, query in shape_queries(chosen, shape):
            answer_sql, measured_sql = statements(args.engine, table, shape, query, cutoff)
            record = {"shape": shape, "query_id": query_id, "query": query, "sql": measured_sql}
            try:
                if args.fresh:
                    fresh = fresh_session_run(args.engine, args, measured_sql)
                    record["fresh"] = {k: fresh.get(k) for k in (
                        "execution_ms", "planning_ms", "shared_hit", "shared_read", "page_touches",
                        "stannum_pages_by_area", "stannum_bytes_by_area", "custom_counters") if fresh.get(k) is not None}
                answer = connection.execute(answer_sql).fetchall()
                record["answer"] = answer[0][0] if shape.endswith("count") else [list(r) for r in answer]
                runs = []
                for _ in range(args.discard + args.keep):
                    document = connection.execute(
                        "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, FORMAT JSON) " + measured_sql).fetchone()[0][0]
                    runs.append(summarize_plan(document))
                kept = runs[args.discard:]
                median = statistics.median_low([run["execution_ms"] for run in kept])
                chosen_run = next(run for run in kept if run["execution_ms"] == median)
                record.update(chosen_run)
                record["execution_ms_kept"] = [run["execution_ms"] for run in kept]
            except psycopg.Error as error:
                record["error"] = {"sqlstate": error.sqlstate, "message": error.diag.message_primary or str(error)}
            results.append(record)
            status = record.get("error", {}).get("sqlstate") or f"{record['execution_ms']:.2f} ms"
            print(f"{args.engine:8} {shape:20} {query_id:6} {status}", flush=True)
    out = output_dir(args)
    out.mkdir(parents=True, exist_ok=True)
    path = out / f"measure-{args.engine}.json"
    runs = {shape: {"discard": args.discard, "keep": args.keep, "fresh": args.fresh} for shape in shapes}
    if args.merge and path.exists():
        previous = json.loads(path.read_text())
        replaced = {(r["shape"], r["query_id"]) for r in results}
        results = [r for r in previous["results"] if (r["shape"], r["query_id"]) not in replaced] + results
        results.sort(key=lambda r: SHAPES.index(r["shape"]))
        runs = {**previous.get("runs", {}), **runs}
    document = {"engine": args.engine, "extension_version": version, "server_version": server,
                "host": args.host_note, "date": now(), "dataset": args.dataset, "rows": rows,
                "id_cutoff": cutoff, "discard": args.discard, "keep": args.keep, "runs": runs,
                "settings": args.set or [], "results": results}
    path.write_text(json.dumps(document, indent=1) + "\n")
    print(f"wrote {path}")


# ---------------------------------------------------------------- report


def equality(shape, tin, stannum):
    """How the two answers agree: equal, equal up to ties at the 10th score, scores differ, or differ."""
    if "error" in tin or "error" in stannum:
        return "error"
    a, b = tin["answer"], stannum["answer"]
    if a == b:
        return "equal"
    if shape.endswith("count"):
        return "differ"
    if len(a) == len(b) and a and sorted(s for _, s in a) == sorted(s for _, s in b):
        last = a[-1][1]
        if [r for r in a if r[1] != last] == [r for r in b if r[1] != last]:
            return "equal up to ties"
    if [r[0] for r in a] == [r[0] for r in b]:
        return "scores differ"
    return "differ"


def median(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def mean(values):
    values = [v for v in values if v is not None]
    return sum(values) / len(values) if values else None


def fmt(value, digits=1):
    if value is None:
        return "-"
    if isinstance(value, float):
        return f"{value:,.{digits}f}"
    return f"{value:,}"


def report(args):
    out = output_dir(args)
    measured = {engine: json.loads((out / f"measure-{engine}.json").read_text()) for engine in ("tin", "stannum")}
    loads = {engine: json.loads(p.read_text()) for engine in ("tin", "stannum")
             if (p := out / f"load-{engine}.json").exists()}
    by_key = {engine: {(r["shape"], r["query_id"]): r for r in doc["results"]} for engine, doc in measured.items()}
    keys = [k for k in by_key["tin"] if k in by_key["stannum"]]
    comparison, lines = [], []
    tin_doc, stannum_doc = measured["tin"], measured["stannum"]
    lines += [f"# TIN vs Stannum: {tin_doc['dataset']}, {tin_doc['rows']:,} rows", "",
              f"- TIN {tin_doc['extension_version']} on {tin_doc['server_version'].split(' on ')[0]}"
              f" ({tin_doc.get('host') or 'remote'}), measured {tin_doc['date']}",
              f"- Stannum {stannum_doc['extension_version']} on {stannum_doc['server_version'].split(' on ')[0]}"
              f" ({stannum_doc.get('host') or 'local'}), measured {stannum_doc['date']}",
              f"- Warm: {tin_doc['discard']} discarded + {tin_doc['keep']} kept EXPLAIN ANALYZE runs per query; "
              "medians of the kept runs, counters of the median run.",
              *[f"- {engine}: {shape} measured with {r['discard']} discarded + {r['keep']} kept runs"
                + ("" if r.get("fresh", True) else ", no fresh run")
                for engine, doc in measured.items() for shape, r in (doc.get("runs") or {}).items()
                if (r["discard"], r["keep"]) != (doc["discard"], doc["keep"]) or not r.get("fresh", True)],
              f"- Filter cutoff: id <= {tin_doc['id_cutoff']} (TIN) / {stannum_doc['id_cutoff']} (Stannum).",
              "- The servers differ in hardware and settings: compare pages and work; milliseconds are a rough guide.",
              ""]
    if loads:
        lines += ["| engine | rows | COPY s | CREATE INDEX s | VACUUM ANALYZE s | table MiB | index MiB | segments |",
                  "|---|---:|---:|---:|---:|---:|---:|---:|"]
        for engine, load_doc in loads.items():
            t = load_doc["timings"]
            lines.append(f"| {engine} | {load_doc['rows']:,} | {t['copy_s']} | {t['create_index_s']} | "
                         f"{t['vacuum_analyze_s']} | {load_doc['table_bytes'] / 2**20:,.1f} | "
                         f"{load_doc['index_bytes'] / 2**20:,.1f} | {load_doc['segments']} |")
        lines.append("")
    for key in keys:
        tin, stannum = by_key["tin"][key], by_key["stannum"][key]
        comparison.append({"shape": key[0], "query_id": key[1], "query": tin["query"],
                           "equality": equality(key[0], tin, stannum),
                           "tin": {k: v for k, v in tin.items() if k not in ("answer", "sql")},
                           "stannum": {k: v for k, v in stannum.items() if k not in ("answer", "sql")},
                           "tin_answer": tin.get("answer"), "stannum_answer": stannum.get("answer")})
    lines += ["## Per shape", "",
              "Warm: medians over the shape's queries of each query's median execution time, and means of shared "
              "blocks (hit + read, the whole plan). Fresh: the same query's first run in a new session (shared "
              "buffers warm, backend caches empty): mean shared blocks; below, TIN's Page Touches and the index "
              "bytes Stannum fetched into its cache (Bytes Fetched), by area.", "",
              "| shape | n | TIN ms | Stannum ms | Stannum/TIN | TIN blocks | Stannum blocks | "
              "TIN fresh ms | Stannum fresh ms | TIN fresh blocks | Stannum fresh blocks | answers equal |",
              "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|"]
    shapes = list(dict.fromkeys(c["shape"] for c in comparison))
    area_lines = ["", "## Where the pages go (fresh run, mean per query)", "",
                  "| shape | TIN touches | TIN touches by area | Stannum fetched (8 KiB pages) | "
                  "Stannum KiB fetched by area | "
                  "TIN plan | Stannum plan | TIN strategy |",
                  "|---|---:|---|---:|---|---|---|---|"]

    def fresh_blocks(side):
        fresh = side.get("fresh") or {}
        if "shared_hit" not in fresh:
            return None
        return fresh.get("shared_hit", 0) + fresh.get("shared_read", 0)

    def by_area(rows, side, key):
        areas = {}
        for c in rows:
            for area, value in ((c[side].get("fresh") or {}).get(key) or {}).items():
                areas[area] = areas.get(area, 0) + value
        return {a: v / len(rows) for a, v in areas.items() if a not in ("Total", "Approx MiB") and v}

    def plan_of(rows, side):
        plans = {}
        for c in rows:
            label = " > ".join(n.replace("Custom Scan ", "") for n in c[side].get("nodes") or ["error"])
            plans[label] = plans.get(label, 0) + 1
        return "; ".join(f"{label}" + (f" ({n})" if len(plans) > 1 else "") for label, n in plans.items())

    for shape in shapes:
        rows = [c for c in comparison if c["shape"] == shape]
        t_ms = median([c["tin"].get("execution_ms") for c in rows])
        s_ms = median([c["stannum"].get("execution_ms") for c in rows])
        t_pages = mean([c["tin"].get("shared_hit", 0) + c["tin"].get("shared_read", 0) for c in rows
                        if "error" not in c["tin"]])
        s_pages = mean([c["stannum"].get("shared_hit", 0) + c["stannum"].get("shared_read", 0) for c in rows
                        if "error" not in c["stannum"]])
        areas = {}
        for c in rows:
            for area, value in (c["tin"].get("page_touches") or {}).items():
                areas.setdefault(area, []).append(value)
        touches = mean([(c["tin"].get("page_touches") or {}).get("Total") for c in rows])
        breakdown = ", ".join(f"{area} {sum(v) / len(rows):,.0f}" for area, v in areas.items()
                              if area not in ("Total", "Approx MiB") and sum(v))
        modes = "; ".join(dict.fromkeys(
            " ".join(filter(None, [c["tin"].get("count_strategy"), c["tin"].get("execution_mode")]))
            for c in rows if c["tin"].get("count_strategy") or c["tin"].get("execution_mode"))) or "-"
        equal = sum(1 for c in rows if c["equality"] == "equal")
        ties = sum(1 for c in rows if c["equality"] == "equal up to ties")
        verdict = f"{equal}/{len(rows)}" + (f" (+{ties} up to ties)" if ties else "")
        ratio = s_ms / t_ms if t_ms and s_ms else None
        t_fresh_ms = median([(c["tin"].get("fresh") or {}).get("execution_ms") for c in rows])
        s_fresh_ms = median([(c["stannum"].get("fresh") or {}).get("execution_ms") for c in rows])
        lines.append(f"| {shape} | {len(rows)} | {fmt(t_ms, 2)} | {fmt(s_ms, 2)} | {fmt(ratio, 2)} | "
                     f"{fmt(t_pages)} | {fmt(s_pages)} | {fmt(t_fresh_ms, 2)} | {fmt(s_fresh_ms, 2)} | "
                     f"{fmt(mean([fresh_blocks(c['tin']) for c in rows]))} | "
                     f"{fmt(mean([fresh_blocks(c['stannum']) for c in rows]))} | {verdict} |")
        t_areas = by_area(rows, "tin", "page_touches")
        s_areas = by_area(rows, "stannum", "stannum_bytes_by_area")
        fresh_touches = mean([((c["tin"].get("fresh") or {}).get("page_touches") or {}).get("Total") for c in rows])
        area_lines.append(
            f"| {shape} | {fmt(fresh_touches if fresh_touches is not None else touches)} | "
            f"{', '.join(f'{a} {v:,.1f}' for a, v in t_areas.items()) or breakdown or '-'} | "
            f"{fmt(sum(s_areas.values()) / 8192 if s_areas else None)} | "
            f"{', '.join(f'{a} {v / 1024:,.0f}' for a, v in s_areas.items()) or '-'} | "
            f"{plan_of(rows, 'tin')} | {plan_of(rows, 'stannum')} | {modes} |")
    lines += area_lines
    lines += ["", "## Per query", ""]
    for shape in shapes:
        lines += [f"### {shape}", "",
                  "| query | TIN ms | Stannum ms | TIN plan ms | Stannum plan ms | TIN hit/read | Stannum hit/read |"
                  " TIN touches | TIN strategy | Stannum counters | answers |",
                  "|---|---:|---:|---:|---:|---:|---:|---:|---|---|---|"]
        for c in (c for c in comparison if c["shape"] == shape):
            t, s = c["tin"], c["stannum"]
            counters = ", ".join(f"{k} {fmt(v, 0)}" for k, v in (s.get("custom_counters") or {}).items()
                                 if k not in ("Startup Cost", "Total Cost")) or "-"
            query = c["query"] if len(c["query"]) <= 48 else c["query"][:45] + "..."
            lines.append(
                f"| `{query.replace('|', '/')}` | {fmt(t.get('execution_ms'), 2)} | {fmt(s.get('execution_ms'), 2)} | "
                f"{fmt(t.get('planning_ms'), 2)} | {fmt(s.get('planning_ms'), 2)} | "
                f"{t.get('shared_hit', '-')}/{t.get('shared_read', '-')} | "
                f"{s.get('shared_hit', '-')}/{s.get('shared_read', '-')} | "
                f"{fmt((t.get('page_touches') or {}).get('Total'))} | "
                f"{t.get('execution_strategy') or t.get('count_strategy') or '-'} | {counters} | {c['equality']} |")
        lines.append("")
    (out / "comparison.json").write_text(json.dumps(comparison, indent=1) + "\n")
    (out / "comparison.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines[:lines.index("## Per query")]))
    print(f"wrote {out / 'comparison.md'} and {out / 'comparison.json'}")


# ---------------------------------------------------------------- main


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("load", "measure", "report"):
        sub = commands.add_parser(name)
        sub.add_argument("--dataset", choices=sorted(CSV_FILES), default="stackexchange")
        sub.add_argument("--rows", type=int, required=True)
        sub.add_argument("--output", help="results directory (default benchmarks/results/compare/<dataset>-<rows>)")
        if name in ("load", "measure"):
            sub.add_argument("--engine", choices=["tin", "stannum"], required=True)
            sub.add_argument("--dsn-env", help="environment variable with the connection string "
                             "(default TIN_DSN or STANNUM_DSN)")
            sub.add_argument("--dbname", help="use this database instead of the connection string's")
        if name == "load":
            sub.add_argument("--csv", help="read this CSV (id, body) instead of the dataset's")
            sub.add_argument("--timeout-minutes", type=int, default=30,
                             help="statement_timeout of every load statement (default 30)")
        if name == "measure":
            sub.add_argument("--queries", help="published query trace (default the Stack Exchange queries.json)")
            sub.add_argument("--per-shape", type=int, default=8)
            sub.add_argument("--buckets", default="2-6", help="token buckets to sample, e.g. 2-6")
            sub.add_argument("--shape", action="append", choices=SHAPES, help="only this shape (repeatable)")
            sub.add_argument("--discard", type=int, default=2)
            sub.add_argument("--keep", type=int, default=5)
            sub.add_argument("--set", action="append", metavar="NAME=VALUE", help="a session setting (repeatable)")
            sub.add_argument("--no-fresh", dest="fresh", action="store_false",
                             help="skip the extra run of each query in a fresh session")
            sub.add_argument("--merge", action="store_true",
                             help="replace only the measured shapes' results in an existing measure file")
            sub.add_argument("--host-note", default="", help="free-text host description")
    args = parser.parse_args()
    {"load": load, "measure": measure, "report": report}[args.command](args)


if __name__ == "__main__":
    main()
