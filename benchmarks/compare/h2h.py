#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Head to head with TIN's recorded probes: the same rows, the same queries,
pages per area side by side.

TIN 1.0.4 was probed in three rounds (stannum-lab/tin-probes: round 1's
e2_topk, round 3's c7_shapes, c12_repeat and c68_segments) over one corpus,
corpus.py's 1M synthetic rows at setseed(0.42). This tool rebuilds that
corpus in a Stannum database, row for row and page for page, runs every
recorded query that Stannum supports with the probes' session protocol, and
sets Stannum's per-kind page counts next to TIN's unique pages per area.

    # 1. TIN's answers, from the probes' raw JSON (no connection)
    python3 benchmarks/compare/h2h.py reference --probes ~/stannum-lab/tin-probes \\
        --out OUT/tin-reference.json

    # 2. The corpus (schema probe3_base) and its N-segment copies
    python3 benchmarks/compare/h2h.py corpus --segments 1,2,4,8,16

    # 3. Stannum's counters for every reference case
    python3 benchmarks/compare/h2h.py measure --reference OUT/tin-reference.json \\
        --out OUT/stannum.json --rounds 3

    # 4. Tables
    python3 benchmarks/compare/h2h.py report --reference OUT/tin-reference.json \\
        --measured OUT/stannum.json --out OUT/tables.md

The connection string comes only from $STANNUM_DSN (or --dsn-env); nothing
here prints it.

Protocol (round 3's): each round opens a session, runs an unrelated query on
the same index (`w400 OR w401`, so catalog and per-backend decoding of the
document set and liveness are done), then runs the case three times under
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, FORMAT JSON). Run 1 is compared with
TIN's run 1: TIN keeps nothing between queries, while Stannum keeps parsed
term records and footers (warm runs read neither), so run 3 is reported
apart.

Areas. TIN counts unique pages per area. Stannum counts pages pinned per
blob kind within a query's span (`Native Reads By Kind`: a page is charged
to the first kind that pins it, once), plus the pages a scan reads outside
the segment reader (`Buffer Accesses By Phase`: view capture, scorer
setup). The mapping:

    TIN Metadata + Liveness   Stannum view capture + scorer setup phases
    TIN Term Map              kind other (term map, header, document set)
    TIN Footer                kind record (record header, group directory)
                              + kind footer (block frontiers)
    TIN Payload               kind container + kind sparse
    TIN TF Tail               kind tf
    TIN DL Sidecar            kind lengths + kind inline lengths
    TIN Positions             kind positions
"""

import argparse
import json
import os
from pathlib import Path
import re
import statistics
import sys
import time

# corpus.py of the TIN probes, verbatim: the probe terms, their document
# frequencies and the generator. One session must run setseed and the
# CREATE TABLE.
PROBES = {"t001": 0.0001, "t01": 0.001, "t1": 0.01, "t10": 0.10, "t30": 0.30, "t50": 0.50}


def corpus_sql(schema, table, n, seed=0.42):
    parts = []
    for t, p in PROBES.items():
        parts.append(
            f"case when random() < {p} then repeat(' {t}', 1 + floor(random()*random()*4)::int) else '' end"
        )
    filler = " || ' ' || ".join(["'w' || floor(exp(random()*ln(5000)))::int"] * 8)
    extra = "repeat(' w' || floor(exp(random()*ln(5000)))::int, floor(random()*random()*30)::int)"
    body = f"{filler} || {extra} || " + " || ".join(parts)
    title = (
        "'w' || floor(exp(random()*ln(5000)))::int || ' w' || floor(exp(random()*ln(5000)))::int"
        " || case when random() < 0.05 then ' t5title' else '' end"
    )
    return [
        f"select setseed({seed})",
        f"""create table {schema}.{table} as
            select i as id, (random()*100)::int as cat, ({body}) as body, ({title}) as title
            from generate_series(1, {n}) i""",
        f"alter table {schema}.{table} add primary key (id)",
        f"create index {table}_cat on {schema}.{table}(cat)",
    ]


# What TIN recorded for the corpus (round 1's e2 and round 3's r1_setup).
EXPECTED = {
    "rows": 1_000_000,
    "pages_with_rows": 14_853,
    "rows_per_page": "67.3264660337978859",
    "df": {"t001": 98, "t01": 968, "t1": 9926, "t10": 100216, "t30": 300579, "t50": 499554},
    "sum_doc_lengths": 16_470_082,
    "npostings": 9_347_710,
}

BASE = "probe3_base.c"
WARM_OTHER = "select id from {table} where body ==> 'w400 OR w401' order by stannum.full_score(ctid) desc limit 10"


def table_for(segments):
    return BASE if segments == 1 else f"probe3_s{segments}.c"


def connect(args):
    import psycopg

    dsn = os.environ.get(args.dsn_env)
    if not dsn:
        sys.exit(f"set ${args.dsn_env} to the Stannum database's connection string")
    return psycopg.connect(dsn, autocommit=True, application_name="stannum-h2h")


def rows(conn, sql):
    with conn.cursor() as cur:
        cur.execute(sql)
        return cur.fetchall() if cur.description else []


# --- TIN reference -----------------------------------------------------------


def stannum_sql(sql, segments=1):
    """A TIN probe statement as Stannum's: its functions and our table."""
    sql = sql.replace("tin.full_score(", "stannum.full_score(").replace("tin.score(", "stannum.score(")
    for table in ("probe_e2.c", "probe3_base.c"):
        sql = sql.replace(f"{table} ", f"{table_for(segments)} ")
    return sql


