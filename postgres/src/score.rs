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
    CompiledRegex, FuzzyMatcher, Query, RangeBound, SpanPositionFilter, SpanTermSlot, evaluate,
    parse_tinql_to_query, range_matches, tokenize_doc,
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
            let Some(ordinal) = reader.ordinal_of(tid, label) else {
                continue;
            };
            // Each term's bucket for the document, if the term lists it.
            let buckets: Vec<Option<u8>> = self
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

/// Default of `stannum.warmup_chunks`; see [`IndexScorer::warm_up`].
pub(crate) const DEFAULT_WARMUP_CHUNKS: i32 = 256;

/// `stannum.warmup_chunks`: how many chunks, across every source, a pruned
/// conjunction evaluates first, those with the highest bounds by the chunk
/// directory, so its threshold starts near its final value. Zero disables.
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
    /// Chunks walks evaluated in their warm-ups, before walking in order.
    static WARMUP_EVALUATED: Cell<i64> = const { Cell::new(0) };
    /// The k-th best score once the last warm-up ended, if the heap was full.
    static WARMUP_THRESHOLD: Cell<Option<f32>> = const { Cell::new(None) };
    /// The matches the last conjunction's warm-up was judged by.
    static WARMUP_ESTIMATE: Cell<Option<f64>> = const { Cell::new(None) };
}

/// Chunks evaluated in warm-ups since the last reset.
pub(crate) fn warmup_chunks() -> i64 {
    WARMUP_EVALUATED.get()
}

/// The k-th best score after the last warm-up, if it filled the top k.
pub(crate) fn warmup_threshold() -> Option<f32> {
    WARMUP_THRESHOLD.get()
}

/// The matches the last conjunction estimated for its warm-up (see
/// [`WARMUP_MIN_MATCHES`]), if one was considered.
pub(crate) fn warmup_estimate() -> Option<f64> {
    WARMUP_ESTIMATE.get()
}

thread_local! {
    /// Index pages read while building a walk's per-term state, and while
    /// walking. A pruned walk cannot skip what it reads before it starts, so
    /// the split says whether tighter bounds or cheaper setup is the work.
    static SETUP_BLOCKS: Cell<i64> = const { Cell::new(0) };
    static WALK_BLOCKS: Cell<i64> = const { Cell::new(0) };
    /// Chunks of a term's ordinal stream expanded into members.
    static CHUNK_LOADS: Cell<i64> = const { Cell::new(0) };
    /// Candidates of a phrase walk whose positions were read and checked.
    static POSITION_CHECKS: Cell<i64> = const { Cell::new(0) };
    /// Position lists read for those candidates, at most one per slot each.
    static POSITION_READS: Cell<i64> = const { Cell::new(0) };
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
    POSITION_CHECKS.set(0);
    POSITION_READS.set(0);
    VISIBILITY_CHECKS.set(0);
    VM_HITS.set(0);
    WARMUP_EVALUATED.set(0);
    WARMUP_THRESHOLD.set(None);
    WARMUP_ESTIMATE.set(None);
    PHASE_DISK.with_borrow_mut(Vec::clear);
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

/// Chunks loaded since the last reset.
pub(crate) fn chunk_loads() -> i64 {
    CHUNK_LOADS.get()
}

#[cfg(any(test, feature = "pg_test"))]
thread_local! {
    /// For tests: the chunk load of a scan at which to cancel the query,
    /// zero for none, and the pages held pinned when it was cancelled.
    static CANCEL_AT_LOAD: Cell<(i64, i64)> = const { Cell::new((0, 0)) };
}

/// Cancels the running query at its `load`-th chunk load, as a user's
/// cancel request arriving mid-walk would; returns the pages held pinned
/// at the last such cancel.
#[cfg(any(test, feature = "pg_test"))]
pub(crate) fn cancel_at_chunk_load(load: i64) -> i64 {
    CANCEL_AT_LOAD.replace((load, 0)).1
}

#[cfg(any(test, feature = "pg_test"))]
fn cancel_if_asked() {
    let (at, _) = CANCEL_AT_LOAD.get();
    if at != 0 && CHUNK_LOADS.get() >= at {
        CANCEL_AT_LOAD.set((0, crate::storage::held_pages().0));
        // SAFETY: the flags a cancel request's signal handler sets.
        unsafe {
            pg_sys::QueryCancelPending = 1;
            pg_sys::InterruptPending = 1;
        }
        pgrx::check_for_interrupts!();
    }
}

/// Phrase candidates whose positions were checked since the last reset.
pub(crate) fn position_checks() -> i64 {
    POSITION_CHECKS.get()
}

/// Position lists read for phrase candidates since the last reset.
pub(crate) fn position_reads() -> i64 {
    POSITION_READS.get()
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
/// A phrase (or any span query every slot of which must be present) the
/// walk can prune: its documents are a subset of the conjunction of its
/// terms, and a document scores the same under either, so the conjunction's
/// bounds hold and only the candidates that could enter the top k have
/// their positions read.
struct SpanCheck<'q> {
    /// The term of each slot, in slot order.
    slots: Vec<&'q str>,
    span: &'q boldi_vigna::SpanQuery,
    filter: Option<&'q SpanPositionFilter>,
}

/// Whether every term the span query names must occur for it to match.
fn span_requires_all(query: &boldi_vigna::SpanQuery) -> bool {
    use boldi_vigna::SpanQuery::*;
    match query {
        Term(_) => true,
        Ordered(children) | Unordered(children) => children.iter().all(span_requires_all),
        MaxGaps { inner, .. }
        | GapsInRange { inner, .. }
        | MaxWidth { inner, .. }
        | WithinPositions { inner, .. } => span_requires_all(inner),
        Empty
        | Or(_)
        | NotContaining { .. }
        | NotContainedBy { .. }
        | NonOverlapping { .. }
        | Containing { .. }
        | ContainedBy { .. }
        | Overlapping { .. }
        | Before { .. }
        | After { .. } => false,
    }
}

fn prunable_shape(query: &Query) -> Option<(Combine, Vec<&str>, Option<SpanCheck<'_>>)> {
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
    let mut check = None;
    let (combine, mut terms) = match unboost(query) {
        Query::Term(term) => (Combine::Any, vec![term.as_str()]),
        Query::And(left, right) => (Combine::All, leaves([&**left, &**right])?),
        Query::Conjunction(children) => (Combine::All, leaves(children)?),
        Query::Or(left, right) => (Combine::Any, leaves([&**left, &**right])?),
        Query::Disjunction { min: 1, children } => (Combine::Any, leaves(children)?),
        Query::Span {
            term_slots,
            span_query,
            position_filter,
        } if !term_slots.is_empty() && span_requires_all(span_query) => {
            let slots: Vec<&str> = term_slots
                .iter()
                .map(|slot| match slot {
                    SpanTermSlot::Term(term) => Some(term.as_str()),
                    _ => None,
                })
                .collect::<Option<_>>()?;
            check = Some(SpanCheck {
                slots: slots.clone(),
                span: span_query,
                filter: position_filter.as_ref(),
            });
            (Combine::All, slots)
        }
        _ => return None,
    };
    terms.sort_unstable();
    terms.dedup();
    Some((combine, terms, check))
}

/// A query of terms and all-required spans that [`prunable_shape`] does not
/// accept, combined by AND, OR, AT LEAST and AND NOT: `w OR "p q"`,
/// `(a AND b) OR c`, `a AND (b OR c)`, `a AND NOT b`, boosts anywhere.
///
/// A document's score does not depend on the shape: it is the sum over the
/// scoring terms it holds, whichever part of the query it matched by, a
/// phrase's words included when the phrase itself does not match (see
/// [`IndexScorer::score_listed`]). So the disjunction walk over the scoring
/// terms bounds such a query's documents exactly as it bounds a flat
/// disjunction's, and a document holding a scoring term is a candidate only
/// when the shape holds it too: tested a word of documents at a time over
/// the terms' bits, a phrase's positions read only for a candidate that
/// ranks. A match holding no scoring term scores zero; those are filled
/// from the candidate stream as for a disjunction's elided terms.
enum Shape<'q> {
    Term(&'q str),
    /// A span every slot of which must occur.
    Phrase(SpanCheck<'q>),
    All(Vec<Shape<'q>>),
    /// At least `min` of the children, one or more.
    Any {
        min: usize,
        children: Vec<Shape<'q>>,
    },
    /// Only as a child of [`Shape::All`] beside a positive child.
    Not(Box<Shape<'q>>),
}

/// The largest `AT LEAST` count a [`Shape`] accepts: the walk counts a
/// word of documents' children in as many words.
const MAX_AT_LEAST: usize = 64;

/// Whether a condition holds: certainly, possibly, or certainly not.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tri {
    No,
    Maybe,
    Yes,
}

impl<'q> Shape<'q> {
    fn of(query: &'q Query) -> Option<Self> {
        let children = |children: &mut dyn Iterator<Item = &'q Query>| {
            children.map(Self::of).collect::<Option<Vec<_>>>()
        };
        let all = |children: &mut dyn Iterator<Item = &'q Query>| {
            let children = children
                .map(|child| match child {
                    Query::Not(inner) => Self::of(inner).map(|inner| Self::Not(Box::new(inner))),
                    child => Self::of(child),
                })
                .collect::<Option<Vec<_>>>()?;
            children
                .iter()
                .any(|child| !matches!(child, Self::Not(_)))
                .then_some(Self::All(children))
        };
        Some(match query {
            Query::Boost { inner, .. } => return Self::of(inner),
            Query::Term(term) => Self::Term(term),
            Query::Span {
                term_slots,
                span_query,
                position_filter,
            } if !term_slots.is_empty() && span_requires_all(span_query) => {
                Self::Phrase(SpanCheck {
                    slots: term_slots
                        .iter()
                        .map(|slot| match slot {
                            SpanTermSlot::Term(term) => Some(term.as_str()),
                            _ => None,
                        })
                        .collect::<Option<_>>()?,
                    span: span_query,
                    filter: position_filter.as_ref(),
                })
            }
            Query::And(left, right) => all(&mut [&**left, &**right].into_iter())?,
            Query::Conjunction(children) => all(&mut children.iter())?,
            Query::Or(left, right) => Self::Any {
                min: 1,
                children: children(&mut [&**left, &**right].into_iter())?,
            },
            Query::Disjunction {
                min,
                children: kids,
            }
            | Query::AtLeast {
                min,
                children: kids,
            } if *min >= 1 && (*min as usize) <= kids.len().min(MAX_AT_LEAST) => Self::Any {
                min: *min as usize,
                children: children(&mut kids.iter())?,
            },
            _ => return None,
        })
    }

    /// Every term the shape names, sorted and deduplicated.
    fn leaves(&self) -> Vec<&'q str> {
        fn walk<'q>(shape: &Shape<'q>, out: &mut Vec<&'q str>) {
            match shape {
                Shape::Term(term) => out.push(term),
                Shape::Phrase(check) => out.extend(check.slots.iter().copied()),
                Shape::All(children) | Shape::Any { children, .. } => {
                    children.iter().for_each(|child| walk(child, out));
                }
                Shape::Not(inner) => walk(inner, out),
            }
        }
        let mut out = Vec::new();
        walk(self, &mut out);
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Whether the shape can hold a document, given for each term whether
    /// the document holds it.
    fn holds(&self, term: &impl Fn(&str) -> Tri) -> Tri {
        match self {
            Self::Term(name) => term(name),
            Self::Phrase(check) => {
                if check.slots.iter().any(|slot| term(slot) == Tri::No) {
                    Tri::No
                } else {
                    Tri::Maybe
                }
            }
            Self::All(children) => {
                children
                    .iter()
                    .fold(Tri::Yes, |sum, child| match (sum, child.holds(term)) {
                        (Tri::No, _) | (_, Tri::No) => Tri::No,
                        (Tri::Yes, Tri::Yes) => Tri::Yes,
                        _ => Tri::Maybe,
                    })
            }
            Self::Any { min, children } => {
                let held: Vec<Tri> = children.iter().map(|child| child.holds(term)).collect();
                let yes = held.iter().filter(|h| **h == Tri::Yes).count();
                let maybe = held.iter().filter(|h| **h != Tri::No).count();
                if yes >= *min {
                    Tri::Yes
                } else if maybe >= *min {
                    Tri::Maybe
                } else {
                    Tri::No
                }
            }
            Self::Not(inner) => match inner.holds(term) {
                Tri::Yes => Tri::No,
                Tri::Maybe => Tri::Maybe,
                Tri::No => Tri::Yes,
            },
        }
    }

    /// Whether every document holding a term of `walked` matches: the shape
    /// is a disjunction and each such term one of its children alone, so
    /// the walk's candidates need no test.
    fn any_holds(&self, walked: &[&str]) -> bool {
        match self {
            Self::Any { min: 1, children } => walked.iter().all(|name| {
                children
                    .iter()
                    .any(|child| matches!(child, Self::Term(term) if term == name))
            }),
            _ => false,
        }
    }
}

