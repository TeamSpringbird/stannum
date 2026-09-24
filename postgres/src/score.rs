// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

use crate::bm25::{
    Bm25Overrides, DenseRatio, ScoreStopWords, ScoringTermInput, TermScorer, TermSetEdit,
    compile_scoring_terms, sum_scores_in_order,
};
use pgrx::iter::TableIterator;
use pgrx::{
    FromDatum, Internal, IntoDatum, PgList, PgRelation, Spi, default, name, pg_extern, pg_guard,
    pg_sys,
};
use rustc_hash::{FxHashMap, FxHashSet};
use segment::Tid;
use segment::bound::BlockBound;
use segment::docs::DocTable;
use segment::index::{Expanded, Index, Window};
use segment::segment::Lengths;
use segment::set::Cursor as _;
use segment::tf_bucket::{BUCKET_COUNT, TfBucket};
use segment::tid::MAX_OFFSET;
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap};
use std::ffi::{CStr, CString, c_void};
use tinql::runtime::plan::{Limits, plan};
use tinql::runtime::{
    CompiledRegex, FuzzyMatcher, Query, RangeBound, SpanTermSlot, evaluate, parse_tinql_to_query,
    range_matches, tokenize_doc,
};
use tokenizer::Tokenizer;

use crate::storage::View;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CacheKey {
    /// The executor run the scorer was built for; see [`note_executor_start`].
    statement: u64,
    heap_oid: u32,
    index_oid: u32,
    query: String,
    full: bool,
    dense: u32,
    k1: Option<u32>,
    b: Option<u32>,
    add: Option<Vec<String>>,
    replace: Option<Vec<String>>,
}

struct ScoreCorpus {
    key: CacheKey,
    by_document: FxHashMap<String, f32>,
    max: f32,
}

/// Scoring state read from a segmented index: per-term scorers built from
/// dead-inclusive segment statistics, and the sources to look each row up in.
pub(crate) struct IndexScorer {
    key: CacheKey,
    /// Per-source cursors, declared before `view` so they drop first.
    sources: Vec<SourceReader>,
    view: View,
    dead: Vec<std::rc::Rc<BTreeSet<Tid>>>,
    terms: Vec<(String, TermScorer)>,
    query: Query,
    /// Computed on first request: the maximum over matching documents.
    max: Option<f32>,
    /// Scores the search scan already computed for the rows it emits, so the
    /// projected score function does not move the cursors backwards.
    known: FxHashMap<Tid, f32>,
}

/// One source's readers for scoring rows by location: the document table
/// finds a row's ordinal, each term's stream its rank, and the payload its
/// frequency bucket. Every lookup is a few random reads, in any order.
struct SourceReader {
    docs: DocTable<'static>,
    lengths: segment::segment::Lengths<'static>,
    /// One per scoring term: the term's streams in this source, if present.
    terms: Vec<Option<TermReader>>,
}

struct TermReader {
    /// Rows mostly arrive in heap order, which is ordinal order, so each
    /// lookup is a forward seek; a request behind the cursor rewinds it (a
    /// join hands rows over in its own order). Rewinding repositions the
    /// parsed stream: reopening the term parsed every chunk bound again, and
    /// an exhaustive reference over a hundred million rows did so per row.
    cursor: segment::ordinals::OrdinalCursor<'static>,
    /// The lowest ordinal a seek ran off the end at: any request from there
    /// on is absent without touching the cursor.
    exhausted_at: Option<u32>,
}

impl TermReader {
    /// The term-frequency bucket of `ordinal`, if the term lists it.
    fn bucket(&mut self, ordinal: u32, label: &str) -> Option<u8> {
        if self.exhausted_at.is_some_and(|at| ordinal >= at) {
            return None;
        }
        if self
            .cursor
            .current()
            .is_none_or(|current| current > ordinal)
        {
            segment_error_in(self.cursor.rewind(), label);
        }
        segment_error_in(self.cursor.seek(ordinal), label);
        let Some(current) = self.cursor.current() else {
            self.exhausted_at = Some(ordinal.min(self.exhausted_at.unwrap_or(u32::MAX)));
            return None;
        };
        if current != ordinal {
            return None;
        }
        Some(self.cursor.bucket().unwrap_or_else(|| {
            crate::storage::corrupt(format!("Stannum {label}: a term member carries no bucket"))
        }))
    }
}

impl SourceReader {
    /// # Safety
    /// `segment` must stay alive and unmoved for as long as this reader exists:
    /// the owning `IndexScorer` keeps it in `view` and drops readers first.
    unsafe fn new(segment: &dyn Index, terms: &[(String, TermScorer)]) -> Self {
        let segment: &'static (dyn Index + 'static) =
            unsafe { std::mem::transmute::<&dyn Index, &'static (dyn Index + 'static)>(segment) };
        let terms = terms
            .iter()
            .map(|(term, _)| {
                segment_error(segment.term(term)).map(|term| TermReader {
                    cursor: segment_error(term.ordinals().and_then(|stream| stream.cursor())),
                    exhausted_at: None,
                })
            })
            .collect();
        Self {
            docs: segment_error(segment.doc_table()),
            lengths: segment.lengths(),
            terms,
        }
    }
}

thread_local! {
    static SCORE_CACHE: RefCell<Option<ScoreCorpus>> = const { RefCell::new(None) };
    static INDEX_SCORE_CACHE: RefCell<Option<IndexScorer>> = const { RefCell::new(None) };
    /// One scorer per live ranked scan, newest last, holding the score of
    /// every row the scan ranked. Cursors keep scans open across statements
    /// and two scans on one query can be open at once, so a scan's rows are
    /// scored by the scan's own statistics and never by another's.
    static SCAN_SCORERS: RefCell<Vec<ScanScorer>> = const { RefCell::new(Vec::new()) };
    /// Scan identities and emission stamps, from one counter.
    static NEXT_SCAN: Cell<u64> = const { Cell::new(0) };
    /// Counts executor runs in this backend. Transaction and command ids do
    /// not distinguish consecutive read-only statements, which never assign
    /// a transaction id and each start at command zero.
    static STATEMENT: Cell<u64> = const { Cell::new(0) };
}

/// A live ranked scan's scorer; see [`SCAN_SCORERS`].
struct ScanScorer {
    scan: u64,
    /// The row the scan emitted last: the visible tuple's location and the
    /// location the scan ranked (the root of a HOT chain). A row is projected
    /// after its scan emitted it and before that scan emits another, so this
    /// names exactly the rows whose score the scan owns.
    emitted: Option<Emitted>,
    scorer: IndexScorer,
}

/// The row a ranked scan emitted last; see [`ScanScorer::emitted`].
#[derive(Clone, Copy)]
struct Emitted {
    /// The visible tuple's location, as the executor projects it.
    member: Tid,
    /// The location the scan ranked: the root of the tuple's HOT chain.
    root: Tid,
    /// The statement the row was emitted in; a later statement's score
    /// calls are for other rows.
    statement: u64,
    /// Emission order across scans.
    stamp: u64,
}

fn next_stamp() -> u64 {
    NEXT_SCAN.with(|next| {
        let id = next.get().wrapping_add(1);
        next.set(id);
        id
    })
}

/// Called from the `ExecutorStart` hook so scorers built for one statement
/// are never reused by the next.
pub(crate) fn note_executor_start() {
    STATEMENT.with(|s| s.set(s.get().wrapping_add(1)));
}

fn current_statement() -> u64 {
    STATEMENT.with(Cell::get)
}

fn score_context_error(function: &str) -> ! {
    pgrx::error!(
        "{function} requires a stannum index scan and cannot be used in this query context"
    )
}

#[pg_extern(immutable, parallel_unsafe)]
fn full_score(ctid: pg_sys::ItemPointerData) -> Option<f32> {
    let _ = ctid;
    score_context_error("stannum.full_score()")
}

#[pg_extern(name = "full_score", immutable, parallel_unsafe)]
fn full_score_with_bm25(
    ctid: pg_sys::ItemPointerData,
    k1: Option<f32>,
    b: Option<f32>,
) -> Option<f32> {
    let _ = (ctid, k1, b);
    score_context_error("stannum.full_score()")
}

#[pg_extern(immutable, parallel_unsafe)]
fn score(
    ctid: pg_sys::ItemPointerData,
    dense_ratio: default!(Option<f32>, 0.10),
    k1: default!(Option<f32>, "NULL"),
    b: default!(Option<f32>, "NULL"),
    term_add: default!(Option<Vec<String>>, "NULL"),
    term_replace: default!(Option<Vec<String>>, "NULL"),
) -> Option<f32> {
    let _ = (ctid, dense_ratio, k1, b, term_add, term_replace);
    score_context_error("stannum.score()")
}

#[pg_extern(immutable, parallel_unsafe)]
fn max_score(ctid: pg_sys::ItemPointerData) -> Option<f32> {
    let _ = ctid;
    score_context_error("stannum.max_score()")
}

fn bits(value: Option<f32>) -> Option<u32> {
    value.map(f32::to_bits)
}

#[pg_extern(volatile, parallel_unsafe)]
#[expect(
    clippy::too_many_arguments,
    reason = "SQL signature used by the scoring support function"
)]
fn score_bound(
    document: &str,
    query: &str,
    heap_oid: i32,
    index_oid: i32,
    mode: i32,
    dense_ratio: Option<f32>,
    k1: Option<f32>,
    b: Option<f32>,
    term_add: Option<Vec<String>>,
    term_replace: Option<Vec<String>>,
) -> f32 {
    let key = CacheKey {
        statement: current_statement(),
        heap_oid: heap_oid as u32,
        index_oid: index_oid as u32,
        query: query.to_owned(),
        full: mode == 1 || mode == 3,
        dense: dense_ratio.unwrap_or(DenseRatio::DEFAULT).to_bits(),
        k1: bits(k1),
        b: bits(b),
        add: term_add.clone(),
        replace: term_replace.clone(),
    };
    SCORE_CACHE.with_borrow_mut(|slot| {
        if slot.as_ref().is_none_or(|corpus| corpus.key != key) {
            *slot = Some(build_corpus(key.clone(), k1, b, term_add, term_replace));
        }
        let corpus = slot.as_ref().expect("score corpus was just populated");
        if mode >= 2 {
            corpus.max
        } else {
            corpus.by_document.get(document).copied().unwrap_or(0.0)
        }
    })
}

/// Scoring bound to a segmented index: statistics and per-document term
/// frequencies come from the index, never from the heap.
#[pg_extern(volatile, parallel_unsafe)]
#[expect(
    clippy::too_many_arguments,
    reason = "SQL signature used by the scoring support function"
)]
fn score_bound_indexed(
    ctid: pg_sys::ItemPointerData,
    query: &str,
    heap_oid: i32,
    index_oid: i32,
    mode: i32,
    dense_ratio: Option<f32>,
    k1: Option<f32>,
    b: Option<f32>,
    term_add: Option<Vec<String>>,
    term_replace: Option<Vec<String>>,
) -> f32 {
    let statement = current_statement();
    let dense = dense_ratio.unwrap_or(DenseRatio::DEFAULT).to_bits();
    // Per-row calls compare against the cached key without allocating; the
    // owned key is built only when the cache misses.
    let same_query = |key: &CacheKey| {
        key.heap_oid == heap_oid as u32
            && key.index_oid == index_oid as u32
            && key.full == (mode == 1 || mode == 3)
            && key.dense == dense
            && key.k1 == bits(k1)
            && key.b == bits(b)
            && key.query == query
            && key.add.as_deref() == term_add.as_deref()
            && key.replace.as_deref() == term_replace.as_deref()
    };
    let matches = |key: &CacheKey| key.statement == statement && same_query(key);
    if mode < 2 {
        let block = (u32::from(ctid.ip_blkid.bi_hi) << 16) | u32::from(ctid.ip_blkid.bi_lo);
        let tid = Tid::new(block, ctid.ip_posid)
            .unwrap_or_else(|_| pgrx::error!("invalid heap tuple location"));
        // The row a ranked scan just emitted, in this statement, carries the
        // score that scan ranked it by; any other location (an unpruned
        // scan's row, say, or a row a paused cursor emitted in an earlier
        // statement) is scored by the statement's scorer below. Fetches from
        // several cursors share one statement number, so the newest emission
        // of a location wins.
        let from_scan = SCAN_SCORERS.with_borrow(|scans| {
            scans
                .iter()
                .filter(|entry| same_query(&entry.scorer.key))
                .filter_map(|entry| match entry.emitted {
                    Some(emitted) if emitted.member == tid && emitted.statement == statement => {
                        Some((
                            emitted.stamp,
                            entry.scorer.known.get(&emitted.root).copied(),
                        ))
                    }
                    _ => None,
                })
                .max_by_key(|(stamp, _)| *stamp)
                .and_then(|(_, score)| score)
        });
        if let Some(score) = from_scan {
            return score;
        }
    }
    let cached =
        INDEX_SCORE_CACHE.with_borrow(|slot| slot.as_ref().is_some_and(|s| matches(&s.key)));
    INDEX_SCORE_CACHE.with_borrow_mut(|slot| {
        if !cached {
            let index = unsafe {
                PgRelation::with_lock(
                    pg_sys::Oid::from(index_oid as u32),
                    pg_sys::AccessShareLock as _,
                )
            };
            crate::udfs::require_stannum_index(&index, "score_bound_indexed");
            if unsafe { pg_sys::IndexGetRelation(index.oid(), false) }.to_u32() != heap_oid as u32 {
                pgrx::error!("score index does not belong to the supplied table");
            }
            // SQL callers can bypass the planner's heap-scoring fallback; the
            // statement cache is filled only where the index may be read.
            if !unsafe { crate::storage::index_reads_allowed(index.as_ptr()) } {
                pgrx::error!(
                    "indexed scoring is unavailable for this index during recovery; use stannum.score or stannum.full_score"
                );
            }
            let key = CacheKey {
                statement,
                heap_oid: heap_oid as u32,
                index_oid: index_oid as u32,
                query: query.to_owned(),
                full: mode == 1 || mode == 3,
                dense,
                k1: bits(k1),
                b: bits(b),
                add: term_add.clone(),
                replace: term_replace.clone(),
            };
            *slot = Some(build_index_scorer(key, k1, b, term_add, term_replace));
        }
        let scorer = slot.as_mut().expect("index scorer was just populated");
        if mode >= 2 {
            scorer.max_score()
        } else {
            let block = (u32::from(ctid.ip_blkid.bi_hi) << 16) | u32::from(ctid.ip_blkid.bi_lo);
            let tid = Tid::new(block, ctid.ip_posid)
                .unwrap_or_else(|_| pgrx::error!("invalid heap tuple location"));
            scorer.score(tid)
        }
    })
}

