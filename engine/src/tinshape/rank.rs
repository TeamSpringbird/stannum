// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Ranked top k over a segment in TIN's shape, a 256-page group at a time.
//!
//! A query some terms of which every match holds (a conjunction, a phrase)
//! walks the groups its rarest such term holds. In each, the terms' members
//! are intersected whole (word by word where all are grids, else from the
//! fewest, a grid by bit and a list by a merge), and
//! the survivors are bounded, cheapest first, before anything is read for
//! them: by the footer blocks they fall in (a window per run of slots no
//! block boundary crosses), then by their length alone against the
//! window's largest buckets, and only then scored exactly from the TF tail.
//! A phrase reads positions only for a candidate that would enter the top
//! k: before the top k fill, every match does, so its positions are
//! checked first; after, a group's candidates that score into the top k
//! wait and are checked best first, so a confirmed one raises the
//! threshold over the rest.
//!
//! Any other query is block-max MaxScore over its scoring terms, a group at
//! a time: see [`Walk::run_or`].

use boldi_vigna::{PhrasePlan, SpanQuery, SpanSolver};
use segment::Tid;
use segment::tf_bucket::{BUCKET_COUNT, TfBucket};
use segment::tinshape::bits;
use segment::tinshape::blob::LazyBlob;
use segment::tinshape::docs::Geometry;
use segment::tinshape::ef::{Ef, EfCursor};
use segment::tinshape::positions::Positions;
use segment::tinshape::postings::{Form, KIND_EF, KIND_GRID, LazyFooter, for_each_local};
use segment::tinshape::segment::Segment;
use segment::{Error, Result};

use super::{
    Node, Part, RankedAnswer, Src, TermSet, Touch, kernels, matches, open_terms, required_terms,
};
use crate::bm25::TermScorer;
use crate::walk::{Ranked, TopRows, Visibility};

/// A required term's Elias-Fano container in a group is probed for the
/// candidates left rather than decoded when it holds more than this many
/// times as many members.
const SEEK_RATIO: usize = 4;

/// [`Walk::buckets`] of a term the candidate does not hold.
const NO_BUCKET: u8 = u8::MAX;

/// Candidates staged before they are finished best first: a group's
/// candidates are staged and finished this many at a time, so a threshold
/// that is still low early in a walk rises between chunks rather than
/// letting a whole group's candidates be bounded and sorted against it.
const STAGE_CHUNK: usize = 256;

/// One scoring term of a walk.
pub(super) struct Sc<'a> {
    pub(super) term: usize,
    pub(super) scorer: TermScorer,
    /// The term's footer, decoded as far as the walk has reached.
    pub(super) footer: LazyFooter<'a>,
    /// Per footer block decoded, its bound; NaN until asked.
    bounds: Vec<f32>,
    /// The block a range bound starts from, moved forward only.
    wb: usize,
    /// The block holding the last candidate asked about, moved forward only.
    cb: usize,
    /// [`TermScorer::length_bound_parts`], for [`Walk::length_bound`]'s
    /// cheap first test.
    num: [f32; BUCKET_COUNT],
    den: [f32; BUCKET_COUNT],
    factor: f32,
    /// [`Self::floor`] per bucket in the block last asked about, `u32::MAX`
    /// until asked (a rare term's block spans many groups' candidates), and
    /// the bucket's score at that length.
    bb_block: usize,
    bb: [(u32, f32); BUCKET_COUNT],
}

/// A footer block past what a walk could reach was asked for, or a block
/// it reached is corrupt.
#[cold]
fn corrupt_footer(error: Error) -> ! {
    crate::corrupt(format!("Stannum postings footer: {error}"))
}

impl<'a> Sc<'a> {
    pub(super) fn new(term: usize, scorer: TermScorer, footer: LazyFooter<'a>) -> Self {
        let (num, den, factor) = scorer.length_bound_parts();
        Self {
            term,
            bounds: Vec::new(),
            footer,
            scorer,
            wb: 0,
            cb: 0,
            num,
            den,
            factor,
            bb_block: usize::MAX,
            bb: [(u32::MAX, 0.0); BUCKET_COUNT],
        }
    }

    /// Block `b`'s bound, which must be decoded: the best score of its
    /// frontier.
    #[inline]
    fn bound(&mut self, b: usize) -> f32 {
        if let Some(v) = self.bounds.get(b)
            && !v.is_nan()
        {
            return *v;
        }
        self.bound_of(b)
    }

    /// [`Self::bound`], not known yet.
    #[inline(never)]
    fn bound_of(&mut self, b: usize) -> f32 {
        if b >= self.bounds.len() {
            self.bounds
                .resize(self.footer.decoded().max(b + 1), f32::NAN);
        }
        let scorer = &self.scorer;
        let v = self
            .footer
            .frontier(b)
            .iter()
            .map(|(bucket, length)| {
                scorer.bound_through(TfBucket::new(*bucket).expect("a valid bucket"), *length)
            })
            .fold(0.0_f32, f32::max);
        self.bounds[b] = v;
        v
    }

    /// The first block from `from` on whose last slot is `slot` or later,
    /// decoded; the footer's block count when there is none.
    #[inline]
    fn seek(&mut self, from: usize, slot: u32) -> usize {
        let b = self
            .footer
            .seek(from, slot)
            .unwrap_or_else(|e| corrupt_footer(e));
        if self.bounds.len() < self.footer.decoded() {
            if self.bounds.capacity() == 0 {
                self.bounds.reserve_exact(self.footer.blocks());
            }
            self.bounds.resize(self.footer.decoded(), f32::NAN);
        }
        b
    }

    /// [`Self::seek`], looking among the blocks decoded first.
    #[inline]
    fn first_reaching(&mut self, from: usize, slot: u32) -> usize {
        let last = self.footer.lasts();
        let mut b = from;
        while b < last.len() && last[b] < slot {
            b += 1;
        }
        if b < last.len() {
            b
        } else {
            self.seek(b, slot)
        }
    }

    /// The best bound of the blocks overlapping slots `from..=to`; ranges
    /// must be asked in increasing order of `from`.
    #[inline]
    fn range_bound(&mut self, from: u32, to: u32) -> f32 {
        // The blocks from the first ending at `from` or later through the
        // first ending at `to` or later (or the last), in one pass,
        // decoding as far as the last of them when it is not yet.
        self.wb = self.first_reaching(self.wb, from);
        let blocks = self.footer.blocks();
        let mut best = 0.0_f32;
        let mut b = self.wb;
        while b < blocks {
            if b >= self.footer.decoded() {
                self.seek(b, to);
            }
            best = best.max(self.bound(b));
            if self.footer.last(b) >= to {
                break;
            }
            b += 1;
        }
        best
    }

    /// The block holding `slot`, if the term has a posting at or after it;
    /// slots must be asked in increasing order.
    #[inline]
    fn block_at(&mut self, slot: u32) -> Option<usize> {
        // As a rule a block decoded already: the candidate's, or one after.
        self.cb = self.first_reaching(self.cb, slot);
        (self.cb < self.footer.blocks()).then_some(self.cb)
    }

    /// How short a document holding a posting of block `b` with bucket
    /// `bucket` can be: the shortest the block holds with that bucket or
    /// more, the shortest of the frontier's pairs at or above it (every
    /// posting is dominated by a frontier pair).
    #[inline]
    fn floor(&self, b: usize, bucket: u8) -> u32 {
        self.footer
            .frontier(b)
            .iter()
            .filter(|(at, _)| *at >= bucket)
            .map(|(_, length)| *length)
            .min()
            .unwrap_or(0)
    }

    /// [`Self::floor`] and what bucket `bucket` scores at most there (the
    /// term's own bound, before the other terms raise the length), kept per
    /// bucket for the block last asked about.
    #[inline]
    fn floor_kept(&mut self, b: usize, bucket: u8) -> (u32, f32) {
        if self.bb_block != b {
            self.bb_block = b;
            self.bb = [(u32::MAX, 0.0); BUCKET_COUNT];
        }
        let known = self.bb[usize::from(bucket)];
        if known.0 != u32::MAX {
            return known;
        }
        let floor = self.floor(b, bucket);
        let v = (
            floor,
            self.scorer
                .bound_through(TfBucket::new(bucket).expect("a valid bucket"), floor),
        );
        self.bb[usize::from(bucket)] = v;
        v
    }

    /// Block `b`'s largest bucket: its frontier's last pair.
    #[inline]
    fn max_bucket(&self, b: usize) -> u8 {
        self.footer.max_bucket(b)
    }
}

/// How a term's members in the group at hand are held.
#[derive(Default)]
enum Kind<'a> {
    /// Decoded into [`Mem::list`].
    #[default]
    List,
    /// A grid container, read in place.
    Grid(&'a [u8]),
    /// An Elias-Fano list read by seeking: a group container (`offset` 0)
    /// or a sparse term's whole list (`offset` the group's first slot).
    Cursor { cursor: EfCursor<'a>, offset: u32 },
}

/// A term's members in the group at hand, and a forward cursor over their
/// posting indexes.
#[derive(Default)]
struct Mem<'a> {
    /// The group loaded, plus one; zero for none.
    loaded: u32,
    kind: Kind<'a>,
    /// The members' local slots, ascending, for [`Kind::List`].
    list: Vec<u32>,
    /// Postings in earlier groups (zero for a sparse term's cursor, whose
    /// rank is the index).
    first: u32,
    /// Members in the group: from the directory, or estimated for a sparse
    /// term.
    count: u32,
    /// Grid: members in words `..w` are `run`. List: the next member is at
    /// `pos`.
    w: usize,
    run: u32,
    pos: usize,
    /// The term's group directory position, moved forward only.
    hint: usize,
    /// Containers loaded from the directory, for [`MemoCounts`].
    ///
    /// [`MemoCounts`]: segment::tinshape::segment::MemoCounts
    used: u32,
}

