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
use segment::tinshape::bits;
use segment::tinshape::blob::Bytes;
use segment::tinshape::docs::{Geometry, Group};
use segment::tinshape::ef::EfCursor;
use segment::tinshape::positions::Positions;
use segment::tinshape::postings::{Form, GroupEntry, KIND_GRID, Postings, for_each_local};
use segment::tinshape::segment::{Area, Segment};
use segment::{Error, Result};
use tinql::runtime::{Query, SpanTermSlot};

use crate::bm25::TermScorer;

mod rank;

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

    /// Set bits in a byte string (a grid's words, as stored).
    #[inline]
    pub fn popcount_bytes(bytes: &[u8]) -> u32 {
        #[cfg(target_arch = "aarch64")]
        // SAFETY: NEON is baseline on aarch64; every load is within `bytes`.
        // Each 16-bit lane gains at most 32 per step, so it cannot overflow
        // below 2,048 steps (64 KiB; a group's grid is at most 9.3 KiB).
        unsafe {
            use core::arch::aarch64::*;
            let n = bytes.len();
            let p = bytes.as_ptr();
            let mut i = 0;
            let mut total = 0u32;
            while i + 32 <= n {
                let mut acc = vdupq_n_u16(0);
                let end = (i + 32 * 2048).min(n - n % 32);
                while i < end {
                    let a = vcntq_u8(vld1q_u8(p.add(i)));
                    let b = vcntq_u8(vld1q_u8(p.add(i + 16)));
                    acc = vpadalq_u8(acc, vaddq_u8(a, b));
                    i += 32;
                }
                total += vaddlvq_u16(acc);
            }
            while i + 8 <= n {
                total +=
                    u64::from_le_bytes(bytes[i..i + 8].try_into().expect("eight")).count_ones();
                i += 8;
            }
            while i < n {
                total += bytes[i].count_ones();
                i += 1;
            }
            total
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            let mut chunks = bytes.chunks_exact(8);
            let mut total: u32 = (&mut chunks)
                .map(|c| u64::from_le_bytes(c.try_into().expect("eight")).count_ones())
                .sum();
            total += chunks
                .remainder()
                .iter()
                .map(|b| b.count_ones())
                .sum::<u32>();
            total
        }
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
        // Leaf boosts weigh a span's terms in a score, not which documents
        // match it. A span is folded as the AND of its words, then checked
        // by positions: only one every word of which must occur.
        Query::Span {
            term_slots,
            span_query,
            position_filter: None,
            ..
        } if crate::walk::span_requires_all(span_query) => {
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
    /// A container of a grouped record, with its blob offset; its bytes
    /// are read when the group is (a segment over a lazy blob loads them
    /// then).
    Container {
        entry: GroupEntry,
        bytes: Bytes<'a>,
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

/// A container's bytes, read for a path that cannot return an error: a
/// failed load is reported as corruption.
#[inline]
fn container_bytes(bytes: Bytes<'_>) -> &[u8] {
    bytes
        .all()
        .unwrap_or_else(|e| crate::corrupt(format!("Stannum postings: {e}")))
}

/// One query term opened in a segment.
pub struct TermSet<'a> {
    pub df: u32,
    pub max_bucket: u8,
    pub postings: Postings<'a>,
    /// Where its record starts in the blob.
    pub at: usize,
    /// The positions stream and its blob offset.
    pub positions: (segment::tinshape::blob::Bytes<'a>, usize),
    groups: Vec<G<'a>>,
    /// Local slots of a single or sparse term, all groups in a row.
    locals: Vec<u32>,
    // Cursor state, for ranked walks and phrase checks.
    /// The target of the last seek: the cursor may move forward from it.
    sought: u32,
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
                groups.reserve_exact(entries.len());
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
            sought: 0,
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
        self.sought = self.sought.max(target);
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
            let group = *group;
            let local_target = target.saturating_sub(base);
            if matches!(self.loaded, Loaded::None) {
                self.load(&g, &group, touch);
            }
            let first = self.first_of(&g);
            let found = match (&self.loaded, g.src) {
                (Loaded::Grid, Src::Container { bytes, .. }) => {
                    let bytes = container_bytes(bytes);
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

    fn load(&mut self, g: &G<'a>, group: &Group, touch: &mut impl Touch) {
        self.pos = 0;
        match g.src {
            Src::Container { entry, bytes, at } => {
                touch.touch(Part::Payload, at, bytes.len());
                let bytes = container_bytes(bytes);
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
                    for_each_local(&entry, bytes, group, |l| list.push(l))
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

    /// Moves the cursor back before the first slot.
    pub fn rewind(&mut self) {
        self.sought = 0;
        self.gpos = 0;
        self.loaded = Loaded::None;
        self.current = None;
        self.sparse = None;
    }

    /// The largest target sought since the cursor was opened or rewound:
    /// a seek to a slot below it needs a [`Self::rewind`] first.
    pub fn sought(&self) -> u32 {
        self.sought
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
                        kernels::and_bytes(out, bytes.all()?);
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
            kernels::or_bytes(out, bytes.all()?);
            return Ok(true);
        }
        self.term_into(t, group, out)
    }

    /// The members of term `t`'s group `g`, appended to `list` in order.
    fn decode(&mut self, t: usize, g: &G<'a>, list: &mut Vec<u32>) -> Result<()> {
        self.touch_src(&g.src);
        let group = self.segment.docs.geometry.groups[g.index as usize];
        match g.src {
            Src::Container { entry, bytes, .. } => {
                for_each_local(&entry, bytes.all()?, &group, |l| list.push(l))?;
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
                let bytes = bytes.all()?;
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
                        let bytes = bytes.all()?;
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
                    for (w, c) in out.iter_mut().zip(bytes.all()?.chunks_exact(8)) {
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
        let geometry_group = self.segment.docs.geometry.groups[group as usize];
        let set = self.terms[t].as_ref().expect("found above");
        match g.src {
            Src::Container { entry, bytes, .. } => {
                let bytes = bytes.all()?;
                if entry.kind == KIND_GRID {
                    kernels::load(out, bytes);
                } else {
                    for_each_local(&entry, bytes, &geometry_group, |l| {
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
                    // Another span of the query may have read this term
                    // further into the group.
                    if slot < set.sought() {
                        set.rewind();
                    }
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

/// Reads entry `index` of a positions stream (a
/// [`segment::tinshape::positions`] stream) into `out`.
pub fn read_positions(
    stream: (segment::tinshape::blob::Bytes<'_>, usize),
    index: u32,
    out: &mut Vec<u32>,
    touch: &mut impl Touch,
) -> Result<()> {
    let (bytes, at) = stream;
    let positions = Positions::parse(bytes)?;
    let (mut entry, mut p, reads) = positions.locate(index)?;
    for (read_at, len) in reads {
        if len > 0 {
            touch.touch(Part::Positions, at + read_at, len);
        }
    }
    if let Some(m) = positions.mask_at(index) {
        touch.touch(Part::Positions, at + m, 4);
    }
    let from = p;
    while entry < index {
        p = positions.skip(entry, p)?;
        entry += 1;
    }
    let end = positions.read_entry(index, p, out)?;
    touch.touch(Part::Positions, at + from, end - from);
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

/// The live documents of `segment` matching `node`, a group at a time:
/// `emit` is called with each group holding any, ascending, and the
/// group's words over its slots (bit `local` for slot `local`; see
/// [`Geometry::tid_in`]). Ctid order is group order, then slot order.
pub fn for_each_match<'a>(
    segment: &Segment<'a>,
    node: &Node,
    terms: &mut [Option<TermSet<'a>>],
    touch: &mut impl Touch,
    emit: &mut dyn FnMut(usize, &[u64]),
) -> Result<()> {
    let live = &segment.liveness;
    touch.touch(Part::Liveness, segment.area_at(Area::Liveness), 1);
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
    let mut out = Vec::new();
    for group in groups {
        crate::check_interrupts();
        let words = geometry.groups[group as usize].words();
        out.clear();
        out.resize(words, 0);
        if !fold.eval(node, group, &mut out, 0)? {
            continue;
        }
        if let Some(dead) = live.groups[group as usize].as_deref() {
            for (o, d) in out.iter_mut().zip(dead) {
                *o &= !d;
            }
        }
        if out.iter().any(|w| *w != 0) {
            emit(group as usize, &out);
        }
    }
    Ok(())
}

/// The score of the document at `tid` by the summed `scorers` (in their
/// order, as the walks sum), or `None` when the segment does not hold it
/// live or it holds no scoring term: a row looked up alone, as `score()`
/// projects one.
pub fn score_at(
    segment: &Segment<'_>,
    scorers: &[(String, TermScorer)],
    tid: Tid,
) -> Result<Option<f32>> {
    let mut sets = Vec::with_capacity(scorers.len());
    for (name, _) in scorers {
        sets.push(TermSet::open(segment, name, &mut NoTouch)?);
    }
    score_at_in(segment, scorers, &mut sets, tid)
}

/// [`score_at`] over the scoring terms opened already, `sets[i]` the term
/// of `scorers[i]`: a caller scoring row after row keeps them, so a row in
/// the group of the last reads no container again. Their cursors are moved
/// back when a row comes before the last.
pub fn score_at_in<'a>(
    segment: &Segment<'a>,
    scorers: &[(String, TermScorer)],
    sets: &mut [Option<TermSet<'a>>],
    tid: Tid,
) -> Result<Option<f32>> {
    let geometry = &segment.docs.geometry;
    let Some(slot) = geometry.slot_of(tid) else {
        return Ok(None);
    };
    let Some(rank) = segment.docs.rank(slot) else {
        return Ok(None);
    };
    if segment.liveness.is_dead(geometry, slot) {
        return Ok(None);
    }
    let mut total = None::<f32>;
    let mut length = None;
    for ((_, scorer), set) in scorers.iter().zip(sets.iter_mut()) {
        let Some(set) = set.as_mut() else {
            continue;
        };
        // Sought past this row on an earlier one.
        if slot < set.sought() {
            set.rewind();
        }
        if set.seek(geometry, slot, &mut NoTouch) != Some(slot) {
            continue;
        }
        let footer = segment.footer_memo(set.at, &set.postings, set.max_bucket)?;
        let bucket = footer.bucket(set.postings.tf, set.index())?;
        let length = match length {
            Some(length) => length,
            None => *length.insert(segment.lengths.get(rank)?),
        };
        let score = scorer.score_bucket(
            segment::tf_bucket::TfBucket::new(bucket).ok_or(Error::InvalidTfBucket)?,
            length,
        );
        total = Some(total.unwrap_or(0.0) + score);
    }
    Ok(total)
}

/// The documents set in `words`, group `group`'s matches, in ctid order.
pub fn tids_in(geometry: &Geometry, group: usize, words: &[u64], mut visit: impl FnMut(Tid)) {
    for (w, word) in words.iter().enumerate() {
        let mut word = *word;
        while word != 0 {
            visit(geometry.tid_in(group, (w * 64) as u32 + word.trailing_zeros()));
            word &= word - 1;
        }
    }
}

/// [`count_terms`] trusting only the slots on all-visible heap pages, as an
/// index-only count must: per candidate group, `visible` fills a mask of
/// the group's slots on all-visible pages (zeroed when called) and returns
/// whether it covers every document of the group; members on other slots
/// are not counted but handed to `hidden` (the group and its words of
/// hidden members), for the caller to check against the heap.
pub fn count_terms_visible<'a>(
    segment: &Segment<'a>,
    node: &Node,
    terms: &mut [Option<TermSet<'a>>],
    touch: &mut impl Touch,
    visible: &mut dyn FnMut(u32, &mut [u64]) -> bool,
    hidden: &mut dyn FnMut(u32, &[u64]),
) -> Result<u64> {
    let live = &segment.liveness;
    touch.touch(Part::Liveness, segment.area_at(Area::Liveness), 1);
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
    let mut mask = Vec::new();
    for group in groups {
        let words = geometry.groups[group as usize].words();
        mask.clear();
        mask.resize(words, 0);
        let whole = visible(group, &mut mask);
        let dead = live.groups[group as usize].as_deref();
        if whole
            && dead.is_none()
            && let Some(n) = fold.shortcut(node, group)
        {
            total += n;
            continue;
        }
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
        if whole {
            total += kernels::popcount(&out);
            continue;
        }
        let mut any = false;
        for (o, m) in out.iter_mut().zip(&mask) {
            total += u64::from((*o & m).count_ones());
            *o &= !m;
            any |= *o != 0;
        }
        if any {
            hidden(group, &out);
        }
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
                        // An unreadable container takes the general path,
                        // which reports it.
                        Some(kernels::and_count_two(xa.all().ok()?, xb.all().ok()?))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

// ---- Ranked top-k ----

/// A ranked answer and what the walk did.
#[derive(Debug, Default)]
pub struct RankedAnswer {
    pub rows: Vec<(f32, Tid)>,
    /// Candidates scored exactly.
    pub scored: u64,
    pub windows: u64,
    pub windows_pruned: u64,
    /// Candidates the walk examined (bounded before anything was read for
    /// them), and those whose positions were checked.
    pub candidates: u64,
    pub position_checks: u64,
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
/// is led by the rarest of them; any other is block-max MaxScore over its
/// scoring terms, a group at a time (see [`rank`]). Candidates come in ctid
/// order, so one that only ties the threshold ranks after the `k`-th row.
pub fn top_k(
    segment: &Segment<'_>,
    node: &Node,
    names: &[String],
    scorers: &[(String, TermScorer)],
    k: usize,
    touch: &mut impl Touch,
) -> Result<RankedAnswer> {
    rank::top_k(segment, node, names, scorers, k, touch)
}

/// [`top_k`] into rows shared with walks over other sources: the walk
/// keeps a row only if it ranks above `top`'s bar and `visibility` passes
/// it (asked once per row so kept, in no particular order), and prunes
/// against the bar as it rises, from rows of any source. Dead documents
/// (the segment's liveness) are never candidates. Matches holding no
/// scoring term are not walked: a caller wanting them fills them in, as
/// [`top_k`] does. Returns the walk's counters.
pub fn top_k_into(
    segment: &Segment<'_>,
    node: &Node,
    names: &[String],
    scorers: &[(String, TermScorer)],
    top: &mut crate::walk::TopRows,
    visibility: &mut dyn crate::walk::Visibility,
    touch: &mut impl Touch,
) -> Result<RankedAnswer> {
    let mut answer = RankedAnswer::default();
    rank::walk_into(
        segment,
        node,
        names,
        scorers,
        top,
        visibility,
        touch,
        &mut answer,
    )?;
    Ok(answer)
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
        set.rewind();
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
        if !segment.liveness.is_dead(geometry, slot)
            && matches(segment, node, terms, slot, &mut positions, touch)?
        {
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
    use segment::tf_bucket::TfBucket;
    use segment::tinshape::blob::LazyBlob;
    use segment::tinshape::postings::Options;
    use segment::tinshape::segment::Builder;

    /// `parsed`'s blob assembled over `lazy`, which loads what is read, as
    /// the extension assembles a segment.
    fn assembled<'a>(lazy: &'a LazyBlob, parsed: &Segment<'_>) -> Segment<'a> {
        let term_map = lazy
            .bytes()
            .get(parsed.bounds[1], parsed.bounds[2])
            .unwrap();
        let prefix = segment::dictionary::DictionaryIndex::prefix_len(term_map).unwrap();
        let index = segment::dictionary::DictionaryIndex::parse(&term_map[..prefix]).unwrap();
        Segment::assemble(
            lazy.bytes(),
            index,
            parsed.docs.clone(),
            parsed.liveness.clone(),
        )
        .unwrap()
    }

    /// `parsed`'s blob `raw` assembled over `pinned`, which reads in place
    /// within spans, as the extension assembles one: within a span, so
    /// nothing is copied into the blob, and over a term map index of its
    /// own.
    fn assembled_in_place<'a>(
        pinned: &'a LazyBlob,
        parsed: &Segment<'_>,
        raw: &[u8],
    ) -> Segment<'a> {
        let term_map = &raw[parsed.bounds[1]..parsed.bounds[2]];
        let prefix = segment::dictionary::DictionaryIndex::prefix_len(term_map).unwrap();
        let index: &'static [u8] = Box::leak(term_map[..prefix].to_vec().into_boxed_slice());
        let index = segment::dictionary::DictionaryIndex::parse(index).unwrap();
        pinned.open_span();
        let segment = Segment::assemble(
            pinned.bytes(),
            index,
            parsed.docs.clone(),
            parsed.liveness.clone(),
        )
        .unwrap();
        // SAFETY: the segment keeps no slice of what assembling read.
        unsafe { pinned.close_span() };
        assert_eq!(pinned.loaded(), 0);
        segment
    }

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
            // Term `t` of a document holding it `tf` times is at positions
            // `0..tf` (see `build`).
            Node::Span { slots, query } => {
                let positions: Vec<Vec<u32>> = slots
                    .iter()
                    .map(|t| holds(members, *t, rank).map_or_else(Vec::new, |tf| (0..tf).collect()))
                    .collect();
                !positions.iter().any(Vec::is_empty)
                    && SpanSolver::new(query)
                        .unwrap()
                        .intervals(&positions)
                        .next()
                        .is_some()
            }
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
            Span {
                slots: vec![0, 1],
                query: SpanQuery::phrase([0, 1]),
            },
            Span {
                slots: vec![2, 0, 2],
                query: SpanQuery::phrase([0, 1, 2]),
            },
            And(vec![
                Span {
                    slots: vec![1, 3],
                    query: SpanQuery::phrase([0, 1]),
                },
                Term(0),
            ]),
            // Two spans reading one word: each from the group's start.
            Or(vec![
                Span {
                    slots: vec![0, 1],
                    query: SpanQuery::phrase([0, 1]),
                },
                Span {
                    slots: vec![0, 2],
                    query: SpanQuery::phrase([0, 1]),
                },
            ]),
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
            page_len in prop::sample::select(vec![37usize, 509, 8160]),
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
            let options = Options { block_size, grid_density, grid_min_postings: 0, inline_lengths_min_documents: 0, ..Options::default() };
            let blob = build(&docs, &members, options);
            let segment = Segment::parse(&blob).unwrap();
            let lazy_blob = LazyBlob::new(Box::new(blob.clone()));
            let lazy = assembled(&lazy_blob, &segment);
            // Read in place from pages pinned per span, poisoned once it
            // closes: the records the segment keeps past a span must not
            // borrow its pages.
            let pinned_blob = LazyBlob::new(Box::new(
                segment::tinshape::blob::PinningSource::new(blob.clone(), page_len),
            ));
            let in_place = assembled_in_place(&pinned_blob, &segment, &blob);
            let in_span = |read: &mut dyn FnMut()| {
                pinned_blob.open_span();
                read();
                in_place.forget_borrowed();
                // SAFETY: `read` returned owned results only.
                unsafe { pinned_blob.close_span() };
                assert_eq!(pinned_blob.loaded(), 0, "a span copies nothing into the blob");
            };
            let names: Vec<String> = (0..4).map(|t| format!("t{t}")).collect();
            let lengths: Vec<u32> = (0..docs.len()).map(|i| (i as u32 * 37) % 90 + 5).collect();
            for node in shapes() {
                let want = (0..docs.len()).filter(|r| eval(&node, &members, *r)).count() as u64;
                let got = count(&segment, &node, &names, &mut NoTouch).unwrap();
                prop_assert_eq!(got, want, "count of {:?}", node);
                prop_assert_eq!(count(&lazy, &node, &names, &mut NoTouch).unwrap(), want, "lazy count of {:?}", node);
                for round in 0..2 {
                    let mut got = None;
                    let mut listed = Vec::new();
                    in_span(&mut || {
                        got = Some(count(&in_place, &node, &names, &mut NoTouch).unwrap());
                        let mut terms = open_terms(&in_place, &names, &mut NoTouch).unwrap();
                        for_each_match(&in_place, &node, &mut terms, &mut NoTouch, &mut |group, words| {
                            tids_in(&in_place.docs.geometry, group, words, |tid| listed.push(tid));
                        })
                        .unwrap();
                    });
                    prop_assert_eq!(got, Some(want), "in-place count of {:?}, round {}", node, round);
                    let expected: Vec<Tid> = (0..docs.len()).filter(|r| eval(&node, &members, *r)).map(|r| docs[r]).collect();
                    prop_assert_eq!(listed, expected, "in-place matches of {:?}, round {}", node, round);
                }
                let mut terms = open_terms(&segment, &names, &mut NoTouch).unwrap();
                let mut listed = Vec::new();
                for_each_match(&segment, &node, &mut terms, &mut NoTouch, &mut |group, words| {
                    tids_in(&segment.docs.geometry, group, words, |tid| listed.push(tid));
                })
                .unwrap();
                let expected: Vec<Tid> = (0..docs.len()).filter(|r| eval(&node, &members, *r)).map(|r| docs[r]).collect();
                prop_assert_eq!(listed, expected, "matches of {:?}", node);
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
                for r in 0..docs.len() {
                    let mut total = None::<f32>;
                    for (name, scorer) in &scorers {
                        let t: usize = name[1..].parse().unwrap();
                        if let Some(tf) = holds(&members, t, r) {
                            total = Some(total.unwrap_or(0.0) + scorer.score_bucket(TfBucket::from_count(tf), lengths[r]));
                        }
                    }
                    let got = score_at(&segment, &scorers, docs[r]).unwrap();
                    prop_assert_eq!(got.map(f32::to_bits), total.map(f32::to_bits), "score of {:?}", docs[r]);
                }
                let absent = Tid { block: 5000, offset: 1 };
                prop_assert_eq!(score_at(&segment, &scorers, absent).unwrap(), None);
                // Kept term cursors, rows in descending then ascending order.
                let mut sets: Vec<Option<TermSet<'_>>> = scorers.iter().map(|(n, _)| TermSet::open(&segment, n, &mut NoTouch).unwrap()).collect();
                for r in (0..docs.len()).rev().chain(0..docs.len()) {
                    let kept = score_at_in(&segment, &scorers, &mut sets, docs[r]).unwrap();
                    let alone = score_at(&segment, &scorers, docs[r]).unwrap();
                    prop_assert_eq!(kept.map(f32::to_bits), alone.map(f32::to_bits));
                }
                want.sort_by(crate::walk::rank);
                want.truncate(k);
                let got = top_k(&segment, &node, &names, &scorers, k, &mut NoTouch).unwrap();
                let bits = |rows: &[(f32, Tid)]| rows.iter().map(|(s, t)| (s.to_bits(), *t)).collect::<Vec<_>>();
                prop_assert_eq!(bits(&got.rows), bits(&want), "top {} of {:?}", k, node);
                let lazily = top_k(&lazy, &node, &names, &scorers, k, &mut NoTouch).unwrap();
                prop_assert_eq!(bits(&lazily.rows), bits(&want), "lazy top {} of {:?}", k, node);
                // Nothing scoring (every term elided): the first k matches
                // in ctid order, each scoring zero.
                let first: Vec<(f32, Tid)> = (0..docs.len())
                    .filter(|r| eval(&node, &members, *r))
                    .map(|r| (0.0, docs[r]))
                    .take(k)
                    .collect();
                let unscored = top_k(&segment, &node, &names, &[], k, &mut NoTouch).unwrap();
                prop_assert_eq!(bits(&unscored.rows), bits(&first), "unscored top {} of {:?}", k, node);
                for round in 0..2 {
                    let mut rows = Vec::new();
                    let mut scores = Vec::new();
                    in_span(&mut || {
                        rows = top_k(&in_place, &node, &names, &scorers, k, &mut NoTouch).unwrap().rows;
                        scores = docs.iter().map(|tid| score_at(&in_place, &scorers, *tid).unwrap().map(f32::to_bits)).collect();
                    });
                    prop_assert_eq!(bits(&rows), bits(&want), "in-place top {} of {:?}, round {}", k, node, round);
                    let alone: Vec<Option<u32>> = docs.iter().map(|tid| score_at(&segment, &scorers, *tid).unwrap().map(f32::to_bits)).collect();
                    prop_assert_eq!(scores, alone, "in-place scores, round {}", round);
                }
            }
        }
    }

    /// A phrase of two words in every document, ranked over a segment that
    /// loads what it reads, reads a few of their positions' pages and a few
    /// of the DL sidecar's, not their areas: a backend's memory must not
    /// grow with a common word's positions (at 150M rows a phrase of two
    /// common words loaded over a gigabyte of positions per backend).
    #[test]
    fn a_ranked_phrase_loads_what_it_reads() {
        let docs: Vec<Tid> = (0..60_000u32)
            .map(|i| Tid {
                block: i / 20,
                offset: (i % 20) as u16 + 1,
            })
            .collect();
        let lengths: Vec<u32> = (0..docs.len()).map(|i| (i as u32 * 37) % 90 + 5).collect();
        let mut builder = Builder::new(docs.clone(), lengths, Options::default()).unwrap();
        let ranks: Vec<u32> = (0..docs.len() as u32).collect();
        for (name, at) in [("a", 0u32), ("b", 1)] {
            let mut payload = PayloadBuilder::default();
            let mut buckets = Vec::new();
            for r in 0..docs.len() as u32 {
                // Positions a document apart, so entries are several bytes.
                let positions: Vec<u32> = (0..(r % 7 + 1)).map(|p| p * 10 + at).collect();
                buckets.push(TfBucket::from_count(positions.len() as u32).value());
                payload.push(&positions).unwrap();
            }
            builder
                .add_term(name, &ranks, &buckets, &payload.finish())
                .unwrap();
        }
        let blob = builder.finish(&[]).0;
        let parsed = Segment::parse(&blob).unwrap();
        let names = vec!["a".to_owned(), "b".to_owned()];
        let node = Node::Span {
            slots: vec![0, 1],
            query: SpanQuery::phrase([0, 1]),
        };
        let scorers: Vec<(String, TermScorer)> = names
            .iter()
            .map(|n| {
                let scorer = TermScorer::from_statistics(
                    docs.len() as u64,
                    docs.len() as u64,
                    1.0,
                    Bm25Params::default(),
                    50.0,
                )
                .unwrap();
                (n.clone(), scorer)
            })
            .collect();
        let want = top_k(&parsed, &node, &names, &scorers, 10, &mut NoTouch).unwrap();
        let lazy = LazyBlob::new(Box::new(blob.clone()));
        let segment = assembled(&lazy, &parsed);
        let before = lazy.loaded();
        let got = top_k(&segment, &node, &names, &scorers, 10, &mut NoTouch).unwrap();
        assert_eq!(got.rows, want.rows);
        let read = lazy.loaded() - before;
        let positions = parsed.bounds[4] - parsed.bounds[3];
        let sidecar = parsed.bounds[6] - parsed.bounds[5];
        assert!(positions > 400_000, "{positions}");
        assert!(
            read < (positions + sidecar) / 4,
            "read {read} bytes of {positions} of positions and {sidecar} of lengths"
        );
    }

    #[test]
    fn only_spans_needing_every_word_lower() {
        let span = |span_query| Query::Span {
            term_slots: vec![
                SpanTermSlot::Term("a".into()),
                SpanTermSlot::Term("b".into()),
            ],
            span_query,
            position_filter: None,
            leaf_boosts: Default::default(),
        };
        let mut names = Vec::new();
        assert!(lower(&span(SpanQuery::phrase([0, 1])), &mut names).is_some());
        // `a NOT ENCLOSES b` matches documents without b: its words are not
        // an AND to fold.
        let not_containing = SpanQuery::NotContaining {
            big: Box::new(SpanQuery::Term(0)),
            little: Box::new(SpanQuery::Term(1)),
        };
        assert!(lower(&span(not_containing), &mut Vec::new()).is_none());
    }

    /// Rejects every fifth ctid, as a heap visibility check or a scan's
    /// other restriction would.
    struct EveryFifth(usize);

    impl crate::walk::Visibility for EveryFifth {
        fn visible(&mut self, tid: Tid) -> bool {
            self.0 += 1;
            !(tid.block + u32::from(tid.offset)).is_multiple_of(5)
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]
        /// Walks over two segments into shared rows, with dead documents, a
        /// visibility check and ties kept or not, keep what a brute force
        /// over the eligible matches keeps.
        #[test]
        fn shared_walks_keep_the_eligible_top_rows(
            doc_set in prop::collection::btree_set((0u32..900, 1u16..=30), 2..500),
            density in prop::collection::vec(1u32..100, 4),
            seed in any::<u64>(),
            k in 1usize..12,
            ties in any::<bool>(),
        ) {
            let docs: Vec<Tid> = doc_set.into_iter().map(|(block, offset)| Tid { block, offset }).collect();
            let mut state = seed | 1;
            let mut next = || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            };
            // Few distinct frequencies and lengths, so scores tie.
            let lengths: Vec<u32> = (0..docs.len()).map(|i| (i as u32 % 3) * 10 + 5).collect();
            let mut members: Vec<Vec<(usize, u32)>> = vec![Vec::new(); density.len()];
            for (t, d) in density.iter().enumerate() {
                for r in 0..docs.len() {
                    if next() % 100 < u64::from(*d) {
                        members[t].push((r, (next() % 2 + 1) as u32));
                    }
                }
            }
            let side: Vec<usize> = (0..docs.len()).map(|_| (next() % 2) as usize).collect();
            let dead: Vec<bool> = (0..docs.len()).map(|_| next() % 7 == 0).collect();
            let options = Options { block_size: 16, grid_min_postings: 0, ..Options::default() };
            let blobs: Vec<Vec<u8>> = (0..2)
                .map(|half| {
                    let local: Vec<usize> = (0..docs.len()).filter(|r| side[*r] == half).collect();
                    let tids: Vec<Tid> = local.iter().map(|r| docs[*r]).collect();
                    let mut builder = Builder::new(tids, local.iter().map(|r| lengths[*r]).collect(), options).unwrap();
                    for (t, m) in members.iter().enumerate() {
                        let held: Vec<(u32, u32)> = m
                            .iter()
                            .filter_map(|(r, tf)| local.iter().position(|l| l == r).map(|i| (i as u32, *tf)))
                            .collect();
                        if held.is_empty() {
                            continue;
                        }
                        let ranks: Vec<u32> = held.iter().map(|(i, _)| *i).collect();
                        let buckets: Vec<u8> = held.iter().map(|(_, tf)| TfBucket::from_count(*tf).value()).collect();
                        let mut payload = PayloadBuilder::default();
                        for (_, tf) in &held {
                            payload.push(&(0..*tf).collect::<Vec<u32>>()).unwrap();
                        }
                        builder.add_term(&format!("t{t}"), &ranks, &buckets, &payload.finish()).unwrap();
                    }
                    let gone: Vec<u32> = local
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| dead[**r])
                        .map(|(i, _)| i as u32)
                        .collect();
                    builder.finish(&gone).0
                })
                .collect();
            let segments: Vec<Segment<'_>> = blobs.iter().map(|b| Segment::parse(b).unwrap()).collect();
            let names: Vec<String> = (0..4).map(|t| format!("t{t}")).collect();
            for node in shapes() {
                if matches!(node, Node::Not(_)) {
                    continue;
                }
                let mut scorers = Vec::new();
                for t in leaf_terms(&node) {
                    if members[t].is_empty() || scorers.iter().any(|(n, _): &(String, TermScorer)| *n == names[t]) {
                        continue;
                    }
                    let scorer = TermScorer::from_statistics(docs.len() as u64, members[t].len() as u64, 1.0, Bm25Params::default(), 15.0).unwrap();
                    scorers.push((names[t].clone(), scorer));
                }
                let mut want = crate::walk::TopRows::new(k, ties);
                for r in 0..docs.len() {
                    if dead[r] || !eval(&node, &members, r) || (docs[r].block + u32::from(docs[r].offset)).is_multiple_of(5) {
                        continue;
                    }
                    let mut total = 0.0_f32;
                    for (name, scorer) in &scorers {
                        let t: usize = name[1..].parse().unwrap();
                        if let Some(tf) = holds(&members, t, r) {
                            total += scorer.score_bucket(TfBucket::from_count(tf), lengths[r]);
                        }
                    }
                    let row = crate::walk::Ranked(total, docs[r]);
                    if want.admits(&row) {
                        want.push(row);
                    }
                }
                // The live matches, segment by segment.
                let mut listed = Vec::new();
                for segment in &segments {
                    let mut terms = open_terms(segment, &names, &mut NoTouch).unwrap();
                    for_each_match(segment, &node, &mut terms, &mut NoTouch, &mut |group, words| {
                        tids_in(&segment.docs.geometry, group, words, |tid| listed.push(tid));
                    })
                    .unwrap();
                }
                listed.sort_unstable();
                let live: Vec<Tid> = (0..docs.len()).filter(|r| !dead[*r] && eval(&node, &members, *r)).map(|r| docs[r]).collect();
                prop_assert_eq!(listed, live, "live matches of {:?}", node);
                let mut top = crate::walk::TopRows::new(k, ties);
                let mut visibility = EveryFifth(0);
                for segment in segments.iter().rev() {
                    top_k_into(segment, &node, &names, &scorers, &mut top, &mut visibility, &mut NoTouch).unwrap();
                }
                let bits = |rows: Vec<(f32, Tid)>| rows.into_iter().map(|(s, t)| (s.to_bits(), t)).collect::<Vec<_>>();
                prop_assert_eq!(bits(top.into_rows()), bits(want.into_rows()), "top {} ties {} of {:?}", k, ties, node);
                // Nothing scoring (every term elided) in a led walk: the
                // first k live matches in ctid order, though the segment
                // walked first fills the rows with later ones.
                if required_terms(&node).is_empty() {
                    continue;
                }
                let mut want = crate::walk::TopRows::new(k, ties);
                for r in 0..docs.len() {
                    let row = crate::walk::Ranked(0.0, docs[r]);
                    if !dead[r] && eval(&node, &members, r) && !(docs[r].block + u32::from(docs[r].offset)).is_multiple_of(5) && want.admits(&row) {
                        want.push(row);
                    }
                }
                let mut top = crate::walk::TopRows::new(k, ties);
                let mut visibility = EveryFifth(0);
                for segment in segments.iter().rev() {
                    top_k_into(segment, &node, &names, &[], &mut top, &mut visibility, &mut NoTouch).unwrap();
                }
                prop_assert_eq!(bits(top.into_rows()), bits(want.into_rows()), "unscored top {} ties {} of {:?}", k, ties, node);
            }
        }
    }
}