/// Codec results from a source this code cannot name; prefer
/// [`segment_error_in`] where the source is known.
fn segment_error<T>(result: segment::Result<T>) -> T {
    result.unwrap_or_else(|error| crate::storage::corrupt(format!("Stannum index data: {error}")))
}

fn segment_error_in<T>(result: segment::Result<T>, label: &str) -> T {
    crate::storage::codec_in(result, label)
}

impl IndexScorer {
    /// Score of one visible document, or zero if the index does not hold it.
    ///
    /// A HOT-updated row keeps its posting at the root of its chain while the
    /// executor hands the projection the visible member's location, so a
    /// location absent from every source is resolved to its root first.
    pub(crate) fn score(&mut self, tid: Tid) -> f32 {
        if let Some(score) = self.known.get(&tid) {
            return *score;
        }
        if let Some(score) = self.score_listed(tid) {
            return score;
        }
        let root = unsafe { hot_root(pg_sys::Oid::from(self.key.heap_oid), tid) };
        if root != tid {
            if let Some(score) = self.known.get(&root) {
                return *score;
            }
            if let Some(score) = self.score_listed(root) {
                return score;
            }
        }
        0.0
    }

    /// Score of the document at `tid` in the first source listing it live.
    fn score_listed(&mut self, tid: Tid) -> Option<f32> {
        for i in 0..self.view.sources.len() {
            if self.dead[i].contains(&tid) {
                continue;
            }
            let label = self.view.labels[i].as_str();
            let reader = &mut self.sources[i];
            let Some(ordinal) = segment_error_in(reader.docs.ordinal_of(tid), label) else {
                continue;
            };
            // Each term's bucket for the document, if the term lists it.
            let buckets: Vec<Option<u8>> = reader
                .terms
                .iter_mut()
                .map(|slot| slot.as_mut()?.bucket(ordinal, label))
                .collect();
            if buckets.iter().all(Option::is_none) {
                // The document is in this source but holds no scoring term.
                continue;
            }
            let length = segment_error_in(reader.lengths.get(ordinal), label);
            // Left-to-right f32 fold in lexical term order, as production does.
            let mut total = 0.0_f32;
            for ((_, scorer), bucket) in self.terms.iter().zip(&buckets) {
                let Some(bucket) = *bucket else {
                    continue;
                };
                let bucket = TfBucket::new(bucket).unwrap_or_else(|| {
                    crate::storage::corrupt(format!(
                        "Stannum {label}: term-frequency bucket {bucket} out of range"
                    ))
                });
                total += scorer.score_bucket(bucket, length);
            }
            return Some(total);
        }
        None
    }
}

/// The root of the HOT chain holding `tid`, or `tid` itself when it is not a
/// heap-only member (including when the page has no such line pointer).
///
/// # Safety
/// `heap_oid` names a relation the caller may open; `tid` was fetched from
/// it under the active snapshot, so its block exists.
unsafe fn hot_root(heap_oid: pg_sys::Oid, tid: Tid) -> Tid {
    unsafe {
        let heap = pg_sys::table_open(heap_oid, pg_sys::AccessShareLock as _);
        let buffer = pg_sys::ReadBuffer(heap, tid.block);
        pg_sys::LockBuffer(buffer, pg_sys::BUFFER_LOCK_SHARE as i32);
        let page = pg_sys::BufferGetPage(buffer);
        let max = pg_sys::PageGetMaxOffsetNumber(page);
        let mut root = tid;
        if tid.offset <= max {
            // Written for every possible line pointer of a page; a page can
            // hold at most BLCKSZ / 4 of them.
            let mut roots = vec![pg_sys::InvalidOffsetNumber; pg_sys::BLCKSZ as usize / 4];
            pg_sys::heap_get_root_tuples(page, roots.as_mut_ptr());
            let offset = roots[usize::from(tid.offset) - 1];
            if offset != pg_sys::InvalidOffsetNumber {
                root = Tid {
                    block: tid.block,
                    offset,
                };
            }
        }
        pg_sys::UnlockReleaseBuffer(buffer);
        pg_sys::table_close(heap, pg_sys::AccessShareLock as _);
        root
    }
}

/// Output order of a ranked scan: descending score, then heap order, so ties
/// are stable.
pub(crate) fn rank(a: &(f32, Tid), b: &(f32, Tid)) -> Ordering {
    b.0.total_cmp(&a.0).then(a.1.cmp(&b.1))
}

/// Most rows a scan prunes for. Beyond this the heap bookkeeping outweighs
/// the scoring it saves, and the scan scores every candidate instead.
pub(crate) const PRUNE_MAX_K: usize = 4096;

/// `stannum.debug_seed_score`: a measurement aid. When not negative, a pruned
/// walk prunes against this score from its first candidate, as if the top k
/// were already known: the pages and candidates it then costs are what a
/// walk seeded from per-term champion lists would cost.
pub(crate) static DEBUG_SEED_SCORE: pgrx::GucSetting<f64> = pgrx::GucSetting::<f64>::new(-1.0);

thread_local! {
    /// Index pages read while building a walk's per-term state, and while
    /// walking. A pruned walk cannot skip what it reads before it starts, so
    /// the split says whether tighter bounds or cheaper setup is the work.
    static SETUP_BLOCKS: Cell<i64> = const { Cell::new(0) };
    static WALK_BLOCKS: Cell<i64> = const { Cell::new(0) };
    /// Chunks of a term's ordinal stream expanded into members.
    static CHUNK_LOADS: Cell<i64> = const { Cell::new(0) };
    /// Heap visibility checks, each a random read of the table.
    static VISIBILITY_CHECKS: Cell<i64> = const { Cell::new(0) };
    /// Visibility checks answered by the visibility map without a heap read.
    static VM_HITS: Cell<i64> = const { Cell::new(0) };
}

/// Pages this backend has read from storage so far.
pub(crate) fn disk_pages() -> i64 {
    // SAFETY: a read of the backend's own instrumentation counters.
    unsafe {
        let usage = &raw const pg_sys::pgBufferUsage;
        (*usage).shared_blks_read
    }
}

thread_local! {
    /// Pages read from storage per named phase of a scan, for accounting
    /// that segment areas do not cover: the heap, and the index structures
    /// storage reads without a segment reader.
    static PHASE_DISK: RefCell<Vec<(&'static str, i64)>> = const { RefCell::new(Vec::new()) };
}

/// Runs `body`, charging the pages it reads from storage to `label`.
pub(crate) fn charging<T>(label: &'static str, body: impl FnOnce() -> T) -> T {
    let before = disk_pages();
    let value = body();
    let pages = disk_pages() - before;
    if pages != 0 {
        PHASE_DISK.with_borrow_mut(|phases| {
            match phases.iter_mut().find(|(name, _)| *name == label) {
                Some(entry) => entry.1 += pages,
                None => phases.push((label, pages)),
            }
        });
    }
    value
}

/// Pages read from storage per phase since the counters were reset.
pub(crate) fn phase_disk() -> Vec<(&'static str, i64)> {
    PHASE_DISK.with_borrow(Clone::clone)
}

/// Index pages this backend has read or hit so far.
fn blocks_used() -> i64 {
    // SAFETY: a read of the backend's own instrumentation counters.
    unsafe {
        let usage = &raw const pg_sys::pgBufferUsage;
        (*usage).shared_blks_hit + (*usage).shared_blks_read
    }
}

/// Pages read from storage per segment area since the counters were reset.
pub(crate) fn area_disk() -> [u64; segment::cache::AREAS] {
    segment::cache::area_disk()
}

/// Bytes fetched per segment area since the walk counters were reset.
pub(crate) fn area_bytes() -> [u64; segment::cache::AREAS] {
    segment::cache::area_bytes()
}

pub(crate) fn reset_walk_blocks() {
    segment::cache::reset_areas();
    SETUP_BLOCKS.set(0);
    WALK_BLOCKS.set(0);
    CHUNK_LOADS.set(0);
    VISIBILITY_CHECKS.set(0);
    VM_HITS.set(0);
    PHASE_DISK.with_borrow_mut(Vec::clear);
}

/// Heap visibility checks since the last reset.
pub(crate) fn visibility_checks() -> i64 {
    VISIBILITY_CHECKS.get()
}

/// Visibility checks the visibility map answered since the last reset.
pub(crate) fn vm_hits() -> i64 {
    VM_HITS.get()
}

/// Chunks loaded since the last reset.
pub(crate) fn chunk_loads() -> i64 {
    CHUNK_LOADS.get()
}

/// Pages spent on walk setup and on the walk itself since the last reset.
pub(crate) fn walk_blocks() -> (i64, i64) {
    (SETUP_BLOCKS.get(), WALK_BLOCKS.get())
}

/// The seeded threshold, if any: ties are admitted, as the latest location.
fn seeded_threshold() -> Option<(f32, Tid)> {
    let seed = DEBUG_SEED_SCORE.get();
    (seed >= 0.0).then_some((
        seed as f32,
        Tid {
            block: u32::MAX,
            offset: MAX_OFFSET,
        },
    ))
}

/// The best rows of a pruned ranked scan.
pub(crate) struct TopK {
    /// In output order; fewer than `k` only when the query matched fewer, or
    /// when `zero_fill` is set.
    pub(crate) rows: Vec<(f32, Tid)>,
    /// Candidates whose score was computed.
    pub(crate) scored: usize,
    /// True when `rows` holds every candidate: the threshold never formed,
    /// so nothing was skipped.
    pub(crate) complete: bool,
    /// `rows` holds every match with a positive score and they are fewer than
    /// `k`: the rest of the top k are matches of elided terms alone, which tie
    /// at zero and rank in heap order. The caller streams them.
    pub(crate) zero_fill: bool,
    /// The walk ran over the ordinal streams.
    pub(crate) ordinal: bool,
    /// Every candidate was scored from the stream (see
    /// [`IndexScorer::top_k_streamed`]): nothing was pruned.
    pub(crate) streamed: bool,
}

/// How a query's leaf terms combine into its candidate set.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Combine {
    /// Every term must be present: a conjunction.
    All,
    /// Any term suffices: a disjunction or a single term.
    Any,
}

/// A query the scan can prune: a flat conjunction or disjunction of terms
/// (a single term included), each optionally boosted, with the terms in
/// lexical order and deduplicated.
fn prunable_shape(query: &Query) -> Option<(Combine, Vec<&str>)> {
    fn unboost(query: &Query) -> &Query {
        match query {
            Query::Boost { inner, .. } => unboost(inner),
            other => other,
        }
    }
    fn leaves<'q>(children: impl IntoIterator<Item = &'q Query>) -> Option<Vec<&'q str>> {
        children
            .into_iter()
            .map(|child| match unboost(child) {
                Query::Term(term) => Some(term.as_str()),
                _ => None,
            })
            .collect()
    }
    let (combine, mut terms) = match unboost(query) {
        Query::Term(term) => (Combine::Any, vec![term.as_str()]),
        Query::And(left, right) => (Combine::All, leaves([&**left, &**right])?),
        Query::Conjunction(children) => (Combine::All, leaves(children)?),
        Query::Or(left, right) => (Combine::Any, leaves([&**left, &**right])?),
        Query::Disjunction { min: 1, children } => (Combine::Any, leaves(children)?),
        _ => return None,
    };
    terms.sort_unstable();
    terms.dedup();
    Some((combine, terms))
}

/// A heap entry ordered so the worst-ranked row is the greatest.
struct Ranked(f32, Tid);

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Ranked {}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        rank(&(self.0, self.1), &(other.0, other.1))
    }
}

impl IndexScorer {
    /// True when no term scores: every leaf is absent or an elided dense
    /// term, so every match scores zero and ranks in heap order.
    pub(crate) fn scores_nothing(&self) -> bool {
        self.terms.is_empty()
    }

    /// The `k` best of every candidate `stream` yields, scored as they
    /// arrive: only the heap of `k` rows is held. Scoring every candidate
    /// first held every match, its score and a map of both for the
    /// projection, which for a phrase of common words at scale was hundreds
    /// of megabytes per backend. Bit-identical to that, including tie order.
    ///
    /// `None` when the stream is a superset that needs rechecking.
    pub(crate) fn top_k_streamed(
        &mut self,
        stream: &mut crate::stream::CandidateStream,
        k: usize,
    ) -> Option<TopK> {
        if stream.recheck {
            return None;
        }
        let mut heap = BinaryHeap::with_capacity(k + 1);
        let mut scored = 0usize;
        while let Some(tid) = stream.next() {
            pgrx::check_for_interrupts!();
            scored += 1;
            let entry = Ranked(self.score(tid), tid);
            if heap.len() < k {
                heap.push(entry);
            } else if heap.peek().is_some_and(|worst| entry < *worst) {
                heap.pop();
                heap.push(entry);
            }
        }
        let mut rows: Vec<(f32, Tid)> = heap.into_iter().map(|Ranked(s, t)| (s, t)).collect();
        rows.sort_by(rank);
        Some(TopK {
            complete: rows.len() < k,
            rows,
            scored,
            zero_fill: false,
            ordinal: false,
            streamed: true,
        })
    }