impl Mem<'_> {
    /// The posting index of `local` if the term holds it; slots must be
    /// asked in increasing order (asking one again is allowed).
    #[inline]
    fn find(&mut self, local: u32) -> Option<u32> {
        match &mut self.kind {
            Kind::Grid(bytes) => {
                if bytes[local as usize / 8] >> (local % 8) & 1 == 0 {
                    return None;
                }
                let w = local as usize / 64;
                if w > self.w {
                    self.run += kernels::popcount_bytes(&bytes[self.w * 8..w * 8]);
                    self.w = w;
                }
                let word = bits::word(bytes, w);
                Some(self.first + self.run + (word & ((1u64 << (local % 64)) - 1)).count_ones())
            }
            Kind::List => {
                let list = &self.list;
                let mut pos = self.pos;
                while pos < list.len() && list[pos] < local {
                    pos += 1;
                }
                self.pos = pos;
                (pos < list.len() && list[pos] == local).then(|| self.first + pos as u32)
            }
            Kind::Cursor { cursor, offset } => {
                let target = *offset + local;
                cursor.seek(target);
                (cursor.current() == Some(target)).then(|| self.first + cursor.rank() as u32)
            }
        }
    }
}

/// The first group at or after `from` holding a member of `set`.
fn next_group(
    set: &mut TermSet<'_>,
    geometry: &Geometry,
    from: u32,
    hint: &mut usize,
    touch: &mut impl Touch,
) -> Option<u32> {
    if let Form::Sparse(list) = &set.postings.form {
        let group = geometry.groups.get(from as usize)?;
        let cursor = set.sparse.get_or_insert_with(|| {
            touch.touch(
                Part::Payload,
                set.at + set.postings.payload_at,
                set.postings.payload.len(),
            );
            list.cursor()
        });
        cursor.seek(group.slot_base);
        return cursor
            .current()
            .map(|slot| geometry.group_of_slot(slot) as u32);
    }
    let count = set.group_count();
    while *hint < count && set.group_index(*hint) < from {
        *hint += 1;
    }
    (*hint < count).then(|| set.group_index(*hint))
}

/// Loads `set`'s members in group `g`: decoded into a list, or an
/// Elias-Fano list of more than `probe_over` members left to be sought
/// (for a sparse term, which must hold the group, its share of the term's
/// postings by slots stands for its count there). False, with nothing
/// loaded, when a grouped term does not hold the group: the directory is
/// looked up once.
fn load<'a>(
    set: &mut TermSet<'a>,
    geometry: &Geometry,
    g: u32,
    mem: &mut Mem<'a>,
    probe_over: usize,
    touch: &mut impl Touch,
) -> Result<bool> {
    mem.loaded = g + 1;
    mem.kind = Kind::List;
    mem.list.clear();
    mem.w = 0;
    mem.run = 0;
    mem.pos = 0;
    let group = geometry.groups[g as usize];
    if let Form::Sparse(list) = &set.postings.form {
        let cursor = set.sparse.get_or_insert_with(|| {
            touch.touch(
                Part::Payload,
                set.at + set.postings.payload_at,
                set.postings.payload.len(),
            );
            list.cursor()
        });
        cursor.seek(group.slot_base);
        let share = u64::from(set.df) * u64::from(group.slots()) / u64::from(geometry.slots).max(1);
        if share.max(1) > probe_over as u64 {
            mem.count = share.max(1) as u32;
            mem.first = 0;
            mem.kind = Kind::Cursor {
                cursor: cursor.clone(),
                offset: group.slot_base,
            };
            return Ok(true);
        }
        mem.first = cursor.rank() as u32;
        let end = group.slot_base + group.slots();
        let list = &mut mem.list;
        cursor.drain_below(end, |slot| list.push(slot - group.slot_base));
        mem.count = mem.list.len() as u32;
        return Ok(true);
    }
    let Some(entry) = set.find(g, &mut mem.hint) else {
        mem.count = 0;
        return Ok(false);
    };
    mem.count = entry.count;
    match entry.src {
        Src::Container {
            entry: e,
            bytes,
            at,
        } => {
            touch.touch(Part::Payload, at, bytes.len());
            let bytes = bytes.all()?;
            mem.used += 1;
            mem.first = e.first;
            if e.kind == KIND_GRID {
                mem.kind = Kind::Grid(bytes);
            } else if e.kind == KIND_EF && e.count as usize > probe_over {
                let ef = Ef::parse(bytes, e.count as usize, group.slots())?;
                mem.kind = Kind::Cursor {
                    cursor: ef.cursor(),
                    offset: 0,
                };
            } else {
                let list = &mut mem.list;
                for_each_local(&e, bytes, &group, |l| list.push(l))?;
            }
        }
        Src::Locals { from, to } => {
            mem.first = from;
            mem.list
                .extend_from_slice(&set.locals[from as usize..to as usize]);
        }
    }
    Ok(true)
}

/// A term's positions read by posting index, forward from the last entry
/// read where that is nearer than the entry its tables locate.
pub(super) struct PosCursor<'a> {
    positions: Positions<'a>,
    at: usize,
    /// Entry `next` starts at byte `next_at`.
    next: u32,
    next_at: usize,
}

impl<'a> PosCursor<'a> {
    pub(super) fn new(stream: (segment::tinshape::blob::Bytes<'a>, usize)) -> Result<Self> {
        let positions = Positions::parse(stream.0)?;
        Ok(Self {
            next: 0,
            next_at: positions.data_at,
            positions,
            at: stream.1,
        })
    }

    /// Reads entry `index` into `out`.
    pub(super) fn read(
        &mut self,
        index: u32,
        out: &mut Vec<u32>,
        touch: &mut impl Touch,
    ) -> Result<()> {
        let (entry, entry_at, reads) = self.positions.locate(index)?;
        if index < self.next || entry > self.next {
            for (at, len) in reads {
                if len > 0 {
                    touch.touch(Part::Positions, self.at + at, len);
                }
            }
            self.next = entry;
            self.next_at = entry_at;
        }
        if let Some(m) = self.positions.mask_at(index) {
            touch.touch(Part::Positions, self.at + m, 4);
        }
        let from = self.next_at;
        let mut p = self
            .positions
            .skip_entries(self.next, self.next_at, index)?;
        p = self.positions.read_entry(index, p, out)?;
        self.next = index + 1;
        self.next_at = p;
        touch.touch(Part::Positions, self.at + from, p - from);
        Ok(())
    }
}

/// A top-level span's positions check.
struct SpanCheck<'a> {
    /// Per span slot, its term, and the first slot of the same term (a
    /// phrase repeating a word reads its positions once).
    slots: Vec<usize>,
    first: Vec<usize>,
    cursors: Vec<PosCursor<'a>>,
    solver: SpanSolver,
    plan: Option<PhrasePlan>,
    positions: Vec<Vec<u32>>,
    read: Vec<bool>,
}

impl SpanCheck<'_> {
    /// Reads slot `slot`'s positions unless read: its term's first slot's,
    /// copied, when an earlier slot holds the same term.
    fn read_slot(&mut self, slot: usize, index: &[u32], touch: &mut impl Touch) -> Result<()> {
        if std::mem::replace(&mut self.read[slot], true) {
            return Ok(());
        }
        let first = self.first[slot];
        if first == slot {
            return self.cursors[slot].read(index[slot], &mut self.positions[slot], touch);
        }
        self.read_slot(first, index, touch)?;
        let (head, tail) = self.positions.split_at_mut(slot);
        tail[0].clone_from(&head[first]);
        Ok(())
    }

    /// Whether the candidate whose posting index in each slot's term is
    /// `index[slot]` holds the span.
    fn holds(&mut self, index: &[u32], touch: &mut impl Touch) -> Result<bool> {
        self.read.fill(false);
        if let Some(plan) = self.plan.take() {
            let mut kept = true;
            for step in plan.steps() {
                if let Err(error) = self.read_slot(step.slot, index, touch) {
                    self.plan = Some(plan);
                    return Err(error);
                }
                if let Some(pair) = step.pair
                    && !plan.pair_keeps(pair, &self.positions)
                {
                    kept = false;
                    break;
                }
            }
            self.plan = Some(plan);
            if !kept {
                return Ok(false);
            }
        }
        for slot in 0..index.len() {
            self.read_slot(slot, index, touch)?;
        }
        Ok(self.solver.intervals(&self.positions).next().is_some())
    }
}

/// How a candidate holding the walk's terms is confirmed as a match.
enum Verify<'a> {
    /// Holding them is matching (a flat AND or a term).
    Flat,
    /// A top-level span: positions.
    Span(Box<SpanCheck<'a>>),
    /// Anything else: the node evaluated at the candidate, in slot order.
    Node,
}

