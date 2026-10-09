// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

use crate::bm25::{
    Bm25Overrides, DenseRatio, ScoreStopWords, TermScorer, TermSetEdit, compile_scoring_terms,
    sum_scores_in_order,
};
use pgrx::iter::TableIterator;
use pgrx::{
    Internal, IntoDatum, PgList, PgRelation, Spi, default, name, pg_extern, pg_guard, pg_sys,
};
use rustc_hash::FxHashMap;
use segment::Tid;
use segment::docs::DocTable;
use segment::index::Index;
use segment::set::Cursor as _;
use segment::tf_bucket::TfBucket;
use segment::tid::MAX_OFFSET;
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::ffi::{CStr, CString, c_void};
use tinql::runtime::plan::{Limits, plan};
use tinql::runtime::{evaluate, parse_tinql_to_scoring_query, tokenize_doc};
use tokenizer::Tokenizer;

use crate::storage::View;
use engine::terms::{
    Collected, Expansion, ScoringError, ScoringPolicy, TooManyTerms, collect_score_terms,
    corpus_universe, inputs_of,
};
pub(crate) use engine::walk::walks_pruned;
pub(crate) use engine::walk::{
    PRUNE_MAX_K, TopK, chunk_loads, position_checks, position_reads, rank, walk_blocks,
    warmup_chunks, warmup_estimate, warmup_threshold,
};
use engine::walk::{Ranked, Scorer, Source, TopRows, WalkConfig};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CacheKey {
    /// The executor run the scorer was built for; see [`note_executor_start`].
    statement: u64,
    heap_oid: u32,
    index_oid: u32,
    /// The search texts, one per `==>` clause; see
    /// [`crate::score_binding::parse_searches`].
    queries: Vec<String>,
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
    dead: Vec<crate::storage::DeadSet>,
    /// The scoring terms and the query, which the ranked walk reads.
    scoring: Scorer,
    /// The scoring terms' names, in the scorer's order.
    names: Vec<String>,
    /// Computed on first request: the maximum over matching documents.
    max: Option<f32>,
    /// Scores the search scan already computed for the rows it emits, so the
    /// projected score function does not move the cursors backwards.
    known: FxHashMap<Tid, f32>,
}

/// One source's readers for scoring rows by location: the document table
/// finds a row's ordinal, each term's stream its rank, and the payload its
/// frequency bucket. Every lookup is a few random reads, in any order.
///
/// Everything is opened on first use: a ranked scan scores its rows in the
/// walk and projects them from what it ranked, so most statements never
/// look a row up here, and opening each term's stream up front parsed every
/// chunk bound of every term in every source, and loaded each stream's
/// first chunk, per statement.
struct SourceReader {
    segment: &'static dyn Index,
    docs: Option<DocTable<'static>>,
    lengths: Option<segment::segment::Lengths<'static>>,
    /// One per scoring term, once opened: the term's streams in this
    /// source, if present.
    terms: Vec<Option<Option<TermReader>>>,
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
        Self {
            segment,
            docs: None,
            lengths: None,
            terms: (0..terms.len()).map(|_| None).collect(),
        }
    }

    /// The source's ordinal for `tid`, if it lists the location.
    fn ordinal_of(&mut self, tid: Tid, label: &str) -> Option<u32> {
        let segment = self.segment;
        let docs = match &mut self.docs {
            Some(docs) => docs,
            slot => slot.insert(segment_error_in(segment.doc_table(), label)),
        };
        segment_error_in(docs.ordinal_of(tid), label)
    }

    /// The length of the document at `ordinal`.
    fn length(&mut self, ordinal: u32, label: &str) -> u32 {
        let segment = self.segment;
        let lengths = self.lengths.get_or_insert_with(|| segment.lengths());
        segment_error_in(lengths.get(ordinal), label)
    }

    /// The bucket of `ordinal` in scoring term `n`, named `name`, if the
    /// term lists it here.
    fn bucket(&mut self, n: usize, name: &str, ordinal: u32, label: &str) -> Option<u8> {
        let segment = self.segment;
        let reader = self.terms[n].get_or_insert_with(|| {
            segment_error_in(segment.term(name), label).map(|term| TermReader {
                cursor: segment_error_in(term.ordinals().and_then(|stream| stream.cursor()), label),
                exhausted_at: None,
            })
        });
        reader.as_mut()?.bucket(ordinal, label)
    }
}

