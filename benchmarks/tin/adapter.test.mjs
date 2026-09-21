// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {pathToFileURL} from 'node:url';
import {resolve} from 'node:path';
import test from 'node:test';

const driver = resolve(process.env.BENCHMARK_DRIVER || 'benchmarks/results/tin-driver');
const {buildRequest, selectQueries, selectBackends} = await import(pathToFileURL(`${driver}/benchmarks/queries.js`));
const trace = JSON.parse(readFileSync(`${driver}/datasets/wikipedia/queries.json`));

test('all published Stannum forms retain upstream ordering and bound parameters', () => {
  const entries = selectQueries(trace, 'mixed', 1592614637);
  assert.equal(entries.length, 906);
  assert.deepEqual(selectBackends('stannum,postgres', 'count', 'mixed', 10), ['stannum', 'postgres']);
  for (const entry of entries) {
    const [count, argument] = buildRequest('stannum', 'count', entry, 10);
    assert.equal(count, 'SELECT count(*) FROM documents WHERE body ==> $1');
    assert.equal(argument, entry.record.engines.tin[entry.style]);
    const [ranked, rankedArgument] = buildRequest('stannum', 'topk', entry, 10);
    assert.equal(rankedArgument, argument);
    assert.equal(ranked, 'SELECT id, body, stannum.score(ctid) AS score FROM documents WHERE body ==> $1 ORDER BY score DESC LIMIT 10');
    const [gin] = buildRequest('postgres', 'count', entry, 10);
    assert.equal(gin, "SELECT count(*) FROM documents WHERE body_tsv @@ to_tsquery('simple', $1)");
  }
});

test('conjunction-phrase retains both families and excludes disjunctions', () => {
  const entries = selectQueries(trace, 'conjunction-phrase', 1592614637);
  assert.equal(entries.length, trace.queries.length * 2);
  assert.deepEqual(new Set(entries.map(entry => entry.style)), new Set(['conjunction', 'phrase']));
  assert.equal(new Set(entries.map(entry => `${entry.record.source_id}:${entry.style}`)).size, entries.length);
  assert.deepEqual(entries, selectQueries(trace, 'conjunction-phrase', 1592614637));
  assert.deepEqual(selectBackends('stannum,postgres', 'topk', 'conjunction-phrase'), ['stannum', 'postgres']);
  for (const entry of entries) {
    assert.equal(buildRequest('stannum', 'topk', entry, 10)[1], entry.record.engines.tin[entry.style]);
    assert.equal(buildRequest('postgres', 'topk', entry, 10)[1], entry.record.engines.postgres[entry.style]);
  }
});
