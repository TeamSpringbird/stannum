// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

use pgrx::{FromDatum, PgBox, PgMemoryContexts, pg_extern, pg_guard, pg_sys};
use std::ffi::c_void;
use tinql::runtime::Query;

/// Operator class strategies: `text ==> text` and `text ==> indexed_query`
/// (see [`crate::operator`]).
const BOUND_STRATEGY: u16 = 2;

/// Added to the cost of scanning an index for a clause bound to an index
/// with other tokenizer settings (PostgreSQL's own `disable_cost`).
const FOREIGN_CLAUSE_COST: f64 = 1.0e10;

#[pg_extern(sql = "
    CREATE OR REPLACE FUNCTION @extschema@.amhandler(internal)
        RETURNS index_am_handler
        PARALLEL SAFE IMMUTABLE STRICT
        LANGUAGE c AS 'MODULE_PATHNAME', '@FUNCTION_NAME@';
    CREATE ACCESS METHOD stannum TYPE INDEX HANDLER @extschema@.amhandler;
")]
pub(crate) fn amhandler(_fcinfo: pg_sys::FunctionCallInfo) -> PgBox<pg_sys::IndexAmRoutine> {
    let mut routine =
        unsafe { PgBox::<pg_sys::IndexAmRoutine>::alloc_node(pg_sys::NodeTag::T_IndexAmRoutine) };
    routine.amstrategies = BOUND_STRATEGY;
    routine.amsupport = 0;
    routine.amcanmulticol = false;
    routine.amsearcharray = false;
    routine.amkeytype = pg_sys::InvalidOid;
    routine.amvalidate = Some(amvalidate);
    routine.ambuild = Some(ambuild);
    routine.ambuildempty = Some(ambuildempty);
    routine.aminsert = Some(aminsert);
    routine.ambulkdelete = Some(ambulkdelete);
    routine.amvacuumcleanup = Some(amvacuumcleanup);
    routine.amcostestimate = Some(amcostestimate);
    routine.amoptions = Some(crate::options::amoptions);
    routine.ambeginscan = Some(ambeginscan);
    routine.amrescan = Some(amrescan);
    routine.amgetbitmap = Some(amgetbitmap);
    routine.amendscan = Some(amendscan);
    routine.into_pg_boxed()
}

#[pg_guard]
unsafe extern "C-unwind" fn amvalidate(_opclassoid: pg_sys::Oid) -> bool {
    true
}

struct BuildState {
    builder: crate::storage::Builder,
    tuples: u64,
}

#[pg_guard]
unsafe extern "C-unwind" fn ambuild(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    unsafe { crate::operator::warn_about_search_predicate(index) };
    unsafe { crate::storage::build_empty(index) };
    let mut state = BuildState {
        builder: unsafe { crate::storage::Builder::new(index) },
        tuples: 0,
    };
    let heap_tuples = unsafe {
        pg_sys::table_index_build_scan(
            heap,
            index,
            index_info,
            true,
            true,
            Some(build_callback),
            (&mut state as *mut BuildState).cast(),
            std::ptr::null_mut(),
        )
    };
    unsafe { state.builder.finish(index) };
    let mut result = unsafe { PgBox::<pg_sys::IndexBuildResult>::alloc0() };
    result.heap_tuples = heap_tuples;
    result.index_tuples = state.tuples as f64;
    result.into_pg_boxed().into_pg()
}

#[pg_guard]
unsafe extern "C-unwind" fn build_callback(
    index: pg_sys::Relation,
    tid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    _tuple_is_alive: bool,
    state: *mut c_void,
) {
    unsafe {
        let state = &mut *state.cast::<BuildState>();
        state.builder.add(index, values, isnull, tid);
        state.tuples += 1;
    };
}

#[pg_guard]
unsafe extern "C-unwind" fn ambuildempty(index: pg_sys::Relation) {
    unsafe { crate::storage::build_init_fork(index) };
}

