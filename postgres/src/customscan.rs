//! Custom scan paths for `==>` on a segmented index.
//!
//! Two nodes, mirroring what TIN exposes:
//!
//! * **Stannum Text Search Scan** replaces a scan of a base relation whose
//!   restrictions include `expr ==> 'query'` backed by a segmented `stannum` index.
//!   It plans the query against the index with the index's own tokenizer,
//!   fetches each visible tuple by TID, and evaluates any remaining quals. When
//!   the query orders by a bound score call on the same index, the node claims
//!   those path keys and emits rows in score order, so `LIMIT k` stops after k
//!   fetches instead of sorting every match.
//! * **Stannum Count** replaces `SELECT count(*)` over such a scan when the `==>`
//!   clause is the only restriction. Pages the visibility map marks all-visible
//!   are counted without a heap fetch.
//!
//! Both fall back to a plain heap scan evaluating the original clause when the
//! snapshot was taken during recovery, where selective index reads are not yet
//! safe. The bitmap index scan path remains available; `stannum.enable_custom_scan`
//! disables these nodes.

use std::ffi::{CStr, c_void};

use pgrx::{
    FromDatum, GucContext, GucFlags, GucRegistry, GucSetting, PgList, PgMemoryContexts, pg_guard,
    pg_sys,
};
use rustc_hash::FxHashSet;
use segment::Tid;
use segment::set::Cursor as _;
use tinql::runtime::Query;
use tinql::runtime::plan::{Limits, plan};

use crate::score::rank;

static ENABLE: GucSetting<bool> = GucSetting::<bool>::new(true);

/// Method tables hold C string pointers; they are immutable and never
/// touched off the backend's main thread.
struct Methods<T>(T);
unsafe impl<T> Sync for Methods<T> {}

static mut PREVIOUS_REL_HOOK: pg_sys::set_rel_pathlist_hook_type = None;
static mut PREVIOUS_UPPER_HOOK: pg_sys::create_upper_paths_hook_type = None;
static mut PREVIOUS_EXECUTOR_START: pg_sys::ExecutorStart_hook_type = None;

static SEARCH_PATH_METHODS: Methods<pg_sys::CustomPathMethods> =
    Methods(pg_sys::CustomPathMethods {
        CustomName: c"Stannum Text Search".as_ptr(),
        PlanCustomPath: Some(plan_search_path),
        ReparameterizeCustomPathByChild: None,
    });
static COUNT_PATH_METHODS: Methods<pg_sys::CustomPathMethods> =
    Methods(pg_sys::CustomPathMethods {
        CustomName: c"Stannum Count".as_ptr(),
        PlanCustomPath: Some(plan_count_path),
        ReparameterizeCustomPathByChild: None,
    });
static SEARCH_SCAN_METHODS: Methods<pg_sys::CustomScanMethods> =
    Methods(pg_sys::CustomScanMethods {
        CustomName: c"Stannum Text Search Scan".as_ptr(),
        CreateCustomScanState: Some(create_search_state),
    });
static COUNT_SCAN_METHODS: Methods<pg_sys::CustomScanMethods> =
    Methods(pg_sys::CustomScanMethods {
        CustomName: c"Stannum Count".as_ptr(),
        CreateCustomScanState: Some(create_count_state),
    });
static SEARCH_EXEC_METHODS: Methods<pg_sys::CustomExecMethods> =
    Methods(pg_sys::CustomExecMethods {
        CustomName: c"Stannum Text Search Scan".as_ptr(),
        BeginCustomScan: Some(begin_scan),
        ExecCustomScan: Some(exec_search),
        EndCustomScan: Some(end_scan),
        ReScanCustomScan: Some(rescan),
        MarkPosCustomScan: None,
        RestrPosCustomScan: None,
        EstimateDSMCustomScan: None,
        InitializeDSMCustomScan: None,
        ReInitializeDSMCustomScan: None,
        InitializeWorkerCustomScan: None,
        ShutdownCustomScan: None,
        ExplainCustomScan: Some(explain),
    });
static COUNT_EXEC_METHODS: Methods<pg_sys::CustomExecMethods> =
    Methods(pg_sys::CustomExecMethods {
        CustomName: c"Stannum Count".as_ptr(),
        BeginCustomScan: Some(begin_scan),
        ExecCustomScan: Some(exec_count),
        EndCustomScan: Some(end_scan),
        ReScanCustomScan: Some(rescan),
        MarkPosCustomScan: None,
        RestrPosCustomScan: None,
        EstimateDSMCustomScan: None,
        InitializeDSMCustomScan: None,
        ReInitializeDSMCustomScan: None,
        InitializeWorkerCustomScan: None,
        ShutdownCustomScan: None,
        ExplainCustomScan: Some(explain),
    });

pub fn init() {
    GucRegistry::define_bool_guc(
        c"stannum.enable_custom_scan",
        c"Enable Stannum's custom scan nodes for ==> queries",
        c"Off leaves the bitmap index scan path, which rechecks nothing either.",
        &ENABLE,
        GucContext::Userset,
        GucFlags::default(),
    );
    unsafe {
        PREVIOUS_REL_HOOK = pg_sys::set_rel_pathlist_hook;
        pg_sys::set_rel_pathlist_hook = Some(rel_pathlist_hook);
        PREVIOUS_UPPER_HOOK = pg_sys::create_upper_paths_hook;
        pg_sys::create_upper_paths_hook = Some(upper_paths_hook);
        PREVIOUS_EXECUTOR_START = pg_sys::ExecutorStart_hook;
        pg_sys::ExecutorStart_hook = Some(executor_start_hook);
        pg_sys::RegisterCustomScanMethods(&SEARCH_SCAN_METHODS.0);
        pg_sys::RegisterCustomScanMethods(&COUNT_SCAN_METHODS.0);
    }
}

/// Marks the start of an executor run so per-statement scorer state is not
/// carried into the next statement.
#[pg_guard]
unsafe extern "C-unwind" fn executor_start_hook(
    query_desc: *mut pg_sys::QueryDesc,
    eflags: std::ffi::c_int,
) {
    crate::score::note_executor_start();
    crate::selectivity::note_executor_start();
    unsafe {
        match PREVIOUS_EXECUTOR_START {
            Some(previous) => previous(query_desc, eflags),
            None => pg_sys::standard_ExecutorStart(query_desc, eflags),
        }
    }
}

// --- Planning -------------------------------------------------------------------

