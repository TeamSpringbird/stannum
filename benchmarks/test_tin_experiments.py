# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import argparse
import json
from pathlib import Path
import tempfile
import types
import unittest
from unittest.mock import MagicMock, patch

import tin_experiments


class DatabaseError(Exception):
    sqlstate = 'XX000'


class ExperimentSafetyTests(unittest.TestCase):
    def args(self, root):
        return argparse.Namespace(output=root, rows=1000, minutes=1, repetitions=1,
                                  seconds=1, max_clients=1, skip_synthetic=False)

    def test_failure_redacts_error_and_cleans_only_owned_schema(self):
        for fail_create in (False, True):
            with tempfile.TemporaryDirectory() as tmp:
                conn = MagicMock()
                conn.__enter__.return_value = conn
                def execute(sql, *params):
                    if sql.startswith('CREATE SCHEMA') and fail_create:
                        raise DatabaseError('SECRET in connection details')
                    value = MagicMock()
                    value.fetchone.return_value = ({'extensions': {'tin': 'test'}},) if sql.startswith('SELECT json_build_object') else (0,)
                    value.fetchall.return_value = []
                    return value
                conn.execute.side_effect = execute
                driver = types.SimpleNamespace(connect=MagicMock(return_value=conn), Error=DatabaseError)
                with patch.dict('sys.modules', {'psycopg': driver}), patch.object(tin_experiments.Experiment, 'synthetic', side_effect=DatabaseError('SECRET')):
                    experiment = tin_experiments.Experiment(self.args(Path(tmp)/'output'))
                    with self.assertRaises(SystemExit):
                        experiment.run()
                content = (experiment.root/'experiment.json').read_text()
                self.assertNotIn('SECRET', content)
                self.assertEqual(json.loads(content)['status'], 'failed')
                drops = [call.args[0] for call in conn.execute.call_args_list if call.args[0].startswith('DROP SCHEMA')]
                self.assertEqual(drops, [] if fail_create else [f'DROP SCHEMA {experiment.schema} CASCADE'])

    def test_query_failure_restores_session_settings_and_retains_sqlstate(self):
        with tempfile.TemporaryDirectory() as tmp:
            driver = types.SimpleNamespace(Error=DatabaseError)
            with patch.dict('sys.modules', {'psycopg': driver}):
                experiment = tin_experiments.Experiment(self.args(Path(tmp)/'output'))
            conn = MagicMock()
            def execute(sql, *params):
                if sql.startswith('EXPLAIN'):
                    raise DatabaseError('SECRET')
                result = MagicMock()
                result.fetchone.return_value = ('4MB',)
                return result
            conn.execute.side_effect = execute
            result = experiment.probe(conn, 'failure', 'SELECT 1', {'work_mem':'2MB'})
            self.assertEqual(result['sqlstate'], 'XX000')
            self.assertEqual(conn.execute.call_args.args, ('SELECT set_config(%s,%s,false)', ('work_mem','4MB')))
            self.assertNotIn('SECRET', (experiment.root/'experiment.json').read_text())

    def test_invalid_limits_fail_before_connecting_or_creating_output(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(tin_experiments, 'Experiment') as experiment:
            args=self.args(Path(tmp)/'output')
            args.rows=1000001
            with self.assertRaises(ValueError):
                tin_experiments.run(args)
            experiment.assert_not_called()
            self.assertFalse(args.output.exists())
