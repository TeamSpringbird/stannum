// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! The census of length codes kept in the TF tail (a lab census, no format
//! change): how many length-class and DL reads, and how many of their
//! pages and lines, a per-posting length code stored beside each bucket
//! (TIN's TF tail, its probe round 12 P5) would save, what it would cost
//! in index bytes, and how many more TF tail pages it would make a query
//! read.
//!
//! The walk reports each class and DL read ([`LengthRead`]) and each
//! bucket read ([`TfRead`]). A variant is a policy (which postings carry a
//! code) and a code (how a length is kept):
//! - policies: `T` the postings of a (term, group) or block whose buckets
//!   are stored (width above 0: TIN keeps no TF tail for a term whose tf
//!   never varies), `R` every posting of a record whose buckets vary
//!   somewhere, `A` every posting of a sparse or grouped record;
//! - codes: `c2` the length class (today's class plane on the bucket's
//!   line), `p4` the largest power of two no longer than the length, `fN`
//!   an N-bit code exact below 2^(N-1) and rounded down to N-3 significant
//!   bits above, `x16` the exact length.
//!
//! A read is served when the candidate holds a scoring term whose posting
//! carries a code (or a record carrying its lengths inline): a class read
//! always (every code bounds the length at least as well as its class), a
//! DL read when the code is the exact length or bounds the candidate out
//! (its rounded-down length is at least the shortest length at which the
//! candidate, as read then, is cut). Scores stay exact either way.
//!
//! TF pages are modeled with each posting's code right after its bucket
//! (interleaved, as TIN's tail), a record's tail growing by its codes and
//! every record moved by the segment's growth before it (proportionally).

use std::fmt::Write as _;

use engine::tinshape::{HeldTf, LengthKind, LengthRead, TfRead};
use rustc_hash::{FxHashMap, FxHashSet};
use segment::tinshape::postings::Form;
use segment::tinshape::segment::{Area, Segment};

use crate::paged::PAGE_DATA;

/// Codes: bits a posting, and the length each keeps of a length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    Class,
    Pow2,
    /// Exact below `2^N - 1`, that for longer (an escape: a bound).
    Escape(u32),
    Float(u32),
    Exact,
}

pub const CODES: [Code; 4] = [Code::Class, Code::Pow2, Code::Escape(8), Code::Float(8)];

impl Code {
    pub fn bits(self) -> u64 {
        match self {
            Code::Class => 2,
            Code::Pow2 => 4,
            Code::Escape(b) | Code::Float(b) => u64::from(b),
            Code::Exact => 16,
        }
    }

    pub fn name(self) -> String {
        match self {
            Code::Class => "c2".into(),
            Code::Pow2 => "p4".into(),
            Code::Escape(b) => format!("e{b}"),
            Code::Float(b) => format!("f{b}"),
            Code::Exact => "x16".into(),
        }
    }

    /// The length the code keeps of `length`: never longer.
    pub fn floor(self, length: u32) -> u32 {
        match self {
            Code::Class => [256, 64, 16, 0]
                .into_iter()
                .find(|f| length >= *f)
                .unwrap_or(0),
            Code::Pow2 => {
                if length == 0 {
                    0
                } else {
                    1 << (31 - length.leading_zeros())
                }
            }
            Code::Escape(b) => length.min((1 << b) - 1),
            Code::Float(b) => {
                let exact = 1u32 << (b - 1);
                if length < exact {
                    return length;
                }
                let m = b - 4;
                let e = 31 - length.leading_zeros();
                // Eight exponents from the exact range's top.
                let emax = (b - 1) + 7;
                if e > emax {
                    return ((1u32 << (m + 1)) - 1) << (emax - m);
                }
                (length >> (e - m)) << (e - m)
            }
            Code::Exact => length.min(u32::from(u16::MAX)),
        }
    }
}

/// Which postings carry a code: those of a block or group whose buckets
/// are stored (`Tail`, TIN's rule), or every posting of a sparse or
/// grouped record of at most this share of the segment's documents
/// (`Capped(1.0)`: all of them).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Policy {
    Tail,
    Capped(f64),
    /// `Capped`'s records, only their blocks or groups with stored buckets.
    TailCapped(f64),
}

