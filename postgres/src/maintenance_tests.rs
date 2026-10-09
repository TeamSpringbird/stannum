// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Maintenance SQL functions and settings in a server without
//! `shared_preload_libraries`, where every backend maintains inline. The
//! workers themselves run in a private cluster that preloads the library:
//! `postgres/tests/maintenance_workers.py` and, for crashes,
//! `postgres/tests/maintenance_worker_crash.py`.

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    /// Creates `sqlstate_of(statement)`: NULL when the statement succeeds,
    /// else `SQLSTATE: message`.
    fn create_sqlstate_of() {
        Spi::run(
            "CREATE FUNCTION sqlstate_of(statement text) RETURNS text LANGUAGE plpgsql AS $$
             BEGIN
                 EXECUTE statement;
                 RETURN NULL;
             EXCEPTION WHEN OTHERS THEN
                 RETURN SQLSTATE || ': ' || SQLERRM;
             END$$",
        )
        .unwrap();
    }

    fn text(sql: &str) -> String {
        Spi::get_one::<String>(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
            .unwrap_or_else(|| panic!("{sql}: NULL"))
    }

    fn count(sql: &str) -> i64 {
        Spi::get_one::<i64>(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
            .unwrap_or_else(|| panic!("{sql}: NULL"))
    }

    /// A table `name` with a stannum index `name_idx` and one segment per
    /// row of 1..=rows after the first (each insert seals the one before,
    /// and promote() makes the sealed ones immutable), merging nothing.
    fn one_segment_per_row(name: &str, rows: i32) {
        Spi::run(&format!(
            "CREATE TABLE {name}(id int, body text);
             CREATE INDEX {name}_idx ON {name} USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 1;
             SET LOCAL stannum.index_maintenance_mode = manual;"
        ))
        .unwrap();
        for id in 1..=rows {
            Spi::run(&format!("INSERT INTO {name} VALUES ({id}, 'needle w{id}')")).unwrap();
        }
        Spi::run(&format!("SELECT stannum.promote('{name}_idx')")).unwrap();
        Spi::run("RESET stannum.write_buffer_docs; RESET stannum.index_maintenance_mode").unwrap();
    }

    /// The directory as `kind:docs[:origin]` in segment_info's order.
    fn directory(index: &str) -> String {
        text(&format!(
            "SELECT coalesce(string_agg(kind || ':' || docs || coalesce(':' || origin, ''), ','
                 ORDER BY ordinal), '')
             FROM stannum.segment_info('{index}')"
        ))
    }

    /// score_inspect's terms with weights, elided terms left out.
    fn inspect(index: &str, query: &str) -> String {
        text(&format!(
            "SELECT coalesce(string_agg(term, ',' ORDER BY term), '')
             FROM stannum.score_inspect('{index}'::regclass, '{query}')"
        ))
    }

    fn segments(index: &str) -> i64 {
        count(&format!(
            "SELECT count(*) FROM stannum.segment_info('{index}') WHERE kind = 'immutable'"
        ))
    }

    /// TIN's signatures (pg_proc of TIN 1.0.3, conformance case
    /// catalog.S-07h and the hidden-function catalog): the argument names
    /// and types, and the result columns.
    #[pg_test]
    fn promote_and_merge_have_tins_signatures() {
        assert_eq!(
            text("SELECT pg_get_function_arguments('stannum.promote'::regproc)"),
            "index regclass, extent_cap_bytes bigint DEFAULT NULL::bigint"
        );
        assert_eq!(
            text("SELECT pg_get_function_result('stannum.promote'::regproc)"),
            "TABLE(consumed_controls integer, linked_segments integer, docs_promoted bigint, terms_added bigint)"
        );
        assert_eq!(
            text(
                "SELECT string_agg(name || ' ' || format_type(type, NULL), ', ' ORDER BY n)
                 FROM pg_proc, unnest(proargnames, proargtypes::oid[]) WITH ORDINALITY AS a(name, type, n)
                 WHERE oid = 'stannum.merge'::regproc"
            ),
            "index regclass, target_segment_count integer, high_water_multiplier integer, \
             max_fan_in integer, force boolean"
        );
        assert_eq!(
            text("SELECT pg_get_function_result('stannum.merge'::regproc)"),
            "TABLE(considered_segments integer, retired_only_segments integer, \
             merged_segments integer, linked_segments integer, output_docs bigint, \
             output_postings bigint, replayed_kills bigint, no_op_reason text)"
        );
    }

    /// promote() consumes sealed write segments only, as TIN's does: rows
    /// in the write segment stay mutable, and elision, which counts
    /// immutable segments only, still scores `w`, in every row, before and
    /// after (catalog.S-07).
    #[pg_test]
    fn promote_leaves_the_write_segment_alone() {
        Spi::run(
            "CREATE TABLE promoted(id int, body text);
             CREATE INDEX promoted_idx ON promoted USING stannum(body);
             INSERT INTO promoted SELECT n, 'w k' || n FROM generate_series(1, 20) n;",
        )
        .unwrap();
        let scores = || {
            text(
                "SELECT string_agg(stannum.score(ctid)::text || '/' || stannum.full_score(ctid)::text,
                     ',' ORDER BY id)
                 FROM promoted WHERE body ==> 'w' AND id <= 3",
            )
        };
        let before = scores();
        assert!(!before.starts_with("0/"), "{before}");
        assert_eq!(directory("promoted_idx"), "mutable:20");
        assert_eq!(inspect("promoted_idx", "w OR k1"), "k1,w");
        assert_eq!(
            text("SELECT row(p.*)::text FROM stannum.promote('promoted_idx') p"),
            "(0,0,0,0)"
        );
        assert_eq!(directory("promoted_idx"), "mutable:20");
        assert_eq!(scores(), before);
        assert_eq!(
            count("SELECT count(*) FROM promoted WHERE body ==> 'w AND k7'"),
            1
        );
    }

    /// A full write segment is sealed in place and, in manual mode, waits
    /// for promote(), which makes it immutable (origin promotion) and
    /// reports TIN's columns; searches see sealed rows throughout. Elision
    /// counts the promoted documents only: `a` (3 of 20, 15%) and `w` are
    /// elided after it, `b` (2 of 20, exactly 10%) and `e` (only in the
    /// write segment) are not (catalog.S-07f).
    #[pg_test]
    fn sealed_write_segments_wait_for_promote_in_manual_mode() {
        Spi::run(
            "CREATE TABLE sealing(id int, body text);
             CREATE INDEX sealing_idx ON sealing USING stannum(body);
             SET LOCAL stannum.index_maintenance_mode = manual;
             SET LOCAL stannum.write_buffer_docs = 10;
             INSERT INTO sealing SELECT n, 'w k' || n
                 || CASE WHEN n <= 2 THEN ' a b' WHEN n = 3 THEN ' a' WHEN n = 21 THEN ' e' ELSE '' END
                 FROM generate_series(1, 21) n;",
        )
        .unwrap();
        assert_eq!(directory("sealing_idx"), "mutable:1,sealed:10,sealed:10");
        assert_eq!(count("SELECT count(*) FROM sealing WHERE body ==> 'w'"), 21);
        assert_eq!(
            count("SELECT count(*) FROM sealing WHERE body ==> 'w AND k7'"),
            1
        );
        let query = "w OR a OR b OR e OR k1";
        assert_eq!(inspect("sealing_idx", query), "a,b,e,k1,w");
        assert_eq!(
            text("SELECT row(p.*)::text FROM stannum.promote('sealing_idx') p"),
            "(2,2,20,24)"
        );
        assert_eq!(
            directory("sealing_idx"),
            "immutable:10:promotion,immutable:10:promotion,mutable:1"
        );
        assert_eq!(inspect("sealing_idx", query), "b,e,k1");
        assert_eq!(
            text("SELECT row(p.*)::text FROM stannum.promote('sealing_idx') p"),
            "(0,0,0,0)"
        );
        assert_eq!(count("SELECT count(*) FROM sealing WHERE body ==> 'w'"), 21);
        assert_eq!(count("SELECT count(*) FROM sealing WHERE body ==> 'a'"), 3);
    }

    /// A third seal in manual mode promotes the oldest sealed segment
    /// first, as TIN does, so at most two wait.
    #[pg_test]
    fn a_third_seal_promotes_the_oldest_first() {
        Spi::run(
            "CREATE TABLE thirds(id int, body text);
             CREATE INDEX thirds_idx ON thirds USING stannum(body);
             SET LOCAL stannum.index_maintenance_mode = manual;
             SET LOCAL stannum.write_buffer_docs = 10;
             INSERT INTO thirds SELECT n, 'w k' || n FROM generate_series(1, 35) n;",
        )
        .unwrap();
        assert_eq!(
            directory("thirds_idx"),
            "immutable:10:promotion,mutable:5,sealed:10,sealed:10"
        );
        assert_eq!(count("SELECT count(*) FROM thirds WHERE body ==> 'w'"), 35);
    }

    /// Without workers to take it (this server does not preload the
    /// library) the inserting session promotes the segment it sealed.
    #[pg_test]
    fn a_sealed_write_segment_is_promoted_inline_without_workers() {
        Spi::run(
            "CREATE TABLE inline_seal(id int, body text);
             CREATE INDEX inline_seal_idx ON inline_seal USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 10;
             INSERT INTO inline_seal SELECT n, 'w k' || n FROM generate_series(1, 25) n;",
        )
        .unwrap();
        assert_eq!(
            directory("inline_seal_idx"),
            "immutable:10:promotion,immutable:10:promotion,mutable:5"
        );
        assert_eq!(
            count("SELECT count(*) FROM inline_seal WHERE body ==> 'w'"),
            25
        );
    }

    /// promote(index, extent_cap_bytes) splits each sealed segment into
    /// segments of about that many bytes of input; the smallest cap gives
    /// one document each here, terms_added counts distinct terms.
    #[pg_test]
    fn promote_splits_a_sealed_segment_by_its_extent_cap() {
        Spi::run(
            "CREATE TABLE extents(id int, body text);
             CREATE INDEX extents_idx ON extents USING stannum(body);
             SET LOCAL stannum.index_maintenance_mode = manual;
             SET LOCAL stannum.write_buffer_docs = 40;
             INSERT INTO extents SELECT n, 'w k' || n FROM generate_series(1, 41) n;",
        )
        .unwrap();
        assert_eq!(directory("extents_idx"), "mutable:1,sealed:40");
        assert_eq!(
            text("SELECT row(p.*)::text FROM stannum.promote('extents_idx', 1) p"),
            "(1,40,40,41)"
        );
        assert_eq!(
            count(
                "SELECT count(*) FROM stannum.segment_info('extents_idx')
                 WHERE kind = 'immutable' AND docs = 1 AND origin = 'promotion'"
            ),
            40
        );
        assert_eq!(count("SELECT count(*) FROM extents WHERE body ==> 'w'"), 41);
        // Uneven records under the smallest cap: never more segments than
        // the directory has room for (here 96 - 40 = 56), every one linked.
        Spi::run(
            "INSERT INTO extents SELECT n, 'w k' || n || repeat(' pad', n % 7)
                 FROM generate_series(42, 121) n;",
        )
        .unwrap();
        assert_eq!(
            text(
                "SELECT row(consumed_controls, linked_segments <= 56, docs_promoted)::text
                 FROM stannum.promote('extents_idx', 1) p"
            ),
            "(2,t,80)"
        );
        assert_eq!(
            count("SELECT count(*) FROM extents WHERE body ==> 'w'"),
            121
        );
    }

    /// TIN refuses a cap that is not positive, with this message (XX000).
    #[pg_test]
    fn promote_refuses_an_extent_cap_that_is_not_positive() {
        create_sqlstate_of();
        Spi::run(
            "CREATE TABLE capped(id int, body text);
             CREATE INDEX capped_idx ON capped USING stannum(body);",
        )
        .unwrap();
        for cap in ["0", "-1"] {
            assert_eq!(
                text(&format!(
                    "SELECT sqlstate_of('SELECT * FROM stannum.promote(''capped_idx'', {cap})')"
                )),
                "XX000: extent_cap_bytes must be positive"
            );
        }
        assert_eq!(
            text("SELECT row(p.*)::text FROM stannum.promote('capped_idx', 1) p"),
            "(0,0,0,0)"
        );
    }

    /// merge() merges the smallest segments down to the target and reports
    /// TIN's columns; at the target, or below the high-water mark without
    /// force, it says why it did nothing.
    #[pg_test]
    fn merge_merges_down_to_the_target_and_reports_it() {
        one_segment_per_row("merged", 10);
        assert_eq!(segments("merged_idx"), 9);
        assert_eq!(
            text("SELECT row(m.*)::text FROM stannum.merge('merged_idx', 3, 1, NULL, false) m"),
            // 9 entries, the 7 smallest merged into one: 7 documents of two
            // tokens each.
            "(9,0,7,1,7,14,0,)"
        );
        assert_eq!(segments("merged_idx"), 3);
        assert_eq!(
            text("SELECT no_op_reason FROM stannum.merge('merged_idx', 3, 1, NULL, false)"),
            "at or below target_segment_count"
        );
        assert_eq!(
            text("SELECT no_op_reason FROM stannum.merge('merged_idx', 2, 2, NULL, false)"),
            "below the high-water mark"
        );
        assert_eq!(segments("merged_idx"), 3);
        assert_eq!(
            text("SELECT row(m.*)::text FROM stannum.merge('merged_idx', 2, 2, NULL, true) m"),
            "(3,0,2,1,2,4,0,)"
        );
        assert_eq!(segments("merged_idx"), 2);
        assert_eq!(
            count("SELECT count(*) FROM merged WHERE body ==> 'needle'"),
            10
        );
        for id in [1, 5, 9, 10] {
            assert_eq!(
                count(&format!(
                    "SELECT count(*) FROM merged WHERE id = {id} AND body ==> 'w{id}'"
                )),
                1
            );
        }
    }

    /// max_fan_in bounds every merge: nine entries down to one, two at a
    /// time, is eight merges.
    #[pg_test]
    fn merge_takes_at_most_max_fan_in_segments_at_a_time() {
        one_segment_per_row("fanned", 10);
        assert_eq!(
            text(
                "SELECT row(considered_segments, merged_segments, linked_segments, output_docs)::text
                 FROM stannum.merge('fanned_idx', 1, 1, 2, false)"
            ),
            // Documents written: 2+2+2+2, then 3, 4, 5 and 9.
            "(9,16,8,29)"
        );
        assert_eq!(segments("fanned_idx"), 1);
    }

    /// Dead documents a merge drops are its replayed kills; inputs with no
    /// live document leave without a successor.
    #[pg_test]
    fn merge_reports_the_dead_documents_it_drops() {
        one_segment_per_row("killed", 6);
        assert_eq!(segments("killed_idx"), 5);
        // Rows 1-3 are dead in the index's dead lists; rows 1 and 2 are
        // whole segments.
        Spi::run(
            "SELECT tests.direct_bulk_delete('killed_idx'::regclass::oid,
                 (SELECT array_agg(ctid::text) FROM killed WHERE id <= 3));
             DELETE FROM killed WHERE id <= 3;",
        )
        .unwrap();
        assert_eq!(
            text(
                "SELECT row(merged_segments + retired_only_segments, output_docs, replayed_kills)::text
                 FROM stannum.merge('killed_idx', 1, 1, NULL, false)"
            ),
            "(5,2,3)"
        );
        assert_eq!(
            count("SELECT count(*) FROM killed WHERE body ==> 'needle'"),
            3
        );
    }

    #[pg_test]
    fn merge_refuses_arguments_out_of_range() {
        create_sqlstate_of();
        Spi::run(
            "CREATE TABLE refused(id int, body text);
             CREATE INDEX refused_idx ON refused USING stannum(body);",
        )
        .unwrap();
        for (arguments, message) in [
            ("0", "target_segment_count must be between 1 and 4096"),
            ("4097", "target_segment_count must be between 1 and 4096"),
            ("2, 0", "high_water_multiplier must be at least 1"),
            ("2, 1, 1", "max_fan_in must be at least 2"),
        ] {
            assert_eq!(
                text(&format!(
                    "SELECT sqlstate_of('SELECT * FROM stannum.merge(''refused_idx'', {arguments})')"
                )),
                format!("22023: {message}")
            );
        }
        assert_eq!(
            text("SELECT no_op_reason FROM stannum.merge('refused_idx')"),
            "at or below target_segment_count"
        );
    }

    /// Only a role that may maintain the table may promote or merge its index.
    #[pg_test]
    fn promote_and_merge_require_the_maintain_privilege() {
        create_sqlstate_of();
        Spi::run(
            "CREATE TABLE guarded(id int, body text);
             CREATE INDEX guarded_idx ON guarded USING stannum(body);
             CREATE ROLE maintenance_outsider;
             GRANT SELECT ON guarded TO maintenance_outsider;
             GRANT USAGE ON SCHEMA stannum, tests TO maintenance_outsider;
             SET ROLE maintenance_outsider;",
        )
        .unwrap();
        for call in [
            "SELECT * FROM stannum.promote(''guarded_idx'')",
            "SELECT * FROM stannum.merge(''guarded_idx'')",
        ] {
            assert_eq!(
                text(&format!("SELECT sqlstate_of('{call}')")),
                "42501: permission denied for table guarded"
            );
        }
        Spi::run("RESET ROLE; GRANT MAINTAIN ON guarded TO maintenance_outsider; SET ROLE maintenance_outsider")
            .unwrap();
        assert_eq!(
            text("SELECT row(p.*)::text FROM stannum.promote('guarded_idx') p"),
            "(0,0,0,0)"
        );
        Spi::run("RESET ROLE").unwrap();
    }

    /// TIN's maintenance_jobs_per_db takes effect on reload, never in a
    /// session (catalog.I-07: SQLSTATE 55P02).
    #[pg_test]
    fn maintenance_jobs_per_db_cannot_be_set_in_a_session() {
        create_sqlstate_of();
        assert_eq!(text("SHOW stannum.maintenance_jobs_per_db"), "0");
        assert_eq!(
            text("SELECT sqlstate_of('SET stannum.maintenance_jobs_per_db = 1')"),
            "55P02: parameter \"stannum.maintenance_jobs_per_db\" cannot be changed now"
        );
        assert_eq!(
            text("SELECT context FROM pg_settings WHERE name = 'stannum.maintenance_jobs_per_db'"),
            "sighup"
        );
    }

    /// TIN's three modes; background is the default.
    #[pg_test]
    fn index_maintenance_mode_takes_tins_values() {
        create_sqlstate_of();
        assert_eq!(text("SHOW stannum.index_maintenance_mode"), "background");
        assert_eq!(
            text(
                "SELECT array_to_string(enumvals, ',') FROM pg_settings
                 WHERE name = 'stannum.index_maintenance_mode'"
            ),
            "background,foreground,manual"
        );
        assert!(
            text("SELECT sqlstate_of('SET stannum.index_maintenance_mode = sometimes')")
                .starts_with("22023: ")
        );
    }

    /// Without workers, background mode maintains inline as foreground mode
    /// does, and manual mode leaves the merges to VACUUM and merge().
    #[pg_test]
    fn without_workers_background_merges_inline_and_manual_does_not() {
        for (mode, expected) in [
            ("background", true),
            ("foreground", true),
            ("manual", false),
        ] {
            let name = format!("mode_{mode}");
            Spi::run(&format!(
                "CREATE TABLE {name}(id int, body text);
                 CREATE INDEX {name}_idx ON {name} USING stannum(body);
                 SET LOCAL stannum.write_buffer_docs = 1;
                 SET LOCAL stannum.merge_tier_factor = 2;
                 SET LOCAL stannum.max_merge_docs = 0;
                 SET LOCAL stannum.index_maintenance_mode = {mode};"
            ))
            .unwrap();
            for id in 1..=9 {
                Spi::run(&format!("INSERT INTO {name} VALUES ({id}, 'needle')")).unwrap();
            }
            // Eight seals of one document: unmerged, eight entries, sealed
            // or promoted (in manual mode at most two wait sealed, a third
            // seal promoting the oldest); merged two by two after each
            // promotion, as a deferred merge does, fewer.
            let merged = count(&format!(
                "SELECT count(*) FROM stannum.segment_info('{name}_idx')
                 WHERE kind IN ('immutable', 'sealed')"
            )) < 8;
            assert_eq!(merged, expected, "{mode}");
            assert_eq!(
                count(&format!(
                    "SELECT count(*) FROM {name} WHERE body ==> 'needle'"
                )),
                9
            );
        }
        Spi::run(
            "RESET stannum.write_buffer_docs; RESET stannum.merge_tier_factor;
             RESET stannum.max_merge_docs; RESET stannum.index_maintenance_mode",
        )
        .unwrap();
    }

    /// Without `shared_preload_libraries` there is no queue to report.
    #[pg_test]
    fn without_preload_there_are_no_workers_or_jobs() {
        assert_eq!(
            text(
                "SELECT row(preloaded, launcher_pid, worker_pid, queued_jobs)::text
                 FROM stannum.maintenance_status()"
            ),
            "(f,,,)"
        );
        assert_eq!(count("SELECT count(*) FROM stannum.maintenance_jobs()"), 0);
    }

    /// segment_info carries TIN's columns: npostings (one per term and
    /// document) and origin for segments, NULL for the write buffer as for
    /// TIN's mutable segment; every listed entry is current; sequence counts
    /// the immutable segments from 0. A merge's output is of origin merge.
    #[pg_test]
    fn segment_info_has_tins_extra_columns() {
        Spi::run(
            "CREATE TABLE described(id int, body text);
             INSERT INTO described SELECT n, 'w k' || n FROM generate_series(1, 20) n;
             CREATE INDEX described_idx ON described USING stannum(body);
             INSERT INTO described VALUES (21, 'w k21 extra');",
        )
        .unwrap();
        assert_eq!(
            text(
                "SELECT string_agg(row(kind, npostings, source_state, origin,
                     sequence)::text, ';' ORDER BY ordinal)
                 FROM stannum.segment_info('described_idx')"
            ),
            "(immutable,40,current,build,0);(mutable,,current,,)"
        );
        Spi::run(
            "SET LOCAL stannum.write_buffer_docs = 1;
             SET LOCAL stannum.index_maintenance_mode = manual;
             INSERT INTO described VALUES (22, 'w k22');",
        )
        .unwrap();
        assert_eq!(
            text(
                "SELECT string_agg(row(kind, docs, npostings, origin, sequence)::text, ';'
                     ORDER BY ordinal)
                 FROM stannum.segment_info('described_idx')"
            ),
            "(immutable,20,40,build,0);(mutable,1,,,);(sealed,1,,,)"
        );
        Spi::run("SELECT stannum.promote('described_idx'::regclass)").unwrap();
        Spi::run("SELECT stannum.merge('described_idx'::regclass, 1)").unwrap();
        assert_eq!(
            text(
                "SELECT string_agg(row(kind, docs, npostings, origin, sequence)::text, ';'
                     ORDER BY ordinal)
                 FROM stannum.segment_info('described_idx')"
            ),
            "(immutable,21,43,merge,0);(mutable,1,,,)"
        );
    }
}
