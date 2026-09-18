#!/usr/bin/env python3
"""Differential oracle: run identical fixtures and queries on two servers and
diff match sets and scores, bit for bit.

Both sides use equivalent SQL with engine-specific schema and access-method
names, comparing Stannum against TIN (or two Stannum builds). Connection details come from libpq environment variables read
from two env files, one per side, so credentials never appear on a command
line or in results. Each side gets a fresh table in its own database; the
script never drops anything it did not create.

    python3 benchmarks/oracle.py --left stannum.env --right tin.env --rows 5000 \
        --output benchmarks/results/oracle-01

States exercised, in order: after build; after deletes before VACUUM; after
VACUUM; after inserts into the mutable side; after REINDEX. Any difference in
result sets or in the float bits of full_score / score is reported.
Query shapes cover terms, Boolean, phrases, gaps, alternatives, slop,
proximity, relations, positional filters, wildcards, regex, ranges, fuzzy,
AT LEAST, boosts and the match-all form.
"""
import argparse
import json
import os
import struct
import subprocess
import sys
import time
from pathlib import Path

QUERIES = [
    "common", "rare", "absenttoken", "*",
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
    return json.loads(result.stdout)


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
# these compare match sets only.
REFERENCE_UNSCORED = frozenset(["alp*", "*eta", "b?ta", "MATCHES al.*a", "alpha TO beta", "* TO alpha"])


def comparable(observed, scores, query=""):
    """What is compared for one side: everything for `bits`; for `order` the
    match set and the rank order of the full and dense scores, so a reference
    with slightly different corpus statistics can still be checked, and the
    match set alone for shapes the reference leaves unscored."""
    if "error" in observed or scores == "bits":
        return observed
    if query in REFERENCE_UNSCORED:
        return {"ids": observed["ids"]}
    return {"ids": observed["ids"], "full": ranking(observed["full"]), "dense": ranking(observed["dense"])}


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
    args = parser.parse_args()
    sides = {"left": load_env(args.left), "right": load_env(args.right)}
    out = Path(args.output)
    out.mkdir(parents=True, exist_ok=True)
    engines = {"left": args.left_engine, "right": args.right_engine}
    for side, env in sides.items():
        engine = engines[side]
        run(f"CREATE EXTENSION IF NOT EXISTS {engine};", env)
        # CREATE TABLE fails on an existing fixture instead of deleting user data.
        run(FIXTURE.format(rows=args.rows, engine=engine), env)
    report = {"rows": args.rows, "scores": args.scores, "started": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "engines": engines, "states": {}}
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
