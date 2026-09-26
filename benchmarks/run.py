#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Small, dependency-free PostgreSQL benchmark recorder."""
import argparse
import collections
import csv
import datetime as dt
import dataset
import hashlib
import json
import math
import mutation
import os
from pathlib import Path
import platform
import subprocess
import sys
import time

ROOT = Path(os.environ.get("STANNUM_BENCH_ROOT", Path(__file__).resolve().parents[1]))
# "tin" targets PlanetScale TIN; "stannum" targets this extension under its own schema.
ENGINES = ("stannum", "tin", "gin", "paradedb", "pg_textsearch")
# "mutation" runs the mixed read shapes under inserts, deletes and match-changing updates.
PROFILES = ("count", "ranked", "mixed", "mutation", "mutation-count")
SETTINGS_SQL = """SELECT json_object_agg(name, setting) FROM pg_settings
WHERE name = ANY(ARRAY['server_version','block_size','shared_buffers','work_mem',
 'maintenance_work_mem','effective_cache_size','max_connections','max_worker_processes',
 'max_parallel_workers','max_parallel_workers_per_gather','max_parallel_maintenance_workers',
 'effective_io_concurrency','maintenance_io_concurrency','random_page_cost','seq_page_cost',
 'cpu_tuple_cost','cpu_index_tuple_cost','cpu_operator_cost','default_statistics_target',
 'jit','jit_above_cost','enable_seqscan','enable_bitmapscan','enable_indexscan',
 'enable_indexonlyscan','enable_sort','enable_incremental_sort','synchronous_commit',
 'fsync','full_page_writes','wal_level','wal_compression','max_wal_size','checkpoint_timeout',
 'autovacuum','autovacuum_vacuum_scale_factor','autovacuum_analyze_scale_factor',
 'autovacuum_max_workers','autovacuum_vacuum_cost_limit','autovacuum_vacuum_cost_delay',
 'shared_preload_libraries','default_text_search_config','statement_timeout',
 'track_io_timing','huge_pages','hash_mem_multiplier','gin_pending_list_limit',
 'pg_textsearch.memtable_spill_threshold','pg_textsearch.bulk_load_threshold',
 'pg_textsearch.default_limit','pg_textsearch.compress_segments']) OR name LIKE 'tin.%' OR name LIKE 'stannum.%';"""
CASES = [
    ("miss", "absenttoken", "absenttoken", "absenttoken", "term", 0),
    ("rare", "rare", "rare", "rare", "term", 100),
    ("and", "common AND rare", "common & rare", "common rare", "and", 100),
    ("or", "common OR rare", "common | rare", "common rare", "or", 1),
    ("phrase", '"alpha beta"', "alpha <-> beta", "alpha beta", "phrase", 1000),
    ("phrase_miss", '"beta alpha"', "beta <-> alpha", "beta alpha", "phrase", 0),
]


def digest(data):
    return hashlib.sha256(data).hexdigest()


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def command(args, **kw):
    try:
        return subprocess.run(args, check=True, capture_output=True, text=True, **kw).stdout.strip()
    except subprocess.CalledProcessError as error:
        sys.stderr.write(error.stderr or "")
        raise


def save(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def psql(sql, env):
    return command(["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1"], input=sql, env=env)


def sql_json(sql, env):
    return json.loads(psql(sql, env))


def predicate(engine, case):
    _, tinql, ts, plain, kind, _ = case
    if engine in ("stannum", "tin"):
        return f"body ==> '{tinql}'"
    if engine == "gin":
        return f"to_tsvector('simple', body) @@ to_tsquery('simple', '{ts}')"
    if engine == "pg_textsearch":
        return f"body @@ to_tsquery('simple', '{ts}')"
    op = {"term": "|||", "or": "|||", "and": "&&&", "phrase": "###"}[kind]
    return f"body {op} '{plain}'"


def workload(engine, profile, cases=CASES):
    queries = []
    for case in cases:
        name = case[0]
        where = predicate(engine, case)
        if profile != "ranked":
            queries.append((name + "_count", f"SELECT count(*) FROM documents WHERE {where};"))
        if profile not in ("count", "mutation-count"):
            score, order = ranking(engine, case)
            queries.append((name + "_ranked", f"SELECT id, {score} AS score FROM documents "
                            f"WHERE {where} ORDER BY {order} LIMIT 10;"))
    return queries


def ranking(engine, case):
    score = {
        "stannum": "stannum.full_score(ctid)",
        "tin": "tin.full_score(ctid)",
        "paradedb": "pdb.score(id)",
        "pg_textsearch": f"-(body <@> '{case[3]}')",
    }[engine]
    # No secondary sort: ties are deliberately unordered in the measured workload.
    order = f"body <@> '{case[3]}' ASC" if engine == "pg_textsearch" else "score DESC"
    return score, order


def fixture_sql(rows, body_repeat=1):
    repetitions = '8 + n % 7' if body_repeat == 1 else f'(8 + n % 7) * {body_repeat}'
    return f"""CREATE TABLE documents (id bigint PRIMARY KEY, body text NOT NULL);
INSERT INTO documents
SELECT n, 'common ' || repeat('filler ', {repetitions})
 || CASE WHEN n % 100 = 0 THEN 'rare ' ELSE '' END
 || CASE WHEN n % 1000 = 0 THEN 'alpha beta ' ELSE '' END
 || 'mutablea'
FROM generate_series(1, {rows}) AS n;
"""


def index_sql(engine, gin_fastupdate=None):
    if engine == "gin" and gin_fastupdate is not None:
        if gin_fastupdate not in ("on", "off"):
            raise ValueError("gin_fastupdate must be on or off")
        return "CREATE INDEX search_idx ON documents USING gin(to_tsvector('simple', body)) WITH (fastupdate=" + gin_fastupdate + ");"
    return {
        "stannum": "CREATE INDEX search_idx ON documents USING stannum(body);",
        "tin": "CREATE INDEX search_idx ON documents USING tin(body);",
        "gin": "CREATE INDEX search_idx ON documents USING gin(to_tsvector('simple', body));",
        "paradedb": "CREATE INDEX search_idx ON documents USING paradedb(id, body) WITH (key_field='id');",
        "pg_textsearch": "CREATE INDEX search_idx ON documents USING bm25(body) WITH (text_config='simple');",
    }[engine]


def percentile(values, fraction):
    if not values:
        return None
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)]


