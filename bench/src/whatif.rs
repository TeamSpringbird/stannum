// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! What tighter sub-block bounds would have pruned, measured offline.
//!
//! A sub-block's bound today pairs each term-frequency bucket with the
//! shortest document holding it anywhere in the 65,536-document chunk (a
//! list's, anywhere in the list), and takes the best of those up to the
//! largest bucket the sub-block holds. Two tighter bounds are computed here
//! from the members themselves, for every sub-block the walk judged and
//! kept:
//!
//! - **own length**: the sub-block's largest bucket at the sub-block's own
//!   shortest member (never above today's bound, which is also valid);
//! - **exact**: the best score any member of the sub-block has, which is
//!   what per-block maxima stored at the sub-block's grain would give.
//!
//! A conjunction's shared documents are at least as long as every term's
//! shortest member in the sub-block, so that length floors both. A sub-block
//! the walk kept whose tighter bound falls below the threshold it was judged
//! against would have been skipped, with the candidates the walk scored in
//! it; pruning more never moves the threshold, since a skipped sub-block
//! cannot hold a document that ranks, so the counts are exact.

use engine::bm25::TermScorer;
use rustc_hash::FxHashMap;
use segment::bound::BlockBound;
use segment::index::Index;
use segment::segment::Segment;
use segment::tf_bucket::{BUCKET_COUNT, TfBucket};

use crate::dump::Dump;
use crate::replay::SubEvent;

const SUBS: usize = segment::ordinals::SUBS;
const SUB: u32 = segment::ordinals::SUB;
const NONE: u32 = u32::MAX;

/// Per sub-block of a chunk and bucket, the shortest member.
type ChunkTable = Box<[[u32; BUCKET_COUNT]; SUBS]>;

/// One term's members in one segment, tabulated.
struct TermTable {
    chunks: FxHashMap<u16, ChunkTable>,
    /// For a list stream, its one stored bound: per bucket the shortest
    /// member, and per sub-block index (folded across chunks) one past the
    /// largest bucket.
    list: Option<([u32; BUCKET_COUNT], [u8; SUBS])>,
}

/// The outcome over one query's sub-blocks.
#[derive(Clone, Copy, Debug, Default)]
pub struct WhatIf {
    /// Sub-blocks holding candidates that the walk judged.
    pub visited: u64,
    /// Of those, skipped by today's bound.
    pub pruned: u64,
    /// Kept by today's bound and skipped by the own-length bound.
    pub own: u64,
    /// Kept by today's bound and skipped by the exact bound.
    pub exact: u64,
    /// Candidates the walk examined in the sub-blocks each would skip.
    pub own_examined: u64,
    pub exact_examined: u64,
    /// Candidates the walk examined in the sub-blocks it kept.
    pub kept_examined: u64,
    /// Sub-blocks whose bound, recomputed from the members, differs from
    /// the walk's: should be zero.
    pub mismatches: u64,
}

impl WhatIf {
    pub fn add(&mut self, other: &WhatIf) {
        self.visited += other.visited;
        self.pruned += other.pruned;
        self.own += other.own;
        self.exact += other.exact;
        self.own_examined += other.own_examined;
        self.exact_examined += other.exact_examined;
        self.kept_examined += other.kept_examined;
        self.mismatches += other.mismatches;
    }
}

/// The tables of the terms queried so far, per segment.
pub struct Analyzer<'d> {
    segments: Vec<Segment<'d>>,
    tables: FxHashMap<(usize, String), Option<TermTable>>,
}

impl<'d> Analyzer<'d> {
    pub fn new(dump: &'d Dump) -> Result<Self, String> {
        let segments = dump
            .segments
            .iter()
            .map(|s| Segment::parse(&s.blob).map_err(|error| error.to_string()))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            segments,
            tables: FxHashMap::default(),
        })
    }

    fn table(&mut self, segment: usize, term: &str) -> Result<Option<&TermTable>, String> {
        let key = (segment, term.to_owned());
        if !self.tables.contains_key(&key) {
            let table = tabulate(&self.segments[segment], term).map_err(|e| e.to_string())?;
            self.tables.insert(key.clone(), table);
        }
        Ok(self.tables[&key].as_ref())
    }

    /// Judges one query's events: `source` maps an event to its segment,
    /// `terms` are the query's scorers, `examined` the candidates the walk
    /// examined in all.
    pub fn judge(
        &mut self,
        events: &[SubEvent],
        source: impl Fn(usize) -> Option<usize>,
        terms: &[(String, TermScorer)],
        examined: u64,
    ) -> Result<WhatIf, String> {
        let mut out = WhatIf::default();
        for (n, event) in events.iter().enumerate() {
            out.visited += 1;
            if event.pruned {
                out.pruned += 1;
                continue;
            }
            let in_sub = events
                .get(n + 1)
                .map_or(examined, |next| next.examined)
                .saturating_sub(event.examined);
            let Some(threshold) = event.threshold.filter(|_| event.bound.is_finite()) else {
                out.kept_examined += in_sub;
                continue;
            };
            let segment = source(event.index).ok_or("an event from an unknown source")?;
            let floor = event.min_length.unwrap_or(0);
            // Per term: today's bound, the own-length bound, the exact one,
            // and its shortest member in the sub-block.
            let mut parts = Vec::with_capacity(event.slots.len());
            for &slot in &event.slots {
                let (name, scorer) = &terms[slot];
                let table = self
                    .table(segment, name)?
                    .ok_or("a walked term missing from its segment")?;
                parts.push(bounds(table, scorer, event.key, event.sub, floor));
            }
            let conjunction = event.min_length.is_some();
            let shared = parts.iter().map(|p| p.shortest).max().unwrap_or(0);
            let mut today = 0.0_f32;
            let mut own = 0.0_f32;
            let mut exact = 0.0_f32;
            for (&slot, part) in event.slots.iter().zip(&parts) {
                let scorer = &terms[slot].1;
                today += part.today;
                let (own_part, exact_part) = if conjunction && shared > floor {
                    // Shared documents are as long as every term's shortest.
                    let again = bounds(
                        self.tables[&(segment, terms[slot].0.clone())]
                            .as_ref()
                            .expect("tabulated"),
                        scorer,
                        event.key,
                        event.sub,
                        shared,
                    );
                    (again.own, again.exact)
                } else {
                    (part.own, part.exact)
                };
                own += own_part.min(part.today);
                exact += exact_part;
            }
            if today.to_bits() != event.bound.to_bits() {
                out.mismatches += 1;
            }
            out.kept_examined += in_sub;
            if own < threshold {
                out.own += 1;
                out.own_examined += in_sub;
            }
            if exact < threshold {
                out.exact += 1;
                out.exact_examined += in_sub;
            }
        }
        Ok(out)
    }
}