/// Where the walk finds a term's members in one source.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bits {
    /// A walked scoring term, by index.
    Term(usize),
    /// A term that scores nothing, walked as a filter, by index.
    Filter(usize),
    /// Absent from the source: no document holds it.
    Absent,
}

/// A [`Shape`] resolved against one source's walked streams.
enum Condition {
    Leaf(Bits),
    /// A phrase: its slots' streams, and its check among the walk's.
    Phrase(Vec<Bits>, usize),
    All(Vec<Condition>),
    Any {
        min: usize,
        children: Vec<Condition>,
    },
    Not(Box<Condition>),
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

    /// True when no term scores and the query is a conjunction (a phrase
    /// included) of its leaves: every match scores zero and ranks in heap
    /// order, and the ordinal walk finds the first `k` as a conjunction of
    /// filters without scoring anything (see [`OrdinalWalk::all_unscored`]).
    /// A disjunction of elided terms is read from the candidate stream
    /// instead.
    pub(crate) fn walks_unscored(&self) -> bool {
        self.scores_nothing()
            && prunable_shape(&self.query).is_some_and(|(combine, _, _)| combine == Combine::All)
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
    /// A query of terms and all-required spans under AND, OR, AT LEAST and
    /// AND NOT that is not flat (see [`Shape`]) is walked as the
    /// disjunction of its scoring terms, each candidate tested against it.
    ///
    /// `None` when the query is neither, when a scoring term is not one of
    /// its terms, or a source carries no block bounds; the caller then
    /// scores every candidate.
    pub(crate) fn top_k(&self, k: usize) -> Option<TopK> {
        let (combine, leaves, check, mixed) = match prunable_shape(&self.query) {
            Some((combine, leaves, check)) => (combine, leaves, check, None),
            None => {
                let shape = Shape::of(&self.query)?;
                (Combine::Any, shape.leaves(), None, Some(shape))
            }
        };
        // Every scoring term must be a leaf (no added terms), and a leaf that
        // is not a scoring term must be absent from the index altogether: it
        // then adds nothing to a disjunction and empties a conjunction. A
        // present leaf without a scorer is an elided dense term. In a
        // disjunction its documents score zero unless a scoring term also
        // lists them, so the walk over the scoring terms is exact as far as
        // its positive scores reach; when they are fewer than k the caller
        // fills the rest from the zero-scoring matches. In a conjunction the elided term adds
        // nothing to a score but still filters, so its cursor joins the walk
        // without a bound; a conjunction of elided terms alone is walked as
        // filters only, every match at zero, and stops at the first k.
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
                    // A mixed shape tests its candidates for every term it
                    // names, a scoring one or not.
                    Combine::Any if mixed.is_some() => filters.push(leaf),
                    Combine::Any => elided = true,
                    Combine::All => filters.push(leaf),
                }
                continue;
            }
            absent = true;
        }
        if let Some(shape) = &mixed {
            // Whether a document holding no scoring term can match: each
            // term that scores nothing but is present may be held.
            elided = shape.holds(&|term| {
                if filters.contains(&term) {
                    Tri::Maybe
                } else {
                    Tri::No
                }
            }) != Tri::No;
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
            // Largest source first: the threshold prunes only once the heap
            // holds k rows, and the biggest segment is the likeliest to hold
            // the best of them, so the smaller ones are walked pruned.
            let mut order: Vec<usize> = (0..self.view.sources.len()).collect();
            order.sort_by_key(|&i| std::cmp::Reverse(self.view.sources[i].0.document_count()));
            // Only a conjunction of scoring terms, elided filters beside them
            // or not, is warmed up (see [`Self::warm_up`]): a phrase's
            // best-bounded chunks may hold no phrase match, and a
            // disjunction's threshold forms early without it.
            let warmup = if combine == Combine::All
                && check.is_none()
                && mixed.is_none()
                && !self.terms.is_empty()
            {
                usize::try_from(WARMUP_CHUNKS.get()).unwrap_or(0)
            } else {
                0
            };
            loop {
                let mut visibility =
                    unsafe { Visibility::open(pg_sys::Oid::from(self.key.heap_oid), shortcut) };
                if warmup > 0 {
                    self.warm_up(
                        &order,
                        &filters,
                        warmup,
                        &mut visibility,
                        k,
                        &mut heap,
                        &mut scored,
                    );
                    ordinal = true;
                } else {
                    for &i in &order {
                        self.walk_by_ordinal(
                            i,
                            combine,
                            &filters,
                            check.as_ref(),
                            mixed.as_ref(),
                            &mut visibility,
                            k,
                            &mut heap,
                            &mut scored,
                        );
                        ordinal = true;
                    }
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
        check: Option<&SpanCheck<'_>>,
        mixed: Option<&Shape<'_>>,
        visibility: &mut Visibility,
        k: usize,
        heap: &mut BinaryHeap<Ranked>,
        scored: &mut usize,
    ) {
        if let Some(mut parts) = self.open_walk(i, combine, filters, check, mixed) {
            self.run_walk(&mut parts, visibility, k, heap, scored, Pass::Walk(&[]));
        }
    }

    /// The streams and checks of source `i`'s walk, or `None` when the
    /// source holds no match.
    fn open_walk(
        &self,
        i: usize,
        combine: Combine,
        filters: &[&str],
        check: Option<&SpanCheck<'_>>,
        mixed: Option<&Shape<'_>>,
    ) -> Option<WalkParts<'_>> {
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
        // An immutable segment's identity, under which its terms' parsed
        // bounds are kept across statements; the write buffer has none.
        let key = self.view.keys.get(i).copied();
        let mut terms = Vec::with_capacity(self.terms.len());
        for (slot, (name, scorer)) in self.terms.iter().enumerate() {
            let Some(term) = segment_error_in(source.term(name), label) else {
                match combine {
                    // A missing term empties the conjunction in this source.
                    Combine::All => return None,
                    Combine::Any => continue,
                }
            };
            terms.push(Self::ordinal_term(
                &term,
                slot,
                Some(scorer),
                label,
                key.map(|k| (k, name.as_str())),
            ));
        }
        // A mixed shape is tested per candidate unless every document
        // holding a walked term matches it; untested, it walks no filters.
        let tested = mixed.filter(|shape| {
            let walked: Vec<&str> = terms
                .iter()
                .map(|t| self.terms[t.slot].0.as_str())
                .collect();
            !shape.any_holds(&walked)
        });
        let filters = if mixed.is_some() && tested.is_none() {
            &[]
        } else {
            filters
        };
        let mut filter_terms = Vec::with_capacity(filters.len());
        // A mixed shape's filters, by name: one absent from this source is
        // held by no document here.
        let mut filter_names = Vec::with_capacity(filters.len());
        for name in filters {
            match segment_error_in(source.term(name), label) {
                Some(term) => {
                    filter_terms.push(Self::ordinal_term(
                        &term,
                        usize::MAX,
                        None,
                        label,
                        key.map(|k| (k, *name)),
                    ));
                    filter_names.push(*name);
                }
                None if mixed.is_some() => {}
                None => return None,
            }
        }
        // With no scoring term a conjunction is walked as its filters alone;
        // a disjunction of none is nothing.
        let unscored = terms.is_empty();
        if unscored && (combine != Combine::All || filter_terms.is_empty()) {
            return None;
        }
        // A phrase reads each slot's positions from the term's payload; a
        // slot is a scoring term or, when the scorer elided it, a filter.
        let phrase = check.map(|check| {
            let slots = check
                .slots
                .iter()
                .map(|name| {
                    let member = terms
                        .iter()
                        .position(|t| self.terms[t.slot].0 == *name)
                        .map(Member::Term)
                        .or_else(|| filters.iter().position(|f| f == name).map(Member::Filter))
                        .unwrap_or_else(|| {
                            crate::storage::corrupt(format!(
                                "Stannum {label}: phrase slot {name} is not walked"
                            ))
                        });
                    let term = segment_error_in(source.term(name), label).unwrap_or_else(|| {
                        crate::storage::corrupt(format!(
                            "Stannum {label}: phrase slot {name} vanished from the source"
                        ))
                    });
                    let payload = segment_error_in(term.payload(), label);
                    (member, walk_cursor(&payload), payload.count())
                })
                .collect::<Vec<_>>();
            PhraseCheck {
                plan: boldi_vigna::PhrasePlan::new(check.span, |slot| u64::from(slots[slot].2)),
                slots: slots
                    .into_iter()
                    .map(|(member, cursor, _)| (member, cursor))
                    .collect(),
                solver: boldi_vigna::SpanSolver::new(check.span).unwrap_or_else(|error| {
                    crate::storage::corrupt(format!("Stannum {label}: span solver: {error}"))
                }),
                filter: check.filter.cloned(),
                positions: vec![Vec::new(); check.slots.len()],
                read: vec![false; check.slots.len()],
            }
        });
        let mut phrases = Vec::new();
        let condition = tested.map(|shape| {
            let bits = |name: &str| {
                terms
                    .iter()
                    .position(|t| self.terms[t.slot].0 == name)
                    .map(Bits::Term)
                    .or_else(|| {
                        filter_names
                            .iter()
                            .position(|f| *f == name)
                            .map(Bits::Filter)
                    })
                    .unwrap_or(Bits::Absent)
            };
            Self::condition(shape, &bits, &**source, label, &mut phrases)
        });
        let docs = segment_error_in(source.doc_table(), label);
        SETUP_BLOCKS.set(SETUP_BLOCKS.get() + blocks_used() - started);
        Some(WalkParts {
            combine,
            unscored,
            terms,
            filters: filter_terms,
            docs,
            source: &**source,
            dead,
            phrase,
            condition,
            phrases,
        })
    }

    /// Runs `pass` of a source's walk over `parts`, and moves its streams
    /// back to their first chunks for the next pass.
    fn run_walk<'a>(
        &'a self,
        parts: &mut WalkParts<'a>,
        visibility: &mut Visibility,
        k: usize,
        heap: &mut BinaryHeap<Ranked>,
        scored: &mut usize,
        pass: Pass<'_>,
    ) {
        let ready = blocks_used();
        // The pages of the length and class tables stay pinned while the
        // walk reads them per candidate, and are released as it ends.
        let _held = HeldPages::open(parts.source);
        let mut walk = OrdinalWalk {
            scorer: self,
            terms: std::mem::take(&mut parts.terms),
            filters: std::mem::take(&mut parts.filters),
            docs: &parts.docs,
            index: parts.source,
            document_count: parts.source.document_count(),
            lengths: parts.source.lengths(),
            dead: &parts.dead,
            visibility,
            k,
            heap,
            scored,
            iterations: 0,
            // An unscored walk has no score to seed: every match ties at zero.
            seed: if parts.unscored {
                None
            } else {
                seeded_threshold()
            },
            bar: None,
            values: Vec::new(),
            uppers: Vec::new(),
            buckets: Vec::new(),
            term_subs: Vec::new(),
            class_bounds: Box::new([(0, 0.0); 256]),
            class_stamp: 0,
            phrase: parts.phrase.take(),
            pending: Vec::new(),
            condition: parts.condition.take(),
            phrases: std::mem::take(&mut parts.phrases),
            warmed: match pass {
                Pass::Walk(warmed) => warmed,
                _ => &[],
            },
        };
        // The heap is shared by every source's walk and by the warm-up's
        // passes: a new walk takes its threshold from it as it stands.
        walk.raise();
        match (pass, parts.combine) {
            (Pass::Bound(out), _) => walk.chunk_bounds(out),
            (Pass::Warm(keys), _) => walk.warm(keys),
            (Pass::Walk(_), Combine::Any) => walk.any(),
            (Pass::Walk(_), Combine::All) if parts.unscored => walk.all_unscored(),
            (Pass::Walk(_), Combine::All) => walk.all(),
        }
        parts.terms = walk.terms;
        parts.filters = walk.filters;
        parts.phrase = walk.phrase;
        parts.condition = walk.condition;
        parts.phrases = walk.phrases;
        for term in parts.terms.iter_mut().chain(parts.filters.iter_mut()) {
            term.pos = 0;
        }
        WALK_BLOCKS.set(WALK_BLOCKS.get() + blocks_used() - ready);
    }

    /// Resolves a mixed shape against one source's walked streams, `bits`
    /// naming each term's, and builds the position check of each phrase
    /// whose slots the source holds into `phrases`.
    fn condition<'a>(
        shape: &Shape<'_>,
        bits: &impl Fn(&str) -> Bits,
        source: &'a dyn Index,
        label: &str,
        phrases: &mut Vec<Option<PhraseCheck<'a>>>,
    ) -> Condition {
        let mut all = |children: &[Shape<'_>]| {
            children
                .iter()
                .map(|child| Self::condition(child, bits, source, label, phrases))
                .collect()
        };
        match shape {
            Shape::Term(name) => Condition::Leaf(bits(name)),
            Shape::Phrase(check) => {
                let slots: Vec<Bits> = check.slots.iter().map(|name| bits(name)).collect();
                if slots.contains(&Bits::Absent) {
                    return Condition::Leaf(Bits::Absent);
                }
                let members = slots
                    .iter()
                    .map(|slot| match *slot {
                        Bits::Term(t) => Member::Term(t),
                        Bits::Filter(f) => Member::Filter(f),
                        Bits::Absent => unreachable!("checked above"),
                    })
                    .collect::<Vec<_>>();
                phrases.push(Some(phrase_check(check, &members, source, label)));
                Condition::Phrase(slots, phrases.len() - 1)
            }
            Shape::All(children) => Condition::All(all(children)),
            Shape::Any { min, children } => Condition::Any {
                min: *min,
                children: all(children),
            },
            Shape::Not(inner) => Condition::Not(Box::new(Self::condition(
                inner, bits, source, label, phrases,
            ))),
        }
    }

    /// A term's streams in one source for the walk over ordinals. A filter
    /// has no scorer: it is a member test only. `cached` names the term in
    /// an immutable segment, whose parsed bounds are kept across statements.
    fn ordinal_term<'a>(
        term: &segment::segment::Term<'a>,
        slot: usize,
        scorer: Option<&TermScorer>,
        label: &str,
        cached: Option<((u64, u32), &str)>,
    ) -> OrdinalTerm<'a> {
        let ordinals = segment_error_in(term.ordinals(), label);
        let list = ordinals.list().map(<[u32]>::to_vec);
        let bounds = match cached {
            // A list is a few bytes, parsed with the stream.
            Some((segment, name)) if list.is_none() => {
                TermBounds::cached(segment, name, || TermBounds::parse(&ordinals, label))
            }
            _ => TermBounds::parse(&ordinals, label),
        };
        OrdinalTerm {
            slot,
            ordinals,
            chunk: None,
            keys: bounds.keys,
            list,
            bounds: bounds.bounds,
            sub_bounds: bounds.sub_bounds,
            bound_scores: Vec::new(),
            by_bucket: Vec::new(),
            pos: 0,
            term_max: scorer.map_or(0.0, |scorer| scorer.bound(&bounds.whole)),
            words: Box::new([0; segment::ordinals::WORDS]),
            head: std::ptr::null(),
            head_len: 0,
            rest: std::ptr::null(),
            rest_end: 0,
            held: None,
            nibbles: Cell::new(None),
            loaded: None,
            counted: (0, 0),
            members: Vec::new(),
            dense: false,
            rank_base: 0,
        }
    }
}

