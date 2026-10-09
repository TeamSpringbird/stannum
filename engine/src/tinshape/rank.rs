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
use segment::tinshape::docs::Geometry;
use segment::tinshape::ef::{Ef, EfCursor};
use segment::tinshape::positions::Positions;
use segment::tinshape::postings::{Footer, Form, KIND_EF, KIND_GRID, for_each_local};
use segment::tinshape::segment::Segment;
use segment::{Error, Result};

use super::{
    Node, Part, RankedAnswer, Src, TermSet, Touch, below, kernels, matches, open_terms,
    required_terms,
};
use crate::bm25::TermScorer;
use crate::walk::{Ranked, TopRows, Visibility};

/// A required term's Elias-Fano container in a group is probed for the
/// candidates left rather than decoded when it holds more than this many
/// times as many members.
const SEEK_RATIO: usize = 4;

/// [`Walk::buckets`] of a term the candidate does not hold.
const NO_BUCKET: u8 = u8::MAX;

/// One scoring term of a walk.
pub(super) struct Sc {
    pub(super) term: usize,
    pub(super) scorer: TermScorer,
    pub(super) footer: std::rc::Rc<Footer>,
    /// Per footer block, its bound; NaN until asked.
    bounds: Vec<f32>,
    /// The block a range bound starts from, moved forward only.
    wb: usize,
    /// The block holding the last candidate asked about, moved forward only.
    cb: usize,
    /// [`TermScorer::length_bound_parts`].
    num: [f32; BUCKET_COUNT],
    den: [f32; BUCKET_COUNT],
    factor: f32,
    /// [`Self::bucket_bound`] per bucket in the block last asked about,
    /// NaN until asked: a rare term's block spans many groups' candidates.
    bb_block: usize,
    bb: [f32; BUCKET_COUNT],
}

impl Sc {
    pub(super) fn new(term: usize, scorer: TermScorer, footer: std::rc::Rc<Footer>) -> Self {
        let (num, den, factor) = scorer.length_bound_parts();
        Self {
            term,
            bounds: vec![f32::NAN; footer.blocks()],
            footer,
            scorer,
            wb: 0,
            cb: 0,
            num,
            den,
            factor,
            bb_block: usize::MAX,
            bb: [f32::NAN; BUCKET_COUNT],
        }
    }

    /// Block `b`'s bound: the best score of its frontier.
    #[inline]
    fn bound(&mut self, b: usize) -> f32 {
        let v = self.bounds[b];
        if !v.is_nan() {
            return v;
        }
        let v = self
            .footer
            .frontier_of(b)
            .iter()
            .map(|(bucket, length)| {
                self.scorer
                    .bound_through(TfBucket::new(*bucket).expect("a valid bucket"), *length)
            })
            .fold(0.0_f32, f32::max);
        self.bounds[b] = v;
        v
    }