#[pg_guard]
#[expect(
    clippy::too_many_arguments,
    reason = "PostgreSQL index AM callback signature"
)]
unsafe extern "C-unwind" fn aminsert(
    index: pg_sys::Relation,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    heap_tid: pg_sys::ItemPointer,
    _heap: pg_sys::Relation,
    _check_unique: pg_sys::IndexUniqueCheck::Type,
    _index_unchanged: bool,
    _index_info: *mut pg_sys::IndexInfo,
) -> bool {
    unsafe { crate::storage::insert(index, values, isnull, heap_tid) };
    false
}

/// What a rescan compiled from its scan keys.
enum ScanPlan {
    /// A NULL key: nothing can match.
    Inactive,
    /// Queries analyzed with the index's own tokenizer, one per key.
    Queries(Vec<Query>),
    /// The index has no LDP2 storage; every heap page is a candidate.
    Fallback,
}

struct ScanState {
    plan: ScanPlan,
}

// The reset callback owns the outer holder. Normal end-of-scan takes its Box;
// ERROR/cancellation can skip amendscan, so context reset must own that path too.
type ScanOwner = Option<Box<ScanState>>;

fn new_scan_state() -> *mut ScanOwner {
    PgMemoryContexts::CurrentMemoryContext.leak_and_drop_on_delete(Some(Box::new(ScanState {
        plan: ScanPlan::Inactive,
    })))
}