pub const POLICIES: [Policy; 12] = [
    Policy::Tail,
    Policy::Capped(0.005),
    Policy::Capped(0.01),
    Policy::Capped(0.02),
    Policy::Capped(0.03),
    Policy::Capped(0.05),
    Policy::Capped(0.10),
    Policy::Capped(1.0),
    Policy::TailCapped(0.01),
    Policy::TailCapped(0.03),
    Policy::TailCapped(0.05),
    Policy::TailCapped(0.10),
];

impl Policy {
    pub fn name(self) -> String {
        match self {
            Policy::Tail => "T".into(),
            Policy::Capped(x) if x >= 1.0 => "A".into(),
            Policy::Capped(x) => format!("<{}%", x * 100.0),
            Policy::TailCapped(x) => format!("T<{}%", x * 100.0),
        }
    }

    fn index(self) -> usize {
        POLICIES.iter().position(|x| *x == self).expect("a policy")
    }

    /// Whether a record of `df` postings carries codes, in a segment of
    /// `documents`.
    fn record(self, df: u32, documents: u64) -> bool {
        match self {
            Policy::Tail => true,
            Policy::Capped(x) | Policy::TailCapped(x) => f64::from(df) <= x * documents as f64,
        }
    }

    /// Whether held term `h`'s posting carries a code.
    fn covers(self, h: &HeldTf, documents: u64) -> bool {
        !h.single
            && !h.carrier
            && self.record(h.df, documents)
            && match self {
                Policy::Tail | Policy::TailCapped(_) => h.width > 0,
                Policy::Capped(_) => true,
            }
    }

    /// Codes before posting `index` of a record (the TF model's): its
    /// postings before it, for a record whose postings carry codes
    /// (`Tail`'s modeled as all of a record whose buckets vary: an upper
    /// bound on its tail's growth).
    fn before(self, tail: bool, df: u32, documents: u64, index: u32) -> u64 {
        let coded = match self {
            Policy::Tail => tail,
            Policy::Capped(_) => self.record(df, documents),
            Policy::TailCapped(_) => tail && self.record(df, documents),
        };
        if coded { u64::from(index) } else { 0 }
    }
}

pub const VARIANTS: usize = POLICIES.len() * CODES.len();

fn variant(v: usize) -> (Policy, Code) {
    (POLICIES[v / CODES.len()], CODES[v % CODES.len()])
}

/// One segment's sizes, and its blob's growth per variant (for the TF
/// model's record offsets).
#[derive(Clone, Debug, Default)]
pub struct SegmentSizes {
    pub documents: u64,
    pub blob: u64,
    pub postings_at: u64,
    pub postings_area: u64,
    pub records: u64,
    pub singles: u64,
    pub carriers: u64,
    pub carrier_postings: u64,
    /// Postings per policy (the codes' count).
    pub covered: [u64; POLICIES.len()],
    pub all_postings: u64,
    pub tf_bits: u64,
    /// Documents by length: < 16, 32, 64, 128, 256, 512, 1024, more.
    pub lengths: [u64; 8],
    pub length_sum: u64,
}

impl SegmentSizes {
    /// What the codes of variant `v` add to the blob, over the postings
    /// area: the scale of a record's offset.
    fn growth(&self, v: usize) -> f64 {
        let (p, c) = variant(v);
        (self.covered[p.index()] * c.bits()) as f64 / 8.0 / self.postings_area.max(1) as f64
    }
}