def summarize_logs(paths, names, elapsed):
    samples = collections.defaultdict(list)
    failures = collections.Counter()
    lag = []
    for path in paths:
        for line in path.read_text().splitlines():
            fields = line.split()
            if len(fields) < 6:
                raise ValueError(f"Malformed pgbench record in {path}")
            name = names[int(fields[3])]
            if fields[2] in ("failed", "skipped", "serialization", "deadlock"):
                failures[name + ":" + fields[2]] += 1
                continue
            samples[name].append(int(fields[2]) / 1000)
            if len(fields) >= 7:
                lag.append(int(fields[6]) / 1000)
    result = {}
    for name in names:
        vals = samples[name]
        result[name] = {
            "completed": len(vals), "completed_per_second": len(vals) / elapsed,
            "p50_ms": percentile(vals, .5), "p95_ms": percentile(vals, .95),
            "p99_ms": percentile(vals, .99) if len(vals) >= 1000 else None,
            "p99_insufficient_samples": len(vals) < 1000,
        }
    return {"queries": result, "failures": dict(failures),
            "schedule_lag_p95_ms": percentile(lag, .95), "wall_seconds": elapsed}


def validate(engine, rows, env, corpus=None):
    records = {}
    for case in (corpus["cases"] if corpus else CASES):
        expected = corpus["match_counts"][case[0]] if corpus else (rows // case[-1] if case[-1] else 0)
        where = predicate(engine, case)
        # Exact set comparison against a fixture oracle independent of text evaluation.
        truth = "false" if not case[-1] else f"id % {case[-1]} = 0"
        expected_sql = (f"SELECT id FROM benchmark_matches WHERE name = '{case[0]}'" if corpus
                        else f"SELECT id FROM documents WHERE {truth}")
        result = sql_json(f"""WITH actual AS (SELECT id FROM documents WHERE {where}),
expected AS ({expected_sql}),
differences AS ((SELECT * FROM actual EXCEPT SELECT * FROM expected)
 UNION ALL (SELECT * FROM expected EXCEPT SELECT * FROM actual))
SELECT json_build_object('count', (SELECT count(*) FROM actual),
 'differences', (SELECT count(*) FROM differences));""", env)
        records[case[0]] = result
        if result != {"count": expected, "differences": 0}:
            raise ValueError(f"Incorrect match set for {case[0]}: {result}")
    return records


def validate_ranked(engine, rows, env, corpus=None):
    records = {}
    cases = corpus["cases"] if corpus else CASES
    for case, (name, query) in zip(cases, workload(engine, "ranked", cases)):
        result = sql_json("SELECT coalesce(json_agg(r), '[]'::json) FROM (" + query.rstrip(";") + ") r;", env)
        count = corpus["match_counts"][case[0]] if corpus else (rows // case[-1] if case[-1] else 0)
        ids = [r["id"] for r in result]
        scores = [float(r["score"]) for r in result]
        if corpus and ids:
            valid_ids = sql_json(f"SELECT coalesce(json_agg(id), '[]'::json) FROM benchmark_matches "
                                 f"WHERE name = '{case[0]}' AND id IN ({','.join(str(int(i)) for i in ids)});", env)
            invalid_member = set(ids) != set(valid_ids)
        else:
            invalid_member = any(not (1 <= i <= rows) or not case[-1] or i % case[-1] for i in ids)
        if (len(ids) != min(10, count) or len(set(ids)) != len(ids)
                or invalid_member
                or any(not math.isfinite(s) for s in scores) or scores != sorted(scores, reverse=True)):
            raise ValueError(f"Incorrect ranked result shape/membership/order: {name}")
        records[name] = result
    # This is deliberately NOT a proof of BM25 arithmetic or global top-k optimality.
    return records


def snapshot(env):
    return sql_json("""SELECT json_build_object(
 'wal_lsn', pg_current_wal_insert_lsn()::text,
 'table_bytes', pg_table_size('documents'),
 'index_bytes', pg_indexes_size('documents'),
 'database', (SELECT row_to_json(s) FROM pg_stat_database s WHERE datname=current_database()),
 'table_io', (SELECT row_to_json(s) FROM pg_statio_user_tables s WHERE relname='documents'),
 'indexes', (SELECT json_agg(s) FROM pg_statio_user_indexes s WHERE relname='documents'));""", env)


def container_counters(name):
    result = {}
    for metric in ("cpu.stat", "cpu.max", "cpuset.cpus.effective", "memory.current",
                   "memory.peak", "memory.events", "memory.max", "memory.swap.max", "io.stat"):
        proc = subprocess.run(["docker", "exec", name, "cat", "/sys/fs/cgroup/" + metric],
                              capture_output=True, text=True)
        result[metric] = proc.stdout.strip() if proc.returncode == 0 else None
    return result


def provenance(out):
    paths = command(["git", "ls-files", "--cached", "--others", "--exclude-standard"], cwd=ROOT).splitlines()
    selected = [p for p in paths if p.startswith(("postgres/", "tinql/", "tokenizer/", "boldi-vigna/", "segment/", ".cargo/"))
                or p in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml")]
    hashes = {p: digest((ROOT / p).read_bytes()) for p in selected if (ROOT / p).is_file()}
    patch = command(["git", "diff", "HEAD", "--binary", "--", *selected], cwd=ROOT)
    (out / "source.patch").write_text(patch + "\n" if patch else "")
    tracked = set(command(["git", "ls-files"], cwd=ROOT).splitlines())
    return {"commit": command(["git", "rev-parse", "HEAD"], cwd=ROOT),
            "tags": command(["git", "tag", "--points-at", "HEAD"], cwd=ROOT).splitlines(),
            "source_sha256": digest(canonical(hashes)), "source_files": hashes,
            "source_dirty": bool(patch) or any(p not in tracked for p in selected),
            "working_tree_dirty": bool(command(["git", "status", "--porcelain"], cwd=ROOT))}


def run(args):
    if args.engine == "gin" and args.profile not in ("count", "mutation-count"):
        raise ValueError("GIN does not implement BM25; use --profile count or mutation-count")
    if args.rows < 1000 or args.rows % 1000:
        raise ValueError("--rows must be a positive multiple of 1000")
    corpus = dataset.verify(args.dataset, args.rows) if args.dataset else None
    cases = corpus["cases"] if corpus else CASES
    out = Path(args.output).resolve()
    out.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ)
    # Credentials remain in libpq environment/.pgpass, never in artifacts or command arguments.
    env["PGDATABASE"] = args.database
    if not args.database.startswith("stannum_bench_"):
        raise ValueError("Use a dedicated database named stannum_bench_*; create it before running")
    env["PGOPTIONS"] = env.get("PGOPTIONS", "") + f" -c default_text_search_config=simple -c statement_timeout={args.statement_timeout_ms}"
    mutating = args.profile in ("mutation", "mutation-count")
    for setting in (args.set if mutating else []):
        env["PGOPTIONS"] += " -c " + setting
    queries = workload(args.engine, args.profile, cases)
    checks = [(case[0], mutation.check_sql(case, predicate(args.engine, case),
               *(ranking(args.engine, case) if args.profile == "mutation" else (None, None))))
              for case in cases] if mutating else []
    settings = sql_json(SETTINGS_SQL, env)
    manifest = {
        "schema_version": 1, "started_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "status": "running", "engine": args.engine, "label": args.label,
        "environment": args.environment, "build_id": args.build_id,
        "artifact_sha256": digest(Path(args.artifact).read_bytes()) if args.artifact else None,
        "source": json.loads(Path(args.source_manifest).read_text()) if args.source_manifest else provenance(out),
        "harness_sha256": digest(Path(__file__).read_bytes() + Path(dataset.__file__).read_bytes()
                                 + (Path(mutation.__file__).read_bytes() if mutating else b"")),
        "host": {"platform": platform.platform(), "machine": platform.machine(), "cpus": os.cpu_count()},
        "server_version": psql("SELECT version();", env), "settings": settings,
        "pgbench_version": command(["pgbench", "--version"]),
        "config": {k: getattr(args, k) for k in ("rows", "body_repeat", "seconds", "warmup", "clients", "writers", "read_rate", "write_rate", "seed", "profile")},
        "fixture_sha256": digest(canonical(corpus)) if corpus else digest(fixture_sql(args.rows, args.body_repeat).encode()),
        "dataset": corpus,
        "sql_sha256": digest(canonical(queries)), "query_names": [q[0] for q in queries],
        "cache_policy": "warm read workload; no OS cache eviction", "load_model": "closed-loop readers; rate-scheduled single writer",
        "execution_context": json.loads(Path(args.context).read_text()) if args.context else None,
    }
    if mutating:
        mix = mutation.parse_mix(args.mix)
        kinds = [kind for kind in mutation.KINDS if mix[kind]]
        manifest["config"]["mutation"] = {
            "mix": mix, "settings": args.set, "check_interval": args.check_interval,
            "vacuum_interval": args.vacuum_interval, "sample_interval": args.sample_interval,
            "drain_vacuums": args.drain_vacuums, "min_vacuums": args.min_vacuums, "min_checks": args.min_checks,
            "autovacuum": False, "gin_fastupdate": args.gin_fastupdate if args.engine == "gin" else None, "writer_accounting": "live-key-wrap-v2; atomic exactly-one-row assertion",
            "bucket_seconds": args.bucket_seconds, "oracle": "regex sequential scan in one repeatable-read snapshot"}
        manifest["load_model"] = ("independently rate-scheduled readers when read_rate is set; "
                                  "rate-scheduled writers mixing inserts, deletes and match-changing updates; "
                                  "serialized scheduled VACUUM; separate periodic oracle checks")
        manifest["check_sql_sha256"] = digest(canonical(checks))
    save(out / "manifest.json", manifest)
    children = []
    maintenance = None
    try:
        extension = {"stannum": "stannum", "tin": "tin", "paradedb": "pg_search", "pg_textsearch": "pg_textsearch"}.get(args.engine)
        if extension:
            psql(f"CREATE EXTENSION IF NOT EXISTS {extension} CASCADE;", env)
        manifest["extensions"] = sql_json("SELECT json_object_agg(extname, extversion) FROM pg_extension;", env)
        if extension:
            # Load the library so its GUCs are visible; LOAD needs privileges a
            # managed server may not grant, so prefer calling into the extension.
            load = {"tin": "DO $$ BEGIN PERFORM tin.tokenize('load'); END $$; ",
                    "stannum": "DO $$ BEGIN PERFORM stannum.tokenize('load'); END $$; "}.get(extension, f"LOAD '{extension}'; ")
            manifest["settings"] = sql_json(load + SETTINGS_SQL, env)
        # Never overwrite an existing table. Each repetition uses a fresh dedicated database.
        (out / "fixture.sql").write_text("-- External verified corpus; see dataset.json.\n" if corpus else fixture_sql(args.rows, args.body_repeat))
        (out / "index.sql").write_text(index_sql(args.engine, args.gin_fastupdate) + "\n")
        setup_env = dict(env, PGOPTIONS=env["PGOPTIONS"] + " -c statement_timeout=0")
        if corpus:
            save(out / "dataset.json", corpus)
            psql("CREATE TABLE documents (id bigint PRIMARY KEY, body text NOT NULL); "
                 "CREATE TABLE benchmark_matches (name text NOT NULL, id bigint NOT NULL, PRIMARY KEY(name,id));", setup_env)
            for filename, table in (("documents.csv", "documents"), ("matches.csv", "benchmark_matches")):
                # Stream CSV through libpq; no Docker bind mount or multi-GB Python string.
                with (Path(args.dataset) / filename).open("rb") as source:
                    subprocess.run(["psql", "-X", "-q", "-v", "ON_ERROR_STOP=1", "-c",
                                    f"COPY {table} FROM STDIN WITH (FORMAT csv)"],
                                   stdin=source, env=setup_env, check=True)
            psql("ANALYZE benchmark_matches;", setup_env)
        else:
            psql(fixture_sql(args.rows, args.body_repeat), setup_env)
        start = time.monotonic()
        psql(index_sql(args.engine, args.gin_fastupdate), setup_env)
        manifest["index_build_seconds"] = time.monotonic() - start
        manifest["index_definition"] = psql("SELECT pg_get_indexdef('search_idx'::regclass);", env)
        psql("VACUUM ANALYZE documents;", setup_env)
        freespace = False
        if mutating:
            if args.engine == "gin":
                psql("CREATE EXTENSION IF NOT EXISTS pgstattuple;", setup_env)
            psql("ALTER TABLE documents SET (autovacuum_enabled=false);", setup_env)
            psql(mutation.pool_sql(args.rows), setup_env)
            probe = subprocess.run(["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1", "-c",
                                    "CREATE EXTENSION IF NOT EXISTS pg_freespacemap;"], env=setup_env, capture_output=True)
            freespace = probe.returncode == 0
            manifest["config"]["mutation"]["fsm_sampled"] = freespace
            for name, sql in checks:
                (out / f"check-{name}.sql").write_text(sql + "\n")
                plan = sql_json("SET enable_seqscan = off; EXPLAIN (FORMAT JSON) " + sql, env)
                save(out / f"plan-check-{name}.json", plan)
                if not mutation.oracle_plan_is_independent(plan):
                    raise RuntimeError(f"Oracle check for {name} does not separate index and sequential scans; see plan-check-{name}.json")
        save(out / "correctness-before.json", validate(args.engine, args.rows, env, corpus))
        if args.profile not in ("count", "mutation-count"):
            save(out / "ranked-before.json", validate_ranked(args.engine, args.rows, env, corpus))
        for i, (name, sql) in enumerate(queries):
            (out / f"query-{i}.sql").write_text(sql + "\n")
            plan = sql_json("EXPLAIN (ANALYZE, BUFFERS, WAL, SETTINGS, FORMAT JSON) " + sql, env)
            save(out / f"plan-{name}.json", plan)
        writer_sql = f"""\\set id random(1, {args.rows})
UPDATE documents SET body = CASE WHEN right(body, 8) = 'mutablea'
 THEN left(body, length(body)-8) || 'mutableb'
 ELSE left(body, length(body)-8) || 'mutablea' END WHERE id = :id;
"""
        writer_files = ["-f", str(out / "writer.sql")]
        writer_names = ["update"]
        if mutating:
            scripts = {kind: mutation.accounted_writer(script, kind) for kind, script in
                       mutation.writer_scripts(args.rows, cases).items()}
            writer_files, writer_names = [], kinds
            for kind in kinds:
                (out / f"writer-{kind}.sql").write_text(scripts[kind])
                writer_files += ["-f", f"{out / f'writer-{kind}.sql'}@{mix[kind]}"]
            manifest["writer_sql_sha256"] = digest(canonical([scripts[kind] for kind in kinds]))
        else:
            (out / "writer.sql").write_text(writer_sql)
        base = ["pgbench", "-n", "-M", "simple", "--random-seed", str(args.seed)]
        reader = base + ["-c", str(args.clients), "-j", str(min(args.clients, 4))]
        for i in range(len(queries)):
            reader += ["-f", str(out / f"query-{i}.sql")]
        (out / "warmup.txt").write_text(command(reader + ["-T", str(args.warmup)], env=env))
        if args.read_rate:
            reader += ["-R", str(args.read_rate)]
        save(out / "before.json", snapshot(env))
        if args.container:
            save(out / "cgroup-before.json", container_counters(args.container))
        jobs = [("reader", reader + ["-T", str(args.seconds), "-l", "--log-prefix", str(out / "reader-log")])]
        if args.write_rate:
            jobs.insert(0, ("writer", base + ["-c", str(args.writers), "-j", str(min(args.writers, 4)), *writer_files,
                         "-T", str(args.seconds), "-R", str(args.write_rate), "-l", "--log-prefix", str(out / "writer-log")]))
        starts = {}
        handles = []
        manifest["traffic_started_at"] = {}
        manifest["traffic_finished_at"] = {}
        if mutating:
            check_env = dict(env)
            maintenance = mutation.Maintenance(psql, sql_json, env, check_env, args.engine, checks, freespace, out,
                                               args.sample_interval, args.vacuum_interval, args.check_interval)
            manifest["traffic_origin_epoch"] = time.time()
            save(out / "manifest.json", manifest)
            maintenance.start(manifest["traffic_origin_epoch"])
        for name, cmd in jobs:
            handle = (out / f"{name}.txt").open("w")
            handles.append(handle)
            starts[name] = time.monotonic()
            manifest["traffic_started_at"][name] = dt.datetime.now(dt.timezone.utc).isoformat()
            children.append((name, subprocess.Popen(cmd, env=env, stdout=handle, stderr=subprocess.STDOUT)))
        summary = {}
        pending = dict(children)
        while pending:
            for name, proc in list(pending.items()):
                if proc.poll() is not None:
                    manifest["traffic_finished_at"][name] = dt.datetime.now(dt.timezone.utc).isoformat()
                    elapsed = time.monotonic() - starts[name]
                    if proc.returncode:
                        raise RuntimeError(f"{name} exited {proc.returncode}; see {name}.txt")
                    names = [q[0] for q in queries] if name == "reader" else writer_names
                    summary[name] = summarize_logs(out.glob(f"{name}-log.*"), names, elapsed)
                    rate = args.read_rate if name == 'reader' else args.write_rate
                    summary[name]['load'] = mutation.traffic_load(out.glob(f"{name}-log.*"), rate, args.seconds)
                    del pending[name]
            if maintenance and maintenance.failures:
                raise RuntimeError("Maintenance thread failed: " + json.dumps(maintenance.failures))
            if pending:
                time.sleep(.05)
        for handle in handles:
            handle.close()
        if maintenance:
            maintenance.finish()
            save_maintenance(out, maintenance)
            if maintenance.failures:
                raise RuntimeError("Maintenance thread failed: " + json.dumps(maintenance.failures))
            save(out / 'after.json', snapshot(env))
            affected = mutation.affected_rows(summary['writer'])
            summary['affected_rows'] = affected
            # All traffic/check connections have finished. These are quiescent
            # samples, unlike physical/heap counts sampled during active writes.
            before_drain = maintenance.sample(phase='drain')
            expected_rows = args.rows + affected.get('insert', {}).get('affected', 0) - affected.get('delete', {}).get('affected', 0)
            if before_drain['rows'] != expected_rows:
                raise RuntimeError('committed mutation accounting disagrees with final heap count')
            for _ in range(args.drain_vacuums):
                maintenance.vacuum(phase='drain')
            after_drain = maintenance.sample(phase='drain')
            save(out / 'after-drain.json', snapshot(env))
            summary['drain'] = dict(before=before_drain, after=after_drain, vacuums=args.drain_vacuums)
            if args.engine == 'stannum' and args.drain_vacuums:
                findings = sql_json("SELECT coalesce(json_agg(v), '[]'::json) FROM stannum.verify_index('search_idx',true) v", check_env)
                save(out / 'verification-after.json', findings)
                if findings:
                    raise RuntimeError('final index verification found inconsistencies')
            writer_start = dt.datetime.fromisoformat(manifest['traffic_started_at']['writer']).timestamp() - maintenance.origin
            writer_end = dt.datetime.fromisoformat(manifest['traffic_finished_at']['writer']).timestamp() - maintenance.origin
            overlapping = sum(v['phase'] == 'traffic' and writer_start <= v['started'] and v['t'] <= writer_end
                              for v in maintenance.vacuums)
            if overlapping < args.min_vacuums:
                raise RuntimeError(f'only {overlapping} VACUUM cycles completed during writer traffic; require {args.min_vacuums}')
            overlapping_checks = sum(writer_start <= c['started'] and c['t'] <= writer_end for c in maintenance.checks)
            if overlapping_checks < args.min_checks:
                raise RuntimeError(f'only {overlapping_checks} oracle rounds completed during writer traffic; require {args.min_checks}')
            save_maintenance(out, maintenance)
            summary["maintenance"] = {"samples": len(maintenance.samples), "vacuums": len(maintenance.vacuums),
                                      "checks": len(maintenance.checks), "writer_overlap_vacuums": overlapping,
                                      "writer_overlap_checks": overlapping_checks,
                                      "schedule": maintenance.schedule, "reclaim": mutation.reclaim_summary(maintenance.vacuums)}
        save(out / "summary.json", summary)
        if not maintenance:
            save(out / "after.json", snapshot(env))
        if args.container:
            save(out / "cgroup-after.json", container_counters(args.container))
        if mutating:
            # Match sets drifted by design; the oracle, not the fixture's expected sets, is the reference.
            save(out / "correctness-after.json", mutation.check_round(psql, check_env, checks))
            buckets = timeline(out, args.bucket_seconds)
            summary["worst_mutations"] = mutation.worst_mutations(buckets, kinds)
            save(out / "summary.json", summary)
            print((out / "timeline.txt").read_text())
        else:
            save(out / "correctness-after.json", validate(args.engine, args.rows, env, corpus))
            if args.profile not in ("count", "mutation-count"):
                save(out / "ranked-after.json", validate_ranked(args.engine, args.rows, env, corpus))
        traffic = [summary[name] for name in ("reader", "writer") if name in summary]
        if any(s["failures"] for s in traffic):
            raise RuntimeError("Transactions failed/skipped; run retained but not comparable")
        if any(q["completed"] == 0 for s in traffic for q in s["queries"].values()):
            raise RuntimeError("At least one workload had no completed transactions; lengthen the run")
        manifest["status"] = "complete"
    except BaseException as error:
        for _, proc in children:
            if proc.poll() is None:
                proc.terminate()
                proc.wait()
        if maintenance:
            maintenance.finish()
            save_maintenance(out, maintenance)
        manifest["status"] = "failed"
        manifest["error"] = type(error).__name__
        raise
    finally:
        manifest["finished_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
        save(out / "manifest.json", manifest)
    print(out)


def save_maintenance(out, maintenance):
    save(out / "samples.json", maintenance.samples)
    save(out / "vacuums.json", maintenance.vacuums)
    save(out / "checks.json", maintenance.checks)
    save(out / "maintenance-schedule.json", maintenance.schedule)


def timeline(out, bucket_seconds):
    """Bucketed reader/writer latency with layout samples and events; writes timeline.json/.txt."""
    manifest = json.loads((out / "manifest.json").read_text())
    kinds = [k for k in mutation.KINDS if manifest["config"]["mutation"]["mix"][k]]
    origin = manifest["traffic_origin_epoch"]
    buckets = mutation.bucket_logs(sorted(out.glob("reader-log.*")), manifest["query_names"], bucket_seconds, origin)
    for bucket in buckets:
        bucket['reader_schedule_lag_max_ms'] = bucket['schedule_lag_max_ms']
    by_number = {b["bucket"]: b for b in buckets}
    for bucket in mutation.bucket_logs(sorted(out.glob("writer-log.*")), kinds, bucket_seconds, origin):
        target = by_number.setdefault(bucket["bucket"], {"bucket": bucket["bucket"], "start_seconds": bucket["start_seconds"],
                                                         "queries": {}, "shapes": {}, "schedule_lag_max_ms": None})
        target["queries"].update(bucket["queries"])
        target["schedule_lag_max_ms"] = bucket["schedule_lag_max_ms"]
    buckets = [by_number[n] for n in sorted(by_number)]
    load = lambda name: json.loads((out / name).read_text()) if (out / name).exists() else []
    mutation.annotate(buckets, bucket_seconds, load("samples.json"), load("vacuums.json"), load("checks.json"))
    save(out / "timeline.json", {"bucket_seconds": bucket_seconds, "buckets": buckets})
    (out / "timeline.txt").write_text(mutation.render(buckets, kinds, bucket_seconds))
    return buckets


def comparison_keys(cross_engine=False):
    keys = ["schema_version", "environment", "host", "server_version", "pgbench_version", "config", "fixture_sha256", "harness_sha256", "cache_policy", "query_names", "execution_context"]
    # Extension GUCs differ across engines; compare PostgreSQL settings in that mode.
    if not cross_engine:
        keys += ["engine", "sql_sha256", "settings"]
    return keys


def comparison_mismatches(a, b, cross_engine=False):
    mismatches = [key for key in comparison_keys(cross_engine)
                  if not (cross_engine and key == 'config') and a.get(key) != b.get(key)]
    if cross_engine:
        configs = []
        for manifest in (a, b):
            config = json.loads(json.dumps(manifest['config']))
            if 'mutation' in config:
                config['mutation'].pop('gin_fastupdate', None)
                config['mutation']['settings'] = [s for s in config['mutation']['settings']
                                                  if not s.startswith(('stannum.', 'tin.'))]
            configs.append(config)
        if configs[0] != configs[1]:
            mismatches.append('config')
        if any(m['config']['profile'] not in ('count', 'mutation-count') for m in (a, b)):
            mismatches.append("ranking contract")
        pg_settings = [{k: v for k, v in m["settings"].items() if "." not in k} for m in (a, b)]
        if pg_settings[0] != pg_settings[1]:
            mismatches.append("PostgreSQL settings")
    if a["status"] != "complete" or b["status"] != "complete":
        mismatches.append("completion status")
    return mismatches


def compare(args):
    dirs = [Path(args.before), Path(args.after)]
    manifests = [json.loads((d / "manifest.json").read_text()) for d in dirs]
    a, b = manifests
    mismatches = comparison_mismatches(a, b, args.cross_engine)
    if mismatches:
        raise ValueError(f"Incomparable/incomplete runs; differing fields: {mismatches}")
    summaries = [json.loads((d / "summary.json").read_text()) for d in dirs]
    print("query,before_p50_ms,after_p50_ms,before_over_after,before_p95_ms,after_p95_ms,before_qps,after_qps")
    for name in a["query_names"]:
        qa, qb = [s["reader"]["queries"][name] for s in summaries]
        x, y = qa["p50_ms"], qb["p50_ms"]
        print(f"{name},{x},{y},{x/y if x and y else ''},{qa['p95_ms']},{qb['p95_ms']},"
              f"{qa['completed_per_second']},{qb['completed_per_second']}")
    if a["config"]["write_rate"]:
        for kind in summaries[0]["writer"]["queries"]:
            qa, qb = [s["writer"]["queries"][kind] for s in summaries]
            print(f"# Achieved {kind}s/s: {qa['completed_per_second']:.3f} -> {qb['completed_per_second']:.3f}; "
                  f"writer p95 ms: {qa['p95_ms']} -> {qb['p95_ms']}")
    print("# Descriptive single-run comparison, not statistical significance. Inspect writer throughput and tails.")


def history(args):
    writer = csv.writer(sys.stdout)
    writer.writerow(["started_at", "commit", "source_sha256", "dirty", "label", "engine", "environment", "cohort", "query", "completed", "p50_ms", "p95_ms", "p99_ms", "read_qps", "write_qps"])
    for path in sorted(Path(args.directory).glob("*/manifest.json")):
        m = json.loads(path.read_text())
        if m["status"] != "complete":
            continue
        s = json.loads((path.parent / "summary.json").read_text())
        cohort = digest(canonical({k: m.get(k) for k in comparison_keys()}))[:16]
        writes = sum(q["completed_per_second"] for q in s.get("writer", {}).get("queries", {}).values())
        for name, q in s["reader"]["queries"].items():
            writer.writerow([m["started_at"], m["source"]["commit"], m["source"]["source_sha256"],
                             m["source"]["source_dirty"], m["label"], m["engine"], m["environment"], cohort,
                             name, q["completed"], q["p50_ms"], q["p95_ms"], q["p99_ms"], q["completed_per_second"], writes])


def positive(value):
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    r = sub.add_parser("run")
    r.add_argument("--engine", choices=ENGINES, required=True)
    r.add_argument("--gin-fastupdate", choices=("on", "off"), help="GIN pending-list variant; default is PostgreSQL on")
    r.add_argument("--database", required=True)
    r.add_argument("--output", required=True)
    r.add_argument("--environment", required=True, help="Stable server hardware/resource identity; no credentials")
    r.add_argument("--build-id", required=True, help="Release build command/commit or immutable image digest")
    r.add_argument("--artifact", help="Optional path to installed extension binary to hash")
    r.add_argument("--context", help="Campaign resource/protocol JSON to preserve and compare")
    r.add_argument("--container", help="Local Docker container for timed-phase cgroup snapshots")
    r.add_argument("--label", default="")
    r.add_argument("--profile", choices=PROFILES, default="mixed")
    r.add_argument("--source-manifest", help=argparse.SUPPRESS)
    r.add_argument("--dataset", help="Verified dataset directory from dataset.py")
    r.add_argument("--statement-timeout-ms", type=positive, default=60000)
    r.add_argument("--rows", type=positive, default=10000)
    r.add_argument("--body-repeat", type=positive, default=1, help="Synthetic filler multiplier; incompatible with dataset")
    r.add_argument("--seconds", type=positive, default=60)
    r.add_argument("--warmup", type=positive, default=10)
    r.add_argument("--clients", type=positive, default=2)
    r.add_argument("--writers", type=positive, default=1, help="Writer connections sharing the total write rate")
    r.add_argument("--read-rate", type=positive, help="Total reader transactions/sec; omitted means closed-loop")
    r.add_argument("--write-rate", type=int, default=20)
    r.add_argument("--seed", type=positive, default=1729)
    m = r.add_argument_group("mutation profile")
    m.add_argument("--mix", default="insert=1,delete=1,update=1", help="Writer weights per mutation kind")
    m.add_argument("--set", action="append", default=[], metavar="NAME=VALUE", help="Session setting for every connection, e.g. stannum.write_buffer_docs=256")
    m.add_argument("--check-interval", type=positive, default=30, help="Seconds between oracle checks of every query")
    m.add_argument("--vacuum-interval", type=int, default=60, help="Seconds between VACUUM (INDEX_CLEANUP ON); 0 disables")
    m.add_argument("--sample-interval", type=positive, default=5, help="Seconds between index layout samples")
    m.add_argument("--bucket-seconds", type=positive, default=10, help="Latency bucket width in the timeline")
    m.add_argument("--drain-vacuums", type=int, default=0, help="Quiescent VACUUM passes after traffic; excluded from traffic latency")
    m.add_argument("--min-vacuums", type=int, default=0, help="Require this many VACUUM cycles wholly inside writer traffic")
    m.add_argument("--min-checks", type=int, default=0, help="Require this many oracle rounds wholly inside writer traffic")
    c = sub.add_parser("compare")
    c.add_argument("before")
    c.add_argument("after")
    c.add_argument("--cross-engine", action="store_true")
    h = sub.add_parser("history")
    h.add_argument("directory")
    t = sub.add_parser("timeline", help="Rebucket a mutation run's logs, samples and events")
    t.add_argument("directory")
    t.add_argument("--bucket-seconds", type=positive, default=10)
    args = parser.parse_args()
    if args.action == "run":
        if args.gin_fastupdate is not None and args.engine != "gin":
            parser.error("gin-fastupdate requires the gin engine")
        if args.dataset and args.body_repeat != 1:
            parser.error("body-repeat applies only to synthetic fixtures")
        if args.profile not in ('mutation', 'mutation-count') and (args.drain_vacuums or args.min_vacuums or args.min_checks):
            parser.error("drain and maintenance coverage requirements need the mutation profile")
        if args.write_rate < 0:
            parser.error("--write-rate must be nonnegative")
        if args.profile in ("mutation", "mutation-count"):
            if args.write_rate == 0 or args.vacuum_interval < 0:
                parser.error("the mutation profile needs a positive --write-rate and a nonnegative --vacuum-interval")
            if args.engine == "gin" and args.profile != "mutation-count":
                parser.error("GIN does not implement BM25; the mutation profile runs ranked shapes")
            mutation.parse_mix(args.mix)
            if args.drain_vacuums < 0 or args.min_vacuums < 0 or args.min_checks < 0:
                parser.error("drain-vacuums, min-vacuums and min-checks must be nonnegative")
            if args.min_vacuums and not args.vacuum_interval:
                parser.error("min-vacuums requires scheduled VACUUM")
            if any("=" not in setting or not setting.split("=", 1)[0] for setting in args.set):
                parser.error("--set expects NAME=VALUE")
    if args.action == "run":
        run(args)
    elif args.action == "compare":
        compare(args)
    elif args.action == "timeline":
        timeline(Path(args.directory).resolve(), args.bucket_seconds)
        print((Path(args.directory) / "timeline.txt").read_text())
    else:
        history(args)


if __name__ == "__main__":
    main()