def short_areas(touches):
    names = {
        "Metadata": "M", "Term Map": "TM", "Postings Footer": "F", "Postings Payload": "P",
        "Postings TF Tail": "TF", "DL Sidecar": "DL", "Positions": "Pos", "Liveness Bitmap": "L",
    }
    return {short: int(touches[name]) for name, short in names.items() if touches.get(name)}


def reference(args):
    probes = Path(args.probes).expanduser()
    cases = []

    def add(group, name, segments, sql, uniq, exec_ms, warm_ms=None, extra=None):
        cases.append({
            "group": group, "case": name, "segments": segments, "tin_sql": sql,
            "sql": stannum_sql(sql, segments), "tin": {"uniq": uniq, "exec_ms": exec_ms, "warm_ms": warm_ms, **(extra or {})},
        })

    # Round 1, E2: single terms over df, k, score() vs full_score(), counts.
    e2 = json.loads((probes / "results/e2_topk.json").read_text())
    for term, runs in e2["runs"].items():
        for key, sm in runs.items():
            if "exh" in key or key == "count_scan":
                continue  # TIN debug settings Stannum lacks
            if key == "count":
                sql = f"select count(*) from probe_e2.c where body ==> '{term}'"
            else:
                fn, k = key.rsplit("_k", 1)
                sql = f"select id from probe_e2.c where body ==> '{term}' order by tin.{fn}(ctid) desc limit {k}"
            add("r1_e2", f"{term}/{key}", 1, sql, short_areas(sm.get("unique") or sm["touches"]), sm.get("exec_ms"))

    # Round 3, C6: the nine queries at N = 1, 2, 4, 8, 16 segments.
    c68 = json.loads((probes / "round3/results/c68_segments.json").read_text())
    sys.path.insert(0, str(probes / "round3"))
    queries = c68_queries()
    for n, res in c68["by_n"].items():
        for name, q in res["q"].items():
            s = q["summary"]
            first = [r[0] for r in q["rows"]]
            add("r3_segments", name, int(n), queries[name], s["uniq1"], s["A1_exec"], s["A3_exec"], {
                "plan_ms": s["plan_ms"], "plan_hit": s["plan_hit"],
                "tot": s["tot1"], "exec_runs": [r["exec_ms"] for r in first],
            })

    # Round 3, C1: or4_k100 and score_or_k10 (the others repeat C6's).
    c12 = json.loads((probes / "round3/results/c12_repeat_seg1.json").read_text())
    for name, sql in c12["queries"].items():
        a = [rd[name]["A"] for rd in c12["rounds"]]
        add("r3_repeat", name, 1, sql, a[0][0]["uniq"], statistics.median(x[0]["exec_ms"] for x in a),
            statistics.median(x[-1]["exec_ms"] for x in a))

    # Round 3, C7: shapes.
    c7 = json.loads((probes / "round3/results/c7_shapes.json").read_text())
    for name, r in c7.items():
        if name.startswith("_") or r.get("error") or "sql" not in r:
            continue
        add("r3_shapes", name, 1, r["sql"], r["uniq"], r["exec_ms"])

    Path(args.out).write_text(json.dumps({"source": str(probes), "cases": cases}, indent=1))
    print(f"{len(cases)} cases -> {args.out}")