/// The sizes of every segment's codes (a pass over every record).
pub fn sizes(segment: &Segment<'_>, blob: u64) -> SegmentSizes {
    let mut s = SegmentSizes {
        documents: u64::from(segment.documents),
        blob,
        postings_at: segment.area_at(Area::Postings) as u64,
        postings_area: segment.area(Area::Postings).map_or(0, |a| a.len() as u64),
        ..Default::default()
    };
    let geometry = &segment.docs.geometry;
    for item in segment.dictionary().iter() {
        let (_, entry) = item.expect("a term");
        let found = segment.resolve(entry).expect("a record");
        let p = &found.postings;
        s.records += 1;
        let df = u64::from(entry.df);
        s.all_postings += df;
        if matches!(p.form, Form::Single(_)) {
            s.singles += 1;
            continue;
        }
        if p.lengths.is_some() {
            s.carriers += 1;
            s.carrier_postings += df;
            continue;
        }
        let tail = entry.max_tf_bucket > 0;
        for (i, p) in POLICIES.iter().enumerate().skip(1) {
            if matches!(p, Policy::Capped(_)) && p.record(entry.df, s.documents) {
                s.covered[i] += df;
            }
        }
        let tail_capped = |s: &mut SegmentSizes, n: u64| {
            for (i, p) in POLICIES.iter().enumerate() {
                if matches!(p, Policy::TailCapped(_)) && p.record(entry.df, s.documents) {
                    s.covered[i] += n;
                }
            }
        };
        match &p.form {
            Form::Grouped(dir) => {
                for e in dir.entries(&geometry.groups).expect("a directory") {
                    if e.width > 0 {
                        s.covered[0] += u64::from(e.count);
                        tail_capped(&mut s, u64::from(e.count));
                        s.tf_bits += u64::from(e.count) * u64::from(e.width);
                    }
                }
            }
            _ => {
                if !tail {
                    continue;
                }
                let footer = p
                    .footer(
                        geometry,
                        segment.block_size,
                        entry.max_tf_bucket,
                        segment.adaptive_tf,
                    )
                    .expect("a footer");
                let bs = u64::from(footer.block_size);
                let blocks = footer.widths.len() as u64;
                for (b, w) in footer.widths.iter().enumerate() {
                    let n = if b as u64 + 1 == blocks {
                        df - bs * (blocks - 1)
                    } else {
                        bs
                    };
                    if *w > 0 {
                        s.covered[0] += n;
                        tail_capped(&mut s, n);
                        s.tf_bits += n * u64::from(*w);
                    }
                }
            }
        }
    }
    for rank in 0..segment.documents {
        let l = segment.lengths.get(rank).expect("a length");
        s.length_sum += u64::from(l);
        let at = match l {
            0..16 => 0,
            16..32 => 1,
            32..64 => 2,
            64..128 => 3,
            128..256 => 4,
            256..512 => 5,
            512..1024 => 6,
            _ => 7,
        };
        s.lengths[at] += 1;
    }
    s
}

/// The sizes report over every segment.
pub fn sizes_report(all: &[SegmentSizes]) -> String {
    let sum = |f: &dyn Fn(&SegmentSizes) -> u64| all.iter().map(f).sum::<u64>();
    let blob = sum(&|s| s.blob);
    let mut out = String::new();
    let gb = |b: f64| b / 1e9;
    let _ = writeln!(
        out,
        "lencode sizes: blobs {:.3} GB; records {} (singles {}, inline-length carriers {} with {} postings); postings {}; TF tail {:.3} GB",
        gb(blob as f64),
        sum(&|s| s.records),
        sum(&|s| s.singles),
        sum(&|s| s.carriers),
        sum(&|s| s.carrier_postings),
        sum(&|s| s.all_postings),
        gb(sum(&|s| s.tf_bits) as f64 / 8.0),
    );
    for (i, p) in POLICIES.iter().enumerate() {
        let n = sum(&|s| s.covered[i]);
        let _ = write!(out, "  policy {}: {n} postings coded;", p.name());
        for c in CODES {
            let bytes = (n * c.bits()) as f64 / 8.0;
            let _ = write!(
                out,
                " {} {:.3} GB {:+.2}%",
                c.name(),
                gb(bytes),
                100.0 * bytes / blob as f64
            );
        }
        let _ = writeln!(out);
    }
    let docs = sum(&|s| s.documents);
    let _ = writeln!(
        out,
        "  (T: blocks or groups with stored buckets; <x%: records of at most x% of the segment's documents; T<x%: those records' blocks or groups with stored buckets; A: all)"
    );
    let mut acc = 0;
    let _ = write!(
        out,
        "  documents {docs}, mean length {:.1}; cumulative share below 16/32/64/128/256/512/1024:",
        sum(&|s| s.length_sum) as f64 / docs.max(1) as f64
    );
    for k in 0..7 {
        acc += sum(&|s| s.lengths[k]);
        let _ = write!(out, " {:.1}%", 100.0 * acc as f64 / docs.max(1) as f64);
    }
    let _ = writeln!(out);
    out
}

