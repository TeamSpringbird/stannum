// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Faults occur after real temporary I/O, including after an unpublished index
//! page is written. These are transaction-abort tests, not crash-restart tests.
mod spill_recovery {
    use pgrx::prelude::*;
    use std::cell::Cell;
    use std::rc::Rc;

    fn temp_files() -> i64 {
        Spi::get_one::<i64>(
            "SELECT count(*) FROM pg_ls_tmpdir() WHERE name LIKE
             'pgsql_tmp' || pg_backend_pid() || '.%'",
        )
        .unwrap()
        .unwrap()
    }

    fn recover_fault(point: &'static str, code: pgrx::PgSqlErrorCode, condition: &str) {
        Spi::run(
            "CREATE TABLE spill_recovery_rows(id int, body text);
             CREATE INDEX spill_recovery_idx ON spill_recovery_rows USING stannum(body);
             SET LOCAL stannum.experimental_merge_output_kb=1;
             SET LOCAL stannum.write_buffer_docs=1;
             SET LOCAL stannum.merge_tier_factor=2;
             SET LOCAL stannum.max_merge_docs=1024;
             INSERT INTO spill_recovery_rows VALUES
                (1,repeat('needle first ',2000)),(2,repeat('needle second ',2000));
             SET LOCAL enable_seqscan=off;
             SET LOCAL stannum.enable_custom_scan=off;",
        )
        .unwrap();
        let initial_files = temp_files();
        // Repeat to catch leaked temporary files across subtransaction aborts.
        for _ in 0..3 {
            let fired = Rc::new(Cell::new(false));
            let observed = fired.clone();
            crate::storage::testing::set_race_hook(Some(Box::new(move |name| {
                if name == point && !observed.replace(true) {
                    assert!(temp_files() > initial_files, "fault must follow real spill");
                    pgrx::ereport!(pgrx::PgLogLevel::ERROR, code, "injected spill failure");
                }
            })));
            Spi::run(&format!(
                "DO $$BEGIN
                    INSERT INTO spill_recovery_rows VALUES (3,repeat('needle third ',2000));
                    RAISE EXCEPTION 'spill fault did not fire';
                 EXCEPTION WHEN {condition} THEN NULL;
                 END$$;"
            ))
            .unwrap();
            crate::storage::testing::set_race_hook(None);
            assert!(fired.get(), "missing checkpoint {point}");
            assert_eq!(temp_files(), initial_files, "aborted spill leaked files");
            assert_eq!(
                Spi::get_one::<i64>(
                    "SELECT sum(id)::bigint FROM spill_recovery_rows WHERE body ==> 'needle'"
                )
                .unwrap(),
                Some(3)
            );
            assert_eq!(
                Spi::get_one::<i64>(
                    "SELECT sum(docs)::bigint FROM stannum.segment_info('spill_recovery_idx')"
                )
                .unwrap(),
                Some(2)
            );
        }
        Spi::run("INSERT INTO spill_recovery_rows VALUES (3,repeat('needle third ',2000));")
            .unwrap();
        assert_eq!(temp_files(), initial_files, "successful spill leaked files");
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT sum(id)::bigint FROM spill_recovery_rows WHERE body ==> 'needle THEN/0 third'"
            )
            .unwrap(),
            Some(3)
        );
        // Aborted publication may leave unreferenced runs. Exercise existing
        // orphan reclamation directly; this does not enable spilling in VACUUM.
        let index = unsafe { pgrx::PgRelation::open_with_name("spill_recovery_idx") }.unwrap();
        unsafe { crate::storage::cleanup(index.as_ptr()) };
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM stannum.verify_index('spill_recovery_idx', true)"
            )
            .unwrap(),
            Some(0)
        );
    }

    #[pg_test(schema = "tests")]
    fn spill_write_error_rolls_back_cleans_up_and_retries() {
        recover_fault(
            "spill:written",
            pgrx::PgSqlErrorCode::ERRCODE_IO_ERROR,
            "io_error",
        );
    }

    #[pg_test(schema = "tests")]
    fn spill_read_error_rolls_back_cleans_up_and_retries() {
        recover_fault(
            "spill:read",
            pgrx::PgSqlErrorCode::ERRCODE_IO_ERROR,
            "io_error",
        );
    }

    #[pg_test(schema = "tests")]
    fn spill_cancellation_after_spilling_cleans_up_and_retries() {
        recover_fault(
            "spill:ready",
            pgrx::PgSqlErrorCode::ERRCODE_QUERY_CANCELED,
            "query_canceled",
        );
    }

    #[pg_test(schema = "tests")]
    fn spill_partial_publication_rolls_back_and_reclaims_orphans() {
        recover_fault(
            "spill:page-written",
            pgrx::PgSqlErrorCode::ERRCODE_IO_ERROR,
            "io_error",
        );
    }
}
