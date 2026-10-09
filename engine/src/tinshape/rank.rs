// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Ranked top k over a segment in TIN's shape, a 256-page group at a time.
//!
//! A query some terms of which every match holds (a conjunction, a phrase)
//! walks the groups those terms share. In each, their members are ANDed
//! (word by word over grids; a list's members probed in the others), and
//! the survivors are bounded, cheapest first, before anything is read for
//! them: by the footer blocks they fall in (a window per run of slots no
//! block boundary crosses), then by their length alone against the
//! window's largest buckets, and only then scored exactly from the TF tail.
//! A phrase reads positions only for a candidate that would enter the top
//! k: before the top k fill, every match does, so its positions are
//! checked first; after, a group's candidates that score into the top k
//! wait and are checked best first, so a confirmed one raises the
//! threshold over the rest.

use std::collections::BinaryHeap;

use boldi_vigna::{PhrasePlan, SpanQuery, SpanSolver};
use segment::Tid;
use segment::tf_bucket::{BUCKET_COUNT, TfBucket};
use segment::tinshape::bits;
use segment::tinshape::docs::{GROUP_PAGES, Geometry};
use segment::tinshape::ef::{Ef, EfCursor};
use segment::tinshape::postings::{Footer, Form, KIND_EF, KIND_GRID, for_each_local};
use segment::tinshape::segment::Segment;
use segment::tinshape::varint;
use segment::{Error, Result};

use super::{
    Entry, Node, Part, RankedAnswer, Src, TermSet, Touch, below, kernels, matches, open_terms,
    required_terms,
};
use crate::bm25::TermScorer;