    /// The `k` best candidates of the scan's query in output order, found
    /// with block-max pruning: the sources are walked in tuple order with
    /// one cursor per scoring term, the `k`-th best score so far is the
    /// threshold, and runs of postings whose block bounds cannot reach it
    /// are skipped without decoding. Bit-identical to scoring every
    /// candidate and sorting, including tie order.
    ///
    /// `None` when the query is not a flat conjunction or disjunction of
    /// exactly the scoring terms, or a source carries no block bounds; the
    /// caller then scores every candidate.
    pub(crate) fn top_k(&self, k: usize) -> Option<TopK> {
        let (combine, leaves) = prunable_shape(&self.query)?;
        // Every scoring term must be a leaf (no added terms), and a leaf that
        // is not a scoring term must be absent from the index altogether: it
        // then adds nothing to a disjunction and empties a conjunction. A
        // present leaf without a scorer is an elided dense term. In a
        // disjunction its documents score zero unless a scoring term also
        // lists them, so the walk over the scoring terms is exact as far as
        // its positive scores reach; when they are fewer than k the caller
        // fills the rest from the zero-scoring matches. In a conjunction the elided term adds
        // nothing to a score but still filters, so its cursor joins the walk
        // without a bound.
        if self
            .terms
            .iter()
            .any(|(term, _)| leaves.binary_search(&term.as_str()).is_err())
        {
            return None;
        }
        let mut absent = false;
        let mut elided = false;
        let mut filters: Vec<&str> = Vec::new();
        for leaf in &leaves {
            if self.terms.iter().any(|(term, _)| term == leaf) {
                continue;
            }
            if self
                .view
                .sources
                .iter()
                .any(|(source, _)| segment_error(source.term(leaf)).is_some())
            {
                match combine {
                    Combine::Any => elided = true,
                    Combine::All => filters.push(leaf),
                }
                continue;
            }
            absent = true;
        }
        let mut heap = BinaryHeap::with_capacity(k + 1);
        let mut scored = 0usize;
        let mut ordinal = false;
        if k > 0 && !(absent && combine == Combine::All) {
            // Admission trusts the visibility map for all-visible pages. A
            // page VACUUM marked all-visible after the view was captured may
            // hold a tuple the view still lists; the walk is repeated against
            // the heap if a dead list was published meanwhile.
            let mut shortcut = true;
            loop {
                let mut visibility =
                    unsafe { Visibility::open(pg_sys::Oid::from(self.key.heap_oid), shortcut) };
                for i in 0..self.view.sources.len() {
                    self.walk_by_ordinal(
                        i,
                        combine,
                        &filters,
                        &mut visibility,
                        k,
                        &mut heap,
                        &mut scored,
                    );
                    ordinal = true;
                }
                if !visibility.shortcuts
                    || unsafe {
                        crate::storage::view_is_current(
                            pg_sys::Oid::from(self.key.index_oid),
                            &self.view,
                        )
                    }
                {
                    break;
                }
                heap.clear();
                scored = 0;
                shortcut = false;
            }
        }
        let mut rows: Vec<(f32, Tid)> = heap.into_iter().map(|Ranked(s, t)| (s, t)).collect();
        rows.sort_by(rank);
        // A location present in two sources is scored by the first only in
        // the unpruned path; leave that case to it.
        let mut seen = FxHashSet::default();
        if !rows.iter().all(|(_, tid)| seen.insert(*tid)) {
            return None;
        }
        if elided && rows.last().is_some_and(|(score, _)| *score <= 0.0) {
            // Documents holding only elided terms tie at zero and belong here.
            return None;
        }
        let zero_fill = elided && rows.len() < k;
        let complete = rows.len() < k && !zero_fill;
        Some(TopK {
            rows,
            scored,
            complete,
            zero_fill,
            ordinal,
            streamed: false,
        })
    }

    /// Walks source `i` over its ordinal streams into the shared heap:
    /// block-max WAND over the terms' chunks, then a fold of each admitted
    /// chunk into a candidate set scored in ordinal order.
    #[expect(
        clippy::too_many_arguments,
        reason = "one call site; the arguments are the walk's state"
    )]
    fn walk_by_ordinal(
        &self,
        i: usize,
        combine: Combine,
        filters: &[&str],
        visibility: &mut Visibility,
        k: usize,
        heap: &mut BinaryHeap<Ranked>,
        scored: &mut usize,
    ) {
        let started = blocks_used();
        let (source, dead_list) = &self.view.sources[i];
        let label = &self.view.labels[i];
        let dead = if i < self.view.keys.len() {
            segment_error_in(
                crate::fold::dead_ordinals(self.view.keys[i], dead_list.as_ref()),
                label,
            )
        } else {
            // The write buffer has no dead list.
            std::rc::Rc::default()
        };
        let mut terms = Vec::with_capacity(self.terms.len());
        for (slot, (name, scorer)) in self.terms.iter().enumerate() {
            let Some(term) = segment_error_in(source.term(name), label) else {
                match combine {
                    // A missing term empties the conjunction in this source.
                    Combine::All => return,
                    Combine::Any => continue,
                }
            };
            terms.push(Self::ordinal_term(&term, slot, Some(scorer), label));
        }
        let mut filter_terms = Vec::with_capacity(filters.len());
        for name in filters {
            match segment_error_in(source.term(name), label) {
                Some(term) => filter_terms.push(Self::ordinal_term(&term, usize::MAX, None, label)),
                None => return,
            }
        }
        if terms.is_empty() {
            return;
        }
        let ready = blocks_used();
        SETUP_BLOCKS.set(SETUP_BLOCKS.get() + ready - started);
        let docs = segment_error_in(source.doc_table(), label);
        let mut walk = OrdinalWalk {
            scorer: self,
            terms,
            filters: filter_terms,
            docs: &docs,
            index: &**source,
            document_count: source.document_count(),
            lengths: source.lengths(),
            dead: &dead,
            visibility,
            k,
            heap,
            scored,
            iterations: 0,
            seed: seeded_threshold(),
            values: Vec::new(),
            uppers: Vec::new(),
        };
        match combine {
            Combine::Any => walk.any(),
            Combine::All => walk.all(),
        }
        WALK_BLOCKS.set(WALK_BLOCKS.get() + blocks_used() - ready);
    }

    /// A term's streams in one source for the walk over ordinals. A filter
    /// has no scorer: it is a member test only.
    fn ordinal_term<'a>(
        term: &segment::segment::Term<'a>,
        slot: usize,
        scorer: Option<&TermScorer>,
        label: &str,
    ) -> OrdinalTerm<'a> {
        let ordinals = segment_error_in(term.ordinals(), label);
        let whole = ordinals
            .bounds()
            .iter()
            .map(|bound| BlockBound {
                min_len: bound.min_len,
            })
            .reduce(|merged, block| merged.merge(&block))
            .unwrap_or_else(|| {
                crate::storage::corrupt(format!(
                    "Stannum {label}: a term's stream carries no bounds"
                ))
            });
        let (keys, list) = match ordinals.list() {
            Some(list) => {
                let mut keys: Vec<u16> = list.iter().map(|o| (o >> 16) as u16).collect();
                keys.dedup();
                (keys, Some(list.to_vec()))
            }
            None => (
                (0..ordinals.chunk_count())
                    .map(|c| ordinals.chunk_key(c))
                    .collect(),
                None,
            ),
        };
        // The stored bound per chunk, or a list's one bound for every chunk
        // it touches.
        let mut bounds = Vec::with_capacity(keys.len());
        let mut sub_bounds = Vec::with_capacity(keys.len());
        for i in 0..keys.len() {
            let bound = ordinals.chunk_bound(i).unwrap_or_else(|| {
                crate::storage::corrupt(format!("Stannum {label}: a chunk carries no bound"))
            });
            bounds.push(bound.min_len);
            sub_bounds.push(bound.subs);
        }
        OrdinalTerm {
            slot,
            ordinals,
            chunk: None,
            keys,
            list,
            bounds,
            sub_bounds,
            bound_scores: Vec::new(),
            by_bucket: Vec::new(),
            pos: 0,
            term_max: scorer.map_or(0.0, |scorer| scorer.bound(&whole)),
            words: Box::new([0; segment::ordinals::WORDS]),
            counted: (0, 0),
            members: Vec::new(),
            dense: false,
            rank_base: 0,
        }
    }
}

/// Ordinals per sub-block of a chunk, at which a walk prunes within a chunk.
const SUB: usize = segment::ordinals::SUB as usize;

/// Sub-blocks per chunk.
const SUBS: usize = segment::ordinals::SUBS;

/// One scoring term's streams in one source, for the walk over ordinals.
struct OrdinalTerm<'a> {
    slot: usize,
    ordinals: segment::ordinals::Ordinals<'a>,
    /// The chunk last loaded, for its members' buckets.
    chunk: Option<segment::ordinals::Chunk>,
    /// Keys of the chunks the term occupies, ascending.
    keys: Vec<u16>,
    /// The stream as a list, when it is one.
    list: Option<Vec<u32>>,
    /// Per key: the shortest document per bucket among the term's postings there.
    bounds: Vec<[u32; BUCKET_COUNT]>,
    /// Per key and sub-block: one past the largest bucket the term has there,
    /// zero where it has no posting.
    sub_bounds: Vec<[u8; SUBS]>,
    /// Per key: the score bound, once asked for.
    bound_scores: Vec<Option<f32>>,
    /// Per key: the per-bucket bound table at the length floor it was
    /// computed for, once asked for; a candidate asks per term, so the
    /// table is computed once per chunk rather than sixteen scores per
    /// candidate.
    by_bucket: Vec<Option<(u32, [f32; BUCKET_COUNT])>>,
    /// Index into `keys` of the current chunk.
    pos: usize,
    term_max: f32,
    /// The current chunk's members, once loaded.
    words: Box<segment::ordinals::Words>,
    /// The current chunk's members as low bits when it is not a bitmap.
    members: Vec<u16>,
    dense: bool,
    /// The rank of the current chunk's first member.
    rank_base: u32,
    /// Members counted so far in the current chunk: (word index, members
    /// in the words before it).
    counted: (usize, u32),
}

impl OrdinalTerm<'_> {
    fn key(&self) -> Option<u16> {
        self.keys.get(self.pos).copied()
    }

    fn bound_block(&self, pos: usize) -> BlockBound {
        BlockBound {
            min_len: self.bounds[pos],
        }
    }

    /// The per-bucket bound table of chunk `pos` at `min_length`; see
    /// [`TermScorer::bounds_by_bucket`].
    fn bounds_by_bucket(
        &mut self,
        pos: usize,
        scorer: &TermScorer,
        min_length: u32,
    ) -> [f32; BUCKET_COUNT] {
        if self.by_bucket.len() < self.keys.len() {
            self.by_bucket.resize(self.keys.len(), None);
        }
        if let Some((at, table)) = self.by_bucket[pos]
            && at == min_length
        {
            return table;
        }
        let table = scorer.bounds_by_bucket(&self.bound_block(pos), min_length);
        self.by_bucket[pos] = Some((min_length, table));
        table
    }

    fn bound_score(&mut self, pos: usize, scorer: &TermScorer) -> f32 {
        if self.bound_scores.len() < self.keys.len() {
            self.bound_scores.resize(self.keys.len(), None);
        }
        if let Some(score) = self.bound_scores[pos] {
            return score;
        }
        let score = scorer.bound(&self.bound_block(pos)).min(self.term_max);
        self.bound_scores[pos] = Some(score);
        score
    }

    /// Loads the current chunk's members and the rank of its first member.
    fn load(&mut self) {
        CHUNK_LOADS.set(CHUNK_LOADS.get() + 1);
        let key = self.keys[self.pos];
        self.members.clear();
        self.counted = (0, 0);
        match &self.list {
            Some(list) => {
                self.words.fill(0);
                let start = list.partition_point(|o| ((*o >> 16) as u16) < key);
                self.rank_base = start as u32;
                for o in &list[start..] {
                    if (*o >> 16) as u16 != key {
                        break;
                    }
                    let low = (*o & 0xffff) as usize;
                    self.words[low / 64] |= 1 << (low % 64);
                    self.members.push(low as u16);
                }
                self.dense = false;
            }
            None => {
                let chunk = segment_error(self.ordinals.chunk(self.pos));
                chunk.words(&mut self.words);
                chunk.members(&mut self.members);
                self.dense = chunk.is_bitmap();
                self.rank_base = chunk.before;
                self.chunk = Some(chunk);
            }
        }
    }

    /// The bucket of the member at `rank` in the stream, which lies in the
    /// loaded chunk.
    fn bucket(&self, rank: u32) -> Option<u8> {
        match &self.list {
            Some(_) => self.ordinals.list_buckets().get(rank as usize).copied(),
            None => self.chunk.as_ref()?.bucket(rank - self.rank_base),
        }
    }

    /// Whether the loaded chunk holds `low`, and the member's rank in the stream.
    ///
    /// Candidates are ranked in ascending order within a chunk, so the
    /// members before `low` are counted from where the last call stopped;
    /// counting from the chunk's start each time was a thousand words per
    /// candidate per term.
    fn rank(&mut self, low: u16) -> Option<u32> {
        let word_at = usize::from(low / 64);
        let word = self.words[word_at];
        if word & (1 << (low % 64)) == 0 {
            return None;
        }
        let (mut counted_to, mut before) = self.counted;
        if counted_to > word_at {
            (counted_to, before) = (0, 0);
        }
        before += self.words[counted_to..word_at]
            .iter()
            .map(|w| w.count_ones())
            .sum::<u32>();
        self.counted = (word_at, before);
        Some(self.rank_base + before + (word & ((1u64 << (low % 64)) - 1)).count_ones())
    }
}

/// One source walked over its ordinal streams (ADR 0003): block-max WAND
/// over the terms' chunks, then a fold of each admitted chunk into a
/// candidate set scored in ordinal order.
struct OrdinalWalk<'a, 's> {
    scorer: &'s IndexScorer,
    /// In slot order.
    terms: Vec<OrdinalTerm<'a>>,
    /// A conjunction's elided terms: a match must hold them, and they add
    /// nothing to its score.
    filters: Vec<OrdinalTerm<'a>>,
    docs: &'s DocTable<'a>,
    /// The source, for length classes.
    index: &'a dyn Index,
    document_count: u32,
    lengths: Lengths<'a>,
    /// Dead ordinals, ascending.
    dead: &'s [u32],
    visibility: &'s mut Visibility,
    k: usize,
    heap: &'s mut BinaryHeap<Ranked>,
    scored: &'s mut usize,
    iterations: u32,
    /// `stannum.debug_seed_score`, read once: the threshold is consulted
    /// per sub-block and per candidate, and a setting read is a thread
    /// check and a lookup each time.
    seed: Option<(f32, Tid)>,
    /// Scratch for scoring a candidate: per present term its bound or score,
    /// and the terms holding the document.
    values: Vec<f32>,
    uppers: Vec<(f32, usize)>,
}

