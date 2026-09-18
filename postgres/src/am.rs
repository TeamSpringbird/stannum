use boldi_vigna::SpanQuery;
use pgrx::{FromDatum, PgBox, PgMemoryContexts, pg_extern, pg_guard, pg_sys};
use std::ffi::c_void;
use tinql::runtime::{Query, SpanTermSlot};

#[pg_extern(sql = "
    CREATE OR REPLACE FUNCTION @extschema@.amhandler(internal)
        RETURNS index_am_handler
        PARALLEL SAFE IMMUTABLE STRICT
        LANGUAGE c AS 'MODULE_PATHNAME', '@FUNCTION_NAME@';
    CREATE ACCESS METHOD tin TYPE INDEX HANDLER @extschema@.amhandler;
")]
pub(crate) fn amhandler(_fcinfo: pg_sys::FunctionCallInfo) -> PgBox<pg_sys::IndexAmRoutine> {
    let mut routine =
        unsafe { PgBox::<pg_sys::IndexAmRoutine>::alloc_node(pg_sys::NodeTag::T_IndexAmRoutine) };
    routine.amstrategies = 1;
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

#[pg_guard]
unsafe extern "C-unwind" fn ambuild(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    unsafe { crate::postings::build_empty(index) };
    let mut index_tuples = 0_u64;
    let heap_tuples = unsafe {
        pg_sys::table_index_build_scan(
            heap,
            index,
            index_info,
            true,
            true,
            Some(build_callback),
            (&mut index_tuples as *mut u64).cast(),
            std::ptr::null_mut(),
        )
    };
    let mut result = unsafe { PgBox::<pg_sys::IndexBuildResult>::alloc0() };
    result.heap_tuples = heap_tuples;
    result.index_tuples = index_tuples as f64;
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
        crate::postings::insert(index, values, isnull, tid);
        *state.cast::<u64>() += 1;
    };
}

#[pg_guard]
unsafe extern "C-unwind" fn ambuildempty(_index: pg_sys::Relation) {}

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
    unsafe { crate::postings::insert(index, values, isnull, heap_tid) };
    false
}

