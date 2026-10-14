// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Research round 2's offline gates N1 and N3 (a lab census, no format
//! change): what finer summaries would have saved the walks the replay
//! records.
//!
//! The walk reports, through [`engine::tinshape::Touch`]'s census hooks,
//! each group a led walk intersected (its required terms and what each
//! left), each group a disjunction reached with its threshold, and each DL
//! read with the shortest length that would have cut it. Here those events
//! are replayed against the segments' postings:
//!
//! - **N1** (presence masks): per intersected group, the lead's mask from
//!   its members ANDed with the other required terms' stored masks, at
//!   page grain (256, 128 and 64 bits a group) and slot-block grain (16
//!   and 64 slots a bit), a mask stored for non-grid entries of at least
//!   `T` members. A group whose AND is empty is ruled out before any other
//!   term's container is loaded. Masks are supersets of the members, so a
//!   group ruled out always ended empty.
//! - **N3(a)** (impact tiers): a group-grain MaxScore model of the
//!   disjunction walk, under the threshold the walk had on reaching the
//!   group, with each grouped term's postings split into a high and a low
//!   tier (per group or per segment top p%, or tf >= 2), each tier bounded
//!   by its best posting. Tighter bounds never change which documents
//!   enter the heap, or when, so the threshold trajectory holds.
//! - **N3(c)** (length classes): the DL reads, with the shortest length
//!   that cuts each; written out for a scheme search.

#![allow(clippy::needless_range_loop)]

use std::cell::RefCell;
use std::io::Write;

use engine::bm25::TermScorer;
use rustc_hash::FxHashMap;
use segment::tf_bucket::TfBucket;
use segment::tinshape::postings::{Form, KIND_GRID, for_each_local};
use segment::tinshape::segment::Segment;

/// A census event, with the segment (index into the dump's) it came from.
pub enum Event {
    And {
        seg: usize,
        g: u32,
        req: Vec<usize>,
        left: Vec<u32>,
        grids: bool,
    },
    Or {
        seg: usize,
        g: u32,
        theta: Option<f32>,
        present: Vec<usize>,
    },
    Len {
        seg: usize,
        g: u32,
        length: u32,
        need: u32,
        at: usize,
        held: Vec<usize>,
    },
}

thread_local! {
    pub static EVENTS: RefCell<Vec<Event>> = const { RefCell::new(Vec::new()) };
}

pub fn push(event: Event) {
    EVENTS.with_borrow_mut(|e| e.push(event));
}

pub fn take() -> Vec<Event> {
    EVENTS.with_borrow_mut(std::mem::take)
}

/// Mask forms: page masks of 256, 128 and 64 bits a group, slot-block
/// masks of 16 and 64 slots a bit.
pub const FORMS: [&str; 5] = ["page256", "page128", "page64", "slot16", "slot64"];
const NF: usize = FORMS.len();
/// Least members of a non-grid entry that stores a mask.
pub const THRESHOLDS: [u32; 5] = [1, 4, 8, 16, 32];
const NT: usize = THRESHOLDS.len();
/// Which terms' masks are ANDed: the lead's and the second term's only, or
/// every required term's.
pub const SCOPES: [&str; 2] = ["lead+second", "all"];

/// Kind of a sparse record's group (no directory entry).
const SPARSE: u8 = 255;

/// One term's members in one group.
struct TermGroup {
    count: u32,
    kind: u8,
    len: u32,
    /// Page bits (256), slot16 bits, slot64 bits.
    pages: [u64; 4],
    slot16: Vec<u64>,
    slot64: Vec<u64>,
    /// Its locals, for at most [`LOCALS_MAX`] members.
    locals: Vec<u32>,
    /// Where its container lies in the blob (none for a sparse record).
    blob_at: usize,
}

const LOCALS_MAX: u32 = 256;

#[derive(Default)]
struct TermData {
    groups: FxHashMap<u32, TermGroup>,
}

fn fold(pages: &[u64; 4], per_bit: u32) -> [u64; 4] {
    if per_bit == 1 {
        return *pages;
    }
    let mut out = [0u64; 4];
    for p in 0..256u32 {
        if pages[(p / 64) as usize] >> (p % 64) & 1 == 1 {
            let q = p / per_bit;
            out[(q / 64) as usize] |= 1 << (q % 64);
        }
    }
    out
}

impl TermGroup {
    fn new(kind: u8, len: u32, slots: u32) -> Self {
        Self {
            count: 0,
            kind,
            len,
            pages: [0; 4],
            slot16: vec![0; (slots.div_ceil(16) as usize).div_ceil(64)],
            slot64: vec![0; (slots.div_ceil(64) as usize).div_ceil(64)],
            locals: Vec::new(),
            blob_at: usize::MAX,
        }
    }