impl OrdinalWalk<'_, '_> {
    fn threshold(&self) -> Option<(f32, Tid)> {
        let real = if self.heap.len() == self.k {
            self.heap.peek().map(|w| (w.0, w.1))
        } else {
            None
        };
        match (real, self.seed) {
            (Some(real), Some(seed)) if seed.0 > real.0 => Some(seed),
            (None, seed) => seed,
            (real, _) => real,
        }
    }

    /// Whether a document at or after `ordinal` scoring at most `bound`
    /// could enter the top k: it must beat the k-th row's score, or tie it
    /// from an earlier location. Ordinals ascend with locations, so the
    /// location of `ordinal` is the earliest of every document from it on;
    /// it is resolved only when a tie asks for it.
    fn can_beat(&mut self, bound: f32, ordinal: u32) -> bool {
        match self.threshold() {
            None => true,
            Some((threshold, holder)) => {
                bound > threshold
                    || (bound == threshold
                        && ordinal < self.document_count
                        && self.resolve(ordinal) < holder)
            }
        }
    }

    /// The heap location of the document at `ordinal`.
    fn resolve(&mut self, ordinal: u32) -> Tid {
        segment_error(self.docs.tid_at(ordinal))
    }

    fn any(&mut self) {
        let mut order: Vec<usize> = (0..self.terms.len()).collect();
        let mut set: Box<segment::ordinals::Words> = Box::new([0; segment::ordinals::WORDS]);
        let mut present: Vec<usize> = Vec::with_capacity(self.terms.len());
        loop {
            self.iterations = self.iterations.wrapping_add(1);
            if self.iterations.is_multiple_of(64) {
                pgrx::check_for_interrupts!();
            }
            order.retain(|&t| self.terms[t].key().is_some());
            if order.is_empty() {
                return;
            }
            order.sort_unstable_by_key(|&t| self.terms[t].key());
            let threshold = self.threshold();
            // The pivot: the first chunk at which the terms up to it could
            // together reach the threshold, by their whole-term maxima.
            let mut reach = 0.0_f64;
            let mut p = None;
            for (j, &t) in order.iter().enumerate() {
                reach += f64::from(self.terms[t].term_max);
                if order
                    .get(j + 1)
                    .is_some_and(|&next| self.terms[next].key() == self.terms[t].key())
                {
                    continue;
                }
                if threshold.is_none_or(|(threshold, _)| {
                    reach * (1.0 + f64::from(f32::EPSILON) * 256.0) >= f64::from(threshold)
                }) {
                    p = Some(j);
                    break;
                }
            }
            let Some(p) = p else {
                // Even every remaining term together cannot reach the threshold.
                return;
            };
            let pivot = self.terms[order[p]].key().expect("retained");
            if self.terms[order[0]].key() != Some(pivot) {
                // Move the terms behind the pivot chunk up to it.
                for &t in &order[..p] {
                    let term = &mut self.terms[t];
                    if term.key() < Some(pivot) {
                        term.pos += term.keys[term.pos..].partition_point(|key| *key < pivot);
                    }
                }
                continue;
            }
            // Every term of the prefix is on the pivot chunk. Its bounds decide
            // whether anything in it can enter the top k.
            let mut bound = 0.0_f64;
            for &t in &order[..=p] {
                let pos = self.terms[t].pos;
                let scorer = &self.scorer.terms[self.terms[t].slot].1;
                bound += f64::from(self.terms[t].bound_score(pos, scorer));
            }
            if threshold.is_some_and(|(threshold, _)| {
                bound * (1.0 + f64::from(f32::EPSILON) * 256.0) < f64::from(threshold)
            }) {
                for &t in &order[..=p] {
                    self.terms[t].pos += 1;
                }
                continue;
            }
            // Every term of the prefix is on the pivot chunk: fold and score it.
            present.clear();
            present.extend(order[..=p].iter().copied());
            present.sort_unstable();
            self.evaluate(pivot, &present, &mut set);
            for &t in &present {
                self.terms[t].pos += 1;
            }
        }
    }

    /// A conjunction: the rarest stream leads through its chunks; the others
    /// and the filters are aligned to each, and a chunk every stream holds is
    /// folded to the members they share.
    fn all(&mut self) {
        let lead = (0..self.terms.len())
            .min_by_key(|&t| self.terms[t].keys.len())
            .expect("a conjunction has terms");
        let mut set: Box<segment::ordinals::Words> = Box::new([0; segment::ordinals::WORDS]);
        loop {
            self.iterations = self.iterations.wrapping_add(1);
            if self.iterations.is_multiple_of(64) {
                pgrx::check_for_interrupts!();
            }
            let Some(key) = self.terms[lead].key() else {
                return;
            };
            // Every other stream moves to `key` or past it; the furthest
            // one is where the lead goes next.
            let mut next = key;
            for t in 0..self.terms.len() {
                if t == lead {
                    continue;
                }
                let term = &mut self.terms[t];
                term.pos += term.keys[term.pos..].partition_point(|k| *k < key);
                match term.key() {
                    None => return,
                    Some(found) => next = next.max(found),
                }
            }
            for filter in &mut self.filters {
                filter.pos += filter.keys[filter.pos..].partition_point(|k| *k < key);
                match filter.key() {
                    None => return,
                    Some(found) => next = next.max(found),
                }
            }
            if next > key {
                let term = &mut self.terms[lead];
                term.pos += term.keys[term.pos..].partition_point(|k| *k < next);
                continue;
            }
            // Conjunction members share one document, so every term's bound
            // holds at the longest of the chunks' shortest documents.
            let min_length = (0..self.terms.len())
                .map(|t| self.terms[t].bound_block(self.terms[t].pos).shortest())
                .max()
                .expect("a conjunction has terms");
            let mut bound = 0.0_f32;
            for t in 0..self.terms.len() {
                let term = &self.terms[t];
                let scorer = &self.scorer.terms[term.slot].1;
                bound += scorer.bound_with_min_length(&term.bound_block(term.pos), min_length);
            }
            let base = u32::from(key) << 16;
            if self.threshold().is_some() && !self.can_beat(bound, base) {
                self.step_all();
                continue;
            }
            self.evaluate_all(key, lead, min_length, &mut set);
            self.step_all();
        }
    }

    /// Moves every term and filter past its current chunk.
    fn step_all(&mut self) {
        for term in &mut self.terms {
            term.pos += 1;
        }
        for filter in &mut self.filters {
            filter.pos += 1;
        }
    }