/// One query's reads, per variant (and the base, last).
#[derive(Default)]
struct QuerySets {
    class_pages: Vec<FxHashSet<usize>>,
    dl_pages: Vec<FxHashSet<usize>>,
    class_lines: Vec<FxHashSet<usize>>,
    dl_lines: Vec<FxHashSet<usize>>,
    tf_pages: Vec<FxHashSet<usize>>,
    tf_lines: Vec<FxHashSet<usize>>,
}

impl QuerySets {
    fn new() -> Self {
        let v = |n: usize| (0..n).map(|_| FxHashSet::default()).collect::<Vec<_>>();
        Self {
            class_pages: v(VARIANTS + 1),
            dl_pages: v(VARIANTS + 1),
            class_lines: v(VARIANTS + 1),
            dl_lines: v(VARIANTS + 1),
            tf_pages: v(TF_SETS),
            tf_lines: v(TF_SETS),
        }
    }

    fn clear(&mut self) {
        for s in [
            &mut self.class_pages,
            &mut self.dl_pages,
            &mut self.class_lines,
            &mut self.dl_lines,
            &mut self.tf_pages,
            &mut self.tf_lines,
        ] {
            for x in s.iter_mut() {
                x.clear();
            }
        }
    }
}

/// Totals of a style, per variant (and the base, last).
#[derive(Clone, Default)]
struct Totals {
    queries: u64,
    class_reads: u64,
    class_first: u64,
    dl_reads: u64,
    /// DL reads by the document's length (as [`SegmentSizes::lengths`]),
    /// and those not cut at their length (their exact score needed).
    dl_by_length: [u64; 8],
    dl_survivors: u64,
    /// Reads with a carrier of inline lengths held.
    class_carrier: u64,
    dl_carrier: u64,
    /// Per policy, reads holding a coded posting.
    class_covered: [u64; POLICIES.len()],
    dl_covered: [u64; POLICIES.len()],
    /// Per variant.
    class_served: Vec<u64>,
    dl_served: Vec<u64>,
    dl_exact: Vec<u64>,
    class_pages: Vec<u64>,
    dl_pages: Vec<u64>,
    class_lines: Vec<u64>,
    dl_lines: Vec<u64>,
    tf_pages: Vec<u64>,
    tf_lines: Vec<u64>,
    code_reads_new_line: Vec<u64>,
}

impl Totals {
    fn new() -> Self {
        let n = VARIANTS + 1;
        Self {
            class_served: vec![0; n],
            dl_served: vec![0; n],
            dl_exact: vec![0; n],
            class_pages: vec![0; n],
            dl_pages: vec![0; n],
            class_lines: vec![0; n],
            dl_lines: vec![0; n],
            tf_pages: vec![0; TF_SETS],
            tf_lines: vec![0; TF_SETS],
            code_reads_new_line: vec![0; TF_SETS],
            ..Default::default()
        }
    }
}

/// The census of one replay run.
pub struct Census {
    /// Per segment (by its index in the replay), its sizes.
    segments: Vec<SegmentSizes>,
    /// The segment of the walk at hand.
    pub segment: usize,
    style: String,
    sets: QuerySets,
    totals: FxHashMap<String, Totals>,
    order: Vec<String>,
}

const BASE: usize = VARIANTS;

/// TF page sets: per layout ([`LAYOUTS`]) and variant, then the base.
const TF_SETS: usize = 2 * VARIANTS + 1;
const TF_BASE: usize = 2 * VARIANTS;

/// How a group's (or block's) codes lie in its TF tail: each posting's
/// right after its bucket, or all of them after the group's buckets.
pub const LAYOUTS: [&str; 2] = ["interleaved", "planar"];

fn page(seg: usize, at: usize) -> usize {
    (seg << 40 | at) / PAGE_DATA
}

fn line(seg: usize, at: usize) -> usize {
    (seg << 40 | at) / 64
}

/// Where record `record` of segment `seg` starts under variant `v`: the
/// records before it grown by their codes (proportionally).
fn moved(segments: &[SegmentSizes], seg: usize, v: usize, record: usize) -> usize {
    let Some(s) = segments.get(seg) else {
        return record;
    };
    let from = s.postings_at as usize;
    let into = record.saturating_sub(from);
    record + (into as f64 * s.growth(v)) as usize
}

impl Census {
    pub fn new(segments: Vec<SegmentSizes>) -> Self {
        Self {
            segments,
            segment: 0,
            style: String::new(),
            sets: QuerySets::new(),
            totals: FxHashMap::default(),
            order: Vec::new(),
        }
    }

