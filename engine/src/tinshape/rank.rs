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
use segment::tinshape::postings::{Footer, Form, KIND_EF, KIND_GRID, for_each_local, or_into};
use segment::tinshape::segment::Segment;
use segment::{Error, Result};

use super::{
    Node, Part, RankedAnswer, Src, TermSet, Touch, below, kernels, matches, open_terms,
    required_terms,
};
use crate::bm25::TermScorer;
use crate::walk::{Ranked, TopRows, Visibility};
use segment::lanes::LaneSums;

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
    grid: bool,
    /// Slots in the group.
    end: u32,
    /// Grid: members in words `..w` are `run`. List: the next member is at
    /// `pos`.
    w: usize,
    run: u32,
    pos: usize,
    /// The term's group directory position, moved forward only.
    hint: usize,
}

impl Mem<'_> {
    /// The first member at or after `local`, if the group holds one.
    #[inline]
    fn next_from(&mut self, local: u32) -> Option<u32> {
        match &mut self.kind {
            Kind::Grid(bytes) => {
                let words = bytes.len() / 8;
                let mut w = local as usize / 64;
                if w >= words {
                    return None;
                }
                let mut word = bits::word(bytes, w) & (u64::MAX << (local % 64));
                loop {
                    if word != 0 {
                        return Some((w * 64) as u32 + word.trailing_zeros());
                    }
                    w += 1;
                    if w >= words {
                        return None;
                    }
                    word = bits::word(bytes, w);
                }
            }
            Kind::List => {
                let list = &self.list;
                let mut pos = self.pos;
                while pos < list.len() && list[pos] < local {
                    pos += 1;
                }
                self.pos = pos;
                list.get(pos).copied()
            }
            Kind::Cursor { cursor, offset } => {
                cursor.seek(*offset + local);
                cursor
                    .current()
                    .map(|slot| slot - *offset)
                    .filter(|l| *l < self.end)
            }
        }
    }

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
    while *hint < set.groups.len() && set.groups[*hint].index < from {
        *hint += 1;
    }
    set.groups.get(*hint).map(|g| g.index)
}

/// Sets `mem`'s count of `set`'s members in group `g` (which it holds)
/// without reading them: the directory's, or for a sparse term its share
/// of the term's postings by slots.
fn count_in(set: &TermSet<'_>, geometry: &Geometry, g: u32, mem: &mut Mem<'_>) {
    if let Form::Sparse(_) = &set.postings.form {
        let slots = u64::from(geometry.groups[g as usize].slots());
        mem.count = (u64::from(set.df) * slots / u64::from(geometry.slots).max(1)).max(1) as u32;
        mem.grid = false;
        return;
    }
    match set.find(g, &mut mem.hint) {
        Some(entry) => {
            mem.count = entry.count;
            mem.grid = matches!(entry.src, Src::Container { entry, .. } if entry.kind == KIND_GRID);
        }
        None => {
            mem.count = 0;
            mem.grid = false;
        }
    }
}