    /// Scores the documents of chunk `key` that every term and filter holds.
    fn evaluate_all(
        &mut self,
        key: u16,
        lead: usize,
        min_length: u32,
        set: &mut segment::ordinals::Words,
    ) {
        let base = u32::from(key) << 16;
        // The shared members: the lead's array tested against the others'
        // bits, or the words of every stream combined. Streams are loaded
        // rarest first and only while members remain, so a conjunction of
        // common words with a rare one reads the common words' chunks only
        // where the rare one has documents that survive.
        self.terms[lead].load();
        let sparse = !self.terms[lead].dense;
        let mut lows: Vec<u16> = Vec::new();
        if sparse {
            lows.extend_from_slice(&self.terms[lead].members);
        } else {
            set.copy_from_slice(&*self.terms[lead].words);
        }
        let mut order: Vec<usize> = (0..self.terms.len()).filter(|&t| t != lead).collect();
        order.sort_by_key(|&t| self.terms[t].keys.len());
        for t in order {
            if if sparse {
                lows.is_empty()
            } else {
                set.iter().all(|w| *w == 0)
            } {
                return;
            }
            let term = &mut self.terms[t];
            term.load();
            if sparse {
                lows.retain(|low| term.words[usize::from(low / 64)] & (1 << (low % 64)) != 0);
            } else {
                for (out, word) in set.iter_mut().zip(term.words.iter()) {
                    *out &= *word;
                }
            }
        }
        for filter in &mut self.filters {
            if if sparse {
                lows.is_empty()
            } else {
                set.iter().all(|w| *w == 0)
            } {
                return;
            }
            filter.load();
            if sparse {
                lows.retain(|low| filter.words[usize::from(low / 64)] & (1 << (low % 64)) != 0);
            } else {
                for (out, word) in set.iter_mut().zip(filter.words.iter()) {
                    *out &= *word;
                }
            }
        }
        let from = self.dead.partition_point(|o| *o < base);
        for dead in &self.dead[from..] {
            if *dead >= base + segment::ordinals::CHUNK {
                break;
            }
            let low = (dead - base) as usize;
            if sparse {
                if let Ok(at) = lows.binary_search(&(low as u16)) {
                    lows.remove(at);
                }
            } else {
                set[low / 64] &= !(1 << (low % 64));
            }
        }
        // Per sub-block, the best a shared document could score: each term's
        // largest bucket there at the shared shortest length; a sub-block
        // some term lacks holds no shared document.
        let mut sub_scores = [0.0_f32; SUBS];
        let mut sub_empty = [false; SUBS];
        for term in &mut self.terms {
            let scorer = &self.scorer.terms[term.slot].1;
            let by_bucket = term.bounds_by_bucket(term.pos, scorer, min_length);
            for (i, (sub, score)) in term.sub_bounds[term.pos]
                .iter()
                .zip(sub_scores.iter_mut())
                .enumerate()
            {
                if *sub == 0 {
                    sub_empty[i] = true;
                } else {
                    *score += by_bucket[usize::from(*sub - 1)];
                }
            }
        }
        let mut sparse_at = 0usize;
        let mut skip_sub = false;
        #[expect(
            clippy::needless_range_loop,
            reason = "the index addresses the sub-block and the sparse members too"
        )]
        for i in 0..segment::ordinals::WORDS {
            if i % (SUB / 64) == 0 {
                // The threshold moves as candidates are admitted, so it is
                // consulted afresh at every sub-block and candidate: the
                // chunk that fills the top k also prunes the rest of itself.
                let sub = i / (SUB / 64);
                let pruning = self.threshold().is_some();
                skip_sub = sub_empty[sub]
                    || (pruning && !self.can_beat(sub_scores[sub], base + (sub * SUB) as u32));
            }
            let mut word = if sparse {
                let mut word = 0u64;
                while sparse_at < lows.len() && usize::from(lows[sparse_at] / 64) == i {
                    word |= 1 << (lows[sparse_at] % 64);
                    sparse_at += 1;
                }
                word
            } else {
                set[i]
            };
            if word == 0 || skip_sub {
                continue;
            }
            while word != 0 {
                let low = (i * 64) as u16 + word.trailing_zeros() as u16;
                word &= word - 1;
                let ordinal = base + u32::from(low);
                let sub = usize::from(low) / SUB;
                let all: Vec<usize> = (0..self.terms.len()).collect();
                let pruning = self.threshold().is_some();
                let Some(total) =
                    self.score_candidate(&all, low, ordinal, sub, pruning, Some(min_length))
                else {
                    continue;
                };
                let admit =
                    self.heap.len() < self.k || self.heap.peek().is_some_and(|w| total >= w.0);
                if !admit {
                    continue;
                }
                let tid = self.resolve(ordinal);
                let candidate = Ranked(total, tid);
                if self.heap.len() < self.k {
                    if self.visibility.visible(tid) {
                        self.heap.push(candidate);
                    }
                } else if self.heap.peek().is_some_and(|w| candidate < *w)
                    && self.visibility.visible(tid)
                {
                    self.heap.pop();
                    self.heap.push(candidate);
                }
            }
        }
    }

    /// Scores the documents of chunk `key` that the terms `present` (in slot
    /// order) hold, against the bounds of those terms' chunks.
    /// The score of the document `low` of the current chunk, or `None` when
    /// it cannot reach the threshold. Of the terms `present` (in slot order)
    /// those holding the document are bounded by their sub-block's largest
    /// bucket at the document's length, which costs no read; the payloads
    /// are then read largest bound first and abandoned as soon as the exact
    /// contributions so far and the bounds of the rest fall short. Every sum
    /// is folded in slot order, as the exhaustive path folds the total, so
    /// the bound of a fully read candidate is its exact score, bit for bit.
    fn score_candidate(
        &mut self,
        present: &[usize],
        low: u16,
        ordinal: u32,
        sub: usize,
        pruning: bool,
        floor: Option<u32>,
    ) -> Option<f32> {
        // The scratch vectors are the walk's: a candidate is scored a
        // hundred thousand times a query, and two allocations each showed.
        let mut values = std::mem::take(&mut self.values);
        let mut uppers = std::mem::take(&mut self.uppers);
        values.clear();
        values.resize(present.len(), 0.0);
        uppers.clear();
        let score = self.score_candidate_in(
            present,
            low,
            ordinal,
            sub,
            pruning,
            floor,
            &mut values,
            &mut uppers,
        );
        self.values = values;
        self.uppers = uppers;
        score
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one call site; the arguments are the candidate and the scratch"
    )]
    fn score_candidate_in(
        &mut self,
        present: &[usize],
        low: u16,
        ordinal: u32,
        sub: usize,
        pruning: bool,
        floor: Option<u32>,
        values: &mut [f32],
        uppers: &mut Vec<(f32, usize)>,
    ) -> Option<f32> {
        // Per term of `present`: its bound, replaced by its exact score once
        // read; and the terms holding the document by descending bound.
        // First at the shortest document the bounds allow, which costs no
        // read: the length table is one page per 2,048 documents and
        // candidates are scattered, so reading a length before the bound
        // rejects the candidate was a page per candidate.
        for (n, &t) in present.iter().enumerate() {
            let term = &mut self.terms[t];
            if term.words[usize::from(low / 64)] & (1 << (low % 64)) == 0 {
                continue;
            }
            let scorer = &self.scorer.terms[term.slot].1;
            let top = term.sub_bounds[term.pos][sub];
            let upper = if top == 0 {
                0.0
            } else {
                term.bounds_by_bucket(term.pos, scorer, floor.unwrap_or(0))[usize::from(top - 1)]
            };
            values[n] = upper;
            uppers.push((upper, n));
        }
        let fold = |values: &[f32]| values.iter().fold(0.0_f32, |sum, v| sum + v);
        if pruning && !self.can_beat(fold(values), ordinal) {
            return None;
        }
        // The document's own length tightens every bound. Its class is a
        // byte per document and a lower bound on the length, so the bounds
        // are tightened at the class first; the exact length, four bytes per
        // document, is read only when the class bound admits the candidate.
        let bound_at = |values: &mut [f32], terms: &[OrdinalTerm<'_>], length: u32| {
            for &(_, n) in uppers.iter() {
                let term = &terms[present[n]];
                let scorer = &self.scorer.terms[term.slot].1;
                let top = term.sub_bounds[term.pos][sub];
                // The sub-block's largest bucket is a member's, so the score
                // at that bucket and this length bounds every member: no
                // sweep of the chunk's buckets tightens it.
                values[n] = if top == 0 {
                    0.0
                } else {
                    let bucket = TfBucket::new(top - 1).expect("bucket from a chunk bound");
                    scorer.score_bucket(bucket, length)
                };
            }
        };
        if pruning {
            let class = segment_error(self.index.length_class(ordinal));
            bound_at(
                values,
                &self.terms,
                segment::length_class::min_length(class),
            );
            if !self.can_beat(fold(values), ordinal) {
                return None;
            }
        }
        let length = segment_error(self.lengths.get(ordinal));
        bound_at(values, &self.terms, length);
        if pruning && !self.can_beat(fold(values), ordinal) {
            return None;
        }
        *self.scored += 1;
        for entry in uppers.iter_mut() {
            entry.0 = values[entry.1];
        }
        uppers.sort_by(|a, b| b.0.total_cmp(&a.0));
        for &(_, n) in uppers.iter() {
            let t = present[n];
            let term = &mut self.terms[t];
            let rank = term.rank(low).unwrap_or_else(|| {
                crate::storage::corrupt(format!(
                    "Stannum index data: ordinal {ordinal} is missing from a term's chunk"
                ))
            });
            let bucket = term.bucket(rank).unwrap_or_else(|| {
                crate::storage::corrupt(format!(
                    "Stannum index data: ordinal {ordinal} carries no term-frequency bucket"
                ))
            });
            let bucket = TfBucket::new(bucket).unwrap_or_else(|| {
                crate::storage::corrupt(format!(
                    "Stannum index data: term-frequency bucket {bucket} out of range"
                ))
            });
            values[n] = self.scorer.terms[term.slot].1.score_bucket(bucket, length);
            if pruning && !self.can_beat(fold(values), ordinal) {
                return None;
            }
        }
        Some(fold(values))
    }

    fn evaluate(&mut self, key: u16, present: &[usize], set: &mut segment::ordinals::Words) {
        let base = u32::from(key) << 16;
        // Every present term's chunk is loaded: a candidate is scored by
        // testing each term's bits for it.
        for &t in present {
            self.terms[t].load();
        }
        // The essential terms: sorted by chunk bound, the fewest whose absence
        // leaves the rest unable to reach the threshold. A candidate holds at
        // least one of them, so only their members are visited; the other
        // terms are tested by bit. Without a threshold every term is essential.
        let mut by_bound: Vec<(f32, usize)> = present
            .iter()
            .map(|&t| {
                let pos = self.terms[t].pos;
                let scorer = &self.scorer.terms[self.terms[t].slot].1;
                (self.terms[t].bound_score(pos, scorer), t)
            })
            .collect();
        by_bound.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut essential = by_bound.len();
        if let Some((threshold, _)) = self.threshold() {
            let mut tail = 0.0_f64;
            while essential > 0 {
                let next = tail + f64::from(by_bound[essential - 1].0);
                if next * (1.0 + f64::from(f32::EPSILON) * 256.0) >= f64::from(threshold) {
                    break;
                }
                tail = next;
                essential -= 1;
            }
            essential = essential.max(1);
        }
        let sparse = by_bound[..essential]
            .iter()
            .all(|(_, t)| !self.terms[*t].dense);
        let mut lows: Vec<u16> = Vec::new();
        if sparse {
            for (_, t) in &by_bound[..essential] {
                lows.extend_from_slice(&self.terms[*t].members);
            }
            lows.sort_unstable();
            lows.dedup();
        } else {
            set.fill(0);
            for (_, t) in &by_bound[..essential] {
                for (out, word) in set.iter_mut().zip(self.terms[*t].words.iter()) {
                    *out |= *word;
                }
            }
        }
        // Dead documents leave the candidates, whichever form they take: a
        // mask applied to the union before it is rebuilt from the essential
        // terms is lost, and scored a deleted document's location, by then
        // reused by a row that never matched.
        let from = self.dead.partition_point(|o| *o < base);
        for dead in &self.dead[from..] {
            if *dead >= base + segment::ordinals::CHUNK {
                break;
            }
            let low = (dead - base) as usize;
            if sparse {
                if let Ok(at) = lows.binary_search(&(low as u16)) {
                    lows.remove(at);
                }
            } else {
                set[low / 64] &= !(1 << (low % 64));
            }
        }
        // Per sub-block, the best a candidate could score: the sum over the
        // present terms of their largest bucket there at the chunk's shortest
        // document; sub-blocks that cannot reach the threshold are skipped.
        let mut sub_scores = [0.0_f32; SUBS];
        for &t in present {
            let term = &mut self.terms[t];
            let scorer = &self.scorer.terms[term.slot].1;
            let by_bucket = term.bounds_by_bucket(term.pos, scorer, 0);
            for (sub, score) in term.sub_bounds[term.pos].iter().zip(sub_scores.iter_mut()) {
                if *sub > 0 {
                    *score += by_bucket[usize::from(*sub - 1)];
                }
            }
        }
        let mut sparse_at = 0usize;
        // A sub-block is judged once, at its first word: the walk resolves
        // locations in ordinal order, so the judgement cannot be repeated
        // after a candidate of the sub-block has been resolved.
        let mut skip_sub = false;
        #[expect(
            clippy::needless_range_loop,
            reason = "the index addresses the sub-block and the sparse members too"
        )]
        for i in 0..segment::ordinals::WORDS {
            if i % (SUB / 64) == 0 {
                // The threshold moves as candidates are admitted, so it is
                // consulted afresh at every sub-block and candidate: the
                // chunk that fills the top k also prunes the rest of itself.
                let sub = i / (SUB / 64);
                let pruning = self.threshold().is_some();
                skip_sub = pruning && !self.can_beat(sub_scores[sub], base + (sub * SUB) as u32);
            }
            let mut word = if sparse {
                let mut word = 0u64;
                while sparse_at < lows.len() && usize::from(lows[sparse_at] / 64) == i {
                    word |= 1 << (lows[sparse_at] % 64);
                    sparse_at += 1;
                }
                word
            } else {
                set[i]
            };
            if word == 0 || skip_sub {
                continue;
            }
            while word != 0 {
                let low = (i * 64) as u16 + word.trailing_zeros() as u16;
                word &= word - 1;
                let ordinal = base + u32::from(low);
                let sub = usize::from(low) / SUB;
                let pruning = self.threshold().is_some();
                let Some(total) = self.score_candidate(present, low, ordinal, sub, pruning, None)
                else {
                    continue;
                };
                let admit =
                    self.heap.len() < self.k || self.heap.peek().is_some_and(|w| total >= w.0);
                if !admit {
                    continue;
                }
                let tid = self.resolve(ordinal);
                let candidate = Ranked(total, tid);
                if self.heap.len() < self.k {
                    if self.visibility.visible(tid) {
                        self.heap.push(candidate);
                    }
                } else if self.heap.peek().is_some_and(|w| candidate < *w)
                    && self.visibility.visible(tid)
                {
                    self.heap.pop();
                    self.heap.push(candidate);
                }
            }
        }
    }
}

/// Heap visibility of index locations under the active snapshot, following
/// HOT chains as an index scan would. Opened once per walk over the index.
struct Visibility {
    heap: pg_sys::Relation,
    fetch: *mut pg_sys::IndexFetchTableData,
    slot: *mut pg_sys::TupleTableSlot,
    snapshot: pg_sys::Snapshot,
    /// The visibility map page last consulted, pinned.
    vmbuf: pg_sys::Buffer,
    /// Whether an all-visible page answers without a heap read.
    shortcut: bool,
    /// Whether any check was answered by the map.
    shortcuts: bool,
}

impl Visibility {
    /// # Safety
    /// `heap_oid` names a relation the caller may open; an active snapshot
    /// exists and outlives the value.
    unsafe fn open(heap_oid: pg_sys::Oid, shortcut: bool) -> Self {
        unsafe {
            let heap = pg_sys::table_open(heap_oid, pg_sys::AccessShareLock as _);
            Self {
                heap,
                fetch: pg_sys::table_index_fetch_begin(heap),
                slot: pg_sys::table_slot_create(heap, std::ptr::null_mut()),
                snapshot: pg_sys::GetActiveSnapshot(),
                vmbuf: pg_sys::InvalidBuffer as pg_sys::Buffer,
                shortcut,
                shortcuts: false,
            }
        }
    }

    /// Whether the snapshot sees a tuple at `tid` or on its HOT chain. On an
    /// all-visible page every tuple is visible to every snapshot, and the
    /// index lists no tuple VACUUM removed, so the map alone answers.
    fn visible(&mut self, tid: Tid) -> bool {
        VISIBILITY_CHECKS.set(VISIBILITY_CHECKS.get() + 1);
        if self.shortcut {
            let status =
                unsafe { pg_sys::visibilitymap_get_status(self.heap, tid.block, &mut self.vmbuf) };
            if status & pg_sys::VISIBILITYMAP_ALL_VISIBLE as u8 != 0 {
                VM_HITS.set(VM_HITS.get() + 1);
                self.shortcuts = true;
                return true;
            }
        }
        charging("heap visibility", || self.visible_inner(tid))
    }

    fn visible_inner(&mut self, tid: Tid) -> bool {
        let mut pointer = pg_sys::ItemPointerData {
            ip_blkid: pg_sys::BlockIdData {
                bi_hi: (tid.block >> 16) as u16,
                bi_lo: tid.block as u16,
            },
            ip_posid: tid.offset,
        };
        let mut call_again = false;
        let mut all_dead = false;
        loop {
            if unsafe {
                pg_sys::table_index_fetch_tuple(
                    self.fetch,
                    &mut pointer,
                    self.snapshot,
                    self.slot,
                    &mut call_again,
                    &mut all_dead,
                )
            } {
                return true;
            }
            if !call_again {
                return false;
            }
        }
    }
}

impl Drop for Visibility {
    fn drop(&mut self) {
        unsafe {
            if self.vmbuf != pg_sys::InvalidBuffer as pg_sys::Buffer {
                pg_sys::ReleaseBuffer(self.vmbuf);
            }
            pg_sys::ExecDropSingleTupleTableSlot(self.slot);
            pg_sys::table_index_fetch_end(self.fetch);
            pg_sys::table_close(self.heap, pg_sys::AccessShareLock as _);
        }
    }
}

/// Filters tuple locations to those visible under the active snapshot.
///
/// # Safety
/// As [`Visibility::open`].
unsafe fn visible_tids(heap_oid: pg_sys::Oid, tids: BTreeSet<Tid>) -> Vec<Tid> {
    let mut visibility = unsafe { Visibility::open(heap_oid, false) };
    let mut visible = Vec::new();
    for tid in tids {
        pgrx::check_for_interrupts!();
        if visibility.visible(tid) {
            visible.push(tid);
        }
    }
    visible
}

/// A fresh identity for a ranked scan; see [`SCAN_SCORERS`].
pub(crate) fn scan_id() -> u64 {
    next_stamp()
}

/// Records the row the scan just emitted; see [`ScanScorer::emitted`].
pub(crate) fn note_scan_emitted(scan: u64, member: Tid, root: Tid) {
    SCAN_SCORERS.with_borrow_mut(|scans| {
        if let Some(entry) = scans.iter_mut().find(|entry| entry.scan == scan) {
            entry.emitted = Some(Emitted {
                member,
                root,
                statement: current_statement(),
                stamp: next_stamp(),
            });
        }
    });
}

/// Drops the scorer a scan published, when the scan ends.
pub(crate) fn forget_scan_scorer(scan: u64) {
    SCAN_SCORERS.with_borrow_mut(|scans| scans.retain(|entry| entry.scan != scan));
}

/// The scorer for a custom scan's top-k ordering, from the bound arguments
/// of a `score_bound_indexed` call: the one the scan published earlier (when
/// it completes a pruned top k), otherwise a new one over the current index.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the bound scoring function's arguments"
)]
pub(crate) fn scorer_for_scan(
    scan: u64,
    heap_oid: u32,
    index_oid: u32,
    query: &str,
    full: bool,
    dense_ratio: Option<f32>,
    k1: Option<f32>,
    b: Option<f32>,
    term_add: Option<Vec<String>>,
    term_replace: Option<Vec<String>>,
) -> IndexScorer {
    let key = CacheKey {
        statement: current_statement(),
        heap_oid,
        index_oid,
        query: query.to_owned(),
        full,
        dense: dense_ratio.unwrap_or(DenseRatio::DEFAULT).to_bits(),
        k1: bits(k1),
        b: bits(b),
        add: term_add.clone(),
        replace: term_replace.clone(),
    };
    let published = SCAN_SCORERS.with_borrow_mut(|scans| {
        scans
            .iter()
            .position(|entry| entry.scan == scan)
            .map(|at| scans.remove(at).scorer)
    });
    match published {
        Some(mut scorer) => {
            // Rebuilt readers: the retained cursors may sit past rows a
            // completed ordering scores again.
            scorer.key = key;
            scorer.sources = scorer
                .view
                .sources
                .iter()
                .map(|(index, _)| unsafe { SourceReader::new(&**index, &scorer.terms) })
                .collect();
            scorer
        }
        None => build_index_scorer(key, k1, b, term_add, term_replace),
    }
}