/// A C `char` as a byte; the type is `i8` on x86-64 and `u8` on AArch64.
fn byte(c: std::ffi::c_char) -> u8 {
    c.to_ne_bytes()[0]
}

/// Whether a path key asks for descending order (`ORDER BY ... DESC`).
/// PostgreSQL 18 replaced the btree strategy number with a compare type.
unsafe fn descending(pathkey: *mut pg_sys::PathKey) -> bool {
    #[cfg(feature = "pg18")]
    unsafe {
        (*pathkey).pk_cmptype == pg_sys::CompareType::COMPARE_GT
    }
    #[cfg(not(feature = "pg18"))]
    unsafe {
        (*pathkey).pk_strategy == pg_sys::BTGreaterStrategyNumber as i32
    }
}

/// What the path hook found: the `==>` clause and the index that answers it.
struct Match {
    clause: *mut pg_sys::OpExpr,
    index_oid: pg_sys::Oid,
    query: String,
}

/// Top-k ordering the scan can provide: a `score_bound_indexed` sort key.
struct Ordering {
    full: bool,
    dense_ratio: Option<f32>,
    k1: Option<f32>,
    b: Option<f32>,
    term_add: Option<Vec<String>>,
    term_replace: Option<Vec<String>>,
    /// Rows the query will consume (offset plus limit) when the planner
    /// knows; only that many are sorted up front.
    top_k: Option<usize>,
}

/// The text of a non-null `Const` node.
pub(crate) unsafe fn const_text(node: *mut pg_sys::Node) -> Option<String> {
    unsafe {
        if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_Const {
            return None;
        }
        let value = &*node.cast::<pg_sys::Const>();
        if value.constisnull || value.consttype != pg_sys::TEXTOID {
            return None;
        }
        String::from_datum(value.constvalue, false)
    }
}

unsafe fn const_datum<T: FromDatum>(node: *mut pg_sys::Node) -> Result<Option<T>, ()> {
    unsafe {
        if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_Const {
            return Err(());
        }
        let value = &*node.cast::<pg_sys::Const>();
        if value.constisnull {
            return Ok(None);
        }
        Ok(T::from_datum(value.constvalue, false))
    }
}

/// Finds a `expr ==> 'literal'` restriction on `rel` (in either operator
/// form) answered by a segmented stannum index whose tokenizer settings are
/// the ones the clause is bound to.
unsafe fn find_match(
    rel: *mut pg_sys::RelOptInfo,
    rte: *mut pg_sys::RangeTblEntry,
) -> Option<Match> {
    unsafe {
        let restrictions = PgList::<pg_sys::RestrictInfo>::from_pg((*rel).baserestrictinfo);
        for info in restrictions.iter_ptr() {
            let clause = (*info).clause.cast::<pg_sys::Node>();
            let Some(search) = crate::operator::search_clause(clause) else {
                continue;
            };
            let Some(query) = const_text(search.query) else {
                continue;
            };
            let candidates = crate::score::matching_stannum_indexes(
                (*rte).relid,
                (*rel).relid as i32,
                search.document,
            )
            .into_iter()
            .filter(|&index_oid| {
                crate::storage::is_segmented(index_oid) && predicate_proven(rel, index_oid)
            })
            .collect::<Vec<_>>();
            let Some(index_oid) = crate::score::pick_index(&candidates, search.index) else {
                continue;
            };
            return Some(Match {
                clause: clause.cast(),
                index_oid,
                query,
            });
        }
        None
    }
}

/// A partial index may only answer a query whose restrictions imply its
/// predicate; the planner has already decided that per index.
unsafe fn predicate_proven(rel: *mut pg_sys::RelOptInfo, index_oid: pg_sys::Oid) -> bool {
    unsafe {
        for info in PgList::<pg_sys::IndexOptInfo>::from_pg((*rel).indexlist).iter_ptr() {
            if (*info).indexoid == index_oid {
                return (*info).indpred.is_null() || (*info).predOK;
            }
        }
        false
    }
}

/// Recognizes `ORDER BY stannum.score(ctid) DESC` and friends after the scoring
/// support function has bound them to this index.
unsafe fn find_ordering(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    found: &Match,
) -> Option<Ordering> {
    unsafe {
        let pathkeys = (*root).sort_pathkeys;
        if pg_sys::list_length(pathkeys) != 1 {
            return None;
        }
        let pathkey = pg_sys::list_nth(pathkeys, 0).cast::<pg_sys::PathKey>();
        if !descending(pathkey) {
            return None;
        }
        let members =
            PgList::<pg_sys::EquivalenceMember>::from_pg((*(*pathkey).pk_eclass).ec_members);
        for member in members.iter_ptr() {
            if !pg_sys::bms_equal((*member).em_relids, (*rel).relids) {
                continue;
            }
            let expr = (*member).em_expr.cast::<pg_sys::Node>();
            if expr.is_null() || (*expr).type_ != pg_sys::NodeTag::T_FuncExpr {
                continue;
            }
            let func = expr.cast::<pg_sys::FuncExpr>();
            let name = pg_sys::get_func_name((*func).funcid);
            if name.is_null() || CStr::from_ptr(name).to_bytes() != b"score_bound_indexed" {
                continue;
            }
            let arg = |i: i32| pg_sys::list_nth((*func).args, i).cast::<pg_sys::Node>();
            let Some(query) = const_text(arg(1)) else {
                continue;
            };
            let Ok(Some(index_oid)) = const_datum::<i32>(arg(3)) else {
                continue;
            };
            if query != found.query || index_oid as u32 != found.index_oid.to_u32() {
                continue;
            }
            let Ok(Some(mode)) = const_datum::<i32>(arg(4)) else {
                continue;
            };
            let Ok(dense_ratio) = const_datum::<f32>(arg(5)) else {
                continue;
            };
            let Ok(k1) = const_datum::<f32>(arg(6)) else {
                continue;
            };
            let Ok(b) = const_datum::<f32>(arg(7)) else {
                continue;
            };
            let Ok(term_add) = const_datum::<Vec<String>>(arg(8)) else {
                continue;
            };
            let Ok(term_replace) = const_datum::<Vec<String>>(arg(9)) else {
                continue;
            };
            // `limit_tuples` is offset plus count when both are known.
            let limit = (*root).limit_tuples;
            let top_k = (limit >= 0.0).then(|| limit.ceil() as usize);
            return Some(Ordering {
                full: mode == 1,
                dense_ratio,
                k1,
                b,
                term_add,
                term_replace,
                top_k,
            });
        }
        None
    }
}