/// A ranked walk over one segment: led by required terms (`run`) or a
/// disjunction of the scoring terms (`run_or`).
struct Walk<'s, 'a, T: Touch> {
    segment: &'s Segment<'a>,
    geometry: &'s Geometry,
    node: &'s Node,
    /// The rows kept so far, shared with the walks over other sources, and
    /// the check a row passes as it is kept (heap visibility, the scan's
    /// other restrictions).
    top: &'s mut TopRows,
    visibility: &'s mut dyn Visibility,
    touch: &'s mut T,
    terms: Vec<Option<TermSet<'a>>>,
    /// Per term (by index into `terms`), its members in the group at hand.
    mems: Vec<Mem<'a>>,
    sc: Vec<Sc<'a>>,
    /// Required terms, rarest first.
    req: Vec<usize>,
    /// Per scoring term: whether it is required; whether it holds the group
    /// at hand, and the candidate at hand, and that candidate's block; its
    /// bound over the sub-range at hand.
    sc_required: Vec<bool>,
    present: Vec<bool>,
    /// A disjunction's present terms in the group at hand, and those the
    /// candidate at hand holds, both in the scorer's order.
    present_list: Vec<usize>,
    held_list: Vec<usize>,
    blocks: Vec<usize>,
    sub_bounds: Vec<f32>,
    plan: Plan,
    /// A disjunction's group at hand: per scoring term a row of the
    /// group's words holding its members, and its index cursor.
    rows: Vec<u64>,
    row_state: Vec<Row>,
    /// Per scoring term, whether its row holds the group at hand; and the
    /// essential terms' union there.
    row_loaded: Vec<bool>,
    any: Vec<u64>,
    /// A disjunction's group at hand: per present term with a positive
    /// bound there, where its row starts in `rows`, that bound and its bound
    /// over the sub-range at hand; and its word at hand.
    or_terms: Vec<(usize, f32, f32)>,
    or_held: Vec<u64>,
    /// Per scoring term, the candidate at hand's bucket ([`NO_BUCKET`]
    /// when it does not hold the term).
    buckets: Vec<u8>,
    /// Per scoring term, the candidate at hand's posting index in it (read
    /// with its bucket); and per term, its index into `sc` (`usize::MAX`
    /// for a term that does not score).
    held_index: Vec<u32>,
    term_sc: Vec<usize>,
    /// The window: the last slot of every scoring term's footer block at
    /// the slot it was set at, and its bound; per scoring term, its block's
    /// largest bucket ([`NO_BUCKET`] when it holds nothing from the slot
    /// on).
    window_end: Option<u32>,
    window_bound: f32,
    window_buckets: Vec<u8>,
    /// The window's length bound parts: its terms' numerators summed, and
    /// the least of their denominators and length factors.
    window_parts: (f64, f64, f64),
    /// A required term whose record carries its documents' lengths, the
    /// led walk reads them from.
    inline: Option<usize>,
    /// A phrase's scoring terms it uses more than once, by index into `sc`,
    /// each with the least bucket of a count reaching their number of uses.
    repeats: Vec<(usize, u8)>,
    answer: RankedAnswer,
    verify: Verify<'a>,
    /// Phrase candidates of the group that scored into the top k, awaiting
    /// their positions check: (score, local slot), and per one its posting
    /// index in each span slot's term, `span` apiece.
    pending: Vec<(f32, u32)>,
    pending_index: Vec<u32>,
    span_index: Vec<u32>,
    /// The group's staged candidates: (bound, local slot, length or
    /// `u32::MAX`), and per one its buckets (one per scoring term) and, for
    /// a span, its posting indexes; and the order they are finished in.
    staged: Vec<(f32, u32, u32)>,
    staged_buckets: Vec<u8>,
    staged_span: Vec<u32>,
    staged_order: Vec<u32>,
    cands: Vec<u32>,
    words: Vec<u64>,
    /// The node check's own cursors (the walk moves the terms' sparse
    /// cursors past each group), and its scratch.
    node_terms: Vec<Option<TermSet<'a>>>,
    positions: Vec<Vec<u32>>,
    /// The blob the segment is read from in place, and the mark of the
    /// pages pinned before the walk opened its terms (see
    /// [`Walk::group_done`]); the ranges of the blob the terms' records keep
    /// slices of.
    frame: Option<(&'a LazyBlob, usize)>,
    held: Vec<(usize, usize)>,
}

impl<'a, T: Touch> Walk<'_, 'a, T> {
    /// Records the footer bytes the walk read, and adds what it used of
    /// its terms' footers and directories to this thread's
    /// [`segment::tinshape::segment::MemoCounts`].
    fn count_metadata(&mut self) {
        for s in &self.sc {
            let read = s.footer.read();
            if read > 0 {
                let set = self.terms[s.term].as_ref().expect("scoring");
                self.touch
                    .touch(Part::Footer, set.at + set.postings.footer_at, read);
            }
        }
        let used: u64 = self
            .sc
            .iter()
            .map(|s| s.bounds.iter().filter(|b| !b.is_nan()).count() as u64)
            .sum();
        let blocks_reached: u64 = self.sc.iter().map(|s| s.footer.decoded() as u64).sum();
        let blocks_whole: u64 = self.sc.iter().map(|s| s.footer.blocks() as u64).sum();
        let bytes_whole: u64 = self
            .sc
            .iter()
            .filter_map(|s| self.terms[s.term].as_ref())
            .map(|set| set.postings.footer.len() as u64)
            .sum();
        let (mut reached, mut loaded) = (0u64, 0u64);
        for (set, mem) in self.terms.iter().zip(&self.mems) {
            if let Some(set) = set
                && matches!(set.postings.form, Form::Grouped(_))
            {
                reached += (mem.hint + 1).min(set.group_count()) as u64;
                loaded += u64::from(mem.used);
            }
        }
        segment::tinshape::segment::count_memo(|c| {
            c.blocks_used += used;
            c.blocks_reached += blocks_reached;
            c.footer_blocks_whole += blocks_whole;
            c.footer_bytes_whole += bytes_whole;
            c.entries_reached += reached;
            c.entries_used += loaded;
        });
    }

    #[inline]
    fn threshold(&self) -> Option<f32> {
        self.top.bar().map(|bar| bar.0)
    }

    /// Whether no match of ctid `from()` or later scoring at most `bound`
    /// can be kept: it scores below the bar, or ties it from a later ctid
    /// (rows rank by score, then ctid).
    ///
    /// Bounds are exact: each term's is an `f32` score of the scorer's
    /// own at a bucket and length no worse than the match's, and they are
    /// summed in `f32` in the scorer's order, the order [`Walk::process`]
    /// sums the scores in. Correctly rounded addition is monotonic in both
    /// operands, so the summed bound is at least the summed score, bit for
    /// bit; a term a match lacks adds zero to its score.
    #[inline]
    fn cut(&self, bound: f32, from: impl FnOnce() -> Tid) -> bool {
        match self.top.bar() {
            None => false,
            Some((score, tid)) => match bound.total_cmp(&score) {
                std::cmp::Ordering::Less => true,
                std::cmp::Ordering::Equal => from() >= tid,
                std::cmp::Ordering::Greater => false,
            },
        }
    }

    /// Whether `(total, tid)` would be kept.
    #[inline]
    fn admits(&self, total: f32, tid: Tid) -> bool {
        self.top.admits(&Ranked(total, tid))
    }

    /// Keeps `(total, tid)` if it ranks above the bar and passes the check.
    fn push(&mut self, total: f32, tid: Tid) {
        let row = Ranked(total, tid);
        if self.top.admits(&row) && self.visibility.visible(tid) {
            self.top.push(row);
        }
    }

    /// With nothing scoring, whether no match from `tid` on can be kept:
    /// each scores zero, and the bar ranks before it (rows of other
    /// sources may sit anywhere in ctid order).
    fn unscored_done(&self, tid: Tid) -> bool {
        !self.admits(0.0, tid)
    }

    /// Walks the groups the rarest required term holds. A group whose
    /// scoring terms' bounds cannot reach the threshold is skipped unread;
    /// in any other the required terms' members are intersected whole (word
    /// by word where every one is a grid, else term by term from the fewest)
    /// and the candidates left are bounded, scored and admitted in order.
    fn run(&mut self) -> Result<()> {
        let lead = self.req[0];
        let n = self.sc.len();
        let mut from = 0u32;
        loop {
            // The lead's next group, from its directory (or its list): its
            // members are read only once the group's bound passes.
            let set = self.terms[lead].as_mut().expect("the lead");
            let Some(g) = next_group(
                set,
                self.geometry,
                from,
                &mut self.mems[lead].hint,
                self.touch,
            ) else {
                break;
            };
            from = g + 1;
            let group = self.geometry.groups[g as usize];
            let (base, end) = (group.slot_base, group.slot_base + group.slots() - 1);
            if n == 0 && self.unscored_done(self.geometry.tid_in(g as usize, 0)) {
                // Nothing scores: every later match ties at zero and ranks
                // after the bar.
                break;
            }
            self.answer.windows += 1;
            if self.group_below(g, base, end) {
                self.answer.windows_pruned += 1;
                continue;
            }
            if self.mems[lead].loaded != g + 1 {
                let set = self.terms[lead].as_mut().expect("the lead");
                load(
                    set,
                    self.geometry,
                    g,
                    &mut self.mems[lead],
                    usize::MAX,
                    self.touch,
                )?;
            }
            let more = if self.all_grids(g) {
                self.dense_group(g)?
            } else {
                self.group_and(g)?
            };
            if !more {
                break;
            }
            self.group_done();
        }
        Ok(())
    }

    /// Ends a group's reads: the pages pinned since the walk's mark are
    /// released, all but those the terms' records borrow and the few read
    /// last the blob keeps (see [`LazyBlob::release_since`]), once more
    /// than those are held. A group's reads leave nothing behind that
    /// borrows its pages: its candidates were scored, its phrase
    /// candidates checked, and the terms' members (a grid or an
    /// Elias-Fano list read in place) are forgotten here; positions, TF
    /// tails and the DL sidecar are read a value at a time, and a
    /// disjunction copies its terms' rows. What the walk keeps across
    /// groups holds no slice (directories, decoded; positions streams,
    /// read when asked), lies in [`Walk::held`] (a sparse term's list,
    /// inline lengths) or is held for this release: a scoring term's
    /// footer, decoded lazily, reads on from the page it read last
    /// ([`LazyFooter::borrowed`]), one page per term at most.
    #[inline]
    fn group_done(&mut self) {
        let Some((blob, mark)) = self.frame else {
            return;
        };
        if !blob.over_keep(mark) {
            return;
        }
        debug_assert!(self.pending.is_empty() && self.staged.is_empty());
        for mem in &mut self.mems {
            mem.loaded = 0;
            mem.kind = Kind::List;
        }
        let held = self.held.len();
        self.held
            .extend(self.sc.iter().filter_map(|s| s.footer.borrowed()));
        // SAFETY: nothing read in place since the mark is used after but
        // what lies in `held` (see above): the members' slices were
        // dropped just now, and the footers' windows are held.
        unsafe { blob.release_since(mark, &self.held) };
        self.held.truncate(held);
    }

