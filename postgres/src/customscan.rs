// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

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
//! index cannot be read selectively at execution time: on a hot standby that
//! is the case unless the primary logs removal horizons and this server
//! replays them (see `storage::index_reads_allowed`). The bitmap index scan
//! path remains available; `stannum.enable_custom_scan` disables these nodes.

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
// Experimental diagnostic control: preserve production selection unless requested.
static PROFILE_COUNT_SELECTION: GucSetting<bool> = GucSetting::<bool>::new(false);
static COUNT_PAGE_THRESHOLD: GucSetting<i32> = GucSetting::<i32>::new(0);
static FORCE_COUNT_PAGES: GucSetting<bool> = GucSetting::<bool>::new(false);
static COUNT_FOLD: GucSetting<bool> = GucSetting::<bool>::new(true);

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
        c"stannum.rank_by_ordinal",
        c"Rank pruned disjunctions over the ordinal streams instead of the TID postings",
        c"A prototype of ADR 0003; chunk bounds are derived from block bounds at query time.",
        &crate::score::RANK_BY_ORDINAL,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"stannum.profile_count_selection",
        c"Collect bounded count-selector features without changing the default strategy",
        c"Estimation timing includes feature collection; counters accumulate over rescans.",
        &PROFILE_COUNT_SELECTION,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"stannum.count_page_threshold",
        c"Experimental OR input-posting threshold for page counts; zero disables",
        c"Uncalibrated diagnostic rule. Unsupported shapes retain automatic selection; force_count_pages takes precedence.",
        &COUNT_PAGE_THRESHOLD, 0, i32::MAX, GucContext::Userset, GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"stannum.force_count_pages",
        c"Force the page-bitmap count strategy for diagnostic comparisons",
        c"Off retains automatic selection; visibility checks and rechecks remain enabled.",
        &FORCE_COUNT_PAGES,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"stannum.count_fold",
        c"Count Boolean term queries by folding document-ordinal streams",
        c"Off keeps the scalar and page-bitmap strategies; other query shapes always use those.",
        &COUNT_FOLD,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"stannum.reader_cache_mb",
        c"Per-backend memory for bytes fetched from immutable segments",
        c"The cache is emptied when its readers hold more; dense terms' ordinal chunks make a larger cache worthwhile.",
        &crate::storage::READER_CACHE_MB, 1, 1024 * 1024, GucContext::Userset, GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"stannum.read_cache_mb",
        c"Per-backend memory for the ranges cursors sweep: postings windows, ordinal chunks, payload spans",
        c"Least recently used ranges are evicted beyond this; the reader cache holds headers and tables separately.",
        &crate::storage::READ_CACHE_MB, 1, 1024 * 1024, GucContext::Userset, GucFlags::default(),
    );
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
    query: Option<String>,
    query_expr: *mut pg_sys::Node,
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