/// Conservative candidate supersets, before visibility and exact heap rechecks.
/// `All` is an explicit fallback, while `Empty` is a proven candidate miss.
#[derive(Debug, PartialEq, Eq)]
enum CandidatePlan {
    All,
    Empty,
    Term(String),
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl CandidatePlan {
    // Bound both recursive execution depth and the number of temporary bitmaps.
    // An over-budget query retains the complete reference path.
    const NODE_BUDGET: usize = 128;

    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Empty, _) | (_, Self::Empty) => Self::Empty,
            (Self::All, plan) | (plan, Self::All) => plan,
            (a, b) => Self::And(Box::new(a), Box::new(b)),
        }
    }

    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Self::All,
            (Self::Empty, plan) | (plan, Self::Empty) => plan,
            (a, b) => Self::Or(Box::new(a), Box::new(b)),
        }
    }

    fn consume(budget: &mut usize) -> Result<(), ()> {
        *budget = budget.checked_sub(1).ok_or(())?;
        Ok(())
    }

    fn query(query: &Query, budget: &mut usize) -> Result<Self, ()> {
        Self::consume(budget)?;
        Ok(match query {
            Query::Term(term) => Self::Term(term.clone()),
            Query::And(a, b) => Self::query(a, budget)?.and(Self::query(b, budget)?),
            Query::Or(a, b) => Self::query(a, budget)?.or(Self::query(b, budget)?),
            Query::Conjunction(children) => {
                children.iter().try_fold(Self::All, |plan, child| {
                    Ok::<_, ()>(plan.and(Self::query(child, budget)?))
                })?
            }
            Query::Disjunction { min: 1, children } | Query::AtLeast { min: 1, children } => {
                children.iter().try_fold(Self::Empty, |plan, child| {
                    Ok::<_, ()>(plan.or(Self::query(child, budget)?))
                })?
            }
            Query::Boost { inner, .. } => Self::query(inner, budget)?,
            Query::Span {
                term_slots,
                span_query,
                ..
            } => Self::span(span_query, term_slots, budget)?,
            // Never complement an approximate posting set. Expansion, threshold
            // and advanced span expressions remain the exact evaluator's job.
            Query::Not(_)
            | Query::MatchAll
            | Query::Regex(_)
            | Query::Range { .. }
            | Query::Fuzzy { .. }
            | Query::Disjunction { .. }
            | Query::AtLeast { .. }
            | Query::SpanExpr { .. } => Self::All,
        })
    }

    fn span(query: &SpanQuery, slots: &[SpanTermSlot], budget: &mut usize) -> Result<Self, ()> {
        Self::consume(budget)?;
        Ok(match query {
            SpanQuery::Empty => Self::Empty,
            SpanQuery::Term(i) => match slots.get(*i) {
                Some(SpanTermSlot::Term(term)) => Self::Term(term.clone()),
                _ => Self::All,
            },
            SpanQuery::Ordered(children) | SpanQuery::Unordered(children) => {
                children.iter().try_fold(Self::All, |plan, child| {
                    Ok::<_, ()>(plan.and(Self::span(child, slots, budget)?))
                })?
            }
            SpanQuery::Or(children) => children.iter().try_fold(Self::Empty, |plan, child| {
                Ok::<_, ()>(plan.or(Self::span(child, slots, budget)?))
            })?,
            SpanQuery::MaxGaps { inner, .. }
            | SpanQuery::GapsInRange { inner, .. }
            | SpanQuery::MaxWidth { inner, .. }
            | SpanQuery::WithinPositions { inner, .. } => Self::span(inner, slots, budget)?,
            // Negative positional relations require only the retained side.
            SpanQuery::NotContaining { big, .. } => Self::span(big, slots, budget)?,
            SpanQuery::NotContainedBy { little, .. } => Self::span(little, slots, budget)?,
            SpanQuery::NonOverlapping { a, .. } => Self::span(a, slots, budget)?,
            SpanQuery::Containing { big: a, little: b }
            | SpanQuery::ContainedBy { little: a, big: b }
            | SpanQuery::Overlapping { a, b }
            | SpanQuery::Before { a, b }
            | SpanQuery::After { a, b } => {
                Self::span(a, slots, budget)?.and(Self::span(b, slots, budget)?)
            }
        })
    }

    /// Number of concurrently live bitmaps under left-to-right evaluation.
    fn bitmap_slots(&self) -> usize {
        match self {
            Self::And(a, b) | Self::Or(a, b) => a.bitmap_slots().max(1 + b.bitmap_slots()),
            _ => 1,
        }
    }

    /// # Safety
    /// The caller holds a live LDP1 relation and a scratch memory context; all
    /// returned bitmap guards must be dropped before deleting that context.
    unsafe fn execute(&self, index: pg_sys::Relation, bytes: usize) -> CandidateBitmap {
        unsafe {
            pgrx::check_for_interrupts!();
            match self {
                Self::All => unreachable!("fallback plans are never materialized"),
                Self::Empty => CandidateBitmap::new(bytes),
                Self::Term(term) => {
                    let mut result = CandidateBitmap::new(bytes);
                    result.estimated_tuples = crate::postings::lookup(index, term, result.raw);
                    result
                }
                Self::And(a, b) | Self::Or(a, b) => {
                    let mut left = a.execute(index, bytes);
                    let right = b.execute(index, bytes);
                    if matches!(self, Self::And(..)) {
                        // PostgreSQL preserves conservative membership and
                        // recheck flags for both exact and lossy intersections.
                        pg_sys::tbm_intersect(left.raw, right.raw);
                        left.estimated_tuples = left.estimated_tuples.min(right.estimated_tuples);
                    } else {
                        pg_sys::tbm_union(left.raw, right.raw);
                        left.estimated_tuples =
                            left.estimated_tuples.saturating_add(right.estimated_tuples);
                    }
                    left
                }
            }
        }
    }
}