unsafe fn make_int(value: i64) -> *mut pg_sys::Node {
    unsafe { pg_sys::makeInteger(value.try_into().expect("fits")).cast() }
}

unsafe fn make_string(value: &str) -> *mut pg_sys::Node {
    let c = std::ffi::CString::new(value).expect("no interior NUL");
    unsafe { pg_sys::makeString(pg_sys::pstrdup(c.as_ptr())).cast() }
}

unsafe fn make_float_or_null(value: Option<f32>) -> *mut pg_sys::Node {
    unsafe {
        match value {
            Some(value) => make_string(&value.to_bits().to_string()),
            None => make_string(""),
        }
    }
}

unsafe fn make_array_or_null(value: &Option<Vec<String>>) -> *mut pg_sys::Node {
    unsafe {
        match value {
            Some(values) => {
                let mut list = PgList::<pg_sys::Node>::new();
                for value in values {
                    list.push(make_string(value));
                }
                list.into_pg().cast()
            }
            None => std::ptr::null_mut(),
        }
    }
}

/// Serialized plan parameters, in `custom_private`.
#[derive(Clone)]
struct Private {
    index_oid: u32,
    heap_oid: u32,
    query: String,
    ordering: Option<Ordering>,
}

impl Clone for Ordering {
    fn clone(&self) -> Self {
        Self {
            full: self.full,
            dense_ratio: self.dense_ratio,
            k1: self.k1,
            b: self.b,
            term_add: self.term_add.clone(),
            term_replace: self.term_replace.clone(),
            top_k: self.top_k,
        }
    }
}

impl Private {
    unsafe fn to_list(&self, clause: *mut pg_sys::OpExpr) -> *mut pg_sys::List {
        unsafe {
            let mut list = PgList::<pg_sys::Node>::new();
            list.push(make_int(i64::from(self.index_oid)));
            list.push(make_int(i64::from(self.heap_oid)));
            list.push(make_string(&self.query));
            list.push(pg_sys::copyObjectImpl(clause.cast()).cast());
            match &self.ordering {
                None => list.push(make_int(-1)),
                Some(ordering) => {
                    list.push(make_int(i64::from(ordering.full)));
                    list.push(make_float_or_null(ordering.dense_ratio));
                    list.push(make_float_or_null(ordering.k1));
                    list.push(make_float_or_null(ordering.b));
                    list.push(make_array_or_null(&ordering.term_add));
                    list.push(make_array_or_null(&ordering.term_replace));
                    list.push(make_int(ordering.top_k.map_or(-1, |k| k as i64)));
                }
            }
            list.into_pg()
        }
    }

