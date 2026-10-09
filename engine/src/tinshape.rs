// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Counts and ranked top-k over segments in TIN's shape
//! ([`segment::tinshape`]): postings are sets of ctid-grid slots, so a
//! Boolean count is a word-wise fold over each 256-page group the terms
//! share, and a ranked query walks the terms' slots in ctid order with
//! block-max bounds from their footers, reading a candidate's bucket from
//! the TF tail and its length from the DL sidecar.
//!
//! Phase A: measured offline by the bench crate against the ordinal fold
//! and walk; the extension does not call it.

use boldi_vigna::{SpanQuery, SpanSolver};
use segment::Tid;
use segment::tf_bucket::TfBucket;
use segment::tinshape::bits;
use segment::tinshape::docs::Geometry;
use segment::tinshape::ef::EfCursor;
use segment::tinshape::postings::{Footer, Form, GroupEntry, KIND_GRID, Postings, for_each_local};
use segment::tinshape::segment::{Area, Segment};
use segment::tinshape::varint;
use segment::{Error, Result};
use tinql::runtime::{Query, SpanTermSlot};

use crate::bm25::TermScorer;

/// The parts of a segment a query reads, named as TIN's EXPLAIN names its
/// page touches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Part {
    Metadata,
    TermMap,
    /// A record's header and group directory and its footer blocks.
    Footer,
    /// The slot sets: Elias-Fano lists and group containers.
    Payload,
    TfTail,
    DlSidecar,
    Positions,
    Liveness,
}

impl Part {
    pub const ALL: [Part; 8] = [
        Part::Metadata,
        Part::TermMap,
        Part::Footer,
        Part::Payload,
        Part::TfTail,
        Part::DlSidecar,
        Part::Positions,
        Part::Liveness,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Part::Metadata => "Metadata",
            Part::TermMap => "Term Map",
            Part::Footer => "Postings Footer",
            Part::Payload => "Postings Payload",
            Part::TfTail => "Postings TF Tail",
            Part::DlSidecar => "DL Sidecar",
            Part::Positions => "Positions",
            Part::Liveness => "Liveness Bitmap",
        }
    }
}

/// Where a query reads: `(part, blob offset, length)` per read.
pub trait Touch {
    fn touch(&mut self, part: Part, at: usize, len: usize);
}

/// Reads recorded nowhere, for timing.
pub struct NoTouch;

impl Touch for NoTouch {
    #[inline]
    fn touch(&mut self, _: Part, _: usize, _: usize) {}
}

/// Word kernels. NEON is part of every aarch64 target, so they need no
/// runtime dispatch there; elsewhere the portable loops vectorize as the
/// compiler can.
pub mod kernels {
    /// Set bits in `words`.
    #[inline]
    pub fn popcount(words: &[u64]) -> u64 {
        #[cfg(target_arch = "aarch64")]
        // SAFETY: NEON is baseline on aarch64; loads stay within `words`.
        unsafe {
            use core::arch::aarch64::*;
            let mut acc = vdupq_n_u64(0);
            let chunks = words.chunks_exact(4);
            let rest = chunks.remainder();
            for c in chunks {
                let a = vcntq_u8(vreinterpretq_u8_u64(vld1q_u64(c.as_ptr())));
                let b = vcntq_u8(vreinterpretq_u8_u64(vld1q_u64(c.as_ptr().add(2))));
                acc = vaddq_u64(acc, vpaddlq_u32(vpaddlq_u16(vpaddlq_u8(vaddq_u8(a, b)))));
            }
            vaddvq_u64(acc) + rest.iter().map(|w| u64::from(w.count_ones())).sum::<u64>()
        }
        #[cfg(not(target_arch = "aarch64"))]
        words.iter().map(|w| u64::from(w.count_ones())).sum()
    }

    /// `out = bytes` read as little-endian words.
    #[inline]
    pub fn load(out: &mut [u64], bytes: &[u8]) {
        for (w, c) in out.iter_mut().zip(bytes.chunks_exact(8)) {
            *w = u64::from_le_bytes(c.try_into().expect("eight bytes"));
        }
    }

    /// `out &= bytes`.
    #[inline]
    pub fn and_bytes(out: &mut [u64], bytes: &[u8]) {
        for (w, c) in out.iter_mut().zip(bytes.chunks_exact(8)) {
            *w &= u64::from_le_bytes(c.try_into().expect("eight bytes"));
        }
    }

    /// `out |= bytes`.
    #[inline]
    pub fn or_bytes(out: &mut [u64], bytes: &[u8]) {
        for (w, c) in out.iter_mut().zip(bytes.chunks_exact(8)) {
            *w |= u64::from_le_bytes(c.try_into().expect("eight bytes"));
        }
    }

    /// Set bits of `a & b`, with `b` as little-endian bytes.
    #[inline]
    pub fn and_count_bytes(a: &[u64], bytes: &[u8]) -> u64 {
        #[cfg(target_arch = "aarch64")]
        // SAFETY: NEON is baseline on aarch64; every load is within its
        // slice, the byte loads unaligned as `vld1q_u8` allows.
        unsafe {
            use core::arch::aarch64::*;
            let n = a.len().min(bytes.len() / 8);
            let mut acc = vdupq_n_u64(0);
            let mut i = 0;
            while i + 4 <= n {
                let x0 = vandq_u8(
                    vreinterpretq_u8_u64(vld1q_u64(a.as_ptr().add(i))),
                    vld1q_u8(bytes.as_ptr().add(i * 8)),
                );
                let x1 = vandq_u8(
                    vreinterpretq_u8_u64(vld1q_u64(a.as_ptr().add(i + 2))),
                    vld1q_u8(bytes.as_ptr().add(i * 8 + 16)),
                );
                let s = vaddq_u8(vcntq_u8(x0), vcntq_u8(x1));
                acc = vaddq_u64(acc, vpaddlq_u32(vpaddlq_u16(vpaddlq_u8(s))));
                i += 4;
            }
            let mut total = vaddvq_u64(acc);
            while i < n {
                let b = u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().expect("eight"));
                total += u64::from((a[i] & b).count_ones());
                i += 1;
            }
            total
        }
        #[cfg(not(target_arch = "aarch64"))]
        a.iter()
            .zip(bytes.chunks_exact(8))
            .map(|(w, c)| {
                u64::from((w & u64::from_le_bytes(c.try_into().expect("eight"))).count_ones())
            })
            .sum()
    }

    /// Set bits of the AND of two little-endian byte bitmaps.
    #[inline]
    pub fn and_count_two(x: &[u8], y: &[u8]) -> u64 {
        #[cfg(target_arch = "aarch64")]
        // SAFETY: as above.
        unsafe {
            use core::arch::aarch64::*;
            let n = x.len().min(y.len());
            let mut acc = vdupq_n_u64(0);
            let mut i = 0;
            while i + 32 <= n {
                let a = vandq_u8(vld1q_u8(x.as_ptr().add(i)), vld1q_u8(y.as_ptr().add(i)));
                let b = vandq_u8(
                    vld1q_u8(x.as_ptr().add(i + 16)),
                    vld1q_u8(y.as_ptr().add(i + 16)),
                );
                let s = vaddq_u8(vcntq_u8(a), vcntq_u8(b));
                acc = vaddq_u64(acc, vpaddlq_u32(vpaddlq_u16(vpaddlq_u8(s))));
                i += 32;
            }
            let mut total = vaddvq_u64(acc);
            while i < n {
                total += u64::from((x[i] & y[i]).count_ones());
                i += 1;
            }
            total
        }
        #[cfg(not(target_arch = "aarch64"))]
        x.iter()
            .zip(y)
            .map(|(a, b)| u64::from((a & b).count_ones()))
            .sum()
    }
}

/// A query lowered over a table of distinct terms.
#[derive(Clone, Debug)]
pub enum Node {
    Term(usize),
    And(Vec<Node>),
    Or(Vec<Node>),
    /// Only as a child of `And`, or at the top (against every document).
    Not(Box<Node>),
    /// A span over term slots, `slots[i]` the term of slot `i`.
    Span {
        slots: Vec<usize>,
        query: SpanQuery,
    },
}