    /// Whether no match in slots `base..=end` of group `g` can be kept by
    /// the scoring terms' bounds there (see [`Walk::cut`]). Bounds are not
    /// negative, so the sum stops once it passes the threshold. With
    /// nothing scoring every match scores zero.
    #[inline]
    fn group_below(&mut self, g: u32, base: u32, end: u32) -> bool {
        let Some(theta) = self.threshold() else {
            return false;
        };
        let mut bound = 0.0_f32;
        for i in 0..self.sc.len() {
            bound += self.group_term_bound(i, g, base, end);
            if bound > theta {
                return false;
            }
        }
        let geometry = self.geometry;
        self.cut(bound, || geometry.tid_in(g as usize, 0))
    }

    /// The group-skip hook of both walks: scoring term `i`'s bound over
    /// group `g` (slots `base..=end`) before any of the group is read. It
    /// is the term's footer blocks' bound over the slots, capped by the
    /// group's frontier where the term's directory holds one
    /// ([`TermSet::group_bound`]): zero, with no block bounded, where the
    /// term does not hold the group. Either is at least the term's score
    /// of each of its postings there, bit for bit, so their sum in the
    /// scorer's order bounds a match as [`Walk::cut`] requires.
    #[inline]
    fn group_term_bound(&mut self, i: usize, g: u32, base: u32, end: u32) -> f32 {
        let t = self.sc[i].term;
        let cap = self.terms[t]
            .as_ref()
            .and_then(|set| set.group_bound(g, &mut self.mems[t].hint, &self.sc[i].scorer));
        match cap {
            Some(0.0) => 0.0,
            Some(cap) => self.sc[i].range_bound(base, end).min(cap),
            None => self.sc[i].range_bound(base, end),
        }
    }

    /// Whether every required term holds group `g` as a grid.
    fn all_grids(&mut self, g: u32) -> bool {
        for r in 0..self.req.len() {
            let t = self.req[r];
            let set = self.terms[t].as_ref().expect("required");
            if matches!(set.postings.form, Form::Sparse(_)) {
                return false;
            }
            let mem = &mut self.mems[t];
            match set.find(g, &mut mem.hint) {
                Some(entry) if matches!(entry.src, Src::Container { entry, .. } if entry.kind == KIND_GRID) =>
                    {}
                _ => return false,
            }
        }
        true
    }

    /// Intersects the required terms' members in group `g`, which the lead
    /// holds and has loaded: the lead's members, then each other required
    /// term's, rarest first, only while candidates are left (in most groups
    /// a rare lead's few members leave none after one or two terms). A grid
    /// filters them by bit, an Elias-Fano list much longer than they are by
    /// probing its high bits in place, any other list by a merge. Then walks
    /// the candidates left.
    fn group_and(&mut self, g: u32) -> Result<bool> {
        let mut cands = std::mem::take(&mut self.cands);
        cands.clear();
        let lead = &self.mems[self.req[0]];
        match &lead.kind {
            Kind::Grid(bytes) => {
                for w in 0..bytes.len() / 8 {
                    let mut word = bits::word(bytes, w);
                    while word != 0 {
                        cands.push((w * 64) as u32 + word.trailing_zeros());
                        word &= word - 1;
                    }
                }
            }
            Kind::List => cands.extend_from_slice(&lead.list),
            Kind::Cursor { .. } => unreachable!("a lead is loaded whole"),
        }
        for r in 1..self.req.len() {
            if cands.is_empty() {
                break;
            }
            let t = self.req[r];
            if self.mems[t].loaded != g + 1 {
                let set = self.terms[t].as_mut().expect("required");
                let mem = &mut self.mems[t];
                let sparse = matches!(set.postings.form, Form::Sparse(_));
                if sparse && next_group(set, self.geometry, g, &mut mem.hint, self.touch) != Some(g)
                {
                    cands.clear();
                    break;
                }
                let probe_over = if sparse {
                    usize::MAX
                } else {
                    SEEK_RATIO * cands.len()
                };
                if !load(set, self.geometry, g, mem, probe_over, self.touch)? {
                    cands.clear();
                    break;
                }
            }
            let mem = &self.mems[t];
            match &mem.kind {
                Kind::Grid(bytes) => {
                    cands.retain(|l| bytes[*l as usize / 8] >> (l % 8) & 1 == 1);
                }
                Kind::Cursor { cursor, offset: 0 } => cursor.list().retain_members(&mut cands),
                Kind::Cursor { .. } => {
                    let mem = &mut self.mems[t];
                    cands.retain(|l| mem.find(*l).is_some());
                }
                Kind::List => super::intersect_sorted(&mut cands, &mem.list),
            }
        }
        let more = self.walk_candidates(g, &cands)?;
        self.cands = cands;
        Ok(more)
    }

    /// Bounds, scores and admits group `g`'s candidates `cands` (local
    /// slots, ascending), then checks its pending phrase candidates; false
    /// once nothing later can enter the top k. A span's candidates, with
    /// terms to score, are bounded by their buckets first and scored best
    /// first (see [`Walk::finish_staged`]): its top k fill only with
    /// confirmed matches, which its best-bounded candidates find soonest,
    /// and each one scored may mean a positions check. Any other walk scores
    /// in ctid order: on TIN's corpus staging its candidates scored fewer
    /// of them but read no fewer DL pages (the sidecar holds about a
    /// group's lengths per page), and took longer.
    fn walk_candidates(&mut self, g: u32, cands: &[u32]) -> Result<bool> {
        let base = self.geometry.groups[g as usize].slot_base;
        let staging = !self.sc.is_empty() && matches!(self.verify, Verify::Span(_));
        let mut more = true;
        let mut at = 0;
        while at < cands.len() {
            let local = cands[at];
            at += 1;
            if self.window(g, base, local) {
                // Skip the rest of the window unread.
                match self.window_end {
                    Some(e) if e < u32::MAX => {
                        at += cands[at..].partition_point(|l| base + *l <= e);
                        continue;
                    }
                    _ => break,
                }
            }
            if staging {
                self.stage_into(g, local)?;
                if self.staged.len() >= STAGE_CHUNK {
                    self.finish_staged(g)?;
                }
            } else if !self.process(g, local)? {
                more = false;
                break;
            }
        }
        if staging {
            self.finish_staged(g)?;
        }
        if !self.pending.is_empty() {
            self.verify_pending(g)?;
        }
        Ok(more)
    }

