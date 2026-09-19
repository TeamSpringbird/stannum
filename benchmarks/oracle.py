#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Differential oracle: run identical fixtures and queries on two servers and
diff match sets, scores, and HTML/ANSI highlights.

Both sides use equivalent SQL with engine-specific schema and access-method
names, comparing Stannum against TIN (or two Stannum builds). Connection details come from libpq environment variables read
from two env files, one per side, so credentials never appear on a command
line or in results. Each side gets a fresh table in its own database; the
script never drops anything it did not create.

    python3 benchmarks/oracle.py --left stannum.env --right tin.env --rows 5000 \
        --output benchmarks/results/oracle-01

States exercised, in order: after build; after deletes before VACUUM; after
VACUUM; after inserts into the mutable side; after REINDEX. Any difference in
result sets, highlights, or the float bits of full_score / score is reported.
Query shapes cover terms, Boolean, phrases, gaps, alternatives, slop,
proximity, relations, positional filters, wildcards, regex, ranges, fuzzy,
AT LEAST, boosts and the match-all form.
"""
import argparse
import csv
import gzip
import math
import platform
import shutil
import json
import os
import struct
import subprocess
import sys
import time
from pathlib import Path

QUERIES = [
    "common", "rare", "absenttoken", "*",
    "eclair", "3.14", "can't", "wi-fi", "example.com", "👩‍💻",
    "common AND rare", "common OR rare", "rare AND NOT beta", "* AND NOT common",
    '"alpha beta"', '"beta alpha"', '"alpha _ gamma"', '"[alpha rare] beta"', '"alpha beta"~2',
    "alpha NEAR/2 gamma", "alpha THEN/0 beta", "beta THEN/0 alpha", "(alpha NEAR/5 gamma) WITHIN 3",
    "(alpha NEAR/5 gamma) ENCLOSES beta", "(alpha NEAR/5 gamma) NOT ENCLOSES beta",
    "beta ENCLOSED BY (alpha NEAR/5 gamma)", "alpha BEFORE gamma", "gamma AFTER alpha",
    "(alpha NEAR/2 beta) OVERLAPPING (beta NEAR/2 gamma)", "(alpha NEAR/2 beta) NOT OVERLAPPING rare",
    "rare IN FIRST 3 WORDS", "rare IN LAST 50%", "common IN MIDDLE 50%", "rare IN WORDS 2 TO 4",
    "alp*", "*eta", "b?ta", "MATCHES al.*a", "alpha TO beta", "* TO alpha", "rare~1", "gamma~0:2",
    "AT LEAST 2 OF [alpha beta rare]", "ALL OF [alpha beta gamma]", "rare^2 OR common",
    "AT LEAST 1 OF [alpha, rare] THEN/1 beta", '"alpha [MATCHES b.*]"',
]

FIXTURE = """
CREATE TABLE oracle_docs (id int PRIMARY KEY, body text);
INSERT INTO oracle_docs
SELECT n,
  'common w' || (n % 31) || ' ' ||
  CASE WHEN n % 100 = 0 THEN 'rare ' ELSE '' END ||
  CASE WHEN n % 250 = 0 THEN 'alpha beta gamma ' WHEN n % 251 = 0 THEN 'alpha x gamma beta ' ELSE '' END ||
  CASE WHEN n % 7 = 0 THEN 'Éclair naïve ' ELSE '' END ||
  CASE WHEN n % 11 = 0 THEN '3.14 can''t wi-fi https://example.com/a 👩‍💻 ' ELSE '' END ||
  repeat('pad ', n % 5)