/// Lowers `query` into a tree over `terms`, or `None` for shapes this path
/// does not handle (expansions, position filters, AT LEAST beyond one).
pub fn lower(query: &Query, terms: &mut Vec<String>) -> Option<Node> {
    let term = |name: &str, terms: &mut Vec<String>| {
        terms.iter().position(|t| t == name).unwrap_or_else(|| {
            terms.push(name.to_owned());
            terms.len() - 1
        })
    };
    Some(match query {
        Query::Term(name) => Node::Term(term(name, terms)),
        Query::And(a, b) => Node::And(vec![lower(a, terms)?, lower(b, terms)?]),
        Query::Or(a, b) => Node::Or(vec![lower(a, terms)?, lower(b, terms)?]),
        Query::Conjunction(children) => Node::And(
            children
                .iter()
                .map(|c| lower(c, terms))
                .collect::<Option<_>>()?,
        ),
        Query::Disjunction { min: 1, children } | Query::AtLeast { min: 1, children } => Node::Or(
            children
                .iter()
                .map(|c| lower(c, terms))
                .collect::<Option<_>>()?,
        ),
        Query::Not(inner) => Node::Not(Box::new(lower(inner, terms)?)),
        Query::Boost { inner, .. } => lower(inner, terms)?,
        Query::Span {
            term_slots,
            span_query,
            position_filter: None,
        } => {
            let mut slots = Vec::with_capacity(term_slots.len());
            for slot in term_slots {
                match slot {
                    SpanTermSlot::Term(name) => slots.push(term(name, terms)),
                    _ => return None,
                }
            }
            Node::Span {
                slots,
                query: span_query.clone(),
            }
        }
        _ => return None,
    })
}

/// Where a group's members are.
#[derive(Clone, Copy, Debug)]
enum Src<'a> {
    /// A container of a grouped record, with its blob offset.
    Container {
        entry: GroupEntry,
        bytes: &'a [u8],
        at: usize,
    },
    /// Local slots `from..to` of the term's decoded list.
    Locals { from: u32, to: u32 },
}

#[derive(Clone, Copy, Debug)]
struct G<'a> {
    index: u32,
    count: u32,
    src: Src<'a>,
}

/// One query term opened in a segment.
pub struct TermSet<'a> {
    pub df: u32,
    pub max_bucket: u8,
    pub postings: Postings<'a>,
    /// Where its record starts in the blob.
    pub at: usize,
    /// The positions stream and its blob offset.
    pub positions: (&'a [u8], usize),
    groups: Vec<G<'a>>,
    /// Local slots of a single or sparse term, all groups in a row.
    locals: Vec<u32>,
    // Cursor state, for ranked walks and phrase checks.
    gpos: usize,
    loaded: Loaded,
    current: Option<u32>,
    index: u32,
    sparse: Option<EfCursor<'a>>,
    prefix: Vec<u32>,
    list: Vec<u32>,
    pos: usize,
}

#[derive(Default)]
enum Loaded {
    #[default]
    None,
    /// A grid group, with `prefix` its members before each word.
    Grid,
    /// A decoded group, its local slots in `list`, at `pos`.
    List,
}

impl<'a> TermSet<'a> {
    /// Opens `name` in `segment`, `None` when the segment lacks it. Records
    /// the term map and the record's header and group directory as read.
    pub fn open(segment: &Segment<'a>, name: &str, touch: &mut impl Touch) -> Result<Option<Self>> {
        touch.touch(Part::TermMap, segment.area_at(Area::TermMap), 1);
        let Some(term) = segment.term_memo(name)? else {
            return Ok(None);
        };
        let geometry = &segment.docs.geometry;
        let postings = term.postings;
        let record_at = term.at;
        let mut groups = Vec::new();
        let mut locals = Vec::new();
        match &postings.form {
            Form::Single(slot) => {
                touch.touch(Part::Payload, record_at, postings.tf_at);
                let index = geometry.group_of_slot(*slot);
                locals.push(slot - geometry.groups[index].slot_base);
                groups.push(G {
                    index: index as u32,
                    count: 1,
                    src: Src::Locals { from: 0, to: 1 },
                });
            }
            Form::Sparse(_) => {
                // The header only; the list is read where it is decoded.
                touch.touch(Part::Footer, record_at, postings.footer_at);
            }
            Form::Grouped(entries) => {
                // The header and the group directory: per-term metadata,
                // which TIN would count as the postings footer.
                touch.touch(Part::Footer, record_at, postings.footer_at);
                touch.touch(
                    Part::Footer,
                    record_at + postings.payload_at,
                    postings.containers_at,
                );
                let containers = record_at + postings.payload_at + postings.containers_at;
                for entry in entries {
                    groups.push(G {
                        index: entry.index,
                        count: entry.count,
                        src: Src::Container {
                            entry: *entry,
                            bytes: postings.container(entry),
                            at: containers + entry.at as usize,
                        },
                    });
                }
            }
        }
        Ok(Some(Self {
            df: term.entry.df,
            max_bucket: term.entry.max_tf_bucket,
            positions: segment.positions(&term.entry)?,
            postings,
            at: record_at,
            groups,
            locals,
            gpos: 0,
            loaded: Loaded::None,
            current: None,
            index: 0,
            sparse: None,
            prefix: Vec::new(),
            list: Vec::new(),
            pos: 0,
        }))
    }

    /// Decodes a sparse term's list into per-group local slots, for folds.
    fn ensure_groups(&mut self, geometry: &Geometry, touch: &mut impl Touch) {
        let Form::Sparse(list) = &self.postings.form else {
            return;
        };
        if !self.groups.is_empty() {
            return;
        }
        touch.touch(
            Part::Payload,
            self.at + self.postings.payload_at,
            self.postings.payload.len(),
        );
        let mut slots = Vec::with_capacity(self.df as usize);
        list.for_each(|s| slots.push(s));
        let mut index = 0usize;
        let mut from = 0u32;
        for (i, slot) in slots.iter().enumerate() {
            while geometry.groups[index].slot_base + geometry.groups[index].slots() <= *slot {
                index += 1;
            }
            let local = slot - geometry.groups[index].slot_base;
            match self.groups.last_mut() {
                Some(g) if g.index == index as u32 => {
                    g.count += 1;
                    if let Src::Locals { to, .. } = &mut g.src {
                        *to += 1;
                    }
                }
                _ => {
                    from = i as u32;
                    self.groups.push(G {
                        index: index as u32,
                        count: 1,
                        src: Src::Locals { from, to: from + 1 },
                    });
                }
            }
            self.locals.push(local);
        }
        let _ = from;
    }

    fn find(&self, group: u32, hint: &mut usize) -> Option<&G<'a>> {
        while *hint < self.groups.len() && self.groups[*hint].index < group {
            *hint += 1;
        }
        self.groups.get(*hint).filter(|g| g.index == group)
    }

    // ---- The cursor: slots in order, each with its posting index. ----

    /// Moves to the first slot at or after `target`; the slot, if any.
    #[inline]
    pub fn seek(
        &mut self,
        geometry: &Geometry,
        target: u32,
        touch: &mut impl Touch,
    ) -> Option<u32> {
        if self.current.is_some_and(|c| c >= target) {
            return self.current;
        }
        if let Form::Sparse(list) = &self.postings.form {
            let cursor = self.sparse.get_or_insert_with(|| {
                touch.touch(
                    Part::Payload,
                    self.at + self.postings.payload_at,
                    self.postings.payload.len(),
                );
                list.cursor()
            });
            cursor.seek(target);
            self.current = cursor.current();
            self.index = cursor.rank() as u32;
            return self.current;
        }
        loop {
            let Some(g) = self.groups.get(self.gpos).copied() else {
                self.current = None;
                return None;
            };
            let group = &geometry.groups[g.index as usize];
            let base = group.slot_base;
            if base + group.slots() <= target {
                // Skip the groups wholly before the target.
                let skip = self.groups[self.gpos..].partition_point(|g| {
                    let group = &geometry.groups[g.index as usize];
                    group.slot_base + group.slots() <= target
                });
                self.gpos += skip;
                self.loaded = Loaded::None;
                continue;
            }
            let width = u32::from(group.width);
            let local_target = target.saturating_sub(base);
            if matches!(self.loaded, Loaded::None) {
                self.load(&g, width, touch);
            }
            let first = self.first_of(&g);
            let found = match (&self.loaded, g.src) {
                (Loaded::Grid, Src::Container { bytes, .. }) => {
                    let words = bytes.len() / 8;
                    let mut w = local_target as usize / 64;
                    let mut found = None;
                    if w < words {
                        let mut word = bits::word(bytes, w) & (u64::MAX << (local_target % 64));
                        loop {
                            if word != 0 {
                                let bit = word.trailing_zeros();
                                let local = (w * 64) as u32 + bit;
                                let rank = self.prefix[w]
                                    + (bits::word(bytes, w) & ((1u64 << bit) - 1)).count_ones();
                                found = Some((local, rank));
                                break;
                            }
                            w += 1;
                            if w >= words {
                                break;
                            }
                            word = bits::word(bytes, w);
                        }
                    }
                    found
                }
                (Loaded::List, _) => {
                    let list = &self.list;
                    let mut pos = self.pos;
                    // Forward steps are short: look at the next few first.
                    let mut steps = 0;
                    while pos < list.len() && list[pos] < local_target && steps < 8 {
                        pos += 1;
                        steps += 1;
                    }
                    if pos < list.len() && list[pos] < local_target {
                        pos += list[pos..].partition_point(|l| *l < local_target);
                    }
                    self.pos = pos;
                    list.get(pos).map(|l| (*l, pos as u32))
                }
                _ => unreachable!("a loaded group"),
            };
            if let Some((local, rank)) = found {
                self.current = Some(base + local);
                self.index = first + rank;
                return self.current;
            }
            self.gpos += 1;
            self.loaded = Loaded::None;
        }
    }

    fn first_of(&self, g: &G<'_>) -> u32 {
        match g.src {
            Src::Container { entry, .. } => entry.first,
            Src::Locals { from, .. } => from,
        }
    }

    fn load(&mut self, g: &G<'a>, width: u32, touch: &mut impl Touch) {
        self.pos = 0;
        match g.src {
            Src::Container { entry, bytes, at } => {
                touch.touch(Part::Payload, at, bytes.len());
                if entry.kind == KIND_GRID {
                    let words = bytes.len() / 8;
                    self.prefix.clear();
                    let mut n = 0u32;
                    for c in bytes.chunks_exact(8).take(words) {
                        self.prefix.push(n);
                        n += u64::from_le_bytes(c.try_into().expect("eight bytes")).count_ones();
                    }
                    self.loaded = Loaded::Grid;
                } else {
                    let mut list = std::mem::take(&mut self.list);
                    list.clear();
                    for_each_local(&entry, bytes, width, |l| list.push(l))
                        .unwrap_or_else(|e| crate::corrupt(format!("Stannum postings: {e}")));
                    self.list = list;
                    self.loaded = Loaded::List;
                }
            }
            Src::Locals { from, to } => {
                self.list.clear();
                self.list
                    .extend_from_slice(&self.locals[from as usize..to as usize]);
                self.loaded = Loaded::List;
            }
        }
    }

    /// The current slot's posting index.
    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn current(&self) -> Option<u32> {
        self.current
    }
}

