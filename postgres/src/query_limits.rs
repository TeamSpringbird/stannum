// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Oversized and deeply nested queries through the whole server path.
//!
//! A query past `tinql::limits` must end in a clean ERROR, and one within
//! them in its answer, wherever the server parses it: planning (the custom
//! scan's selectivity estimate), EXPLAIN, execution through the index or the
//! custom scan, the `==>` operator on an unindexed table, and ranking, with
//! SQLSTATE 54001 (`statement_too_complex`). Before the limits, 300,000
//! words or 100,000 nested parentheses overflowed the backend's stack, and
//! `AT LEAST 15 OF` 30 terms inside `NEAR` exhausted its memory; either
//! abort restarted every session.

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    /// Query text, as SQL, with its expected count of the 100 matching rows
    /// (`None`: rejected with an error containing `reason`, with SQLSTATE
    /// `sqlstate`).
    struct Case {
        label: &'static str,
        sql: &'static str,
        count: Option<i64>,
        reason: &'static str,
        sqlstate: &'static str,
        /// A tree a thousand operators high: a release build answers it in
        /// about 1.1 MiB of stack, but the unoptimized build these tests run
        /// needs about 6 MiB, past the default `max_stack_depth` of 2 MiB,
        /// so there it may end in `check_stack_depth`'s ERROR instead,
        /// never a crash.
        backstop: bool,
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
            sqlstate: "",
            backstop: false,
        },
        Case {
            label: "10,000 words",
            sql: "repeat('a ', 10000)",
            count: Some(100),
            reason: "",
            sqlstate: "",
            backstop: false,
        },
        Case {
            label: "10,000-term OR chain",
            sql: "'a' || repeat(' OR a', 9999)",
            count: Some(100),
            reason: "",
            sqlstate: "",
            backstop: false,
        },
        Case {
            label: "1,000 nested parentheses",
            sql: "repeat('(', 1000) || 'a' || repeat(')', 1000)",
            count: Some(100),
            reason: "",
            sqlstate: "",
            backstop: false,
        },
        Case {
            label: "999 nested OR groups",
            sql: "repeat('(zz OR ', 999) || 'a' || repeat(')', 999)",
            count: Some(100),
            reason: "",
            sqlstate: "",
            backstop: true,
        },
        Case {
            label: "1,000 nested alternatives",
            sql: "repeat('[', 1000) || 'a' || repeat(']', 1000)",
            count: Some(100),
            reason: "",
            sqlstate: "",
            backstop: true,
        },
        // Past them.
        Case {
            label: "300,000 words",
            sql: "repeat('a ', 300000)",
            count: None,
            reason: "more than 10000 terms",
            sqlstate: "54001",
            backstop: false,
        },
        Case {
            label: "300,000-term OR chain",
            sql: "'a' || repeat(' OR a', 299999)",
            count: None,
            reason: "more than 10000 terms",
            sqlstate: "54001",
            backstop: false,
        },
        Case {
            label: "100,000 nested parentheses",
            sql: "repeat('(', 100000) || 'a' || repeat(')', 100000)",
            count: None,
            reason: "nesting exceeds 1000 levels",
            sqlstate: "54001",
            backstop: false,
        },
        Case {
            label: "5,000 nested parentheses",
            sql: "repeat('(', 5000) || 'a' || repeat(')', 5000)",
            count: None,
            reason: "nesting exceeds 1000 levels",
            sqlstate: "54001",
            backstop: false,
        },
        Case {
            label: "100,000 nested alternatives",
            sql: "repeat('[', 100000) || 'a' || repeat(']', 100000)",
            count: None,
            reason: "nesting exceeds 1000 levels",
            sqlstate: "54001",
            backstop: false,
        },
        Case {
            label: "300,000-term AND NOT chain",
            sql: "'a' || repeat(' AND NOT zz', 299999)",
            count: None,
            reason: "nesting exceeds 1000 levels",
            sqlstate: "54001",
            backstop: false,
        },
        // AT LEAST inside a proximity operator is matched as the
        // disjunction of its combinations; C(30, 15) is 155 million, which
        // exhausted the backend's memory without answering a cancel.
        Case {
            label: "AT LEAST 15 OF 30 inside NEAR",
            sql: "'(AT LEAST 15 OF [' || (SELECT string_agg('t' || n, ' ')
                  FROM generate_series(1, 30) n) || ']) NEAR/5 a'",
            count: None,
            reason: "AT LEAST 15 OF 30 operands inside a proximity operator expands to more than \
                     10000 combinations",
            sqlstate: "54001",
            backstop: false,
        },
        Case {
            label: "AT LEAST 999 OF 1000 inside NEAR",
            sql: "'(AT LEAST 999 OF [' || (SELECT string_agg('t' || n, ' ')
                  FROM generate_series(1, 1000) n) || ']) NEAR/5 a'",
            count: None,
            reason: "AT LEAST inside a proximity operator expands the query by more than 100000 \
                     operands",
            sqlstate: "54001",
            backstop: false,
        },
        Case {
            label: "AT LEAST 2 OF 100 inside WITHIN",
            sql: "'(AT LEAST 2 OF [a b ' || (SELECT string_agg('t' || n, ' ')
                  FROM generate_series(1, 98) n) || ']) WITHIN 3'",
            count: Some(100),
            reason: "",
            sqlstate: "",
            backstop: false,
        },
        Case {
            label: "MATCHES with 100,000 nested groups",
            sql: "'MATCHES ' || repeat('(', 100000) || 'a' || repeat(')', 100000)",
            count: None,
            reason: "invalid regex",
            sqlstate: "XX000",
            backstop: false,
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
                 label text, statement text, n bigint, message text, sqlstate text);",
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
                         INSERT INTO query_limits_outcome
                             VALUES ('{label}', '{statement}', n, NULL, NULL);
                     EXCEPTION WHEN OTHERS THEN
                         INSERT INTO query_limits_outcome
                             VALUES ('{label}', '{statement}', NULL, SQLERRM, SQLSTATE);
                     END $$",
                    query = case.sql,
                    label = case.label,
                ))
                .unwrap();
            }
        }

        for case in CASES {
            for (statement, _) in STATEMENTS {
                let (n, message, sqlstate) = Spi::get_three::<i64, String, String>(&format!(
                    "SELECT n, message, sqlstate FROM query_limits_outcome
                     WHERE label = '{}' AND statement = '{statement}'",
                    case.label
                ))
                .unwrap();
                let explain = statement.starts_with("explain");
                match case.count {
                    Some(_)
                        if case.backstop
                            && message
                                .as_deref()
                                .is_some_and(|m| m.contains("stack depth limit exceeded")) => {}
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
                        message.is_none()
                            || message.as_deref().unwrap().contains(case.reason)
                                && sqlstate.as_deref() == Some(case.sqlstate),
                        "{} / {statement}: {sqlstate:?} {message:?}",
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
                        assert_eq!(
                            sqlstate.as_deref(),
                            Some(case.sqlstate),
                            "{} / {statement}: {message}",
                            case.label
                        );
                    }
                }
            }
        }
    }

    /// Runs `sql` with a cancel requested at the first interrupt check of a
    /// dictionary scan (the `expand:scan` race point), and returns how many
    /// such checks ran. `sql` must end in `query_canceled`: a scan that
    /// never checks runs to the end, and the query answers.
    fn cancel_at_first_dictionary_check(sql: &str) -> usize {
        use std::cell::Cell;
        use std::rc::Rc;
        let checks = Rc::new(Cell::new(0));
        let counted = checks.clone();
        crate::storage::testing::set_race_hook(Some(Box::new(move |name| {
            if name == "expand:scan" {
                counted.set(counted.get() + 1);
                if counted.get() == 1 {
                    unsafe {
                        pg_sys::QueryCancelPending = 1;
                        pg_sys::InterruptPending = 1;
                    }
                }
            }
        })));
        let outcome = Spi::run(&format!(
            "DO $$BEGIN
                 PERFORM count(*) FROM ({sql}) q;
                 RAISE EXCEPTION 'the dictionary scan was not canceled';
             EXCEPTION WHEN query_canceled THEN NULL;
             END$$"
        ));
        crate::storage::testing::set_race_hook(None);
        outcome.unwrap_or_else(|error| panic!("{sql}: {error}"));
        checks.get()
    }

    /// A wildcard, regex or fuzzy term with no fixed prefix scans the whole
    /// dictionary of every segment and of the write buffer, when planning
    /// estimates it and when ranking expands it. Each scan checks for
    /// interrupts every 1,024 entries, so a cancel or `statement_timeout`
    /// ends it within that many entries instead of at its end.
    #[pg_test]
    fn dictionary_scans_answer_a_cancel() {
        Spi::run(
            "CREATE TABLE expand_segment(id int, body text);
             INSERT INTO expand_segment SELECT n, 'w' || n FROM generate_series(1, 5000) n;
             INSERT INTO expand_segment VALUES (0, 'xzzx');
             CREATE INDEX expand_segment_idx ON expand_segment USING stannum(body);
             ANALYZE expand_segment;
             SET LOCAL stannum.write_buffer_docs = 100000;
             CREATE TABLE expand_buffer(id int, body text);
             CREATE INDEX expand_buffer_idx ON expand_buffer USING stannum(body);
             INSERT INTO expand_buffer SELECT n, 'w' || n FROM generate_series(1, 5000) n;
             INSERT INTO expand_buffer VALUES (0, 'xzzx');
             ANALYZE expand_buffer;",
        )
        .unwrap();
        for table in ["expand_segment", "expand_buffer"] {
            // Both ends of the scan are covered: the whole dictionary is
            // read, and one term matches.
            for query in ["MATCHES .*zz.*", "xzzy~0:1"] {
                assert_eq!(
                    Spi::get_one::<i64>(&format!(
                        "SELECT count(*) FROM {table} WHERE body ==> '{query}'"
                    ))
                    .unwrap(),
                    Some(1),
                    "{table} {query}"
                );
                for sql in [
                    format!("SELECT id FROM {table} WHERE body ==> '{query}'"),
                    format!(
                        "SELECT id FROM {table} WHERE body ==> '{query}'
                         ORDER BY stannum.full_score(ctid) DESC LIMIT 5"
                    ),
                    format!("SELECT * FROM stannum.score_inspect('{table}_idx', '{query}')"),
                ] {
                    assert_eq!(cancel_at_first_dictionary_check(&sql), 1, "{sql}");
                }
            }
        }
    }
}
