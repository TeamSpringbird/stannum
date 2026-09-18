#!/usr/bin/env python3
"""Check release SQL drift, snapshot installs, and every retained upgrade path.

Run after cargo pgrx install with matching pg_config/server binaries on PATH.
Callers sharing a PostgreSQL installation must hold the machine-wide pgrx lock.
"""
import difflib
import os
from pathlib import Path
import re
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[2]


def normalized(sql):
    # pgrx's dependency graph can emit independent objects in different orders.
    # Keep each connected object's SQL intact, ignore source-location lines,
    # and sort objects. Executing snapshots below still verifies dependencies.
    objects = []
    for block in sql.split('/* <begin connected objects> */'):
        block = block.replace('/* </end connected objects> */', '')
        text = '\n'.join(line.rstrip() for line in block.splitlines()
                         if line.strip() and not line.lstrip().startswith('--'))
        if text:
            objects.append(text)
    return '\n\n'.join(sorted(objects)) + '\n'


def command(args, **kwargs):
    try:
        return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT, **kwargs)
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f"{' '.join(map(str, args))} failed:\n{error.output}") from error


def main():
    version = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['package']['version']
    shared = Path(command(['pg_config', '--sharedir']).strip()) / 'extension'
    installed = shared / f'stannum--{version}.sql'
    snapshot = ROOT / 'postgres/sql' / installed.name
    actual, expected = normalized(installed.read_text()), normalized(snapshot.read_text())
    if actual != expected:
        raise AssertionError('Release schema drift: bump version and add snapshot/upgrade SQL.\n' +
                             ''.join(difflib.unified_diff(expected.splitlines(True), actual.splitlines(True))))
    assert 'corrupt_index_page' not in actual and 'index_page_kinds' not in actual
    assert 'SECURITY DEFINER' not in actual.upper()
    snapshots = sorted((ROOT / 'postgres/sql').glob('stannum--*.sql'))
    snapshots = [p for p in snapshots if p.name.count('--') == 1]
    backups = {shared / p.name: (shared / p.name).read_bytes() if (shared / p.name).exists() else None
               for p in snapshots}
    with tempfile.TemporaryDirectory(prefix='stannum-upgrade-') as directory:
        root = Path(directory)
        data = root / 'data'
        env = dict(os.environ, PGHOST=str(root), PGPORT='28938', PGUSER='postgres', PGDATABASE='postgres')
        for key in ('PGSERVICE', 'PGSERVICEFILE', 'PGPASSWORD', 'PGOPTIONS'):
            env.pop(key, None)
        def sql(text, database='postgres'):
            return command(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'], input=text,
                           env=dict(env, PGDATABASE=database)).strip()
        def fingerprint(database):
            return sql("""SELECT pg_describe_object(d.classid,d.objid,d.objsubid) || '|' ||
                CASE
                  WHEN d.classid='pg_proc'::regclass THEN
                    pg_get_functiondef(d.objid) || COALESCE((SELECT proacl::text FROM pg_proc WHERE oid=d.objid), 'DEFAULT')
                  WHEN d.classid='pg_type'::regclass THEN
                    (SELECT jsonb_build_array(typtype, typlen, typbyval, typalign, typstorage,
                      typinput::regproc::text, typoutput::regproc::text, typreceive::regproc::text,
                      typsend::regproc::text, typacl)::text FROM pg_type WHERE oid=d.objid)
                  WHEN d.classid='pg_operator'::regclass THEN
                    (SELECT jsonb_build_array(oprcode::regproc::text, oprrest::regproc::text,
                      oprjoin::regproc::text, oprcanmerge, oprcanhash)::text FROM pg_operator WHERE oid=d.objid)
                  WHEN d.classid='pg_am'::regclass THEN
                    (SELECT amhandler::regproc::text || amtype::text FROM pg_am WHERE oid=d.objid)
                  WHEN d.classid='pg_opclass'::regclass THEN
                    (SELECT jsonb_build_array(c.opcintype::regtype::text, c.opckeytype::regtype::text,
                       c.opcdefault,
                       (SELECT jsonb_agg(jsonb_build_array(a.amopstrategy, a.amoppurpose,
                          a.amoplefttype::regtype::text, a.amoprighttype::regtype::text,
                          a.amopopr::regoperator::text) ORDER BY a.amopstrategy,
                          a.amoplefttype::regtype::text, a.amoprighttype::regtype::text)
                        FROM pg_amop a WHERE a.amopfamily=c.opcfamily),
                       (SELECT jsonb_agg(jsonb_build_array(p.amprocnum,
                          p.amproclefttype::regtype::text, p.amprocrighttype::regtype::text,
                          p.amproc::regprocedure::text) ORDER BY p.amprocnum,
                          p.amproclefttype::regtype::text, p.amprocrighttype::regtype::text)
                        FROM pg_amproc p WHERE p.amprocfamily=c.opcfamily))::text
                     FROM pg_opclass c WHERE c.oid=d.objid)
                  ELSE '' END
                FROM pg_depend d JOIN pg_extension e ON e.oid=d.refobjid
                WHERE d.refclassid='pg_extension'::regclass AND e.extname='stannum'
                  AND d.deptype='e' ORDER BY 1;""", database)
        started = False
        try:
            command(['initdb', '-D', str(data), '-U', 'postgres', '-A', 'trust', '--no-locale', '--encoding=UTF8'])
            command(['pg_ctl', '-D', str(data), '-l', str(root / 'server.log'), '-w',
                     '-o', f'-p 28938 -c listen_addresses= -c unix_socket_directories={root}', 'start'])
            started = True
            sql('CREATE DATABASE fresh')
            sql('CREATE EXTENSION stannum', 'fresh')
            fresh = fingerprint('fresh')
            assert sql('SELECT stannum.version()', 'fresh') == version
            # Bootstrap compares the baseline snapshot with the generated fresh
            # install. Once a second version exists, every old snapshot must have
            # a real ALTER EXTENSION UPDATE path to the current release.
            for number, path in enumerate(snapshots):
                previous = path.stem.split('--')[1]
                assert re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', previous)
                (shared / path.name).write_bytes(path.read_bytes())
                database = f'upgrade_{number}'
                sql(f'CREATE DATABASE {database}')
                sql(f"CREATE EXTENSION stannum VERSION '{previous}'", database)
                if previous != version:
                    sql(f"ALTER EXTENSION stannum UPDATE TO '{version}'", database)
                assert sql("SELECT extversion FROM pg_extension WHERE extname='stannum'", database) == version
                assert fingerprint(database) == fresh, f'{previous} upgrade differs from fresh installation'
            print(f'Release schema {version}: drift, snapshot install, and {len(snapshots)-1} upgrade paths passed')
        finally:
            if started:
                command(['pg_ctl', '-D', str(data), '-m', 'immediate', '-w', 'stop'])
            for path, backup in backups.items():
                if backup is None:
                    path.unlink(missing_ok=True)
                else:
                    path.write_bytes(backup)


if __name__ == '__main__':
    main()
