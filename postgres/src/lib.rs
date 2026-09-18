use pgrx::pg_guard;

::pgrx::pg_module_magic!(name);

mod am;
mod bm25;
mod customscan;
mod highlight;
mod highlight_udfs;
mod match_positions;
mod operator;
pub(crate) mod options;
mod score;
mod selectivity;
mod storage;
mod tf_bucket {
    pub(crate) use segment::tf_bucket::*;
}
mod udfs;

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    options::init();
    storage::init();
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

    /// Rows of a ranked query as `(id, score bits)` so scores compare exactly.
    fn ranked(custom: bool, query: &str, order_by: &str, limit: &str) -> Vec<(i32, u32)> {
        Spi::run(&format!(
            "SET LOCAL stannum.enable_custom_scan = {custom}; SET LOCAL enable_seqscan = off;"
        ))
        .unwrap();
        Spi::connect(|client| {
            client
                .select(
                    &format!(
                        "SELECT id, {order_by} AS score FROM bmw WHERE body ==> '{query}'
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
            heap, index, "needle", true, None, None, None, None, None,
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
            "alpha OR missing",
            "alpha^2 OR beta",
            "(alpha AND beta)^0.5",
            "alpha OR alpha",
            "pad OR alpha",
            // Shapes the pruned path leaves to full scoring.
            "\"alpha beta\"",
            "alpha AND NOT beta",
            "al*",
            "AT LEAST 2 OF [alpha beta gamma]",
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
        assert_eq!(scan["Pruning"], "block-max");
        // The ten best rows share the best score and are the earliest such
        // rows, so once they are found every later block is skipped.
        let scored = scan["Scored Candidates"].as_i64().unwrap();
        assert!(scored > 0 && scored < 1000, "{scan}");
        // ...and the conjunction and disjunction too.
        for query in ["alpha AND beta", "alpha OR gamma"] {
            let scan = explain(query);
            assert_eq!(scan["Pruning"], "block-max", "{query}");
            assert!(scan["Scored Candidates"].as_i64().unwrap() < 1500, "{scan}");
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
        assert_eq!(scan["Pruning"], "block-max");
        assert!(scan["Candidates"].as_i64().unwrap() > 3, "{scan}");
        assert_eq!(
            ranked(true, "delta", "stannum.full_score(ctid)", "LIMIT 3"),
            ranked(false, "delta", "stannum.full_score(ctid)", "LIMIT 3")
        );
        // A phrase query is not pruned and reports its candidates as before.
        Spi::run("SET LOCAL stannum.enable_custom_scan = on;").unwrap();
        let scan = explain("\"alpha beta\"");
        assert!(scan["Pruning"].is_null());
        assert!(scan["Candidates"].as_i64().unwrap() > 0);
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
            b"LSG2"
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

    #[pg_test]
    fn insert_defers_large_merges_and_cleanup_finishes_them() {
        Spi::run(
            "CREATE TABLE merge_budget(body text);
             CREATE INDEX merge_budget_idx ON merge_budget USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 1;
             SET LOCAL stannum.merge_tier_factor = 2;
             SET LOCAL stannum.max_merge_docs = 4;
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
             INSERT INTO merge_full SELECT 'needle' FROM generate_series(1,130);",
        )
        .unwrap();
        assert_eq!(
            value(
                "SELECT count(*) FROM stannum.segment_info('merge_full_idx') WHERE kind = 'immutable'"
            ),
            128
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
            128
        );
        Spi::run("SET LOCAL stannum.max_segments = 3; INSERT INTO merge_full VALUES ('needle')")
            .unwrap();
        assert_eq!(
            value(
                "SELECT count(*) FROM stannum.segment_info('merge_full_idx') WHERE kind = 'immutable'"
            ),
            3
        );
        assert_eq!(
            value("SELECT sum(docs)::bigint FROM stannum.segment_info('merge_full_idx')"),
            131
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
            131
        );
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
}