/// Keeps the scan's scorer, with the score of every row it ranked, for the
/// SQL score functions to project from until the scan ends.
pub(crate) fn publish_scan_scorer(scan: u64, mut scorer: IndexScorer, ranked: &[(f32, Tid)]) {
    scorer.known.clear();
    scorer
        .known
        .extend(ranked.iter().map(|(score, tid)| (*tid, *score)));
    SCAN_SCORERS.with_borrow_mut(|scans| {
        scans.retain(|entry| entry.scan != scan);
        scans.push(ScanScorer {
            scan,
            emitted: None,
            scorer,
        });
    });
}

fn build_index_scorer(
    key: CacheKey,
    k1: Option<f32>,
    b: Option<f32>,
    term_add: Option<Vec<String>>,
    term_replace: Option<Vec<String>>,
) -> IndexScorer {
    charging("scorer setup", || {
        build_index_scorer_inner(key, k1, b, term_add, term_replace)
    })
}

fn build_index_scorer_inner(
    key: CacheKey,
    k1: Option<f32>,
    b: Option<f32>,
    term_add: Option<Vec<String>>,
    term_replace: Option<Vec<String>>,
) -> IndexScorer {
    let heap_oid = pg_sys::Oid::from(key.heap_oid);
    let index = unsafe {
        PgRelation::with_lock(
            pg_sys::Oid::from(key.index_oid),
            pg_sys::AccessShareLock as _,
        )
    };
    if unsafe { pg_sys::IndexGetRelation(index.oid(), false) } != heap_oid {
        pgrx::error!("stannum score index no longer belongs to the scored relation");
    }
    let tokenizer = unsafe { crate::storage::index_tokenizer(index.as_ptr()) };
    let defaults = unsafe { crate::options::bm25(index.as_ptr()) };
    let stop_csv = unsafe { crate::options::score_stop_words(index.as_ptr()) };
    let params = Bm25Overrides { k1, b }
        .resolve(defaults)
        .checked()
        .unwrap_or_else(|error| pgrx::error!("stannum score parameters: {error}"));
    let dense = DenseRatio::new(Some(f32::from_bits(key.dense)));
    if !key.full && !dense.is_valid() {
        pgrx::error!("dense_ratio must be finite and non-negative");
    }
    let query = parse_tinql_to_query(&key.query, tokenizer.as_ref())
        .unwrap_or_else(|error| pgrx::error!("Stannum score query error: {error}"));
    let edit = TermSetEdit::from_bound_arrays(term_add, term_replace)
        .unwrap_or_else(|error| pgrx::error!("stannum.score(): {error}"))
        .analyzed_with(|text| {
            tokenizer
                .tokenize(text)
                .map(|token| token.text.into_owned())
                .collect::<Vec<_>>()
        });
    let stop = if key.full {
        None
    } else {
        stop_csv.as_deref().and_then(ScoreStopWords::from_csv)
    };

    let view = unsafe { crate::storage::view(index.oid()) };
    let segments: Vec<&dyn Index> = view.sources.iter().map(|(index, _)| &**index).collect();
    let mut collected = Collected::default();
    collect_score_terms(&query, 1.0, false, &mut collected);
    let owned = collected.resolve(|expansion| expansion.expand_in(&segments));
    let terms = compile_scoring_terms(inputs_of(&owned), &edit, stop.as_ref());
    let dead = view.dead_sets.clone();
    // Statistics include dead documents until their segment is rewritten,
    // and buffered documents immediately; elision uses immutable segments only.
    let is_immutable = |i: usize| i < view.immutable_sources;
    let total_docs: u64 = segments.iter().map(|s| u64::from(s.document_count())).sum();
    let immutable_docs: u64 = segments
        .iter()
        .enumerate()
        .filter(|(i, _)| is_immutable(*i))
        .map(|(_, s)| u64::from(s.document_count()))
        .sum();
    let total_length: u64 = segments.iter().map(|s| s.total_length()).sum();
    let average_length = if total_docs == 0 {
        1.0
    } else {
        total_length as f32 / total_docs as f32
    };
    let mut scorers = Vec::new();
    for term in terms {
        let mut total_df = 0u64;
        let mut immutable_df = 0u64;
        for (i, segment) in segments.iter().enumerate() {
            let df = segment_error(segment.term(term.text())).map_or(0, |t| u64::from(t.df()));
            total_df += df;
            if is_immutable(i) {
                immutable_df += df;
            }
        }
        let ratio = (!key.full).then_some(dense);
        if !term.is_retained(total_df, immutable_df, immutable_docs, ratio) {
            continue;
        }
        let scorer =
            TermScorer::from_statistics(total_docs, total_df, term.boost(), params, average_length)
                .unwrap_or_else(|error| pgrx::error!("stannum score parameters: {error}"));
        scorers.push((term.text().to_owned(), scorer));
    }
    drop(segments);
    let sources = view
        .sources
        .iter()
        .map(|(index, _)| unsafe { SourceReader::new(&**index, &scorers) })
        .collect();
    IndexScorer {
        key,
        sources,
        view,
        dead,
        terms: scorers,
        query,
        max: None,
        known: FxHashMap::default(),
    }
}

impl IndexScorer {
    /// Maximum score over the visible documents matching the query, as TIN
    /// reports over its result rows. Restores the readers afterwards.
    fn max_score(&mut self) -> f32 {
        if let Some(max) = self.max {
            return max;
        }
        let mut candidates = BTreeSet::new();
        for (i, (segment, _)) in self.view.sources.iter().enumerate() {
            let planned = plan(&self.query, &**segment, &Limits::default())
                .unwrap_or_else(|error| pgrx::error!("Stannum query plan: {error}"));
            let mut cursor = planned.cursor;
            while let Some(tid) = cursor.current() {
                if !self.dead[i].contains(&tid) {
                    candidates.insert(tid);
                }
                segment_error_in(cursor.advance(), &self.view.labels[i]);
            }
        }
        // The index cannot see deletes that VACUUM has not reported yet, so
        // each candidate is checked against the active snapshot.
        let visible = unsafe { visible_tids(pg_sys::Oid::from(self.key.heap_oid), candidates) };
        let mut max = 0.0_f32;
        for tid in visible {
            pgrx::check_for_interrupts!();
            max = max.max(self.score(tid));
        }
        self.sources = self
            .view
            .sources
            .iter()
            .map(|(index, _)| unsafe { SourceReader::new(&**index, &self.terms) })
            .collect();
        self.max = Some(max);
        max
    }
}

fn build_corpus(
    key: CacheKey,
    k1: Option<f32>,
    b: Option<f32>,
    term_add: Option<Vec<String>>,
    term_replace: Option<Vec<String>>,
) -> ScoreCorpus {
    let heap_oid = pg_sys::Oid::from(key.heap_oid);
    let index = unsafe {
        PgRelation::with_lock(
            pg_sys::Oid::from(key.index_oid),
            pg_sys::AccessShareLock as _,
        )
    };
    if unsafe { pg_sys::IndexGetRelation(index.oid(), false) } != heap_oid {
        pgrx::error!("stannum score index no longer belongs to the scored relation");
    }
    let tokenizer = unsafe { crate::options::tokenizer(index.as_ptr()) };
    let defaults = unsafe { crate::options::bm25(index.as_ptr()) };
    let stop_csv = unsafe { crate::options::score_stop_words(index.as_ptr()) };
    let params = Bm25Overrides { k1, b }
        .resolve(defaults)
        .checked()
        .unwrap_or_else(|error| pgrx::error!("stannum score parameters: {error}"));
    let dense = DenseRatio::new(Some(f32::from_bits(key.dense)));
    if !key.full && !dense.is_valid() {
        pgrx::error!("dense_ratio must be finite and non-negative");
    }
    let query = parse_tinql_to_query(&key.query, &tokenizer)
        .unwrap_or_else(|error| pgrx::error!("Stannum score query error: {error}"));
    let edit = TermSetEdit::from_bound_arrays(term_add, term_replace)
        .unwrap_or_else(|error| pgrx::error!("stannum.score(): {error}"))
        .analyzed_with(|text| {
            tokenizer
                .tokenize(text)
                .map(|token| token.text.into_owned())
                .collect::<Vec<_>>()
        });
    let stop = if key.full {
        None
    } else {
        stop_csv.as_deref().and_then(ScoreStopWords::from_csv)
    };
    let documents = load_documents(heap_oid, index.oid());
    let positioned = tokenize_documents(&documents, |document| tokenize_doc(document, &tokenizer));
    let tokenized: Vec<Vec<String>> = positioned.iter().map(|doc| doc.tokens().to_vec()).collect();
    let universe = corpus_universe(&tokenized);
    let mut collected = Collected::default();
    collect_score_terms(&query, 1.0, false, &mut collected);
    let owned = collected.resolve(|expansion| {
        let matcher = expansion.matcher();
        universe
            .iter()
            .filter(|term| matcher(term))
            .map(|term| (*term).to_owned())
            .collect()
    });
    let terms = compile_scoring_terms(inputs_of(&owned), &edit, stop.as_ref());
    // Token-less documents are not documents for scoring, as in TIN.
    let total_docs = tokenized.iter().filter(|tokens| !tokens.is_empty()).count() as u64;
    let average_length = if total_docs == 0 {
        1.0
    } else {
        tokenized.iter().map(Vec::len).sum::<usize>() as f32 / total_docs as f32
    };
    let mut scorers = Vec::new();
    for term in terms {
        let df = tokenized
            .iter()
            .filter(|tokens| tokens.iter().any(|token| token == term.text()))
            .count() as u64;
        let ratio = (!key.full).then_some(dense);
        if !term.is_retained(df, df, total_docs, ratio) {
            continue;
        }
        let scorer =
            TermScorer::from_statistics(total_docs, df, term.boost(), params, average_length)
                .unwrap_or_else(|error| pgrx::error!("stannum score parameters: {error}"));
        scorers.push((term.text().to_owned(), scorer));
    }
    let mut by_document = FxHashMap::default();
    let mut max = 0.0_f32;
    for ((document, tokens), doc) in documents.into_iter().zip(tokenized).zip(&positioned) {
        let score = sum_scores_in_order(scorers.iter().map(|(term, scorer)| {
            let tf = tokens.iter().filter(|token| *token == term).count() as u32;
            if tf == 0 {
                0.0
            } else {
                scorer.score_count(tf, tokens.len() as u32)
            }
        }));
        // The maximum is over matching documents only, as in TIN.
        let matched = evaluate(&query, doc)
            .unwrap_or_else(|error| pgrx::error!("stannum score query evaluation failed: {error}"))
            .matched;
        if matched {
            max = max.max(score);
        }
        by_document.insert(document, score);
    }
    ScoreCorpus {
        key,
        by_document,
        max,
    }
}

// Both heap-scoring paths can spend a long time tokenizing after SPI returns.
// Keep the interrupt cadence shared while allowing positioned or plain tokens.
pub(super) fn tokenize_documents<T>(
    documents: &[String],
    mut tokenize: impl FnMut(&str) -> T,
) -> Vec<T> {
    documents
        .iter()
        .enumerate()
        .map(|(row, document)| {
            if row.is_multiple_of(10) {
                pgrx::check_for_interrupts!();
            }
            tokenize(document)
        })
        .collect()
}

fn load_documents(heap_oid: pg_sys::Oid, index_oid: pg_sys::Oid) -> Vec<String> {
    unsafe {
        let relname = pg_sys::get_rel_name(heap_oid);
        let namespace = pg_sys::get_namespace_name(pg_sys::get_rel_namespace(heap_oid));
        if relname.is_null() || namespace.is_null() {
            pgrx::error!("stannum score relation no longer exists");
        }
        let qualified = pg_sys::quote_qualified_identifier(namespace, relname);
        let index_sql = format!(
            "SELECT CASE WHEN i.indkey[0] = 0 \
             THEN pg_catalog.pg_get_expr(i.indexprs, i.indrelid) \
             ELSE pg_catalog.quote_ident(a.attname) END, \
             pg_catalog.pg_get_expr(i.indpred, i.indrelid) \
             FROM pg_catalog.pg_index i \
             LEFT JOIN pg_catalog.pg_attribute a \
               ON a.attrelid=i.indrelid AND a.attnum=i.indkey[0] \
             WHERE i.indexrelid={}::oid AND i.indrelid={}::oid",
            index_oid.to_u32(),
            heap_oid.to_u32(),
        );
        // pgrx's Spi::get_two uses a mutable connection and assigns an XID.
        // This catalog lookup must remain read-only for standby heap scoring.
        let (expression, predicate) = Spi::connect(|client| {
            client
                .select(&index_sql, Some(1), &[])?
                .first()
                .get_two::<String, String>()
        })
        .unwrap_or_else(|error| pgrx::error!("stannum score index lookup failed: {error}"));
        let expression = expression
            .unwrap_or_else(|| pgrx::error!("stannum score index expression no longer exists"));
        let predicate = predicate
            .map(|predicate| format!(" AND ({predicate})"))
            .unwrap_or_default();
        let sql = format!(
            "SELECT ({expression})::text FROM {} WHERE ({expression}) IS NOT NULL{predicate}",
            CStr::from_ptr(qualified).to_string_lossy(),
        );
        Spi::connect(|client| {
            client
                .select(&sql, None, &[])
                .unwrap_or_else(|error| pgrx::error!("stannum score corpus scan failed: {error}"))
                .map(|row| {
                    row.get::<String>(1)
                        .unwrap_or_else(|error| {
                            pgrx::error!("stannum score corpus row failed: {error}")
                        })
                        .expect("corpus query excludes null documents")
                })
                .collect()
        })
    }
}

