// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A dumped index queried as the extension queries it, without a server:
//! the ranked `stannum.score` top k and the count, through the engine
//! crate's walk and fold, over readers of paged sources.
//!
//! Every row is taken as visible: there is no heap, so the only deletions
//! seen are the dead lists VACUUM published, which the dump holds. The
//! write buffer is not dumped; an index whose buffer holds documents
//! scores with other statistics than the server would.

use std::cell::RefCell;
use std::collections::BinaryHeap;
use std::rc::Rc;

use engine::bm25::{Bm25Params, DenseRatio, ScoreStopWords, TermScorer, TermSetEdit};
use engine::terms::{ScoringError, ScoringPolicy};
use engine::walk::{PRUNE_MAX_K, Ranked, Scorer, Source, TopK, WalkConfig, rank};
use rustc_hash::FxHashMap;
use segment::Tid;
use segment::dead::DeadDocs;
use segment::dictionary::TermEntry;
use segment::docs::{DocCursor, DocTable, PageTable};
use segment::index::{Expanded, Index, Window};
use segment::segment::{Lengths, Reader, Term};
use segment::set::Cursor as _;
use segment::tf_bucket::TfBucket;
use tinql::runtime::plan::{Limits, plan};
use tinql::runtime::{Query, parse_tinql_to_query, parse_tinql_to_scoring_query};
use tokenizer::CompiledTokenizerPipeline;

use crate::dump::Dump;
use crate::paged::PagedSource;

/// The extension's `stannum.warmup_chunks` and `stannum.warmup_min_matches`
/// defaults.
pub const DEFAULT_CONFIG: WalkConfig = WalkConfig {
    warmup_chunks: 256,
    warmup_min_matches: 4.0,
    seed: None,
};

/// Dictionary lookups a memo holds before it is emptied, as the
/// extension's per-segment memo.
const TERM_MEMO_LIMIT: usize = 4096;

/// A segment reader with the extension's memo of dictionary lookups.
pub struct Memoized {
    reader: Reader<PagedSource>,
    terms: RefCell<FxHashMap<String, Option<TermEntry>>>,
}

impl Index for Memoized {
    fn document_count(&self) -> u32 {
        self.reader.document_count()
    }
    fn total_length(&self) -> u64 {
        self.reader.total_length()
    }
    fn term(&self, term: &str) -> segment::Result<Option<Term<'_>>> {
        if let Some(entry) = self.terms.borrow().get(term) {
            return entry.map(|entry| self.reader.resolve(entry)).transpose();
        }
        let found = self.reader.term(term)?;
        let mut memo = self.terms.borrow_mut();
        if memo.len() >= TERM_MEMO_LIMIT {
            memo.clear();
        }
        memo.insert(term.to_owned(), found.as_ref().map(|found| found.entry));
        Ok(found)
    }
    fn expand(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
        limit: usize,
    ) -> segment::Result<Expanded<'_>> {
        Index::expand(&self.reader, window, filter, limit)
    }
    fn documents(&self) -> segment::Result<DocCursor<'_>> {
        self.reader.documents()
    }
    fn doc_table(&self) -> segment::Result<DocTable<'_>> {
        self.reader.doc_table()
    }
    fn page_table(&self) -> segment::Result<PageTable<'_>> {
        self.reader.page_table()
    }
    fn lengths(&self) -> Lengths<'_> {
        self.reader.lengths()
    }
    fn length_class(&self, ordinal: u32) -> segment::Result<u8> {
        self.reader.length_class(ordinal)
    }
    fn hold(&self, open: bool) {
        self.reader.hold(open);
    }
}

/// A match: its source, its ordinal there and its heap location.
pub type Candidate = (usize, u32, Tid);

/// How a ranked query was answered, as the extension's custom scan would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    /// The ordinal walk, pruned.
    Walk,
    /// The walk, with its positive scores fewer than k: the rest are matches
    /// of elided terms alone, in heap order.
    ZeroFill,
    /// No term scores: the first k matches in heap order.
    Unscored,
    /// The walk cannot prune the query: every candidate scored.
    Streamed,
}

impl Path {
    pub fn name(self) -> &'static str {
        match self {
            Path::Walk => "walk",
            Path::ZeroFill => "zero-fill",
            Path::Unscored => "unscored",
            Path::Streamed => "streamed",
        }
    }
}

/// A ranked query's answer.
pub struct RankedAnswer {
    pub rows: Vec<(f32, Tid)>,
    pub path: Path,
    /// Candidates scored exactly.
    pub scored: usize,
    /// The scoring terms, as the walk's events name them by slot.
    pub terms: Vec<(String, TermScorer)>,
}

