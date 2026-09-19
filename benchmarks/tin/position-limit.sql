-- Copyright (C) 2026 Ben Weis <ben@springbird.app>
--
-- See LICENSE in the repository root for license terms.

-- Regression for a false oracle failure found by the published query trace.
-- PostgreSQL 18.6 drops late repeated-word positions. This is a semantic
-- difference, not a Stannum phrase-index failure. Use raw text for its oracle.
CREATE TEMP TABLE position_limit_probe(n integer, body text);
INSERT INTO position_limit_probe
SELECT n, repeat('the ', n) || 'movement'
FROM unnest(ARRAY[250, 255, 256, 257, 300]) n;
CREATE INDEX ON position_limit_probe USING stannum(body);
DO $$
DECLARE actual integer[]; reference integer[]; native integer[];
BEGIN
  PERFORM set_config('enable_seqscan', 'off', true);
  SELECT array_agg(n ORDER BY n) INTO actual
    FROM position_limit_probe WHERE body ==> '"the movement"';
  SELECT array_agg(n ORDER BY n) INTO reference
    FROM position_limit_probe WHERE body ~ '(^| )the movement( |$)';
  SELECT array_agg(n ORDER BY n) INTO native
    FROM position_limit_probe
    WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'the <-> movement');
  IF actual IS DISTINCT FROM ARRAY[250,255,256,257,300]
     OR actual IS DISTINCT FROM reference OR native IS DISTINCT FROM ARRAY[250,255] THEN
    RAISE EXCEPTION 'Unexpected position semantics: stannum %, lexical %, postgres %',
      actual, reference, native;
  END IF;
END $$;
DROP TABLE position_limit_probe;