/// Scratch bitmaps for a fold, one per tree depth.
#[derive(Default)]
pub struct Scratch {
    levels: Vec<Vec<u64>>,
}

impl Scratch {
    fn level(&mut self, depth: usize, words: usize) -> Vec<u64> {
        while self.levels.len() <= depth {
            self.levels.push(Vec::new());
        }
        let mut v = std::mem::take(&mut self.levels[depth]);
        v.clear();
        v.resize(words, 0);
        v
    }
    fn give(&mut self, depth: usize, v: Vec<u64>) {
        self.levels[depth] = v;
    }
}

/// The candidate groups of a node: those that can hold a match.
fn candidate_groups(node: &Node, terms: &[Option<TermSet<'_>>], all: usize) -> Vec<u32> {
    match node {
        Node::Term(t) => terms[*t]
            .as_ref()
            .map_or_else(Vec::new, |t| t.groups.iter().map(|g| g.index).collect()),
        Node::Span { slots, .. } => {
            let sets: Vec<Vec<u32>> = slots
                .iter()
                .map(|t| candidate_groups(&Node::Term(*t), terms, all))
                .collect();
            intersect(sets)
        }
        Node::And(children) => {
            let sets: Vec<Vec<u32>> = children
                .iter()
                .filter(|c| !matches!(c, Node::Not(_)))
                .map(|c| candidate_groups(c, terms, all))
                .collect();
            if sets.is_empty() {
                (0..all as u32).collect()
            } else {
                intersect(sets)
            }
        }
        Node::Or(children) => {
            // A presence bitmap over the groups, rather than a sort.
            let mut present = vec![0u64; all.div_ceil(64)];
            for child in children {
                for g in candidate_groups(child, terms, all) {
                    present[g as usize / 64] |= 1 << (g % 64);
                }
            }
            let mut out = Vec::new();
            for (w, word) in present.iter().enumerate() {
                let mut word = *word;
                while word != 0 {
                    out.push((w * 64) as u32 + word.trailing_zeros());
                    word &= word - 1;
                }
            }
            out
        }
        Node::Not(_) => (0..all as u32).collect(),
    }
}

fn intersect(mut sets: Vec<Vec<u32>>) -> Vec<u32> {
    sets.sort_by_key(Vec::len);
    let mut out = sets.first().cloned().unwrap_or_default();
    for set in &sets[1.min(sets.len())..] {
        out.retain(|g| set.binary_search(g).is_ok());
    }
    out
}

/// Context of a fold over one segment.
struct Fold<'s, 'a, T: Touch> {
    segment: &'s Segment<'a>,
    terms: &'s mut [Option<TermSet<'a>>],
    hints: Vec<usize>,
    scratch: Scratch,
    touch: &'s mut T,
    /// Member lists of a probing AND.
    list: Vec<u32>,
    other: Vec<u32>,
}

/// Keeps the members of `list` that are in `other`; both ascending.
fn intersect_sorted(list: &mut Vec<u32>, other: &[u32]) {
    let mut j = 0;
    list.retain(|l| {
        while j < other.len() && other[j] < *l {
            j += 1;
        }
        j < other.len() && other[j] == *l
    });
}