/// A term's chunk keys and bounds in one source, as the walk reads them.
#[derive(Clone)]
struct TermBounds {
    /// Keys of the chunks the term occupies, ascending.
    keys: std::rc::Rc<[u16]>,
    /// Per key: the shortest document per bucket among the term's postings there.
    bounds: std::rc::Rc<[[u32; BUCKET_COUNT]]>,
    /// Per key and sub-block: one past the largest bucket the term has there.
    sub_bounds: std::rc::Rc<[[u8; SUBS]]>,
    /// Over the whole stream.
    whole: BlockBound,
}

/// Bytes of parsed bounds [`TERM_BOUNDS`] holds before it is emptied.
const TERM_BOUNDS_BUDGET: usize = 32 << 20;

/// Parsed bounds by segment (index identity and generation) and term, and
/// the bytes they hold.
type KeptBounds = (usize, FxHashMap<(u64, u32), FxHashMap<String, TermBounds>>);

thread_local! {
    /// Segments are immutable, so a term's bounds hold for as long as its
    /// segment exists; parsing every chunk bound of every query term in
    /// every segment was a few percent of each statement. Emptied wholesale
    /// past [`TERM_BOUNDS_BUDGET`].
    static TERM_BOUNDS: RefCell<KeptBounds> = RefCell::new((0, FxHashMap::default()));
}

impl TermBounds {
    fn parse(ordinals: &segment::ordinals::Ordinals<'_>, label: &str) -> Self {
        let corrupt =
            |what: &str| -> ! { crate::storage::corrupt(format!("Stannum {label}: {what}")) };
        let parsed = segment_error_in(ordinals.bounds(), label);
        let whole = parsed
            .iter()
            .map(|bound| BlockBound {
                min_len: bound.min_len,
            })
            .reduce(|merged, block| merged.merge(&block))
            .unwrap_or_else(|| corrupt("a term's stream carries no bounds"));
        let keys: Vec<u16> = match ordinals.list() {
            Some(list) => {
                let mut keys: Vec<u16> = list.iter().map(|o| (o >> 16) as u16).collect();
                keys.dedup();
                keys
            }
            None => (0..ordinals.chunk_count())
                .map(|c| ordinals.chunk_key(c))
                .collect(),
        };
        // The stored bound per chunk, or a list's one bound for every chunk
        // it touches.
        let mut bounds = Vec::with_capacity(keys.len());
        let mut sub_bounds = Vec::with_capacity(keys.len());
        for i in 0..keys.len() {
            let bound = segment_error_in(ordinals.chunk_bound(i), label)
                .unwrap_or_else(|| corrupt("a chunk carries no bound"));
            bounds.push(bound.min_len);
            sub_bounds.push(bound.subs);
        }
        Self {
            keys: keys.into(),
            bounds: bounds.into(),
            sub_bounds: sub_bounds.into(),
            whole,
        }
    }

    /// The bounds of `term` in `segment`, parsed by `parse` unless kept.
    fn cached(segment: (u64, u32), term: &str, parse: impl FnOnce() -> Self) -> Self {
        let found = TERM_BOUNDS.with_borrow(|(_, kept)| kept.get(&segment)?.get(term).cloned());
        if let Some(found) = found {
            return found;
        }
        let parsed = parse();
        let bytes = term.len()
            + parsed.keys.len()
                * (size_of::<u16>() + size_of::<[u32; BUCKET_COUNT]>() + size_of::<[u8; SUBS]>());
        TERM_BOUNDS.with_borrow_mut(|(held, kept)| {
            if *held + bytes > TERM_BOUNDS_BUDGET {
                kept.clear();
                *held = 0;
            }
            *held += bytes;
            kept.entry(segment)
                .or_default()
                .insert(term.to_owned(), parsed.clone());
        });
        parsed
    }
}

/// A source's streams and checks for its walk, kept across the passes of a
/// warm-up (see [`IndexScorer::open_walk`]).
struct WalkParts<'a> {
    combine: Combine,
    /// A conjunction of elided terms alone: every match scores zero.
    unscored: bool,
    terms: Vec<OrdinalTerm<'a>>,
    filters: Vec<OrdinalTerm<'a>>,
    docs: DocTable<'a>,
    source: &'a dyn Index,
    /// Dead ordinals, ascending.
    dead: std::rc::Rc<Vec<u32>>,
    phrase: Option<PhraseCheck<'a>>,
    condition: Option<Condition>,
    phrases: Vec<Option<PhraseCheck<'a>>>,
}

impl WalkParts<'_> {
    /// The documents of the source holding every term and filter, as if
    /// each occurred independently of the others.
    fn estimated_matches(&self) -> f64 {
        let documents = f64::from(self.source.document_count().max(1));
        self.terms
            .iter()
            .chain(&self.filters)
            .fold(documents, |matches, term| {
                matches * f64::from(term.ordinals.count()) / documents
            })
    }
}

/// A conjunction's chunk bounded from the chunk directory alone: the length
/// every term's bound holds at, the longest of the terms' shortest
/// documents there, and the chunk's bound at it.
#[derive(Clone, Copy)]
struct Bounded {
    key: u16,
    min_length: u32,
    bound: f32,
}

/// Which part of its work a source's walk runs (see
/// [`IndexScorer::run_walk`]).
enum Pass<'p> {
    /// The walk in chunk order, skipping the chunks the warm-up evaluated,
    /// whose keys are given ascending.
    Walk(&'p [u16]),
    /// A conjunction's warm-up: every chunk each term and filter holds,
    /// bounded, nothing loaded.
    Bound(&'p mut Vec<Bounded>),
    /// A conjunction's warm-up: these chunks evaluated, in this order.
    Warm(&'p [Bounded]),
}

impl IndexScorer {
    /// Walks a conjunction's sources (`order`) after evaluating up to
    /// `limit` of their chunks, across every source, before any walk runs:
    /// those whose bound by the chunk directory alone is highest, best
    /// first, into the shared heap. Each walk then skips the chunks warmed
    /// in its source, so no document is scored twice.
    ///
    /// A walk in chunk order raises its threshold only as it happens to meet
    /// good documents; for a conjunction of common words that is most of the
    /// chunks, where seeding the walk with its final threshold loaded a
    /// ninth of them. Exact whatever the order: each chunk is evaluated once,
    /// against a threshold that only rises, and a range is judged by the
    /// location of its first ordinal, which within a source is the earliest
    /// of every document in the range wherever the range lies.
    #[expect(
        clippy::too_many_arguments,
        reason = "one call site; the arguments are the walk's state"
    )]
    fn warm_up(
        &self,
        order: &[usize],
        filters: &[&str],
        limit: usize,
        visibility: &mut Visibility,
        k: usize,
        heap: &mut BinaryHeap<Ranked>,
        scored: &mut usize,
    ) {
        // Each source's streams are opened once, for all three passes.
        let mut walks: Vec<WalkParts<'_>> = order
            .iter()
            .filter_map(|&i| self.open_walk(i, Combine::All, filters, None, None))
            .collect();
        let estimate: f64 = walks.iter().map(WalkParts::estimated_matches).sum();
        WARMUP_ESTIMATE.set(Some(estimate));
        let limit = if estimate < k as f64 * WARMUP_MIN_MATCHES.get() {
            0
        } else {
            limit
        };
        let mut picks: Vec<(usize, Bounded)> = Vec::new();
        if limit > 0 {
            let mut bounded = Vec::new();
            for (n, parts) in walks.iter_mut().enumerate() {
                bounded.clear();
                self.run_walk(
                    parts,
                    visibility,
                    k,
                    heap,
                    scored,
                    Pass::Bound(&mut bounded),
                );
                picks.extend(bounded.iter().map(|chunk| (n, *chunk)));
            }
        }
        let best = |a: &(usize, Bounded), b: &(usize, Bounded)| {
            b.1.bound
                .total_cmp(&a.1.bound)
                .then(a.0.cmp(&b.0))
                .then(a.1.key.cmp(&b.1.key))
        };
        if picks.len() > limit {
            picks.select_nth_unstable_by(limit - 1, best);
            picks.truncate(limit);
        }
        picks.sort_unstable_by(best);
        // Each source's picks are evaluated in one walk, best first, the
        // largest source first as the walks go.
        let mut warmed = vec![Vec::new(); walks.len()];
        for &(n, chunk) in &picks {
            warmed[n].push(chunk);
        }
        let mut skip = Vec::with_capacity(walks.len());
        for (parts, chunks) in walks.iter_mut().zip(&warmed) {
            if !chunks.is_empty() {
                self.run_walk(parts, visibility, k, heap, scored, Pass::Warm(chunks));
            }
            let mut keys: Vec<u16> = chunks.iter().map(|chunk| chunk.key).collect();
            keys.sort_unstable();
            skip.push(keys);
        }
        if !picks.is_empty() {
            WARMUP_EVALUATED.set(WARMUP_EVALUATED.get() + picks.len() as i64);
            WARMUP_THRESHOLD.set(if heap.len() == k {
                heap.peek().map(|worst| worst.0)
            } else {
                None
            });
        }
        for (parts, keys) in walks.iter_mut().zip(&skip) {
            self.run_walk(parts, visibility, k, heap, scored, Pass::Walk(keys));
        }
    }
}