/// One scoring term of a walk.
pub(super) struct Sc {
    pub(super) term: usize,
    pub(super) scorer: TermScorer,
    pub(super) footer: Footer,
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
    pub(super) fn new(term: usize, scorer: TermScorer, footer: Footer) -> Self {
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
        while let Some(slot) = cursor.current()
            && slot < end
        {
            mem.list.push(slot - group.slot_base);
            cursor.advance();
        }
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
            let width = u32::from(group.width);
            if e.kind == KIND_GRID {
                mem.kind = Kind::Grid(bytes);
            } else if seek && e.kind == KIND_EF {
                let ef = Ef::parse(bytes, e.count as usize, GROUP_PAGES * width)?;
                mem.kind = Kind::Cursor {
                    cursor: ef.cursor(),
                    offset: 0,
                };
            } else {
                let list = &mut mem.list;
                for_each_local(&e, bytes, width, |l| list.push(l))?;
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

/// Skips `n` varints from byte `p`: each ends at a byte below 0x80.
#[inline]
fn skip_varints(bytes: &[u8], mut p: usize, mut n: u32) -> Result<usize> {
    while n > 0 {
        if let Some(chunk) = bytes.get(p..p + 8) {
            let word = u64::from_le_bytes(chunk.try_into().expect("eight bytes"));
            let mut ends = !word & 0x8080_8080_8080_8080;
            let c = ends.count_ones();
            if c < n {
                n -= c;
                p += 8;
                continue;
            }
            for _ in 1..n {
                ends &= ends - 1;
            }
            return Ok(p + ends.trailing_zeros() as usize / 8 + 1);
        }
        let b = *bytes.get(p).ok_or(Error::Truncated)?;
        p += 1;
        if b < 0x80 {
            n -= 1;
        }
    }
    Ok(p)
}

/// A term's positions stream read by posting index, forward from the last
/// entry read where that is nearer than its skip table's entry.
struct PosCursor<'a> {
    bytes: &'a [u8],
    at: usize,
    count: u32,
    skips_at: usize,
    data_at: usize,
    /// Entry `next` starts at byte `next_at`.
    next: u32,
    next_at: usize,
}

impl<'a> PosCursor<'a> {
    fn new(stream: (&'a [u8], usize)) -> Result<Self> {
        let (bytes, at) = stream;
        let mut pos = 0;
        let count = varint::get_u32(bytes, &mut pos)?;
        let slots = if count == 0 {
            0
        } else {
            (count.div_ceil(segment::payload::SKIP_INTERVAL) - 1) as usize
        };
        let data_at = pos + slots * 4;
        Ok(Self {
            bytes,
            at,
            count,
            skips_at: pos,
            data_at,
            next: 0,
            next_at: data_at,
        })
    }

    /// Reads entry `index` into `out`.
    fn read(&mut self, index: u32, out: &mut Vec<u32>, touch: &mut impl Touch) -> Result<()> {
        if index >= self.count {
            return Err(Error::Corrupt("positions index"));
        }
        let interval = segment::payload::SKIP_INTERVAL;
        let slot = index / interval;
        if index < self.next || slot > self.next / interval {
            if slot == 0 {
                self.next = 0;
                self.next_at = self.data_at;
            } else {
                let s = self.skips_at + (slot as usize - 1) * 4;
                touch.touch(Part::Positions, self.at + s, 4);
                let skip = self.bytes.get(s..s + 4).ok_or(Error::Truncated)?;
                self.next_at =
                    self.data_at + u32::from_le_bytes(skip.try_into().expect("four")) as usize;
                self.next = slot * interval;
            }
        }
        let from = self.next_at;
        let bytes = self.bytes;
        let mut p = self.next_at;
        for _ in self.next..index {
            let n = varint::get_u32(bytes, &mut p)?;
            p = skip_varints(bytes, p, n)?;
        }
        let n = varint::get_u32(bytes, &mut p)?;
        out.clear();
        let mut previous: Option<u32> = None;
        for _ in 0..n {
            let v = varint::get_u32(bytes, &mut p)?;
            let position = match previous {
                None => v,
                Some(q) => q + v + 1,
            };
            out.push(position);
            previous = Some(position);
        }
        self.next = index + 1;
        self.next_at = p;
        touch.touch(Part::Positions, self.at + from, p - from);
        Ok(())
    }
}

/// A top-level span's positions check.
struct SpanCheck<'a> {
    /// Per span slot, its term.
    slots: Vec<usize>,
    cursors: Vec<PosCursor<'a>>,
    solver: SpanSolver,
    plan: Option<PhrasePlan>,
    positions: Vec<Vec<u32>>,
    read: Vec<bool>,
}

impl SpanCheck<'_> {
    /// Whether the candidate whose posting index in each slot's term is
    /// `index[slot]` holds the span.
    fn holds(&mut self, index: &[u32], touch: &mut impl Touch) -> Result<bool> {
        self.read.fill(false);
        if let Some(plan) = &self.plan {
            for step in plan.steps() {
                if !std::mem::replace(&mut self.read[step.slot], true) {
                    self.cursors[step.slot].read(
                        index[step.slot],
                        &mut self.positions[step.slot],
                        touch,
                    )?;
                }
                if let Some(pair) = step.pair
                    && !plan.pair_keeps(pair, &self.positions)
                {
                    return Ok(false);
                }
            }
        }
        for (slot, &at) in index.iter().enumerate() {
            if !std::mem::replace(&mut self.read[slot], true) {
                self.cursors[slot].read(at, &mut self.positions[slot], touch)?;
            }
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

/// Whether `(total, tid)` ranks before the heap's worst.
#[inline]
fn beats(total: f32, tid: Tid, worst: &Entry) -> bool {
    crate::walk::rank(&(total, tid), &(worst.0, worst.1)) == std::cmp::Ordering::Less
}

/// The walk of a query some terms of which every match holds.
struct Led<'s, 'a, T: Touch> {
    segment: &'s Segment<'a>,
    geometry: &'s Geometry,
    node: &'s Node,
    k: usize,
    touch: &'s mut T,
    terms: Vec<Option<TermSet<'a>>>,
    /// Per term (by index into `terms`), its members in the group at hand.
    mems: Vec<Mem<'a>>,
    sc: Vec<Sc>,
    /// Required terms, rarest first.
    req: Vec<usize>,
    /// Per scoring term: whether it is required.
    sc_required: Vec<bool>,
    /// The window: the last slot of every scoring term's footer block at
    /// the slot it was set at, and its bound, and its length bound's
    /// numerator, denominator and length factor.
    window_end: Option<u32>,
    window_parts: (f64, f64, f64, f64),
    heap: BinaryHeap<Entry>,
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

impl<'a, T: Touch> Led<'_, 'a, T> {
    #[inline]
    fn threshold(&self) -> Option<f32> {
        (self.heap.len() >= self.k)
            .then(|| self.heap.peek().map(|e| e.0))
            .flatten()
    }

    fn push(&mut self, total: f32, tid: Tid) {
        self.heap.push(Entry(total, tid));
        if self.heap.len() > self.k {
            self.heap.pop();
        }
    }

    /// Leapfrogs the required terms over slots, led by the rarest: a slot
    /// they all hold is a candidate. A group every required term holds as a
    /// grid is sieved whole instead, by the AND of their words. A group and
    /// a window whose bounds cannot reach the threshold are skipped unread.
    fn run(&mut self) -> Result<()> {
        let lead = self.req[0];
        let n = self.sc.len();
        let mut cursor = 0u32;
        let mut current: Option<u32> = None;
        'slots: while let Some(slot) = self.seek(lead, cursor, true)? {
            let g = self.mems[lead].loaded - 1;
            let group = self.geometry.groups[g as usize];
            let (base, end) = (group.slot_base, group.slot_base + group.slots() - 1);
            if current != Some(g) {
                if let Some(previous) = current
                    && !self.pending.is_empty()
                {
                    self.verify_pending(previous)?;
                }
                current = Some(g);
                if n == 0 && self.heap.len() >= self.k {
                    // Nothing scores: every later match ties at zero and
                    // ranks after the first k.
                    break;
                }
                self.answer.windows += 1;
                let mut bound = 0.0_f64;
                for i in 0..n {
                    bound += f64::from(self.sc[i].range_bound(base, end));
                }
                if below(bound, self.threshold()) {
                    self.answer.windows_pruned += 1;
                    cursor = end + 1;
                    continue;
                }
                if self.req.len() > 1 && self.all_grids(g) {
                    self.dense_group(g)?;
                    cursor = end + 1;
                    continue;
                }
            }
            if self.window(slot) {
                // Skip the rest of the window unread.
                match self.window_end {
                    Some(e) if e < u32::MAX => cursor = e + 1,
                    _ => break,
                }
                continue;
            }
            for r in 1..self.req.len() {
                let t = self.req[r];
                match self.seek(t, slot, false)? {
                    None => break 'slots,
                    Some(s) if s != slot => {
                        cursor = s;
                        continue 'slots;
                    }
                    Some(_) => {}
                }
            }
            if !self.process(g, slot - base)? {
                break;
            }
            cursor = slot + 1;
        }
        if let Some(g) = current
            && !self.pending.is_empty()
        {
            self.verify_pending(g)?;
        }
        Ok(())
    }

    /// Moves term `t` to its first member at or after `target`, loading
    /// the group that holds it; the member's slot, if any. A term's
    /// Elias-Fano list is sought rather than decoded where its members far
    /// outnumber the lead's: the leapfrog visits few of them.
    fn seek(&mut self, t: usize, mut target: u32, lead: bool) -> Result<Option<u32>> {
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
                count_in(set, geometry, next, mem);
                let seek = !lead && mem.count > 4 * self.mems[self.req[0]].count.max(1);
                let mem = &mut self.mems[t];
                load(set, geometry, next, mem, seek, self.touch)?;
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

    /// Sieves group `g`, every required term of which is a grid there, by
    /// the AND of their words, and walks its candidates.
    fn dense_group(&mut self, g: u32) -> Result<()> {
        let group = self.geometry.groups[g as usize];
        let base = group.slot_base;
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
        let mut at = 0;
        while at < cands.len() {
            let local = cands[at];
            at += 1;
            if self.window(base + local) {
                match self.window_end {
                    Some(e) if e < u32::MAX => {
                        at += cands[at..].partition_point(|l| base + *l <= e);
                        continue;
                    }
                    _ => break,
                }
            }
            if !self.process(g, local)? {
                break;
            }
        }
        self.cands = cands;
        if !self.pending.is_empty() {
            self.verify_pending(g)?;
        }
        Ok(())
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
        if n == 0 && self.heap.len() >= self.k {
            return Ok(false);
        }
        if let Some(dead) = self.segment.liveness.groups[g as usize].as_deref()
            && dead[local as usize / 64] >> (local % 64) & 1 == 1
        {
            return Ok(true);
        }
        self.answer.candidates += 1;
        let theta = self.threshold();
        // The length alone, against the window's largest buckets.
        let rank = self
            .segment
            .docs
            .rank_in(g as usize, local)
            .ok_or(Error::Corrupt("a posting without a document"))?;
        self.touch
            .touch(Part::DlSidecar, self.segment.length_at(rank), 2);
        let length = self.segment.lengths.get(rank)?;
        if theta.is_some() && n > 0 {
            let (_, nsum, dmin, fmin) = self.window_parts;
            if below(nsum / (dmin + fmin * f64::from(length)), theta) {
                return Ok(true);
            }
        }
        // A span checks positions before scoring while the top k fill:
        // every match enters them.
        let tid = self.geometry.tid_in(g as usize, local);
        let span_first = matches!(self.verify, Verify::Span(_)) && self.heap.len() < self.k;
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
        self.answer.scored += 1;
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
        if let Some(worst) = self.heap.peek()
            && self.heap.len() >= self.k
            && !beats(total, tid, worst)
        {
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
            if let Some(worst) = self.heap.peek()
                && self.heap.len() >= self.k
                && !beats(total, tid, worst)
            {
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

/// [`super::top_k`] for a query some terms of which every match holds.
pub(super) fn top_k_led(
    segment: &Segment<'_>,
    node: &Node,
    names: &[String],
    scorers: &[(String, TermScorer)],
    k: usize,
    touch: &mut impl Touch,
) -> Result<RankedAnswer> {
    let required = required_terms(node);
    debug_assert!(!required.is_empty());
    let terms = open_terms(segment, names, touch)?;
    if k == 0 || required.iter().any(|t| terms[*t].is_none()) {
        return Ok(RankedAnswer::default());
    }
    let mut sc: Vec<Sc> = Vec::new();
    for (name, scorer) in scorers {
        let Some(t) = names.iter().position(|n| n == name) else {
            continue;
        };
        let Some(set) = &terms[t] else { continue };
        let footer =
            set.postings
                .footer(segment.block_size, set.max_bucket, segment.adaptive_tf)?;
        touch.touch(
            Part::Footer,
            set.at + set.postings.footer_at,
            set.postings.footer.len(),
        );
        sc.push(Sc::new(t, scorer.clone(), footer));
    }
    let flat = match node {
        Node::And(c) => c.iter().all(|c| matches!(c, Node::Term(_))),
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
    let mut walk = Led {
        segment,
        geometry: &segment.docs.geometry,
        node,
        k,
        mems: (0..terms.len()).map(|_| Mem::default()).collect(),
        terms,
        sc_required: Vec::with_capacity(n),
        window_end: None,
        window_parts: (0.0, 0.0, 0.0, 0.0),
        req,
        heap: BinaryHeap::with_capacity(k + 1),
        answer: RankedAnswer::default(),
        verify,
        pending: Vec::new(),
        pending_index: Vec::new(),
        span_index: Vec::new(),
        cands: Vec::new(),
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
    walk.run()?;
    let mut rows: Vec<(f32, Tid)> = walk.heap.into_iter().map(|e| (e.0, e.1)).collect();
    rows.sort_by(crate::walk::rank);
    let mut answer = walk.answer;
    answer.rows = rows;
    Ok(answer)
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
    Ok(Verify::Span(Box::new(SpanCheck {
        slots: slots.to_vec(),
        cursors,
        solver,
        plan,
        positions: vec![Vec::new(); slots.len()],
        read: vec![false; slots.len()],
    })))
}