    /// The best bound of the blocks overlapping slots `from..=to`; ranges
    /// must be asked in increasing order of `from`.
    #[inline]
    fn range_bound(&mut self, from: u32, to: u32) -> f32 {
        let blocks = self.footer.last.len();
        while self.wb < blocks && self.footer.last[self.wb] < from {
            self.wb += 1;
        }
        let mut best = 0.0_f32;
        let mut b = self.wb;
        while b < blocks {
            best = best.max(self.bound(b));
            if self.footer.last[b] >= to {
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
        let last = &self.footer.last;
        while self.cb < last.len() && last[self.cb] < slot {
            self.cb += 1;
        }
        (self.cb < last.len()).then_some(self.cb)
    }

    /// What a posting of block `b` with bucket `bucket` scores at most: its
    /// document is no shorter than the shortest the block holds with that
    /// bucket or more, the shortest of the frontier's pairs at or above it
    /// (every posting is dominated by a frontier pair).
    #[inline]
    fn bucket_bound(&self, b: usize, bucket: u8) -> f32 {
        let length = self
            .footer
            .frontier_of(b)
            .iter()
            .filter(|(at, _)| *at >= bucket)
            .map(|(_, length)| *length)
            .min()
            .unwrap_or(0);
        self.scorer
            .bound_through(TfBucket::new(bucket).expect("a valid bucket"), length)
    }

    /// [`Self::bucket_bound`], kept per bucket for the block last asked
    /// about.
    #[inline]
    fn bucket_bound_kept(&mut self, b: usize, bucket: u8) -> f32 {
        if self.bb_block != b {
            self.bb_block = b;
            self.bb = [f32::NAN; BUCKET_COUNT];
        }
        let known = self.bb[usize::from(bucket)];
        if !known.is_nan() {
            return known;
        }
        let v = self.bucket_bound(b, bucket);
        self.bb[usize::from(bucket)] = v;
        v
    }

    /// Block `b`'s largest bucket: its frontier's last pair.
    #[inline]
    fn max_bucket(&self, b: usize) -> usize {
        usize::from(self.footer.frontier_of(b).last().expect("a frontier").0)
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
struct PosCursor<'a> {
    positions: Positions<'a>,
    at: usize,
    /// Entry `next` starts at byte `next_at`.
    next: u32,
    next_at: usize,
}

impl<'a> PosCursor<'a> {
    fn new(stream: (segment::tinshape::blob::Bytes<'a>, usize)) -> Result<Self> {
        let positions = Positions::parse(stream.0)?;
        Ok(Self {
            next: 0,
            next_at: positions.data_at,
            positions,
            at: stream.1,
        })
    }

    /// Reads entry `index` into `out`.
    fn read(&mut self, index: u32, out: &mut Vec<u32>, touch: &mut impl Touch) -> Result<()> {
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
    sc: Vec<Sc>,
    /// The scoring terms (indexes into `sc`), rarest first.
    bound_order: Vec<usize>,
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
    or_terms: Vec<(usize, f64, f64)>,
    or_held: Vec<u64>,
    /// Per scoring term, the candidate at hand's bucket ([`NO_BUCKET`]
    /// when it does not hold the term).
    buckets: Vec<u8>,
    /// The window: the last slot of every scoring term's footer block at
    /// the slot it was set at, and its bound, and its length bound's
    /// numerator, denominator and length factor.
    window_end: Option<u32>,
    window_parts: (f64, f64, f64, f64),
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
    cands: Vec<u32>,
    words: Vec<u64>,
    /// The node check's own cursors (the walk moves the terms' sparse
    /// cursors past each group), and its scratch.
    node_terms: Vec<Option<TermSet<'a>>>,
    positions: Vec<Vec<u32>>,
}

impl<'a, T: Touch> Walk<'_, 'a, T> {
    #[inline]
    fn threshold(&self) -> Option<f32> {
        self.top.bar().map(|bar| bar.0)
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
            if self.group_below(base, end) {
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
        }
        Ok(())
    }

    /// Whether the scoring terms' bounds over slots `base..=end` fall short
    /// of the threshold. Bounds are not negative, so the sum stops once it
    /// reaches the threshold; it is taken in [`Walk::bound_order`], rarest
    /// term first, which most often gets there soonest.
    #[inline]
    fn group_below(&mut self, base: u32, end: u32) -> bool {
        let Some(theta) = self.threshold() else {
            return false;
        };
        let mut bound = 0.0_f64;
        for o in 0..self.bound_order.len() {
            bound += f64::from(self.sc[self.bound_order[o]].range_bound(base, end));
            if !below(bound, Some(theta)) {
                return false;
            }
        }
        // Nothing scoring: every match scores zero, which ties a bar of zero.
        below(bound, Some(theta))
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
    /// once nothing later can enter the top k.
    fn walk_candidates(&mut self, g: u32, cands: &[u32]) -> Result<bool> {
        let base = self.geometry.groups[g as usize].slot_base;
        let mut more = true;
        let mut at = 0;
        while at < cands.len() {
            let local = cands[at];
            at += 1;
            if self.window(base + local) {
                // Skip the rest of the window unread.
                match self.window_end {
                    Some(e) if e < u32::MAX => {
                        at += cands[at..].partition_point(|l| base + *l <= e);
                        continue;
                    }
                    _ => break,
                }
            }
            if !self.process(g, local)? {
                more = false;
                break;
            }
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
            for (w, d) in words.iter_mut().zip(dead) {
                *w &= !d;
            }
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

    /// Moves the window to `slot` and says whether its bound falls short
    /// of the threshold: slots up to [`Self::window_end`] share every
    /// scoring term's footer block, so its bound and its length bound's
    /// parts.
    #[inline]
    fn window(&mut self, slot: u32) -> bool {
        if self.window_end.is_none_or(|e| slot > e) {
            let mut e = u32::MAX;
            let (mut bound, mut nsum, mut dmin, mut fmin) =
                (0.0_f64, 0.0_f64, f64::INFINITY, f64::INFINITY);
            for s in &mut self.sc {
                let Some(b) = s.block_at(slot) else { continue };
                bound += f64::from(s.bound(b));
                e = e.min(s.footer.last[b]);
                let mb = s.max_bucket(b);
                nsum += f64::from(s.num[mb]);
                dmin = dmin.min(f64::from(s.den[mb]));
                fmin = fmin.min(f64::from(s.factor));
            }
            self.window_end = Some(e);
            self.window_parts = (bound, nsum, dmin, fmin);
        }
        below(self.window_parts.0, self.threshold())
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
        let n = self.sc.len();
        if n == 0 && self.unscored_done(self.geometry.tid_in(g as usize, local)) {
            return Ok(false);
        }
        if let Some(dead) = self.segment.liveness.groups[g as usize].as_deref()
            && dead[local as usize / 64] >> (local % 64) & 1 == 1
        {
            return Ok(true);
        }
        self.answer.candidates += 1;
        let theta = self.threshold();
        let length = match self.inline {
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
                if theta.is_some() && n > 0 {
                    let (_, nsum, dmin, fmin) = self.window_parts;
                    if below(nsum / (dmin + fmin * f64::from(length)), theta) {
                        return Ok(true);
                    }
                }
                self.read_buckets(g, local)?;
                if self.too_few() {
                    return Ok(true);
                }
                length
            }
            // The candidate's buckets first, each against the shortest
            // document of its block holding that bucket or more (the
            // footer's frontier), and the DL sidecar only for a candidate
            // they leave in reach: at 150 million rows two in three scored
            // candidates fell short on their buckets alone.
            None => {
                let reach = self.read_buckets(g, local)?;
                if below(reach, theta) || self.too_few() {
                    return Ok(true);
                }
                self.dl_length(g, local)?
            }
        };
        // A span checks positions before scoring while the top k fill:
        // every match enters them.
        let tid = self.geometry.tid_in(g as usize, local);
        let span_first = matches!(self.verify, Verify::Span(_)) && self.top.bar().is_none();
        if span_first {
            self.span_indexes(local);
            let Verify::Span(check) = &mut self.verify else {
                unreachable!()
            };
            self.answer.position_checks += 1;
            if !check.holds(&self.span_index, self.touch)? {
                return Ok(true);
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
            return Ok(true);
        }
        match &self.verify {
            Verify::Flat => self.push(total, tid),
            Verify::Span(_) if span_first => self.push(total, tid),
            Verify::Span(_) => {
                self.span_indexes(local);
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
        Ok(true)
    }

    /// Reads into [`Walk::buckets`] the bucket of each scoring term the
    /// candidate at slot `local` of group `g` holds ([`NO_BUCKET`] for the
    /// others), and returns what the candidate can score at most by them:
    /// per term, its bucket at the shortest length its block holds for
    /// that bucket or more.
    fn read_buckets(&mut self, g: u32, local: u32) -> Result<f64> {
        let mut reach = 0.0_f64;
        for i in 0..self.sc.len() {
            let t = self.sc[i].term;
            if !self.sc_required[i] {
                self.ensure(t, g)?;
            }
            let Some(index) = self.mems[t].find(local) else {
                self.buckets[i] = NO_BUCKET;
                continue;
            };
            let s = &self.sc[i];
            let set = self.terms[t].as_ref().expect("scoring");
            let bucket = s.footer.bucket(set.postings.tf, index)?;
            let block = (index / s.footer.block_size) as usize;
            if s.footer.single.is_none() {
                self.touch.touch(
                    Part::TfTail,
                    set.at + set.postings.tf_at + s.footer.tf_at[block] as usize,
                    1,
                );
            }
            reach += f64::from(self.sc[i].bucket_bound_kept(block, bucket));
            self.buckets[i] = bucket;
        }
        Ok(reach)
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
            let index = self.mems[t]
                .find(local)
                .expect("a candidate holds every span term");
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
        let footer = segment.footer_memo(set.at, &set.postings, set.max_bucket)?;
        touch.touch(
            Part::Footer,
            set.at + set.postings.footer_at,
            set.postings.footer.len(),
        );
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
        window_end: None,
        window_parts: (0.0, 0.0, 0.0, 0.0),
        inline: None,
        repeats: Vec::new(),
        req,
        answer: RankedAnswer::default(),
        verify,
        pending: Vec::new(),
        pending_index: Vec::new(),
        span_index: Vec::new(),
        cands: Vec::new(),
        bound_order: Vec::new(),
        words: Vec::new(),
        positions: Vec::new(),
        node_terms,
        touch,
        sc,
    };
    for i in 0..n {
        let r = required.contains(&walk.sc[i].term);
        walk.sc_required.push(r);
    }
    let mut bound_order: Vec<usize> = (0..n).collect();
    bound_order.sort_by_key(|i| walk.terms[walk.sc[*i].term].as_ref().map_or(0, |s| s.df));
    walk.bound_order = bound_order;
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
    if required.is_empty() {
        walk.run_or()?;
    } else {
        walk.run()?;
    }
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
        let mut total = 0.0_f64;
        for (i, bound) in sb.iter_mut().enumerate() {
            if self.present[i] {
                *bound = self.sc[i].range_bound(base, end);
                total += f64::from(*bound);
            }
        }
        if below(total, self.threshold()) {
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
        self.plan_group(&mut plan, self.threshold(), &sb, total);
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
                for (m, r) in mask.iter_mut().zip(&self.rows[i * words..(i + 1) * words]) {
                    *m &= *r;
                }
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
                for (a, r) in any.iter_mut().zip(&self.rows[i * words..(i + 1) * words]) {
                    *a |= *r;
                }
            }
            if required {
                for (m, a) in mask.iter_mut().zip(&any) {
                    *m &= *a;
                }
            } else {
                mask.copy_from_slice(&any);
            }
            self.any = any;
        }
        if let Some(dead) = dead {
            for (m, d) in mask.iter_mut().zip(dead) {
                *m &= !d;
            }
        }
        if mask.iter().all(|m| *m == 0) {
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
                terms.push((i * words, f64::from(sb[i]), 0.0));
            }
        }
        held.clear();
        held.resize(terms.len(), 0);
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
                let theta = self.threshold();
                if theta.is_some() {
                    let mut most = 0.0_f64;
                    for (h, &(at, bound, _)) in held.iter_mut().zip(&terms) {
                        *h = self.rows[at + w];
                        most += if *h & word != 0 { bound } else { 0.0 };
                    }
                    if below(most, theta) {
                        continue;
                    }
                    if !bounded {
                        bounded = true;
                        let from = base + (w0 * 64) as u32;
                        let to = (base + (w1 * 64) as u32 - 1).min(end);
                        let mut total = 0.0_f64;
                        for term in terms.iter_mut() {
                            let bound = f64::from(self.sc[term.0 / words].range_bound(from, to));
                            term.2 = bound;
                            total += bound;
                        }
                        if below(total, theta) {
                            break;
                        }
                    }
                    let mut most = 0.0_f64;
                    for (h, &(_, _, bound)) in held.iter().zip(&terms) {
                        most += if *h & word != 0 { bound } else { 0.0 };
                    }
                    if below(most, theta) {
                        continue;
                    }
                    let mut keep = 0u64;
                    let mut left = word;
                    while left != 0 {
                        let bit = left.trailing_zeros();
                        left &= left - 1;
                        let mut sum = 0.0_f64;
                        for (h, &(_, _, bound)) in held.iter().zip(&terms) {
                            sum += if *h >> bit & 1 == 1 { bound } else { 0.0 };
                        }
                        keep |= u64::from(!below(sum, theta)) << bit;
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
        let slot = base + local;
        self.answer.candidates += 1;
        let theta = self.threshold();
        let mut bound = 0.0_f64;
        self.held_list.clear();
        for p in 0..self.present_list.len() {
            let i = self.present_list[p];
            if self.rows[i * words + w] >> bit & 1 == 1 {
                self.held_list.push(i);
                let s = &mut self.sc[i];
                let b = s.block_at(slot).expect("a member's block");
                self.blocks[i] = b;
                bound += f64::from(s.bound(b));
            }
        }
        if below(bound, theta) {
            return Ok(());
        }
        // The candidate's buckets, each against the shortest document of
        // its block holding that bucket or more, before its length is read
        // (see `Walk::process`).
        let mut reach = 0.0_f64;
        for h in 0..self.held_list.len() {
            let i = self.held_list[h];
            let index = self.row_index(i, words, local);
            let s = &self.sc[i];
            let set = self.terms[s.term].as_ref().expect("scoring");
            let bucket = s.footer.bucket(set.postings.tf, index)?;
            if s.footer.single.is_none() {
                let block = (index / s.footer.block_size) as usize;
                self.touch.touch(
                    Part::TfTail,
                    set.at + set.postings.tf_at + s.footer.tf_at[block] as usize,
                    1,
                );
            }
            reach += f64::from(s.bucket_bound(self.blocks[i], bucket));
            self.buckets[i] = bucket;
        }
        if below(reach, theta) {
            return Ok(());
        }
        let carrier = self.held_list.iter().copied().find(|i| {
            self.terms[self.sc[*i].term]
                .as_ref()
                .is_some_and(|s| s.postings.lengths.is_some())
        });
        let length = match carrier {
            Some(i) => {
                let index = self.row_index(i, words, local);
                let set = self.terms[self.sc[i].term].as_ref().expect("scoring");
                let inline = set.postings.lengths.as_ref().expect("inline lengths");
                self.touch
                    .touch(Part::Payload, set.at + inline.at_of(index), 4);
                inline.get(index)?
            }
            None => self.dl_length(g, local)?,
        };
        self.answer.scored += 1;
        let mut total = 0.0_f32;
        // The held terms are in the scorer's order: the sum is exact.
        for h in 0..self.held_list.len() {
            let i = self.held_list[h];
            total += self.sc[i].scorer.score_bucket(
                TfBucket::new(self.buckets[i]).ok_or(Error::InvalidTfBucket)?,
                length,
            );
        }
        let tid = self.geometry.tid_in(g as usize, local);
        if !self.admits(total, tid) {
            return Ok(());
        }
        match &self.verify {
            Verify::Flat => self.push(total, tid),
            _ => {
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
}