/// An index opened for queries on one thread: a reader per segment, with
/// their dead documents.
pub struct Engine<'d> {
    pub dump: &'d Dump,
    pub tokenizer: CompiledTokenizerPipeline,
    pub segments: Vec<Memoized>,
    pub dead: Vec<DeadDocs>,
    labels: Vec<String>,
    /// The segments' cache keys for the walk's parsed bounds.
    keys: Vec<(u64, u32)>,
    pub config: WalkConfig,
    pub params: Bm25Params,
    pub dense: DenseRatio,
}

thread_local! {
    /// Engines opened on this thread, so each one's bounds cache keys are
    /// its own.
    static OPENED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl<'d> Engine<'d> {
    pub fn open(dump: &'d Dump) -> Result<Self, String> {
        let identity = OPENED.get() + 1;
        OPENED.set(identity);
        let mut segments = Vec::new();
        let mut dead = Vec::new();
        let mut labels = Vec::new();
        let mut keys = Vec::new();
        for (n, dumped) in dump.segments.iter().enumerate() {
            let label = format!("segment generation {}", dumped.generation);
            let source = PagedSource::new(n as u32, dumped.blob.clone(), dumped.areas.clone());
            let reader = Reader::new(source).map_err(|error| format!("{label}: {error}"))?;
            dead.push(match &dumped.dead {
                Some(list) => {
                    crate::paged::note_pages(
                        u32::MAX - n as u32,
                        list.len().div_ceil(crate::paged::PAGE_DATA) as u32,
                        crate::areas::Area::DeadList,
                    );
                    DeadDocs::decode(list, reader.document_count())
                        .map_err(|error| format!("{label} dead list: {error}"))?
                }
                None => DeadDocs::default(),
            });
            segments.push(Memoized {
                reader,
                terms: RefCell::default(),
            });
            labels.push(label);
            keys.push((identity, dumped.generation));
        }
        Ok(Self {
            dump,
            tokenizer: dump.tokenizer()?,
            segments,
            dead,
            labels,
            keys,
            config: DEFAULT_CONFIG,
            params: dump.params,
            dense: DenseRatio::new(Some(DenseRatio::DEFAULT)),
        })
    }

    fn indexes(&self) -> Vec<&dyn Index> {
        self.segments.iter().map(|s| s as &dyn Index).collect()
    }

    fn sources(&self) -> Vec<Source<'_>> {
        (0..self.segments.len())
            .map(|i| Source {
                index: &self.segments[i],
                label: &self.labels[i],
                dead: &self.dead[i],
                key: Some(self.keys[i]),
                native: None,
            })
            .collect()
    }

    /// The source a walk event names by its index's address.
    pub fn source_of(&self, index: *const ()) -> Option<usize> {
        self.segments
            .iter()
            .position(|s| std::ptr::from_ref(s).cast::<()>() == index)
    }

    pub fn parse(&self, text: &str) -> Result<Query, String> {
        parse_tinql_to_query(text, &self.tokenizer).map_err(|error| error.to_string())
    }

    /// Every match of `query` in heap order, distinct, dead documents left
    /// out; and whether the plan was exact (no recheck needed).
    pub fn candidates(&self, query: &Query) -> Result<(Vec<Candidate>, bool), String> {
        let limits = Limits::default();
        let mut exact = true;
        let mut found = Vec::new();
        for (i, segment) in self.segments.iter().enumerate() {
            let planned = plan(query, segment as &dyn Index, &limits)
                .map_err(|error| format!("query plan: {error}"))?;
            exact &= planned.exact;
            let docs = segment.doc_table().map_err(|error| error.to_string())?;
            let mut cursor = planned.cursor;
            while let Some(tid) = cursor.current() {
                let ordinal = docs
                    .ordinal_of(tid)
                    .map_err(|error| error.to_string())?
                    .ok_or("a candidate missing from its document table")?;
                if !self.dead[i].contains(ordinal) {
                    found.push((i, ordinal, tid));
                }
                cursor.advance().map_err(|error| error.to_string())?;
            }
        }
        found.sort_by_key(|(_, _, tid)| *tid);
        found.dedup_by_key(|(_, _, tid)| *tid);
        Ok((found, exact))
    }

    /// The first `n` matches of `query` in heap order that `skip` does not
    /// name, dead documents left out: the candidate stream's first rows, read
    /// as far as needed, as the custom scan reads them.
    pub fn first_matches(
        &self,
        query: &Query,
        n: usize,
        skip: &rustc_hash::FxHashSet<Tid>,
    ) -> Result<Vec<Tid>, String> {
        let limits = Limits::default();
        let error = |error: segment::Error| error.to_string();
        let mut cursors = Vec::new();
        for segment in &self.segments {
            let planned = plan(query, segment as &dyn Index, &limits)
                .map_err(|error| format!("query plan: {error}"))?;
            if !planned.exact {
                return Err("the plan needs a recheck".into());
            }
            cursors.push((planned.cursor, segment.doc_table().map_err(error)?));
        }
        let mut out: Vec<Tid> = Vec::with_capacity(n);
        while out.len() < n {
            // The least location any source holds next.
            let Some(at) = (0..cursors.len())
                .filter(|&i| cursors[i].0.current().is_some())
                .min_by_key(|&i| cursors[i].0.current())
            else {
                break;
            };
            let tid = cursors[at].0.current().expect("checked");
            let ordinal = cursors[at]
                .1
                .ordinal_of(tid)
                .map_err(error)?
                .ok_or("a match missing from its document table")?;
            if !self.dead[at].contains(ordinal) && !skip.contains(&tid) && out.last() != Some(&tid)
            {
                out.push(tid);
            }
            cursors[at].0.advance().map_err(error)?;
        }
        Ok(out)
    }

    /// `SELECT count(*) ... WHERE body ==> text`: the ordinal fold for a
    /// Boolean combination of terms, else the distinct candidates.
    pub fn count(&self, text: &str) -> Result<u64, String> {
        let query = self.parse(text)?;
        if engine::fold::supported(&query) {
            let visibility = engine::fold::Visibility::all_visible();
            let mut total = 0;
            let mut folded = true;
            for (i, segment) in self.segments.iter().enumerate() {
                match engine::fold::count_segment(
                    segment,
                    &self.dead[i],
                    &query,
                    &visibility,
                    |_, _| {},
                )
                .map_err(|error| error.to_string())?
                {
                    Some(count) => total += count,
                    None => folded = false,
                }
            }
            if folded {
                return Ok(total);
            }
        }
        let (candidates, exact) = self.candidates(&query)?;
        if !exact {
            return Err("the plan needs a recheck against row text, which a dump lacks".into());
        }
        Ok(candidates.len() as u64)
    }

    /// The scorer `stannum.score(ctid)` ranks `text` by: dense terms elided
    /// at the default ratio, the index's BM25 parameters.
    pub fn scorer(&self, text: &str) -> Result<Scorer, String> {
        let query = self.parse(text)?;
        let scoring = parse_tinql_to_scoring_query(text, &self.tokenizer)
            .map_err(|error| error.to_string())?;
        let edit = TermSetEdit::from_bound_arrays(None, None).map_err(|error| error.to_string())?;
        let stop = self
            .dump
            .stop_words
            .as_deref()
            .and_then(ScoreStopWords::from_csv);
        let policy = ScoringPolicy {
            params: self.params,
            full: false,
            dense: self.dense,
            edit: &edit,
            stop: stop.as_ref(),
            max_expansion_terms: 65_536,
        };
        let terms =
            engine::terms::term_scorers(&scoring, &self.indexes(), self.segments.len(), &policy)
                .map_err(|error| match error {
                    ScoringError::TooManyTerms(limit) => {
                        format!("expands to more than {limit} terms")
                    }
                    ScoringError::Parameters(error) => error.to_string(),
                })?;
        Ok(Scorer { terms, query })
    }

    /// `SELECT id, stannum.score(ctid) ... ORDER BY score DESC LIMIT k`, as
    /// the extension's ranked custom scan answers it.
    pub fn ranked(&self, text: &str, k: usize) -> Result<RankedAnswer, String> {
        let scorer = self.scorer(text)?;
        if k > PRUNE_MAX_K {
            return Err("k beyond the pruning cap".into());
        }
        if scorer.scores_nothing() && !scorer.walks_unscored() {
            let rows = self
                .first_matches(&scorer.query, k, &rustc_hash::FxHashSet::default())?
                .into_iter()
                .map(|tid| (0.0, tid))
                .collect();
            return Ok(RankedAnswer {
                rows,
                path: Path::Unscored,
                scored: 0,
                terms: scorer.terms,
            });
        }
        let sources = self.sources();
        let top: Option<TopK> =
            scorer.top_k(&sources, k, false, &self.config, |_| AllVisible, |_| true);
        drop(sources);
        let Some(mut top) = top else {
            return self.streamed(scorer, k);
        };
        let mut path = Path::Walk;
        if top.zero_fill {
            path = Path::ZeroFill;
            let positive: rustc_hash::FxHashSet<Tid> = top.rows.iter().map(|(_, t)| *t).collect();
            let rest = self.first_matches(&scorer.query, k - top.rows.len(), &positive)?;
            top.rows.extend(rest.into_iter().map(|tid| (0.0, tid)));
        }
        Ok(RankedAnswer {
            rows: top.rows,
            path,
            scored: top.scored,
            terms: scorer.terms,
        })
    }

    /// Scores every candidate and keeps the best `k`, as
    /// `IndexScorer::top_k_streamed` does.
    fn streamed(&self, scorer: Scorer, k: usize) -> Result<RankedAnswer, String> {
        let (candidates, exact) = self.candidates(&scorer.query)?;
        if !exact {
            return Err("the plan needs a recheck".into());
        }
        let mut readers: Vec<SourceScorer<'_>> = self
            .segments
            .iter()
            .map(|segment| SourceScorer::new(segment, scorer.terms.len()))
            .collect();
        let mut heap = BinaryHeap::with_capacity(k + 1);
        for &(i, ordinal, tid) in &candidates {
            let score = readers[i].score(&scorer.terms, ordinal)?.unwrap_or(0.0);
            let entry = Ranked(score, tid);
            if heap.len() < k {
                heap.push(entry);
            } else if heap.peek().is_some_and(|worst| entry < *worst) {
                heap.pop();
                heap.push(entry);
            }
        }
        let mut rows: Vec<(f32, Tid)> = heap.into_iter().map(|Ranked(s, t)| (s, t)).collect();
        rows.sort_by(rank);
        Ok(RankedAnswer {
            rows,
            path: Path::Streamed,
            scored: candidates.len(),
            terms: scorer.terms,
        })
    }
}