/// Loads `set`'s members in group `g`, which it holds: decoded into a list,
/// or, when `seek`, an Elias-Fano list left to be sought.
fn load<'a>(
    set: &mut TermSet<'a>,
    geometry: &Geometry,
    g: u32,
    mem: &mut Mem<'a>,
    seek: bool,
    touch: &mut impl Touch,
) -> Result<()> {
    mem.loaded = g + 1;
    mem.kind = Kind::List;
    mem.list.clear();
    mem.w = 0;
    mem.run = 0;
    mem.pos = 0;
    let group = geometry.groups[g as usize];
    mem.end = group.slots();
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
        if seek {
            mem.first = 0;
            mem.kind = Kind::Cursor {
                cursor: cursor.clone(),
                offset: group.slot_base,
            };
            return Ok(());
        }
        mem.first = cursor.rank() as u32;
        let end = group.slot_base + group.slots();
        let list = &mut mem.list;
        cursor.drain_below(end, |slot| list.push(slot - group.slot_base));
        mem.count = mem.list.len() as u32;
        return Ok(());
    }
    let entry = set
        .find(g, &mut mem.hint)
        .copied()
        .ok_or(Error::Corrupt("a group the term holds"))?;
    mem.count = entry.count;
    match entry.src {
        Src::Container {
            entry: e,
            bytes,
            at,
        } => {
            touch.touch(Part::Payload, at, bytes.len());
            mem.first = e.first;
            if e.kind == KIND_GRID {
                mem.kind = Kind::Grid(bytes);
            } else if seek && e.kind == KIND_EF {
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
    Ok(())
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
        let mut p = self.next_at;
        for i in self.next..index {
            p = self.positions.skip(i, p)?;
        }
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
    /// The window: the last slot of every scoring term's footer block at
    /// the slot it was set at, and its bound, and its length bound's
    /// numerator, denominator and length factor.
    window_end: Option<u32>,
    window_parts: (f64, f64, f64, f64),
    /// A required term whose record carries its documents' lengths, the
    /// led walk reads them from.
    inline: Option<usize>,
    answer: RankedAnswer,
    verify: Verify<'a>,
    /// Phrase candidates of the group that scored into the top k, awaiting
    /// their positions check: (score, local slot), and per one its posting
    /// index in each span slot's term, `span` apiece.
    pending: Vec<(f32, u32)>,
    pending_index: Vec<u32>,
    span_index: Vec<u32>,
    cands: Vec<u32>,
    order: Vec<usize>,
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
        let mut cursor = 0u32;
        while self.seek(lead, cursor)?.is_some() {
            let g = self.mems[lead].loaded - 1;
            let group = self.geometry.groups[g as usize];
            let (base, end) = (group.slot_base, group.slot_base + group.slots() - 1);
            cursor = end + 1;
            if n == 0 && self.unscored_done(self.geometry.tid_in(g as usize, 0)) {
                // Nothing scores: every later match ties at zero and ranks
                // after the bar.
                break;
            }
            self.answer.windows += 1;
            let mut bound = 0.0_f64;
            for i in 0..n {
                bound += f64::from(self.sc[i].range_bound(base, end));
            }
            if below(bound, self.threshold()) {
                self.answer.windows_pruned += 1;
                continue;
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

    /// Moves term `t` to its first member at or after `target`, loading
    /// (decoding) the group that holds it; the member's slot, if any.
    fn seek(&mut self, t: usize, mut target: u32) -> Result<Option<u32>> {
        let geometry = self.geometry;
        loop {
            if target >= geometry.slots {
                return Ok(None);
            }
            let mem = &self.mems[t];
            let within = mem.loaded > 0 && {
                let group = &geometry.groups[mem.loaded as usize - 1];
                group.slot_base <= target && target < group.slot_base + group.slots()
            };
            if !within {
                let g = geometry.group_of_slot(target) as u32;
                let set = self.terms[t].as_mut().expect("a term of the walk");
                let mem = &mut self.mems[t];
                let Some(next) = next_group(set, geometry, g, &mut mem.hint, self.touch) else {
                    return Ok(None);
                };
                if next > g {
                    target = geometry.groups[next as usize].slot_base;
                }
                load(set, geometry, next, mem, false, self.touch)?;
            }
            let mem = &mut self.mems[t];
            let group = &geometry.groups[mem.loaded as usize - 1];
            match mem.next_from(target - group.slot_base) {
                Some(local) => return Ok(Some(group.slot_base + local)),
                None => target = group.slot_base + group.slots(),
            }
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
    /// holds, term by term from the fewest (a grid's by bit, a list's by a
    /// merge), and walks the candidates left.
    fn group_and(&mut self, g: u32) -> Result<bool> {
        for r in 1..self.req.len() {
            let t = self.req[r];
            // A term loaded here already holds the group (a sparse term's
            // cursor has moved past it).
            if self.mems[t].loaded == g + 1 {
                continue;
            }
            let set = self.terms[t].as_mut().expect("required");
            if next_group(set, self.geometry, g, &mut self.mems[t].hint, self.touch) != Some(g) {
                return Ok(true);
            }
        }
        for r in 0..self.req.len() {
            let t = self.req[r];
            let set = self.terms[t].as_ref().expect("required");
            if self.mems[t].loaded != g + 1 {
                count_in(set, self.geometry, g, &mut self.mems[t]);
            }
        }
        let mut order = std::mem::take(&mut self.order);
        order.clear();
        order.extend_from_slice(&self.req);
        order.sort_by_key(|t| self.mems[*t].count);
        let mut cands = std::mem::take(&mut self.cands);
        cands.clear();
        for (o, &t) in order.iter().enumerate() {
            let mem = &self.mems[t];
            if mem.loaded != g + 1 || matches!(mem.kind, Kind::Cursor { .. }) {
                let set = self.terms[t].as_mut().expect("required");
                load(set, self.geometry, g, &mut self.mems[t], false, self.touch)?;
            }
            let mem = &self.mems[t];
            match (o, &mem.kind) {
                (0, Kind::Grid(bytes)) => {
                    for w in 0..bytes.len() / 8 {
                        let mut word = bits::word(bytes, w);
                        while word != 0 {
                            cands.push((w * 64) as u32 + word.trailing_zeros());
                            word &= word - 1;
                        }
                    }
                }
                (0, _) => cands.extend_from_slice(&mem.list),
                (_, Kind::Grid(bytes)) => {
                    cands.retain(|l| bytes[*l as usize / 8] >> (l % 8) & 1 == 1);
                }
                (_, _) => super::intersect_sorted(&mut cands, &mem.list),
            }
            if cands.is_empty() {
                break;
            }
        }
        self.order = order;
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
                count_in(set, self.geometry, g, &mut self.mems[t]);
                load(set, self.geometry, g, &mut self.mems[t], false, self.touch)?;
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
        if next_group(set, self.geometry, g, &mut mem.hint, self.touch) == Some(g) {
            count_in(set, self.geometry, g, mem);
            let seek = mem.count > 64;
            load(set, self.geometry, g, mem, seek, self.touch)?;
        } else {
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
        // The length alone, against the window's largest buckets: from a
        // rare required term's record when one carries lengths, else from
        // the DL sidecar.
        let length = match self.inline {
            Some(t) => {
                let index = self.mems[t]
                    .find(local)
                    .expect("a candidate holds every required term");
                let set = self.terms[t].as_ref().expect("required");
                let inline = set.postings.lengths.as_ref().expect("inline lengths");
                self.touch
                    .touch(Part::Payload, set.at + inline.at_of(index), 4);
                inline.get(index)?
            }
            None => self.dl_length(g, local)?,
        };
        if theta.is_some() && n > 0 {
            let (_, nsum, dmin, fmin) = self.window_parts;
            if below(nsum / (dmin + fmin * f64::from(length)), theta) {
                return Ok(true);
            }
        }
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
            let t = self.sc[i].term;
            if !self.sc_required[i] {
                self.ensure(t, g)?;
            }
            let Some(index) = self.mems[t].find(local) else {
                continue;
            };
            let s = &self.sc[i];
            let set = self.terms[t].as_ref().expect("scoring");
            let bucket = s.footer.bucket(set.postings.tf, index)?;
            if s.footer.single.is_none() {
                let block = (index / s.footer.block_size) as usize;
                self.touch.touch(
                    Part::TfTail,
                    set.at + set.postings.tf_at + s.footer.tf_at[block] as usize,
                    1,
                );
            }
            total += s
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
        window_end: None,
        window_parts: (0.0, 0.0, 0.0, 0.0),
        inline: None,
        req,
        answer: RankedAnswer::default(),
        verify,
        pending: Vec::new(),
        pending_index: Vec::new(),
        span_index: Vec::new(),
        cands: Vec::new(),
        order: Vec::new(),
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

/// Words of a disjunction's sub-range: a group is planned and sieved this
/// many words (1,024 slots) at a time.
const SUB_WORDS: usize = 16;

/// The sieve's target, in units of the threshold: a lane is kept when the
/// weights of the terms it holds, each strictly above its bound in units of
/// `threshold / SIEVE_TARGET`, reach it.
const SIEVE_TARGET: u32 = LaneSums::MAX_TARGET;

/// Bits per lane counter of the sub-range sieve (as [`LaneSums`]).
const SLICES: usize = 6;

/// A disjunction's plan for a sub-range at one threshold (MaxScore with the
/// sub-range's bounds, as STN3's word sieve).
#[derive(Default)]
struct Plan {
    /// Scoring terms (indexes into the walk's) every candidate must hold:
    /// without any one, the others' bounds cannot reach the threshold.
    required: Vec<usize>,
    /// The terms a candidate must hold one of, when `by_essential`.
    essential: Vec<usize>,
    by_essential: bool,
    /// The weighted sum's start (the required terms' weights) and the other
    /// present terms' weights, when `by_count`.
    by_count: bool,
    start: u32,
    adds: Vec<(usize, u32)>,
    order: Vec<(f32, usize)>,
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
    /// group, and a sub-range of it, whose terms' bounds cannot reach the
    /// threshold is skipped; else each sub-range is planned (required
    /// terms ANDed, essential terms ORed, the rest weighed bit-parallel)
    /// and its words sieved; each candidate is bounded by its terms' blocks,
    /// then by its length, then scored.
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
            cursor.drain_below(end, |slot| {
                let l = slot - group.slot_base;
                row[l as usize / 64] |= 1 << (l % 64);
            });
            return Ok(());
        }
        let entry = set
            .find(g, &mut self.mems[t].hint)
            .copied()
            .ok_or(Error::Corrupt("a group the term holds"))?;
        match entry.src {
            Src::Container {
                entry: e,
                bytes,
                at,
            } => {
                self.touch.touch(Part::Payload, at, bytes.len());
                r.first = e.first;
                if e.kind == KIND_GRID {
                    kernels::load(row, bytes);
                } else {
                    row.fill(0);
                    or_into(&e, bytes, &group, row)?;
                }
            }
            Src::Locals { from, to } => {
                r.first = from;
                row.fill(0);
                for l in &set.locals[from as usize..to as usize] {
                    row[*l as usize / 64] |= 1 << (l % 64);
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
        let mut total = 0.0_f64;
        for i in 0..n {
            if self.present[i] {
                total += f64::from(self.sc[i].range_bound(base, end));
            }
        }
        if below(total, self.threshold()) {
            self.answer.windows_pruned += 1;
            return Ok(());
        }
        let words = group.words();
        // Rows are overwritten where read: only present terms' are.
        if self.rows.len() < n * words {
            self.rows.resize(n * words, 0);
        }
        for i in 0..n {
            if self.present[i] {
                self.or_load(i, g, words)?;
            }
        }
        let dead = self.segment.liveness.groups[g as usize].as_deref();
        let mut sb = std::mem::take(&mut self.sub_bounds);
        sb.clear();
        sb.resize(n, 0.0);
        let mut plan = std::mem::take(&mut self.plan);
        // The plan's threshold and bounds: a block spans many sub-ranges, so
        // a plan usually holds for the next.
        let mut planned: Option<(Option<f32>, Vec<f32>)> = None;
        let mut w0 = 0;
        while w0 < words {
            let w1 = (w0 + SUB_WORDS).min(words);
            let from = base + (w0 * 64) as u32;
            let to = (base + (w1 * 64) as u32 - 1).min(end);
            let mut total = 0.0_f64;
            for (i, bound) in sb.iter_mut().enumerate() {
                *bound = if self.present[i] {
                    self.sc[i].range_bound(from, to)
                } else {
                    0.0
                };
                total += f64::from(*bound);
            }
            let theta = self.threshold();
            if below(total, theta) {
                w0 = w1;
                continue;
            }
            if planned
                .as_ref()
                .is_none_or(|(t, b)| *t != theta || b.as_slice() != sb.as_slice())
            {
                self.plan_sub(&mut plan, theta, &sb, total);
                match &mut planned {
                    Some((t, b)) => {
                        *t = theta;
                        b.clone_from(&sb);
                    }
                    None => planned = Some((theta, sb.clone())),
                }
            }
            let mut cand = [0u64; SUB_WORDS];
            self.sieve(&plan, words, w0, w1, &mut cand);
            for (j, word) in cand[..w1 - w0].iter().enumerate() {
                let w = w0 + j;
                let mut word = *word;
                if let Some(dead) = dead {
                    word &= !dead[w];
                }
                while word != 0 {
                    let bit = word.trailing_zeros();
                    word &= word - 1;
                    self.or_candidate(g, base, words, w, bit)?;
                }
            }
            w0 = w1;
        }
        self.sub_bounds = sb;
        self.plan = plan;
        Ok(())
    }

    /// Plans a sub-range whose present terms' bounds are `sb` (summing to
    /// `total`) at threshold `theta`.
    #[inline(never)]
    fn plan_sub(&self, plan: &mut Plan, theta: Option<f32>, sb: &[f32], total: f64) {
        plan.required.clear();
        plan.essential.clear();
        plan.adds.clear();
        plan.order.clear();
        plan.by_essential = true;
        plan.by_count = false;
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
        // The weights, in units of the threshold.
        let unit = t / f64::from(SIEVE_TARGET);
        let weight = |bound: f32| {
            let units = f64::from(bound) / unit;
            if units < f64::from(SIEVE_TARGET) {
                units.floor() as u32 + 1
            } else {
                SIEVE_TARGET
            }
        };
        let mut start = 0u32;
        for &i in &plan.required {
            start = start.saturating_add(weight(sb[i]));
        }
        for &(bound, i) in &plan.order {
            plan.adds.push((i, weight(bound)));
        }
        if start < SIEVE_TARGET {
            plan.by_count = true;
            plan.start = start;
        }
    }

    /// The candidates of words `w0..w1` under `plan`, term by term over the
    /// sub-range's words (the lane sums bit-sliced as [`LaneSums`]).
    #[inline]
    fn sieve(&self, plan: &Plan, words: usize, w0: usize, w1: usize, out: &mut [u64; SUB_WORDS]) {
        let len = w1 - w0;
        let rows = &self.rows;
        // A full sub-range's row is read in place; a short one padded.
        let row = |i: usize| -> [u64; SUB_WORDS] {
            let at = i * words + w0;
            if len == SUB_WORDS {
                *<&[u64; SUB_WORDS]>::try_from(&rows[at..at + SUB_WORDS]).expect("a full row")
            } else {
                let mut r = [0u64; SUB_WORDS];
                r[..len].copy_from_slice(&rows[at..at + len]);
                r
            }
        };
        let mut cand = [0u64; SUB_WORDS];
        if let Some((&first, rest)) = plan.required.split_first() {
            cand = row(first);
            for &i in rest {
                let r = row(i);
                for j in 0..SUB_WORDS {
                    cand[j] &= r[j];
                }
            }
        }
        if plan.by_essential {
            let mut any = [0u64; SUB_WORDS];
            for &i in &plan.essential {
                let r = row(i);
                for j in 0..SUB_WORDS {
                    any[j] |= r[j];
                }
            }
            if plan.required.is_empty() {
                cand = any;
            } else {
                for j in 0..SUB_WORDS {
                    cand[j] &= any[j];
                }
            }
        }
        let live = cand.iter().filter(|c| **c != 0).count();
        if plan.by_count && live > 0 && live <= SUB_WORDS / 4 {
            // Few words hold candidates: weigh those alone.
            for j in 0..len {
                if cand[j] != 0 {
                    let mut lanes = LaneSums::new(plan.start, SIEVE_TARGET);
                    for &(i, weight) in &plan.adds {
                        lanes.add(rows[i * words + w0 + j], weight);
                    }
                    cand[j] &= lanes.reached();
                }
            }
        } else if plan.by_count && live > 0 {
            // Each lane's counter starts at 2^SLICES - target + start, so it
            // reaches the target as it carries out of the top slice.
            let init = (1u32 << SLICES) - SIEVE_TARGET + plan.start;
            let mut slices = [[0u64; SUB_WORDS]; SLICES];
            for (s, slice) in slices.iter_mut().enumerate() {
                if init >> s & 1 != 0 {
                    *slice = [!0u64; SUB_WORDS];
                }
            }
            let mut reached = [0u64; SUB_WORDS];
            for &(i, weight) in &plan.adds {
                let mask = row(i);
                let mut carry = [0u64; SUB_WORDS];
                for (s, slice) in slices.iter_mut().enumerate() {
                    if weight >> s & 1 != 0 {
                        for j in 0..SUB_WORDS {
                            let sum = slice[j] ^ mask[j];
                            let next = (slice[j] & mask[j]) | (carry[j] & sum);
                            slice[j] = sum ^ carry[j];
                            carry[j] = next;
                        }
                    } else {
                        for j in 0..SUB_WORDS {
                            let next = carry[j] & slice[j];
                            slice[j] ^= carry[j];
                            carry[j] = next;
                        }
                    }
                }
                for j in 0..SUB_WORDS {
                    reached[j] |= carry[j];
                }
            }
            for j in 0..SUB_WORDS {
                cand[j] &= reached[j];
            }
        }
        *out = cand;
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
    /// sieve's word loop it measured 8% slower over the disjunction trace.
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
        if theta.is_some() {
            let (mut nsum, mut dmin, mut fmin) = (0.0_f64, f64::INFINITY, f64::INFINITY);
            for &i in &self.held_list {
                {
                    let s = &self.sc[i];
                    let mb = s.max_bucket(self.blocks[i]);
                    nsum += f64::from(s.num[mb]);
                    dmin = dmin.min(f64::from(s.den[mb]));
                    fmin = fmin.min(f64::from(s.factor));
                }
            }
            if below(nsum / (dmin + fmin * f64::from(length)), theta) {
                return Ok(());
            }
        }
        self.answer.scored += 1;
        let mut total = 0.0_f32;
        // The held terms are in the scorer's order: the sum is exact.
        for h in 0..self.held_list.len() {
            let i = self.held_list[h];
            let index = self.row_index(i, words, local);
            let t = self.sc[i].term;
            let s = &self.sc[i];
            let set = self.terms[t].as_ref().expect("scoring");
            let bucket = s.footer.bucket(set.postings.tf, index)?;
            if s.footer.single.is_none() {
                let block = (index / s.footer.block_size) as usize;
                self.touch.touch(
                    Part::TfTail,
                    set.at + set.postings.tf_at + s.footer.tf_at[block] as usize,
                    1,
                );
            }
            total += s
                .scorer
                .score_bucket(TfBucket::new(bucket).ok_or(Error::InvalidTfBucket)?, length);
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
