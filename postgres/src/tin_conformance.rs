// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Stannum against TIN's recorded responses.
//!
//! Every expected value in this module is a response of PlanetScale TIN,
//! not a value Stannum produced, read from
//! `postgres/tests/tin_responses/tin-1.0.3.json`:
//!
//! - TIN extension version: 1.0.3
//! - Server: PostgreSQL 18.6 (Debian 18.6-1.pgdg12+2, aarch64), PlanetScale
//!   Postgres, us-east-1
//! - Measured: 2026-09-26, with `benchmarks/tin_behavior_probe.py` at
//!   `29a520e`
//!
//! See `docs/tin-behavior.md` for the measurement and the decisions taken
//! from it. To follow a new TIN release, measure it into a new
//! `tin-<version>.json` and point [`tests::RESPONSES`] at it; never edit the
//! recorded values of an existing file.

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use serde_json::Value;

    /// TIN 1.0.3 on PostgreSQL 18.6 (PlanetScale), measured 2026-09-26.
    pub(super) const RESPONSES: &str = include_str!("../tests/tin_responses/tin-1.0.3.json");
    const TIN_VERSION: &str = "1.0.3";

    fn responses() -> Value {
        let responses: Value =
            serde_json::from_str(RESPONSES).expect("recorded TIN responses parse");
        assert_eq!(
            responses["source"]["extension_version"], TIN_VERSION,
            "the tests name the TIN version they were recorded from"
        );
        responses
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

    /// TIN 1.0.3 (PostgreSQL 18.6, PlanetScale, 2026-09-26): spans whose
    /// operands are phrases, and the plain spans beside them. Matches must
    /// equal TIN's with the custom scan on and off; the ranked top ten must
    /// equal TIN's order under `full_score`.
    #[pg_test]
    fn tin_1_0_3_spans_with_phrase_operands() {
        let responses = responses();
        let spans = &responses["spans"];
        Spi::run("CREATE TABLE tin_spans(id int PRIMARY KEY, body text)").unwrap();
        for (id, body) in spans["documents"].as_object().unwrap() {
            Spi::run(&format!(
                "INSERT INTO tin_spans VALUES ({id}, '{}')",
                literal(body.as_str().unwrap())
            ))
            .unwrap();
        }
        Spi::run("CREATE INDEX tin_spans_idx ON tin_spans USING stannum(body); SET LOCAL enable_seqscan = off")
            .unwrap();
        let mut differences = Vec::new();
        for case in spans["cases"].as_array().unwrap() {
            let query = case["query"].as_str().unwrap();
            let quoted = literal(query);
            let ids = numbers(&case["ids"]);
            let ranked = numbers(&case["ranked"]);
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
                if count as usize != ids.len() {
                    differences.push(format!(
                        "{query} (custom scan {custom}): count {count}, TIN {}",
                        ids.len()
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

    /// TIN 1.0.3 (PostgreSQL 18.6, PlanetScale, 2026-09-26): BM25 with a
    /// chosen k1. Every score must be bit-identical to TIN's, and the pruned
    /// `LIMIT 1` must return TIN's row, which is the exhaustive top; at
    /// k1 = 0 the scores of higher term frequencies round one ulp lower.
    #[pg_test]
    fn tin_1_0_3_bm25_k1_scores_and_pruned_top() {
        let responses = responses();
        let bm25 = &responses["bm25_parameters"];
        let b = bm25["b"].as_f64().unwrap();
        Spi::run(
            "CREATE TABLE tin_w(id int PRIMARY KEY, body text);
             INSERT INTO tin_w VALUES (1, 'w w w');
             INSERT INTO tin_w SELECT n, 'w' FROM generate_series(2, 10) n;
             CREATE INDEX tin_w_idx ON tin_w USING stannum(body);
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        let mut differences = Vec::new();
        for (k1, case) in bm25["k1"].as_object().unwrap() {
            let bits: Vec<String> = column(&format!(
                "SELECT encode(float4send(stannum.full_score(ctid, {k1}::real, {b}::real)), 'hex')
                 FROM tin_w WHERE body ==> 'w' ORDER BY id"
            ));
            for (id, got) in (1..).zip(&bits) {
                let want = case["float4_bits"][id.to_string()].as_str().unwrap();
                if got != want {
                    differences.push(format!("k1 {k1}, id {id}: score bits {got}, TIN {want}"));
                }
            }
            let top = Spi::get_one::<i32>(&format!(
                "SELECT id FROM tin_w WHERE body ==> 'w'
                 ORDER BY stannum.full_score(ctid, {k1}::real, {b}::real) DESC LIMIT 1"
            ))
            .unwrap()
            .unwrap();
            let want = case["pruned_top_limit_1"].as_i64().unwrap() as i32;
            assert_eq!(
                want,
                case["exhaustive_top"].as_i64().unwrap() as i32,
                "TIN's pruned top is its exhaustive top"
            );
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

    /// TIN 1.0.3 (PostgreSQL 18.6, PlanetScale, 2026-09-26): query size and
    /// nesting. Stannum must answer every query TIN answered, with TIN's
    /// count; the sizes that crashed TIN's server must get an ERROR or a
    /// correct count from Stannum, never a crash (a crash ends this test's
    /// backend, which fails the test).
    #[pg_test]
    fn tin_1_0_3_query_size_and_nesting() {
        let responses = responses();
        let sizes = &responses["query_size"];
        Spi::run("CREATE TABLE tin_size(id int PRIMARY KEY, body text)").unwrap();
        for (id, body) in responses["spans"]["documents"].as_object().unwrap() {
            Spi::run(&format!(
                "INSERT INTO tin_size VALUES ({id}, '{}')",
                literal(body.as_str().unwrap())
            ))
            .unwrap();
        }
        Spi::run(
            "CREATE INDEX tin_size_idx ON tin_size USING stannum(body);
             CREATE TEMP TABLE tin_size_outcome(label text, ok bool, n bigint, state text);",
        )
        .unwrap();
        let query_of = |case: &Value| -> (String, String) {
            let shape = case["shape"].as_str().unwrap();
            match shape {
                "words" => {
                    let n = case["terms"].as_i64().unwrap();
                    (format!("words {n}"), format!("repeat('a ', {n})"))
                }
                "or_chain" => {
                    let n = case["terms"].as_i64().unwrap();
                    (
                        format!("or_chain {n}"),
                        format!("array_to_string(array_fill('a'::text, ARRAY[{n}]), ' OR ')"),
                    )
                }
                "nested" => {
                    let n = case["depth"].as_i64().unwrap();
                    (
                        format!("nested {n}"),
                        format!("repeat('(', {n}) || 'a' || repeat(')', {n})"),
                    )
                }
                other => panic!("unknown recorded shape {other}"),
            }
        };
        // The expression is parenthesized: `||` and `==>` share PostgreSQL's
        // precedence for other operators, left to right, so without it
        // `body ==> repeat('(', n) || 'a'` applies `==>` to the first term.
        let run = |label: &str, expression: &str| {
            Spi::run(&format!(
                "DO $probe$ DECLARE n bigint; BEGIN
                   SELECT count(*) INTO n FROM tin_size WHERE body ==> ({expression});
                   INSERT INTO tin_size_outcome VALUES ('{label}', true, n, NULL);
                 EXCEPTION WHEN OTHERS THEN
                   INSERT INTO tin_size_outcome VALUES ('{label}', false, NULL, SQLSTATE);
                 END $probe$"
            ))
            .unwrap();
        };
        let mut differences = Vec::new();
        for case in sizes["accepted"].as_array().unwrap() {
            let (label, expression) = query_of(case);
            run(&label, &expression);
            let (ok, n, state) = Spi::get_three::<bool, i64, String>(&format!(
                "SELECT ok, n, state FROM tin_size_outcome WHERE label = '{label}'"
            ))
            .unwrap();
            let want = case["count"].as_i64().unwrap();
            if ok != Some(true) || n != Some(want) {
                differences.push(format!(
                    "{label}: TIN answered {want}, Stannum ok {ok:?} count {n:?} sqlstate {state:?}"
                ));
            }
        }
        for case in sizes["crashed_server"].as_array().unwrap() {
            let (label, expression) = query_of(case);
            run(&label, &expression);
            let (ok, n, _) = Spi::get_three::<bool, i64, String>(&format!(
                "SELECT ok, n, state FROM tin_size_outcome WHERE label = '{label}'"
            ))
            .unwrap();
            if ok == Some(true) && n != Some(0) {
                differences.push(format!(
                    "{label}: TIN crashed; Stannum answered a wrong count {n:?}"
                ));
            }
        }
        assert!(
            differences.is_empty(),
            "Stannum differs from TIN {TIN_VERSION}:\n{}",
            differences.join("\n")
        );
    }
}