    /// Sieves group `g`, every required term of which is a grid there, by
    /// the AND of their words, and walks its candidates.
    fn dense_group(&mut self, g: u32) -> Result<bool> {
        let group = self.geometry.groups[g as usize];
        for r in 0..self.req.len() {
            let t = self.req[r];
            if self.mems[t].loaded != g + 1 {
                let set = self.terms[t].as_mut().expect("required");
                load(
                    set,
                    self.geometry,
                    g,
                    &mut self.mems[t],
                    usize::MAX,
                    self.touch,
                )?;
            }
        }
        let grid = |m: &Mem<'a>| match m.kind {
            Kind::Grid(bytes) => bytes,
            _ => unreachable!("a grid"),
        };
        let words = &mut self.words;
        words.clear();
        words.resize(group.words(), 0);
        kernels::load(words, grid(&self.mems[self.req[0]]));
        for &t in &self.req[1..] {
            kernels::and_bytes(words, grid(&self.mems[t]));
        }
        if let Some(dead) = self.segment.liveness.groups[g as usize].as_deref() {
            kernels::andnot_words(words, dead);
        }
        let mut cands = std::mem::take(&mut self.cands);
        cands.clear();
        for (w, word) in self.words.iter().enumerate() {
            let mut word = *word;
            while word != 0 {
                cands.push((w * 64) as u32 + word.trailing_zeros());
                word &= word - 1;
            }
        }
        let more = self.walk_candidates(g, &cands)?;
        self.cands = cands;
        Ok(more)
    }

    /// Moves the window to slot `local` of group `g` (whose first slot is
    /// `base`) and says whether no match there can be kept by its bound:
    /// slots up to [`Self::window_end`] share every scoring term's footer
    /// block, so its bound and its largest buckets.
    #[inline]
    fn window(&mut self, g: u32, base: u32, local: u32) -> bool {
        let slot = base + local;
        if self.window_end.is_none_or(|e| slot > e) {
            let mut e = u32::MAX;
            let mut bound = 0.0_f32;
            let (mut nsum, mut dmin, mut fmin) = (0.0_f64, f64::INFINITY, f64::INFINITY);
            for (s, mb) in self.sc.iter_mut().zip(&mut self.window_buckets) {
                let Some(b) = s.block_at(slot) else {
                    *mb = NO_BUCKET;
                    continue;
                };
                bound += s.bound(b);
                e = e.min(s.footer.last(b));
                *mb = s.max_bucket(b);
                let m = usize::from(*mb);
                nsum += f64::from(s.num[m]);
                dmin = dmin.min(f64::from(s.den[m]));
                fmin = fmin.min(f64::from(s.factor));
            }
            self.window_end = Some(e);
            self.window_bound = bound;
            self.window_parts = (nsum, dmin, fmin);
        }
        let geometry = self.geometry;
        self.cut(self.window_bound, || geometry.tid_in(g as usize, local))
    }

    /// Whether no document of `length` and ctid `tid` in the window can be
    /// kept: first by the terms' numerators over the least of their
    /// denominators, one division, pruning only a bound below the
    /// threshold by far more than `f32` rounding can explain; then exactly,
    /// by [`Self::length_bound`].
    #[inline]
    fn length_cut(&self, length: u32, tid: Tid) -> bool {
        let Some(theta) = self.threshold() else {
            return false;
        };
        let (nsum, dmin, fmin) = self.window_parts;
        if nsum / (dmin + fmin * f64::from(length)) * (1.0 + 1e-5) < f64::from(theta) {
            return true;
        }
        self.cut(self.length_bound(length), || tid)
    }

    /// What a document of `length` in the window scores at most: per
    /// scoring term, the best score of a bucket up to its block's largest
    /// at that length, summed in the scorer's order.
    #[inline]
    fn length_bound(&self, length: u32) -> f32 {
        let mut bound = 0.0_f32;
        for (s, mb) in self.sc.iter().zip(&self.window_buckets) {
            if *mb != NO_BUCKET {
                bound += s
                    .scorer
                    .bound_through(TfBucket::new(*mb).expect("a valid bucket"), length);
            }
        }
        bound
    }

    /// Term `t`'s members in group `g`, loaded on first use (none when it
    /// holds no member there).
    fn ensure(&mut self, t: usize, g: u32) -> Result<()> {
        if self.mems[t].loaded == g + 1 {
            return Ok(());
        }
        let set = self.terms[t].as_mut().expect("a term of the walk");
        let mem = &mut self.mems[t];
        let held = !matches!(set.postings.form, Form::Sparse(_))
            || next_group(set, self.geometry, g, &mut mem.hint, self.touch) == Some(g);
        if !(held && load(set, self.geometry, g, mem, 64, self.touch)?) {
            mem.loaded = g + 1;
            mem.kind = Kind::List;
            mem.list.clear();
            mem.pos = 0;
        }
        Ok(())
    }

    /// Bounds, scores and admits the candidate at slot `local` of group
    /// `g`, which every required term holds; false once nothing later can
    /// enter the top k.
    fn process(&mut self, g: u32, local: u32) -> Result<bool> {
        if self.sc.is_empty() && self.unscored_done(self.geometry.tid_in(g as usize, local)) {
            return Ok(false);
        }
        if let Some((_, length)) = self.stage(g, local)? {
            self.finish(g, local, length)?;
        }
        Ok(true)
    }

    /// Stages group `g`'s candidate at slot `local`: bounds it by its
    /// buckets and keeps it, best bound first, for [`Walk::finish_staged`].
    fn stage_into(&mut self, g: u32, local: u32) -> Result<()> {
        let Some((reach, length)) = self.stage(g, local)? else {
            return Ok(());
        };
        self.staged.push((reach, local, length));
        self.staged_buckets.extend_from_slice(&self.buckets);
        if matches!(self.verify, Verify::Span(_)) {
            self.staged_span.extend_from_slice(&self.span_index);
        }
        Ok(())
    }

    /// Reads the lengths of the group's staged candidates and scores them,
    /// best bound first, each against the threshold as it then stands: the
    /// best of a group raise it over the rest, which its first candidates
    /// in ctid order did not (on TIN's corpus, `w3 AND w17` read the DL
    /// sidecar for six times the candidates the final threshold needs).
    fn finish_staged(&mut self, g: u32) -> Result<()> {
        let n = self.sc.len();
        let width = self.span_index.len();
        let mut staged = std::mem::take(&mut self.staged);
        let mut order = std::mem::take(&mut self.staged_order);
        order.clear();
        order.extend(0..staged.len() as u32);
        order.sort_unstable_by(|a, b| {
            let (x, y) = (staged[*a as usize], staged[*b as usize]);
            y.0.total_cmp(&x.0).then(x.1.cmp(&y.1))
        });
        let mut result = Ok(());
        for &i in &order {
            let i = i as usize;
            let (reach, local, length) = staged[i];
            let tid = self.geometry.tid_in(g as usize, local);
            // In this order (bound down, then ctid up) and with a bar that
            // only rises, every candidate after one cut is cut too.
            if self.cut(reach, || tid) {
                break;
            }
            self.buckets
                .copy_from_slice(&self.staged_buckets[i * n..(i + 1) * n]);
            if matches!(self.verify, Verify::Span(_)) {
                self.span_index
                    .copy_from_slice(&self.staged_span[i * width..(i + 1) * width]);
            }
            result = self.finish(g, local, length);
            if result.is_err() {
                break;
            }
        }
        staged.clear();
        self.staged = staged;
        self.staged_order = order;
        self.staged_buckets.clear();
        self.staged_span.clear();
        result?;
        // A span's candidates that scored into the top k are checked now,
        // so the threshold their matches raise prunes the next chunk.
        if !self.pending.is_empty() {
            self.verify_pending(g)?;
        }
        Ok(())
    }

    /// Bounds the candidate at slot `local` of group `g` by what can be
    /// read before its length: `None` when it cannot be kept, else its
    /// bound and its length when that is known (else `u32::MAX`), with its
    /// buckets in [`Walk::buckets`] and, for a span, its posting indexes in
    /// [`Walk::span_index`].
    fn stage(&mut self, g: u32, local: u32) -> Result<Option<(f32, u32)>> {
        let n = self.sc.len();
        if let Some(dead) = self.segment.liveness.groups[g as usize].as_deref()
            && dead[local as usize / 64] >> (local % 64) & 1 == 1
        {
            return Ok(None);
        }
        self.answer.candidates += 1;
        let tid = self.geometry.tid_in(g as usize, local);
        let staged = match self.inline {
            // The length alone, against the window's largest buckets, from
            // a rare required term's record that carries lengths.
            Some(t) => {
                let index = self.mems[t]
                    .find(local)
                    .expect("a candidate holds every required term");
                let set = self.terms[t].as_ref().expect("required");
                let inline = set.postings.lengths.as_ref().expect("inline lengths");
                self.touch
                    .touch(Part::Payload, set.at + inline.at_of(index), 4);
                let length = inline.get(index)?;
                if n > 0 && self.length_cut(length, tid) {
                    return Ok(None);
                }
                self.read_buckets(g, local, tid)?;
                if self.too_few() {
                    return Ok(None);
                }
                // The length known, its buckets bound it at that length.
                (
                    self.reach((0..n).filter(|i| self.buckets[*i] != NO_BUCKET), length),
                    length,
                )
            }
            // The candidate's buckets first, each against the shortest
            // document of its block holding that bucket or more (the
            // footer's frontier), and the DL sidecar only for a candidate
            // they leave in reach: at 150 million rows two in three scored
            // candidates fell short on their buckets alone.
            None => {
                let reach = self.read_buckets(g, local, tid)?;
                if self.cut(reach, || tid) || self.too_few() {
                    return Ok(None);
                }
                (reach, u32::MAX)
            }
        };
        self.span_indexes(local);
        Ok(Some(staged))
    }

    /// Scores and admits the candidate at slot `local` of group `g`, staged
    /// with its buckets in [`Walk::buckets`] (and a span's posting indexes
    /// in [`Walk::span_index`]); `length` is its length, or `u32::MAX` to
    /// read it from the DL sidecar.
    fn finish(&mut self, g: u32, local: u32, length: u32) -> Result<()> {
        let n = self.sc.len();
        let tid = self.geometry.tid_in(g as usize, local);
        let length = if length == u32::MAX {
            self.dl_length(g, local)?
        } else {
            length
        };
        // A span checks positions before scoring while the top k fill:
        // every match enters them.
        let span_first = matches!(self.verify, Verify::Span(_)) && self.top.bar().is_none();
        if span_first {
            let Verify::Span(check) = &mut self.verify else {
                unreachable!()
            };
            self.answer.position_checks += 1;
            if !check.holds(&self.span_index, self.touch)? {
                return Ok(());
            }
        }
        // A walk with nothing to score (every term elided) scores nothing.
        self.answer.scored += u64::from(n > 0);
        let mut total = 0.0_f32;
        for i in 0..n {
            let bucket = self.buckets[i];
            if bucket == NO_BUCKET {
                continue;
            }
            total += self.sc[i]
                .scorer
                .score_bucket(TfBucket::new(bucket).ok_or(Error::InvalidTfBucket)?, length);
        }
        if !self.admits(total, tid) {
            return Ok(());
        }
        match &self.verify {
            Verify::Flat => self.push(total, tid),
            Verify::Span(_) if span_first => self.push(total, tid),
            Verify::Span(_) => {
                self.pending.push((total, local));
                self.pending_index.extend_from_slice(&self.span_index);
            }
            Verify::Node => {
                let slot = self.geometry.groups[g as usize].slot_base + local;
                if matches(
                    self.segment,
                    self.node,
                    &mut self.node_terms,
                    slot,
                    &mut self.positions,
                    self.touch,
                )? {
                    self.push(total, tid);
                }
            }
        }
        Ok(())
    }

    /// Reads into [`Walk::buckets`] the bucket of each scoring term the
    /// candidate at slot `local` of group `g` holds ([`NO_BUCKET`] for the
    /// others), and returns what the candidate can score at most by them:
    /// see [`Walk::reach`], taken only when each term's own bound (kept per
    /// block and bucket) leaves the candidate, of ctid `tid`, in reach.
    fn read_buckets(&mut self, g: u32, local: u32, tid: Tid) -> Result<f32> {
        let (mut floor, mut low, mut own) = (0u32, u32::MAX, 0.0_f32);
        for i in 0..self.sc.len() {
            let t = self.sc[i].term;
            if !self.sc_required[i] {
                self.ensure(t, g)?;
            }
            let Some(index) = self.mems[t].find(local) else {
                self.buckets[i] = NO_BUCKET;
                continue;
            };
            let s = &mut self.sc[i];
            let set = self.terms[t].as_ref().expect("scoring");
            let bucket = s.footer.bucket(set.postings.tf, index)?;
            let block = (index / s.footer.block_size()) as usize;
            if s.footer.single().is_none() {
                self.touch.touch(
                    Part::TfTail,
                    set.at + set.postings.tf_at + s.footer.tf_at(block) as usize,
                    1,
                );
            }
            let (f, bound) = self.sc[i].floor_kept(block, bucket);
            (floor, low) = (floor.max(f), low.min(f));
            own += bound;
            self.buckets[i] = bucket;
            self.held_index[i] = index;
        }
        // Each term's own bound is at least the shared one: it prunes what
        // it can without a division per term, and is the shared one when
        // the terms' lengths agree.
        if low >= floor || self.cut(own, || tid) {
            return Ok(own);
        }
        Ok(self.reach(
            (0..self.sc.len()).filter(|i| self.buckets[*i] != NO_BUCKET),
            floor,
        ))
    }

    /// What a candidate holding the scoring terms `held` (in the scorer's
    /// order), with the buckets in [`Walk::buckets`], scores at most when it
    /// is at least `floor` long. A document has one length, and every term
    /// it holds bounds that from below: by the shortest length its block
    /// holds at the candidate's bucket or above. The longest of those
    /// bounds every term's score (on TIN's corpus, where a word's count
    /// grows with the length, it left a seventh of the candidates in reach
    /// that each term's own bound did).
    #[inline]
    fn reach(&self, held: impl Iterator<Item = usize>, floor: u32) -> f32 {
        let mut reach = 0.0_f32;
        for i in held {
            let bucket = TfBucket::new(self.buckets[i]).expect("a valid bucket");
            reach += self.sc[i].scorer.bound_through(bucket, floor);
        }
        reach
    }

    /// Whether the candidate at hand, its buckets read, holds a word its
    /// phrase repeats fewer times than the phrase does (its bucket's counts
    /// all fall short), so cannot match.
    #[inline]
    fn too_few(&self) -> bool {
        self.repeats
            .iter()
            .any(|&(i, least)| self.buckets[i] < least)
    }

    /// The length of the document at slot `local` of group `g`, from the DL
    /// sidecar.
    #[inline]
    fn dl_length(&mut self, g: u32, local: u32) -> Result<u32> {
        let rank = self
            .segment
            .docs
            .rank_in(g as usize, local)
            .ok_or(Error::Corrupt("a posting without a document"))?;
        let (header, bits) = self.segment.length_at(rank);
        self.touch.touch(Part::DlSidecar, header, 8);
        self.touch.touch(Part::DlSidecar, bits, 4);
        self.segment.lengths.get(rank)
    }

    /// The candidate's posting index in each span slot's term.
    fn span_indexes(&mut self, local: u32) {
        let Verify::Span(check) = &self.verify else {
            return;
        };
        self.span_index.clear();
        for &t in &check.slots {
            // A scoring term's index was found with its bucket.
            let s = self.term_sc[t];
            let index = if s != usize::MAX && self.buckets[s] != NO_BUCKET {
                self.held_index[s]
            } else {
                self.mems[t]
                    .find(local)
                    .expect("a candidate holds every span term")
            };
            self.span_index.push(index);
        }
    }

    /// Checks the group's pending phrase candidates, best first, each
    /// against the threshold as it then stands.
    fn verify_pending(&mut self, g: u32) -> Result<()> {
        let width = match &self.verify {
            Verify::Span(check) => check.slots.len(),
            _ => 0,
        };
        let mut order: Vec<usize> = (0..self.pending.len()).collect();
        order.sort_by(|a, b| {
            let (x, y) = (self.pending[*a], self.pending[*b]);
            y.0.total_cmp(&x.0).then(x.1.cmp(&y.1))
        });
        for i in order {
            let (total, local) = self.pending[i];
            let tid = self.geometry.tid_in(g as usize, local);
            if !self.admits(total, tid) {
                continue;
            }
            let Verify::Span(check) = &mut self.verify else {
                unreachable!()
            };
            self.answer.position_checks += 1;
            if check.holds(&self.pending_index[i * width..(i + 1) * width], self.touch)? {
                self.push(total, tid);
            }
        }
        self.pending.clear();
        self.pending_index.clear();
        Ok(())
    }
}

