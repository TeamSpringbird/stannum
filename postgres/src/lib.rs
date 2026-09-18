use pgrx::pg_guard;

::pgrx::pg_module_magic!(name);

mod am;
mod bm25;
mod highlight;
mod highlight_udfs;
mod match_positions;
mod operator;
pub(crate) mod options;
mod postings;
mod score;
mod tf_bucket {
    pub(crate) use segment::tf_bucket::*;
}
mod udfs;

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    options::init();
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
        Spi::run("CREATE INDEX lite_search_idx ON lite_search USING tin (body)").unwrap();
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
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
          CREATE INDEX posting_probe_idx ON posting_probe USING tin(body);
          SET LOCAL enable_seqscan=off;",
        )
        .unwrap();
        assert!(
            Spi::get_one::<i64>("SELECT pg_relation_size('posting_probe_idx')")
                .unwrap()
                .unwrap()
                > 129 * 8192
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
             CREATE INDEX boolean_docs_search ON boolean_docs USING tin(body);",
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
        assert_eq!(
            phrase[0]["Plan"]["Rows Removed by Index Recheck"].as_f64(),
            Some(1.0)
        );
        assert_eq!(phrase[0]["Plan"]["Lossy Heap Blocks"], 0);
        let fallback = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM boolean_docs WHERE body ==> 'missing OR win*'",
        ).unwrap().unwrap().0;
        assert!(fallback[0]["Plan"]["Lossy Heap Blocks"].as_u64().unwrap() > 0);

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
             CREATE INDEX lossy_docs_search ON lossy_docs USING tin(body);
             SET LOCAL work_mem='64kB'; SET LOCAL enable_seqscan=off;",
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
            assert!(
                plan[0]["Plan"]["Lossy Heap Blocks"].as_u64().unwrap() > 0,
                "{query}"
            );
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
    fn posting_inserts_rolled_back_by_subtransaction_are_not_visible() {
        Spi::run(
            "CREATE TABLE posting_abort(body text);
          CREATE INDEX posting_abort_idx ON posting_abort USING tin(body);
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
    fn unlogged_indexes_use_the_reference_path() {
        Spi::run(
            "CREATE UNLOGGED TABLE posting_unlogged(body text);
          INSERT INTO posting_unlogged VALUES ('beer');
          CREATE INDEX posting_unlogged_idx ON posting_unlogged USING tin(body);
          SET LOCAL enable_seqscan=off;",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT pg_relation_size('posting_unlogged_idx')").unwrap(),
            Some(0)
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
             CREATE INDEX lite_growth_idx ON lite_growth USING tin (body);
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
               USING tin (lower(body)) WHERE active;
             SET LOCAL enable_seqscan = off;",
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
             CREATE INDEX lite_union_title_idx ON lite_union USING tin (title);
             CREATE INDEX lite_union_body_idx ON lite_union USING tin (body);
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
             CREATE INDEX lite_mvcc_idx ON lite_mvcc USING tin (body);
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
             CREATE INDEX lite_score_idx ON lite_score USING tin (body);",
        )
        .unwrap();
        let ids = Spi::get_one::<Vec<i32>>(
            "SELECT array_agg(id ORDER BY tin.full_score(ctid) DESC, id)
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
             CREATE INDEX lite_score_helpers_idx ON lite_score_helpers USING tin (body)",
        )
        .unwrap();
        let full_max = Spi::get_one::<f32>(
            "SELECT max(tin.full_score(ctid))
             FROM lite_score_helpers WHERE body ==> 'rare^1.0'",
        )
        .unwrap()
        .unwrap();
        let reported = Spi::get_one::<f32>(
            "SELECT tin.max_score(ctid)
             FROM lite_score_helpers WHERE body ==> 'rare^1.0' LIMIT 1",
        )
        .unwrap()
        .unwrap();
        assert_eq!(reported, full_max);
        let inspected = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(term ORDER BY term)
             FROM tin.score_inspect('lite_score_helpers_idx', 'common OR rare', 0.5)",
        )
        .unwrap();
        assert_eq!(inspected, Some(vec!["rare".to_owned()]));
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
               USING tin (((s1 || ' '::text) || s2));",
        )
        .unwrap();
        let rows = Spi::connect(|client| {
            client
                .select(
                    "SELECT id, tin.score(ctid) AS score
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
    fn highlighting_supports_explicit_and_implicit_queries() {
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT tin.highlight('Beer and wine', '[', ']', query => 'beer')"
            )
            .unwrap(),
            Some("[Beer] and wine".into())
        );
        Spi::run(
            "CREATE TABLE lite_highlight (id int, s1 text, s2 text);
             INSERT INTO lite_highlight VALUES
               (1, 'Beer', 'and wine'), (2, 'cider', 'only');
             CREATE INDEX lite_highlight_idx ON lite_highlight
               USING tin (((s1 || ' '::text) || s2));",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT tin.highlight(s1 || ' ' || s2)
                 FROM lite_highlight
                 WHERE (s1 || ' ' || s2) ==> 'beer'"
            )
            .unwrap(),
            Some("<b>Beer</b> and wine".into())
        );
        let ansi = Spi::get_one::<String>(
            "SELECT tin.highlight_ansi(s1 || ' ' || s2)
             FROM lite_highlight
             WHERE (s1 || ' ' || s2) ==> 'beer'",
        )
        .unwrap()
        .unwrap();
        assert!(ansi.contains("\x1b["));
        assert!(ansi.contains("Beer"));
    }
}
