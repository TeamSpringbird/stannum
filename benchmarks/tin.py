#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Local setup for the pinned PlanetScale benchmarker; upstream owns measurement."""
import argparse
import collections
import csv
import datetime
import gzip
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import shlex
import subprocess
import statistics
import time
import threading
import uuid

import dataset
import published_dataset
import run as bench
import resources

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / 'benchmarks/tin'
REVISION = 'f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86'
REPOSITORY = 'https://github.com/planetscale/paradedb-benchmarker.git'
DEFAULT_DRIVER = ROOT / 'benchmarks/results/tin-driver'
# The final ParadeDB 0.26.0 release on PostgreSQL 18 (18.6-1.pgdg13+2 inside), the version
# TIN v1.0.6's post measured; one multi-arch index, native on amd64 and arm64.
PARADEDB_IMAGE = 'paradedb/paradedb:0.26.0-pg18@sha256:52fc9c95fdfd462201168d1d82334ed61f85cbd921957cab083ec39800a217ac'
# The benchmarker's index (datasets' benchmarks/paradedb/post.sql at the pinned revision);
# key_field is a no-op since 0.26.0, kept verbatim. `bm25` is 0.26.0's alias of the
# `paradedb` access method, and the name the pinned driver's index I/O counters select.
PARADEDB_INDEX_SQL = ('CREATE INDEX documents_body_bm25_idx ON documents USING bm25 (id, body) '
                      'WITH (key_field=id, target_segment_count=8);')
# The native match operators the v1.0.6 post used for ParadeDB, by query family.
PARADEDB_OPERATORS = dict(conjunction='&&&', disjunction='|||', phrase='###')
# The base for arm64 builds: the Dockerfile's ParadeDB base is published for amd64 only.
ARM64_BASE = 'postgres:18-trixie'
INDEXES = dict(stannum='documents_body_stannum_idx', postgres='documents_body_gin_idx',
               paradedb='documents_body_bm25_idx')
LOADED_SOURCES = {str(p): dataset.sha256(p) for p in
                  [Path(__file__), Path(bench.__file__), Path(dataset.__file__), Path(published_dataset.__file__), Path(resources.__file__), *ASSETS.iterdir()]
                  if p.is_file()}


# Server sizing for the published Stack Exchange comparisons. A profile only
# fills options left unset, so any explicit flag still wins; the resolved
# values are what the manifest records. See "Matching TIN v1.0.6's published
# setup" in docs/benchmarks.md for the source of every value.
PROFILES = {
    # Benchmarks v2, TIN v1.0.6's published setup (planetscale.com/blog/tin-v106):
    # 8 vCPUs pinned, 64 GB; the benchmarker's defaults otherwise (shm 16g, 8
    # parallel maintenance workers). The default for every 150M run.
    'v2': dict(cpus=8, memory='64g', build_memory='64g', shared_buffers='24GB',
               maintenance_work_mem='24GB', shm_size='16g', cpu_layout='siblings',
               max_parallel_maintenance_workers=8, clients=8),
    # The launch post (TIN 1.0.2) and our AWS runs r5 to r8: 8 CPUs by CFS
    # quota, unpinned, 32 GB for queries and 64 GB for the build.
    'legacy': dict(cpus=8, memory='32g', build_memory='64g', shared_buffers='24GB',
                   maintenance_work_mem='24GB', shm_size='1g', cpu_layout='none',
                   max_parallel_maintenance_workers=None, clients=8),
}
# What run used before profiles existed; an unprofiled run is unchanged.
RUN_DEFAULTS = dict(cpus=4, memory='4g', build_memory=None, shared_buffers='1GB',
                    maintenance_work_mem='512MB', shm_size='1g', cpu_layout='none',
                    max_parallel_maintenance_workers=None, clients=2)
CPU_LAYOUTS = ('none', 'siblings', 'distinct-cores')


def apply_profile(args):
    """Fills every sizing option left unset from --profile, then the defaults."""
    for key, default in dict(profile=None, score_function='score', cpuset_cpus=None, lscpu_file=None,
                             paradedb_image=PARADEDB_IMAGE).items():
        if not hasattr(args, key):
            setattr(args, key, default)
    profile = PROFILES[args.profile] if args.profile else {}
    for key, default in RUN_DEFAULTS.items():
        if getattr(args, key, None) is None:
            setattr(args, key, profile.get(key, default))
    return args


def parse_cpuset(text):
    """'0-3,16-19' -> [0, 1, 2, 3, 16, 17, 18, 19], as docker --cpuset-cpus reads it."""
    cpus = []
    for part in text.split(','):
        if not re.fullmatch(r'\d+(-\d+)?', part.strip()):
            raise ValueError(f'invalid cpuset {text!r}')
        low, _, high = part.strip().partition('-')
        low, high = int(low), int(high or low)
        if high < low:
            raise ValueError(f'invalid cpuset range {part!r}')
        cpus.extend(range(low, high + 1))
    if len(set(cpus)) != len(cpus):
        raise ValueError(f'cpuset {text!r} repeats a CPU')
    return sorted(cpus)


def format_cpuset(cpus):
    ranges, cpus = [], sorted(cpus)
    for cpu in cpus:
        if ranges and cpu == ranges[-1][1] + 1:
            ranges[-1][1] = cpu
        else:
            ranges.append([cpu, cpu])
    return ','.join(str(a) if a == b else f'{a}-{b}' for a, b in ranges)


def parse_lscpu(text):
    """Rows of `lscpu -e=CPU,CORE,SOCKET,NODE,ONLINE`; a '-' NODE (no NUMA) reads as 0."""
    lines = [line.split() for line in text.strip().splitlines() if line.strip()]
    if not lines or lines[0][:3] != ['CPU', 'CORE', 'SOCKET']:
        raise ValueError('expected lscpu -e=CPU,CORE,SOCKET,NODE,ONLINE output')
    header, rows = lines[0], []
    for fields in lines[1:]:
        row = dict(zip(header, fields))
        rows.append(dict(cpu=int(row['CPU']), core=int(row['CORE']), socket=int(row['SOCKET']),
                         node=int(row['NODE']) if row.get('NODE', '-').isdigit() else 0,
                         online=row.get('ONLINE', 'yes') == 'yes'))
    return rows


def cpu_topology(image):
    """The CPUs a container can be pinned to, as the Docker host's kernel numbers them
    (the Linux VM's on Docker Desktop or OrbStack, not macOS's)."""
    text = output(['docker', 'run', '--rm', '--network', 'none', '--entrypoint', 'lscpu', image,
                   '-e=CPU,CORE,SOCKET,NODE,ONLINE'])
    return dict(lscpu=text, source='docker run ' + image)


