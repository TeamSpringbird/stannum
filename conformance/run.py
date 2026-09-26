#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Run the TIN interface conformance suite against one engine.

    python3 conformance/run.py --engine tin|stannum [--dsn-env NAME]
        [--record OUTDIR | --check EXPECTED_DIR] [--area A] [--case ID]
        [--skip-crash] [--host-note TEXT]

The connection string is read from the environment variable named by
--dsn-env (default CONFORMANCE_DSN), never from the command line. Every run
works in a fresh scratch schema, conformance_<random>, which is dropped at
the end, also when the run fails. See conformance/README.md.
"""

import argparse
import datetime
import decimal
import fnmatch
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import sys
import time

try:
    import psycopg
    import yaml
except ImportError as error:  # pragma: no cover - environment guidance
    sys.exit(f"{error}; install the dependencies with: pip install pyyaml psycopg")

RUNNER_VERSION = "1"
SUITE = Path(__file__).resolve().parent
CAPTURES = ("ids", "count", "ranked", "scores", "highlight", "value", "error", "script")
PAD_ROWS_START = 1000
DEFAULT_SCORE = "{engine}.full_score(ctid)"
DEFAULT_TIMEOUT = "60s"
RECONNECT_SECONDS = 120


class CaseError(Exception):
    """A case file is malformed."""


# ---------------------------------------------------------------- case files


def load_cases(directory):
    """Read every cases/*.yaml file: returns (corpora by name, cases in file order)."""
    corpora, cases, seen = {}, [], set()
    for path in sorted(directory.glob("*.yaml")):
        document = yaml.safe_load(path.read_text()) or {}
        area = document.get("area")
        if not area:
            raise CaseError(f"{path.name}: missing top-level 'area'")
        for name, corpus in (document.get("corpora") or {}).items():
            if name in corpora:
                raise CaseError(f"{path.name}: corpus {name!r} is defined twice")
            if not re.fullmatch(r"[a-z][a-z0-9_]*", name):
                raise CaseError(f"{path.name}: corpus name {name!r} must be [a-z][a-z0-9_]*")
            corpora[name] = dict(corpus, name=name, file=path.name)
        defaults = document.get("defaults") or {}
        for raw in document.get("cases") or []:
            case = merge_defaults(defaults, raw)
            case.setdefault("area", area)
            case["file"] = path.name
            validate_case(case, seen)
            seen.add(case["id"])
            cases.append(case)
    for case in cases:
        for name in case_corpora(case):
            if name not in corpora:
                raise CaseError(f"{case['id']}: unknown corpus {name!r}")
    return corpora, cases


def case_corpora(case):
    """The corpora a case uses: its own and any a capture names."""
    names = [case["corpus"]] if case.get("corpus") else []
    names += [capture["corpus"] for capture in case["capture"] if capture.get("corpus")]
    return list(dict.fromkeys(names))


def merge_defaults(defaults, case):
    merged = dict(defaults)
    merged.update(case)
    if "settings" in defaults or "settings" in case:
        merged["settings"] = {**defaults.get("settings", {}), **case.get("settings", {})}
    return merged


def validate_case(case, seen):
    case_id = case.get("id")
    if not case_id or not re.fullmatch(r"[A-Za-z0-9_-]+(\.[A-Za-z0-9_-]+)+", case_id):
        raise CaseError(f"{case_id!r}: ids are dotted words, e.g. span.then_phrase_operand.1 or catalog.F-02")
    if case_id in seen:
        raise CaseError(f"{case_id}: duplicate case id")
    for field in ("description", "capture"):
        if not case.get(field):
            raise CaseError(f"{case_id}: missing {field!r}")
    if "query" in case and "query_sql" in case:
        raise CaseError(f"{case_id}: give 'query' or 'query_sql', not both")
    case["capture"] = [normalize_capture(case_id, item) for item in case["capture"]]
    names = [capture["as"] for capture in case["capture"]]
    if len(set(names)) != len(names):
        raise CaseError(f"{case_id}: two captures share a name; use 'as' to rename one")
    has_query = any(key in case for key in ("query", "query_sql"))
    for capture in case["capture"]:
        if "query" in capture and "query_sql" in capture:
            raise CaseError(f"{case_id}: capture {capture['as']}: give 'query' or 'query_sql', not both")
        own_query = any(key in capture for key in ("query", "query_sql"))
        needs_query = capture["kind"] in ("ids", "count", "ranked", "scores") and "sql" not in capture
        if needs_query and not (has_query or own_query):
            raise CaseError(f"{case_id}: capture {capture['as']} needs 'query' or 'query_sql'")
        if capture["kind"] == "value" and "sql" not in capture and "sql" not in case:
            raise CaseError(f"{case_id}: capture value needs 'sql'")
        if capture["kind"] == "script":
            steps = capture.get("steps")
            if not steps or not all(isinstance(step, dict) and "sql" in step for step in steps):
                raise CaseError(f"{case_id}: capture script needs 'steps', each with 'sql'")


def normalize_capture(case_id, item):
    """A capture is a name ('ids') or a one-key mapping ({'ranked': {'k': 1}})."""
    if isinstance(item, str):
        kind, params = item, {}
    elif isinstance(item, dict) and len(item) == 1:
        kind, params = next(iter(item.items()))
        params = dict(params or {})
    else:
        raise CaseError(f"{case_id}: capture {item!r} must be a name or a one-key mapping")
    if kind not in CAPTURES:
        raise CaseError(f"{case_id}: unknown capture {kind!r}; known: {', '.join(CAPTURES)}")
    params["kind"] = kind
    params.setdefault("as", kind)
    return params


def selected(cases, areas, patterns):
    chosen = []
    for case in cases:
        if areas and case["area"] not in areas:
            continue
        if patterns and not any(fnmatch.fnmatchcase(case["id"], p) for p in patterns):
            continue
        chosen.append(case)
    return chosen


def crash_tagged(case, engine, version):
    tags = case.get("crashes") or []
    return engine in tags or f"{engine}-{version}" in tags


# ---------------------------------------------------------------- SQL


def literal(text):
    return "'" + str(text).replace("'", "''") + "'"


def substitute(sql, values):
    """Replace {name} placeholders we know; leave any other braces (array literals) alone."""
    def replace(match):
        name = match.group(1)
        return values[name] if name in values else match.group(0)
    previous = None
    while previous != sql:  # placeholders may expand to placeholders ({score} -> {engine})
        previous, sql = sql, re.sub(r"\{([a-z_]+)\}", replace, sql)
    return sql


def jsonable(value):
    if isinstance(value, (bool, int, str)) or value is None:
        return value
    if isinstance(value, float):
        return value
    if isinstance(value, decimal.Decimal):
        return str(value)
    if isinstance(value, (list, tuple)):
        return [jsonable(item) for item in value]
    if isinstance(value, dict):
        return {str(key): jsonable(item) for key, item in value.items()}
    if isinstance(value, (bytes, memoryview)):
        return bytes(value).hex()
    return str(value)


class ConnectionLost(Exception):
    """The session's connection died while running a statement."""


class Session:
    """A working connection plus an idle sentinel that dies when PostgreSQL reinitializes."""

    def __init__(self, dsn, schema):
        self.dsn, self.schema = dsn, schema
        self.connection = self.sentinel = None
        self.connect()

    def open(self):
        return psycopg.connect(self.dsn, connect_timeout=20, application_name="tin-conformance")

    def connect(self):
        self.connection = self.open()
        self.connection.autocommit = True
        if self.schema:
            self.connection.execute(f"SET search_path = {self.schema}, public")
        self.sentinel = self.open()
        self.sentinel.autocommit = True

    def close(self):
        for connection in (self.connection, self.sentinel):
            try:
                if connection is not None:
                    connection.close()
            except Exception:
                pass

    def sentinel_alive(self):
        try:
            self.sentinel.execute("SELECT 1").fetchone()
            return True
        except psycopg.Error:
            return False

    def server_reinitialized(self, grace=5.0):
        """After a lost connection: did the whole server restart? The postmaster
        signals the other backends asynchronously, so give the sentinel a moment."""
        deadline = time.monotonic() + grace
        while time.monotonic() < deadline:
            if not self.sentinel_alive():
                return True
            time.sleep(0.25)
        return False

    def reconnect_after_crash(self, delay=2.0):
        """Wait for the server to finish crash recovery, then open fresh connections."""
        self.close()
        deadline = time.monotonic() + RECONNECT_SECONDS
        time.sleep(delay)
        while True:
            try:
                self.connect()
                return
            except psycopg.OperationalError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(1)

    def run(self, sql, settings, fetch=True):
        """Run sql in its own transaction with SET LOCAL settings; always roll back."""
        connection = self.connection
        try:
            connection.autocommit = False
            try:
                with connection.cursor() as cursor:
                    cursor.execute("SELECT set_config('statement_timeout', %s, true)",
                                   (settings.get("statement_timeout", DEFAULT_TIMEOUT),))
                    for name, value in settings.items():
                        if name != "statement_timeout":
                            cursor.execute("SELECT set_config(%s, %s, true)", (name, str(value)))
                    cursor.execute(sql)
                    rows = cursor.fetchall() if fetch and cursor.description else []
            finally:
                if not connection.closed:
                    connection.rollback()
                    connection.autocommit = True
            return rows
        except psycopg.OperationalError as error:
            if connection.closed or connection.broken:
                raise ConnectionLost(str(error)) from error
            raise


# ---------------------------------------------------------------- engine facts


def engine_facts(session, engine):
    row = session.connection.execute(
        "SELECT extversion FROM pg_extension WHERE extname = %s", (engine,)).fetchone()
    if row is None:
        try:
            session.connection.execute(f"CREATE EXTENSION IF NOT EXISTS {engine}")
        except psycopg.Error as error:
            sys.exit(f"extension {engine} is not installed and could not be created: {error}")
        row = session.connection.execute(
            "SELECT extversion FROM pg_extension WHERE extname = %s", (engine,)).fetchone()
    server = session.connection.execute("SELECT version()").fetchone()[0]
    return row[0], server


def suite_commit():
    try:
        head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=SUITE, text=True,
                                       stderr=subprocess.DEVNULL).strip()
        dirty = subprocess.check_output(["git", "status", "--porcelain", "--", "."], cwd=SUITE,
                                        text=True, stderr=subprocess.DEVNULL).strip()
        return head + ("-dirty" if dirty else "")
    except (OSError, subprocess.CalledProcessError):
        return "unknown"


# ---------------------------------------------------------------- corpora


def corpus_table(name):
    return f"corpus_{name}"


def corpus_values(engine, name, schema=None):
    values = {"engine": engine}
    if name:
        table = corpus_table(name)
        values.update(table=table, index=f"{table}_idx")
    if schema:
        values["schema"] = schema
    return values


def build_corpus(session, engine, corpus):
    """Create the corpus table, fill it, and build its index.

    Defaults: columns `id int PRIMARY KEY, body text`, rows [id, body] (body
    may be null), `pad: N` adds rows (1000 + n, 'pad' || n) for n in 1..N, and
    one index `USING {engine}(body)` with `index_options`, built after the
    inserts. `index_sql` replaces that index statement (a list; empty for no
    index); `setup_sql` runs before the index, `after_index_sql` after it.
    """
    values = corpus_values(engine, corpus["name"], session.schema)
    table = values["table"]
    connection = session.connection
    columns = corpus.get("columns") or "id int PRIMARY KEY, body text"
    connection.execute(f"CREATE TABLE {table} ({substitute(columns, values)})")
    rows = []
    for row in corpus.get("rows") or []:
        if isinstance(row, dict):
            rows.append((int(row["id"]), row.get("body")))
        else:
            rows.append((int(row[0]), row[1]))
    rows += [(PAD_ROWS_START + n, f"pad{n}") for n in range(1, int(corpus.get("pad", 0)) + 1)]
    with connection.cursor() as cursor:
        cursor.executemany(f"INSERT INTO {table} (id, body) VALUES (%s, %s)", rows)
    for statement in corpus.get("setup_sql") or []:
        connection.execute(substitute(statement, values))
    if "index_sql" in corpus:
        statements = corpus["index_sql"] or []
    else:
        options = corpus.get("index_options") or {}
        with_clause = ""
        if options:
            with_clause = " WITH (" + ", ".join(f"{key} = {value}" for key, value in options.items()) + ")"
        statements = [f"CREATE INDEX {{index}} ON {{table}} USING {{engine}}(body){with_clause}"]
    for statement in statements:
        connection.execute(substitute(statement, values))
    for statement in corpus.get("after_index_sql") or []:
        connection.execute(substitute(statement, values))
    connection.execute(f"ANALYZE {table}")


# ---------------------------------------------------------------- captures


def capture_sql(case, capture, values):
    kind = capture["kind"]
    if "sql" in capture:
        return capture["sql"]
    if kind == "ids":
        return "SELECT id FROM {table} WHERE body ==> {query} ORDER BY id"
    if kind == "count":
        return "SELECT count(*) FROM {table} WHERE body ==> {query}"
    if kind == "ranked":
        return ("SELECT id FROM (SELECT id, {score} AS s FROM {table} WHERE body ==> {query} "
                "ORDER BY {score} DESC LIMIT {k}) top ORDER BY s DESC, id")
    if kind == "scores":
        return ("SELECT id, encode(float4send(({score})::real), 'hex') FROM {table} "
                "WHERE body ==> {query} ORDER BY id")
    if kind == "highlight":
        return case.get("sql") or "SELECT id, {engine}.highlight(body) FROM {table} WHERE body ==> {query} ORDER BY id"
    if kind in ("value", "error"):
        return case.get("sql") or "SELECT id FROM {table} WHERE body ==> {query} ORDER BY id"
    raise AssertionError(kind)


def shape(kind, rows):
    if kind == "ids" or kind == "ranked":
        return [row[0] for row in rows]
    if kind == "count":
        return rows[0][0]
    if kind == "scores":
        return {str(row[0]): row[1] for row in rows}
    if kind == "highlight":
        return [jsonable(list(row)) for row in rows]
    if len(rows) == 1 and len(rows[0]) == 1:
        return jsonable(rows[0][0])
    return [jsonable(list(row)) for row in rows]


def query_value(owner, values):
    """The SQL for {query}: a literal of 'query', or the expression 'query_sql'."""
    if "query" in owner:
        return literal(owner["query"])
    if "query_sql" in owner:
        return "(" + substitute(owner["query_sql"], values) + ")"
    return None


def error_of(error):
    return {"sqlstate": error.sqlstate, "message": error.diag.message_primary or str(error)}


def run_script(session, capture, values, settings):
    """Ordered steps over named sessions (fresh autocommit connections, closed at
    the end, which rolls back anything left open). Returns one entry per step
    with capture: true: its rows (shaped like value) or its ERROR."""
    connections, results = {}, []
    try:
        for step in capture["steps"]:
            name = str(step.get("session", "a"))
            if name not in connections:
                connection = session.open()
                connection.autocommit = True
                connection.execute(f"SET search_path = {session.schema}, public")
                connection.execute("SELECT set_config('statement_timeout', %s, false)",
                                   (settings.get("statement_timeout", DEFAULT_TIMEOUT),))
                for key, value in settings.items():
                    if key != "statement_timeout":
                        connection.execute("SELECT set_config(%s, %s, false)", (key, str(value)))
                connections[name] = connection
            sql = substitute(step["sql"], values)
            try:
                with connections[name].cursor() as cursor:
                    cursor.execute(sql)
                    rows = cursor.fetchall() if cursor.description else []
                outcome = shape("value", rows) if rows else None
            except psycopg.OperationalError as error:
                if connections[name].closed or connections[name].broken:
                    raise ConnectionLost(str(error)) from error
                outcome = {"error": error_of(error)}
            except psycopg.Error as error:
                outcome = {"error": error_of(error)}
            if step.get("capture", True):
                results.append(outcome)
    finally:
        for connection in connections.values():
            try:
                connection.close()
            except Exception:
                pass
    return results


def run_capture(session, case, capture, values, settings):
    """Returns the capture's JSON result; an ERROR becomes {"error": {...}}."""
    local = dict(values)
    if capture.get("corpus"):
        local.update(corpus_values(values["engine"], capture["corpus"], values.get("schema")))
    own_query = query_value(capture, local)
    if own_query is not None:
        local["query"] = own_query
    local["score"] = capture.get("score", case.get("score", DEFAULT_SCORE))
    local["k"] = str(int(capture.get("k", case.get("k", 10))))
    settings = {**settings, **{substitute(name, values): value
                               for name, value in (capture.get("settings") or {}).items()}}
    if capture["kind"] == "script":
        return run_script(session, capture, local, settings)
    sql = substitute(capture_sql(case, capture, local), local)
    try:
        rows = session.run(sql, settings)
    except ConnectionLost:
        raise
    except psycopg.Error as error:
        failure = error_of(error)
        return failure if capture["kind"] == "error" else {"error": failure}
    if capture["kind"] == "error":
        return {"no_error": shape("value", rows)}
    return shape(capture["kind"], rows)


def run_case(session, engine, case):
    """Returns the case record: {"captures": {...}} or {"server_crashed": true, ...}."""
    values = corpus_values(engine, case.get("corpus"), session.schema)
    query = query_value(case, values)
    if query is not None:
        values["query"] = query
    base = {substitute(name, values): value for name, value in (case.get("settings") or {}).items()}
    variants = case.get("variants") or {}
    variant_settings = [
        {**base, **{substitute(name, values): value for name, value in variant.items()}}
        for variant in variants.get("settings") or []
    ]
    varied = set(variants.get("captures") or [])
    captures = {}
    for capture in case["capture"]:
        name = capture["as"]
        if name in varied and variant_settings:
            answers = [run_capture(session, case, capture, values, s) for s in variant_settings]
            if all(answer == answers[0] for answer in answers):
                captures[name] = answers[0]
            else:
                captures[name] = {"variants_disagree": [
                    {"settings": s, "answer": a} for s, a in zip(variant_settings, answers)]}
        else:
            captures[name] = run_capture(session, case, capture, values, base)
    return {"captures": captures}


def execute_case(session, engine, case, risky):
    """Run a case with crash detection; reconnects when the server reinitialized."""
    if risky and not session.sentinel_alive():
        session.reconnect_after_crash()
    try:
        record = run_case(session, engine, case)
        lost = None
    except ConnectionLost as error:
        record, lost = None, str(error)
    if lost is not None:
        crashed = session.server_reinitialized()
    else:
        crashed = risky and not session.sentinel_alive()
    if crashed:
        session.reconnect_after_crash()
        detail = "the sentinel connection died too (the server reinitialized)"
        return {"server_crashed": True, "detail": detail}
    if lost is not None:
        # The backend died but the server did not reinitialize (terminated from outside).
        session.reconnect_after_crash(delay=0)
        return {"captures": {"connection_lost": lost}}
    return record


# ---------------------------------------------------------------- comparison


def compare_capture(case, capture, want, got):
    """Returns (status, detail) for one capture: PASS, DIFF (compatible) or FAIL."""
    if capture["kind"] == "error":
        if "no_error" in got:
            if "no_error" in want and want == got:
                return "PASS", ""
            return "FAIL", f"expected ERROR {want.get('sqlstate')}, got an answer {got['no_error']!r:.80}"
        if "no_error" in want:
            return "FAIL", f"expected answer {want['no_error']!r:.80}, got ERROR {got['sqlstate']}"
        return compare_errors(capture.get("message_prefix"), want, got)
    if isinstance(want, dict) and "error" in want and isinstance(got, dict) and "error" in got:
        return compare_errors(capture.get("message_prefix"), want["error"], got["error"])
    if want == got:
        return "PASS", ""
    if without_messages(want) == without_messages(got):
        return "DIFF", f"{capture['as']}: same answers and SQLSTATEs, different ERROR messages"
    return "FAIL", f"{capture['as']}: got {brief(got)}, recorded {brief(want)}"


def without_messages(value):
    """The value with every ERROR's message removed, keeping its SQLSTATE."""
    if isinstance(value, dict):
        if "error" in value and isinstance(value["error"], dict):
            return {"error": {"sqlstate": value["error"].get("sqlstate")}}
        return {key: without_messages(item) for key, item in value.items()}
    if isinstance(value, list):
        return [without_messages(item) for item in value]
    return value


def compare_errors(prefix, want, got):
    if got["sqlstate"] != want["sqlstate"]:
        return "FAIL", f"ERROR {got['sqlstate']} {got['message']!r:.60}, recorded {want['sqlstate']}"
    if prefix is not None and not got["message"].startswith(prefix):
        return "FAIL", f"message {got['message']!r:.60} lacks prefix {prefix!r}"
    if got["message"] != want["message"]:
        return "DIFF", f"same SQLSTATE {got['sqlstate']}, message {got['message']!r:.60} vs {want['message']!r:.60}"
    return "PASS", ""


def brief(value):
    text = json.dumps(value, sort_keys=True)
    return text if len(text) <= 100 else text[:97] + "..."


def only_errors(record):
    captures = record.get("captures") or {}
    return captures and all(isinstance(v, dict) and ("error" in v or "sqlstate" in v)
                            for v in captures.values())


def compare_case(case, want, got):
    if "corpus_error" in got or "corpus_error" in want:
        if "corpus_error" not in got:
            return "FAIL", f"recorded engine could not build the corpus: {brief(want['corpus_error'])}"
        if "corpus_error" not in want:
            return "FAIL", f"could not build the corpus: {brief(got['corpus_error'])}"
        return compare_errors(None, want["corpus_error"], got["corpus_error"])
    if got.get("server_crashed"):
        return "FAIL", "the server crashed" + (" (as the recorded engine did)" if want.get("server_crashed") else "")
    if "connection_lost" in (got.get("captures") or {}):
        return "FAIL", f"connection lost: {got['captures']['connection_lost']}"
    if want.get("server_crashed"):
        # The recorded engine crashed: an ERROR or a correct answer passes; a crash fails.
        if only_errors(got):
            states = sorted({(v.get("error") or v)["sqlstate"] for v in got["captures"].values()})
            return "PASS", f"recorded engine crashed; answered ERROR {', '.join(states)}"
        expect = case.get("expect_if_answered")
        if expect is None:
            return "PASS", "recorded engine crashed; answered (no reference answer to check)"
        wrong = [name for name, value in expect.items()
                 if name in got["captures"] and got["captures"][name] != value]
        if wrong:
            return "FAIL", "recorded engine crashed; answered wrongly: " + ", ".join(
                f"{name} {brief(got['captures'][name])}, correct {brief(expect[name])}" for name in wrong)
        return "PASS", "recorded engine crashed; answered correctly"
    statuses, details, compared = [], [], 0
    for capture in case["capture"]:
        name = capture["as"]
        if name not in (want.get("captures") or {}):
            continue
        compared += 1
        status, detail = compare_capture(case, capture, want["captures"][name], got["captures"][name])
        statuses.append(status)
        if detail:
            details.append(detail)
    if not compared:
        return "SKIP", "no recorded capture for this case"
    for status in ("FAIL", "DIFF"):
        if status in statuses:
            return status, "; ".join(details)
    return "PASS", ""


# ---------------------------------------------------------------- expected files


def read_expected(directory):
    source, records = None, {}
    files = sorted(Path(directory).glob("*.json"))
    if not files:
        sys.exit(f"no recorded answers (*.json) in {directory}")
    for path in files:
        document = json.loads(path.read_text())
        header = document.get("source") or {}
        if source is None:
            source = header
        elif (header.get("engine"), header.get("extension_version")) != (
                source.get("engine"), source.get("extension_version")):
            sys.exit(f"{path}: recorded from {header.get('engine')} {header.get('extension_version')}, "
                     f"but other files in {directory} from {source.get('engine')} {source.get('extension_version')}")
        for record in document.get("cases") or []:
            records[record["id"]] = dict(record, recorded_in=path.name, source=header)
    return source, records


def write_expected(directory, header, results, cases, merge):
    directory.mkdir(parents=True, exist_ok=True)
    by_area = {}
    for case in cases:
        if case["id"] in results:
            by_area.setdefault(case["area"], []).append(dict(id=case["id"], **results[case["id"]]))
    written = []
    for area, records in by_area.items():
        path = directory / f"{area}.json"
        if merge and path.exists():
            kept = {r["id"]: r for r in json.loads(path.read_text()).get("cases") or []}
            kept.update({r["id"]: r for r in records})
            order = [c["id"] for c in cases if c["id"] in kept]
            records = [kept[i] for i in order] + [r for i, r in kept.items() if i not in order]
        path.write_text(json.dumps({"source": header, "cases": records}, indent=1) + "\n")
        written.append(path)
    return written


# ---------------------------------------------------------------- main


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--engine", required=True, choices=["tin", "stannum"])
    parser.add_argument("--dsn-env", default="CONFORMANCE_DSN",
                        help="environment variable holding the connection string (default CONFORMANCE_DSN)")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--record", metavar="OUTDIR", help="write answers to OUTDIR/<engine>-<version>/<area>.json")
    mode.add_argument("--check", metavar="EXPECTED_DIR", help="compare answers with a recorded directory")
    parser.add_argument("--area", action="append", default=[], help="only this area (repeatable)")
    parser.add_argument("--case", action="append", default=[], help="only this case id or glob (repeatable)")
    parser.add_argument("--skip-crash", action="store_true",
                        help="skip cases tagged as crashing the engine under test")
    parser.add_argument("--host-note", default="", help="free-text host description for the source header")
    parser.add_argument("--cases-dir", default=str(SUITE / "cases"), help=argparse.SUPPRESS)
    args = parser.parse_args()

    dsn = os.environ.get(args.dsn_env)
    if not dsn:
        sys.exit(f"set the connection string in the environment variable {args.dsn_env}")
    try:
        corpora, all_cases = load_cases(Path(args.cases_dir))
    except CaseError as error:
        sys.exit(f"invalid case file: {error}")
    cases = selected(all_cases, set(args.area), args.case)
    if not cases:
        sys.exit("no case matches the --area/--case filters")

    expected_source, expected = (None, {})
    if args.check:
        expected_source, expected = read_expected(args.check)
        print(f"Comparing against {expected_source.get('engine')} {expected_source.get('extension_version')} "
              f"recorded in {args.check}")
        for key in ("postgres", "server_version", "host", "measured", "date", "measured_by", "imported_from"):
            if expected_source.get(key):
                print(f"  {key}: {expected_source[key]}")

    schema = f"conformance_{secrets.token_hex(4)}"
    session = Session(dsn, None)
    version, server = engine_facts(session, args.engine)
    print(f"Engine under test: {args.engine} {version}; {server}")
    session.connection.execute(f"CREATE SCHEMA {schema}")
    session.schema = schema
    session.connection.execute(f"SET search_path = {schema}, public")
    print(f"Scratch schema: {schema}")

    results, rows = {}, []
    width = max(len(case["id"]) for case in cases)

    def emit(status, case_id, detail=""):
        rows.append((status, case_id, detail))
        print(f"{status:<5} {case_id:<{width}}  {detail}".rstrip(), flush=True)

    print()
    try:
        corpus_errors = {}
        for name in dict.fromkeys(name for case in cases for name in case_corpora(case)):
            try:
                build_corpus(session, args.engine, corpora[name])
            except psycopg.Error as error:
                corpus_errors[name] = dict(error_of(error), corpus=name)
        for case in cases:
            tagged = crash_tagged(case, args.engine, version)
            if tagged and args.skip_crash:
                emit("SKIP", case["id"], f"tagged crashes: {args.engine}-{version}")
                continue
            failed = [corpus_errors[name] for name in case_corpora(case) if name in corpus_errors]
            if failed:
                record = {"corpus_error": failed[0]}
            else:
                record = execute_case(session, args.engine, case, risky=bool(case.get("risky") or tagged))
            results[case["id"]] = record
            if args.check:
                want = expected.get(case["id"])
                if want is None:
                    emit("SKIP", case["id"], "no expectation recorded")
                else:
                    status, detail = compare_case(case, want, record)
                    emit(status, case["id"], detail)
            else:
                if "corpus_error" in record:
                    error = record["corpus_error"]
                    emit("ERROR", case["id"], f"corpus {error['corpus']}: {error['sqlstate']} {error['message']}")
                elif record.get("server_crashed"):
                    emit("CRASH", case["id"], record.get("detail", ""))
                elif "connection_lost" in record["captures"]:
                    emit("LOST", case["id"], record["captures"]["connection_lost"])
                else:
                    errors = [n for n, v in record["captures"].items()
                              if isinstance(v, dict) and ("error" in v or "sqlstate" in v or "variants_disagree" in v)]
                    emit("OK", case["id"], ("ERROR or disagreement in: " + ", ".join(errors)) if errors else "")
    finally:
        try:
            if not session.sentinel_alive() or session.connection.closed:
                session.reconnect_after_crash()
            session.connection.execute(f"DROP SCHEMA IF EXISTS {schema} CASCADE")
            print(f"Dropped scratch schema {schema}")
        except Exception as error:  # report, but do not mask the run's own failure
            print(f"WARNING: could not drop scratch schema {schema}: {error}", file=sys.stderr)
        session.close()

    counts = {}
    for status, _, _ in rows:
        counts[status] = counts.get(status, 0) + 1
    print()
    print("Summary: " + ", ".join(f"{counts[s]} {s}" for s in sorted(counts)) + f" of {len(rows)} cases")
    if args.check:
        unknown = sorted(set(expected) - {case["id"] for case in all_cases})
        if unknown:
            print(f"Recorded answers without a case in the suite: {', '.join(unknown)}")
        print(f"Compared {args.engine} {version} against {expected_source.get('engine')} "
              f"{expected_source.get('extension_version')} ({args.check})")
        return 1 if counts.get("FAIL") else 0
    if args.record:
        header = {
            "engine": args.engine,
            "extension_version": version,
            "server_version": server,
            "host": args.host_note,
            "date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "suite_commit": suite_commit(),
            "runner_version": RUNNER_VERSION,
        }
        directory = Path(args.record) / f"{args.engine}-{version}"
        written = write_expected(directory, header, results, cases, merge=bool(args.case or args.area))
        for path in written:
            print(f"Recorded {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