struct Bounds {
    today: f32,
    own: f32,
    exact: f32,
    /// The term's shortest member in the sub-block; zero when it has none.
    shortest: u32,
}

/// One term's bounds on sub-block `sub` of chunk `key`, at least `floor`
/// long.
fn bounds(table: &TermTable, scorer: &TermScorer, key: u16, sub: usize, floor: u32) -> Bounds {
    let empty = [[NONE; BUCKET_COUNT]; SUBS];
    let chunk: &[[u32; BUCKET_COUNT]; SUBS] = table.chunks.get(&key).map_or(&empty, |c| c);
    // Today's: the stored bound, by bucket at its shortest member over the
    // chunk (the list), best up to the sub-block's largest bucket.
    let (block, top) = match &table.list {
        Some((min_len, subs)) => (*min_len, subs[sub]),
        None => {
            let mut min_len = [NONE; BUCKET_COUNT];
            for row in chunk {
                for (slot, len) in min_len.iter_mut().zip(row) {
                    *slot = (*slot).min(*len);
                }
            }
            let top = chunk[sub]
                .iter()
                .rposition(|len| *len != NONE)
                .map_or(0, |b| b as u8 + 1);
            (min_len, top)
        }
    };
    let today = if top == 0 {
        0.0
    } else {
        scorer.bounds_by_bucket(&BlockBound { min_len: block }, floor)[usize::from(top - 1)]
    };
    let row = &chunk[sub];
    let shortest = row.iter().copied().min().unwrap_or(NONE);
    if shortest == NONE {
        return Bounds {
            today,
            own: 0.0,
            exact: 0.0,
            shortest: 0,
        };
    }
    let own_top = row.iter().rposition(|len| *len != NONE).expect("a member") as u8;
    let own = scorer.bound_through(
        TfBucket::new(own_top).expect("a bucket"),
        shortest.max(floor),
    );
    let mut exact = 0.0_f32;
    for (bucket, len) in row.iter().enumerate() {
        if *len != NONE {
            let bucket = TfBucket::new(bucket as u8).expect("a bucket");
            exact = exact.max(scorer.score_bucket(bucket, (*len).max(floor)));
        }
    }
    Bounds {
        today,
        own,
        exact,
        shortest,
    }
}

/// Tabulates `term`'s members in `segment`, `None` when it is absent.
fn tabulate(segment: &Segment<'_>, term: &str) -> segment::Result<Option<TermTable>> {
    let Some(found) = Index::term(segment, term)? else {
        return Ok(None);
    };
    let ordinals = found.ordinals()?;
    let is_list = ordinals.list().is_some();
    let lengths = Index::lengths(segment);
    let mut cursor = ordinals.cursor()?;
    let mut chunks: FxHashMap<u16, ChunkTable> = FxHashMap::default();
    let mut list_len = [NONE; BUCKET_COUNT];
    let mut list_subs = [0u8; SUBS];
    while let Some(ordinal) = cursor.current() {
        let bucket = cursor
            .bucket()
            .ok_or(segment::Error::Corrupt("member bucket"))?;
        let len = lengths.get(ordinal)?.min(u32::MAX - 1);
        let sub = ((ordinal & 0xffff) / SUB) as usize;
        let row = &mut chunks
            .entry((ordinal >> 16) as u16)
            .or_insert_with(|| Box::new([[NONE; BUCKET_COUNT]; SUBS]))[sub];
        row[usize::from(bucket)] = row[usize::from(bucket)].min(len);
        list_len[usize::from(bucket)] = list_len[usize::from(bucket)].min(len);
        list_subs[sub] = list_subs[sub].max(bucket + 1);
        cursor.advance()?;
    }
    Ok(Some(TermTable {
        chunks,
        list: is_list.then_some((list_len, list_subs)),
    }))
}
