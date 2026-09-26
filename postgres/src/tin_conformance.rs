// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Stannum against TIN 1.0.3's recorded answers.
//!
//! Every expected value in this module is an answer of PlanetScale TIN, not
//! a value Stannum produced:
//!
//! - TIN extension version: 1.0.3
//! - Server: PostgreSQL 18.6 (Debian 18.6-1.pgdg12+2, aarch64), PlanetScale
//!   Postgres, us-east-1
//! - Recorded: 2026-09-26
//!
//! The span, BM25 and query-size tests read the conformance suite's
//! recorded answers, `conformance/expected/tin-1.0.3/{spans,bm25,query_size}.json`
//! (each file's `source` header says how it was measured), and run the
//! inputs of the cases with the same ids in `conformance/cases/`. They check
//! that each file records TIN 1.0.3. The other tests assert TIN 1.0.3's
//! answers from the catalog cases they cite. `conformance/run.py --check`
//! runs the whole suite against an installed build; these tests run a part
//! of it in every `cargo pgrx test`.
//!
//! See `docs/tin-behavior.md` for the decisions taken from TIN's answers. To
//! follow a new TIN release, record it into a new
//! `conformance/expected/tin-<version>/` and point the constants below at
//! it; never edit the recorded values of an existing directory.

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use serde_json::Value;

    const TIN_VERSION: &str = "1.0.3";
    const SPANS: &str = include_str!("../../conformance/expected/tin-1.0.3/spans.json");
    const BM25: &str = include_str!("../../conformance/expected/tin-1.0.3/bm25.json");
    const QUERY_SIZE: &str = include_str!("../../conformance/expected/tin-1.0.3/query_size.json");

    /// The `spans4` corpus of `conformance/cases/spans.yaml`.
    const SPAN_DOCUMENTS: [(i32, &str); 4] = [
        (1, "alpha x beta gamma"),
        (2, "alpha beta gamma"),
        (3, "beta gamma alpha"),
        (4, "alpha x y beta gamma"),
    ];

    /// The queries of `conformance/cases/spans.yaml`, by case id.
    const SPAN_QUERIES: [(&str, &str); 6] = [
        ("span.then_phrase_operand.1", r#"alpha THEN/1 "beta gamma""#),
        ("span.then_phrase_operand.2", r#"alpha THEN/2 "beta gamma""#),
        (
            "span.then_phrase_operand.3",
            r#""alpha x" THEN/2 "beta gamma""#,
        ),
        ("span.then_phrase_operand.4", r#""beta gamma" THEN/1 alpha"#),
        ("span.then_terms.1", "alpha THEN/1 beta"),
        ("span.near_phrase_operand.1", r#"alpha NEAR/1 "beta gamma""#),
    ];

    /// The recorded cases of one area file, after checking that the file
    /// holds TIN's answers for the version these tests name.
    fn recorded(file: &str) -> Vec<Value> {
        let answers: Value = serde_json::from_str(file).expect("recorded TIN answers parse");
        assert_eq!(
            answers["source"]["engine"], "tin",
            "answers recorded from TIN"
        );
        assert_eq!(
            answers["source"]["extension_version"], TIN_VERSION,
            "the tests name the TIN version they were recorded from"
        );
        answers["cases"].as_array().unwrap().clone()
    }

    fn id(case: &Value) -> &str {
        case["id"].as_str().unwrap()
    }

    fn literal(text: &str) -> String {
        text.replace('\'', "''")
    }

    fn column<T: pgrx::IntoDatum + pgrx::FromDatum>(sql: &str) -> Vec<T> {
        Spi::connect(|client| {
            client
                .select(sql, None, &[])
                .unwrap()
                .map(|row| row.get::<T>(1).unwrap().unwrap())
                .collect()
        })
    }

    fn numbers(value: &Value) -> Vec<i32> {
        serde_json::from_value(value.clone()).expect("recorded ids")
    }

    /// TIN 1.0.3: spans whose operands are phrases, and the plain spans
    /// beside them. Matches and counts must equal TIN's with the custom scan
    /// on and off; the ranked top ten must equal TIN's order under
    /// `full_score`.
    #[pg_test]
    fn tin_1_0_3_spans_with_phrase_operands() {
        let cases = recorded(SPANS);
        assert_eq!(
            cases.len(),
            SPAN_QUERIES.len(),
            "every span case is recorded"
        );
        Spi::run("CREATE TABLE tin_spans(id int PRIMARY KEY, body text)").unwrap();
        for (id, body) in SPAN_DOCUMENTS {
            Spi::run(&format!(
                "INSERT INTO tin_spans VALUES ({id}, '{}')",
                literal(body)
            ))
            .unwrap();
        }
        Spi::run("CREATE INDEX tin_spans_idx ON tin_spans USING stannum(body); SET LOCAL enable_seqscan = off")
            .unwrap();
        let mut differences = Vec::new();
        for case in &cases {
            let (_, query) = SPAN_QUERIES
                .iter()
                .find(|(known, _)| *known == id(case))
                .unwrap_or_else(|| panic!("recorded case {} has no query here", id(case)));
            let quoted = literal(query);
            let captures = &case["captures"];
            let ids = numbers(&captures["ids"]);
            let want_count = captures["count"].as_i64().unwrap();
            let ranked = numbers(&captures["ranked"]);
            for custom in ["on", "off"] {
                Spi::run(&format!("SET LOCAL stannum.enable_custom_scan = {custom}")).unwrap();
                let got: Vec<i32> = column(&format!(
                    "SELECT id FROM tin_spans WHERE body ==> '{quoted}' ORDER BY id"
                ));
                if got != ids {
                    differences.push(format!(
                        "{query} (custom scan {custom}): matched {got:?}, TIN {ids:?}"
                    ));
                }
                let count = Spi::get_one::<i64>(&format!(
                    "SELECT count(*) FROM tin_spans WHERE body ==> '{quoted}'"
                ))
                .unwrap()
                .unwrap();
                if count != want_count {
                    differences.push(format!(
                        "{query} (custom scan {custom}): count {count}, TIN {want_count}"
                    ));
                }
            }
            Spi::run("SET LOCAL stannum.enable_custom_scan = on").unwrap();
            let got: Vec<i32> = column(&format!(
                "SELECT id FROM (SELECT id, stannum.full_score(ctid) s FROM tin_spans
                 WHERE body ==> '{quoted}' ORDER BY s DESC LIMIT 10) top ORDER BY s DESC, id"
            ));
            if got != ranked {
                differences.push(format!("{query}: ranked {got:?}, TIN {ranked:?}"));
            }
        }
        assert!(
            differences.is_empty(),
            "Stannum differs from TIN {TIN_VERSION}:\n{}",
            differences.join("\n")
        );
    }

    /// TIN 1.0.3: BM25 with a chosen k1 and b = 0.75 on the `w10` corpus of
    /// `conformance/cases/bm25.yaml`. Every score must be bit-identical to
    /// TIN's, and the pruned `LIMIT 1` must return TIN's row, which is the
    /// exhaustive top of TIN's scores; at k1 = 0 the scores of higher term
    /// frequencies round one ulp lower.
    #[pg_test]
    fn tin_1_0_3_bm25_k1_scores_and_pruned_top() {
        let cases = recorded(BM25);
        assert_eq!(cases.len(), 4, "every k1 case is recorded");
        Spi::run(
            "CREATE TABLE tin_w(id int PRIMARY KEY, body text);
             INSERT INTO tin_w VALUES (1, 'w w w');
             INSERT INTO tin_w SELECT n, 'w' FROM generate_series(2, 10) n;
             CREATE INDEX tin_w_idx ON tin_w USING stannum(body);
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        let mut differences = Vec::new();
        for case in &cases {
            // `bm25.k1.0_001` is k1 = 0.001.
            let k1 = id(case)
                .strip_prefix("bm25.k1.")
                .unwrap_or_else(|| panic!("unknown recorded case {}", id(case)))
                .replace('_', ".");
            let scores = case["captures"]["scores"].as_object().unwrap();
            let bits: Vec<String> = column(&format!(
                "SELECT encode(float4send(stannum.full_score(ctid, {k1}::real, 0.75::real)), 'hex')
                 FROM tin_w WHERE body ==> 'w' ORDER BY id"
            ));
            assert_eq!(bits.len(), scores.len(), "k1 {k1}: every row is scored");
            for (id, got) in (1..).zip(&bits) {
                let want = scores[&id.to_string()].as_str().unwrap();
                if got != want {
                    differences.push(format!("k1 {k1}, id {id}: score bits {got}, TIN {want}"));
                }
            }
            let top = Spi::get_one::<i32>(&format!(
                "SELECT id FROM tin_w WHERE body ==> 'w'
                 ORDER BY stannum.full_score(ctid, {k1}::real, 0.75::real) DESC LIMIT 1"
            ))
            .unwrap()
            .unwrap();
            let want = numbers(&case["captures"]["pruned_top_limit_1"])[0];
            // TIN's exhaustive top: the best (score desc, id asc) of its scores.
            let score = |id: i32| {
                f32::from_bits(
                    u32::from_str_radix(scores[&id.to_string()].as_str().unwrap(), 16).unwrap(),
                )
            };
            let exhaustive = (1..=10)
                .reduce(|best, id| if score(id) > score(best) { id } else { best })
                .unwrap();
            assert_eq!(want, exhaustive, "TIN's pruned top is its exhaustive top");
            if top != want {
                differences.push(format!("k1 {k1}: LIMIT 1 returned id {top}, TIN {want}"));
            }
        }
        assert!(
            differences.is_empty(),
            "Stannum differs from TIN {TIN_VERSION}:\n{}",
            differences.join("\n")
        );
    }

    /// TIN 1.0.3: query size and nesting, on the spans corpus, where the term
    /// `a` matches nothing. Stannum must answer every query TIN answered, with
    /// TIN's count; the sizes that crashed TIN's server must get an ERROR or
    /// a correct count from Stannum, never a crash (a crash ends this test's
    /// backend, which fails the test).
    #[pg_test]
    fn tin_1_0_3_query_size_and_nesting() {
        let cases = recorded(QUERY_SIZE);
        Spi::run("CREATE TABLE tin_size(id int PRIMARY KEY, body text)").unwrap();
        for (id, body) in SPAN_DOCUMENTS {
            Spi::run(&format!(
                "INSERT INTO tin_size VALUES ({id}, '{}')",
                literal(body)
            ))
            .unwrap();
        }
        Spi::run(
            "CREATE INDEX tin_size_idx ON tin_size USING stannum(body);
             CREATE TEMP TABLE tin_size_outcome(label text, ok bool, n bigint, state text);",
        )
        .unwrap();
        // `query_size.<shape>.<n>`, as in conformance/cases/query_size.yaml.
        let expression_of = |label: &str| -> String {
            let (shape, n) = label
                .strip_prefix("query_size.")
                .and_then(|rest| rest.split_once('.'))
                .unwrap_or_else(|| panic!("unknown recorded case {label}"));
            let n: u32 = n.parse().unwrap();
            match shape {
                "words" => format!("repeat('a ', {n})"),
                "or_chain" => format!("array_to_string(array_fill('a'::text, ARRAY[{n}]), ' OR ')"),
                "nested" => format!("repeat('(', {n}) || 'a' || repeat(')', {n})"),
                other => panic!("unknown recorded shape {other}"),
            }
        };
        // The expression is parenthesized: `||` and `==>` share PostgreSQL's
        // precedence for other operators, left to right, so without it
        // `body ==> repeat('(', n) || 'a'` applies `==>` to the first term.
        let run = |label: &str| {
            let expression = expression_of(label);
            Spi::run(&format!(
                "DO $probe$ DECLARE n bigint; BEGIN
                   SELECT count(*) INTO n FROM tin_size WHERE body ==> ({expression});
                   INSERT INTO tin_size_outcome VALUES ('{label}', true, n, NULL);
                 EXCEPTION WHEN OTHERS THEN
                   INSERT INTO tin_size_outcome VALUES ('{label}', false, NULL, SQLSTATE);
                 END $probe$"
            ))
            .unwrap();
            Spi::get_three::<bool, i64, String>(&format!(
                "SELECT ok, n, state FROM tin_size_outcome WHERE label = '{label}'"
            ))
            .unwrap()
        };
        let mut differences = Vec::new();
        let mut crashed = 0;
        for case in &cases {
            let label = id(case);
            let (ok, n, state) = run(label);
            if case["server_crashed"] == true {
                crashed += 1;
                if ok == Some(true) && n != Some(0) {
                    differences.push(format!(
                        "{label}: TIN crashed; Stannum answered a wrong count {n:?}"
                    ));
                }
            } else {
                let want = case["captures"]["count"].as_i64().unwrap();
                if ok != Some(true) || n != Some(want) {
                    differences.push(format!(
                        "{label}: TIN answered {want}, Stannum ok {ok:?} count {n:?} sqlstate {state:?}"
                    ));
                }
            }
        }
        assert_eq!(
            (cases.len(), crashed),
            (9, 3),
            "six answered and three crashing sizes are recorded"
        );
        assert!(
            differences.is_empty(),
            "Stannum differs from TIN {TIN_VERSION}:\n{}",
            differences.join("\n")
        );
    }

    /// `[[id, "score bits"], ...]` of the rows `sql` returns as (id, score).
    fn score_bits(sql: &str) -> String {
        Spi::get_one::<String>(&format!(
            "SELECT coalesce(json_agg(json_build_array(id, encode(float4send(score), 'hex'))
             ORDER BY id), '[]')::text FROM ({sql}) scored(id, score)"
        ))
        .unwrap()
        .unwrap()
    }

    /// A repeated query term adds its boosts, in `score_inspect` and in the
    /// scores: TIN 1.0.3 weighs `a OR a^2` 3.0, `a a` 2.0 and `(a^2)^3` 6.0
    /// (conformance/expected/tin-1.0.3/catalog.scoring.json, catalog.S-12),
    /// so `a a` scores as `a^2` does.
    #[pg_test]
    fn tin_1_0_3_repeated_terms_add_their_boosts() {
        // A segmented index, then a temporary table's heap-scored one.
        for table_kind in ["", "TEMP"] {
            Spi::run(&format!(
                "CREATE {table_kind} TABLE repeats(id int, body text);
                 INSERT INTO repeats VALUES (1, 'a b');
                 INSERT INTO repeats SELECT 1000 + n, 'pad' || n FROM generate_series(1, 90) n;
                 CREATE INDEX repeats_idx ON repeats USING stannum(body)"
            ))
            .unwrap();
            for (query, weight) in [
                ("a OR a^2", "40400000"),
                ("a a", "40000000"),
                ("(a^2)^3", "40c00000"),
            ] {
                let inspected = Spi::get_one::<String>(&format!(
                    "SELECT coalesce(json_agg(json_build_array(term, encode(float4send(weight), 'hex'))
                     ORDER BY term), '[]')::text
                     FROM stannum.score_inspect('repeats_idx'::regclass, '{query}')"
                ))
                .unwrap()
                .unwrap();
                assert_eq!(inspected, format!("[[\"a\", \"{weight}\"]]"), "{query}");
            }
            for custom_scan in ["on", "off"] {
                Spi::run(&format!(
                    "SET LOCAL stannum.enable_custom_scan = {custom_scan}"
                ))
                .unwrap();
                for function in ["score", "full_score"] {
                    let scores = |query: &str, limit: &str| {
                        score_bits(&format!(
                            "SELECT id, stannum.{function}(ctid) FROM repeats
                             WHERE body ==> '{query}'
                             ORDER BY stannum.{function}(ctid) DESC {limit}"
                        ))
                    };
                    for limit in ["", "LIMIT 1"] {
                        let doubled = scores("a^2", limit);
                        assert_ne!(doubled, scores("a", limit));
                        assert_eq!(scores("a a", limit), doubled, "{function} {limit}");
                        assert_eq!(scores("a AND a", limit), doubled, "{function} {limit}");
                    }
                }
            }
            Spi::run("DROP TABLE repeats").unwrap();
        }
    }

    /// With `==>` clauses on two indexed columns, a row's score is the sum of
    /// its scores for each column's query: TIN 1.0.3 gives the row matching
    /// both columns both scores and a row matching one column that column's
    /// (conformance/expected/tin-1.0.3/catalog.scoring.json, catalog.S-18).
    #[pg_test]
    fn tin_1_0_3_scores_sum_across_indexed_columns() {
        // A segmented index, then a temporary table's heap-scored one.
        for table_kind in ["", "TEMP"] {
            Spi::run(&format!(
                "CREATE {table_kind} TABLE two_columns(id int, name text, notes text);
                 INSERT INTO two_columns VALUES (1, 'fuji', 'citrus'), (2, 'fuji', 'x'), (3, 'x', 'citrus');
                 INSERT INTO two_columns SELECT 1000 + n, 'pad' || n, 'pad' || n FROM generate_series(1, 90) n;
                 CREATE INDEX two_columns_name ON two_columns USING stannum(name);
                 CREATE INDEX two_columns_notes ON two_columns USING stannum(notes)"
            ))
            .unwrap();
            for custom_scan in ["on", "off"] {
                Spi::run(&format!(
                    "SET LOCAL stannum.enable_custom_scan = {custom_scan}"
                ))
                .unwrap();
                // No term is dense here, so full_score scores as score does.
                for function in ["score", "full_score"] {
                    let scores = |clause: &str, order: &str| {
                        score_bits(&format!(
                            "SELECT id, stannum.{function}(ctid) FROM two_columns
                             WHERE {clause} {order}"
                        ))
                    };
                    for (clause, recorded, top) in [
                        (
                            "name ==> 'fuji' OR notes ==> 'citrus'",
                            r#"[[1, "40e820d6"], [2, "406820d6"], [3, "406820d6"]]"#,
                            r#"[[1, "40e820d6"]]"#,
                        ),
                        (
                            "name ==> 'fuji^1.5' OR notes ==> 'citrus'",
                            r#"[[1, "41111486"], [2, "40ae18a0"], [3, "406820d6"]]"#,
                            r#"[[1, "41111486"]]"#,
                        ),
                    ] {
                        let context = format!("{table_kind} {custom_scan} {function} {clause}");
                        assert_eq!(scores(clause, ""), recorded, "{context}");
                        // Ranked, the row matching both columns comes first.
                        assert_eq!(
                            scores(
                                clause,
                                &format!("ORDER BY stannum.{function}(ctid) DESC, id LIMIT 1")
                            ),
                            top,
                            "{context}"
                        );
                    }
                    // AND sums too, and a ranked AND, which the index scan
                    // cannot order by a sum, is sorted over its matches.
                    for order in ["", "ORDER BY 2 DESC LIMIT 1"] {
                        assert_eq!(
                            scores("name ==> 'fuji' AND notes ==> 'citrus'", order),
                            r#"[[1, "40e820d6"]]"#
                        );
                    }
                }
            }
            Spi::run("DROP TABLE two_columns").unwrap();
        }
    }

    /// An implicit highlight finds the `==>` clause through an UPDATE's
    /// RETURNING, a CTE and a subquery, and without one returns the text
    /// unmarked, as TIN 1.0.3 answers (conformance/expected/tin-1.0.3/
    /// catalog.highlight.json, catalog.H-08 and catalog.H-12).
    #[pg_test]
    fn tin_1_0_3_implicit_highlight_binding() {
        let text = |sql: &str| Spi::get_one::<String>(sql).unwrap();
        assert_eq!(text("SELECT stannum.highlight(NULL, query => 'a')"), None);
        assert_eq!(text("SELECT stannum.highlight('x')"), Some("x".into()));
        assert_eq!(
            text("SELECT stannum.highlight('a b', '<b>', '</b>', 'a AND')"),
            Some("a b".into())
        );
        Spi::run(
            "CREATE TABLE implicit_binding(id int, body text);
             INSERT INTO implicit_binding VALUES (1, 'urgent x');
             CREATE INDEX implicit_binding_idx ON implicit_binding USING stannum(body)",
        )
        .unwrap();
        for sql in [
            "UPDATE implicit_binding SET body = body WHERE body ==> 'urgent'
             RETURNING stannum.highlight(body)",
            "WITH m AS (SELECT body FROM implicit_binding WHERE body ==> 'urgent')
             SELECT stannum.highlight(body) FROM m",
            "SELECT stannum.highlight(body)
             FROM (SELECT body FROM implicit_binding WHERE body ==> 'urgent') s",
        ] {
            assert_eq!(text(sql), Some("<b>urgent</b> x".into()), "{sql}");
        }
    }

    /// The SQLSTATE and message `sql` fails with, or `None` when it succeeds.
    /// The statement runs in a subtransaction, so a failure leaves the
    /// test's transaction usable.
    fn outcome(sql: &str) -> Option<(String, String)> {
        Spi::run(
            "CREATE OR REPLACE FUNCTION pg_temp.outcome(statement text) RETURNS text[]
             LANGUAGE plpgsql AS $$
             DECLARE state text; message text;
             BEGIN
                 EXECUTE statement;
                 RETURN NULL;
             EXCEPTION WHEN OTHERS THEN
                 GET STACKED DIAGNOSTICS state = RETURNED_SQLSTATE, message = MESSAGE_TEXT;
                 RETURN ARRAY[state, message];
             END $$",
        )
        .unwrap();
        Spi::get_one::<Vec<String>>(&format!("SELECT pg_temp.outcome($outcome${sql}$outcome$)"))
            .unwrap()
            .map(|pair| (pair[0].clone(), pair[1].clone()))
    }

    /// An invalid `==>` query raises TIN 1.0.3's message on every path: the
    /// query and the byte the error points at, then the error without the
    /// stage that raised it (conformance/expected/tin-1.0.3/
    /// catalog.syntax.json, catalog.expansion.json and catalog.relations.json:
    /// catalog.Q-14, catalog.E-07, catalog.E-10, catalog.R-06). The syntax
    /// errors TIN words after its pest grammar ("expected expected base")
    /// are left out: the descent parser names what it expected in words.
    #[pg_test]
    fn tin_1_0_3_invalid_query_messages() {
        Spi::run(
            "CREATE TABLE invalid_queries(body text);
             INSERT INTO invalid_queries VALUES ('craft beer');
             CREATE INDEX invalid_queries_idx ON invalid_queries USING stannum(body)",
        )
        .unwrap();
        for (query, message) in [
            (
                r#""""#,
                r#"invalid ==> query at byte 0 in "\"\"": empty phrase (at byte 0)"#,
            ),
            (
                "[]",
                r#"invalid ==> query at byte 0 in "[]": empty alternatives (at byte 0)"#,
            ),
            (
                "* IN FIRST 3 WORDS",
                r#"invalid ==> query in "* IN FIRST 3 WORDS": MatchAll (*) is not valid inside a span/positional context"#,
            ),
            (
                "a* TO c",
                r#"invalid ==> query at byte 0 in "a* TO c": range bound "a*" contains a wildcard (at byte 0): bounds must be plain terms"#,
            ),
            (
                "... TO z",
                r#"invalid ==> query in "... TO z": range bound "..." sub-tokenizes into no tokens"#,
            ),
            (
                r"MATCHES (a)\1",
                "invalid ==> query in \"MATCHES (a)\\\\1\": invalid regex \"(a)\\1\": regex parse error:\n    \\A(?:(a)\\1)\\z\n            ^^\nerror: backreferences are not supported",
            ),
        ] {
            let quoted = query.replace('\'', "''");
            for (setting, sql) in [
                (
                    "on",
                    format!("SELECT count(*) FROM invalid_queries WHERE body ==> '{quoted}'"),
                ),
                (
                    "off",
                    format!("SELECT count(*) FROM invalid_queries WHERE body ==> '{quoted}'"),
                ),
                ("on", format!("SELECT 'craft beer' ==> '{quoted}'")),
            ] {
                Spi::run(&format!("SET LOCAL stannum.enable_custom_scan = {setting}")).unwrap();
                assert_eq!(
                    outcome(&sql),
                    Some(("XX000".to_owned(), message.to_owned())),
                    "{sql} (custom scan {setting})"
                );
            }
        }
    }

    /// Every WITH option at the ends of its domain and one step outside:
    /// TIN 1.0.3 accepts the ends and rejects the rest with 22023 (answers
    /// recorded in conformance/expected/tin-1.0.3/catalog.ddl.json,
    /// catalog.I-02).
    #[pg_test]
    fn tin_1_0_3_index_option_domains() {
        Spi::run("CREATE TABLE option_domains(body text)").unwrap();
        for (option, rejected) in [
            ("k1 = 0", None),
            ("k1 = 10000", None),
            ("k1 = -1", Some("value -1 out of bounds for option \"k1\"")),
            (
                "k1 = 10001",
                Some("value 10001 out of bounds for option \"k1\""),
            ),
            ("b = 0", None),
            ("b = 1", None),
            (
                "b = -0.1",
                Some("value -0.1 out of bounds for option \"b\""),
            ),
            ("b = 1.1", Some("value 1.1 out of bounds for option \"b\"")),
            ("max_token_bytes = 4", None),
            ("max_token_bytes = 2692", None),
            (
                "max_token_bytes = 3",
                Some("value 3 out of bounds for option \"max_token_bytes\""),
            ),
            (
                "max_token_bytes = 2693",
                Some("value 2693 out of bounds for option \"max_token_bytes\""),
            ),
            ("initial_segment_count = 1", None),
            ("initial_segment_count = 4096", None),
            (
                "initial_segment_count = 0",
                Some("value 0 out of bounds for option \"initial_segment_count\""),
            ),
            (
                "initial_segment_count = 4097",
                Some("value 4097 out of bounds for option \"initial_segment_count\""),
            ),
            ("target_segment_count = 1", None),
            ("target_segment_count = 4096", None),
            (
                "target_segment_count = 0",
                Some("value 0 out of bounds for option \"target_segment_count\""),
            ),
            (
                "target_segment_count = 4097",
                Some("value 4097 out of bounds for option \"target_segment_count\""),
            ),
            ("max_mutable_segment_size = 131072", None),
            (
                "max_mutable_segment_size = 131071",
                Some("value 131071 out of bounds for option \"max_mutable_segment_size\""),
            ),
            ("max_merged_segment_size = 100", None),
            (
                "max_merged_segment_size = 99",
                Some("value 99 out of bounds for option \"max_merged_segment_size\""),
            ),
            ("dead_percent_threshold = 0", None),
            ("dead_percent_threshold = 1", None),
            (
                "dead_percent_threshold = -0.1",
                Some("value -0.1 out of bounds for option \"dead_percent_threshold\""),
            ),
            (
                "dead_percent_threshold = 1.1",
                Some("value 1.1 out of bounds for option \"dead_percent_threshold\""),
            ),
            ("tokenizer = 'unicode'", None),
            ("tokenizer = 'whitespace'", None),
            (
                "tokenizer = 'icu'",
                Some("invalid value for enum option \"tokenizer\": icu"),
            ),
            ("case_folding = 'fold'", None),
            ("case_folding = 'preserve'", None),
            (
                "case_folding = 'upper'",
                Some("invalid value for enum option \"case_folding\": upper"),
            ),
            ("accent_folding = 'fold'", None),
            ("accent_folding = 'preserve'", None),
            (
                "accent_folding = 'strip'",
                Some("invalid value for enum option \"accent_folding\": strip"),
            ),
            ("long_tokens = 'split'", None),
            ("long_tokens = 'truncate'", None),
            ("long_tokens = 'discard'", None),
            (
                "long_tokens = 'wrap'",
                Some("invalid value for enum option \"long_tokens\": wrap"),
            ),
            ("graphemes = 'emoji'", None),
            ("graphemes = 'retain'", None),
            ("graphemes = 'discard'", None),
            (
                "graphemes = 'all'",
                Some("invalid value for enum option \"graphemes\": all"),
            ),
            ("position_gaps = 'preserve'", None),
            ("position_gaps = 'collapse'", None),
            (
                "position_gaps = 'keep'",
                Some("invalid value for enum option \"position_gaps\": keep"),
            ),
            ("score_stop_words = 'the, a'", None),
        ] {
            let got = outcome(&format!(
                "CREATE INDEX ON option_domains USING stannum(body) WITH ({option})"
            ));
            let want = rejected.map(|message| ("22023".to_owned(), message.to_owned()));
            assert_eq!(got, want, "WITH ({option})");
        }
    }
}
