//! Planner estimates for `==>` from the index's own statistics.
//!
//! Three planner entry points must agree on how many rows a `==>` predicate
//! matches: the operator's restriction selectivity (which sets the relation's
//! row estimate), `amcostestimate` for the bitmap index path, and the custom
//! scan paths. All three call [`estimate_query`], which parses the constant
//! query with the index's tokenizer and combines per-term document
//! frequencies from every segment and the write buffer (see
//! [`tinql::runtime::estimate`]).
//!
//! Reading the index at plan time is cheap: segment readers are cached per
//! backend, so a dictionary lookup is a memoized read, and the result is
//! memoized here per statement, keyed by index, query text and the index's
//! content [`Stamp`]. When the index cannot be read at plan time (recovery,
//! no LDP2 storage, an unparsable query) the caller falls back to the
//! historical constants in [`FALLBACK`].

use std::cell::RefCell;
use std::collections::HashMap;

use pgrx::{PgList, pg_extern, pg_sys};
use tinql::runtime::estimate::{Estimate, IndexStatistics, estimate};
use tinql::runtime::plan::Limits;

use crate::storage::Stamp;

/// The estimate used when the index gives no better answer: what the
/// planner assumed for every `==>` predicate before index statistics.
pub const FALLBACK: Estimate = Estimate {
    selectivity: 0.1,
    candidates: 0.1,
    exact: true,
};

thread_local! {
    static MEMO: RefCell<HashMap<(u32, String), (Stamp, Estimate)>> = RefCell::new(HashMap::new());
}

/// Forgets the memo at the start of an executor run, so it never outlives
/// the statement that filled it.
pub fn note_executor_start() {
    MEMO.with_borrow_mut(HashMap::clear);
}

/// The estimate for `query` over the index `index_oid`, or `None` when the
/// index is not readable at plan time.
///
/// # Safety
/// `index_oid` names an index relation the caller may open.
pub unsafe fn estimate_query(index_oid: pg_sys::Oid, query: &str) -> Option<Estimate> {
    unsafe {
        if pg_sys::RecoveryInProgress() {
            return None;
        }
        let stamp = crate::storage::stamp(index_oid)?;
        let key = (index_oid.to_u32(), query.to_owned());
        let memoized = MEMO.with_borrow(|memo| memo.get(&key).copied());
        if let Some((at, estimate)) = memoized
            && at == stamp
        {
            return Some(estimate);
        }
        let index = pg_sys::index_open(index_oid, pg_sys::AccessShareLock as _);
        let tokenizer = crate::storage::index_tokenizer(index);
        pg_sys::index_close(index, pg_sys::AccessShareLock as _);
        let query = tinql::runtime::parse_tinql_to_query(query, tokenizer.as_ref()).ok()?;
        let view = crate::storage::view(index_oid);
        let statistics = IndexStatistics {
            sources: view.sources.iter().map(|(index, _)| &**index).collect(),
            max_expansion: Limits::default().max_expansion,
        };
        let estimate = estimate(&query, &statistics).ok()?;
        MEMO.with_borrow_mut(|memo| memo.insert(key, (stamp, estimate)));
        Some(estimate)
    }
}

/// Several `==>` clauses on one index are ANDed by the scan.
pub fn conjoin(estimates: &[Estimate]) -> Estimate {
    use tinql::runtime::estimate::conjunction;
    Estimate {
        selectivity: conjunction(estimates.iter().map(|e| e.selectivity)),
        candidates: conjunction(estimates.iter().map(|e| e.candidates)),
        exact: estimates.iter().all(|e| e.exact),
    }
}

