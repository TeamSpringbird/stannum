// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! SQL functions: `stannum.promote()` and `stannum.merge()` with TIN's
//! signatures, run in the calling session, and two views of the queue.

use pgrx::iter::TableIterator;
use pgrx::{PgRelation, PgSqlErrorCode, default, name, pg_extern, pg_sys};

use super::{QUEUE, preloaded};
use crate::maintenance::ops;
use crate::udfs::validate_stannum_index;

/// Raises an invalid-parameter ERROR.
fn invalid(message: &str) -> ! {
    pgrx::ereport!(
        pgrx::PgLogLevel::ERROR,
        PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
        message.to_owned()
    );
    unreachable!()
}

/// Checks that the caller may maintain the index's table (owner, `MAINTAIN`
/// or `pg_maintain`, as for VACUUM and REINDEX), that the server takes
/// writes, and takes the locks an insert takes: the table, then the index,
/// in `RowExclusiveLock`.
fn prepare(index: &PgRelation, function: &str) {
    validate_stannum_index(index, function);
    unsafe {
        let heap = pg_sys::IndexGetRelation(index.oid(), false);
        let acl = pg_sys::pg_class_aclcheck(heap, pg_sys::GetUserId(), pg_sys::ACL_MAINTAIN as _);
        if acl != pg_sys::AclResult::ACLCHECK_OK {
            pg_sys::aclcheck_error(
                acl,
                pg_sys::ObjectType::OBJECT_TABLE,
                pg_sys::get_rel_name(heap),
            );
        }
        if pg_sys::RecoveryInProgress() {
            pgrx::ereport!(
                pgrx::PgLogLevel::ERROR,
                PgSqlErrorCode::ERRCODE_READ_ONLY_SQL_TRANSACTION,
                format!("cannot execute stannum.{function}() during recovery")
            );
        }
        let mode = pg_sys::RowExclusiveLock as pg_sys::LOCKMODE;
        pg_sys::LockRelationOid(heap, mode);
        pg_sys::LockRelationOid(index.oid(), mode);
    }
}

/// Promotes the index's sealed write segments into immutable segments now
/// and merges nothing; TIN's `promote()`. The write buffer, TIN's mutable
/// write segment, is left alone until it fills and is sealed. Returns one
/// row: the sealed segments consumed, the immutable segments linked, the
/// documents promoted and their distinct terms. With `extent_cap_bytes`
/// (positive) each sealed segment is split into segments of about that many
/// bytes of forward records, as many as the directory has room for.
#[pg_extern(volatile, parallel_unsafe)]
#[allow(clippy::type_complexity)]
fn promote(
    index: PgRelation,
    extent_cap_bytes: default!(Option<i64>, "NULL"),
) -> TableIterator<
    'static,
    (
        name!(consumed_controls, i32),
        name!(linked_segments, i32),
        name!(docs_promoted, i64),
        name!(terms_added, i64),
    ),
> {
    if extent_cap_bytes.is_some_and(|cap| cap <= 0) {
        // TIN's message and SQLSTATE (XX000).
        pgrx::error!("extent_cap_bytes must be positive");
    }
    prepare(&index, "promote");
    let report = if unsafe { crate::storage::present(index.as_ptr()) } {
        unsafe { ops::promote(index.as_ptr(), extent_cap_bytes.map(|cap| cap as u64)) }
    } else {
        ops::FoldReport::default()
    };
    TableIterator::once((
        report.consumed as i32,
        report.segments as i32,
        report.docs as i64,
        report.terms as i64,
    ))
}

/// Merges the index's smallest segments until it holds
/// `target_segment_count` (default: its target, else `stannum.max_segments`),
/// at most `max_fan_in` at a time (default: no limit), when it holds more
/// than `target_segment_count * high_water_multiplier` (default 1) segments
/// or `force` is true; TIN's `merge()`. Runs in this session, as VACUUM's
/// merges do: built without the index's metadata lock and published only if
/// the inputs are unchanged. Returns one row; `no_op_reason` says why
/// nothing was merged, when nothing was.
#[pg_extern(volatile, parallel_unsafe)]
#[allow(clippy::type_complexity)]
fn merge(
    index: PgRelation,
    target_segment_count: default!(Option<i32>, "NULL"),
    high_water_multiplier: default!(Option<i32>, "NULL"),
    max_fan_in: default!(Option<i32>, "NULL"),
    force: default!(Option<bool>, "NULL"),
) -> TableIterator<
    'static,
    (
        name!(considered_segments, i32),
        name!(retired_only_segments, i32),
        name!(merged_segments, i32),
        name!(linked_segments, i32),
        name!(output_docs, i64),
        name!(output_postings, i64),
        name!(replayed_kills, i64),
        name!(no_op_reason, Option<String>),
    ),