impl OrdinalWalk<'_, '_> {
    /// Reports every chunk of a conjunction each term and filter holds,
    /// bounded as [`Self::all`] bounds it, from the chunk directory alone.
    fn chunk_bounds(&mut self, out: &mut Vec<Bounded>) {
        let lead = (0..self.terms.len())
            .min_by_key(|&t| self.terms[t].keys.len())
            .expect("a conjunction has terms");
        'keys: for p in 0..self.terms[lead].keys.len() {
            let key = self.terms[lead].keys[p];
            self.terms[lead].pos = p;
            for (t, term) in self.terms.iter_mut().enumerate() {
                if t == lead {
                    continue;
                }
                term.pos += term.keys[term.pos..].partition_point(|k| *k < key);
                match term.key() {
                    None => return,
                    Some(found) if found != key => continue 'keys,
                    Some(_) => {}
                }
            }
            for filter in &mut self.filters {
                filter.pos += filter.keys[filter.pos..].partition_point(|k| *k < key);
                match filter.key() {
                    None => return,
                    Some(found) if found != key => continue 'keys,
                    Some(_) => {}
                }
            }
            // Conjunction members share one document, so every term's bound
            // holds at the longest of the chunks' shortest documents.
            let min_length = self
                .terms
                .iter()
                .map(|term| term.bound_block(term.pos).shortest())
                .max()
                .expect("a conjunction has terms");
            let mut bound = 0.0_f32;
            for term in &self.terms {
                let scorer = &self.scorer.terms[term.slot].1;
                bound += scorer.bound_with_min_length(&term.bound_block(term.pos), min_length);
            }
            out.push(Bounded {
                key,
                min_length,
                bound,
            });
        }
    }

    /// Evaluates a conjunction's chunks, in the order given, into the heap;
    /// [`Self::chunk_bounds`] found and bounded them.
    fn warm(&mut self, chunks: &[Bounded]) {
        let lead = (0..self.terms.len())
            .min_by_key(|&t| self.terms[t].keys.len())
            .expect("a conjunction has terms");
        // As `all` orders them: the other terms rarest first.
        let mut others: Vec<usize> = (0..self.terms.len()).filter(|&t| t != lead).collect();
        others.sort_by_key(|&t| self.terms[t].keys.len());
        let mut set: Box<segment::ordinals::Words> = Box::new([0; segment::ordinals::WORDS]);
        for chunk in chunks {
            self.iterations = self.iterations.wrapping_add(1);
            if self.iterations.is_multiple_of(64) {
                pgrx::check_for_interrupts!();
            }
            let base = u32::from(chunk.key) << 16;
            if self.threshold().is_some() && !self.can_beat(chunk.bound, base) {
                continue;
            }
            for term in self.terms.iter_mut().chain(self.filters.iter_mut()) {
                term.pos = term
                    .keys
                    .binary_search(&chunk.key)
                    .expect("every stream holds a bounded chunk");
            }
            self.evaluate_all(chunk.key, lead, &others, chunk.min_length, &mut set);
        }
    }
}

/// Ordinals per sub-block of a chunk, at which a walk prunes within a chunk.
const SUB: usize = segment::ordinals::SUB as usize;

/// Sub-blocks per chunk.
const SUBS: usize = segment::ordinals::SUBS;

/// Words per sub-block.
const SUB_WORDS: usize = SUB / 64;

/// One scoring term's streams in one source, for the walk over ordinals.
struct OrdinalTerm<'a> {
    slot: usize,
    ordinals: segment::ordinals::Ordinals<'a>,
    /// The chunk last loaded, for its members' buckets.
    chunk: Option<segment::ordinals::Chunk>,
    /// Keys of the chunks the term occupies, ascending.
    keys: std::rc::Rc<[u16]>,
    /// The stream as a list, when it is one.
    list: Option<Vec<u32>>,
    /// Per key: the shortest document per bucket among the term's postings there.
    bounds: std::rc::Rc<[[u32; BUCKET_COUNT]]>,
    /// Per key and sub-block: one past the largest bucket the term has there,
    /// zero where it has no posting.
    sub_bounds: std::rc::Rc<[[u8; SUBS]]>,
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
    /// The current chunk's members as words when it is a list or an array
    /// chunk; a bitmap chunk's words are read in place from `chunk`, whose
    /// bytes are a pinned page's or the read cache's, rather than copied
    /// here per load.
    words: Box<segment::ordinals::Words>,
    /// The current chunk's members as low bits when it is not a bitmap.
    members: Vec<u16>,
    /// A loaded bitmap chunk's first `head_len` bytes, contiguous in memory
    /// (see [`segment::ordinals::Chunk::pieces`]): the words are read from
    /// here, a bounds check per word, where going through `chunk` for every
    /// word was a branch and two dereferences more in the loops that test a
    /// bit per word per term. Valid while `chunk` holds the chunk.
    head: *const u8,
    head_len: usize,
    /// The chunk's bytes `head_len..rest_end`, on its second page when it
    /// is held in place, at `rest` plus their offset in the chunk; a word
    /// in neither run is read through `chunk`.
    rest: *const u8,
    rest_end: usize,
    /// The slots the walk's segment holds this term's pages in, once it
    /// handed them out: its chunk's members, and the page of its bucket
    /// nibbles past those.
    held: Option<(usize, usize)>,
    /// The nibble page held last, in stream offsets.
    nibbles: Cell<Option<segment::source::HeldSpan>>,
    /// Whether the loaded chunk is a bitmap, held in `chunk`.
    dense: bool,
    /// Index into `keys` of the loaded chunk, if any: a load of the current
    /// chunk is asked for wherever a bit or bucket of it is first needed,
    /// and happens once.
    loaded: Option<usize>,
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

    /// Whether the current chunk is loaded.
    #[inline]
    fn is_loaded(&self) -> bool {
        self.loaded == Some(self.pos)
    }

    /// Loads the current chunk's members and the rank of its first member,
    /// unless it is loaded already.
    fn load(&mut self) {
        if self.is_loaded() {
            return;
        }
        self.loaded = Some(self.pos);
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
                self.head = std::ptr::null();
                (self.head_len, self.rest_end) = (0, 0);
            }
            None => {
                let chunk = self.load_chunk();
                self.dense = chunk.is_bitmap();
                let [(head, head_len), (rest, rest_len)] = if self.dense {
                    chunk.pieces()
                } else {
                    [(std::ptr::null(), 0); 2]
                };
                (self.head, self.head_len) = (head, head_len);
                self.rest = rest.wrapping_sub(head_len);
                self.rest_end = head_len + rest_len;
                if !self.dense {
                    // An array chunk's members are scattered into words for
                    // the bit tests; a bitmap's words are read in place.
                    chunk.words(&mut self.words);
                    chunk.members(&mut self.members);
                }
                self.rank_base = chunk.before;
                self.chunk = Some(chunk);
            }
        }
        #[cfg(any(test, feature = "pg_test"))]
        cancel_if_asked();
    }

    /// The current chunk of a chunked stream: in place from the pages the
    /// walk's segment holds pinned while the walk runs (a walk's hold span,
    /// see [`HeldPages`]), else copied through the read cache.
    fn load_chunk(&mut self) -> segment::ordinals::Chunk {
        // The chunk in hand borrows the pages the slot is about to move off.
        self.chunk = None;
        if self.held.is_none() {
            self.held = self
                .ordinals
                .held_slot()
                .and_then(|members| Some((members, self.ordinals.held_slot()?)));
        }
        match self.held {
            // SAFETY: the chunk is kept in `self.chunk` until the next load,
            // which empties it first, as above; the term lives in the walk,
            // which ends before its hold span (see `walk_by_ordinal`), and
            // the slot is this term's alone.
            Some((members, _)) => {
                segment_error(unsafe { self.ordinals.chunk_held(self.pos, members) })
            }
            None => segment_error(self.ordinals.chunk(self.pos)),
        }
    }

    /// The loaded bitmap chunk; `dense` says there is one.
    #[inline]
    fn bitmap(&self) -> &segment::ordinals::Chunk {
        self.chunk.as_ref().expect("a dense term holds its chunk")
    }

    /// Word `i` of the loaded chunk's members.
    #[inline]
    fn word(&self, i: usize) -> u64 {
        if !self.dense {
            return self.words[i];
        }
        match self.run(i * 8, 8) {
            // SAFETY: `run` found the word's eight bytes in one run.
            Some(at) => u64::from_le(unsafe { at.cast::<u64>().read_unaligned() }),
            None => self.bitmap().word(i),
        }
    }

    /// Where `len` bytes at `at` of a loaded bitmap chunk lie contiguously
    /// in `head` or `rest`, if they do; valid while the chunk is loaded.
    #[inline]
    fn run(&self, at: usize, len: usize) -> Option<*const u8> {
        if at + len <= self.head_len {
            Some(self.head.wrapping_add(at))
        } else if at >= self.head_len && at + len <= self.rest_end {
            Some(self.rest.wrapping_add(at))
        } else {
            None
        }
    }

    /// Whether the loaded chunk holds `low`.
    #[inline]
    fn holds(&self, low: u16) -> bool {
        self.word(usize::from(low / 64)) & (1 << (low % 64)) != 0
    }

    /// Sets `out` to the loaded chunk's members.
    fn assign_into(&self, out: &mut segment::ordinals::Words) {
        if self.dense {
            self.bitmap().words(out);
        } else {
            out.copy_from_slice(&*self.words);
        }
    }

    /// Ors the loaded chunk's members into `out`.
    fn or_into(&self, out: &mut segment::ordinals::Words) {
        if self.dense {
            self.bitmap().or_into(out);
        } else {
            for (o, word) in out.iter_mut().zip(self.words.iter()) {
                *o |= *word;
            }
        }
    }

    /// Keeps of `block`, words `from..` of sub-block `sub`, the loaded
    /// chunk's members: one pass over the bytes rather than a bounds check
    /// per word.
    fn and_sub(&self, sub: usize, from: usize, block: &mut [u64; SUB_WORDS]) {
        let first = sub * SUB_WORDS + from;
        let last = (sub + 1) * SUB_WORDS;
        if !self.dense {
            for (o, word) in block[from..].iter_mut().zip(&self.words[first..last]) {
                *o &= *word;
            }
        } else if let Some(at) = self.run(first * 8, (last - first) * 8) {
            // SAFETY: `run` found the sub-block's words in one run.
            let bytes = unsafe { std::slice::from_raw_parts(at, (last - first) * 8) };
            for (o, word) in block[from..].iter_mut().zip(bytes.chunks_exact(8)) {
                *o &= u64::from_le_bytes(word.try_into().expect("8"));
            }
        } else {
            for (o, i) in block[from..].iter_mut().zip(first..last) {
                *o &= self.word(i);
            }
        }
    }

    /// Keeps of `out` the loaded chunk's members.
    fn and_into(&self, out: &mut segment::ordinals::Words) {
        if self.dense {
            self.bitmap().and_into(out);
        } else {
            for (o, word) in out.iter_mut().zip(self.words.iter()) {
                *o &= *word;
            }
        }
    }

    /// The bucket of the member at `rank` in the stream, which lies in the
    /// loaded chunk.
    fn bucket(&self, rank: u32) -> Option<u8> {
        match &self.list {
            Some(_) => self.ordinals.list_buckets().get(rank as usize).copied(),
            None => {
                let within = rank - self.rank_base;
                match self.chunk.as_ref()?.bucket_in_place(within) {
                    Ok(bucket) => bucket,
                    Err(offset) => Some(self.far_bucket(offset, within)),
                }
            }
        }
    }

    /// The bucket of member `within` of a chunk held in place whose nibble,
    /// in the byte at `offset` of the stream, lies past the chunk's pages:
    /// read from the page the term holds for nibbles, moved there if need
    /// be. Candidates ask in ordinal order, so the page serves a run of
    /// them.
    #[inline(never)]
    fn far_bucket(&self, offset: u64, within: u32) -> u8 {
        let span = match self.nibbles.get() {
            Some(span) if offset.wrapping_sub(span.start) < span.len as u64 => span,
            _ => {
                let (_, slot) = self.held.expect("a chunk held in place has its slots");
                let span = segment_error(
                    self.ordinals
                        .held_span(slot, offset)
                        .expect("pages are held while a chunk held in place is"),
                );
                self.nibbles.set(Some(span));
                span
            }
        };
        // SAFETY: the span's page stays pinned until the next call on the
        // term's nibble slot or the end of the walk's hold span, and the
        // byte lies within it.
        let byte = unsafe { *span.data.add(offset.wrapping_sub(span.start) as usize) };
        segment::ordinals::nibble_in(byte, within)
    }

    /// Whether the loaded chunk holds `low`, and the member's rank in the stream.
    ///
    /// Candidates are mostly ranked in ascending order within a chunk, so
    /// the members before `low` are counted from where the last call
    /// stopped, forwards or back; counting from the chunk's start each time
    /// was a thousand words per candidate per term.
    fn rank(&mut self, low: u16) -> Option<u32> {
        if !self.dense {
            // A list's or an array chunk's members are held ascending: the
            // rank is the position.
            return self
                .members
                .binary_search(&low)
                .ok()
                .map(|at| self.rank_base + at as u32);
        }
        let word_at = usize::from(low / 64);
        let word = self.bitmap().word(word_at);
        if word & (1 << (low % 64)) == 0 {
            return None;
        }
        let (counted_to, mut before) = self.counted;
        if counted_to > word_at {
            before -= self.bitmap().count_words(word_at, counted_to);
        } else {
            before += self.bitmap().count_words(counted_to, word_at);
        }
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
    /// The threshold as the heap and the seed set it, kept current by every
    /// change to the heap: it is consulted per sub-block and per candidate,
    /// and deriving it each time peeked the heap and matched the seed.
    bar: Option<(f32, Tid)>,
    /// Scratch for scoring a candidate: per present term its bound or score,
    /// and the terms holding the document.
    values: Vec<f32>,
    uppers: Vec<(f32, usize)>,
    /// Scratch: per present term, the candidate's bucket once read.
    buckets: Vec<TfBucket>,
    /// Scratch for a chunk: per present term its bound per sub-block.
    term_subs: Vec<[f32; SUBS]>,
    /// A conjunction's bound per length class in the sub-block being
    /// scored, each with the stamp of the sub-block it was computed for:
    /// within a sub-block it depends on the class alone (see
    /// [`Self::score_conjunct`]).
    class_bounds: Box<[(u32, f32); 256]>,
    /// The stamp of the sub-block being scored; never zero once one is.
    class_stamp: u32,
    /// For a phrase: the positions check a candidate must pass to be admitted.
    phrase: Option<PhraseCheck<'a>>,
    /// Scratch for a sub-block: the phrase candidates that scored into the
    /// top k, as (score, low bits), awaiting their positions check.
    pending: Vec<(f32, u16)>,
    /// For a disjunction of a mixed shape (see [`Shape`]): the condition a
    /// candidate must meet, and its phrases' position checks.
    condition: Option<Condition>,
    phrases: Vec<Option<PhraseCheck<'a>>>,
    /// Keys of the chunks a conjunction's warm-up evaluated, ascending: the
    /// walk skips them.
    warmed: &'s [u16],
}