#[cfg(feature = "pg_test")]
static DROPPED_SCAN_STATES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(feature = "pg_test")]
impl Drop for ScanState {
    fn drop(&mut self) {
        DROPPED_SCAN_STATES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn ambeginscan(
    index: pg_sys::Relation,
    nkeys: i32,
    norderbys: i32,
) -> pg_sys::IndexScanDesc {
    let scan = unsafe { pg_sys::RelationGetIndexScan(index, nkeys, norderbys) };
    unsafe {
        (*scan).opaque = new_scan_state().cast();
    }
    scan
}

/// Selective retrieval needs LDP2 storage and, during recovery, the logged
/// removal horizons that make freeing pages a snapshot conflict on this
/// standby (see `storage::index_reads_allowed`); otherwise every heap page is
/// a candidate.
unsafe fn selective(scan: pg_sys::IndexScanDesc) -> bool {
    unsafe { crate::storage::index_reads_allowed((*scan).indexRelation) }
}

#[pg_guard]
unsafe extern "C-unwind" fn amrescan(
    scan: pg_sys::IndexScanDesc,
    keys: pg_sys::ScanKey,
    nkeys: i32,
    _orderbys: pg_sys::ScanKey,
    _norderbys: i32,
) {
    let state = unsafe {
        (&mut *(*scan).opaque.cast::<ScanOwner>())
            .as_deref_mut()
            .expect("active scan")
    };
    state.plan = ScanPlan::Inactive;
    // PostgreSQL may rescan with no replacement keys. Keep a copy in the scan
    // descriptor, as the built-in AMs do, and recompile its current arguments.
    if !keys.is_null() {
        assert_eq!(nkeys, unsafe { (*scan).numberOfKeys });
        unsafe { std::ptr::copy(keys, (*scan).keyData, nkeys as usize) };
    }
    let nkeys = unsafe { (*scan).numberOfKeys };
    if nkeys <= 0 {
        return;
    }
    let keys = unsafe { (*scan).keyData };
    if keys.is_null() {
        return;
    }
    let keys = unsafe { std::slice::from_raw_parts(keys, nkeys as usize) };
    if keys
        .iter()
        .any(|key| key.sk_flags & pg_sys::SK_ISNULL as i32 != 0)
    {
        return;
    }
    let selective = unsafe { selective(scan) };
    let spec = selective.then(|| unsafe { crate::storage::index_spec((*scan).indexRelation) });
    let tokenizer = spec.as_ref().map(crate::storage::tokenizer_for);
    let mut queries = Vec::with_capacity(keys.len());
    for key in keys {
        if key.sk_flags != 0 {
            // An unknown key flag keeps the full reference path.
            state.plan = ScanPlan::Fallback;
            return;
        }
        let text = if key.sk_strategy == BOUND_STRATEGY {
            let bound =
                unsafe { crate::operator::indexed_query::from_datum(key.sk_argument, false) }
                    .expect("non-null search key");
            // A query bound to an index with other settings than this one
            // can only be answered by rechecking every row with its own.
            if let Some(spec) = spec
                && unsafe { crate::storage::spec_by_oid(pg_sys::Oid::from(bound.index)) } != spec
            {
                state.plan = ScanPlan::Fallback;
                return;
            }
            bound.query
        } else {
            unsafe { String::from_datum(key.sk_argument, false) }.expect("non-null search key")
        };
        let query = match &tokenizer {
            Some(tokenizer) => tinql::runtime::parse_tinql_to_query(&text, tokenizer.as_ref()),
            None => tinql::runtime::parse_tinql_to_query_default(&text),
        }
        .unwrap_or_else(|error| pgrx::error!("invalid ==> query: {error}"));
        queries.push(query);
    }
    state.plan = if selective {
        ScanPlan::Queries(queries)
    } else {
        ScanPlan::Fallback
    };
}

#[pg_guard]
unsafe extern "C-unwind" fn amgetbitmap(
    scan: pg_sys::IndexScanDesc,
    bitmap: *mut pg_sys::TIDBitmap,
) -> i64 {
    let state = unsafe {
        (&*(*scan).opaque.cast::<ScanOwner>())
            .as_deref()
            .expect("active scan")
    };
    let index = unsafe { (*scan).indexRelation };
    match &state.plan {
        ScanPlan::Inactive => 0,
        ScanPlan::Queries(queries) => unsafe { crate::storage::scan(index, queries, bitmap) },
        ScanPlan::Fallback => {
            let heap_oid = unsafe { (*(*index).rd_index).indrelid };
            let heap = unsafe { pg_sys::table_open(heap_oid, pg_sys::NoLock as _) };
            let heap_blocks = unsafe {
                pg_sys::RelationGetNumberOfBlocksInFork(heap, pg_sys::ForkNumber::MAIN_FORKNUM)
            };
            unsafe { pg_sys::table_close(heap, pg_sys::NoLock as _) };
            // Lossy pages make PostgreSQL check every visible tuple against the
            // original query, including partial-index predicates and expressions.
            for block in 0..heap_blocks {
                pgrx::check_for_interrupts!();
                unsafe { pg_sys::tbm_add_page(bitmap, block) };
            }
            // Like BRIN, estimate ten tuples per page for scan statistics only.
            i64::from(heap_blocks) * 10
        }
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn amendscan(scan: pg_sys::IndexScanDesc) {
    let state = unsafe { (*scan).opaque.cast::<ScanOwner>() };
    if !state.is_null() {
        unsafe { (*state).take() };
        unsafe { (*scan).opaque = std::ptr::null_mut() };
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn ambulkdelete(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
    callback: pg_sys::IndexBulkDeleteCallback,
    callback_state: *mut c_void,
) -> *mut pg_sys::IndexBulkDeleteResult {
    unsafe {
        let index = (*info).index;
        if !crate::storage::present(index) {
            return stats;
        }
        let stats = if stats.is_null() {
            pg_sys::palloc0(std::mem::size_of::<pg_sys::IndexBulkDeleteResult>())
                .cast::<pg_sys::IndexBulkDeleteResult>()
        } else {
            stats
        };
        let (live, removed) = crate::storage::bulk_delete(index, callback, callback_state);
        (*stats).num_index_tuples = live as f64;
        (*stats).estimated_count = false;
        (*stats).tuples_removed += removed as f64;
        (*stats).num_pages =
            pg_sys::RelationGetNumberOfBlocksInFork(index, pg_sys::ForkNumber::MAIN_FORKNUM);
        stats
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn amvacuumcleanup(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
    unsafe {
        let index = (*info).index;
        if (*info).analyze_only || !crate::storage::present(index) {
            return stats;
        }
        crate::storage::cleanup(index);
        let stats = if stats.is_null() {
            pg_sys::palloc0(std::mem::size_of::<pg_sys::IndexBulkDeleteResult>())
                .cast::<pg_sys::IndexBulkDeleteResult>()
        } else {
            stats
        };
        (*stats).num_index_tuples = crate::storage::document_count(index) as f64;
        (*stats).estimated_count = false;
        (*stats).num_pages =
            pg_sys::RelationGetNumberOfBlocksInFork(index, pg_sys::ForkNumber::MAIN_FORKNUM);
        stats
    }
}

#[pg_guard]
#[expect(
    clippy::too_many_arguments,
    reason = "PostgreSQL index AM callback signature"
)]
unsafe extern "C-unwind" fn amcostestimate(
    root: *mut pg_sys::PlannerInfo,
    path: *mut pg_sys::IndexPath,
    loop_count: f64,
    startup: *mut pg_sys::Cost,
    total: *mut pg_sys::Cost,
    selectivity: *mut pg_sys::Selectivity,
    correlation: *mut f64,
    pages: *mut f64,
) {
    unsafe {
        let info = (*path).indexinfo;
        let tuples = (*(*info).rel).tuples.max(1.0);
        // Every ==> clause is estimated from the index; the scan ANDs them.
        let clauses = crate::selectivity::index_path_clauses(path);
        let estimates: Vec<_> = clauses
            .iter()
            .map(|clause| {
                clause
                    .query
                    .as_deref()
                    .and_then(|query| crate::selectivity::estimate_query((*info).indexoid, query))
                    .unwrap_or(crate::selectivity::FALLBACK)
            })
            .collect();
        let estimate = if estimates.is_empty() {
            crate::selectivity::FALLBACK
        } else {
            crate::selectivity::conjoin(&estimates)
        };
        let cost = crate::selectivity::index_cost(
            root,
            (*info).indexoid,
            (*info).reltablespace,
            &estimate,
            tuples,
            loop_count,
            clauses.len().max(1),
        );
        // A clause bound to an index with other tokenizer settings makes
        // this index scan a full recheck (see amrescan): every heap page is
        // a candidate.
        let own_spec = crate::storage::spec_by_oid((*info).indexoid);
        let foreign = clauses.iter().any(|clause| {
            clause
                .bound
                .is_some_and(|bound| crate::storage::spec_by_oid(bound) != own_spec)
        });
        *startup = cost.startup;
        // The bitmap heap scan fetches every candidate the index yields,
        // including the superset of an inexact plan. A foreign clause yields
        // every page, and is priced out so the bound index wins whenever it
        // is available.
        if foreign {
            *total = cost.total + FOREIGN_CLAUSE_COST;
            *selectivity = 1.0;
        } else {
            *total = cost.total;
            *selectivity = estimate.candidates.clamp(0.0, 1.0);
        }
        *correlation = 0.0;
        *pages = cost.pages;
    }
}

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use super::*;
    use pgrx::prelude::*;

    #[pg_test]
    fn scan_state_is_reclaimed_on_context_reset_or_normal_end() {
        use std::sync::atomic::Ordering;
        let before = DROPPED_SCAN_STATES.load(Ordering::Relaxed);
        let mut context = PgMemoryContexts::new("stannum scan ownership regression");
        unsafe {
            context.switch_to(|_| {
                let owner = new_scan_state();
                (*owner).as_deref_mut().unwrap().plan = ScanPlan::Fallback;
                // Simulate ERROR teardown: no amendscan, only context reset.
            });
            context.reset();
        }
        assert_eq!(DROPPED_SCAN_STATES.load(Ordering::Relaxed), before + 1);
        unsafe {
            context.switch_to(|_| {
                let mut scan = PgBox::<pg_sys::IndexScanDescData>::alloc0();
                scan.opaque = new_scan_state().cast();
                amendscan(scan.as_ptr());
                assert!(scan.opaque.is_null());
            });
            context.reset(); // Must not destroy the same state a second time.
        }
        assert_eq!(DROPPED_SCAN_STATES.load(Ordering::Relaxed), before + 2);
    }
}