/// Finds a constant (or, when allowed, external text parameter) restriction
/// on `rel` (in either operator form) answered by a segmented stannum index whose tokenizer settings are
/// the ones the clause is bound to.
unsafe fn find_match(
    rel: *mut pg_sys::RelOptInfo,
    rte: *mut pg_sys::RangeTblEntry,
    allow_parameter: bool,
) -> Option<Match> {
    unsafe {
        let restrictions = PgList::<pg_sys::RestrictInfo>::from_pg((*rel).baserestrictinfo);
        for info in restrictions.iter_ptr() {
            let clause = (*info).clause.cast::<pg_sys::Node>();
            let Some(search) = crate::operator::search_clause(clause) else {
                continue;
            };
            let query = const_text(search.query);
            if query.is_none() {
                if !allow_parameter
                    || search.query.is_null()
                    || (*search.query).type_ != pg_sys::NodeTag::T_Param
                {
                    continue;
                }
                let parameter = &*search.query.cast::<pg_sys::Param>();
                // Only client bind parameters: no outer-row values, subplans,
                // functions, or expressions with execution-dependent effects.
                if parameter.paramkind != pg_sys::ParamKind::PARAM_EXTERN
                    || parameter.paramtype != pg_sys::TEXTOID
                {
                    continue;
                }
            }
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
                query_expr: search.query,
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
            let same_query = match &found.query {
                Some(query) => const_text(arg(1)).as_ref() == Some(query),
                None => pg_sys::equal(arg(1).cast(), found.query_expr.cast()),
            };
            let Ok(Some(index_oid)) = const_datum::<i32>(arg(3)) else {
                continue;
            };
            if !same_query || index_oid as u32 != found.index_oid.to_u32() {
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
            || (*rel).reloptkind != pg_sys::RelOptKind::RELOPT_BASEREL
            || (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION
            || byte((*rte).relkind) != b'r'
            || !(*rel).lateral_relids.is_null()
        {
            return;
        }
        let Some(mut found) = find_match(rel, rte, true) else {
            return;
        };
        let mut ordering = find_ordering(root, rel, &found);
        // A parameter that cannot supply this ordering must not hide an
        // existing constant-clause path in a query with multiple restrictions.
        if found.query.is_none() && ordering.is_none() {
            let Some(constant) = find_match(rel, rte, false) else {
                return;
            };
            found = constant;
            ordering = find_ordering(root, rel, &found);
        }
        let private = Private {
            index_oid: found.index_oid.to_u32(),
            heap_oid: (*rte).relid.to_u32(),
            query: found.query.clone().unwrap_or_else(|| "<parameter>".into()),
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
        let estimate = found
            .query
            .as_deref()
            .and_then(|query| crate::selectivity::estimate_query(found.index_oid, query))
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
        // Candidate decoding streams, but opening a term still fetches its
        // encoded postings bytes. Keep estimated index I/O in startup and move
        // only per-candidate traversal CPU into run cost for unordered scans.
        let index_run = if private.ordering.is_some() {
            0.0
        } else {
            index.candidates * pg_sys::cpu_index_tuple_cost
        };
        let mut startup =
            (index.total - index_run).max(index.startup) + (*rel).baserestrictcost.startup;
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
        path.path.total_cost =
            startup + index_run + heap_pages * cost_per_page + index.candidates * per_tuple;
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

/// Limit hints are useful only below a simple ranked SELECT. Avoid importing
/// a bound through joins, aggregation, DISTINCT, window functions or SRFs.
unsafe fn runtime_bound_shape(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
) -> bool {
    unsafe {
        let query = &*(*root).parse;
        if query.hasAggs
            || query.hasWindowFuncs
            || query.hasTargetSRFs
            || !query.groupClause.is_null()
            || !query.groupingSets.is_null()
            || !query.distinctClause.is_null()
            || !query.havingQual.is_null()
            || !query.setOperations.is_null()
            || query.jointree.is_null()
            || pg_sys::list_length((*query.jointree).fromlist) != 1
        {
            return false;
        }
        let from = pg_sys::list_nth((*query.jointree).fromlist, 0).cast::<pg_sys::Node>();
        (*from).type_ == pg_sys::NodeTag::T_RangeTblRef
            && (*from.cast::<pg_sys::RangeTblRef>()).rtindex as u32 == (*rel).relid
    }
}

unsafe fn safe_bound_expr(expr: *mut pg_sys::Node) -> bool {
    unsafe {
        if expr.is_null() {
            return false;
        }
        match (*expr).type_ {
            pg_sys::NodeTag::T_Const => {
                (*expr.cast::<pg_sys::Const>()).consttype == pg_sys::INT8OID
            }
            pg_sys::NodeTag::T_Param => {
                let param = &*expr.cast::<pg_sys::Param>();
                param.paramkind == pg_sys::ParamKind::PARAM_EXTERN
                    && param.paramtype == pg_sys::INT8OID
            }
            _ => false,
        }
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn plan_search_path(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    best_path: *mut pg_sys::CustomPath,
    tlist: *mut pg_sys::List,
    clauses: *mut pg_sys::List,
    _custom_plans: *mut pg_sys::List,
) -> *mut pg_sys::Plan {
    unsafe {
        let (private, clause) = Private::from_list((*best_path).custom_private);
        let mut scan = pgrx::PgBox::<pg_sys::CustomScan>::alloc_node(pg_sys::NodeTag::T_CustomScan);
        scan.scan.plan.targetlist = tlist;
        scan.scan.plan.qual = remaining_quals(clauses, clause);
        scan.scan.scanrelid = (*rel).relid;
        scan.flags = 0;
        scan.custom_plans = std::ptr::null_mut();
        // Expressions belong in custom_exprs so PostgreSQL can account for
        // parameters during plan finalization and copy/serialization.
        let query = crate::operator::search_clause(clause.cast())
            .expect("search clause")
            .query;
        let mut expressions = PgList::<pg_sys::Node>::new();
        expressions.push(pg_sys::copyObjectImpl(query.cast()).cast());
        // Only copy simple external parameters/constants. Evaluating arbitrary
        // LIMIT expressions here could duplicate volatile work or depend on an
        // outer tuple. The enclosing Limit remains responsible for SQL errors.
        let parse = &*(*root).parse;
        // Residual filters can reject the initial top k and force a second,
        // exhaustive pass. Until candidate filtering participates in ranking,
        // keep generic filtered scans unbounded rather than adding that work.
        if private.ordering.is_some()
            && scan.scan.plan.qual.is_null()
            && runtime_bound_shape(root, rel)
            && safe_bound_expr(parse.limitCount)
            && (parse.limitOffset.is_null() || safe_bound_expr(parse.limitOffset))
            && ((*parse.limitCount).type_ == pg_sys::NodeTag::T_Param
                || (!parse.limitOffset.is_null()
                    && (*parse.limitOffset).type_ == pg_sys::NodeTag::T_Param))
        {
            expressions.push(pg_sys::copyObjectImpl(parse.limitCount.cast()).cast());
            if !parse.limitOffset.is_null() {
                expressions.push(pg_sys::copyObjectImpl(parse.limitOffset.cast()).cast());
            }
        }
        scan.custom_exprs = expressions.into_pg();
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
        if (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION || byte((*rte).relkind) != b'r' {
            return;
        }
        let Some(found) = find_match(input_rel, rte, false) else {
            return;
        };
        let private = Private {
            index_oid: found.index_oid.to_u32(),
            heap_oid: (*rte).relid.to_u32(),
            query: found.query.clone().unwrap_or_else(|| "<parameter>".into()),
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
        let estimate = found
            .query
            .as_deref()
            .and_then(|query| crate::selectivity::estimate_query(found.index_oid, query))
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
    /// of inexact plans and for the heap fallback.
    clause: *mut pg_sys::ExprState,
    runtime_query: *mut pg_sys::ExprState,
    query_bound: bool,
    runtime_limit: *mut pg_sys::ExprState,
    runtime_offset: *mut pg_sys::ExprState,
    bounds_bound: bool,
    query_null: bool,
    /// Whether this execution scans the heap instead of the index, decided
    /// once on first access (see [`heap_fallback`]).
    heap_fallback: Option<bool>,
    fetch: *mut pg_sys::IndexFetchTableData,
    /// A heap tuple slot for the count node, whose scan slot is virtual.
    fetch_slot: *mut pg_sys::TupleTableSlot,
    heap: pg_sys::Relation,
    /// Some candidate came from an inexact plan and must pass `clause`.
    recheck: bool,
    /// Candidates in output order, filled on first execution.
    tids: Vec<Tid>,
    /// Unordered scans retain only their stream and current page.
    stream: Option<crate::stream::CandidateStream>,
    stream_visits: usize,
    /// Scores aligned with `tids` for an ordered scan; only the first
    /// `sorted` entries are in order, the rest are sorted if ever reached.
    scores: Vec<f32>,
    sorted: usize,
    next: usize,
    started: bool,
    /// Heap scan for the fallback.
    fallback: *mut pg_sys::TableScanDescData,
    /// `tids` holds only the pruned top k; the rest are produced on demand.
    pruned: bool,
    /// The k the pruned rows were found for, which a completion deepens.
    pruned_k: usize,
    /// The pruned walk ran over the ordinal streams.
    ordinal_walk: bool,
    /// Every location emitted before a completion, across completions.
    emitted: FxHashSet<Tid>,
    /// Explain counters. Candidates are unknown while pruned; `scored`
    /// counts the candidates a pruned scan scored.
    candidates: Option<usize>,
    scored: Option<usize>,
    /// Cumulative work across rescans, like `fetched`. Counts calls made by
    /// exhaustive ranking, not block-max traversal or score projection.
    exhaustive_score_calls: usize,
    top_k_completions: usize,
    fetched: usize,
    skipped_pages: usize,
    page_masks: Option<bool>,
    count_fold: bool,
    selection_calls: usize,
    selection_time: std::time::Duration,
    estimation_time: std::time::Duration,
    estimation_calls: usize,
    selection_threshold: i32,
    count_estimate: Option<tinql::runtime::plan::CountEstimate>,
    ordered: bool,
    /// Identity under which the scan publishes its scorer.
    scan_id: u64,
}

impl Drop for ScanExec {
    /// Runs when the executor's query context is deleted, including after an
    /// error, so a scan's scorer never outlives the scan.
    fn drop(&mut self) {
        crate::score::forget_scan_scorer(self.scan_id);
    }
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
        let expressions = (*cscan).custom_exprs;
        let expression = |index: i32| {
            if pg_sys::list_length(expressions) > index {
                pg_sys::list_nth(expressions, index).cast::<pg_sys::Expr>()
            } else {
                std::ptr::null_mut()
            }
        };
        let query = expression(0);
        let runtime_query = if !query.is_null() && (*query).type_ == pg_sys::NodeTag::T_Param {
            pg_sys::ExecInitExpr(query, node.cast())
        } else {
            std::ptr::null_mut()
        };
        let runtime_limit = pg_sys::ExecInitExpr(expression(1), node.cast());
        let runtime_offset = pg_sys::ExecInitExpr(expression(2), node.cast());
        let ordered = private.ordering.is_some();
        let exec = ScanExec {
            private,
            clause,
            runtime_query,
            query_bound: runtime_query.is_null(),
            runtime_limit,
            runtime_offset,
            bounds_bound: runtime_limit.is_null(),
            query_null: false,
            heap_fallback: None,
            fetch,
            fetch_slot,
            heap,
            recheck: false,
            tids: Vec::new(),
            stream: None,
            stream_visits: 0,
            scores: Vec::new(),
            sorted: 0,
            next: 0,
            started: false,
            fallback: std::ptr::null_mut(),
            pruned: false,
            pruned_k: 0,
            ordinal_walk: false,
            emitted: FxHashSet::default(),
            candidates: None,
            scored: None,
            exhaustive_score_calls: 0,
            top_k_completions: 0,
            fetched: 0,
            skipped_pages: 0,
            page_masks: None,
            count_fold: false,
            selection_calls: 0,
            selection_time: std::time::Duration::ZERO,
            estimation_time: std::time::Duration::ZERO,
            estimation_calls: 0,
            selection_threshold: 0,
            count_estimate: None,
            ordered,
            scan_id: crate::score::scan_id(),
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
/// k, and only those k rows are materialized. Ranked queries the scorer cannot
/// bound enumerate and score every candidate; unordered scans use a stream.
unsafe fn gather(exec: &mut ScanExec) {
    unsafe {
        exec.scores.clear();
        exec.pruned = false;
        exec.scored = None;
        let mut scorer = exec.private.ordering.as_ref().map(|ordering| {
            crate::score::scorer_for_scan(
                exec.scan_id,
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
            && scorer
                .as_ref()
                .is_some_and(|scorer| scorer.scores_nothing())
        {
            // Every match scores zero and ties rank in heap order, so the
            // best k are the first k of the heap-ordered stream: a sentence of
            // stopwords need not collect and sort its million matches.
            let view = crate::storage::view(pg_sys::Oid::from(exec.private.index_oid));
            let mut stream = crate::stream::CandidateStream::new(view, scan_query(exec));
            if !stream.recheck {
                let mut rows = Vec::with_capacity(k);
                while rows.len() < k {
                    pgrx::check_for_interrupts!();
                    match stream.next() {
                        Some(tid) => rows.push((0.0_f32, tid)),
                        None => break,
                    }
                }
                let complete = rows.len() < k;
                exec.candidates = complete.then_some(rows.len());
                exec.scored = Some(0);
                exec.scores = vec![0.0; rows.len()];
                exec.tids = rows.iter().map(|(_, tid)| *tid).collect();
                exec.sorted = exec.tids.len();
                exec.pruned = !complete;
                exec.pruned_k = k;
                let scorer = scorer.take().expect("checked above");
                crate::score::publish_scan_scorer(exec.scan_id, scorer, &rows);
                exec.next = 0;
                exec.started = true;
                return;
            }
        }
        if let Some(k) = top_k
            && k <= crate::score::PRUNE_MAX_K
            && let Some(top) = scorer.as_mut().and_then(|scorer| top_rows(exec, scorer, k))
        {
            exec.candidates = top.complete.then_some(top.rows.len());
            if top.streamed {
                exec.candidates = Some(top.scored);
                exec.exhaustive_score_calls += top.scored;
            } else {
                exec.scored = Some(top.scored);
            }
            exec.scores = top.rows.iter().map(|(score, _)| *score).collect();
            exec.tids = top.rows.iter().map(|(_, tid)| *tid).collect();
            exec.sorted = exec.tids.len();
            exec.pruned = !top.complete;
            exec.pruned_k = k;
            exec.ordinal_walk = top.ordinal;
            let scorer = scorer.take().expect("a top k needs a scorer");
            crate::score::publish_scan_scorer(exec.scan_id, scorer, &top.rows);
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

/// The scorer's pruned top `k`. When a disjunction's positive scores are
/// fewer than `k`, the rest are matches of its elided terms alone: they tie at
/// zero and rank in heap order, which is the order the candidate stream
/// yields, so they are read from it instead of scoring every match. A query
/// of common words and one rare word otherwise scored millions of rows as
/// soon as an updated row's dead version took a place in the first top k.
///
/// A query the scorer cannot prune, such as a phrase, is scored from the
/// candidate stream instead, holding only the top `k`.
unsafe fn top_rows(
    exec: &ScanExec,
    scorer: &mut crate::score::IndexScorer,
    k: usize,
) -> Option<crate::score::TopK> {
    let Some(mut top) = scorer.top_k(k) else {
        let view = unsafe { crate::storage::view(pg_sys::Oid::from(exec.private.index_oid)) };
        let mut stream = crate::stream::CandidateStream::new(view, unsafe { scan_query(exec) });
        return scorer.top_k_streamed(&mut stream, k);
    };
    if top.zero_fill {
        let view = unsafe { crate::storage::view(pg_sys::Oid::from(exec.private.index_oid)) };
        let mut stream = crate::stream::CandidateStream::new(view, unsafe { scan_query(exec) });
        if stream.recheck {
            return None;
        }
        let positive: FxHashSet<Tid> = top.rows.iter().map(|(_, tid)| *tid).collect();
        while top.rows.len() < k {
            pgrx::check_for_interrupts!();
            match stream.next() {
                Some(tid) if positive.contains(&tid) => {}
                Some(tid) => top.rows.push((0.0, tid)),
                None => break,
            }
        }
        top.complete = top.rows.len() < k;
    }
    Some(top)
}

unsafe fn scan_query(exec: &ScanExec) -> Query {
    unsafe {
        let index_oid = pg_sys::Oid::from(exec.private.index_oid);
        let index = pg_sys::index_open(index_oid, pg_sys::AccessShareLock as _);
        let tokenizer = crate::storage::index_tokenizer(index);
        pg_sys::index_close(index, pg_sys::AccessShareLock as _);
        let query: Query =
            tinql::runtime::parse_tinql_to_query(&exec.private.query, tokenizer.as_ref())
                .unwrap_or_else(|error| pgrx::error!("invalid ==> query: {error}"));
        query
    }
}

unsafe fn start_stream(exec: &mut ScanExec) {
    unsafe {
        let query = scan_query(exec);
        let view = crate::storage::view(pg_sys::Oid::from(exec.private.index_oid));
        let stream = crate::stream::CandidateStream::new(view, query);
        exec.recheck = stream.recheck;
        exec.stream = Some(stream);
        exec.started = true;
    }
}

/// Every matching TID across the index's sources, in heap order.
unsafe fn candidates(exec: &mut ScanExec) -> Vec<Tid> {
    unsafe {
        let index_oid = pg_sys::Oid::from(exec.private.index_oid);
        let query = scan_query(exec);
        let view = crate::storage::view(index_oid);
        candidates_in_view(exec, &query, &view)
    }
}

fn candidates_in_view(exec: &mut ScanExec, query: &Query, view: &crate::storage::View) -> Vec<Tid> {
    let limits = Limits::default();
    let mut tids = Vec::new();
    for ((segment, dead), label) in view.sources.iter().zip(&view.labels) {
        pgrx::check_for_interrupts!();
        let planned = plan(query, segment, &limits)
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

/// Stores every candidate; with a scorer, scored and in output order.
fn finish(exec: &mut ScanExec, mut tids: Vec<Tid>, scorer: Option<crate::score::IndexScorer>) {
    exec.candidates = Some(tids.len());
    exec.scores.clear();
    exec.sorted = tids.len();
    if let Some(mut scorer) = scorer {
        let top_k = exec.private.ordering.as_ref().and_then(|o| o.top_k);
        let mut scored: Vec<(f32, Tid)> =
            tids.iter().map(|tid| (scorer.score(*tid), *tid)).collect();
        exec.exhaustive_score_calls += scored.len();
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
        // Every candidate's score, not only the ordered prefix: rows past it
        // are emitted too once the parent reads that far.
        crate::score::publish_scan_scorer(exec.scan_id, scorer, &scored);
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
        exec.top_k_completions += 1;
        let ordering = exec
            .private
            .ordering
            .as_ref()
            .expect("pruned scans are ordered");
        let mut scorer = crate::score::scorer_for_scan(
            exec.scan_id,
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
        // A scan can complete more than once, so the consumed locations accumulate.
        let emitted: Vec<Tid> = exec.tids[..exec.next].to_vec();
        exec.emitted.extend(emitted);
        let consumed = exec.emitted.clone();
        // Rows of the top k that the snapshot cannot see, such as the dead
        // version an update leaves in the index beside its successor with the
        // same score, send the parent past k. The same scorer's top 4k, 16k
        // and so on extend the rows already emitted, so the search deepens
        // before it gives up pruning and scores every match.
        let deeper = exec.pruned_k.saturating_mul(4).max(40);
        if exec.pruned_k > 0 && deeper <= crate::score::PRUNE_MAX_K {
            let rows = if scorer.scores_nothing() {
                let view = crate::storage::view(pg_sys::Oid::from(exec.private.index_oid));
                let mut stream = crate::stream::CandidateStream::new(view, scan_query(exec));
                (!stream.recheck).then(|| {
                    let mut rows = Vec::with_capacity(deeper);
                    while rows.len() < deeper {
                        match stream.next() {
                            Some(tid) => rows.push((0.0_f32, tid)),
                            None => break,
                        }
                    }
                    let complete = rows.len() < deeper;
                    (rows, complete)
                })
            } else {
                top_rows(exec, &mut scorer, deeper).map(|top| {
                    if top.streamed {
                        exec.exhaustive_score_calls += top.scored;
                    }
                    (top.rows, top.complete)
                })
            };
            if let Some((mut rows, complete)) = rows {
                crate::score::publish_scan_scorer(exec.scan_id, scorer, &rows);
                rows.retain(|(_, tid)| !consumed.contains(tid));
                exec.scores = rows.iter().map(|(score, _)| *score).collect();
                exec.tids = rows.iter().map(|(_, tid)| *tid).collect();
                exec.sorted = exec.tids.len();
                exec.next = 0;
                exec.pruned = !complete;
                exec.pruned_k = deeper;
                return;
            }
        }
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

/// Whether this execution reads the heap instead of the index. The path was
/// planned when index reads were allowed; they can have become unavailable
/// since (a prepared plan executed on a standby whose primary stopped logging
/// removal horizons), and once decided the choice holds for the execution.
unsafe fn heap_fallback(exec: &mut ScanExec) -> bool {
    if let Some(fallback) = exec.heap_fallback {
        return fallback;
    }
    let fallback =
        !unsafe { crate::storage::is_segmented(pg_sys::Oid::from(exec.private.index_oid)) };
    exec.heap_fallback = Some(fallback);
    fallback
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

/// A checked hint, never a truncation: visibility checks or remaining quals
/// can reject the first k candidates, in which case `complete` supplies the rest.
fn runtime_top_k(limit: Option<i64>, offset: Option<i64>) -> Option<usize> {
    let limit = limit?;
    let offset = offset.unwrap_or(0);
    if limit < 0 || offset < 0 {
        return None;
    }
    usize::try_from(limit.checked_add(offset)?).ok()
}

unsafe fn bind_bounds(exec: &mut ScanExec, context: *mut pg_sys::ExprContext) {
    unsafe {
        if exec.bounds_bound {
            return;
        }
        let evaluate = |expr: *mut pg_sys::ExprState| {
            if expr.is_null() {
                return None;
            }
            let mut is_null = false;
            let value = pg_sys::ExecEvalExprSwitchContext(expr, context, &mut is_null);
            i64::from_datum(value, is_null)
        };
        let limit = evaluate(exec.runtime_limit);
        let offset = evaluate(exec.runtime_offset);
        if let Some(ordering) = &mut exec.private.ordering {
            ordering.top_k = runtime_top_k(limit, offset);
        }
        exec.bounds_bound = true;
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
        if !exec.query_bound {
            let mut is_null = false;
            let value = pg_sys::ExecEvalExprSwitchContext(
                exec.runtime_query,
                (*scan).ps.ps_ExprContext,
                &mut is_null,
            );
            exec.query_null = is_null;
            if let Some(query) = String::from_datum(value, is_null) {
                exec.private.query = query;
            }
            exec.query_bound = true;
        }
        // ==> is strict: NULL has no matches and must never be parsed as text.
        if exec.query_null {
            exec.started = true;
            return pg_sys::ExecClearTuple(slot);
        }
        bind_bounds(exec, (*scan).ps.ps_ExprContext);
        if heap_fallback(exec) {
            return fallback_access(scan, exec, slot);
        }
        if !exec.started {
            if exec.ordered {
                gather(exec);
            } else {
                start_stream(exec);
            }
        }
        loop {
            pgrx::check_for_interrupts!();
            let tid = if let Some(stream) = &mut exec.stream {
                let Some(tid) = stream.next() else {
                    break;
                };
                exec.stream_visits += 1;
                tid
            } else {
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
                tid
            };
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
                        if exec.ordered {
                            let member = (*slot).tts_tid;
                            let block = (u32::from(member.ip_blkid.bi_hi) << 16)
                                | u32::from(member.ip_blkid.bi_lo);
                            let member = Tid {
                                block,
                                offset: member.ip_posid,
                            };
                            crate::score::note_scan_emitted(exec.scan_id, member, tid);
                        }
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

/// The fallback reads the whole heap and evaluates the original clause.
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

/// Counts one exact candidate page; only all-visible pages can bypass the heap.
unsafe fn count_page(
    node: *mut pg_sys::CustomScanState,
    exec: &mut ScanExec,
    block: u32,
    offsets: impl Iterator<Item = u16>,
    size: usize,
    vmbuf: &mut pg_sys::Buffer,
) -> i64 {
    unsafe {
        pgrx::check_for_interrupts!();
        let status = pg_sys::visibilitymap_get_status(exec.heap, block, vmbuf);
        if !exec.recheck && status & pg_sys::VISIBILITYMAP_ALL_VISIBLE as u8 != 0 {
            exec.skipped_pages += 1;
            return size as i64;
        }
        let snapshot = (*(*node).ss.ps.state).es_snapshot;
        let mut count = 0;
        for offset in offsets {
            let mut pointer = pointer_of(Tid { block, offset });
            let mut call_again = false;
            let mut all_dead = false;
            loop {
                if pg_sys::table_index_fetch_tuple(
                    exec.fetch,
                    &mut pointer,
                    snapshot,
                    exec.fetch_slot,
                    &mut call_again,
                    &mut all_dead,
                ) {
                    exec.fetched += 1;
                    if !exec.recheck || passes_clause(node, exec, exec.fetch_slot) {
                        count += 1;
                    }
                    break;
                }
                if !call_again {
                    break;
                }
            }
        }
        count
    }
}

/// Counts every offset of `block` that is visible and passes the recheck,
/// always through the heap.
unsafe fn count_fetched(
    node: *mut pg_sys::CustomScanState,
    exec: &mut ScanExec,
    block: u32,
    offsets: impl Iterator<Item = u16>,
) -> i64 {
    unsafe {
        let snapshot = (*(*node).ss.ps.state).es_snapshot;
        let mut count = 0;
        if !exec.recheck && (*exec.heap).rd_tableam == pg_sys::GetHeapamTableAmRoutine() {
            // No tuple is needed, only whether each chain has a visible
            // member: one buffer read and one lock serve the whole page.
            let buffer = pg_sys::ReadBuffer(exec.heap, block);
            pg_sys::LockBuffer(buffer, pg_sys::BUFFER_LOCK_SHARE as i32);
            let mut tuple = std::mem::zeroed::<pg_sys::HeapTupleData>();
            for offset in offsets {
                let mut pointer = pointer_of(Tid { block, offset });
                if pg_sys::heap_hot_search_buffer(
                    &mut pointer,
                    exec.heap,
                    buffer,
                    snapshot,
                    &mut tuple,
                    std::ptr::null_mut(),
                    true,
                ) {
                    count += 1;
                }
                exec.fetched += 1;
            }
            pg_sys::UnlockReleaseBuffer(buffer);
            return count;
        }
        for offset in offsets {
            let mut pointer = pointer_of(Tid { block, offset });
            let mut call_again = false;
            let mut all_dead = false;
            loop {
                if pg_sys::table_index_fetch_tuple(
                    exec.fetch,
                    &mut pointer,
                    snapshot,
                    exec.fetch_slot,
                    &mut call_again,
                    &mut all_dead,
                ) {
                    exec.fetched += 1;
                    if !exec.recheck || passes_clause(node, exec, exec.fetch_slot) {
                        count += 1;
                    }
                    break;
                }
                if !call_again {
                    break;
                }
            }
        }
        count
    }
}

/// Counts by folding each `LSG4` segment's ordinal streams (see
/// [`crate::fold`]). The visibility map is read once, after the view; if a
/// dead list was published in between, the count starts over, and after
/// repeated interference it trusts no page. The write buffer and segments
/// without ordinal streams are counted through their TID postings, against
/// the heap: the buffer's removals are not covered by the view check.
unsafe fn fold_count(
    node: *mut pg_sys::CustomScanState,
    exec: &mut ScanExec,
    query: &Query,
    index_oid: pg_sys::Oid,
    mut view: crate::storage::View,
) -> i64 {
    unsafe {
        let mut attempts = 0;
        let visibility = loop {
            let visibility = crate::fold::Visibility::read(exec.heap);
            if crate::storage::view_is_current(index_oid, &view) {
                break visibility;
            }
            attempts += 1;
            if attempts == 3 {
                break crate::fold::Visibility::none();
            }
            view = crate::storage::view(index_oid);
        };
        let limits = Limits::default();
        let mut count = 0i64;
        let mut candidates = 0usize;
        for (i, ((source, _), label)) in view.sources.iter().zip(&view.labels).enumerate() {
            pgrx::check_for_interrupts!();
            if i < view.immutable_sources {
                let mut pending: Vec<(u32, Vec<u16>)> = Vec::new();
                let sure = crate::storage::codec_in(
                    crate::fold::count_segment(
                        view.keys[i],
                        source.as_ref(),
                        &view.dead_sets[i],
                        query,
                        &visibility,
                        |block, offsets| pending.push((block, offsets.to_vec())),
                    ),
                    label,
                );
                if let Some(sure) = sure {
                    exec.skipped_pages += usize::from(sure > 0);
                    candidates += sure as usize;
                    count += sure as i64;
                    for (block, offsets) in pending {
                        candidates += offsets.len();
                        count += count_fetched(node, exec, block, offsets.into_iter());
                    }
                    continue;
                }
            }
            let planned = plan(query, source, &limits)
                .unwrap_or_else(|error| pgrx::error!("Stannum query plan: {error}"));
            let dead = &view.dead_sets[i];
            let mut cursor = planned.cursor;
            let mut tids = Vec::new();
            while let Some(tid) = cursor.current() {
                if !dead.contains(&tid) {
                    tids.push(tid);
                }
                crate::storage::codec_in(cursor.advance(), label);
            }
            candidates += tids.len();
            for page in tids.chunk_by(|a, b| a.block == b.block) {
                count +=
                    count_fetched(node, exec, page[0].block, page.iter().map(|tid| tid.offset));
            }
        }
        exec.candidates = Some(candidates);
        count
    }
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
        if heap_fallback(exec) {
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
            let index_oid = pg_sys::Oid::from(exec.private.index_oid);
            let index = pg_sys::index_open(index_oid, pg_sys::AccessShareLock as _);
            let tokenizer = crate::storage::index_tokenizer(index);
            pg_sys::index_close(index, pg_sys::AccessShareLock as _);
            let query =
                tinql::runtime::parse_tinql_to_query(&exec.private.query, tokenizer.as_ref())
                    .unwrap_or_else(|error| pgrx::error!("invalid ==> query: {error}"));
            let view = crate::storage::view(index_oid);
            // The diagnostic settings ask for the older strategies by name.
            let diagnostic = FORCE_COUNT_PAGES.get()
                || COUNT_PAGE_THRESHOLD.get() > 0
                || PROFILE_COUNT_SELECTION.get();
            if COUNT_FOLD.get() && !diagnostic && !exec.recheck && crate::fold::supported(&query) {
                exec.count_fold = true;
                count = fold_count(node, exec, &query, index_oid, view);
            } else {
                // Time the existing policy too, so paired runs expose incremental cost.
                let selection_start = std::time::Instant::now();
                let mut use_pages = FORCE_COUNT_PAGES.get();
                for (source, _) in &view.sources {
                    if use_pages {
                        break;
                    }
                    use_pages |= tinql::runtime::plan::prefers_pages(&query, source)
                        .unwrap_or_else(|error| pgrx::error!("Stannum query plan: {error}"));
                    if use_pages {
                        break;
                    }
                }
                exec.count_estimate = None;
                exec.selection_threshold = COUNT_PAGE_THRESHOLD.get();
                if !FORCE_COUNT_PAGES.get()
                    && (PROFILE_COUNT_SELECTION.get() || COUNT_PAGE_THRESHOLD.get() > 0)
                {
                    // Start before allocating the source list or reading any metadata.
                    let estimation_start = std::time::Instant::now();
                    let estimate = if view.sources.len() > 512 {
                        tinql::runtime::plan::CountEstimate {
                            sources: view.sources.len(),
                            ..Default::default()
                        }
                    } else {
                        let sources: Vec<_> =
                            view.sources.iter().map(|(source, _)| source).collect();
                        tinql::runtime::plan::estimate_count_disjunction(&query, &sources)
                            .unwrap_or_else(|error| {
                                pgrx::error!("Stannum count estimation: {error}")
                            })
                    };
                    use_pages = estimate.choose_pages(use_pages, exec.selection_threshold as u64);
                    exec.count_estimate = Some(estimate);
                    exec.estimation_calls += 1;
                    exec.estimation_time += estimation_start.elapsed();
                }
                exec.selection_calls += 1;
                exec.selection_time += selection_start.elapsed();
                exec.page_masks = Some(use_pages);
                let mut vmbuf = pg_sys::InvalidBuffer as pg_sys::Buffer;
                let mut candidates = 0usize;
                if !use_pages {
                    let tids = candidates_in_view(exec, &query, &view);
                    candidates = tids.len();
                    let mut i = 0;
                    while i < tids.len() {
                        let block = tids[i].block;
                        let mut end = i + 1;
                        while end < tids.len() && tids[end].block == block {
                            end += 1;
                        }
                        count += count_page(
                            node,
                            exec,
                            block,
                            tids[i..end].iter().map(|tid| tid.offset),
                            end - i,
                            &mut vmbuf,
                        );
                        i = end;
                    }
                } else {
                    let mut sources: Vec<Box<dyn segment::pages::Cursor>> = Vec::new();
                    for ((source, dead), label) in view.sources.iter().zip(&view.labels) {
                        pgrx::check_for_interrupts!();
                        let planned =
                            tinql::runtime::plan::page_plan(&query, source, &Limits::default())
                                .unwrap_or_else(|error| {
                                    pgrx::error!("Stannum query plan: {error}")
                                });
                        exec.recheck |= !planned.exact;
                        let mut cursor = planned.cursor;
                        if let Some(dead) = dead {
                            let dead = crate::storage::codec_in(
                                segment::postings::Postings::parse(dead).and_then(|p| p.pages()),
                                &format!("{label} dead list"),
                            );
                            cursor = Box::new(crate::storage::codec_in(
                                segment::pages::Difference::new(cursor, dead),
                                label,
                            ));
                        }
                        sources.push(cursor);
                    }
                    use segment::pages::Cursor as _;
                    // Union deduplicates across segments before visibility checks, with
                    // one offset mask per source rather than a sorted vector of TIDs.
                    let mut pages = segment::pages::Union::new(sources);
                    while let Some(page) = pages.current() {
                        let size = page.offsets.count() as usize;
                        candidates += size;
                        count += count_page(
                            node,
                            exec,
                            page.block,
                            page.offsets.iter(),
                            size,
                            &mut vmbuf,
                        );
                        crate::storage::codec_in(pages.advance(), "count page stream");
                    }
                }
                exec.candidates = Some(candidates);
                if vmbuf != pg_sys::InvalidBuffer as pg_sys::Buffer {
                    pg_sys::ReleaseBuffer(vmbuf);
                }
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
        let parameterized = !exec.runtime_query.is_null() || !exec.runtime_limit.is_null();
        // Completion removed already-consumed roots from the cached arrays.
        // A constant rescan must rebuild those arrays, retaining its scorer's
        // frozen view/statistics. Merely rewinding would omit qualifying rows.
        if parameterized || exec.top_k_completions > 0 {
            if parameterized {
                crate::score::forget_scan_scorer(exec.scan_id);
            }
            exec.query_bound = exec.runtime_query.is_null();
            exec.bounds_bound = exec.runtime_limit.is_null();
            exec.query_null = false;
            exec.started = false;
            exec.tids.clear();
            exec.scores.clear();
            exec.sorted = 0;
            exec.pruned = false;
            exec.pruned_k = 0;
            exec.emitted.clear();
            exec.recheck = false;
            exec.candidates = None;
            exec.scored = None;
        }
        if let Some(stream) = &mut exec.stream {
            stream.rewind();
            exec.recheck = stream.recheck;
        }
        if !exec.fallback.is_null() {
            pg_sys::table_rescan(exec.fallback, std::ptr::null_mut());
        }
        // Counts re-count; intact constant searches rewind their captured results.
        // Parameterized ranked searches bind again and rebuild their scorer.
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
            if let Some(stream) = &exec.stream {
                pg_sys::ExplainPropertyText(
                    c"Candidate Strategy".as_ptr(),
                    if stream.page_masks {
                        c"streaming page bitmaps".as_ptr()
                    } else {
                        c"streaming scalar".as_ptr()
                    },
                    es,
                );
                pg_sys::ExplainPropertyInteger(
                    c"Candidates Visited".as_ptr(),
                    std::ptr::null(),
                    exec.stream_visits as i64,
                    es,
                );
            }
            if exec.selection_calls > 0 {
                pg_sys::ExplainPropertyInteger(
                    c"Count Estimation Calls".as_ptr(),
                    std::ptr::null(),
                    exec.estimation_calls as i64,
                    es,
                );
                pg_sys::ExplainPropertyInteger(
                    c"Count Page Threshold (Last)".as_ptr(),
                    std::ptr::null(),
                    exec.selection_threshold as i64,
                    es,
                );
                pg_sys::ExplainPropertyInteger(
                    c"Count Selection Calls".as_ptr(),
                    std::ptr::null(),
                    exec.selection_calls as i64,
                    es,
                );
                pg_sys::ExplainPropertyFloat(
                    c"Count Selection Time".as_ptr(),
                    c"ms".as_ptr(),
                    exec.selection_time.as_secs_f64() * 1000.0,
                    6,
                    es,
                );
                pg_sys::ExplainPropertyFloat(
                    c"Count Estimation Time".as_ptr(),
                    c"ms".as_ptr(),
                    exec.estimation_time.as_secs_f64() * 1000.0,
                    6,
                    es,
                );
            }
            if let Some(estimate) = exec.count_estimate {
                pg_sys::ExplainPropertyBool(
                    c"Count Estimate Supported (Last)".as_ptr(),
                    estimate.supported,
                    es,
                );
                if let Some(minimum) = estimate.min_postings {
                    pg_sys::ExplainPropertyInteger(
                        c"Count Estimate Minimum DF (Last)".as_ptr(),
                        std::ptr::null(),
                        i64::from(minimum),
                        es,
                    );
                }
                for (label, value) in [
                    (c"Count Estimate Leaves (Last)", estimate.leaves as i64),
                    (c"Count Estimate Sources (Last)", estimate.sources as i64),
                    (c"Count Estimate Lookups (Last)", estimate.lookups as i64),
                    (c"Count Estimate Postings (Last)", estimate.postings as i64),
                ] {
                    pg_sys::ExplainPropertyInteger(label.as_ptr(), std::ptr::null(), value, es);
                }
            }
            if exec.count_fold {
                pg_sys::ExplainPropertyText(
                    c"Count Strategy".as_ptr(),
                    c"ordinal fold".as_ptr(),
                    es,
                );
            } else if let Some(pages) = exec.page_masks {
                pg_sys::ExplainPropertyText(
                    c"Count Strategy".as_ptr(),
                    if pages {
                        c"page bitmaps".as_ptr()
                    } else {
                        c"scalar".as_ptr()
                    },
                    es,
                );
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
                pg_sys::ExplainPropertyText(
                    c"Pruning".as_ptr(),
                    if exec.ordinal_walk {
                        c"ordinal".as_ptr()
                    } else {
                        c"block-max".as_ptr()
                    },
                    es,
                );
                pg_sys::ExplainPropertyInteger(
                    c"Scored Candidates".as_ptr(),
                    std::ptr::null(),
                    scored as i64,
                    es,
                );
            }
            if exec.ordered {
                pg_sys::ExplainPropertyInteger(
                    c"Exhaustive Score Calls".as_ptr(),
                    std::ptr::null(),
                    exec.exhaustive_score_calls as i64,
                    es,
                );
                pg_sys::ExplainPropertyInteger(
                    c"Top-K Completions".as_ptr(),
                    std::ptr::null(),
                    exec.top_k_completions as i64,
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