/// Which walked stream a phrase slot's term is.
#[derive(Clone, Copy)]
enum Member {
    Term(usize),
    Filter(usize),
}

/// A phrase's position check over the walk's candidates. Only a candidate
/// that scores into the top k has its positions read: a phrase of common
/// words matches a sliver of their conjunction, and reading positions for
/// every member of the conjunction was seconds per query at scale.
struct PhraseCheck<'a> {
    /// Per slot: the walked stream it ranks in, and its positions.
    slots: Vec<(Member, segment::payload::PayloadCursor<'a>)>,
    solver: boldi_vigna::SpanSolver,
    filter: Option<SpanPositionFilter>,
    /// The order to read the slots in, rarest first, and the distance each
    /// adjacent pair of leaves must keep, when the span's shape allows
    /// reading the slots one at a time.
    plan: Option<boldi_vigna::PhrasePlan>,
    /// Scratch: per slot, the candidate's positions.
    positions: Vec<Vec<u32>>,
    /// Scratch: per slot, whether the candidate's positions are read.
    read: Vec<bool>,
}

/// A cursor over a phrase slot's positions for a walk, reading its spans in
/// place from the pages the walk's segment holds pinned.
fn walk_cursor<'a>(payload: &segment::payload::Payload<'a>) -> segment::payload::PayloadCursor<'a> {
    let mut cursor = payload.cursor();
    // SAFETY: the cursor goes into a `PhraseCheck` of the walk, which is
    // dropped before the walk's hold span closes (see `walk_by_ordinal`).
    unsafe { cursor.hold_in_place() };
    cursor
}

/// The position check of a mixed shape's phrase, whose slot `n` ranks in
/// the walked stream `members[n]`; every slot's term is in `source`.
fn phrase_check<'a>(
    check: &SpanCheck<'_>,
    members: &[Member],
    source: &'a dyn Index,
    label: &str,
) -> PhraseCheck<'a> {
    let slots = check
        .slots
        .iter()
        .zip(members)
        .map(|(name, member)| {
            let term = segment_error_in(source.term(name), label).unwrap_or_else(|| {
                crate::storage::corrupt(format!(
                    "Stannum {label}: phrase slot {name} vanished from the source"
                ))
            });
            let payload = segment_error_in(term.payload(), label);
            (*member, walk_cursor(&payload), payload.count())
        })
        .collect::<Vec<_>>();
    PhraseCheck {
        plan: boldi_vigna::PhrasePlan::new(check.span, |slot| u64::from(slots[slot].2)),
        slots: slots
            .into_iter()
            .map(|(member, cursor, _)| (member, cursor))
            .collect(),
        solver: boldi_vigna::SpanSolver::new(check.span).unwrap_or_else(|error| {
            crate::storage::corrupt(format!("Stannum {label}: span solver: {error}"))
        }),
        filter: check.filter.cloned(),
        positions: vec![Vec::new(); check.slots.len()],
        read: vec![false; check.slots.len()],
    }
}

/// Narrows the shared members of a chunk (`lows` when `sparse`, else
/// `set`) to those `term` holds there, loading its chunk: false when none
/// were left to narrow, so the chunk need not be read.
fn narrow(
    term: &mut OrdinalTerm<'_>,
    sparse: bool,
    set: &mut segment::ordinals::Words,
    lows: &mut Vec<u16>,
) -> bool {
    if if sparse {
        lows.is_empty()
    } else {
        set.iter().all(|w| *w == 0)
    } {
        return false;
    }
    term.load();
    if sparse {
        lows.retain(|low| term.holds(*low));
    } else {
        term.and_into(set);
    }
    true
}

/// The sum a lane of a [`WordSieve`] must reach: the threshold in units of
/// `1 / SIEVE_TARGET` of itself.
const SIEVE_TARGET: u32 = segment::lanes::LaneSums::MAX_TARGET;

/// A conservative filter over a sub-block's candidate words: it clears the
/// lanes of a word whose first bound, the sum over the present terms
/// holding the member of their sub-block bounds, certainly falls below
/// the threshold, a word at a time rather than a member at a time.
///
/// Two tests, each sound alone. A member must hold one of the sub-block's
/// essential terms, the terms whose absence leaves the required ones and
/// the rest unable to reach the threshold (MaxScore, with the walk's
/// float slack). And per lane, the bounds are summed as small integers:
/// each term's bound is rounded up to a whole unit of `threshold /
/// SIEVE_TARGET`, and a lane is kept only when its sum reaches
/// `SIEVE_TARGET`. A lane cleared sums to at most `SIEVE_TARGET - 1` units,
/// so its bounds fall short of the threshold by at least a unit, 1/63 of
/// it: far more than the float error of summing them in any order, so its
/// first bound is below the threshold and `can_beat` would reject it, tie
/// or no tie. The threshold only rises, so a sieve planned at an earlier
/// threshold stays sound.
#[derive(Default)]
struct WordSieve {
    /// Whether the sieve filters at all.
    active: bool,
    /// Whether the terms `essential` filter, and those terms, as indices
    /// into the present terms; filtering by none clears every lane.
    by_essential: bool,
    essential: Vec<usize>,
    /// Whether the weighted sum filters; the required terms' weights, which
    /// every lane holds; and the other terms' as (index, weight).
    by_count: bool,
    start: u32,
    adds: Vec<(usize, u32)>,
    /// Scratch: the other terms' bounds, lightest first.
    order: Vec<(f32, usize)>,
}

impl WordSieve {
    /// Plans the sieve of sub-block `sub` at `threshold`, given per present
    /// term its bounds per sub-block and whether it is required there.
    fn plan(&mut self, threshold: f32, subs: &[[f32; SUBS]], sub: usize, required: &[bool]) {
        self.active = false;
        self.by_essential = false;
        self.by_count = false;
        self.essential.clear();
        self.adds.clear();
        let t = f64::from(threshold);
        // Without a positive threshold every lane could reach it. The float
        // error argument above holds for sums of bounds none of which is
        // negative, over any plausible number of terms.
        if !(t > 0.0 && t.is_finite())
            || subs.len() > 4096
            || subs
                .iter()
                .any(|bounds| bounds[sub].is_nan() || bounds[sub] < 0.0)
        {
            return;
        }
        // The essential terms.
        {
            let slack = 1.0 + f64::from(f32::EPSILON) * 256.0;
            let mut fixed = 0.0_f64;
            self.order.clear();
            for (n, bounds) in subs.iter().enumerate() {
                let bound = bounds[sub];
                if required[n] {
                    fixed += f64::from(bound);
                } else if bound > 0.0 {
                    self.order.push((bound, n));
                }
            }
            insertion_sort_by(&mut self.order, |a, b| a.0.total_cmp(&b.0).is_lt());
            let mut tail = fixed;
            let mut inessential = 0;
            for &(bound, _) in &self.order {
                let next = tail + f64::from(bound);
                if next * slack >= t {
                    break;
                }
                tail = next;
                inessential += 1;
            }
            if inessential > 0 {
                self.by_essential = true;
                self.essential
                    .extend(self.order[inessential..].iter().map(|&(_, n)| n));
            }
        }
        // The weights, in units of the threshold.
        {
            let unit = t / f64::from(SIEVE_TARGET);
            let weight = |bound: f32| {
                let units = f64::from(bound) / unit;
                if units < f64::from(SIEVE_TARGET) {
                    // Strictly above the bound: the floor plus one.
                    units.floor() as u32 + 1
                } else {
                    SIEVE_TARGET
                }
            };
            let mut start = 0u32;
            for (n, bounds) in subs.iter().enumerate() {
                let bound = bounds[sub];
                if bound == 0.0 {
                    continue;
                }
                if required[n] {
                    start = start.saturating_add(weight(bound));
                } else {
                    self.adds.push((n, weight(bound)));
                }
            }
            if start < SIEVE_TARGET {
                self.by_count = true;
                self.start = start;
            }
        }
        self.active = self.by_essential || self.by_count;
    }

    /// The lanes of `word` the sieve keeps, given per present term its word.
    #[inline]
    fn keep(&self, word: u64, words: &[u64]) -> u64 {
        if !self.active {
            return word;
        }
        let mut kept = word;
        if self.by_essential {
            let mut any = 0;
            for &n in &self.essential {
                any |= words[n];
            }
            kept &= any;
        }
        if self.by_count && kept != 0 {
            let mut lanes = segment::lanes::LaneSums::new(self.start, SIEVE_TARGET);
            // Every term is added: stopping once every kept lane reached the
            // target measured no faster than the branch it costs per term.
            for &(n, weight) in &self.adds {
                lanes.add(words[n], weight);
            }
            kept &= lanes.reached();
        }
        kept
    }
}

/// Sorts `v` by `less`, stably, by insertion: the walk sorts a few terms
/// per chunk, mostly in order already, where the general sort's setup and
/// partitioning cost more than the comparisons.
#[inline]
fn insertion_sort_by<T: Copy>(v: &mut [T], mut less: impl FnMut(&T, &T) -> bool) {
    for i in 1..v.len() {
        let x = v[i];
        let mut j = i;
        while j > 0 && less(&x, &v[j - 1]) {
            v[j] = v[j - 1];
            j -= 1;
        }
        v[j] = x;
    }
}