impl<'a, T: Touch> Fold<'_, 'a, T> {
    /// Writes the members of `node` in group `group` into `out` (zeroed by
    /// the caller); false when there are none.
    fn eval(&mut self, node: &Node, group: u32, out: &mut [u64], depth: usize) -> Result<bool> {
        match node {
            Node::Term(t) => self.term_into(*t, group, out),
            Node::And(children) => {
                // A flat AND of terms (and negated terms) probes, led by its
                // rarest term in the group.
                let flat_pos: Option<Vec<usize>> = children
                    .iter()
                    .filter(|c| !matches!(c, Node::Not(_)))
                    .map(|c| {
                        if let Node::Term(t) = c {
                            Some(*t)
                        } else {
                            None
                        }
                    })
                    .collect();
                let flat_neg: Option<Vec<usize>> = children
                    .iter()
                    .filter_map(|c| {
                        if let Node::Not(inner) = c {
                            Some(inner)
                        } else {
                            None
                        }
                    })
                    .map(|c| {
                        if let Node::Term(t) = &**c {
                            Some(*t)
                        } else {
                            None
                        }
                    })
                    .collect();
                if let (Some(pos), Some(neg)) = (&flat_pos, &flat_neg)
                    && !pos.is_empty()
                {
                    return self.and_terms(pos, neg, group, out);
                }
                // Positive children first, rarest in the group first.
                let mut order: Vec<(u32, usize)> = children
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| !matches!(c, Node::Not(_)))
                    .map(|(i, c)| (self.estimate(c, group), i))
                    .collect();
                order.sort_unstable();
                let mut first = true;
                for (_, i) in order {
                    if first {
                        if !self.eval(&children[i], group, out, depth + 1)? {
                            return Ok(false);
                        }
                        first = false;
                        continue;
                    }
                    if let Node::Term(t) = &children[i]
                        && let Some(g) = self.group_of(*t, group)
                        && let Src::Container { entry, bytes, .. } = g.src
                        && entry.kind == KIND_GRID
                    {
                        self.touch_src(&g.src);
                        kernels::and_bytes(out, bytes);
                    } else {
                        let mut other = self.scratch.level(depth, out.len());
                        let any = self.eval(&children[i], group, &mut other, depth + 1)?;
                        if any {
                            for (o, x) in out.iter_mut().zip(&other) {
                                *o &= x;
                            }
                        }
                        self.scratch.give(depth, other);
                        if !any {
                            return Ok(false);
                        }
                    }
                }
                if first {
                    // Only negations: start from every document.
                    let words = self.segment.docs.group_words(group as usize);
                    out.copy_from_slice(words);
                }
                for child in children {
                    if let Node::Not(inner) = child {
                        let mut other = self.scratch.level(depth, out.len());
                        if self.eval(inner, group, &mut other, depth + 1)? {
                            for (o, x) in out.iter_mut().zip(&other) {
                                *o &= !x;
                            }
                        }
                        self.scratch.give(depth, other);
                    }
                }
                Ok(out.iter().any(|w| *w != 0))
            }
            Node::Or(children) => {
                let mut any = false;
                for child in children {
                    // A term's members are set in place.
                    if let Node::Term(t) = child {
                        any |= self.term_or(*t, group, out)?;
                        continue;
                    }
                    let mut other = self.scratch.level(depth, out.len());
                    if self.eval(child, group, &mut other, depth + 1)? {
                        for (o, x) in out.iter_mut().zip(&other) {
                            *o |= x;
                        }
                        any = true;
                    }
                    self.scratch.give(depth, other);
                }
                Ok(any)
            }
            Node::Not(inner) => {
                let words = self.segment.docs.group_words(group as usize);
                out.copy_from_slice(words);
                let mut other = self.scratch.level(depth, out.len());
                if self.eval(inner, group, &mut other, depth + 1)? {
                    for (o, x) in out.iter_mut().zip(&other) {
                        *o &= !x;
                    }
                }
                self.scratch.give(depth, other);
                Ok(out.iter().any(|w| *w != 0))
            }
            Node::Span { slots, query } => {
                let mut distinct = slots.clone();
                distinct.sort_unstable();
                distinct.dedup();
                let and = Node::And(distinct.iter().map(|t| Node::Term(*t)).collect());
                if !self.eval(&and, group, out, depth + 1)? {
                    return Ok(false);
                }
                self.verify_span(slots, query, group, out)?;
                Ok(out.iter().any(|w| *w != 0))
            }
        }
    }

    /// An estimate of a node's members in a group, to order an AND.
    fn estimate(&mut self, node: &Node, group: u32) -> u32 {
        match node {
            Node::Term(t) => self.group_of(*t, group).map_or(0, |g| g.count),
            Node::And(c) => c
                .iter()
                .map(|c| self.estimate(c, group))
                .min()
                .unwrap_or(u32::MAX),
            Node::Span { slots, .. } => slots
                .iter()
                .map(|t| self.group_of(*t, group).map_or(0, |g| g.count))
                .min()
                .unwrap_or(0),
            _ => u32::MAX,
        }
    }

    fn group_of(&mut self, t: usize, group: u32) -> Option<G<'a>> {
        let hint = &mut self.hints[t];
        let set = self.terms[t].as_ref()?;
        if *hint > 0 && set.groups.get(*hint - 1).is_some_and(|g| g.index >= group) {
            *hint = 0;
        }
        set.find(group, hint).copied()
    }

    fn touch_src(&mut self, src: &Src<'_>) {
        if let Src::Container { bytes, at, .. } = src {
            self.touch.touch(Part::Payload, *at, bytes.len());
        }
    }

    /// Sets the members of term `t` in group `group` in `out`.
    fn term_or(&mut self, t: usize, group: u32, out: &mut [u64]) -> Result<bool> {
        let Some(g) = self.group_of(t, group) else {
            return Ok(false);
        };
        if let Src::Container { entry, bytes, .. } = g.src
            && entry.kind == KIND_GRID
        {
            self.touch_src(&g.src);
            kernels::or_bytes(out, bytes);
            return Ok(true);
        }
        self.term_into(t, group, out)
    }

    /// The members of term `t`'s group `g`, appended to `list` in order.
    fn decode(&mut self, t: usize, g: &G<'a>, list: &mut Vec<u32>) -> Result<()> {
        self.touch_src(&g.src);
        let width = u32::from(self.segment.docs.geometry.groups[g.index as usize].width);
        match g.src {
            Src::Container { entry, bytes, .. } => {
                for_each_local(&entry, bytes, width, |l| list.push(l))?;
            }
            Src::Locals { from, to } => {
                let set = self.terms[t].as_ref().expect("a term with a group");
                list.extend_from_slice(&set.locals[from as usize..to as usize]);
            }
        }
        Ok(())
    }

    /// A flat AND of terms `pos` without terms `neg` in a group: grids are
    /// combined word by word; otherwise the rarest term's members are
    /// listed and the others probed.
    fn and_terms(
        &mut self,
        pos: &[usize],
        neg: &[usize],
        group: u32,
        out: &mut [u64],
    ) -> Result<bool> {
        let mut gs: Vec<(u32, usize, G<'a>)> = Vec::with_capacity(pos.len());
        for t in pos {
            match self.group_of(*t, group) {
                Some(g) => gs.push((g.count, *t, g)),
                None => return Ok(false),
            }
        }
        gs.sort_unstable_by_key(|(count, t, _)| (*count, *t));
        let is_grid =
            |g: &G<'_>| matches!(g.src, Src::Container { entry, .. } if entry.kind == KIND_GRID);
        let lead_is_grid = is_grid(&gs[0].2);
        // Grids throughout, or a lead too dense to list: words.
        if lead_is_grid && gs.iter().all(|(_, _, g)| is_grid(g)) {
            for (i, (_, _, g)) in gs.iter().enumerate() {
                self.touch_src(&g.src);
                let Src::Container { bytes, .. } = g.src else {
                    unreachable!()
                };
                if i == 0 {
                    kernels::load(out, bytes);
                } else {
                    kernels::and_bytes(out, bytes);
                }
            }
        } else {
            let mut list = std::mem::take(&mut self.list);
            let mut other = std::mem::take(&mut self.other);
            list.clear();
            let (_, lead, g) = gs[0];
            self.decode(lead, &g, &mut list)?;
            for (_, t, g) in &gs[1..] {
                if list.is_empty() {
                    break;
                }
                match g.src {
                    Src::Container { entry, bytes, .. } if entry.kind == KIND_GRID => {
                        self.touch_src(&g.src);
                        list.retain(|l| bytes[*l as usize / 8] >> (l % 8) & 1 == 1);
                    }
                    _ => {
                        other.clear();
                        self.decode(*t, g, &mut other)?;
                        intersect_sorted(&mut list, &other);
                    }
                }
            }
            for l in &list {
                out[*l as usize / 64] |= 1 << (l % 64);
            }
            self.list = list;
            self.other = other;
        }
        for t in neg {
            let Some(g) = self.group_of(*t, group) else {
                continue;
            };
            match g.src {
                Src::Container { entry, bytes, .. } if entry.kind == KIND_GRID => {
                    self.touch_src(&g.src);
                    for (w, c) in out.iter_mut().zip(bytes.chunks_exact(8)) {
                        *w &= !u64::from_le_bytes(c.try_into().expect("eight bytes"));
                    }
                }
                _ => {
                    let mut other = std::mem::take(&mut self.other);
                    other.clear();
                    self.decode(*t, &g, &mut other)?;
                    for l in &other {
                        out[*l as usize / 64] &= !(1 << (l % 64));
                    }
                    self.other = other;
                }
            }
        }
        Ok(out.iter().any(|w| *w != 0))
    }

    fn term_into(&mut self, t: usize, group: u32, out: &mut [u64]) -> Result<bool> {
        let Some(g) = self.group_of(t, group) else {
            return Ok(false);
        };
        self.touch_src(&g.src);
        let width = u32::from(self.segment.docs.geometry.groups[group as usize].width);
        let set = self.terms[t].as_ref().expect("found above");
        match g.src {
            Src::Container { entry, bytes, .. } => {
                if entry.kind == KIND_GRID {
                    kernels::load(out, bytes);
                } else {
                    for_each_local(&entry, bytes, width, |l| {
                        out[l as usize / 64] |= 1 << (l % 64)
                    })?;
                }
            }
            Src::Locals { from, to } => {
                for l in &set.locals[from as usize..to as usize] {
                    out[*l as usize / 64] |= 1 << (l % 64);
                }
            }
        }
        Ok(true)
    }

    /// Clears the members of `out` whose positions do not satisfy the span.
    fn verify_span(
        &mut self,
        slots: &[usize],
        query: &SpanQuery,
        group: u32,
        out: &mut [u64],
    ) -> Result<()> {
        let mut solver = SpanSolver::new(query).map_err(|_| Error::Corrupt("span query"))?;
        let base = self.segment.docs.geometry.groups[group as usize].slot_base;
        let geometry = &self.segment.docs.geometry;
        let mut positions: Vec<Vec<u32>> = vec![Vec::new(); slots.len()];
        #[allow(clippy::needless_range_loop)]
        for w in 0..out.len() {
            let mut word = out[w];
            while word != 0 {
                let bit = word.trailing_zeros();
                let slot = base + (w * 64) as u32 + bit;
                for (i, t) in slots.iter().enumerate() {
                    let set = self.terms[*t].as_mut().expect("a span's terms exist");
                    set.seek(geometry, slot, self.touch);
                    debug_assert_eq!(set.current(), Some(slot));
                    let index = set.index();
                    read_positions(set.positions, index, &mut positions[i], self.touch)?;
                }
                if solver.intervals(&positions).next().is_none() {
                    out[w] &= !(1u64 << bit);
                }
                word &= word - 1;
            }
        }
        Ok(())
    }
}