/// A query node that scores every dictionary term it expands to, as TIN does.
enum Expansion<'a> {
    Regex(&'a CompiledRegex),
    Range(&'a RangeBound, &'a RangeBound),
    Fuzzy {
        term: &'a str,
        prefix: u32,
        distance: u32,
    },
}

impl Expansion<'_> {
    fn matcher(&self) -> Box<dyn Fn(&str) -> bool + '_> {
        match self {
            Self::Regex(regex) => Box::new(move |candidate| regex.is_match(candidate)),
            Self::Range(lower, upper) => {
                Box::new(move |candidate| range_matches(candidate, lower, upper))
            }
            Self::Fuzzy {
                term,
                prefix,
                distance,
            } => {
                let matcher = FuzzyMatcher::new(term, *prefix, *distance);
                Box::new(move |candidate| matcher.is_match(candidate))
            }
        }
    }

    /// Every matching term across the given indexes.
    fn expand_in(&self, segments: &[&dyn Index]) -> Vec<String> {
        let mut found = BTreeSet::new();
        let matcher = self.matcher();
        for segment in segments {
            let expanded = match self {
                Self::Regex(regex) => match regex.pure_prefix() {
                    Some(prefix) => segment.expand(Window::Prefix(&prefix), &|_| true, usize::MAX),
                    None => segment.expand(Window::All, &*matcher, usize::MAX),
                },
                Self::Range(lower, upper) => {
                    fn bound(bound: &RangeBound) -> Option<&str> {
                        match bound {
                            RangeBound::Open => None,
                            RangeBound::Term(term) => Some(term.as_str()),
                        }
                    }
                    segment.expand(
                        Window::Range(bound(lower), bound(upper)),
                        &|_| true,
                        usize::MAX,
                    )
                }
                Self::Fuzzy { term, prefix, .. } => {
                    let fixed: String = term.chars().take(*prefix as usize).collect();
                    segment.expand(Window::Prefix(&fixed), &*matcher, usize::MAX)
                }
            };
            if let Expanded::Terms(terms) = segment_error(expanded) {
                found.extend(terms.into_iter().map(|(t, _)| t));
            }
        }
        found.into_iter().collect()
    }
}

/// Scoring inputs gathered from a query before expansions are resolved.
#[derive(Default)]
struct Collected<'a> {
    terms: Vec<ScoringTermInput<'a>>,
    expansions: Vec<(Expansion<'a>, f32, bool)>,
}

impl<'a> Collected<'a> {
    /// Resolves expansions through `expand` and returns owned inputs.
    fn resolve(
        self,
        mut expand: impl FnMut(&Expansion<'a>) -> Vec<String>,
    ) -> Vec<(String, f32, bool)> {
        let mut out: Vec<(String, f32, bool)> = self
            .terms
            .iter()
            .map(|input| (input.text.to_owned(), input.boost, input.explicitly_boosted))
            .collect();
        for (expansion, boost, explicit) in &self.expansions {
            for term in expand(expansion) {
                out.push((term, *boost, *explicit));
            }
        }
        out
    }
}

fn inputs_of(owned: &[(String, f32, bool)]) -> impl Iterator<Item = ScoringTermInput<'_>> {
    owned
        .iter()
        .map(|(text, boost, explicit)| ScoringTermInput {
            text,
            boost: *boost,
            explicitly_boosted: *explicit,
        })
}

/// Boolean NOT contributes nothing to scoring; negative span relations keep
/// both sides. Wildcards, regexes, ranges and fuzzy terms score every term
/// they expand to with the node's boost.
fn collect_score_terms<'a>(
    query: &'a Query,
    boost: f32,
    explicitly_boosted: bool,
    out: &mut Collected<'a>,
) {
    let mut push = |text: &'a str| {
        out.terms.push(ScoringTermInput {
            text,
            boost,
            explicitly_boosted,
        });
    };
    match query {
        Query::Term(text) => push(text),
        Query::Fuzzy {
            term,
            prefix,
            distance,
        } => out.expansions.push((
            Expansion::Fuzzy {
                term,
                prefix: *prefix,
                distance: *distance,
            },
            boost,
            explicitly_boosted,
        )),
        Query::Regex(regex) => {
            out.expansions
                .push((Expansion::Regex(regex), boost, explicitly_boosted))
        }
        Query::Range { lower, upper } => {
            out.expansions
                .push((Expansion::Range(lower, upper), boost, explicitly_boosted))
        }
        Query::Span { term_slots, .. } | Query::SpanExpr { term_slots, .. } => {
            for slot in term_slots {
                match slot {
                    SpanTermSlot::Term(text) => push(text),
                    SpanTermSlot::Regex(regex) => {
                        out.expansions
                            .push((Expansion::Regex(regex), boost, explicitly_boosted))
                    }
                    SpanTermSlot::Range { lower, upper } => out.expansions.push((
                        Expansion::Range(lower, upper),
                        boost,
                        explicitly_boosted,
                    )),
                    SpanTermSlot::Fuzzy {
                        term,
                        prefix,
                        distance,
                    } => out.expansions.push((
                        Expansion::Fuzzy {
                            term,
                            prefix: *prefix,
                            distance: *distance,
                        },
                        boost,
                        explicitly_boosted,
                    )),
                }
            }
        }
        Query::And(left, right) | Query::Or(left, right) => {
            collect_score_terms(left, boost, explicitly_boosted, out);
            collect_score_terms(right, boost, explicitly_boosted, out);
        }
        Query::Conjunction(children)
        | Query::Disjunction { children, .. }
        | Query::AtLeast { children, .. } => {
            for child in children {
                collect_score_terms(child, boost, explicitly_boosted, out);
            }
        }
        Query::Not(_) | Query::MatchAll => {}
        Query::Boost { factor, inner } => {
            collect_score_terms(inner, boost * *factor, true, out);
        }
    }
}

/// Distinct tokens of a corpus, the expansion universe without a dictionary.
fn corpus_universe(tokenized: &[Vec<String>]) -> BTreeSet<&str> {
    tokenized
        .iter()
        .flat_map(|tokens| tokens.iter().map(String::as_str))
        .collect()
}

#[pg_extern(volatile, parallel_unsafe)]
fn score_inspect(
    index: Option<PgRelation>,
    query: Option<&str>,
    dense_ratio: default!(Option<f32>, 0.10),
    term_add: default!(Option<Vec<Option<String>>>, "NULL"),
    term_replace: default!(Option<Vec<Option<String>>>, "NULL"),
) -> TableIterator<'static, (name!(term, String), name!(weight, f32))> {
    let (Some(index), Some(query)) = (index, query) else {
        return TableIterator::new(Vec::new());
    };
    let stannum_name = CString::new("stannum").expect("static access method name is valid");
    let stannum_am = unsafe { pg_sys::get_index_am_oid(stannum_name.as_ptr(), false) };
    if unsafe { (*(*index.as_ptr()).rd_rel).relam } != stannum_am {
        pgrx::error!("stannum.score_inspect() requires a stannum index");
    }
    let unwrap = |which: &str, values: Option<Vec<Option<String>>>| {
        values.map(|values| {
            values
                .into_iter()
                .map(|value| {
                    value.unwrap_or_else(|| {
                        pgrx::error!(
                            "stannum.score_inspect() {which} array elements must not be NULL"
                        )
                    })
                })
                .collect::<Vec<_>>()
        })
    };
    crate::udfs::require_index_select(&index);
    let heap_oid = unsafe { pg_sys::IndexGetRelation(index.oid(), false) };
    let tokenizer = unsafe { crate::options::tokenizer(index.as_ptr()) };
    let parsed = parse_tinql_to_query(query, &tokenizer)
        .unwrap_or_else(|error| pgrx::error!("stannum.score_inspect() query error: {error}"));
    let edit = TermSetEdit::from_bound_arrays(
        unwrap("term_add", term_add),
        unwrap("term_replace", term_replace),
    )
    .unwrap_or_else(|error| pgrx::error!("stannum.score_inspect(): {error}"))
    .analyzed_with(|text| {
        tokenizer
            .tokenize(text)
            .map(|t| t.text.into_owned())
            .collect::<Vec<_>>()
    });
    let stop_csv = unsafe { crate::options::score_stop_words(index.as_ptr()) };
    let stop = stop_csv.as_deref().and_then(ScoreStopWords::from_csv);
    let ratio = DenseRatio::new(dense_ratio);
    if !ratio.is_valid() {
        pgrx::error!("dense_ratio must be finite and non-negative");
    }
    let mut collected = Collected::default();
    collect_score_terms(&parsed, 1.0, false, &mut collected);
    let rows = if unsafe { crate::storage::present(index.as_ptr()) } {
        let view = unsafe { crate::storage::view(index.oid()) };
        let segments: Vec<&dyn Index> = view.sources.iter().map(|(index, _)| &**index).collect();
        let owned = collected.resolve(|expansion| expansion.expand_in(&segments));
        let terms = compile_scoring_terms(inputs_of(&owned), &edit, stop.as_ref());
        let is_immutable = |i: usize| i < view.immutable_sources;
        let immutable_docs: u64 = segments
            .iter()
            .enumerate()
            .filter(|(i, _)| is_immutable(*i))
            .map(|(_, s)| u64::from(s.document_count()))
            .sum();
        terms
            .into_iter()
            .filter_map(|term| {
                let (mut total_df, mut immutable_df) = (0u64, 0u64);
                for (i, segment) in segments.iter().enumerate() {
                    let df =
                        segment_error(segment.term(term.text())).map_or(0, |t| u64::from(t.df()));
                    total_df += df;
                    if is_immutable(i) {
                        immutable_df += df;
                    }
                }
                term.is_retained(total_df, immutable_df, immutable_docs, Some(ratio))
                    .then(|| (term.text().to_owned(), term.boost()))
            })
            .collect::<Vec<_>>()
    } else {
        let docs = load_documents(heap_oid, index.oid());
        let tokenized = tokenize_documents(&docs, |doc| {
            tokenizer
                .tokenize(doc)
                .map(|t| t.text.into_owned())
                .collect::<Vec<_>>()
        });
        let universe = corpus_universe(&tokenized);
        let owned = collected.resolve(|expansion| {
            let matcher = expansion.matcher();
            universe
                .iter()
                .filter(|term| matcher(term))
                .map(|term| (*term).to_owned())
                .collect()
        });
        let terms = compile_scoring_terms(inputs_of(&owned), &edit, stop.as_ref());
        let n = tokenized.iter().filter(|tokens| !tokens.is_empty()).count() as u64;
        terms
            .into_iter()
            .filter_map(|term| {
                let df = tokenized
                    .iter()
                    .filter(|doc| doc.iter().any(|t| t == term.text()))
                    .count() as u64;
                term.is_retained(df, df, n, Some(ratio))
                    .then(|| (term.text().to_owned(), term.boost()))
            })
            .collect::<Vec<_>>()
    };
    TableIterator::new(rows)
}

/// The `==>` clauses of a qual tree as (document, text query, bound index).
struct QualBinding {
    matches: Vec<(*mut pg_sys::Node, *mut pg_sys::Node, Option<pg_sys::Oid>)>,
}

#[pg_guard]
unsafe extern "C-unwind" fn find_qual(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
    if node.is_null() {
        return false;
    }
    let binding = unsafe { &mut *context.cast::<QualBinding>() };
    if let Some(clause) = unsafe { crate::operator::search_clause(node) } {
        binding
            .matches
            .push((clause.document, clause.query, clause.index));
    }
    unsafe { pg_sys::expression_tree_walker(node, Some(find_qual), context) }
}

/// Pulling EXISTS/NOT EXISTS up can wrap the original FromExpr in a join,
/// leaving the new top-level FromExpr with no quals. Keep finding its WHERE
/// clauses without treating JOIN ON expressions or nested query scopes as
/// additional scoring predicates. Visit parent quals first to retain binding order.
unsafe fn find_where_quals(node: *mut pg_sys::Node, binding: &mut QualBinding) {
    unsafe {
        if node.is_null() {
            return;
        }
        pg_sys::check_stack_depth();
        match (*node).type_ {
            pg_sys::NodeTag::T_FromExpr => {
                let from = &*node.cast::<pg_sys::FromExpr>();
                find_qual(from.quals, (binding as *mut QualBinding).cast());
                for i in 0..pg_sys::list_length(from.fromlist) {
                    find_where_quals(pg_sys::list_nth(from.fromlist, i).cast(), binding);
                }
            }
            pg_sys::NodeTag::T_JoinExpr => {
                let join = &*node.cast::<pg_sys::JoinExpr>();
                find_where_quals(join.larg, binding);
                find_where_quals(join.rarg, binding);
            }
            _ => {}
        }
    }
}

/// The index a clause bound to `bound` should be answered by, among
/// `candidates` (in OID order): the bound index when it is one of them,
/// otherwise the first with the same tokenizer settings, so an index scan
/// never disagrees with the clause's own evaluation.
pub(crate) unsafe fn pick_index(
    candidates: &[pg_sys::Oid],
    bound: Option<pg_sys::Oid>,
) -> Option<pg_sys::Oid> {
    match bound {
        Some(bound) if candidates.contains(&bound) => Some(bound),
        Some(bound) => {
            let spec = unsafe { crate::storage::spec_by_oid(bound) };
            candidates
                .iter()
                .copied()
                .find(|&candidate| unsafe { crate::storage::spec_by_oid(candidate) } == spec)
        }
        None => candidates.first().copied(),
    }
}