FROM generate_series(1, {rows}) n;
INSERT INTO oracle_docs VALUES ({rows} + 1, ''), ({rows} + 2, NULL), ({rows} + 3, 'alpha alpha beta beta rare');
CREATE INDEX oracle_docs_idx ON oracle_docs USING {engine}(body);
"""

STATES = [
    ("built", None),
    ("deleted", "DELETE FROM oracle_docs WHERE id % 3 = 0;"),
    ("vacuumed", "VACUUM (INDEX_CLEANUP ON) oracle_docs;"),
    ("inserted", "INSERT INTO oracle_docs SELECT n, 'late alpha beta rare w' || (n % 4) FROM generate_series(900000, 900300) n;"),
    ("reindexed", "REINDEX INDEX oracle_docs_idx;"),
]


def load_env(path):
    env = dict(os.environ)
    for line in Path(path).read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        line = line.removeprefix("export ")
        key, _, value = line.partition("=")
        env[key.strip()] = value.strip().strip("'\"")
    env["PGOPTIONS"] = env.get("PGOPTIONS", "") + " -c enable_seqscan=off"
    return env


def psql(sql, env):
    return subprocess.run(["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1"], input=sql, env=env,
                          text=True, capture_output=True)


def run(sql, env):
    result = psql(sql, env)
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip())
    return result.stdout


def observe(env, query, engine="stannum"):
    """Match set and score bits, or the error text if the server rejects the query."""
    literal = query.replace("'", "''")
    sql = f"""SELECT json_build_object(
      'ids', (SELECT coalesce(json_agg(id ORDER BY id), '[]') FROM oracle_docs WHERE body ==> '{literal}'),
      'full', (SELECT coalesce(json_agg(json_build_array(id, float4send({engine}.full_score(ctid))::text) ORDER BY id), '[]')
               FROM oracle_docs WHERE body ==> '{literal}'),
      'dense', (SELECT coalesce(json_agg(json_build_array(id, float4send({engine}.score(ctid))::text) ORDER BY id), '[]')
                FROM oracle_docs WHERE body ==> '{literal}'),
      'max', (SELECT float4send(max(m))::text FROM (SELECT {engine}.max_score(ctid) m FROM oracle_docs WHERE body ==> '{literal}' LIMIT 1) s)
    );"""
    result = psql(sql, env)
    if result.returncode != 0:
        return {"error": result.stderr.strip().splitlines()[0] if result.stderr.strip() else "unknown error"}
    observed = json.loads(result.stdout)
    observed["highlights"] = {}
    for style, function in HIGHLIGHT_FUNCTIONS[engine].items():
        # Separate statements preserve membership/scoring evidence when a
        # reference highlighter rejects a shape. Never silently omit errors.
        highlighted = psql(f"""SELECT json_build_array(id, {function}(body))
            FROM oracle_docs WHERE body ==> '{literal}' ORDER BY id;""", env)
        observed["highlights"][style] = ([json.loads(row) for row in highlighted.stdout.splitlines()]
            if highlighted.returncode == 0
            else {"error": highlighted.stderr.strip()})
    return observed


# Verified from Lead's pg_proc catalog by script/reference-oracle. The one-
# argument calls deliberately exercise implicit query binding in both engines.
HIGHLIGHT_FUNCTIONS = {
    "stannum": {"html": "stannum.highlight", "ansi": "stannum.highlight_ansi"},
    "tin": {"html": "tin.highlight", "ansi": "tin.highlight_ansi"},
}

# Query -> reason. Only confirmed Lead highlighting defects belong here;
# observations remain in oracle.json even when excluded from order comparison.
REFERENCE_UNHIGHLIGHTED = {}


def score_value(bits):
    """float4send output such as '\\x407403eb' as a Python float."""
    return struct.unpack(">f", bytes.fromhex(bits[2:]))[0]


def ranking(scored):
    """Document ids ordered as a top-k scan would emit them: score descending,
    then id ascending. Ties in score keep both ids adjacent, so a reference
    that differs only in the last bits of an IDF still ranks identically."""
    return [id_ for id_, _ in sorted(scored, key=lambda pair: (-score_value(pair[1]), pair[0]))]


# Shapes the Lead reference does not score: it returns zero for every match of
# an expansion, while TIN and Stannum score the expanded terms. In `order` mode
# these compare match sets and highlights, but not rank order.
REFERENCE_UNSCORED = frozenset(["alp*", "*eta", "b?ta", "MATCHES al.*a", "alpha TO beta", "* TO alpha"])


def comparable(observed, scores, query=""):
    """What is compared for one side: everything for `bits`; for `order` the
    match set and the rank order of the full and dense scores, so a reference
    with slightly different corpus statistics can still be checked, and the
    exact highlights even for shapes the reference leaves unscored."""
    if "error" in observed or scores == "bits":
        return observed
    result = {"ids": observed["ids"]}
    if query not in REFERENCE_UNSCORED:
        result.update(full=ranking(observed["full"]), dense=ranking(observed["dense"]))
    if query not in REFERENCE_UNHIGHLIGHTED:
        result["highlights"] = observed["highlights"]
    return result


def unexpected_highlight_error(observed, scores, query):
    if scores == "order" and query in REFERENCE_UNHIGHLIGHTED:
        return False
    return any(isinstance(value, dict) and "error" in value
               for value in observed.get("highlights", {}).values())


def trace_queries(path):
    records = json.loads(Path(path).read_text())['queries']
    result = [(f"{r['source_id']}:{style}", r['engines']['tin'][style])
              for r in records for style in ('conjunction', 'disjunction', 'phrase')]
    if not result or len({name for name, _ in result}) != len(result):
        raise ValueError('trace must have distinct source IDs and all three forms')
    if any(not isinstance(query, str) or not query for _, query in result):
        raise ValueError('trace query must be nonempty text')
    return result


def trace_sql(query, engine, topk=False):
    literal = query.replace("'", "''")
    order = f'{engine}.full_score(ctid) DESC LIMIT 10' if topk else 'id'
    return (f'SELECT json_build_array(id,float4send({engine}.full_score(ctid))::text) '
            f"FROM oracle_trace_docs WHERE body ==> '{literal}' ORDER BY {order}")


def checked_scores(rows):
    result = {}
    for row in rows:
        if (not isinstance(row, list) or len(row) != 2 or type(row[0]) is not int
                or row[0] in result or not math.isfinite(score_value(row[1]))):
            raise ValueError('invalid, duplicate, or nonfinite scored result')
        result[row[0]] = row[1]
    return result


def compare_trace(left, right, topk):
    candidate, reference, selected = map(checked_scores, (left, right, topk))
    problems = []
    if candidate.keys() != reference.keys():
        problems.append('membership')
    if candidate != reference:
        problems.append('full_score_bits')
    expected = sorted(reference.values(), key=score_value, reverse=True)[:10]
    actual = [bits for _, bits in topk]
    if (len(selected) != min(10, len(reference))
            or any(reference.get(id_) != bits for id_, bits in selected.items())
            or actual != sorted(actual, key=score_value, reverse=True)
            or sorted(actual, key=score_value, reverse=True) != expected):
        problems.append('top10')
    return problems


def run_trace(args):
    # This is a read-only benchmark-corpus check, separate from the synthetic
    # mutation/highlight oracle below. No score exclusions or tolerances apply.
    import dataset
    import run as bench
    if args.rows <= 0 or args.budget_seconds <= 0 or args.statement_seconds <= 0:
        raise ValueError('rows and time budgets must be positive')
    if args.scores != 'bits' or args.left_engine != 'stannum' or args.right_engine != 'tin':
        raise ValueError('trace mode compares Stannum against Lead with exact score bits')
    if not args.reference_source or not args.trace:
        raise ValueError('trace mode requires --trace and --reference-source')
    root = Path(args.output).resolve()
    root.mkdir(parents=True, exist_ok=False)
    started = time.monotonic()
    deadline = started + args.budget_seconds
    sides = {'left': load_env(args.left), 'right': load_env(args.right)}
    # Use normal planning for this workload, including the optimized top-k
    # projection. The older synthetic oracle deliberately forces index access.
    for env in sides.values():
        env['PGOPTIONS'] = env['PGOPTIONS'].removesuffix(' -c enable_seqscan=off')
    engines = {'left': 'stannum', 'right': 'tin'}
    owned = []
    report = dict(status='running', rows=args.rows, budget_seconds=args.budget_seconds,
                  statement_seconds=args.statement_seconds, queries_completed=0,
                  differences=0, checks=['membership', 'full_score_bits', 'top10'],
                  score_exclusions=[], queries=[])
    def save():
        report['elapsed_seconds'] = time.monotonic() - started
        (root / 'oracle.json').write_text(json.dumps(report, indent=2) + '\n')
    def command(command_args, side, *, stdin=None, sql=None):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('verification wall-clock budget exhausted')
        timeout = min(args.statement_seconds, remaining)
        env = dict(sides[side])
        env['PGOPTIONS'] += f' -c statement_timeout={max(1, int(timeout * 1000))}'
        result = subprocess.run(command_args, input=sql, stdin=stdin, env=env,
                                text=stdin is None, capture_output=True, timeout=timeout + 2)
        if result.returncode:
            error = result.stderr if isinstance(result.stderr, str) else result.stderr.decode()
            raise RuntimeError(error.strip())
        return result.stdout
    def sql(statement, side):
        return command(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'], side, sql=statement)
    save()
    try:
        report['corpus'] = corpus = dataset.verify(args.dataset)
        if args.rows > corpus['rows']:
            raise ValueError('requested rows exceed verified dataset')
        queries = trace_queries(args.trace)
        report['queries_expected'] = len(queries)
        source = Path(args.reference_source).resolve()
        if subprocess.check_output(['git', '-C', str(source), 'status', '--porcelain'], text=True).strip():
            raise ValueError('Lead reference checkout must be clean')
        report['lead_revision'] = subprocess.check_output(
            ['git', '-C', str(source), 'rev-parse', 'HEAD'], text=True).strip()
        report['stannum_source'] = bench.provenance(root)
        libdir = Path(subprocess.check_output(['pg_config', '--pkglibdir'], text=True).strip())
        suffix = '.dylib' if platform.system() == 'Darwin' else '.so'
        guard_paths = [Path(__file__).resolve(), Path(dataset.__file__).resolve(),
                       Path(bench.__file__).resolve(), Path(args.trace).resolve(),
                       libdir / ('stannum' + suffix), libdir / ('tin' + suffix)]
        report['files'] = {str(p): dataset.sha256(p) for p in guard_paths}
        protocol = root / 'protocol'
        protocol.mkdir()
        for path in guard_paths[:4]:
            shutil.copy2(path, protocol / path.name)
        report['host'] = dict(platform=platform.platform(), cpu_count=os.cpu_count())
        prefix = root / 'input.csv'
        count = 0
        with (Path(args.dataset) / 'documents.csv').open() as src, prefix.open('w') as dst:
            reader, writer = csv.reader(src), csv.writer(dst)
            for row in reader:
                if count == args.rows:
                    break
                writer.writerow(row)
                count += 1
        if count != args.rows:
            raise ValueError('corpus prefix is shorter than requested')
        report['input_sha256'] = dataset.sha256(prefix)
        report['input_bytes'] = prefix.stat().st_size
        report['files'][str(prefix)] = report['input_sha256']
        report['servers'] = {}
        for side, engine in engines.items():
            sql(f'CREATE EXTENSION IF NOT EXISTS {engine}', side)
            sql('CREATE TABLE oracle_trace_docs(id bigint PRIMARY KEY, body text NOT NULL)', side)
            owned.append(side)
            with prefix.open('rb') as data:
                command(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1', '-c',
                         'COPY oracle_trace_docs FROM STDIN WITH (FORMAT csv)'], side, stdin=data)
            sql(f'CREATE INDEX oracle_trace_idx ON oracle_trace_docs USING {engine}(body)', side)
            sql('VACUUM ANALYZE oracle_trace_docs', side)
            report['servers'][side] = json.loads(sql(
                "SELECT json_build_object('version',version(),'settings',(" +
                "SELECT json_object_agg(name,setting) FROM pg_settings WHERE name IN " +
                "('shared_buffers','work_mem','max_parallel_workers_per_gather','jit','enable_seqscan'))," +
                f"'extension_version',(SELECT extversion FROM pg_extension WHERE extname='{engine}')," +
                "'rows',(SELECT count(*) FROM oracle_trace_docs)," +
                "'body_bytes',(SELECT sum(octet_length(body)) FROM oracle_trace_docs))", side))
        report['setup_seconds'] = time.monotonic() - started
        save()
        with gzip.open(root / 'observations.jsonl.gz', 'wt') as observations:
            for name, query in queries:
                report['active_query'] = name
                item = dict(query_id=name, query=query, seconds={})
                for side, engine in engines.items():
                    tick = time.monotonic()
                    item[side] = [json.loads(line) for line in sql(trace_sql(query, engine), side).splitlines()]
                    item['seconds'][side] = time.monotonic() - tick
                tick = time.monotonic()
                item['topk'] = [json.loads(line) for line in sql(trace_sql(query, 'stannum', True), 'left').splitlines()]
                item['seconds']['topk'] = time.monotonic() - tick
                item['problems'] = compare_trace(item['left'], item['right'], item['topk'])
                observations.write(json.dumps(item) + '\n')
                observations.flush()
                report['queries'].append({k: item[k] for k in ('query_id', 'seconds', 'problems')})
                report['queries'][-1]['matched_rows'] = len(item['right'])
                report.pop('active_query', None)
                report['queries_completed'] += 1
                report['differences'] += bool(item['problems'])
                if report['queries_completed'] % 25 == 0 or item['problems']:
                    save()
                    print(f"{args.rows} rows: {report['queries_completed']}/{len(queries)} forms, "
                          f"{report['differences']} differences, {report['elapsed_seconds']:.1f}s", flush=True)
        for path, digest in report['files'].items():
            if dataset.sha256(path) != digest:
                raise ValueError('source or installed binary changed during verification: ' + path)
        if time.monotonic() > deadline:
            raise TimeoutError('verification exceeded wall-clock budget')
        report['status'] = 'passed' if not report['differences'] else 'mismatch'
    except Exception as error:
        report.update(status='incomplete', error=str(error))
    finally:
        # Cleanup is outside the verification budget and only touches tables
        # whose CREATE succeeded in this invocation.
        save()
        if not args.keep:
            for side in owned:
                try:
                    result = subprocess.run(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'],
                        input='DROP TABLE oracle_trace_docs', text=True, capture_output=True,
                        env=dict(sides[side], PGOPTIONS=sides[side]['PGOPTIONS'] +
                                 ' -c statement_timeout=10000'), timeout=12)
                    if result.returncode:
                        report.setdefault('cleanup_errors', []).append(result.stderr.strip())
                except (OSError, subprocess.TimeoutExpired) as error:
                    report.setdefault('cleanup_errors', []).append(str(error))
        if report.get('cleanup_errors'):
            report['status'] = 'incomplete'
        save()
    print(f"{report['status']}: {report['queries_completed']}/{report.get('queries_expected', 0)} forms, "
          f"{report['differences']} differences, {report['elapsed_seconds']:.1f}s; {root / 'oracle.json'}", flush=True)
    return 0 if report['status'] == 'passed' else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--left", required=True, help="env file for the first server (libpq variables)")
    parser.add_argument("--right", required=True, help="env file for the second server")
    parser.add_argument("--left-engine", choices=("stannum", "tin"), default="stannum")
    parser.add_argument("--right-engine", choices=("stannum", "tin"), default="tin")
    parser.add_argument("--rows", type=int, default=5000)
    parser.add_argument("--output", required=True)
    parser.add_argument("--keep", action="store_true", help="leave oracle_docs in place afterwards")
    parser.add_argument("--scores", choices=("bits", "order"), default="bits",
                        help="bits: scores must match bit for bit (TIN); order: match sets and rank "
                             "order must match (the Lead reference, whose corpus size counts empty documents)")
    parser.add_argument("--dataset", type=Path, help="verified Wikipedia corpus; enables read-only trace mode")
    parser.add_argument("--trace", type=Path, help="published queries.json with TIN forms")
    parser.add_argument("--reference-source", type=Path, help="clean Lead checkout used to build the installed reference")
    parser.add_argument("--budget-seconds", type=int, default=900)
    parser.add_argument("--statement-seconds", type=int, default=60)
    args = parser.parse_args()
    if args.dataset:
        return run_trace(args)
    if args.trace or args.reference_source:
        parser.error('--trace and --reference-source require --dataset')
    sides = {"left": load_env(args.left), "right": load_env(args.right)}
    out = Path(args.output)
    out.mkdir(parents=True, exist_ok=True)
    engines = {"left": args.left_engine, "right": args.right_engine}
    for side, env in sides.items():
        engine = engines[side]
        run(f"CREATE EXTENSION IF NOT EXISTS {engine};", env)
        # CREATE TABLE fails on an existing fixture instead of deleting user data.
        run(FIXTURE.format(rows=args.rows, engine=engine), env)
    report = {"rows": args.rows, "scores": args.scores, "started": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "engines": engines, "highlight_exclusions": REFERENCE_UNHIGHLIGHTED if args.scores == "order" else {}, "states": {}}
    differences = 0
    for state, transition in STATES:
        if transition:
            for env in sides.values():
                run(transition, env)
        results = {}
        for query in QUERIES:
            observed = {side: observe(env, query, engines[side]) for side, env in sides.items()}
            same = (comparable(observed["left"], args.scores, query)
                    == comparable(observed["right"], args.scores, query))
            same = same and not any(unexpected_highlight_error(value, args.scores, query)
                                    for value in observed.values())
            if not same:
                differences += 1
            results[query] = {"same": same, **observed}
        report["states"][state] = results
        mismatched = [q for q, r in results.items() if not r["same"]]
        print(f"{state}: {len(QUERIES) - len(mismatched)} agree, {len(mismatched)} differ" +
              (": " + "; ".join(mismatched) if mismatched else ""))
    report["differences"] = differences
    (out / "oracle.json").write_text(json.dumps(report, indent=2) + "\n")
    if not args.keep:
        for env in sides.values():
            run("DROP TABLE IF EXISTS oracle_docs;", env)
    print(f"{differences} differing query/state pairs; report at {out / 'oracle.json'}")
    return 1 if differences else 0


if __name__ == "__main__":
    sys.exit(main())
