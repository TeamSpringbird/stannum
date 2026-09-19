#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Local setup for the pinned PlanetScale benchmarker; upstream owns measurement."""
import argparse
import collections
import csv
import fcntl
import gzip
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import statistics
import time
import threading
import uuid

import dataset
import run as bench
import resources

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / 'benchmarks/tin'
REVISION = 'f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86'
REPOSITORY = 'https://github.com/planetscale/paradedb-benchmarker.git'
DEFAULT_DRIVER = ROOT / 'benchmarks/results/tin-driver'
LOADED_SOURCES = {str(p): dataset.sha256(p) for p in
                  [Path(__file__), Path(bench.__file__), Path(dataset.__file__), Path(resources.__file__), *ASSETS.iterdir()]
                  if p.is_file()}


def verify_sources(expected):
    for name, digest in expected.items():
        if not Path(name).is_file() or dataset.sha256(name) != digest:
            raise ValueError('benchmark source changed after process startup: ' + name)


def command(args, **kwargs):
    return subprocess.run([str(a) for a in args], check=True, **kwargs)


def output(args, **kwargs):
    return subprocess.check_output([str(a) for a in args], text=True, **kwargs).strip()


def identities(driver):
    files = output(['git', '-C', driver, 'ls-files', '--cached', '--others', '--exclude-standard', '-z']).rstrip('\0').split('\0')
    files = [p for p in files if p not in ('stannum-adapter.json', 'pg-driver-test')]
    return {name: dataset.sha256(driver / name) for name in files}


def prepare(args):
    driver = args.driver.resolve()
    if driver.exists():
        raise ValueError('driver directory already exists; use a fresh path')
    env = dict(os.environ, GIT_LFS_SKIP_SMUDGE='1')
    command(['git', 'clone', '--no-checkout', REPOSITORY, driver], env=env)
    command(['git', '-C', driver, 'checkout', '--detach', REVISION], env=env)
    command(['git', '-C', driver, 'apply', '--check', ASSETS / 'upstream.patch'])
    command(['git', '-C', driver, 'apply', ASSETS / 'upstream.patch'])
    for source, target in [('register.go', 'backends/stannum/register.go'),
                           ('main.go', 'cmd/stannum-k6/main.go'),
                           ('live_rows_test.go', 'backends/shared/postgres/stannum_live_rows_test.go')]:
        path = driver / target
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ASSETS / source, path)
    bench.save(driver / 'stannum-adapter.json', {
        'repository': REPOSITORY, 'revision': REVISION,
        'update_target_protocol': 'random-page-live-wrap-v1',
        'patch_sha256': dataset.sha256(ASSETS / 'upstream.patch'),
        'files': identities(driver),
    })
    print(driver)


def verify_driver(driver):
    manifest = json.loads((driver / 'stannum-adapter.json').read_text())
    if output(['git', '-C', driver, 'rev-parse', 'HEAD']) != REVISION:
        raise ValueError('unexpected upstream revision')
    if manifest['files'] != identities(driver):
        raise ValueError('prepared adapter changed; prepare/build a fresh driver')
    if manifest['patch_sha256'] != dataset.sha256(ASSETS / 'upstream.patch'):
        raise ValueError('repository adapter changed; prepare/build a fresh driver')
    for source, target in [('register.go', 'backends/stannum/register.go'), ('main.go', 'cmd/stannum-k6/main.go'),
                           ('live_rows_test.go', 'backends/shared/postgres/stannum_live_rows_test.go')]:
        if dataset.sha256(ASSETS / source) != manifest['files'][target]:
            raise ValueError('repository adapter changed; prepare/build a fresh driver')
    return manifest


def build(args):
    driver = args.driver.resolve()
    manifest = verify_driver(driver)
    target_os = {'Darwin': 'darwin', 'Linux': 'linux'}[platform.system()]
    target_arch = {'arm64': 'arm64', 'aarch64': 'arm64', 'x86_64': 'amd64'}[platform.machine()]
    # Use the pinned module's k6 version, rather than xk6's changing latest default.
    command(['docker', 'pull', 'golang:1.26.1-bookworm'])
    image = output(['docker', 'image', 'inspect', 'golang:1.26.1-bookworm', '--format', '{{.Id}}'])
    builder = ['docker', 'run', '--rm', '--cpus', '4', '--memory', '6g',
             '-v', f'{driver}:/src', '-w', '/src',
             '-v', 'stannum-benchmark-go-mod:/go/pkg/mod',
             '-v', 'stannum-benchmark-go-cache:/root/.cache/go-build',
             '-e', f'GOOS={target_os}', '-e', f'GOARCH={target_arch}',
             '-e', 'CGO_ENABLED=0', '-e', 'GOMAXPROCS=4', image]
    command(builder + ['go', 'build', '-mod=readonly', '-p', '4', '-o', 'k6', './cmd/stannum-k6'])
    command(builder + ['go', 'test', '-mod=readonly', '-p', '4', '-c', '-o', 'pg-driver-test', './backends/shared/postgres'])
    if manifest['files'] != identities(driver):
        raise ValueError('driver source changed while building')
    manifest.update(binary_sha256=dataset.sha256(driver / 'k6'),
                    test_binary_sha256=dataset.sha256(driver / 'pg-driver-test'),
                    build_image=image, target_os=target_os, target_arch=target_arch)
    bench.save(driver / 'stannum-adapter.json', manifest)


def build_image(args):
    args.output.mkdir(parents=True, exist_ok=False)
    source = bench.provenance(args.output)
    bench.save(args.output / 'source.json', source)
    recipe = bench.digest((ROOT / 'benchmarks/Dockerfile').read_bytes() +
                          (ROOT / 'benchmarks/Dockerfile.dockerignore').read_bytes())
    with (args.output / 'build.log').open('w') as log:
        command(['docker', 'build', '-f', 'benchmarks/Dockerfile',
                 '--build-arg', 'STANNUM_SOURCE_SHA256=' + source['source_sha256'],
                 '--build-arg', 'STANNUM_COMMIT=' + source['commit'],
                 '--build-arg', 'RECIPE_SHA256=' + recipe, '-t', args.image, '.'],
                cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)
    bench.save(args.output / 'image.json', json.loads(output(['docker', 'image', 'inspect', args.image])))