/// Every row is visible.
struct Everything;

impl Visibility for Everything {
    fn visible(&mut self, _: Tid) -> bool {
        true
    }
}

/// [`super::top_k`]: the walk into fresh rows, then the zero fill.
pub(super) fn top_k(
    segment: &Segment<'_>,
    node: &Node,
    names: &[String],
    scorers: &[(String, TermScorer)],
    k: usize,
    touch: &mut impl Touch,
) -> Result<RankedAnswer> {
    let mut top = TopRows::new(k, false);
    let mut answer = RankedAnswer::default();
    let mut terms = walk_into(
        segment,
        node,
        names,
        scorers,
        &mut top,
        &mut Everything,
        touch,
        &mut answer,
    )?;
    let mut rows = top.into_rows();
    // Zero fill: fewer than k rows, the rest are matches of non-scoring
    // terms in ctid order (a led walk saw every match already).
    if rows.len() < k
        && required_terms(node).is_empty()
        && let Some(terms) = terms.as_mut()
    {
        let seen: std::collections::HashSet<Tid> = rows.iter().map(|r| r.1).collect();
        let fill = super::first_matches(segment, node, terms, k - rows.len(), &seen, touch)?;
        rows.extend(fill.into_iter().map(|t| (0.0, t)));
        rows.sort_by(crate::walk::rank);
        rows.truncate(k);
    }
    answer.rows = rows;
    Ok(answer)
}

/// [`super::top_k_into`]: walks `segment` into `top`, keeping only rows
/// `visibility` passes, and adds what it did to `answer`. Returns the
/// opened terms, for a zero fill; `None` when nothing can match.
#[expect(
    clippy::too_many_arguments,
    reason = "the walk's inputs and its two outputs"
)]
pub(super) fn walk_into<'a>(
    segment: &Segment<'a>,
    node: &Node,
    names: &[String],
    scorers: &[(String, TermScorer)],
    top: &mut TopRows,
    visibility: &mut dyn Visibility,
    touch: &mut impl Touch,
    answer: &mut RankedAnswer,
) -> Result<Option<Vec<Option<TermSet<'a>>>>> {
    let k = top.k();
    let required = required_terms(node);
    // A segment lacking a required term matches nothing: its terms'
    // records (their group directories) are not parsed.
    for t in &required {
        if !segment.holds_memo(&names[*t])? {
            return Ok(None);
        }
    }
    // Pages pinned from here on may be released a group at a time when
    // nothing the walk keeps borrows them (see `Walk::group_done`).
    let blob = segment.bytes.blob();
    let opened = blob.map(LazyBlob::pin_mark);
    let terms = open_terms(segment, names, touch)?;
    if k == 0 || required.iter().any(|t| terms[*t].is_none()) {
        return Ok(None);
    }
    let mut sc: Vec<Sc> = Vec::new();
    for (name, scorer) in scorers {
        let Some(t) = names.iter().position(|n| n == name) else {
            continue;
        };
        let Some(set) = &terms[t] else { continue };
        // Decoded per query, a block at a time as the walk reaches it, and
        // read as far (recorded as read when the walk ends).
        let footer =
            set.postings
                .lazy_footer(segment.block_size, set.max_bucket, segment.adaptive_tf)?;
        sc.push(Sc::new(t, scorer.clone(), footer));
    }
    // Leaves only: a candidate holding every required term matches a flat
    // AND, and one holding a scoring term a flat OR.
    let flat = match node {
        Node::And(c) | Node::Or(c) => c.iter().all(|c| matches!(c, Node::Term(_))),
        Node::Term(_) => true,
        _ => false,
    };
    let verify = if flat {
        Verify::Flat
    } else if let Node::Span { slots, query } = node {
        span_check(slots, query, &terms)?
    } else {
        Verify::Node
    };
    let node_terms = if matches!(verify, Verify::Node) {
        open_terms(segment, names, touch)?
    } else {
        Vec::new()
    };
    let mut req: Vec<usize> = required.clone();
    req.sort_by_key(|t| terms[*t].as_ref().expect("checked").df);
    let n = sc.len();
    let mut walk = Walk {
        segment,
        geometry: &segment.docs.geometry,
        node,
        top,
        visibility,
        mems: (0..terms.len()).map(|_| Mem::default()).collect(),
        terms,
        sc_required: Vec::with_capacity(n),
        present: vec![false; n],
        present_list: Vec::with_capacity(n),
        held_list: Vec::with_capacity(n),
        blocks: vec![0; n],
        sub_bounds: Vec::new(),
        plan: Plan::default(),
        rows: Vec::new(),
        row_state: vec![Row::default(); n],
        row_loaded: Vec::new(),
        any: Vec::new(),
        or_terms: Vec::new(),
        or_held: Vec::new(),
        buckets: vec![NO_BUCKET; n],
        held_index: vec![0; n],
        term_sc: Vec::new(),
        window_end: None,
        window_bound: 0.0,
        window_parts: (0.0, f64::INFINITY, f64::INFINITY),
        window_buckets: vec![NO_BUCKET; n],
        inline: None,
        repeats: Vec::new(),
        req,
        answer: RankedAnswer::default(),
        verify,
        pending: Vec::new(),
        pending_index: Vec::new(),
        span_index: Vec::new(),
        staged: Vec::new(),
        staged_buckets: Vec::new(),
        staged_span: Vec::new(),
        staged_order: Vec::new(),
        cands: Vec::new(),
        words: Vec::new(),
        positions: Vec::new(),
        node_terms,
        touch,
        sc,
        frame: None,
        held: Vec::new(),
    };
    walk.term_sc = vec![usize::MAX; walk.terms.len()];
    for i in 0..n {
        let r = required.contains(&walk.sc[i].term);
        walk.sc_required.push(r);
        walk.term_sc[walk.sc[i].term] = i;
    }
    if let Verify::Span(check) = &walk.verify
        && let Some(plan) = &check.plan
    {
        // A phrase's leaves sit at distinct positions: a word that is
        // several of them needs a count of at least that many.
        for i in 0..n {
            let uses = plan
                .leaves()
                .iter()
                .filter(|slot| check.slots[**slot] == walk.sc[i].term)
                .count() as u32;
            if uses > 1 {
                walk.repeats.push((i, TfBucket::from_count(uses).value()));
            }
        }
    }
    walk.inline = walk.req.iter().copied().find(|t| {
        walk.terms[*t]
            .as_ref()
            .is_some_and(|s| s.postings.lengths.is_some())
    });
    // A node check's cursors keep a group's grid between seeks: its walk
    // holds what it pins. Otherwise the pages read to open the terms
    // (records, directories, decoded) are released with the
    // groups', but for the ranges the records keep slices of.
    if let (Some(blob), Some(opened)) = (blob, opened)
        && !matches!(walk.verify, Verify::Node)
    {
        for set in walk.terms.iter().flatten() {
            for (from, to) in set.postings.borrowed().into_iter().flatten() {
                walk.held.push((set.at + from, set.at + to));
            }
        }
        // The pages those ranges lie on, read to open the terms, are held
        // to the walk's end before the mark the groups release to.
        walk.frame = Some((blob, blob.hold_since(opened, &walk.held)));
        // A footer's pages hold only its bytes: each is released once the
        // footer has read past it, not left pinned to the group's end.
        for s in &mut walk.sc {
            s.footer.release_passed();
        }
    }
    if required.is_empty() {
        walk.run_or()?;
    } else {
        walk.run()?;
    }
    walk.count_metadata();
    let done = walk.answer;
    answer.scored += done.scored;
    answer.windows += done.windows;
    answer.windows_pruned += done.windows_pruned;
    answer.candidates += done.candidates;
    answer.position_checks += done.position_checks;
    Ok(Some(walk.terms))
}