    unsafe fn from_list(list: *mut pg_sys::List) -> (Self, *mut pg_sys::OpExpr) {
        unsafe {
            let int = |i: i32| (*pg_sys::list_nth(list, i).cast::<pg_sys::Integer>()).ival;
            let string = |i: i32| -> String {
                let node = pg_sys::list_nth(list, i).cast::<pg_sys::String>();
                CStr::from_ptr((*node).sval).to_string_lossy().into_owned()
            };
            let float = |i: i32| -> Option<f32> {
                let text = string(i);
                (!text.is_empty()).then(|| f32::from_bits(text.parse().expect("stored bits")))
            };
            let array = |i: i32| -> Option<Vec<String>> {
                let node = pg_sys::list_nth(list, i).cast::<pg_sys::List>();
                (!node.is_null()).then(|| {
                    PgList::<pg_sys::String>::from_pg(node)
                        .iter_ptr()
                        .map(|s| CStr::from_ptr((*s).sval).to_string_lossy().into_owned())
                        .collect()
                })
            };
            let clause = pg_sys::list_nth(list, 3).cast::<pg_sys::OpExpr>();
            let ordering = match int(4) {
                -1 => None,
                full => Some(Ordering {
                    full: full == 1,
                    dense_ratio: float(5),
                    k1: float(6),
                    b: float(7),
                    term_add: array(8),
                    term_replace: array(9),
                    top_k: usize::try_from(int(10)).ok(),
                }),
            };
            (
                Self {
                    index_oid: int(0) as u32,
                    heap_oid: int(1) as u32,
                    query: string(2),
                    ordering,
                },
                clause,
            )
        }
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn rel_pathlist_hook(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    rti: pg_sys::Index,
    rte: *mut pg_sys::RangeTblEntry,
) {
    unsafe {
        if let Some(previous) = PREVIOUS_REL_HOOK {
            previous(root, rel, rti, rte);
        }
        if !ENABLE.get()
            || pg_sys::RecoveryInProgress()
            || (*rel).reloptkind != pg_sys::RelOptKind::RELOPT_BASEREL
            || (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION
            || byte((*rte).relkind) != b'r'
            || !(*rel).lateral_relids.is_null()
        {
            return;
        }
        let Some(found) = find_match(rel, rte) else {
            return;
        };
        let ordering = find_ordering(root, rel, &found);
        let private = Private {
            index_oid: found.index_oid.to_u32(),
            heap_oid: (*rte).relid.to_u32(),
            query: found.query.clone(),
            ordering,
        };
        let mut path = pgrx::PgBox::<pg_sys::CustomPath>::alloc_node(pg_sys::NodeTag::T_CustomPath);
        path.path.pathtype = pg_sys::NodeTag::T_CustomScan;
        path.path.parent = rel;
        path.path.pathtarget = (*rel).reltarget;
        path.path.param_info = std::ptr::null_mut();
        path.path.parallel_aware = false;
        // An unordered scan keeps no state outside its own process, so a
        // worker may run it (as a join's inner side). An ordered scan
        // publishes scorer state to the score calls of its own backend.
        path.path.parallel_safe = (*rel).consider_parallel && private.ordering.is_none();
        // This is a complete, worker-local path, never a partial path: giving
        // each worker a full candidate list would duplicate rows/counts. A DSM
        // cursor and partial aggregate protocol are required before changing it.
        path.path.parallel_workers = 0;
        // The relation's row estimate already reflects the clause's
        // selectivity through the operator's restriction function; the same
        // estimate prices the index read and the heap fetches.
        path.path.rows = (*rel).rows.max(1.0);
        let estimate = crate::selectivity::estimate_query(found.index_oid, &found.query)
            .unwrap_or(crate::selectivity::FALLBACK);
        let index = crate::selectivity::index_cost(
            root,
            found.index_oid,
            (*rel).reltablespace,
            &estimate,
            (*rel).tuples,
            1.0,
            1,
        );
        let (heap_pages, cost_per_page) =
            crate::selectivity::heap_fetch(&index, f64::from((*rel).pages));
        // Each candidate is fetched by TID and passes the remaining quals; an
        // exact plan skips re-evaluating ==> itself, unlike the bitmap path.
        let saved = if estimate.exact {
            pg_sys::cpu_operator_cost
        } else {
            0.0
        };
        let per_tuple =
            pg_sys::cpu_tuple_cost + ((*rel).baserestrictcost.per_tuple - saved).max(0.0);
        // Candidates are gathered before the first row is returned.
        let mut startup = index.total + (*rel).baserestrictcost.startup;
        if let Some(ordering) = &private.ordering {
            // Scoring every candidate, then ordering the ones the query
            // consumes (or all of them).
            let sorted = ordering
                .top_k
                .map_or(index.candidates, |k| (k as f64).min(index.candidates))
                .max(2.0);
            startup += index.candidates * pg_sys::cpu_operator_cost * 2.0
                + index.candidates * sorted.log2() * pg_sys::cpu_operator_cost;
            path.path.pathkeys = (*root).sort_pathkeys;
        }
        path.path.startup_cost = startup;
        path.path.total_cost = startup + heap_pages * cost_per_page + index.candidates * per_tuple;
        path.flags = 0;
        path.custom_paths = std::ptr::null_mut();
        path.custom_restrictinfo = std::ptr::null_mut();
        path.custom_private = private.to_list(found.clause);
        path.methods = &SEARCH_PATH_METHODS.0;
        pg_sys::add_path(rel, path.into_pg().cast());
    }
}

/// Removes our clause from the restriction clauses and returns the rest.
unsafe fn remaining_quals(
    clauses: *mut pg_sys::List,
    ours: *mut pg_sys::OpExpr,
) -> *mut pg_sys::List {
    unsafe {
        let mut quals = PgList::<pg_sys::Node>::new();
        for info in PgList::<pg_sys::RestrictInfo>::from_pg(clauses).iter_ptr() {
            let clause = (*info).clause.cast::<pg_sys::Node>();
            if pg_sys::equal(clause.cast(), ours.cast()) {
                continue;
            }
            quals.push(clause);
        }
        quals.into_pg()
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn plan_search_path(
    _root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    best_path: *mut pg_sys::CustomPath,
    tlist: *mut pg_sys::List,
    clauses: *mut pg_sys::List,
    _custom_plans: *mut pg_sys::List,
) -> *mut pg_sys::Plan {
    unsafe {
        let (_, clause) = Private::from_list((*best_path).custom_private);
        let mut scan = pgrx::PgBox::<pg_sys::CustomScan>::alloc_node(pg_sys::NodeTag::T_CustomScan);
        scan.scan.plan.targetlist = tlist;
        scan.scan.plan.qual = remaining_quals(clauses, clause);
        scan.scan.scanrelid = (*rel).relid;
        scan.flags = 0;
        scan.custom_plans = std::ptr::null_mut();
        scan.custom_exprs = std::ptr::null_mut();
        scan.custom_private = (*best_path).custom_private;
        scan.custom_scan_tlist = std::ptr::null_mut();
        scan.methods = &SEARCH_SCAN_METHODS.0;
        scan.into_pg().cast()
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn upper_paths_hook(
    root: *mut pg_sys::PlannerInfo,
    stage: pg_sys::UpperRelationKind::Type,
    input_rel: *mut pg_sys::RelOptInfo,
    output_rel: *mut pg_sys::RelOptInfo,
    extra: *mut c_void,
) {
    unsafe {
        if let Some(previous) = PREVIOUS_UPPER_HOOK {
            previous(root, stage, input_rel, output_rel, extra);
        }
        if !ENABLE.get() || stage != pg_sys::UpperRelationKind::UPPERREL_GROUP_AGG {
            return;
        }
        let parse = (*root).parse;
        if !(*parse).hasAggs
            || !(*parse).groupClause.is_null()
            || !(*parse).havingQual.is_null()
            || (*parse).hasWindowFuncs
            || (*parse).hasDistinctOn
            || !(*parse).groupingSets.is_null()
            || pg_sys::list_length((*parse).targetList) != 1
        {
            return;
        }
        // Exactly count(*), unfiltered.
        let entry = pg_sys::list_nth((*parse).targetList, 0).cast::<pg_sys::TargetEntry>();
        let expr = (*entry).expr.cast::<pg_sys::Node>();
        if expr.is_null() || (*expr).type_ != pg_sys::NodeTag::T_Aggref {
            return;
        }
        let aggref = expr.cast::<pg_sys::Aggref>();
        if !(*aggref).aggstar
            || !(*aggref).aggfilter.is_null()
            || !(*aggref).aggdistinct.is_null()
            || !(*aggref).aggorder.is_null()
        {
            return;
        }
        // The input must be a base relation whose only restriction is ours.
        if (*input_rel).reloptkind != pg_sys::RelOptKind::RELOPT_BASEREL
            || pg_sys::list_length((*input_rel).baserestrictinfo) != 1
        {
            return;
        }
        let rte = pg_sys::list_nth((*parse).rtable, (*input_rel).relid as i32 - 1)
            .cast::<pg_sys::RangeTblEntry>();
        if (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION
            || byte((*rte).relkind) != b'r'
            || pg_sys::RecoveryInProgress()
        {
            return;
        }
        let Some(found) = find_match(input_rel, rte) else {
            return;
        };
        let private = Private {
            index_oid: found.index_oid.to_u32(),
            heap_oid: (*rte).relid.to_u32(),
            query: found.query.clone(),
            ordering: None,
        };
        let mut path = pgrx::PgBox::<pg_sys::CustomPath>::alloc_node(pg_sys::NodeTag::T_CustomPath);
        path.path.pathtype = pg_sys::NodeTag::T_CustomScan;
        path.path.parent = output_rel;
        path.path.pathtarget = (*output_rel).reltarget;
        path.path.param_info = std::ptr::null_mut();
        path.path.parallel_safe = (*output_rel).consider_parallel;
        path.path.parallel_aware = false;
        path.path.parallel_workers = 0;
        path.path.rows = 1.0;
        let estimate = crate::selectivity::estimate_query(found.index_oid, &found.query)
            .unwrap_or(crate::selectivity::FALLBACK);
        let index = crate::selectivity::index_cost(
            root,
            found.index_oid,
            (*input_rel).reltablespace,
            &estimate,
            (*input_rel).tuples,
            1.0,
            1,
        );
        let (heap_pages, cost_per_page) =
            crate::selectivity::heap_fetch(&index, f64::from((*input_rel).pages));
        // Candidates on all-visible pages are counted without a heap fetch;
        // an inexact plan fetches and rechecks every candidate.
        let fetched_pages = if estimate.exact {
            heap_pages * (1.0 - (*input_rel).allvisfrac.clamp(0.0, 1.0))
        } else {
            heap_pages
        };
        let recheck = if estimate.exact {
            0.0
        } else {
            (*input_rel).baserestrictcost.per_tuple
        };
        path.path.startup_cost = index.total
            + fetched_pages * cost_per_page
            + index.candidates * (pg_sys::cpu_operator_cost + recheck);
        path.path.total_cost = path.path.startup_cost + pg_sys::cpu_tuple_cost;
        path.flags = 0;
        path.custom_private = private.to_list(found.clause);
        path.methods = &COUNT_PATH_METHODS.0;
        pg_sys::add_path(output_rel, path.into_pg().cast());
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn plan_count_path(
    root: *mut pg_sys::PlannerInfo,
    _rel: *mut pg_sys::RelOptInfo,
    best_path: *mut pg_sys::CustomPath,
    tlist: *mut pg_sys::List,
    _clauses: *mut pg_sys::List,
    _custom_plans: *mut pg_sys::List,
) -> *mut pg_sys::Plan {
    unsafe {
        let mut scan = pgrx::PgBox::<pg_sys::CustomScan>::alloc_node(pg_sys::NodeTag::T_CustomScan);
        scan.scan.plan.targetlist = tlist;
        scan.scan.plan.qual = std::ptr::null_mut();
        scan.scan.scanrelid = 0;
        // The scan tuple is the aggregate itself, so the planner maps the
        // target list's Aggref onto our single output column.
        let entry = pg_sys::list_nth((*(*root).parse).targetList, 0).cast::<pg_sys::TargetEntry>();
        let mut scan_tlist = PgList::<pg_sys::TargetEntry>::new();
        scan_tlist.push(pg_sys::makeTargetEntry(
            pg_sys::copyObjectImpl((*entry).expr.cast()).cast(),
            1,
            std::ptr::null_mut(),
            false,
        ));
        scan.custom_scan_tlist = scan_tlist.into_pg();
        scan.custom_private = (*best_path).custom_private;
        scan.custom_exprs = std::ptr::null_mut();
        scan.methods = &COUNT_SCAN_METHODS.0;
        scan.into_pg().cast()
    }
}

// --- Execution ------------------------------------------------------------------

/// Executor state, owned by the executor's memory context.
struct ScanExec {
    private: Private,
    /// The original `==>` clause, compiled against heap tuples, for rechecks
    /// of inexact plans and for the recovery fallback.
    clause: *mut pg_sys::ExprState,
    fetch: *mut pg_sys::IndexFetchTableData,
    /// A heap tuple slot for the count node, whose scan slot is virtual.
    fetch_slot: *mut pg_sys::TupleTableSlot,
    heap: pg_sys::Relation,
    /// Some candidate came from an inexact plan and must pass `clause`.
    recheck: bool,
    /// Candidates in output order, filled on first execution.
    tids: Vec<Tid>,
    /// Scores aligned with `tids` for an ordered scan; only the first
    /// `sorted` entries are in order, the rest are sorted if ever reached.
    scores: Vec<f32>,
    sorted: usize,
    next: usize,
    started: bool,
    /// Heap scan for the recovery-snapshot fallback.
    fallback: *mut pg_sys::TableScanDescData,
    /// `tids` holds only the pruned top k; the rest are produced on demand.
    pruned: bool,
    /// Explain counters. Candidates are unknown while pruned; `scored`
    /// counts the candidates a pruned scan scored.
    candidates: Option<usize>,
    scored: Option<usize>,
    fetched: usize,
    skipped_pages: usize,
    ordered: bool,
}

#[repr(C)]
struct StannumScanState {
    css: pg_sys::CustomScanState,
    exec: *mut Option<Box<ScanExec>>,
}

unsafe fn create_state(
    cscan: *mut pg_sys::CustomScan,
    methods: &'static pg_sys::CustomExecMethods,
    buffer_slots: bool,
) -> *mut pg_sys::Node {
    unsafe {
        let state =
            pg_sys::palloc0(std::mem::size_of::<StannumScanState>()).cast::<StannumScanState>();
        (*state).css.ss.ps.type_ = pg_sys::NodeTag::T_CustomScanState;
        (*state).css.methods = methods;
        (*state).css.flags = (*cscan).flags;
        if buffer_slots {
            (*state).css.slotOps = &raw const pg_sys::TTSOpsBufferHeapTuple;
        }
        (*state).exec = std::ptr::null_mut();
        state.cast()
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn create_search_state(
    cscan: *mut pg_sys::CustomScan,
) -> *mut pg_sys::Node {
    unsafe { create_state(cscan, &SEARCH_EXEC_METHODS.0, true) }
}

#[pg_guard]
unsafe extern "C-unwind" fn create_count_state(
    cscan: *mut pg_sys::CustomScan,
) -> *mut pg_sys::Node {
    unsafe { create_state(cscan, &COUNT_EXEC_METHODS.0, false) }
}

unsafe fn exec_of<'a>(node: *mut pg_sys::CustomScanState) -> &'a mut ScanExec {
    unsafe {
        (&mut *(*node.cast::<StannumScanState>()).exec)
            .as_deref_mut()
            .expect("scan state initialized")
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn begin_scan(
    node: *mut pg_sys::CustomScanState,
    estate: *mut pg_sys::EState,
    eflags: i32,
) {
    unsafe {
        let cscan = (*node).ss.ps.plan.cast::<pg_sys::CustomScan>();
        let (private, clause) = Private::from_list((*cscan).custom_private);
        // The private copy keeps the parser's Vars, which evaluate against a
        // heap tuple in the scan slot whatever their range-table index.
        let mut clause_list = PgList::<pg_sys::Node>::new();
        clause_list.push(pg_sys::copyObjectImpl(clause.cast()).cast());
        let clause = pg_sys::ExecInitQual(clause_list.into_pg(), node.cast());
        let is_count = (*cscan).scan.scanrelid == 0;
        let heap = if is_count {
            pg_sys::table_open(
                pg_sys::Oid::from(private.heap_oid),
                pg_sys::AccessShareLock as _,
            )
        } else {
            (*node).ss.ss_currentRelation
        };
        let explain_only = eflags & pg_sys::EXEC_FLAG_EXPLAIN_ONLY as i32 != 0;
        let fetch = if explain_only {
            std::ptr::null_mut()
        } else {
            pg_sys::table_index_fetch_begin(heap)
        };
        let fetch_slot = if is_count && !explain_only {
            pg_sys::table_slot_create(heap, std::ptr::null_mut())
        } else {
            std::ptr::null_mut()
        };
        let ordered = private.ordering.is_some();
        let exec = ScanExec {
            private,
            clause,
            fetch,
            fetch_slot,
            heap,
            recheck: false,
            tids: Vec::new(),
            scores: Vec::new(),
            sorted: 0,
            next: 0,
            started: false,
            fallback: std::ptr::null_mut(),
            pruned: false,
            candidates: None,
            scored: None,
            fetched: 0,
            skipped_pages: 0,
            ordered,
        };
        let holder = PgMemoryContexts::For((*estate).es_query_cxt)
            .leak_and_drop_on_delete(Some(Box::new(exec)));
        (*node.cast::<StannumScanState>()).exec = holder;
    }
}

/// Gathers the matching TIDs from the index, in output order.
///
/// A ranked scan with a known top k first tries to prune: the scorer walks
/// the index itself, skipping blocks of postings that cannot enter the top
/// k, and only those k rows are materialized. Every other scan, and any
/// query the scorer cannot bound, enumerates and scores every candidate.
unsafe fn gather(exec: &mut ScanExec) {
    unsafe {
        exec.scores.clear();
        exec.pruned = false;
        exec.scored = None;
        let mut scorer = exec.private.ordering.as_ref().map(|ordering| {
            crate::score::scorer_for_scan(
                exec.private.heap_oid,
                exec.private.index_oid,
                &exec.private.query,
                ordering.full,
                ordering.dense_ratio,
                ordering.k1,
                ordering.b,
                ordering.term_add.clone(),
                ordering.term_replace.clone(),
            )
        });
        let top_k = exec.private.ordering.as_ref().and_then(|o| o.top_k);
        if let Some(k) = top_k
            && k <= crate::score::PRUNE_MAX_K
            && let Some(top) = scorer.as_ref().and_then(|scorer| scorer.top_k(k))
        {
            exec.candidates = top.complete.then_some(top.rows.len());
            exec.scored = Some(top.scored);
            exec.scores = top.rows.iter().map(|(score, _)| *score).collect();
            exec.tids = top.rows.iter().map(|(_, tid)| *tid).collect();
            exec.sorted = exec.tids.len();
            exec.pruned = !top.complete;
            let scorer = scorer.take().expect("a top k needs a scorer");
            crate::score::publish_scan_scorer(scorer, &top.rows);
            exec.next = 0;
            exec.started = true;
            return;
        }
        let tids = candidates(exec);
        finish(exec, tids, scorer);
        exec.next = 0;
        exec.started = true;
    }
}

/// Every matching TID across the index's sources, in heap order.
unsafe fn candidates(exec: &mut ScanExec) -> Vec<Tid> {
    unsafe {
        let index_oid = pg_sys::Oid::from(exec.private.index_oid);
        let index = pg_sys::index_open(index_oid, pg_sys::AccessShareLock as _);
        let tokenizer = crate::storage::index_tokenizer(index);
        pg_sys::index_close(index, pg_sys::AccessShareLock as _);
        let query: Query =
            tinql::runtime::parse_tinql_to_query(&exec.private.query, tokenizer.as_ref())
                .unwrap_or_else(|error| pgrx::error!("invalid ==> query: {error}"));
        let view = crate::storage::view(index_oid);
        let limits = Limits::default();
        let mut tids = Vec::new();
        for ((segment, dead), label) in view.sources.iter().zip(&view.labels) {
            pgrx::check_for_interrupts!();
            let planned = plan(&query, segment, &limits)
                .unwrap_or_else(|error| pgrx::error!("Stannum query plan: {error}"));
            let mut cursor: Box<dyn segment::set::Cursor> = planned.cursor;
            // A capped expansion yields a superset; those rows are rechecked.
            exec.recheck |= !planned.exact;
            if let Some(dead) = dead {
                let dead = crate::storage::codec_in(
                    segment::postings::Postings::parse(dead).and_then(|p| p.cursor()),
                    &format!("{label} dead list"),
                );
                cursor = Box::new(crate::storage::codec_in(
                    segment::set::Difference::new(cursor, dead),
                    label,
                ));
            }
            while let Some(tid) = cursor.current() {
                tids.push(tid);
                crate::storage::codec_in(cursor.advance(), label);
            }
        }
        tids.sort_unstable();
        tids.dedup();
        tids
    }
}

/// Stores every candidate; with a scorer, scored and in output order.
fn finish(exec: &mut ScanExec, mut tids: Vec<Tid>, scorer: Option<crate::score::IndexScorer>) {
    exec.candidates = Some(tids.len());
    exec.scores.clear();
    exec.sorted = tids.len();
    if let Some(mut scorer) = scorer {
        let top_k = exec.private.ordering.as_ref().and_then(|o| o.top_k);
        let mut scored: Vec<(f32, Tid)> =
            tids.iter().map(|tid| (scorer.score(*tid), *tid)).collect();
        // Only the rows the query will consume are ordered now; the rest
        // are ordered on demand should the executor ask for them.
        let sorted = match top_k {
            Some(k) if k < scored.len() => {
                scored.select_nth_unstable_by(k, rank);
                scored[..k].sort_by(rank);
                k
            }
            _ => {
                scored.sort_by(rank);
                scored.len()
            }
        };
        exec.sorted = sorted;
        crate::score::publish_scan_scorer(scorer, &scored[..sorted]);
        exec.scores = scored.iter().map(|(score, _)| *score).collect();
        tids = scored.into_iter().map(|(_, tid)| tid).collect();
    }
    exec.tids = tids;
}

/// Replaces a pruned top k with the complete ordering once the executor
/// reads past it. The locations already consumed are removed from the
/// complete ordering rather than skipped by position: documents indexed
/// since the top k was built (invisible to the snapshot, but present in the
/// index and scored) can rank above them and would otherwise shift the
/// consumed rows back into the output.
unsafe fn complete(exec: &mut ScanExec) {
    unsafe {
        let ordering = exec
            .private
            .ordering
            .as_ref()
            .expect("pruned scans are ordered");
        let scorer = crate::score::scorer_for_scan(
            exec.private.heap_oid,
            exec.private.index_oid,
            &exec.private.query,
            ordering.full,
            ordering.dense_ratio,
            ordering.k1,
            ordering.b,
            ordering.term_add.clone(),
            ordering.term_replace.clone(),
        );
        let consumed: FxHashSet<Tid> = exec.tids[..exec.next].iter().copied().collect();
        let tids = candidates(exec);
        finish(exec, tids, Some(scorer));
        let mut kept = 0;
        let mut sorted = exec.sorted;
        for i in 0..exec.tids.len() {
            if consumed.contains(&exec.tids[i]) {
                if i < exec.sorted {
                    sorted -= 1;
                }
                continue;
            }
            exec.tids[kept] = exec.tids[i];
            exec.scores[kept] = exec.scores[i];
            kept += 1;
        }
        exec.tids.truncate(kept);
        exec.scores.truncate(kept);
        exec.sorted = sorted;
        exec.next = 0;
        exec.pruned = false;
    }
}

/// Orders the candidates past the up-front top-k, once the executor reads
/// beyond them.
fn sort_rest(exec: &mut ScanExec) {
    if exec.sorted >= exec.tids.len() {
        return;
    }
    let mut rest: Vec<(f32, Tid)> = exec.scores[exec.sorted..]
        .iter()
        .copied()
        .zip(exec.tids[exec.sorted..].iter().copied())
        .collect();
    rest.sort_by(rank);
    for (i, (score, tid)) in rest.into_iter().enumerate() {
        exec.scores[exec.sorted + i] = score;
        exec.tids[exec.sorted + i] = tid;
    }
    exec.sorted = exec.tids.len();
}

/// Evaluates the original clause against the tuple in `slot`.
unsafe fn passes_clause(
    node: *mut pg_sys::CustomScanState,
    exec: &ScanExec,
    slot: *mut pg_sys::TupleTableSlot,
) -> bool {
    unsafe {
        let econtext = (*node).ss.ps.ps_ExprContext;
        (*econtext).ecxt_scantuple = slot;
        pg_sys::ExecQual(exec.clause, econtext)
    }
}

unsafe fn recovery_snapshot(node: *mut pg_sys::CustomScanState) -> bool {
    unsafe {
        let snapshot = (*(*node).ss.ps.state).es_snapshot;
        !snapshot.is_null() && (*snapshot).takenDuringRecovery
    }
}

unsafe fn pointer_of(tid: Tid) -> pg_sys::ItemPointerData {
    pg_sys::ItemPointerData {
        ip_blkid: pg_sys::BlockIdData {
            bi_hi: (tid.block >> 16) as u16,
            bi_lo: tid.block as u16,
        },
        ip_posid: tid.offset,
    }
}

/// Next visible matching tuple into the scan slot, or an empty slot.
#[pg_guard]
unsafe extern "C-unwind" fn search_access(
    scan: *mut pg_sys::ScanState,
) -> *mut pg_sys::TupleTableSlot {
    unsafe {
        let node = scan.cast::<pg_sys::CustomScanState>();
        let slot = (*scan).ss_ScanTupleSlot;
        let snapshot = (*(*scan).ps.state).es_snapshot;
        let exec = exec_of(node);
        if recovery_snapshot(node) {
            return fallback_access(scan, exec, slot);
        }
        if !exec.started {
            gather(exec);
        }
        loop {
            if exec.next >= exec.tids.len() {
                if !exec.pruned {
                    break;
                }
                // The executor reads past the pruned top k: score everything.
                complete(exec);
                continue;
            }
            pgrx::check_for_interrupts!();
            if exec.next >= exec.sorted {
                sort_rest(exec);
            }
            let tid = exec.tids[exec.next];
            exec.next += 1;
            let mut pointer = pointer_of(tid);
            let mut call_again = false;
            let mut all_dead = false;
            loop {
                if pg_sys::table_index_fetch_tuple(
                    exec.fetch,
                    &mut pointer,
                    snapshot,
                    slot,
                    &mut call_again,
                    &mut all_dead,
                ) {
                    exec.fetched += 1;
                    if !exec.recheck || passes_clause(node, exec, slot) {
                        return slot;
                    }
                    break;
                }
                if !call_again {
                    break;
                }
            }
        }
        pg_sys::ExecClearTuple(slot)
    }
}

/// Recovery-era snapshots read the whole heap and evaluate the original clause.
unsafe fn fallback_access(
    scan: *mut pg_sys::ScanState,
    exec: &mut ScanExec,
    slot: *mut pg_sys::TupleTableSlot,
) -> *mut pg_sys::TupleTableSlot {
    unsafe {
        let snapshot = (*(*scan).ps.state).es_snapshot;
        if exec.fallback.is_null() {
            exec.fallback = pg_sys::table_beginscan(exec.heap, snapshot, 0, std::ptr::null_mut());
        }
        let node = scan.cast::<pg_sys::CustomScanState>();
        while pg_sys::table_scan_getnextslot(
            exec.fallback,
            pg_sys::ScanDirection::ForwardScanDirection,
            slot,
        ) {
            pgrx::check_for_interrupts!();
            if passes_clause(node, exec, slot) {
                exec.fetched += 1;
                return slot;
            }
        }
        pg_sys::ExecClearTuple(slot)
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn search_recheck(
    _scan: *mut pg_sys::ScanState,
    _slot: *mut pg_sys::TupleTableSlot,
) -> bool {
    true
}

#[pg_guard]
unsafe extern "C-unwind" fn exec_search(
    node: *mut pg_sys::CustomScanState,
) -> *mut pg_sys::TupleTableSlot {
    unsafe { pg_sys::ExecScan(&mut (*node).ss, Some(search_access), Some(search_recheck)) }
}

/// Counts visible candidates, skipping heap fetches on all-visible pages.
#[pg_guard]
unsafe extern "C-unwind" fn exec_count(
    node: *mut pg_sys::CustomScanState,
) -> *mut pg_sys::TupleTableSlot {
    unsafe {
        let slot = (*node).ss.ss_ScanTupleSlot;
        let snapshot = (*(*node).ss.ps.state).es_snapshot;
        let exec = exec_of(node);
        if exec.started && exec.next > 0 {
            return pg_sys::ExecClearTuple(slot);
        }
        let mut count = 0i64;
        let fetch_slot = exec.fetch_slot;
        if recovery_snapshot(node) {
            // Count through a heap scan with the original clause.
            let scan = pg_sys::table_beginscan(exec.heap, snapshot, 0, std::ptr::null_mut());
            while pg_sys::table_scan_getnextslot(
                scan,
                pg_sys::ScanDirection::ForwardScanDirection,
                fetch_slot,
            ) {
                pgrx::check_for_interrupts!();
                if passes_clause(node, exec, fetch_slot) {
                    count += 1;
                }
            }
            pg_sys::table_endscan(scan);
        } else {
            gather(exec);
            let mut vmbuf = pg_sys::InvalidBuffer as pg_sys::Buffer;
            let mut i = 0;
            while i < exec.tids.len() {
                pgrx::check_for_interrupts!();
                let block = exec.tids[i].block;
                let mut end = i;
                while end < exec.tids.len() && exec.tids[end].block == block {
                    end += 1;
                }
                let status = pg_sys::visibilitymap_get_status(exec.heap, block, &mut vmbuf);
                if !exec.recheck && status & pg_sys::VISIBILITYMAP_ALL_VISIBLE as u8 != 0 {
                    count += (end - i) as i64;
                    exec.skipped_pages += 1;
                } else {
                    for tid in &exec.tids[i..end] {
                        let mut pointer = pointer_of(*tid);
                        let mut call_again = false;
                        let mut all_dead = false;
                        loop {
                            if pg_sys::table_index_fetch_tuple(
                                exec.fetch,
                                &mut pointer,
                                snapshot,
                                fetch_slot,
                                &mut call_again,
                                &mut all_dead,
                            ) {
                                exec.fetched += 1;
                                if !exec.recheck || passes_clause(node, exec, fetch_slot) {
                                    count += 1;
                                }
                                break;
                            }
                            if !call_again {
                                break;
                            }
                        }
                    }
                }
                i = end;
            }
            if vmbuf != pg_sys::InvalidBuffer as pg_sys::Buffer {
                pg_sys::ReleaseBuffer(vmbuf);
            }
        }
        exec.next = 1;
        exec.started = true;
        pg_sys::ExecClearTuple(slot);
        *(*slot).tts_values = pg_sys::Datum::from(count);
        *(*slot).tts_isnull = false;
        pg_sys::ExecStoreVirtualTuple(slot)
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn end_scan(node: *mut pg_sys::CustomScanState) {
    unsafe {
        let holder = (*node.cast::<StannumScanState>()).exec;
        if holder.is_null() {
            return;
        }
        if let Some(mut exec) = (*holder).take() {
            if !exec.fallback.is_null() {
                pg_sys::table_endscan(exec.fallback);
                exec.fallback = std::ptr::null_mut();
            }
            if !exec.fetch.is_null() {
                pg_sys::table_index_fetch_end(exec.fetch);
            }
            if !exec.fetch_slot.is_null() {
                pg_sys::ExecDropSingleTupleTableSlot(exec.fetch_slot);
            }
            let cscan = (*node).ss.ps.plan.cast::<pg_sys::CustomScan>();
            if (*cscan).scan.scanrelid == 0 {
                pg_sys::table_close(exec.heap, pg_sys::AccessShareLock as _);
            }
        }
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn rescan(node: *mut pg_sys::CustomScanState) {
    unsafe {
        let exec = exec_of(node);
        exec.next = 0;
        if !exec.fallback.is_null() {
            pg_sys::table_rescan(exec.fallback, std::ptr::null_mut());
        }
        // The count node re-counts; the search node replays its candidates.
        let cscan = (*node).ss.ps.plan.cast::<pg_sys::CustomScan>();
        if (*cscan).scan.scanrelid == 0 {
            exec.started = false;
        }
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn explain(
    node: *mut pg_sys::CustomScanState,
    _ancestors: *mut pg_sys::List,
    es: *mut pg_sys::ExplainState,
) {
    unsafe {
        let exec = exec_of(node);
        let index_name = pg_sys::get_rel_name(pg_sys::Oid::from(exec.private.index_oid));
        if !index_name.is_null() {
            pg_sys::ExplainPropertyText(c"Index".as_ptr(), index_name, es);
        }
        let query = std::ffi::CString::new(exec.private.query.clone()).unwrap_or_default();
        pg_sys::ExplainPropertyText(c"Query".as_ptr(), query.as_ptr(), es);
        if exec.ordered {
            pg_sys::ExplainPropertyText(c"Order".as_ptr(), c"score DESC".as_ptr(), es);
            if let Some(k) = exec.private.ordering.as_ref().and_then(|o| o.top_k) {
                pg_sys::ExplainPropertyInteger(c"Top K".as_ptr(), std::ptr::null(), k as i64, es);
            }
        }
        if (*es).analyze {
            // Core instrumentation is copied from workers, but these private
            // Rust counters are not. Do not report the idle leader's zero
            // heap fetches as if they described work done in another process.
            if !exec.started && !(*node).ss.ps.worker_instrument.is_null() {
                pg_sys::ExplainPropertyText(
                    c"Execution Counters".as_ptr(),
                    c"Unavailable from parallel workers".as_ptr(),
                    es,
                );
                return;
            }
            if let Some(candidates) = exec.candidates {
                pg_sys::ExplainPropertyInteger(
                    c"Candidates".as_ptr(),
                    std::ptr::null(),
                    candidates as i64,
                    es,
                );
            }
            if let Some(scored) = exec.scored {
                pg_sys::ExplainPropertyText(c"Pruning".as_ptr(), c"block-max".as_ptr(), es);
                pg_sys::ExplainPropertyInteger(
                    c"Scored Candidates".as_ptr(),
                    std::ptr::null(),
                    scored as i64,
                    es,
                );
            }
            pg_sys::ExplainPropertyInteger(
                c"Heap Fetches".as_ptr(),
                std::ptr::null(),
                exec.fetched as i64,
                es,
            );
            if exec.skipped_pages > 0 {
                pg_sys::ExplainPropertyInteger(
                    c"All-Visible Pages".as_ptr(),
                    std::ptr::null(),
                    exec.skipped_pages as i64,
                    es,
                );
            }
        }
    }
}