def literal(value):
    return "'" + value.replace("'", "''") + "'"


def trace_queries(driver):
    records = json.loads((driver / 'datasets/wikipedia/queries.json').read_text())['queries']
    if any(not re.fullmatch('[a-z]+(?: [a-z]+)*', q['text']) for q in records):
        raise ValueError('lexical oracle requires normalized ASCII word queries')
    return [(f"{q['source_id']}:{style}", q['engines']['tin'][style],
             q['engines']['postgres'][style], q['text'])
            for q in records for style in ('conjunction', 'disjunction', 'phrase')]


def lexical_predicate(name, text):
    style = name.split(':')[1]
    if style == 'phrase':
        return f"body ~ {literal('(^| )' + text + '( |$)')}"
    terms = [f"body ~ {literal('(^| )' + term + '( |$)')}" for term in text.split()]
    return '(' + (' OR ' if style == 'disjunction' else ' AND ').join(terms) + ')'


def check_sql(queries, engine='stannum'):
    # Exact set equality, not just equal counts. The reference has no search index.
    statements = []
    for name, tin, postgres, text in queries:
        predicate = (f'body ==> {literal(tin)}' if engine == 'stannum' else
                     f"body_tsv @@ to_tsquery('simple', {literal(postgres)})")
        indexed = f'SELECT id FROM documents WHERE {predicate}'
        actual = 'SELECT id FROM indexed JOIN reference USING(id)'
        reference = (lexical_predicate(name, text) if engine == 'stannum' else
                     f"body_tsv @@ to_tsquery('simple', {literal(postgres)})")
        expected = f'SELECT id FROM reference WHERE {reference}'
        statements.append(f"WITH indexed AS MATERIALIZED ({indexed}) SELECT {literal(name)}, count(*), "
                          f"(SELECT count(*) FROM indexed) FROM (({actual} EXCEPT ALL {expected}) "
                          f"UNION ALL ({expected} EXCEPT ALL {actual})) difference;")
    return '\n'.join(statements)


def ranked_check_sql(queries, engine):
    statements = []
    for name, tin, postgres, text in queries:
        tsquery = f"to_tsquery('simple', {literal(postgres)})"
        predicate = f'body ==> {literal(tin)}' if engine == 'stannum' else f'body_tsv @@ {tsquery}'
        score = 'stannum.full_score(ctid)' if engine == 'stannum' else f'ts_rank_cd(body_tsv,{tsquery})'
        # Compare score multisets, allowing arbitrary document order within ties.
        # MATERIALIZED forces exhaustive scoring before the reference sort/limit.
        select = f'SELECT id, {score} AS score FROM documents WHERE {predicate}'
        statements.append(f"WITH all_scores AS MATERIALIZED ({select}), "
                          f"expected AS (SELECT score FROM all_scores ORDER BY score DESC LIMIT 10), "
                          f"actual AS MATERIALIZED ({select} ORDER BY score DESC LIMIT 10) "
                          f"SELECT {literal(name)}, count(*) FROM ("
                          '(SELECT score FROM actual EXCEPT ALL SELECT score FROM expected) UNION ALL '
                          '(SELECT score FROM expected EXCEPT ALL SELECT score FROM actual) UNION ALL '
                          'SELECT score FROM actual GROUP BY id,score HAVING count(*) > 1 UNION ALL '
                          "SELECT score FROM actual WHERE score IS NULL OR score::float8 IN ('NaN'::float8,'Infinity'::float8,'-Infinity'::float8)"
                          ') differences;')
    return '\n'.join(statements)


def prepared_plan_sql(query, engine, workload, mode):
    if mode not in ('force_custom_plan', 'force_generic_plan'):
        raise ValueError('unsupported diagnostic plan mode')
    _, tin, postgres, _ = query
    if engine == 'stannum':
        fields = 'count(*)' if workload == 'count' else 'id, body, stannum.full_score(ctid) AS score'
        predicate, argument = 'body ==> $1', tin
    else:
        fields = ('count(*)' if workload == 'count' else
                  "id, body, ts_rank_cd(body_tsv, to_tsquery('simple', $1)) AS score")
        predicate, argument = "body_tsv @@ to_tsquery('simple', $1)", postgres
    suffix = '' if workload == 'count' else ' ORDER BY score DESC LIMIT 10'
    # Match the driver's bind parameter and projection. Literal count plans do
    # not reveal planner-support failures inside parameterized score calls.
    # Each invocation runs in a separate psql session, outside measured traffic.
    return (f'SET plan_cache_mode={mode};\n'
            f'PREPARE stannum_bench_plan AS SELECT {fields} FROM documents WHERE {predicate}{suffix};\n'
            'EXPLAIN (ANALYZE, BUFFERS, SETTINGS, FORMAT JSON) '
            f'EXECUTE stannum_bench_plan({literal(argument)});\n'
            'DEALLOCATE stannum_bench_plan;\nRESET plan_cache_mode;')


def semantics_sql(queries):
    return '\n'.join(
        f"SELECT {literal(name)}, count(*) FROM reference WHERE "
        f"({lexical_predicate(name, text)}) IS DISTINCT FROM "
        f"(body_tsv @@ to_tsquery('simple',{literal(postgres)}));"
        for name, _, postgres, text in queries)


def validate_result(text, names):
    rows = [line.split('|') for line in text.splitlines() if line]
    if [row[0] for row in rows] != names or any(
            len(row) not in (2, 3) or row[1] != '0' or (len(row) == 3 and not row[2].isdigit()) for row in rows):
        raise ValueError('query membership mismatch or incomplete oracle output; see correctness.txt')


