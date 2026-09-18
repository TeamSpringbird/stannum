import unittest

from foreground_writes import analyze, classify, decode_stream, summarize


class AttributionTests(unittest.TestCase):
    def test_same_segment_count_can_hide_a_merge(self):
        before = [{'generation': 1, 'docs': 32}, {'generation': 2, 'docs': 32}]
        after = [{'generation': 2, 'docs': 32}, {'generation': 4, 'docs': 64}]
        result = classify(before, after)
        self.assertEqual(result['kind'], 'fold_and_merge')
        self.assertEqual(result['retired_existing_docs'], 32)
        self.assertEqual(result['new_generations'], [4])

    def test_buffer_append_and_fold_are_distinct(self):
        segment = [{'generation': 1, 'docs': 32}]
        self.assertEqual(classify(segment, segment)['kind'], 'buffered')
        self.assertEqual(classify([], segment)['kind'], 'fold')
        with self.assertRaises(ValueError):
            classify(segment, [])

    def test_json_stream_does_not_drop_truncated_results(self):
        self.assertEqual(decode_stream('[]\n[\n {"Plan": {}}\n]\n'), [[], [{'Plan': {}}]])
        with self.assertRaises(ValueError):
            decode_stream('[]\n[{')
        with self.assertRaises(ValueError):
            analyze([[]], 1)

    def test_correctness_failure_is_not_a_successful_measurement(self):
        plan = [{'Execution Time': 1.2, 'Plan': {'Node Type': 'ModifyTable',
                 'WAL Bytes': 10, 'Shared Hit Blocks': 2, 'Shared Read Blocks': 0}}]
        correct = dict(differences=0, heap_docs=1, index_docs=1, verify_errors=0)
        result = analyze([[], plan, [], correct], 1)
        self.assertEqual(result['summary']['buffered']['samples'], 1)
        self.assertIsNone(result['summary']['buffered']['p99_ms'])
        for key in correct:
            wrong = dict(correct, **{key: correct[key] + 1})
            with self.assertRaises(ValueError):
                analyze([[], plan, [], wrong], 1)

    def test_rare_spikes_are_not_hidden_by_buffered_percentiles(self):
        samples = [dict(id=i, kind='buffered', execution_ms=.1, wal_bytes=10)
                   for i in range(100)]
        samples.append(dict(id=101, kind='fold_and_merge', execution_ms=50, wal_bytes=9000))
        result = summarize(samples)
        self.assertEqual(result['buffered']['p99_ms'], .1)
        self.assertEqual(result['fold_and_merge']['max_ms'], 50)
        self.assertIsNone(result['fold_and_merge']['p99_ms'])
