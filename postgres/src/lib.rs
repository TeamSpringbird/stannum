// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

use pgrx::pg_guard;

::pgrx::pg_module_magic!(name);

mod am;
mod bm25;
mod customscan;
mod fold;
mod highlight;
mod highlight_udfs;
mod match_positions;
mod operator;
pub(crate) mod options;
mod score;
mod selectivity;
mod storage;
mod stream;
mod tf_bucket {
    pub(crate) use segment::tf_bucket::*;
}
mod udfs;

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    options::init();
    storage::init();
    storage::wal::init();
    operator::init();
    customscan::init();
}

#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec!["shared_preload_libraries=''"]
    }
}

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use pgrx::Json;
    use pgrx::prelude::*;

    #[pg_test]
    fn bitmap_index_rechecks_heap_pages_without_preloading() {
        assert_eq!(
            Spi::get_one::<String>("SHOW shared_preload_libraries").unwrap(),
            Some(String::new())
        );
        Spi::run("CREATE TABLE lite_search (id int, body text)").unwrap();
        Spi::run(
            "INSERT INTO lite_search VALUES
               (1, 'craft beer'), (2, 'wine'), (3, 'beer festival')",
        )
        .unwrap();
        Spi::run("CREATE INDEX lite_search_idx ON lite_search USING stannum (body)").unwrap();
        Spi::run("SET LOCAL enable_seqscan = off; SET LOCAL stannum.enable_custom_scan = off")
            .unwrap();
        let ids = Spi::get_one::<Vec<i32>>(
            "SELECT array_agg(id ORDER BY id) FROM lite_search WHERE body ==> 'beer'",
        )
        .unwrap();
        assert_eq!(ids, Some(vec![1, 3]));
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON)
             SELECT id FROM lite_search WHERE body ==> 'beer'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(plan[0]["Plan"]["Node Type"], "Bitmap Heap Scan");
        assert_eq!(plan[0]["Plan"]["Lossy Heap Blocks"], 0);
        assert_eq!(plan[0]["Plan"]["Plans"][0]["Index Name"], "lite_search_idx");
    }

    #[pg_test]
    fn a_build_packs_its_segments_into_its_lowest_pages() {
        // Small build segments and a low merge cap: the build's tier merges
        // retire many runs and end with several segments among their holes.
        Spi::run(
            "CREATE TABLE packed(id int, body text);
             INSERT INTO packed SELECT n, 'common ' || (SELECT string_agg('w' || (n * k % 1009), ' ')
             FROM generate_series(1, 40) k) FROM generate_series(1, 20000) n;
             SET LOCAL stannum.build_segment_docs = 500;
             CREATE INDEX packed_idx ON packed USING stannum(body)
             WITH (max_merged_segment_size = 2);",
        )
        .unwrap();
        let index = unsafe {
            pgrx::PgRelation::with_lock(
                Spi::get_one::<pg_sys::Oid>("SELECT 'packed_idx'::regclass::oid")
                    .unwrap()
                    .unwrap(),
                pg_sys::AccessShareLock as _,
            )
        };
        let segments = unsafe { crate::storage::testing::segment_pages(index.as_ptr()) };
        assert!(segments.len() >= 2, "{} segments", segments.len());
        let live: i64 = segments
            .iter()
            .map(|(run, map)| (run.len() + map.len()) as i64)
            .sum();
        // Meta page, the write buffer's page, and the runs: nothing else.
        assert_eq!(
            value("SELECT pg_relation_size('packed_idx') / 8192"),
            live + 2
        );
        // And the free space map lists none of them: a reused page left
        // there is a page every later allocation reads under the meta lock.
        Spi::run("CREATE EXTENSION IF NOT EXISTS pg_freespacemap").unwrap();
        assert_eq!(
            value("SELECT count(*) FROM pg_freespace('packed_idx') WHERE avail > 0"),
            0
        );
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
        assert_eq!(
            value("SELECT count(*) FROM packed WHERE body ==> 'common'"),
            20000
        );
        assert_clean("packed_idx");
    }

    #[pg_test]
    fn selective_postings_skip_unrelated_heap_pages_and_follow_overflow() {
        Spi::run(
            "CREATE TABLE posting_probe (id int, body text);
          INSERT INTO posting_probe SELECT n, 'common ' || repeat('filler ', 120) ||
            CASE WHEN n=777 THEN 'needle' ELSE '' END FROM generate_series(1,1500) n;
          CREATE INDEX posting_probe_idx ON posting_probe USING stannum(body);
          SET LOCAL enable_seqscan=off; SET LOCAL stannum.enable_custom_scan=off;",
        )
        .unwrap();
        // Meta page, one write-buffer page, and at least one segment page.
        assert!(
            Spi::get_one::<i64>("SELECT pg_relation_size('posting_probe_idx')")
                .unwrap()
                .unwrap()
                >= 3 * 8192
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM posting_probe WHERE body ==> 'common'")
                .unwrap(),
            Some(1500)
        );
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM posting_probe WHERE body ==> 'needle'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(plan[0]["Plan"]["Exact Heap Blocks"], 1);
        assert_eq!(plan[0]["Plan"]["Lossy Heap Blocks"], 0);
        let miss = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM posting_probe WHERE body ==> 'missing'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(miss[0]["Plan"]["Exact Heap Blocks"], 0);
        Spi::run("INSERT INTO posting_probe VALUES (1501,'needle');").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM posting_probe WHERE body ==> 'needle'")
                .unwrap(),
            Some(2)
        );
    }

    #[pg_test]
    fn persisted_boolean_and_phrase_candidates_preserve_exact_matches() {
        Spi::run(
            "CREATE TABLE boolean_docs(id int, body text);
             INSERT INTO boolean_docs VALUES (1,'beer wine'), (2,'wine beer'),
                 (3,'beer craft'), (4,'wine'), (5,'beer beer'), (6,'cider');
             INSERT INTO boolean_docs SELECT n, repeat('padding ',120)
                 FROM generate_series(7,1000) n;
             CREATE INDEX boolean_docs_search ON boolean_docs USING stannum(body);
             SET LOCAL stannum.enable_custom_scan = off;",
        )
        .unwrap();
        for (query, expected) in [
            ("beer AND wine", vec![1, 2]),
            ("beer OR wine", vec![1, 2, 3, 4, 5]),
            ("\"beer wine\"", vec![1]),
            ("\"beer beer\"", vec![5]),
            ("beer AND NOT wine", vec![3, 5]),
            ("beer OR win*", vec![1, 2, 3, 4, 5]),
            ("missing OR win*", vec![1, 2, 4]),
            ("beer AND win*", vec![1, 2]),
            ("missing AND wine", vec![]),
            ("(beer OR wine) AND craft", vec![3]),
            ("beer NOT ENCLOSES wine", vec![1, 2, 3, 5]),
            ("beer NOT ENCLOSED BY wine", vec![1, 2, 3, 5]),
            ("beer NOT OVERLAPPING wine", vec![1, 2, 3, 5]),
            ("beer BEFORE wine", vec![1]),
            ("beer AFTER wine", vec![2]),
            ("beer THEN/1 win*", vec![1]),
            ("AT LEAST 2 OF [beer, wine, craft]", vec![1, 2, 3]),
        ] {
            Spi::run("SET LOCAL enable_seqscan=off; SET LOCAL enable_bitmapscan=on;").unwrap();
            let actual = Spi::get_one::<Vec<i32>>(&format!(
                "SELECT coalesce(array_agg(id ORDER BY id),'{{}}'::int[]) \
                 FROM boolean_docs WHERE body ==> '{query}'"
            ))
            .unwrap()
            .unwrap();
            assert_eq!(actual, expected, "{query}");
            Spi::run("SET LOCAL enable_seqscan=on; SET LOCAL enable_bitmapscan=off;").unwrap();
            let reference = Spi::get_one::<Vec<i32>>(&format!(
                "SELECT coalesce(array_agg(id ORDER BY id),'{{}}'::int[]) \
                 FROM boolean_docs WHERE body ==> '{query}'"
            ))
            .unwrap()
            .unwrap();
            assert_eq!(actual, reference, "reference disagreement: {query}");
        }
        Spi::run("SET LOCAL enable_seqscan=off; SET LOCAL enable_bitmapscan=on;").unwrap();
        let phrase = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM boolean_docs WHERE body ==> '\"beer wine\"'",
        ).unwrap().unwrap().0;
        assert_eq!(phrase[0]["Plan"]["Node Type"], "Bitmap Heap Scan");
        assert_eq!(phrase[0]["Plan"]["Actual Rows"].as_f64(), Some(1.0));
        // Positions are stored, so the phrase is exact: no candidate is rechecked.
        assert_eq!(
            phrase[0]["Plan"]["Rows Removed by Index Recheck"].as_f64(),
            Some(0.0)
        );
        assert_eq!(phrase[0]["Plan"]["Lossy Heap Blocks"], 0);
        let expansion = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM boolean_docs WHERE body ==> 'missing OR win*'",
        ).unwrap().unwrap().0;
        assert_eq!(expansion[0]["Plan"]["Lossy Heap Blocks"], 0);
        assert_eq!(expansion[0]["Plan"]["Actual Rows"].as_f64(), Some(3.0));
        assert_eq!(
            expansion[0]["Plan"]["Rows Removed by Index Recheck"].as_f64(),
            Some(0.0)
        );

        // The inner bitmap scan is rescanned with each outer row's query value.
        Spi::run("SET LOCAL enable_material=off; SET LOCAL enable_memoize=off;").unwrap();
        let counts = Spi::get_one::<Vec<i64>>(
            "SELECT array_agg(found.n ORDER BY q.ordinal) FROM
             (VALUES (1,'beer AND wine'), (2,'beer OR wine'),
                     (3,'\"beer wine\"'), (4,'missing'), (5,'missing OR win*')) q(ordinal,query)
             CROSS JOIN LATERAL (SELECT count(*) n FROM boolean_docs
                 WHERE body ==> q.query OFFSET 0) found",
        )
        .unwrap()
        .unwrap();
        assert_eq!(counts, vec![2, 5, 1, 0, 3]);
    }

    #[pg_test]
    fn persisted_boolean_candidates_recheck_lossy_bitmaps() {
        Spi::run(
            "CREATE TABLE lossy_docs(id int, body text);
             ALTER TABLE lossy_docs ALTER COLUMN body SET STORAGE PLAIN;
             INSERT INTO lossy_docs SELECT n,
               CASE WHEN n%4=0 THEN 'beer wine '
                    WHEN n%4=1 THEN 'wine beer '
                    WHEN n%4=2 THEN 'beer craft ' ELSE 'wine craft ' END
               || CASE WHEN n=1500 THEN 'needle ' ELSE '' END
               || repeat('padding ',500) FROM generate_series(1,3000) n;
             CREATE INDEX lossy_docs_search ON lossy_docs USING stannum(body);
             SET LOCAL work_mem='64kB'; SET LOCAL enable_seqscan=off; SET LOCAL stannum.enable_custom_scan=off;",
        )
        .unwrap();
        // The 3.5KB inline documents create enough heap pages to force lossiness
        // in both input bitmaps under the shared work_mem target.
        for (query, expected) in [
            ("beer AND wine", 1500_i64),
            ("beer OR wine", 3000),
            ("\"beer wine\"", 750),
            ("(beer OR craft) AND wine", 2250),
        ] {
            let plan = Spi::get_one::<Json>(&format!(
                "EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM lossy_docs WHERE body ==> '{query}'"
            ))
            .unwrap()
            .unwrap()
            .0;
            assert_eq!(plan[0]["Plan"]["Node Type"], "Bitmap Heap Scan");
            assert_eq!(
                plan[0]["Plan"]["Actual Rows"].as_f64(),
                Some(expected as f64),
                "{query}"
            );
            // Exact results with at least 1,500 tuples exceed the 64kB bitmap
            // budget; smaller exact results may stay exact.
            if expected >= 1500 {
                assert!(
                    plan[0]["Plan"]["Lossy Heap Blocks"].as_u64().unwrap() > 0,
                    "{query}"
                );
            }
            let actual = Spi::get_one::<Vec<i32>>(&format!(
                "SELECT array_agg(id ORDER BY id) FROM lossy_docs WHERE body ==> '{query}'"
            ))
            .unwrap()
            .unwrap();
            Spi::run("SET LOCAL enable_seqscan=on; SET LOCAL enable_bitmapscan=off;").unwrap();
            let reference = Spi::get_one::<Vec<i32>>(&format!(
                "SELECT array_agg(id ORDER BY id) FROM lossy_docs WHERE body ==> '{query}'"
            ))
            .unwrap()
            .unwrap();
            assert_eq!(actual, reference, "lossy reference disagreement: {query}");
            Spi::run("SET LOCAL enable_seqscan=off; SET LOCAL enable_bitmapscan=on;").unwrap();
        }
        // Intersection must remain conservative with exact/lossy operands in
        // either order; the rare posting list stays exact at this work_mem.
        for query in ["beer AND needle", "needle AND beer"] {
            assert_eq!(
                Spi::get_one::<Vec<i32>>(&format!(
                    "SELECT array_agg(id ORDER BY id) FROM lossy_docs WHERE body ==> '{query}'"
                ))
                .unwrap(),
                Some(vec![1500])
            );
        }
    }

    #[pg_test]
    fn write_buffer_folds_and_merges_keep_results_exact() {
        Spi::run(
            "CREATE TABLE folded(id int, body text);
             CREATE INDEX folded_idx ON folded USING stannum(body);
             SET LOCAL stannum.enable_custom_scan = off;
             SET LOCAL stannum.write_buffer_docs = 4;
             SET LOCAL stannum.max_segments = 3;
             INSERT INTO folded
               SELECT n, 'w' || (n % 7) || ' common ' ||
                      CASE WHEN n % 10 = 0 THEN 'rare needle' ELSE 'other filler' END
               FROM generate_series(1, 100) n;
             INSERT INTO folded VALUES (101, ''), (102, NULL), (103, 'w1 w1 w1');",
        )
        .unwrap();
        for query in [
            "rare",
            "common",
            "missing",
            "\"rare needle\"",
            "\"needle rare\"",
            "w1 AND NOT rare",
            "* AND NOT common",
            "w* AND rare",
            "AT LEAST 2 OF [w1 w2 rare]",
            "(w3 NEAR/2 needle) IN FIRST 3 WORDS",
            "common IN LAST 50%",
        ] {
            Spi::run("SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = on;").unwrap();
            let plan = Spi::get_one::<Json>(&format!(
                "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM folded WHERE body ==> '{query}'"
            ))
            .unwrap()
            .unwrap()
            .0;
            assert_eq!(plan[0]["Plan"]["Node Type"], "Bitmap Heap Scan", "{query}");
            assert_eq!(
                plan[0]["Plan"]["Rows Removed by Index Recheck"].as_f64(),
                Some(0.0),
                "{query}"
            );
            let actual = Spi::get_one::<Vec<i32>>(&format!(
                "SELECT coalesce(array_agg(id ORDER BY id), '{{}}'::int[]) FROM folded WHERE body ==> '{query}'"
            ))
            .unwrap()
            .unwrap();
            Spi::run("SET LOCAL enable_seqscan = on; SET LOCAL enable_bitmapscan = off;").unwrap();
            let reference = Spi::get_one::<Vec<i32>>(&format!(
                "SELECT coalesce(array_agg(id ORDER BY id), '{{}}'::int[]) FROM folded WHERE body ==> '{query}'"
            ))
            .unwrap()
            .unwrap();
            assert_eq!(actual, reference, "{query}");
        }
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM folded WHERE body ==> 'rare'").unwrap(),
            Some(10)
        );
    }

    /// Every query must return the same ids through the bitmap index path
    /// as through a sequential scan, with no rows removed by recheck.
    fn assert_index_matches_seqscan(table: &str, queries: &[&str]) {
        for query in queries {
            Spi::run("SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = on;").unwrap();
            let plan = Spi::get_one::<Json>(&format!(
                "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM {table} WHERE body ==> '{query}'"
            ))
            .unwrap()
            .unwrap()
            .0;
            assert_eq!(plan[0]["Plan"]["Node Type"], "Bitmap Heap Scan", "{query}");
            assert_eq!(
                plan[0]["Plan"]["Rows Removed by Index Recheck"].as_f64(),
                Some(0.0),
                "{query}"
            );
            let actual = Spi::get_one::<Vec<i32>>(&format!(
                "SELECT coalesce(array_agg(id ORDER BY id), '{{}}'::int[]) FROM {table} WHERE body ==> '{query}'"
            ))
            .unwrap()
            .unwrap();
            Spi::run("SET LOCAL enable_seqscan = on; SET LOCAL enable_bitmapscan = off;").unwrap();
            let reference = Spi::get_one::<Vec<i32>>(&format!(
                "SELECT coalesce(array_agg(id ORDER BY id), '{{}}'::int[]) FROM {table} WHERE body ==> '{query}'"
            ))
            .unwrap()
            .unwrap();
            assert_eq!(actual, reference, "{query}");
        }
    }

    /// (immutable segment count, distinct generations, documents) of an index.
    fn directory_shape(index: &str) -> (i64, i64, i64) {
        Spi::get_three::<i64, i64, i64>(&format!(
            "SELECT count(*), count(DISTINCT generation), coalesce(sum(docs), 0)::bigint
             FROM stannum.segment_info('{index}') WHERE kind = 'immutable'"
        ))
        .map(|(a, b, c)| (a.unwrap(), b.unwrap(), c.unwrap()))
        .unwrap()
    }

    const TIERED_QUERIES: &[&str] = &[
        "rare",
        "common",
        "missing",
        "\"rare needle\"",
        "\"needle rare\"",
        "w1 AND NOT rare",
        "* AND NOT common",
        "w* AND rare",
        "updated",
        "AT LEAST 2 OF [w1 w2 rare]",
        "(w3 NEAR/2 needle) IN FIRST 3 WORDS",
        "common IN LAST 50%",
    ];

    #[pg_test]
    fn tiered_merges_keep_results_exact_across_deletes_and_updates() {
        // Two-document folds and a tier factor of two drive a merge on nearly
        // every fold; a directory limit of six forces the smallest-entries
        // fallback as well. Deleted rows stay in segments (VACUUM cannot run
        // in a test transaction), so merges carry dead documents along.
        Spi::run(
            "CREATE TABLE tiered(id int, body text);
             CREATE INDEX tiered_idx ON tiered USING stannum(body);
             SET LOCAL stannum.enable_custom_scan = off;
             SET LOCAL stannum.write_buffer_docs = 2;
             SET LOCAL stannum.merge_tier_factor = 2;
             SET LOCAL stannum.max_segments = 6;
             INSERT INTO tiered
               SELECT n, 'w' || (n % 7) || ' common ' ||
                      CASE WHEN n % 10 = 0 THEN 'rare needle' ELSE 'other filler' END
               FROM generate_series(1, 200) n;",
        )
        .unwrap();
        assert_index_matches_seqscan("tiered", TIERED_QUERIES);
        let (segments, generations, docs) = directory_shape("tiered_idx");
        assert!((2..=6).contains(&segments), "{segments} segments");
        assert_eq!(generations, segments);
        assert_eq!(docs, 198, "one two-document buffer is still unfolded");
        Spi::run(
            "DELETE FROM tiered WHERE id % 3 = 0;
             INSERT INTO tiered
               SELECT n, 'w' || (n % 7) || ' common ' ||
                      CASE WHEN n % 10 = 0 THEN 'rare needle' ELSE 'other filler' END
               FROM generate_series(201, 300) n;
             UPDATE tiered SET body = body || ' updated' WHERE id % 11 = 0;
             INSERT INTO tiered VALUES (301, NULL), (302, 'w1 w1 w1');",
        )
        .unwrap();
        assert_index_matches_seqscan("tiered", TIERED_QUERIES);
        let (segments, generations, docs) = directory_shape("tiered_idx");
        assert!((2..=6).contains(&segments), "{segments} segments");
        assert_eq!(generations, segments);
        let updated = Spi::get_one::<i64>("SELECT count(*) FROM tiered WHERE id % 11 = 0")
            .unwrap()
            .unwrap();
        // Dead versions stay in segments until VACUUM; nulls are not indexed.
        let buffered = Spi::get_one::<i64>(
            "SELECT coalesce(sum(docs), 0)::bigint FROM stannum.segment_info('tiered_idx') WHERE kind = 'mutable'",
        )
        .unwrap()
        .unwrap();
        assert_eq!(docs + buffered, 300 + updated + 1);
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM tiered WHERE body ==> 'rare'").unwrap(),
            Some(24)
        );
    }

    #[pg_test]
    fn merge_tier_factor_bounds_the_directory_logarithmically() {
        // One-document folds: after tiered merges the directory holds one
        // segment per base-four digit of the folded document count, never
        // more than three per tier, and never everything in one segment.
        Spi::run(
            "CREATE TABLE lsm(id int, body text);
             CREATE INDEX lsm_idx ON lsm USING stannum(body);
             SET LOCAL stannum.enable_custom_scan = off;
             SET LOCAL stannum.write_buffer_docs = 1;
             SET LOCAL stannum.merge_tier_factor = 4;
             INSERT INTO lsm
               SELECT n, 'w' || (n % 7) || ' common ' ||
                      CASE WHEN n % 10 = 0 THEN 'rare needle' ELSE 'other filler' END
               FROM generate_series(1, 500) n;",
        )
        .unwrap();
        let (segments, generations, docs) = directory_shape("lsm_idx");
        assert_eq!(docs, 499, "the last document is still buffered");
        let mut digits = 0;
        let mut rest = docs;
        while rest > 0 {
            digits += rest % 4;
            rest /= 4;
        }
        assert_eq!(segments, digits, "499 = 13303 in base four");
        assert_eq!(generations, segments);
        let per_tier = Spi::get_one::<i64>(
            "SELECT max(n) FROM (
               SELECT count(*) AS n FROM stannum.segment_info('lsm_idx')
               WHERE kind = 'immutable' GROUP BY floor(ln(docs) / ln(4) + 1e-9)
             ) tiers",
        )
        .unwrap()
        .unwrap();
        assert!(per_tier <= 3, "{per_tier} segments in one tier");
        assert_index_matches_seqscan("lsm", TIERED_QUERIES);
        // Lowering the directory bound below the tier layout merges the
        // smallest entries on the next fold; results stay exact.
        Spi::run(
            "SET LOCAL stannum.max_segments = 3;
             INSERT INTO lsm VALUES (501, 'w1 rare needle'), (502, 'w2 common');",
        )
        .unwrap();
        let (segments, generations, docs) = directory_shape("lsm_idx");
        assert!(segments <= 3, "{segments} segments");
        assert_eq!(generations, segments);
        assert_eq!(docs, 501);
        assert_index_matches_seqscan("lsm", TIERED_QUERIES);
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM lsm WHERE body ==> 'rare'").unwrap(),
            Some(51)
        );
    }

    #[pg_test]
    fn index_tokenizer_options_govern_matching() {
        Spi::run(
            "CREATE TABLE cased(id int, body text);
             INSERT INTO cased VALUES (1, 'Beer'), (2, 'beer'), (3, 'BEER');
             CREATE INDEX cased_idx ON cased USING stannum(body) WITH (case_folding = preserve);
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        // Exact index results honor the index's own analyzer settings.
        assert_eq!(
            Spi::get_one::<Vec<i32>>(
                "SELECT array_agg(id ORDER BY id) FROM cased WHERE body ==> 'beer'"
            )
            .unwrap(),
            Some(vec![2])
        );
        assert_eq!(
            Spi::get_one::<Vec<i32>>(
                "SELECT array_agg(id ORDER BY id) FROM cased WHERE body ==> 'Beer'"
            )
            .unwrap(),
            Some(vec![1])
        );
    }

    /// Values observed from TIN 1.0.2 on the same documents.
    #[pg_test]
    fn scoring_matches_tin_statistics_contract_bit_for_bit() {
        Spi::run(
            "CREATE TABLE parity(id int primary key, body text);
             INSERT INTO parity VALUES (1,'rare common'), (2,'common common'),
               (3,'common'), (4,'rare rare common x'), (5,'other');
             CREATE INDEX parity_idx ON parity USING stannum(body);",
        )
        .unwrap();
        let scores = |label: &str| -> Vec<(i32, u32)> {
            let rows = Spi::connect(|client| {
                client
                    .select(
                        "SELECT id, stannum.full_score(ctid) FROM parity
                         WHERE body ==> 'rare OR common' ORDER BY id",
                        None,
                        &[],
                    )
                    .unwrap()
                    .map(|row| {
                        (
                            row.get::<i32>(1).unwrap().unwrap(),
                            row.get::<f32>(2).unwrap().unwrap().to_bits(),
                        )
                    })
                    .collect::<Vec<_>>()
            });
            eprintln!("{label}: {rows:?}");
            rows
        };
        let bits = |value: f32| value.to_bits();
        assert_eq!(
            scores("all live"),
            vec![
                (1, bits(1.163_150_8)),
                (2, bits(0.395_562_86)),
                (3, bits(0.361_657_47)),
                (4, bits(1.143_688_9))
            ]
        );
        // Deleted documents stay in the statistics until their segment is rewritten.
        Spi::run("DELETE FROM parity WHERE id IN (2, 3)").unwrap();
        assert_eq!(
            scores("two deleted"),
            vec![(1, bits(1.163_150_8)), (4, bits(1.143_688_9))]
        );
        // Buffered documents count immediately, alongside the dead ones.
        Spi::run("INSERT INTO parity VALUES (6,'common common common'), (7,'rare')").unwrap();
        assert_eq!(
            scores("two buffered"),
            vec![
                (1, bits(1.201_372)),
                (4, bits(1.153_078_8)),
                (6, bits(0.531_823)),
                (7, bits(1.039_253_1))
            ]
        );
        // A rebuild re-indexes rows deleted by this still-open transaction, as
        // every index AM must, so inside one transaction the statistics keep
        // seven documents. TIN observed after a committed delete and VACUUM
        // gave 1.1196322, 1.0063113, 0.78576607, 0.6938147 for five.
        Spi::run("REINDEX INDEX parity_idx").unwrap();
        assert_eq!(
            scores("reindexed in transaction"),
            vec![
                (1, bits(1.201_372)),
                (4, bits(1.153_078_8)),
                (6, bits(0.531_823)),
                (7, bits(1.039_253_1))
            ]
        );
        // Dense-term elision from a single immutable segment.
        Spi::run(
            "CREATE TABLE dense(id int primary key, body text);
             INSERT INTO dense SELECT n, CASE WHEN n <= 3 THEN 'rare common' ELSE 'common filler' END
               FROM generate_series(1, 30) n;
             CREATE INDEX dense_idx ON dense USING stannum(body);",
        )
        .unwrap();
        let dense = Spi::get_one::<Vec<f32>>(
            "SELECT array_agg(stannum.score(ctid) ORDER BY id) FROM dense
             WHERE body ==> 'rare OR common' AND id <= 5",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            dense.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            [2.181_224_3_f32, 2.181_224_3, 2.181_224_3, 0.0, 0.0]
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>()
        );
    }

    /// Values observed from TIN 1.0.2 on the same documents.
    #[pg_test]
    fn scoring_terms_expansions_not_and_max_match_tin() {
        Spi::run(
            "CREATE TABLE mx(id int primary key, body text);
             INSERT INTO mx VALUES (1,'a'), (2,'a a'), (3,'a b c d'), (4,'a a a b'), (5,'b'),
               (6,'c c c c c c'), (7,'rare'), (8,'rate'), (9,'rave'), (10,'x y z');
             CREATE INDEX mx_idx ON mx USING stannum(body);",
        )
        .unwrap();
        let bits = |sql: &str| -> Vec<u32> {
            Spi::get_one::<Vec<f32>>(sql)
                .unwrap()
                .unwrap()
                .into_iter()
                .map(f32::to_bits)
                .collect()
        };
        assert_eq!(
            bits(
                "SELECT array_agg(stannum.full_score(ctid) ORDER BY id) FROM mx WHERE body ==> 'a'"
            ),
            [0x3f96_44a5, 0x3fa5_0c72, 0x3f33_c8fc, 0x3f9d_4fdb]
        );
        // Standalone max_score: full policy, maximum over matching rows.
        assert_eq!(
            bits(
                "SELECT array_agg(m) FROM (SELECT stannum.max_score(ctid) m FROM mx WHERE body ==> 'a' LIMIT 1) s"
            ),
            [0x3fa5_0c72]
        );
        assert_eq!(
            bits(
                "SELECT array_agg(m) FROM (SELECT stannum.max_score(ctid) m FROM mx WHERE body ==> 'a OR b' LIMIT 1) s"
            ),
            [0x4008_3d61]
        );
        assert_eq!(
            bits(
                "SELECT array_agg(m) FROM (SELECT stannum.max_score(ctid) m FROM mx WHERE body ==> 'c' LIMIT 1) s"
            ),
            [0x400a_26fb]
        );
        // Beside stannum.score in the same target list it adapts to the dense
        // policy (a is in 4 of 9 documents, so it is elided and scores zero).
        assert_eq!(
            bits(
                "SELECT array_agg(m) FROM (SELECT stannum.max_score(ctid) + 0::real * stannum.score(ctid) AS m FROM mx WHERE body ==> 'a' LIMIT 1) t"
            ),
            [0x0000_0000]
        );
        // Fuzzy and wildcard expansions score every matching dictionary term.
        assert_eq!(
            bits(
                "SELECT array_agg(stannum.full_score(ctid) ORDER BY id) FROM mx WHERE body ==> 'rare~1'"
            ),
            [0x4027_7bac, 0x4027_7bac, 0x4027_7bac]
        );
        assert_eq!(
            bits(
                "SELECT array_agg(stannum.full_score(ctid) ORDER BY id) FROM mx WHERE body ==> 'ra*'"
            ),
            [0x4027_7bac, 0x4027_7bac, 0x4027_7bac]
        );
        let inspect = |query: &str| -> Vec<String> {
            Spi::get_one::<Vec<String>>(&format!(
                "SELECT array_agg(term || ':' || weight ORDER BY term) FROM stannum.score_inspect('mx_idx', '{query}', 1.0)"
            ))
            .unwrap()
            .unwrap_or_default()
        };
        assert_eq!(inspect("rare~1"), ["rare:1", "rate:1", "rave:1"]);
        assert_eq!(inspect("ra*^2"), ["rare:2", "rate:2", "rave:2"]);
        assert_eq!(inspect("MATCHES r.*e"), ["rare:1", "rate:1", "rave:1"]);
        assert_eq!(inspect("x TO z"), ["x:1", "y:1", "z:1"]);
        assert_eq!(inspect("a AND NOT (b OR c)"), ["a:1"]);
        assert_eq!(inspect("a OR (b AND NOT c)"), ["a:1", "b:1"]);
        assert_eq!(inspect("* AND NOT c"), Vec::<String>::new());
        assert_eq!(inspect("a NOT OVERLAPPING b"), ["a:1", "b:1"]);
    }

    #[pg_test]
    fn custom_scan_search_count_and_topk_match_the_bitmap_path() {
        Spi::run(
            "CREATE TABLE cs(id int primary key, body text, active bool DEFAULT true);
             INSERT INTO cs SELECT n, 'common w' || (n % 7) || ' ' ||
               CASE WHEN n % 100 = 0 THEN 'rare alpha beta' ELSE 'filler' END
               FROM generate_series(1, 3000) n;
             CREATE INDEX cs_idx ON cs USING stannum(body);
             CREATE INDEX cs_partial ON cs USING stannum(lower(body)) WHERE active;
             UPDATE cs SET active = false WHERE id = 300;
             DELETE FROM cs WHERE id % 500 = 0;",
        )
        .unwrap();
        let queries = [
            "rare",
            "missing",
            "common AND NOT rare",
            "\"alpha beta\"",
            "ra* OR filler",
            "common OR rare",
        ];
        for query in queries {
            let both = |custom: bool| -> (Vec<i32>, i64, Vec<i32>) {
                Spi::run(&format!(
                    "SET LOCAL stannum.enable_custom_scan = {custom}; SET LOCAL enable_seqscan = off;"
                ))
                .unwrap();
                let ids = Spi::get_one::<Vec<i32>>(&format!(
                    "SELECT coalesce(array_agg(id ORDER BY id), '{{}}') FROM cs WHERE body ==> '{query}' AND id % 3 = 0"
                ))
                .unwrap()
                .unwrap();
                let count = Spi::get_one::<i64>(&format!(
                    "SELECT count(*) FROM cs WHERE body ==> '{query}'"
                ))
                .unwrap()
                .unwrap();
                let top = Spi::get_one::<Vec<i32>>(&format!(
                    "SELECT coalesce(array_agg(id), '{{}}') FROM (SELECT id FROM cs WHERE body ==> '{query}'
                     ORDER BY stannum.full_score(ctid) DESC, id LIMIT 5) t"
                ))
                .unwrap()
                .unwrap();
                (ids, count, top)
            };
            assert_eq!(both(true), both(false), "{query}");
        }
        Spi::run("SET LOCAL stannum.enable_custom_scan = on; SET LOCAL enable_seqscan = off;")
            .unwrap();
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT count(*) FROM cs WHERE body ==> 'rare'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(plan[0]["Plan"]["Custom Plan Provider"], "Stannum Count");
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM cs WHERE body ==> 'common OR rare'
             ORDER BY stannum.full_score(ctid) DESC LIMIT 2",
        )
        .unwrap()
        .unwrap()
        .0;
        let scan = &plan[0]["Plan"]["Plans"][0]["Plans"][0];
        assert_eq!(scan["Custom Plan Provider"], "Stannum Text Search Scan");
        assert_eq!(scan["Order"], "score DESC");
        assert_eq!(scan["Heap Fetches"], 2);
        // A partial index answers only queries that imply its predicate; the
        // inactive row must still be found through the full index.
        assert_eq!(
            Spi::get_one::<Vec<i32>>(
                "SELECT array_agg(id ORDER BY id) FROM cs WHERE lower(body) ==> 'rare' AND id <= 400"
            )
            .unwrap(),
            Some(vec![100, 200, 300, 400])
        );
        assert_eq!(
            Spi::get_one::<Vec<i32>>(
                "SELECT array_agg(id ORDER BY id) FROM cs WHERE active AND lower(body) ==> 'rare' AND id <= 400"
            )
            .unwrap(),
            Some(vec![100, 200, 400])
        );
        // A join above the ordered scan can consume more rows than the LIMIT
        // it was planned for; the rows past the top-k are ordered on demand.
        Spi::run(
            "CREATE TABLE cs_keep(id int primary key);
             INSERT INTO cs_keep SELECT n FROM generate_series(2500, 3000) n;",
        )
        .unwrap();
        let joined = |custom: bool| -> Vec<i32> {
            Spi::run(&format!(
                "SET LOCAL stannum.enable_custom_scan = {custom}; SET LOCAL enable_seqscan = off;
                 SET LOCAL enable_sort = off; SET LOCAL enable_hashjoin = off;
                 SET LOCAL enable_mergejoin = off;"
            ))
            .unwrap();
            Spi::get_one::<Vec<i32>>(
                "SELECT array_agg(id ORDER BY id) FROM (SELECT d.id FROM cs d JOIN cs_keep k USING (id)
                 WHERE d.body ==> 'common OR rare' ORDER BY stannum.full_score(d.ctid) DESC LIMIT 4) t",
            )
            .unwrap()
            .unwrap()
        };
        // The four surviving 'rare' rows tie on score; the plain sort breaks
        // ties arbitrarily, so the comparison is by set.
        let with_custom = joined(true);
        assert_eq!(with_custom, vec![2600, 2700, 2800, 2900]);
        assert_eq!(with_custom, joined(false));
        Spi::run("SET LOCAL stannum.enable_custom_scan = on;").unwrap();
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT d.id FROM cs d JOIN cs_keep k USING (id)
             WHERE d.body ==> 'common OR rare' ORDER BY stannum.full_score(d.ctid) DESC LIMIT 4",
        )
        .unwrap()
        .unwrap()
        .0;
        let text = plan.to_string();
        assert!(text.contains("Stannum Text Search Scan"), "{text}");
        assert!(text.contains("\"Top K\":4"), "{text}");
    }

    #[pg_test]
    fn prepared_ranked_queries_use_custom_and_generic_ranked_plans() {
        Spi::run(
            "CREATE TABLE prepared_rank(id int, body text);
             INSERT INTO prepared_rank SELECT n, repeat('common ', n % 7 + 1) ||
               CASE WHEN n % 10 = 0 THEN 'rare alpha' ELSE 'filler beta' END
               FROM generate_series(1, 1000) n;
             CREATE INDEX prepared_rank_idx ON prepared_rank USING stannum(body);
             ANALYZE prepared_rank;",
        )
        .unwrap();
        for scorer in [
            "stannum.full_score(ctid)",
            "stannum.score(ctid)",
            "stannum.score(ctid, k1 => 1.7, b => 0.6)",
        ] {
            for mode in ["force_custom_plan", "auto", "force_generic_plan"] {
                Spi::run(&format!(
                    "SET LOCAL plan_cache_mode = {mode};
             PREPARE ranked_parameter(text) AS
               SELECT id, body, {scorer} AS score FROM prepared_rank
               WHERE body ==> $1 ORDER BY score DESC LIMIT 10;"
                ))
                .unwrap();
                // Reuse the prepared statement beyond PostgreSQL's first five custom plans.
                for _ in 0..3 {
                    for query in [
                        "common OR rare",
                        "alpha OR missing",
                        "beta AND filler",
                        "\"rare alpha\"",
                        "missing",
                    ] {
                        let plan = Spi::get_one::<Json>(&format!(
                            "EXPLAIN (ANALYZE, FORMAT JSON) EXECUTE ranked_parameter('{query}')"
                        ))
                        .unwrap()
                        .unwrap()
                        .0
                        .to_string();
                        if mode != "auto" {
                            assert!(plan.contains("\"Order\":\"score DESC\""), "{query}: {plan}");
                            assert!(plan.contains("\"Top K\":10"), "{query}: {plan}");
                            if scorer == "stannum.full_score(ctid)" && query == "common OR rare" {
                                assert!(plan.contains("\"Pruning\":\"ordinal\""), "{plan}");
                            }
                        }
                        let scores = |sql: &str| -> Vec<u32> {
                            Spi::connect(|client| {
                                client
                                    .select(sql, None, &[])
                                    .unwrap()
                                    .map(|row| row.get::<f32>(3).unwrap().unwrap().to_bits())
                                    .collect()
                            })
                        };
                        let actual = scores(&format!("EXECUTE ranked_parameter('{query}')"));
                        let expected = scores(&format!(
                            "WITH all_scores AS MATERIALIZED (SELECT id, body,
                   {scorer} AS score FROM prepared_rank WHERE body ==> '{query}')
                 SELECT id, body, score FROM all_scores ORDER BY score DESC LIMIT 10"
                        ));
                        assert_eq!(actual, expected, "{query}");
                    }
                }
                // NULL must not parse the previous query or retain its results.
                assert!(ids("EXECUTE ranked_parameter(NULL)").is_empty());
                assert_eq!(ids("EXECUTE ranked_parameter('rare')").len(), 10);
                // Plain EXPLAIN initializes executor state without evaluating the
                // query parameter; malformed TINQL only errors on execution.
                Spi::run("EXPLAIN EXECUTE ranked_parameter('AND AND')").unwrap();
                Spi::run(
                    "DO $$ DECLARE failed boolean := false; BEGIN
                BEGIN EXECUTE 'EXECUTE ranked_parameter(''AND AND'')';
                EXCEPTION WHEN OTHERS THEN failed := true; END;
                IF NOT failed THEN RAISE EXCEPTION 'malformed query unexpectedly succeeded'; END IF;
                END $$",
                )
                .unwrap();
                assert_eq!(ids("EXECUTE ranked_parameter('rare')").len(), 10);
                Spi::run("DEALLOCATE ranked_parameter").unwrap();
            }
        }
    }

    #[pg_test]
    fn generic_ranked_runtime_bounds_preserve_scores_and_sql_semantics() {
        Spi::run(
            "CREATE TABLE runtime_rank(id int, body text);
             INSERT INTO runtime_rank SELECT n, repeat('common ', n % 7 + 1) ||
                 CASE WHEN n % 2 = 0 THEN 'blue' ELSE 'red' END FROM generate_series(1,1000) n;
             CREATE INDEX runtime_rank_idx ON runtime_rank USING stannum(body);
             ANALYZE runtime_rank;
             SET LOCAL plan_cache_mode = force_generic_plan;
             SET LOCAL enable_seqscan = off;
             SET LOCAL enable_bitmapscan = off;
             SET LOCAL enable_indexscan = off;",
        )
        .unwrap();
        // Constant query text must also acquire a runtime bound. Selective quals
        // retain exhaustive ranking to avoid a wasted top-k pass and completion.
        for query in ["$1", "'common OR blue'"] {
            for filter in ["true", "id > 950"] {
                Spi::run(&format!(
                    "PREPARE runtime_bound(text, bigint, bigint) AS
                     SELECT id, stannum.full_score(ctid) AS score FROM runtime_rank
                     WHERE body ==> {query} AND {filter}
                     ORDER BY score DESC LIMIT $2 OFFSET $3"
                ))
                .unwrap();
                for (limit, offset, top_k) in [
                    ("10", "0", Some(10)),
                    ("7", "12", Some(19)),
                    ("10", "NULL", Some(10)),
                    ("NULL", "3", None),
                    ("0", "0", None),
                    ("9223372036854775807", "1", None),
                    ("10", "0", Some(10)),
                ] {
                    let sql = format!("EXECUTE runtime_bound('common OR blue', {limit}, {offset})");
                    let plan =
                        Spi::get_one::<Json>(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}"))
                            .unwrap()
                            .unwrap()
                            .0
                            .to_string();
                    if let Some(k) = top_k.filter(|_| filter == "true") {
                        assert!(plan.contains(&format!("\"Top K\":{k}")), "{plan}");
                    } else {
                        assert!(!plan.contains("\"Top K\":"), "{plan}");
                    }
                    let expected: std::collections::HashMap<i32, u32> = Spi::connect(|client| {
                        client
                            .select(
                                &format!(
                                    "SELECT id, stannum.full_score(ctid) AS score
                            FROM runtime_rank WHERE body ==> 'common OR blue' AND {filter}"
                                ),
                                None,
                                &[],
                            )
                            .unwrap()
                            .map(|row| {
                                (
                                    row.get::<i32>(1).unwrap().unwrap(),
                                    row.get::<f32>(2).unwrap().unwrap().to_bits(),
                                )
                            })
                            .collect()
                    });
                    let mut scores: Vec<u32> = expected.values().copied().collect();
                    scores
                        .sort_unstable_by(|a, b| f32::from_bits(*b).total_cmp(&f32::from_bits(*a)));
                    let offset = offset.parse::<usize>().unwrap_or(0);
                    let limit = limit.parse::<usize>().unwrap_or(usize::MAX);
                    let scores: Vec<_> = scores.into_iter().skip(offset).take(limit).collect();
                    let mut seen = std::collections::HashSet::new();
                    let actual: Vec<u32> = Spi::connect(|client| {
                        client
                            .select(&sql, None, &[])
                            .unwrap()
                            .map(|row| {
                                let id = row.get::<i32>(1).unwrap().unwrap();
                                let bits = row.get::<f32>(2).unwrap().unwrap().to_bits();
                                assert_eq!(expected.get(&id), Some(&bits));
                                assert!(seen.insert(id));
                                bits
                            })
                            .collect()
                    });
                    assert_eq!(actual, scores, "{sql}, filter={filter}");
                }
                Spi::run(
                    "DO $$ BEGIN
                    BEGIN EXECUTE 'EXECUTE runtime_bound(''common'', -1, 0)';
                        RAISE EXCEPTION 'negative limit accepted';
                    EXCEPTION WHEN invalid_row_count_in_limit_clause THEN NULL; END;
                    BEGIN EXECUTE 'EXECUTE runtime_bound(''common'', 10, -1)';
                        RAISE EXCEPTION 'negative offset accepted';
                    EXCEPTION WHEN invalid_row_count_in_result_offset_clause THEN NULL; END;
                    END $$;
                    DEALLOCATE runtime_bound;",
                )
                .unwrap();
            }
        }
        Spi::run(
            "SET LOCAL enable_material = off; SET LOCAL enable_memoize = off;
            PREPARE runtime_rescan(bigint, bigint) AS
            SELECT s.id, s.score, s.iteration FROM generate_series(1,3) g
            CROSS JOIN LATERAL (SELECT id, stannum.full_score(ctid) AS score, g AS iteration
                FROM runtime_rank WHERE body ==> 'common OR blue'
                ORDER BY score DESC LIMIT $1 OFFSET $2) s",
        )
        .unwrap();
        for (limit, offset) in [(7, 0), (10, 12), (3, 2)] {
            let sql = format!("EXECUTE runtime_rescan({limit}, {offset})");
            let plan = Spi::get_one::<Json>(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}"))
                .unwrap()
                .unwrap()
                .0;
            fn ranked_scan(value: &serde_json::Value) -> Option<&serde_json::Value> {
                match value {
                    serde_json::Value::Object(fields) => {
                        if fields.get("Order").and_then(|v| v.as_str()) == Some("score DESC") {
                            return Some(value);
                        }
                        fields.values().find_map(ranked_scan)
                    }
                    serde_json::Value::Array(items) => items.iter().find_map(ranked_scan),
                    _ => None,
                }
            }
            let scan = ranked_scan(&plan).expect("ranked custom scan");
            assert_eq!(scan["Actual Loops"].as_u64(), Some(3), "{plan}");
            assert_eq!(scan["Top K"].as_i64(), Some(limit + offset), "{plan}");
            let mut actual = [Vec::new(), Vec::new(), Vec::new()];
            Spi::connect(|client| {
                for row in client.select(&sql, None, &[]).unwrap() {
                    let iteration = row.get::<i32>(3).unwrap().unwrap();
                    actual[(iteration - 1) as usize]
                        .push(row.get::<f32>(2).unwrap().unwrap().to_bits());
                }
            });
            assert_eq!(actual[0].len(), limit as usize);
            assert_eq!(actual[0], actual[1]);
            assert_eq!(actual[0], actual[2]);
        }
        Spi::run("DEALLOCATE runtime_rescan").unwrap();

        // WITH TIES must read beyond k, preserving every row at the boundary.
        Spi::run(
            "PREPARE runtime_ties(bigint) AS
            SELECT id, stannum.full_score(ctid) AS score FROM runtime_rank
            WHERE body ==> 'common OR blue' ORDER BY score DESC FETCH FIRST $1 ROWS WITH TIES",
        )
        .unwrap();
        let actual = ids("EXECUTE runtime_ties(7)");
        let expected = ids("WITH scores AS MATERIALIZED (
            SELECT id, stannum.full_score(ctid) AS score FROM runtime_rank
            WHERE body ==> 'common OR blue') SELECT id FROM scores ORDER BY score DESC
            FETCH FIRST 7 ROWS WITH TIES");
        assert!(actual.len() > 7);
        assert_eq!(
            actual
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            expected
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
        );
        Spi::run("DEALLOCATE runtime_ties").unwrap();

        // A bound above an SRF counts projected rows, not matching documents.
        // Grouping/aggregation also cannot import the outer LIMIT into ranking.
        for projection in [
            "SELECT id, stannum.full_score(ctid) AS score, generate_series(1,3) FROM runtime_rank
             WHERE body ==> 'common OR blue' ORDER BY score DESC LIMIT $1",
            "SELECT max(stannum.full_score(ctid)) AS score FROM runtime_rank
             WHERE body ==> 'common OR blue' GROUP BY id ORDER BY score DESC LIMIT $1",
        ] {
            Spi::run(&format!(
                "PREPARE runtime_unsupported(bigint) AS {projection}"
            ))
            .unwrap();
            let plan = Spi::get_one::<Json>(
                "EXPLAIN (ANALYZE, FORMAT JSON) EXECUTE runtime_unsupported(7)",
            )
            .unwrap()
            .unwrap()
            .0
            .to_string();
            assert!(!plan.contains("\"Top K\":"), "{plan}");
            Spi::run("DEALLOCATE runtime_unsupported").unwrap();
        }
    }

    #[pg_test]
    fn generic_ranked_scan_rescans_and_preserves_remaining_filters() {
        Spi::run(
            "CREATE TABLE generic_rescan(id int, body text);
            INSERT INTO generic_rescan SELECT n, repeat('common ', n % 7 + 1) ||
                CASE WHEN n % 2 = 0 THEN 'blue' ELSE 'red' END FROM generate_series(1,1000) n;
            CREATE INDEX generic_rescan_idx ON generic_rescan USING stannum(body);
            ANALYZE generic_rescan;
            SET LOCAL plan_cache_mode = force_generic_plan;
            SET LOCAL enable_seqscan = off;
            SET LOCAL enable_bitmapscan = off;
            SET LOCAL enable_indexscan = off;
            SET LOCAL enable_material = off;
            SET LOCAL enable_memoize = off;
            PREPARE generic_loop(text) AS SELECT s.id, s.score, g FROM generate_series(1,3) g
                CROSS JOIN LATERAL (SELECT id, stannum.full_score(ctid) AS score
                FROM generic_rescan WHERE body ==> $1 AND id > 900
                ORDER BY score DESC LIMIT 10 OFFSET g * 0) s;",
        )
        .unwrap();
        for query in ["common OR blue", "red", "missing", "blue"] {
            let expected: std::collections::HashMap<i32, u32> = Spi::connect(|client| {
                client
                    .select(
                        &format!(
                            "SELECT id, stannum.full_score(ctid) AS score
                    FROM generic_rescan WHERE body ==> '{query}' AND id > 900"
                        ),
                        None,
                        &[],
                    )
                    .unwrap()
                    .map(|r| {
                        (
                            r.get::<i32>(1).unwrap().unwrap(),
                            r.get::<f32>(2).unwrap().unwrap().to_bits(),
                        )
                    })
                    .collect()
            });
            let mut best: Vec<u32> = expected.values().copied().collect();
            best.sort_unstable_by(|a, b| f32::from_bits(*b).total_cmp(&f32::from_bits(*a)));
            best.truncate(10);
            let mut actual = [Vec::new(), Vec::new(), Vec::new()];
            let mut seen = std::collections::HashSet::new();
            Spi::connect(|client| {
                for row in client
                    .select(&format!("EXECUTE generic_loop('{query}')"), None, &[])
                    .unwrap()
                {
                    let id = row.get::<i32>(1).unwrap().unwrap();
                    let bits = row.get::<f32>(2).unwrap().unwrap().to_bits();
                    let iteration = row.get::<i32>(3).unwrap().unwrap();
                    assert_eq!(expected.get(&id), Some(&bits), "{query}: row {id}");
                    assert!(seen.insert((iteration, id)), "duplicate row within rescan");
                    actual[(iteration - 1) as usize].push(bits);
                }
            });
            for scores in actual {
                // Tied boundary rows can differ; exact ordered scores cannot.
                assert_eq!(scores, best, "{query}");
            }
        }
        let plan =
            Spi::get_one::<Json>("EXPLAIN (ANALYZE, FORMAT JSON) EXECUTE generic_loop('blue')")
                .unwrap()
                .unwrap()
                .0;
        fn find_scan(plan: &serde_json::Value) -> Option<&serde_json::Value> {
            if plan["Custom Plan Provider"] == "Stannum Text Search Scan" {
                return Some(plan);
            }
            plan.get("Plans")?.as_array()?.iter().find_map(find_scan)
        }
        let scan = find_scan(&plan[0]["Plan"])
            .unwrap_or_else(|| panic!("ranked generic scan beneath lateral limit: {plan}"));
        assert_eq!(scan["Actual Loops"], 3);
        assert_eq!(scan["Order"], "score DESC");
        Spi::run("DEALLOCATE generic_loop").unwrap();
    }

    /// Rows of a ranked query as `(id, score bits)` so scores compare exactly.
    fn ranked(custom: bool, query: &str, order_by: &str, limit: &str) -> Vec<(i32, u32)> {
        ranked_in("bmw", custom, query, order_by, limit)
    }

    /// As [`ranked`], over `table`.
    fn ranked_in(
        table: &str,
        custom: bool,
        query: &str,
        order_by: &str,
        limit: &str,
    ) -> Vec<(i32, u32)> {
        Spi::run(&format!(
            "SET LOCAL stannum.enable_custom_scan = {custom}; SET LOCAL enable_seqscan = off;"
        ))
        .unwrap();
        Spi::connect(|client| {
            client
                .select(
                    &format!(
                        "SELECT id, {order_by} AS score FROM {table} WHERE body ==> '{query}'
                         ORDER BY score DESC{} {limit}",
                        if custom { "" } else { ", ctid" }
                    ),
                    None,
                    &[],
                )
                .unwrap()
                .map(|row| {
                    (
                        row.get::<i32>(1).unwrap().unwrap(),
                        row.get::<f32>(2).unwrap().unwrap().to_bits(),
                    )
                })
                .collect()
        })
    }

    #[pg_test]
    fn buffered_scoring_keeps_document_lengths_when_heap_space_is_reused() {
        Spi::run(
            "CREATE TABLE length_snapshot(id int, body text) WITH (fillfactor=50);
             INSERT INTO length_snapshot SELECT n, repeat('filler ', 100)
               FROM generate_series(1, 200) n;
             CREATE INDEX length_snapshot_idx ON length_snapshot USING stannum(body);
             INSERT INTO length_snapshot VALUES (1000, 'needle ' || repeat('filler ', 100));",
        )
        .unwrap();
        let heap = oid_of("length_snapshot");
        let index = oid_of("length_snapshot_idx");
        let tid = Spi::get_one::<pgrx::pg_sys::ItemPointerData>(
            "SELECT ctid FROM length_snapshot WHERE id=1000",
        )
        .unwrap()
        .unwrap();
        let tid = segment::Tid::new(
            (u32::from(tid.ip_blkid.bi_hi) << 16) | u32::from(tid.ip_blkid.bi_lo),
            tid.ip_posid,
        )
        .unwrap();
        let mut scorer = crate::score::scorer_for_scan(
            crate::score::scan_id(),
            heap,
            index,
            "needle",
            true,
            None,
            None,
            None,
            None,
            None,
        );
        let before = scorer.score(tid);
        assert!(before > 0.0);
        Spi::run("UPDATE length_snapshot SET body='needle' WHERE id=1").unwrap();
        assert!(
            Spi::get_one::<bool>(
                "SELECT a.ctid < b.ctid FROM length_snapshot a, length_snapshot b
                 WHERE a.id=1 AND b.id=1000",
            )
            .unwrap()
            .unwrap()
        );
        // Completing a pruned scan refreshes the buffer while retaining its
        // scorer. The earlier insertion must not change the retained lengths.
        let _refreshed = unsafe { crate::storage::view(index.into()) };
        assert_eq!(scorer.score(tid).to_bits(), before.to_bits());
    }

    #[pg_test]
    fn a_completed_ranked_scan_does_not_repeat_the_rows_it_emitted() {
        // The second-best row is deleted, so its location stays in the index
        // but the parent reads past the pruned top k and the scan completes
        // the ordering. By then documents indexed after the cursor's snapshot
        // outrank every visible row; the completed ordering must skip the
        // rows already emitted rather than resume at a position.
        Spi::run(
            "CREATE TABLE cur(id int primary key, body text);
             INSERT INTO cur SELECT n, 'other filler' FROM generate_series(1, 200) n;
             INSERT INTO cur VALUES (301, 'needle needle needle needle'),
               (302, 'needle needle needle'), (303, 'needle needle'), (304, 'needle');
             CREATE INDEX cur_idx ON cur USING stannum(body);
             INSERT INTO cur VALUES (305, 'needle other');
             DELETE FROM cur WHERE id = 302;
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        let ids = |sql: &str| {
            Spi::connect(|client| {
                client
                    .select(sql, None, &[])
                    .unwrap()
                    .map(|row| row.get::<i32>(1).unwrap().unwrap())
                    .collect::<Vec<_>>()
            })
        };
        let query = "SELECT id FROM cur WHERE body ==> 'needle' ORDER BY stannum.full_score(ctid) DESC LIMIT 3";
        Spi::run("SET LOCAL stannum.enable_custom_scan = off;").unwrap();
        let expected = ids(query);
        assert_eq!(expected.len(), 3);
        Spi::run(&format!(
            "SET LOCAL stannum.enable_custom_scan = on; SET LOCAL enable_bitmapscan = off;
             DECLARE top CURSOR FOR {query};"
        ))
        .unwrap();
        let mut seen = ids("FETCH 1 FROM top");
        Spi::run(
            "INSERT INTO cur SELECT n, 'needle needle needle needle needle needle'
             FROM generate_series(401, 403) n",
        )
        .unwrap();
        seen.extend(ids("FETCH ALL FROM top"));
        assert_eq!(seen, expected);
        Spi::run("CLOSE top").unwrap();
    }

    #[pg_test]
    fn wide_disjunction_pivots_preserve_exact_ranked_results() {
        let words = (0..128)
            .map(|i| format!("word{}{}", (b'a' + i / 26) as char, (b'a' + i % 26) as char))
            .collect::<Vec<_>>();
        let array = words
            .iter()
            .map(|w| format!("'{w}'"))
            .collect::<Vec<_>>()
            .join(",");
        Spi::run(&format!("CREATE TABLE bmw(id int PRIMARY KEY, body text);
            SET LOCAL stannum.build_segment_docs=128;
            INSERT INTO bmw SELECT n, string_agg(repeat(w || ' ', 1+n%3), ' ' ORDER BY j)
            FROM generate_series(1,512) n CROSS JOIN unnest(ARRAY[{array}]) WITH ORDINALITY words(w,j)
            WHERE n%17=0 OR (n*31+j*7)%19<5 GROUP BY n;
            CREATE INDEX bmw_idx ON bmw USING stannum(body);
            INSERT INTO bmw VALUES (513, '{}');
            DELETE FROM bmw WHERE id%23=0;
            UPDATE bmw SET body=body || ' extra' WHERE id%31=0;", words.join(" "))).unwrap();
        // Exercise both sides of the grouping cutoff, boosts, ties, segment
        // boundaries, mutable postings, dead rows, and LIMIT continuation.
        for width in [31, 32, 33, 128] {
            let query = words[..width]
                .iter()
                .enumerate()
                .map(|(i, w)| format!("{w}^{}", if i % 2 == 0 { "0.25" } else { "2" }))
                .collect::<Vec<_>>()
                .join(" OR ");
            for limit in ["LIMIT 1", "LIMIT 10", "LIMIT 100", "LIMIT 10 OFFSET 20"] {
                assert_eq!(
                    ranked(true, &query, "stannum.full_score(ctid)", limit),
                    ranked(false, &query, "stannum.full_score(ctid)", limit),
                    "width={width}, {limit}"
                );
            }
        }
        Spi::run("SET LOCAL stannum.enable_custom_scan=on").unwrap();
        let query = words.join(" OR ");
        let plan = Spi::get_one::<Json>(&format!(
            "EXPLAIN (ANALYZE, FORMAT JSON)
            SELECT id,stannum.full_score(ctid) AS score FROM bmw WHERE body ==> '{query}'
            ORDER BY score DESC LIMIT 10"
        ))
        .unwrap()
        .unwrap()
        .0
        .to_string();
        assert!(plan.contains("\"Pruning\":\"ordinal\""), "{plan}");
    }

    /// A disjunction's walk loads a term's chunk only where a candidate
    /// needs its bits. One segment spans two chunks of ordinals and every
    /// term occupies both; in the first the rare and the middling term
    /// share documents, which fill the top k, and in the second they never
    /// do, so the required terms' AND empties every sub-block there and the
    /// common term's chunk is never read, yet the ranking is the exhaustive
    /// one bit for bit.
    #[pg_test]
    fn disjunction_walk_skips_the_chunks_no_candidate_needs() {
        Spi::run(
            "CREATE TABLE lazy(id int primary key, body text);
             INSERT INTO lazy SELECT n,
               CASE WHEN n % 2 = 0 THEN 'alpha ' ELSE '' END ||
               CASE WHEN n <= 65536 AND n % 100 = 0 THEN 'delta delta gamma gamma ' ELSE '' END ||
               CASE WHEN n <= 65536 AND n % 10 = 0 AND n % 100 <> 0 THEN 'gamma ' ELSE '' END ||
               CASE WHEN n > 65536 AND n % 10 = 5 THEN 'gamma gamma ' ELSE '' END ||
               CASE WHEN n > 65536 AND n % 100 = 0 THEN 'delta delta delta ' ELSE '' END ||
               repeat('pad ', CASE WHEN n % 100 = 0 THEN 0 ELSE n % 7 END) || 'tail'
               FROM generate_series(1, 70000) n;
             CREATE INDEX lazy_idx ON lazy USING stannum(body);",
        )
        .unwrap();
        let ranked = |custom: bool| -> Vec<(i32, u32)> {
            Spi::run(&format!(
                "SET LOCAL stannum.enable_custom_scan = {custom}; SET LOCAL enable_seqscan = off;
                 SET LOCAL enable_bitmapscan = off;"
            ))
            .unwrap();
            Spi::connect(|client| {
                client
                    .select(
                        &format!(
                            "SELECT id, stannum.score(ctid, 1.0) AS score FROM lazy
                             WHERE body ==> 'delta OR gamma OR alpha'
                             ORDER BY score DESC{} LIMIT 10",
                            if custom { "" } else { ", ctid" }
                        ),
                        None,
                        &[],
                    )
                    .unwrap()
                    .map(|row| {
                        (
                            row.get::<i32>(1).unwrap().unwrap(),
                            row.get::<f32>(2).unwrap().unwrap().to_bits(),
                        )
                    })
                    .collect()
            })
        };
        let expected = ranked(false);
        assert_eq!(ranked(true), expected);
        assert!(
            expected.iter().all(|(id, _)| id % 100 == 0 && *id <= 65536),
            "{expected:?}"
        );
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM lazy WHERE body ==> 'delta OR gamma OR alpha'
             ORDER BY stannum.score(ctid, 1.0) DESC LIMIT 10",
        )
        .unwrap()
        .unwrap()
        .0;
        fn search_scan(node: &serde_json::Value) -> Option<serde_json::Value> {
            if node["Custom Plan Provider"] == "Stannum Text Search Scan" {
                return Some(node.clone());
            }
            node["Plans"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(search_scan)
        }
        let scan = search_scan(&plan[0]["Plan"]).unwrap_or_else(|| panic!("{plan}"));
        assert_eq!(scan["Pruning"], "ordinal", "{scan}");
        // Three chunks in the first chunk of ordinals; delta and gamma in
        // the second, whose AND leaves nothing for alpha's chunk to settle.
        // Loading every present term's chunk up front read six.
        assert_eq!(scan["Chunks Loaded"], 5, "{scan}");
    }

    #[pg_test]
    fn ranked_walks_release_the_pages_they_hold() {
        // 20,000 documents in one segment: its class table spans three
        // pages and its length table ten, so a walk moves its held pages.
        Spi::run(
            "CREATE TABLE held(id int primary key, body text);
             SET LOCAL stannum.build_segment_docs = 100000;
             INSERT INTO held SELECT n,
               repeat('alpha ', 1 + n % 3) ||
               CASE WHEN n % 7 = 0 THEN 'beta ' ELSE '' END ||
               repeat('pad ', n % 40) || 'tail'
               FROM generate_series(1, 20000) n;
             CREATE INDEX held_idx ON held USING stannum(body);
             SET LOCAL enable_seqscan = off;
             SET LOCAL enable_bitmapscan = off;",
        )
        .unwrap();
        fn search_scan(node: &serde_json::Value) -> Option<serde_json::Value> {
            if node["Custom Plan Provider"] == "Stannum Text Search Scan" {
                return Some(node.clone());
            }
            node["Plans"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(search_scan)
        }
        let before = crate::storage::held_pages();
        assert_eq!(before.0, 0, "{before:?}");
        for query in [
            "alpha OR beta",
            "pad OR beta",
            "alpha AND beta",
            "\"alpha beta\"",
        ] {
            let plan = Spi::get_one::<Json>(&format!(
                "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM held WHERE body ==> '{query}'
                 ORDER BY stannum.score(ctid, 1.0) DESC LIMIT 10"
            ))
            .unwrap()
            .unwrap()
            .0;
            let scan = search_scan(&plan[0]["Plan"]).unwrap_or_else(|| panic!("{plan}"));
            assert_eq!(scan["Pruning"], "ordinal", "{query}: {scan}");
            assert_eq!(scan["Actual Rows"].as_f64(), Some(10.0), "{query}: {scan}");
            let held = crate::storage::held_pages();
            assert_eq!(held.0, 0, "{query}: {held:?}");
        }
        let after = crate::storage::held_pages();
        assert!(after.1 > before.1 + 4, "the walks held no pages: {after:?}");
    }

    #[pg_test]
    fn walks_release_the_chunk_and_position_pages_they_read_in_place() {
        // 140,000 documents in one segment: three chunks of ordinals per
        // term, bitmaps with their bucket nibbles running over pages past
        // the members for all but rare, whose chunks are arrays, and
        // position lists over many pages for the phrases. Beta and gamma
        // are dense enough to be elided from the default score, delta and
        // eps not.
        Spi::run(
            "CREATE TABLE inplace(id int primary key, body text);
             SET LOCAL stannum.build_segment_docs = 200000;
             INSERT INTO inplace SELECT n,
               repeat('alpha ', 1 + n % 3) ||
               CASE WHEN n % 7 = 0 THEN 'beta ' ELSE '' END ||
               CASE WHEN n % 3 = 0 THEN repeat('gamma ', 1 + n % 4) ELSE '' END ||
               CASE WHEN n % 13 = 0 THEN 'delta ' ELSE '' END ||
               CASE WHEN n % 29 = 0 THEN repeat('eps ', 1 + n % 2) ELSE '' END ||
               CASE WHEN n % 97 = 0 THEN 'rare ' ELSE '' END ||
               repeat('pad ', n % 5) || 'tail'
               FROM generate_series(1, 140000) n;
             CREATE INDEX inplace_idx ON inplace USING stannum(body);
             SET LOCAL enable_seqscan = off;
             SET LOCAL enable_bitmapscan = off;",
        )
        .unwrap();
        fn search_scan(node: &serde_json::Value) -> Option<serde_json::Value> {
            if node["Custom Plan Provider"] == "Stannum Text Search Scan" {
                return Some(node.clone());
            }
            node["Plans"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(search_scan)
        }
        let before = crate::storage::held_pages();
        assert_eq!(before.0, 0, "{before:?}");
        let mut peak = 0;
        let mut walks = 0;
        for query in [
            "alpha OR beta",
            "delta OR eps OR rare",
            "alpha AND beta",
            "delta AND eps",
            "delta AND gamma AND rare",
            "delta AND NOT eps",
            "(delta OR rare) AND gamma",
            "\"alpha beta\"",
            "\"delta eps\"",
            "\"alpha beta\" OR rare",
            "\"gamma delta\" AND eps",
        ] {
            for score in ["stannum.score(ctid)", "stannum.score(ctid, 1.0)"] {
                let plan = Spi::get_one::<Json>(&format!(
                    "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM inplace WHERE body ==> '{query}'
                     ORDER BY {score} DESC LIMIT 10"
                ))
                .unwrap()
                .unwrap()
                .0;
                let scan = search_scan(&plan[0]["Plan"]).unwrap_or_else(|| panic!("{plan}"));
                assert_eq!(scan["Actual Rows"].as_f64(), Some(10.0), "{query}: {scan}");
                walks += usize::from(scan["Pruning"] == "ordinal");
                peak = peak.max(scan["Pages Held Peak"].as_i64().unwrap_or(0));
                let held = crate::storage::held_pages();
                assert_eq!(held.0, 0, "{query}, {score}: {held:?}");
            }
        }
        let after = crate::storage::held_pages();
        assert!(
            after.1 > before.1 + 20,
            "the walks held no pages: {after:?}"
        );
        assert!(walks >= 16, "{walks} walks");
        assert!(peak >= 4, "no walk held a chunk's pages: {peak}");
        // A walk cancelled mid-way, with chunk pages held, releases them as
        // the cancel unwinds it.
        for query in [
            "alpha OR beta OR gamma",
            "beta AND gamma AND delta",
            "\"gamma delta\"",
        ] {
            let plan = Spi::get_one::<Json>(&format!(
                "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM inplace WHERE body ==> '{query}'
                 ORDER BY stannum.score(ctid, 1.0) DESC LIMIT 10"
            ))
            .unwrap()
            .unwrap()
            .0;
            let scan = search_scan(&plan[0]["Plan"]).unwrap_or_else(|| panic!("{plan}"));
            let loads = scan["Chunks Loaded"].as_i64().unwrap_or(0);
            assert!(
                loads >= 2 && scan["Pruning"] == "ordinal",
                "{query}: {scan}"
            );
            crate::score::cancel_at_chunk_load(loads / 2 + 1);
            Spi::run(&format!(
                "DO $$ BEGIN
                   PERFORM id FROM inplace WHERE body ==> '{query}'
                     ORDER BY stannum.score(ctid, 1.0) DESC LIMIT 10;
                   RAISE EXCEPTION 'the walk was not cancelled';
                 EXCEPTION WHEN query_canceled THEN NULL;
                 END $$"
            ))
            .unwrap();
            let holding = crate::score::cancel_at_chunk_load(0);
            assert!(holding > 0, "{query}: cancelled holding no pages");
            let held = crate::storage::held_pages();
            assert_eq!(held.0, 0, "{query}: cancelled walk left {held:?}");
        }
        // And the next walk reads as before.
        let count = Spi::get_one::<i64>(
            "SELECT count(*) FROM (SELECT id FROM inplace WHERE body ==> 'alpha AND beta'
             ORDER BY stannum.score(ctid) DESC LIMIT 10) top",
        )
        .unwrap();
        assert_eq!(count, Some(10));
        assert_eq!(crate::storage::held_pages().0, 0);
    }

    /// A backend exiting on FATAL drops its cached readers with a walk's
    /// hold span still open, after PostgreSQL released the span's pin and
    /// relation reference itself; the reader must not release them again.
    /// postgres/tests/exit_during_walk.py drives the real exit on Linux.
    #[pg_test]
    fn a_reader_dropped_at_exit_leaves_its_pins_to_postgres() {
        Spi::run(
            "CREATE TABLE exiting(id int, body text);
             INSERT INTO exiting SELECT n, 'alpha beta ' || n FROM generate_series(1, 5000) n;
             CREATE INDEX exiting_idx ON exiting USING stannum(body);",
        )
        .unwrap();
        let oid = Spi::get_one::<pg_sys::Oid>("SELECT 'exiting_idx'::regclass::oid")
            .unwrap()
            .unwrap();
        let before = crate::storage::held_pages().0;
        // Otherwise the drop releases the span's page and relation.
        unsafe { crate::storage::drop_holding_source(oid, false) };
        assert_eq!(crate::storage::held_pages().0, before);
        // At exit it leaves both to the resource owner, so releasing them
        // here, as exit processing would, releases each once: a second
        // release raises "not owned by resource owner".
        let (buffer, relation) = unsafe { crate::storage::drop_holding_source(oid, true) };
        assert_eq!(crate::storage::held_pages().0, before);
        unsafe {
            pg_sys::ReleaseBuffer(buffer);
            pg_sys::RelationClose(relation);
        }
    }

    #[pg_test]
    fn held_pages_are_pinned_through_the_buffers_they_were_last_in() {
        // A walk pins a held page through the buffer the backend last
        // pinned its block in. The buffer is only a hint: after eviction,
        // other relations' pages, or a rebuild that reuses the block
        // numbers under a new relfilenode, the walk must read the pages it
        // asks for, and still release every pin.
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS pg_buffercache;
             CREATE TABLE recent(id int primary key, body text);
             SET LOCAL stannum.build_segment_docs = 200000;
             INSERT INTO recent SELECT n,
               repeat('alpha ', 1 + n % 3) ||
               CASE WHEN n % 7 = 0 THEN 'beta ' ELSE '' END ||
               CASE WHEN n % 13 = 0 THEN 'delta ' ELSE '' END ||
               CASE WHEN n % 29 = 0 THEN repeat('eps ', 1 + n % 2) ELSE '' END ||
               repeat('pad ', n % 5) || 'tail'
               FROM generate_series(1, 90000) n;
             CREATE INDEX recent_idx ON recent USING stannum(body);
             CREATE TABLE other(id int, body text);
             INSERT INTO other SELECT n, repeat('beta alpha ', 1 + n % 4) || 'tail'
               FROM generate_series(1, 90000) n;
             CREATE INDEX other_idx ON other USING stannum(body);
             SET LOCAL enable_seqscan = off;
             SET LOCAL enable_bitmapscan = off;",
        )
        .unwrap();
        fn search_scan(node: &serde_json::Value) -> Option<serde_json::Value> {
            if node["Custom Plan Provider"] == "Stannum Text Search Scan" {
                return Some(node.clone());
            }
            node["Plans"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(search_scan)
        }
        let queries = [
            "alpha OR beta",
            "delta OR eps",
            "beta AND delta",
            "\"alpha beta\"",
            "\"delta eps\"",
        ];
        let top = |table: &str, query: &str| {
            Spi::get_one::<String>(&format!(
                "SELECT coalesce(string_agg(id || ':' || score, ',' ORDER BY score DESC, id), '') FROM
                   (SELECT id, stannum.score(ctid) AS score FROM {table}
                    WHERE body ==> '{query}' ORDER BY score DESC LIMIT 10) top"
            ))
            .unwrap()
            .unwrap()
        };
        // (pages pinned, of those through their recent buffer)
        let pins = |table: &str, query: &str| {
            let plan = Spi::get_one::<Json>(&format!(
                "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM {table} WHERE body ==> '{query}'
                 ORDER BY stannum.score(ctid) DESC LIMIT 10"
            ))
            .unwrap()
            .unwrap()
            .0;
            let scan = search_scan(&plan[0]["Plan"]).unwrap_or_else(|| panic!("{plan}"));
            let held = crate::storage::held_pages();
            assert_eq!(held.0, 0, "{table}, {query}: {held:?}");
            (
                scan["Pages Pinned"].as_i64().unwrap(),
                scan["Pages Pinned Recent"].as_i64().unwrap(),
            )
        };
        let expected: Vec<String> = queries.iter().map(|query| top("recent", query)).collect();
        let others: Vec<String> = queries.iter().map(|query| top("other", query)).collect();
        // Repeated, a walk pins its pages through the buffers it saw them in.
        let mut walked = 0;
        for query in queries {
            pins("recent", query);
            let (pinned, recent) = pins("recent", query);
            walked += usize::from(pinned > 0);
            assert_eq!(recent, pinned, "{query}: repeated but pinned afresh");
        }
        assert!(walked >= 3, "{walked} walks held pages");
        let evict = |relation: &str| {
            Spi::run(&format!(
                "SELECT count(pg_buffercache_evict(bufferid)) FROM pg_buffercache
                 WHERE relfilenode = pg_relation_filenode('{relation}')"
            ))
            .unwrap();
            let left = Spi::get_one::<i64>(&format!(
                "SELECT count(*) FROM pg_buffercache
                 WHERE relfilenode = pg_relation_filenode('{relation}')"
            ))
            .unwrap();
            assert_eq!(left, Some(0), "{relation} kept buffers");
        };
        let check = |round: &str| {
            for (query, expected) in queries.iter().zip(&expected) {
                assert_eq!(&top("recent", query), expected, "{round}: {query}");
                pins("recent", query);
            }
            for (query, expected) in queries.iter().zip(&others) {
                assert_eq!(&top("other", query), expected, "{round}: other, {query}");
            }
        };
        // Evicted, the remembered buffers go to other pages, the other
        // index's among them; the walks read their own.
        evict("recent_idx");
        evict("other_idx");
        check("evicted");
        evict("recent_idx");
        check("evicted again");
        // Rebuilt, the index has the same block numbers in a new file.
        Spi::run("REINDEX INDEX recent_idx").unwrap();
        check("reindexed");
        for query in queries {
            let (pinned, recent) = pins("recent", query);
            assert_eq!(recent, pinned, "reindexed, repeated: {query}");
        }
        assert_eq!(crate::storage::held_pages().0, 0);
    }

    #[pg_test]
    fn pruned_top_k_matches_full_scoring_bit_for_bit() {
        // Four build segments of 1,000 documents and a write buffer, with a
        // 60-row pattern of term frequencies and lengths so exact score ties
        // abound, deleted and updated rows that the index still lists, and
        // terms present in every document, in one segment only, or absent.
        Spi::run(
            "CREATE TABLE bmw(id int primary key, body text);
             SET LOCAL stannum.build_segment_docs = 1000;
             SET LOCAL stannum.write_buffer_docs = 1000;
             INSERT INTO bmw SELECT n,
               repeat('alpha ', n % 4) ||
               CASE WHEN n % 3 = 0 THEN 'beta ' ELSE '' END ||
               CASE WHEN n % 5 = 0 THEN repeat('gamma ', 1 + n % 2) ELSE '' END ||
               CASE WHEN n BETWEEN 2000 AND 2100 THEN 'delta ' ELSE '' END ||
               repeat('pad ', n % 6) || 'tail'
               FROM generate_series(1, 4000) n;
             CREATE INDEX bmw_idx ON bmw USING stannum(body);
             INSERT INTO bmw SELECT n, 'alpha alpha beta gamma tail' FROM generate_series(4001, 4300) n;
             INSERT INTO bmw SELECT n, 'alpha ' || repeat('pad ', n % 9) || 'tail' FROM generate_series(4301, 4400) n;
             DELETE FROM bmw WHERE id % 17 = 0;
             UPDATE bmw SET body = body || ' extra' WHERE id % 23 = 0;",
        )
        .unwrap();
        let queries = [
            "alpha",
            "beta",
            "delta",
            "pad",
            "tail",
            "missing",
            "alpha AND beta",
            "alpha AND beta AND gamma",
            "alpha AND beta AND gamma AND tail",
            "pad AND alpha AND tail",
            "delta AND gamma AND alpha",
            "alpha^2 AND beta^0.25",
            "alpha AND delta",
            "alpha AND missing",
            "alpha OR gamma",
            "alpha OR beta OR gamma",
            "delta OR gamma",
            "delta OR alpha",
            "alpha OR missing",
            "alpha^2 OR beta",
            "(alpha AND beta)^0.5",
            "alpha OR alpha",
            "pad OR alpha",
            // Phrases: the conjunction's walk, admitting only candidates whose
            // positions hold the phrase.
            "\"alpha beta\"",
            "\"alpha alpha\"",
            "\"beta gamma\"",
            "\"pad tail\"",
            "\"alpha pad tail\"",
            "\"alpha beta\"~1",
            "\"alpha _ gamma\"",
            "\"gamma delta\"",
            "\"alpha missing\"",
            "\"alpha beta\"^2",
            // Shapes the incremental check reads slot by slot: repeated
            // words, slop over several words, pinned gaps alone and with
            // slop, and pairs no document keeps at the pinned distance.
            "\"alpha alpha beta\"",
            "\"alpha beta alpha\"",
            "\"pad pad tail\"",
            "\"alpha alpha pad alpha\"",
            "\"alpha _ alpha\"",
            "\"alpha __ pad\"",
            "\"alpha _ beta\"~1",
            "\"alpha pad tail\"~2",
            "\"alpha beta gamma tail\"~3",
            "\"gamma gamma tail\"",
            "\"beta _ tail\"",
            "\"alpha pad _ pad\"",
            // Mixed shapes: the disjunction's walk over every scoring term,
            // admitting only candidates the shape holds. A phrase's words
            // score wherever they occur, the phrase matching or not.
            "delta OR \"alpha beta\"",
            "gamma OR \"beta gamma\"",
            "\"alpha beta\" OR \"beta gamma\"",
            "\"pad tail\" OR delta",
            "beta OR \"gamma gamma tail\"",
            "alpha OR \"alpha beta\"",
            "delta OR \"alpha beta\"~1 OR \"gamma _ tail\"",
            "(alpha AND beta) OR gamma",
            "(delta AND gamma) OR beta",
            "(alpha AND beta) OR (gamma AND pad)",
            "alpha AND (beta OR gamma)",
            "delta AND (beta OR \"alpha alpha\")",
            "(alpha OR delta) AND (beta OR gamma)",
            "delta OR (beta AND (gamma OR \"pad tail\"))",
            "delta OR beta^2 OR \"alpha pad\"^0.5",
            "(delta^3 AND alpha) OR gamma^0.5",
            "alpha AND NOT beta",
            "delta OR (alpha AND NOT gamma)",
            "(alpha OR beta) AND NOT \"beta gamma\"",
            "AT LEAST 2 OF [alpha beta gamma]",
            "AT LEAST 2 OF [delta \"alpha beta\" gamma pad]",
            "AT LEAST 3 OF [alpha beta gamma delta]",
            "AT LEAST 2 OF [alpha alpha beta]",
            "delta OR (gamma AND NOT \"alpha beta\")",
            "missing OR \"alpha beta\"",
            "\"alpha missing\" OR delta",
            "(alpha AND missing) OR beta",
            "(beta AND missing) OR \"missing gamma\"",
            // Shapes the pruned path leaves to full scoring.
            "al*",
            "al* OR beta",
        ];
        let limits = [
            "LIMIT 1",
            "LIMIT 3",
            "LIMIT 10",
            "LIMIT 5 OFFSET 8",
            "LIMIT 100",
            "LIMIT 127",
            "LIMIT 128",
            "LIMIT 129",
            "LIMIT 255",
            "LIMIT 256",
            "LIMIT 257",
            "LIMIT 5000",
        ];
        for query in queries {
            for limit in limits {
                for order_by in ["stannum.full_score(ctid)", "stannum.score(ctid)"] {
                    let expected = ranked(false, query, order_by, limit);
                    let actual = ranked(true, query, order_by, limit);
                    assert_eq!(actual, expected, "{query} {limit} {order_by}");
                }
            }
        }
        // The custom scan pruned the single-term query...
        Spi::run(
            "SET LOCAL stannum.enable_custom_scan = on; SET LOCAL enable_seqscan = off;
             SET LOCAL enable_bitmapscan = off;",
        )
        .unwrap();
        fn search_scan(node: &serde_json::Value) -> Option<serde_json::Value> {
            if node["Custom Plan Provider"] == "Stannum Text Search Scan" {
                return Some(node.clone());
            }
            node["Plans"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(search_scan)
        }
        let explain = |query: &str| {
            let plan = Spi::get_one::<Json>(&format!(
                "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM bmw WHERE body ==> '{query}'
                 ORDER BY stannum.full_score(ctid) DESC LIMIT 10"
            ))
            .unwrap()
            .unwrap()
            .0;
            search_scan(&plan[0]["Plan"]).unwrap_or_else(|| panic!("{query}: {plan}"))
        };
        let scan = explain("alpha");
        assert_eq!(scan["Custom Plan Provider"], "Stannum Text Search Scan");
        assert_eq!(scan["Top K"], 10);
        // A disjunction, of one term here, walks the ordinal streams; a
        // conjunction walks the TID postings with block bounds.
        assert_eq!(scan["Pruning"], "ordinal");
        // The ten best rows share the best score and are the earliest such
        // rows, so once they are found every later sub-block of 1,024
        // ordinals is skipped; the first ones are scored to the last tie.
        let scored = scan["Scored Candidates"].as_i64().unwrap();
        assert!(scored > 0 && scored < 1500, "{scan}");
        // ...and the conjunction and disjunction too.
        for (query, pruning) in [("alpha AND beta", "ordinal"), ("alpha OR gamma", "ordinal")] {
            let scan = explain(query);
            assert_eq!(scan["Pruning"], pruning, "{query}");
            assert!(scan["Scored Candidates"].as_i64().unwrap() < 1500, "{scan}");
        }
        // Mixed shapes walk the disjunction of their scoring terms, and
        // score nothing exhaustively.
        for query in [
            "delta OR \"alpha beta\"",
            "\"alpha beta\" OR \"beta gamma\"",
            "(alpha AND beta) OR gamma",
            "alpha AND (beta OR gamma)",
            "(alpha OR delta) AND (beta OR gamma)",
            "delta OR beta^2 OR \"alpha pad\"^0.5",
            "alpha AND NOT beta",
            "delta OR (alpha AND NOT gamma)",
            "AT LEAST 2 OF [alpha beta gamma]",
        ] {
            let scan = explain(query);
            assert_eq!(scan["Pruning"], "ordinal", "{query}: {scan}");
            assert_eq!(scan["Exhaustive Score Calls"], 0, "{query}: {scan}");
            assert!(
                scan["Scored Candidates"].as_i64().unwrap() < 1500,
                "{query}: {scan}"
            );
        }
        // A phrase child's positions are read only for candidates that
        // would rank and that no other child already admits.
        let scan = explain("\"alpha beta\" OR \"beta gamma\"");
        let checked = scan["Positions Checked"].as_i64().unwrap();
        assert!(checked > 0 && checked < 1300, "{scan}");
        let scan = explain("gamma OR \"alpha beta\"");
        assert!(scan["Positions Checked"].as_i64().unwrap() < 1300, "{scan}");
        // An expansion is left to full scoring.
        let scan = explain("al* OR beta");
        assert!(scan["Pruning"].is_null(), "{scan}");
        // `score` elides `alpha`, which most documents hold. The disjunction
        // is still pruned, over `delta` alone, because its top three all
        // score above the zero of a document holding only `alpha`. Asked for
        // more rows than `delta` has, the scan fills the rest from documents
        // holding only `alpha`, in heap order, without scoring them.
        for (limit, pruned) in [(3, true), (200, true), (5000, false)] {
            let plan = Spi::get_one::<Json>(&format!(
                "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM bmw WHERE body ==> 'delta OR alpha'
                 ORDER BY stannum.score(ctid) DESC LIMIT {limit}"
            ))
            .unwrap()
            .unwrap()
            .0;
            let scan = search_scan(&plan[0]["Plan"]).unwrap();
            assert_eq!(
                scan["Pruning"] == "ordinal",
                pruned,
                "LIMIT {limit}: {scan}"
            );
        }
        // In a conjunction the elided `alpha` still filters: its cursor joins
        // the walk that `delta` drives.
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM bmw WHERE body ==> 'alpha AND delta'
             ORDER BY stannum.score(ctid) DESC LIMIT 3",
        )
        .unwrap()
        .unwrap()
        .0;
        let scan = search_scan(&plan[0]["Plan"]).unwrap();
        assert_eq!(scan["Pruning"], "ordinal", "{scan}");
        // A phrase or conjunction of elided words alone still walks by
        // ordinal, as a conjunction of filters: nothing is scored, and the
        // walk stops at the first k matches in heap order, so the positions
        // of a handful of the 1,300 documents holding both words are read.
        // A single elided word is read from the candidate stream.
        for (query, pruning) in [
            ("\"alpha beta\"", "ordinal"),
            ("\"alpha pad tail\"", "ordinal"),
            ("alpha AND beta", "ordinal"),
            ("alpha", "block-max"),
        ] {
            let plan = Spi::get_one::<Json>(&format!(
                "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM bmw WHERE body ==> '{query}'
                 ORDER BY stannum.score(ctid) DESC LIMIT 10"
            ))
            .unwrap()
            .unwrap()
            .0;
            let scan = search_scan(&plan[0]["Plan"]).unwrap();
            assert_eq!(scan["Pruning"], pruning, "{query}: {scan}");
            assert_eq!(scan["Scored Candidates"], 0, "{query}: {scan}");
            assert!(
                scan["Positions Checked"].as_i64().unwrap() < 100,
                "{query}: {scan}"
            );
        }
        // Rows deleted after the top k was built are invisible, so the parent
        // reads past k and the scan completes the ordering from scratch.
        let top: Vec<i32> = ranked(true, "delta", "stannum.full_score(ctid)", "LIMIT 3")
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        Spi::run(&format!(
            "DELETE FROM bmw WHERE id IN ({})",
            top.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ))
        .unwrap();
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM bmw WHERE body ==> 'delta'
             ORDER BY stannum.full_score(ctid) DESC LIMIT 3",
        )
        .unwrap()
        .unwrap()
        .0;
        let scan = search_scan(&plan[0]["Plan"]).unwrap();
        assert_eq!(scan["Pruning"], "ordinal");
        // The walk checks visibility as rows enter its top k, so the deleted
        // rows never take a place and the scan needs no completion.
        assert_eq!(scan["Top-K Completions"], 0, "{scan}");
        assert_eq!(scan["Exhaustive Score Calls"], 0, "{scan}");
        assert_eq!(
            ranked(true, "delta", "stannum.full_score(ctid)", "LIMIT 3"),
            ranked(false, "delta", "stannum.full_score(ctid)", "LIMIT 3")
        );
        // A phrase walks its terms' conjunction and reads positions only for
        // the candidates that score into the top k: fewer than the 1,300
        // documents holding both words.
        Spi::run("SET LOCAL stannum.enable_custom_scan = on;").unwrap();
        let scan = explain("\"alpha beta\"");
        assert_eq!(scan["Pruning"], "ordinal", "{scan}");
        let checked = scan["Positions Checked"].as_i64().unwrap();
        assert!(checked > 0 && checked < 1300, "{scan}");
        // A span shape not every slot of which must occur is left to full
        // scoring.
        let scan = explain("\"alpha beta\" NOT ENCLOSES \"gamma\"");
        assert!(scan["Pruning"].is_null(), "{scan}");
        // Long disjunctions, whose sub-blocks the walk sieves a word at a
        // time: fourteen words from one in every document to one in a
        // thousand, with periodic frequencies and lengths so scores tie
        // across many documents, over segments of several sub-blocks each,
        // with deleted rows the index still lists. Two runs of identical
        // documents, one filling whole sub-blocks of a build segment and one
        // written later, tie exactly at their sub-blocks' bounds.
        Spi::run(
            "CREATE TABLE bmw_long(id int primary key, body text);
             SET LOCAL stannum.build_segment_docs = 9000;
             INSERT INTO bmw_long SELECT n, CASE WHEN n BETWEEN 5000 AND 7100 THEN 'w0 w1 w2 w3 w5 w8 end' ELSE
               repeat('w0 ', 1 + n % 3) ||
               CASE WHEN n % 2 = 0 THEN 'w1 ' ELSE '' END ||
               CASE WHEN n % 3 = 0 THEN repeat('w2 ', 1 + n % 4) ELSE '' END ||
               CASE WHEN n % 5 = 0 THEN 'w3 ' ELSE '' END ||
               CASE WHEN n % 7 = 0 THEN repeat('w4 ', 1 + n % 2) ELSE '' END ||
               CASE WHEN n % 11 = 0 THEN 'w5 ' ELSE '' END ||
               CASE WHEN n % 13 = 0 THEN 'w6 ' ELSE '' END ||
               CASE WHEN n % 50 = 0 THEN repeat('w7 ', 1 + n % 3) ELSE '' END ||
               CASE WHEN n % 97 = 0 THEN 'w8 ' ELSE '' END ||
               CASE WHEN n % 200 = 0 THEN 'w9 w9 ' ELSE '' END ||
               CASE WHEN n % 500 = 0 THEN 'w10 ' ELSE '' END ||
               CASE WHEN n BETWEEN 12000 AND 12040 THEN 'w11 ' ELSE '' END ||
               CASE WHEN n % 1000 = 7 THEN repeat('w12 ', 1 + n % 5) ELSE '' END ||
               repeat('pad ', n % 8) || 'end' END
               FROM generate_series(1, 20000) n;
             CREATE INDEX bmw_long_idx ON bmw_long USING stannum(body);
             INSERT INTO bmw_long SELECT n, 'w0 w1 w2 w3 w5 w8 end' FROM generate_series(20001, 20200) n;
             DELETE FROM bmw_long WHERE id % 19 = 0;",
        )
        .unwrap();
        let long = [
            "w0 OR w1 OR w2 OR w3 OR w4 OR w5 OR w6 OR w7 OR w8 OR w9 OR w10 OR w11 OR w12 OR missing",
            "w12 OR w11 OR w10 OR w9 OR w8 OR w7 OR w6 OR w5 OR w4 OR w3",
            "w0 OR w1 OR w2 OR w3 OR w4 OR w5 OR w6 OR w7 OR w8 OR w9 OR w10",
            "w1^2 OR w3 OR w5^0.5 OR w7 OR w9 OR w11 OR w0 OR w2 OR w4 OR w6^3",
            "w2 OR w2 OR w3 OR w4 OR w5 OR w6 OR w7 OR w8 OR w9 OR w10 OR w12",
            "w8 OR w5 OR w3 OR w2 OR w1 OR w0 OR w6 OR w7 OR w9 OR w10",
            // Mixed shapes over the same sieve and required terms.
            "(w0 AND w1) OR w8 OR \"w2 w3\" OR (w9 AND NOT w5) OR w12",
            "\"w0 w1\" OR \"w2 w3\" OR w12 OR w11 OR (w4 AND w6)",
            "w1 AND (w2 OR w3 OR \"w5 w8\") AND NOT w7",
            "AT LEAST 3 OF [w1 w2 w3 w4 w5 w6 \"w0 w1\"]",
        ];
        for query in long {
            for limit in ["LIMIT 1", "LIMIT 3", "LIMIT 10", "LIMIT 100", "LIMIT 1000"] {
                for order_by in [
                    "stannum.full_score(ctid)",
                    "stannum.score(ctid)",
                    "stannum.score(ctid, 1.0)",
                ] {
                    let expected = ranked_in("bmw_long", false, query, order_by, limit);
                    let actual = ranked_in("bmw_long", true, query, order_by, limit);
                    assert_eq!(actual, expected, "{query} {limit} {order_by}");
                }
            }
        }
    }

    /// A conjunction's warm-up evaluates the chunks with the highest
    /// directory bounds first, across every source, and the walks skip them
    /// after. Two build segments of two chunks each (the second chunk of
    /// each holding the shortest documents, so the warm-up picks it) and a
    /// write buffer of tying rows: whether the warm-up covers none, some or
    /// all of the chunks holding the final top k, the rows are the
    /// exhaustive ones bit for bit, ties and dead rows included, across the
    /// LIMIT and OFFSET matrix. Phrases and disjunctions are not warmed.
    #[pg_test]
    fn warmed_top_k_matches_full_scoring_bit_for_bit() {
        // The second segment is the write buffer folded at 70,000 documents,
        // with merges deferred; the rows after it stay in the buffer.
        let rows = |from: i32, to: i32| {
            format!(
                "INSERT INTO warm SELECT n,
                   repeat('alpha ', n % 4) ||
                   CASE WHEN n % 3 = 0 THEN 'beta ' ELSE '' END ||
                   CASE WHEN n % 5 = 0 THEN repeat('gamma ', 1 + n % 2) ELSE '' END ||
                   CASE WHEN n % 101 = 0 THEN repeat('delta ', 1 + (n / 101) % 3) ELSE '' END ||
                   repeat('pad ', CASE WHEN n % 70000 > 65536 THEN n % 3 ELSE 3 + n % 6 END) ||
                   'tail'
                   FROM generate_series({from}, {to}) n;"
            )
        };
        Spi::run(&format!(
            "CREATE TABLE warm(id int primary key, body text);
             {}
             CREATE INDEX warm_idx ON warm USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 70000;
             SET LOCAL stannum.write_buffer_bytes = 67108864;
             SET LOCAL stannum.max_merge_docs = 0;
             SET LOCAL stannum.deferred_merge_docs = 0;
             {}",
            rows(1, 70000),
            rows(70001, 140000)
        ))
        .unwrap();
        Spi::run(
            "INSERT INTO warm SELECT n, 'alpha beta gamma delta tail'
               FROM generate_series(140001, 140300) n;
             INSERT INTO warm SELECT n, 'delta delta delta ' || repeat('pad ', n % 4) || 'tail'
               FROM generate_series(140301, 140400) n;
             DELETE FROM warm WHERE id % 17 = 0;
             UPDATE warm SET body = body || ' extra' WHERE id % 23 = 0;",
        )
        .unwrap();
        let ranked = |custom: bool, warmup: i32, query: &str, order_by: &str, limit: &str| {
            Spi::run(&format!(
                "SET LOCAL stannum.enable_custom_scan = {custom}; SET LOCAL enable_seqscan = off;
                 SET LOCAL stannum.warmup_chunks = {warmup};
                 SET LOCAL stannum.warmup_min_matches = 0;"
            ))
            .unwrap();
            Spi::connect(|client| {
                client
                    .select(
                        &format!(
                            "SELECT id, {order_by} AS score FROM warm WHERE body ==> '{query}'
                             ORDER BY score DESC{} {limit}",
                            if custom { "" } else { ", ctid" }
                        ),
                        None,
                        &[],
                    )
                    .unwrap()
                    .map(|row| {
                        (
                            row.get::<i32>(1).unwrap().unwrap(),
                            row.get::<f32>(2).unwrap().unwrap().to_bits(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
        };
        let queries = [
            "alpha AND delta",
            "beta AND gamma",
            "alpha AND beta AND gamma",
            "gamma AND delta AND tail",
            "alpha AND pad AND tail",
            "delta^2 AND gamma^0.5",
            "alpha OR delta",
            "\"gamma delta\"",
        ];
        let limits = [
            "LIMIT 1",
            "LIMIT 10",
            "LIMIT 10 OFFSET 20",
            "LIMIT 100",
            "LIMIT 257",
        ];
        for query in queries {
            for limit in limits {
                for order_by in [
                    "stannum.full_score(ctid)",
                    "stannum.score(ctid)",
                    "stannum.score(ctid, 1.0)",
                ] {
                    let expected = ranked(false, 0, query, order_by, limit);
                    for warmup in [0, 1, 2, 3, 4096] {
                        assert_eq!(
                            ranked(true, warmup, query, order_by, limit),
                            expected,
                            "{query} {limit} {order_by} warm-up {warmup}"
                        );
                    }
                }
            }
        }
        fn search_scan(node: &serde_json::Value) -> Option<serde_json::Value> {
            if node["Custom Plan Provider"] == "Stannum Text Search Scan" {
                return Some(node.clone());
            }
            node["Plans"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(search_scan)
        }
        Spi::run(
            "SET LOCAL stannum.enable_custom_scan = on; SET LOCAL enable_seqscan = off;
             SET LOCAL enable_bitmapscan = off;",
        )
        .unwrap();
        let layout = Spi::get_one::<String>(
            "SELECT string_agg(kind || ' ' || docs, ', ' ORDER BY ordinal)
             FROM stannum.segment_info('warm_idx')",
        )
        .unwrap()
        .unwrap();
        // `alpha`, `beta` and `gamma` are in every chunk of ordinals of
        // every source.
        let every = Spi::get_one::<i64>(
            "SELECT sum((docs + 65535) / 65536)::bigint FROM stannum.segment_info('warm_idx')",
        )
        .unwrap()
        .unwrap();
        assert!(every >= 4, "{layout}");
        assert_eq!(layout.matches("immutable").count(), 2, "{layout}");
        assert!(layout.contains(", mutable "), "{layout}");
        // A conjunction whose estimated matches fall below the bar per row
        // is not warmed up; phrases and disjunctions never are.
        for (query, warmup, bar, chunks) in [
            ("alpha AND beta AND gamma", 0, 0.0, None),
            ("alpha AND beta AND gamma", 3, 0.0, Some(3)),
            ("alpha AND beta AND gamma", 4096, 4.0, Some(every)),
            ("alpha AND beta AND gamma", 4096, 1e6, None),
            ("alpha OR delta", 4096, 0.0, None),
            ("\"alpha beta\"", 4096, 0.0, None),
        ] {
            Spi::run(&format!(
                "SET LOCAL stannum.warmup_chunks = {warmup};
                 SET LOCAL stannum.warmup_min_matches = {bar};"
            ))
            .unwrap();
            let plan = Spi::get_one::<Json>(&format!(
                "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM warm WHERE body ==> '{query}'
                 ORDER BY stannum.full_score(ctid) DESC LIMIT 10"
            ))
            .unwrap()
            .unwrap()
            .0;
            let scan = search_scan(&plan[0]["Plan"]).unwrap_or_else(|| panic!("{plan}"));
            assert_eq!(scan["Pruning"], "ordinal", "{query}: {scan}");
            assert_eq!(
                scan["Warm-up Chunks"].as_i64(),
                chunks,
                "{query} {warmup} {bar} over {layout}: {scan}"
            );
            assert_eq!(
                scan["Warm-up Estimate"].is_number(),
                warmup > 0 && query.contains(" AND "),
                "{query}: {scan}"
            );
        }
    }

    #[pg_test]
    fn warm_up_passes_read_in_place_only_through_their_own_hold_span() {
        // One segment of three chunks of ordinals. `zeta` and `eta` are in
        // the first two, `theta` in every document. In the first chunk
        // every document is short, so it bounds highest and is the one
        // warmed; there `zeta` and `eta` never meet, so the warm pass loads
        // their chunks in place and never `theta`'s. The walk then loads
        // `zeta` and `eta` again in the second chunk, and `theta` for the
        // first time: a term may read in place only through slots of the
        // hold span it runs in, and `theta`'s fresh slots are the numbers
        // the warm pass handed the others.
        Spi::run(
            "CREATE TABLE warm_span(id int primary key, body text);
             INSERT INTO warm_span SELECT n,
               CASE WHEN n <= 65536 THEN
                 CASE n % 500 WHEN 0 THEN 'zeta theta' WHEN 250 THEN 'eta theta'
                   ELSE 'theta' END
               ELSE
                 'theta pad pad pad pad pad pad pad pad' ||
                 CASE WHEN n > 131072 THEN ''
                   WHEN n % 500 = 0 THEN ' zeta eta'
                   WHEN n % 500 = 250 THEN ' eta' ELSE '' END
               END
               FROM generate_series(1, 150000) n;
             SET LOCAL stannum.build_segment_docs = 200000;
             CREATE INDEX warm_span_idx ON warm_span USING stannum(body);",
        )
        .unwrap();
        let layout = Spi::get_one::<String>(
            "SELECT string_agg(kind || ' ' || docs, ', ' ORDER BY ordinal)
             FROM stannum.segment_info('warm_span_idx')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(layout, "immutable 150000", "{layout}");
        let query = "zeta AND eta AND theta";
        let ranked = |custom: bool, warmup: i32, order_by: &str| {
            Spi::run(&format!(
                "SET LOCAL stannum.enable_custom_scan = {custom}; SET LOCAL enable_seqscan = off;
                 SET LOCAL enable_bitmapscan = {};
                 SET LOCAL stannum.warmup_chunks = {warmup};
                 SET LOCAL stannum.warmup_min_matches = 0;",
                !custom
            ))
            .unwrap();
            Spi::connect(|client| {
                client
                    .select(
                        &format!(
                            "SELECT id, {order_by} AS score FROM warm_span
                             WHERE body ==> '{query}'
                             ORDER BY score DESC{} LIMIT 10",
                            if custom { "" } else { ", ctid" }
                        ),
                        None,
                        &[],
                    )
                    .unwrap()
                    .map(|row| {
                        (
                            row.get::<i32>(1).unwrap().unwrap(),
                            row.get::<f32>(2).unwrap().unwrap().to_bits(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
        };
        for order_by in [
            "stannum.full_score(ctid)",
            "stannum.score(ctid)",
            "stannum.score(ctid, 1.0)",
        ] {
            let expected = ranked(false, 0, order_by);
            assert_eq!(expected.len(), 10, "{order_by}");
            for warmup in [0, 1, 2] {
                assert_eq!(
                    ranked(true, warmup, order_by),
                    expected,
                    "{order_by} warm-up {warmup}"
                );
            }
        }
        // The first chunk alone was warmed, so the walk had the second.
        fn search_scan(node: &serde_json::Value) -> Option<serde_json::Value> {
            if node["Custom Plan Provider"] == "Stannum Text Search Scan" {
                return Some(node.clone());
            }
            node["Plans"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(search_scan)
        }
        Spi::run(
            "SET LOCAL stannum.enable_custom_scan = on; SET LOCAL enable_bitmapscan = off;
             SET LOCAL stannum.warmup_chunks = 1;",
        )
        .unwrap();
        let plan = Spi::get_one::<Json>(&format!(
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM warm_span WHERE body ==> '{query}'
             ORDER BY stannum.score(ctid, 1.0) DESC LIMIT 10"
        ))
        .unwrap()
        .unwrap()
        .0;
        let scan = search_scan(&plan[0]["Plan"]).unwrap_or_else(|| panic!("{plan}"));
        assert_eq!(scan["Pruning"], "ordinal", "{scan}");
        assert_eq!(scan["Warm-up Chunks"].as_i64(), Some(1), "{scan}");
    }

    #[pg_test]
    fn ranked_work_counters_distinguish_filtered_completion() {
        Spi::run(
            "CREATE TABLE ranked_work(id int, body text);
             INSERT INTO ranked_work SELECT n, 'alpha beta' FROM generate_series(1, 1000) n;
             CREATE INDEX ranked_work_idx ON ranked_work USING stannum(body);
             SET LOCAL stannum.enable_custom_scan = on;
             SET LOCAL enable_seqscan = off;
             SET LOCAL enable_bitmapscan = off;",
        )
        .unwrap();
        fn search_scan(node: &serde_json::Value) -> Option<&serde_json::Value> {
            if node["Custom Plan Provider"] == "Stannum Text Search Scan" {
                return Some(node);
            }
            node["Plans"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(search_scan)
        }
        let explain_by = |query: &str, filter: &str, order_by: &str| {
            Spi::get_one::<Json>(&format!(
                "EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM ranked_work
                 WHERE body ==> '{query}' {filter}
                 ORDER BY {order_by} DESC LIMIT 10"
            ))
            .unwrap()
            .unwrap()
            .0
        };
        let explain =
            |query: &str, filter: &str| explain_by(query, filter, "stannum.full_score(ctid)");
        let ordinary = explain("alpha", "");
        let scan = search_scan(&ordinary[0]["Plan"]).unwrap();
        assert_eq!(scan["Pruning"], "ordinal");
        assert_eq!(scan["Exhaustive Score Calls"], 0);
        assert_eq!(scan["Top-K Completions"], 0);

        // Equal scores put the first ten physical rows in the pruned prefix.
        // None passes the SQL filter, so the pruned search deepens to 40, 160,
        // 640 and 2,560 rows; the last holds all 1,000 and the ten that pass.
        let filtered = explain("alpha", "AND id > 990");
        let scan = search_scan(&filtered[0]["Plan"]).unwrap();
        assert_eq!(scan["Pruning"], "ordinal");
        assert_eq!(scan["Top-K Completions"], 4);
        assert_eq!(scan["Exhaustive Score Calls"], 0);
        assert_eq!(filtered[0]["Plan"]["Actual Rows"].as_f64(), Some(10.0));

        // A phrase is pruned like the conjunction of its words, reading
        // positions only for the candidates that enter its top ten.
        let phrase = explain("\"alpha beta\"", "");
        let scan = search_scan(&phrase[0]["Plan"]).unwrap();
        assert_eq!(scan["Pruning"], "ordinal", "{scan}");
        assert_eq!(scan["Top-K Completions"], 0);
        assert_eq!(scan["Exhaustive Score Calls"], 0);
        assert_eq!(scan["Positions Checked"], 10, "{scan}");

        // A span shape that does not need every word scores exhaustively
        // without a completion.
        let span = explain("\"alpha beta\" NOT ENCLOSES \"gamma\"", "");
        let scan = search_scan(&span[0]["Plan"]).unwrap();
        assert!(scan["Pruning"].is_null(), "{scan}");
        assert_eq!(scan["Top-K Completions"], 0);
        assert_eq!(scan["Exhaustive Score Calls"], 1000);

        // `score` elides both words, every document holding them. The phrase
        // is still walked by ordinal, as a conjunction of filters: nothing
        // is scored, the first ten matches in heap order are the top ten,
        // and only their positions are read.
        let elided = explain_by("\"alpha beta\"", "", "stannum.score(ctid)");
        let scan = search_scan(&elided[0]["Plan"]).unwrap();
        assert_eq!(scan["Pruning"], "ordinal", "{scan}");
        assert_eq!(scan["Scored Candidates"], 0, "{scan}");
        assert_eq!(scan["Positions Checked"], 10, "{scan}");
        assert_eq!(scan["Top-K Completions"], 0);
        assert_eq!(scan["Exhaustive Score Calls"], 0);
        assert_eq!(elided[0]["Plan"]["Actual Rows"].as_f64(), Some(10.0));
        // Filtered, the unscored walk deepens through the same completions
        // as the scored one, and still reads positions for the matches it
        // admits alone: 10 + 40 + 160 + 640 + 1,000.
        let elided = explain_by("\"alpha beta\"", "AND id > 990", "stannum.score(ctid)");
        let scan = search_scan(&elided[0]["Plan"]).unwrap();
        assert_eq!(scan["Pruning"], "ordinal", "{scan}");
        assert_eq!(scan["Top-K Completions"], 4, "{scan}");
        assert_eq!(scan["Exhaustive Score Calls"], 0);
        assert_eq!(scan["Positions Checked"], 1850, "{scan}");
        assert_eq!(elided[0]["Plan"]["Actual Rows"].as_f64(), Some(10.0));
        // A single elided word has no conjunction to walk and is read from
        // the candidate stream.
        let single = explain_by("alpha", "", "stannum.score(ctid)");
        let scan = search_scan(&single[0]["Plan"]).unwrap();
        assert_eq!(scan["Pruning"], "block-max", "{scan}");
        assert_eq!(scan["Scored Candidates"], 0, "{scan}");
    }

    #[pg_test]
    fn filtered_literal_prefix_rescans_preserve_consumed_rows() {
        Spi::run(
            "CREATE TABLE prefix_rescan(id int primary key, body text);
             INSERT INTO prefix_rescan SELECT n, 'alpha beta'
               FROM generate_series(1, 1000) n;
             CREATE INDEX prefix_rescan_idx ON prefix_rescan USING stannum(body);
             SET LOCAL enable_seqscan = off;
             SET LOCAL enable_material = off;
             SET LOCAL enable_memoize = off;",
        )
        .unwrap();
        fn ranked_scan(node: &serde_json::Value) -> Option<&serde_json::Value> {
            if node["Custom Plan Provider"] == "Stannum Text Search Scan" {
                return Some(node);
            }
            node["Plans"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(ranked_scan)
        }
        for filter in ["id % 4 = 0", "id % 100 = 0 OR id <= 2"] {
            Spi::run(
                "SET LOCAL stannum.enable_custom_scan = off;
                 SET LOCAL enable_bitmapscan = on;",
            )
            .unwrap();
            let expected: Vec<(i32, u32)> = Spi::connect(|client| {
                client
                    .select(
                        &format!(
                            "SELECT id, stannum.full_score(ctid) AS score FROM prefix_rescan
                             WHERE body ==> 'alpha' AND ({filter})
                             ORDER BY score DESC, id LIMIT 10"
                        ),
                        None,
                        &[],
                    )
                    .unwrap()
                    .map(|row| {
                        (
                            row.get::<i32>(1).unwrap().unwrap(),
                            row.get::<f32>(2).unwrap().unwrap().to_bits(),
                        )
                    })
                    .collect()
            });
            Spi::run(
                "SET LOCAL stannum.enable_custom_scan = on;
                 SET LOCAL enable_bitmapscan = off;",
            )
            .unwrap();
            // The outer reference is above the relation scan; the constant
            // ranked scan itself must rewind on each nested-loop iteration.
            let sql = format!(
                "SELECT s.id, s.score, s.iteration FROM generate_series(1,3) g
                 CROSS JOIN LATERAL (SELECT id, stannum.full_score(ctid) AS score,
                     g AS iteration FROM prefix_rescan
                     WHERE body ==> 'alpha' AND ({filter})
                     ORDER BY score DESC LIMIT 10) s"
            );
            let plan = Spi::get_one::<Json>(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}"))
                .unwrap()
                .unwrap()
                .0;
            let scan = ranked_scan(&plan[0]["Plan"]).expect("ranked custom scan");
            assert_eq!(scan["Actual Loops"].as_u64(), Some(3), "{plan}");
            assert_eq!(scan["Top K"], 10, "{plan}");
            let mut actual = [Vec::new(), Vec::new(), Vec::new()];
            Spi::connect(|client| {
                for row in client.select(&sql, None, &[]).unwrap() {
                    let iteration = row.get::<i32>(3).unwrap().unwrap();
                    actual[(iteration - 1) as usize].push((
                        row.get::<i32>(1).unwrap().unwrap(),
                        row.get::<f32>(2).unwrap().unwrap().to_bits(),
                    ));
                }
            });
            for (iteration, rows) in actual.iter().enumerate() {
                assert_eq!(rows, &expected, "{filter}, iteration {iteration}");
            }
        }
    }

    #[pg_test]
    fn segment_info_reports_segments_and_the_write_buffer() {
        Spi::run(
            "CREATE TABLE si(id int primary key, body text);
             INSERT INTO si SELECT n, 'w' || (n % 5) || ' common' FROM generate_series(1, 50) n;
             CREATE INDEX si_idx ON si USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 4;
             INSERT INTO si SELECT n, 'late needle' FROM generate_series(100, 109) n;
             DELETE FROM si WHERE id <= 10;",
        )
        .unwrap();
        let rows = Spi::connect(|client| {
            client
                .select(
                    "SELECT kind, docs, dead_docs, sum_doc_lengths, total_pages
                     FROM stannum.segment_info('si_idx') ORDER BY ordinal",
                    None,
                    &[],
                )
                .unwrap()
                .map(|row| {
                    (
                        row.get::<String>(1).unwrap().unwrap(),
                        row.get::<i64>(2).unwrap().unwrap(),
                        row.get::<i64>(3).unwrap().unwrap(),
                        row.get::<i64>(4).unwrap().unwrap(),
                        row.get::<i64>(5).unwrap().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        });
        // The build segment, two folded segments of four, and two buffered.
        assert_eq!(rows[0], ("immutable".into(), 50, 0, 100, 1));
        assert_eq!(rows[1].0, "immutable");
        assert_eq!(rows[1].1, 4);
        assert_eq!(rows[2].1, 4);
        assert_eq!(rows.last().unwrap().0, "mutable");
        assert_eq!(rows.last().unwrap().1, 2);
        assert_eq!(rows.len(), 4);
        // Dead documents only appear after VACUUM reports them.
        assert!(rows.iter().all(|row| row.2 == 0));
    }

    #[pg_test]
    fn posting_inserts_rolled_back_by_subtransaction_are_not_visible() {
        Spi::run(
            "CREATE TABLE posting_abort(body text);
          CREATE INDEX posting_abort_idx ON posting_abort USING stannum(body);
          DO $$ BEGIN
            INSERT INTO posting_abort VALUES ('aborted');
            RAISE EXCEPTION 'abort subtransaction';
          EXCEPTION WHEN raise_exception THEN NULL; END $$;
          INSERT INTO posting_abort VALUES ('committed');
          SET LOCAL enable_seqscan=off;",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM posting_abort WHERE body ==> 'aborted'")
                .unwrap(),
            Some(0)
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM posting_abort WHERE body ==> 'committed'")
                .unwrap(),
            Some(1)
        );
    }

    #[pg_test]
    fn unlogged_indexes_have_physical_storage() {
        Spi::run(
            "CREATE UNLOGGED TABLE posting_unlogged(body text);
          INSERT INTO posting_unlogged VALUES ('beer');
          CREATE INDEX posting_unlogged_idx ON posting_unlogged USING stannum(body);
          SET LOCAL enable_seqscan=off;",
        )
        .unwrap();
        assert!(
            Spi::get_one::<i64>("SELECT pg_relation_size('posting_unlogged_idx')")
                .unwrap()
                .unwrap()
                >= 2 * 8192
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM posting_unlogged WHERE body ==> 'beer'")
                .unwrap(),
            Some(1)
        );
    }

    #[pg_test]
    fn bitmap_scan_follows_heap_growth_and_truncate() {
        Spi::run(
            "CREATE TABLE lite_growth (id int, body text);
             CREATE INDEX lite_growth_idx ON lite_growth USING stannum (body);
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM lite_growth WHERE body ==> 'beer'").unwrap(),
            Some(0)
        );
        Spi::run(
            "INSERT INTO lite_growth
               SELECT n, CASE WHEN n % 50 = 0 THEN 'beer' ELSE 'wine' END
                         || repeat(' filler', 80)
               FROM generate_series(1, 400) AS n;",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM lite_growth WHERE body ==> 'beer'").unwrap(),
            Some(8)
        );
        Spi::run(
            "TRUNCATE lite_growth;
             INSERT INTO lite_growth VALUES (1, 'beer'), (2, 'wine');",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM lite_growth WHERE body ==> 'beer'").unwrap(),
            Some(1)
        );
    }

    #[pg_test]
    fn bitmap_scan_rechecks_partial_index_predicates_and_expressions() {
        Spi::run(
            "CREATE TABLE lite_partial (id int, body text, active boolean);
             INSERT INTO lite_partial VALUES
               (1, 'BEER', true), (2, 'wine', true),
               (3, 'BEER', false), (4, NULL, true);
             CREATE INDEX lite_partial_idx ON lite_partial
               USING stannum (lower(body)) WHERE active;
             SET LOCAL enable_seqscan = off; SET LOCAL stannum.enable_custom_scan = off;",
        )
        .unwrap();
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (FORMAT JSON)
             SELECT id FROM lite_partial WHERE active AND lower(body) ==> 'beer'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(plan[0]["Plan"]["Node Type"], "Bitmap Heap Scan");
        assert_eq!(
            plan[0]["Plan"]["Plans"][0]["Index Name"],
            "lite_partial_idx"
        );
        assert_eq!(
            Spi::get_one::<Vec<i32>>(
                "SELECT array_agg(id ORDER BY id) FROM lite_partial
                 WHERE active AND lower(body) ==> 'beer'"
            )
            .unwrap(),
            Some(vec![1])
        );
        Spi::run("UPDATE lite_partial SET active = true WHERE id = 3").unwrap();
        assert_eq!(
            Spi::get_one::<Vec<i32>>(
                "SELECT array_agg(id ORDER BY id) FROM lite_partial
                 WHERE active AND lower(body) ==> 'beer'"
            )
            .unwrap(),
            Some(vec![1, 3])
        );
    }

    #[pg_test]
    fn bitmap_union_rechecks_both_search_predicates() {
        Spi::run(
            "CREATE TABLE lite_union (id int, title text, body text);
             INSERT INTO lite_union VALUES
               (1, 'beer', 'wine'), (2, 'wine', 'beer'),
               (3, 'beer', 'beer'), (4, 'wine', 'wine');
             CREATE INDEX lite_union_title_idx ON lite_union USING stannum (title);
             CREATE INDEX lite_union_body_idx ON lite_union USING stannum (body);
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (FORMAT JSON)
             SELECT id FROM lite_union WHERE title ==> 'beer' OR body ==> 'beer'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(plan[0]["Plan"]["Plans"][0]["Node Type"], "BitmapOr");
        assert_eq!(
            Spi::get_one::<Vec<i32>>(
                "SELECT array_agg(id ORDER BY id) FROM lite_union
                 WHERE title ==> 'beer' OR body ==> 'beer'"
            )
            .unwrap(),
            Some(vec![1, 2, 3])
        );
    }

    #[pg_test]
    fn heap_mvcc_owns_updates_and_deletes() {
        Spi::run(
            "CREATE TABLE lite_mvcc (id int, body text);
             INSERT INTO lite_mvcc VALUES (1, 'old term'), (2, 'keep term');
             CREATE INDEX lite_mvcc_idx ON lite_mvcc USING stannum (body);
             UPDATE lite_mvcc SET body = 'new term' WHERE id = 1;
             DELETE FROM lite_mvcc WHERE id = 2;
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM lite_mvcc WHERE body ==> 'old'").unwrap(),
            Some(0)
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM lite_mvcc WHERE body ==> 'new'").unwrap(),
            Some(1)
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM lite_mvcc WHERE body ==> 'keep'").unwrap(),
            Some(0)
        );
        Spi::run("UPDATE lite_mvcc SET id = 3 WHERE id = 1").unwrap();
        assert_eq!(
            Spi::get_one::<Vec<i32>>("SELECT array_agg(id) FROM lite_mvcc WHERE body ==> 'new'")
                .unwrap(),
            Some(vec![3])
        );
    }

    #[pg_test]
    fn scoring_rewrite_orders_matching_rows() {
        Spi::run(
            "CREATE TABLE lite_score (id int, body text);
             INSERT INTO lite_score VALUES
               (1, 'rare'), (2, 'rare rare rare'), (3, 'common');
             CREATE INDEX lite_score_idx ON lite_score USING stannum (body);",
        )
        .unwrap();
        let ids = Spi::get_one::<Vec<i32>>(
            "SELECT array_agg(id ORDER BY stannum.full_score(ctid) DESC, id)
             FROM lite_score WHERE body ==> 'rare'",
        )
        .unwrap();
        assert_eq!(ids, Some(vec![2, 1]));
    }

    #[pg_test]
    fn scoring_helpers_share_the_same_policy() {
        Spi::run(
            "CREATE TABLE lite_score_helpers (id int, body text);
             INSERT INTO lite_score_helpers VALUES
               (1, 'common rare'), (2, 'common'), (3, 'common');
             CREATE INDEX lite_score_helpers_idx ON lite_score_helpers USING stannum (body)",
        )
        .unwrap();
        let full_max = Spi::get_one::<f32>(
            "SELECT max(stannum.full_score(ctid))
             FROM lite_score_helpers WHERE body ==> 'rare^1.0'",
        )
        .unwrap()
        .unwrap();
        let reported = Spi::get_one::<f32>(
            "SELECT stannum.max_score(ctid)
             FROM lite_score_helpers WHERE body ==> 'rare^1.0' LIMIT 1",
        )
        .unwrap()
        .unwrap();
        assert_eq!(reported, full_max);
        let inspected = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(term ORDER BY term)
             FROM stannum.score_inspect('lite_score_helpers_idx', 'common OR rare', 0.5)",
        )
        .unwrap();
        assert_eq!(inspected, Some(vec!["rare".to_owned()]));
    }

    #[pg_test]
    fn full_score_normalization_matches_tin() {
        // Exercise the custom scan, bitmap scan, and heap-reference scorer.
        for (table_kind, custom_scan) in [("", "on"), ("", "off"), ("TEMP", "off")] {
            Spi::run(&format!(
                "SET LOCAL stannum.enable_custom_scan = {custom_scan};
                 SET LOCAL enable_seqscan = off;
                 CREATE {table_kind} TABLE lite_normalization (id int, body text);
                 INSERT INTO lite_normalization VALUES
                   (1, 'I love fuji apples and juicy mangoes'),
                   (2, 'Grape tasting notes from the orchard'),
                   (3, 'The best juicy fuji apple in town');
                 CREATE INDEX lite_normalization_idx ON lite_normalization USING stannum (body)"
            ))
            .unwrap();
            for expression in [
                "stannum.full_score(ctid) / stannum.max_score(ctid)",
                "1::real / stannum.max_score(ctid) * stannum.full_score(ctid)",
            ] {
                let sql = format!(
                    "SELECT {expression} FROM lite_normalization
                     WHERE body ==> 'apple OR grape' AND stannum.max_score(ctid) > 0 ORDER BY id"
                );
                let scores = Spi::connect(|client| {
                    client
                        .select(&sql, None, &[])
                        .unwrap()
                        .map(|row| row.get::<f32>(1).unwrap().unwrap())
                        .collect::<Vec<_>>()
                });
                assert_eq!(scores.len(), 2);
                assert!((scores[0] - 1.0).abs() < 0.000001);
                assert!((scores[1] - 0.9398665).abs() < 0.000001);
            }
            Spi::run("DROP TABLE lite_normalization").unwrap();
        }
    }

    #[pg_test(error = "canceling statement due to user request")]
    fn document_tokenization_honors_cancellation_between_rows() {
        let documents = vec!["beer wine".to_owned(); 100];
        let mut rows = 0;
        crate::score::tokenize_documents(&documents, |_| {
            rows += 1;
            // Queue the same flags as a query-cancel signal after work starts.
            // Without an in-loop check, the explicit panic below fails the test.
            if rows == 1 {
                unsafe {
                    pg_sys::QueryCancelPending = 1;
                    pg_sys::InterruptPending = 1;
                }
            }
        });
        panic!("document tokenization ignored cancellation");
    }

    // Adapted from PlanetScale Lead fdffe7b; exercise the explicit heap scorer
    // too, since temporary indexes now use Stannum's indexed storage path.
    #[pg_test]
    fn max_score_excludes_nonmatching_documents() {
        for (name, matching, nonmatching, query) in [
            (
                "boolean",
                "beer wine",
                "beer beer beer beer beer",
                "beer^1 AND wine^0",
            ),
            (
                "phrase",
                "beer beer wine",
                "beer noise beer noise beer noise beer noise wine",
                "\"beer beer\"^1 AND wine^0",
            ),
            (
                "positional",
                "beer wine",
                "wine beer beer beer beer beer",
                "beer BEFORE wine^0",
            ),
        ] {
            for partial in [false, true] {
                let predicate = if partial { "WHERE active" } else { "" };
                Spi::run(&format!(
                    "CREATE TABLE max_match (body text, active boolean);
                     INSERT INTO max_match VALUES ('{matching}', true), ('{nonmatching}', true);
                     CREATE INDEX max_match_idx ON max_match USING stannum (body) {predicate};"
                ))
                .unwrap();
                if partial {
                    Spi::run("INSERT INTO max_match VALUES ('beer beer beer beer wine', false)")
                        .unwrap();
                }
                let (score, max) = Spi::get_two::<f32, f32>(&format!(
                    "SELECT stannum.full_score(ctid), stannum.max_score(ctid)
                     FROM max_match WHERE active AND body ==> '{query}'"
                ))
                .unwrap();
                assert!(
                    score.is_some_and(|score| score > 0.0),
                    "{name}, partial={partial}"
                );
                assert_eq!(max, score, "indexed {name}, partial={partial}");
                let (score, max) = Spi::get_two::<f32, f32>(&format!(
                    "SELECT stannum.score_bound(body, '{query}',
                         'max_match'::regclass::oid::int, 'max_match_idx'::regclass::oid::int,
                         1, NULL, NULL, NULL, NULL, NULL),
                        stannum.score_bound(body, '{query}',
                         'max_match'::regclass::oid::int, 'max_match_idx'::regclass::oid::int,
                         3, NULL, NULL, NULL, NULL, NULL)
                     FROM max_match WHERE active AND body ==> '{query}'"
                ))
                .unwrap();
                assert!(
                    score.is_some_and(|score| score > 0.0),
                    "{name}, partial={partial}"
                );
                assert_eq!(max, score, "fallback {name}, partial={partial}");
                Spi::run("DROP TABLE max_match").unwrap();
            }
        }
    }

    #[pg_test]
    fn ranked_exists_filters_preserve_scores() {
        Spi::run("CREATE TABLE exists_docs(id int PRIMARY KEY, body text);
            INSERT INTO exists_docs SELECT n, CASE WHEN n % 3 = 0 THEN 'alpha beta beta' ELSE 'alpha gamma' END FROM generate_series(1,120) n;
            CREATE INDEX exists_docs_search ON exists_docs USING stannum(body);
            CREATE TABLE exists_allowed(id int PRIMARY KEY);
            INSERT INTO exists_allowed SELECT n FROM generate_series(1,120) n WHERE n % 7 = 0;
            ANALYZE exists_docs; ANALYZE exists_allowed;
            CREATE TEMP TABLE exists_reference AS SELECT id, stannum.full_score(ctid) AS score FROM exists_docs WHERE body ==> 'alpha OR beta';").unwrap();
        for filter in [
            "EXISTS (SELECT 1 FROM exists_allowed a WHERE a.id=d.id)",
            "NOT EXISTS (SELECT 1 FROM exists_allowed a WHERE a.id=d.id)",
            "EXISTS (SELECT 1 FROM exists_allowed a WHERE a.id=d.id) AND NOT EXISTS (SELECT 1 FROM exists_allowed a WHERE a.id=d.id+1)",
            // The inner alias has the same name, but belongs to another query.
            "EXISTS (SELECT 1 FROM exists_docs d WHERE body ==> 'gamma' OFFSET 0)",
        ] {
            for custom in ["on", "off"] {
                Spi::run(&format!("SET LOCAL stannum.enable_custom_scan={custom}")).unwrap();
                let actual = Spi::get_one::<pgrx::JsonB>(&format!("SELECT jsonb_agg(to_jsonb(s) ORDER BY score DESC,id) FROM (SELECT d.id,stannum.full_score(d.ctid) AS score FROM exists_docs d WHERE body ==> 'alpha OR beta' AND {filter} ORDER BY score DESC,id LIMIT 10) s")).unwrap();
                let expected = Spi::get_one::<pgrx::JsonB>(&format!("SELECT jsonb_agg(to_jsonb(s) ORDER BY score DESC,id) FROM (SELECT d.id,d.score FROM exists_reference d WHERE {filter} ORDER BY score DESC,id LIMIT 10) s")).unwrap();
                let expected = expected.map(|v| v.0);
                assert_eq!(actual.map(|v| v.0), expected, "{filter}, custom={custom}");
                for mode in ["force_custom_plan", "force_generic_plan"] {
                    Spi::run(&format!("SET LOCAL plan_cache_mode={mode};
                        PREPARE exists_ranked(text) AS SELECT jsonb_agg(to_jsonb(s) ORDER BY score DESC,id)
                        FROM (SELECT d.id,stannum.full_score(d.ctid) AS score FROM exists_docs d
                        WHERE body ==> $1 AND {filter} ORDER BY score DESC,id LIMIT 10) s")).unwrap();
                    let prepared =
                        Spi::get_one::<pgrx::JsonB>("EXECUTE exists_ranked('alpha OR beta')")
                            .unwrap()
                            .map(|v| v.0);
                    assert_eq!(prepared, expected, "{filter}, custom={custom}, plan={mode}");
                    Spi::run("DEALLOCATE exists_ranked").unwrap();
                }
            }
        }
    }

    #[pg_test]
    fn scoring_binds_to_expression_indexes() {
        Spi::run(
            "CREATE TABLE lite_expression_score (id int, s1 text, s2 text);
             INSERT INTO lite_expression_score VALUES
               (1, 'hello', 'world 10'),
               (2, 'hello hello', 'world 10'),
               (3, 'unrelated', 'document');
             INSERT INTO lite_expression_score
               SELECT n, 'noise', n::text FROM generate_series(4, 30) AS n;
             CREATE INDEX lite_expression_score_idx ON lite_expression_score
               USING stannum (((s1 || ' '::text) || s2));",
        )
        .unwrap();
        let rows = Spi::connect(|client| {
            client
                .select(
                    "SELECT id, stannum.score(ctid) AS score
                     FROM lite_expression_score
                     WHERE (s1 || ' ' || s2) ==> 'hello world 10'
                     ORDER BY score DESC, id LIMIT 5",
                    None,
                    &[],
                )
                .unwrap()
                .map(|row| {
                    (
                        row.get::<i32>(1).unwrap().unwrap(),
                        row.get::<f32>(2).unwrap().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [2, 1]);
        assert!(rows[0].1 > rows[1].1);
    }

    #[pg_test]
    fn scoring_and_inspection_respect_partial_index_predicates() {
        // Partial populations must agree for indexed and fallback scoring.
        for table_kind in ["", "TEMP"] {
            Spi::run(&format!(
                "CREATE {table_kind} TABLE lite_partial_score (id int, body text, active boolean);
                 INSERT INTO lite_partial_score VALUES
                   (1, 'beer', true), (2, 'wine', true),
                   (3, 'wine', NULL), (4, NULL, true);
                 INSERT INTO lite_partial_score
                   SELECT n, 'wine', false FROM generate_series(5, 104) AS n;
                 CREATE INDEX lite_partial_score_idx ON lite_partial_score
                   USING stannum (body) WHERE active;
                 CREATE {table_kind} TABLE lite_partial_score_control AS
                   SELECT id, body FROM lite_partial_score WHERE active;
                 CREATE INDEX lite_partial_score_control_idx ON lite_partial_score_control
                   USING stannum (body);"
            ))
            .unwrap();
            let partial = Spi::get_one::<f32>(
                "SELECT stannum.full_score(ctid) FROM lite_partial_score
                 WHERE active AND body ==> 'beer'",
            )
            .unwrap()
            .unwrap();
            let control = Spi::get_one::<f32>(
                "SELECT stannum.full_score(ctid) FROM lite_partial_score_control
                 WHERE body ==> 'beer'",
            )
            .unwrap()
            .unwrap();
            assert!(control > 0.0);
            assert_eq!(partial, control);

            // In the indexed population, beer occurs in half the documents and
            // must be elided at the default dense ratio, despite the excluded rows.
            assert_eq!(
                Spi::get_one::<i64>(
                    "SELECT count(*) FROM stannum.score_inspect('lite_partial_score_idx', 'beer')"
                )
                .unwrap(),
                Some(0)
            );
            assert_eq!(
                Spi::get_one::<f32>(
                    "SELECT stannum.score(ctid) FROM lite_partial_score
                     WHERE active AND body ==> 'beer'"
                )
                .unwrap(),
                Some(0.0)
            );
            Spi::run("DROP TABLE lite_partial_score, lite_partial_score_control").unwrap();
        }
    }

    #[pg_test]
    fn scoring_respects_partial_expression_index_predicates() {
        // Partial populations must agree for indexed and fallback scoring.
        for table_kind in ["", "TEMP"] {
            Spi::run(
                &format!("CREATE {table_kind} TABLE lite_partial_expression (id int, body text, active boolean);
                 INSERT INTO lite_partial_expression VALUES
                   (1, 'BEER', true), (2, 'wine wine', true),
                   (3, 'BEER BEER', false), (4, 'excluded', false),
                   (5, 'excluded', NULL), (6, NULL, true);
                 CREATE INDEX lite_partial_expression_idx ON lite_partial_expression
                   USING stannum (lower(body)) WHERE active OR id = 3;
                 CREATE {table_kind} TABLE lite_partial_expression_control AS
                   SELECT id, body FROM lite_partial_expression WHERE active OR id = 3;
                 CREATE INDEX lite_partial_expression_control_idx
                   ON lite_partial_expression_control USING stannum (lower(body));"),
            )
            .unwrap();
            let partial = Spi::get_one::<Vec<f32>>(
                "SELECT array_agg(stannum.full_score(ctid) ORDER BY id)
                 FROM lite_partial_expression
                 WHERE (active OR id = 3) AND lower(body) ==> 'beer'",
            )
            .unwrap()
            .unwrap();
            let control = Spi::get_one::<Vec<f32>>(
                "SELECT array_agg(stannum.full_score(ctid) ORDER BY id)
                 FROM lite_partial_expression_control WHERE lower(body) ==> 'beer'",
            )
            .unwrap()
            .unwrap();
            assert_eq!(control.len(), 2);
            assert_eq!(partial, control);
            Spi::run("DROP TABLE lite_partial_expression, lite_partial_expression_control")
                .unwrap();
        }
    }

    #[pg_test]
    fn highlighting_supports_explicit_and_implicit_queries() {
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT stannum.highlight('Beer and wine', '[', ']', query => 'beer')"
            )
            .unwrap(),
            Some("[Beer] and wine".into())
        );
        Spi::run(
            "CREATE TABLE lite_highlight (id int, s1 text, s2 text);
             INSERT INTO lite_highlight VALUES
               (1, 'Beer', 'and wine'), (2, 'cider', 'only');
             CREATE INDEX lite_highlight_idx ON lite_highlight
               USING stannum (((s1 || ' '::text) || s2));",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT stannum.highlight(s1 || ' ' || s2)
                 FROM lite_highlight
                 WHERE (s1 || ' ' || s2) ==> 'beer'"
            )
            .unwrap(),
            Some("<b>Beer</b> and wine".into())
        );
        let ansi = Spi::get_one::<String>(
            "SELECT stannum.highlight_ansi(s1 || ' ' || s2)
             FROM lite_highlight
             WHERE (s1 || ' ' || s2) ==> 'beer'",
        )
        .unwrap()
        .unwrap();
        assert!(ansi.contains("\x1b["));
        assert!(ansi.contains("Beer"));
    }

    /// A corpus with known term frequencies: 4000 rows where `seven` and
    /// `five` are independent (every 7th and 5th row), `needle` is rare,
    /// `alpha beta` is a phrase on every 100th row and `beta` alone on
    /// another 40 rows. Returns nothing; the table is `est`.
    fn known_frequency_fixture() {
        Spi::run(
            "CREATE TABLE est(id int primary key, body text);
             INSERT INTO est SELECT n, 'every '
               || CASE WHEN n % 7 = 0 THEN 'seven ' ELSE '' END
               || CASE WHEN n % 5 = 0 THEN 'five ' ELSE '' END
               || CASE WHEN n % 400 = 0 THEN 'needle ' ELSE '' END
               || CASE WHEN n % 100 = 0 THEN 'alpha beta ' WHEN n % 100 = 50 THEN 'beta ' ELSE '' END
               || 'filler w' || (n % 13)
               FROM generate_series(1, 4000) n;
             CREATE INDEX est_idx ON est USING stannum(body);
             ANALYZE est;",
        )
        .unwrap();
    }

    fn plan_of(sql: &str) -> Json {
        Spi::get_one::<Json>(&format!("EXPLAIN (FORMAT JSON) {sql}"))
            .unwrap()
            .unwrap()
    }

    fn true_count(query: &str) -> f64 {
        Spi::get_one::<i64>(&format!(
            "SELECT count(*) FROM est WHERE body ==> '{query}'"
        ))
        .unwrap()
        .unwrap() as f64
    }

    #[pg_test]
    fn planner_row_estimates_follow_index_statistics() {
        known_frequency_fixture();
        // (query, true count, allowed factor either way). Boolean shapes use
        // independence, which the fixture satisfies; a phrase is bounded by
        // its rarest term with a discount, so it is allowed a factor of 2.5.
        let cases = [
            ("needle", 10.0, 1.5),
            ("every", 4000.0, 1.5),
            ("seven", 571.0, 1.5),
            ("seven AND five", 114.0, 1.5),
            ("seven five", 114.0, 1.5),
            ("seven OR five", 1257.0, 1.5),
            ("every AND NOT seven", 3429.0, 1.5),
            ("\"alpha beta\"", 40.0, 2.5),
            ("seven AND needle", 1.0, 2.0),
            ("need*", 10.0, 1.5),
            ("needle OR missing", 10.0, 1.5),
            ("AT LEAST 2 OF [seven five needle]", 123.0, 1.5),
        ];
        for custom in [true, false] {
            Spi::run(&format!("SET LOCAL stannum.enable_custom_scan = {custom}")).unwrap();
            for (query, expected, factor) in cases {
                assert_eq!(true_count(query), expected, "{query}: fixture");
                let plan = plan_of(&format!("SELECT * FROM est WHERE body ==> '{query}'")).0;
                let rows = plan[0]["Plan"]["Plan Rows"].as_f64().unwrap();
                assert!(
                    rows <= expected * factor && rows >= expected / factor,
                    "{query}: estimated {rows} rows, true {expected} (custom scan {custom})"
                );
            }
        }
        // The bitmap path prices itself from the same estimate: a rare
        // query costs far less than a common one.
        Spi::run("SET LOCAL stannum.enable_custom_scan = off; SET LOCAL enable_seqscan = off")
            .unwrap();
        let rare = plan_of("SELECT * FROM est WHERE body ==> 'needle'").0;
        let common = plan_of("SELECT * FROM est WHERE body ==> 'every'").0;
        assert_eq!(rare[0]["Plan"]["Node Type"], "Bitmap Heap Scan");
        let rare_cost = rare[0]["Plan"]["Total Cost"].as_f64().unwrap();
        let common_cost = common[0]["Plan"]["Total Cost"].as_f64().unwrap();
        assert!(
            rare_cost * 2.0 < common_cost,
            "{rare_cost} vs {common_cost}"
        );
        // Unreadable at plan time: a query the index tokenizer rejects still
        // plans (the executor reports the error), with the fallback estimate.
        let plan = plan_of("SELECT * FROM est WHERE body ==> 'needle OR'").0;
        assert_eq!(plan[0]["Plan"]["Plan Rows"], 400);
        Spi::run("SET LOCAL enable_seqscan = on").unwrap();
        // A table with no stannum index keeps the fallback too.
        Spi::run("CREATE TABLE unindexed AS SELECT * FROM est; ANALYZE unindexed").unwrap();
        let plan = plan_of("SELECT * FROM unindexed WHERE body ==> 'needle'").0;
        assert_eq!(plan[0]["Plan"]["Plan Rows"], 400);
    }

    #[pg_test]
    fn selective_queries_use_the_index_and_common_ones_scan_the_heap() {
        known_frequency_fixture();
        let rare = plan_of("SELECT * FROM est WHERE body ==> 'needle'").0;
        assert_eq!(
            rare[0]["Plan"]["Custom Plan Provider"], "Stannum Text Search Scan",
            "{rare}"
        );
        let count = plan_of("SELECT count(*) FROM est WHERE body ==> 'needle'").0;
        assert_eq!(
            count[0]["Plan"]["Custom Plan Provider"], "Stannum Count",
            "{count}"
        );
        let everything = plan_of("SELECT * FROM est WHERE body ==> 'every'").0;
        assert_eq!(
            everything[0]["Plan"]["Node Type"], "Seq Scan",
            "{everything}"
        );
        // The estimate follows the index as it grows: the write buffer counts.
        Spi::run("INSERT INTO est SELECT n, 'needle fresh' FROM generate_series(4001, 4400) n")
            .unwrap();
        let grown = plan_of("SELECT * FROM est WHERE body ==> 'needle'").0;
        let rows = grown[0]["Plan"]["Plan Rows"].as_f64().unwrap();
        assert!((300.0..=500.0).contains(&rows), "{rows}");
    }

    #[pg_test]
    fn rare_predicate_drives_a_nested_loop_join() {
        known_frequency_fixture();
        Spi::run(
            "CREATE TABLE big(id int primary key, payload text);
             INSERT INTO big SELECT n, 'row ' || n FROM generate_series(1, 60000) n;
             ANALYZE big;",
        )
        .unwrap();
        let sql = "SELECT b.payload FROM est d JOIN big b ON b.id = d.id WHERE d.body ==> 'needle'";
        let plan = plan_of(sql).0;
        let join = &plan[0]["Plan"];
        assert_eq!(join["Node Type"], "Nested Loop", "{plan}");
        assert_eq!(
            join["Plans"][0]["Custom Plan Provider"], "Stannum Text Search Scan",
            "{plan}"
        );
        assert!(
            join["Plans"][1]["Node Type"]
                .as_str()
                .unwrap()
                .contains("Index"),
            "{plan}"
        );
        assert_eq!(
            Spi::get_one::<i64>(&format!("SELECT count(*) FROM ({sql}) t")).unwrap(),
            Some(10)
        );
    }

    #[pg_test]
    fn plans_stay_correct_when_the_estimate_is_wrong() {
        known_frequency_fixture();
        Spi::run(
            "CREATE TABLE big(id int primary key, payload text);
             INSERT INTO big SELECT n, 'row ' || n FROM generate_series(1, 20000) n;
             ANALYZE big;
             -- Dead rows: the index still counts them, the heap no longer has them
             -- (nine of the forty 'alpha beta' rows are multiples of 400 too).
             DELETE FROM est WHERE id % 400 = 0 AND id <> 400;",
        )
        .unwrap();
        // Overestimated (needle: 10 indexed, 1 live), underestimated
        // (alpha AND beta always co-occur; independence says 1 row, 31 live), and a
        // phrase that never occurs in that order (estimate 20, truth 0).
        let queries = [
            "needle",
            "alpha AND beta",
            "\"beta alpha\"",
            "beta OR needle",
        ];
        for query in queries {
            let sql = format!(
                "SELECT d.id FROM est d JOIN big b ON b.id = d.id WHERE d.body ==> '{query}' ORDER BY d.id"
            );
            let planned = Spi::connect(|client| {
                client
                    .select(&sql, None, &[])
                    .unwrap()
                    .map(|row| row.get::<i32>(1).unwrap().unwrap())
                    .collect::<Vec<_>>()
            });
            Spi::run(
                "SET LOCAL stannum.enable_custom_scan = off; SET LOCAL enable_bitmapscan = off;
                 SET LOCAL enable_indexscan = off",
            )
            .unwrap();
            let reference = Spi::connect(|client| {
                client
                    .select(&sql, None, &[])
                    .unwrap()
                    .map(|row| row.get::<i32>(1).unwrap().unwrap())
                    .collect::<Vec<_>>()
            });
            Spi::run(
                "SET LOCAL stannum.enable_custom_scan = on; SET LOCAL enable_bitmapscan = on;
                 SET LOCAL enable_indexscan = on",
            )
            .unwrap();
            assert_eq!(planned, reference, "{query}");
        }
        assert_eq!(true_count("needle"), 1.0);
        assert_eq!(true_count("alpha AND beta"), 31.0);
        assert_eq!(true_count("\"beta alpha\""), 0.0);
    }

    /// Every row of `stannum.verify_index` as `severity: location: message`.
    fn findings(index: &str, heap_check: bool) -> Vec<String> {
        Spi::connect(|client| {
            client
                .select(
                    &format!(
                        "SELECT severity || ': ' || location || ': ' || message
                         FROM stannum.verify_index('{index}', {heap_check})"
                    ),
                    None,
                    &[],
                )
                .unwrap()
                .map(|row| row.get::<String>(1).unwrap().unwrap())
                .collect::<Vec<_>>()
        })
    }

    fn assert_clean(index: &str) {
        let rows = findings(index, true);
        assert!(rows.is_empty(), "{index}:\n{}", rows.join("\n"));
    }

    #[pg_test]
    fn verify_index_is_clean_across_folds_merges_deletes_and_updates() {
        // A built index: one segment per `build_segment_docs`, then folds of
        // two documents with a tier factor of two so merges run on nearly
        // every fold, deletes and updates that leave dead versions behind,
        // an expression index, a partial index and rows with no tokens.
        Spi::run(
            "CREATE TABLE checked(id int primary key, body text, tag text);
             SET LOCAL stannum.build_segment_docs = 40;
             INSERT INTO checked
               SELECT n, 'w' || (n % 7) || ' common ' ||
                      CASE WHEN n % 10 = 0 THEN 'rare needle' ELSE 'other filler' END,
                      CASE WHEN n % 3 = 0 THEN 'odd' ELSE 'even' END
               FROM generate_series(1, 150) n;
             INSERT INTO checked VALUES (151, NULL, 'even'), (152, '', 'odd'), (153, '   ', 'odd');
             CREATE INDEX checked_idx ON checked USING stannum(body);
             CREATE INDEX checked_expr_idx ON checked USING stannum((body || ' ' || tag));
             CREATE INDEX checked_part_idx ON checked USING stannum(body) WHERE tag = 'odd';",
        )
        .unwrap();
        assert_clean("checked_idx");
        assert_clean("checked_expr_idx");
        assert_clean("checked_part_idx");
        Spi::run(
            "SET LOCAL stannum.write_buffer_docs = 2;
             SET LOCAL stannum.merge_tier_factor = 2;
             SET LOCAL stannum.max_segments = 6;
             INSERT INTO checked
               SELECT n, 'w' || (n % 7) || ' common ' ||
                      CASE WHEN n % 10 = 0 THEN 'rare needle' ELSE 'other filler' END,
                      CASE WHEN n % 3 = 0 THEN 'odd' ELSE 'even' END
               FROM generate_series(200, 260) n;
             DELETE FROM checked WHERE id % 3 = 0;
             UPDATE checked SET body = body || ' updated' WHERE id % 11 = 0;
             INSERT INTO checked VALUES (300, 'w1 w1 w1', 'odd'), (301, NULL, 'odd'), (302, '', 'even');",
        )
        .unwrap();
        let (segments, generations, _) = directory_shape("checked_idx");
        assert!(segments >= 2, "{segments} segments");
        assert_eq!(generations, segments);
        assert_clean("checked_idx");
        assert_clean("checked_expr_idx");
        assert_clean("checked_part_idx");
        Spi::run("SET LOCAL stannum.enable_custom_scan = off").unwrap();
        assert_index_matches_seqscan("checked", TIERED_QUERIES);
        // The buffer alone, the buffer empty, and an index with no storage.
        Spi::run(
            "CREATE TABLE fresh(id int, body text);
             CREATE INDEX fresh_idx ON fresh USING stannum(body);
             INSERT INTO fresh VALUES (1, 'only buffered');
             CREATE TABLE empty_docs(id int, body text);
             CREATE INDEX empty_idx ON empty_docs USING stannum(body);
             CREATE UNLOGGED TABLE volatile(id int, body text);
             INSERT INTO volatile VALUES (1, 'x');
             CREATE INDEX volatile_idx ON volatile USING stannum(body);",
        )
        .unwrap();
        assert_clean("fresh_idx");
        assert_clean("empty_idx");
        assert_clean("volatile_idx");
        assert!(findings("fresh_idx", false).is_empty());
    }

    /// Fresh single-segment index on `table`; returns the segment's root block.
    fn corruptible(table: &str) -> i64 {
        Spi::run(&format!(
            "CREATE TABLE {table}(id int, body text);
             INSERT INTO {table} SELECT n, 'w' || (n % 5) || ' common needle' FROM generate_series(1, 60) n;
             CREATE INDEX {table}_idx ON {table} USING stannum(body);"
        ))
        .unwrap();
        Spi::get_one::<i64>(&format!(
            "SELECT root_block FROM stannum.segment_info('{table}_idx') WHERE kind = 'immutable'"
        ))
        .unwrap()
        .unwrap()
    }

    fn corrupt(index: &str, block: i64, at: i32, bytes: &str) {
        Spi::run(&format!(
            "SELECT stannum.corrupt_index_page('{index}', {block}, {at}, '\\x{bytes}'::bytea)"
        ))
        .unwrap();
    }

    /// Page header bytes before the payload, then the chain link.
    const PAGE_HEADER: i32 = 24;
    const DATA_AT: i32 = PAGE_HEADER + 4;
    /// The kind byte in the special area.
    const KIND_AT: i32 = 8192 - 8 + 4;
    const PD_LOWER_AT: i32 = 12;

    #[pg_test]
    fn verify_index_reports_deliberate_corruption_without_crashing() {
        // The segment's magic.
        let root = corruptible("c_magic");
        corrupt("c_magic_idx", root, DATA_AT, "58585858");
        let rows = findings("c_magic_idx", false);
        assert_eq!(rows.len(), 1, "{}", rows.join("\n"));
        assert_eq!(
            rows[0],
            "error: segment generation 1, header: corrupt segment data: segment magic"
        );

        // A run page marked FREE while the directory still references it.
        let root = corruptible("c_free");
        corrupt("c_free_idx", root, KIND_AT, "04");
        let rows = findings("c_free_idx", false);
        assert!(
            rows.contains(&format!(
                "error: segment generation 1 run: page {root} is marked FREE but still referenced"
            )),
            "{}",
            rows.join("\n")
        );

        // A run page with the buffer kind.
        let root = corruptible("c_kind");
        corrupt("c_kind_idx", root, KIND_AT, "02");
        let rows = findings("c_kind_idx", false);
        assert_eq!(
            rows,
            [format!(
                "error: segment generation 1 run: page {root} has kind buffer instead of run"
            )]
        );

        // A run page truncated by its page header: pd_lower just past the link.
        let root = corruptible("c_short");
        corrupt("c_short_idx", root, PD_LOWER_AT, "2600");
        let rows = findings("c_short_idx", false);
        assert!(
            rows.iter().any(|row| row.starts_with(&format!(
                "error: segment generation 1 run: page {root} holds 10 bytes; "
            ))),
            "{}",
            rows.join("\n")
        );
        assert!(
            rows.iter()
                .all(|row| row.starts_with("error: segment generation 1 run: ")),
            "{}",
            rows.join("\n")
        );

        // The page table chain: a fresh index writes it right after the run.
        let root = corruptible("c_table");
        corrupt("c_table_idx", root + 1, KIND_AT, "02");
        let rows = findings("c_table_idx", false);
        assert_eq!(
            rows,
            [format!(
                "error: segment generation 1 page table: page {} has kind buffer instead of run",
                root + 1
            )]
        );

        // A header varint inside the blob (the document count) so the
        // header's length check fails; every finding names the segment.
        let root = corruptible("c_dict");
        corrupt("c_dict_idx", root, DATA_AT + 4, "ff");
        let rows = findings("c_dict_idx", false);
        assert!(!rows.is_empty());
        assert!(
            rows.iter()
                .all(|row| row.starts_with("error: segment generation 1")),
            "{}",
            rows.join("\n")
        );

        // The meta page: an unreadable tokenizer spec.
        corruptible("c_meta");
        corrupt("c_meta_idx", 0, PAGE_HEADER + 8, "ff");
        let rows = findings("c_meta_idx", false);
        assert_eq!(rows.len(), 1, "{}", rows.join("\n"));
        assert!(rows[0].starts_with("error: meta page: tokenizer spec"));

        // A meta page that is not a meta page at all.
        corruptible("c_nometa");
        corrupt("c_nometa_idx", 0, KIND_AT, "03");
        let rows = findings("c_nometa_idx", false);
        assert_eq!(
            rows,
            ["error: meta page: page 0 has kind run instead of meta"]
        );

        // The write buffer: a record whose length runs past the stream, so
        // the buffered rows are neither decodable nor found by the heap check.
        corruptible("c_buffer");
        Spi::run("INSERT INTO c_buffer VALUES (61, 'late needle'), (62, 'late needle')").unwrap();
        corrupt("c_buffer_idx", 1, DATA_AT, "ffff");
        let rows = findings("c_buffer_idx", true);
        assert!(
            rows.contains(
                &"error: write buffer, record 0 at byte 0: record runs past the end of the buffer"
                    .to_owned()
            ),
            "{}",
            rows.join("\n")
        );
        assert!(
            rows.contains(
                &"error: write buffer: buffer state says 2 documents but the stream holds 0"
                    .to_owned()
            ),
            "{}",
            rows.join("\n")
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.starts_with("error: heap: visible row")
                    && row.ends_with("is not in the index"))
                .count(),
            2,
            "{}",
            rows.join("\n")
        );
    }

    #[pg_test(
        error = "Stannum segment generation 1: corrupt segment data: segment magic; REINDEX required"
    )]
    fn corrupted_segments_name_their_generation_when_read() {
        let root = corruptible("c_read");
        corrupt("c_read_idx", root, DATA_AT, "58585858");
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
        Spi::get_one::<i64>("SELECT count(*) FROM c_read WHERE body ==> 'needle'").unwrap();
    }

    // --- Tokenizer settings agree across plans ----------------------------------

    /// Plan modes for a `==>` query: the sequential scan evaluating the
    /// operator itself, the bitmap index path, and the custom scan.
    const PLAN_MODES: [(&str, &str, &str); 3] = [
        (
            "seq",
            "SET LOCAL enable_seqscan = on; SET LOCAL enable_indexscan = off;
             SET LOCAL enable_bitmapscan = off; SET LOCAL stannum.enable_custom_scan = off",
            "Seq Scan",
        ),
        (
            "bitmap",
            "SET LOCAL enable_seqscan = off; SET LOCAL enable_indexscan = off;
             SET LOCAL enable_bitmapscan = on; SET LOCAL stannum.enable_custom_scan = off",
            "Bitmap Heap Scan",
        ),
        (
            "custom",
            "SET LOCAL enable_seqscan = off; SET LOCAL enable_indexscan = off;
             SET LOCAL enable_bitmapscan = on; SET LOCAL stannum.enable_custom_scan = on",
            "Custom Scan",
        ),
    ];

    fn ids(sql: &str) -> Vec<i32> {
        Spi::connect(|client| {
            client
                .select(sql, None, &[])
                .unwrap()
                .map(|row| row.get::<i32>(1).unwrap().unwrap())
                .collect()
        })
    }

    fn oid_of(relation: &str) -> u32 {
        Spi::get_one::<pgrx::pg_sys::Oid>(&format!("SELECT '{relation}'::regclass::oid"))
            .unwrap()
            .unwrap()
            .to_u32()
    }

    /// Whether any string in a JSON plan contains `needle`.
    fn plan_mentions(plan: &serde_json::Value, needle: &str) -> bool {
        match plan {
            serde_json::Value::String(text) => text.contains(needle),
            serde_json::Value::Array(items) => items.iter().any(|item| plan_mentions(item, needle)),
            serde_json::Value::Object(fields) => {
                fields.values().any(|value| plan_mentions(value, needle))
            }
            _ => false,
        }
    }

    /// The plan node below any sort the `ORDER BY` added.
    fn under_sort(plan: &serde_json::Value) -> serde_json::Value {
        if plan["Node Type"] == "Sort" {
            under_sort(&plan["Plans"][0])
        } else {
            plan.clone()
        }
    }

    /// Runs `sql` under every plan mode: (mode, top plan node, rows).
    fn by_mode(sql: &str) -> Vec<(&'static str, serde_json::Value, Vec<i32>)> {
        PLAN_MODES
            .iter()
            .map(|(mode, settings, _)| {
                Spi::run(settings).unwrap();
                let plan = Spi::get_one::<Json>(&format!("EXPLAIN (VERBOSE, FORMAT JSON) {sql}"))
                    .unwrap()
                    .unwrap()
                    .0;
                (*mode, under_sort(&plan[0]["Plan"]), ids(sql))
            })
            .collect()
    }

    /// The rows `body ==> query` matches in `table`, asserting the plan modes
    /// agree, each uses its own node, and every one evaluates the operator
    /// bound to `index`.
    fn agreed_ids(table: &str, index: &str, query: &str) -> Vec<i32> {
        let sql = format!(
            "SELECT id FROM {table} WHERE body ==> '{}' ORDER BY id",
            query.replace('\'', "''")
        );
        let bound = format!("\"index\":{}", oid_of(index));
        let results = by_mode(&sql);
        for ((mode, plan, _), (_, _, node)) in results.iter().zip(PLAN_MODES) {
            assert_eq!(plan["Node Type"], node, "{mode}: {query}: {plan}");
            if *mode == "custom" {
                assert_eq!(plan["Index"], index, "{mode}: {query}: {plan}");
            } else {
                assert!(plan_mentions(plan, &bound), "{mode}: {query}: {plan}");
            }
            if *mode == "bitmap" {
                assert!(plan_mentions(plan, index), "{mode}: {query}: {plan}");
            }
        }
        let rows = results.iter().map(|(_, _, rows)| rows).collect::<Vec<_>>();
        assert!(
            rows.windows(2).all(|pair| pair[0] == pair[1]),
            "{query}: {rows:?}"
        );
        rows[0].clone()
    }

    /// The rows the default tokenizer settings match: an expression no index
    /// covers stays unbound.
    fn default_ids(table: &str, query: &str) -> Vec<i32> {
        ids(&format!(
            "SELECT id FROM {table} WHERE (body || '') ==> '{}' ORDER BY id",
            query.replace('\'', "''")
        ))
    }

    #[pg_test]
    fn whitespace_tokenizer_agrees_across_plans() {
        Spi::run(
            "CREATE TABLE ws(id int, body text);
             INSERT INTO ws VALUES (1, 'craft-beer'), (2, 'craft beer'), (3, 'beer,wine'),
               (4, 'foo.bar baz'), (5, 'Craft');
             CREATE INDEX ws_idx ON ws USING stannum(body) WITH (tokenizer = whitespace);",
        )
        .unwrap();
        for query in [
            "craft",
            "craft-beer",
            "\"craft beer\"",
            "beer",
            "wine",
            "foo.bar",
        ] {
            agreed_ids("ws", "ws_idx", query);
        }
        assert_eq!(agreed_ids("ws", "ws_idx", "craft"), vec![2, 5]);
        assert_eq!(default_ids("ws", "craft"), vec![1, 2, 5]);
        assert_eq!(agreed_ids("ws", "ws_idx", "beer,wine"), vec![3]);
    }

    #[pg_test]
    fn case_folding_preserve_agrees_across_plans() {
        Spi::run(
            "CREATE TABLE cs(id int, body text);
             INSERT INTO cs VALUES (1, 'Beer'), (2, 'beer'), (3, 'BEER'), (4, 'Craft Beer');
             CREATE INDEX cs_idx ON cs USING stannum(body) WITH (case_folding = preserve);",
        )
        .unwrap();
        for query in [
            "beer",
            "Beer",
            "BEER",
            "\"craft beer\"",
            "\"Craft Beer\"",
            "Be*",
        ] {
            agreed_ids("cs", "cs_idx", query);
        }
        assert_eq!(agreed_ids("cs", "cs_idx", "Beer"), vec![1, 4]);
        assert_eq!(default_ids("cs", "Beer"), vec![1, 2, 3, 4]);
    }

    #[pg_test]
    fn accent_folding_preserve_agrees_across_plans() {
        Spi::run(
            "CREATE TABLE ac(id int, body text);
             INSERT INTO ac VALUES (1, 'jalapeño'), (2, 'jalapeno'), (3, 'Crème brûlée'),
               (4, 'creme brulee');
             CREATE INDEX ac_idx ON ac USING stannum(body) WITH (accent_folding = preserve);",
        )
        .unwrap();
        for query in [
            "jalapeño",
            "jalapeno",
            "\"crème brûlée\"",
            "creme",
            "jalapeno~1",
        ] {
            agreed_ids("ac", "ac_idx", query);
        }
        assert_eq!(agreed_ids("ac", "ac_idx", "jalapeño"), vec![1]);
        assert_eq!(default_ids("ac", "jalapeño"), vec![1, 2]);
    }

    #[pg_test]
    fn long_token_modes_agree_across_plans() {
        Spi::run(
            "CREATE TABLE lt(id int, body text);
             INSERT INTO lt VALUES (1, 'abcdefghijklmnop short'), (2, 'short'),
               (3, 'abcdefgh'), (4, 'ijklmnop');",
        )
        .unwrap();
        let queries = [
            "short",
            "abcdefgh",
            "ijklmnop",
            "abcdefghijklmnop",
            "abcd*",
            "\"abcdefgh ijklmnop\"",
        ];
        for mode in ["discard", "truncate", "split"] {
            Spi::run(&format!(
                "CREATE INDEX lt_idx ON lt USING stannum(body)
                   WITH (long_tokens = {mode}, max_token_bytes = 8)"
            ))
            .unwrap();
            for query in queries {
                agreed_ids("lt", "lt_idx", query);
            }
            match mode {
                "discard" => {
                    assert_eq!(agreed_ids("lt", "lt_idx", "abcdefgh"), vec![3]);
                    assert_eq!(
                        agreed_ids("lt", "lt_idx", "\"abcdefgh ijklmnop\""),
                        Vec::<i32>::new()
                    );
                }
                "truncate" => assert_eq!(agreed_ids("lt", "lt_idx", "abcdefgh"), vec![1, 3]),
                _ => {
                    assert_eq!(agreed_ids("lt", "lt_idx", "ijklmnop"), vec![1, 4]);
                    assert_eq!(agreed_ids("lt", "lt_idx", "\"abcdefgh ijklmnop\""), vec![1]);
                }
            }
            Spi::run("DROP INDEX lt_idx").unwrap();
        }
        assert_eq!(default_ids("lt", "abcdefgh"), vec![3]);
        assert_eq!(default_ids("lt", "ijklmnop"), vec![4]);
        assert_eq!(default_ids("lt", "abcdefghijklmnop"), vec![1]);
    }

    #[pg_test]
    fn grapheme_modes_agree_across_plans() {
        Spi::run(
            "CREATE TABLE gr(id int, body text);
             INSERT INTO gr VALUES (1, 'I love 🍺'), (2, 'beer 🍺🍻 wine'), (3, 'plain → text'),
               (4, '👍'), (5, 'craft 🍺 beer');",
        )
        .unwrap();
        let queries = [
            "🍺",
            "love",
            "\"love 🍺\"",
            "→",
            "\"plain → text\"",
            "\"plain text\"",
            "\"craft beer\"",
            "👍",
        ];
        for mode in ["discard", "retain"] {
            Spi::run(&format!(
                "CREATE INDEX gr_idx ON gr USING stannum(body) WITH (graphemes = {mode})"
            ))
            .unwrap();
            for query in queries {
                agreed_ids("gr", "gr_idx", query);
            }
            if mode == "discard" {
                assert_eq!(agreed_ids("gr", "gr_idx", "🍺"), Vec::<i32>::new());
                // Discarded graphemes leave no position behind.
                assert_eq!(agreed_ids("gr", "gr_idx", "\"plain text\""), vec![3]);
            } else {
                assert_eq!(agreed_ids("gr", "gr_idx", "→"), vec![3]);
                assert_eq!(agreed_ids("gr", "gr_idx", "\"plain → text\""), vec![3]);
            }
            Spi::run("DROP INDEX gr_idx").unwrap();
        }
        assert_eq!(default_ids("gr", "🍺"), vec![1, 2, 5]);
        assert_eq!(default_ids("gr", "→"), Vec::<i32>::new());
    }

    #[pg_test]
    fn position_gap_modes_agree_across_plans() {
        // A discarded long token leaves a gap in its phrase when positions are
        // preserved, and none when they collapse.
        Spi::run(
            "CREATE TABLE pgap(id int, body text);
             INSERT INTO pgap VALUES (1, 'craft abcdefghijklmnop beer'), (2, 'craft beer'),
               (3, 'craft abcdefgh beer');",
        )
        .unwrap();
        let queries = ["\"craft beer\"", "craft beer", "\"craft abcdefgh beer\""];
        for gaps in ["collapse", "preserve"] {
            Spi::run(&format!(
                "CREATE INDEX pgap_idx ON pgap USING stannum(body)
                   WITH (long_tokens = discard, max_token_bytes = 8, position_gaps = {gaps})"
            ))
            .unwrap();
            for query in queries {
                agreed_ids("pgap", "pgap_idx", query);
            }
            let expected = if gaps == "collapse" {
                vec![1, 2]
            } else {
                vec![2]
            };
            assert_eq!(agreed_ids("pgap", "pgap_idx", "\"craft beer\""), expected);
            Spi::run("DROP INDEX pgap_idx").unwrap();
        }
        assert_eq!(default_ids("pgap", "\"craft beer\""), vec![2]);
    }

    #[pg_test]
    fn two_indexes_bind_the_first_by_oid() {
        Spi::run(
            "CREATE TABLE pair(id int, body text);
             INSERT INTO pair VALUES (1, 'Beer'), (2, 'beer'), (3, 'BEER');
             CREATE INDEX pair_fold ON pair USING stannum(body);
             CREATE INDEX pair_case ON pair USING stannum(body) WITH (case_folding = preserve);",
        )
        .unwrap();
        // The first index by OID binds; every plan follows it, and the bitmap
        // path scans it rather than the index with other settings.
        assert_eq!(agreed_ids("pair", "pair_fold", "Beer"), vec![1, 2, 3]);
        assert_eq!(agreed_ids("pair", "pair_fold", "beer"), vec![1, 2, 3]);
        Spi::run("DROP INDEX pair_fold").unwrap();
        assert_eq!(agreed_ids("pair", "pair_case", "Beer"), vec![1]);
        assert_eq!(agreed_ids("pair", "pair_case", "beer"), vec![2]);
        // Created in the other order, the case-preserving index binds.
        Spi::run(
            "CREATE TABLE pair2(id int, body text);
             INSERT INTO pair2 VALUES (1, 'Beer'), (2, 'beer'), (3, 'BEER');
             CREATE INDEX pair2_case ON pair2 USING stannum(body) WITH (case_folding = preserve);
             CREATE INDEX pair2_fold ON pair2 USING stannum(body);",
        )
        .unwrap();
        assert_eq!(agreed_ids("pair2", "pair2_case", "Beer"), vec![1]);
        // Forcing the other index scans every page and rechecks with the
        // bound settings, so the result is unchanged.
        Spi::run(
            "DROP INDEX pair2_case;
             CREATE INDEX pair2_case ON pair2 USING stannum(body) WITH (case_folding = preserve);",
        )
        .unwrap();
        assert_eq!(agreed_ids("pair2", "pair2_fold", "Beer"), vec![1, 2, 3]);
    }

    #[pg_test]
    fn partial_index_predicates_select_the_binding() {
        Spi::run(
            "CREATE TABLE part(id int, body text, active boolean);
             INSERT INTO part VALUES (1, 'Beer', true), (2, 'beer', true), (3, 'BEER', false);
             CREATE INDEX part_case ON part USING stannum(body)
               WITH (case_folding = preserve) WHERE active;
             CREATE INDEX part_fold ON part USING stannum(body);",
        )
        .unwrap();
        // Without the predicate, the partial index cannot answer and the
        // full one binds.
        assert_eq!(agreed_ids("part", "part_fold", "Beer"), vec![1, 2, 3]);
        let sql = "SELECT id FROM part WHERE active AND body ==> 'Beer' ORDER BY id";
        let bound = format!("\"index\":{}", oid_of("part_case"));
        for (mode, plan, rows) in by_mode(sql) {
            assert_eq!(rows, vec![1], "{mode}");
            if mode == "custom" {
                assert_eq!(plan["Index"], "part_case", "{mode}: {plan}");
            } else {
                assert!(plan_mentions(&plan, &bound), "{mode}: {plan}");
            }
        }
    }

    #[pg_test]
    fn non_constant_queries_bind_to_the_index() {
        Spi::run(
            "CREATE TABLE nc(id int, body text);
             INSERT INTO nc VALUES (1, 'Beer'), (2, 'beer'), (3, 'BEER');
             CREATE INDEX nc_idx ON nc USING stannum(body) WITH (case_folding = preserve);",
        )
        .unwrap();
        let sql = "SELECT nc.id FROM nc, (VALUES ('Beer'), ('beer')) v(q)
                   WHERE nc.body ==> v.q ORDER BY nc.id";
        for (mode, plan, rows) in by_mode(sql) {
            assert_eq!(rows, vec![1, 2], "{mode}");
            assert!(plan_mentions(&plan, "bind_query"), "{mode}: {plan}");
        }
        // A prepared statement's generic plan holds the binding; dropping the
        // index replans with the default settings.
        Spi::run(
            "SET LOCAL enable_seqscan = on; SET LOCAL enable_bitmapscan = on;
             SET LOCAL stannum.enable_custom_scan = on;
             SET LOCAL plan_cache_mode = force_generic_plan;
             PREPARE nc_plan AS SELECT id FROM nc WHERE body ==> 'Beer' ORDER BY id",
        )
        .unwrap();
        assert_eq!(ids("EXECUTE nc_plan"), vec![1]);
        Spi::run("DROP INDEX nc_idx").unwrap();
        assert_eq!(ids("EXECUTE nc_plan"), vec![1, 2, 3]);
    }

    #[pg_test]
    fn partitioned_indexes_bind_their_partitions() {
        Spi::run(
            "CREATE TABLE pt (id int, body text) PARTITION BY RANGE (id);
             CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (1) TO (3);
             CREATE TABLE pt2 PARTITION OF pt FOR VALUES FROM (3) TO (5);
             INSERT INTO pt VALUES (1, 'Beer'), (2, 'beer'), (3, 'BEER'), (4, 'Beer');
             CREATE INDEX pt_case ON pt USING stannum(body) WITH (case_folding = preserve);",
        )
        .unwrap();
        let sql = "SELECT id FROM pt WHERE body ==> 'Beer' ORDER BY id";
        let bound = format!("\"index\":{}", oid_of("pt_case"));
        for (mode, plan, rows) in by_mode(sql) {
            assert_eq!(rows, vec![1, 4], "{mode}");
            assert!(
                plan_mentions(&plan, &bound) || plan_mentions(&plan, "pt1_body_idx"),
                "{mode}: {plan}"
            );
        }
    }

    #[pg_test]
    fn highlighting_follows_the_index_tokenizer() {
        Spi::run(
            "CREATE TABLE hl(id int, body text);
             INSERT INTO hl VALUES (1, 'Beer beer BEER');
             CREATE INDEX hl_idx ON hl USING stannum(body) WITH (case_folding = preserve);",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<String>("SELECT stannum.highlight(body) FROM hl WHERE body ==> 'Beer'")
                .unwrap(),
            Some("<b>Beer</b> beer BEER".into())
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT stannum.highlight(body, query => 'beer') FROM hl")
                .unwrap(),
            Some("Beer <b>beer</b> BEER".into())
        );
        // An uncovered expression keeps the default settings.
        assert_eq!(
            Spi::get_one::<String>("SELECT stannum.highlight(body || '', query => 'beer') FROM hl")
                .unwrap(),
            Some("<b>Beer</b> <b>beer</b> <b>BEER</b>".into())
        );
        let ansi = Spi::get_one::<String>(
            "SELECT stannum.highlight_ansi(body) FROM hl WHERE body ==> 'BEER'",
        )
        .unwrap()
        .unwrap();
        assert_eq!(ansi.matches("\x1b[").count(), 2, "{ansi:?}");
        assert!(ansi.ends_with("BEER\x1b[0m"), "{ansi:?}");
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (VERBOSE, FORMAT JSON)
             SELECT stannum.highlight(body) FROM hl WHERE body ==> 'Beer'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert!(plan_mentions(&plan, "indexed_query"), "{plan}");
    }

    #[pg_test]
    fn diagnostics_require_heap_select_and_reject_row_security() {
        Spi::run(
            "CREATE ROLE diagnostic_reader; GRANT USAGE ON SCHEMA stannum TO diagnostic_reader;
            CREATE TABLE private_docs(body text); INSERT INTO private_docs VALUES ('secret');
            CREATE INDEX private_idx ON private_docs USING stannum(body);",
        )
        .unwrap();
        for function in ["segment_info", "verify_index", "score_inspect"] {
            let args = if function == "score_inspect" {
                "'private_idx', 'secret'"
            } else {
                "'private_idx'"
            };
            Spi::run(&format!(
                "SET LOCAL ROLE diagnostic_reader;
                DO $$ BEGIN
                  BEGIN PERFORM * FROM stannum.{function}({args});
                    RAISE EXCEPTION 'diagnostic disclosed private index';
                  EXCEPTION WHEN insufficient_privilege THEN NULL; END;
                END $$; RESET ROLE;"
            ))
            .unwrap();
        }
        Spi::run(
            "SET LOCAL ROLE diagnostic_reader;
            DO $$ BEGIN
              BEGIN PERFORM stannum.score_bound_indexed('(0,1)'::tid, 'secret',
                'private_docs'::regclass::oid::int, 'private_idx'::regclass::oid::int,
                1, NULL, NULL, NULL, NULL, NULL);
                RAISE EXCEPTION 'bound scorer disclosed private index';
              EXCEPTION WHEN insufficient_privilege THEN NULL; END;
            END $$; RESET ROLE;",
        )
        .unwrap();
        Spi::run("GRANT SELECT ON private_docs TO diagnostic_reader;
            SET LOCAL ROLE diagnostic_reader;
            SELECT * FROM stannum.segment_info('private_idx');
            SELECT * FROM stannum.verify_index('private_idx', true);
            SELECT * FROM stannum.score_inspect('private_idx', 'secret');
            RESET ROLE;
            ALTER TABLE private_docs ENABLE ROW LEVEL SECURITY;
            SET LOCAL ROLE diagnostic_reader;
            DO $$ BEGIN
              BEGIN PERFORM * FROM stannum.segment_info('private_idx');
                RAISE EXCEPTION 'diagnostic ignored row security';
              EXCEPTION WHEN OTHERS THEN
                IF SQLERRM <> 'index diagnostics require ownership or SELECT without row security' THEN RAISE; END IF;
              END;
            END $$; RESET ROLE;").unwrap();
    }

    #[pg_test]
    fn indexed_query_rejects_malformed_values_as_unprivileged_user() {
        Spi::run(
            "CREATE ROLE query_reader; GRANT USAGE ON SCHEMA stannum TO query_reader;
            SET LOCAL ROLE query_reader;
            DO $$ DECLARE value text; BEGIN
              FOREACH value IN ARRAY ARRAY['garbage', '{}', 'null', '[]',
                '{\"index\":-1,\"query\":\"beer\"}',
                '{\"index\":1,\"query\":null}',
                '{\"index\":1,\"query\":\"beer\",\"extra\":true}'] LOOP
                BEGIN EXECUTE format('SELECT %L::stannum.indexed_query', value);
                EXCEPTION WHEN OTHERS THEN CONTINUE; END;
                RAISE EXCEPTION 'accepted malformed indexed_query: %', value;
              END LOOP;
            END $$; RESET ROLE;",
        )
        .unwrap();
    }

    #[pg_test]
    fn reindex_writes_current_format_and_future_pages_fail_cleanly() {
        use crate::storage::layout;
        let root = corruptible("release_format");
        let index = unsafe { pgrx::PgRelation::open_with_name("release_format_idx") }.unwrap();
        let read_page = |block| unsafe {
            let buffer = pg_sys::ReadBuffer(index.as_ptr(), block);
            pg_sys::LockBuffer(buffer, pg_sys::BUFFER_LOCK_SHARE as i32);
            let bytes = std::slice::from_raw_parts(
                pg_sys::BufferGetPage(buffer).cast::<u8>(),
                layout::PAGE_SIZE,
            )
            .to_vec();
            pg_sys::UnlockReleaseBuffer(buffer);
            bytes
        };
        assert_eq!(
            &read_page(root as u32)[DATA_AT as usize..DATA_AT as usize + 4],
            segment::segment::MAGIC
        );
        drop(index);
        corrupt("release_format_idx", 0, KIND_AT + 1, "ff");
        assert!(
            findings("release_format_idx", false)
                .iter()
                .any(|s| s.contains("unsupported Stannum page version"))
        );
        Spi::run("REINDEX INDEX release_format_idx").unwrap();
        assert_clean("release_format_idx");
        let index = unsafe { pgrx::PgRelation::open_with_name("release_format_idx") }.unwrap();
        unsafe {
            let buffer = pg_sys::ReadBuffer(index.as_ptr(), 0);
            pg_sys::LockBuffer(buffer, pg_sys::BUFFER_LOCK_SHARE as i32);
            let bytes = std::slice::from_raw_parts(
                pg_sys::BufferGetPage(buffer).cast::<u8>(),
                layout::PAGE_SIZE,
            )
            .to_vec();
            pg_sys::UnlockReleaseBuffer(buffer);
            assert_eq!(
                bytes[layout::PAGE_SIZE - layout::SPECIAL_SIZE + 5],
                layout::VERSION
            );
            assert_eq!(layout::kind(&bytes), Ok(layout::KIND_META));
        }
    }

    #[pg_test]
    fn planner_estimates_follow_dead_lists_before_segment_rewrite() {
        use std::collections::BTreeSet;
        // pg_test runs inside a transaction, where SQL VACUUM is forbidden.
        // Exercise its two storage callbacks separately so the estimate is
        // checked while the original segment and its dead list still exist.
        Spi::run("CREATE TABLE est_live(id int, body text);
            INSERT INTO est_live SELECT n, CASE WHEN n <= 20 THEN 'rare common' ELSE 'common' END FROM generate_series(1, 200) n;
            CREATE INDEX est_live_idx ON est_live USING stannum(body);
            ANALYZE est_live").unwrap();
        let mut dead: BTreeSet<(u32, u16)> = Spi::connect(|client| {
            client
                .select("SELECT ctid FROM est_live WHERE id <= 10", None, &[])
                .unwrap()
                .map(|row| {
                    let tid = row.get::<pg_sys::ItemPointerData>(1).unwrap().unwrap();
                    (
                        (u32::from(tid.ip_blkid.bi_hi) << 16) | u32::from(tid.ip_blkid.bi_lo),
                        tid.ip_posid,
                    )
                })
                .collect()
        });
        Spi::run("DELETE FROM est_live WHERE id <= 10; ANALYZE est_live").unwrap();
        unsafe extern "C-unwind" fn deleted(
            tid: pg_sys::ItemPointer,
            state: *mut std::ffi::c_void,
        ) -> bool {
            let tid = unsafe { *tid };
            let dead = unsafe { &*state.cast::<BTreeSet<(u32, u16)>>() };
            dead.contains(&(
                (u32::from(tid.ip_blkid.bi_hi) << 16) | u32::from(tid.ip_blkid.bi_lo),
                tid.ip_posid,
            ))
        }
        let index = unsafe { pgrx::PgRelation::open_with_name("est_live_idx") }.unwrap();
        unsafe {
            crate::storage::bulk_delete(
                index.as_ptr(),
                Some(deleted),
                std::ptr::from_mut(&mut dead).cast(),
            );
        }
        let check = || {
            assert_eq!(
                Spi::get_one::<i64>("SELECT count(*) FROM est_live WHERE body ==> 'rare'").unwrap(),
                Some(10)
            );
            let plan = plan_of("SELECT * FROM est_live WHERE body ==> 'rare'").0;
            let rows = plan[0]["Plan"]["Plan Rows"].as_f64().unwrap();
            assert!((10.0 / 1.5..=15.0).contains(&rows), "{plan}");
        };
        check();
        // Cleanup does not rewrite a segment less than half dead.
        unsafe {
            crate::storage::cleanup(index.as_ptr());
        }
        check();
        // Force the rewrite threshold, preserving ten live rare matches.
        dead.extend(Spi::connect(|client| {
            client
                .select(
                    "SELECT ctid FROM est_live WHERE id > 20 AND id <= 120",
                    None,
                    &[],
                )
                .unwrap()
                .map(|row| {
                    let tid = row.get::<pg_sys::ItemPointerData>(1).unwrap().unwrap();
                    (
                        (u32::from(tid.ip_blkid.bi_hi) << 16) | u32::from(tid.ip_blkid.bi_lo),
                        tid.ip_posid,
                    )
                })
                .collect::<Vec<_>>()
        }));
        Spi::run("DELETE FROM est_live WHERE id > 20 AND id <= 120; ANALYZE est_live").unwrap();
        unsafe {
            crate::storage::bulk_delete(
                index.as_ptr(),
                Some(deleted),
                std::ptr::from_mut(&mut dead).cast(),
            );
        }
        check();
        unsafe {
            crate::storage::cleanup(index.as_ptr());
        }
        check();
    }

    #[pg_test]
    fn tin_maintenance_options_are_accepted_without_changing_search() {
        Spi::run("CREATE TABLE compat_options(id int, body text);
            INSERT INTO compat_options VALUES (1, 'Éclair 3.14 can''t wi-fi 👩‍💻'), (2, 'other');
            CREATE INDEX compat_options_idx ON compat_options USING stannum(body) WITH (
                initial_segment_count=4096, target_segment_count=4096,
                max_mutable_segment_size=131072, max_merged_segment_size=100, dead_percent_threshold=0.0)").unwrap();
        for query in ["eclair", "3.14", "can't", "wi-fi", "👩‍💻"] {
            assert_eq!(
                agreed_ids("compat_options", "compat_options_idx", query),
                vec![1]
            );
        }
        Spi::run("ALTER INDEX compat_options_idx SET (target_segment_count=1, max_mutable_segment_size=2147483647,
            max_merged_segment_size=2147483647, dead_percent_threshold=1.0)").unwrap();
        assert_eq!(
            agreed_ids("compat_options", "compat_options_idx", "eclair"),
            vec![1]
        );
    }

    #[pg_test]
    fn maintenance_options_apply_to_their_own_index() {
        let immutable = |index: &str| {
            value(&format!(
                "SELECT count(*) FROM stannum.segment_info('{index}') WHERE kind = 'immutable'"
            ))
        };
        // The write buffer folds at the index's own size.
        Spi::run(
            "CREATE TABLE per_index(body text);
             CREATE INDEX per_index_default ON per_index USING stannum(body);
             CREATE INDEX per_index_small ON per_index USING stannum(body)
                 WITH (max_mutable_segment_size=2000);
             INSERT INTO per_index SELECT 'needle number ' || n FROM generate_series(1,200) n;",
        )
        .unwrap();
        assert_eq!(immutable("per_index_default"), 0);
        assert!(immutable("per_index_small") > 0);
        // VACUUM's cleanup brings each directory under its own bound.
        Spi::run(
            "CREATE TABLE per_bound(body text);
             CREATE INDEX per_bound_default ON per_bound USING stannum(body);
             CREATE INDEX per_bound_two ON per_bound USING stannum(body) WITH (target_segment_count=2);
             SET LOCAL stannum.write_buffer_docs = 1;
             SET LOCAL stannum.merge_tier_factor = 64;
             SET LOCAL stannum.max_merge_docs = 0;
             SET LOCAL stannum.deferred_merge_docs = 0;
             INSERT INTO per_bound SELECT 'needle' FROM generate_series(1,10);",
        )
        .unwrap();
        for index in ["per_bound_default", "per_bound_two"] {
            let oid = Spi::get_one::<pg_sys::Oid>(&format!("SELECT '{index}'::regclass::oid"))
                .unwrap()
                .unwrap();
            unsafe {
                let index = pgrx::PgRelation::with_lock(oid, pg_sys::ShareUpdateExclusiveLock as _);
                crate::storage::cleanup(index.as_ptr());
            }
            assert_clean(index);
        }
        assert_eq!(immutable("per_bound_default"), 9);
        assert!(immutable("per_bound_two") <= 2);
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
        assert_eq!(
            value("SELECT count(*) FROM per_bound WHERE body ==> 'needle'"),
            10
        );
    }

    #[pg_test]
    fn tokenizer_audit_inputs_use_index_options_for_matching_and_highlighting() {
        Spi::run("CREATE TABLE audit_tokens(id int, body text);
            INSERT INTO audit_tokens VALUES (1, 'Éclair Éclair Ελληνικά 東京 👩‍💻 3.14 can''t wi-fi https://Example.com/a');").unwrap();
        for (options, query) in [
            ("tokenizer=unicode", "eclair"),
            ("tokenizer=whitespace", "wi-fi"),
            ("case_folding=preserve", "Éclair"),
            ("accent_folding=preserve", "Éclair"),
            ("graphemes=emoji", "👩‍💻"),
            ("graphemes=retain", "👩‍💻"),
            ("graphemes=discard", "3.14"),
            ("long_tokens=split, max_token_bytes=4", "ecla"),
            ("long_tokens=truncate, max_token_bytes=4", "ecla"),
            (
                "long_tokens=discard, max_token_bytes=4, position_gaps=preserve",
                "3.14",
            ),
            (
                "long_tokens=discard, max_token_bytes=4, position_gaps=collapse",
                "3.14",
            ),
        ] {
            Spi::run(&format!(
                "CREATE INDEX audit_tokens_idx ON audit_tokens USING stannum(body) WITH ({options})"
            ))
            .unwrap();
            assert_eq!(
                agreed_ids("audit_tokens", "audit_tokens_idx", query),
                vec![1],
                "{options} {query}"
            );
            let literal = query.replace('\'', "''");
            let rendered = Spi::get_one::<String>(&format!(
                "SELECT stannum.highlight(body) FROM audit_tokens WHERE body ==> '{literal}'"
            ))
            .unwrap()
            .unwrap();
            assert!(rendered.contains("<b>"), "{options}: {rendered}");
            Spi::run("DROP INDEX audit_tokens_idx").unwrap();
        }
    }

    #[pg_test]
    fn temporary_indexes_use_segments_local_buffers_and_all_scan_paths() {
        Spi::run("SET LOCAL stannum.write_buffer_docs=4;
            SET LOCAL stannum.merge_tier_factor=2; SET LOCAL stannum.max_segments=3;
            CREATE TEMP TABLE local_search(id int, body text);
            CREATE INDEX local_search_idx ON local_search USING stannum(body);
            INSERT INTO local_search SELECT n, CASE WHEN n%5=0 THEN 'needle common' ELSE 'common' END
              FROM generate_series(1,160) n;
            UPDATE local_search SET body='needle' WHERE id%7=0;
            DELETE FROM local_search WHERE id%11=0;
            ANALYZE local_search;").unwrap();
        assert!(Spi::get_one::<i64>("SELECT count(*) FROM stannum.segment_info('local_search_idx') WHERE kind='immutable'").unwrap().unwrap() > 0);
        assert_clean("local_search_idx");
        let expected = Spi::get_one::<Vec<i32>>(
            "SELECT array_agg(id ORDER BY id) FROM local_search WHERE body LIKE '%needle%'",
        )
        .unwrap();
        for custom in ["off", "on"] {
            Spi::run(&format!(
                "SET LOCAL enable_seqscan=off; SET LOCAL stannum.enable_custom_scan={custom}"
            ))
            .unwrap();
            assert_eq!(
                Spi::get_one::<Vec<i32>>(
                    "SELECT array_agg(id ORDER BY id) FROM local_search WHERE body ==> 'needle'"
                )
                .unwrap(),
                expected
            );
        }
        let plan = Spi::get_one::<Json>("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT * FROM local_search WHERE body ==> 'needle'").unwrap().unwrap().0;
        assert_eq!(
            plan[0]["Plan"]["Custom Plan Provider"],
            "Stannum Text Search Scan"
        );
        assert!(plan[0]["Plan"]["Local Hit Blocks"].as_u64().unwrap() > 0);
        let count = Spi::get_one::<Json>("EXPLAIN (ANALYZE, FORMAT JSON) SELECT count(*) FROM local_search WHERE body ==> 'needle'").unwrap().unwrap().0;
        assert_eq!(count[0]["Plan"]["Custom Plan Provider"], "Stannum Count");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM local_search WHERE body ==> 'needle'")
                .unwrap(),
            Some(expected.unwrap().len() as i64)
        );
        assert!(Spi::get_one::<f32>("SELECT stannum.full_score(ctid) FROM local_search WHERE body ==> 'needle' ORDER BY stannum.full_score(ctid) DESC LIMIT 1").unwrap().unwrap() > 0.0);
        let insert = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, WAL, FORMAT JSON) INSERT INTO local_search VALUES(999, 'needle')",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(insert[0]["Plan"]["WAL Records"], 0);
        Spi::run("REINDEX INDEX local_search_idx").unwrap();
        assert_clean("local_search_idx");
    }

    #[pg_test]
    fn unlogged_indexes_have_valid_init_forks_and_segmented_main_forks() {
        Spi::run(
            "CREATE UNLOGGED TABLE unlogged_search(body text);
            INSERT INTO unlogged_search VALUES('needle'), ('common');
            CREATE INDEX unlogged_search_idx ON unlogged_search USING stannum(body);",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT pg_relation_size('unlogged_search_idx', 'init')").unwrap(),
            Some(2 * 8192)
        );
        assert!(Spi::get_one::<i64>("SELECT count(*) FROM stannum.segment_info('unlogged_search_idx') WHERE kind='immutable'").unwrap().unwrap() > 0);
        assert_clean("unlogged_search_idx");
        Spi::run("SET LOCAL enable_seqscan=off; SET LOCAL stannum.enable_custom_scan=on").unwrap();
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM unlogged_search WHERE body ==> 'needle'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(
            plan[0]["Plan"]["Custom Plan Provider"],
            "Stannum Text Search Scan"
        );
        assert_eq!(plan[0]["Plan"]["Actual Rows"].as_f64(), Some(1.0));
    }

    #[pg_test]
    fn unordered_search_and_count_are_correct_with_debug_parallel_query() {
        Spi::run("CREATE TABLE worker_search(id int, body text);
            INSERT INTO worker_search SELECT n, CASE WHEN n%10=0 THEN 'needle common' ELSE 'common' END FROM generate_series(1,2000) n;
            CREATE INDEX worker_search_idx ON worker_search USING stannum(body);
            ANALYZE worker_search; SET LOCAL debug_parallel_query=on;
            SET LOCAL max_parallel_workers_per_gather=2; SET LOCAL min_parallel_table_scan_size=0;
            SET LOCAL parallel_setup_cost=0; SET LOCAL parallel_tuple_cost=0;
            SET LOCAL enable_seqscan=off;").unwrap();
        for custom in ["off", "on"] {
            Spi::run(&format!("SET LOCAL stannum.enable_custom_scan={custom}")).unwrap();
            assert_eq!(
                Spi::get_one::<i64>("SELECT count(*) FROM worker_search WHERE body ==> 'needle'")
                    .unwrap(),
                Some(200)
            );
            assert_eq!(
                Spi::get_one::<i64>(
                    "SELECT sum(id)::bigint FROM worker_search WHERE body ==> 'needle'"
                )
                .unwrap(),
                Some(201000)
            );
            let plan = Spi::get_one::<Json>("EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM worker_search WHERE body ==> 'needle'").unwrap().unwrap().0;
            assert_eq!(plan[0]["Plan"]["Actual Rows"].as_f64(), Some(200.0));
        }
    }

    fn value(sql: &str) -> i64 {
        Spi::get_one::<i64>(sql).unwrap().unwrap()
    }

    fn direct_merge_fixture() {
        Spi::run(
            "CREATE TABLE direct_merge_cancel(id int, body text);
            CREATE INDEX direct_merge_cancel_idx ON direct_merge_cancel USING stannum(body);
            SET LOCAL stannum.write_buffer_docs=1;
            SET LOCAL stannum.merge_tier_factor=2;
            SET LOCAL stannum.max_merge_docs=1024;
            INSERT INTO direct_merge_cancel VALUES (1,'needle first'),(2,'needle second');
            SET LOCAL enable_seqscan=off;",
        )
        .unwrap();
    }

    #[pg_test]
    fn direct_merge_error_before_publication_preserves_directory_and_retry() {
        use std::cell::Cell;
        use std::rc::Rc;
        direct_merge_fixture();
        let fired = Rc::new(Cell::new(false));
        let observed = fired.clone();
        crate::storage::testing::set_race_hook(Some(Box::new(move |name| {
            if name == "merge:built" && !observed.replace(true) {
                pgrx::ereport!(
                    pgrx::PgLogLevel::ERROR,
                    pgrx::PgSqlErrorCode::ERRCODE_QUERY_CANCELED,
                    "injected merge cancellation"
                );
            }
        })));
        Spi::run(
            "DO $$BEGIN
            INSERT INTO direct_merge_cancel VALUES (3,'needle third');
            RAISE EXCEPTION 'merge cancellation was not injected';
            EXCEPTION WHEN query_canceled THEN NULL;
        END$$;",
        )
        .unwrap();
        crate::storage::testing::set_race_hook(None);
        assert!(fired.get());
        assert_eq!(
            value("SELECT count(*) FROM direct_merge_cancel WHERE body ==> 'needle'"),
            2
        );
        assert_eq!(
            value("SELECT sum(docs)::bigint FROM stannum.segment_info('direct_merge_cancel_idx')"),
            2
        );
        Spi::run("INSERT INTO direct_merge_cancel VALUES (3,'needle third')").unwrap();
        assert_eq!(
            value("SELECT sum(id)::bigint FROM direct_merge_cancel WHERE body ==> 'needle'"),
            6
        );
        // The interrupted fold can leave unpublished run pages. Validate
        // that these are the only findings, then exercise VACUUM's recovery.
        let rows = findings("direct_merge_cancel_idx", true);
        assert!(!rows.is_empty(), "expected unpublished fold pages");
        assert!(
            rows.iter().all(|row| row.starts_with("warning: page")
                && row.ends_with("run page referenced by nothing; VACUUM reclaims it")),
            "unexpected findings: {rows:?}"
        );
        let index = unsafe { pgrx::PgRelation::open_with_name("direct_merge_cancel_idx") }.unwrap();
        unsafe { crate::storage::cleanup(index.as_ptr()) };
        assert_clean("direct_merge_cancel_idx");
    }

    #[pg_test]
    fn direct_merge_pending_cancel_is_delivered_after_metadata_unlock() {
        use std::cell::Cell;
        use std::rc::Rc;
        direct_merge_fixture();
        let queued = Rc::new(Cell::new(false));
        let built = Rc::new(Cell::new(false));
        let q = queued.clone();
        let b = built.clone();
        crate::storage::testing::set_race_hook(Some(Box::new(move |name| {
            if name == "merge:checkpoint" && !q.replace(true) {
                unsafe {
                    assert!(pg_sys::InterruptHoldoffCount > 0);
                    pg_sys::QueryCancelPending = 1;
                    pg_sys::InterruptPending = 1;
                }
            }
            if name == "merge:built" {
                b.set(true);
            }
        })));
        Spi::run(
            "DO $$BEGIN
            INSERT INTO direct_merge_cancel VALUES (3,'needle third');
            RAISE EXCEPTION 'pending cancellation was not delivered';
            EXCEPTION WHEN query_canceled THEN NULL;
        END$$;",
        )
        .unwrap();
        crate::storage::testing::set_race_hook(None);
        assert!(
            queued.get() && built.get(),
            "cancel must be deferred while the merge lock is held"
        );
        assert_eq!(
            value("SELECT count(*) FROM direct_merge_cancel WHERE body ==> 'needle'"),
            2
        );
        Spi::run("INSERT INTO direct_merge_cancel VALUES (3,'needle third')").unwrap();
        assert_eq!(
            value("SELECT sum(id)::bigint FROM direct_merge_cancel WHERE body ==> 'needle'"),
            6
        );
        assert_clean("direct_merge_cancel_idx");
    }

    #[pg_test]
    fn insert_defers_large_merges_and_cleanup_finishes_them() {
        Spi::run(
            "CREATE TABLE merge_budget(body text);
             CREATE INDEX merge_budget_idx ON merge_budget USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 1;
             SET LOCAL stannum.merge_tier_factor = 2;
             SET LOCAL stannum.max_merge_docs = 4;
             SET LOCAL stannum.deferred_merge_docs = 0;
             INSERT INTO merge_budget SELECT 'needle common' FROM generate_series(1,5);",
        )
        .unwrap();
        // The fourth fold spends two documents merging singletons. Its
        // four-document cascade would exceed the remaining budget of two.
        assert_eq!(
            value(
                "SELECT max(docs) FROM stannum.segment_info('merge_budget_idx') WHERE kind = 'immutable'"
            ),
            2
        );
        Spi::run("INSERT INTO merge_budget SELECT 'needle common' FROM generate_series(6,33)")
            .unwrap();
        assert!(
            value(
                "SELECT max(docs) FROM stannum.segment_info('merge_budget_idx') WHERE kind = 'immutable'"
            ) <= 4
        );
        assert!(
            value(
                "SELECT count(*) FROM stannum.segment_info('merge_budget_idx') WHERE kind = 'immutable'"
            ) >= 8
        );
        // Exercise the same entry point as amvacuumcleanup without issuing
        // VACUUM inside the pg_test transaction.
        let oid = Spi::get_one::<pg_sys::Oid>("SELECT 'merge_budget_idx'::regclass::oid")
            .unwrap()
            .unwrap();
        unsafe {
            let index = pgrx::PgRelation::with_lock(oid, pg_sys::ShareUpdateExclusiveLock as _);
            crate::storage::cleanup(index.as_ptr());
        }
        assert_eq!(
            value(
                "SELECT count(*) FROM stannum.segment_info('merge_budget_idx') WHERE kind = 'immutable'"
            ),
            1
        );
        assert_eq!(
            value("SELECT sum(docs)::bigint FROM stannum.segment_info('merge_budget_idx')"),
            33
        );
        assert_eq!(
            value(
                "SELECT count(*) FROM stannum.verify_index('merge_budget_idx') WHERE severity = 'error'"
            ),
            0
        );
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
        assert_eq!(
            value("SELECT count(*) FROM merge_budget WHERE body ==> 'needle'"),
            33
        );
    }

    #[pg_test]
    fn full_directory_merges_only_smallest_entries_even_with_zero_budget() {
        Spi::run(
            "CREATE TABLE merge_full(body text);
             CREATE INDEX merge_full_idx ON merge_full USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 1;
             SET LOCAL stannum.merge_tier_factor = 2;
             SET LOCAL stannum.max_merge_docs = 0;
             SET LOCAL stannum.deferred_merge_docs = 0;
             INSERT INTO merge_full SELECT 'needle' FROM generate_series(1,98);",
        )
        .unwrap();
        assert_eq!(
            value(
                "SELECT count(*) FROM stannum.segment_info('merge_full_idx') WHERE kind = 'immutable'"
            ),
            96
        );
        assert_eq!(
            value(
                "SELECT max(docs) FROM stannum.segment_info('merge_full_idx') WHERE kind = 'immutable'"
            ),
            2
        );
        assert_eq!(
            value(
                "SELECT count(DISTINCT generation) FROM stannum.segment_info('merge_full_idx') WHERE kind = 'immutable'"
            ),
            96
        );
        // `stannum.max_segments` is a soft bound: with no budget, an insert
        // leaves the directory over it and only the on-disk bound forces the
        // two smallest entries to merge.
        Spi::run("SET LOCAL stannum.max_segments = 3; INSERT INTO merge_full VALUES ('needle')")
            .unwrap();
        assert_eq!(
            value(
                "SELECT count(*) FROM stannum.segment_info('merge_full_idx') WHERE kind = 'immutable'"
            ),
            96
        );
        assert_eq!(
            value("SELECT max(docs) FROM stannum.segment_info('merge_full_idx')"),
            2
        );
        // With a budget that covers it, the cheapest merge brings the
        // directory back to the soft bound.
        Spi::run(
            "SET LOCAL stannum.max_merge_docs = 1000; INSERT INTO merge_full VALUES ('needle')",
        )
        .unwrap();
        assert_eq!(
            value(
                "SELECT count(*) FROM stannum.segment_info('merge_full_idx') WHERE kind = 'immutable'"
            ),
            3
        );
        assert_eq!(
            value("SELECT sum(docs)::bigint FROM stannum.segment_info('merge_full_idx')"),
            100
        );
        assert_eq!(
            value(
                "SELECT count(*) FROM stannum.verify_index('merge_full_idx') WHERE severity = 'error'"
            ),
            0
        );
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
        assert_eq!(
            value("SELECT count(*) FROM merge_full WHERE body ==> 'needle'"),
            100
        );
    }

    #[pg_test]
    fn overflow_merges_stay_within_the_budget_below_the_hard_bound() {
        // Tier 0 fills at eight singletons (cost 8) and the soft bound is
        // four: with a budget of two, only the two smallest entries merge
        // when they are both singletons.
        Spi::run(
            "CREATE TABLE merge_soft(body text);
             CREATE INDEX merge_soft_idx ON merge_soft USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 1;
             SET LOCAL stannum.merge_tier_factor = 8;
             SET LOCAL stannum.max_segments = 4;
             SET LOCAL stannum.max_merge_docs = 2;
             SET LOCAL stannum.deferred_merge_docs = 0;
             INSERT INTO merge_soft SELECT 'needle' FROM generate_series(1,9);",
        )
        .unwrap();
        let segments = || {
            value(
                "SELECT count(*) FROM stannum.segment_info('merge_soft_idx') WHERE kind = 'immutable'",
            )
        };
        assert_eq!(segments(), 4);
        // [2,2,2,2] plus a singleton: the cheapest merge costs three.
        Spi::run("INSERT INTO merge_soft VALUES ('needle')").unwrap();
        assert_eq!(segments(), 5);
        assert_eq!(
            value("SELECT max(docs) FROM stannum.segment_info('merge_soft_idx')"),
            2
        );
        Spi::run("INSERT INTO merge_soft VALUES ('needle')").unwrap();
        assert_eq!(segments(), 6);
        // Three over the soft bound, the cheapest merge takes the four
        // smallest entries (1+1+1+2); a budget of five covers it.
        Spi::run("SET LOCAL stannum.max_merge_docs = 5; INSERT INTO merge_soft VALUES ('needle')")
            .unwrap();
        assert_eq!(segments(), 4);
        assert_eq!(
            value("SELECT max(docs) FROM stannum.segment_info('merge_soft_idx')"),
            5
        );
        // VACUUM's cleanup enforces the soft bound with no budget at all.
        Spi::run("SET LOCAL stannum.max_merge_docs = 0; INSERT INTO merge_soft SELECT 'needle' FROM generate_series(1,6)")
            .unwrap();
        assert_eq!(segments(), 10);
        let oid = Spi::get_one::<pg_sys::Oid>("SELECT 'merge_soft_idx'::regclass::oid")
            .unwrap()
            .unwrap();
        unsafe {
            let index = pgrx::PgRelation::with_lock(oid, pg_sys::ShareUpdateExclusiveLock as _);
            crate::storage::cleanup(index.as_ptr());
        }
        assert!(segments() <= 4, "{}", segments());
        assert_clean("merge_soft_idx");
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
        assert_eq!(
            value("SELECT count(*) FROM merge_soft WHERE body ==> 'needle'"),
            18
        );
    }

    #[pg_test]
    fn insert_merges_a_due_tier_over_its_inline_budget_outside_the_lock() {
        Spi::run(
            "CREATE TABLE merge_deferred(body text);
             CREATE INDEX merge_deferred_idx ON merge_deferred USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 1;
             SET LOCAL stannum.merge_tier_factor = 8;
             SET LOCAL stannum.max_merge_docs = 0;
             SET LOCAL stannum.deferred_merge_docs = 8;
             INSERT INTO merge_deferred SELECT 'needle' FROM generate_series(1,20);",
        )
        .unwrap();
        // Nineteen folds: two tiers of eight merged, three singletons since.
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT array_agg(docs ORDER BY docs)::text FROM stannum.segment_info('merge_deferred_idx') WHERE kind = 'immutable'"
            ),
            Ok(Some("{1,1,1,8,8}".into()))
        );
        // Eight eights are due next, and cost more than this budget allows.
        Spi::run("INSERT INTO merge_deferred SELECT 'needle' FROM generate_series(1,48)").unwrap();
        assert_eq!(
            value(
                "SELECT max(docs) FROM stannum.segment_info('merge_deferred_idx') WHERE kind = 'immutable'"
            ),
            8
        );
        assert_clean("merge_deferred_idx");
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
        assert_eq!(
            value("SELECT count(*) FROM merge_deferred WHERE body ==> 'needle'"),
            68
        );
    }

    /// Heap locations as `(block,offset)` text, from a statement returning ctids.
    fn tids(sql: &str) -> std::collections::BTreeSet<segment::Tid> {
        Spi::connect_mut(|client| {
            client
                .update(sql, None, &[])
                .unwrap()
                .map(|row| {
                    let text = row.get::<String>(1).unwrap().unwrap();
                    let (block, offset) = text
                        .trim_matches(|c| c == '(' || c == ')')
                        .split_once(',')
                        .unwrap();
                    segment::Tid::new(block.parse().unwrap(), offset.parse().unwrap()).unwrap()
                })
                .collect()
        })
    }

    /// Runs `sql` once at the first race point named `at`, so an insert
    /// lands between VACUUM's unlocked work and its publication.
    fn insert_at_race_point(at: &'static str, sql: &'static str) {
        let mut fired = false;
        crate::storage::testing::set_race_hook(Some(Box::new(move |name| {
            if name == at && !fired {
                fired = true;
                Spi::run(sql).unwrap();
            }
        })));
    }

    #[pg_test]
    fn insert_prepares_unlocked_and_publishes_against_current_buffer() {
        Spi::run(
            "CREATE TABLE insert_race(id int PRIMARY KEY, body text);
             CREATE INDEX insert_race_idx ON insert_race USING stannum(body)
                 WITH (case_folding = preserve);
             CREATE TEMP TABLE insert_race_observed(matches bigint, docs bigint);
             SET LOCAL enable_seqscan = off;
             SET LOCAL stannum.write_buffer_docs = 1;
             SET LOCAL stannum.max_segments = 2;
             INSERT INTO insert_race VALUES (1, 'Craft Beer');
             ALTER INDEX insert_race_idx SET (case_folding = fold);",
        )
        .unwrap();
        // Preparing the outer record must use the persisted tokenizer, not
        // the newly changed reloptions. The callback searches and reads the
        // directory before inserting: either operation would self-deadlock
        // if preparation still held the metadata page exclusively.
        insert_at_race_point(
            "insert:prepared",
            "INSERT INTO insert_race_observed SELECT
                 (SELECT count(*) FROM insert_race WHERE body ==> 'Beer'),
                 (SELECT sum(docs) FROM stannum.segment_info('insert_race_idx'));
             INSERT INTO insert_race VALUES (2, 'Craft Beer'), (3, 'craft beer');",
        );
        Spi::run("INSERT INTO insert_race VALUES (4, 'Craft Beer')").unwrap();
        crate::storage::testing::set_race_hook(None);
        // This also proves that the hook fired. Its nested inserts fold the
        // original buffer; publishing a stale captured Meta would lose them.
        assert_eq!(value("SELECT count(*) FROM insert_race_observed"), 1);
        assert_eq!(value("SELECT matches FROM insert_race_observed"), 1);
        assert_eq!(value("SELECT docs FROM insert_race_observed"), 1);
        assert_eq!(
            ids("SELECT id FROM insert_race WHERE body ==> 'Beer' ORDER BY id"),
            vec![1, 2, 4]
        );
        assert_eq!(
            ids("SELECT id FROM insert_race WHERE body ==> 'beer' ORDER BY id"),
            vec![3]
        );
        assert_eq!(
            ids("SELECT id FROM insert_race WHERE body ==> '\"Craft Beer\"' ORDER BY id"),
            vec![1, 2, 4]
        );
        assert_eq!(
            value("SELECT sum(docs)::bigint FROM stannum.segment_info('insert_race_idx')"),
            4
        );
        assert_clean("insert_race_idx");
    }

    #[pg_extern]
    fn direct_vacuum_cleanup(index_oid: pg_sys::Oid) {
        let index = unsafe {
            pgrx::PgRelation::with_lock(index_oid, pg_sys::ShareUpdateExclusiveLock as _)
        };
        unsafe { crate::storage::cleanup(index.as_ptr()) };
    }

    fn direct_vacuum_fixture() {
        Spi::run(
            "CREATE TABLE direct_vacuum(id int primary key, body text);
             CREATE INDEX direct_vacuum_idx ON direct_vacuum USING stannum(body);
             SET LOCAL stannum.write_buffer_docs=1;
             SET LOCAL stannum.max_merge_docs=0;
             SET LOCAL stannum.deferred_merge_docs = 0;
             INSERT INTO direct_vacuum SELECT n, 'needle common' FROM generate_series(1,17) n;
             SET LOCAL enable_seqscan=off;",
        )
        .unwrap();
    }

    #[pg_test]
    fn reconstructed_vacuum_cancels_before_output_and_retries() {
        Spi::run("SET LOCAL stannum.experimental_vacuum_merge_strategy = 'reconstruct'").unwrap();
        direct_vacuum_cancels_before_output_and_retries();
    }

    #[pg_test]
    fn reconstructed_vacuum_discards_retired_inputs() {
        Spi::run("SET LOCAL stannum.experimental_vacuum_merge_strategy = 'reconstruct'").unwrap();
        direct_vacuum_discards_inputs_retired_during_construction();
    }

    #[pg_test]
    fn reconstructed_vacuum_classifies_corruption_and_stale_errors() {
        Spi::run("SET LOCAL stannum.experimental_vacuum_merge_strategy = 'reconstruct'").unwrap();
        direct_vacuum_distinguishes_corruption_from_retired_inputs();
    }

    #[pg_test]
    fn reconstructed_vacuum_removes_all_dead_sources() {
        Spi::run("SET LOCAL stannum.experimental_vacuum_merge_strategy = 'reconstruct'").unwrap();
        direct_vacuum_removes_all_dead_sources_without_empty_successors();
    }

    #[pg_test]
    fn direct_vacuum_cancels_before_output_and_retries() {
        use std::{cell::Cell, rc::Rc};
        direct_vacuum_fixture();
        let before =
            value("SELECT sum(docs)::bigint FROM stannum.segment_info('direct_vacuum_idx')");
        let queued = Rc::new(Cell::new(false));
        let built = Rc::new(Cell::new(false));
        let q = queued.clone();
        let b = built.clone();
        crate::storage::testing::set_race_hook(Some(Box::new(move |name| {
            if name == "maintenance:checkpoint" && !q.replace(true) {
                unsafe {
                    let holdoff = pg_sys::InterruptHoldoffCount;
                    assert_eq!(holdoff, 0);
                    pg_sys::QueryCancelPending = 1;
                    pg_sys::InterruptPending = 1;
                }
            }
            if name == "maintenance:built" {
                b.set(true);
            }
        })));
        Spi::run(
            "DO $$BEGIN
            PERFORM tests.direct_vacuum_cleanup('direct_vacuum_idx'::regclass::oid);
            RAISE EXCEPTION 'cleanup cancellation was not delivered';
            EXCEPTION WHEN query_canceled THEN NULL;
        END$$;",
        )
        .unwrap();
        crate::storage::testing::set_race_hook(None);
        assert!(
            queued.get() && !built.get(),
            "cancel before writing merge output"
        );
        assert_eq!(
            value("SELECT sum(docs)::bigint FROM stannum.segment_info('direct_vacuum_idx')"),
            before
        );
        assert_clean("direct_vacuum_idx");
        Spi::run("SELECT tests.direct_vacuum_cleanup('direct_vacuum_idx'::regclass::oid)").unwrap();
        assert_eq!(
            value("SELECT count(*) FROM direct_vacuum WHERE body ==> 'needle'"),
            17
        );
        assert!(
            value(
                "SELECT count(*) FROM stannum.segment_info('direct_vacuum_idx') WHERE kind='immutable'"
            ) < 16
        );
        assert_clean("direct_vacuum_idx");
    }

    #[pg_test]
    fn direct_vacuum_discards_inputs_retired_during_construction() {
        use std::{cell::Cell, rc::Rc};
        direct_vacuum_fixture();
        let fired = Rc::new(Cell::new(false));
        let observed = fired.clone();
        crate::storage::testing::set_race_hook(Some(Box::new(move |name| {
            if name == "maintenance:checkpoint" && !observed.replace(true) {
                assert_eq!(unsafe { pg_sys::InterruptHoldoffCount }, 0);
                Spi::run(
                    "SET LOCAL stannum.max_merge_docs=1000000;
                    SET LOCAL stannum.max_segments=2;
                    INSERT INTO direct_vacuum VALUES (18,'needle later'), (19,'needle later');",
                )
                .unwrap();
            }
        })));
        Spi::run("SELECT tests.direct_vacuum_cleanup('direct_vacuum_idx'::regclass::oid)").unwrap();
        crate::storage::testing::set_race_hook(None);
        assert!(fired.get());
        assert_eq!(
            value("SELECT count(*) FROM direct_vacuum WHERE body ==> 'needle'"),
            19
        );
        assert_eq!(
            value("SELECT sum(docs)::bigint FROM stannum.segment_info('direct_vacuum_idx')"),
            19
        );
        assert_clean("direct_vacuum_idx");
    }

    #[pg_test]
    fn direct_vacuum_distinguishes_corruption_from_retired_inputs() {
        direct_vacuum_fixture();
        // Corrupt the owned snapshot, not the on-disk index, so the same
        // decoder error can be tested against unchanged and retired inputs.
        crate::storage::testing::CORRUPT_MAINTENANCE_INPUT.with(|flag| flag.set(true));
        Spi::run(
            "DO $$BEGIN
            PERFORM tests.direct_vacuum_cleanup('direct_vacuum_idx'::regclass::oid);
            RAISE EXCEPTION 'published corruption was not reported';
            EXCEPTION WHEN index_corrupted THEN NULL;
        END$$;",
        )
        .unwrap();
        assert_clean("direct_vacuum_idx");
        crate::storage::testing::CORRUPT_MAINTENANCE_INPUT.with(|flag| flag.set(true));
        insert_at_race_point(
            "maintenance:loaded",
            "SET LOCAL stannum.max_merge_docs=1000000;
            SET LOCAL stannum.max_segments=2;
            INSERT INTO direct_vacuum VALUES (18,'needle late'),(19,'needle late');",
        );
        Spi::run("SELECT tests.direct_vacuum_cleanup('direct_vacuum_idx'::regclass::oid)").unwrap();
        crate::storage::testing::set_race_hook(None);
        assert!(!crate::storage::testing::CORRUPT_MAINTENANCE_INPUT.with(|flag| flag.get()));
        assert_eq!(
            value("SELECT count(*) FROM direct_vacuum WHERE body ==> 'needle'"),
            19
        );
        assert_clean("direct_vacuum_idx");
    }

    #[pg_test]
    fn direct_vacuum_removes_all_dead_sources_without_empty_successors() {
        direct_vacuum_fixture();
        let dead = tids("DELETE FROM direct_vacuum RETURNING ctid::text");
        let oid = Spi::get_one::<pg_sys::Oid>("SELECT 'direct_vacuum_idx'::regclass::oid")
            .unwrap()
            .unwrap();
        let index =
            unsafe { pgrx::PgRelation::with_lock(oid, pg_sys::ShareUpdateExclusiveLock as _) };
        unsafe {
            crate::storage::testing::bulk_delete_with(index.as_ptr(), &dead);
        }
        unsafe { crate::storage::cleanup(index.as_ptr()) };
        assert_eq!(
            value("SELECT count(*) FROM stannum.segment_info('direct_vacuum_idx')"),
            0
        );
        assert_clean("direct_vacuum_idx");
        Spi::run("INSERT INTO direct_vacuum VALUES (18,'needle after cleanup')").unwrap();
        assert_eq!(
            value("SELECT sum(id)::bigint FROM direct_vacuum WHERE body ==> 'needle'"),
            18
        );
        assert_clean("direct_vacuum_idx");
    }

    #[pg_test]
    fn vacuum_publishes_against_a_directory_inserts_changed_meanwhile() {
        Spi::run(
            "CREATE TABLE vac_race(id int primary key, body text);
             CREATE INDEX vac_race_idx ON vac_race USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 1;
             SET LOCAL stannum.max_merge_docs = 0;
             SET LOCAL stannum.deferred_merge_docs = 0;
             INSERT INTO vac_race
               SELECT n, CASE WHEN n % 3 = 0 THEN 'needle common' ELSE 'other common' END
               FROM generate_series(1, 30) n;",
        )
        .unwrap();
        let dead = tids("DELETE FROM vac_race WHERE id % 5 = 0 RETURNING ctid::text");
        assert_eq!(dead.len(), 6);
        let oid = Spi::get_one::<pg_sys::Oid>("SELECT 'vac_race_idx'::regclass::oid")
            .unwrap()
            .unwrap();
        let index =
            unsafe { pgrx::PgRelation::with_lock(oid, pg_sys::ShareUpdateExclusiveLock as _) };
        let counts = || {
            (
                value(
                    "SELECT count(*) FROM stannum.segment_info('vac_race_idx') WHERE kind = 'immutable'",
                ),
                value("SELECT sum(dead_docs)::bigint FROM stannum.segment_info('vac_race_idx')"),
            )
        };
        assert_eq!(counts(), (29, 0));
        // Between scanning the 29 entries and attaching their dead lists, an
        // insert folds the buffer and merges every entry into one. The
        // prepared lists no longer apply, the merged entry is scanned in
        // the next round, and every dead location is still recorded once.
        insert_at_race_point(
            "bulk_delete:scanned",
            "SET LOCAL stannum.max_merge_docs = 1000000; SET LOCAL stannum.max_segments = 2;
             INSERT INTO vac_race SELECT n, 'needle late' FROM generate_series(31, 33) n;",
        );
        let (live, removed) =
            unsafe { crate::storage::testing::bulk_delete_with(index.as_ptr(), &dead) };
        crate::storage::testing::set_race_hook(None);
        assert_eq!((live, removed), (27, 6));
        assert_eq!(counts(), (2, 6));
        assert_clean("vac_race_idx");
        // Between building a merge or rewrite and publishing it, an insert
        // retires its inputs: the output is discarded and its pages freed,
        // and cleanup retries against the new directory.
        Spi::run(
            "SET LOCAL stannum.max_merge_docs = 0; SET LOCAL stannum.max_segments = 96;
                  INSERT INTO vac_race SELECT n, 'needle later' FROM generate_series(34, 40) n;",
        )
        .unwrap();
        assert_eq!(counts().0, 9);
        insert_at_race_point(
            "maintenance:built",
            "SET LOCAL stannum.max_merge_docs = 1000000; SET LOCAL stannum.max_segments = 2;
             INSERT INTO vac_race SELECT n, 'needle latest' FROM generate_series(41, 43) n;",
        );
        Spi::run("SET LOCAL stannum.max_segments = 2").unwrap();
        unsafe { crate::storage::cleanup(index.as_ptr()) };
        crate::storage::testing::set_race_hook(None);
        assert!(counts().0 <= 2, "{:?}", counts());
        assert_clean("vac_race_idx");
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
        assert_eq!(
            value("SELECT count(*) FROM vac_race WHERE body ==> 'needle'"),
            value("SELECT count(*) FROM vac_race WHERE body LIKE '%needle%'")
        );
        assert_eq!(
            value("SELECT count(*) FROM vac_race WHERE body ==> 'needle'"),
            21
        );
    }

    #[pg_test]
    fn cleanup_reclaims_orphaned_pages_and_leaves_pages_inserts_allocate_alone() {
        Spi::run(
            "CREATE TABLE orphans(body text);
             CREATE INDEX orphans_idx ON orphans USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 1;
             SET LOCAL stannum.max_merge_docs = 0;
             INSERT INTO orphans SELECT 'needle' FROM generate_series(1, 4);",
        )
        .unwrap();
        let oid = Spi::get_one::<pg_sys::Oid>("SELECT 'orphans_idx'::regclass::oid")
            .unwrap()
            .unwrap();
        let index =
            unsafe { pgrx::PgRelation::with_lock(oid, pg_sys::ShareUpdateExclusiveLock as _) };
        // A run written but never published, as a crash leaves behind.
        let mut leaked =
            unsafe { crate::storage::testing::leak_run(index.as_ptr(), &vec![7u8; 8 * 8000]) };
        assert_eq!(leaked.len(), 8);
        leaked.sort_unstable();
        let warnings = findings("orphans_idx", false);
        assert_eq!(warnings.len(), 8, "{}", warnings.join("\n"));
        for (block, warning) in leaked.iter().zip(&warnings) {
            assert!(
                warning.starts_with(&format!(
                    "warning: page {block}: run page referenced by nothing; VACUUM reclaims it"
                )),
                "{warning}"
            );
        }
        let size = || value("SELECT pg_relation_size('orphans_idx')");
        let before = size();
        unsafe { crate::storage::cleanup(index.as_ptr()) };
        assert_clean("orphans_idx");
        // The freed pages are reused: two folds take two pages each, and
        // four FREE pages remain for the race below.
        Spi::run("INSERT INTO orphans SELECT 'needle' FROM generate_series(5, 6)").unwrap();
        assert_eq!(size(), before);
        // An insert allocating FREE pages after the capture, before their
        // kinds are read, makes them look like orphans; the walk of what
        // changed since the capture finds them published and keeps them.
        let leaked =
            unsafe { crate::storage::testing::leak_run(index.as_ptr(), &vec![9u8; 2 * 8000]) };
        assert_eq!(leaked.len(), 2);
        insert_at_race_point(
            "orphans:captured",
            "INSERT INTO orphans SELECT 'needle' FROM generate_series(7, 9)",
        );
        unsafe { crate::storage::cleanup(index.as_ptr()) };
        crate::storage::testing::set_race_hook(None);
        assert_clean("orphans_idx");
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
        assert_eq!(
            value("SELECT count(*) FROM orphans WHERE body ==> 'needle'"),
            9
        );
    }

    #[pg_test]
    fn cleanup_leaves_a_deferred_merges_unpublished_run_alone() {
        // Four one-document segments of the lowest tier and one document in
        // the buffer; no merge has run.
        Spi::run(
            "CREATE TABLE merge_orphans(id int, body text);
             CREATE INDEX merge_orphans_idx ON merge_orphans USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 1;
             SET LOCAL stannum.merge_tier_factor = 4;
             SET LOCAL stannum.max_merge_docs = 0;
             SET LOCAL stannum.deferred_merge_docs = 0;
             INSERT INTO merge_orphans SELECT n, 'needle w' || n FROM generate_series(1, 5) n;",
        )
        .unwrap();
        let segments = || {
            Spi::get_one::<String>(
                "SELECT array_agg(docs ORDER BY docs)::text
                 FROM stannum.segment_info('merge_orphans_idx') WHERE kind = 'immutable'",
            )
            .unwrap()
            .unwrap()
        };
        assert_eq!(segments(), "{1,1,1,1}");
        let oid = Spi::get_one::<pg_sys::Oid>("SELECT 'merge_orphans_idx'::regclass::oid")
            .unwrap()
            .unwrap();
        // The next insert folds and then merges the due tier of four outside
        // the meta lock. Between writing the merged run and publishing it,
        // VACUUM's cleanup runs, with no merge of its own to do.
        let mut fired = false;
        crate::storage::testing::set_race_hook(Some(Box::new(move |name| {
            if name == "maintenance:built" && !fired {
                fired = true;
                Spi::run("SET LOCAL stannum.merge_tier_factor = 64").unwrap();
                let index = unsafe {
                    pgrx::PgRelation::with_lock(oid, pg_sys::ShareUpdateExclusiveLock as _)
                };
                unsafe { crate::storage::cleanup(index.as_ptr()) };
                Spi::run("SET LOCAL stannum.merge_tier_factor = 4").unwrap();
            }
        })));
        Spi::run(
            "SET LOCAL stannum.deferred_merge_docs = 8;
             INSERT INTO merge_orphans VALUES (6, 'needle w6');",
        )
        .unwrap();
        crate::storage::testing::set_race_hook(None);
        // The merge published: four documents in one segment, one more
        // folded beside it and the newest in the buffer.
        assert_eq!(segments(), "{1,4}");
        assert_clean("merge_orphans_idx");
        // Later folds allocate whatever pages cleanup freed; none may be the
        // merged segment's.
        Spi::run(
            "SET LOCAL stannum.deferred_merge_docs = 0;
             SET LOCAL stannum.merge_tier_factor = 64;
             INSERT INTO merge_orphans SELECT n, 'needle w' || n FROM generate_series(7, 10) n;",
        )
        .unwrap();
        assert_clean("merge_orphans_idx");
        let index =
            unsafe { pgrx::PgRelation::with_lock(oid, pg_sys::ShareUpdateExclusiveLock as _) };
        unsafe { crate::storage::cleanup(index.as_ptr()) };
        assert_clean("merge_orphans_idx");
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
        assert_eq!(
            value("SELECT count(*) FROM merge_orphans WHERE body ==> 'needle'"),
            10
        );
        for n in 1..=10 {
            assert_eq!(
                ids(&format!(
                    "SELECT id FROM merge_orphans WHERE body ==> 'w{n}' ORDER BY id"
                )),
                vec![n],
            );
        }
    }

    #[pg_test]
    fn byte_cap_folds_oversized_documents_one_at_a_time() {
        Spi::run(
            "CREATE TABLE merge_bytes(body text);
             CREATE INDEX merge_bytes_idx ON merge_bytes USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 1000;
             SET LOCAL stannum.write_buffer_bytes = 1024;
             SET LOCAL stannum.max_merge_docs = 0;
             INSERT INTO merge_bytes SELECT repeat('needle ', 3000) FROM generate_series(1,3);",
        )
        .unwrap();
        assert_eq!(
            value(
                "SELECT count(*) FROM stannum.segment_info('merge_bytes_idx') WHERE kind = 'immutable'"
            ),
            2
        );
        assert_eq!(
            value("SELECT max(docs) FROM stannum.segment_info('merge_bytes_idx')"),
            1
        );
        assert_eq!(
            value(
                "SELECT count(*) FROM stannum.verify_index('merge_bytes_idx') WHERE severity = 'error'"
            ),
            0
        );
    }

    // --- Where the unbound operator is evaluated ---------------------------------

    /// A table whose only index preserves case: `body ==> 'Beer'` matches
    /// row 1 with the index's settings and every row with the defaults.
    fn case_preserving_fixture(table: &str) {
        Spi::run(&format!(
            "CREATE TABLE {table}(id int, body text);
             INSERT INTO {table} VALUES (1, 'Beer'), (2, 'beer'), (3, 'BEER');
             CREATE INDEX {table}_idx ON {table} USING stannum(body)
               WITH (case_folding = preserve);"
        ))
        .unwrap();
    }

    #[pg_test]
    fn planned_statements_bind_wherever_they_run() {
        case_preserving_fixture("pl");
        // Views, CTEs and cursors are planned like the statement itself.
        Spi::run("CREATE VIEW pl_view AS SELECT id FROM pl WHERE body ==> 'Beer'").unwrap();
        assert_eq!(ids("SELECT id FROM pl_view ORDER BY id"), vec![1]);
        assert_eq!(
            ids("WITH m AS (SELECT id FROM pl WHERE body ==> 'Beer') SELECT id FROM m ORDER BY id"),
            vec![1]
        );
        Spi::run(
            "DECLARE pl_cursor CURSOR FOR SELECT id FROM pl WHERE body ==> 'Beer' ORDER BY id",
        )
        .unwrap();
        assert_eq!(ids("FETCH ALL FROM pl_cursor"), vec![1]);
        Spi::run("CLOSE pl_cursor").unwrap();
        // Data-modifying statements too.
        assert_eq!(
            ids(
                "WITH u AS (UPDATE pl SET id = id WHERE body ==> 'Beer' RETURNING id)
                 SELECT id FROM u"
            ),
            vec![1]
        );
        // SQL-language bodies are planned: inlined into the caller, or as
        // their own statements.
        Spi::run(
            "CREATE FUNCTION pl_count() RETURNS bigint LANGUAGE sql AS
               $$ SELECT count(*) FROM pl WHERE body ==> 'Beer' $$;
             CREATE FUNCTION pl_is(t text) RETURNS boolean LANGUAGE sql AS
               $$ SELECT t ==> 'Beer' $$;",
        )
        .unwrap();
        assert_eq!(value("SELECT pl_count()"), 1);
        assert_eq!(
            ids("SELECT id FROM pl WHERE pl_is(body) ORDER BY id"),
            vec![1]
        );
        // PL/pgSQL statements go through SPI and the planner, EXECUTE too.
        Spi::run(
            "CREATE FUNCTION pl_spi() RETURNS bigint LANGUAGE plpgsql AS $$
               DECLARE n bigint; BEGIN
                 SELECT count(*) INTO n FROM pl WHERE body ==> 'Beer';
                 RETURN n;
               END $$;
             CREATE FUNCTION pl_execute(q text) RETURNS bigint LANGUAGE plpgsql AS $$
               DECLARE n bigint; BEGIN
                 EXECUTE 'SELECT count(*) FROM pl WHERE body ==> $1' INTO n USING q;
                 RETURN n;
               END $$;",
        )
        .unwrap();
        assert_eq!(value("SELECT pl_spi()"), 1);
        assert_eq!(value("SELECT pl_execute('Beer')"), 1);
        assert_eq!(value("SELECT pl_execute('beer')"), 1);
        // Row-security policies are planned with the statement they guard:
        // USING as a restriction, WITH CHECK on the new row.
        Spi::run(
            "CREATE ROLE pl_reader; GRANT SELECT, INSERT ON pl TO pl_reader;
             ALTER TABLE pl ENABLE ROW LEVEL SECURITY;
             CREATE POLICY pl_select ON pl FOR SELECT USING (body ==> 'Beer');
             CREATE POLICY pl_insert ON pl FOR INSERT WITH CHECK (body ==> 'Beer');
             SET LOCAL ROLE pl_reader;",
        )
        .unwrap();
        assert_eq!(ids("SELECT id FROM pl ORDER BY id"), vec![1]);
        Spi::run(
            "INSERT INTO pl VALUES (4, 'Beer');
             DO $$ BEGIN
               BEGIN INSERT INTO pl VALUES (5, 'beer');
                 RAISE EXCEPTION 'policy accepted a row its index settings reject';
               EXCEPTION WHEN insufficient_privilege THEN NULL; END;
             END $$;
             RESET ROLE;",
        )
        .unwrap();
        assert_eq!(ids("SELECT id FROM pl ORDER BY id"), vec![1, 2, 3, 4]);
    }

    #[pg_test]
    fn expressions_planned_without_a_query_use_the_default_settings() {
        case_preserving_fixture("np");
        // A partial index's predicate is evaluated at build and insert time
        // with `expression_planner`, which has no query to bind against: the
        // index holds every row the default settings match.
        Spi::run(
            "CREATE INDEX np_part ON np USING stannum(body)
               WITH (case_folding = preserve) WHERE body ==> 'beer';
             INSERT INTO np VALUES (4, 'Beer');",
        )
        .unwrap();
        assert_eq!(
            value("SELECT sum(docs)::bigint FROM stannum.segment_info('np_part')"),
            4
        );
        // CHECK constraints and stored generated columns likewise.
        Spi::run(
            "CREATE TABLE np_check(id int, body text, CHECK (body ==> 'beer'));
             CREATE INDEX np_check_idx ON np_check USING stannum(body)
               WITH (case_folding = preserve);
             INSERT INTO np_check VALUES (1, 'Beer'), (2, 'BEER');
             CREATE TABLE np_gen(id int, body text,
               hit boolean GENERATED ALWAYS AS (body ==> 'beer') STORED);
             CREATE INDEX np_gen_idx ON np_gen USING stannum(body)
               WITH (case_folding = preserve);
             INSERT INTO np_gen VALUES (1, 'Beer');",
        )
        .unwrap();
        assert_eq!(value("SELECT count(*) FROM np_check"), 2);
        assert_eq!(
            Spi::get_one::<bool>("SELECT hit FROM np_gen").unwrap(),
            Some(true)
        );
        // A trigger's WHEN clause sees the new row, not a table column.
        Spi::run(
            "CREATE FUNCTION np_tag() RETURNS trigger LANGUAGE plpgsql AS
               $$ BEGIN NEW.id := NEW.id + 100; RETURN NEW; END $$;
             CREATE TRIGGER np_when BEFORE INSERT ON np FOR EACH ROW
               WHEN (NEW.body ==> 'beer') EXECUTE FUNCTION np_tag();
             INSERT INTO np VALUES (5, 'Beer');",
        )
        .unwrap();
        assert_eq!(ids("SELECT id FROM np WHERE id > 100"), vec![105]);
        // A PL/pgSQL variable, a non-inlined SQL function's argument, an
        // expression no index covers and a literal are not columns: nothing
        // to bind to.
        Spi::run(
            "CREATE FUNCTION np_var(t text) RETURNS boolean LANGUAGE plpgsql AS
               $$ BEGIN RETURN t ==> 'beer'; END $$;
             CREATE FUNCTION np_opaque(t text) RETURNS boolean LANGUAGE sql
               SET search_path = pg_catalog AS $$ SELECT t ==> 'beer' $$;",
        )
        .unwrap();
        assert_eq!(
            ids("SELECT id FROM np WHERE np_var(body) ORDER BY id"),
            vec![1, 2, 3, 4, 105]
        );
        assert_eq!(
            ids("SELECT id FROM np WHERE np_opaque(body) ORDER BY id"),
            vec![1, 2, 3, 4, 105]
        );
        assert_eq!(
            ids("SELECT id FROM np WHERE (body || '') ==> 'beer' ORDER BY id"),
            vec![1, 2, 3, 4, 105]
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT 'Beer' ==> 'beer'").unwrap(),
            Some(true)
        );
        // The same statements bound: the column's settings.
        assert_eq!(
            ids("SELECT id FROM np WHERE body ==> 'beer' ORDER BY id"),
            vec![2]
        );
    }

    #[pg_test]
    fn search_predicates_of_partial_indexes_are_proven() {
        // A partial index whose predicate is a `==>` clause holds the rows
        // the default settings match. A query clause bound to an index with
        // the default settings proves it; the planner then uses the index.
        Spi::run(
            "CREATE TABLE pp(id int, body text);
             INSERT INTO pp SELECT g, CASE WHEN g % 2 = 0 THEN 'craft beer' ELSE 'wine' END
               FROM generate_series(1, 200) g;
             CREATE INDEX pp_part ON pp USING stannum(body) WHERE body ==> 'beer';
             CREATE INDEX pp_btree ON pp (id) WHERE body ==> 'beer';",
        )
        .unwrap();
        let sql = "SELECT id FROM pp WHERE body ==> 'beer' AND body ==> 'craft' ORDER BY id";
        let expected = (2..=200).step_by(2).collect::<Vec<i32>>();
        let bound = format!("\"index\":{}", oid_of("pp_part"));
        for (mode, plan, rows) in by_mode(sql) {
            assert_eq!(rows, expected, "{mode}");
            if mode == "custom" {
                assert_eq!(plan["Index"], "pp_part", "{mode}: {plan}");
            } else {
                assert!(plan_mentions(&plan, &bound), "{mode}: {plan}");
            }
            if mode == "bitmap" {
                assert!(plan_mentions(&plan, "pp_part"), "{mode}: {plan}");
            }
        }
        // Any index with such a predicate, not only a stannum one.
        Spi::run(
            "SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off;
             SET LOCAL enable_indexscan = on; SET LOCAL stannum.enable_custom_scan = off",
        )
        .unwrap();
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (FORMAT JSON) SELECT id FROM pp WHERE body ==> 'beer' AND id = 4",
        )
        .unwrap()
        .unwrap()
        .0;
        assert!(plan_mentions(&plan, "pp_btree"), "{plan}");
        assert_eq!(
            ids("SELECT id FROM pp WHERE body ==> 'beer' AND id = 4"),
            vec![4]
        );
        // A partition's index is proven through the parent's clause.
        Spi::run(
            "CREATE TABLE ppt(id int, body text) PARTITION BY RANGE (id);
             CREATE TABLE ppt1 PARTITION OF ppt FOR VALUES FROM (1) TO (101);
             CREATE TABLE ppt2 PARTITION OF ppt FOR VALUES FROM (101) TO (201);
             INSERT INTO ppt SELECT * FROM pp;
             CREATE INDEX ppt_part ON ppt USING stannum(body) WHERE body ==> 'beer';",
        )
        .unwrap();
        let sql = "SELECT id FROM ppt WHERE body ==> 'beer' AND body ==> 'craft' ORDER BY id";
        for (mode, plan, rows) in by_mode(sql) {
            assert_eq!(rows, expected, "{mode}");
            if mode != "seq" {
                assert!(plan_mentions(&plan, "ppt1_body_idx"), "{mode}: {plan}");
            }
        }
    }

    #[pg_test]
    fn search_predicates_with_other_settings_are_never_proven() {
        // The predicate of a partial index with other settings was evaluated
        // with the defaults, so a clause bound to that index (still the first
        // covering index by OID) cannot prove it: every plan evaluates the
        // clause with the index's settings, and none scans that index.
        Spi::run(
            "CREATE TABLE pn(id int, body text);
             INSERT INTO pn VALUES (1, 'Beer'), (2, 'beer'), (3, 'BEER'), (4, 'wine');
             CREATE INDEX pn_part ON pn USING stannum(body)
               WITH (case_folding = preserve) WHERE body ==> 'beer';
             CREATE INDEX pn_full ON pn USING stannum(body);",
        )
        .unwrap();
        let bound = format!("\"index\":{}", oid_of("pn_part"));
        let sql = "SELECT id FROM pn WHERE body ==> 'beer' ORDER BY id";
        for (mode, plan, rows) in by_mode(sql) {
            assert_eq!(rows, vec![2], "{mode}");
            assert!(plan_mentions(&plan, &bound), "{mode}: {plan}");
            assert!(!plan_mentions(&plan, "pn_part"), "{mode}: {plan}");
        }
        Spi::run("DROP INDEX pn_full").unwrap();
        for (mode, plan, rows) in by_mode(sql) {
            assert_eq!(rows, vec![2], "{mode}");
            assert_eq!(plan["Node Type"], "Seq Scan", "{mode}: {plan}");
        }
    }
    #[pg_test]
    fn streaming_search_stops_at_limit_and_retains_its_snapshot() {
        Spi::run(
            "CREATE TABLE streamed(id int, body text, payload int) WITH (fillfactor=60);
            INSERT INTO streamed SELECT n, 'common red blue', 0 FROM generate_series(1, 6000) n;
            CREATE INDEX streamed_idx ON streamed USING stannum(body);
            INSERT INTO streamed VALUES (6001, 'common red blue', 0);
            SET LOCAL enable_seqscan = off;
            SET LOCAL stannum.enable_custom_scan = on;",
        )
        .unwrap();
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON)
            SELECT id FROM streamed WHERE body ==> 'common AND red' LIMIT 10",
        )
        .unwrap()
        .unwrap()
        .0;
        let scan = &plan[0]["Plan"]["Plans"][0];
        assert_eq!(scan["Custom Plan Provider"], "Stannum Text Search Scan");
        assert_eq!(scan["Candidate Strategy"], "streaming page bitmaps");
        assert_eq!(scan["Candidates Visited"], 10);
        assert!(
            scan.get("Candidates").is_none(),
            "partial traversal is not a total count"
        );
        let ids = |sql: &str| {
            Spi::connect(|client| {
                client
                    .select(sql, None, &[])
                    .unwrap()
                    .map(|row| row.get::<i32>(1).unwrap().unwrap())
                    .collect::<Vec<_>>()
            })
        };
        Spi::run("DECLARE paused NO SCROLL CURSOR FOR SELECT id FROM streamed WHERE body ==> 'common AND red'").unwrap();
        let mut seen = ids("FETCH 7 FROM paused");
        Spi::run("UPDATE streamed SET payload=1 WHERE id % 11 = 0;
            DELETE FROM streamed WHERE id % 13 = 0;
            SET LOCAL stannum.write_buffer_docs = 1;
            INSERT INTO streamed VALUES (7000, 'common red blue', 0), (7001, 'common red blue', 0);").unwrap();
        // Refresh backend caches while the earlier stream still borrows its view.
        assert!(value("SELECT count(*) FROM streamed WHERE body ==> 'common'") > 0);
        seen.extend(ids("FETCH ALL FROM paused"));
        seen.sort_unstable();
        assert_eq!(seen, (1..=6001).collect::<Vec<_>>());
        Spi::run("CLOSE paused; SET LOCAL stannum.enable_custom_scan = off;").unwrap();
        assert_index_matches_seqscan(
            "streamed",
            &[
                "common AND red",
                "common OR missing",
                "common AND NOT missing",
                "\"red blue\"",
            ],
        );
        // An extra SQL filter may require consuming more than LIMIT candidates.
        Spi::run("SET LOCAL stannum.enable_custom_scan = on; SET LOCAL enable_seqscan = off;")
            .unwrap();
        assert_eq!(
            ids("SELECT id FROM streamed WHERE body ==> 'common' AND id >= 6000 LIMIT 3"),
            vec![6000, 6001, 7000]
        );
    }

    #[pg_test]
    fn streaming_search_rewinds_for_nested_loop_rescans() {
        Spi::run("CREATE TABLE stream_rescan(id int, body text);
            INSERT INTO stream_rescan SELECT n, CASE WHEN n % 2 = 0 THEN 'common blue' ELSE 'common red' END FROM generate_series(1, 2000) n;
            CREATE INDEX stream_rescan_idx ON stream_rescan USING stannum(body);
            SET LOCAL enable_seqscan = off; SET LOCAL enable_material = off; SET LOCAL enable_memoize = off;").unwrap();
        let sql = "SELECT sum(s.id) FROM generate_series(1,3) g
            CROSS JOIN LATERAL (SELECT id FROM stream_rescan WHERE body ==> 'common AND blue' OFFSET g * 0) s";
        assert_eq!(value(sql), 3 * 1001000);
        let plan = Spi::get_one::<Json>(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}"))
            .unwrap()
            .unwrap()
            .0;
        fn find_scan(plan: &serde_json::Value) -> Option<&serde_json::Value> {
            if plan["Custom Plan Provider"] == "Stannum Text Search Scan" {
                return Some(plan);
            }
            plan.get("Plans")?.as_array()?.iter().find_map(find_scan)
        }
        let scan = find_scan(&plan[0]["Plan"]).expect("custom scan beneath lateral limit");
        assert_eq!(scan["Actual Loops"], 3);
        assert_eq!(scan["Candidates Visited"], 3000);
    }

    #[pg_test]
    fn pg_page_counts_match_heap_predicates_after_hot_updates_and_deletes() {
        Spi::run(
            "CREATE TABLE page_counts(id int, body text, payload int) WITH (fillfactor=60);
            INSERT INTO page_counts SELECT n, CASE n % 5
              WHEN 0 THEN '' WHEN 1 THEN 'common red' WHEN 2 THEN 'common blue'
              WHEN 3 THEN 'rare blue' ELSE 'common red blue' END, 0
              FROM generate_series(1, 6000) n;
            CREATE INDEX page_counts_idx ON page_counts USING stannum(body);
            SET LOCAL enable_seqscan = off;
            SET LOCAL stannum.enable_custom_scan = on;",
        )
        .unwrap();
        for mutation in [
            "SELECT 1",
            "UPDATE page_counts SET payload = 1 WHERE id % 7 = 0",
            "DELETE FROM page_counts WHERE id % 11 = 0",
            "UPDATE page_counts SET body = 'rare blue' WHERE id % 13 = 0",
        ] {
            Spi::run(mutation).unwrap();
            for (query, predicate) in [
                ("common", "body LIKE '%common%'"),
                (
                    "common AND blue",
                    "body LIKE '%common%' AND body LIKE '%blue%'",
                ),
                (
                    "common OR rare",
                    "body LIKE '%common%' OR body LIKE '%rare%'",
                ),
                (
                    "common AND NOT red",
                    "body LIKE '%common%' AND body NOT LIKE '%red%'",
                ),
                ("\"red blue\"", "body LIKE '%red blue%'"),
                (
                    "* AND NOT common",
                    "body <> '' AND body NOT LIKE '%common%'",
                ),
            ] {
                // Boolean term queries fold ordinals unless told otherwise.
                for fold in ["on", "off"] {
                    Spi::run(&format!("SET LOCAL stannum.count_fold={fold}")).unwrap();
                    assert_eq!(
                        value(&format!(
                            "SELECT count(*) FROM page_counts WHERE body ==> '{query}'"
                        )),
                        value(&format!(
                            "SELECT count(*) FROM page_counts WHERE {predicate}"
                        )),
                        "{mutation}: {query} (count_fold={fold})",
                    );
                }
            }
        }
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT count(*) FROM page_counts WHERE body ==> 'common OR rare'"
        ).unwrap().unwrap().0;
        assert_eq!(plan[0]["Plan"]["Custom Plan Provider"], "Stannum Count");
        assert_eq!(plan[0]["Plan"]["Count Strategy"], "page bitmaps");
    }

    #[pg_test]
    fn count_selector_reports_cost_and_preserves_visibility() {
        Spi::run("CREATE TABLE selector_counts(id int, body text, payload int) WITH (fillfactor=60);
            INSERT INTO selector_counts SELECT n, CASE WHEN n % 100=0 THEN 'needle red' ELSE 'common blue' END, 0 FROM generate_series(1,2000) n;
            CREATE INDEX ON selector_counts USING stannum(body);
            SET LOCAL enable_seqscan=off;
            SET LOCAL stannum.enable_custom_scan=on;
            SET LOCAL stannum.profile_count_selection=on;").unwrap();
        for mutation in [
            "SELECT 1",
            "UPDATE selector_counts SET payload=1 WHERE id%200=0",
            "DELETE FROM selector_counts WHERE id%300=0",
            "UPDATE selector_counts SET body='needle red' WHERE id%101=0",
        ] {
            Spi::run(mutation).unwrap();
            let reference =
                value("SELECT count(*) FROM selector_counts WHERE body LIKE '%needle%'");
            for threshold in [0, 1] {
                Spi::run(&format!(
                    "SET LOCAL stannum.count_page_threshold={threshold}"
                ))
                .unwrap();
                assert_eq!(
                    value("SELECT count(*) FROM selector_counts WHERE body ==> 'needle OR absent'"),
                    reference
                );
                let plan = Spi::get_one::<Json>("EXPLAIN (ANALYZE, FORMAT JSON) SELECT count(*) FROM selector_counts WHERE body ==> 'needle OR absent'").unwrap().unwrap().0;
                let node = &plan[0]["Plan"];
                assert_eq!(node["Count Selection Calls"], 1);
                assert_eq!(node["Count Estimate Supported (Last)"], true);
                assert_eq!(
                    node["Count Strategy"],
                    if threshold == 0 {
                        "scalar"
                    } else {
                        "page bitmaps"
                    }
                );
                let total = node["Count Selection Time"].as_f64().unwrap();
                let estimation = node["Count Estimation Time"].as_f64().unwrap();
                assert!(total >= estimation && estimation >= 0.0);
            }
        }
        let unexecuted = Spi::get_one::<Json>("EXPLAIN (FORMAT JSON) SELECT count(*) FROM selector_counts WHERE body ==> 'needle OR absent'").unwrap().unwrap().0;
        assert!(unexecuted[0]["Plan"].get("Count Estimation Time").is_none());
        Spi::run("SET LOCAL stannum.force_count_pages=on").unwrap();
        let plan = Spi::get_one::<Json>("EXPLAIN (ANALYZE, FORMAT JSON) SELECT count(*) FROM selector_counts WHERE body ==> 'needle OR absent'").unwrap().unwrap().0;
        assert_eq!(plan[0]["Plan"]["Count Strategy"], "page bitmaps");
        assert!(
            plan[0]["Plan"]
                .get("Count Estimate Supported (Last)")
                .is_none()
        );
        assert_eq!(plan[0]["Plan"]["Count Estimation Time"].as_f64(), Some(0.0));
    }

    #[pg_test]
    fn forced_count_pages_preserve_sparse_results_and_visibility() {
        Spi::run(
            "CREATE TABLE force_count_pages(id int, body text, payload int) WITH (fillfactor=60);
             INSERT INTO force_count_pages SELECT n,
               CASE WHEN n % 100 = 0 THEN 'needle red' ELSE 'common blue' END, 0
               FROM generate_series(1, 2000) n;
             CREATE INDEX ON force_count_pages USING stannum(body);
             SET LOCAL enable_seqscan = off;
             SET LOCAL stannum.enable_custom_scan = on;
             SET LOCAL stannum.count_fold = off;",
        )
        .unwrap();
        for mutation in [
            "SELECT 1",
            "UPDATE force_count_pages SET payload=1 WHERE id % 200=0",
            "DELETE FROM force_count_pages WHERE id % 300=0",
            "UPDATE force_count_pages SET body='needle red' WHERE id % 101=0",
        ] {
            Spi::run(mutation).unwrap();
            let reference =
                value("SELECT count(*) FROM force_count_pages WHERE body LIKE '%needle%'");
            for (setting, strategy) in [("off", "scalar"), ("on", "page bitmaps")] {
                Spi::run(&format!("SET LOCAL stannum.force_count_pages={setting}")).unwrap();
                assert_eq!(
                    value("SELECT count(*) FROM force_count_pages WHERE body ==> 'needle'"),
                    reference
                );
                let plan = Spi::get_one::<Json>(
                    "EXPLAIN (ANALYZE, FORMAT JSON) SELECT count(*) FROM force_count_pages WHERE body ==> 'needle'"
                ).unwrap().unwrap().0;
                assert_eq!(plan[0]["Plan"]["Custom Plan Provider"], "Stannum Count");
                assert_eq!(
                    plan[0]["Plan"]["Count Strategy"], strategy,
                    "{mutation}: {setting}"
                );
            }
        }
    }

    #[pg_test]
    fn count_fold_agrees_with_scalar_counts_across_mutations() {
        Spi::run(
            "CREATE TABLE fold_counts(id int, body text, payload int) WITH (fillfactor=60);
             INSERT INTO fold_counts SELECT n,
               CASE WHEN n % 100 = 0 THEN 'needle red'
                    WHEN n % 3 = 0 THEN 'common red'
                    ELSE 'common blue' END, 0
               FROM generate_series(1, 3000) n;
             CREATE INDEX ON fold_counts USING stannum(body);
             SET LOCAL enable_seqscan = off;
             SET LOCAL stannum.enable_custom_scan = on;",
        )
        .unwrap();
        for mutation in [
            "SELECT 1",
            "UPDATE fold_counts SET payload=1 WHERE id % 200=0",
            "DELETE FROM fold_counts WHERE id % 7=0",
            "UPDATE fold_counts SET body='needle blue' WHERE id % 101=0",
            // Rows that reach the write buffer rather than a segment.
            "INSERT INTO fold_counts SELECT n, 'needle common green', 0 FROM generate_series(3001, 3050) n",
        ] {
            Spi::run(mutation).unwrap();
            for query in [
                "needle",
                "needle OR blue",
                "needle AND red",
                "common AND (red OR green)",
                "absent OR needle OR green",
                "absent AND common",
            ] {
                let sql = format!("SELECT count(*) FROM fold_counts WHERE body ==> '{query}'");
                Spi::run("SET LOCAL stannum.count_fold=off").unwrap();
                let reference = value(&sql);
                Spi::run("SET LOCAL stannum.count_fold=on").unwrap();
                assert_eq!(value(&sql), reference, "{mutation}: {query}");
                let plan = Spi::get_one::<Json>(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}"))
                    .unwrap()
                    .unwrap()
                    .0;
                assert_eq!(plan[0]["Plan"]["Count Strategy"], "ordinal fold", "{query}");
            }
            assert_eq!(
                value("SELECT count(*) FROM fold_counts WHERE body ==> 'needle'"),
                value("SELECT count(*) FROM fold_counts WHERE body LIKE '%needle%'")
            );
            // Positional queries keep the existing strategies.
            let plan = Spi::get_one::<Json>(
                "EXPLAIN (ANALYZE, FORMAT JSON) SELECT count(*) FROM fold_counts WHERE body ==> '\"common red\"'",
            )
            .unwrap()
            .unwrap()
            .0;
            assert_ne!(plan[0]["Plan"]["Count Strategy"], "ordinal fold");
        }
    }

    /// Index and sequential-scan answers for `query`, which must agree, with
    /// the custom scan path enabled.
    fn exact_count(table: &str, query: &str) -> i64 {
        Spi::run("SET LOCAL stannum.enable_custom_scan = on; SET LOCAL enable_seqscan = off;")
            .unwrap();
        let indexed = value(&format!(
            "SELECT count(*) FROM {table} WHERE body ==> '{query}'"
        ));
        Spi::run("SET LOCAL enable_seqscan = on; SET LOCAL enable_bitmapscan = off;").unwrap();
        let reference = value(&format!(
            "SELECT count(*) FROM {table} WHERE body ==> '{query}'"
        ));
        Spi::run(
            "SET LOCAL stannum.enable_custom_scan = off; SET LOCAL enable_seqscan = off;
             SET LOCAL enable_bitmapscan = on;",
        )
        .unwrap();
        assert_eq!(indexed, reference, "{query}");
        indexed
    }

    /// The top three `needle` rows of `table` by full score, through the
    /// custom scan and through the bitmap path.
    fn top_needle_both_paths(table: &str) -> (Vec<i32>, Vec<i32>) {
        let sql = format!(
            "SELECT id FROM {table} WHERE body ==> 'needle' ORDER BY stannum.full_score(ctid) DESC, id LIMIT 3"
        );
        Spi::run("SET LOCAL stannum.enable_custom_scan = on; SET LOCAL enable_seqscan = off;")
            .unwrap();
        let custom = ids(&sql);
        Spi::run("SET LOCAL stannum.enable_custom_scan = off;").unwrap();
        let bitmap = ids(&sql);
        (custom, bitmap)
    }

    #[pg_test]
    fn memoized_term_lookups_stay_exact_across_folds_merges_reindex_and_drop() {
        // Every segment memoizes its dictionary lookups per backend. The memo
        // must never answer for a segment it was not built from: new folds,
        // merged generations, a rebuilt identity and a recreated index all
        // carry fresh lookups, while the memo of a live segment keeps serving
        // its (immutable) answer, including a remembered absence.
        Spi::run(
            "CREATE TABLE memo(id int primary key, body text);
             INSERT INTO memo SELECT n, 'filler w' || (n % 5) FROM generate_series(1, 40) n;
             CREATE INDEX memo_idx ON memo USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 3;
             SET LOCAL stannum.merge_tier_factor = 2;",
        )
        .unwrap();
        assert_eq!(exact_count("memo", "needle"), 0);
        assert_eq!(exact_count("memo", "w1"), 8);
        let probe = crate::storage::cache_probe();
        assert_eq!(probe.cached_segments, 1);
        assert!(probe.memoized_terms >= 2, "{probe:?}");
        // Absence is memoized for generation 1; the folds that follow put
        // `needle` into new generations and into merged ones.
        Spi::run(
            "INSERT INTO memo VALUES (101, 'needle'), (102, 'needle other'), (103, 'other'),
               (104, 'needle w1'), (105, 'w1'), (106, 'needle'), (107, 'needle');",
        )
        .unwrap();
        assert!(directory_shape("memo_idx").0 >= 2);
        assert_eq!(exact_count("memo", "needle"), 5);
        assert_eq!(exact_count("memo", "w1"), 10);
        assert_eq!(exact_count("memo", "needle AND w1"), 1);
        Spi::run("UPDATE memo SET body = 'needle moved' WHERE id = 1").unwrap();
        assert_eq!(exact_count("memo", "needle"), 6);
        assert_eq!(exact_count("memo", "w1"), 9);
        let (custom, bitmap) = top_needle_both_paths("memo");
        assert_eq!(custom.len(), 3);
        assert_eq!(custom, bitmap);
        let before = crate::storage::cache_probe();
        Spi::run("REINDEX INDEX memo_idx").unwrap();
        assert_eq!(exact_count("memo", "needle"), 6);
        assert_eq!(exact_count("memo", "w1"), 9);
        assert_eq!(exact_count("memo", "missing"), 0);
        let after = crate::storage::cache_probe();
        assert_ne!(before, after);
        Spi::run(
            "DROP INDEX memo_idx;
             INSERT INTO memo VALUES (108, 'needle w1');
             CREATE INDEX memo_idx ON memo USING stannum(body);",
        )
        .unwrap();
        assert_eq!(exact_count("memo", "needle"), 7);
        assert_eq!(exact_count("memo", "needle AND w1"), 2);
        assert_index_matches_seqscan("memo", &["needle", "w1", "needle AND w1", "w*", "missing"]);
    }

    #[pg_test]
    fn buffer_index_extends_incrementally_and_restarts_on_epoch_and_identity_changes() {
        // The per-backend buffer index keys on what the meta page says, which
        // is the same for every backend: appends by any writer are absorbed
        // from the covered byte onward; a VACUUM rewrite or a fold starts a
        // new epoch and a new index; REINDEX changes the identity.
        use std::collections::BTreeSet;
        Spi::run(
            "CREATE TABLE bufidx(id int primary key, body text);
             CREATE INDEX bufidx_idx ON bufidx USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 100;
             INSERT INTO bufidx SELECT n, 'needle n' || n FROM generate_series(1, 10) n;",
        )
        .unwrap();
        assert_eq!(exact_count("bufidx", "needle"), 10);
        let first = crate::storage::cache_probe().buffer.unwrap();
        assert_eq!(first.3, 10);
        // Another writer appends: the index covers the new bytes only.
        Spi::run("INSERT INTO bufidx SELECT n, 'needle n' || n FROM generate_series(11, 15) n")
            .unwrap();
        assert_eq!(exact_count("bufidx", "needle"), 15);
        assert_eq!(exact_count("bufidx", "n12"), 1);
        let grown = crate::storage::cache_probe().buffer.unwrap();
        assert_eq!(
            (grown.0, grown.1),
            (first.0, first.1),
            "same identity and epoch"
        );
        assert!(grown.2 > first.2, "covers the appended bytes");
        assert_eq!(grown.3, 15);
        // VACUUM rewrites the buffer without the dead records: new epoch.
        let mut dead: BTreeSet<(u32, u16)> = Spi::connect(|client| {
            client
                .select("SELECT ctid FROM bufidx WHERE id <= 3", None, &[])
                .unwrap()
                .map(|row| {
                    let tid = row.get::<pg_sys::ItemPointerData>(1).unwrap().unwrap();
                    (
                        (u32::from(tid.ip_blkid.bi_hi) << 16) | u32::from(tid.ip_blkid.bi_lo),
                        tid.ip_posid,
                    )
                })
                .collect()
        });
        Spi::run("DELETE FROM bufidx WHERE id <= 3").unwrap();
        unsafe extern "C-unwind" fn deleted(
            tid: pg_sys::ItemPointer,
            state: *mut std::ffi::c_void,
        ) -> bool {
            let tid = unsafe { *tid };
            let dead = unsafe { &*state.cast::<BTreeSet<(u32, u16)>>() };
            dead.contains(&(
                (u32::from(tid.ip_blkid.bi_hi) << 16) | u32::from(tid.ip_blkid.bi_lo),
                tid.ip_posid,
            ))
        }
        let index = unsafe { pgrx::PgRelation::open_with_name("bufidx_idx") }.unwrap();
        unsafe {
            crate::storage::bulk_delete(
                index.as_ptr(),
                Some(deleted),
                std::ptr::from_mut(&mut dead).cast(),
            );
        }
        drop(index);
        assert_eq!(exact_count("bufidx", "needle"), 12);
        assert_eq!(exact_count("bufidx", "n2"), 0);
        let rewritten = crate::storage::cache_probe().buffer.unwrap();
        assert_eq!(rewritten.0, first.0);
        assert_ne!(rewritten.1, first.1, "VACUUM's rewrite starts an epoch");
        assert_eq!(rewritten.3, 12);
        // A fold empties the buffer and starts another epoch; the one record
        // appended afterwards is all the new index holds.
        Spi::run(
            "SET LOCAL stannum.write_buffer_docs = 1;
             INSERT INTO bufidx VALUES (16, 'needle n16');",
        )
        .unwrap();
        assert_eq!(exact_count("bufidx", "needle"), 13);
        let folded = crate::storage::cache_probe().buffer.unwrap();
        assert_ne!(folded.1, rewritten.1);
        assert_eq!(folded.3, 1);
        assert!(directory_shape("bufidx_idx").0 >= 1);
        // REINDEX changes the identity; nothing of the old index is reused.
        Spi::run(
            "SET LOCAL stannum.write_buffer_docs = 100;
             REINDEX INDEX bufidx_idx;
             INSERT INTO bufidx VALUES (17, 'needle n17');",
        )
        .unwrap();
        assert_eq!(exact_count("bufidx", "needle"), 14);
        let rebuilt = crate::storage::cache_probe().buffer.unwrap();
        assert_ne!(rebuilt.0, first.0, "REINDEX changes the identity");
        assert_eq!(rebuilt.3, 1);
        assert_index_matches_seqscan("bufidx", &["needle", "n17", "n2", "n1*"]);
        assert_eq!(
            value(
                "SELECT count(*) FROM stannum.verify_index('bufidx_idx') WHERE severity = 'error'"
            ),
            0
        );
    }

    #[pg_test]
    fn ranked_walks_skip_dead_listed_documents_in_every_chunk_form() {
        // VACUUM reports rows dead while their heap tuples stay visible, as
        // a slot reused after VACUUM is: the dead list alone must keep them
        // out of the ranked walk. Found by the ranked-scan fuzzer: the
        // disjunction walk masked the dead ordinals before it rebuilt its
        // candidates from the essential terms, and never masked a list chunk.
        Spi::run(
            "CREATE TABLE deadlist(id int primary key, body text);
             INSERT INTO deadlist SELECT n,
               CASE WHEN n % 3 = 0 THEN 'needle pad' WHEN n % 3 = 1 THEN 'other pad'
                    ELSE 'needle other' END || CASE WHEN n % 10 = 0 THEN ' rare' ELSE '' END
             FROM generate_series(1, 300) n;
             CREATE INDEX deadlist_idx ON deadlist USING stannum(body);
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        let dead = tids("SELECT ctid::text FROM deadlist WHERE id IN (30, 60, 3, 5)");
        assert_eq!(dead.len(), 4);
        let oid = Spi::get_one::<pg_sys::Oid>("SELECT 'deadlist_idx'::regclass::oid")
            .unwrap()
            .unwrap();
        let index =
            unsafe { pgrx::PgRelation::with_lock(oid, pg_sys::ShareUpdateExclusiveLock as _) };
        unsafe { crate::storage::testing::bulk_delete_with(index.as_ptr(), &dead) };
        drop(index);
        // Bitmap chunks (needle, other: 200 documents each), a list chunk
        // (rare: 30), and their combinations, at a limit past every match.
        for query in [
            "needle",
            "rare",
            "rare OR needle",
            "needle OR other",
            "needle AND other",
            "rare AND needle",
        ] {
            let ids: Vec<i32> = Spi::connect(|client| {
                client
                    .select(
                        &format!(
                            "SELECT id FROM deadlist WHERE body ==> '{query}'
                             ORDER BY stannum.full_score(ctid) DESC LIMIT 400"
                        ),
                        None,
                        &[],
                    )
                    .unwrap()
                    .map(|row| row.get::<i32>(1).unwrap().unwrap())
                    .collect()
            });
            assert!(
                ids.iter().all(|id| ![30, 60, 3, 5].contains(id)),
                "{query}: dead-listed rows returned by the ranked walk"
            );
            assert_eq!(
                ids.len() as i64,
                value(&format!(
                    "SELECT count(*) FROM deadlist WHERE body ==> '{query}' AND id NOT IN (30, 60, 3, 5)"
                )),
                "{query}"
            );
        }
    }

    #[pg_test]
    fn a_replaced_dead_list_in_the_same_pages_is_not_served_from_cache() {
        // A dead list is rewritten whole by every VACUUM that finds more
        // dead rows, and the old list's pages come back through the free
        // space map; a replacement of the same size lands in the same pages
        // with the same byte count, so a reader's cached copy keyed by the
        // run alone would stand. Found by the ranked-scan fuzzer.
        Spi::run(
            "CREATE TABLE stamped(id int primary key, body text);
             INSERT INTO stamped SELECT n, 'needle pad' FROM generate_series(1, 100) n;
             CREATE INDEX stamped_idx ON stamped USING stannum(body);
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        let oid = Spi::get_one::<pg_sys::Oid>("SELECT 'stamped_idx'::regclass::oid")
            .unwrap()
            .unwrap();
        let index =
            unsafe { pgrx::PgRelation::with_lock(oid, pg_sys::ShareUpdateExclusiveLock as _) };
        let ranked = || -> Vec<i32> {
            Spi::connect(|client| {
                client
                    .select(
                        "SELECT id FROM stamped WHERE body ==> 'needle'
                         ORDER BY stannum.full_score(ctid) DESC LIMIT 200",
                        None,
                        &[],
                    )
                    .unwrap()
                    .map(|row| row.get::<i32>(1).unwrap().unwrap())
                    .collect()
            })
        };
        let unordered = || value("SELECT count(*) FROM stamped WHERE body ==> 'needle'");
        let swap = |dead: i32| unsafe {
            crate::storage::testing::set_dead_list(
                index.as_ptr(),
                0,
                &tids(&format!("SELECT ctid::text FROM stamped WHERE id = {dead}")),
            )
        };
        let first = swap(7);
        let ids = ranked();
        assert!(!ids.contains(&7) && ids.len() == 99, "{ids:?}");
        assert_eq!(unordered(), 99);
        // Each list is written before the one it replaces is freed, so two
        // swaps bring the third list back to the first list's page, at the
        // first list's size, with a reader that last saw the first list.
        let second = swap(8);
        let third = swap(9);
        assert_ne!(second, first);
        assert_eq!(third, first, "{first:?} {second:?} {third:?}");
        let ids = ranked();
        assert!(ids.contains(&7) && ids.contains(&8), "{ids:?}");
        assert!(!ids.contains(&9) && ids.len() == 99, "{ids:?}");
        assert_eq!(unordered(), 99);
        drop(index);
        // The fabricated list names live rows, which the heap check would
        // rightly report; the structure is what must hold.
        let rows = findings("stamped_idx", false);
        assert!(rows.is_empty(), "{}", rows.join("\n"));
    }

    #[pg_test]
    fn per_row_scores_do_not_depend_on_the_order_rows_are_scored_in() {
        // The unpruned path scores rows as the executor hands them over: in
        // heap order under a bitmap scan, in any order under a join or an
        // ordered index scan. Found by the ranked-scan fuzzer: a lookup past a
        // term's last member exhausted its cursor, and every later row, even
        // one the term listed, then scored without that term.
        Spi::run(
            "CREATE TABLE ordered(id int primary key, body text);
             INSERT INTO ordered SELECT n, CASE WHEN n <= 100 THEN 'needle common' ELSE 'common' END
             FROM generate_series(1, 200) n;
             CREATE INDEX ordered_idx ON ordered USING stannum(body);
             SET LOCAL stannum.enable_custom_scan = off;
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        let scores = |order: &str, bitmap: bool| -> Vec<(i32, u32)> {
            Spi::run(&format!("SET LOCAL enable_bitmapscan = {bitmap}")).unwrap();
            Spi::connect(|client| {
                client
                    .select(
                        &format!(
                            "SELECT id, stannum.full_score(ctid) AS score FROM ordered
                             WHERE body ==> 'needle OR common' ORDER BY id {order}"
                        ),
                        None,
                        &[],
                    )
                    .unwrap()
                    .map(|row| {
                        (
                            row.get::<i32>(1).unwrap().unwrap(),
                            row.get::<f32>(2).unwrap().unwrap().to_bits(),
                        )
                    })
                    .collect()
            })
        };
        // Heap order, under the bitmap scan, is the reference.
        let mut ascending = scores("ASC", true);
        // A backward scan of the primary key hands rows over in descending
        // heap order, with the text predicate as a filter.
        let mut descending = scores("DESC", false);
        assert_eq!(ascending.len(), 200);
        assert_eq!(descending.len(), 200);
        assert_ne!(ascending[0].1, ascending[199].1);
        ascending.sort_unstable();
        descending.sort_unstable();
        assert_eq!(ascending, descending);
    }

    #[pg_test]
    fn hot_updated_rows_keep_their_score_on_both_ranked_paths() {
        // A HOT update leaves the posting at the root of the chain while the
        // executor projects the visible member's location. Found by the
        // ranked-scan fuzzer: the unpruned path scored such rows zero and the
        // pruned path ordered them by their real score but reported zero.
        Spi::run(
            "CREATE TABLE hot(id int primary key, body text, revision int default 0)
               WITH (fillfactor = 50);
             INSERT INTO hot SELECT n, CASE WHEN n % 3 = 0 THEN 'needle needle pad'
               WHEN n % 3 = 1 THEN 'needle pad pad' ELSE 'other' END
               FROM generate_series(1, 30) n;
             CREATE INDEX hot_idx ON hot USING stannum(body);
             UPDATE hot SET revision = revision + 1 WHERE id IN (3, 4);
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        // The updated rows moved within their page: heap-only members.
        assert_eq!(
            value("SELECT count(*) FROM hot WHERE id IN (3, 4) AND ctid > '(0,30)'::tid"),
            2
        );
        let rows = |custom: bool, limit: usize| -> Vec<(i32, u32)> {
            Spi::run(&format!(
                "SET LOCAL stannum.enable_custom_scan = {custom};
                 SET LOCAL enable_bitmapscan = {};",
                !custom
            ))
            .unwrap();
            Spi::connect(|client| {
                client
                    .select(
                        &format!(
                            "SELECT id, stannum.full_score(ctid) AS score FROM hot
                             WHERE body ==> 'needle' ORDER BY score DESC{} LIMIT {limit}",
                            // The custom scan breaks score ties by the indexed
                            // HOT root, while SQL ctid is the visible member.
                            // IDs follow the original root order in this fixture.
                            if custom { "" } else { ", id" }
                        ),
                        None,
                        &[],
                    )
                    .unwrap()
                    .map(|row| {
                        (
                            row.get::<i32>(1).unwrap().unwrap(),
                            row.get::<f32>(2).unwrap().unwrap().to_bits(),
                        )
                    })
                    .collect()
            })
        };
        let pruned = rows(true, 8);
        let unpruned = rows(false, 8);
        assert_eq!(pruned, unpruned);
        let score = |id: i32| pruned.iter().find(|(i, _)| *i == id).map(|(_, s)| *s);
        // The member scores as its root document: the same as any unmoved
        // row with the same body, and never zero.
        assert_eq!(pruned[0].0, 3);
        assert_eq!(score(3), score(6));
        assert!(f32::from_bits(score(3).unwrap()) > 0.0);
        // Compare the lower-scoring HOT member too, with scoring and the
        // matching predicate at one query level (no flattened self-join).
        let all = rows(false, 30);
        let score = |id: i32| all.iter().find(|(i, _)| *i == id).unwrap().1;
        assert_eq!(score(4), score(7));
        assert!(f32::from_bits(score(4)) > 0.0);
    }

    #[pg_test]
    fn concurrent_cursors_on_one_query_keep_their_own_scores() {
        // Found by the ranked-scan fuzzer: a scorer keyed by a backend-wide
        // statement counter is replaced by any later scan on the same query,
        // so a cursor's remaining rows were projected with statistics that
        // documents indexed in between had changed, out of step with the
        // order the cursor ranked them in.
        Spi::run(
            "CREATE TABLE twin(id int primary key, body text);
             INSERT INTO twin SELECT n, 'other filler' FROM generate_series(1, 300) n;
             INSERT INTO twin SELECT n, repeat('needle ', 1 + n % 4) || repeat('pad ', n % 7)
               FROM generate_series(301, 340) n;
             CREATE INDEX twin_idx ON twin USING stannum(body);
             DELETE FROM twin WHERE id IN (303, 307, 311);
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        // The deleted rows stay posted, so cursor a's pruned top 12 holds
        // invisible rows and a completes its ordering after b was opened.
        let rows = |sql: &str| {
            Spi::connect(|client| {
                client
                    .select(sql, None, &[])
                    .unwrap()
                    .map(|row| {
                        (
                            row.get::<i32>(1).unwrap().unwrap(),
                            row.get::<f32>(2).unwrap().unwrap().to_bits(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
        };
        let query = "SELECT id, stannum.full_score(ctid) AS score FROM twin WHERE body ==> 'needle' ORDER BY score DESC";
        Spi::run("SET LOCAL stannum.enable_custom_scan = off;").unwrap();
        let before = rows(&format!("{query}, ctid"));
        Spi::run(&format!(
            "SET LOCAL stannum.enable_custom_scan = on; SET LOCAL enable_bitmapscan = off;
             DECLARE a CURSOR FOR {query} LIMIT 12;"
        ))
        .unwrap();
        let mut from_a = rows("FETCH 3 FROM a");
        // New documents change every statistic the scores depend on.
        Spi::run(
            "INSERT INTO twin SELECT n, 'needle needle needle needle needle needle'
             FROM generate_series(401, 460) n",
        )
        .unwrap();
        Spi::run("SET LOCAL stannum.enable_custom_scan = off;").unwrap();
        let after = rows(&format!("{query}, ctid"));
        assert_ne!(before[..12], after[..12]);
        Spi::run(&format!(
            "SET LOCAL stannum.enable_custom_scan = on; DECLARE b CURSOR FOR {query} LIMIT 12;"
        ))
        .unwrap();
        let mut from_b = rows("FETCH 5 FROM b");
        from_a.extend(rows("FETCH ALL FROM a"));
        from_b.extend(rows("FETCH ALL FROM b"));
        assert_eq!(from_a, before[..12]);
        assert_eq!(from_b, after[..12]);
        Spi::run("CLOSE a; CLOSE b;").unwrap();
    }

    /// The advisory lock a count waits on at its `count:view` race point.
    const COUNT_RACE_LOCK: i64 = 0x5354_4e43;

    /// Makes this session's counts wait at the `count:view` race point, after
    /// they captured their index view, until [`COUNT_RACE_LOCK`] is free.
    #[pg_extern]
    fn count_race_pause() {
        crate::storage::testing::set_race_hook(Some(Box::new(|name| {
            if name == "count:view" {
                Spi::run(&format!(
                    "SELECT pg_advisory_lock_shared({COUNT_RACE_LOCK});
                     SELECT pg_advisory_unlock_shared({COUNT_RACE_LOCK});"
                ))
                .unwrap();
            }
        })));
    }

    /// Whether the visibility map marks `block` of `heap` all-visible.
    #[pg_extern]
    fn count_race_all_visible(heap: pg_sys::Oid, block: i64) -> bool {
        unsafe {
            let relation = pg_sys::table_open(heap, pg_sys::AccessShareLock as _);
            let mut vmbuf = pg_sys::InvalidBuffer as pg_sys::Buffer;
            let status = pg_sys::visibilitymap_get_status(relation, block as u32, &mut vmbuf);
            if vmbuf != pg_sys::InvalidBuffer as pg_sys::Buffer {
                pg_sys::ReleaseBuffer(vmbuf);
            }
            pg_sys::table_close(relation, pg_sys::AccessShareLock as _);
            status & pg_sys::VISIBILITYMAP_ALL_VISIBLE as u8 != 0
        }
    }

    /// Lets [`count_misses_rows_vacuum_removes_after_its_view`] start the
    /// test server.
    #[pg_extern]
    fn count_race_ready() {}

    /// A count captures its view listing a deleted row; VACUUM then publishes
    /// the row's removal (a segment's dead list, or a rewritten write buffer),
    /// frees its line pointer and marks the page all-visible, as the deleter
    /// being older than the count's snapshot allows. The count must not take
    /// the page's all-visible bit to cover a row its view still lists. This
    /// needs separate connections: VACUUM cannot run inside the test
    /// transaction, and the row must be removable under the count's snapshot.
    #[test]
    fn count_misses_rows_vacuum_removes_after_its_view() {
        pgrx_tests::run_test(
            "count_race_ready",
            None,
            crate::pg_test::postgresql_conf_options(),
        )
        .unwrap();
        // A database of its own: VACUUM's horizon for its tables ignores the
        // snapshots of tests running in parallel in the test database.
        let (mut server, _) = pgrx_tests::client().unwrap();
        let setting = |server: &mut postgres::Client, name: &str| -> String {
            server
                .query_one(&format!("SELECT current_setting('{name}')"), &[])
                .unwrap()
                .get(0)
        };
        let socket = setting(&mut server, "unix_socket_directories");
        let socket = socket.split(',').next().unwrap().trim().to_owned();
        let port: u16 = setting(&mut server, "port").parse().unwrap();
        let user: String = server
            .query_one("SELECT current_user::text", &[])
            .unwrap()
            .get(0);
        server
            .batch_execute("DROP DATABASE IF EXISTS count_race WITH (FORCE)")
            .unwrap();
        server.batch_execute("CREATE DATABASE count_race").unwrap();
        let connect = || {
            postgres::Config::new()
                .host(&socket)
                .port(port)
                .user(&user)
                .dbname("count_race")
                .connect(postgres::NoTls)
                .unwrap()
        };
        let mut admin = connect();
        // Rows 1-400 are in a segment, 1001-1060 in the write buffer.
        admin
            .batch_execute(
                "CREATE EXTENSION stannum;
                 CREATE TABLE count_race(id int PRIMARY KEY, body text)
                     WITH (autovacuum_enabled = off);
                 INSERT INTO count_race SELECT n,
                     CASE WHEN n % 2 = 0 THEN 'alpha beta' ELSE 'alpha gamma' END
                     FROM generate_series(1, 400) n;
                 CREATE INDEX count_race_idx ON count_race USING stannum(body);
                 INSERT INTO count_race SELECT n,
                     CASE WHEN n % 2 = 0 THEN 'alpha beta' ELSE 'alpha gamma' END
                     FROM generate_series(1001, 1060) n;",
            )
            .unwrap();
        admin.batch_execute("VACUUM count_race").unwrap();
        let buffered: i64 = admin
            .query_one(
                "SELECT count(*) FROM stannum.segment_info('count_race_idx')
                 WHERE kind = 'mutable' AND docs = 60",
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(buffered, 1, "rows 1001-1060 are in the write buffer");
        // (deleted row, ==> query, equivalent SQL predicate, setting); every
        // deleted row matches its query.
        let cases = [
            (2, "alpha", "true", "stannum.count_fold = off"),
            (4, "\"alpha beta\"", "body = 'alpha beta'", ""),
            (
                6,
                "\"alpha beta\"",
                "body = 'alpha beta'",
                "stannum.force_count_pages = on",
            ),
            (8, "alpha AND NOT gamma", "body = 'alpha beta'", ""),
            (10, "alph*", "true", ""),
            (1002, "\"alpha beta\"", "body = 'alpha beta'", ""),
            (1004, "alpha", "true", "stannum.count_fold = off"),
            (
                1006,
                "alpha AND NOT gamma",
                "body = 'alpha beta'",
                "stannum.force_count_pages = on",
            ),
            // The fold already checks its view after reading the map.
            (12, "alpha", "true", "stannum.count_fold = on"),
        ];
        let mut wrong = Vec::new();
        for (id, query, predicate, setting) in cases {
            let block: i64 = admin
                .query_one(
                    "SELECT (ctid::text::point)[0]::bigint FROM count_race WHERE id = $1",
                    &[&id],
                )
                .unwrap()
                .get(0);
            let mut delete = admin.transaction().unwrap();
            delete
                .execute("DELETE FROM count_race WHERE id = $1", &[&id])
                .unwrap();
            let deleter: i64 = delete
                .query_one("SELECT txid_current()", &[])
                .unwrap()
                .get(0);
            delete.commit().unwrap();
            // A snapshot's xmin counts transactions in every database: wait
            // until none that could still see the row is running, so the
            // count's snapshot lets VACUUM remove it.
            let mut polls = 0;
            loop {
                let past: bool = admin
                    .query_one(
                        "SELECT pg_snapshot_xmin(pg_current_snapshot())::text::bigint > $1",
                        &[&deleter],
                    )
                    .unwrap()
                    .get(0);
                if past {
                    break;
                }
                polls += 1;
                assert!(
                    polls < 6000,
                    "transactions older than the delete kept running"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let mut locker = connect();
            locker
                .batch_execute(&format!("SELECT pg_advisory_lock({COUNT_RACE_LOCK})"))
                .unwrap();
            let mut counter = connect();
            let pid: i32 = counter
                .query_one("SELECT pg_backend_pid()", &[])
                .unwrap()
                .get(0);
            let settings = if setting.is_empty() {
                String::new()
            } else {
                format!("SET {setting};")
            };
            counter
                .batch_execute(&format!(
                    "SELECT tests.count_race_pause(); SET enable_seqscan = off; {settings}"
                ))
                .unwrap();
            let count = format!("SELECT count(*) FROM count_race WHERE body ==> '{query}'");
            let plan: Vec<String> = counter
                .query(&format!("EXPLAIN {count}"), &[])
                .unwrap()
                .iter()
                .map(|row| row.get(0))
                .collect();
            assert!(
                plan.iter().any(|line| line.contains("Stannum Count")),
                "{query}: {plan:?}"
            );
            let truth = format!("SELECT count(*) FROM count_race WHERE {predicate}");
            let counting = std::thread::spawn(move || {
                counter
                    .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
                    .unwrap();
                // The first statement fixes the snapshot, after the delete.
                let expected: i64 = counter.query_one(&truth, &[]).unwrap().get(0);
                let counted: i64 = counter.query_one(&count, &[]).unwrap().get(0);
                counter.batch_execute("COMMIT").unwrap();
                (expected, counted)
            });
            let mut polls = 0;
            loop {
                let waiting: bool = admin
                    .query_one(
                        "SELECT count(*) > 0 FROM pg_locks
                         WHERE pid = $1 AND locktype = 'advisory' AND NOT granted",
                        &[&pid],
                    )
                    .unwrap()
                    .get(0);
                if waiting {
                    break;
                }
                polls += 1;
                assert!(
                    polls < 3000 && !counting.is_finished(),
                    "the count did not reach count:view"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            // VACUUM skips a page it cannot lock for cleanup; retry until the
            // row's page is all-visible.
            let mut vacuums = 0;
            loop {
                admin
                    .batch_execute("VACUUM (INDEX_CLEANUP ON) count_race")
                    .unwrap();
                let visible: bool = admin
                    .query_one(
                        "SELECT tests.count_race_all_visible('count_race'::regclass, $1)",
                        &[&block],
                    )
                    .unwrap()
                    .get(0);
                if visible {
                    break;
                }
                vacuums += 1;
                assert!(
                    vacuums < 300,
                    "VACUUM never marked block {block} all-visible"
                );
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            locker
                .batch_execute(&format!("SELECT pg_advisory_unlock({COUNT_RACE_LOCK})"))
                .unwrap();
            let (expected, counted) = counting.join().unwrap();
            if expected != counted {
                wrong.push(format!(
                    "row {id}, {query} ({setting}): counted {counted}, expected {expected}"
                ));
            }
        }
        drop(admin);
        server
            .batch_execute("DROP DATABASE count_race WITH (FORCE)")
            .unwrap();
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }
}