def report(root):
    manifest = json.loads((root / 'manifest.json').read_text())
    rows = []
    for job in manifest['jobs']:
        engine = job['engine']
        path = root / engine
        exports = list(path.glob('result_*.json'))
        if manifest['status'] != 'complete' or job['status'] != 'complete' or len(exports) != 1:
            rows.append(dict(engine=engine, status='incomplete'))
            continue
        exported = json.loads(exports[0].read_text())['runs'][engine]
        elapsed = (exported['endTime'] - exported['startTime']) / 1000
        resource_summary = resources.summarize(path / 'resources.jsonl',
                                               exported['startTime'] / 1000, exported['endTime'] / 1000)
        bench.save(path / 'resource-summary.json', resource_summary)
        samples, groups = [], collections.defaultdict(list)
        queries = collections.defaultdict(list)
        updates = collections.Counter()
        with gzip.open(path / 'samples.json.gz', 'rt') as records:
            for line in records:
                point = json.loads(line)
                if point['type'] != 'Point':
                    continue
                if point['metric'] in ('update_docs', 'update_errors'):
                    updates[point['metric']] += point['data']['value']
                if point['metric'] == 'update_duration':
                    updates['attempted'] += 1
                if point['metric'] != 'query_duration':
                    continue
                data = point['data']
                query = data['tags']['query_id']
                samples.append(data['value'])
                groups[query.split(':')[1]].append(data['value'])
                queries[query].append(data['value'])
        if not samples or elapsed <= 0:
            raise ValueError('completed run has no measured query samples')
        def distribution(values):
            return dict(completed=len(values), p50_ms=bench.percentile(values, .5),
                        p95_ms=bench.percentile(values, .95),
                        p99_ms=bench.percentile(values, .99) if len(values) >= 1000 else None)
        metrics = exported['queries'][engine]
        rows.append(dict(engine=engine, status='complete', seconds=elapsed,
                         qps=len(samples) / elapsed, **distribution(samples),
                         resources=resource_summary,
                         families={k: distribution(v) for k, v in groups.items()},
                         queries={k: distribution(v) for k, v in queries.items()},
                         measured_query_forms=len(queries),
                         index_bytes=job['sizes']['index'], total_bytes=job['sizes']['total'],
                         index_read_bytes=metrics.get('indexReadBytes'),
                         index_hit_bytes=metrics.get('indexHitBytes'),
                         updates_completed=updates['update_docs'], updates_attempted=updates['attempted'],
                         update_errors=updates['update_errors'],
                         cross_engine_membership_differences=job['cross_engine_membership_differences']))
    bench.save(root / 'comparison.json', rows)
    lines = ['# Published-trace local run', '',
             'Diagnostic measurements, not a capacity claim. Each engine uses its native query semantics.',
             'GIN ranks with ts_rank_cd; Stannum ranks with BM25. Phrase membership can also differ.',
             'Percentiles use nearest rank; p99 is omitted below 1,000 samples. No speedup ratio is inferred.', '',
             '| Engine | QPS | p95 ms | p99 ms | Measured query forms | Index MiB | Total relation MiB |',
             '| --- | ---: | ---: | ---: | ---: | ---: | ---: |']
    for row in rows:
        if row['status'] != 'complete':
            lines.append(f"| {row['engine']} | incomplete | | | | | |")
        else:
            p99 = f"{row['p99_ms']:.3f}" if row['p99_ms'] is not None else '—'
            lines.append(f"| {row['engine']} | {row['qps']:.1f} | {row['p95_ms']:.3f} | {p99} | "
                         f"{row['measured_query_forms']} | {row['index_bytes']/2**20:.2f} | {row['total_bytes']/2**20:.2f} |")
    lines += ['', 'See comparison.json for query-family and individual-query distributions,',
              'semantic differences on the validation sample, and completed updates.',
              'Index read/hit bytes are block accesses, not physical disk traffic.',
              'Resource summaries in comparison.json and resource-summary.json use samples wholly inside the measured window; boundary gaps are reported.',
              'The full pinned trace may not be traversed during short or slow runs.']
    differences = manifest.get('full_count_differences')
    if differences is None:
        lines += ['', 'Full-corpus count comparison was not recorded for this run.']
    else:
        style = manifest['config']['style']
        timed = sum(style == 'mixed' or name.split(':')[1] == style for name in differences)
        lines += ['', f'Full-corpus count disagreements: {len(differences)} checked forms, '
                  f'{timed} in the timed query mix. See manifest.json.']
    (root / 'report.md').write_text('\n'.join(lines) + '\n')


CGROUP_METRICS = ('cpu.stat', 'memory.current', 'memory.peak', 'memory.events',
                  'memory.stat', 'memory.pressure', 'io.stat', 'io.pressure')


def resource_snapshot(name):
    # One short-lived exec per sample, rather than one per counter. These are
    # Linux VM block-device bytes; they are not macOS host physical disk bytes.
    script = 'set -e\n' + '\n'.join(f"printf '\n@@{metric}\n'; if test -r /sys/fs/cgroup/{metric}; then cat /sys/fs/cgroup/{metric}; else printf 'unavailable\n'; fi"
                       for metric in CGROUP_METRICS)
    started = time.time()
    result = subprocess.run(['docker', 'exec', name, 'sh', '-c', script],
                            capture_output=True, text=True, timeout=10)
    if result.returncode:
        raise RuntimeError(result.stderr.strip())
    counters = {}
    for section in result.stdout.split('\n@@')[1:]:
        metric, value = section.split('\n', 1)
        counters[metric] = None if value.strip() == 'unavailable' else value.strip()
    if set(counters) != set(CGROUP_METRICS) or any(v == '' for v in counters.values()):
        raise ValueError('incomplete cgroup snapshot')
    return dict(started=started, finished=time.time(), counters=counters)