    fn add(&mut self, local: u32, page: usize) {
        self.count += 1;
        self.pages[page / 64] |= 1 << (page % 64);
        let b = local / 16;
        self.slot16[(b / 64) as usize] |= 1 << (b % 64);
        let b = local / 64;
        self.slot64[(b / 64) as usize] |= 1 << (b % 64);
        if self.locals.len() < LOCALS_MAX as usize {
            self.locals.push(local);
        }
    }

    /// Mask of form `f` as words.
    fn mask(&self, f: usize) -> Vec<u64> {
        match f {
            0 => self.pages.to_vec(),
            1 => fold(&self.pages, 2).to_vec(),
            2 => fold(&self.pages, 4).to_vec(),
            3 => self.slot16.clone(),
            _ => self.slot64.clone(),
        }
    }

    fn holds(f: usize, mask: &[u64], local: u32, page: usize) -> bool {
        let b = match f {
            0 => page as u32,
            1 => page as u32 / 2,
            2 => page as u32 / 4,
            3 => local / 16,
            _ => local / 64,
        };
        mask[(b / 64) as usize] >> (b % 64) & 1 == 1
    }
}

fn term_data(segment: &Segment<'_>, name: &str) -> TermData {
    let mut data = TermData::default();
    let geometry = &segment.docs.geometry;
    let Some(term) = segment.term(name).expect("a term") else {
        return data;
    };
    let p = &term.postings;
    match &p.form {
        Form::Grouped(dir) => {
            for b in 0..dir.len() {
                let entry = dir.get(b, &geometry.groups).expect("an entry");
                let g = entry.index as usize;
                let group = &geometry.groups[g];
                let mut tg = TermGroup::new(entry.kind, entry.len, group.slots());
                tg.blob_at = term.at + p.payload_at + p.containers_at + entry.at as usize;
                let bytes = p.container(&entry).all().expect("a container");
                for_each_local(&entry, bytes, geometry.starts(g), |local| {
                    tg.add(local, geometry.page_of(g, local));
                })
                .expect("members");
                data.groups.insert(entry.index, tg);
            }
        }
        _ => {
            p.for_each_slot(geometry, |slot| {
                let g = geometry.group_of_slot(slot);
                let group = &geometry.groups[g];
                let local = slot - group.slot_base;
                data.groups
                    .entry(g as u32)
                    .or_insert_with(|| TermGroup::new(SPARSE, 0, group.slots()))
                    .add(local, geometry.page_of(g, local));
            })
            .expect("members");
        }
    }
    data
}

/// N1's tallies for one style.
#[derive(Default, Clone)]
pub struct AndStats {
    pub queries: u64,
    pub intersected: u64,
    pub grid_groups: u64,
    pub empty_final: u64,
    pub empty_after2: u64,
    /// Of those, the second term had no entry (the directory ruled the
    /// group out; nothing was loaded).
    pub absent2: u64,
    /// Empty after the second term with its container loaded.
    pub empty2_loaded: u64,
    pub second_members_loaded: u64,
    pub second_bytes_loaded: u64,
    pub later_loads: u64,
    /// Per scope, form, threshold.
    pub ruled: [[[u64; NT]; NF]; 2],
    pub ruled_loaded2: [[[u64; NT]; NF]; 2],
    pub members2_avoided: [[[u64; NT]; NF]; 2],
    pub bytes2_avoided: [[[u64; NT]; NF]; 2],
    pub later_avoided: [[[u64; NT]; NF]; 2],
    /// Masks read (one per term ANDed beyond the lead).
    pub masks_read: [[[u64; NT]; NF]; 2],
    /// In groups not ruled out: lead members, and those on surviving bits.
    pub lead_before: [[[u64; NT]; NF]; 2],
    pub lead_after: [[[u64; NT]; NF]; 2],
    /// Groups ruled out by a later required term's absence alone (its
    /// entry looked up with the masks, scope "all").
    pub presence_ruled: [[[u64; NT]; NF]; 2],
    /// Distinct payload pages of the grouped containers the walk loaded,
    /// summed over queries, and those left once ruled-out groups load
    /// only their lead.
    pub pages_before: u64,
    pub pages_after: [[[u64; NT]; NF]; 2],
    pub violations: u64,
}

/// A combination of scope, form and threshold, as a bit.
fn combo(scope: usize, f: usize, ti: usize) -> usize {
    (scope * NF + f) * NT + ti
}

/// N3's model counts for one variant.
#[derive(Default, Clone, Copy)]
pub struct OrCounts {
    pub groups: u64,
    pub worked: u64,
    pub term_loads: u64,
    pub essential_postings: u64,
    pub live_words: u64,
    pub passing_words: u64,
    pub weighed: u64,
    pub kept: u64,
}

pub const OR_VARIANTS: [&str; 11] = [
    "today",
    "grp-top1",
    "grp-top5",
    "grp-top10",
    "grp-top20",
    "seg-top1",
    "seg-top5",
    "seg-top10",
    "seg-top20",
    "tf>=2",
    "oracle",
];
const NV: usize = OR_VARIANTS.len();
const PCTS: [f64; 4] = [0.01, 0.05, 0.10, 0.20];