/// Removes the dead ordinals of the chunk at `base` from the shared members.
fn drop_dead(
    dead: &[u32],
    base: u32,
    sparse: bool,
    set: &mut segment::ordinals::Words,
    lows: &mut Vec<u16>,
) {
    let from = dead.partition_point(|o| *o < base);
    for dead in &dead[from..] {
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
}

impl OrdinalWalk<'_, '_> {
    /// Reads the candidate `low`'s positions for `slot` into the check's
    /// scratch, unless they are read already.
    fn read_slot(&mut self, phrase: &mut PhraseCheck<'_>, slot: usize, low: u16) {
        if std::mem::replace(&mut phrase.read[slot], true) {
            return;
        }
        let (member, payload) = &mut phrase.slots[slot];
        let term = match *member {
            Member::Term(t) => &mut self.terms[t],
            Member::Filter(f) => &mut self.filters[f],
        };
        let rank = term.rank(low).unwrap_or_else(|| {
            crate::storage::corrupt("Stannum: a phrase candidate is missing a term")
        });
        let positions = &mut phrase.positions[slot];
        positions.clear();
        segment_error(payload.seek(rank));
        segment_error(payload.next_into(positions));
        POSITION_READS.set(POSITION_READS.get() + 1);
    }

    /// Reads the candidate's slots in the plan's order and says whether
    /// every pair the plan tests keeps its distance: rarest slot first,
    /// each leaf tested against the nearest leaf read before it, so a
    /// candidate fails on the fewest and shortest reads.
    fn pairs_keep_distance(&mut self, phrase: &mut PhraseCheck<'_>, low: u16) -> bool {
        let Some(plan) = phrase.plan.take() else {
            return true;
        };
        let mut kept = true;
        for step in plan.steps() {
            self.read_slot(phrase, step.slot, low);
            if let Some(pair) = step.pair
                && !plan.pair_keeps(pair, &phrase.positions)
            {
                kept = false;
                break;
            }
        }
        phrase.plan = Some(plan);
        kept
    }

    /// Whether the candidate `low` of the loaded chunk, at `ordinal`, holds
    /// the phrase. Every slot's term lists the candidate: the walk only
    /// reaches here through the conjunction of them.
    fn phrase_matches(&mut self, low: u16, ordinal: u32) -> bool {
        let Some(mut phrase) = self.phrase.take() else {
            return true;
        };
        POSITION_CHECKS.set(POSITION_CHECKS.get() + 1);
        phrase.read.fill(false);
        if !self.pairs_keep_distance(&mut phrase, low) {
            self.phrase = Some(phrase);
            return false;
        }
        for slot in 0..phrase.slots.len() {
            self.read_slot(&mut phrase, slot, low);
        }
        let matched = match &phrase.filter {
            None => phrase.solver.intervals(&phrase.positions).next().is_some(),
            Some(filter) => {
                let length = if filter.needs_doc_length() {
                    segment_error(self.lengths.get(ordinal))
                } else {
                    0
                };
                phrase
                    .solver
                    .intervals(&phrase.positions)
                    .any(|interval| filter.matches_interval(length, interval))
            }
        };
        self.phrase = Some(phrase);
        matched
    }

    #[inline]
    fn threshold(&self) -> Option<(f32, Tid)> {
        self.bar
    }

    /// Brings the threshold up to date after the heap changed.
    fn raise(&mut self) {
        let real = if self.heap.len() == self.k {
            self.heap.peek().map(|w| (w.0, w.1))
        } else {
            None
        };
        self.bar = match (real, self.seed) {
            (Some(real), Some(seed)) if seed.0 > real.0 => Some(seed),
            (None, seed) => seed,
            (real, _) => real,
        };
    }

    /// Adds `candidate` to the heap, dropping the worst row once it holds k.
    fn push(&mut self, candidate: Ranked) {
        if self.heap.len() == self.k {
            self.heap.pop();
        }
        self.heap.push(candidate);
        self.raise();
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
            // Only the terms stepped last round moved, so the order is
            // nearly sorted.
            let terms = &self.terms;
            insertion_sort_by(&mut order, |&a, &b| {
                terms[a].keys[terms[a].pos] < terms[b].keys[terms[b].pos]
            });
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
            // A mixed shape may need a stream the chunk lacks.
            if self.condition.is_some() {
                self.seek_filters(pivot);
                if let Some(condition) = &self.condition
                    && !self.chunk_may_hold(condition, pivot)
                {
                    for &t in &order[..=p] {
                        self.terms[t].pos += 1;
                    }
                    continue;
                }
            }
            // Every term of the prefix is on the pivot chunk: fold and score it.
            present.clear();
            present.extend(order[..=p].iter().copied());
            insertion_sort_by(&mut present, |a, b| a < b);
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
        // The other terms, rarest first: the order each chunk narrows in.
        let mut others: Vec<usize> = (0..self.terms.len()).filter(|&t| t != lead).collect();
        others.sort_by_key(|&t| self.terms[t].keys.len());
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
            if self.warmed.binary_search(&key).is_ok() {
                // The warm-up evaluated this chunk.
                self.step_all();
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
            self.evaluate_all(key, lead, &others, min_length, &mut set);
            self.step_all();
        }
    }

    /// A conjunction with no scoring term, a phrase of elided words say:
    /// every match scores zero and ranks in heap order, and ordinals ascend
    /// with locations within a source, so the source's share of the top
    /// `k` is its first `k` visible matches in ordinal order. The rarest
    /// filter leads through its chunks as in [`Self::all`]; a chunk every
    /// filter holds is folded to the shared members, and each is checked
    /// for the phrase and admitted in turn, no bound or score consulted.
    /// The source is abandoned at the first ordinal whose location cannot
    /// rank, the heap being full and the location at or past the k-th, as
    /// every later ordinal lies further on. Across sources the heap keeps
    /// the `k` earliest of every source's share, which are the `k` earliest
    /// overall.
    fn all_unscored(&mut self) {
        let lead = (0..self.filters.len())
            .min_by_key(|&f| self.filters[f].keys.len())
            .expect("an unscored conjunction has filters");
        let mut others: Vec<usize> = (0..self.filters.len()).filter(|&f| f != lead).collect();
        others.sort_by_key(|&f| self.filters[f].keys.len());
        let mut set: Box<segment::ordinals::Words> = Box::new([0; segment::ordinals::WORDS]);
        loop {
            self.iterations = self.iterations.wrapping_add(1);
            if self.iterations.is_multiple_of(64) {
                pgrx::check_for_interrupts!();
            }
            let Some(key) = self.filters[lead].key() else {
                return;
            };
            let mut next = key;
            for f in 0..self.filters.len() {
                if f == lead {
                    continue;
                }
                let filter = &mut self.filters[f];
                filter.pos += filter.keys[filter.pos..].partition_point(|k| *k < key);
                match filter.key() {
                    None => return,
                    Some(found) => next = next.max(found),
                }
            }
            if next > key {
                let filter = &mut self.filters[lead];
                filter.pos += filter.keys[filter.pos..].partition_point(|k| *k < next);
                continue;
            }
            let base = u32::from(key) << 16;
            if !self.can_beat(0.0, base) || !self.evaluate_unscored(key, lead, &others, &mut set) {
                return;
            }
            for filter in &mut self.filters {
                filter.pos += 1;
            }
        }
    }

    /// Admits the documents of chunk `key` that every filter holds and the
    /// phrase check passes, in ordinal order. False once a document's
    /// location cannot rank, which ends the source.
    fn evaluate_unscored(
        &mut self,
        key: u16,
        lead: usize,
        others: &[usize],
        set: &mut segment::ordinals::Words,
    ) -> bool {
        let base = u32::from(key) << 16;
        self.filters[lead].load();
        let sparse = !self.filters[lead].dense;
        let mut lows: Vec<u16> = Vec::new();
        if sparse {
            lows.extend_from_slice(&self.filters[lead].members);
        } else {
            self.filters[lead].assign_into(set);
        }
        for &f in others {
            if !narrow(&mut self.filters[f], sparse, set, &mut lows) {
                return true;
            }
        }
        drop_dead(self.dead, base, sparse, set, &mut lows);
        if sparse {
            for low in lows {
                if !self.admit_unscored(base, low) {
                    return false;
                }
            }
        } else {
            for (i, &bits) in set.iter().enumerate() {
                let mut word = bits;
                while word != 0 {
                    let low = (i * 64) as u16 + word.trailing_zeros() as u16;
                    word &= word - 1;
                    if !self.admit_unscored(base, low) {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Admits the unscored candidate `low` of the loaded chunk if it holds
    /// the phrase. False when it cannot rank: the heap is full and its
    /// location is at or past the k-th, as is every later ordinal's.
    fn admit_unscored(&mut self, base: u32, low: u16) -> bool {
        let ordinal = base + u32::from(low);
        if !self.can_beat(0.0, ordinal) {
            return false;
        }
        if self.phrase_matches(low, ordinal) {
            self.admit(0.0, ordinal);
        }
        true
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
        others: &[usize],
        min_length: u32,
        set: &mut segment::ordinals::Words,
    ) {
        let base = u32::from(key) << 16;
        // The chunk's bounds per sub-block (see `sub_bounds_all`), taken
        // once there is a threshold to hold them to: a conjunction of common
        // words that matches rarely walks most of its chunks without one.
        let mut subs = None;
        if self.threshold().is_some() {
            let (sub_scores, sub_empty) = *subs.insert(self.sub_bounds_all(min_length));
            if !(0..SUBS).any(|sub| {
                !sub_empty[sub] && self.can_beat(sub_scores[sub], base + (sub * SUB) as u32)
            }) {
                return;
            }
        }
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
            self.terms[lead].assign_into(set);
        }
        for &t in others {
            if !narrow(&mut self.terms[t], sparse, set, &mut lows) {
                return;
            }
        }
        for filter in &mut self.filters {
            if !narrow(filter, sparse, set, &mut lows) {
                return;
            }
        }
        drop_dead(self.dead, base, sparse, set, &mut lows);
        let mut sparse_at = 0usize;
        let mut pending = std::mem::take(&mut self.pending);
        pending.clear();
        let mut block = [0u64; SUB_WORDS];
        for sub in 0..SUBS {
            // The sub-block's candidates. Most sub-blocks of a conjunction
            // of common words hold none, and are passed over whole: taking
            // the chunk a word at a time, with a sub-block's judgement at
            // every sixteenth, was a sixth of such a walk.
            let end = (sub + 1) * SUB;
            if sparse {
                if sparse_at == lows.len() {
                    break;
                }
                if usize::from(lows[sparse_at]) >= end {
                    continue;
                }
                block.fill(0);
                while let Some(&low) = lows.get(sparse_at)
                    && usize::from(low) < end
                {
                    let low = usize::from(low) % SUB;
                    block[low / 64] |= 1 << (low % 64);
                    sparse_at += 1;
                }
            } else {
                block.copy_from_slice(&set[sub * SUB_WORDS..(sub + 1) * SUB_WORDS]);
                if block.iter().fold(0, |any, word| any | word) == 0 {
                    continue;
                }
            }
            // The threshold moves as candidates are admitted, so it is
            // consulted afresh at every sub-block and candidate: the chunk
            // that fills the top k also prunes the rest of itself.
            if self.threshold().is_some() {
                let (sub_scores, sub_empty) =
                    *subs.get_or_insert_with(|| self.sub_bounds_all(min_length));
                if sub_empty[sub] || !self.can_beat(sub_scores[sub], base + (sub * SUB) as u32) {
                    continue;
                }
            }
            self.class_stamp = self.class_stamp.wrapping_add(1);
            if self.class_stamp == 0 {
                self.class_bounds.fill((0, 0.0));
                self.class_stamp = 1;
            }
            for (w, &bits) in block.iter().enumerate() {
                let mut word = bits;
                while word != 0 {
                    let low = ((sub * SUB_WORDS + w) * 64) as u16 + word.trailing_zeros() as u16;
                    word &= word - 1;
                    let ordinal = base + u32::from(low);
                    let pruning = self.threshold().is_some();
                    if !pruning && self.phrase.is_some() {
                        // Until the top k fill every match enters it, so a
                        // candidate's positions are checked before it is
                        // scored: a phrase of common words that matches
                        // rarely never fills its top k, and scoring every
                        // member of the conjunction first was a sixth of its
                        // walk. Admitted in ordinal order, as
                        // `verify_pending` would admit them.
                        if self.phrase_matches(low, ordinal) {
                            let total = self
                                .score_conjunct(low, ordinal, sub, 0.0, false)
                                .expect("a candidate unpruned is scored");
                            self.admit(total, ordinal);
                        }
                        continue;
                    }
                    let first = if pruning {
                        subs.get_or_insert_with(|| self.sub_bounds_all(min_length))
                            .0[sub]
                    } else {
                        0.0
                    };
                    let Some(total) = self.score_conjunct(low, ordinal, sub, first, pruning) else {
                        continue;
                    };
                    let admit =
                        self.heap.len() < self.k || self.heap.peek().is_some_and(|w| total >= w.0);
                    if !admit {
                        continue;
                    }
                    if self.phrase.is_some() {
                        // A phrase candidate is held until the sub-block is
                        // scored: its positions are read only if it still
                        // ranks.
                        pending.push((total, low));
                        continue;
                    }
                    self.admit(total, ordinal);
                }
            }
            self.verify_pending(&mut pending, base);
        }
        self.pending = pending;
    }

    /// Admits the document at `ordinal`, scoring `total`, to the heap if it
    /// ranks in the top k and is visible.
    fn admit(&mut self, total: f32, ordinal: u32) {
        let tid = self.resolve(ordinal);
        let candidate = Ranked(total, tid);
        if self.heap.len() < self.k {
            if self.visibility.visible(tid) {
                self.push(candidate);
            }
        } else if self.heap.peek().is_some_and(|w| candidate < *w) && self.visibility.visible(tid) {
            self.push(candidate);
        }
    }

    /// Checks a sub-block's phrase candidates, best score first, each
    /// against the threshold as it then stands. A confirmed match raises
    /// the threshold, and the candidates it now outranks are dropped
    /// without reading their positions: checked in ordinal order, a
    /// sub-block of common words read positions for every candidate
    /// scoring above a threshold only its rare matches could raise. The
    /// heap ends the same whichever order admits to it, so the top k is
    /// bit for bit the exhaustive one.
    ///
    /// Until the heap is full nothing is outranked, so the candidates are
    /// checked in ordinal order, which reads each slot's positions
    /// forwards; best first, a phrase that never fills its top k read them
    /// scattered, and a payload cursor moved back re-reads its skip span.
    fn verify_pending(&mut self, pending: &mut Vec<(f32, u16)>, base: u32) {
        let by_score = |a: &(f32, u16), b: &(f32, u16)| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1));
        let mut sorted = false;
        let mut at = 0;
        while at < pending.len() {
            if !sorted && self.threshold().is_some() {
                pending[at..].sort_by(by_score);
                sorted = true;
            }
            let (total, low) = pending[at];
            at += 1;
            let ordinal = base + u32::from(low);
            if self.heap.len() == self.k
                && let Some(worst) = self.heap.peek().map(|w| (w.0, w.1))
            {
                // As `admit` ranks: a better score, or the same from an
                // earlier location.
                match total.total_cmp(&worst.0) {
                    Ordering::Less => continue,
                    Ordering::Equal if self.resolve(ordinal) >= worst.1 => continue,
                    _ => {}
                }
            }
            if !self.phrase_matches(low, ordinal) {
                continue;
            }
            self.admit(total, ordinal);
        }
        pending.clear();
    }

    /// Per sub-block of a conjunction's current chunk, the best a shared
    /// document could score, each term's largest bucket there at the shared
    /// shortest length `min_length` summed in slot order; and whether some
    /// term lacks the sub-block, which then holds no shared document. From
    /// the directory, so a chunk no sub-block of which can reach the
    /// threshold is skipped before any stream is read.
    fn sub_bounds_all(&mut self, min_length: u32) -> ([f32; SUBS], [bool; SUBS]) {
        let mut sub_scores = [0.0_f32; SUBS];
        let mut sub_empty = [false; SUBS];
        for term in &mut self.terms {
            let scorer = &self.scorer.terms[term.slot].1;
            let by_bucket = term.bounds_by_bucket(term.pos, scorer, min_length);
            for ((sub, score), empty) in term.sub_bounds[term.pos]
                .iter()
                .zip(sub_scores.iter_mut())
                .zip(sub_empty.iter_mut())
            {
                if *sub == 0 {
                    *empty = true;
                } else {
                    *score += by_bucket[usize::from(*sub - 1)];
                }
            }
        }
        (sub_scores, sub_empty)
    }

    /// The score of the document `low` of a conjunction's current chunk,
    /// which every term holds, or `None` when it cannot reach the threshold:
    /// [`Self::score_candidate`] over every term, bit for bit, with its
    /// first two bounds taken whole. Every term holds the document, so its
    /// first bound is the sub-block's, `first`, the sum of every term's
    /// sub-block bound in slot order; and its bound at its length class is
    /// the sum of every term's largest sub-block bucket at the class's
    /// shortest length, which within the sub-block depends on the class
    /// alone, so it is computed once per class a sub-block meets rather
    /// than per candidate. Four in five candidates of a conjunction fell to
    /// the class bound, each after testing every term's bit and scoring
    /// every term's largest bucket at its class.
    fn score_conjunct(
        &mut self,
        low: u16,
        ordinal: u32,
        sub: usize,
        first: f32,
        pruning: bool,
    ) -> Option<f32> {
        let class = if pruning {
            if !self.can_beat(first, ordinal) {
                return None;
            }
            let class = segment_error(self.index.length_class(ordinal));
            let (stamp, bound) = self.class_bounds[usize::from(class)];
            let bound = if stamp == self.class_stamp {
                bound
            } else {
                let floor = segment::length_class::min_length(class);
                // Folded as `score_candidate` folds its values: from zero,
                // in slot order.
                let mut bound = 0.0_f32;
                for term in &self.terms {
                    let top = term.sub_bounds[term.pos][sub];
                    bound += if top == 0 {
                        0.0
                    } else {
                        let bucket = TfBucket::new(top - 1).expect("bucket from a chunk bound");
                        self.scorer.terms[term.slot].1.score_bucket(bucket, floor)
                    };
                }
                self.class_bounds[usize::from(class)] = (self.class_stamp, bound);
                bound
            };
            if !self.can_beat(bound, ordinal) {
                return None;
            }
            Some(class)
        } else {
            None
        };
        let mut buckets = std::mem::take(&mut self.buckets);
        buckets.clear();
        for term in &mut self.terms {
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
            buckets.push(TfBucket::new(bucket).unwrap_or_else(|| {
                crate::storage::corrupt(format!(
                    "Stannum index data: term-frequency bucket {bucket} out of range"
                ))
            }));
        }
        let at = |walk: &Self, buckets: &[TfBucket], length: u32| {
            let mut total = 0.0_f32;
            for (term, bucket) in walk.terms.iter().zip(buckets) {
                total += walk.scorer.terms[term.slot].1.score_bucket(*bucket, length);
            }
            total
        };
        if let Some(class) = class
            && !self.can_beat(
                at(self, &buckets, segment::length_class::min_length(class)),
                ordinal,
            )
        {
            self.buckets = buckets;
            return None;
        }
        let length = segment_error(self.lengths.get(ordinal));
        let total = at(self, &buckets, length);
        self.buckets = buckets;
        if pruning && !self.can_beat(total, ordinal) {
            return None;
        }
        *self.scored += 1;
        Some(total)
    }

    /// The score of the document `low` of the current chunk, or `None` when
    /// it cannot reach the threshold. Of the terms `present` (in slot order)
    /// those holding the document are bounded by their sub-block's largest
    /// bucket at the document's length class, which costs no read; the
    /// document's buckets, nibbles of the loaded chunk, then tighten the
    /// bound at the class, and its length is read only when that still
    /// reaches the threshold. Every sum is folded in slot order, as the
    /// exhaustive path folds the total, so the bound of a fully read
    /// candidate is its exact score, bit for bit.
    fn score_candidate(
        &mut self,
        present: &[usize],
        low: u16,
        ordinal: u32,
        sub: usize,
        pruning: bool,
    ) -> Option<f32> {
        // The scratch vectors are the walk's: a candidate is scored a
        // hundred thousand times a query, and two allocations each showed.
        let mut values = std::mem::take(&mut self.values);
        let mut uppers = std::mem::take(&mut self.uppers);
        let mut buckets = std::mem::take(&mut self.buckets);
        values.clear();
        values.resize(present.len(), 0.0);
        uppers.clear();
        buckets.clear();
        buckets.resize(present.len(), TfBucket::from_count(0));
        let score = self.score_candidate_in(
            present,
            low,
            ordinal,
            sub,
            pruning,
            &mut values,
            &mut uppers,
            &mut buckets,
        );
        self.values = values;
        self.uppers = uppers;
        self.buckets = buckets;
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
        values: &mut [f32],
        uppers: &mut Vec<(f32, usize)>,
        buckets: &mut [TfBucket],
    ) -> Option<f32> {
        // Per term of `present`: its bound, replaced by its exact score once
        // read; and the terms holding the document, in slot order.
        // First at the shortest document the bounds allow, which costs no
        // read: the length table is one page per 2,048 documents and
        // candidates are scattered, so reading a length before the bound
        // rejects the candidate was a page per candidate.
        for (n, &t) in present.iter().enumerate() {
            let term = &self.terms[t];
            if !term.holds(low) {
                continue;
            }
            // The chunk's per-term bound for this sub-block, computed once
            // per chunk by the caller; it was a table copy per candidate.
            let upper = self.term_subs[n][sub];
            values[n] = upper;
            uppers.push((upper, n));
        }
        let fold = |values: &[f32]| values.iter().fold(0.0_f32, |sum, v| sum + v);
        if pruning && !self.can_beat(fold(values), ordinal) {
            return None;
        }
        // The document's own length tightens every bound. Its class is a
        // byte per document and a lower bound on the length, so the bounds
        // are tightened at the class first. The exact length, four bytes per
        // document in a table a walk touches once per scattered candidate,
        // is read last: the candidate's buckets are nibbles of the chunk
        // already loaded, so they are read first and bounded at the class
        // before the length is. Three in four candidates fell to the class
        // bound and nearly every survivor read its length, only to lose to
        // the threshold once scored; bounding exact buckets at the class
        // rejects most of them without the read.
        let class = if pruning {
            let class = segment_error(self.index.length_class(ordinal));
            let floor = segment::length_class::min_length(class);
            for &(_, n) in uppers.iter() {
                let term = &self.terms[present[n]];
                let scorer = &self.scorer.terms[term.slot].1;
                let top = term.sub_bounds[term.pos][sub];
                // The sub-block's largest bucket is a member's, so the score
                // at that bucket and this length bounds every member: no
                // sweep of the chunk's buckets tightens it.
                values[n] = if top == 0 {
                    0.0
                } else {
                    let bucket = TfBucket::new(top - 1).expect("bucket from a chunk bound");
                    scorer.score_bucket(bucket, floor)
                };
            }
            if !self.can_beat(fold(values), ordinal) {
                return None;
            }
            Some(class)
        } else {
            None
        };
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
            buckets[n] = TfBucket::new(bucket).unwrap_or_else(|| {
                crate::storage::corrupt(format!(
                    "Stannum index data: term-frequency bucket {bucket} out of range"
                ))
            });
        }
        if let Some(class) = class {
            let floor = segment::length_class::min_length(class);
            for &(_, n) in uppers.iter() {
                let slot = self.terms[present[n]].slot;
                values[n] = self.scorer.terms[slot].1.score_bucket(buckets[n], floor);
            }
            if !self.can_beat(fold(values), ordinal) {
                return None;
            }
        }
        let length = segment_error(self.lengths.get(ordinal));
        for &(_, n) in uppers.iter() {
            let slot = self.terms[present[n]].slot;
            values[n] = self.scorer.terms[slot].1.score_bucket(buckets[n], length);
        }
        if pruning && !self.can_beat(fold(values), ordinal) {
            return None;
        }
        *self.scored += 1;
        Some(fold(values))
    }

    fn evaluate(&mut self, key: u16, present: &[usize], set: &mut segment::ordinals::Words) {
        let base = u32::from(key) << 16;
        // Per sub-block, the best a candidate could score: the sum over the
        // present terms of their largest bucket there at that bucket's
        // shortest document. The bounds come from the directory, parsed when
        // the term opened, so a chunk no sub-block of which can reach the
        // threshold is skipped before any of its members are read: at 150
        // million rows a three-term disjunction loaded every chunk of every
        // term for 2,300 candidates.
        let mut sub_scores = [0.0_f32; SUBS];
        // Per present term, its bound per sub-block: a member's first bound
        // is the sum over the terms holding it, taken inline below before
        // any call, since most members of a common term's chunk fail it.
        self.term_subs.clear();
        for &t in present {
            let term = &mut self.terms[t];
            let scorer = &self.scorer.terms[term.slot].1;
            let by_bucket = term.bounds_by_bucket(term.pos, scorer, 0);
            let mut mine = [0.0_f32; SUBS];
            for (i, (sub, score)) in term.sub_bounds[term.pos]
                .iter()
                .zip(sub_scores.iter_mut())
                .enumerate()
            {
                if *sub > 0 {
                    let bound = by_bucket[usize::from(*sub - 1)];
                    *score += bound;
                    mine[i] = bound;
                }
            }
            self.term_subs.push(mine);
        }
        if self.threshold().is_some()
            && !(0..SUBS).any(|sub| self.can_beat(sub_scores[sub], base + (sub * SUB) as u32))
        {
            return;
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
        insertion_sort_by(&mut by_bound, |a, b| b.0.total_cmp(&a.0).is_lt());
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
        // Only the essential terms' chunks are loaded here: they form the
        // candidate union. A required term's chunk is loaded when its words
        // are ANDed in, and any other present term's only when a candidate
        // survives the bounds without it; over common words the top k fill
        // early and most chunks are settled by the required terms alone, so
        // the other terms' chunks are never read.
        for (_, t) in &by_bound[..essential] {
            self.terms[*t].load();
        }
        // One array's members are the candidates as they stand; the union
        // of several is taken as words, where sorting their members was a
        // sort of thousands of ordinals per chunk.
        let sparse = essential == 1 && !self.terms[by_bound[0].1].dense;
        let mut lows: Vec<u16> = Vec::new();
        if sparse {
            lows.extend_from_slice(&self.terms[by_bound[0].1].members);
        } else {
            set.fill(0);
            for (_, t) in &by_bound[..essential] {
                self.terms[*t].or_into(set);
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
        let mut sparse_at = 0usize;
        // A sub-block is judged once, at its first word: the walk resolves
        // locations in ordinal order, so the judgement cannot be repeated
        // after a candidate of the sub-block has been resolved.
        let mut skip_sub = false;
        // The sub-block's candidate words, with the required terms ANDed in.
        let mut block = [0u64; SUB_WORDS];
        // Per present term, whether it is required in the sub-block, and the
        // threshold that decided it.
        let mut required = vec![false; present.len()];
        let mut required_at = None;
        let mut settled = false;
        // The sub-block's word sieve, planned at its first surviving word
        // and again whenever the threshold moves; and per present term its
        // word at hand.
        let mut sieve = WordSieve::default();
        let mut planned = false;
        let mut words = vec![0u64; present.len()];
        for i in 0..segment::ordinals::WORDS {
            let sub = i / SUB_WORDS;
            let w = i % SUB_WORDS;
            if w == 0 {
                // The threshold moves as candidates are admitted, so it is
                // consulted afresh at every sub-block and candidate: the
                // chunk that fills the top k also prunes the rest of itself.
                let pruning = self.threshold().is_some();
                skip_sub = pruning && !self.can_beat(sub_scores[sub], base + (sub * SUB) as u32);
                if sparse {
                    block.fill(0);
                    while sparse_at < lows.len() && usize::from(lows[sparse_at]) < (sub + 1) * SUB {
                        let low = usize::from(lows[sparse_at]) % SUB;
                        block[low / 64] |= 1 << (low % 64);
                        sparse_at += 1;
                    }
                } else {
                    block.copy_from_slice(&set[i..i + SUB_WORDS]);
                }
                required.fill(false);
                required_at = None;
                planned = false;
            }
            if skip_sub {
                continue;
            }
            // A term the sub-block's other bounds cannot reach the threshold
            // without is required: the words keep only the members it lists.
            // A sub-block is settled by one AND of sixteen words per term,
            // where bounding each member against every term was most of a
            // walk over common words: once the top k fill, nearly every
            // term of such a query is required, and the survivors are the
            // few documents holding them all. The threshold only rises, so
            // a term once required stays so; when the threshold moves, the
            // terms it newly requires are ANDed into the words still ahead.
            // A required term's chunk is loaded here, at its first AND.
            if let Some((threshold, holder)) = self.threshold()
                && required_at != Some((threshold, holder))
            {
                required_at = Some((threshold, holder));
                planned = false;
                let all = f64::from(sub_scores[sub]);
                let slack = 1.0 + f64::from(f32::EPSILON) * 256.0;
                for (n, &t) in present.iter().enumerate() {
                    if !required[n]
                        && (all - f64::from(self.term_subs[n][sub])) * slack < f64::from(threshold)
                    {
                        required[n] = true;
                        let term = &mut self.terms[t];
                        term.load();
                        term.and_sub(sub, w, &mut block);
                    }
                }
            }
            let mut word = block[w];
            if word == 0 {
                continue;
            }
            // A candidate is bounded and scored by every present term's
            // bits, so the terms not loaded yet are loaded at the chunk's
            // first surviving word; a chunk the required terms empty never
            // reads them.
            if !settled {
                for &t in present {
                    self.terms[t].load();
                }
                if self.condition.is_some() {
                    for filter in &mut self.filters {
                        if filter.key() == Some(key) {
                            filter.load();
                        }
                    }
                }
                settled = true;
            }
            for (out, &t) in words.iter_mut().zip(present) {
                *out = self.terms[t].word(i);
            }
            // Lanes the sieve clears cannot reach the threshold, so the
            // member-by-member bound below would reject each of them.
            if let Some((threshold, _)) = required_at {
                if !planned {
                    sieve.plan(threshold, &self.term_subs, sub, &required);
                    planned = true;
                }
                let kept = sieve.keep(word, &words);
                word = kept;
            }
            // A mixed shape keeps the documents that may meet it; those it
            // does not settle here are tested once they would rank.
            let mut certain = !0u64;
            if word != 0
                && let Some(condition) = &self.condition
            {
                let (sure, may) = self.condition_word(condition, key, i);
                word &= may;
                certain = sure;
            }
            while word != 0 {
                let low = (i * 64) as u16 + word.trailing_zeros() as u16;
                word &= word - 1;
                let ordinal = base + u32::from(low);
                let pruning = self.threshold().is_some();
                if pruning {
                    let bit = 1u64 << (low % 64);
                    let mut first = 0.0_f32;
                    for (n, held) in words.iter().enumerate() {
                        if held & bit != 0 {
                            first += self.term_subs[n][sub];
                        }
                    }
                    if !self.can_beat(first, ordinal) {
                        continue;
                    }
                }
                let Some(total) = self.score_candidate(present, low, ordinal, sub, pruning) else {
                    continue;
                };
                let admit =
                    self.heap.len() < self.k || self.heap.peek().is_some_and(|w| total >= w.0);
                if !admit {
                    continue;
                }
                let tid = self.resolve(ordinal);
                let candidate = Ranked(total, tid);
                if certain & (1u64 << (low % 64)) == 0
                    && (self.heap.len() == self.k
                        && self.heap.peek().is_none_or(|w| candidate >= *w)
                        || !self.mixed_holds(key, low, ordinal))
                {
                    continue;
                }
                if self.heap.len() < self.k {
                    if self.visibility.visible(tid) {
                        self.push(candidate);
                    }
                } else if self.heap.peek().is_some_and(|w| candidate < *w)
                    && self.visibility.visible(tid)
                {
                    self.push(candidate);
                }
            }
        }
    }
}

/// A span of [`Index::hold`] over one walk: opened before the walk reads a
/// length or class, closed when the walk ends, by return or by unwind, so
/// the pages it held pinned never outlive the statement.
struct HeldPages<'a>(&'a dyn Index);

impl<'a> HeldPages<'a> {
    fn open(index: &'a dyn Index) -> Self {
        index.hold(true);
        Self(index)
    }
}

impl Drop for HeldPages<'_> {
    fn drop(&mut self) {
        self.0.hold(false);
    }
}

/// The disjunction walk's test of a mixed shape (see [`Shape`]).
impl<'a> OrdinalWalk<'a, '_> {
    /// The stream `bits` names, if it has members in chunk `key`. A walked
    /// term on the chunk is one of the disjunction's present terms, and a
    /// filter is on it once [`Self::seek_filters`] moved it there.
    fn stream(&self, bits: Bits, key: u16) -> Option<&OrdinalTerm<'a>> {
        let term = match bits {
            Bits::Term(t) => &self.terms[t],
            Bits::Filter(f) => &self.filters[f],
            Bits::Absent => return None,
        };
        (term.key() == Some(key)).then_some(term)
    }

    /// Moves every filter to chunk `key` or past it.
    fn seek_filters(&mut self, key: u16) {
        for filter in &mut self.filters {
            filter.pos += filter.keys[filter.pos..].partition_point(|k| *k < key);
        }
    }

    /// Whether some document of chunk `key` may meet `condition`, by which
    /// streams have members there.
    fn chunk_may_hold(&self, condition: &Condition, key: u16) -> bool {
        match condition {
            Condition::Leaf(bits) => self.stream(*bits, key).is_some(),
            Condition::Phrase(slots, _) => slots.iter().all(|b| self.stream(*b, key).is_some()),
            Condition::All(children) => children.iter().all(|c| self.chunk_may_hold(c, key)),
            Condition::Any { min, children } => {
                children
                    .iter()
                    .filter(|c| self.chunk_may_hold(c, key))
                    .take(*min)
                    .count()
                    == *min
            }
            // A term on the chunk need not be in every document of it.
            Condition::Not(_) => true,
        }
    }

    /// Of word `i` of chunk `key`, the documents that certainly meet
    /// `condition` and those that may: a phrase's are only possible until
    /// its positions are read. Every stream on the chunk is loaded.
    fn condition_word(&self, condition: &Condition, key: u16, i: usize) -> (u64, u64) {
        let word = |bits: Bits| self.stream(bits, key).map_or(0, |term| term.word(i));
        match condition {
            Condition::Leaf(bits) => {
                let word = word(*bits);
                (word, word)
            }
            Condition::Phrase(slots, _) => (0, slots.iter().fold(!0, |all, b| all & word(*b))),
            Condition::All(children) => children.iter().fold((!0, !0), |(sure, may), child| {
                let (s, m) = self.condition_word(child, key, i);
                (sure & s, may & m)
            }),
            Condition::Any { min: 1, children } => {
                children.iter().fold((0, 0), |(sure, may), child| {
                    let (s, m) = self.condition_word(child, key, i);
                    (sure | s, may | m)
                })
            }
            Condition::Any { min, children } => {
                // Per lane, whether at least `j + 1` children hold it, for
                // each `j` below `min`.
                let mut sure = [0u64; MAX_AT_LEAST];
                let mut may = [0u64; MAX_AT_LEAST];
                for child in children {
                    let (s, m) = self.condition_word(child, key, i);
                    for j in (1..*min).rev() {
                        sure[j] |= sure[j - 1] & s;
                        may[j] |= may[j - 1] & m;
                    }
                    sure[0] |= s;
                    may[0] |= m;
                }
                (sure[min - 1], may[min - 1])
            }
            Condition::Not(inner) => {
                let (sure, may) = self.condition_word(inner, key, i);
                (!may, !sure)
            }
        }
    }

    /// Whether the document `low` of chunk `key`, at `ordinal`, meets
    /// `condition`, reading a phrase's positions only where its slots'
    /// terms all hold the document and the rest does not settle it.
    fn condition_holds(&mut self, condition: &Condition, key: u16, low: u16, ordinal: u32) -> bool {
        let holds = |walk: &Self, bits: Bits| walk.stream(bits, key).is_some_and(|t| t.holds(low));
        match condition {
            Condition::Leaf(bits) => holds(self, *bits),
            Condition::Phrase(slots, n) => {
                if !slots.iter().all(|b| holds(self, *b)) {
                    return false;
                }
                self.phrase = self.phrases[*n].take();
                let matched = self.phrase_matches(low, ordinal);
                self.phrases[*n] = self.phrase.take();
                matched
            }
            Condition::All(children) => children
                .iter()
                .all(|child| self.condition_holds(child, key, low, ordinal)),
            Condition::Any { min, children } => {
                let mut held = 0;
                for (n, child) in children.iter().enumerate() {
                    if children.len() - n < min - held {
                        return false;
                    }
                    if self.condition_holds(child, key, low, ordinal) {
                        held += 1;
                        if held == *min {
                            return true;
                        }
                    }
                }
                false
            }
            Condition::Not(inner) => !self.condition_holds(inner, key, low, ordinal),
        }
    }

    /// Whether the document `low` of chunk `key` meets the walk's mixed
    /// shape.
    fn mixed_holds(&mut self, key: u16, low: u16, ordinal: u32) -> bool {
        let Some(condition) = self.condition.take() else {
            return true;
        };
        let holds = self.condition_holds(&condition, key, low, ordinal);
        self.condition = Some(condition);
        holds
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
