// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Stemming (`WITH (stemmer = '<code>')`, TIN 1.0.4) against TIN 1.0.4's
//! recorded answers.
//!
//! Unless a test says otherwise, every expected value is an answer of
//! PlanetScale TIN 1.0.4 (PostgreSQL 18.6, PlanetScale Postgres, recorded
//! 2026-10-09) to the `conformance/cases/stemming.yaml` case it names, on
//! that case's corpus: the rows below plus 30 rows `(1000 + n, 'pad' || n)`.
//! Where Stannum knowingly differs (it answers with its custom scan off, and
//! applies a changed stemmer at REINDEX, not before), the test says so.

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    /// The rows of the `stem_en` and `stem_plain` corpora.
    const STEM_ROWS: &str = "(1, 'I run every day'), (2, 'she runs fast'),
        (3, 'they were running fast'), (4, 'the runner ran'),
        (5, 'running shoes for runners'), (6, 'Running FAST'),
        (7, 'connection connected connecting'), (8, 'the café cafés'),
        (9, 'generous generously generosity')";

    fn corpus(table: &str, rows: &str, pad: u32, options: &str) {
        Spi::run(&format!(
            "CREATE TABLE {table}(id int PRIMARY KEY, body text);
             INSERT INTO {table} VALUES {rows};
             INSERT INTO {table} SELECT 1000 + n, 'pad' || n FROM generate_series(1, {pad}) n;
             CREATE INDEX {table}_idx ON {table} USING stannum(body) {options};
             SET enable_seqscan = off;"
        ))
        .unwrap();
    }

    fn ids(table: &str, query: &str) -> Vec<i32> {
        Spi::get_one::<Vec<i32>>(&format!(
            "SELECT coalesce(array_agg(id ORDER BY id), '{{}}') FROM {table} WHERE body ==> $q${query}$q$"
        ))
        .unwrap()
        .unwrap()
    }

    fn text(sql: &str) -> Option<String> {
        Spi::get_one::<String>(sql).unwrap()
    }

    fn tokens(sql: &str) -> Vec<String> {
        Spi::get_one::<Vec<String>>(&format!("SELECT array_agg(t) FROM {sql} t"))
            .unwrap()
            .unwrap_or_default()
    }

    /// The ERROR `sql` raises, as SQLSTATE and message, run in a
    /// subtransaction so the test goes on.
    fn error(sql: &str) -> (String, String) {
        let outcome = Spi::get_one::<String>(&format!("SELECT stem_error_of($s${sql}$s$)"))
            .unwrap()
            .unwrap_or_else(|| panic!("{sql} raised no ERROR"));
        let (state, message) = outcome.split_once(' ').unwrap();
        (state.to_owned(), message.to_owned())
    }

    fn error_helper() {
        Spi::run(
            "CREATE FUNCTION stem_error_of(statement text) RETURNS text LANGUAGE plpgsql AS $f$
             BEGIN
                 EXECUTE statement;
                 RETURN NULL;
             EXCEPTION WHEN OTHERS THEN
                 RETURN SQLSTATE || ' ' || SQLERRM;
             END $f$",
        )
        .unwrap();
    }

    #[pg_test]
    fn stemming_tokenize_matches_tin() {
        error_helper();
        // stemming.tokenize.en
        assert_eq!(
            tokens(
                "stannum.tokenize('runs running runner ran easily connection connected generously ponies', stemmer => 'en')"
            ),
            [
                "run", "run", "runner", "ran", "easili", "connect", "connect", "generous", "poni"
            ]
        );
        // stemming.tokenize.fold_order: lowercase, stem, then fold accents.
        assert_eq!(
            tokens("stannum.tokenize('Running CAFÉS Naïvely résumés', stemmer => 'en')"),
            ["run", "cafe", "naiv", "resume"]
        );
        assert_eq!(
            tokens(
                "stannum.tokenize('Running CAFÉS Naïvely résumés', stemmer => 'en', accent_folding => 'preserve')"
            ),
            ["run", "café", "naïv", "résumé"]
        );
        // stemming.tokenize.codes
        assert_eq!(
            tokens("stannum.tokenize('running', stemmer => NULL)"),
            ["running"]
        );
        for code in ["hy", "xx", "english", "EN", ""] {
            assert_eq!(
                error(&format!(
                    "SELECT stannum.tokenize('running', stemmer => '{code}')"
                )),
                (
                    "XX000".into(),
                    format!("unknown stemmer language code: {code}")
                )
            );
        }
        // stemming.tokenize.case_preserve
        assert_eq!(
            error(
                "SELECT stannum.tokenize('Running runs', case_folding => 'preserve', stemmer => 'en')"
            ),
            (
                "XX000".into(),
                "stemming requires case_folding = fold".into()
            )
        );
        // ql_parse takes the same argument.
        assert_eq!(
            text("SELECT stannum.ql_parse('running OR \"runs fast\"', stemmer => 'en')").as_deref(),
            text("SELECT stannum.ql_parse('run OR \"run fast\"')").as_deref()
        );
    }

    #[pg_test]
    fn stemmer_option_is_validated_like_tin() {
        error_helper();
        corpus("stem_ddl", STEM_ROWS, 30, "");
        // stemming.ddl.errors
        let unknown = (
            "XX000".to_owned(),
            "unknown stemmer language code: xx".to_owned(),
        );
        let preserve = (
            "XX000".to_owned(),
            "stemming requires case_folding = fold".to_owned(),
        );
        assert_eq!(
            error("CREATE INDEX ON stem_ddl USING stannum(body) WITH (stemmer = 'xx')"),
            unknown
        );
        assert_eq!(
            error(
                "CREATE INDEX ON stem_ddl USING stannum(body) WITH (stemmer = 'en', case_folding = 'preserve')"
            ),
            preserve
        );
        assert_eq!(
            error("ALTER INDEX stem_ddl_idx SET (stemmer = 'xx')"),
            unknown
        );
        assert_eq!(
            error("ALTER INDEX stem_ddl_idx SET (case_folding = 'preserve', stemmer = 'en')"),
            preserve
        );
    }

    #[pg_test]
    fn stemmed_index_matches_like_tin() {
        corpus("stem_en", STEM_ROWS, 30, "WITH (stemmer = 'en')");
        corpus("stem_plain", STEM_ROWS, 30, "");
        // stemming.match.term
        assert_eq!(ids("stem_en", "run"), [1, 2, 3, 5, 6]);
        assert_eq!(ids("stem_en", "running"), [1, 2, 3, 5, 6]);
        assert_eq!(ids("stem_en", "runners"), [4, 5]);
        assert_eq!(ids("stem_en", "ran"), [4]);
        assert_eq!(ids("stem_en", "connections"), [7]);
        assert_eq!(ids("stem_en", "generosity"), [9]);
        assert_eq!(ids("stem_en", "cafe"), [8]);
        assert_eq!(ids("stem_plain", "run"), [1]);
        assert_eq!(ids("stem_plain", "running"), [3, 5, 6]);
        // stemming.match.boolean
        assert_eq!(ids("stem_en", "runs AND fast"), [2, 3, 6]);
        assert_eq!(ids("stem_en", "runner OR connects"), [4, 5, 7]);
        assert_eq!(ids("stem_en", "running AND NOT fast"), [1, 5]);
        // stemming.phrase.1
        assert_eq!(ids("stem_en", "\"runs fast\""), [2, 3, 6]);
        assert_eq!(ids("stem_en", "\"running shoe\""), [5]);
        assert_eq!(ids("stem_plain", "\"runs fast\""), [2]);
        // stemming.proximity.1
        assert_eq!(ids("stem_en", "runs NEAR/1 fast"), [2, 3, 6]);
        assert_eq!(ids("stem_en", "fast THEN/1 running"), Vec::<i32>::new());
        assert_eq!(ids("stem_en", "running THEN/2 runner"), [5]);
        // stemming.positions.1
        assert_eq!(ids("stem_en", "runs IN FIRST 1 WORDS"), [5, 6]);
        assert_eq!(ids("stem_en", "run IN LAST 1 WORDS"), Vec::<i32>::new());
        assert_eq!(ids("stem_en", "fasting IN LAST 1 WORDS"), [2, 3, 6]);
        assert_eq!(ids("stem_en", "runner IN WORDS 1 TO 2"), [4]);
        // stemming.expansion.*: pattern literals are not stemmed and search
        // the stored stems.
        assert_eq!(ids("stem_en", "runn*"), [4, 5]);
        assert_eq!(ids("stem_en", "running*"), Vec::<i32>::new());
        assert_eq!(ids("stem_en", "run*"), [1, 2, 3, 4, 5, 6]);
        assert_eq!(ids("stem_en", "generos*"), [9]);
        assert_eq!(ids("stem_plain", "runn*"), [3, 4, 5, 6]);
        assert_eq!(ids("stem_en", "runs~0"), Vec::<i32>::new());
        assert_eq!(ids("stem_en", "runs~1"), [1, 2, 3, 5, 6]);
        assert_eq!(ids("stem_en", "runnin~1"), Vec::<i32>::new());
        assert_eq!(ids("stem_en", "MATCHES runn.*"), [4, 5]);
        assert_eq!(ids("stem_en", "MATCHES running"), Vec::<i32>::new());
        assert_eq!(ids("stem_en", "MATCHES connect.*"), [7]);
        assert_eq!(ids("stem_en", "run TO runz"), [1, 2, 3, 4, 5, 6]);
        assert_eq!(ids("stem_en", "running TO runningz"), Vec::<i32>::new());
        // stemming.match.custom_scan_off: TIN refuses with its custom scan
        // off; Stannum answers with the index's tokenizer either way, on the
        // heap too.
        for setting in [
            "SET stannum.enable_custom_scan = off",
            "SET enable_seqscan = on; SET enable_indexscan = off; SET enable_bitmapscan = off",
        ] {
            Spi::run(setting).unwrap();
            assert_eq!(ids("stem_en", "running"), [1, 2, 3, 5, 6], "{setting}");
            assert_eq!(ids("stem_en", "\"runs fast\""), [2, 3, 6], "{setting}");
        }
    }

    #[pg_test]
    fn rows_in_the_write_buffer_are_stemmed() {
        // stemming.match.write_buffer: the rows arrive after the build.
        corpus("stem_buf", "(2000, 'pad')", 30, "WITH (stemmer = 'en')");
        Spi::run(
            "INSERT INTO stem_buf VALUES (1, 'I run every day'), (2, 'she runs fast'),
               (3, 'they were running fast'), (4, 'the runner ran'),
               (5, 'running shoes for runners'), (6, 'Running FAST')",
        )
        .unwrap();
        assert_eq!(ids("stem_buf", "run"), [1, 2, 3, 5, 6]);
        assert_eq!(ids("stem_buf", "running"), [1, 2, 3, 5, 6]);
        assert_eq!(ids("stem_buf", "\"runs fast\""), [2, 3, 6]);
    }

    #[pg_test]
    fn stemmed_scores_and_stop_words_match_tin() {
        corpus("stem_sc", STEM_ROWS, 30, "WITH (stemmer = 'en')");
        corpus("stem_scp", STEM_ROWS, 30, "");
        let bits = |table: &str, query: &str| -> Vec<(i32, String)> {
            Spi::connect(|client| {
                client
                    .select(
                        &format!(
                            "SELECT id, encode(float4send(stannum.full_score(ctid)), 'hex')
                               FROM {table} WHERE body ==> $q${query}$q$ ORDER BY id"
                        ),
                        None,
                        &[],
                    )
                    .unwrap()
                    .map(|row| {
                        (
                            row.get::<i32>(1).unwrap().unwrap(),
                            row.get::<String>(2).unwrap().unwrap(),
                        )
                    })
                    .collect()
            })
        };
        let pairs = |expected: &[(i32, &str)]| -> Vec<(i32, String)> {
            expected
                .iter()
                .map(|(id, hex)| (*id, (*hex).to_owned()))
                .collect()
        };
        // stemming.score.1
        assert_eq!(
            bits("stem_sc", "run"),
            pairs(&[
                (1, "3f97d7da"),
                (2, "3fb52090"),
                (3, "3f97d7da"),
                (5, "3f97d7da"),
                (6, "3fe067c8")
            ])
        );
        assert_eq!(
            bits("stem_sc", "running OR runs"),
            pairs(&[
                (1, "4017d7da"),
                (2, "40352090"),
                (3, "4017d7da"),
                (5, "4017d7da"),
                (6, "406067c8")
            ])
        );
        assert_eq!(bits("stem_scp", "run"), pairs(&[(1, "3ffb4696")]));
        // stemming.score_inspect.1: the query's stems, run elided as dense.
        let inspect = |index: &str, query: &str| {
            text(&format!(
                "SELECT coalesce(json_agg(json_build_array(term, encode(float4send(weight), 'hex'))
                   ORDER BY term), '[]')::text FROM stannum.score_inspect('{index}'::regclass, '{query}')"
            ))
            .unwrap()
        };
        assert_eq!(
            inspect("stem_sc_idx", "running OR runs OR runner OR fast"),
            r#"[["fast", "3f800000"], ["runner", "3f800000"]]"#
        );
        // stemming.stop_words.1: stop words are written as stored stems.
        let rows = "(1, 'running fast'), (2, 'runs slowly'), (3, 'fast cars')";
        corpus(
            "stem_stop",
            rows,
            30,
            "WITH (stemmer = 'en', score_stop_words = 'run')",
        );
        corpus(
            "stem_stop2",
            rows,
            30,
            "WITH (stemmer = 'en', score_stop_words = 'running')",
        );
        assert_eq!(
            inspect("stem_stop_idx", "running OR fast"),
            r#"[["fast", "3f800000"]]"#
        );
        assert_eq!(
            inspect("stem_stop2_idx", "running OR fast"),
            r#"[["fast", "3f800000"], ["run", "3f800000"]]"#
        );
        assert_eq!(ids("stem_stop", "running"), [1, 2]);
    }

    #[pg_test]
    fn stemmed_highlights_match_tin() {
        corpus("stem_hl", STEM_ROWS, 30, "WITH (stemmer = 'en')");
        let highlights = |sql: &str| -> Vec<String> {
            Spi::get_one::<Vec<String>>(&format!("SELECT array_agg(h ORDER BY id) FROM ({sql}) s"))
                .unwrap()
                .unwrap()
        };
        let run = [
            "I <b>run</b> every day",
            "she <b>runs</b> fast",
            "they were <b>running</b> fast",
            "<b>running</b> shoes for runners",
            "<b>Running</b> FAST",
        ];
        // stemming.highlight.implicit
        assert_eq!(
            highlights("SELECT id, stannum.highlight(body) h FROM stem_hl WHERE body ==> 'run'"),
            run
        );
        assert_eq!(
            highlights(
                "SELECT id, stannum.highlight(body) h FROM stem_hl WHERE body ==> '\"runs fast\"'"
            ),
            [
                "she <b>runs fast</b>",
                "they were <b>running fast</b>",
                "<b>Running FAST</b>"
            ]
        );
        // stemming.highlight.explicit
        assert_eq!(
            highlights(
                "SELECT id, stannum.highlight(body, query => 'run') h FROM stem_hl WHERE body ==> 'run'"
            ),
            run
        );
        assert_eq!(
            highlights(
                "SELECT id, stannum.highlight(body, query => 'run', stemmer => 'en') h FROM stem_hl WHERE body ==> 'run'"
            ),
            run
        );
        // stemming.highlight.literal
        assert_eq!(
            text(
                "SELECT stannum.highlight('Runners were running; she runs.', query => 'run', stemmer => 'en')"
            )
            .as_deref(),
            Some("Runners were <b>running</b>; she <b>runs</b>.")
        );
        assert_eq!(
            text(
                "SELECT stannum.highlight('Runners were running; she runs.', query => '\"runs fast\" OR runner', stemmer => 'en')"
            )
            .as_deref(),
            Some("<b>Runners</b> were running; she runs.")
        );
        assert_eq!(
            text(
                "SELECT encode(convert_to(stannum.highlight_ansi('she runs', query => 'running', stemmer => 'en'), 'UTF8'), 'hex')"
            )
            .as_deref(),
            Some("736865201b5b313b33366d72756e731b5b306d")
        );
        // Not a TIN answer: explicit settings are what an explicit call
        // analyzes with, so without a stemmer only the written form marks.
        assert_eq!(
            text(
                "SELECT stannum.highlight('she runs; I run', query => 'run', case_folding => 'fold')"
            )
            .as_deref(),
            Some("she runs; I <b>run</b>")
        );
    }

    #[pg_test]
    fn other_languages_stem_like_tin() {
        // stemming.language.fr, .de, .ru (pad 10)
        corpus(
            "stem_fr",
            "(1, 'je chante'), (2, 'nous chantons'), (3, 'elle a chanté'), (4, 'le chanteur')",
            10,
            "WITH (stemmer = 'fr')",
        );
        assert_eq!(ids("stem_fr", "chanter"), [1, 3]);
        assert_eq!(ids("stem_fr", "chantez"), [1, 3]);
        assert_eq!(ids("stem_fr", "chanteurs"), [4]);
        corpus(
            "stem_de",
            "(1, 'das Haus'), (2, 'die Häuser'), (3, 'in den Häusern'), (4, 'er läuft'), (5, 'wir laufen')",
            10,
            "WITH (stemmer = 'de')",
        );
        assert_eq!(ids("stem_de", "häuser"), [1, 2, 3]);
        assert_eq!(ids("stem_de", "haus"), [1, 2, 3]);
        assert_eq!(ids("stem_de", "läuft"), [4]);
        corpus(
            "stem_ru",
            "(1, 'кошка спит'), (2, 'две кошки'), (3, 'с кошкой'), (4, 'кот')",
            10,
            "WITH (stemmer = 'ru')",
        );
        assert_eq!(ids("stem_ru", "кошку"), [1, 2, 3]);
        assert_eq!(ids("stem_ru", "кот"), [4]);
    }

    /// Not TIN's answers: Stannum persists an index's tokenizer settings at
    /// build, so a changed stemmer takes effect at REINDEX, never between
    /// (TIN 1.0.4 stems queries at once against terms stored the old way:
    /// `stemming.ddl.alter_reindex`). Indexes built without the option are
    /// unchanged by it.
    #[pg_test]
    fn a_changed_stemmer_takes_effect_at_reindex() {
        corpus(
            "stem_alter",
            "(1, 'running fast'), (2, 'runs slowly'), (3, 'I run')",
            10,
            "",
        );
        let both = || (ids("stem_alter", "run"), ids("stem_alter", "running"));
        assert_eq!(both(), (vec![3], vec![1]));
        Spi::run("ALTER INDEX stem_alter_idx SET (stemmer = 'en')").unwrap();
        assert_eq!(both(), (vec![3], vec![1]));
        Spi::run("REINDEX INDEX stem_alter_idx").unwrap();
        assert_eq!(both(), (vec![1, 2, 3], vec![1, 2, 3]));
        Spi::run("ALTER INDEX stem_alter_idx RESET (stemmer)").unwrap();
        assert_eq!(both(), (vec![1, 2, 3], vec![1, 2, 3]));
        Spi::run("REINDEX INDEX stem_alter_idx").unwrap();
        assert_eq!(both(), (vec![3], vec![1]));
    }
}