/// The estimate for the `==>` clause `args` (`expr ==> 'query'`) in the
/// planner state `root`, when `expr` is answered by a segmented index.
///
/// # Safety
/// `root` is the planner state the clause belongs to; `args` is its
/// argument list.
pub unsafe fn clause_estimate(
    root: *mut pg_sys::PlannerInfo,
    args: *mut pg_sys::List,
) -> Option<Estimate> {
    unsafe {
        if root.is_null() || pg_sys::list_length(args) != 2 {
            return None;
        }
        let left = pg_sys::list_nth(args, 0).cast::<pg_sys::Node>();
        let right = pg_sys::list_nth(args, 1).cast::<pg_sys::Node>();
        let query = crate::operator::query_text(right)?;
        let relids = pg_sys::pull_varnos(root, left);
        if pg_sys::bms_membership(relids) != pg_sys::BMS_Membership::BMS_SINGLETON {
            return None;
        }
        let varno = pg_sys::bms_singleton_member(relids);
        if varno <= 0 || varno >= (*root).simple_rel_array_size {
            return None;
        }
        let rte = *(*root).simple_rte_array.add(varno as usize);
        if rte.is_null()
            || (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION
            || (*rte).relkind.to_ne_bytes()[0] != b'r'
        {
            return None;
        }
        let candidates = crate::score::matching_stannum_indexes((*rte).relid, varno, left);
        let index_oid = crate::score::pick_index(&candidates, crate::operator::bound_index(right))?;
        estimate_query(index_oid, &query)
    }
}

/// Restriction selectivity of `==>`: the fraction of the relation's rows the
/// query matches, from the index's statistics.
#[pg_extern(sql = "
    CREATE OR REPLACE FUNCTION @extschema@.stannum_text_restrict(internal, oid, internal, integer)
        RETURNS float8
        PARALLEL SAFE STRICT
        LANGUAGE c AS 'MODULE_PATHNAME', '@FUNCTION_NAME@';
")]
pub(crate) fn stannum_text_restrict(fcinfo: pg_sys::FunctionCallInfo) -> f64 {
    unsafe {
        let root = pgrx::pg_getarg_datum_raw(fcinfo, 0).cast_mut_ptr::<pg_sys::PlannerInfo>();
        let args = pgrx::pg_getarg_datum_raw(fcinfo, 2).cast_mut_ptr::<pg_sys::List>();
        clause_estimate(root, args)
            .unwrap_or(FALLBACK)
            .selectivity
            .clamp(0.0, 1.0)
    }
}

// --- Costs ----------------------------------------------------------------------

/// Postings bytes read per candidate document, averaged over boolean terms
/// (a delta-coded TID) and positional queries (positions too).
const POSTING_BYTES: f64 = 8.0;

/// The cost of reading the index for one query.
pub struct IndexCost {
    pub startup: f64,
    pub total: f64,
    /// Index pages read per scan.
    pub pages: f64,
    /// Candidate rows the index yields per scan.
    pub candidates: f64,
    pub random_page_cost: f64,
    pub seq_page_cost: f64,
}

/// Costs reading `index_oid` for a query with `estimate` over a relation of
/// `tuples` rows, `loop_count` times. Dictionary lookups touch a couple of
/// pages; postings pages grow with the candidates, never past the index.
///
/// # Safety
/// `root` is the current planner state; `index_oid` names an index relation
/// the caller may open.
pub unsafe fn index_cost(
    root: *mut pg_sys::PlannerInfo,
    index_oid: pg_sys::Oid,
    tablespace: pg_sys::Oid,
    estimate: &Estimate,
    tuples: f64,
    loop_count: f64,
    clauses: usize,
) -> IndexCost {
    unsafe {
        let index = pg_sys::index_open(index_oid, pg_sys::AccessShareLock as _);
        let index_pages = f64::from(pg_sys::RelationGetNumberOfBlocksInFork(
            index,
            pg_sys::ForkNumber::MAIN_FORKNUM,
        ))
        .max(1.0);
        pg_sys::index_close(index, pg_sys::AccessShareLock as _);
        let mut random_page_cost = 0.0;
        let mut seq_page_cost = 0.0;
        pg_sys::get_tablespace_page_costs(tablespace, &mut random_page_cost, &mut seq_page_cost);
        let candidates = pg_sys::clamp_row_est(estimate.candidates * tuples.max(1.0));
        // The meta page and a dictionary probe, then postings: a few bytes
        // per candidate (delta-coded TIDs, positions for phrases).
        let pages = (2.0 + candidates * POSTING_BYTES / f64::from(pg_sys::BLCKSZ))
            .ceil()
            .clamp(1.0, index_pages);
        let loop_count = loop_count.max(1.0);
        let page_cost = if loop_count > 1.0 {
            let fetched = pg_sys::index_pages_fetched(
                pages * loop_count,
                index_pages as pg_sys::BlockNumber,
                index_pages,
                root,
            );
            fetched * random_page_cost / loop_count
        } else {
            pages * random_page_cost
        };
        // Parsing and tokenizing the query is a per-scan operator cost.
        let startup = clauses as f64 * pg_sys::cpu_operator_cost;
        let total = startup + page_cost + candidates * pg_sys::cpu_index_tuple_cost;
        IndexCost {
            startup,
            total,
            pages,
            candidates,
            random_page_cost,
            seq_page_cost,
        }
    }
}

/// Heap pages fetched for `candidates` rows of a relation of `heap_pages`,
/// and the cost per page: random access for a few pages, approaching
/// sequential cost as the fetch covers the table. This is the bitmap heap
/// scan's own model (`compute_bitmap_pages`), so the custom scan, which
/// fetches the same TIDs in the same order, is compared like for like.
pub fn heap_fetch(index: &IndexCost, heap_pages: f64) -> (f64, f64) {
    let heap_pages = heap_pages.max(1.0);
    let fetched = (2.0 * heap_pages * index.candidates) / (2.0 * heap_pages + index.candidates);
    let fetched = if fetched >= heap_pages {
        heap_pages
    } else {
        fetched.ceil()
    };
    let cost_per_page = if fetched >= 2.0 {
        index.random_page_cost
            - (index.random_page_cost - index.seq_page_cost) * (fetched / heap_pages).sqrt()
    } else {
        index.random_page_cost
    };
    (fetched, cost_per_page)
}

/// A `==>` clause of an index path: its query text, when constant, and the
/// index it was bound to at plan time.
pub struct PathClause {
    pub query: Option<String>,
    pub bound: Option<pg_sys::Oid>,
}

/// The `==>` clauses of an index path.
///
/// # Safety
/// `path` is a valid index path.
pub unsafe fn index_path_clauses(path: *mut pg_sys::IndexPath) -> Vec<PathClause> {
    unsafe {
        let mut clauses = Vec::new();
        for clause in PgList::<pg_sys::IndexClause>::from_pg((*path).indexclauses).iter_ptr() {
            let rinfo = (*clause).rinfo;
            if rinfo.is_null() {
                continue;
            }
            let expr = (*rinfo).clause.cast::<pg_sys::Node>();
            if expr.is_null() || (*expr).type_ != pg_sys::NodeTag::T_OpExpr {
                continue;
            }
            let op = expr.cast::<pg_sys::OpExpr>();
            if pg_sys::list_length((*op).args) != 2 {
                continue;
            }
            let right = pg_sys::list_nth((*op).args, 1).cast::<pg_sys::Node>();
            clauses.push(PathClause {
                query: crate::operator::query_text(right),
                bound: crate::operator::bound_index(right),
            });
        }
        clauses
    }
}