/// Reads entry `index` of a positions stream (a [`segment::payload`]
/// stream) into `out`.
pub fn read_positions(
    stream: (&[u8], usize),
    index: u32,
    out: &mut Vec<u32>,
    touch: &mut impl Touch,
) -> Result<()> {
    let (bytes, at) = stream;
    let mut pos = 0;
    let count = varint::get_u32(bytes, &mut pos)?;
    if index >= count {
        return Err(Error::Corrupt("positions index"));
    }
    let interval = segment::payload::SKIP_INTERVAL;
    let slots = if count == 0 {
        0
    } else {
        (count.div_ceil(interval) - 1) as usize
    };
    let skips_at = pos;
    let data_at = skips_at + slots * 4;
    let slot = (index / interval) as usize;
    let mut entry_at = data_at;
    if slot > 0 {
        let s = skips_at + (slot - 1) * 4;
        touch.touch(Part::Positions, at + s, 4);
        entry_at += u32::from_le_bytes(
            bytes
                .get(s..s + 4)
                .ok_or(Error::Truncated)?
                .try_into()
                .expect("four"),
        ) as usize;
    }
    let from = entry_at;
    for _ in slot as u32 * interval..index {
        let n = varint::get_u32(bytes, &mut entry_at)?;
        for _ in 0..n {
            varint::get(bytes, &mut entry_at)?;
        }
    }
    let n = varint::get_u32(bytes, &mut entry_at)?;
    out.clear();
    let mut previous: Option<u32> = None;
    for _ in 0..n {
        let v = varint::get_u32(bytes, &mut entry_at)?;
        let p = match previous {
            None => v,
            Some(p) => p + v + 1,
        };
        out.push(p);
        previous = Some(p);
    }
    touch.touch(Part::Positions, at + from, entry_at - from);
    Ok(())
}

/// Opens every term of a query.
pub fn open_terms<'a>(
    segment: &Segment<'a>,
    names: &[String],
    touch: &mut impl Touch,
) -> Result<Vec<Option<TermSet<'a>>>> {
    names
        .iter()
        .map(|n| TermSet::open(segment, n, touch))
        .collect()
}

/// Counts the live documents of `segment` matching `node`.
pub fn count(
    segment: &Segment<'_>,
    node: &Node,
    names: &[String],
    touch: &mut impl Touch,
) -> Result<u64> {
    let mut terms = open_terms(segment, names, touch)?;
    count_terms(segment, node, &mut terms, touch)
}

/// [`count`] over terms opened already.
pub fn count_terms<'a>(
    segment: &Segment<'a>,
    node: &Node,
    terms: &mut [Option<TermSet<'a>>],
    touch: &mut impl Touch,
) -> Result<u64> {
    let live = &segment.liveness;
    touch.touch(Part::Liveness, segment.area_at(Area::Liveness), 1);
    // A term alone is its document frequency when nothing is dead.
    if let Node::Term(t) = node {
        let Some(set) = &terms[*t] else { return Ok(0) };
        if live.dead == 0 {
            return Ok(u64::from(set.df));
        }
    }
    let geometry = &segment.docs.geometry;
    for set in terms.iter_mut().flatten() {
        set.ensure_groups(geometry, touch);
    }
    let all = geometry.groups.len();
    let groups = candidate_groups(node, terms, all);
    let hints = vec![0usize; terms.len()];
    let mut fold = Fold {
        segment,
        terms,
        hints,
        scratch: Scratch::default(),
        touch,
        list: Vec::new(),
        other: Vec::new(),
    };
    let mut total = 0u64;
    let mut out = Vec::new();
    for group in groups {
        let dead = live.groups[group as usize].as_deref();
        // An OR whose group only one term holds counts that term's members;
        // an AND of two grids counts their intersection without a copy.
        if dead.is_none()
            && let Some(n) = fold.shortcut(node, group)
        {
            total += n;
            continue;
        }
        let words = geometry.groups[group as usize].words();
        out.clear();
        out.resize(words, 0);
        if !fold.eval(node, group, &mut out, 0)? {
            continue;
        }
        if let Some(dead) = dead {
            for (o, d) in out.iter_mut().zip(dead) {
                *o &= !d;
            }
        }
        total += kernels::popcount(&out);
    }
    Ok(total)
}