> {
    if target_segment_count.is_some_and(|n| !(1..=4096).contains(&n)) {
        invalid("target_segment_count must be between 1 and 4096");
    }
    if high_water_multiplier.is_some_and(|n| n < 1) {
        invalid("high_water_multiplier must be at least 1");
    }
    if max_fan_in.is_some_and(|n| n < 2) {
        invalid("max_fan_in must be at least 2");
    }
    prepare(&index, "merge");
    let report = if unsafe { crate::storage::present(index.as_ptr()) } {
        let request = ops::MergeRequest {
            target: target_segment_count.map(|n| n as usize),
            high_water: high_water_multiplier.map(|n| n as usize),
            max_fan_in: max_fan_in.map(|n| n as usize),
            force: force.unwrap_or(false),
        };
        unsafe { ops::merge(index.as_ptr(), request) }
    } else {
        ops::MergeReport {
            no_op_reason: Some("at or below target_segment_count"),
            ..ops::MergeReport::default()
        }
    };
    TableIterator::once((
        report.considered as i32,
        report.retired_only as i32,
        report.merged as i32,
        report.linked as i32,
        report.output_docs as i64,
        report.output_postings as i64,
        report.replayed_kills as i64,
        report.no_op_reason.map(str::to_owned),
    ))
}

/// The maintenance launcher, worker and queue, as one row. Without
/// `shared_preload_libraries = 'stannum'` there are none: `preloaded` is
/// false and the rest is NULL.
#[pg_extern(volatile, parallel_safe)]
#[allow(clippy::type_complexity)]
fn maintenance_status() -> TableIterator<
    'static,
    (
        name!(preloaded, bool),
        name!(launcher_pid, Option<i32>),
        name!(worker_pid, Option<i32>),
        name!(worker_database, Option<pg_sys::Oid>),
        name!(queued_jobs, Option<i64>),
        name!(running_jobs, Option<i64>),
        name!(requested, Option<i64>),
        name!(completed, Option<i64>),
        name!(abandoned, Option<i64>),
        name!(inline_fallbacks, Option<i64>),
        name!(launches, Option<i64>),
        name!(launch_failures, Option<i64>),
    ),
> {
    if !preloaded() {
        return TableIterator::once((
            false, None, None, None, None, None, None, None, None, None, None, None,
        ));
    }
    let queue = *QUEUE.share();
    let jobs = queue.jobs();
    let running = jobs.iter().filter(|(_, running)| *running).count() as i64;
    let worker = queue.worker;
    let attached = worker.pid != 0;
    let counters = queue.counters;
    TableIterator::once((
        true,
        (queue.launcher_pid != 0).then_some(queue.launcher_pid),
        attached.then_some(worker.pid),
        attached.then_some(pg_sys::Oid::from(worker.database)),
        Some(jobs.len() as i64 - running),
        Some(running),
        Some(counters.requested as i64),
        Some(counters.completed as i64),
        Some(counters.abandoned as i64),
        Some(counters.inline as i64),
        Some(counters.launches as i64),
        Some(counters.launch_failures as i64),
    ))
}

/// The queued and running maintenance jobs, oldest first, across every
/// database: `index` is an OID in `database`.
#[pg_extern(volatile, parallel_safe)]
#[allow(clippy::type_complexity)]
fn maintenance_jobs() -> TableIterator<
    'static,
    (
        name!(database, pg_sys::Oid),
        name!(index, pg_sys::Oid),
        name!(kinds, Vec<String>),
        name!(state, String),
    ),
> {
    if !preloaded() {
        return TableIterator::new(Vec::new());
    }
    let jobs = QUEUE.share().jobs();
    TableIterator::new(jobs.into_iter().map(|(job, running)| {
        (
            pg_sys::Oid::from(job.database),
            pg_sys::Oid::from(job.index),
            kind_names(job.kinds),
            if running { "running" } else { "queued" }.to_owned(),
        )
    }))
}

fn kind_names(kinds: u8) -> Vec<String> {
    use super::queue::kind;
    [
        (kind::MERGE, "merge"),
        (kind::REWRITE, "rewrite"),
        (kind::RECLAIM, "reclaim"),
        (kind::PROMOTE, "promote"),
    ]
    .into_iter()
    .filter(|(bit, _)| kinds & bit != 0)
    .map(|(_, name)| name.to_owned())
    .collect()
}

/// pg_test builds only: applies `stannum.debug_maintenance_race` to the
/// worker's next job.
#[cfg(feature = "pg_test")]
pub(super) fn arm_worker_race() {
    let Some(setting) = super::WORKER_RACE.get() else {
        crate::storage::testing::set_race_hook(None);
        return;
    };
    let setting = setting.to_string_lossy().into_owned();
    let Some((action, at)) = setting.split_once(':') else {
        return;
    };
    let (action, at) = (action.to_owned(), at.to_owned());
    let mut fired = false;
    crate::storage::testing::set_race_hook(Some(Box::new(move |name| {
        if fired || name != at {
            return;
        }
        fired = true;
        match action.as_str() {
            "crash" => {
                unsafe { pg_sys::XLogFlush(pg_sys::GetXLogInsertRecPtr()) };
                pgrx::ereport!(
                    pgrx::PgLogLevel::PANIC,
                    PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
                    format!("crash injected at race point {name} in a maintenance worker")
                );
            }
            "terminate" => {
                pgrx::log!("maintenance worker terminating itself at race point {name}");
                unsafe {
                    pg_sys::ProcDiePending = 1;
                    pg_sys::InterruptPending = 1;
                }
            }
            _ => {}
        }
    })));
}