#[derive(Default, Clone)]
pub struct OrStats {
    pub queries: u64,
    pub counts: [OrCounts; NV],
    /// Postings of grouped terms split, and all postings, in the groups
    /// reached.
    pub split_postings: u64,
}

/// A query term's postings in one segment, with impacts.
struct Impacts {
    grouped: bool,
    /// Per group, its range in the arrays.
    ranges: FxHashMap<u32, (usize, usize)>,
    locals: Vec<u32>,
    impact: Vec<f32>,
    bucket: Vec<u8>,
    length: Vec<u32>,
    /// Per segment top p cutoffs (an impact at or above is high).
    seg_cut: [f32; 4],
    scorer: TermScorer,
}

fn impacts(segment: &Segment<'_>, name: &str, scorer: &TermScorer) -> Option<Impacts> {
    let term = segment.term(name).expect("a term")?;
    let geometry = &segment.docs.geometry;
    let p = &term.postings;
    let mut out = Impacts {
        grouped: matches!(p.form, Form::Grouped(_)),
        ranges: FxHashMap::default(),
        locals: Vec::with_capacity(term.entry.df as usize),
        impact: Vec::with_capacity(term.entry.df as usize),
        bucket: Vec::with_capacity(term.entry.df as usize),
        length: Vec::with_capacity(term.entry.df as usize),
        seg_cut: [f32::INFINITY; 4],
        scorer: scorer.clone(),
    };
    let push = |out: &mut Impacts, g: usize, local: u32, bucket: u8| {
        let rank = segment.docs.rank_in(g, local).expect("a document");
        let length = segment.lengths.get(rank).expect("a length");
        let b = TfBucket::new(bucket).expect("a bucket");
        out.locals.push(local);
        out.impact.push(scorer.bound_through(b, length));
        out.bucket.push(bucket);
        out.length.push(length);
    };
    match &p.form {
        Form::Grouped(dir) => {
            for b in 0..dir.len() {
                let entry = dir.get(b, &geometry.groups).expect("an entry");
                let g = entry.index as usize;
                let from = out.locals.len();
                let bytes = p.container(&entry).all().expect("a container");
                let mut locals = Vec::with_capacity(entry.count as usize);
                for_each_local(&entry, bytes, geometry.starts(g), |l| locals.push(l))
                    .expect("members");
                for (i, l) in locals.into_iter().enumerate() {
                    let bucket = p
                        .bucket_in(&entry, entry.first + i as u32)
                        .expect("a bucket");
                    push(&mut out, g, l, bucket);
                }
                out.ranges.insert(entry.index, (from, out.locals.len()));
            }
        }
        _ => {
            let footer = match &p.form {
                Form::Sparse(_) => Some(
                    p.footer(
                        geometry,
                        segment.block_size,
                        term.entry.max_tf_bucket,
                        segment.adaptive_tf,
                    )
                    .expect("a footer"),
                ),
                _ => None,
            };
            let slots = p.slots(geometry).expect("slots");
            let mut open: Option<(u32, usize)> = None;
            for (i, slot) in slots.into_iter().enumerate() {
                let g = geometry.group_of_slot(slot);
                let local = slot - geometry.groups[g].slot_base;
                let bucket = match &footer {
                    Some(f) => f.bucket(p.tf, i as u32).expect("a bucket"),
                    None => term.entry.max_tf_bucket,
                };
                if open.is_none_or(|(og, _)| og != g as u32) {
                    if let Some((og, from)) = open {
                        out.ranges.insert(og, (from, out.locals.len()));
                    }
                    open = Some((g as u32, out.locals.len()));
                }
                push(&mut out, g, local, bucket);
            }
            if let Some((og, from)) = open {
                out.ranges.insert(og, (from, out.locals.len()));
            }
        }
    }
    if out.grouped && !out.impact.is_empty() {
        let mut sorted = out.impact.clone();
        sorted.sort_unstable_by(|a, b| b.total_cmp(a));
        for (k, pct) in PCTS.iter().enumerate() {
            let h = ((sorted.len() as f64 * pct).ceil() as usize).clamp(1, sorted.len());
            out.seg_cut[k] = sorted[h - 1];
        }
    }
    Some(out)
}

/// The census of one run (one ranking mode).
#[derive(Default)]
pub struct Census {
    pub and: FxHashMap<String, AndStats>,
    pub or: FxHashMap<String, OrStats>,
    /// Per style, DL read records (query, segment, page, length, need).
    pub len: FxHashMap<String, Vec<[u32; 6]>>,
    /// One query's loaded containers: segment, first and last page, and
    /// the combinations ([`combo`]) that avoid it.
    containers: Vec<(u32, u32, u32, u64)>,
    /// Per style, DL reads by the smallest (term, group) list the
    /// candidate holds: [<= 62, >= 63] x [reads, cut at its own length].
    pub len_lists: FxHashMap<String, [[u64; 2]; 2]>,
    /// Per segment and term, its members by group (N1).
    cache: FxHashMap<(usize, String), TermData>,
    /// Scratch for the OR model.
    doc_bound: Vec<f32>,
    doc_ess: Vec<bool>,
    touched: Vec<u32>,
    word_bound: Vec<f32>,
    word_live: Vec<bool>,
    /// Groups of OR queries skipped to bound the run: every `or_every`th
    /// query is modelled.
    pub or_every: usize,
    pub queries_seen: usize,
}

