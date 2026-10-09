// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

//! Which `==>` clauses bind scoring and highlighting, and how several
//! combine: parameters, clauses on other relations or under `NOT`, search
//! texts parsed one by one, search text read from a joined relation,
//! partial indexes, and rows that no search admits. Several tests adapt
//! PlanetScale Lead's (`postgres/src/lib.rs` at e3ed2f4).

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    /// The float4 bits of an `array_agg` of scores, NULLs kept.
    fn score_bits(sql: &str) -> Vec<Option<u32>> {
        Spi::get_one::<Vec<Option<f32>>>(sql)
            .unwrap()
            .unwrap_or_default()
            .into_iter()
            .map(|score| score.map(f32::to_bits))
            .collect()
    }

    /// `SQLSTATE | message | detail` of the error `statement` raises, or
    /// NULL when it succeeds.
    fn error_of(statement: &str) -> Option<String> {
        Spi::run(
            "CREATE OR REPLACE FUNCTION pg_temp.error_of(statement text) RETURNS text
             LANGUAGE plpgsql AS $f$
             DECLARE state text; message text; detail text;
             BEGIN
               EXECUTE statement;
               RETURN NULL;
             EXCEPTION WHEN OTHERS THEN
               GET STACKED DIAGNOSTICS state = RETURNED_SQLSTATE, message = MESSAGE_TEXT,
                 detail = PG_EXCEPTION_DETAIL;
               RETURN state || ' | ' || message || ' | ' || coalesce(detail, '');
             END $f$",
        )
        .unwrap();
        Spi::get_one_with_args::<String>("SELECT pg_temp.error_of($1)", &[statement.into()])
            .unwrap()
    }

    #[pg_test]
    fn parameterized_searches_score_like_literals() {
        Spi::run(
            "CREATE TABLE bind_param (id int, title text);
             INSERT INTO bind_param VALUES (1, 'lorem ipsum'), (2, 'lorem ipsun'),
               (3, 'lorem dolor ipsum ipsum'), (4, 'ipsum sit');
             INSERT INTO bind_param SELECT g, 'filler word ' || g FROM generate_series(5, 30) g;
             CREATE INDEX ON bind_param USING stannum (title);",
        )
        .unwrap();
        for function in ["score", "full_score", "max_score"] {
            let literal = score_bits(&format!(
                "SELECT array_agg(stannum.{function}(ctid) ORDER BY id) FROM bind_param
                 WHERE title ==> 'lorem^4' AND title ==> '(ipsum^4 OR ipsum~1^1.4 OR ipsum*^2)'"
            ));
            assert_eq!(literal.len(), 3, "{function}: {literal:?}");
            for mode in ["force_custom_plan", "force_generic_plan"] {
                Spi::run(&format!(
                    "SET LOCAL plan_cache_mode = {mode};
                     PREPARE bind_param_{function}(text, text) AS
                       SELECT array_agg(stannum.{function}(ctid) ORDER BY id) FROM bind_param
                       WHERE title ==> $1 AND title ==> $2"
                ))
                .unwrap();
                let prepared = score_bits(&format!(
                    "EXECUTE bind_param_{function}('lorem^4', '(ipsum^4 OR ipsum~1^1.4 OR ipsum*^2)')"
                ));
                Spi::run(&format!("DEALLOCATE bind_param_{function}")).unwrap();
                assert_eq!(prepared, literal, "{function} under {mode}");
            }
        }
    }

    #[pg_test]
    fn parameterized_searches_highlight_every_term() {
        Spi::run(
            "CREATE TABLE bind_param_marks (id int, body text);
             INSERT INTO bind_param_marks VALUES (1, 'alpha beta gamma');
             CREATE INDEX ON bind_param_marks USING stannum (body);",
        )
        .unwrap();
        for mode in ["force_custom_plan", "force_generic_plan"] {
            Spi::run(&format!(
                "SET LOCAL plan_cache_mode = {mode};
                 PREPARE bind_param_marks(text, text) AS
                   SELECT stannum.highlight(body) FROM bind_param_marks
                   WHERE body ==> $1 AND body ==> $2"
            ))
            .unwrap();
            let marked =
                Spi::get_one::<String>("EXECUTE bind_param_marks('alpha', 'gamma')").unwrap();
            Spi::run("DEALLOCATE bind_param_marks").unwrap();
            assert_eq!(
                marked.as_deref(),
                Some("<b>alpha</b> beta <b>gamma</b>"),
                "{mode}"
            );
        }
    }

    #[pg_test]
    fn searches_on_other_relations_do_not_bind_to_the_scored_relation() {
        Spi::run(
            "CREATE TABLE bind_xa (id int, title text);
             INSERT INTO bind_xa VALUES (1, 'shared alpha');
             INSERT INTO bind_xa SELECT g, 'filler word ' || g FROM generate_series(2, 30) g;
             CREATE INDEX ON bind_xa USING stannum (title);
             CREATE TABLE bind_xb (id int, title text);
             INSERT INTO bind_xb VALUES (1, 'beta shared alpha'), (2, 'beta gamma'), (3, 'beta');
             INSERT INTO bind_xb SELECT g, 'padding text ' || g FROM generate_series(4, 30) g;
             CREATE INDEX ON bind_xb USING stannum (title);",
        )
        .unwrap();
        for function in ["score", "full_score", "max_score"] {
            let alone = score_bits(&format!(
                "SELECT array_agg(stannum.{function}(ctid) ORDER BY id) FROM bind_xb
                 WHERE title ==> 'beta'"
            ));
            assert_eq!(alone.len(), 3, "{function}: {alone:?}");
            // `a` is the first range-table entry, as the scored relation's
            // varno is normalized to.
            let joined = score_bits(&format!(
                "SELECT array_agg(stannum.{function}(b.ctid) ORDER BY b.id)
                 FROM bind_xa a, bind_xb b
                 WHERE a.title ==> 'shared alpha' AND b.title ==> 'beta'"
            ));
            assert_eq!(joined, alone, "{function}");
        }
    }

    fn create_negated_table() {
        Spi::run(
            "CREATE TABLE bind_negated (id int, body text);
             INSERT INTO bind_negated VALUES (1, 'zeta omega beta'), (2, 'zeta alpha'),
               (3, 'omega gamma zeta'), (4, 'omega omega gamma');
             INSERT INTO bind_negated SELECT g, 'filler word ' || g FROM generate_series(5, 30) g;
             CREATE INDEX ON bind_negated USING stannum (body);",
        )
        .unwrap();
    }

    #[pg_test]
    fn negated_searches_do_not_contribute_to_scores() {
        create_negated_table();
        for function in ["score", "full_score", "max_score"] {
            let baseline = score_bits(&format!(
                "SELECT array_agg(stannum.{function}(ctid) ORDER BY id) FROM bind_negated
                 WHERE body ==> 'zeta' AND id IN (1, 2)"
            ));
            assert_eq!(baseline.len(), 2, "{function}: {baseline:?}");
            let negated = score_bits(&format!(
                "SELECT array_agg(stannum.{function}(ctid) ORDER BY id) FROM bind_negated
                 WHERE body ==> 'zeta' AND NOT (body ==> 'omega gamma')"
            ));
            assert_eq!(negated, baseline, "{function}");
        }
    }

    #[pg_test]
    fn negated_searches_do_not_mark_highlights() {
        create_negated_table();
        let marked = Spi::get_one::<String>(
            "SELECT stannum.highlight(body) FROM bind_negated
             WHERE body ==> 'zeta' AND NOT (body ==> 'omega gamma') AND id = 1",
        )
        .unwrap();
        assert_eq!(marked.as_deref(), Some("<b>zeta</b> omega beta"));
        // A NOT anywhere, here under OR, hides the searches below it.
        let marked = Spi::get_one::<String>(
            "SELECT stannum.highlight(body) FROM bind_negated
             WHERE body ==> 'zeta' AND (id = 1 OR NOT (body ==> 'beta'))
             ORDER BY id LIMIT 1",
        )
        .unwrap();
        assert_eq!(marked.as_deref(), Some("<b>zeta</b> omega beta"));
    }

    fn create_ored_table() {
        Spi::run(
            "CREATE TABLE bind_ored (id int, body text);
             INSERT INTO bind_ored VALUES
               (1, 'beer wine'), (2, 'beer beer ale'), (3, 'wine cellar'),
               (4, 'craft beer bar'), (5, 'water');
             CREATE INDEX ON bind_ored USING stannum (body);",
        )
        .unwrap();
    }

    #[pg_test]
    fn several_searches_score_like_one_ored_search() {
        create_ored_table();
        for (separate, ored) in [
            (
                "body ==> 'beer' OR body ==> 'wine'",
                "body ==> '(beer) OR (wine)'",
            ),
            (
                "body ==> 'beer^2 OR ale' OR body ==> '\"craft beer\"'
                   OR body ==> 'wine~1 cellar'",
                "body ==> '(beer^2 OR ale) OR (\"craft beer\") OR (wine~1 cellar)'",
            ),
            // An empty search matches nothing and leaves the others to score.
            ("body ==> 'beer' OR body ==> ''", "body ==> 'beer'"),
        ] {
            for function in ["score", "full_score", "max_score"] {
                let scores = |quals: &str| {
                    score_bits(&format!(
                        "SELECT array_agg(stannum.{function}(ctid) ORDER BY id) FROM bind_ored
                         WHERE {quals}"
                    ))
                };
                let (separate, ored) = (scores(separate), scores(ored));
                if function == "full_score" {
                    assert!(
                        separate
                            .iter()
                            .all(|score| score.is_some_and(|bits| f32::from_bits(bits) > 0.0)),
                        "{separate:?}"
                    );
                }
                assert_eq!(separate, ored, "{function}");
            }
        }
    }

    #[pg_test]
    fn several_searches_highlight_each_text() {
        create_ored_table();
        let marked = Spi::get_one::<String>(
            "SELECT stannum.highlight(body) FROM bind_ored
             WHERE (body ==> 'beer' OR body ==> '') AND id = 4",
        )
        .unwrap();
        assert_eq!(marked.as_deref(), Some("craft <b>beer</b> bar"));
    }

    #[pg_test]
    fn malformed_searches_fail_like_the_operator() {
        Spi::run(
            "CREATE TABLE bind_malformed (id integer, body text);
             INSERT INTO bind_malformed VALUES (1, 'beer a b'), (2, 'beer foo or bar');
             CREATE INDEX ON bind_malformed USING stannum (body);
             SET LOCAL enable_indexscan = off;
             SET LOCAL enable_bitmapscan = off;
             SET LOCAL stannum.enable_custom_scan = off;",
        )
        .unwrap();
        // Every row matches 'beer', so the malformed searches after it never
        // run. Each text is invalid on its own but valid once wrapped in
        // parentheses and joined with the others.
        for searches in [
            &["a) OR (b"][..],
            &["\"foo\\", "bar\""],
            &["beer AND (wine", "ale)"],
        ] {
            let operator = error_of(&format!("SELECT 'beer' ==> '{}'", searches[0]))
                .expect("the operator rejects the text");
            assert!(operator.contains("invalid ==> query"), "{operator}");
            let quals = searches
                .iter()
                .map(|search| format!(" OR body ==> '{search}'"))
                .collect::<String>();
            for function in [
                "stannum.score(ctid)",
                "stannum.full_score(ctid)",
                "stannum.max_score(ctid)",
                "stannum.highlight(body)",
                "stannum.highlight_ansi(body)",
            ] {
                let sql =
                    format!("SELECT {function} FROM bind_malformed WHERE body ==> 'beer'{quals}");
                assert_eq!(error_of(&sql).as_deref(), Some(operator.as_str()), "{sql}");
            }
        }
    }

    #[pg_test]
    fn search_text_from_a_joined_relation_scores_per_row() {
        Spi::run(
            "CREATE TABLE bind_join_docs (id int, body text);
             INSERT INTO bind_join_docs VALUES
               (1, 'red sofa'), (2, 'blue sofa sofa'), (3, 'oak table'),
               (4, 'pine table'), (5, 'glass table'), (6, 'green chair');
             CREATE INDEX ON bind_join_docs USING stannum (body);
             CREATE TABLE bind_join_queries (query text);
             INSERT INTO bind_join_queries VALUES ('sofa'), ('red'), ('red OR table');",
        )
        .unwrap();
        let joined = Spi::connect(|client| {
            client
                .select(
                    "SELECT q.query, d.id, stannum.full_score(d.ctid)
                     FROM bind_join_queries q JOIN bind_join_docs d ON d.body ==> q.query
                     ORDER BY 1, 2",
                    None,
                    &[],
                )
                .unwrap()
                .map(|row| {
                    (
                        row.get::<String>(1).unwrap().unwrap(),
                        row.get::<i32>(2).unwrap().unwrap(),
                        row.get::<f32>(3).unwrap().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(joined.len(), 7, "{joined:?}");
        for (query, id, score) in &joined {
            let literal = Spi::get_one::<f32>(&format!(
                "SELECT stannum.full_score(ctid) FROM bind_join_docs
                 WHERE body ==> '{query}' AND id = {id}"
            ))
            .unwrap();
            assert_eq!(literal, Some(*score), "{query} / {id}");
        }
        let score = |search: &str, id| {
            joined
                .iter()
                .find(|(query, row, _)| query == search && *row == id)
                .unwrap()
                .2
        };
        assert!(score("red", 1) > 0.0);
        assert_ne!(score("red", 1), score("sofa", 1));
    }

    #[pg_test]
    fn unproved_partial_indexes_refuse_scoring() {
        Spi::run(
            "CREATE TABLE bind_unproved (id integer, body text, active boolean);
             INSERT INTO bind_unproved VALUES (1, 'beer', true), (2, 'beer', false);
             CREATE INDEX ON bind_unproved USING stannum (body) WHERE active;
             CREATE INDEX ON bind_unproved USING stannum (lower(body)) WHERE active;",
        )
        .unwrap();
        for (sql, expression) in [
            (
                "SELECT stannum.score(ctid) FROM bind_unproved WHERE body ==> 'beer'",
                "bind_unproved.body",
            ),
            (
                "SELECT stannum.full_score(t.ctid) FROM bind_unproved t
                 WHERE lower(t.body) ==> 'beer' AND lower(t.body) ==> 'wine'",
                "lower(bind_unproved.body)",
            ),
            (
                "SELECT stannum.max_score(ctid) FROM bind_unproved
                 WHERE body ==> 'beer' AND active IS NOT NULL",
                "bind_unproved.body",
            ),
        ] {
            assert_eq!(
                error_of(sql),
                Some(format!(
                    "0A000 | cannot compute scores for this query | \
                     No matching stannum index for: {expression}."
                )),
                "{sql}"
            );
        }
    }

    #[pg_test]
    fn implied_partial_indexes_score_their_rows() {
        Spi::run(
            "CREATE TABLE bind_implied (id int, body text, active boolean);
             INSERT INTO bind_implied VALUES
               (1, 'beer wine', true), (2, 'beer', true), (3, 'wine', true);
             INSERT INTO bind_implied SELECT n, 'beer', false FROM generate_series(4, 40) AS n;
             CREATE INDEX ON bind_implied USING stannum (body) WHERE active;
             CREATE TABLE bind_implied_control AS SELECT id, body FROM bind_implied WHERE active;
             CREATE INDEX ON bind_implied_control USING stannum (body);",
        )
        .unwrap();
        let control = score_bits(
            "SELECT array_agg(stannum.full_score(ctid) + stannum.max_score(ctid) ORDER BY id)
             FROM bind_implied_control WHERE body ==> 'beer'",
        );
        assert_eq!(control.len(), 2);
        for quals in [
            "active AND body ==> 'beer'",
            "active = true AND body ==> 'beer'",
        ] {
            let partial = score_bits(&format!(
                "SELECT array_agg(stannum.full_score(ctid) + stannum.max_score(ctid) ORDER BY id)
                 FROM bind_implied WHERE {quals}"
            ));
            assert_eq!(partial, control, "{quals}");
        }
    }

    #[pg_test]
    fn rows_no_search_admits_have_no_score() {
        Spi::run(
            "CREATE TABLE bind_unsearched (id int, body text);
             INSERT INTO bind_unsearched VALUES (1, 'gems ruby'), (2, 'stone'), (3, 'gems'),
               (4, 'pebble'), (5, 'ruby gems gems');
             INSERT INTO bind_unsearched SELECT g, 'filler ' || g FROM generate_series(6, 30) g;
             CREATE INDEX ON bind_unsearched USING stannum (body);",
        )
        .unwrap();
        for function in ["score", "full_score"] {
            let searched = score_bits(&format!(
                "SELECT array_agg(stannum.{function}(ctid) ORDER BY id) FROM bind_unsearched
                 WHERE body ==> 'gems'"
            ));
            assert_eq!(searched.len(), 3, "{searched:?}");
            assert!(searched.iter().all(Option::is_some), "{searched:?}");
            let either = score_bits(&format!(
                "SELECT array_agg(stannum.{function}(ctid) ORDER BY id) FROM bind_unsearched
                 WHERE body ==> 'gems' OR id = 4"
            ));
            assert_eq!(
                either,
                vec![searched[0], searched[1], None, searched[2]],
                "{function}"
            );
            // A row a search admits keeps its score whatever else admits it.
            let both = score_bits(&format!(
                "SELECT array_agg(stannum.{function}(ctid) ORDER BY id) FROM bind_unsearched
                 WHERE body ==> 'gems' OR id = 3"
            ));
            assert_eq!(both, searched, "{function}");
        }
    }
}