    pub fn begin(&mut self, style: &str) {
        style.clone_into(&mut self.style);
        self.sets.clear();
        if !self.totals.contains_key(style) {
            self.totals.insert(style.to_owned(), Totals::new());
            self.order.push(style.to_owned());
        }
    }

    pub fn end(&mut self) {
        let t = self.totals.get_mut(&self.style).expect("begun");
        t.queries += 1;
        for v in 0..=VARIANTS {
            t.class_pages[v] += self.sets.class_pages[v].len() as u64;
            t.dl_pages[v] += self.sets.dl_pages[v].len() as u64;
            t.class_lines[v] += self.sets.class_lines[v].len() as u64;
            t.dl_lines[v] += self.sets.dl_lines[v].len() as u64;
        }
        for v in 0..TF_SETS {
            t.tf_pages[v] += self.sets.tf_pages[v].len() as u64;
            t.tf_lines[v] += self.sets.tf_lines[v].len() as u64;
        }
    }

    /// Where a record's TF tail's bytes `from_bit..to_bit` (postings
    /// `i0..=i1` of a block whose buckets start at bit `start` from posting
    /// `first`) lie under variant `v` and layout `layout`: interleaved, each
    /// posting's code right after its bucket; planar, a block's codes after
    /// its buckets (its buckets moved by the codes before the block).
    #[allow(clippy::too_many_arguments)]
    fn tf_span(&self, layout: usize, v: usize, r: &TfRead, i0: u32, i1: u32) -> (usize, usize) {
        let (p, c) = variant(v);
        let docs = self.documents();
        let cov = |i: u32| c.bits() * p.before(r.tail, r.df, docs, i);
        let moved = moved(&self.segments, self.segment, v, r.record) + (r.tf - r.record);
        let (a, b) = if layout == 0 {
            (
                r.from_bit as u64 + cov(i0),
                r.to_bit as u64 + cov(i1) + c.bits(),
            )
        } else {
            let shift = cov(r.first);
            (r.from_bit as u64 + shift, r.to_bit as u64 + shift)
        };
        (moved + (a / 8) as usize, moved + (b.div_ceil(8)) as usize)
    }

    /// Where held term `h`'s code lies under variant `v` and `layout`.
    fn code_at(&self, layout: usize, v: usize, h: &HeldTf, docs: u64) -> usize {
        let (p, c) = variant(v);
        let cov = |i: u32| c.bits() * p.before(h.tail, h.df, docs, i);
        let within = u64::from(h.index.saturating_sub(h.first));
        let bit = if layout == 0 {
            h.start_bit as u64 + within * u64::from(h.width) + cov(h.index) + u64::from(h.width)
        } else {
            h.start_bit as u64
                + cov(h.first)
                + u64::from(h.count) * u64::from(h.width)
                + within * c.bits()
        };
        moved(&self.segments, self.segment, v, h.record) + (h.tf - h.record) + (bit / 8) as usize
    }

    pub fn tf(&mut self, r: &TfRead) {
        let seg = self.segment;
        if r.width == 0 || r.to_bit <= r.from_bit {
            return;
        }
        let index =
            |bit: usize| r.first + ((bit.saturating_sub(r.start_bit)) / r.width as usize) as u32;
        let (i0, i1) = (index(r.from_bit), index(r.to_bit - 1));
        {
            let (a, b) = (r.tf + r.from_bit / 8, r.tf + r.to_bit.div_ceil(8));
            let set = &mut self.sets.tf_pages[TF_BASE];
            for p in page(seg, a)..=page(seg, b.max(a + 1) - 1) {
                set.insert(p);
            }
            let set = &mut self.sets.tf_lines[TF_BASE];
            for l in line(seg, a)..=line(seg, b.max(a + 1) - 1) {
                set.insert(l);
            }
        }
        for layout in 0..2 {
            for v in 0..VARIANTS {
                let (a, b) = self.tf_span(layout, v, r, i0, i1);
                let set = &mut self.sets.tf_pages[layout * VARIANTS + v];
                for p in page(seg, a)..=page(seg, b.max(a + 1) - 1) {
                    set.insert(p);
                }
                let set = &mut self.sets.tf_lines[layout * VARIANTS + v];
                for l in line(seg, a)..=line(seg, b.max(a + 1) - 1) {
                    set.insert(l);
                }
            }
        }
    }