fn span_check<'a>(
    slots: &[usize],
    query: &SpanQuery,
    terms: &[Option<TermSet<'a>>],
) -> Result<Verify<'a>> {
    let solver = SpanSolver::new(query).map_err(|_| Error::Corrupt("span query"))?;
    let set = |slot: usize| terms[slots[slot]].as_ref().expect("a span's terms exist");
    let plan = PhrasePlan::new(query, |slot| u64::from(set(slot).df));
    let cursors = (0..slots.len())
        .map(|slot| PosCursor::new(set(slot).positions))
        .collect::<Result<Vec<_>>>()?;
    let first = (0..slots.len())
        .map(|slot| {
            slots
                .iter()
                .position(|t| *t == slots[slot])
                .expect("the slot itself")
        })
        .collect();
    Ok(Verify::Span(Box::new(SpanCheck {
        slots: slots.to_vec(),
        first,
        cursors,
        solver,
        plan,
        positions: vec![Vec::new(); slots.len()],
        read: vec![false; slots.len()],
    })))
}

/// Words of a disjunction's sub-range: a group's members are bounded by
/// their terms' blocks over this many words (1,024 slots) at a time.
const SUB_WORDS: usize = 16;

/// A disjunction's plan for a group at one threshold (MaxScore with the
/// group's bounds).
#[derive(Default)]
struct Plan {
    /// Scoring terms (indexes into the walk's) every candidate must hold:
    /// without any one, the others' bounds cannot reach the threshold.
    required: Vec<usize>,
    /// The terms a candidate must hold one of, when `by_essential`.
    essential: Vec<usize>,
    by_essential: bool,
    order: Vec<(f32, usize)>,
}

/// Sets a group's local slots, given in ascending order, in a row of words
/// cleared before: the word at hand's bits are kept in a register and
/// stored whole at each member, so a member never reads back the word the
/// one before it stored.
struct RowBits<'r> {
    row: &'r mut [u64],
    w: usize,
    bits: u64,
}

impl<'r> RowBits<'r> {
    fn new(row: &'r mut [u64]) -> Self {
        Self { row, w: 0, bits: 0 }
    }

    #[inline]
    fn set(&mut self, local: u32) {
        let w = local as usize / 64;
        debug_assert!(w >= self.w, "local slots in ascending order");
        let kept = if w == self.w { self.bits } else { 0 };
        self.bits = kept | 1 << (local % 64);
        self.w = w;
        self.row[w] = self.bits;
    }
}

/// A disjunction term's members in the group at hand, as a row of words in
/// [`Walk::rows`], and a forward cursor over their posting indexes.
#[derive(Clone, Copy, Default)]
struct Row {
    /// Postings in earlier groups.
    first: u32,
    /// Members in words `..w` are `run`.
    w: usize,
    run: u32,
}