class ResourceSampler:
    """Approximate phase samples; never pretend the last sample is an end counter."""
    def __init__(self, name, path):
        self.name, self.path = name, path
        self.phase = 'setup'
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.collect, daemon=True)

    def collect(self):
        with self.path.open('w') as log:
            while not self.stop.is_set():
                phase = self.phase
                try:
                    record = resource_snapshot(self.name)
                except (RuntimeError, ValueError, subprocess.SubprocessError) as error:
                    record = dict(started=time.time(), error=str(error))
                record['phase'] = phase
                log.write(json.dumps(record) + '\n')
                log.flush()
                self.stop.wait(2)

    def close(self):
        self.stop.set()
        self.thread.join()


def run(args):
    verify_sources(LOADED_SOURCES)
    driver = args.driver.resolve()
    adapter = verify_driver(driver)
    if adapter.get('binary_sha256') != dataset.sha256(driver / 'k6'):
        raise ValueError('driver binary not built from recorded adapter')
    if adapter.get('test_binary_sha256') != dataset.sha256(driver / 'pg-driver-test'):
        raise ValueError('driver regression binary does not match recorded build')
    corpus = dataset.verify(args.dataset)
    if len(set(args.engines)) != len(args.engines) or not 0 <= args.updates <= 1000000000:
        raise ValueError('engines must be distinct and updates must be between 0 and 1000000000')
    if args.rows > corpus['rows']:
        raise ValueError('requested rows exceed dataset')
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    source_path = getattr(args, 'source_manifest', None)
    source = recorded_source(source_path) if source_path else bench.provenance(root)
    image = json.loads(output(['docker', 'image', 'inspect', args.image]))[0]
    if image['Architecture'] != {'arm64': 'arm64', 'aarch64': 'arm64', 'x86_64': 'amd64'}[platform.machine()]:
        raise ValueError('image must run natively; emulated timings are not supported')
    if image['Config'].get('Labels', {}).get('benchmark.stannum_source_sha256') != source['source_sha256']:
        raise ValueError('image does not match extension source provenance')
    recipe = bench.digest((ROOT / 'benchmarks/Dockerfile').read_bytes() +
                          (ROOT / 'benchmarks/Dockerfile.dockerignore').read_bytes())
    if image['Config'].get('Labels', {}).get('benchmark.recipe_sha256') != recipe:
        raise ValueError('image does not match current Docker build recipe')
    manifest = dict(status='running', adapter=adapter, image=image['Id'], source=source,
                    corpus=corpus, config={k: str(v) if isinstance(v, Path) else v
                                          for k, v in vars(args).items() if k != 'func'},
                    runner_sha256=LOADED_SOURCES[str(Path(__file__))], harness_sources=LOADED_SOURCES, jobs=[])
    bench.save(root / 'manifest.json', manifest)
    protocol = root / 'protocol'
    protocol.mkdir()
    for filename in ('tin.py', 'run.py', 'dataset.py'):
        shutil.copy2(ROOT / 'benchmarks' / filename, protocol / filename)
    shutil.copytree(ASSETS, protocol / 'tin')
    manifest['host'] = dict(system=platform.platform(), machine=platform.machine(),
                            docker=json.loads(output(['docker', 'info', '--format', '{{json .}}'])))
    env = dict(os.environ, PGHOST='127.0.0.1', PGPORT=str(args.port), PGUSER='postgres',
               PGPASSWORD='postgres', PGDATABASE='benchmark',
               PGOPTIONS='-c statement_timeout=120000 -c jit=off')
    for key in ('PGSERVICE', 'PGSERVICEFILE'):
        env.pop(key, None)
    def sql(text, setup=False):
        sql_env = dict(env, PGOPTIONS=f'-c statement_timeout={args.setup_timeout_seconds * 1000} -c jit=off') if setup else env
        return output(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1', '-c', text], env=sql_env)
    try:
        for engine in args.engines:
            verify_sources(LOADED_SOURCES)
            if adapter != verify_driver(driver) or adapter['binary_sha256'] != dataset.sha256(driver / 'k6'):
                raise ValueError('driver changed between engines')
            name = 'stannum-trace-' + uuid.uuid4().hex[:12]
            volume = name + '-data'
            path = root / engine
            path.mkdir()
            job = dict(engine=engine, status='running', container=name, volume=volume)
            manifest['jobs'].append(job)
            bench.save(root / 'manifest.json', manifest)
            command(['docker', 'volume', 'create', volume], stdout=subprocess.DEVNULL)
            sampler = None
            try:
                command(['docker', 'run', '-d', '--name', name, '--cpus', str(args.cpus),
                         '--memory', args.memory, '--memory-swap', args.memory, '--shm-size', '1g',
                         '-p', f'127.0.0.1:{args.port}:5432',
                         '-v', f'{volume}:/var/lib/postgresql',
                         '-e', 'POSTGRES_PASSWORD=postgres', '-e', 'POSTGRES_DB=benchmark',
                         image['Id'], 'postgres', '-c', f'shared_buffers={args.shared_buffers}',
                         '-c', 'maintenance_work_mem=512MB', '-c', 'work_mem=16MB',
                         '-c', f'max_parallel_workers={args.cpus}', '-c', 'jit=off',
                         '-c', 'track_io_timing=on', '-c', f'plan_cache_mode={args.plan_cache_mode}'], stdout=subprocess.DEVNULL)
                deadline = time.monotonic() + 90
                while subprocess.run(['pg_isready'], env=env, capture_output=True).returncode:
                    if time.monotonic() > deadline:
                        raise TimeoutError('PostgreSQL startup')
                    time.sleep(.5)
                sampler = ResourceSampler(name, path / 'resources.jsonl')
                sampler.thread.start()
                sql('CREATE EXTENSION pg_prewarm; CREATE EXTENSION stannum;')
                with (path / 'driver-regressions.txt').open('w') as log:
                    command([driver / 'pg-driver-test', '-test.v'],
                            env=dict(env, STANNUM_BENCH_TEST_URL=f'postgres://postgres:postgres@127.0.0.1:{args.port}/benchmark',
                                     BENCHMARKER_POSTGRES_TEST_URL=f'postgres://postgres:postgres@127.0.0.1:{args.port}/benchmark'),
                            stdout=log, stderr=subprocess.STDOUT)
                sql((ASSETS / 'position-limit.sql').read_text())
                sql('CREATE TABLE documents(id bigint PRIMARY KEY, body text NOT NULL);')
                # COPY streams the immutable prefix; no modified shared dataset.
                with (path / 'input.csv').open('w') as target, (Path(args.dataset) / 'documents.csv').open() as source_csv:
                    reader, writer = csv.reader(source_csv), csv.writer(target)
                    for number, row in enumerate(reader):
                        if number == args.rows:
                            break
                        writer.writerow(row)
                job['input_sha256'] = dataset.sha256(path / 'input.csv')
                sampler.phase = 'import'
                started = time.monotonic()
                with (path / 'input.csv').open('rb') as data:
                    command(['psql', '-Xq', '-v', 'ON_ERROR_STOP=1', '-c',
                             'COPY documents FROM STDIN WITH (FORMAT csv)'], stdin=data, env=dict(env, PGOPTIONS=f'-c statement_timeout={args.setup_timeout_seconds * 1000} -c jit=off'))
                job['import_seconds'] = time.monotonic() - started
                started = time.monotonic()
                sampler.phase = 'vector-preparation'
                if engine == 'postgres':
                    sql("ALTER TABLE documents ADD COLUMN body_tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', body)) STORED;", setup=True)
                job['vector_preparation_seconds'] = time.monotonic() - started
                started = time.monotonic()
                index = 'documents_body_gin_idx' if engine == 'postgres' else 'documents_body_stannum_idx'
                expression = 'gin(body_tsv)' if engine == 'postgres' else 'stannum(body)'
                sampler.phase = 'index-build'
                bench.save(path / 'before-build-cgroup.json', resource_snapshot(name))
                sql(f'CREATE INDEX {index} ON documents USING {expression};', setup=True)
                bench.save(path / 'after-build-cgroup.json', resource_snapshot(name))
                job['index_build_seconds'] = time.monotonic() - started
                sampler.phase = 'validation'
                sql('VACUUM ANALYZE documents;', setup=True)
                job['sizes'] = json.loads(sql(f"SELECT json_build_object('index',pg_relation_size('{index}'),'table',pg_table_size('documents'),'total',pg_total_relation_size('documents'),'rows',(SELECT count(*) FROM documents));"))
                sql(f"CREATE TABLE reference AS SELECT id, body, to_tsvector('simple',body) AS body_tsv FROM documents ORDER BY id LIMIT {args.validation_rows};")
                queries = trace_queries(driver)
                oracle = 'SET enable_seqscan=off;\n' + check_sql(queries, engine)
                (path / 'correctness.sql').write_text(oracle)
                started = time.monotonic()
                text = sql(oracle)
                (path / 'correctness.txt').write_text(text + '\n')
                validate_result(text, [q[0] for q in queries])
                job['full_counts_before'] = {name: int(count) for name, _, count in
                                            (line.split('|') for line in text.splitlines())}
                job['correctness'] = dict(queries=len(queries), mismatches=0,
                                          sampled_rows=min(args.rows, args.validation_rows),
                                          seconds=time.monotonic() - started)
                semantics = sql(semantics_sql(queries))
                (path / 'semantics.txt').write_text(semantics + '\n')
                differences = {name: int(count) for name, count in
                               (line.split('|') for line in semantics.splitlines()) if int(count)}
                job['cross_engine_membership_differences'] = differences
                sql('DROP TABLE reference;')
                if args.workload == 'topk':
                    ranked_sql = ranked_check_sql(queries, engine)
                    (path / 'ranked-correctness.sql').write_text(ranked_sql)
                    ranked = sql(ranked_sql)
                    (path / 'ranked-correctness.txt').write_text(ranked + '\n')
                    validate_result(ranked, [q[0] for q in queries])
                    job['ranked_correctness'] = dict(queries=len(queries), mismatches=0,
                                                     reference='exhaustive same-engine score multiset; ties unordered')
                plan_queries = [q for q in queries if args.style == 'mixed' or q[0].split(':')[1] == args.style]
                for query in plan_queries[:6]:
                    for mode in ('force_custom_plan', 'force_generic_plan'):
                        statement = prepared_plan_sql(query, engine, args.workload, mode)
                        filename = 'plan-' + query[0].replace(':', '-') + '-' + mode
                        (path / (filename + '.sql')).write_text(statement + '\n')
                        (path / (filename + '.json')).write_text(sql(statement))
                sql('CHECKPOINT;')
                job['settings'] = json.loads(sql("SELECT json_object_agg(name,setting) FROM pg_settings;"))
                job['extensions'] = json.loads(sql('SELECT json_object_agg(extname,extversion) FROM pg_extension;'))
                bench.save(path / 'before-cgroup.json', bench.container_counters(name))
                # Upstream stops this owned container at phase end. No Makefile
                # or project-wide cleanup is invoked, and cooldown stays zero.
                run_env = dict(os.environ, BACKENDS=engine, WORKLOAD=args.workload,
                               QUERY_STYLE=args.style, QUERIES=str(driver / 'datasets/wikipedia/queries.json'),
                               CONFIG_DIR=str(driver / 'datasets/wikipedia'),
                               STANNUM_PORT=str(args.port), POSTGRES_PORT=str(args.port),
                               STANNUM_CONTAINER=name, POSTGRES_CONTAINER=name,
                               VUS=str(args.clients), DURATION=f'{args.seconds}s',
                               PREWARM=f'{args.warmup}s', UPDATES_PER_SECOND=str(args.updates),
                               SEED=str(args.seed), COOLDOWN='0s', TOP_K='10',
                               DASHBOARD_EXPORT_DIR=str(path), DASHBOARD_EXPORT_PREFIX='result')
                sampler.phase = 'driver-warmup-and-measurement'
                with (path / 'driver.log').open('w') as log:
                    command([driver / 'k6', 'run', '--out', 'dashboard=json',
                             '--out', 'json=' + str(path / 'samples.json.gz'),
                             '--summary-export', path / 'summary.json',
                             driver / 'benchmarks/search.js'], env=run_env, stdout=log, stderr=subprocess.STDOUT)
                (path / 'post-traffic-container.json').write_text(output(['docker', 'inspect', name]))
                if args.updates:
                    # Upstream owns the measured phase and stops the container.
                    # Restart only for an untimed post-update correctness check.
                    command(['docker', 'start', name], stdout=subprocess.DEVNULL)
                    deadline = time.monotonic() + 90
                    while subprocess.run(['pg_isready'], env=env, capture_output=True).returncode:
                        if time.monotonic() > deadline:
                            raise TimeoutError('PostgreSQL restart for update validation')
                        time.sleep(.5)
                    if int(sql('SELECT count(*) FROM documents;')) != args.rows:
                        raise ValueError('update-only phase changed row count')
                    sql(f"CREATE TABLE reference AS SELECT id, body, to_tsvector('simple',body) AS body_tsv FROM documents ORDER BY id LIMIT {args.validation_rows};")
                    after = sql(oracle)
                    (path / 'correctness-after.txt').write_text(after + '\n')
                    validate_result(after, [q[0] for q in queries])
                    counts_after = {name: int(count) for name, _, count in
                                    (line.split('|') for line in after.splitlines())}
                    if counts_after != job['full_counts_before']:
                        raise ValueError('whitespace-only updates changed full-corpus query counts')
                    job['post_update_correctness'] = dict(queries=len(queries), mismatches=0)
                job['status'] = 'complete'
            except BaseException as error:
                job.update(status='failed', error=str(error))
                raise
            finally:
                if sampler is not None:
                    sampler.close()
                (path / 'server.log').write_text(subprocess.run(['docker', 'logs', name], capture_output=True, text=True).stderr)
                state = subprocess.run(['docker', 'inspect', name], capture_output=True, text=True)
                (path / 'container.json').write_text(state.stdout)
                command(['docker', 'rm', '-f', name], stdout=subprocess.DEVNULL)
                command(['docker', 'volume', 'rm', volume], stdout=subprocess.DEVNULL)
                bench.save(root / 'manifest.json', manifest)
        full_counts = {job['engine']: job['full_counts_before'] for job in manifest['jobs']}
        if 'stannum' in full_counts and 'postgres' in full_counts:
            manifest['full_count_differences'] = {
                name: {engine: counts[name] for engine, counts in full_counts.items()}
                for name in full_counts['stannum']
                if full_counts['stannum'][name] != full_counts['postgres'][name]}
        verify_sources(LOADED_SOURCES)
        if adapter != verify_driver(driver) or adapter['binary_sha256'] != dataset.sha256(driver / 'k6'):
            raise ValueError('benchmark source changed during campaign')
        manifest['status'] = 'complete'
    except BaseException as error:
        manifest.update(status='failed', error=str(error))
        raise
    finally:
        bench.save(root / 'manifest.json', manifest)
        report(root)


def describe(values):
    if not values or any(not math.isfinite(v) or v <= 0 for v in values):
        raise ValueError('comparison requires finite, positive measurements')
    mean = statistics.mean(values)
    return dict(n=len(values), median=statistics.median(values), mean=mean,
                min=min(values), max=max(values),
                stdev=statistics.stdev(values) if len(values) > 1 else None,
                cv=statistics.stdev(values) / mean if len(values) > 1 else None)


def paired_jobs(repetitions):
    return [dict(pair=number, variant=variant,
                 directory=f'r{number:02d}-{variant}', status='pending')
            for number in range(1, repetitions + 1)
            for variant in (('baseline', 'candidate') if number % 2 else ('candidate', 'baseline'))]


def recorded_source(path):
    source = json.loads(path.read_text())
    files = source.get('source_files')
    if not files or source.get('source_sha256') != bench.digest(bench.canonical(files)):
        raise ValueError('recorded source fingerprint is missing or inconsistent: ' + str(path))
    if not all(isinstance(name, str) and isinstance(value, str) and
               re.fullmatch('[0-9a-f]{64}', value) for name, value in files.items()):
        raise ValueError('invalid recorded source file fingerprints: ' + str(path))
    return source


COMPARISON_SETTINGS = ('rows', 'validation_rows', 'workload', 'style', 'clients',
                       'seconds', 'warmup', 'updates', 'seed', 'cpus', 'memory',
                       'shared_buffers', 'engines', 'plan_cache_mode')


def comparison_contract(manifest):
    if manifest['status'] != 'complete' or len(manifest['jobs']) != 1:
        raise ValueError('trial did not complete exactly one engine')
    job = manifest['jobs'][0]
    if job['status'] != 'complete' or job['engine'] != 'stannum':
        raise ValueError('trial is not a completed Stannum measurement')
    if job['correctness']['mismatches'] != 0:
        raise ValueError('trial correctness failed')
    if manifest['config']['workload'] == 'topk' and job['ranked_correctness']['mismatches'] != 0:
        raise ValueError('trial ranked correctness failed')
    if manifest['config']['updates'] and job['post_update_correctness']['mismatches'] != 0:
        raise ValueError('trial post-update correctness failed')
    docker = manifest['host']['docker']
    return dict(adapter=manifest['adapter'], corpus=manifest['corpus'],
                harness_sources=manifest['harness_sources'],
                runner_sha256=manifest['runner_sha256'],
                config={k: (manifest['config'].get(k, 'auto') if k == 'plan_cache_mode' else manifest['config'][k]) for k in COMPARISON_SETTINGS},
                host={k: manifest['host'][k] for k in ('system', 'machine')},
                docker={k: docker.get(k) for k in
                        ('NCPU', 'MemTotal', 'Architecture', 'OperatingSystem', 'ServerVersion', 'KernelVersion')},
                input_sha256=job['input_sha256'], settings=job['settings'],
                extensions=job['extensions'], full_counts=job['full_counts_before'])


def paired_report(root):
    campaign = json.loads((root / 'paired.json').read_text())
    trials, invalid, contract = {}, [], None
    planned = paired_jobs(campaign['repetitions'])
    if [(j['pair'], j['variant'], j['directory']) for j in campaign['jobs']] != [
            (j['pair'], j['variant'], j['directory']) for j in planned]:
        invalid.append('planned trial schedule changed')
    for job in campaign['jobs']:
        try:
            if job['status'] != 'complete':
                raise ValueError('trial ' + job['status'])
            path = root / job['directory']
            manifest = json.loads((path / 'manifest.json').read_text())
            variant = job['variant']
            if manifest['image'] != campaign['images'][variant]['Id']:
                raise ValueError('image identity differs from planned immutable image')
            if manifest['source'] != campaign['sources'][variant]:
                raise ValueError('extension source differs from recorded build')
            current = comparison_contract(manifest)
            if contract is None:
                contract = current
            elif contract != current:
                mismatches = [key for key in contract if current[key] != contract[key]]
                raise ValueError('incompatible trial: ' + ', '.join(mismatches))
            rows = json.loads((path / 'comparison.json').read_text())
            if len(rows) != 1 or rows[0]['engine'] != 'stannum' or rows[0]['status'] != 'complete':
                raise ValueError('missing completed Stannum summary')
            row = rows[0]
            if set(row['queries']) != set(campaign['query_ids']):
                missing = set(campaign['query_ids']) - set(row['queries'])
                raise ValueError(f'incomplete timed trace coverage ({len(missing)} missing forms); increase --seconds')
            if row['update_errors'] or row['updates_attempted'] != row['updates_completed']:
                raise ValueError('failed or incomplete update attempts')
            if manifest['config']['updates'] > 0 and row['updates_completed'] <= 0:
                raise ValueError('update workload completed no updates')
            for metric in ('qps', 'p50_ms', 'p95_ms'):
                describe([row[metric]])
            for query in row['queries'].values():
                for metric in ('p50_ms', 'p95_ms'):
                    describe([query[metric]])
            trials[(job['pair'], variant)] = row
        except (OSError, ValueError, KeyError, TypeError) as error:
            invalid.append(job['directory'] + ': ' + str(error))
    complete = campaign['status'] == 'complete' and not invalid and len(trials) == len(planned)
    result = dict(complete=complete, repetitions=campaign['repetitions'],
                  completed_trials=len(trials), invalid_trials=invalid, variants={}, paired={}, queries={})
    lines = ['# Repeated Stannum comparison', '',
             f"Complete trials: {len(trials)} / {len(planned)}. Order alternates each pair.", '',
             'Fresh containers and volumes; immutable images, identical corpus, trace, seed and runtime settings.',
             'Variation is observed across trials, not a confidence interval or a capacity claim.', '']
    if complete:
        for variant in ('baseline', 'candidate'):
            rows = [trials[(n, variant)] for n in range(1, campaign['repetitions'] + 1)]
            result['variants'][variant] = {metric: describe([row[metric] for row in rows])
                                          for metric in ('qps', 'p50_ms', 'p95_ms')}
        pairs = [(trials[(n, 'baseline')], trials[(n, 'candidate')])
                 for n in range(1, campaign['repetitions'] + 1)]
        result['paired'] = dict(qps_ratio=describe([b['qps'] / a['qps'] for a, b in pairs]),
                              p95_speedup=describe([a['p95_ms'] / b['p95_ms'] for a, b in pairs]))
        for name in campaign['query_ids']:
            result['queries'][name] = {
                metric: describe([a['queries'][name][metric] / b['queries'][name][metric] for a, b in pairs])
                for metric in ('p50_ms', 'p95_ms')}
        lines += ['| Variant | Median QPS | QPS min–max | QPS CV | Median p95 ms |',
                  '| --- | ---: | ---: | ---: | ---: |']
        for variant, metrics in result['variants'].items():
            q = metrics['qps']
            lines.append(f"| {variant} | {q['median']:.1f} | {q['min']:.1f}–{q['max']:.1f} | "
                         f"{q['cv']:.3f} | {metrics['p95_ms']['median']:.3f} |")
        ratio = result['paired']['qps_ratio']
        lines += ['', f"Median paired candidate/baseline QPS: {ratio['median']:.3f}x "
                  f"(range {ratio['min']:.3f}–{ratio['max']:.3f}x).",
                  'Ratios above one favor the candidate. Per-query p50/p95 ratios are baseline/candidate latency.',
                  'See aggregate.json for every distribution. Raw trials remain alongside it.']
    else:
        lines += ['**Incomplete comparison: aggregate ratios withheld until every planned trial is valid.**', '',
                  *['- ' + reason for reason in invalid]]
    bench.save(root / 'aggregate.json', result)
    (root / 'report.md').write_text('\n'.join(lines) + '\n')
    return complete


def compare(args):
    if args.repetitions < 2:
        raise ValueError('comparison needs at least two repetitions to alternate order and measure variation')
    verify_sources(LOADED_SOURCES)
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    sources, images = {}, {}
    for variant in ('baseline', 'candidate'):
        source_path = getattr(args, variant + '_source').resolve()
        sources[variant] = recorded_source(source_path)
        images[variant] = json.loads(output(['docker', 'image', 'inspect', getattr(args, variant + '_image')]))[0]
        labels = images[variant]['Config'].get('Labels') or {}
        if labels.get('benchmark.stannum_source_sha256') != sources[variant]['source_sha256']:
            raise ValueError(variant + ' image does not match recorded source')
        bench.save(root / (variant + '-source.json'), sources[variant])
        patch = source_path.parent / 'source.patch'
        if patch.exists():
            shutil.copy2(patch, root / (variant + '-source.patch'))
    queries = trace_queries(args.driver.resolve())
    campaign = dict(status='running', repetitions=args.repetitions, sources=sources, images=images,
                    query_ids=[q[0] for q in queries if args.style == 'mixed' or q[0].split(':')[1] == args.style],
                    jobs=paired_jobs(args.repetitions))
    bench.save(root / 'paired.json', campaign)
    try:
        for job in campaign['jobs']:
            job['status'] = 'running'
            bench.save(root / 'paired.json', campaign)
            trial = argparse.Namespace(**vars(args))
            trial.command, trial.engines = 'run', ['stannum']
            trial.image = images[job['variant']]['Id']
            trial.source_manifest = root / (job['variant'] + '-source.json')
            trial.output = root / job['directory']
            print(job['directory'], flush=True)
            try:
                run(trial)
                job['status'] = 'complete'
            except BaseException as error:
                job.update(status='failed', error=str(error))
                raise
            finally:
                bench.save(root / 'paired.json', campaign)
        campaign['status'] = 'complete'
    except BaseException as error:
        campaign.update(status='failed', error=str(error))
        raise
    finally:
        bench.save(root / 'paired.json', campaign)
        valid = paired_report(root)
    if not valid:
        campaign['status'] = 'invalid'
        bench.save(root / 'paired.json', campaign)
        raise ValueError('comparison failed validation; see report.md')


def render_report(args):
    if (args.output / 'paired.json').exists():
        if not paired_report(args.output):
            raise ValueError('comparison is incomplete or invalid; see report.md')
    else:
        report(args.output)


def catalog_command(args):
    import tin_catalog
    tin_catalog.run(args)


def experiment_command(args):
    import tin_experiments
    tin_experiments.run(args)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--driver', type=Path, default=DEFAULT_DRIVER)
    commands = parser.add_subparsers(dest='command', required=True)
    p = commands.add_parser('catalog', help='observe plans on an existing TIN server using libpq environment')
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--rows', type=int, nargs='+', default=[1000, 10000])
    p.set_defaults(func=catalog_command)
    p = commands.add_parser('experiment', help='bounded remote TIN capacity and strategy experiments')
    p.add_argument('--dataset', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--rows', type=int, default=100000)
    p.add_argument('--minutes', type=int, default=45)
    p.add_argument('--seconds', type=int, default=15)
    p.add_argument('--repetitions', type=int, default=3)
    p.add_argument('--max-clients', type=int, default=8)
    p.add_argument('--skip-synthetic', action='store_true')
    p.add_argument('--stages', nargs='+', choices=['synthetic','queries','prepared','forced','concurrency','multi','maintenance','projection','planner-settings','ctid-layout'])
    p.add_argument('--index-segments', type=int, choices=[1,2,4,8])
    p.add_argument('--vacuum-before-queries', action='store_true')
    p.add_argument('--build-memory-mb', type=int, choices=[16,64,256,512])
    p.set_defaults(func=experiment_command)
    commands.add_parser('prepare').set_defaults(func=prepare)
    commands.add_parser('build').set_defaults(func=build)
    p = commands.add_parser('report')
    p.add_argument('--output', type=Path, required=True)
    p.set_defaults(func=render_report)
    p = commands.add_parser('build-image')
    p.set_defaults(func=build_image)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--image', required=True)
    common = argparse.ArgumentParser(add_help=False)
    p = common
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--dataset', type=Path, required=True)
    p.add_argument('--rows', type=bench.positive, default=1000)
    p.add_argument('--validation-rows', type=bench.positive, default=1000)
    p.add_argument('--workload', choices=['count', 'topk'], default='count')
    p.add_argument('--style', choices=['mixed', 'conjunction', 'disjunction', 'phrase'], default='mixed')
    p.add_argument('--clients', type=bench.positive, default=2)
    p.add_argument('--seconds', type=bench.positive, default=60)
    p.add_argument('--warmup', type=bench.positive, default=10)
    p.add_argument('--updates', type=int, default=0)
    p.add_argument('--seed', type=int, default=1592614637)
    p.add_argument('--cpus', type=bench.positive, default=4)
    p.add_argument('--memory', default='4g')
    p.add_argument('--shared-buffers', default='1GB')
    p.add_argument('--plan-cache-mode', choices=['auto', 'force_custom_plan', 'force_generic_plan'], default='auto')
    p.add_argument('--setup-timeout-seconds', type=bench.positive, default=1800)
    p.add_argument('--port', type=bench.positive, default=28928)
    p = commands.add_parser('run', parents=[common])
    p.set_defaults(func=run)
    p.add_argument('--image', required=True)
    p.add_argument('--engines', nargs='+', choices=['stannum', 'postgres'], default=['stannum', 'postgres'])
    p = commands.add_parser('compare', parents=[common])
    p.set_defaults(func=compare)
    p.add_argument('--repetitions', type=bench.positive, default=5)
    for variant in ('baseline', 'candidate'):
        p.add_argument('--' + variant + '-image', required=True)
        p.add_argument('--' + variant + '-source', type=Path, required=True)
    args = parser.parse_args()
    if args.command in ('prepare', 'report', 'catalog', 'experiment'):
        args.func(args)
    else:
        # Shared with native pgrx builds/tests across worktrees on this machine.
        # Do not wrap this command in another acquisition of the same lock.
        with open('/tmp/stannum-pgrx.lock', 'w') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            args.func(args)


if __name__ == '__main__':
    main()
