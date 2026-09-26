// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Oversized and deeply nested queries through the whole server path.
//!
//! A query past `tinql::limits` must end in a clean ERROR, and one within
//! them in its answer, wherever the server parses it: planning (the custom
//! scan's selectivity estimate), EXPLAIN, execution through the index or the
//! custom scan, the `==>` operator on an unindexed table, and ranking. Before
//! the limits, 300,000 words or 100,000 nested parentheses overflowed the
//! backend's stack, and the abort restarted every session.

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    /// Query text, as SQL, with its expected count of the 100 matching rows
    /// (`None`: rejected with an error containing `reason`).
    struct Case {
        label: &'static str,
        sql: &'static str,
        count: Option<i64>,
        reason: &'static str,
    }

    const CASES: &[Case] = &[
        // Within the limits, including every size PlanetScale TIN 1.0.3
        // answered (it crashed at 10,000 words, 10,000 OR terms and 5,000
        // nesting levels).
        Case {
            label: "3,000 words",
            sql: "repeat('a ', 3000)",
            count: Some(100),
            reason: "",
        },
        Case {
            label: "10,000 words",
            sql: "repeat('a ', 10000)",
            count: Some(100),
            reason: "",
        },
        Case {
            label: "10,000-term OR chain",
            sql: "'a' || repeat(' OR a', 9999)",
            count: Some(100),
            reason: "",
        },
        Case {
            label: "1,000 nested parentheses",
            sql: "repeat('(', 1000) || 'a' || repeat(')', 1000)",
            count: Some(100),
            reason: "",
        },
        Case {
            label: "999 nested OR groups",
            sql: "repeat('(zz OR ', 999) || 'a' || repeat(')', 999)",
            count: Some(100),
            reason: "",
        },
        Case {
            label: "1,000 nested alternatives",
            sql: "repeat('[', 1000) || 'a' || repeat(']', 1000)",
            count: Some(100),
            reason: "",
        },
        // Past them.
        Case {
            label: "300,000 words",
            sql: "repeat('a ', 300000)",
            count: None,
            reason: "more than 10000 terms",
        },
        Case {
            label: "300,000-term OR chain",
            sql: "'a' || repeat(' OR a', 299999)",
            count: None,
            reason: "more than 10000 terms",
        },
        Case {
            label: "100,000 nested parentheses",
            sql: "repeat('(', 100000) || 'a' || repeat(')', 100000)",
            count: None,
            reason: "nesting exceeds 1000 levels",
        },
        Case {
            label: "5,000 nested parentheses",
            sql: "repeat('(', 5000) || 'a' || repeat(')', 5000)",
            count: None,
            reason: "nesting exceeds 1000 levels",
        },
        Case {
            label: "100,000 nested alternatives",
            sql: "repeat('[', 100000) || 'a' || repeat(']', 100000)",
            count: None,
            reason: "nesting exceeds 1000 levels",
        },
        Case {
            label: "300,000-term AND NOT chain",
            sql: "'a' || repeat(' AND NOT zz', 299999)",
            count: None,
            reason: "nesting exceeds 1000 levels",
        },
        Case {
            label: "MATCHES with 100,000 nested groups",
            sql: "'MATCHES ' || repeat('(', 100000) || 'a' || repeat(')', 100000)",
            count: None,
            reason: "invalid regex",
        },
    ];

    /// The statements each case runs, `$q` standing for its query text.
    const STATEMENTS: &[(&str, &str)] = &[
        (
            "count",
            "SELECT count(*) FROM query_limits WHERE body ==> $q",
        ),
        (
            "unindexed count",
            "SELECT count(*) FROM query_limits_heap WHERE body ==> $q",
        ),
        (
            "ranked",
            "SELECT count(*) FROM (SELECT id FROM query_limits WHERE body ==> $q
             ORDER BY stannum.full_score(ctid) DESC LIMIT 5) ranked",
        ),
        (
            "explain",
            "EXPLAIN SELECT count(*) FROM query_limits WHERE body ==> $q",
        ),
        (
            "explain analyze",
            "EXPLAIN ANALYZE SELECT count(*) FROM query_limits WHERE body ==> $q",
        ),
    ];

    #[pg_test]
    fn oversized_and_deep_queries_end_in_an_answer_or_a_clean_error() {
        Spi::run(
            "CREATE TABLE query_limits(id int PRIMARY KEY, body text);
             INSERT INTO query_limits SELECT n,
                 CASE WHEN n % 3 = 0 THEN 'a b' ELSE 'b c' END
             FROM generate_series(1, 300) n;
             CREATE INDEX query_limits_idx ON query_limits USING stannum(body);
             CREATE TABLE query_limits_heap AS SELECT * FROM query_limits;
             ANALYZE query_limits;
             CREATE TEMP TABLE query_limits_outcome(
                 label text, statement text, n bigint, message text);",
        )
        .unwrap();

        for case in CASES {
            for (statement, sql) in STATEMENTS {
                // Each statement runs in a subtransaction, so an ERROR is
                // recorded and the test goes on; a crash would end it.
                let run = if sql.starts_with("EXPLAIN") {
                    format!(
                        "FOR line IN EXECUTE format('{}', q) LOOP n := coalesce(n, 0) + 1; END LOOP;",
                        sql.replace('\'', "''").replace("$q", "%L")
                    )
                } else {
                    format!(
                        "EXECUTE format('{}', q) INTO n;",
                        sql.replace('\'', "''").replace("$q", "%L")
                    )
                };
                Spi::run(&format!(
                    "DO $$ DECLARE q text := {query}; n bigint; line text; BEGIN
                         {run}
                         INSERT INTO query_limits_outcome VALUES ('{label}', '{statement}', n, NULL);
                     EXCEPTION WHEN OTHERS THEN
                         INSERT INTO query_limits_outcome
                             VALUES ('{label}', '{statement}', NULL, SQLERRM);
                     END $$",
                    query = case.sql,
                    label = case.label,
                ))
                .unwrap();
            }
        }

        for case in CASES {
            for (statement, _) in STATEMENTS {
                let (n, message) = Spi::get_two::<i64, String>(&format!(
                    "SELECT n, message FROM query_limits_outcome
                     WHERE label = '{}' AND statement = '{statement}'",
                    case.label
                ))
                .unwrap();
                let explain = statement.starts_with("explain");
                match case.count {
                    Some(expected) => {
                        assert_eq!(message, None, "{} / {statement}", case.label);
                        if !explain {
                            let expected = if *statement == "ranked" { 5 } else { expected };
                            assert_eq!(n, Some(expected), "{} / {statement}", case.label);
                        }
                    }
                    // Plain EXPLAIN only plans, and planning falls back to a
                    // default estimate for a query it cannot parse; every
                    // statement that runs the query must fail cleanly.
                    None if *statement == "explain" => assert!(
                        message.is_none() || message.as_deref().unwrap().contains(case.reason),
                        "{} / {statement}: {message:?}",
                        case.label
                    ),
                    None => {
                        let message = message.unwrap_or_default();
                        assert!(
                            message.contains(case.reason),
                            "{} / {statement}: expected an error containing {:?}, got {message:?} \
                             (n = {n:?})",
                            case.label,
                            case.reason
                        );
                    }
                }
            }
        }
    }
}