    /// The documents of the segment at hand.
    fn documents(&self) -> u64 {
        self.segments
            .get(self.segment)
            .map_or(u64::MAX, |s| s.documents)
    }

    pub fn length(&mut self, r: &LengthRead<'_>) {
        let seg = self.segment;
        let docs = self.documents();
        let class = r.kind != LengthKind::Dl;
        let mut taken = std::mem::take(self.totals.get_mut(&self.style).expect("begun"));
        let t = &mut taken;
        if class {
            t.class_reads += 1;
            t.class_first += u64::from(r.kind == LengthKind::ClassOrFirst);
        } else {
            t.dl_reads += 1;
            let at = match r.length {
                0..16 => 0,
                16..32 => 1,
                32..64 => 2,
                64..128 => 3,
                128..256 => 4,
                256..512 => 5,
                512..1024 => 6,
                _ => 7,
            };
            t.dl_by_length[at] += 1;
            t.dl_survivors += u64::from(r.cut_at == u32::MAX);
        }
        let carrier = r.held.iter().any(|h| h.carrier);
        if class {
            t.class_carrier += u64::from(carrier);
        } else {
            t.dl_carrier += u64::from(carrier);
        }
        for (i, p) in POLICIES.iter().enumerate() {
            let c = r.held.iter().any(|h| p.covers(h, docs));
            if class {
                t.class_covered[i] += u64::from(c);
            } else {
                t.dl_covered[i] += u64::from(c);
            }
        }
        for v in 0..=VARIANTS {
            let served = if v == BASE {
                false
            } else if carrier {
                true
            } else {
                let (p, c) = variant(v);
                let coded = r.held.iter().filter(|h| p.covers(h, docs));
                // The code read: the line of a bucket read already, else the
                // first bucket a disjunction reads (the highest bound).
                let mut by: Option<&HeldTf> = None;
                for h in coded {
                    by = match by {
                        None => Some(h),
                        Some(b) if !b.read && (h.read || h.bound > b.bound) => Some(h),
                        keep => keep,
                    };
                }
                match by {
                    None => false,
                    Some(h) => {
                        let lo = c.floor(r.length);
                        let ok = class || lo == r.length || lo >= r.cut_at;
                        if ok {
                            if !class {
                                t.dl_exact[v] += u64::from(lo == r.length);
                            }
                            let h = *h;
                            for layout in 0..2 {
                                let a = self.code_at(layout, v, &h, docs);
                                let k = layout * VARIANTS + v;
                                self.sets.tf_pages[k].insert(page(seg, a));
                                let fresh = self.sets.tf_lines[k].insert(line(seg, a));
                                t.code_reads_new_line[k] += u64::from(fresh);
                            }
                        }
                        ok
                    }
                }
            };
            if served {
                if class {
                    t.class_served[v] += 1;
                } else {
                    t.dl_served[v] += 1;
                }
                continue;
            }
            if class {
                self.sets.class_pages[v].insert(page(seg, r.at));
                self.sets.class_lines[v].insert(line(seg, r.at));
            } else {
                self.sets.dl_pages[v].insert(page(seg, r.at));
                self.sets.dl_pages[v].insert(page(seg, r.at2));
                self.sets.dl_lines[v].insert(line(seg, r.at));
                self.sets.dl_lines[v].insert(line(seg, r.at2));
            }
        }
        *self.totals.get_mut(&self.style).expect("begun") = taken;
    }