def plan_cpus(lscpu_text, count, layout, cpuset=None):
    """The server's CPUs and the client's, from the host's real topology.

    `siblings` gives the server count/threads-per-core whole cores, every
    hyperthread of each: "8 vCPUs" as AWS sells them (4 cores x 2 threads on
    an i7i). `distinct-cores` gives it one thread on each of `count` cores and
    leaves their siblings idle. On a host without SMT (Graviton) the two are
    the same. The client gets every CPU on the remaining cores, so it never
    shares a core with the server. Cores are taken in (socket, node, core)
    order from the lowest, so the server stays on one socket while it fits.
    """
    rows = [r for r in parse_lscpu(lscpu_text) if r['online']]
    cores = {}
    for row in rows:
        cores.setdefault((row['socket'], row['node'], row['core']), []).append(row['cpu'])
    ordered = [sorted(cores[key]) for key in sorted(cores)]
    threads = max(len(c) for c in ordered)
    if cpuset:
        server = parse_cpuset(cpuset)
        unknown = set(server) - {r['cpu'] for r in rows}
        if unknown:
            raise ValueError(f'--cpuset-cpus names CPUs the host does not have online: {format_cpuset(unknown)}')
        layout = 'explicit'
    elif layout == 'siblings':
        if count % threads:
            raise ValueError(f'{count} CPUs is not a whole number of {threads}-thread cores; use distinct-cores')
        chosen = ordered[:count // threads]
        if len(chosen) < count // threads or any(len(c) != threads for c in chosen):
            raise ValueError(f'host has too few complete cores for {count} sibling CPUs')
        server = [cpu for core in chosen for cpu in core]
    elif layout == 'distinct-cores':
        if len(ordered) < count:
            raise ValueError(f'host has {len(ordered)} cores, fewer than {count}')
        server = [core[0] for core in ordered[:count]]
    else:
        raise ValueError(f'unknown CPU layout {layout!r}')
    if len(server) != count:
        raise ValueError(f'the server cpuset has {len(server)} CPUs but --cpus is {count}')
    used = [core for core in ordered if set(core) & set(server)]
    client = sorted(cpu for core in ordered if core not in used for cpu in core)
    return dict(layout=layout, server_cpuset=format_cpuset(server), server_cpus=len(server),
                server_physical_cores=len(used), threads_per_core=threads, smt=threads > 1,
                idle_siblings=format_cpuset(sorted(set(cpu for core in used for cpu in core) - set(server))),
                client_cpuset=format_cpuset(client) if client else None,
                host_cpus=len(rows), host_cores=len(ordered))


def client_pinning(plan, system=None):
    """The prefix that pins the driver (k6) to the client CPUs, and why it is empty when it is.
    k6 runs on the Docker host: on Linux that kernel numbers the CPUs the cpuset names; on
    macOS it runs outside the Docker VM and cannot be pinned to the VM's CPUs. `system`
    overrides the running host's, for printing another host's commands."""
    if not plan or not plan.get('client_cpuset'):
        return [], 'unpinned: no server cpuset, or no CPUs left over for the client'
    system = system or platform.system()
    if system != 'Linux':
        return [], (f'unpinned: the driver runs on the {system} host, outside the Docker VM '
                    f'(on Linux it would get CPUs {plan["client_cpuset"]})')
    if not shutil.which('taskset') and system == platform.system():
        return [], 'unpinned: taskset is not installed'
    return ['taskset', '-c', plan['client_cpuset']], 'taskset -c ' + plan['client_cpuset']


def server_command(args, name, image, volume, path, engine, plan, extra):
    """The `docker run` that starts the measured server."""
    pinning = ['--cpuset-cpus', plan['server_cpuset']] if plan else []
    settings = ['shared_buffers=' + args.shared_buffers,
                'maintenance_work_mem=' + args.maintenance_work_mem]
    if engine == 'paradedb':
        # As the benchmarker starts it: the image's own bootstrap and auto-tuning
        # (work_mem, effective_cache_size, ...) stay, under these overrides.
        settings += [f'max_parallel_workers={args.cpus}', 'max_parallel_workers_per_gather=2']
    else:
        settings += ['work_mem=16MB', f'max_parallel_workers={args.cpus}', 'jit=off']
    settings += ['track_io_timing=on', f'plan_cache_mode={args.plan_cache_mode}']
    if args.max_parallel_maintenance_workers is not None:
        settings.append(f'max_parallel_maintenance_workers={args.max_parallel_maintenance_workers}')
    if engine == 'stannum' and getattr(args, 'build_segment_docs', None) is not None:
        settings.append(f'stannum.build_segment_docs={args.build_segment_docs}')
    # Local experiments only, recorded in the manifest: extra server
    # settings such as STANNUM_POSTGRES_SETTINGS="stannum.read_cache_mb=256".
    settings += shlex.split(os.environ.get('STANNUM_POSTGRES_SETTINGS', ''))
    memory = args.build_memory or args.memory
    return (['docker', 'run', '-d', '--name', name, '--cpus', str(args.cpus), *pinning, *extra,
             '--memory', memory, '--memory-swap', memory, '--shm-size', args.shm_size,
             '-p', f'127.0.0.1:{args.port}:5432',
             '-v', f'{volume}:/var/lib/postgresql',
             # The server reads the prepared CSV itself: see the import below.
             '-v', f'{path}:/import:ro',
             '-e', 'POSTGRES_PASSWORD=postgres', '-e', 'POSTGRES_DB=benchmark',
             image, 'postgres'] + [a for setting in settings for a in ('-c', setting)])


def dry_run(args):
    """Prints what a run would start, without Docker state, the driver or the dataset."""
    apply_profile(args)
    plan = None
    if args.cpu_layout != 'none' or args.cpuset_cpus:
        text = Path(args.lscpu_file).read_text() if args.lscpu_file else cpu_topology(args.image)['lscpu']
        plan = plan_cpus(text, args.cpus, args.cpu_layout, args.cpuset_cpus)
    extra = shlex.split(os.environ.get('STANNUM_DOCKER_RUN_ARGS', ''))
    for engine in args.engines:
        print(f'# {engine}: profile {args.profile or "none"}, style {args.style}, '
              f'score {args.score_function}, layout {plan["layout"] if plan else "none"}')
        image = args.paradedb_image if engine == 'paradedb' else args.image
        print(shlex.join(server_command(args, 'stannum-trace-DRYRUN', image, 'stannum-trace-DRYRUN-data',
                                        args.output.resolve() / engine, engine, plan, extra)))
        if args.build_memory and args.build_memory != args.memory:
            print(shlex.join(['docker', 'update', '--memory', args.memory, '--memory-swap', args.memory,
                              'stannum-trace-DRYRUN']))
        # A saved topology is another (Linux) host's: print the commands that host would run.
        prefix, why = client_pinning(plan, 'Linux' if args.lscpu_file else None)
        env = dict(BACKENDS=engine, WORKLOAD=args.workload, QUERY_STYLE=args.style, VUS=args.clients,
                   DURATION=f'{args.seconds}s', PREWARM=f'{args.warmup}s', UPDATES_PER_SECOND=args.updates,
                   TOP_K=10, STANNUM_SCORE_FUNCTION=args.score_function, PARADEDB_QUERY_FORM='operators')
        print(' '.join(f'{k}={v}' for k, v in env.items()) + ' ' +
              shlex.join(prefix + [str(args.driver / 'k6'), 'run', str(args.driver / 'benchmarks/search.js')]) +
              f'   # client: {why}')
    return plan


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
                           ('live_rows_test.go', 'backends/shared/postgres/stannum_live_rows_test.go'),
                           ('timing_test.go', 'dashboard/stannum_timing_test.go')]:
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
                           ('live_rows_test.go', 'backends/shared/postgres/stannum_live_rows_test.go'),
                           ('timing_test.go', 'dashboard/stannum_timing_test.go')]:
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
    # Run dashboard tests inside Linux even when cross-compiling the driver for macOS.
    command(builder + ['env', '-u', 'GOOS', '-u', 'GOARCH', 'go', 'test', '-mod=readonly', '-p', '4', './dashboard'])
    command(builder + ['go', 'build', '-mod=readonly', '-p', '4', '-o', 'k6', './cmd/stannum-k6'])
    command(builder + ['go', 'test', '-mod=readonly', '-p', '4', '-c', '-o', 'pg-driver-test', './backends/shared/postgres'])
    if manifest['files'] != identities(driver):
        raise ValueError('driver source changed while building')
    manifest.update(binary_sha256=dataset.sha256(driver / 'k6'),
                    test_binary_sha256=dataset.sha256(driver / 'pg-driver-test'),
                    build_image=image, target_os=target_os, target_arch=target_arch)
    bench.save(driver / 'stannum-adapter.json', manifest)