impl Census {
    /// A census modelling every `or_every`th disjunction.
    pub fn new(or_every: usize) -> Self {
        Self {
            or_every,
            ..Default::default()
        }
    }

    /// Takes one query's events.
    pub fn query(
        &mut self,
        q: usize,
        style: &str,
        events: Vec<Event>,
        names: &[String],
        scorer: &[(String, TermScorer)],
        segments: &[Segment<'static>],
    ) {
        self.queries_seen += 1;
        let mut and_seen = false;
        let mut or_events: Vec<(usize, u32, Option<f32>, Vec<usize>)> = Vec::new();
        for e in events {
            match e {
                Event::And {
                    seg,
                    g,
                    req,
                    left,
                    grids,
                } => {
                    if !and_seen {
                        self.and.entry(style.to_owned()).or_default().queries += 1;
                        and_seen = true;
                    }
                    self.and_group(style, seg, g, &req, &left, grids, names, segments);
                }
                Event::Or {
                    seg,
                    g,
                    theta,
                    present,
                } => or_events.push((seg, g, theta, present)),
                Event::Len {
                    seg,
                    g,
                    length,
                    need,
                    at,
                    held,
                } => {
                    let page = (at / crate::paged::PAGE_DATA) as u32;
                    let mut least = u32::MAX;
                    for &t in &held {
                        let data = self
                            .cache
                            .entry((seg, names[t].clone()))
                            .or_insert_with(|| term_data(&segments[seg], &names[t]));
                        if let Some(tg) = data.groups.get(&g) {
                            least = least.min(tg.count);
                        }
                    }
                    let lists = self.len_lists.entry(style.to_owned()).or_default();
                    let small = usize::from(least > 62);
                    lists[small][0] += 1;
                    lists[small][1] += u64::from(need <= length);
                    self.len
                        .entry(style.to_owned())
                        .or_default()
                        .push([q as u32, seg as u32, page, length, need, least]);
                }
            }
        }
        if and_seen {
            self.and_pages(style);
        }
        if !or_events.is_empty() && q.is_multiple_of(self.or_every.max(1)) {
            self.or_query(style, &or_events, names, scorer, segments);
        }
    }

    /// Distinct payload pages of one query's loaded containers, before
    /// and per combination after.
    fn and_pages(&mut self, style: &str) {
        let st = self.and.entry(style.to_owned()).or_default();
        let mut pages: rustc_hash::FxHashSet<(u32, u32)> = Default::default();
        for &(seg, first, last, _) in &self.containers {
            for p in first..=last {
                pages.insert((seg, p));
            }
        }
        st.pages_before += pages.len() as u64;
        for scope in 0..2 {
            for f in 0..NF {
                for ti in 0..NT {
                    let bit = 1u64 << combo(scope, f, ti);
                    pages.clear();
                    for &(seg, first, last, avoided) in &self.containers {
                        if avoided & bit == 0 {
                            for p in first..=last {
                                pages.insert((seg, p));
                            }
                        }
                    }
                    st.pages_after[scope][f][ti] += pages.len() as u64;
                }
            }
        }
        self.containers.clear();
    }

    #[allow(clippy::too_many_arguments)]
    fn and_group(
        &mut self,
        style: &str,
        seg: usize,
        g: u32,
        req: &[usize],
        left: &[u32],
        grids: bool,
        names: &[String],
        segments: &[Segment<'static>],
    ) {
        for &t in req {
            self.cache
                .entry((seg, names[t].clone()))
                .or_insert_with(|| term_data(&segments[seg], &names[t]));
        }
        let st = self.and.entry(style.to_owned()).or_default();
        let tg: Vec<Option<&TermGroup>> = req
            .iter()
            .map(|&t| self.cache[&(seg, names[t].clone())].groups.get(&g))
            .collect();
        // The containers the walk loaded: the lead's; another term's while
        // candidates were left and it holds the group (all, for grids).
        let loaded: Vec<bool> = (0..req.len())
            .map(|r| tg[r].is_some() && (grids || r == 0 || left[r - 1] > 0))
            .collect();
        let mut avoided = vec![0u64; req.len()];
        let containers = &mut self.containers;
        let mut emit = |avoided: &[u64]| {
            for (r, t) in tg.iter().enumerate() {
                if let Some(t) = t
                    && loaded[r]
                    && t.blob_at != usize::MAX
                {
                    let first = (t.blob_at / crate::paged::PAGE_DATA) as u32;
                    let last = ((t.blob_at + (t.len as usize).max(1) - 1) / crate::paged::PAGE_DATA)
                        as u32;
                    containers.push((seg as u32, first, last, avoided[r]));
                }
            }
        };
        if grids {
            st.grid_groups += 1;
            emit(&avoided);
            return;
        }
        st.intersected += 1;
        let empty_final = *left.last().expect("a lead") == 0;
        st.empty_final += u64::from(empty_final);
        let lead = tg[0].expect("the lead holds the group");
        if req.len() < 2 {
            emit(&avoided);
            return;
        }
        let empty2 = left[1] == 0;
        st.empty_after2 += u64::from(empty2);
        let loaded2 = loaded[1];
        if empty2 && tg[1].is_none() {
            st.absent2 += 1;
        }
        if empty2 && loaded2 {
            st.empty2_loaded += 1;
        }
        if loaded2 {
            let t1 = tg[1].expect("loaded");
            st.second_members_loaded += u64::from(t1.count);
            st.second_bytes_loaded += u64::from(t1.len);
        }
        for r in 2..req.len() {
            st.later_loads += u64::from(loaded[r]);
        }
        let geometry = &segments[seg].docs.geometry;
        for scope in 0..2 {
            let upto = if scope == 0 { 2 } else { req.len() };
            for f in 0..NF {
                let lead_mask = lead.mask(f);
                for (ti, &thr) in THRESHOLDS.iter().enumerate() {
                    if tg[1].is_none() {
                        // The second term's directory rules the group out
                        // today, before any container.
                        continue;
                    }
                    let mut mask = lead_mask.clone();
                    let mut read = 0u64;
                    let mut absent = false;
                    for t in tg.iter().take(upto).skip(1) {
                        let Some(t) = t else {
                            // A later term without an entry, looked up with
                            // the masks.
                            absent = true;
                            break;
                        };
                        if t.kind == KIND_GRID || (t.kind != SPARSE && t.count < thr) {
                            continue;
                        }
                        read += 1;
                        for (m, w) in mask.iter_mut().zip(t.mask(f)) {
                            *m &= w;
                        }
                    }
                    st.masks_read[scope][f][ti] += read;
                    if absent || mask.iter().all(|w| *w == 0) {
                        st.ruled[scope][f][ti] += 1;
                        if mask.iter().any(|w| *w != 0) {
                            st.presence_ruled[scope][f][ti] += 1;
                        }
                        if !empty_final {
                            st.violations += 1;
                        }
                        if loaded2 {
                            let t1 = tg[1].expect("loaded");
                            st.ruled_loaded2[scope][f][ti] += 1;
                            st.members2_avoided[scope][f][ti] += u64::from(t1.count);
                            st.bytes2_avoided[scope][f][ti] += u64::from(t1.len);
                        }
                        for r in 1..req.len() {
                            if loaded[r] {
                                avoided[r] |= 1 << combo(scope, f, ti);
                                if r >= 2 {
                                    st.later_avoided[scope][f][ti] += 1;
                                }
                            }
                        }
                    } else if lead.count <= LOCALS_MAX {
                        st.lead_before[scope][f][ti] += u64::from(lead.count);
                        st.lead_after[scope][f][ti] += lead
                            .locals
                            .iter()
                            .filter(|&&l| {
                                TermGroup::holds(f, &mask, l, geometry.page_of(g as usize, l))
                            })
                            .count() as u64;
                    }
                }
            }
        }
        emit(&avoided);
    }

    fn or_query(
        &mut self,
        style: &str,
        events: &[(usize, u32, Option<f32>, Vec<usize>)],
        names: &[String],
        scorer: &[(String, TermScorer)],
        segments: &[Segment<'static>],
    ) {
        let mut st = self.or.remove(style).unwrap_or_default();
        st.queries += 1;
        let mut data: FxHashMap<(usize, usize), Option<Impacts>> = FxHashMap::default();
        let mut last_seg = usize::MAX;
        for (seg, g, theta, present) in events {
            if *seg != last_seg {
                data.clear();
                last_seg = *seg;
            }
            for &t in present {
                if let Some((_, sc)) = scorer.iter().find(|(n, _)| *n == names[t]) {
                    data.entry((*seg, t))
                        .or_insert_with(|| impacts(&segments[*seg], &names[t], sc));
                }
            }
            let mut terms: Vec<(&Impacts, usize, usize)> = Vec::new();
            for &t in present {
                if let Some(Some(d)) = data.get(&(*seg, t))
                    && let Some(&(a, b)) = d.ranges.get(g)
                {
                    terms.push((d, a, b));
                }
            }
            if terms.is_empty() {
                continue;
            }
            let slots = segments[*seg].docs.geometry.groups[*g as usize].slots() as usize;
            for &(d, a, b) in &terms {
                if d.grouped {
                    st.split_postings += (b - a) as u64;
                }
            }
            for v in 0..NV {
                let c = self.or_group(&terms, v, *theta, slots);
                let s = &mut st.counts[v];
                s.groups += 1;
                s.worked += c.worked;
                s.term_loads += c.term_loads;
                s.essential_postings += c.essential_postings;
                s.live_words += c.live_words;
                s.passing_words += c.passing_words;
                s.weighed += c.weighed;
                s.kept += c.kept;
            }
        }
        self.or.insert(style.to_owned(), st);
    }

    /// The group-grain MaxScore model of one group under variant `v`.
    fn or_group(
        &mut self,
        terms: &[(&Impacts, usize, usize)],
        v: usize,
        theta: Option<f32>,
        slots: usize,
    ) -> OrCounts {
        let mut c = OrCounts::default();
        // Per term: per posting its bound (by tier), and its pseudo-terms'
        // bounds (hi, lo) with whether each has postings.
        let mut bounds: Vec<Vec<f32>> = Vec::with_capacity(terms.len());
        let mut tiers: Vec<Vec<bool>> = Vec::with_capacity(terms.len());
        let mut pseudo: Vec<(f32, f32, bool, bool)> = Vec::with_capacity(terms.len());
        for &(d, a, b) in terms {
            let imp = &d.impact[a..b];
            let mut tier_hi: Vec<bool> = vec![true; b - a];
            let split = d.grouped && (1..=9).contains(&v);
            if split {
                match v {
                    1..=4 => {
                        let n = b - a;
                        let h = ((n as f64 * PCTS[v - 1]).ceil() as usize).clamp(1, n);
                        let mut order: Vec<usize> = (0..n).collect();
                        order.sort_unstable_by(|x, y| imp[*y].total_cmp(&imp[*x]).then(x.cmp(y)));
                        tier_hi.fill(false);
                        for &i in &order[..h] {
                            tier_hi[i] = true;
                        }
                    }
                    5..=8 => {
                        let cut = d.seg_cut[v - 5];
                        for (i, x) in imp.iter().enumerate() {
                            tier_hi[i] = *x >= cut;
                        }
                    }
                    _ => {
                        for (i, bk) in d.bucket[a..b].iter().enumerate() {
                            tier_hi[i] = *bk >= 1;
                        }
                    }
                }
            }
            let (mut hb, mut lb, mut has_hi, mut has_lo) = (0.0_f32, 0.0_f32, false, false);
            for (i, x) in imp.iter().enumerate() {
                if tier_hi[i] {
                    hb = hb.max(*x);
                    has_hi = true;
                } else {
                    lb = lb.max(*x);
                    has_lo = true;
                }
            }
            if split && v == 9 && has_lo {
                // No stored lo frontier: tf 1 at the group's shortest
                // document, which the entry's frontier already knows.
                let shortest = d.length[a..b].iter().copied().min().unwrap_or(0);
                let b0 = TfBucket::new(0).expect("bucket 0");
                lb = lb.max(d.scorer.bound_through(b0, shortest));
            }
            let per: Vec<f32> = if v == 10 {
                imp.to_vec()
            } else {
                tier_hi.iter().map(|h| if *h { hb } else { lb }).collect()
            };
            bounds.push(per);
            tiers.push(tier_hi);
            pseudo.push((hb, lb, has_hi, has_lo));
        }
        // Group bound: per term its best.
        let total: f32 = pseudo
            .iter()
            .map(|&(hb, lb, hh, hl)| {
                let mut m = 0.0_f32;
                if hh {
                    m = m.max(hb);
                }
                if hl {
                    m = m.max(lb);
                }
                m
            })
            .sum();
        let theta = theta.unwrap_or(f32::NEG_INFINITY);
        if total <= theta {
            return c;
        }
        c.worked = 1;
        // MaxScore at pseudo-term grain: the weakest pseudo-terms whose
        // bounds sum to at most theta are non-essential. The oracle keeps
        // today's partition (its gain is per posting).
        let mut ps: Vec<(f32, usize, bool)> = Vec::new();
        for (t, &(hb, lb, hh, hl)) in pseudo.iter().enumerate() {
            if v == 10 || v == 0 {
                // Unsplit: one pseudo-term a term (every posting hi).
                ps.push((hb, t, true));
                continue;
            }
            if hh {
                ps.push((hb, t, true));
            }
            if hl {
                ps.push((lb, t, false));
            }
        }
        ps.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut essential = vec![(false, false); terms.len()];
        // A document holds one tier of a term, so the non-essential set
        // bounds a document by each term's best tier in it: weakest first,
        // a term's hi tier adds only what it scores above its lo tier.
        let mut sum = 0.0_f32;
        let mut taken = vec![0.0_f32; terms.len()];
        let mut closed = false;
        for &(bound, t, hi) in &ps {
            if !closed {
                sum += bound - taken[t];
                taken[t] = bound;
                closed = sum > theta;
            }
            if closed {
                if v == 10 || v == 0 {
                    essential[t] = (true, true);
                } else if hi {
                    essential[t].0 = true;
                } else {
                    essential[t].1 = true;
                }
            }
        }
        let words = slots.div_ceil(64);
        if self.doc_bound.len() < slots {
            self.doc_bound.resize(slots, 0.0);
            self.doc_ess.resize(slots, false);
        }
        if self.word_bound.len() < words {
            self.word_bound.resize(words, 0.0);
            self.word_live.resize(words, false);
        }
        self.touched.clear();
        for (k, &(d, a, b)) in terms.iter().enumerate() {
            let per = &bounds[k];
            let tier = &tiers[k];
            let mut ess_any = false;
            let mut word = usize::MAX;
            let mut wmax = 0.0_f32;
            for (i, &l) in d.locals[a..b].iter().enumerate() {
                let w = l as usize / 64;
                if w != word {
                    if word != usize::MAX {
                        self.word_bound[word] += wmax;
                    }
                    word = w;
                    wmax = 0.0;
                }
                let bd = per[i];
                wmax = wmax.max(bd);
                let ess = if tier[i] {
                    essential[k].0
                } else {
                    essential[k].1
                };
                if ess {
                    ess_any = true;
                    c.essential_postings += 1;
                    self.word_live[w] = true;
                    self.doc_ess[l as usize] = true;
                }
                if self.doc_bound[l as usize] == 0.0 {
                    self.touched.push(l);
                }
                self.doc_bound[l as usize] += bd.max(f32::MIN_POSITIVE);
            }
            if word != usize::MAX {
                self.word_bound[word] += wmax;
            }
            c.term_loads += u64::from(ess_any);
        }
        for w in 0..words {
            if self.word_live[w] {
                c.live_words += 1;
                if self.word_bound[w] > theta {
                    c.passing_words += 1;
                }
            }
        }
        for &l in &self.touched {
            let l = l as usize;
            if self.doc_ess[l] && self.word_live[l / 64] && self.word_bound[l / 64] > theta {
                c.weighed += 1;
                if self.doc_bound[l] > theta {
                    c.kept += 1;
                }
            }
            self.doc_bound[l] = 0.0;
            self.doc_ess[l] = false;
        }
        for w in 0..words {
            self.word_bound[w] = 0.0;
            self.word_live[w] = false;
        }
        c
    }

    /// Writes the tallies under `dir` with `tag` (the ranking mode).
    pub fn write(&self, dir: &std::path::Path, tag: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let mut out = String::new();
        use std::fmt::Write as _;
        let _ = writeln!(
            out,
            "style\tscope\tform\tmin_count\tqueries\tintersected\tgrid_groups\tempty_final\tempty_after2\tabsent2\tempty2_loaded\tsecond_members_loaded\tsecond_bytes_loaded\tlater_loads\truled\truled_loaded2\tmembers2_avoided\tbytes2_avoided\tlater_avoided\tmasks_read\tlead_before\tlead_after\tpresence_ruled\tpages_before\tpages_after\tviolations"
        );
        let mut styles: Vec<&String> = self.and.keys().collect();
        styles.sort();
        for style in styles {
            let s = &self.and[style];
            for scope in 0..2 {
                for f in 0..NF {
                    for (ti, thr) in THRESHOLDS.iter().enumerate() {
                        let _ = writeln!(
                            out,
                            "{style}\t{}\t{}\t{thr}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                            SCOPES[scope],
                            FORMS[f],
                            s.queries,
                            s.intersected,
                            s.grid_groups,
                            s.empty_final,
                            s.empty_after2,
                            s.absent2,
                            s.empty2_loaded,
                            s.second_members_loaded,
                            s.second_bytes_loaded,
                            s.later_loads,
                            s.ruled[scope][f][ti],
                            s.ruled_loaded2[scope][f][ti],
                            s.members2_avoided[scope][f][ti],
                            s.bytes2_avoided[scope][f][ti],
                            s.later_avoided[scope][f][ti],
                            s.masks_read[scope][f][ti],
                            s.lead_before[scope][f][ti],
                            s.lead_after[scope][f][ti],
                            s.presence_ruled[scope][f][ti],
                            s.pages_before,
                            s.pages_after[scope][f][ti],
                            s.violations,
                        );
                    }
                }
            }
        }
        std::fs::write(dir.join(format!("n1-{tag}.tsv")), &out)?;
        let mut out = String::from(
            "style\tvariant\tqueries\tsplit_postings\tgroups\tworked\tterm_loads\tessential_postings\tlive_words\tpassing_words\tweighed\tkept\n",
        );
        let mut styles: Vec<&String> = self.or.keys().collect();
        styles.sort();
        for style in styles {
            let s = &self.or[style];
            for (v, name) in OR_VARIANTS.iter().enumerate() {
                let c = &s.counts[v];
                let _ = writeln!(
                    out,
                    "{style}\t{name}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    s.queries,
                    s.split_postings,
                    c.groups,
                    c.worked,
                    c.term_loads,
                    c.essential_postings,
                    c.live_words,
                    c.passing_words,
                    c.weighed,
                    c.kept
                );
            }
        }
        std::fs::write(dir.join(format!("n3a-{tag}.tsv")), &out)?;
        let mut out = String::from(
            "style\tsmall_reads\tsmall_cut_at_length\tlarge_reads\tlarge_cut_at_length\n",
        );
        let mut styles: Vec<&String> = self.len_lists.keys().collect();
        styles.sort();
        for style in styles {
            let l = &self.len_lists[style];
            let _ = writeln!(
                out,
                "{style}\t{}\t{}\t{}\t{}",
                l[0][0], l[0][1], l[1][0], l[1][1]
            );
        }
        std::fs::write(dir.join(format!("n3c-lists-{tag}.tsv")), &out)?;
        for (style, recs) in &self.len {
            let mut f = std::io::BufWriter::new(std::fs::File::create(
                dir.join(format!("n3c-{tag}-{style}.bin")),
            )?);
            for r in recs {
                for x in r {
                    f.write_all(&x.to_le_bytes())?;
                }
            }
        }
        Ok(())
    }
}

/// The index-wide size side of N1 and N3: per segment, non-grid entries by
/// member count, mask bytes per form and threshold, the directory and
/// index bytes, grouped postings, and the document length histogram.
pub fn sizes(
    segments: &[Segment<'static>],
    blob_bytes: u64,
    dir: &std::path::Path,
) -> std::io::Result<()> {
    use std::fmt::Write as _;
    // Per threshold: entries, page-mask bytes (32/16/8), slot16 and slot64
    // bytes (each rounded up to whole bytes).
    let mut entries = [0u64; NT];
    let mut bytes = [[0u64; NF]; NT];
    let (mut all_entries, mut grid_entries, mut directory, mut grouped_postings, mut postings) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut kinds = [0u64; 4];
    let mut length_hist: FxHashMap<u32, u64> = FxHashMap::default();
    let mut documents = 0u64;
    for segment in segments {
        let geometry = &segment.docs.geometry;
        documents += u64::from(segment.documents);
        for rank in 0..segment.documents {
            *length_hist
                .entry(segment.lengths.get(rank).expect("a length"))
                .or_default() += 1;
        }
        for item in segment.dictionary().iter() {
            let (_, entry) = item.expect("a term");
            postings += u64::from(entry.df);
            let found = segment.resolve(entry).expect("a record");
            let p = &found.postings;
            if let Form::Grouped(dir) = &p.form {
                grouped_postings += u64::from(entry.df);
                directory += p.containers_at as u64;
                for e in dir.entries(&geometry.groups).expect("a directory").iter() {
                    all_entries += 1;
                    kinds[usize::from(e.kind).min(3)] += 1;
                    if e.kind == KIND_GRID {
                        grid_entries += 1;
                        continue;
                    }
                    let slots = geometry.groups[e.index as usize].slots();
                    for (ti, thr) in THRESHOLDS.iter().enumerate() {
                        if e.count >= *thr {
                            entries[ti] += 1;
                            bytes[ti][0] += 32;
                            bytes[ti][1] += 16;
                            bytes[ti][2] += 8;
                            bytes[ti][3] += u64::from(slots.div_ceil(16).div_ceil(8));
                            bytes[ti][4] += u64::from(slots.div_ceil(64).div_ceil(8));
                        }
                    }
                }
            }
        }
        eprintln!("sizes: a segment of {} documents done", segment.documents);
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "index_bytes\t{blob_bytes}\ndocuments\t{documents}\npostings\t{postings}\ngrouped_postings\t{grouped_postings}\ndirectory_bytes\t{directory}\nentries\t{all_entries}\ngrid_entries\t{grid_entries}\nkinds_grid_ef_paged_array\t{}\t{}\t{}\t{}",
        kinds[0], kinds[1], kinds[2], kinds[3]
    );
    let _ = writeln!(out, "min_count\tentries\t{}", FORMS.join("\t"));
    for (ti, thr) in THRESHOLDS.iter().enumerate() {
        let _ = write!(out, "{thr}\t{}", entries[ti]);
        for f in 0..NF {
            let _ = write!(out, "\t{}", bytes[ti][f]);
        }
        let _ = writeln!(out);
    }
    std::fs::write(dir.join("sizes.tsv"), &out)?;
    let mut hist: Vec<(u32, u64)> = length_hist.into_iter().collect();
    hist.sort_unstable();
    let mut out = String::from("length\tdocuments\n");
    for (l, n) in hist {
        let _ = writeln!(out, "{l}\t{n}");
    }
    std::fs::write(dir.join("lengths.tsv"), &out)
}