thread_local! {
    /// The statement's scorers, one per scored query and index: a score over
    /// `==>` clauses on several indexed columns sums one per column, called
    /// in turn for each row (see [`score_support`]). Oldest first, at most
    /// [`STATEMENT_SCORERS`].
    static SCORE_CACHE: RefCell<Vec<ScoreCorpus>> = const { RefCell::new(Vec::new()) };
    static INDEX_SCORE_CACHE: RefCell<Vec<IndexScorer>> = const { RefCell::new(Vec::new()) };
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

/// Scorers a statement keeps at once; see [`SCORE_CACHE`]. A statement
/// needing more rebuilds the oldest, as one with a new query per row (a
/// LATERAL search) rebuilds each time anyway.
const STATEMENT_SCORERS: usize = 8;

/// The cached entry `matches` accepts, else a new one from `build`, which
/// drops earlier statements' entries and, beyond [`STATEMENT_SCORERS`], the
/// oldest.
fn cached_scorer<T>(
    cache: &mut Vec<T>,
    matches: impl Fn(&T) -> bool,
    statement_of: impl Fn(&T) -> u64,
    statement: u64,
    build: impl FnOnce() -> T,
) -> &mut T {
    let at = match cache.iter().position(matches) {
        Some(at) => at,
        None => {
            let built = build();
            cache.retain(|entry| statement_of(entry) == statement);
            if cache.len() >= STATEMENT_SCORERS {
                cache.remove(0);
            }
            cache.push(built);
            cache.len() - 1
        }
    };
    &mut cache[at]
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
    score_heap(
        document,
        &[query],
        (heap_oid, index_oid, mode),
        dense_ratio,
        k1,
        b,
        term_add,
        term_replace,
    )
}

/// [`score_bound`] for several `==>` clauses on one document expression,
/// each search text parsed on its own.
#[pg_extern(volatile, parallel_unsafe)]
#[expect(
    clippy::too_many_arguments,
    reason = "SQL signature used by the scoring support function"
)]
fn score_bound_searches(
    document: &str,
    queries: Vec<Option<String>>,
    heap_oid: i32,
    index_oid: i32,
    mode: i32,
    dense_ratio: Option<f32>,
    k1: Option<f32>,
    b: Option<f32>,
    term_add: Option<Vec<String>>,
    term_replace: Option<Vec<String>>,
) -> f32 {
    let queries = crate::score_binding::search_texts(&queries);
    if queries.is_empty() {
        return 0.0;
    }
    score_heap(
        document,
        &queries,
        (heap_oid, index_oid, mode),
        dense_ratio,
        k1,
        b,
        term_add,
        term_replace,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "the bound scoring functions' arguments"
)]
fn score_heap(
    document: &str,
    queries: &[&str],
    (heap_oid, index_oid, mode): (i32, i32, i32),
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
        queries: queries.iter().map(|&query| query.to_owned()).collect(),
        full: mode == 1 || mode == 3,
        dense: dense_ratio.unwrap_or(DenseRatio::DEFAULT).to_bits(),
        k1: bits(k1),
        b: bits(b),
        add: term_add.clone(),
        replace: term_replace.clone(),
    };
    SCORE_CACHE.with_borrow_mut(|cache| {
        let statement = key.statement;
        let corpus = cached_scorer(
            cache,
            |corpus| corpus.key == key,
            |corpus| corpus.key.statement,
            statement,
            || build_corpus(key.clone(), k1, b, term_add, term_replace),
        );
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
    score_indexed(
        ctid,
        &[query],
        (heap_oid, index_oid, mode),
        dense_ratio,
        k1,
        b,
        term_add,
        term_replace,
    )
}

/// [`score_bound_indexed`] for several `==>` clauses on one document
/// expression, each search text parsed on its own.
#[pg_extern(volatile, parallel_unsafe)]
#[expect(
    clippy::too_many_arguments,
    reason = "SQL signature used by the scoring support function"
)]
fn score_bound_indexed_searches(
    ctid: pg_sys::ItemPointerData,
    queries: Vec<Option<String>>,
    heap_oid: i32,
    index_oid: i32,
    mode: i32,
    dense_ratio: Option<f32>,
    k1: Option<f32>,
    b: Option<f32>,
    term_add: Option<Vec<String>>,
    term_replace: Option<Vec<String>>,
) -> f32 {
    let queries = crate::score_binding::search_texts(&queries);
    if queries.is_empty() {
        return 0.0;
    }
    score_indexed(
        ctid,
        &queries,
        (heap_oid, index_oid, mode),
        dense_ratio,
        k1,
        b,
        term_add,
        term_replace,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "the bound scoring functions' arguments"
)]
fn score_indexed(
    ctid: pg_sys::ItemPointerData,
    queries: &[&str],
    (heap_oid, index_oid, mode): (i32, i32, i32),
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
            && key.queries.len() == queries.len()
            && key
                .queries
                .iter()
                .zip(queries)
                .all(|(key, query)| key == query)
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
    INDEX_SCORE_CACHE.with_borrow_mut(|cache| {
        let build = || {
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
                queries: queries.iter().map(|&query| query.to_owned()).collect(),
                full: mode == 1 || mode == 3,
                dense,
                k1: bits(k1),
                b: bits(b),
                add: term_add.clone(),
                replace: term_replace.clone(),
            };
            build_index_scorer(key, k1, b, term_add.clone(), term_replace.clone())
        };
        let scorer = cached_scorer(
            cache,
            |scorer| matches(&scorer.key),
            |scorer| scorer.key.statement,
            statement,
            build,
        );
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
    /// A segment in TIN's shape finds it by its slot (see
    /// [`engine::tinshape::score_at`]); the write buffer and sealed segments
    /// by ordinal.
    fn score_listed(&mut self, tid: Tid) -> Option<f32> {
        for i in 0..self.view.sources.len() {
            let label = self.view.labels[i].as_str();
            let terms = &self.scoring.terms;
            let names = &self.names;
            if let Some(found) =
                crate::storage::with_native_rows(&self.view, i, names, |segment, sets| {
                    engine::tinshape::score_at_in(segment, terms, sets, tid)
                })
            {
                match segment_error_in(found, label) {
                    Some(score) => return Some(score),
                    None => continue,
                }
            }
            let reader = &mut self.sources[i];
            let Some(ordinal) = reader.ordinal_of(tid, label) else {
                continue;
            };
            if self.dead[i].contains(ordinal) {
                continue;
            }
            // Each term's bucket for the document, if the term lists it.
            let buckets: Vec<Option<u8>> = self
                .scoring
                .terms
                .iter()
                .enumerate()
                .map(|(n, (name, _))| reader.bucket(n, name, ordinal, label))
                .collect();
            if buckets.iter().all(Option::is_none) {
                // The document is in this source but holds no scoring term.
                continue;
            }
            let length = reader.length(ordinal, label);
            // Left-to-right f32 fold in lexical term order, as production does.
            let mut total = 0.0_f32;
            for ((_, scorer), bucket) in self.scoring.terms.iter().zip(&buckets) {
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

/// `stannum.debug_seed_score`: a measurement aid. When not negative, a pruned
/// walk prunes against this score from its first candidate, as if the top k
/// were already known: the pages and candidates it then costs are what a
/// walk seeded from per-term champion lists would cost.
/// It changes which rows a query returns, so only a superuser may set it.
pub(crate) static DEBUG_SEED_SCORE: pgrx::GucSetting<f64> = pgrx::GucSetting::<f64>::new(-1.0);

/// Default of `stannum.warmup_chunks`; see [`WalkConfig::warmup_chunks`].
pub(crate) const DEFAULT_WARMUP_CHUNKS: i32 = 256;

/// `stannum.warmup_chunks`: how many chunks, across every source, a pruned
/// conjunction evaluates first, those with the highest bounds by the chunk
/// directory, so its threshold starts near its final value. Zero disables.
/// Default of `stannum.max_expansion_terms`.
pub(crate) const DEFAULT_MAX_EXPANSION_TERMS: i32 = 65_536;

/// `stannum.max_expansion_terms`: the dictionary terms the wildcards,
/// regexes, ranges and fuzzy terms of one ranked query may expand to in
/// all. Each is scored with a scorer of its own, as TIN scores them, so a
/// query past the limit fails (SQLSTATE 54000) instead of scoring a subset,
/// as Lucene's `maxClauseCount` and Elasticsearch's
/// `indices.query.bool.max_clause_count` do. Matching and counting expand
/// without it.
pub(crate) static MAX_EXPANSION_TERMS: pgrx::GucSetting<i32> =
    pgrx::GucSetting::<i32>::new(DEFAULT_MAX_EXPANSION_TERMS);

pub(crate) static WARMUP_CHUNKS: pgrx::GucSetting<i32> =
    pgrx::GucSetting::<i32>::new(DEFAULT_WARMUP_CHUNKS);

/// Default of `stannum.warmup_min_matches`.
pub(crate) const DEFAULT_WARMUP_MIN_MATCHES: f64 = 4.0;

/// `stannum.warmup_min_matches`: a conjunction is warmed up only when its
/// matches, estimated as if its terms occurred independently, number at
/// least this many per row asked for. With fewer the top k fills late or
/// never, so the threshold the warm-up raises prunes little, and its pass
/// over the directory and its chunks out of order are all it adds. The
/// estimate falls short for words that keep company, so the bar is low.
pub(crate) static WARMUP_MIN_MATCHES: pgrx::GucSetting<f64> =
    pgrx::GucSetting::<f64>::new(DEFAULT_WARMUP_MIN_MATCHES);

thread_local! {
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
    /// Shared buffers accessed (hit or read) per named phase: phases nest
    /// (a run source's page reads within a native read), so they overlap.
    static PHASE_BLOCKS: RefCell<Vec<(&'static str, i64)>> = const { RefCell::new(Vec::new()) };
}

/// Runs `body`, charging the pages it reads from storage to `label`.
pub(crate) fn charging<T>(label: &'static str, body: impl FnOnce() -> T) -> T {
    let before = disk_pages();
    let accessed = blocks_used();
    let value = body();
    let add = |phases: &mut Vec<(&'static str, i64)>, pages: i64| {
        if pages != 0 {
            match phases.iter_mut().find(|(name, _)| *name == label) {
                Some(entry) => entry.1 += pages,
                None => phases.push((label, pages)),
            }
        }
    };
    PHASE_DISK.with_borrow_mut(|phases| add(phases, disk_pages() - before));
    PHASE_BLOCKS.with_borrow_mut(|phases| add(phases, blocks_used() - accessed));
    value
}

/// Shared buffers accessed per phase since the counters were reset.
pub(crate) fn phase_blocks() -> Vec<(&'static str, i64)> {
    PHASE_BLOCKS.with_borrow(Clone::clone)
}

/// Pages read from storage per phase since the counters were reset.
pub(crate) fn phase_disk() -> Vec<(&'static str, i64)> {
    PHASE_DISK.with_borrow(Clone::clone)
}

/// Index pages this backend has read or hit so far: the walk's
/// [`engine::set_blocks_probe`].
pub(crate) fn blocks_used() -> i64 {
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
    engine::walk::reset_counters();
    VISIBILITY_CHECKS.set(0);
    VM_HITS.set(0);
    PHASE_DISK.with_borrow_mut(Vec::clear);
    PHASE_BLOCKS.with_borrow_mut(Vec::clear);
    segment::tinshape::blob::reset_stats();
    crate::storage::reset_held_peak();
}

/// Heap visibility checks since the last reset.
pub(crate) fn visibility_checks() -> i64 {
    VISIBILITY_CHECKS.get()
}

/// Visibility checks the visibility map answered since the last reset.
pub(crate) fn vm_hits() -> i64 {
    VM_HITS.get()
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

impl IndexScorer {
    /// True when no term scores; see [`Scorer::scores_nothing`].
    pub(crate) fn scores_nothing(&self) -> bool {
        self.scoring.scores_nothing()
    }

    /// See [`Scorer::walks_unscored`].
    pub(crate) fn walks_unscored(&self) -> bool {
        self.scoring.walks_unscored()
    }

    /// The `k` best of every candidate `stream` yields, scored as they
    /// arrive: only the heap of `k` rows is held. Scoring every candidate
    /// first held every match, its score and a map of both for the
    /// projection, which for a phrase of common words at scale was hundreds
    /// of megabytes per backend. Bit-identical to that, including tie order.
    ///
    /// `None` when the stream is a superset that needs rechecking.
    ///
    /// With `ties`, the rows tied with the k-th score and the next row after
    /// them are kept too (see [`TopRows`]).
    pub(crate) fn top_k_streamed(
        &mut self,
        stream: &mut crate::stream::CandidateStream,
        k: usize,
        ties: bool,
    ) -> Option<TopK> {
        if stream.recheck {
            return None;
        }
        let mut top = TopRows::new(k, ties);
        let mut scored = 0usize;
        while let Some(tid) = stream.next() {
            pgrx::check_for_interrupts!();
            scored += 1;
            let entry = Ranked(self.score(tid), tid);
            if top.admits(&entry) {
                top.push(entry);
            }
        }
        // With ties and no row kept below them, every candidate was kept.
        let complete = k > 0 && ties && !top.bounded();
        let rows = top.into_rows();
        Some(TopK {
            complete: complete || rows.len() < k,
            rows,
            scored,
            zero_fill: false,
            ordinal: false,
            native: false,
            streamed: true,
        })
    }

    /// The `k` best candidates of the scan's query in output order, by the
    /// ranked walk over the view's sources ([`Scorer::top_k`]): candidates
    /// enter the top k only if visible under the active snapshot, trusting
    /// the visibility map, and the walk is repeated against the heap if the
    /// view is no longer current (a dead list was published or the write
    /// buffer rewritten meanwhile). With `ties`, the rows tied with the k-th
    /// score and the next row after them are kept too (see [`TopRows`]).
    /// With a `filter`, only rows that pass it are kept, so the rows are the
    /// best of those.
    pub(crate) fn top_k(&self, k: usize, ties: bool, filter: Option<RowFilter>) -> Option<TopK> {
        let natives = crate::storage::natives(&self.view);
        let sources: Vec<Source<'_>> = (0..self.view.sources.len())
            .map(|i| Source {
                index: &*self.view.sources[i].0,
                label: &self.view.labels[i],
                dead: &self.view.dead_sets[i],
                key: self.view.keys.get(i).copied(),
                native: natives
                    .get(i)
                    .and_then(Option::as_ref)
                    .map(|native| native as &dyn engine::walk::NativeSegment),
            })
            .collect();
        let config = WalkConfig {
            warmup_chunks: usize::try_from(WARMUP_CHUNKS.get()).unwrap_or(0),
            warmup_min_matches: WARMUP_MIN_MATCHES.get(),
            seed: seeded_threshold(),
        };
        let heap_oid = pg_sys::Oid::from(self.key.heap_oid);
        let index_oid = pg_sys::Oid::from(self.key.index_oid);
        self.scoring.top_k(
            &sources,
            k,
            ties,
            &config,
            |shortcut| {
                let mut visibility = unsafe { Visibility::open(heap_oid, shortcut) };
                visibility.filter = filter;
                visibility
            },
            |visibility| {
                !visibility.shortcuts
                    || unsafe { crate::storage::view_is_current(index_oid, &self.view) }
            },
        )
    }
}

/// The rest of a ranked scan's restrictions, which a walk can apply to a
/// candidate's visible tuple as it admits it: its top k is then the best k
/// rows that pass them. The qual must not be volatile, as the scan
/// evaluates it again on each row it returns.
#[derive(Clone, Copy)]
pub(crate) struct RowFilter {
    /// The scan's qual, compiled in its node.
    pub(crate) qual: *mut pg_sys::ExprState,
    /// The node's expression context; its per-tuple memory is reset after
    /// each evaluation, as the walk evaluates the qual once per admission.
    pub(crate) econtext: *mut pg_sys::ExprContext,
}

impl RowFilter {
    /// Whether the tuple in `slot` passes the qual.
    ///
    /// # Safety
    /// The scan node `qual` and `econtext` belong to is executing; `slot`
    /// holds a tuple of its relation.
    unsafe fn passes(&self, slot: *mut pg_sys::TupleTableSlot) -> bool {
        unsafe {
            (*self.econtext).ecxt_scantuple = slot;
            let passes = pg_sys::ExecQual(self.qual, self.econtext);
            pg_sys::MemoryContextReset((*self.econtext).ecxt_per_tuple_memory);
            passes
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
    /// Restrictions a visible tuple must pass too; the map cannot answer
    /// for them, so the tuple is always read.
    filter: Option<RowFilter>,
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
                filter: None,
            }
        }
    }

    /// Whether the snapshot sees a tuple at `tid` or on its HOT chain, and
    /// it passes the filter if there is one. On an all-visible page every
    /// tuple is visible to every snapshot, and the index lists no tuple
    /// VACUUM removed, so without a filter the map alone answers.
    fn visible(&mut self, tid: Tid) -> bool {
        VISIBILITY_CHECKS.set(VISIBILITY_CHECKS.get() + 1);
        if let Some(filter) = self.filter {
            // SAFETY: the walk runs inside the scan node the filter is of,
            // and the slot holds the visible version of `tid`.
            return charging("heap visibility", || self.visible_inner(tid))
                && unsafe { filter.passes(self.slot) };
        }
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

impl engine::walk::Visibility for Visibility {
    fn visible(&mut self, tid: Tid) -> bool {
        Visibility::visible(self, tid)
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
        queries: vec![query.to_owned()],
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
                .map(|(index, _)| unsafe { SourceReader::new(&**index, &scorer.scoring.terms) })
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
    let query = crate::score_binding::parse_searches(&key.queries, tokenizer.as_ref(), false);
    let scoring = crate::score_binding::parse_searches(&key.queries, tokenizer.as_ref(), true);
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
    let policy = ScoringPolicy {
        params,
        full: key.full,
        dense,
        edit: &edit,
        stop: stop.as_ref(),
        max_expansion_terms: max_expansion_terms(),
    };
    let scorers = engine::terms::term_scorers(&scoring, &segments, view.immutable_sources, &policy)
        .unwrap_or_else(|error| match error {
            ScoringError::TooManyTerms(limit) => too_many_terms(limit),
            ScoringError::Parameters(error) => pgrx::error!("stannum score parameters: {error}"),
        });
    let dead = view.dead_sets.clone();
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
        names: scorers.iter().map(|(name, _)| name.clone()).collect(),
        scoring: Scorer {
            terms: scorers,
            query,
        },
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
        let lowering = crate::native::Lowering::new(std::slice::from_ref(&self.scoring.query));
        for (i, ((segment, dead), label)) in
            self.view.sources.iter().zip(&self.view.labels).enumerate()
        {
            if let Some(lowered) = lowering.get(&self.view, i)
                && crate::native::visit_matches(&self.view, i, &lowered, &mut |tid| {
                    candidates.insert(tid);
                })
            {
                continue;
            }
            let planned = plan(&self.scoring.query, &**segment, &Limits::default())
                .unwrap_or_else(|error| pgrx::error!("Stannum query plan: {error}"));
            let mut cursor = planned.cursor;
            if let Some(dead) = dead {
                let dead = segment_error_in(
                    crate::storage::dead_cursor(&**segment, dead),
                    &format!("{label} dead list"),
                );
                cursor = Box::new(segment_error_in(
                    segment::set::Difference::new(cursor, dead),
                    label,
                ));
            }
            while let Some(tid) = cursor.current() {
                candidates.insert(tid);
                segment_error_in(cursor.advance(), label);
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
            .map(|(index, _)| unsafe { SourceReader::new(&**index, &self.scoring.terms) })
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
    let query = crate::score_binding::parse_searches(&key.queries, &tokenizer, false);
    let scoring = crate::score_binding::parse_searches(&key.queries, &tokenizer, true);
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
    collect_score_terms(&scoring, 1.0, false, &mut collected);
    let owned = resolved(collected, |expansion, limit| {
        expansion.expand_over(&universe, limit)
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

/// `stannum.max_expansion_terms`, as a count.
fn max_expansion_terms() -> usize {
    usize::try_from(MAX_EXPANSION_TERMS.get()).unwrap_or(0)
}

/// Fails the query whose expansions bring more than `limit` terms to score.
fn too_many_terms(limit: usize) -> ! {
    pgrx::ereport!(
        ERROR,
        pgrx::PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
        format!(
            "query expands to more than {limit} terms to score \
             (stannum.max_expansion_terms)"
        )
    );
}

/// `collected`'s terms with its expansions resolved through `expand`,
/// within `stannum.max_expansion_terms` (see [`Collected::resolve`]).
fn resolved<'a>(
    collected: Collected<'a>,
    expand: impl FnMut(&Expansion<'a>, usize) -> Option<Vec<String>>,
) -> Vec<(String, f32, bool)> {
    collected
        .resolve(max_expansion_terms(), expand)
        .unwrap_or_else(|TooManyTerms(limit)| too_many_terms(limit))
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
    let parsed = parse_tinql_to_scoring_query(query, &tokenizer).unwrap_or_else(|error| {
        crate::operator::raise_query_error(
            &error,
            format!("stannum.score_inspect() query error: {error}"),
        )
    });
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
        let owned = resolved(collected, |expansion, limit| {
            expansion.expand_in(&segments, limit)
        });
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
        let owned = resolved(collected, |expansion, limit| {
            expansion.expand_over(&universe, limit)
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
    // The operand is normalized to varno 1 below to compare it with stored
    // index expressions, so an operand on another relation (which may be
    // varno 1) must be rejected first.
    if unsafe { crate::operator::single_varno(operand) } != Some(query_varno) {
        return Vec::new();
    }
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
    bound: [pg_sys::Oid; 2],
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
            if binding.bound.contains(&function.funcid) {
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
        let rte = pg_sys::list_nth((*parse).rtable, (ctid.varno - 1) as i32)
            .cast::<pg_sys::RangeTblEntry>();
        if rte.is_null() || (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION {
            return unhandled();
        }
        // This relation's searches, outside NOT; see `score_binding`.
        let searches = crate::score_binding::collect_searches((*parse).jointree.cast())
            .into_iter()
            .filter(|search| crate::operator::single_varno(search.document) == Some(ctid.varno))
            .collect::<Vec<_>>();
        // Each searched document expression of this relation that an index
        // covers, in the order of its first clause, with the index that
        // clause is bound to, so scoring statistics and matching use the
        // same analyzer. As in TIN, a row's score sums one score per
        // expression (a row matching one column scores that column's), and
        // clauses on one expression score as one query (see below).
        // A partial index answers only where the restrictions imply its
        // predicate, as for `==>` itself; with no index to answer any
        // search, TIN refuses to score.
        let mut documents: Vec<(*mut pg_sys::Node, *mut pg_sys::Node, pg_sys::Oid)> = Vec::new();
        let mut unindexed = Vec::new();
        for search in &searches {
            let document = search.document;
            if documents
                .iter()
                .any(|&(seen, _, _)| pg_sys::equal(seen.cast(), document.cast()))
            {
                continue;
            }
            let candidates = matching_stannum_indexes((*rte).relid, ctid.varno, document)
                .into_iter()
                .filter(|&index_oid| {
                    crate::operator::predicate_holds(request.root, ctid.varno, index_oid)
                })
                .collect::<Vec<_>>();
            match pick_index(&candidates, search.index) {
                Some(index_oid) => documents.push((document, search.query, index_oid)),
                None => unindexed.push(document),
            }
        }
        let Some(&(document, _, index_oid)) = documents.first() else {
            if searches.is_empty() {
                return unhandled();
            }
            crate::score_binding::refuse_unindexed_scoring(rte, ctid.varno, &unindexed);
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
                bound: [
                    lookup_score_bound(segmented, false),
                    lookup_score_bound(segmented, true),
                ],
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
        // max_score reports the first expression's best score: the best sum
        // over several would need every match scored under each.
        if mode >= 2 {
            documents.truncate(1);
        }
        let mut replacement: *mut pg_sys::Node = std::ptr::null_mut();
        for &(document, first_query, index_oid) in &documents {
            let call = bound_score_call(
                request,
                ctid_node,
                &searches,
                (*rte).relid,
                (document, first_query, index_oid),
                mode,
                original_nargs,
            );
            // Left to right in clause order, in float4 as TIN adds them.
            replacement = if replacement.is_null() {
                call
            } else {
                let mut sum = PgList::<pg_sys::Node>::new();
                sum.push(replacement);
                sum.push(call);
                pg_sys::makeFuncExpr(
                    pg_sys::Oid::from(pg_sys::F_FLOAT4PL),
                    pg_sys::FLOAT4OID,
                    sum.into_pg(),
                    pg_sys::InvalidOid,
                    pg_sys::InvalidOid,
                    pg_sys::CoercionForm::COERCE_EXPLICIT_CALL,
                )
                .cast()
            };
        }
        // A row that no search admits has no score; where a search is a
        // top-level conjunct (every ranked scan's shape), every row has one.
        let required = searches.iter().any(|search| {
            search.required
                && documents
                    .iter()
                    .any(|&(seen, _, _)| pg_sys::equal(seen.cast(), search.document.cast()))
        });
        if mode < 2 && !required {
            let scored = searches
                .iter()
                .filter_map(|search| {
                    documents
                        .iter()
                        .find(|&&(seen, _, _)| pg_sys::equal(seen.cast(), search.document.cast()))
                        .map(|&(_, _, index_oid)| (search.document, search.query, index_oid))
                })
                .collect::<Vec<_>>();
            if let Some(guarded) =
                crate::score_binding::unless_unsearched(request.root, ctid, &scored, replacement)
            {
                replacement = guarded;
            }
        }
        Internal::from(Some(pg_sys::Datum::from(replacement as usize)))
    }
}

/// The bound scorer call of one searched document expression: `document`'s
/// clauses (the first is `first_query`) scored with `index_oid` under `mode`.
unsafe fn bound_score_call(
    request: &pg_sys::SupportRequestSimplify,
    ctid_node: *mut pg_sys::Node,
    searches: &[crate::score_binding::Search],
    heap_oid: pg_sys::Oid,
    (document, first_query, index_oid): (*mut pg_sys::Node, *mut pg_sys::Node, pg_sys::Oid),
    mode: i32,
    original_nargs: i32,
) -> *mut pg_sys::Node {
    unsafe {
        let segmented = crate::storage::is_segmented(index_oid);
        let mut args = PgList::<pg_sys::Node>::new();
        if segmented {
            args.push(pg_sys::copyObjectImpl(ctid_node.cast()).cast());
        } else {
            args.push(pg_sys::copyObjectImpl(document.cast()).cast());
        }
        let queries = searches
            .iter()
            .filter(|search| pg_sys::equal(search.document.cast(), document.cast()))
            .map(|search| search.query)
            .collect::<Vec<_>>();
        // Several clauses on the expression score as one ORed query: their
        // texts go to the scorer as an array, parsed one by one at run
        // time, so parameters contribute too.
        let several = queries.len() > 1;
        if several {
            args.push(crate::score_binding::query_array(request.root, &queries));
        } else {
            // This query came from the parse tree's quals, not the already
            // simplified arguments of the supported function. In a custom
            // prepared plan it can still contain a bound Param. Simplify the
            // copy so the score and search clause expose the same constant to
            // ranked-path recognition. Generic plans retain their parameters.
            let query = pg_sys::copyObjectImpl(first_query.cast()).cast();
            args.push(pg_sys::eval_const_expressions(request.root, query));
        }
        args.push(make_int4_const(heap_oid.to_u32() as i32).cast());
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
        pg_sys::makeFuncExpr(
            lookup_score_bound(segmented, several),
            pg_sys::FLOAT4OID,
            args.into_pg(),
            pg_sys::InvalidOid,
            pg_sys::InvalidOid,
            pg_sys::CoercionForm::COERCE_EXPLICIT_CALL,
        )
        .cast()
    }
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

/// The bound scorer: by heap row or by location (`segmented`), for one search
/// text or an array of several.
unsafe fn lookup_score_bound(segmented: bool, several: bool) -> pg_sys::Oid {
    let name = CString::new(match (segmented, several) {
        (true, false) => "stannum.score_bound_indexed",
        (true, true) => "stannum.score_bound_indexed_searches",
        (false, false) => "stannum.score_bound",
        (false, true) => "stannum.score_bound_searches",
    })
    .unwrap();
    let names = unsafe { pg_sys::stringToQualifiedNameList(name.as_ptr(), std::ptr::null_mut()) };
    let types = [
        if segmented {
            pg_sys::TIDOID
        } else {
            pg_sys::TEXTOID
        },
        if several {
            pg_sys::TEXTARRAYOID
        } else {
            pg_sys::TEXTOID
        },
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
REVOKE ALL ON FUNCTION @extschema@.score_bound_searches(pg_catalog.text, pg_catalog.text[], pg_catalog.int4, pg_catalog.int4, pg_catalog.int4, pg_catalog.float4, pg_catalog.float4, pg_catalog.float4, pg_catalog.text[], pg_catalog.text[]) FROM PUBLIC;
REVOKE ALL ON FUNCTION @extschema@.score_bound_indexed_searches(pg_catalog.tid, pg_catalog.text[], pg_catalog.int4, pg_catalog.int4, pg_catalog.int4, pg_catalog.float4, pg_catalog.float4, pg_catalog.float4, pg_catalog.text[], pg_catalog.text[]) FROM PUBLIC;
"#,
    name = "score_support_bindings",
    requires = [
        full_score,
        full_score_with_bm25,
        score,
        max_score,
        score_bound,
        score_bound_indexed,
        score_bound_searches,
        score_bound_indexed_searches,
        score_support
    ]
);