/// Every valid, ready, single-key stannum index of `heap_oid` whose key is
/// `operand` (a variable of range-table entry `query_varno`, or an
/// expression), in OID order.
pub(crate) unsafe fn matching_stannum_indexes(
    heap_oid: pg_sys::Oid,
    query_varno: i32,
    operand: *mut pg_sys::Node,
) -> Vec<pg_sys::Oid> {
    let stannum_name = CString::new("stannum").expect("static access method name is valid");
    let stannum_am = unsafe { pg_sys::get_index_am_oid(stannum_name.as_ptr(), false) };
    let normalized = unsafe { pg_sys::copyObjectImpl(operand.cast()).cast::<pg_sys::Node>() };
    unsafe { pg_sys::ChangeVarNodes(normalized, query_varno, 1, 0) };
    let normalized = unsafe { pg_sys::strip_implicit_coercions(normalized) };
    let heap = unsafe { pg_sys::table_open(heap_oid, pg_sys::AccessShareLock as _) };
    let indexes = unsafe { PgList::<pg_sys::Oid>::from_pg(pg_sys::RelationGetIndexList(heap)) };
    let mut matched = Vec::new();
    for index_oid in indexes.iter_oid() {
        let index = unsafe { pg_sys::index_open(index_oid, pg_sys::AccessShareLock as _) };
        let metadata = unsafe { &*(*index).rd_index };
        let is_stannum = unsafe { (*(*index).rd_rel).relam } == stannum_am;
        let suitable =
            is_stannum && metadata.indisvalid && metadata.indisready && metadata.indnkeyatts == 1;
        let matches = if suitable {
            let key = unsafe { *metadata.indkey.values.as_ptr() };
            if key > 0 {
                !normalized.is_null()
                    && unsafe { (*normalized).type_ } == pg_sys::NodeTag::T_Var
                    && unsafe {
                        let var = &*normalized.cast::<pg_sys::Var>();
                        var.varno == 1 && var.varlevelsup == 0 && var.varattno == key
                    }
            } else {
                let expressions = unsafe { pg_sys::RelationGetIndexExpressions(index) };
                if unsafe { pg_sys::list_length(expressions) } != 1 {
                    false
                } else {
                    let indexed =
                        unsafe { pg_sys::list_nth(expressions, 0).cast::<pg_sys::Node>() };
                    let indexed = unsafe { pg_sys::strip_implicit_coercions(indexed) };
                    unsafe { pg_sys::equal(normalized.cast(), indexed.cast()) }
                }
            }
        } else {
            false
        };
        unsafe { pg_sys::index_close(index, pg_sys::AccessShareLock as _) };
        if matches {
            matched.push(index_oid);
        }
    }
    unsafe { pg_sys::table_close(heap, pg_sys::AccessShareLock as _) };
    matched
}

struct ScoreCalls {
    ctid: *const pg_sys::Var,
    document: *mut pg_sys::Node,
    support: pg_sys::Oid,
    bound: pg_sys::Oid,
    dense: bool,
    full: bool,
}

#[pg_guard]
unsafe extern "C-unwind" fn find_score_calls(
    node: *mut pg_sys::Node,
    context: *mut c_void,
) -> bool {
    unsafe {
        if node.is_null() || (*node).type_ == pg_sys::NodeTag::T_Query {
            return false;
        }
        let binding = &mut *context.cast::<ScoreCalls>();
        if (*node).type_ == pg_sys::NodeTag::T_FuncExpr {
            let function = &*node.cast::<pg_sys::FuncExpr>();
            // Earlier query clauses may already contain the rewritten scorer.
            if function.funcid == binding.bound {
                let mode = pg_sys::list_nth(function.args, 4).cast::<pg_sys::Const>();
                if (*mode).xpr.type_ == pg_sys::NodeTag::T_Const
                    && pg_sys::equal(pg_sys::list_nth(function.args, 0), binding.document.cast())
                {
                    match (*mode).constvalue.value() {
                        0 => binding.dense = true,
                        1 => binding.full = true,
                        _ => {}
                    }
                }
            } else if pg_sys::get_func_support(function.funcid) == binding.support
                && matches!(
                    CStr::from_ptr(pg_sys::get_func_name(function.funcid)).to_bytes(),
                    b"score" | b"full_score"
                )
            {
                for position in 0..pg_sys::list_length(function.args) {
                    let mut argument =
                        pg_sys::list_nth(function.args, position).cast::<pg_sys::Node>();
                    if (*argument).type_ == pg_sys::NodeTag::T_NamedArgExpr {
                        let named = &*argument.cast::<pg_sys::NamedArgExpr>();
                        if named.argnumber != 0 {
                            continue;
                        }
                        argument = named.arg.cast();
                    } else if position != 0 {
                        continue;
                    }
                    if pg_sys::equal(argument.cast(), binding.ctid.cast()) {
                        if CStr::from_ptr(pg_sys::get_func_name(function.funcid)).to_bytes()
                            == b"full_score"
                        {
                            binding.full = true;
                        } else {
                            binding.dense = true;
                        }
                    }
                }
            }
        }
        pg_sys::expression_tree_walker(node, Some(find_score_calls), context)
    }
}

#[pg_extern(immutable, parallel_unsafe)]
fn score_support(request: Internal) -> Internal {
    let unhandled = || Internal::from(Some(pg_sys::Datum::from(0_usize)));
    let Some(datum) = request.into_datum() else {
        return unhandled();
    };
    unsafe {
        let node = datum.cast_mut_ptr::<pg_sys::Node>();
        if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_SupportRequestSimplify {
            return unhandled();
        }
        let request = &*node.cast::<pg_sys::SupportRequestSimplify>();
        if request.root.is_null() || request.fcall.is_null() {
            return unhandled();
        }
        let ctid_node = pg_sys::list_nth((*request.fcall).args, 0).cast::<pg_sys::Node>();
        if ctid_node.is_null() || (*ctid_node).type_ != pg_sys::NodeTag::T_Var {
            return unhandled();
        }
        let ctid = &*ctid_node.cast::<pg_sys::Var>();
        if ctid.varattno != pg_sys::SelfItemPointerAttributeNumber as i16 || ctid.varlevelsup != 0 {
            return unhandled();
        }
        let parse = (*request.root).parse;
        let mut binding = QualBinding {
            matches: Vec::new(),
        };
        find_where_quals((*parse).jointree.cast(), &mut binding);
        let rte = pg_sys::list_nth((*parse).rtable, (ctid.varno - 1) as i32)
            .cast::<pg_sys::RangeTblEntry>();
        if rte.is_null() || (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION {
            return unhandled();
        }
        // Score with the index the ==> clause is bound to, so scoring
        // statistics and matching use the same analyzer.
        let Some((document, first_query, index_oid)) =
            binding
                .matches
                .iter()
                .find_map(|&(document, query, bound)| {
                    let candidates = matching_stannum_indexes((*rte).relid, ctid.varno, document);
                    pick_index(&candidates, bound).map(|index_oid| (document, query, index_oid))
                })
        else {
            return unhandled();
        };
        let original_nargs = pg_sys::list_length((*request.fcall).args);
        let function_name = pg_sys::get_func_name((*request.fcall).funcid);
        let fname = CStr::from_ptr(function_name).to_string_lossy();
        let segmented = crate::storage::is_segmented(index_oid);
        let mode = if fname.as_ref() == "full_score" {
            1
        } else if fname.as_ref() == "max_score" {
            // max_score adapts to a sibling stannum.score() call; alone it uses
            // the full policy, as TIN does.
            // Follow upstream's query-level, tuple-specific binding, including
            // calls already rewritten to the indexed or fallback scorer.
            let mut calls = ScoreCalls {
                ctid,
                document: if segmented { ctid_node } else { document },
                support: pg_sys::get_func_support((*request.fcall).funcid),
                bound: lookup_score_bound(segmented),
                dense: false,
                full: false,
            };
            pg_sys::query_tree_walker(
                parse,
                Some(find_score_calls),
                (&mut calls as *mut ScoreCalls).cast(),
                pg_sys::QTW_IGNORE_RC_SUBQUERIES as i32,
            );
            if calls.dense && !calls.full { 2 } else { 3 }
        } else {
            0
        };
        let mut args = PgList::<pg_sys::Node>::new();
        if segmented {
            args.push(pg_sys::copyObjectImpl(ctid_node.cast()).cast());
        } else {
            args.push(pg_sys::copyObjectImpl(document.cast()).cast());
        }
        let same_expression = binding
            .matches
            .iter()
            .filter(|(candidate, _, _)| pg_sys::equal((*candidate).cast(), document.cast()))
            .map(|&(candidate, query, _)| (candidate, query))
            .collect::<Vec<_>>();
        let combined_query = combine_constant_queries(&same_expression)
            .unwrap_or_else(|| pg_sys::copyObjectImpl(first_query.cast()).cast());
        // This query came from the parse tree's quals, not the already
        // simplified arguments of the supported function. In a custom
        // prepared plan it can still contain a bound Param. Simplify the
        // copy so the score and search clause expose the same constant to
        // ranked-path recognition. Generic plans retain their parameters.
        args.push(pg_sys::eval_const_expressions(request.root, combined_query));
        args.push(make_int4_const((*rte).relid.to_u32() as i32).cast());
        args.push(make_int4_const(index_oid.to_u32() as i32).cast());
        args.push(make_int4_const(mode).cast());
        let null_float = || make_null_const(pg_sys::FLOAT4OID);
        let null_array = || make_null_const(pg_sys::TEXTARRAYOID);
        if mode == 0 {
            for position in 1..=5 {
                args.push(
                    pg_sys::copyObjectImpl(
                        pg_sys::list_nth((*request.fcall).args, position).cast(),
                    )
                    .cast(),
                );
            }
        } else {
            args.push(null_float().cast());
            if mode == 1 && original_nargs == 3 {
                args.push(
                    pg_sys::copyObjectImpl(pg_sys::list_nth((*request.fcall).args, 1).cast())
                        .cast(),
                );
                args.push(
                    pg_sys::copyObjectImpl(pg_sys::list_nth((*request.fcall).args, 2).cast())
                        .cast(),
                );
            } else {
                args.push(null_float().cast());
                args.push(null_float().cast());
            }
            args.push(null_array().cast());
            args.push(null_array().cast());
        }
        let oid = lookup_score_bound(segmented);
        let replacement = pg_sys::makeFuncExpr(
            oid,
            pg_sys::FLOAT4OID,
            args.into_pg(),
            pg_sys::InvalidOid,
            pg_sys::InvalidOid,
            pg_sys::CoercionForm::COERCE_EXPLICIT_CALL,
        );
        Internal::from(Some(pg_sys::Datum::from(replacement as usize)))
    }
}

unsafe fn combine_constant_queries(
    matches: &[(*mut pg_sys::Node, *mut pg_sys::Node)],
) -> Option<*mut pg_sys::Node> {
    if matches.len() < 2 {
        return None;
    }
    let mut queries = Vec::with_capacity(matches.len());
    for &(_, node) in matches {
        if node.is_null() || unsafe { (*node).type_ } != pg_sys::NodeTag::T_Const {
            return None;
        }
        let value = unsafe { &*node.cast::<pg_sys::Const>() };
        if value.constisnull || value.consttype != pg_sys::TEXTOID {
            return None;
        }
        queries.push(unsafe { String::from_datum(value.constvalue, false)? });
    }
    let combined = queries
        .into_iter()
        .map(|query| format!("({query})"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let datum = combined.into_datum()?;
    Some(unsafe {
        pg_sys::makeConst(
            pg_sys::TEXTOID,
            -1,
            pg_sys::DEFAULT_COLLATION_OID,
            -1,
            datum,
            false,
            false,
        )
        .cast()
    })
}

unsafe fn make_int4_const(value: i32) -> *mut pg_sys::Const {
    unsafe {
        pg_sys::makeConst(
            pg_sys::INT4OID,
            -1,
            pg_sys::InvalidOid,
            4,
            pg_sys::Datum::from(value as usize),
            false,
            true,
        )
    }
}

unsafe fn make_null_const(type_oid: pg_sys::Oid) -> *mut pg_sys::Const {
    unsafe {
        pg_sys::makeConst(
            type_oid,
            -1,
            pg_sys::InvalidOid,
            -1,
            pg_sys::Datum::null(),
            true,
            false,
        )
    }
}

unsafe fn lookup_score_bound(segmented: bool) -> pg_sys::Oid {
    let name = CString::new(if segmented {
        "stannum.score_bound_indexed"
    } else {
        "stannum.score_bound"
    })
    .unwrap();
    let names = unsafe { pg_sys::stringToQualifiedNameList(name.as_ptr(), std::ptr::null_mut()) };
    let types = [
        if segmented {
            pg_sys::TIDOID
        } else {
            pg_sys::TEXTOID
        },
        pg_sys::TEXTOID,
        pg_sys::INT4OID,
        pg_sys::INT4OID,
        pg_sys::INT4OID,
        pg_sys::FLOAT4OID,
        pg_sys::FLOAT4OID,
        pg_sys::FLOAT4OID,
        pg_sys::TEXTARRAYOID,
        pg_sys::TEXTARRAYOID,
    ];
    unsafe { pg_sys::LookupFuncName(names, types.len() as i32, types.as_ptr(), false) }
}

pgrx::extension_sql!(
    r#"
ALTER FUNCTION @extschema@.full_score(pg_catalog.tid) SUPPORT @extschema@.score_support;
ALTER FUNCTION @extschema@.full_score(pg_catalog.tid, pg_catalog.float4, pg_catalog.float4) SUPPORT @extschema@.score_support;
ALTER FUNCTION @extschema@.score(pg_catalog.tid, pg_catalog.float4, pg_catalog.float4, pg_catalog.float4, pg_catalog.text[], pg_catalog.text[]) SUPPORT @extschema@.score_support;
ALTER FUNCTION @extschema@.max_score(pg_catalog.tid) SUPPORT @extschema@.score_support;
REVOKE ALL ON FUNCTION @extschema@.score_bound(pg_catalog.text, pg_catalog.text, pg_catalog.int4, pg_catalog.int4, pg_catalog.int4, pg_catalog.float4, pg_catalog.float4, pg_catalog.float4, pg_catalog.text[], pg_catalog.text[]) FROM PUBLIC;
REVOKE ALL ON FUNCTION @extschema@.score_bound_indexed(pg_catalog.tid, pg_catalog.text, pg_catalog.int4, pg_catalog.int4, pg_catalog.int4, pg_catalog.float4, pg_catalog.float4, pg_catalog.float4, pg_catalog.text[], pg_catalog.text[]) FROM PUBLIC;
"#,
    name = "score_support_bindings",
    requires = [
        full_score,
        full_score_with_bm25,
        score,
        max_score,
        score_bound,
        score_bound_indexed,
        score_support
    ]
);