/// Every row visible: a dump has no heap.
struct AllVisible;

impl engine::walk::Visibility for AllVisible {
    fn visible(&mut self, _tid: Tid) -> bool {
        true
    }
}

/// One segment's lookups for scoring a document by ordinal, as the
/// extension's `SourceReader` scores a row by location.
struct SourceScorer<'a> {
    segment: &'a Memoized,
    lengths: Option<Lengths<'a>>,
    terms: Vec<Option<Option<segment::ordinals::OrdinalCursor<'a>>>>,
}

impl<'a> SourceScorer<'a> {
    fn new(segment: &'a Memoized, terms: usize) -> Self {
        Self {
            segment,
            lengths: None,
            terms: (0..terms).map(|_| None).collect(),
        }
    }

    /// The document's score, `None` when it holds no scoring term here.
    fn score(
        &mut self,
        terms: &[(String, TermScorer)],
        ordinal: u32,
    ) -> Result<Option<f32>, String> {
        let error = |error: segment::Error| error.to_string();
        let mut buckets = Vec::with_capacity(terms.len());
        for (n, (name, _)) in terms.iter().enumerate() {
            let segment = self.segment;
            let cursor = match &mut self.terms[n] {
                Some(cursor) => cursor,
                slot => slot.insert(match segment.term(name).map_err(error)? {
                    Some(term) => Some(term.ordinals().and_then(|s| s.cursor()).map_err(error)?),
                    None => None,
                }),
            };
            let bucket = match cursor {
                None => None,
                Some(cursor) => {
                    if cursor.current().is_none_or(|current| current > ordinal) {
                        cursor.rewind().map_err(error)?;
                    }
                    cursor.seek(ordinal).map_err(error)?;
                    (cursor.current() == Some(ordinal))
                        .then(|| cursor.bucket().ok_or("a member without a bucket"))
                        .transpose()?
                }
            };
            buckets.push(bucket);
        }
        if buckets.iter().all(Option::is_none) {
            return Ok(None);
        }
        let segment = self.segment;
        let length = self
            .lengths
            .get_or_insert_with(|| segment.lengths())
            .get(ordinal)
            .map_err(error)?;
        let mut total = 0.0_f32;
        for ((_, scorer), bucket) in terms.iter().zip(&buckets) {
            if let Some(bucket) = *bucket {
                let bucket = TfBucket::new(bucket).ok_or("a bucket out of range")?;
                total += scorer.score_bucket(bucket, length);
            }
        }
        Ok(Some(total))
    }
}

/// Collects the walk's sub-block events of one query (see
/// [`engine::walk::stats`]).
#[derive(Clone, Debug)]
pub struct SubEvent {
    pub index: usize,
    pub key: u16,
    pub sub: usize,
    pub slots: Vec<usize>,
    pub min_length: Option<u32>,
    pub bound: f32,
    pub threshold: Option<f32>,
    pub pruned: bool,
    pub scored: usize,
    pub examined: u64,
}

/// Starts collecting the walk's sub-block events on this thread.
pub fn collect_events() -> Rc<RefCell<Vec<SubEvent>>> {
    let events = Rc::new(RefCell::new(Vec::new()));
    let sink = events.clone();
    engine::walk::stats::observe(Some(Box::new(move |event| {
        sink.borrow_mut().push(SubEvent {
            index: event.index as usize,
            key: event.key,
            sub: event.sub,
            slots: event.slots.to_vec(),
            min_length: event.min_length,
            bound: event.bound,
            threshold: event.threshold.map(|(score, _)| score),
            pruned: event.pruned,
            scored: event.scored,
            examined: event.examined,
        });
    })));
    events
}