impl<'a, T: Touch> Walk<'_, 'a, T> {
    /// Block-max MaxScore over the scoring terms, a group at a time: a
    /// group whose terms' bounds cannot reach the threshold is skipped; else
    /// it is planned at its bounds (required terms ANDed, essential terms
    /// ORed into its mask), and each member of the mask is weighed by the
    /// bounds over its sub-range of the terms it holds; each one left is
    /// bounded by its terms' blocks, then by its buckets and length, then
    /// scored.
    fn run_or(&mut self) -> Result<()> {
        let n = self.sc.len();
        if n == 0 {
            return Ok(());
        }
        let mut next = vec![0u32; n];
        let mut from = 0u32;
        let mut fresh = vec![false; n];
        loop {
            let mut g = u32::MAX;
            for i in 0..n {
                if !fresh[i] || next[i] < from {
                    let t = self.sc[i].term;
                    let set = self.terms[t].as_mut().expect("scoring");
                    next[i] =
                        next_group(set, self.geometry, from, &mut self.mems[t].hint, self.touch)
                            .unwrap_or(u32::MAX);
                    fresh[i] = true;
                }
                g = g.min(next[i]);
            }
            if g == u32::MAX {
                break;
            }
            self.present_list.clear();
            for (i, (present, next)) in self.present.iter_mut().zip(&next).enumerate() {
                *present = *next == g;
                if *present {
                    self.present_list.push(i);
                }
            }
            self.or_group(g)?;
            self.group_done();
            from = g + 1;
        }
        Ok(())
    }

    /// Scoring term `i`'s members in group `g`, which it holds, into its row
    /// of `words` words.
    #[inline(never)]
    fn or_load(&mut self, i: usize, g: u32, words: usize) -> Result<()> {
        let t = self.sc[i].term;
        let group = self.geometry.groups[g as usize];
        let row = &mut self.rows[i * words..(i + 1) * words];
        let set = self.terms[t].as_mut().expect("scoring");
        let r = &mut self.row_state[i];
        r.w = 0;
        r.run = 0;
        if let Form::Sparse(list) = &set.postings.form {
            row.fill(0);
            let cursor = set.sparse.get_or_insert_with(|| {
                self.touch.touch(
                    Part::Payload,
                    set.at + set.postings.payload_at,
                    set.postings.payload.len(),
                );
                list.cursor()
            });
            cursor.seek(group.slot_base);
            r.first = cursor.rank() as u32;
            let end = group.slot_base + group.slots();
            let mut out = RowBits::new(row);
            cursor.drain_below(end, |slot| out.set(slot - group.slot_base));
            return Ok(());
        }
        let entry = set
            .find(g, &mut self.mems[t].hint)
            .ok_or(Error::Corrupt("a group the term holds"))?;
        match entry.src {
            Src::Container {
                entry: e,
                bytes,
                at,
            } => {
                self.touch.touch(Part::Payload, at, bytes.len());
                let bytes = bytes.all()?;
                self.mems[t].used += 1;
                r.first = e.first;
                if e.kind == KIND_GRID {
                    kernels::load(row, bytes);
                } else {
                    row.fill(0);
                    let mut out = RowBits::new(row);
                    for_each_local(&e, bytes, &group, |l| out.set(l))?;
                }
            }
            Src::Locals { from, to } => {
                r.first = from;
                row.fill(0);
                let mut out = RowBits::new(row);
                for l in &set.locals[from as usize..to as usize] {
                    out.set(*l);
                }
            }
        }
        Ok(())
    }

    #[inline(never)]
    fn or_group(&mut self, g: u32) -> Result<()> {
        let n = self.sc.len();
        let group = self.geometry.groups[g as usize];
        let base = group.slot_base;
        let end = base + group.slots() - 1;
        self.answer.windows += 1;
        // The present terms' bounds over the group.
        let mut sb = std::mem::take(&mut self.sub_bounds);
        sb.clear();
        sb.resize(n, 0.0);
        let mut total = 0.0_f32;
        for i in 0..n {
            if self.present[i] {
                sb[i] = self.group_term_bound(i, g, base, end);
                total += sb[i];
            }
        }
        let geometry = self.geometry;
        let gi = g as usize;
        if self.cut(total, || geometry.tid_in(gi, 0)) {
            self.answer.windows_pruned += 1;
            self.sub_bounds = sb;
            return Ok(());
        }
        let words = group.words();
        // Rows are overwritten where read: only present terms' are.
        if self.rows.len() < n * words {
            self.rows.resize(n * words, 0);
        }
        let dead = self.segment.liveness.groups[g as usize].as_deref();
        let mut plan = std::mem::take(&mut self.plan);
        // The group's plan at its own bounds and the threshold now: a
        // document holding none of its essential terms (or lacking one of
        // its required ones) cannot reach the threshold anywhere in the
        // group, which only rises. Those terms' rows are read first and
        // combined into the group's mask; a group whose mask is empty reads
        // nothing else, and a sub-range whose mask is empty is skipped
        // before its bounds are taken.
        let real: f64 = sb.iter().map(|b| f64::from(*b)).sum();
        self.plan_group(&mut plan, self.threshold(), &sb, real);
        let mut loaded = std::mem::take(&mut self.row_loaded);
        loaded.clear();
        loaded.resize(n, false);
        let mut mask = std::mem::take(&mut self.words);
        mask.clear();
        mask.resize(words, 0);
        let filters = plan.required.len()
            + if plan.by_essential {
                plan.essential.len()
            } else {
                0
            };
        if filters == 0 {
            mask.fill(!0);
        }
        if let Some((&first, rest)) = plan.required.split_first() {
            self.or_load(first, g, words)?;
            loaded[first] = true;
            mask.copy_from_slice(&self.rows[first * words..(first + 1) * words]);
            for &i in rest {
                self.or_load(i, g, words)?;
                loaded[i] = true;
                kernels::and_words(&mut mask, &self.rows[i * words..(i + 1) * words]);
            }
        }
        if plan.by_essential && !plan.essential.is_empty() {
            let required = !plan.required.is_empty();
            let mut any = std::mem::take(&mut self.any);
            any.clear();
            any.resize(words, 0);
            for e in 0..plan.essential.len() {
                let i = plan.essential[e];
                self.or_load(i, g, words)?;
                loaded[i] = true;
                kernels::or_words(&mut any, &self.rows[i * words..(i + 1) * words]);
            }
            if required {
                kernels::and_words(&mut mask, &any);
            } else {
                mask.copy_from_slice(&any);
            }
            self.any = any;
        }
        if let Some(dead) = dead {
            kernels::andnot_words(&mut mask, dead);
        }
        if !kernels::any_set(&mask) {
            self.answer.windows_pruned += 1;
            self.words = mask;
            self.row_loaded = loaded;
            self.sub_bounds = sb;
            self.plan = plan;
            return Ok(());
        }
        for (i, done) in loaded.iter().enumerate() {
            if self.present[i] && !done {
                self.or_load(i, g, words)?;
            }
        }
        self.row_loaded = loaded;
        // Each member of the mask weighed alone: the bounds of the terms it
        // holds, summed, must reach the threshold (a word's members first
        // all at once, by the terms holding any of them). A word is weighed
        // by the group's bounds first, and only a word they leave in reach by
        // its sub-range's (1,024 slots), taken once per sub-range: tighter,
        // but a range bound per term. A member's blocks bound it no higher,
        // so one this leaves below the threshold would fail its candidate
        // check. At 150 million rows a group's mask holds ~85 members in a
        // third of its words; weighing them alone, without a plan per
        // sub-range, cost less than sieving every word of it.
        let mut terms = std::mem::take(&mut self.or_terms);
        let mut held = std::mem::take(&mut self.or_held);
        terms.clear();
        for p in 0..self.present_list.len() {
            let i = self.present_list[p];
            if sb[i] > 0.0 {
                terms.push((i * words, sb[i], 0.0));
            }
        }
        held.clear();
        held.resize(terms.len(), 0);
        // A word's members are bounded from its first slot's ctid (see
        // `Walk::cut`), each member from its own.
        let first = |w: usize| geometry.tid_in(gi, (w * 64) as u32);
        let mut w0 = 0;
        while w0 < words {
            let w1 = (w0 + SUB_WORDS).min(words);
            // The live words, walked by their bits rather than tested.
            let mut live = 0u32;
            for (j, word) in mask[w0..w1].iter().enumerate() {
                live |= u32::from(*word != 0) << j;
            }
            let mut bounded = false;
            while live != 0 {
                let w = w0 + live.trailing_zeros() as usize;
                live &= live - 1;
                let mut word = mask[w];
                if self.threshold().is_some() {
                    let mut most = 0.0_f32;
                    for (h, &(at, bound, _)) in held.iter_mut().zip(&terms) {
                        *h = self.rows[at + w];
                        most += if *h & word != 0 { bound } else { 0.0 };
                    }
                    if self.cut(most, || first(w)) {
                        continue;
                    }
                    if !bounded {
                        bounded = true;
                        let from = base + (w0 * 64) as u32;
                        let to = (base + (w1 * 64) as u32 - 1).min(end);
                        let mut total = 0.0_f32;
                        for term in terms.iter_mut() {
                            let bound = self.sc[term.0 / words].range_bound(from, to);
                            term.2 = bound;
                            total += bound;
                        }
                        // The sub-range's words left start at `w`.
                        if self.cut(total, || first(w)) {
                            break;
                        }
                    }
                    let mut most = 0.0_f32;
                    for (h, &(_, _, bound)) in held.iter().zip(&terms) {
                        most += if *h & word != 0 { bound } else { 0.0 };
                    }
                    if self.cut(most, || first(w)) {
                        continue;
                    }
                    let mut keep = 0u64;
                    let mut left = word;
                    while left != 0 {
                        let bit = left.trailing_zeros();
                        left &= left - 1;
                        let mut sum = 0.0_f32;
                        for (h, &(_, _, bound)) in held.iter().zip(&terms) {
                            sum += if *h >> bit & 1 == 1 { bound } else { 0.0 };
                        }
                        let tid = || geometry.tid_in(gi, (w * 64) as u32 + bit);
                        keep |= u64::from(!self.cut(sum, tid)) << bit;
                    }
                    word = keep;
                }
                while word != 0 {
                    let bit = word.trailing_zeros();
                    word &= word - 1;
                    self.or_candidate(g, base, words, w, bit)?;
                }
            }
            w0 = w1;
        }
        self.or_terms = terms;
        self.or_held = held;
        self.words = mask;
        self.sub_bounds = sb;
        self.plan = plan;
        Ok(())
    }

    /// Plans a group whose present terms' bounds are `sb` (summing to
    /// `total`) at threshold `theta`.
    #[inline(never)]
    fn plan_group(&self, plan: &mut Plan, theta: Option<f32>, sb: &[f32], total: f64) {
        plan.required.clear();
        plan.essential.clear();
        plan.order.clear();
        plan.by_essential = true;
        let present = (0..sb.len()).filter(|i| self.present[*i]);
        let t = theta.map_or(0.0, f64::from);
        // Without a positive threshold every member of every term may enter.
        if !(t > 0.0 && t.is_finite()) || sb.iter().any(|b| b.is_nan() || *b < 0.0) {
            plan.essential.extend(present);
            return;
        }
        let slack = 1.0 + f64::from(f32::EPSILON) * 256.0;
        let mut fixed = 0.0_f64;
        for i in present {
            if (total - f64::from(sb[i])) * slack < t {
                plan.required.push(i);
                fixed += f64::from(sb[i]);
            } else if sb[i] > 0.0 {
                plan.order.push((sb[i], i));
            }
        }
        plan.order.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let mut tail = fixed;
        let mut inessential = 0;
        for &(bound, _) in &plan.order {
            let next = tail + f64::from(bound);
            if next * slack >= t {
                break;
            }
            tail = next;
            inessential += 1;
        }
        plan.by_essential = plan.required.is_empty() || inessential > 0;
        plan.essential
            .extend(plan.order[inessential..].iter().map(|&(_, i)| i));
    }

    /// The posting index of member `local` of scoring term `i`'s row;
    /// members must be asked in increasing order.
    #[inline]
    fn row_index(&mut self, i: usize, words: usize, local: u32) -> u32 {
        let row = &self.rows[i * words..(i + 1) * words];
        let r = &mut self.row_state[i];
        let w = local as usize / 64;
        if w > r.w {
            r.run += kernels::popcount(&row[r.w..w]) as u32;
            r.w = w;
        }
        r.first + r.run + (row[w] & ((1u64 << (local % 64)) - 1)).count_ones()
    }

    /// Bounds, scores and admits the candidate at bit `bit` of word `w` of
    /// group `g`.
    ///
    /// Kept out of line, as the group and plan steps are: inlined into the
    /// word loop it measured 8% slower over the disjunction trace.
    #[inline(never)]
    fn or_candidate(&mut self, g: u32, base: u32, words: usize, w: usize, bit: u32) -> Result<()> {
        let local = (w * 64) as u32 + bit;
        if let Some((_, length)) = self.or_stage(g, base, words, w, bit)? {
            self.finish(g, local, length)?;
        }
        Ok(())
    }

    /// Bounds the candidate at bit `bit` of word `w` of group `g` by what
    /// can be read before its length from the DL sidecar (see
    /// [`Walk::stage`]): its terms' blocks, then its buckets, and a length
    /// a term it holds carries inline. Its buckets are left in
    /// [`Walk::buckets`] ([`NO_BUCKET`] for a term it lacks).
    fn or_stage(
        &mut self,
        g: u32,
        base: u32,
        words: usize,
        w: usize,
        bit: u32,
    ) -> Result<Option<(f32, u32)>> {
        let local = (w * 64) as u32 + bit;
        let slot = base + local;
        self.answer.candidates += 1;
        let tid = self.geometry.tid_in(g as usize, local);
        let mut bound = 0.0_f32;
        self.held_list.clear();
        for p in 0..self.present_list.len() {
            let i = self.present_list[p];
            if self.rows[i * words + w] >> bit & 1 == 1 {
                self.held_list.push(i);
                let s = &mut self.sc[i];
                let b = s.block_at(slot).expect("a member's block");
                self.blocks[i] = b;
                bound += s.bound(b);
            }
        }
        if self.cut(bound, || tid) {
            return Ok(None);
        }
        // The candidate's buckets, each against the shortest document of
        // its block holding that bucket or more, before its length is read
        // (see `Walk::stage`).
        self.buckets.fill(NO_BUCKET);
        let (mut floor, mut low, mut own) = (0u32, u32::MAX, 0.0_f32);
        for h in 0..self.held_list.len() {
            let i = self.held_list[h];
            let index = self.row_index(i, words, local);
            let s = &mut self.sc[i];
            let set = self.terms[s.term].as_ref().expect("scoring");
            let bucket = s.footer.bucket(set.postings.tf, index)?;
            if s.footer.single().is_none() {
                let block = (index / s.footer.block_size()) as usize;
                self.touch.touch(
                    Part::TfTail,
                    set.at + set.postings.tf_at + s.footer.tf_at(block) as usize,
                    1,
                );
            }
            let block = self.blocks[i];
            let (f, bound) = self.sc[i].floor_kept(block, bucket);
            (floor, low) = (floor.max(f), low.min(f));
            own += bound;
            self.buckets[i] = bucket;
        }
        // Each term's own bound first, as `Walk::read_buckets` does.
        if self.cut(own, || tid) {
            return Ok(None);
        }
        let reach = if low >= floor {
            own
        } else {
            self.reach(self.held_list.iter().copied(), floor)
        };
        if self.cut(reach, || tid) {
            return Ok(None);
        }
        let carrier = self.held_list.iter().copied().find(|i| {
            self.terms[self.sc[*i].term]
                .as_ref()
                .is_some_and(|s| s.postings.lengths.is_some())
        });
        Ok(Some(match carrier {
            Some(i) => {
                let index = self.row_index(i, words, local);
                let set = self.terms[self.sc[i].term].as_ref().expect("scoring");
                let inline = set.postings.lengths.as_ref().expect("inline lengths");
                self.touch
                    .touch(Part::Payload, set.at + inline.at_of(index), 4);
                let length = inline.get(index)?;
                // The length known, its buckets bound it at that length.
                (self.reach(self.held_list.iter().copied(), length), length)
            }
            None => (reach, u32::MAX),
        }))
    }
}
