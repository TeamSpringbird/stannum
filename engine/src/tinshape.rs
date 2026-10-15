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
use segment::tinshape::docs::DocSet;
use segment::tinshape::docs::Geometry;
use segment::tinshape::ef::EfCursor;
use segment::tinshape::positions::Positions;
use segment::tinshape::postings::{Directory, Form, GroupEntry, KIND_GRID, Postings};
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

pub mod kernels;

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
fn container_bytes(bytes: Bytes<'_>) -> &'_ [u8] {
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
    /// A grouped record's directory, shared with the parsed record and
    /// decoded as far as asked for (each group's [`G`] is made when it is
    /// asked for), the segment's groups it is decoded against, and where
    /// its containers start in the blob.
    dir: Option<std::rc::Rc<Directory>>,
    docs: std::rc::Rc<DocSet>,
    containers: usize,
    /// Groups of a single or sparse term, over [`Self::locals`].
    groups: Vec<G<'a>>,
    /// Local slots of a single or sparse term, all groups in a row.
    locals: Vec<u32>,
    // Cursor state, for ranked walks and phrase checks.
    /// The target of the last seek: the cursor may move forward from it.
    sought: u32,
    gpos: usize,
    loaded: Loaded<'a>,
    current: Option<u32>,
    index: u32,
    sparse: Option<EfCursor<'a>>,
    prefix: Vec<u32>,
    list: Vec<u32>,
    pos: usize,
}

#[derive(Default)]
enum Loaded<'a> {
    #[default]
    None,
    /// A grid group, its bytes read once (a grid across pages is stitched
    /// once, not at every seek), with `prefix` its members before each
    /// word.
    Grid(&'a [u8]),
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
        // A grouped term's directory, shared with its record.
        let mut dir: Option<std::rc::Rc<Directory>> = None;
        let mut containers = 0;
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
            Form::Grouped(directory) => {
                // The header and the group directory: per-term metadata,
                // which TIN would count as the postings footer.
                touch.touch(Part::Footer, record_at, postings.footer_at);
                touch.touch(
                    Part::Footer,
                    record_at + postings.payload_at,
                    postings.containers_at,
                );
                containers = record_at + postings.payload_at + postings.containers_at;
                dir = Some(directory.clone());
            }
        }
        Ok(Some(Self {
            df: term.entry.df,
            max_bucket: term.entry.max_tf_bucket,
            positions: segment.positions(&term.entry)?,
            postings,
            at: record_at,
            dir,
            docs: segment.docs.clone(),
            containers,
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
        kernels::ef_decode(*list, &mut slots);
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

    /// The number of groups the term holds (a sparse term's once
    /// [`Self::ensure_groups`] has listed them).
    #[inline]
    fn group_count(&self) -> usize {
        match &self.dir {
            Some(dir) => dir.len(),
            None => self.groups.len(),
        }
    }

    /// A grouped term's directory entry `i` (decoded if it was not yet).
    #[inline]
    fn entry(dir: &Directory, docs: &DocSet, i: usize) -> GroupEntry {
        *dir.get(i, &docs.geometry.groups)
            .unwrap_or_else(|e| crate::corrupt(format!("Stannum postings: {e}")))
    }

    /// The geometry index of the term's `i`th group.
    #[inline]
    fn group_index(&self, i: usize) -> u32 {
        match &self.dir {
            Some(dir) => Self::entry(dir, &self.docs, i).index,
            None => self.groups[i].index,
        }
    }

    /// The term's `i`th group.
    #[inline]
    fn group_at(&self, i: usize) -> G<'a> {
        let Some(dir) = &self.dir else {
            return self.groups[i];
        };
        self.group_of_entry(Self::entry(dir, &self.docs, i))
    }

    /// A grouped term's group of directory entry `entry`.
    #[inline]
    fn group_of_entry(&self, entry: GroupEntry) -> G<'a> {
        G {
            index: entry.index,
            count: entry.count,
            src: Src::Container {
                entry,
                bytes: self.postings.container(&entry),
                at: self.containers + entry.at as usize,
            },
        }
    }

    /// Moves `hint` forward to the term's first group whose index is
    /// `group` or more (to the group count if none is), and gives that
    /// group's index. A grouped term's directory is sought by its skip
    /// points past the hint's run ([`Directory::seek`]): the entries of the
    /// runs it passes over are not decoded.
    #[inline]
    fn seek_index(&self, group: u32, hint: &mut usize) -> Option<u32> {
        let Some(dir) = &self.dir else {
            while *hint < self.groups.len() && self.groups[*hint].index < group {
                *hint += 1;
            }
            return self.groups.get(*hint).map(|g| g.index);
        };
        *hint = dir
            .seek(group, *hint, &self.docs.geometry.groups)
            .unwrap_or_else(|e| crate::corrupt(format!("Stannum postings: {e}")));
        (*hint < dir.len()).then(|| Self::entry(dir, &self.docs, *hint).index)
    }

    /// The term's group `group`, if it holds it, looked for from `hint`
    /// forward (the hint is left at the first group at or after it).
    #[inline]
    fn find(&self, group: u32, hint: &mut usize) -> Option<G<'a>> {
        if self.seek_index(group, hint)? != group {
            return None;
        }
        Some(match &self.dir {
            // Decoded: the seek reached it.
            Some(dir) => self.group_of_entry(Self::entry(dir, &self.docs, *hint)),
            None => self.groups[*hint],
        })
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
            if self.gpos >= self.group_count() {
                self.current = None;
                return None;
            }
            let g = self.group_at(self.gpos);
            let group = &geometry.groups[g.index as usize];
            let base = group.slot_base;
            if base + group.slots() <= target {
                // Skip the groups wholly before the target: a gallop from
                // the group at hand, then a binary search, so that a lazy
                // directory decodes about as far as the target.
                let count = self.group_count();
                let ends_before = |i: usize| {
                    let group = &geometry.groups[self.group_index(i) as usize];
                    group.slot_base + group.slots() <= target
                };
                let (mut lo, mut step) = (self.gpos, 1);
                let mut hi = loop {
                    let at = lo + step;
                    if at >= count {
                        break count;
                    }
                    if !ends_before(at) {
                        break at;
                    }
                    lo = at + 1;
                    step *= 2;
                };
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    let group = &geometry.groups[self.group_index(mid) as usize];
                    if group.slot_base + group.slots() <= target {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                self.gpos = lo;
                self.loaded = Loaded::None;
                continue;
            }
            let local_target = target.saturating_sub(base);
            if matches!(self.loaded, Loaded::None) {
                self.load(&g, group.slots(), geometry.starts(g.index as usize), touch);
            }
            let first = self.first_of(&g);
            let found = match &self.loaded {
                Loaded::Grid(bytes) => {
                    let bytes = *bytes;
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
                Loaded::List => {
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
                Loaded::None => unreachable!("a loaded group"),
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

    fn load(&mut self, g: &G<'a>, slots: u32, starts: &[u32], touch: &mut impl Touch) {
        self.pos = 0;
        match g.src {
            Src::Container { entry, bytes, at } => {
                touch.touch(Part::Payload, at, bytes.len());
                let bytes = container_bytes(bytes);
                if entry.kind == KIND_GRID {
                    self.prefix.clear();
                    kernels::prefix_counts(bytes, &mut self.prefix);
                    self.loaded = Loaded::Grid(bytes);
                } else {
                    let mut list = std::mem::take(&mut self.list);
                    list.clear();
                    kernels::container_locals(&entry, bytes, slots, starts, &mut list)
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
        Node::Term(t) => terms[*t].as_ref().map_or_else(Vec::new, |t| {
            (0..t.group_count()).map(|i| t.group_index(i)).collect()
        }),
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
    /// A term's members set in a cleared row, to be ORed into a filled one.
    row: Vec<u64>,
    /// Per term, a cursor over its positions, opened when a span first
    /// reads them: documents are checked in slot order, so their entries
    /// are read forward from the last one rather than located afresh.
    cursors: Vec<Option<rank::PosCursor<'a>>>,
}

impl<'a, T: Touch> Fold<'_, 'a, T> {
    /// Writes the members of `node` in group `group` into `out` (zeroed by
    /// the caller); false when there are none.
    fn eval(&mut self, node: &Node, group: u32, out: &mut [u64], depth: usize) -> Result<bool> {
        match node {
            Node::Term(t) => self.term_into(*t, group, out, true),
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
                            kernels::and_words(out, &other);
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
                            kernels::andnot_words(out, &other);
                        }
                        self.scratch.give(depth, other);
                    }
                }
                Ok(kernels::any_set(out))
            }
            Node::Or(children) => {
                let mut any = false;
                for child in children {
                    // A term's members are set in place.
                    if let Node::Term(t) = child {
                        // `out` is clear until a child sets a member.
                        any |= self.term_or(*t, group, out, !any)?;
                        continue;
                    }
                    let mut other = self.scratch.level(depth, out.len());
                    if self.eval(child, group, &mut other, depth + 1)? {
                        kernels::or_words(out, &other);
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
                    kernels::andnot_words(out, &other);
                }
                self.scratch.give(depth, other);
                Ok(kernels::any_set(out))
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
                Ok(kernels::any_set(out))
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
        if *hint > 0 && *hint <= set.group_count() && set.group_index(*hint - 1) >= group {
            *hint = 0;
        }
        set.find(group, hint)
    }

    fn touch_src(&mut self, src: &Src<'_>) {
        if let Src::Container { bytes, at, .. } = src {
            self.touch.touch(Part::Payload, *at, bytes.len());
        }
    }

    /// Sets the members of term `t` in group `group` in `out` (`clear`:
    /// which holds none yet).
    fn term_or(&mut self, t: usize, group: u32, out: &mut [u64], clear: bool) -> Result<bool> {
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
        self.term_into(t, group, out, clear)
    }

    /// The members of term `t`'s group `g`, appended to `list` in order.
    fn decode(&mut self, t: usize, g: &G<'a>, list: &mut Vec<u32>) -> Result<()> {
        self.touch_src(&g.src);
        let segment = self.segment;
        let geometry = &segment.docs.geometry;
        let starts = geometry.starts(g.index as usize);
        let slots = geometry.groups[g.index as usize].slots();
        match g.src {
            Src::Container { entry, bytes, .. } => {
                kernels::container_locals(&entry, bytes.all()?, slots, starts, list)?;
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
                        bits::intersect_sorted(&mut list, &other);
                    }
                }
            }
            // `out` is clear (see `eval`).
            kernels::set_ascending(&list, out);
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
                    kernels::andnot_bytes(out, bytes.all()?);
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
        Ok(kernels::any_set(out))
    }

    /// Sets the members of term `t` in group `group` in `out`; `clear`:
    /// `out` holds none yet (else they are ORed in).
    ///
    /// A clear row takes a container's members as a disjunction's row does
    /// ([`kernels::container_row`]); a filled one ORs in a cleared row of
    /// them where they are at least one in 128 slots, else each member's
    /// word is read and stored (members a word or more apart seldom wait on
    /// the store before).
    fn term_into(&mut self, t: usize, group: u32, out: &mut [u64], clear: bool) -> Result<bool> {
        let Some(g) = self.group_of(t, group) else {
            return Ok(false);
        };
        self.touch_src(&g.src);
        let segment = self.segment;
        let geometry = &segment.docs.geometry;
        let starts = geometry.starts(group as usize);
        let slots = geometry.groups[group as usize].slots();
        let set = self.terms[t].as_ref().expect("found above");
        match g.src {
            Src::Container { entry, bytes, .. } => {
                let bytes = bytes.all()?;
                if entry.kind == KIND_GRID {
                    kernels::load(out, bytes);
                } else if clear {
                    kernels::container_row(&entry, bytes, starts, out)?;
                } else if 2 * entry.count as usize >= out.len() {
                    let mut row = std::mem::take(&mut self.row);
                    row.clear();
                    row.resize(out.len(), 0);
                    kernels::container_row(&entry, bytes, starts, &mut row)?;
                    kernels::or_words(out, &row);
                    self.row = row;
                } else {
                    let mut list = std::mem::take(&mut self.other);
                    list.clear();
                    kernels::container_locals(&entry, bytes, slots, starts, &mut list)?;
                    for l in &list {
                        out[*l as usize / 64] |= 1 << (l % 64);
                    }
                    self.other = list;
                }
            }
            Src::Locals { from, to } => {
                let locals = &set.locals[from as usize..to as usize];
                if clear {
                    kernels::set_ascending(locals, out);
                } else {
                    for l in locals {
                        out[*l as usize / 64] |= 1 << (l % 64);
                    }
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
                    if self.cursors.len() <= *t {
                        self.cursors.resize_with(*t + 1, || None);
                    }
                    let cursor = match &mut self.cursors[*t] {
                        Some(cursor) => cursor,
                        empty => empty.insert(rank::PosCursor::new(set.positions)?),
                    };
                    cursor.read(index, &mut positions[i], self.touch)?;
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
    // Sized once: collected through a `Result`, the vector would grow (and
    // copy its terms) as it filled.
    let mut terms = Vec::with_capacity(names.len());
    for name in names {
        terms.push(TermSet::open(segment, name, touch)?);
    }
    Ok(terms)
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
        row: Vec::new(),
        cursors: Vec::new(),
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
            kernels::andnot_words(&mut out, dead);
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
    let groups = match_groups(segment, node, terms, touch);
    fold_groups(segment, node, terms, touch, &groups, &mut |group, words| {
        emit(group, words);
        true
    })?;
    Ok(())
}

/// The groups of `segment` that may hold matches of `node`, ascending: what
/// [`fold_groups`] is handed to fold a few at a time.
pub fn match_groups<'a>(
    segment: &Segment<'a>,
    node: &Node,
    terms: &mut [Option<TermSet<'a>>],
    touch: &mut impl Touch,
) -> Vec<u32> {
    touch.touch(Part::Liveness, segment.area_at(Area::Liveness), 1);
    let geometry = &segment.docs.geometry;
    for set in terms.iter_mut().flatten() {
        set.ensure_groups(geometry, touch);
    }
    candidate_groups(node, terms, geometry.groups.len())
}

/// [`for_each_match`] over `groups` (ascending, from [`match_groups`]) only,
/// stopping after the first group `emit` returns `false` for: the number of
/// groups folded. A caller wanting the first matches in ctid order reads
/// the containers of the groups it takes and no others.
pub fn fold_groups<'a>(
    segment: &Segment<'a>,
    node: &Node,
    terms: &mut [Option<TermSet<'a>>],
    touch: &mut impl Touch,
    groups: &[u32],
    emit: &mut dyn FnMut(usize, &[u64]) -> bool,
) -> Result<usize> {
    let live = &segment.liveness;
    let geometry = &segment.docs.geometry;
    for set in terms.iter_mut().flatten() {
        set.ensure_groups(geometry, touch);
    }
    let hints = vec![0usize; terms.len()];
    let mut fold = Fold {
        segment,
        terms,
        hints,
        scratch: Scratch::default(),
        touch,
        list: Vec::new(),
        other: Vec::new(),
        row: Vec::new(),
        cursors: Vec::new(),
    };
    let mut out = Vec::new();
    for (folded, &group) in groups.iter().enumerate() {
        crate::check_interrupts();
        let words = geometry.groups[group as usize].words();
        out.clear();
        out.resize(words, 0);
        if !fold.eval(node, group, &mut out, 0)? {
            continue;
        }
        if let Some(dead) = live.groups[group as usize].as_deref() {
            kernels::andnot_words(&mut out, dead);
        }
        if kernels::any_set(&out) && !emit(group as usize, &out) {
            return Ok(folded + 1);
        }
    }
    Ok(groups.len())
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
pub fn tids_in(geometry: &Geometry, group: usize, words: &[u64], visit: impl FnMut(Tid)) {
    geometry.each_tid(group, words, visit);
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
        row: Vec::new(),
        cursors: Vec::new(),
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
            kernels::andnot_words(&mut out, dead);
        }
        if whole {
            total += kernels::popcount(&out);
            continue;
        }
        let (visible, any) = kernels::and_count_clear(&mut out, &mask);
        total += visible;
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

/// Terms every match holds, ascending.
fn required_terms(node: &Node) -> Vec<usize> {
    /// Appends the terms every match of `node` holds: those of each child
    /// of an AND, in one list rather than one per child.
    fn add(node: &Node, out: &mut Vec<usize>) {
        match node {
            Node::Term(t) => out.push(*t),
            Node::And(children) => children.iter().for_each(|c| add(c, out)),
            Node::Span { slots, .. } => out.extend_from_slice(slots),
            _ => {}
        }
    }
    let mut out = Vec::new();
    add(node, &mut out);
    out.sort_unstable();
    out.dedup();
    out
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
        // Walks release every page they are past, which the source then
        // poisons: a walk reading a page it released reads garbage.
        pinned.set_keep(0);
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
    /// Fewer than `max` ctids on blocks below `blocks`, ascending: random
    /// line pointers up to `offsets`, or heap pages each holding rows
    /// `1..=n` for its own `n` up to `offsets`, some with a hole, as a heap
    /// only inserted into is laid out (each page's width its row count).
    fn doc_tids(blocks: u32, offsets: u16, max: usize) -> impl Strategy<Value = Vec<Tid>> {
        let random =
            prop::collection::btree_set((0..blocks, 1..=offsets), 1..max).prop_map(|set| {
                set.into_iter()
                    .map(|(block, offset)| Tid { block, offset })
                    .collect()
            });
        let heap = prop::collection::btree_map(0..blocks, (1..=offsets, any::<u8>()), 1..max / 4)
            .prop_map(move |pages| {
                let mut tids: Vec<Tid> = Vec::new();
                for (block, (rows, shape)) in pages {
                    let hole = (shape % 4 == 0 && rows > 1).then_some(u16::from(shape) % rows + 1);
                    tids.extend(
                        (1..=rows)
                            .filter(|o| Some(*o) != hole)
                            .map(|offset| Tid { block, offset }),
                    );
                }
                tids.truncate(max - 1);
                tids
            });
        prop_oneof![random, heap]
    }

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
        #[test]
        fn intersects_sorted_lists(
            a in prop::collection::btree_set(0u32..2_000, 0..80),
            b in prop::collection::btree_set(0u32..2_000, 0..300),
        ) {
            let mut list: Vec<u32> = a.iter().copied().collect();
            let other: Vec<u32> = b.iter().copied().collect();
            bits::intersect_sorted(&mut list, &other);
            let want: Vec<u32> = a.intersection(&b).copied().collect();
            prop_assert_eq!(list, want);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]
        #[test]
        fn counts_and_top_k_match_brute_force(
            docs in doc_tids(1500, 40, 700),
            density in prop::collection::vec(1u32..100, 4),
            seed in any::<u64>(),
            grid_density in prop::sample::select(vec![0u32, 4, 64]),
            block_size in prop::sample::select(vec![2u32, 16, 128]),
            k in 1usize..12,
            page_len in prop::sample::select(vec![37usize, 509, 8160]),
            array_max in prop::sample::select(vec![0u32, 64, u32::MAX]),
        ) {
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
            let options = Options { block_size, grid_density, grid_min_postings: 0, array_max, inline_lengths_min_documents: 0, ..Options::default() };
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

    /// Ranked walks over a segment read in place release the pages of the
    /// groups they are past rather than holding every page until the span
    /// closes, and never read a page they released: the source poisons it,
    /// so a walk that did would rank garbage. Grids, Elias-Fano and listed
    /// containers, a sparse term (its list kept pinned, the walk borrows
    /// it) and phrases, over some 40 groups of small pages.
    #[test]
    fn ranked_walks_release_the_pages_of_groups_they_are_past() {
        let docs: Vec<Tid> = (0..6_000u32)
            .map(|i| Tid {
                block: i / 3 * 5,
                offset: (i % 3) as u16 + 1,
            })
            .collect();
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let density = [70u64, 35, 12, 1];
        let members: Vec<Vec<(usize, u32)>> = density
            .iter()
            .map(|d| {
                let mut held = Vec::new();
                for r in 0..docs.len() {
                    if next() % 100 < *d {
                        held.push((r, (next() % 5 + 1) as u32));
                    }
                }
                held
            })
            .collect();
        let options = Options {
            block_size: 16,
            grid_density: 32,
            grid_min_postings: 0,
            inline_lengths_min_documents: 0,
            ..Options::default()
        };
        let blob = build(&docs, &members, options);
        let parsed = Segment::parse(&blob).unwrap();
        assert!(parsed.docs.geometry.groups.len() >= 40);
        let source = segment::tinshape::blob::PinningSource::new(blob.clone(), 211);
        let pinned = LazyBlob::new(Box::new(source.clone()));
        let in_place = assembled_in_place(&pinned, &parsed, &blob);
        let names: Vec<String> = (0..4).map(|t| format!("t{t}")).collect();
        let scorers: Vec<(String, TermScorer)> = (0..4)
            .map(|t| {
                let scorer = TermScorer::from_statistics(
                    docs.len() as u64,
                    members[t].len() as u64,
                    1.0,
                    Bm25Params::default(),
                    50.0,
                )
                .unwrap();
                (names[t].clone(), scorer)
            })
            .collect();
        use Node::*;
        let nodes = [
            And(vec![Term(0), Term(1)]),
            And(vec![Term(0), Term(1), Term(2)]),
            And(vec![Term(3), Term(0)]),
            Or(vec![Term(0), Term(1), Term(2), Term(3)]),
            Or(vec![Term(1), Term(2)]),
            Span {
                slots: vec![0, 1],
                query: SpanQuery::phrase([0, 1]),
            },
        ];
        let state = &source.0;
        for node in &nodes {
            let used: Vec<(String, TermScorer)> = scorers
                .iter()
                .filter(|(n, _)| leaf_terms(node).iter().any(|t| names[*t] == *n))
                .cloned()
                .collect();
            for k in [1, 10, 200] {
                let want = top_k(&parsed, node, &names, &used, k, &mut NoTouch).unwrap();
                let (pins, early) = (state.pins.get(), state.early.get());
                state.peak.set(0);
                pinned.open_span();
                let got = top_k(&in_place, node, &names, &used, k, &mut NoTouch);
                in_place.forget_borrowed();
                // SAFETY: `got` is owned.
                unsafe { pinned.close_span() };
                let got = got.unwrap();
                let bits = |rows: &[(f32, Tid)]| {
                    rows.iter()
                        .map(|(s, t)| (s.to_bits(), *t))
                        .collect::<Vec<_>>()
                };
                assert_eq!(bits(&got.rows), bits(&want.rows), "top {k} of {node:?}");
                let (pins, early) = (state.pins.get() - pins, state.early.get() - early);
                if k == 200 {
                    assert!(
                        early > 0 && state.peak.get() * 2 < pins,
                        "{node:?}: {pins} pinned, {early} released early, {} at once",
                        state.peak.get()
                    );
                }
            }
        }
    }

    /// Ranks each of `nodes` (top 1, 10 and 200) over `blob` read in place
    /// in pages of `page_len`, every page a walk is past released (none
    /// kept) and poisoned, and over `blob` in memory, and asserts they
    /// agree. Returns the pages released early.
    fn released_walks_match(
        docs: &[Tid],
        members: &[Vec<(usize, u32)>],
        blob: &[u8],
        page_len: usize,
        nodes: &[Node],
    ) -> usize {
        let parsed = Segment::parse(blob).unwrap();
        let source = segment::tinshape::blob::PinningSource::new(blob.to_vec(), page_len);
        let pinned = LazyBlob::new(Box::new(source.clone()));
        let in_place = assembled_in_place(&pinned, &parsed, blob);
        assert_eq!(pinned.keep(), 0);
        let names: Vec<String> = (0..members.len()).map(|t| format!("t{t}")).collect();
        let scorers: Vec<(String, TermScorer)> = (0..members.len())
            .filter(|t| !members[*t].is_empty())
            .map(|t| {
                let scorer = TermScorer::from_statistics(
                    docs.len() as u64,
                    members[t].len() as u64,
                    1.0,
                    Bm25Params::default(),
                    50.0,
                )
                .unwrap();
                (names[t].clone(), scorer)
            })
            .collect();
        let early = source.0.early.get();
        for node in nodes {
            let used: Vec<(String, TermScorer)> = scorers
                .iter()
                .filter(|(n, _)| leaf_terms(node).iter().any(|t| names[*t] == *n))
                .cloned()
                .collect();
            for k in [1, 10, 200] {
                let want = top_k(&parsed, node, &names, &used, k, &mut NoTouch).unwrap();
                pinned.open_span();
                let got = top_k(&in_place, node, &names, &used, k, &mut NoTouch);
                in_place.forget_borrowed();
                // SAFETY: `got` is owned.
                unsafe { pinned.close_span() };
                let got = got.unwrap();
                let bits = |rows: &[(f32, Tid)]| {
                    rows.iter()
                        .map(|(s, t)| (s.to_bits(), *t))
                        .collect::<Vec<_>>()
                };
                assert_eq!(bits(&got.rows), bits(&want.rows), "top {k} of {node:?}");
            }
        }
        source.0.early.get() - early
    }

    /// Documents `0..n` in small heap blocks, and per entry of `density`
    /// (percent) a term holding about that share of them, its counts
    /// `1..=max_tf`, drawn from a fixed seed.
    fn drawn(n: u32, density: &[u64], max_tf: u64) -> (Vec<Tid>, Vec<Vec<(usize, u32)>>) {
        let docs: Vec<Tid> = (0..n)
            .map(|i| Tid {
                block: i / 3 * 5,
                offset: (i % 3) as u16 + 1,
            })
            .collect();
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let members = density
            .iter()
            .map(|d| {
                let mut held = Vec::new();
                for r in 0..docs.len() {
                    if next() % 100 < *d {
                        held.push((r, (next() % max_tf + 1) as u32));
                    }
                }
                held
            })
            .collect();
        (docs, members)
    }

    /// A scoring term's footer decoded lazily reads on, block by block,
    /// from the page it read last: a walk releasing the pages of each group
    /// it is past must not release that page while the footer has bytes on
    /// it left to decode (the footer would decode the poison: "varint
    /// exceeds 64 bits"). Footers of small blocks, over many small pages.
    #[test]
    fn lazy_footers_read_on_after_their_groups_pages_are_released() {
        // Sparse records (one list each, footers of blocks): grouped
        // records keep their metadata in their directories instead.
        let (docs, members) = drawn(6_000, &[25, 20], 9);
        let options = Options {
            block_size: 2,
            grid_density: 0,
            ef_groups: false,
            paged: false,
            inline_lengths_min_documents: 0,
            ..Options::default()
        };
        let blob = build(&docs, &members, options);
        let parsed = Segment::parse(&blob).unwrap();
        let page_len = 97;
        for t in ["t0", "t1"] {
            let term = parsed.term(t).unwrap().unwrap();
            assert!(
                matches!(term.postings.form, Form::Sparse(_)),
                "a sparse record"
            );
            assert!(
                term.postings.footer.len() > 20 * page_len,
                "{t}: a footer of {} bytes, df {}",
                term.postings.footer.len(),
                term.postings.df
            );
        }
        use Node::*;
        let nodes = [
            And(vec![Term(0), Term(1)]),
            Or(vec![Term(0), Term(1)]),
            Term(0),
        ];
        assert!(released_walks_match(&docs, &members, &blob, page_len, &nodes) > 0);
    }

    /// Group directories are decoded when a term is opened, so a walk
    /// keeps nothing of their pages: every page holding one is released
    /// with the groups'. Many groups, over pages smaller than a directory.
    #[test]
    fn directories_hold_no_pages_a_walk_releases() {
        let (docs, members) = drawn(12_000, &[30, 20, 8], 3);
        let options = Options {
            grid_density: 16,
            grid_min_postings: 0,
            inline_lengths_min_documents: 0,
            ..Options::default()
        };
        let blob = build(&docs, &members, options);
        let parsed = Segment::parse(&blob).unwrap();
        assert!(parsed.docs.geometry.groups.len() >= 60);
        use Node::*;
        let nodes = [
            And(vec![Term(0), Term(1), Term(2)]),
            Or(vec![Term(0), Term(1), Term(2)]),
        ];
        assert!(released_walks_match(&docs, &members, &blob, 64, &nodes) > 0);
    }

    /// TF tails and the DL sidecar are read a value at a time, never
    /// through a slice kept across groups: wide buckets (counts up to 60)
    /// and a sidecar of many pages, each released once a walk is past it.
    #[test]
    fn tf_tails_and_lengths_are_read_again_after_release() {
        let (docs, members) = drawn(8_000, &[50, 25], 60);
        let options = Options {
            block_size: 32,
            grid_min_postings: 0,
            inline_lengths_min_documents: u32::MAX,
            ..Options::default()
        };
        let blob = build(&docs, &members, options);
        use Node::*;
        let nodes = [
            And(vec![Term(0), Term(1)]),
            Or(vec![Term(0), Term(1)]),
            Term(1),
        ];
        assert!(released_walks_match(&docs, &members, &blob, 80, &nodes) > 0);
    }

    /// Phrase checks read positions through cursors that keep offsets, not
    /// slices, across groups: a phrase, and one repeating a word, over
    /// positions spanning many released pages.
    #[test]
    fn positions_cursors_read_again_after_release() {
        let (docs, members) = drawn(6_000, &[55, 40], 6);
        let options = Options {
            block_size: 16,
            grid_min_postings: 0,
            inline_lengths_min_documents: 0,
            ..Options::default()
        };
        let blob = build(&docs, &members, options);
        use Node::*;
        let nodes = [
            Span {
                slots: vec![0, 1],
                query: SpanQuery::phrase([0, 1]),
            },
            Span {
                slots: vec![0, 0],
                query: SpanQuery::phrase([0, 1]),
            },
        ];
        assert!(released_walks_match(&docs, &members, &blob, 72, &nodes) > 0);
    }

    /// A walk whose first bound lies far into a common term's footer reads
    /// the footer up to it within one group: each page it reads past is
    /// released at once, not held to the group's end (a `full_score`
    /// conjunction at 150M rows held 97 pages at once, 426 at most, rather
    /// than some 40). A rare lead whose members all lie late, and a term in
    /// every document: a grouped record, whose score metadata is its
    /// directory, parsed when the term is opened and copied, so a far
    /// bound holds no metadata pages at all.
    #[test]
    fn a_far_bound_releases_the_footer_pages_it_reads_past() {
        let (docs, _) = drawn(9_000, &[], 1);
        let members: Vec<Vec<(usize, u32)>> = vec![
            (8_000..docs.len()).step_by(50).map(|r| (r, 2)).collect(),
            (0..docs.len()).map(|r| (r, (r % 3 + 1) as u32)).collect(),
        ];
        let options = Options {
            block_size: 4,
            grid_min_postings: 0,
            inline_lengths_min_documents: 0,
            ..Options::default()
        };
        let blob = build(&docs, &members, options);
        let parsed = Segment::parse(&blob).unwrap();
        let page_len = 97;
        let dense = parsed.term("t1").unwrap().unwrap().postings;
        assert!(matches!(dense.form, Form::Grouped(_)) && dense.footer.is_empty());
        let footer_pages = dense.directory_bytes().len() / page_len;
        let source = segment::tinshape::blob::PinningSource::new(blob.clone(), page_len);
        let pinned = LazyBlob::new(Box::new(source.clone()));
        let in_place = assembled_in_place(&pinned, &parsed, &blob);
        let names: Vec<String> = vec!["t0".into(), "t1".into()];
        let scorers: Vec<(String, TermScorer)> = (0..2)
            .map(|t| {
                let scorer = TermScorer::from_statistics(
                    docs.len() as u64,
                    members[t].len() as u64,
                    1.0,
                    Bm25Params::default(),
                    50.0,
                )
                .unwrap();
                (names[t].clone(), scorer)
            })
            .collect();
        let node = Node::And(vec![Node::Term(0), Node::Term(1)]);
        let want = top_k(&parsed, &node, &names, &scorers, 10, &mut NoTouch).unwrap();
        source.0.peak.set(0);
        pinned.open_span();
        let got = top_k(&in_place, &node, &names, &scorers, 10, &mut NoTouch);
        in_place.forget_borrowed();
        // SAFETY: `got` is owned.
        unsafe { pinned.close_span() };
        let got = got.unwrap();
        assert_eq!(got.rows, want.rows);
        let peak = source.0.peak.get();
        assert!(
            peak < 12,
            "{peak} pages held at once, of a {footer_pages}-page footer"
        );
    }

    /// A rare term's sparse list and inline lengths are borrowed by its
    /// record for the whole walk: held, not released, while a common
    /// term's pages are.
    #[test]
    fn sparse_lists_and_inline_lengths_stay_held() {
        let (docs, members) = drawn(70_000, &[1, 40], 4);
        // The rare term (about 700 postings) is never forced into grids.
        let options = Options {
            block_size: 8,
            grid_min_postings: 2_000,
            inline_lengths_max_df: 1_000,
            inline_lengths_min_documents: 0,
            ..Options::default()
        };
        let blob = build(&docs, &members, options);
        let parsed = Segment::parse(&blob).unwrap();
        let rare = parsed.term("t0").unwrap().unwrap();
        assert!(matches!(rare.postings.form, Form::Sparse(_)));
        assert!(rare.postings.lengths.is_some());
        use Node::*;
        let nodes = [
            And(vec![Term(0), Term(1)]),
            Or(vec![Term(0), Term(1)]),
            Term(0),
        ];
        assert!(released_walks_match(&docs, &members, &blob, 128, &nodes) > 0);
    }

    /// A phrase of two words in every document, ranked over a segment that
    /// loads what it reads, reads a few of their positions' pages and a few
    /// of the DL sidecar's, not their areas: a backend's memory must not
    /// grow with a common word's positions (at 150M rows a phrase of two
    /// common words loaded over a gigabyte of positions per backend).
    /// A phrase repeating a word needs that word at as many positions: a
    /// document whose bucket puts its count below that is no match, and
    /// its positions are not read (TIN's ranked `"zp zp"` reads none for
    /// documents of tf 1).
    #[test]
    fn a_phrase_repeating_a_word_reads_no_positions_for_too_few() {
        let docs: Vec<Tid> = (0..2_000u32)
            .map(|i| Tid {
                block: i / 20,
                offset: (i % 20) as u16 + 1,
            })
            .collect();
        // Every document holds the word once, every 50th twice.
        let members = vec![
            (0..docs.len())
                .map(|r| (r, if r % 50 == 0 { 2 } else { 1 }))
                .collect::<Vec<_>>(),
        ];
        let blob = build(&docs, &members, Options::default());
        let segment = Segment::parse(&blob).unwrap();
        let names = vec!["t0".to_owned()];
        let node = Node::Span {
            slots: vec![0, 0],
            query: SpanQuery::phrase([0, 1]),
        };
        let scorer = TermScorer::from_statistics(
            docs.len() as u64,
            docs.len() as u64,
            1.0,
            Bm25Params::default(),
            50.0,
        )
        .unwrap();
        let got = top_k(
            &segment,
            &node,
            &names,
            &[("t0".to_owned(), scorer)],
            10,
            &mut NoTouch,
        )
        .unwrap();
        let mut want: Vec<Tid> = (0..docs.len())
            .filter(|r| r % 50 == 0)
            .map(|r| docs[r])
            .collect();
        let rows: Vec<Tid> = got.rows.iter().map(|r| r.1).collect();
        want.retain(|t| rows.contains(t));
        assert_eq!(rows.len(), 10);
        assert_eq!(
            rows.len(),
            want.len(),
            "only documents holding the word twice"
        );
        assert!(
            got.position_checks <= 40,
            "{} position checks for 40 documents holding the word twice",
            got.position_checks
        );
    }

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
            docs in doc_tids(900, 30, 500).prop_filter("two documents", |d| d.len() >= 2),
            density in prop::collection::vec(1u32..100, 4),
            seed in any::<u64>(),
            k in 1usize..12,
            ties in any::<bool>(),
        ) {
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

    /// The segment of documents `local` (ranks into `docs`, ascending) of a
    /// table whose term `t` holds `members[t]`, with the documents `dead`
    /// marks deleted.
    fn build_part(
        docs: &[Tid],
        lengths: &[u32],
        members: &[Vec<(usize, u32)>],
        local: &[usize],
        dead: &[bool],
        options: Options,
    ) -> Vec<u8> {
        let tids: Vec<Tid> = local.iter().map(|r| docs[*r]).collect();
        let mut builder =
            Builder::new(tids, local.iter().map(|r| lengths[*r]).collect(), options).unwrap();
        // Terms are added in their names' order (`t10` before `t2`).
        let mut order: Vec<usize> = (0..members.len()).collect();
        order.sort_by_key(|t| format!("t{t}"));
        for (t, m) in order.into_iter().map(|t| (t, &members[t])) {
            let held: Vec<(u32, u32)> = m
                .iter()
                .filter_map(|(r, tf)| local.binary_search(r).ok().map(|i| (i as u32, *tf)))
                .collect();
            if held.is_empty() {
                continue;
            }
            let ranks: Vec<u32> = held.iter().map(|(i, _)| *i).collect();
            let buckets: Vec<u8> = held
                .iter()
                .map(|(_, tf)| TfBucket::from_count(*tf).value())
                .collect();
            let mut payload = PayloadBuilder::default();
            for (_, tf) in &held {
                payload.push(&(0..*tf).collect::<Vec<u32>>()).unwrap();
            }
            builder
                .add_term(&format!("t{t}"), &ranks, &buckets, &payload.finish())
                .unwrap();
        }
        let gone: Vec<u32> = local
            .iter()
            .enumerate()
            .filter(|(_, r)| dead[**r])
            .map(|(i, _)| i as u32)
            .collect();
        builder.finish(&gone).0
    }

    /// Every match scoring the same, once the top k fill each later one only
    /// ties the threshold from a later ctid and cannot be kept: the walk
    /// prunes it unread rather than scoring it (TIN's `t50` top 10 reads its
    /// first groups and stops).
    #[test]
    fn a_walk_missing_a_required_term_parses_no_record() {
        let docs: Vec<Tid> = (0..3_000u32)
            .map(|i| Tid {
                block: i / 10,
                offset: (i % 10) as u16 + 1,
            })
            .collect();
        let lengths = vec![12u32; docs.len()];
        let members = vec![(0..docs.len()).map(|r| (r, 1)).collect::<Vec<_>>()];
        let all: Vec<usize> = (0..docs.len()).collect();
        let dead = vec![false; docs.len()];
        let blob = build_part(&docs, &lengths, &members, &all, &dead, Options::default());
        let segment = Segment::parse(&blob).unwrap();
        let names = vec!["t0".to_owned(), "absent".to_owned()];
        let scorer =
            TermScorer::from_statistics(docs.len() as u64, 3_000, 1.0, Bm25Params::default(), 12.0)
                .unwrap();
        let scorers = vec![("t0".to_owned(), scorer)];
        let node = Node::And(vec![Node::Term(0), Node::Term(1)]);
        segment::tinshape::segment::reset_memo_counts();
        let got = top_k(&segment, &node, &names, &scorers, 10, &mut NoTouch).unwrap();
        assert!(got.rows.is_empty());
        let memo = segment::tinshape::segment::memo_counts();
        assert_eq!(memo.records_parsed, 0, "{memo:?}");
        assert_eq!(memo.directories, 0, "{memo:?}");
    }

    #[test]
    fn a_tie_at_the_threshold_from_a_later_ctid_is_not_scored() {
        let docs: Vec<Tid> = (0..6_000u32)
            .map(|i| Tid {
                block: i / 20,
                offset: (i % 20) as u16 + 1,
            })
            .collect();
        let lengths = vec![12u32; docs.len()];
        let members = vec![
            (0..docs.len()).map(|r| (r, 1)).collect::<Vec<_>>(),
            (0..docs.len()).step_by(2).map(|r| (r, 1)).collect(),
        ];
        let all: Vec<usize> = (0..docs.len()).collect();
        let dead = vec![false; docs.len()];
        let names: Vec<String> = (0..2).map(|t| format!("t{t}")).collect();
        for inline in [u32::MAX, 0] {
            let options = Options {
                block_size: 64,
                grid_min_postings: 0,
                inline_lengths_min_documents: inline,
                inline_lengths_max_df: u32::MAX,
                ..Options::default()
            };
            let blob = build_part(&docs, &lengths, &members, &all, &dead, options);
            let segment = Segment::parse(&blob).unwrap();
            let shapes = [
                (Node::Term(0), 1),
                (Node::And(vec![Node::Term(0), Node::Term(1)]), 2),
                (Node::Or(vec![Node::Term(0), Node::Term(1)]), 2),
            ];
            for (node, step) in shapes {
                let scorers: Vec<(String, TermScorer)> = leaf_terms(&node)
                    .into_iter()
                    .map(|t| {
                        let df = members[t].len() as u64;
                        let scorer = TermScorer::from_statistics(
                            docs.len() as u64,
                            df,
                            1.0,
                            Bm25Params::default(),
                            12.0,
                        )
                        .unwrap();
                        (names[t].clone(), scorer)
                    })
                    .collect();
                let got = top_k(&segment, &node, &names, &scorers, 10, &mut NoTouch).unwrap();
                let tids: Vec<Tid> = got.rows.iter().map(|r| r.1).collect();
                let want: Vec<Tid> = docs.iter().copied().step_by(step).take(10).collect();
                assert_eq!(tids, want, "{node:?}");
                assert!(
                    got.scored <= 30,
                    "{node:?} (inline {inline}) scored {} candidates",
                    got.scored
                );
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        /// Ties everywhere: few frequencies and lengths, several segments
        /// interleaved or in ctid ranges, walked in any order into shared
        /// rows, ties kept or not. The rows kept are a brute force's, bit
        /// for bit, so a bound that only ties the threshold prunes exactly
        /// what cannot win on ctid.
        #[test]
        fn ties_at_the_threshold_keep_the_exact_top_rows(
            docs in doc_tids(1200, 20, 700).prop_filter("two documents", |d| d.len() >= 2),
            density in prop::collection::vec(5u32..100, 4),
            seed in any::<u64>(),
            k in 1usize..16,
            ties in any::<bool>(),
            parts in 1usize..4,
            ranges in any::<bool>(),
            block_size in prop::sample::select(vec![4u32, 16, 128]),
            lengths_of in prop::sample::select(vec![vec![9u32], vec![9, 9, 9, 30], vec![3, 7, 11, 50, 200]]),
            inline in any::<bool>(),
            avg in prop::sample::select(vec![1.0f32, 9.0, 33.3]),
        ) {
            let mut state = seed | 1;
            let mut next = || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            };
            let lengths: Vec<u32> = (0..docs.len()).map(|_| lengths_of[(next() % lengths_of.len() as u64) as usize]).collect();
            let mut members: Vec<Vec<(usize, u32)>> = vec![Vec::new(); density.len()];
            for (t, d) in density.iter().enumerate() {
                for r in 0..docs.len() {
                    if next() % 100 < u64::from(*d) {
                        let tf = if next() % 5 == 0 { 2 } else { 1 };
                        members[t].push((r, tf));
                    }
                }
            }
            let side: Vec<usize> = (0..docs.len())
                .map(|r| if ranges { r * parts / docs.len() } else { (next() % parts as u64) as usize })
                .collect();
            let dead: Vec<bool> = (0..docs.len()).map(|_| next() % 11 == 0).collect();
            let options = Options {
                block_size,
                grid_min_postings: 0,
                inline_lengths_min_documents: if inline { 0 } else { u32::MAX },
                inline_lengths_max_df: u32::MAX,
                ..Options::default()
            };
            let blobs: Vec<Vec<u8>> = (0..parts)
                .map(|part| {
                    let local: Vec<usize> = (0..docs.len()).filter(|r| side[*r] == part).collect();
                    build_part(&docs, &lengths, &members, &local, &dead, options)
                })
                .collect();
            let segments: Vec<Segment<'_>> = blobs.iter().map(|b| Segment::parse(b).unwrap()).collect();
            let mut order: Vec<usize> = (0..parts).collect();
            for i in (1..order.len()).rev() {
                order.swap(i, (next() % (i as u64 + 1)) as usize);
            }
            let names: Vec<String> = (0..4).map(|t| format!("t{t}")).collect();
            let mut shapes = shapes();
            shapes.push(Node::Or(vec![Node::Term(0), Node::Term(1), Node::Term(2), Node::Term(3)]));
            shapes.push(Node::And(vec![Node::Term(3), Node::Term(1), Node::Term(0)]));
            for node in shapes {
                if matches!(node, Node::Not(_)) {
                    continue;
                }
                // Scorers in lexical term order, as the extension builds them.
                let mut leaves = leaf_terms(&node);
                leaves.sort_unstable();
                leaves.dedup();
                let scorers: Vec<(String, TermScorer)> = leaves
                    .into_iter()
                    .filter(|t| !members[*t].is_empty())
                    .map(|t| {
                        let df = members[t].len() as u64;
                        let scorer = TermScorer::from_statistics(docs.len() as u64, df, 1.0, Bm25Params::default(), avg).unwrap();
                        (names[t].clone(), scorer)
                    })
                    .collect();
                let mut want = crate::walk::TopRows::new(k, ties);
                for r in 0..docs.len() {
                    if dead[r] || !eval(&node, &members, r) {
                        continue;
                    }
                    let mut total = 0.0_f32;
                    for (name, scorer) in &scorers {
                        let t: usize = name[1..].parse().unwrap();
                        if let Some(tf) = holds(&members, t, r) {
                            total += scorer.score_bucket(TfBucket::from_count(tf), lengths[r]);
                        }
                    }
                    // A walk keeps only rows holding a scoring term (they
                    // score above zero); a zero fill is the caller's.
                    let row = crate::walk::Ranked(total, docs[r]);
                    if total > 0.0 && want.admits(&row) {
                        want.push(row);
                    }
                }
                let mut top = crate::walk::TopRows::new(k, ties);
                for &part in &order {
                    top_k_into(&segments[part], &node, &names, &scorers, &mut top, &mut AllVisible, &mut NoTouch).unwrap();
                }
                let bits = |rows: Vec<(f32, Tid)>| rows.into_iter().map(|(s, t)| (s.to_bits(), t)).collect::<Vec<_>>();
                prop_assert_eq!(bits(top.into_rows()), bits(want.into_rows()), "top {} ties {} parts {:?} of {:?}", k, ties, order, node);
            }
        }
    }

    /// A phrase counted over a segment read in place checks positions for
    /// each document holding its words, finding each one's posting index
    /// in its words' grids: a grid spanning pages is stitched once per
    /// group, not once per document (at 1M rows `"w1 w2"` stitched 238 MB
    /// for 30,049 candidates).
    #[test]
    fn a_phrase_count_stitches_a_grid_once_per_group() {
        let docs: Vec<Tid> = (0..4_000u32)
            .map(|i| Tid {
                block: i / 16,
                offset: (i % 16) as u16 + 1,
            })
            .collect();
        let lengths = vec![10u32; docs.len()];
        let members = vec![
            (0..docs.len()).map(|r| (r, 1)).collect::<Vec<_>>(),
            (0..docs.len()).map(|r| (r, 2)).collect(),
        ];
        let all: Vec<usize> = (0..docs.len()).collect();
        let options = Options {
            grid_min_postings: 0,
            grid_density: 4,
            ..Options::default()
        };
        let blob = build_part(
            &docs,
            &lengths,
            &members,
            &all,
            &vec![false; docs.len()],
            options,
        );
        let parsed = Segment::parse(&blob).unwrap();
        let pinned = LazyBlob::new(Box::new(segment::tinshape::blob::PinningSource::new(
            blob.clone(),
            509,
        )));
        let segment = assembled_in_place(&pinned, &parsed, &blob);
        let names: Vec<String> = (0..2).map(|t| format!("t{t}")).collect();
        // Term t holds positions 0..tf (see `build_part`): "t1 t0" is at 1, 2.
        let node = Node::Span {
            slots: vec![1, 0],
            query: SpanQuery::phrase([0, 1]),
        };
        pinned.open_span();
        let got = count(&segment, &node, &names, &mut NoTouch).unwrap();
        let stitched = pinned.stitched();
        segment.forget_borrowed();
        // SAFETY: `count` returned an owned result.
        unsafe { pinned.close_span() };
        assert_eq!(got, 0, "t0 holds only position 0, t1 positions 0 and 1");
        let phrase = Node::Span {
            slots: vec![0, 1],
            query: SpanQuery::phrase([0, 1]),
        };
        pinned.open_span();
        let got = count(&segment, &phrase, &names, &mut NoTouch).unwrap();
        let stitched = stitched.max(pinned.stitched());
        segment.forget_borrowed();
        // SAFETY: as above.
        unsafe { pinned.close_span() };
        assert_eq!(got, docs.len() as u64);
        assert!(
            stitched < blob.len(),
            "stitched {stitched} bytes of a {}-byte blob",
            blob.len()
        );
    }

    /// Counts reads and their bytes per area.
    #[derive(Default)]
    struct AreaReads {
        reads: [u64; Part::ALL.len()],
        bytes: [u64; Part::ALL.len()],
    }

    impl Touch for AreaReads {
        fn touch(&mut self, part: Part, _: usize, len: usize) {
            self.reads[part as usize] += 1;
            self.bytes[part as usize] += len as u64;
        }
    }

    /// The first matches in ctid order read the containers of the groups
    /// they are in and no others: a zero fill of k rows of a term in every
    /// document read every group's container (15 or 16 payload pages for
    /// any k on TIN's probe corpus, TIN 1 or 2). Folded a few groups at a
    /// time, the matches are those of one fold over every group.
    #[test]
    fn folding_the_first_groups_reads_only_their_containers() {
        let n = 5 * 256 * 20usize;
        let docs: Vec<Tid> = (0..n as u32)
            .map(|i| Tid {
                block: i / 20,
                offset: (i % 20) as u16 + 1,
            })
            .collect();
        let members = vec![
            (0..n).map(|r| (r, 1)).collect::<Vec<_>>(),
            (0..n).filter(|r| r % 3 == 0).map(|r| (r, 1)).collect(),
        ];
        let all: Vec<usize> = (0..n).collect();
        let blob = build_part(
            &docs,
            &vec![4u32; n],
            &members,
            &all,
            &vec![false; n],
            Options::default(),
        );
        let segment = Segment::parse(&blob).unwrap();
        let names = vec!["t0".to_owned(), "t1".to_owned()];
        for node in [Node::Term(0), Node::Or(vec![Node::Term(0), Node::Term(1)])] {
            let mut every = Vec::new();
            let mut whole = AreaReads::default();
            let mut terms = open_terms(&segment, &names, &mut NoTouch).unwrap();
            for_each_match(
                &segment,
                &node,
                &mut terms,
                &mut whole,
                &mut |group, words| {
                    tids_in(&segment.docs.geometry, group, words, |tid| every.push(tid));
                },
            )
            .unwrap();
            assert_eq!(every, docs, "{node:?}");
            let mut terms = open_terms(&segment, &names, &mut NoTouch).unwrap();
            let groups = match_groups(&segment, &node, &mut terms, &mut NoTouch);
            assert_eq!(groups.len(), 5, "{node:?}");
            // The first group alone.
            let mut first = AreaReads::default();
            let mut emitted = Vec::new();
            let folded = fold_groups(
                &segment,
                &node,
                &mut terms,
                &mut first,
                &groups,
                &mut |group, _| {
                    emitted.push(group);
                    false
                },
            )
            .unwrap();
            assert_eq!((folded, emitted), (1, vec![groups[0] as usize]), "{node:?}");
            let payload = Part::Payload as usize;
            assert!(
                first.bytes[payload] * 4 <= whole.bytes[payload],
                "{node:?}: the first group read {} payload bytes of {}",
                first.bytes[payload],
                whole.bytes[payload]
            );
            // Two groups at a time, each fold over terms opened again.
            let mut listed = Vec::new();
            let mut at = 0;
            while at < groups.len() {
                let mut terms = open_terms(&segment, &names, &mut NoTouch).unwrap();
                let mut left = 2;
                at += fold_groups(
                    &segment,
                    &node,
                    &mut terms,
                    &mut NoTouch,
                    &groups[at..],
                    &mut |group, words| {
                        tids_in(&segment.docs.geometry, group, words, |tid| listed.push(tid));
                        left -= 1;
                        left > 0
                    },
                )
                .unwrap();
            }
            assert_eq!(listed, every, "{node:?}");
        }
    }

    /// While the top k fill, a phrase's candidates are checked in ctid
    /// order, so each word's positions are read forward from the entry read
    /// last: a phrase of two words in every document, with fewer matches
    /// than k, reads each word's positions about once over. Best bound
    /// first (the first word's count varies), each read went back to its
    /// entry's block start.
    #[test]
    fn a_filling_phrase_reads_positions_forward() {
        let n = 4_000usize;
        let docs: Vec<Tid> = (0..n as u32)
            .map(|i| Tid {
                block: i / 20,
                offset: (i % 20) as u16 + 1,
            })
            .collect();
        let lengths = vec![10u32; n];
        // Term t holds positions 0..tf: "t0 t1" needs t1 at 1, so tf 2.
        let members = vec![
            (0..n).map(|r| (r, r as u32 % 4 + 1)).collect::<Vec<_>>(),
            (0..n)
                .map(|r| (r, if r % 1_000 == 0 { 2 } else { 1 }))
                .collect(),
        ];
        let all: Vec<usize> = (0..n).collect();
        let options = Options {
            grid_min_postings: 0,
            inline_lengths_min_documents: u32::MAX,
            ..Options::default()
        };
        let blob = build_part(&docs, &lengths, &members, &all, &vec![false; n], options);
        let segment = Segment::parse(&blob).unwrap();
        let names = vec!["t0".to_owned(), "t1".to_owned()];
        let scorer =
            |df| TermScorer::from_statistics(10_000, df, 1.0, Bm25Params::default(), 10.0).unwrap();
        let scorers = vec![
            ("t0".to_owned(), scorer(4_000)),
            ("t1".to_owned(), scorer(4_000)),
        ];
        let node = Node::Span {
            slots: vec![0, 1],
            query: SpanQuery::phrase([0, 1]),
        };
        let mut reads = AreaReads::default();
        let got = top_k(&segment, &node, &names, &scorers, 10, &mut reads).unwrap();
        assert_eq!(got.rows.len(), 4);
        assert_eq!(got.position_checks, n as u64);
        // Each word's entries read once (t0's 3.5 bytes on average, t1's
        // one), and per read its block's mask (4 bytes): 12.5 bytes a
        // document. Best first, 20.5.
        let bytes = reads.bytes[Part::Positions as usize];
        assert!(bytes <= 13 * n as u64, "{bytes} bytes of positions read");
    }

    /// While the top k fill, a phrase's candidate is confirmed by its
    /// positions before its length is read: one that holds both words but
    /// not the phrase reads no DL (TIN reads lengths for phrase matches
    /// only). Here every document holds both words, one in 100 as the
    /// phrase, and all score the same, so the walk ends once 10 are found.
    #[test]
    fn a_phrase_reads_lengths_only_for_matches_while_filling() {
        let n = 2_000usize;
        let docs: Vec<Tid> = (0..n as u32)
            .map(|i| Tid {
                block: i / 20,
                offset: (i % 20) as u16 + 1,
            })
            .collect();
        let lengths = vec![10u32; n];
        // Term t holds positions 0..tf: "t0 t1" needs t1 at 1, so tf 2.
        let members = vec![
            (0..n).map(|r| (r, 1)).collect::<Vec<_>>(),
            (0..n)
                .map(|r| (r, if r % 100 == 0 { 2 } else { 1 }))
                .collect(),
        ];
        let all: Vec<usize> = (0..n).collect();
        let options = Options {
            grid_min_postings: 0,
            inline_lengths_min_documents: u32::MAX,
            ..Options::default()
        };
        let blob = build_part(&docs, &lengths, &members, &all, &vec![false; n], options);
        let segment = Segment::parse(&blob).unwrap();
        let names = vec!["t0".to_owned(), "t1".to_owned()];
        // Scores ignore tf here (k1 0): every match scores the same.
        let params = Bm25Params { k1: 0.0, b: 0.75 };
        let scorer = TermScorer::from_statistics(10_000, 2_000, 1.0, params, 10.0).unwrap();
        let scorers = vec![("t0".to_owned(), scorer.clone()), ("t1".to_owned(), scorer)];
        let node = Node::Span {
            slots: vec![0, 1],
            query: SpanQuery::phrase([0, 1]),
        };
        let mut reads = AreaReads::default();
        let got = top_k(&segment, &node, &names, &scorers, 10, &mut reads).unwrap();
        let want: Vec<Tid> = (0..n).step_by(100).take(10).map(|r| docs[r]).collect();
        assert_eq!(got.rows.iter().map(|r| r.1).collect::<Vec<_>>(), want);
        assert!(got.position_checks >= 900, "{} checks", got.position_checks);
        // A length is two reads (its block's header and its bits).
        let lengths_read = reads.reads[Part::DlSidecar as usize] / 2;
        assert!(
            lengths_read <= 10,
            "{lengths_read} lengths read for 10 matches"
        );
        // With nothing to score (both words elided), no length is read.
        let mut reads = AreaReads::default();
        let got = top_k(&segment, &node, &names, &[], 10, &mut reads).unwrap();
        assert_eq!(got.rows.iter().map(|r| r.1).collect::<Vec<_>>(), want);
        assert_eq!(reads.reads[Part::DlSidecar as usize], 0);
    }

    /// A phrase's candidates in a group are scored best bound first: its
    /// best rows come last in ctid order here, each earlier one beating the
    /// one before, so a walk in ctid order would check the positions and
    /// read the length of every one; best first, the best rows set the
    /// threshold and the rest fall short on their buckets.
    #[test]
    fn a_phrase_group_is_scored_best_bound_first() {
        let n = 200usize;
        let docs: Vec<Tid> = (0..n as u32)
            .map(|i| Tid {
                block: i / 20,
                offset: (i % 20) as u16 + 1,
            })
            .collect();
        let lengths: Vec<u32> = (0..n as u32).map(|r| 400 - r).collect();
        // Term t holds positions 0..tf (see `build_part`): "t0 t1" is at 0, 1
        // in every document.
        let members = vec![
            (0..n)
                .map(|r| (r, if r >= n - 20 { 10 } else { 1 }))
                .collect::<Vec<_>>(),
            (0..n).map(|r| (r, 2)).collect(),
        ];
        let all: Vec<usize> = (0..n).collect();
        let options = Options {
            block_size: 64,
            grid_min_postings: 0,
            inline_lengths_min_documents: u32::MAX,
            ..Options::default()
        };
        let blob = build_part(&docs, &lengths, &members, &all, &vec![false; n], options);
        let segment = Segment::parse(&blob).unwrap();
        let names = vec!["t0".to_owned(), "t1".to_owned()];
        let scorer = |df| {
            TermScorer::from_statistics(10_000, df, 1.0, Bm25Params::default(), 100.0).unwrap()
        };
        let scorers = vec![
            ("t0".to_owned(), scorer(200)),
            ("t1".to_owned(), scorer(200)),
        ];
        let node = Node::Span {
            slots: vec![0, 1],
            query: SpanQuery::phrase([0, 1]),
        };
        let got = top_k(&segment, &node, &names, &scorers, 3, &mut NoTouch).unwrap();
        let tids: Vec<Tid> = got.rows.iter().map(|r| r.1).collect();
        assert_eq!(tids, vec![docs[n - 1], docs[n - 2], docs[n - 3]]);
        assert!(got.scored <= 30, "scored {}", got.scored);
        assert!(
            got.position_checks <= 30,
            "{} positions checks",
            got.position_checks
        );
    }

    /// A disjunction's candidates in a group are scored best bound first,
    /// as a phrase's are: the best of the group raise the threshold over the
    /// rest before their lengths are read, where in ctid order each later
    /// (shorter, better) document beat the one before.
    #[test]
    fn a_disjunction_group_is_scored_best_bound_first() {
        let n = 200usize;
        let docs: Vec<Tid> = (0..n as u32)
            .map(|i| Tid {
                block: i / 20,
                offset: (i % 20) as u16 + 1,
            })
            .collect();
        let lengths: Vec<u32> = (0..n as u32).map(|r| 400 - r).collect();
        let members = vec![
            (0..n)
                .map(|r| (r, if r >= n - 20 { 10 } else { 1 }))
                .collect::<Vec<_>>(),
            (0..n).step_by(2).map(|r| (r, 2)).collect(),
        ];
        let all: Vec<usize> = (0..n).collect();
        let options = Options {
            block_size: 64,
            grid_min_postings: 0,
            inline_lengths_min_documents: u32::MAX,
            ..Options::default()
        };
        let blob = build_part(&docs, &lengths, &members, &all, &vec![false; n], options);
        let segment = Segment::parse(&blob).unwrap();
        let names = vec!["t0".to_owned(), "t1".to_owned()];
        let scorer = |df| {
            TermScorer::from_statistics(10_000, df, 1.0, Bm25Params::default(), 100.0).unwrap()
        };
        let scorers = vec![
            ("t0".to_owned(), scorer(200)),
            ("t1".to_owned(), scorer(100)),
        ];
        let node = Node::Or(vec![Node::Term(0), Node::Term(1)]);
        let got = top_k(&segment, &node, &names, &scorers, 3, &mut NoTouch).unwrap();
        let tids: Vec<Tid> = got.rows.iter().map(|r| r.1).collect();
        assert_eq!(tids, vec![docs[n - 2], docs[n - 4], docs[n - 6]]);
        assert!(got.scored <= 30, "scored {}", got.scored);
    }

    /// A ranked disjunction over a segment read in place reads its terms'
    /// grids where they lie: one on a page in place, one across pages
    /// copied into the term's row, neither stitched into the span's buffers
    /// (at 150M rows those were 4 MB per query, 14 MB scoring every term).
    /// Conjunctions and phrases over the same grids (a grid across pages
    /// stitched) rank as in memory. The pages are released as the walk
    /// passes them and poisoned, so a grid read after its page was released
    /// would rank garbage.
    #[test]
    fn ranked_walks_read_grids_in_place() {
        use segment::tinshape::blob::{self, Kind};
        let n = 6_000usize;
        let docs: Vec<Tid> = (0..n as u32)
            .map(|i| Tid {
                block: i / 16,
                offset: (i % 16) as u16 + 1,
            })
            .collect();
        let lengths: Vec<u32> = (0..n as u32).map(|r| r % 50 + 5).collect();
        let members: Vec<Vec<(usize, u32)>> = (0..3)
            .map(|t| {
                (0..n)
                    .filter(|r| (r * 7 + t * 3) % (t + 2) != 0)
                    .map(|r| (r, (r % 5 + t) as u32 + 1))
                    .collect()
            })
            .collect();
        let all: Vec<usize> = (0..n).collect();
        let options = Options {
            grid_min_postings: 0,
            grid_density: 4,
            ..Options::default()
        };
        let blob = build_part(&docs, &lengths, &members, &all, &vec![false; n], options);
        let parsed = Segment::parse(&blob).unwrap();
        let pinned = LazyBlob::new(Box::new(segment::tinshape::blob::PinningSource::new(
            blob.clone(),
            509,
        )));
        let segment = assembled_in_place(&pinned, &parsed, &blob);
        let names: Vec<String> = (0..3).map(|t| format!("t{t}")).collect();
        let scorers: Vec<(String, TermScorer)> = (0..3)
            .map(|t| {
                let df = members[t].len() as u64;
                let s = TermScorer::from_statistics(n as u64, df, 1.0, Bm25Params::default(), 30.0);
                (names[t].clone(), s.unwrap())
            })
            .collect();
        let nodes = [
            Node::Or((0..3).map(Node::Term).collect()),
            Node::And((0..3).map(Node::Term).collect()),
            Node::And(vec![Node::Term(0), Node::Term(2)]),
            // Term t holds positions 0..tf (see `build_part`).
            Node::Span {
                slots: vec![2, 1],
                query: SpanQuery::phrase([0, 1]),
            },
        ];
        let bits = |rows: &[(f32, Tid)]| {
            rows.iter()
                .map(|(s, t)| (s.to_bits(), *t))
                .collect::<Vec<_>>()
        };
        for node in &nodes {
            let want = top_k(&parsed, node, &names, &scorers, 10, &mut NoTouch).unwrap();
            assert!(!want.rows.is_empty(), "{node:?} matches");
            pinned.open_span();
            blob::reset_stats();
            let got = top_k(&segment, node, &names, &scorers, 10, &mut NoTouch).unwrap();
            let stitched = blob::stats()[Kind::Container as usize].stitched;
            segment.forget_borrowed();
            // SAFETY: `top_k` returned owned results.
            unsafe { pinned.close_span() };
            assert_eq!(bits(&got.rows), bits(&want.rows), "{node:?}");
            if matches!(node, Node::Or(_)) {
                assert_eq!(stitched, 0, "container bytes stitched for {node:?}");
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]
        /// A document has one length, so every term it holds bounds it from
        /// below: by the shortest length its block holds at the candidate's
        /// bucket or above. With the final rows kept from the start, the
        /// walk scores (reads the length of) only a candidate whose buckets
        /// at the longest of those lengths could reach the threshold. Term
        /// counts grow with length here, as a document repeating a word
        /// does.
        #[test]
        fn a_candidate_is_bounded_at_the_length_all_its_terms_imply(
            n in 200usize..2500,
            density in prop::collection::vec(5u32..60, 3),
            seed in any::<u64>(),
            k in 1usize..12,
            block_size in prop::sample::select(vec![16u32, 64, 256]),
        ) {
            let docs: Vec<Tid> = (0..n as u32).map(|i| Tid { block: i / 7, offset: (i % 7) as u16 + 1 }).collect();
            let mut state = seed | 1;
            let mut next = || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            };
            // Eight words, then one word repeated: its count rises with the
            // length.
            let mut lengths = Vec::with_capacity(n);
            let mut members: Vec<Vec<(usize, u32)>> = vec![Vec::new(); density.len()];
            for r in 0..n {
                let repeated = (next() % density.len() as u64) as usize;
                let extra = ((next() % 30) * (next() % 30) / 30) as u32;
                lengths.push(8 + extra);
                for (t, d) in density.iter().enumerate() {
                    let base = u32::from(next() % 100 < u64::from(*d));
                    let tf = base + if t == repeated { extra } else { 0 };
                    if tf > 0 {
                        members[t].push((r, tf));
                    }
                }
            }
            let all: Vec<usize> = (0..n).collect();
            let options = Options { block_size, grid_min_postings: 0, inline_lengths_min_documents: u32::MAX, ..Options::default() };
            let blob = build_part(&docs, &lengths, &members, &all, &vec![false; n], options);
            let segment = Segment::parse(&blob).unwrap();
            let names: Vec<String> = (0..3).map(|t| format!("t{t}")).collect();
            // Per term, whether its record is grouped: then its blocks are
            // its groups and its frontiers' lengths are rounded down.
            let grouped: Vec<bool> = names
                .iter()
                .map(|name| {
                    segment.term(name).unwrap().is_some_and(|t| matches!(t.postings.form, Form::Grouped(_)))
                })
                .collect();
            let block_of = |t: usize, p: usize| -> usize {
                if grouped[t] { (docs[members[t][p].0].block / 256) as usize } else { p / block_size as usize }
            };
            let stored = |t: usize, length: u32| -> u32 {
                use segment::tinshape::postings::{round_length, rounded_length};
                if grouped[t] { rounded_length(round_length(length)) } else { length }
            };
            // Per term, per block of its postings, the shortest length at
            // each bucket or above.
            let floors: Vec<Vec<[u32; 16]>> = (0..members.len())
                .map(|t| {
                    let blocks = members[t].len().max(1);
                    let mut out: Vec<[u32; 16]> = vec![[u32::MAX; 16]; blocks];
                    for (p, (r, tf)) in members[t].iter().enumerate() {
                        let b = usize::from(TfBucket::from_count(*tf).value());
                        let best = &mut out[block_of(t, p)];
                        best[b] = best[b].min(stored(t, lengths[*r]));
                    }
                    for best in &mut out {
                        for b in (0..15).rev() {
                            best[b] = best[b].min(best[b + 1]);
                        }
                    }
                    out
                })
                .collect();
            for node in [
                Node::And(vec![Node::Term(0), Node::Term(1)]),
                Node::And(vec![Node::Term(0), Node::Term(1), Node::Term(2)]),
                Node::Or(vec![Node::Term(0), Node::Term(1), Node::Term(2)]),
            ] {
                let scorers: Vec<(String, TermScorer)> = leaf_terms(&node)
                    .into_iter()
                    .filter(|t| !members[*t].is_empty())
                    .map(|t| {
                        let df = members[t].len() as u64;
                        (names[t].clone(), TermScorer::from_statistics(n as u64, df, 1.0, Bm25Params::default(), 14.0).unwrap())
                    })
                    .collect();
                let terms: Vec<usize> = scorers.iter().map(|(name, _)| name[1..].parse().unwrap()).collect();
                let got = top_k(&segment, &node, &names, &scorers, k, &mut NoTouch).unwrap();
                if got.rows.len() < k {
                    continue;
                }
                let theta = got.rows[k - 1].0;
                let mut seeded = crate::walk::TopRows::new(k, false);
                for row in &got.rows {
                    seeded.push(crate::walk::Ranked(row.0, row.1));
                }
                let walked = top_k_into(&segment, &node, &names, &scorers, &mut seeded, &mut AllVisible, &mut NoTouch).unwrap();
                // The candidates whose bound at the shared length reaches
                // the threshold.
                let mut reach = 0u64;
                for r in 0..n {
                    if !eval(&node, &members, r) {
                        continue;
                    }
                    let mut floor = 0u32;
                    let mut held = Vec::new();
                    for (s, t) in terms.iter().enumerate() {
                        if let Some(p) = members[*t].iter().position(|(d, _)| *d == r) {
                            let bucket = TfBucket::from_count(members[*t][p].1);
                            floor = floor.max(floors[*t][block_of(*t, p)][usize::from(bucket.value())]);
                            held.push((s, bucket));
                        }
                    }
                    let mut bound = 0.0_f32;
                    for (s, bucket) in held {
                        bound += scorers[s].1.bound_through(bucket, floor);
                    }
                    reach += u64::from(bound >= theta);
                }
                prop_assert!(walked.scored <= reach, "{:?}: scored {} with {} in reach", node, walked.scored, reach);
            }
        }
    }

    /// Counts TF tail reads.
    #[derive(Default)]
    struct TfReads(u64);

    impl Touch for TfReads {
        fn touch(&mut self, part: Part, _: usize, _: usize) {
            self.0 += u64::from(matches!(part, Part::TfTail));
        }
    }

    /// A disjunction reads a candidate's buckets a term at a time, the
    /// term whose block bounds it highest first, and stops once the bound
    /// they leave falls below the threshold: a rare word's bucket of 1 in a
    /// block that also holds a 6 rules the candidate out without the common
    /// word's bucket being read (TIN reads a term's TF only for a candidate
    /// that still needs it).
    #[test]
    fn a_disjunction_reads_buckets_only_while_a_candidate_needs_them() {
        let n = 4_000usize;
        let docs: Vec<Tid> = (0..n as u32)
            .map(|i| Tid {
                block: i / 20,
                offset: (i % 20) as u16 + 1,
            })
            .collect();
        let lengths = vec![10u32; n];
        // The rare word in every fourth document, six times in one in 64
        // (one per block of its postings: the first ten are the winners);
        // the common word in all, twice in every third but once where the
        // rare word is six times.
        let rare: Vec<(usize, u32)> = (0..n)
            .step_by(4)
            .map(|r| (r, if r % 64 == 0 { 6 } else { 1 }))
            .collect();
        let common: Vec<(usize, u32)> = (0..n)
            .map(|r| (r, if r % 3 == 1 && r % 64 != 0 { 2 } else { 1 }))
            .collect();
        let members = vec![rare, common];
        let all: Vec<usize> = (0..n).collect();
        let options = Options {
            block_size: 16,
            grid_min_postings: 0,
            inline_lengths_min_documents: u32::MAX,
            ..Options::default()
        };
        let blob = build_part(&docs, &lengths, &members, &all, &vec![false; n], options);
        let segment = Segment::parse(&blob).unwrap();
        let names: Vec<String> = (0..2).map(|t| format!("t{t}")).collect();
        let node = Node::Or(vec![Node::Term(0), Node::Term(1)]);
        let scorers: Vec<(String, TermScorer)> = (0..2)
            .map(|t| {
                let df = members[t].len() as u64;
                let scorer =
                    TermScorer::from_statistics(n as u64, df, 1.0, Bm25Params::default(), 10.0)
                        .unwrap();
                (names[t].clone(), scorer)
            })
            .collect();
        let k = 10;
        let got = top_k(&segment, &node, &names, &scorers, k, &mut NoTouch).unwrap();
        let want: Vec<Tid> = (0..n).step_by(64).take(k).map(|r| docs[r]).collect();
        assert_eq!(got.rows.iter().map(|r| r.1).collect::<Vec<_>>(), want);
        // With the final rows kept from the start, every block of the rare
        // word reaches the threshold, and each candidate holding it once is
        // ruled out by its bucket there.
        let mut seeded = crate::walk::TopRows::new(k, false);
        for row in &got.rows {
            seeded.push(crate::walk::Ranked(row.0, row.1));
        }
        let mut reads = TfReads::default();
        let walked = top_k_into(
            &segment,
            &node,
            &names,
            &scorers,
            &mut seeded,
            &mut AllVisible,
            &mut reads,
        )
        .unwrap();
        assert!(walked.candidates > 20, "{} candidates", walked.candidates);
        assert!(
            reads.0 * 10 <= walked.candidates * 12,
            "{} TF reads for {} candidates",
            reads.0,
            walked.candidates
        );
    }

    /// A disjunction bounds a candidate's unread terms at the length its
    /// read buckets imply (TIN's shared length): a rare word held only by
    /// long documents, read first, puts the candidate at their length, where
    /// the common words' largest buckets fall short, though their blocks'
    /// short documents left it in reach. Their TF is not read.
    #[test]
    fn a_disjunction_bounds_unread_terms_at_the_length_read_buckets_imply() {
        let n = 4_000usize;
        let docs: Vec<Tid> = (0..n as u32)
            .map(|i| Tid {
                block: i / 20,
                offset: (i % 20) as u16 + 1,
            })
            .collect();
        let lengths: Vec<u32> = (0..n).map(|r| if r % 10 == 0 { 5 } else { 200 }).collect();
        // The rare word eight times in long documents only; two common words
        // once in every odd (long) document and every short one; the winners'
        // word in twenty long documents (the first ten win the ties).
        let rare: Vec<(usize, u32)> = (0..n).filter(|r| r % 8 == 1).map(|r| (r, 8)).collect();
        let common: Vec<(usize, u32)> = (0..n)
            .filter(|r| r % 2 == 1 || r % 10 == 0)
            .map(|r| (r, 1))
            .collect();
        let winners: Vec<(usize, u32)> = (0..n).filter(|r| r % 200 == 3).map(|r| (r, 1)).collect();
        let members = vec![rare, common.clone(), common, winners];
        let all: Vec<usize> = (0..n).collect();
        let options = Options {
            block_size: 16,
            grid_min_postings: 0,
            inline_lengths_min_documents: u32::MAX,
            ..Options::default()
        };
        let blob = build_part(&docs, &lengths, &members, &all, &vec![false; n], options);
        let segment = Segment::parse(&blob).unwrap();
        let names: Vec<String> = (0..4).map(|t| format!("t{t}")).collect();
        let node = Node::Or((0..4).map(Node::Term).collect());
        let scorers: Vec<(String, TermScorer)> = (0..4)
            .map(|t| {
                let df = members[t].len() as u64;
                let scorer =
                    TermScorer::from_statistics(n as u64, df, 1.0, Bm25Params::default(), 100.0)
                        .unwrap();
                (names[t].clone(), scorer)
            })
            .collect();
        let at = |t: usize, tf: u32, length: u32| {
            scorers[t].1.bound_through(TfBucket::from_count(tf), length)
        };
        // The threshold (a winner's score) lies above what a rare word's
        // candidate scores at its length, and below its blocks' bounds.
        let theta = at(3, 1, 200) + at(1, 1, 200) + at(2, 1, 200);
        assert!(theta > at(0, 8, 200) + at(1, 1, 200) + at(2, 1, 200));
        assert!(theta < at(0, 8, 200) + at(1, 1, 5) + at(2, 1, 5));
        let k = 10;
        let got = top_k(&segment, &node, &names, &scorers, k, &mut NoTouch).unwrap();
        let want: Vec<Tid> = members[3].iter().take(10).map(|(r, _)| docs[*r]).collect();
        assert_eq!(got.rows.iter().map(|r| r.1).collect::<Vec<_>>(), want);
        let mut seeded = crate::walk::TopRows::new(k, false);
        for row in &got.rows {
            seeded.push(crate::walk::Ranked(row.0, row.1));
        }
        let mut reads = TfReads::default();
        let walked = top_k_into(
            &segment,
            &node,
            &names,
            &scorers,
            &mut seeded,
            &mut AllVisible,
            &mut reads,
        )
        .unwrap();
        assert!(walked.candidates >= 400, "{} candidates", walked.candidates);
        assert!(
            reads.0 * 10 <= walked.candidates * 12,
            "{} TF reads for {} candidates",
            reads.0,
            walked.candidates
        );
    }

    /// A conjunction reads a common word's bucket last, and only for a
    /// candidate the rarer word's bucket leaves in reach: a rare word's
    /// bucket of 1 in a block that also holds a 6 rules the candidate out
    /// at the common word's block bound, without its bucket.
    #[test]
    fn a_conjunction_reads_a_common_words_bucket_only_for_candidates_in_reach() {
        let n = 4_000usize;
        let docs: Vec<Tid> = (0..n as u32)
            .map(|i| Tid {
                block: i / 20,
                offset: (i % 20) as u16 + 1,
            })
            .collect();
        let lengths = vec![10u32; n];
        // The rare word in every 16th document (fewer than one in ten), six
        // times in one in 256 (one per block of its postings: the first ten
        // are the winners); the common word in all, twice in every third.
        let rare: Vec<(usize, u32)> = (0..n)
            .step_by(16)
            .map(|r| (r, if r % 256 == 0 { 6 } else { 1 }))
            .collect();
        let common: Vec<(usize, u32)> = (0..n)
            .map(|r| (r, if r % 3 == 1 && r % 256 != 0 { 2 } else { 1 }))
            .collect();
        let members = vec![rare, common];
        let all: Vec<usize> = (0..n).collect();
        let options = Options {
            block_size: 16,
            grid_min_postings: 0,
            inline_lengths_min_documents: u32::MAX,
            ..Options::default()
        };
        let blob = build_part(&docs, &lengths, &members, &all, &vec![false; n], options);
        let segment = Segment::parse(&blob).unwrap();
        let names: Vec<String> = (0..2).map(|t| format!("t{t}")).collect();
        let node = Node::And(vec![Node::Term(0), Node::Term(1)]);
        let scorers: Vec<(String, TermScorer)> = (0..2)
            .map(|t| {
                let df = members[t].len() as u64;
                let scorer =
                    TermScorer::from_statistics(n as u64, df, 1.0, Bm25Params::default(), 10.0)
                        .unwrap();
                (names[t].clone(), scorer)
            })
            .collect();
        let k = 10;
        let got = top_k(&segment, &node, &names, &scorers, k, &mut NoTouch).unwrap();
        let want: Vec<Tid> = (0..n).step_by(256).take(k).map(|r| docs[r]).collect();
        assert_eq!(got.rows.iter().map(|r| r.1).collect::<Vec<_>>(), want);
        // With the final rows kept from the start, every candidate holding
        // the rare word once is ruled out by its bucket there.
        let mut seeded = crate::walk::TopRows::new(k, false);
        for row in &got.rows {
            seeded.push(crate::walk::Ranked(row.0, row.1));
        }
        let mut reads = TfReads::default();
        let walked = top_k_into(
            &segment,
            &node,
            &names,
            &scorers,
            &mut seeded,
            &mut AllVisible,
            &mut reads,
        )
        .unwrap();
        assert!(walked.candidates > 20, "{} candidates", walked.candidates);
        assert!(
            reads.0 * 10 <= walked.candidates * 12,
            "{} TF reads for {} candidates",
            reads.0,
            walked.candidates
        );
    }

    /// Documents, their lengths, and per word its (document, count) pairs.
    type Corpus = (Vec<Tid>, Vec<u32>, Vec<Vec<(usize, u32)>>);

    /// Dense words: `n` documents, seven a page (some 1,800 a group), word
    /// `t` in `density[t]` percent of them, counts mostly one and lengths
    /// from a short list (ties at a threshold are common).
    fn dense_words(n: usize, density: &[u32], seed: u64) -> Corpus {
        let docs: Vec<Tid> = (0..n as u32)
            .map(|i| Tid {
                block: i / 7,
                offset: (i % 7) as u16 + 1,
            })
            .collect();
        let mut state = seed | 1;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let lengths: Vec<u32> = (0..n)
            .map(|_| [4u32, 9, 9, 20, 33, 60][(next() % 6) as usize])
            .collect();
        let mut members: Vec<Vec<(usize, u32)>> = vec![Vec::new(); density.len()];
        for r in 0..n {
            for (t, d) in density.iter().enumerate() {
                if next() % 100 < u64::from(*d) {
                    let tf = match next() % 16 {
                        0 => 2 + (next() % 9) as u32,
                        1..=3 => 2,
                        _ => 1,
                    };
                    members[t].push((r, tf));
                }
            }
        }
        (docs, lengths, members)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]
        /// A group's candidates of dense words are sieved by their buckets
        /// before each is bounded alone (`rank::Walk::sieve`): the rows
        /// kept are a brute force's, bit for bit, for conjunctions,
        /// disjunctions and a conjunction with a disjunction under it (its
        /// scoring terms not all required).
        #[test]
        fn dense_words_sieved_by_their_buckets_keep_the_exact_top_rows(
            n in 2000usize..6000,
            density in prop::collection::vec(40u32..100, 3),
            seed in any::<u64>(),
            k in 1usize..12,
        ) {
            let (docs, lengths, members) = dense_words(n, &density, seed);
            let all: Vec<usize> = (0..n).collect();
            let options = Options { grid_min_postings: 0, inline_lengths_min_documents: u32::MAX, ..Options::default() };
            let blob = build_part(&docs, &lengths, &members, &all, &vec![false; n], options);
            let segment = Segment::parse(&blob).unwrap();
            let names: Vec<String> = (0..3).map(|t| format!("t{t}")).collect();
            for node in [
                Node::And(vec![Node::Term(0), Node::Term(1)]),
                Node::And(vec![Node::Term(0), Node::Term(1), Node::Term(2)]),
                Node::Or(vec![Node::Term(0), Node::Term(1)]),
                Node::Or(vec![Node::Term(0), Node::Term(1), Node::Term(2)]),
                Node::And(vec![Node::Term(0), Node::Or(vec![Node::Term(1), Node::Term(2)])]),
            ] {
                let scorers: Vec<(String, TermScorer)> = leaf_terms(&node)
                    .into_iter()
                    .map(|t| {
                        let df = members[t].len() as u64;
                        (names[t].clone(), TermScorer::from_statistics(n as u64, df, 1.0, Bm25Params::default(), 20.0).unwrap())
                    })
                    .collect();
                let mut want: Vec<(f32, Tid)> = (0..n)
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

        /// Disjunctions of up to five words, dense and sparse, whose groups'
        /// candidates hold some of the terms only.
        #[test]
        fn disjunctions_of_dense_and_sparse_words_keep_the_exact_top_rows(
            n in 3000usize..8000,
            density in prop::collection::vec(5u32..100, 5),
            seed in any::<u64>(),
            k in 1usize..12,
        ) {
            let (docs, lengths, members) = dense_words(n, &density, seed);
            let all: Vec<usize> = (0..n).collect();
            let options = Options { grid_min_postings: 0, inline_lengths_min_documents: u32::MAX, ..Options::default() };
            let blob = build_part(&docs, &lengths, &members, &all, &vec![false; n], options);
            let segment = Segment::parse(&blob).unwrap();
            let names: Vec<String> = (0..5).map(|t| format!("t{t}")).collect();
            for node in [
                Node::Or((0..3).map(Node::Term).collect()),
                Node::Or((0..5).map(Node::Term).collect()),
                Node::Or(vec![Node::Term(4), Node::Term(0), Node::Term(2)]),
            ] {
                let scorers: Vec<(String, TermScorer)> = leaf_terms(&node)
                    .into_iter()
                    .map(|t| {
                        let df = members[t].len().max(1) as u64;
                        (names[t].clone(), TermScorer::from_statistics(n as u64, df, 1.0, Bm25Params::default(), 20.0).unwrap())
                    })
                    .collect();
                let mut want: Vec<(f32, Tid)> = (0..n)
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

    /// Disjunctions of more than 64 words (as a pick of a candidate's
    /// unread terms from a u64 mask would have to handle): with 70 words,
    /// most in nearly every document, candidates hold more than 64, and the
    /// rows kept are a brute force's, bit for bit.
    #[test]
    fn disjunctions_of_more_than_64_words_keep_the_exact_top_rows() {
        let n = 2500;
        let density: Vec<u32> = (0..70)
            .map(|t| if t % 10 == 3 { 20 } else { 96 + t % 4 })
            .collect();
        let (docs, lengths, members) = dense_words(n, &density, 0x5DEE_CE66_D1CE_4E5B);
        let all: Vec<usize> = (0..n).collect();
        let options = Options {
            grid_min_postings: 0,
            inline_lengths_min_documents: u32::MAX,
            ..Options::default()
        };
        let blob = build_part(&docs, &lengths, &members, &all, &vec![false; n], options);
        let segment = Segment::parse(&blob).unwrap();
        let names: Vec<String> = (0..density.len()).map(|t| format!("t{t}")).collect();
        for terms in [70usize, 65, 64, 63] {
            let node = Node::Or((0..terms).map(Node::Term).collect());
            let scorers: Vec<(String, TermScorer)> = leaf_terms(&node)
                .into_iter()
                .map(|t| {
                    let df = members[t].len().max(1) as u64;
                    let scorer =
                        TermScorer::from_statistics(n as u64, df, 1.0, Bm25Params::default(), 20.0);
                    (names[t].clone(), scorer.unwrap())
                })
                .collect();
            let mut want: Vec<(f32, Tid)> = (0..n)
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
            for k in [1, 10] {
                let mut top = want.clone();
                top.truncate(k);
                let got = top_k(&segment, &node, &names, &scorers, k, &mut NoTouch).unwrap();
                let bits = |rows: &[(f32, Tid)]| {
                    rows.iter()
                        .map(|(s, t)| (s.to_bits(), *t))
                        .collect::<Vec<_>>()
                };
                assert_eq!(bits(&got.rows), bits(&top), "top {k} of {terms} words");
            }
        }
    }

    /// Two dense words, with the final rows kept from the start: a group's
    /// candidates are sieved by their buckets, read a chunk at a time from
    /// the TF tail, so far fewer TF reads are made than candidates bounded.
    #[test]
    fn dense_words_read_their_buckets_a_chunk_at_a_time() {
        let n = 6_000;
        let (docs, lengths, members) = dense_words(n, &[90, 80], 7);
        let all: Vec<usize> = (0..n).collect();
        let options = Options {
            grid_min_postings: 0,
            inline_lengths_min_documents: u32::MAX,
            ..Options::default()
        };
        let blob = build_part(&docs, &lengths, &members, &all, &vec![false; n], options);
        let segment = Segment::parse(&blob).unwrap();
        let names: Vec<String> = (0..2).map(|t| format!("t{t}")).collect();
        for node in [
            Node::And(vec![Node::Term(0), Node::Term(1)]),
            Node::Or(vec![Node::Term(0), Node::Term(1)]),
        ] {
            let scorers: Vec<(String, TermScorer)> = (0..2)
                .map(|t| {
                    let df = members[t].len() as u64;
                    let scorer =
                        TermScorer::from_statistics(n as u64, df, 1.0, Bm25Params::default(), 20.0)
                            .unwrap();
                    (names[t].clone(), scorer)
                })
                .collect();
            let k = 10;
            let got = top_k(&segment, &node, &names, &scorers, k, &mut NoTouch).unwrap();
            let mut seeded = crate::walk::TopRows::new(k, false);
            for row in &got.rows {
                seeded.push(crate::walk::Ranked(row.0, row.1));
            }
            let mut reads = TfReads::default();
            let walked = top_k_into(
                &segment,
                &node,
                &names,
                &scorers,
                &mut seeded,
                &mut AllVisible,
                &mut reads,
            )
            .unwrap();
            assert!(
                walked.candidates > 1_000,
                "{node:?}: {} candidates",
                walked.candidates
            );
            assert!(
                reads.0 * 4 < walked.candidates,
                "{node:?}: {} TF reads for {} candidates",
                reads.0,
                walked.candidates
            );
        }
    }

    struct AllVisible;

    impl crate::walk::Visibility for AllVisible {
        fn visible(&mut self, _: Tid) -> bool {
            true
        }
    }
}