impl<'a, T: Touch> Fold<'_, 'a, T> {
    fn shortcut(&mut self, node: &Node, group: u32) -> Option<u64> {
        match node {
            Node::Or(children) if children.iter().all(|c| matches!(c, Node::Term(_))) => {
                let mut held = None;
                for child in children {
                    let Node::Term(t) = child else { unreachable!() };
                    if let Some(g) = self.group_of(*t, group) {
                        if held.is_some() {
                            return None;
                        }
                        held = Some(g.count);
                    }
                }
                held.map(u64::from)
            }
            Node::Term(t) => self.group_of(*t, group).map(|g| u64::from(g.count)),
            Node::And(children) if children.len() == 2 => {
                let (Node::Term(a), Node::Term(b)) = (&children[0], &children[1]) else {
                    return None;
                };
                let ga = self.group_of(*a, group)?;
                let gb = self.group_of(*b, group)?;
                match (ga.src, gb.src) {
                    (
                        Src::Container {
                            entry: ea,
                            bytes: xa,
                            ..
                        },
                        Src::Container {
                            entry: eb,
                            bytes: xb,
                            ..
                        },
                    ) if ea.kind == KIND_GRID && eb.kind == KIND_GRID => {
                        self.touch_src(&ga.src);
                        self.touch_src(&gb.src);
                        Some(kernels::and_count_two(xa, xb))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

// ---- Ranked top-k ----

/// One scoring term of a ranked walk.
struct Scoring {
    term: usize,
    scorer: TermScorer,
    footer: Footer,
    /// Per footer block, its score bound.
    bounds: Vec<f32>,
    block: usize,
}

/// A ranked answer and what the walk did.
#[derive(Debug, Default)]
pub struct RankedAnswer {
    pub rows: Vec<(f32, Tid)>,
    pub scored: u64,
    pub windows: u64,
    pub windows_pruned: u64,
}

#[derive(Clone, Copy, PartialEq)]
struct Entry(f32, Tid);
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Entry {
    /// Best first, as [`crate::walk::rank`]; the heap's top is the worst.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        crate::walk::rank(&(self.0, self.1), &(other.0, other.1))
    }
}

/// A bound below the threshold by more than rounding can explain: partial
/// sums are taken in another order than the score's.
#[inline]
fn below(bound: f64, theta: Option<f32>) -> bool {
    theta.is_some_and(|theta| bound * (1.0 + 1e-5) + 1e-30 <= f64::from(theta))
}

/// The `k` best matches of `node` in `segment` by the summed scores of
/// `scorers` (named by term, in the scorer's order), with block-max
/// pruning; ties by ctid. Matches holding no scoring term score zero.
///
/// A query some terms of which every match holds (a conjunction, a phrase)
/// is led by the rarest of them; any other by block-max MaxScore over its
/// scoring terms: the essential terms, those whose bounds with every lower
/// one's reach the threshold, propose candidates, the others are only
/// tested. Candidates come in ctid order, so a candidate scoring exactly
/// the threshold ranks after the `k`-th row and is skipped like a lower one.
pub fn top_k(
    segment: &Segment<'_>,
    node: &Node,
    names: &[String],
    scorers: &[(String, TermScorer)],
    k: usize,
    touch: &mut impl Touch,
) -> Result<RankedAnswer> {
    let mut terms = open_terms(segment, names, touch)?;
    let geometry = &segment.docs.geometry;
    let mut answer = RankedAnswer::default();
    let mut scoring: Vec<Scoring> = Vec::new();
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
        let bounds: Vec<f32> = (0..footer.blocks())
            .map(|b| {
                footer
                    .frontier_of(b)
                    .iter()
                    .map(|(bucket, length)| {
                        scorer
                            .bound_through(TfBucket::new(*bucket).expect("a valid bucket"), *length)
                    })
                    .fold(0.0_f32, f32::max)
            })
            .collect();
        scoring.push(Scoring {
            term: t,
            scorer: scorer.clone(),
            footer,
            bounds,
            block: 0,
        });
    }
    let required = required_terms(node);
    if required.iter().any(|t| terms[*t].is_none()) {
        return Ok(answer);
    }
    // Leaves only: a candidate holding a scoring term matches a flat OR, and
    // one holding every required term matches a flat AND.
    let flat = match node {
        Node::Or(c) | Node::And(c) => c.iter().all(|c| matches!(c, Node::Term(_))),
        Node::Term(_) => true,
        _ => false,
    };
    let mut heap: std::collections::BinaryHeap<Entry> =
        std::collections::BinaryHeap::with_capacity(k + 1);
    let threshold = |heap: &std::collections::BinaryHeap<Entry>| {
        (heap.len() >= k)
            .then(|| heap.peek().map(|e| e.0))
            .flatten()
    };
    let mut positions: Vec<Vec<u32>> = Vec::new();
    let n = scoring.len();
    let mut held: Vec<Option<f32>> = vec![None; n];
    let mut wb = vec![0.0_f32; n];
    let mut walk = Walk {
        segment,
        geometry,
        node,
        flat,
        k,
        group: std::cell::Cell::new(0),
    };

    if !required.is_empty() {
        // Led by the rarest required term; the others probed rarest first.
        let mut req: Vec<(u32, usize)> = required
            .iter()
            .map(|t| (terms[*t].as_ref().expect("checked").df, *t))
            .collect();
        req.sort_unstable();
        let lead = req[0].1;
        let mut cursor = 0u32;
        'windows: while let Some(mut slot) = terms[lead]
            .as_mut()
            .expect("lead")
            .seek(geometry, cursor, touch)
        {
            let window_end = scoring
                .iter()
                .map(|s| block_end(&s.footer, slot))
                .min()
                .unwrap_or(u32::MAX);
            answer.windows += 1;
            let theta = threshold(&heap);
            if theta.is_some() {
                let mut bound = 0.0_f64;
                for s in scoring.iter_mut() {
                    bound += f64::from(window_bound(s, slot, window_end));
                }
                if below(bound, theta) {
                    answer.windows_pruned += 1;
                    if window_end == u32::MAX {
                        break;
                    }
                    cursor = window_end + 1;
                    continue;
                }
            }
            loop {
                // Every required term, rarest first; a miss moves the lead.
                let mut miss = None;
                for (_, t) in &req[1..] {
                    let at = terms[*t]
                        .as_mut()
                        .expect("required")
                        .seek(geometry, slot, touch);
                    if at != Some(slot) {
                        miss = Some(at);
                        break;
                    }
                }
                let next_target = match miss {
                    Some(None) => break 'windows,
                    Some(Some(at)) => at,
                    None => {
                        walk.consider(
                            &mut terms,
                            &scoring,
                            &mut held,
                            slot,
                            &mut heap,
                            &mut positions,
                            &mut answer,
                            touch,
                        )?;
                        // Nothing scores: every later match ties at zero and
                        // ranks after the first k.
                        if n == 0 && heap.len() >= k {
                            break 'windows;
                        }
                        slot + 1
                    }
                };
                if next_target > window_end {
                    if window_end == u32::MAX {
                        break 'windows;
                    }
                    cursor = next_target;
                    continue 'windows;
                }
                match terms[lead]
                    .as_mut()
                    .expect("lead")
                    .seek(geometry, next_target, touch)
                {
                    Some(s) if s <= window_end => slot = s,
                    Some(s) => {
                        cursor = s;
                        continue 'windows;
                    }
                    None => break 'windows,
                }
                // A raised threshold may now prune the rest of the window.
                if threshold(&heap) != theta {
                    cursor = slot;
                    continue 'windows;
                }
            }
        }
    } else if n > 0 {
        // Block-max MaxScore sieved a group at a time. A group whose terms'
        // block bounds cannot reach the threshold is skipped unread. Else
        // each term's members there become a bitmap; the essential terms
        // (whose group bounds, with every lower one's, reach the threshold)
        // propose candidates 64 slots at a time, and a candidate is bounded
        // by the bucket of each term it holds at its block's shortest
        // document before its length is read. A posting's index is its
        // group's first plus the members before it, counted by words.
        for set in terms.iter_mut().flatten() {
            set.ensure_groups(geometry, touch);
        }
        let all = geometry.groups.len();
        let mut present_groups = vec![0u64; all.div_ceil(64)];
        for sc in &scoring {
            for g in &terms[sc.term].as_ref().expect("scoring").groups {
                present_groups[g.index as usize / 64] |= 1 << (g.index % 64);
            }
        }
        let mut hints = vec![0usize; n];
        let mut entry: Vec<Option<G<'_>>> = vec![None; n];
        let mut tw: Vec<Vec<u64>> = vec![Vec::new(); n];
        let mut run = vec![0u32; n];
        let mut idx = vec![0u32; n];
        let mut order: Vec<usize> = Vec::with_capacity(n);
        let mut ub = vec![0.0_f32; n];
        for (pw, word) in present_groups.iter().enumerate() {
            let mut word = *word;
            while word != 0 {
                let g = (pw * 64) as u32 + word.trailing_zeros();
                word &= word - 1;
                let group = geometry.groups[g as usize];
                let start = group.slot_base;
                let end = start + group.slots() - 1;
                let mut theta = threshold(&heap);
                answer.windows += 1;
                let mut total = 0.0_f64;
                for i in 0..n {
                    let set = terms[scoring[i].term].as_ref().expect("scoring");
                    entry[i] = set.find(g, &mut hints[i]).copied();
                    wb[i] = if entry[i].is_some() {
                        window_bound(&mut scoring[i], start, end)
                    } else {
                        0.0
                    };
                    total += f64::from(wb[i]);
                }
                if below(total, theta) {
                    answer.windows_pruned += 1;
                    continue;
                }
                order.clear();
                order.extend((0..n).filter(|i| entry[*i].is_some()));
                order.sort_unstable_by(|a, b| wb[*a].total_cmp(&wb[*b]));
                let mut first = 0;
                let mut rest = 0.0_f64;
                while first < order.len() && below(rest + f64::from(wb[order[first]]), theta) {
                    rest += f64::from(wb[order[first]]);
                    first += 1;
                }
                let (non, essential) = order.split_at(first);
                // The members of every present term, as words.
                let words = group.words();
                let width = u32::from(group.width);
                for &i in non.iter().chain(essential) {
                    let e = entry[i].expect("present");
                    let set = terms[scoring[i].term].as_ref().expect("scoring");
                    let out = &mut tw[i];
                    out.clear();
                    out.resize(words, 0);
                    match e.src {
                        Src::Container {
                            entry: ge,
                            bytes,
                            at,
                        } => {
                            touch.touch(Part::Payload, at, bytes.len());
                            if ge.kind == KIND_GRID {
                                kernels::load(out, bytes);
                            } else {
                                for_each_local(&ge, bytes, width, |l| {
                                    out[l as usize / 64] |= 1 << (l % 64)
                                })?;
                            }
                            run[i] = ge.first;
                        }
                        Src::Locals { from, to } => {
                            for l in &set.locals[from as usize..to as usize] {
                                out[*l as usize / 64] |= 1 << (l % 64);
                            }
                            run[i] = from;
                        }
                    }
                }
                // Each term's word `w` is read alongside the others'.
                #[allow(clippy::needless_range_loop)]
                for w in 0..words {
                    let mut cand = 0u64;
                    for &i in essential {
                        cand |= tw[i][w];
                    }
                    while cand != 0 {
                        let bit = cand.trailing_zeros();
                        cand &= cand - 1;
                        let below_bit = (1u64 << bit) - 1;
                        let slot = start + (w * 64) as u32 + bit;
                        held.iter_mut().for_each(|h| *h = None);
                        let mut bound = 0.0_f64;
                        for &i in essential {
                            let x = tw[i][w];
                            if x >> bit & 1 == 1 {
                                idx[i] = run[i] + (x & below_bit).count_ones();
                                let set = terms[scoring[i].term].as_ref().expect("scoring");
                                ub[i] = walk.bucket_bound_at(set, &scoring[i], idx[i], touch)?;
                                held[i] = Some(0.0);
                                bound += f64::from(ub[i]);
                            }
                        }
                        answer.scored += 1;
                        let mut left = rest;
                        let mut pruned = below(bound + left, theta);
                        if !pruned {
                            for &i in non.iter().rev() {
                                let x = tw[i][w];
                                if x >> bit & 1 == 1 {
                                    idx[i] = run[i] + (x & below_bit).count_ones();
                                    let set = terms[scoring[i].term].as_ref().expect("scoring");
                                    ub[i] =
                                        walk.bucket_bound_at(set, &scoring[i], idx[i], touch)?;
                                    held[i] = Some(0.0);
                                    bound += f64::from(ub[i]);
                                }
                                left -= f64::from(wb[i]);
                                if below(bound + left, theta) {
                                    pruned = true;
                                    break;
                                }
                            }
                        }
                        if pruned {
                            continue;
                        }
                        let mut length = None;
                        for i in 0..n {
                            if held[i].is_some() {
                                let set = terms[scoring[i].term].as_ref().expect("scoring");
                                held[i] = Some(walk.contribution_at(
                                    set,
                                    &scoring[i],
                                    idx[i],
                                    slot,
                                    &mut length,
                                    touch,
                                )?);
                            }
                        }
                        walk.admit(&mut terms, &held, slot, &mut heap, &mut positions, touch)?;
                        theta = threshold(&heap);
                    }
                    for &i in non.iter().chain(essential) {
                        run[i] += tw[i][w].count_ones();
                    }
                }
            }
        }
    }
    // Zero fill: fewer than k rows, the rest are matches of non-scoring
    // terms in ctid order (a led walk saw every match already).
    let positive = heap.len();
    let mut rows: Vec<(f32, Tid)> = heap.into_iter().map(|e| (e.0, e.1)).collect();
    if positive < k && required.is_empty() {
        let seen: std::collections::HashSet<Tid> = rows.iter().map(|r| r.1).collect();
        let fill = first_matches(segment, node, &mut terms, k - positive, &seen, touch)?;
        rows.extend(fill.into_iter().map(|t| (0.0, t)));
    }
    rows.sort_by(crate::walk::rank);
    rows.truncate(k);
    answer.rows = rows;
    Ok(answer)
}

/// What a walk's candidates share.
struct Walk<'s, 'a> {
    segment: &'s Segment<'a>,
    geometry: &'s Geometry,
    node: &'s Node,
    flat: bool,
    k: usize,
    /// The group of the last rank asked for.
    group: std::cell::Cell<usize>,
}

impl<'a> Walk<'_, 'a> {
    /// The document rank of `slot`, through the last group asked for.
    #[inline]
    fn rank(&self, slot: u32) -> Option<u32> {
        let groups = &self.geometry.groups;
        let mut g = self.group.get();
        let within = |g: usize| {
            groups[g].slot_base <= slot && slot < groups[g].slot_base + groups[g].slots()
        };
        if g >= groups.len() || !within(g) {
            g = self.geometry.group_of_slot(slot);
            self.group.set(g);
        }
        self.segment.docs.rank_in(g, slot - groups[g].slot_base)
    }

    /// A held term's score for the candidate at `slot`, its bucket from the
    /// TF tail and its length from the DL sidecar (read once).
    #[inline]
    fn contribution(
        &self,
        set: &TermSet<'a>,
        s: &Scoring,
        slot: u32,
        length: &mut Option<u32>,
        touch: &mut impl Touch,
    ) -> Result<f32> {
        let index = set.index();
        let bucket = s.footer.bucket(set.postings.tf, index)?;
        if s.footer.single.is_none() {
            let block = (index / s.footer.block_size) as usize;
            touch.touch(
                Part::TfTail,
                set.at + set.postings.tf_at + s.footer.tf_at[block] as usize,
                1,
            );
        }
        let length = match *length {
            Some(l) => l,
            None => {
                let rank = self
                    .rank(slot)
                    .ok_or(Error::Corrupt("a posting without a document"))?;
                touch.touch(Part::DlSidecar, self.segment.length_at(rank), 2);
                let l = self.segment.lengths.get(rank)?;
                *length = Some(l);
                l
            }
        };
        Ok(s.scorer
            .score_bucket(TfBucket::new(bucket).ok_or(Error::InvalidTfBucket)?, length))
    }

    /// [`Self::bucket_bound`] of posting `index`.
    #[inline]
    fn bucket_bound_at(
        &self,
        set: &TermSet<'a>,
        s: &Scoring,
        index: u32,
        touch: &mut impl Touch,
    ) -> Result<f32> {
        let bucket = s.footer.bucket(set.postings.tf, index)?;
        let block = if s.footer.single.is_some() {
            0
        } else {
            (index / s.footer.block_size) as usize
        };
        if s.footer.single.is_none() {
            touch.touch(
                Part::TfTail,
                set.at + set.postings.tf_at + s.footer.tf_at[block] as usize,
                1,
            );
        }
        let shortest = s.footer.frontier_of(block)[0].1;
        Ok(s.scorer.bound_through(
            TfBucket::new(bucket).ok_or(Error::InvalidTfBucket)?,
            shortest,
        ))
    }

    /// [`Self::contribution`] of posting `index`.
    #[inline]
    fn contribution_at(
        &self,
        set: &TermSet<'a>,
        s: &Scoring,
        index: u32,
        slot: u32,
        length: &mut Option<u32>,
        touch: &mut impl Touch,
    ) -> Result<f32> {
        let bucket = s.footer.bucket(set.postings.tf, index)?;
        let length = match *length {
            Some(l) => l,
            None => {
                let rank = self
                    .rank(slot)
                    .ok_or(Error::Corrupt("a posting without a document"))?;
                touch.touch(Part::DlSidecar, self.segment.length_at(rank), 2);
                let l = self.segment.lengths.get(rank)?;
                *length = Some(l);
                l
            }
        };
        Ok(s.scorer
            .score_bucket(TfBucket::new(bucket).ok_or(Error::InvalidTfBucket)?, length))
    }

    /// Scores a led candidate holding every required term.
    #[allow(clippy::too_many_arguments)]
    fn consider(
        &mut self,
        terms: &mut [Option<TermSet<'a>>],
        scoring: &[Scoring],
        held: &mut [Option<f32>],
        slot: u32,
        heap: &mut std::collections::BinaryHeap<Entry>,
        positions: &mut Vec<Vec<u32>>,
        answer: &mut RankedAnswer,
        touch: &mut impl Touch,
    ) -> Result<()> {
        answer.scored += 1;
        let mut length = None;
        for (i, s) in scoring.iter().enumerate() {
            let set = terms[s.term].as_mut().expect("scoring");
            held[i] = if set.seek(self.geometry, slot, touch) == Some(slot) {
                Some(self.contribution(set, s, slot, &mut length, touch)?)
            } else {
                None
            };
        }
        self.admit(terms, held, slot, heap, positions, touch)
    }

    /// Adds the candidate at `slot` to the top k if its score, summed in
    /// the scorer's order, beats the threshold and it matches the query.
    fn admit(
        &mut self,
        terms: &mut [Option<TermSet<'a>>],
        held: &[Option<f32>],
        slot: u32,
        heap: &mut std::collections::BinaryHeap<Entry>,
        positions: &mut Vec<Vec<u32>>,
        touch: &mut impl Touch,
    ) -> Result<()> {
        let mut total = 0.0_f32;
        for c in held.iter().flatten() {
            total += c;
        }
        let full = heap.len() >= self.k;
        if full && heap.peek().is_some_and(|worst| total <= worst.0) {
            return Ok(());
        }
        if !self.flat && !matches(self.segment, self.node, terms, slot, positions, touch)? {
            return Ok(());
        }
        heap.push(Entry(total, self.geometry.tid_of(slot)));
        if heap.len() > self.k {
            heap.pop();
        }
        Ok(())
    }
}

/// The last slot of the footer block holding `slot`, or before it.
fn block_end(footer: &Footer, slot: u32) -> u32 {
    let b = footer.last.partition_point(|l| *l < slot);
    footer.last.get(b).copied().unwrap_or(u32::MAX)
}

/// The best bound of `s`'s blocks overlapping `from..=to`.
fn window_bound(s: &mut Scoring, from: u32, to: u32) -> f32 {
    let last = &s.footer.last;
    while s.block < last.len() && last[s.block] < from {
        s.block += 1;
    }
    let mut bound = 0.0_f32;
    let mut b = s.block;
    while b < last.len() {
        bound = bound.max(s.bounds[b]);
        if last[b] >= to {
            break;
        }
        b += 1;
    }
    bound
}

/// Terms every match holds.
fn required_terms(node: &Node) -> Vec<usize> {
    match node {
        Node::Term(t) => vec![*t],
        Node::And(children) => {
            let mut out: Vec<usize> = children.iter().flat_map(required_terms).collect();
            out.sort_unstable();
            out.dedup();
            out
        }
        Node::Span { slots, .. } => {
            let mut out = slots.clone();
            out.sort_unstable();
            out.dedup();
            out
        }
        _ => Vec::new(),
    }
}

/// Whether the document at `slot` matches `node`; cursors only move
/// forward, so slots must be asked in increasing order.
fn matches<'a>(
    segment: &Segment<'a>,
    node: &Node,
    terms: &mut [Option<TermSet<'a>>],
    slot: u32,
    positions: &mut Vec<Vec<u32>>,
    touch: &mut impl Touch,
) -> Result<bool> {
    let geometry = &segment.docs.geometry;
    Ok(match node {
        Node::Term(t) => match terms[*t].as_mut() {
            Some(set) => set.seek(geometry, slot, touch) == Some(slot),
            None => false,
        },
        Node::And(children) => {
            for child in children {
                if !matches(segment, child, terms, slot, positions, touch)? {
                    return Ok(false);
                }
            }
            true
        }
        Node::Or(children) => {
            let mut any = false;
            for child in children {
                // Every child is asked, so its cursors keep up.
                any |= matches(segment, child, terms, slot, positions, touch)?;
            }
            any
        }
        Node::Not(inner) => !matches(segment, inner, terms, slot, positions, touch)?,
        Node::Span { slots, query } => {
            positions.resize(slots.len(), Vec::new());
            for (i, t) in slots.iter().enumerate() {
                let Some(set) = terms[*t].as_mut() else {
                    return Ok(false);
                };
                if set.seek(geometry, slot, touch) != Some(slot) {
                    return Ok(false);
                }
                let index = set.index();
                read_positions(set.positions, index, &mut positions[i], touch)?;
            }
            let mut solver = SpanSolver::new(query).map_err(|_| Error::Corrupt("span query"))?;
            solver.intervals(&*positions).next().is_some()
        }
    })
}

/// The first `n` matches in ctid order not in `skip`.
fn first_matches<'a>(
    segment: &Segment<'a>,
    node: &Node,
    terms: &mut [Option<TermSet<'a>>],
    n: usize,
    skip: &std::collections::HashSet<Tid>,
    touch: &mut impl Touch,
) -> Result<Vec<Tid>> {
    // Fresh cursors: the walk moved them.
    for set in terms.iter_mut().flatten() {
        set.gpos = 0;
        set.loaded = Loaded::None;
        set.current = None;
        set.sparse = None;
    }
    let geometry = &segment.docs.geometry;
    let mut out = Vec::new();
    let mut positions = Vec::new();
    let leaves = leaf_terms(node);
    let mut cursor = 0u32;
    while out.len() < n {
        // The least slot any leaf holds at or after the cursor.
        let mut next: Option<u32> = None;
        for t in &leaves {
            if let Some(set) = terms[*t].as_mut()
                && let Some(s) = set.seek(geometry, cursor, touch)
            {
                next = Some(next.map_or(s, |b| b.min(s)));
            }
        }
        let Some(slot) = next else { break };
        if matches(segment, node, terms, slot, &mut positions, touch)? {
            let tid = geometry.tid_of(slot);
            if !skip.contains(&tid) {
                out.push(tid);
            }
        }
        cursor = slot + 1;
    }
    Ok(out)
}

fn leaf_terms(node: &Node) -> Vec<usize> {
    match node {
        Node::Term(t) => vec![*t],
        Node::And(c) | Node::Or(c) => c.iter().flat_map(leaf_terms).collect(),
        Node::Not(_) => Vec::new(),
        Node::Span { slots, .. } => slots.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bm25::Bm25Params;
    use proptest::prelude::*;
    use segment::payload::PayloadBuilder;
    use segment::tinshape::postings::Options;
    use segment::tinshape::segment::Builder;

    /// A segment of `docs` (ctids ascending) where term `t` holds the
    /// documents `members[t]` with `tfs`, and its brute-force view.
    fn build(docs: &[Tid], members: &[Vec<(usize, u32)>], options: Options) -> Vec<u8> {
        let lengths: Vec<u32> = (0..docs.len()).map(|i| (i as u32 * 37) % 90 + 5).collect();
        let mut builder = Builder::new(docs.to_vec(), lengths, options).unwrap();
        for (t, m) in members.iter().enumerate() {
            if m.is_empty() {
                continue;
            }
            let ranks: Vec<u32> = m.iter().map(|(r, _)| *r as u32).collect();
            let buckets: Vec<u8> = m
                .iter()
                .map(|(_, tf)| TfBucket::from_count(*tf).value())
                .collect();
            let mut payload = PayloadBuilder::default();
            for (_, tf) in m {
                let positions: Vec<u32> = (0..*tf).collect();
                payload.push(&positions).unwrap();
            }
            builder
                .add_term(&format!("t{t}"), &ranks, &buckets, &payload.finish())
                .unwrap();
        }
        builder.finish(&[]).0
    }

    fn holds(members: &[Vec<(usize, u32)>], t: usize, rank: usize) -> Option<u32> {
        members[t]
            .iter()
            .find(|(r, _)| *r == rank)
            .map(|(_, tf)| *tf)
    }

    fn eval(node: &Node, members: &[Vec<(usize, u32)>], rank: usize) -> bool {
        match node {
            Node::Term(t) => holds(members, *t, rank).is_some(),
            Node::And(c) => c.iter().all(|c| eval(c, members, rank)),
            Node::Or(c) => c.iter().any(|c| eval(c, members, rank)),
            Node::Not(inner) => !eval(inner, members, rank),
            Node::Span { .. } => unreachable!(),
        }
    }

    fn shapes() -> Vec<Node> {
        use Node::*;
        vec![
            Term(0),
            Or(vec![Term(0), Term(1), Term(2)]),
            And(vec![Term(0), Term(1)]),
            And(vec![Term(2), Term(0), Term(3)]),
            And(vec![Term(0), Not(Box::new(Term(1)))]),
            And(vec![Or(vec![Term(1), Term(2)]), Not(Box::new(Term(3)))]),
            Or(vec![And(vec![Term(0), Term(1)]), Term(3)]),
            Not(Box::new(Term(0))),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]
        #[test]
        fn counts_and_top_k_match_brute_force(
            doc_set in prop::collection::btree_set((0u32..1500, 1u16..=40), 1..700),
            density in prop::collection::vec(1u32..100, 4),
            seed in any::<u64>(),
            grid_density in prop::sample::select(vec![0u32, 4, 64]),
            block_size in prop::sample::select(vec![2u32, 16, 128]),
            k in 1usize..12,
        ) {
            let docs: Vec<Tid> = doc_set.into_iter().map(|(block, offset)| Tid { block, offset }).collect();
            // Term t holds a document with probability density[t] percent.
            let mut state = seed | 1;
            let mut next = || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            };
            let mut members: Vec<Vec<(usize, u32)>> = vec![Vec::new(); density.len()];
            for (t, d) in density.iter().enumerate() {
                for r in 0..docs.len() {
                    if next() % 100 < u64::from(*d) {
                        let tf = (next() % 6 + 1) as u32;
                        members[t].push((r, tf));
                    }
                }
            }
            let options = Options { block_size, grid_density, ..Options::default() };
            let blob = build(&docs, &members, options);
            let segment = Segment::parse(&blob).unwrap();
            let names: Vec<String> = (0..4).map(|t| format!("t{t}")).collect();
            let lengths: Vec<u32> = (0..docs.len()).map(|i| (i as u32 * 37) % 90 + 5).collect();
            for node in shapes() {
                let want = (0..docs.len()).filter(|r| eval(&node, &members, *r)).count() as u64;
                let got = count(&segment, &node, &names, &mut NoTouch).unwrap();
                prop_assert_eq!(got, want, "count of {:?}", node);
                if matches!(node, Node::Not(_)) {
                    continue;
                }
                // Top k by BM25 over the node's positive terms, exhaustively.
                let scored: Vec<usize> = leaf_terms(&node);
                let mut scorers = Vec::new();
                for t in &scored {
                    if members[*t].is_empty() || scorers.iter().any(|(n, _): &(String, TermScorer)| *n == names[*t]) {
                        continue;
                    }
                    let scorer = TermScorer::from_statistics(docs.len() as u64, members[*t].len() as u64, 1.0, Bm25Params::default(), 50.0).unwrap();
                    scorers.push((names[*t].clone(), scorer));
                }
                let mut want: Vec<(f32, Tid)> = (0..docs.len())
                    .filter(|r| eval(&node, &members, *r))
                    .map(|r| {
                        let mut total = 0.0_f32;
                        for (name, scorer) in &scorers {
                            let t: usize = name[1..].parse().unwrap();
                            if let Some(tf) = holds(&members, t, r) {
                                total += scorer.score_bucket(TfBucket::from_count(tf), lengths[r]);
                            }
                        }
                        (total, docs[r])
                    })
                    .collect();
                want.sort_by(crate::walk::rank);
                want.truncate(k);
                let got = top_k(&segment, &node, &names, &scorers, k, &mut NoTouch).unwrap();
                let bits = |rows: &[(f32, Tid)]| rows.iter().map(|(s, t)| (s.to_bits(), *t)).collect::<Vec<_>>();
                prop_assert_eq!(bits(&got.rows), bits(&want), "top {} of {:?}", k, node);
            }
        }
    }
}