def build_image(args):
    if getattr(args, 'target_cpu', None) and platform.machine() != 'x86_64':
        # The Dockerfile applies it to the x86-64 target only; elsewhere the label would lie.
        raise ValueError('--target-cpu is an x86-64 build argument; this host is ' + platform.machine())
    if not getattr(args, 'base', None) and platform.machine() in ('arm64', 'aarch64'):
        # The Dockerfile's pinned ParadeDB base is amd64-only; an arm64 host (a Mac, i8g)
        # would build it under emulation, which run() then refuses to time.
        args.base = ARM64_BASE
    args.output.mkdir(parents=True, exist_ok=False)
    source = bench.provenance(args.output)
    bench.save(args.output / 'source.json', source)
    recipe = bench.digest((ROOT / 'benchmarks/Dockerfile').read_bytes() +
                          (ROOT / 'benchmarks/Dockerfile.dockerignore').read_bytes())
    with (args.output / 'build.log').open('w') as log:
        command(['docker', 'build', '-f', 'benchmarks/Dockerfile',
                 '--build-arg', 'STANNUM_SOURCE_SHA256=' + source['source_sha256'],
                 '--build-arg', 'STANNUM_COMMIT=' + source['commit'],
                 '--build-arg', 'RECIPE_SHA256=' + recipe] +
                (['--build-arg', 'BASE=' + args.base] if getattr(args, 'base', None) else []) +
                (['--build-arg', 'STANNUM_TARGET_CPU=' + args.target_cpu] if getattr(args, 'target_cpu', None) else []) +
                ['-t', args.image, '.'],
                cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)
    bench.save(args.output / 'image.json', json.loads(output(['docker', 'image', 'inspect', args.image])))


def literal(value):
    return "'" + value.replace("'", "''") + "'"