def c68_queries():
    """Round 3's c68 query set, with TIN's table name (as c68_segments.py)."""
    t = "probe3_base.c"

    def rk(qq, k=10, fn="tin.full_score(ctid)"):
        return f"select id from {t} where body ==> '{qq}' order by {fn} desc limit {k}"

    return {
        "single_rare_k10": rk("t01"), "or3_k10": rk("w3 OR w17 OR t1"), "and2_k10": rk("w3 AND w17"),
        "w1_k1000": rk("w1", 1000), "rare_or_k10": rk("t001 OR t01 OR w4000"), "phrase_k10": rk('"w1 w2"'),
        "miss_k10": rk("zzzmissing"), "count_or": f"select count(*) from {t} where body ==> 'w3 OR t1'",
        "count_phrase": f"select count(*) from {t} where body ==> '\"w1 w2\"'",
    }


# --- corpus ------------------------------------------------------------------


def layout(conn, table):
    (pages, rpp, digest), = rows(conn, f"""
        select count(*), (select count(*) from {table})::numeric / count(*),
               md5(string_agg(p::text || ':' || n || ':' || ids, ',' order by p))
        from (select (ctid::text::point)[0]::int p, count(*) n, md5(string_agg(id::text, ',' order by ctid)) ids
              from {table} group by 1) x""")
    return {"pages_with_rows": pages, "rows_per_page": str(rpp), "layout_md5": digest}


def corpus(args):
    conn = connect(args)
    rows(conn, "create extension if not exists stannum")
    if not rows(conn, "select 1 from pg_tables where schemaname = 'probe3_base' and tablename = 'c'"):
        rows(conn, "create schema if not exists probe3_base")
        for statement in corpus_sql("probe3_base", "c", 1_000_000):
            rows(conn, statement)
        rows(conn, f"vacuum (analyze, freeze) {BASE}")
    info = {"base": layout(conn, BASE)}
    (info["base"]["content_md5"],), = rows(conn, f"""select md5(string_agg(id || ':' || cat || ':' || body || ':' || title,
        '|' order by id)) from {BASE}""")
    for segments in [int(s) for s in args.segments.split(",")]:
        info[segments] = build_segments(conn, segments)
    df = {t: rows(conn, f"select count(*) from {BASE} where body ==> '{t}'")[0][0] for t in PROBES}
    info["df"] = df
    checks = {
        "pages_with_rows": info["base"]["pages_with_rows"] == EXPECTED["pages_with_rows"],
        "rows_per_page": info["base"]["rows_per_page"].startswith(EXPECTED["rows_per_page"][:12]),
        "df": df == EXPECTED["df"],
    }
    for segments, built in info.items():
        if isinstance(segments, int):
            checks[f"N={segments} layout"] = built["layout_md5"] == info["base"]["layout_md5"]
            checks[f"N={segments} segments"] = built["segment_count"] == segments
            checks[f"N={segments} postings"] = (
                built["npostings"] == EXPECTED["npostings"] and built["sum_doc_lengths"] == EXPECTED["sum_doc_lengths"]
            )
    info["checks"] = checks
    print(json.dumps(info, indent=1, default=str))
    if args.out:
        Path(args.out).write_text(json.dumps(info, indent=1, default=str))
    if not all(checks.values()):
        sys.exit("corpus checks failed")


