# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""The sustained-mutation profile of run.py (`run.py run --profile mutation`).

Rate-scheduled writers insert copies of dataset documents under fresh ids,
delete existing rows, and rewrite the reserved suffix of existing rows with
a bundle of query terms so match sets drift. Closed-loop or rate-scheduled
readers run the count and ranked shapes. VACUUM (INDEX_CLEANUP ON) runs on a schedule, the
index layout is sampled on a schedule, and every check interval the index's
answer for each query is compared with a regular-expression sequential scan
inside one snapshot. Any difference fails the run.
"""
import collections
import json
import math
import subprocess
import threading
import time

KINDS = ("insert", "delete", "update")
DELTA_SAMPLE = 50


def parse_mix(text):
    weights = dict.fromkeys(KINDS, 0)
    for part in text.split(","):
        name, _, value = part.partition("=")
        if name not in KINDS or not value.isdigit():
            raise ValueError(f"--mix expects kind=weight with kinds {KINDS}: {text!r}")
        weights[name] = int(value)
    if sum(weights.values()) <= 0:
        raise ValueError("--mix needs at least one positive weight")
    return weights


def regex_predicate(case):
    """Oracle predicate over the row text with no dependency on the index or the tokenizer."""
    plain, kind = case[3], case[4]
    if kind == "phrase":
        return f"body ~ '\\m{plain}\\M'"
    joiner = " OR " if kind == "or" else " AND "
    return joiner.join(f"body ~ '\\m{term}\\M'" for term in plain.split())


def drift_bundles(cases):
    """Suffixes an update may append: nothing, or the terms of one non-miss query."""
    return [""] + [case[3] for case in cases if "miss" not in case[0]]


def writer_scripts(rows, cases):
    """Pick an unlocked live row using the current ID range, wrapping gaps.

    This samples key space, not live rows uniformly. Selection/locking overhead
    is included in every engine's timed writes. Exhaustion fails the run.
    """
    bundles = drift_bundles(cases)
    choose = " ".join(f"WHEN {i} THEN ' {bundle}'" for i, bundle in enumerate(bundles) if bundle)
    threshold = "(SELECT floor(coalesce(max(id), 1) * (:probe / 2147483647.0))::bigint FROM documents)"
    locate = ("coalesce((SELECT id FROM documents WHERE id >= " + threshold +
              " ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED),"
              " (SELECT id FROM documents ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED))")
    return {
        "insert": f"\\set src random(1, {rows})\n"
                  "INSERT INTO documents (id, body) SELECT nextval('benchmark_ids'), body "
                  "FROM benchmark_pool WHERE id = :src;\n",
        "delete": "\\set probe random(1, 2147483647)\nDELETE FROM documents WHERE id = " + locate + ";\n",
        "update": f"\\set probe random(1, 2147483647)\n\\set k random(0, {len(bundles) - 1})\n"
                  "UPDATE documents SET body = regexp_replace(body, ' mutable[ab].*$', '') || ' mutablea'"
                  f" || CASE :k {choose} ELSE '' END WHERE id = {locate};\n",
    }


def accounted_writer(script, kind):
    """A completion guarantees one committed row effect, without shell logging.

    The count assertion and DML share one autocommit statement. Zero effects
    (empty/fully locked target set) or multiple effects abort the statement.
    Failed pgbench transactions invalidate the run, never count as useful work.
    """
    if kind not in KINDS:
        raise ValueError('unknown mutation kind')
    lines = script.splitlines()
    prefix = []
    while lines and lines[0].startswith('\\set '):
        prefix.append(lines.pop(0))
    sql = '\n'.join(lines).rstrip().removesuffix(';')
    return '\n'.join(prefix) + f"\nWITH changed AS ({sql} RETURNING 1)\nSELECT 1 / (count(*) = 1)::int FROM changed;\n"


def affected_rows(summary):
    if summary['failures'] or set(summary['queries']) - set(KINDS):
        raise ValueError('cannot account mutations with failed transactions or unknown kinds')
    return {kind: dict(completed=q['completed'], noops=0, affected=q['completed'])
            for kind, q in summary['queries'].items()}


def traffic_load(paths, rate, seconds):
    """Scheduled latency includes client lag; execution latency excludes it.

    First/last deciles use scheduled arrival order, not completion order. Nominal
    arrivals are an expectation for pgbench's Poisson schedule, not an exact
    offered count; arrivals never emitted at shutdown are not in its logs.
    """
    records, failed = [], collections.Counter()
    for path in paths:
        for _, status, stamp, lag in read_log(path):
            if not status.isdigit():
                failed[status] += 1
                continue
            scheduled_ms = int(status) / 1000
            if lag is not None and (lag < 0 or lag > scheduled_ms):
                raise ValueError('invalid scheduling lag')
            records.append((stamp - scheduled_ms / 1000, scheduled_ms, lag or 0))
    records.sort()
    tail = max(1, len(records) // 10)
    def describe_part(part):
        return dict(scheduled=describe([r[1] for r in part]),
                    execution=describe([r[1] - r[2] for r in part]),
                    lag_p95_ms=percentile([r[2] for r in part], .95))
    return dict(rate=rate, nominal_arrivals=rate * seconds if rate else None,
                logged=len(records) + sum(failed.values()), completed=len(records), failures=dict(failed),
                overall=describe_part(records), first_decile=describe_part(records[:tail]),
                last_decile=describe_part(records[-tail:]),
                interpretation='Poisson nominal arrivals are not exact. Un-emitted arrivals are absent; '
                               'lag measures client scheduling pressure, not server queue depth.')


def next_tick(due, finished, interval):
    """Skip missed maintenance slots rather than queue concurrent VACUUM jobs."""
    missed = max(0, math.floor((finished - due) / interval))
    return due + (missed + 1) * interval, missed


def pool_sql(rows):
    return ("CREATE TABLE benchmark_pool AS SELECT id, body FROM documents; "
            "ALTER TABLE benchmark_pool ADD PRIMARY KEY (id); "
            f"CREATE SEQUENCE benchmark_ids START {rows + 1};")


def check_sql(case, where, score=None, order=None):
    """One statement, hence one snapshot: index answer, oracle answer, their difference,
    and the ranked top ten from the index, for evaluate_check."""
    if score is None:
        return f"""WITH actual AS MATERIALIZED (SELECT id FROM documents WHERE {where}),