    /// The census per style: per variant its size, the reads it serves and
    /// the length and TF pages a query reads.
    pub fn report(&self) -> String {
        let mut out = String::new();
        let blob: u64 = self.segments.iter().map(|s| s.blob).sum();
        let covered: [u64; POLICIES.len()] =
            std::array::from_fn(|i| self.segments.iter().map(|s| s.covered[i]).sum());
        for style in &self.order {
            let t = &self.totals[style];
            let q = t.queries.max(1) as f64;
            let pct = |n: u64, d: u64| 100.0 * n as f64 / d.max(1) as f64;
            let _ = writeln!(
                out,
                "lencode {style}: {} queries; per query class reads {:.0} ({:.0} before buckets), DL reads {:.0} \
                 ({:.1}% exact-score survivors); class/DL with a carrier held {:.1}% / {:.1}%",
                t.queries,
                t.class_reads as f64 / q,
                t.class_first as f64 / q,
                t.dl_reads as f64 / q,
                pct(t.dl_survivors, t.dl_reads),
                pct(t.class_carrier, t.class_reads),
                pct(t.dl_carrier, t.dl_reads),
            );
            let mut acc = 0;
            let _ = write!(
                out,
                "  DL reads by length, cumulative <16/32/64/128/256/512/1024:"
            );
            for k in 0..7 {
                acc += t.dl_by_length[k];
                let _ = write!(out, " {:.1}%", pct(acc, t.dl_reads));
            }
            let _ = writeln!(out);
            let _ = write!(out, "  covered (a coded posting held) class/DL:");
            for (i, p) in POLICIES.iter().enumerate() {
                let _ = write!(
                    out,
                    "  {} {:.1}/{:.1}%",
                    p.name(),
                    pct(t.class_covered[i], t.class_reads),
                    pct(t.dl_covered[i], t.dl_reads)
                );
            }
            let _ = writeln!(out);
            let base_len_pages = (t.class_pages[BASE] + t.dl_pages[BASE]) as f64 / q;
            let base_len_lines = (t.class_lines[BASE] + t.dl_lines[BASE]) as f64 / q;
            let _ = writeln!(
                out,
                "  base: class pages {:.1}, DL pages {:.1}, length lines {:.0}, TF pages {:.1}, TF lines {:.0} (model) per query",
                t.class_pages[BASE] as f64 / q,
                t.dl_pages[BASE] as f64 / q,
                base_len_lines,
                t.tf_pages[TF_BASE] as f64 / q,
                t.tf_lines[TF_BASE] as f64 / q
            );
            let _ = writeln!(
                out,
                "  variant       size%  | class served  DL served (exact) | len pages/q  avoided | len lines avoided | TF pages/q interleaved (+) / planar (+) | len+TF pages net i / p | TF lines +i / +p | len+TF lines net i / p"
            );
            let tf0 = t.tf_pages[TF_BASE] as f64 / q;
            let tl0 = t.tf_lines[TF_BASE] as f64 / q;
            for v in 0..VARIANTS {
                let (p, c) = variant(v);
                let size =
                    100.0 * (covered[p.index()] * c.bits()) as f64 / 8.0 / blob.max(1) as f64;
                let lp = (t.class_pages[v] + t.dl_pages[v]) as f64 / q;
                let ll = (t.class_lines[v] + t.dl_lines[v]) as f64 / q;
                let ti = t.tf_pages[v] as f64 / q;
                let tp = t.tf_pages[VARIANTS + v] as f64 / q;
                let net = |tf: f64| 100.0 * ((lp + tf) / (base_len_pages + tf0).max(1e-9) - 1.0);
                let li = t.tf_lines[v] as f64 / q;
                let lpl = t.tf_lines[VARIANTS + v] as f64 / q;
                let netl = |tl: f64| 100.0 * ((ll + tl) / (base_len_lines + tl0).max(1e-9) - 1.0);
                let _ = writeln!(
                    out,
                    "  {:<6}{:<4}  {:+6.2}%  | {:>6.1}%  {:>6.1}% ({:>5.1}%) | {:>7.1} {:>6.1}% | {:>6.1}% | {:>7.1} ({:+7.1}) / {:>7.1} ({:+7.1}) | {:+6.1}% / {:+6.1}% | {:+7.0} / {:+7.0} | {:+6.1}% / {:+6.1}%",
                    p.name(),
                    c.name(),
                    size,
                    pct(t.class_served[v], t.class_reads),
                    pct(t.dl_served[v], t.dl_reads),
                    pct(t.dl_exact[v], t.dl_reads),
                    lp,
                    100.0 * (1.0 - lp / base_len_pages.max(1e-9)),
                    100.0 * (1.0 - ll / base_len_lines.max(1e-9)),
                    ti,
                    ti - tf0,
                    tp,
                    tp - tf0,
                    net(ti),
                    net(tp),
                    li - tl0,
                    lpl - tl0,
                    netl(li),
                    netl(lpl),
                );
            }
        }
        out
    }
}