/// Private, non-shared TBM allocated inside the candidate scratch context.
struct CandidateBitmap {
    raw: *mut pg_sys::TIDBitmap,
    estimated_tuples: i64,
}
impl CandidateBitmap {
    unsafe fn new(bytes: usize) -> Self {
        Self {
            raw: unsafe { pg_sys::tbm_create(bytes, std::ptr::null_mut()) },
            estimated_tuples: 0,
        }
    }
}
impl Drop for CandidateBitmap {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns a TBM allocated in the enclosing
        // scratch context, which is alive throughout guard destruction.
        unsafe { pg_sys::tbm_free(self.raw) };
    }
}

struct ScanState {
    plan: CandidatePlan,
}

// The reset callback owns the outer holder. Normal end-of-scan takes its Box;
// ERROR/cancellation can skip amendscan, so context reset must own that path too.
type ScanOwner = Option<Box<ScanState>>;

fn new_scan_state() -> *mut ScanOwner {
    PgMemoryContexts::CurrentMemoryContext.leak_and_drop_on_delete(Some(Box::new(ScanState {
        plan: CandidatePlan::Empty,
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
    state.plan = CandidatePlan::Empty;
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
    let mut plan = CandidatePlan::All;
    let mut budget = CandidatePlan::NODE_BUDGET;
    for key in keys {
        if key.sk_flags != 0 {
            continue;
        }
        let text =
            unsafe { String::from_datum(key.sk_argument, false) }.expect("non-null search key");
        let query = tinql::runtime::parse_tinql_to_query_default(&text)
            .unwrap_or_else(|error| pgrx::error!("invalid ==> query: {error}"));
        let Ok(candidate) = CandidatePlan::query(&query, &mut budget) else {
            state.plan = CandidatePlan::All;
            return;
        };
        // Multiple scan keys are conjunctive; unknown keys keep exact rechecks.
        plan = plan.and(candidate);
    }
    state.plan = plan;
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
    if matches!(state.plan, CandidatePlan::Empty) {
        return 0;
    }
    let index = unsafe { (*scan).indexRelation };
    // Generic WAL does not encode index-VACUUM standby conflicts.
    // A replaying standby must use heap scans until that protocol exists.
    if !matches!(state.plan, CandidatePlan::All)
        && unsafe {
            !pg_sys::RecoveryInProgress()
                && !(*scan).xs_snapshot.is_null()
                && !(*(*scan).xs_snapshot).takenDuringRecovery
                && crate::postings::present(index)
        }
    {
        if let CandidatePlan::Term(term) = &state.plan {
            return unsafe { crate::postings::lookup(index, term, bitmap) };
        }
        // This context also reclaims allocations if a PostgreSQL error interrupts
        // construction before an owned bitmap can be returned.
        let mut scratch = PgMemoryContexts::new("Lead candidate bitmaps");
        let bytes = (unsafe { pg_sys::work_mem } as usize * 1024) / state.plan.bitmap_slots();
        // work_mem is a shared target, not a hard cap: PostgreSQL can exceed a
        // TBM target when even its lossy representation needs more space.
        let result = unsafe { scratch.switch_to(|_| state.plan.execute(index, bytes)) };
        unsafe { pg_sys::tbm_union(bitmap, result.raw) };
        let estimated_tuples = result.estimated_tuples;
        drop(result);
        drop(scratch);
        return estimated_tuples;
    }
    let heap_oid = unsafe { (*(*index).rd_index).indrelid };
    let heap = unsafe { pg_sys::table_open(heap_oid, pg_sys::NoLock as _) };
    let heap_blocks =
        unsafe { pg_sys::RelationGetNumberOfBlocksInFork(heap, pg_sys::ForkNumber::MAIN_FORKNUM) };
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
        if !crate::postings::present(index) {
            return stats;
        }
        let stats = if stats.is_null() {
            pg_sys::palloc0(std::mem::size_of::<pg_sys::IndexBulkDeleteResult>())
                .cast::<pg_sys::IndexBulkDeleteResult>()
        } else {
            stats
        };
        let (_, removed) = crate::postings::vacuum(index, callback, callback_state);
        // Physical term postings are not indexed rows. Use an explicitly marked
        // heap-row estimate instead of reporting term frequency as row count.
        (*stats).num_index_tuples = (*info).num_heap_tuples;
        (*stats).estimated_count = true;
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
    if !stats.is_null() {
        // Cleanup receives the final heap-row estimate; bulk-delete can run
        // multiple times with provisional counts during one VACUUM.
        unsafe {
            (*stats).num_index_tuples = (*info).num_heap_tuples.max(0.0);
            (*stats).estimated_count = true;
        }
    }
    stats
}

#[pg_guard]
#[expect(
    clippy::too_many_arguments,
    reason = "PostgreSQL index AM callback signature"
)]
unsafe extern "C-unwind" fn amcostestimate(
    _root: *mut pg_sys::PlannerInfo,
    path: *mut pg_sys::IndexPath,
    loop_count: f64,
    startup: *mut pg_sys::Cost,
    total: *mut pg_sys::Cost,
    selectivity: *mut pg_sys::Selectivity,
    correlation: *mut f64,
    pages: *mut f64,
) {
    let tuples = unsafe { (*(*(*path).indexinfo).rel).tuples.max(1.0) };
    unsafe {
        *startup = pg_sys::seq_page_cost * loop_count;
        *total = *startup + tuples * pg_sys::cpu_operator_cost * loop_count;
        *selectivity = 0.1;
        *correlation = 0.0;
        *pages = (tuples / 512.0).ceil();
    }
}

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use super::*;
    use pgrx::prelude::*;

    #[pg_test]
    fn candidate_plans_distinguish_fallback_and_misses_and_bound_complexity() {
        let compile = |text: &str| {
            let query = tinql::runtime::parse_tinql_to_query_default(text).unwrap();
            CandidatePlan::query(&query, &mut { CandidatePlan::NODE_BUDGET }).unwrap()
        };
        assert_eq!(compile("beer OR win*"), CandidatePlan::All);
        assert_eq!(compile("beer AND win*"), CandidatePlan::Term("beer".into()));
        assert_eq!(
            compile("beer AND NOT wine"),
            CandidatePlan::Term("beer".into())
        );
        let not = Query::Not(Box::new(Query::Term("beer".into())));
        assert_eq!(
            CandidatePlan::query(&not, &mut 4).unwrap(),
            CandidatePlan::All
        );
        assert_eq!(
            CandidatePlan::All.or(CandidatePlan::Empty),
            CandidatePlan::All
        );
        assert_eq!(
            CandidatePlan::All.and(CandidatePlan::Empty),
            CandidatePlan::Empty
        );
        let terms = vec![Query::Term("beer".into()); CandidatePlan::NODE_BUDGET];
        assert!(
            CandidatePlan::query(&Query::Conjunction(terms), &mut {
                CandidatePlan::NODE_BUDGET
            })
            .is_err()
        );
    }

    #[pg_test]
    fn scan_state_is_reclaimed_on_context_reset_or_normal_end() {
        use std::sync::atomic::Ordering;
        let before = DROPPED_SCAN_STATES.load(Ordering::Relaxed);
        let mut context = PgMemoryContexts::new("lead scan ownership regression");
        unsafe {
            context.switch_to(|_| {
                let owner = new_scan_state();
                (*owner).as_deref_mut().unwrap().plan =
                    CandidatePlan::Term("owned query".repeat(100));
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