def build_segments(conn, segments):
    """An index of `segments` segments of contiguous ctid ranges over the
    corpus. A build always merges to one segment, so N > 1 copies the rows
    in ctid order into a table carrying the index, sealing the write segment
    every 2,500 rows with merges deferred, and merges each slice of 1M/N
    rows into one segment.

    The copy is made by COPY in the transaction that creates the table, so,
    like the CREATE TABLE AS that made the base table, it skips the free
    space map and fills each page before the next (an INSERT would put a
    row that misses a full page into an earlier page with room): its pages
    hold the same rows as the base table's, as the layout digest checks."""
    import psycopg

    table = table_for(segments)
    if segments == 1:
        index = "probe3_base.c_b1"
        if not rows(conn, "select 1 from pg_class where oid = to_regclass('probe3_base.c_b1')"):
            rows(conn, f"create index c_b1 on {BASE} using stannum(body) with (target_segment_count = 1)")
            rows(conn, f"vacuum (analyze) {BASE}")
    else:
        schema = table.split(".")[0]
        index = f"{schema}.c_idx"
        if not rows(conn, "select 1 from pg_tables where schemaname = 'probe3_base' and tablename = 'ctid_order'"):
            rows(conn, f"""create table probe3_base.ctid_order as
                select id, row_number() over (order by ctid) rn from {BASE}""")
            rows(conn, "alter table probe3_base.ctid_order add primary key (id)")
        rows(conn, f"drop schema if exists {schema} cascade")
        per = -(-1_000_000 // segments)
        dsn = conn.info.dsn
        # Write segments of per/25 rows are sealed as the rows arrive and
        # merged in tiers of 5 within the inserting backend, up to per rows:
        # each merge takes the 5 newest segments of a tier, so a segment
        # holds a contiguous run of rows. Larger merges are deferred to
        # VACUUM, which skips the index below (INDEX_CLEANUP off).
        with psycopg.connect(dsn) as tx, psycopg.connect(dsn, autocommit=True) as source:
            for statement in (
                "set stannum.index_maintenance_mode = foreground",
                f"set stannum.max_merge_docs = {per}", "set stannum.deferred_merge_docs = 0",
                "set stannum.merge_tier_factor = 5", "set stannum.max_segments = 96",
                f"set stannum.write_buffer_docs = {per // 25}",
                f"set stannum.write_buffer_bytes = {64 * 1024 * 1024}",
                f"create schema {schema}",
                f"create table {table} (like {BASE} including defaults)",
                f"create index c_idx on {table} using stannum(body) with (target_segment_count = 96)",
            ):
                tx.execute(statement)
            with source.cursor().copy(f"copy (select * from {BASE} order by ctid) to stdout") as out, \
                    tx.cursor().copy(f"copy {table} from stdin") as into:
                for data in out:
                    into.write(data)
            # The last write segment is sealed only by a row that would
            # overflow it: a placeholder (id 0), deleted afterwards.
            tx.execute(f"insert into {table} (id, cat, body, title) values (0, 0, 'h2hplaceholder', '')")
            # Its fold merges the last write segments; the tier above them
            # would merge at the next fold: merge those smallest entries now
            # (the N segments; the write buffer is not counted).
            tx.execute(f"select stannum.merge('{index}', target_segment_count => {segments}, force => true)")
            tx.commit()
        rows(conn, f"delete from {table} where id = 0")
        rows(conn, f"alter table {table} add primary key (id)")
        rows(conn, f"create index c_cat on {table}(cat)")
        rows(conn, f"vacuum (analyze, freeze, index_cleanup off) {table}")
    seg = rows(conn, f"select kind, docs, total_pages, npostings, sum_doc_lengths from stannum.segment_info('{index}') where kind <> 'retired' order by ordinal")
    built = layout(conn, table) if segments > 1 else {}
    built.update({
        # A copy keeps its placeholder's write buffer: one deleted document.
        "index": index, "segment_count": sum(s[0] == "immutable" for s in seg), "segments": [list(s) for s in seg],
        "npostings": sum(s[3] or 0 for s in seg if s[0] == "immutable"),
        "sum_doc_lengths": sum(s[4] or 0 for s in seg if s[0] == "immutable"),
        "index_bytes": rows(conn, f"select pg_relation_size('{index}')")[0][0],
    })
    if segments == 1:
        built["layout_md5"] = layout(conn, BASE)["layout_md5"]
    return built


# --- measure -----------------------------------------------------------------

KIND = re.compile(r"(\w[\w ]*?): copied (\d+) B in (\d+) pages, pinned (\d+), stitched (\d+) B in (\d+) reads")


def parse_kinds(text):
    out = {}
    for name, copied, copied_pages, pinned, stitched, stitches in KIND.findall(text or ""):
        out[name.strip()] = {"pinned": int(pinned), "copied_pages": int(copied_pages), "stitched": int(stitched)}
    return out


def parse_phases(text):
    out = {}
    for part in (text or "").split(", "):
        if part.strip():
            name, value = part.rsplit(" ", 1)
            out[name] = int(value)
    return out


def scan_nodes(plan):
    found = []

    def walk(node):
        if "Stannum" in (node.get("Custom Plan Provider") or ""):
            found.append(node)
        for child in node.get("Plans", []):
            walk(child)

    walk(plan)
    return found


COUNTERS = [
    "Scored Candidates", "Candidates", "Candidates Visited", "Positions Checked", "Position Lists Read",
    "Visibility Checks", "Heap Fetches", "Pages Pinned", "Exhaustive Score Calls", "Top-K Completions",
]


def summarize(explained):
    top = explained[0]
    out = {
        "exec_ms": top["Execution Time"], "plan_ms": top["Planning Time"],
        "plan_hit": top.get("Planning", {}).get("Shared Hit Blocks", 0),
        "hit": top["Plan"].get("Shared Hit Blocks", 0), "read": top["Plan"].get("Shared Read Blocks", 0),
        "kinds": {}, "phases": {}, "props": {},
    }
    for node in scan_nodes(top["Plan"]):
        for name, k in parse_kinds(node.get("Native Reads By Kind")).items():
            have = out["kinds"].setdefault(name, {"pinned": 0, "copied_pages": 0, "stitched": 0})
            for key in have:
                have[key] += k[key]
        for name, v in parse_phases(node.get("Buffer Accesses By Phase")).items():
            out["phases"][name] = out["phases"].get(name, 0) + v
        for key in COUNTERS:
            if key in node:
                out["props"][key] = out["props"].get(key, 0) + int(node[key])
        for key in ("Top K", "Pruning", "Count Strategy", "Candidate Strategy"):
            if key in node:
                out["props"][key] = node[key]
    out["nodes"] = node_names(top["Plan"])
    out["areas"] = areas(out)
    return out


def node_names(plan):
    names = []

    def walk(node, depth):
        names.append("  " * depth + (node.get("Custom Plan Provider") or node["Node Type"]))
        for child in node.get("Plans", []):
            walk(child, depth + 1)

    walk(plan, 0)
    return names


def areas(s):
    """Stannum's counters in TIN's areas (see the module docstring)."""
    def kind(*names):
        return sum(s["kinds"].get(n, {}).get("pinned", 0) + s["kinds"].get(n, {}).get("copied_pages", 0) for n in names)

    a = {
        "M+L": s["phases"].get("view capture", 0) + s["phases"].get("scorer setup", 0),
        "TM": kind("other"),
        "F": kind("record", "footer"),
        "P": kind("container", "sparse"),
        "TF": kind("tf"),
        "DL": kind("lengths", "inline lengths"),
        "Pos": kind("positions"),
    }
    return {k: v for k, v in a.items() if v}


def explain(conn, sql):
    (plan,), = rows(conn, f"explain (analyze, buffers, verbose, format json) {sql}")
    return plan


def tin_records(document):
    """Every probe record of a round-2 result file: a dict holding the
    statement (`sql`) and TIN's page touches."""
    def walk(node):
        if isinstance(node, dict):
            if "sql" in node and ("unique" in node or "touches" in node):
                yield node
            for key, value in node.items():
                if key != "_sql_log":
                    yield from walk(value)
        elif isinstance(node, list):
            for value in node:
                yield from walk(value)

    return {r["sql"]: r for r in walk(document)}


ROUND2_BASE = [
    # round 2's r1_calib.py: 2M int4 rows at exactly 226 per heap page
    "create schema if not exists probe2_base",
    "create table if not exists probe2_base.t (id int4 not null)",
    "insert into probe2_base.t select i from generate_series(1, 2000000) i where not exists (select 1 from probe2_base.t)",
    "vacuum (analyze, freeze) probe2_base.t",
]


def replay(args):
    """Replays round 2's probes (expression and partial indexes over
    probe2_base.t) statement by statement from each result file's SQL log,
    as Stannum's: CREATE INDEX ... USING stannum, stannum's functions.
    Statements under tin.debug_force_topk (a TIN debug setting) are
    skipped. Each EXPLAIN is run once in the replaying session (TIN's
    probes ran each once) and set next to TIN's record of it.

    Stannum refuses to score over an index whose expression PostgreSQL
    folds (a CASE calling repeat() on constants: the index's expression is
    folded, the score function's argument is matched unfolded), so each
    indexed expression is wrapped in an immutable PL/pgSQL function of id,
    which nothing folds, in the index and in the queries alike."""
    import hashlib

    import psycopg

    wrapped = {}

    def wrap(conn, statement):
        found = re.search(r"using stannum \(\((.*)\)\) with", statement)
        if found and found.group(1) not in wrapped:
            expression = found.group(1)
            name = "probe2_base.h2h_e" + hashlib.md5(expression.encode()).hexdigest()[:10]
            rows(conn, f"""create or replace function {name}(id int4) returns text language plpgsql immutable
                as $f$ begin return {expression}; end $f$""")
            wrapped[expression] = name
        for expression, name in wrapped.items():
            statement = statement.replace(f"({expression})", f"{name}(id)")
        return statement

    dsn = os.environ[args.dsn_env]
    out = {"started": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "files": {}}
    with psycopg.connect(dsn, autocommit=True) as conn:
        for statement in ROUND2_BASE:
            rows(conn, statement)
        (bad,), = rows(conn, """select count(*) filter (where (ctid::text::point)[0] <> (id - 1) / 226
            or (ctid::text::point)[1] <> (id - 1) % 226 + 1) from probe2_base.t""")
        out["layout_mismatches"] = bad
    for path in args.files:
        document = json.loads(Path(path).read_text())
        records = tin_records(document)
        results = []
        forced = False
        with psycopg.connect(dsn, autocommit=True) as conn:
            for entry in document["_sql_log"]:
                sql = entry["sql"].strip()
                low = sql.lower()
                if low.startswith(("set tin.", "reset tin.")):
                    if "debug_force_topk" in low:
                        forced = low.startswith("set") and "exhaustive" in low
                    continue
                if low.startswith("set max_parallel"):
                    continue
                stannum = wrap(conn, sql.replace("using tin ", "using stannum ").replace("using tin(", "using stannum(")
                               .replace("tin.", "stannum."))
                if low.startswith("explain"):
                    if forced:
                        continue
                    query = sql.split(")", 1)[1].strip()
                    tin = records.get(query)
                    record = {"sql": query, "tin": short_areas((tin or {}).get("unique") or (tin or {}).get("touches") or {}),
                              "tin_ms": (tin or {}).get("exec_ms")}
                    try:
                        record["stannum"] = summarize(explain(conn, stannum.split(")", 1)[1].strip()))
                    except psycopg.Error as error:
                        record["error"] = str(error).splitlines()[0]
                    results.append(record)
                    continue
                try:
                    rows(conn, stannum)
                except psycopg.Error as error:
                    results.append({"statement": stannum[:200], "error": str(error).splitlines()[0]})
        out["files"][Path(path).name] = results
        Path(args.out).write_text(json.dumps(out, indent=1))
        ok = sum("stannum" in r for r in results)
        print(f"{Path(path).name}: {ok} explains, {sum('error' in r for r in results)} errors", flush=True)
    Path(args.out).write_text(json.dumps(out, indent=1))


def measure(args):
    import psycopg

    ref = json.loads(Path(args.reference).read_text())["cases"]
    if args.group:
        ref = [c for c in ref if c["group"] in args.group.split(",")]
    if args.segments:
        ref = [c for c in ref if c["segments"] in {int(s) for s in args.segments.split(",")}]
    out = {"started": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "rounds": args.rounds, "cases": []}
    dsn = os.environ[args.dsn_env]
    with psycopg.connect(dsn, autocommit=True) as conn:
        out["server"] = rows(conn, "select version()")[0][0]
        out["extension"] = rows(conn, "select extversion from pg_extension where extname = 'stannum'")[0][0]
    for i, case in enumerate(ref):
        table = table_for(case["segments"])
        result = {k: case[k] for k in ("group", "case", "segments", "sql")}
        runs = []
        try:
            for _ in range(args.rounds):
                with psycopg.connect(dsn, autocommit=True) as conn:
                    explain(conn, WARM_OTHER.format(table=table))
                    runs.append([summarize(explain(conn, case["sql"])) for _ in range(3)])
            # A session that ran nothing before: per-backend decoding included.
            with psycopg.connect(dsn, autocommit=True) as conn:
                result["cold_backend"] = summarize(explain(conn, case["sql"]))
        except psycopg.Error as error:
            result["error"] = str(error).splitlines()[0]
            print(f"[{i + 1}/{len(ref)}] {case['group']} {case['case']} N={case['segments']}: {result['error']}", flush=True)
            out["cases"].append(result)
            continue
        result["run1"] = runs[0][0]
        result["run3"] = runs[0][2]
        result["exec1_ms"] = statistics.median(r[0]["exec_ms"] for r in runs)
        result["exec3_ms"] = statistics.median(r[2]["exec_ms"] for r in runs)
        result["stable_pages"] = all(r[0]["areas"] == runs[0][0]["areas"] for r in runs)
        out["cases"].append(result)
        print(f"[{i + 1}/{len(ref)}] {case['group']} {case['case']} N={case['segments']}: "
              f"{result['run1']['areas']} {result['exec1_ms']:.3f}/{result['exec3_ms']:.3f} ms "
              f"tin {case['tin']['uniq']} {case['tin']['exec_ms']}", flush=True)
        Path(args.out).write_text(json.dumps(out, indent=1))
    Path(args.out).write_text(json.dumps(out, indent=1))


# --- report ------------------------------------------------------------------

TIN_AREAS = ["M", "TM", "F", "P", "TF", "DL", "Pos", "L"]
ST_AREAS = ["M+L", "TM", "F", "P", "TF", "DL", "Pos"]


def total(d):
    return sum(d.values())


def fmt_ratio(a, b):
    if not b:
        return "–" if not a else "∞"
    return f"{a / b:.2f}"


def report(args):
    ref = {(c["group"], c["case"], c["segments"]): c for c in json.loads(Path(args.reference).read_text())["cases"]}
    measured = json.loads(Path(args.measured).read_text())
    lines = [f"Stannum {measured['extension']} on {measured['server'].split(' on ')[0]}; {measured['rounds']} rounds.", ""]
    groups = {}
    for m in measured["cases"]:
        groups.setdefault(m["group"], []).append(m)
    for group, cases in groups.items():
        lines += [f"### {group}", "",
                  "| case | N | TIN M TM F P TF DL Pos L = total | Stannum M+L TM F P TF DL Pos = total (run 1) | run 3 total | ratio r1 | TIN ms | Stannum ms r1/r3 | scored | µs/page | µs/scored |",
                  "|---|---:|---|---|---:|---:|---:|---:|---:|---:|---:|"]
        for m in cases:
            c = ref[(m["group"], m["case"], m["segments"])]
            t = c["tin"]["uniq"]
            if "error" in m:
                lines.append(f"| {m['case']} | {m['segments']} | {' '.join(str(t.get(a, 0)) for a in TIN_AREAS)} = {total(t)} | ERROR {m['error'][:60]} | | | | | | | |")
                continue
            s1, s3 = m["run1"]["areas"], m["run3"]["areas"]
            scored = m["run1"]["props"].get("Scored Candidates", m["run1"]["props"].get("Candidates", ""))
            st_total = total(s1)
            us = m["exec3_ms"] * 1000
            lines.append(
                f"| {m['case']} | {m['segments']} | {' '.join(str(t.get(a, 0)) for a in TIN_AREAS)} = {total(t)} "
                f"| {' '.join(str(s1.get(a, 0)) for a in ST_AREAS)} = {st_total} | {total(s3)} "
                f"| {fmt_ratio(st_total, total(t))} | {c['tin']['exec_ms']} | {m['exec1_ms']:.3f}/{m['exec3_ms']:.3f} "
                f"| {scored} | {us / st_total if st_total else 0:.2f} | {us / scored if scored else 0:.3f} |"
            )
        lines.append("")
    Path(args.out).write_text("\n".join(lines))
    print(f"-> {args.out}")


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--dsn-env", default="STANNUM_DSN")
    sub = p.add_subparsers(dest="command", required=True)
    r = sub.add_parser("reference")
    r.add_argument("--probes", required=True)
    r.add_argument("--out", required=True)
    c = sub.add_parser("corpus")
    c.add_argument("--segments", default="1")
    c.add_argument("--out")
    m = sub.add_parser("measure")
    m.add_argument("--reference", required=True)
    m.add_argument("--out", required=True)
    m.add_argument("--rounds", type=int, default=3)
    m.add_argument("--group")
    m.add_argument("--segments")
    rr = sub.add_parser("replay")
    rr.add_argument("--out", required=True)
    rr.add_argument("files", nargs="+", help="round-2 result files (round2/results/*.json)")
    rp = sub.add_parser("report")
    rp.add_argument("--reference", required=True)
    rp.add_argument("--measured", required=True)
    rp.add_argument("--out", required=True)
    args = p.parse_args(argv)
    {"reference": reference, "corpus": corpus, "measure": measure, "replay": replay, "report": report}[args.command](args)


if __name__ == "__main__":
    main()