def sql_output(text, env):
    # Large published traces exceed exec argument limits. -c executes a batch
    # in one transaction; preserve that with --single-transaction for stdin.
    if len(text.encode('utf-8')) > 65536:
        return output(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1',
                       '--single-transaction', '-f', '-'], input=text, env=env)
    return output(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1', '-c', text], env=env)


def trace_queries(driver, query_file=None, raw_text=False):
    records = json.loads((Path(query_file) if query_file else driver / 'datasets/wikipedia/queries.json').read_text())['queries']
    if not records or len({q['source_id'] for q in records}) != len(records):
        raise ValueError('trace requires unique source IDs and at least one query')
    if any(not isinstance(q['text'], str) or not q['text'].strip() for q in records):
        raise ValueError('trace text must be nonempty')
    if not raw_text and any(not re.fullmatch('[a-z]+(?: [a-z]+)*', q['text']) for q in records):
        raise ValueError('lexical oracle requires normalized ASCII word queries')
    return [(f"{q['source_id']}:{style}", q['engines']['tin'][style],
             q['engines']['postgres'][style], q['text'])
            for q in records for style in ('conjunction', 'disjunction', 'phrase')]


def selected_style(query_id, style):
    family = query_id.split(':')[1]
    return style == 'mixed' or family == style or (
        style == 'conjunction-phrase' and family in ('conjunction', 'phrase')) or (
        style == 'conjunction-disjunction' and family in ('conjunction', 'disjunction'))


def validation_queries(queries, limit):
    if limit < 0:
        raise ValueError('validation-queries must be nonnegative')
    if not limit or limit >= len(queries):
        return queries
    return [queries[i * len(queries) // limit] for i in range(limit)]


def lexical_predicate(name, text):
    style = name.split(':')[1]
    if style == 'phrase':
        return f"body ~ {literal('(^| )' + text + '( |$)')}"
    terms = [f"body ~ {literal('(^| )' + term + '( |$)')}" for term in text.split()]
    return '(' + (' OR ' if style == 'disjunction' else ' AND ').join(terms) + ')'


def check_sql(queries, engine='stannum', raw_text=False):
    # Exact set equality, not just equal counts. The reference has no search index.
    statements = []
    for name, tin, postgres, text in queries:
        predicate = (f'body ==> {literal(tin)}' if engine == 'stannum' else
                     f"body_tsv @@ to_tsquery('simple', {literal(postgres)})")
        indexed = f'SELECT id FROM documents WHERE {predicate}'
        actual = 'SELECT id FROM indexed JOIN reference USING(id)'
        reference = ((f'body ==> {literal(tin)}' if raw_text else lexical_predicate(name, text)) if engine == 'stannum' else
                     f"body_tsv @@ to_tsquery('simple', {literal(postgres)})")
        expected = f'SELECT id FROM reference WHERE {reference}'
        statements.append(f"WITH indexed AS MATERIALIZED ({indexed}) SELECT {literal(name)}, count(*), "
                          f"(SELECT count(*) FROM indexed) FROM (({actual} EXCEPT ALL {expected}) "
                          f"UNION ALL ({expected} EXCEPT ALL {actual})) difference;")
    return '\n'.join(statements)


def copy_volume(image, source, target):
    """Copies one mount's tree onto another with the benchmark image's cp, so
    a database moves between a volume and a host directory with ownership kept."""
    command(['docker', 'run', '--rm', '--entrypoint', 'cp', '-v', source, '-v', target, image,
             '-a', '/from/.', '/to/'], stdout=subprocess.DEVNULL)


def ranked_check_sql(queries, engine, score_function='score'):
    statements = []
    for name, tin, postgres, text in queries:
        tsquery = f"to_tsquery('simple', {literal(postgres)})"
        predicate = f'body ==> {literal(tin)}' if engine == 'stannum' else f'body_tsv @@ {tsquery}'
        score = f'stannum.{score_function}(ctid)' if engine == 'stannum' else f'ts_rank_cd(body_tsv,{tsquery})'
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


def prepared_plan_sql(query, engine, workload, mode, score_function='score'):
    if mode not in ('force_custom_plan', 'force_generic_plan'):
        raise ValueError('unsupported diagnostic plan mode')
    name, tin, postgres, text = query
    if engine == 'paradedb':
        fields = 'count(*)' if workload == 'count' else 'id, body, pdb.score(id) AS score'
        predicate, argument = f"body {PARADEDB_OPERATORS[name.split(':')[1]]} $1", text
    elif engine == 'stannum':
        fields = 'count(*)' if workload == 'count' else f'id, body, stannum.{score_function}(ctid) AS score'
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


def paradedb_count_sql(queries):
    """Full-corpus match counts through ParadeDB's index, with the operators the driver uses."""
    return '\n'.join(
        f"SELECT {literal(name)}, count(*) FROM documents WHERE body "
        f"{PARADEDB_OPERATORS[name.split(':')[1]]} {literal(text)};"
        for name, _, _, text in queries)


def semantics_sql(queries, raw_text=False):
    return '\n'.join(
        f"SELECT {literal(name)}, count(*) FROM reference WHERE "
        f"({('body ==> ' + literal(tin)) if raw_text else lexical_predicate(name, text)}) IS DISTINCT FROM "
        f"(body_tsv @@ to_tsquery('simple',{literal(postgres)}));"
        for name, tin, postgres, text in queries)


def validate_result(text, names):
    rows = [line.split('|') for line in text.splitlines() if line]
    if [row[0] for row in rows] != names or any(
            len(row) not in (2, 3) or row[1] != '0' or (len(row) == 3 and not row[2].isdigit()) for row in rows):
        raise ValueError('query membership mismatch or incomplete oracle output; see correctness.txt')


def workload_state(sql):
    """Untimed snapshot; VM counts are observed, tuple counts are estimates."""
    return json.loads(sql("""
        SELECT json_build_object(
            'captured_at', clock_timestamp(),
            'heap_pages', pg_relation_size(c.oid) / current_setting('block_size')::int,
            'all_visible_pages', v.all_visible, 'all_frozen_pages', v.all_frozen,
            'catalog_pages', c.relpages, 'catalog_all_visible', c.relallvisible,
            'estimated_live_tuples', s.n_live_tup, 'estimated_dead_tuples', s.n_dead_tup,
            'inserts', s.n_tup_ins, 'updates', s.n_tup_upd, 'deletes', s.n_tup_del,
            'vacuum_count', s.vacuum_count, 'autovacuum_count', s.autovacuum_count,
            'last_vacuum', s.last_vacuum, 'last_autovacuum', s.last_autovacuum,
            'table_options', c.reloptions)
        FROM pg_class c JOIN pg_stat_user_tables s ON s.relid = c.oid
        CROSS JOIN pg_visibility_map_summary(c.oid) v
        WHERE c.oid = 'documents'::regclass;
    """, setup=True))


def workload_state_contract(job, updates):
    state = job.get('workload_state')
    if not state or state.get('protocol') != 'postvacuum-observed-v1':
        raise ValueError('missing workload-state evidence; rerun with visibility capture')
    keys = ('heap_pages', 'all_visible_pages', 'all_frozen_pages')
    snapshots = {}
    for phase in ('after_vacuum', 'before_driver', 'after_restart'):
        snapshot = state[phase]
        values = {k: snapshot[k] for k in keys}
        if any(type(v) is not int or v < 0 for v in values.values()) or not (
                values['all_frozen_pages'] <= values['all_visible_pages'] <= values['heap_pages']):
            raise ValueError('invalid observed visibility coverage')
        snapshots[phase] = values
    if snapshots['after_vacuum'] != snapshots['before_driver']:
        raise ValueError('heap visibility changed during untimed validation')
    if not updates and snapshots['before_driver'] != snapshots['after_restart']:
        raise ValueError('read-only trial heap visibility changed during driver/restart')
    # Mutation endpoints are outcomes, not comparable starting conditions.
    return dict(protocol=state['protocol'], before_driver=snapshots['before_driver'],
                table_options=state['before_driver']['table_options'])


def index_mib_per_query(metrics, completed):
    read, hit = metrics.get('indexReadBytes'), metrics.get('indexHitBytes')
    if read is None or hit is None or not completed:
        return None
    return (read + hit) / completed / 2**20


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
                timestamp_ms = datetime.datetime.fromisoformat(data['time'].replace('Z', '+00:00')).timestamp() * 1000
                # The driver truncates timestamps to integer milliseconds.
                if not exported['startTime'] <= timestamp_ms < exported['endTime'] + 1:
                    raise ValueError('query sample outside exported measurement window; regenerate the export from raw evidence')
                query = data['tags']['query_id']
                samples.append(data['value'])
                groups[query.split(':')[1]].append(data['value'])
                queries[query].append(data['value'])
        if not samples or elapsed <= 0:
            raise ValueError('completed run has no measured query samples')
        resource_summary = resources.summarize(path / 'resources.jsonl',
                                               exported['startTime'] / 1000, exported['endTime'] / 1000)
        bench.save(path / 'resource-summary.json', resource_summary)
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
                         # The benchmarker's "per query" (TIN's MB/query): logical index
                         # block accesses, read + hit, over completed queries. Its counters
                         # are reset before warm-up, so warm-up traffic is in the numerator.
                         index_mib_per_query=index_mib_per_query(metrics, len(samples)),
                         updates_completed=updates['update_docs'], updates_attempted=updates['attempted'],
                         update_errors=updates['update_errors'],
                         cross_engine_membership_differences=job['cross_engine_membership_differences']))
    bench.save(root / 'comparison.json', rows)
    lines = ['# Published-trace local run', '',
             'Diagnostic measurements, not a capacity claim. Each engine uses its native query semantics.',
             'GIN ranks with ts_rank_cd; Stannum ranks with BM25. Phrase membership can also differ.',
             'Percentiles use nearest rank; p99 is omitted below 1,000 samples. No speedup ratio is inferred.', '',
             '| Engine | QPS | p95 ms | p99 ms | Index MiB/query | Measured query forms | Index MiB | Total relation MiB |',
             '| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |']
    for row in rows:
        if row['status'] != 'complete':
            lines.append(f"| {row['engine']} | incomplete | | | | | | |")
        else:
            p99 = f"{row['p99_ms']:.3f}" if row['p99_ms'] is not None else '—'
            per_query = f"{row['index_mib_per_query']:.1f}" if row['index_mib_per_query'] is not None else '—'
            lines.append(f"| {row['engine']} | {row['qps']:.1f} | {row['p95_ms']:.3f} | {p99} | {per_query} | "
                         f"{row['measured_query_forms']} | {row['index_bytes']/2**20:.2f} | {row['total_bytes']/2**20:.2f} |")
    lines += ['', 'See comparison.json for query-family and individual-query distributions,',
              'semantic differences on the validation sample, and completed updates.',
              'Index read/hit bytes are block accesses, not physical disk traffic. Index MiB/query is',
              "(read + hit) / completed queries: the benchmarker's PER QUERY, which TIN reports as MB/query.",
              'Resource summaries in comparison.json and resource-summary.json use samples wholly inside the measured window; boundary gaps are reported.',
              'The full pinned trace may not be traversed during short or slow runs.']
    for job in manifest['jobs']:
        if job.get('database_from'):
            origin = job['database_from']
            lines += ['', f"- {job['engine']}: database copied from run {origin['source_run']} "
                          f"(saved {origin['saved_at']}); import and index build were not repeated."]
    lines += ['', 'Workload-state snapshots (after VACUUM / before driver / after restart):']
    for job in manifest['jobs']:
        state = job.get('workload_state', {})
        for phase in ('after_vacuum', 'before_driver', 'after_restart'):
            observed = state.get(phase)
            if observed:
                lines.append(f"- {job['engine']} {phase}: {observed['all_visible_pages']}/"
                             f"{observed['heap_pages']} heap pages all-visible; "
                             f"{observed['estimated_dead_tuples']} estimated dead tuples; "
                             f"{observed['autovacuum_count']} autovacuums.")
    lines += ['', 'Before-driver capture precedes upstream warmup. After-restart capture follows '
              'container shutdown/restart and is not an exact end-of-traffic snapshot. '
              'These observations do not establish cold-cache or pristine-heap conditions.']
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
    apply_profile(args)
    if getattr(args, 'dry_run', False):
        dry_run(args)
        return
    verify_sources(LOADED_SOURCES)
    driver = args.driver.resolve()
    adapter = verify_driver(driver)
    if adapter.get('binary_sha256') != dataset.sha256(driver / 'k6'):
        raise ValueError('driver binary not built from recorded adapter')
    if adapter.get('test_binary_sha256') != dataset.sha256(driver / 'pg-driver-test'):
        raise ValueError('driver regression binary does not match recorded build')
    published = getattr(args, 'published_corpus', None)
    raw_text = published == 'stackexchange'
    corpus = published_dataset.inspect(args.dataset, published, json.loads(
        (driver / 'datasets' / published / 'data-manifest.json').read_text())) if published else dataset.verify(args.dataset)
    trace_path = Path(getattr(args, 'query_file', None) or driver / 'datasets' / (published or 'wikipedia') / 'queries.json').resolve()
    queries = trace_queries(driver, trace_path, raw_text=raw_text)
    if len(set(args.engines)) != len(args.engines) or not 0 <= args.updates <= 1000000000:
        raise ValueError('engines must be distinct and updates must be between 0 and 1000000000')
    if args.rows > corpus['rows']:
        raise ValueError('requested rows exceed dataset')
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    source_path = getattr(args, 'source_manifest', None)
    source = recorded_source(source_path) if source_path else bench.provenance(root)
    if 'paradedb' in args.engines and args.updates:
        raise ValueError('ParadeDB runs are read-only calibration runs; its post-update checks are not implemented')
    native = {'arm64': 'arm64', 'aarch64': 'arm64', 'x86_64': 'amd64'}[platform.machine()]
    image = json.loads(output(['docker', 'image', 'inspect', args.image]))[0]
    if image['Architecture'] != native:
        raise ValueError('image must run natively; emulated timings are not supported')
    images = dict(stannum=image, postgres=image)
    if 'paradedb' in args.engines:
        if subprocess.run(['docker', 'image', 'inspect', args.paradedb_image], capture_output=True).returncode:
            command(['docker', 'pull', args.paradedb_image], stdout=subprocess.DEVNULL)
        images['paradedb'] = json.loads(output(['docker', 'image', 'inspect', args.paradedb_image]))[0]
        if images['paradedb']['Architecture'] != native:
            raise ValueError('ParadeDB image must run natively; emulated timings are not supported')
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
    shutil.copy2(trace_path, root / 'queries.json')
    manifest['trace'] = dict(sha256=dataset.sha256(root / 'queries.json'), forms=len(queries))
    queries = trace_queries(driver, root / 'queries.json', raw_text=raw_text)
    manifest['trace']['forms'] = len(queries)
    queries = validation_queries(queries, getattr(args, 'validation_queries', 0))
    manifest['validation_query_ids'] = [q[0] for q in queries]
    bench.save(root / 'manifest.json', manifest)
    protocol = root / 'protocol'
    protocol.mkdir()
    for filename in ('tin.py', 'run.py', 'dataset.py', 'published_dataset.py'):
        shutil.copy2(ROOT / 'benchmarks' / filename, protocol / filename)
    shutil.copytree(ASSETS, protocol / 'tin')
    manifest['host'] = dict(system=platform.platform(), machine=platform.machine(),
                            docker=json.loads(output(['docker', 'info', '--format', '{{json .}}'])))
    # The server's CPUs from the Docker host's topology, recorded with the
    # topology itself so a reader can tell "8 vCPUs" readings apart.
    plan = None
    if args.cpu_layout != 'none' or getattr(args, 'cpuset_cpus', None):
        topology = (dict(lscpu=Path(args.lscpu_file).read_text(), source=str(args.lscpu_file))
                    if getattr(args, 'lscpu_file', None) else cpu_topology(image['Id']))
        plan = plan_cpus(topology['lscpu'], args.cpus, args.cpu_layout, getattr(args, 'cpuset_cpus', None))
        manifest['host']['lscpu'] = topology
    client_prefix, client_note = client_pinning(plan)
    manifest['cpu_pinning'] = dict(plan or dict(layout='none', server_cpuset=None),
                                   method='docker --cpuset-cpus' if plan else 'docker --cpus quota only',
                                   cpus_quota=args.cpus, client=client_note)
    manifest['profile'] = getattr(args, 'profile', None)
    manifest['score_function'] = getattr(args, 'score_function', 'score')
    manifest['engine_images'] = {engine: dict(id=images[engine]['Id'], repo_digests=images[engine].get('RepoDigests'))
                                 for engine in args.engines}
    bench.save(root / 'manifest.json', manifest)
    env = dict(os.environ, PGHOST='127.0.0.1', PGPORT=str(args.port), PGUSER='postgres',
               PGPASSWORD='postgres', PGDATABASE='benchmark',
               PGOPTIONS='-c statement_timeout=120000 -c jit=off')
    for key in ('PGSERVICE', 'PGSERVICEFILE'):
        env.pop(key, None)
    # Every statement the harness itself runs is setup or validation, never
    # a measurement: the driver measures. Under the query timeout a check
    # over the full corpus, such as materializing a stopword disjunction's
    # matches at 150 million rows, ended a campaign after a four-hour build.
    def sql(text, setup=True):
        sql_env = dict(env, PGOPTIONS=f'-c statement_timeout={args.setup_timeout_seconds * 1000} -c jit=off') if setup else env
        return sql_output(text, sql_env)
    try:
        for engine in args.engines:
            verify_sources(LOADED_SOURCES)
            if adapter != verify_driver(driver) or adapter['binary_sha256'] != dataset.sha256(driver / 'k6'):
                raise ValueError('driver changed between engines')
            name = 'stannum-trace-' + uuid.uuid4().hex[:12]
            volume = name + '-data'
            path = root / engine
            path.mkdir()
            job = dict(engine=engine, status='running', container=name, volume=volume,
                       resource_limits=dict(build_memory=getattr(args, 'build_memory', None) or args.memory,
                                            query_memory=args.memory, cpus=args.cpus,
                                            cpuset=plan['server_cpuset'] if plan else None,
                                            shm_size=args.shm_size))
            manifest['jobs'].append(job)
            bench.save(root / 'manifest.json', manifest)
            command(['docker', 'volume', 'create', volume], stdout=subprocess.DEVNULL)
            sampler = None
            loaded = None
            if getattr(args, 'load_database', None):
                # A saved database is what a fresh run holds after its build,
                # VACUUM and checks: the same corpus prefix in the same engine.
                loaded = json.loads((args.load_database / 'snapshot.json').read_text())
                for key, want in (('engine', engine), ('published_corpus', args.published_corpus),
                                  ('rows', args.rows)):
                    if loaded.get(key) != want:
                        raise ValueError(f'saved database {key} {loaded.get(key)!r} does not match {want!r}')
                # A database built by another image is usable as long as this
                # image reads its segment format, which the directory check
                # after startup proves; the origin is recorded either way.
                copy_volume(images[engine]['Id'], f'{args.load_database.resolve()}:/from:ro', f'{volume}:/to')
            try:
                # Extra `docker run` flags for local rehearsals, such as read throttling that
                # stands in for a slower disk: STANNUM_DOCKER_RUN_ARGS="--device-read-iops /dev/vdb:20000".
                extra = shlex.split(os.environ.get('STANNUM_DOCKER_RUN_ARGS', ''))
                start = server_command(args, name, images[engine]['Id'], volume, path.resolve(), engine, plan, extra)
                job['docker_run'] = start
                command(start, stdout=subprocess.DEVNULL)
                deadline = time.monotonic() + 90
                while subprocess.run(['pg_isready'], env=env, capture_output=True).returncode:
                    if time.monotonic() > deadline:
                        raise TimeoutError('PostgreSQL startup')
                    time.sleep(.5)
                sampler = ResourceSampler(name, path / 'resources.jsonl')
                sampler.thread.start()
                job['postgres'] = dict(server_version=sql('SHOW server_version;'), version=sql('SELECT version();'))
                manifest['postgres_version'] = job['postgres']['server_version']
                if loaded is None:
                    sql('CREATE EXTENSION pg_prewarm; CREATE EXTENSION pg_visibility; ' +
                        # The ParadeDB image's bootstrap has already created pg_search.
                        ('CREATE EXTENSION IF NOT EXISTS pg_search;' if engine == 'paradedb' else 'CREATE EXTENSION stannum;'))
                if engine == 'paradedb':
                    job['extension_version'] = sql("SELECT extversion FROM pg_extension WHERE extname = 'pg_search';")
                    (path / 'driver-regressions.txt').write_text('skipped: the regressions exercise the Stannum adapter\n')
                else:
                    with (path / 'driver-regressions.txt').open('w') as log:
                        command([driver / 'pg-driver-test', '-test.v'],
                                env=dict(env, STANNUM_BENCH_TEST_URL=f'postgres://postgres:postgres@127.0.0.1:{args.port}/benchmark',
                                         BENCHMARKER_POSTGRES_TEST_URL=f'postgres://postgres:postgres@127.0.0.1:{args.port}/benchmark'),
                                stdout=log, stderr=subprocess.STDOUT)
                index = INDEXES[engine]
                if loaded is not None:
                    for key in ('input_sha256', 'import_seconds', 'index_build_seconds',
                                'build_segment_docs', 'segments_after_build'):
                        if key in loaded:
                            job[key] = loaded[key]
                    job['database_from'] = dict(path=str(args.load_database.resolve()),
                                                source_run=loaded['source_run'], saved_at=loaded['saved_at'])
                    if int(sql('SELECT count(*) FROM documents;', setup=True)) != args.rows:
                        raise ValueError('saved database row count differs from --rows')
                    if engine == 'stannum':
                        job['segments_after_build'] = json.loads(sql(
                            f"SELECT coalesce(json_agg(s ORDER BY ordinal), '[]'::json) FROM stannum.segment_info('{index}') s;"))
                        job['database_from']['image'] = loaded.get('image')
                        job['database_from']['commit'] = loaded.get('commit')
                if loaded is None:
                    if engine != 'paradedb':
                        sql((ASSETS / 'position-limit.sql').read_text())
                    sql('CREATE TABLE documents(id text NOT NULL, body text NOT NULL);' if published else
                        'CREATE TABLE documents(id bigint PRIMARY KEY, body text NOT NULL);')
                if loaded is None:
                    # Published IDs remain text, matching the upstream schema without a PK.
                    if published:
                        published_dataset.prefix(args.dataset, path / 'input.csv', args.rows)
                    else:
                        with (path / 'input.csv').open('w') as target, (Path(args.dataset) / 'documents.csv').open() as source_csv:
                            reader, writer = csv.reader(source_csv), csv.writer(target)
                            for number, row in enumerate(reader):
                                if number == args.rows:
                                    break
                                writer.writerow(row)
                    job['input_sha256'] = dataset.sha256(path / 'input.csv')
                    sampler.phase = 'import'
                    started = time.monotonic()
                    # Not COPY FROM STDIN: psql ends the data at a line holding only
                    # `\.`, even inside a quoted field, and a published Stack
                    # Exchange document has such a line in a code block. PostgreSQL
                    # 18 reads a CSV file without that marker.
                    (path / 'input.csv').chmod(0o644)
                    sql("COPY documents FROM '/import/input.csv' WITH (FORMAT csv);", setup=True)
                    job['import_seconds'] = time.monotonic() - started
                    if published and sql('SELECT count(*) = count(DISTINCT id) FROM documents;', setup=True) != 't':
                        raise ValueError('membership validation requires unique source IDs')
                    started = time.monotonic()
                    sampler.phase = 'vector-preparation'
                    if engine == 'postgres':
                        sql("ALTER TABLE documents ADD COLUMN body_tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', body)) STORED;", setup=True)
                    job['vector_preparation_seconds'] = time.monotonic() - started
                    started = time.monotonic()
                    expression = 'gin(body_tsv)' if engine == 'postgres' else 'stannum(body)'
                    sampler.phase = 'index-build'
                    bench.save(path / 'before-build-cgroup.json', resource_snapshot(name))
                    build_sql = (PARADEDB_INDEX_SQL if engine == 'paradedb' else
                                 f'CREATE INDEX {index} ON documents USING {expression};')
                    job['index_sql'] = build_sql
                    if engine == 'stannum':
                        build_sql += " SELECT current_setting('stannum.build_segment_docs');"
                    build_result = sql(build_sql, setup=True)
                    if engine == 'stannum':
                        job['build_segment_docs'] = int(build_result)
                        requested = getattr(args, 'build_segment_docs', None)
                        if requested is not None and job['build_segment_docs'] != requested:
                            raise ValueError('effective build batch size differs from requested value')
                    bench.save(path / 'after-build-cgroup.json', resource_snapshot(name))
                    job['index_build_seconds'] = time.monotonic() - started
                    if engine == 'stannum':
                        job['segments_after_build'] = json.loads(sql(
                            f"SELECT coalesce(json_agg(s ORDER BY ordinal), '[]'::json) FROM stannum.segment_info('{index}') s;"))
                if engine == 'paradedb':
                    job['segments_after_build'] = int(sql(f"SELECT count(*) FROM paradedb.index_info('{index}');"))
                if job['resource_limits']['build_memory'] != args.memory:
                    sampler.phase = 'query-memory-transition'
                    command(['docker', 'update', '--memory', args.memory, '--memory-swap', args.memory, name],
                            stdout=subprocess.DEVNULL)
                if getattr(args, 'before_measure_sql', None):
                    # A deliberate change to the loaded or built database before
                    # validation and measurement, such as deleting a fraction of
                    # the rows: recorded with its text and duration. Paragraphs
                    # (separated by blank lines) run as separate statements, so
                    # one can be a VACUUM.
                    sampler.phase = 'before-measure'
                    statement = args.before_measure_sql.read_text()
                    started = time.monotonic()
                    for paragraph in statement.split('\n\n'):
                        if paragraph.strip():
                            sql(paragraph, setup=True)
                    job['before_measure'] = dict(sql=statement, seconds=round(time.monotonic() - started, 3))
                    bench.save(root / 'manifest.json', manifest)
                sampler.phase = 'validation'
                if loaded is None:
                    sql('VACUUM ANALYZE documents;', setup=True)
                job['workload_state'] = dict(protocol='postvacuum-observed-v1',
                                             after_vacuum=workload_state(sql))
                job['sizes'] = json.loads(sql(f"SELECT json_build_object('index',pg_relation_size('{index}'),'table',pg_table_size('documents'),'total',pg_total_relation_size('documents'),'rows',(SELECT count(*) FROM documents));", setup=True))
                if engine == 'paradedb':
                    # A calibration engine with its own tokenizer: no oracle, only the
                    # full-corpus match counts of the validation sample, as a record.
                    counts_sql = paradedb_count_sql(queries)
                    (path / 'correctness.sql').write_text(counts_sql)
                    started = time.monotonic()
                    text = sql(counts_sql)
                    (path / 'correctness.txt').write_text(text + '\n')
                    job['full_counts_before'] = {name: int(count) for name, count in
                                                (line.split('|') for line in text.splitlines())}
                    job['correctness'] = dict(queries=len(queries), mismatches=None,
                                              reference='none: calibration engine; full-corpus counts recorded',
                                              seconds=time.monotonic() - started)
                    job['cross_engine_membership_differences'] = {}
                else:
                    sql(f"CREATE TABLE reference AS SELECT id, body, to_tsvector('simple',body) AS body_tsv FROM documents ORDER BY id LIMIT {args.validation_rows};", setup=True)
                    oracle = 'SET enable_seqscan=off;\n' + check_sql(queries, engine, raw_text=raw_text)
                    (path / 'correctness.sql').write_text(oracle)
                    started = time.monotonic()
                    text = sql(oracle)
                    (path / 'correctness.txt').write_text(text + '\n')
                    validate_result(text, [q[0] for q in queries])
                    job['full_counts_before'] = {name: int(count) for name, _, count in
                                                (line.split('|') for line in text.splitlines())}
                    job['correctness'] = dict(queries=len(queries), mismatches=0,
                                              reference='same-engine-unindexed-tokenizer' if raw_text else 'normalized-lexical-or-gin',
                                              sampled_rows=min(args.rows, args.validation_rows),
                                              seconds=time.monotonic() - started)
                    semantics = sql(semantics_sql(queries, raw_text=raw_text))
                    (path / 'semantics.txt').write_text(semantics + '\n')
                    differences = {name: int(count) for name, count in
                                   (line.split('|') for line in semantics.splitlines()) if int(count)}
                    job['cross_engine_membership_differences'] = differences
                    sql('DROP TABLE reference;')
                    if args.workload == 'topk':
                        ranked_queries = validation_queries(queries, getattr(args, 'ranked_validation_queries', 0))
                        # Like the count check: the references go through the index,
                        # not a sequential scan that tokenizes every body.
                        ranked_sql = 'SET enable_seqscan=off;\n' + ranked_check_sql(ranked_queries, engine, args.score_function)
                        (path / 'ranked-correctness.sql').write_text(ranked_sql)
                        # Exhaustive references over the full table are setup work: one
                        # stopword-heavy disjunction over 15 million rows outlasts the
                        # timeout measured queries run under.
                        ranked = sql(ranked_sql, setup=True)
                        (path / 'ranked-correctness.txt').write_text(ranked + '\n')
                        validate_result(ranked, [q[0] for q in ranked_queries])
                        job['ranked_correctness'] = dict(queries=len(ranked_queries), mismatches=0,
                                                         reference='exhaustive same-engine score multiset; ties unordered')
                plan_queries = [q for q in queries if selected_style(q[0], args.style)]
                for query in plan_queries[:6]:
                    for mode in ('force_custom_plan', 'force_generic_plan'):
                        statement = prepared_plan_sql(query, engine, args.workload, mode, args.score_function)
                        filename = 'plan-' + query[0].replace(':', '-') + '-' + mode
                        (path / (filename + '.sql')).write_text(statement + '\n')
                        # Diagnostics, not measurements: a forced generic plan may scan the heap.
                        (path / (filename + '.json')).write_text(sql(statement, setup=True))
                sql('CHECKPOINT;')
                if getattr(args, 'save_database', None):
                    # The state every workload starts from, copied out of the
                    # stopped container so the files are consistent.
                    sampler.phase = 'save-database'
                    target = args.save_database.resolve()
                    if target.exists() and any(target.iterdir()):
                        raise ValueError(f'--save-database {target} is not empty')
                    target.mkdir(parents=True, exist_ok=True)
                    command(['docker', 'stop', '-t', '600', name], stdout=subprocess.DEVNULL)
                    copy_volume(image['Id'], f'{volume}:/from:ro', f'{target}:/to')
                    (target / 'snapshot.json').write_text(json.dumps(dict(
                        engine=engine, published_corpus=args.published_corpus, rows=args.rows,
                        image=image['Id'], source_run=root.name,
                        saved_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                        **{k: job[k] for k in ('input_sha256', 'import_seconds', 'index_build_seconds',
                                               'build_segment_docs', 'segments_after_build', 'sizes') if k in job}),
                        indent=1))
                    command(['docker', 'start', name], stdout=subprocess.DEVNULL)
                    deadline = time.monotonic() + 90
                    while subprocess.run(['pg_isready'], env=env, capture_output=True).returncode:
                        if time.monotonic() > deadline:
                            raise TimeoutError('PostgreSQL restart after saving the database')
                        time.sleep(.5)
                    job['database_saved'] = str(target)
                job['settings'] = json.loads(sql("SELECT json_object_agg(name,setting) FROM pg_settings;"))
                job['extensions'] = json.loads(sql('SELECT json_object_agg(extname,extversion) FROM pg_extension;'))
                job['workload_state']['before_driver'] = workload_state(sql)
                bench.save(path / 'before-cgroup.json', bench.container_counters(name))
                # Upstream stops this owned container at phase end. No Makefile
                # or project-wide cleanup is invoked, and cooldown stays zero.
                run_env = dict(os.environ, BACKENDS=engine, WORKLOAD=args.workload,
                               QUERY_STYLE=args.style, QUERIES=str(root / 'queries.json'),
                               CONFIG_DIR=str(driver / 'datasets/wikipedia'),
                               STANNUM_PORT=str(args.port), POSTGRES_PORT=str(args.port),
                               PARADEDB_PORT=str(args.port), PARADEDB_CONTAINER=name,
                               # ParadeDB through |||, &&& and ### (the v1.0.6 post), not @@@.
                               PARADEDB_QUERY_FORM='operators',
                               STANNUM_CONTAINER=name, POSTGRES_CONTAINER=name,
                               VUS=str(args.clients), DURATION=f'{args.seconds}s',
                               PREWARM=f'{args.warmup}s', UPDATES_PER_SECOND=str(args.updates),
                               SEED=str(args.seed), COOLDOWN='0s', TOP_K='10',
                               DASHBOARD_EXPORT_DIR=str(path), DASHBOARD_EXPORT_PREFIX='result',
                               STANNUM_SCORE_FUNCTION=args.score_function)
                sampler.phase = 'driver-warmup-and-measurement'
                with (path / 'driver.log').open('w') as log:
                    # Pinned to the CPUs the server does not use, where the host allows it.
                    command([*client_prefix, driver / 'k6', 'run', '--out', 'dashboard=json',
                             '--out', 'json=' + str(path / 'samples.json.gz'),
                             '--summary-export', path / 'summary.json',
                             driver / 'benchmarks/search.js'], env=run_env, stdout=log, stderr=subprocess.STDOUT)
                (path / 'post-traffic-container.json').write_text(output(['docker', 'inspect', name]))
                # Upstream stops the container. This is a post-restart observation,
                # not an exact end-of-traffic snapshot; it is outside timing.
                command(['docker', 'start', name], stdout=subprocess.DEVNULL)
                deadline = time.monotonic() + 90
                while subprocess.run(['pg_isready'], env=env, capture_output=True).returncode:
                    if time.monotonic() > deadline:
                        raise TimeoutError('PostgreSQL restart for workload-state capture')
                    time.sleep(.5)
                job['workload_state']['after_restart'] = workload_state(sql)
                bench.save(path / 'workload-state.json', job['workload_state'])
                if args.updates:
                    if int(sql('SELECT count(*) FROM documents;', setup=True)) != args.rows:
                        raise ValueError('update-only phase changed row count')
                    sql(f"CREATE TABLE reference AS SELECT id, body, to_tsvector('simple',body) AS body_tsv FROM documents ORDER BY id LIMIT {args.validation_rows};", setup=True)
                    after = sql(oracle)
                    (path / 'correctness-after.txt').write_text(after + '\n')
                    validate_result(after, [q[0] for q in queries])
                    counts_after = {name: int(count) for name, _, count in
                                    (line.split('|') for line in after.splitlines())}
                    if counts_after != job['full_counts_before']:
                        raise ValueError('whitespace-only updates changed full-corpus query counts')
                    job['post_update_correctness'] = dict(queries=len(queries), mismatches=0)
                    if args.workload == 'topk':
                        after_ranked = sql(ranked_sql, setup=True)
                        (path / 'ranked-correctness-after.txt').write_text(after_ranked + '\n')
                        validate_result(after_ranked, [q[0] for q in ranked_queries])
                        job['post_update_ranked_correctness'] = dict(queries=len(ranked_queries), mismatches=0,
                            reference='exhaustive same-engine score multiset after updates; ties unordered')
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
        if dataset.sha256(root / 'queries.json') != manifest['trace']['sha256']:
            raise ValueError('query trace changed during campaign')
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
# Recorded since the TIN v1.0.6 alignment; absent (None) in older manifests.
LATER_COMPARISON_SETTINGS = ('profile', 'score_function', 'cpu_layout', 'cpuset_cpus', 'shm_size',
                             'max_parallel_maintenance_workers')


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
    if manifest['config']['updates'] and manifest['config']['workload'] == 'topk':
        ranked_after = job.get('post_update_ranked_correctness')
        if (not ranked_after or ranked_after.get('mismatches') != 0
                or type(ranked_after.get('queries')) is not int or ranked_after['queries'] <= 0
                or ranked_after['queries'] != job['ranked_correctness'].get('queries')):
            raise ValueError('trial post-update ranked correctness missing or failed')
    docker = manifest['host']['docker']
    return dict(build_segment_docs=manifest['config'].get('build_segment_docs'),
                effective_build_segment_docs=job.get('build_segment_docs'),
                resource_limits=job.get('resource_limits'),
                validation_query_ids=manifest.get('validation_query_ids'), trace=manifest.get('trace'), adapter=manifest['adapter'], corpus=manifest['corpus'],
                harness_sources=manifest['harness_sources'],
                runner_sha256=manifest['runner_sha256'],
                config={**{k: (manifest['config'].get(k, 'auto') if k == 'plan_cache_mode' else manifest['config'][k]) for k in COMPARISON_SETTINGS},
                        **{k: manifest['config'].get(k) for k in LATER_COMPARISON_SETTINGS}},
                cpu_pinning=manifest.get('cpu_pinning'),
                host={k: manifest['host'][k] for k in ('system', 'machine')},
                docker={k: docker.get(k) for k in
                        ('NCPU', 'MemTotal', 'Architecture', 'OperatingSystem', 'ServerVersion', 'KernelVersion')},
                workload_state=workload_state_contract(job, manifest['config']['updates']),
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
    queries = trace_queries(args.driver.resolve(), getattr(args, 'query_file', None) or
                            args.driver.resolve() / 'datasets' / (getattr(args, 'published_corpus', None) or 'wikipedia') / 'queries.json',
                            raw_text=getattr(args, 'published_corpus', None) == 'stackexchange')
    campaign = dict(status='running', repetitions=args.repetitions, sources=sources, images=images,
                    query_ids=[q[0] for q in queries if selected_style(q[0], args.style)],
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


def parser():
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
    p.add_argument('--base', help='Base image for a rehearsal on another architecture; the published runs use the pinned ParadeDB image')
    p.add_argument('--target-cpu', help='x86-64 only: STANNUM_TARGET_CPU build argument (x86-64-v3, x86-64-v4); '
                                        'unset builds the baseline with runtime SIMD dispatch')
    common = argparse.ArgumentParser(add_help=False)
    p = common
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--dataset', type=Path, required=True)
    p.add_argument('--published-corpus', choices=['wikipedia', 'stackexchange'])
    p.add_argument('--query-file', type=Path, help='Explicit trace; snapshotted and hashed for every run')
    p.add_argument('--rows', type=bench.positive, default=1000)
    p.add_argument('--validation-rows', type=bench.positive, default=1000)
    p.add_argument('--validation-queries', type=int, default=0, help='Evenly spaced query-form sample for untimed checks; 0 checks every form')
    p.add_argument('--ranked-validation-queries', type=int, default=0,
                   help='Sample of the validation queries for the exhaustive ranked check, which scores every match; 0 checks them all')
    p.add_argument('--save-database', type=Path,
                   help='After the build and its checks, copy the data volume here for later runs to start from')
    p.add_argument('--load-database', type=Path,
                   help='Start from a database saved by --save-database, skipping import and index build')
    p.add_argument('--before-measure-sql', type=Path,
                   help='SQL run on the database after load or build and before validation and measurement')
    p.add_argument('--workload', choices=['count', 'topk'], default='count')
    p.add_argument('--style', choices=['mixed', 'conjunction', 'disjunction', 'phrase', 'conjunction-phrase', 'conjunction-disjunction'], default='mixed')
    p.add_argument('--profile', choices=sorted(PROFILES),
                   help='Server sizing of a published setup (v2 = TIN v1.0.6: 8 pinned CPUs, 64g; legacy = the launch '
                        'post and AWS r5-r8: 8 CPUs by quota, 32g); '
                        'it fills only the sizing options left unset')
    p.add_argument('--clients', type=bench.positive, default=None, help='Driver clients (default 2; 8 under a profile)')
    p.add_argument('--seconds', type=bench.positive, default=60)
    p.add_argument('--warmup', type=bench.positive, default=10)
    p.add_argument('--updates', type=int, default=0)
    p.add_argument('--seed', type=int, default=1592614637)
    p.add_argument('--cpus', type=bench.positive, default=None, help='CPU quota and max_parallel_workers (default 4)')
    p.add_argument('--cpu-layout', choices=CPU_LAYOUTS, default=None,
                   help='Pin the server with --cpuset-cpus chosen from the host topology (lscpu): siblings = '
                        'cpus/threads-per-core whole cores, both hyperthreads of each (8 vCPUs as AWS sells them); '
                        'distinct-cores = one thread on each of cpus cores; none = CFS quota only (the default)')
    p.add_argument('--cpuset-cpus', help='Pin the server to exactly these CPUs (e.g. 0-3,16-19); overrides --cpu-layout')
    p.add_argument('--lscpu-file', type=Path,
                   help='Topology from saved `lscpu -e=CPU,CORE,SOCKET,NODE,ONLINE` output instead of the Docker host')
    p.add_argument('--memory', default=None, help='Container memory for validation and queries (default 4g)')
    p.add_argument('--build-memory', default=None, help='Optional separate build cap; switch to --memory before validation and queries')
    p.add_argument('--shared-buffers', default=None, help='default 1GB')
    p.add_argument('--maintenance-work-mem', default=None, help='default 512MB')
    p.add_argument('--max-parallel-maintenance-workers', type=int, default=None,
                   help='Server setting; unset keeps PostgreSQL\'s default (2), v2 uses the benchmarker\'s 8')
    p.add_argument('--shm-size', default=None, help='Container /dev/shm (default 1g; the benchmarker uses 16g)')
    p.add_argument('--score-function', choices=['score', 'full_score'], default='score',
                   help='Ranking: stannum.score (dense terms elided at 10%%, as tin.score: "TIN") or '
                        'stannum.full_score (every term, as tin.full_score: "TIN_FULL")')
    p.add_argument('--dry-run', action='store_true',
                   help='Print the server and driver commands this run would start, and exit')
    p.add_argument('--build-segment-docs', type=bench.positive,
                   help='Override Stannum construction batch size; default uses the extension setting')
    p.add_argument('--plan-cache-mode', choices=['auto', 'force_custom_plan', 'force_generic_plan'], default='auto')
    p.add_argument('--setup-timeout-seconds', type=bench.positive, default=1800)
    p.add_argument('--port', type=bench.positive, default=28928)
    p = commands.add_parser('run', parents=[common])
    p.add_argument('--source-manifest', type=Path, default=None,
                   help='the source.json build-image wrote for --image, so a run matches the image rather than the working tree')
    p.set_defaults(func=run)
    p.add_argument('--image', required=True)
    p.add_argument('--engines', nargs='+', choices=['stannum', 'postgres', 'paradedb'], default=['stannum', 'postgres'],
                   help='paradedb is the calibration engine: the pinned ParadeDB 0.26.0 image, its |||/&&&/### operators')
    p.add_argument('--paradedb-image', default=PARADEDB_IMAGE, help='default: the final 0.26.0 release, pinned by digest')
    p = commands.add_parser('compare', parents=[common])
    p.set_defaults(func=compare)
    p.add_argument('--repetitions', type=bench.positive, default=5)
    for variant in ('baseline', 'candidate'):
        p.add_argument('--' + variant + '-image', required=True)
        p.add_argument('--' + variant + '-source', type=Path, required=True)
    return parser


def main():
    args = parser().parse_args()
    # No global lock here: a run or an image build works inside Docker and
    # touches no native pgrx build or install, and a full-scale run holding
    # the pgrx lock for hours blocked every test and image build meanwhile.
    # Callers that must serialize with pgrx work wrap the command themselves.
    args.func(args)


if __name__ == '__main__':
    main()