expected AS MATERIALIZED (SELECT id FROM documents WHERE {regex_predicate(case)}),
delta AS ((SELECT id FROM actual EXCEPT SELECT id FROM expected)
 UNION ALL (SELECT id FROM expected EXCEPT SELECT id FROM actual))
SELECT json_build_object('count', (SELECT count(*) FROM actual), 'expected', (SELECT count(*) FROM expected),
 'differences', (SELECT count(*) FROM delta), 'delta_sample',
 (SELECT coalesce(json_agg(d), '[]'::json) FROM (SELECT * FROM delta LIMIT {DELTA_SAMPLE}) d), 'ranked', false);"""
    return f"""WITH actual AS MATERIALIZED (SELECT id FROM documents WHERE {where}),
expected AS MATERIALIZED (SELECT id FROM documents WHERE {regex_predicate(case)}),
delta AS ((SELECT id, 'index_only' AS side FROM actual EXCEPT SELECT id, 'index_only' FROM expected)
 UNION ALL (SELECT id, 'oracle_only' FROM expected EXCEPT SELECT id, 'oracle_only' FROM actual)),
top AS MATERIALIZED (SELECT id, {score} AS score FROM documents WHERE {where} ORDER BY {order} LIMIT 10)
SELECT json_build_object('count', (SELECT count(*) FROM actual), 'expected', (SELECT count(*) FROM expected),
 'differences', (SELECT count(*) FROM delta),
 'delta_sample', (SELECT coalesce(json_agg(d), '[]'::json) FROM (SELECT * FROM delta ORDER BY id LIMIT {DELTA_SAMPLE}) d),
 'top', (SELECT coalesce(json_agg(json_build_array(id, score) ORDER BY score DESC), '[]'::json) FROM top),
 'top_outside', (SELECT count(*) FROM top WHERE id NOT IN (SELECT id FROM expected)));"""


def evaluate_check(name, result):
    """Raises on any disagreement between the index and the oracle, or a malformed top ten."""
    if result["differences"] or result["count"] != result["expected"]:
        raise ValueError(f"Index and oracle disagree for {name}: index {result['count']} rows, "
                         f"oracle {result['expected']} rows, {result['differences']} differences, "
                         f"sample {result['delta_sample']}")
    if result.get('ranked') is False:
        return {"name": name, "count": result["count"]}
    ids = [row[0] for row in result["top"]]
    scores = [float(row[1]) for row in result["top"]]
    if (len(ids) != min(10, result["count"]) or len(set(ids)) != len(ids) or result["top_outside"]
            or any(not math.isfinite(s) for s in scores) or scores != sorted(scores, reverse=True)):
        raise ValueError(f"Malformed ranked top ten for {name}: {result['top']} outside={result['top_outside']}")
    return {"name": name, "count": result["count"], "top": ids}


def plan_nodes(node, subplan=None):
    subplan = node.get("Subplan Name", subplan)
    yield subplan, node["Node Type"]
    for child in node.get("Plans", []):
        yield from plan_nodes(child, subplan)


def oracle_plan_is_independent(plan):
    """The oracle side must be a sequential scan and the index side must not be."""
    kinds = collections.defaultdict(set)
    for subplan, node_type in plan_nodes(plan[0]["Plan"]):
        kinds[subplan].add(node_type)
    index_side = {"Custom Scan", "Bitmap Heap Scan", "Index Scan"}
    return ("Seq Scan" in kinds["CTE expected"] and not (kinds["CTE expected"] & index_side)
            and "Seq Scan" not in kinds["CTE actual"] and bool(kinds["CTE actual"] & index_side))


def percentile(values, fraction):
    if not values:
        return None
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)]


def read_log(path):
    """pgbench -l records: (script index, status or microseconds, completion epoch seconds, lag ms).
    A run stopped early leaves one partial trailing record, which is dropped."""
    lines = path.read_text().splitlines()
    for number, line in enumerate(lines, 1):
        fields = line.split()
        if len(fields) < 6:
            if number == len(lines):
                break
            raise ValueError(f"Malformed pgbench record in {path}")
        stamp = int(fields[4]) + int(fields[5]) / 1e6
        lag = int(fields[6]) / 1000 if len(fields) >= 7 else None
        yield int(fields[3]), fields[2], stamp, lag


def describe(values):
    return {"completed": len(values), "p50_ms": percentile(values, .5), "p95_ms": percentile(values, .95),
            "p99_ms": percentile(values, .99) if len(values) >= 1000 else None,
            "p99_insufficient_samples": len(values) < 1000, "max_ms": max(values) if values else None}


def bucket_logs(paths, names, bucket_seconds, origin):
    """Latency per bucket per script and per shape (count/ranked or the mutation kind)."""
    samples = collections.defaultdict(lambda: collections.defaultdict(list))
    lags = collections.defaultdict(list)
    for path in paths:
        for script, status, stamp, lag in read_log(path):
            if not status.isdigit():
                continue
            bucket = int((stamp - origin) // bucket_seconds)
            # Under -R pgbench reports latency from the scheduled start; keep execution time and lag apart.
            samples[bucket][names[script]].append(int(status) / 1000 - (lag or 0))
            if lag is not None:
                lags[bucket].append(lag)
    buckets = []
    for number in sorted(samples):
        by_name = {name: describe(samples[number].get(name, [])) for name in names}
        shapes = collections.defaultdict(list)
        for name in names:
            shapes[name.rsplit("_", 1)[-1] if "_" in name else name] += samples[number].get(name, [])
        buckets.append({"bucket": number, "start_seconds": number * bucket_seconds,
                        "queries": by_name, "shapes": {shape: describe(vals) for shape, vals in shapes.items()},
                        "schedule_lag_max_ms": max(lags[number]) if lags[number] else None})
    return buckets


def sample_sql(engine, freespace):
    segments = "NULL"
    if engine == "stannum":
        segments = ("(SELECT json_build_object('immutable', count(*) FILTER (WHERE kind = 'immutable'),"
                    " 'docs', coalesce(sum(docs) FILTER (WHERE kind = 'immutable'), 0),"
                    " 'dead_docs', coalesce(sum(dead_docs), 0), 'live_pages', coalesce(sum(total_pages), 0),"
                    " 'max_generation', coalesce(max(generation), 0),"
                    " 'buffer_docs', coalesce(sum(docs) FILTER (WHERE kind = 'mutable'), 0))"
                    " FROM stannum.segment_info('search_idx'))")
    gin = "(SELECT row_to_json(g) FROM pgstatginindex('search_idx') g)" if engine == 'gin' else "NULL"
    free = "(SELECT count(*) FROM pg_freespace('search_idx') WHERE avail > 0)" if freespace else "NULL"
    return f"""SELECT json_build_object('index_bytes', pg_relation_size('search_idx'),
 'table_bytes', pg_table_size('documents'), 'rows', (SELECT count(*) FROM documents),
 'fsm_free_pages', {free}, 'gin_pending', {gin},
 'wal_lsn', pg_current_wal_insert_lsn()::text,
 'table_stats', (SELECT json_build_object('n_live_tup', n_live_tup, 'n_dead_tup', n_dead_tup,
   'n_tup_ins', n_tup_ins, 'n_tup_del', n_tup_del, 'n_tup_upd', n_tup_upd,
   'vacuum_count', vacuum_count, 'autovacuum_count', autovacuum_count)
   FROM pg_stat_user_tables WHERE relname = 'documents'),
 'segments', {segments});"""


class Maintenance:
    """Sampler, VACUUM scheduler and correctness checker threads during the timed window."""

    def __init__(self, psql, sql_json, env, check_env, engine, checks, freespace, out,
                 sample_interval, vacuum_interval, check_interval):
        self.psql, self.sql_json = psql, sql_json
        self.env, self.check_env, self.engine, self.checks_sql = env, check_env, engine, checks
        self.freespace, self.out = freespace, out
        self.intervals = {"sample": sample_interval, "vacuum": vacuum_interval, "check": check_interval}
        self.samples, self.vacuums, self.checks, self.failures = [], [], [], []
        self.schedule = []
        self.stop = threading.Event()
        self.origin = None
        self.threads = []

    def start(self, origin):
        self.origin = origin
        for name, action in (("sample", self.sample), ("vacuum", self.vacuum), ("check", self.check)):
            if self.intervals[name]:
                thread = threading.Thread(target=self.loop, args=(name, action), daemon=True, name=name)
                self.threads.append(thread)
                thread.start()

    def loop(self, name, action):
        due = self.origin + self.intervals[name]
        while not self.stop.wait(max(due - time.time(), 0)):
            started = time.time()
            try:
                action()
            except Exception as error:  # surfaced by the run loop, which stops traffic
                self.failures.append({"thread": name, "error": f"{type(error).__name__}: {error}",
                                      "t": time.time() - self.origin})
                return
            finished = time.time()
            next_due, missed = next_tick(due, finished, self.intervals[name])
            self.schedule.append(dict(kind=name, due=due - self.origin, started=started - self.origin,
                                      finished=finished - self.origin, lag_ms=(started - due) * 1000,
                                      missed_slots=missed))
            due = next_due

    def finish(self):
        self.stop.set()
        for thread in self.threads:
            thread.join()

    def timed(self, sql, env):
        start = time.monotonic()
        result = self.sql_json(sql, env)
        return result, (time.monotonic() - start) * 1000

    def sample(self, phase='traffic'):
        result, ms = self.timed(sample_sql(self.engine, self.freespace), self.env)
        result.update({"t": time.time() - self.origin, "ms": ms, "phase": phase})
        self.samples.append(result)
        return result

    def vacuum(self, phase='traffic'):
        before, _ = self.timed(sample_sql(self.engine, self.freespace), self.env)
        start = time.monotonic()
        started_epoch = time.time()
        proc = subprocess.run(["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1", "-c",
                               "VACUUM (INDEX_CLEANUP ON, VERBOSE) documents;"],
                              env=self.check_env, capture_output=True, text=True, check=True)
        ms = (time.monotonic() - start) * 1000
        after, _ = self.timed(sample_sql(self.engine, self.freespace), self.env)
        number = len(self.vacuums) + 1
        (self.out / f"vacuum-{number}.txt").write_text(proc.stderr + proc.stdout)
        self.vacuums.append({"t": time.time() - self.origin, "ms": ms, "before": before, "after": after,
                             "started": started_epoch - self.origin, "phase": phase})

    def check(self):
        start = time.monotonic()
        started_epoch = time.time()
        result = check_round(self.psql, self.check_env, self.checks_sql)
        record = {"t": time.time() - self.origin, "started": started_epoch - self.origin,
                  "ms": (time.monotonic() - start) * 1000, "queries": result}
        self.checks.append(record)
        return record


def check_round(psql, env, checks):
    """All queries against the oracle inside one repeatable-read snapshot."""
    script = ["BEGIN ISOLATION LEVEL REPEATABLE READ;", "SET LOCAL enable_seqscan = off;"]
    script += [sql for _, sql in checks]
    script.append("COMMIT;")
    lines = [line for line in psql("\n".join(script), env).splitlines() if line.strip()]
    if len(lines) != len(checks):
        raise ValueError(f"Expected {len(checks)} check results, got {len(lines)}")
    return {name: evaluate_check(name, json.loads(line)) for (name, _), line in zip(checks, lines)}


def annotate(buckets, bucket_seconds, samples, vacuums, checks):
    """Attach the last sample and every VACUUM/check that completed in each bucket."""
    by_number = {b["bucket"]: b for b in buckets}
    for sample in samples:
        bucket = by_number.get(int(sample["t"] // bucket_seconds))
        if bucket is not None:
            bucket["sample"] = sample
    for kind, events in (("vacuums", vacuums), ("checks", checks)):
        for event in events:
            bucket = by_number.get(int(event["t"] // bucket_seconds))
            if bucket is not None:
                bucket.setdefault(kind, []).append(event)
    return buckets


def fmt(value, digits=1):
    return "—" if value is None else f"{value:.{digits}f}"


def render(buckets, kinds, bucket_seconds):
    """A text table with one line per bucket: reader tails, writer maxima, layout, events."""
    head = ["t(s)", "reads/s", "count p50", "count p99", "ranked p50", "ranked p99"]
    head += [f"{kind} max" for kind in kinds] + ["write lag max", "read lag max", "segs", "gen", "buf", "idx MB", "free pg", "events"]
    rows = [head]
    for bucket in buckets:
        shapes = bucket["shapes"]
        reads = sum(bucket["queries"][n]["completed"] for n in bucket["queries"] if n.endswith(("_count", "_ranked")))
        row = [str(bucket["start_seconds"]), fmt(reads / bucket_seconds, 0),
               fmt(shapes.get("count", {}).get("p50_ms")), fmt(shapes.get("count", {}).get("p99_ms")),
               fmt(shapes.get("ranked", {}).get("p50_ms")), fmt(shapes.get("ranked", {}).get("p99_ms"))]
        row += [fmt(bucket["queries"].get(kind, {}).get("max_ms")) for kind in kinds]
        row.append(fmt(bucket.get("schedule_lag_max_ms")))
        row.append(fmt(bucket.get("reader_schedule_lag_max_ms")))
        sample = bucket.get("sample") or {}
        segments = sample.get("segments") or {}
        row += [str(segments.get("immutable", "—")), str(segments.get("max_generation", "—")),
                str(segments.get("buffer_docs", "—")),
                fmt(sample["index_bytes"] / 2**20 if sample else None), str(sample.get("fsm_free_pages", "—"))]
        events = [("drain " if v.get('phase') == 'drain' else '') + f"vacuum {v['ms'] / 1000:.1f}s"
                  for v in bucket.get("vacuums", [])]
        events += [f"check {c['ms'] / 1000:.1f}s ok" for c in bucket.get("checks", [])]
        row.append("; ".join(events))
        rows.append(row)
    widths = [max(len(r[i]) for r in rows) for i in range(len(head))]
    return "\n".join("  ".join(cell.rjust(widths[i]) if i < len(head) - 1 else cell for i, cell in enumerate(r)) for r in rows) + "\n"


def worst_mutations(buckets, kinds):
    worst = {}
    for kind in kinds:
        best = max(((b["queries"][kind]["max_ms"], b["start_seconds"]) for b in buckets
                    if b["queries"].get(kind, {}).get("max_ms") is not None), default=(None, None))
        worst[kind] = {"max_ms": best[0], "bucket_start_seconds": best[1]}
    return worst


def reclaim_summary(vacuums):
    """How each VACUUM changed the index: pages freed into the FSM and segments rewritten."""
    rows = []
    for number, vacuum in enumerate(vacuums, 1):
        before, after = vacuum["before"], vacuum["after"]
        rows.append({"vacuum": number, "t": vacuum["t"], "ms": vacuum["ms"],
                     "index_bytes_before": before["index_bytes"], "index_bytes_after": after["index_bytes"],
                     "fsm_free_pages_before": before.get("fsm_free_pages"), "fsm_free_pages_after": after.get("fsm_free_pages"),
                     "dead_docs_before": (before.get("segments") or {}).get("dead_docs"),
                     "dead_docs_after": (after.get("segments") or {}).get("dead_docs"),
                     "segments_before": (before.get("segments") or {}).get("immutable"),
                     "segments_after": (after.get("segments") or {}).get("immutable"),
                     "n_dead_tup_before": (before.get("table_stats") or {}).get("n_dead_tup"),
                     "n_dead_tup_after": (after.get("table_stats") or {}).get("n_dead_tup")})
    return rows
