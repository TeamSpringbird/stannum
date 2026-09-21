#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Owned local PostgreSQL probe: repeat vacuumed timings, mutate, then vacuum.

Input is an existing headerless two-column Wikipedia CSV. Requires installed
experimental Stannum and PostgreSQL tools in PATH. Serializes installation use
with the shared local pgrx lock. This is a local diagnostic, not a capacity run.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--release-library', type=Path, required=True, help='Release artifact whose hash must match the installed extension')
    parser.add_argument('--rows', type=int, default=100000)
    parser.add_argument('--crossover-queries', type=Path, help='Run all 302 published queries using the crossover probe')
    parser.add_argument('--port', type=int, default=29430)
    args = parser.parse_args()
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    protocol = out/'protocol'
    protocol.mkdir()
    for name in ('count_local.py','count_probe.py','count_crossover.py','count_oracle.py'):
        shutil.copy2(Path(__file__).with_name(name),protocol/name)
    with args.input.open('rb') as source:
        digest = hashlib.file_digest(source, 'sha256').hexdigest()
    env = dict(os.environ, PGHOST='127.0.0.1', PGPORT=str(args.port), PGDATABASE='postgres', PGUSER=os.environ['USER'])
    for key in ('PGOPTIONS','PGSERVICE','PGSERVICEFILE','PGPASSWORD'):
        env.pop(key,None)
    meta = dict(status='starting', input=str(args.input.resolve()), input_sha256=digest,
                expected_rows=args.rows, host=platform.platform(), machine=platform.machine(), phases=[])
    def save():
        (out/'manifest.json').write_text(json.dumps(meta,indent=2)+'\n')
    def sql(statement):
        return subprocess.check_output(['psql','-XqAt','-v','ON_ERROR_STOP=1','-c',statement],env=env,text=True).strip()
    save()
    with open('/tmp/stannum-pgrx.lock','a') as lock, tempfile.TemporaryDirectory(prefix='stannum-local-count-') as temp:
        fcntl.flock(lock,fcntl.LOCK_EX)
        data = Path(temp)/'data'
        try:
            subprocess.run(['initdb','-D',str(data),'-A','trust'],check=True,stdout=subprocess.DEVNULL)
            subprocess.run(['pg_ctl','-D',str(data),'-l',str(out/'server.log'),'-o',
                            f'-p {args.port} -h 127.0.0.1 -c shared_buffers=256MB -c maintenance_work_mem=256MB -c work_mem=16MB -c jit=off -c autovacuum=off -c track_io_timing=on','start'],check=True,stdout=subprocess.DEVNULL)
            sql('CREATE EXTENSION stannum; CREATE EXTENSION pg_visibility; CREATE TABLE documents(id text, body text, n bigserial, payload int DEFAULT 0) WITH(fillfactor=80,autovacuum_enabled=false);')
            lib = Path(subprocess.check_output(['pg_config','--pkglibdir'],text=True).strip())/('stannum.dylib' if platform.system() == 'Darwin' else 'stannum.so')
            meta['library_sha256'] = hashlib.sha256(lib.read_bytes()).hexdigest()
            expected = hashlib.sha256(args.release_library.read_bytes()).hexdigest()
            if meta['library_sha256'] != expected:
                raise ValueError('installed extension does not match the specified release artifact')
            meta['release_artifact'] = str(args.release_library.resolve())
            meta['server_version'] = sql('SHOW server_version')
            print('Loading existing Wikipedia subset',flush=True)
            with args.input.open('rb') as source:
                subprocess.run(['psql','-Xq','-v','ON_ERROR_STOP=1','-c','COPY documents(id,body) FROM STDIN WITH(FORMAT csv)'],stdin=source,env=env,check=True)
            assert int(sql('SELECT count(*) FROM documents')) == args.rows
            print('Building Stannum index',flush=True)
            sql('CREATE INDEX documents_idx ON documents USING stannum(body);')
            meta['settings'] = json.loads(sql('SELECT json_object_agg(name,setting) FROM pg_settings'))
            def capture():
                return json.loads(sql("SELECT json_build_object('rows',(SELECT count(*) FROM documents),'visibility',(SELECT row_to_json(v) FROM pg_visibility_map_summary('documents') v),'segments',(SELECT json_agg(s) FROM stannum.segment_info('documents_idx') s))"))
            for phase in ('vacuumed','vacuumed-repeat','mutated','revacuumed'):
                if phase in ('vacuumed','revacuumed'):
                    sql('VACUUM ANALYZE documents;')
                if phase == 'mutated':
                    sql("UPDATE documents SET payload=payload+1 WHERE n%20=0; UPDATE documents SET body=body || ' ' WHERE n%10=1; DELETE FROM documents WHERE n%101=0;")
                sql('CHECKPOINT;')
                meta['phases'].append(dict(name=phase, status='running', before=capture()))
                save()
                print('Starting '+phase,flush=True)
                with (out/(phase+'.log')).open('w') as log:
                    probe = 'count_crossover.py' if args.crossover_queries else 'count_probe.py'
                    command = [sys.executable,str(protocol/probe),
                               '--output',str(out/phase),'--repetitions','9']
                    if args.crossover_queries:
                        command += ['--queries',str(args.crossover_queries.resolve())]
                    subprocess.run(command,env=env,stdout=log,stderr=subprocess.STDOUT,check=True)
                meta['phases'][-1].update(status='complete',after=capture())
                save()
                print('Completed '+phase,flush=True)
            meta['status']='complete'
        except BaseException as error:
            meta.update(status='failed',error=repr(error))
            raise
        finally:
            save()
            subprocess.run(['pg_ctl','-D',str(data),'-m','immediate','stop'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)


if __name__ == '__main__':
    main()
