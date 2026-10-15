// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! The census of inline length codes (a lab census, no format change):
//! how many length-class and DL reads a per-posting length code, stored
//! with a (term, group) list of few postings as TIN stores one, would
//! save, and what the codes would cost.
//!
//! The walk reports each class and DL read through
//! [`engine::tinshape::Touch::census_length`]: the fewest postings of a
//! (term, group) list the candidate holds, and whether it would be cut at
//! its length's largest power of two (TIN's 4-bit code), at its length
//! rounded down to six significant bits (a byte), and at its length. A
//! class read is saved for a candidate holding a short list (the code
//! gives its class); a DL read where the code would have cut it.

use std::fmt::Write as _;

use engine::tinshape::LengthRead;
use segment::tinshape::postings::{Form, for_each_local, round_length, rounded_length};
use segment::tinshape::segment::Segment;

/// Most postings of a (term, group) list carrying codes, per variant.
pub const SHORT: [u32; 4] = [16, 30, 62, 126];

/// Reads of one kind: all, then per [`SHORT`] those holding such a list,
/// and of those the ones cut at each code (power of two, six bits, exact).
#[derive(Clone, Copy, Default)]
struct Reads {
    all: u64,
    short: [u64; SHORT.len()],
    cut: [[u64; 3]; SHORT.len()],
}

impl Reads {
    fn add(&mut self, read: &LengthRead) {
        self.all += 1;
        for (s, max) in SHORT.iter().enumerate() {
            if read.short <= *max {
                self.short[s] += 1;
                for c in 0..3 {
                    self.cut[s][c] += u64::from(read.cut[c]);
                }
            }
        }
    }
}

/// The census of one replay run.
#[derive(Default)]
pub struct Census {
    queries: u64,
    /// Class reads after the buckets (led walks), before them (a
    /// disjunction's), and DL reads.
    class_after: Reads,
    class_before: Reads,
    dl: Reads,
}

impl Census {
    pub fn add(&mut self, read: &LengthRead) {
        match (read.class, read.or) {
            (true, false) => self.class_after.add(read),
            (true, true) => self.class_before.add(read),
            (false, _) => self.dl.add(read),
        }
    }

    pub fn query(&mut self) {
        self.queries += 1;
    }

    /// The census per query, and per variant what the codes save.
    pub fn report(&self, style: &str, mode: &str) -> String {
        let q = self.queries.max(1) as f64;
        let mut out = String::new();
        let class = self.class_after.all + self.class_before.all;
        let total = class + self.dl.all;
        let _ = writeln!(
            out,
            "inline-length census {style} {mode}: {} queries; per query class reads {:.0} \
             (after buckets {:.0}, before {:.0}), DL reads {:.0}, together {:.0}",
            self.queries,
            class as f64 / q,
            self.class_after.all as f64 / q,
            self.class_before.all as f64 / q,
            self.dl.all as f64 / q,
            total as f64 / q
        );
        let _ = writeln!(
            out,
            "  short<=  class held  DL held  | DL removed pow2 / six / exact  | class+DL removed pow2 / six / exact"
        );
        for (s, max) in SHORT.iter().enumerate() {
            let held = self.class_after.short[s] + self.class_before.short[s];
            let pct = |n: u64, d: u64| 100.0 * n as f64 / d.max(1) as f64;
            let dl = |c: usize| self.dl.cut[s][c];
            let _ = writeln!(
                out,
                "  {max:>6}  {:>9.1}%  {:>6.1}%  | {:>6.1}% {:>6.1}% {:>6.1}%  | {:>6.1}% {:>6.1}% {:>6.1}%",
                pct(held, class),
                pct(self.dl.short[s], self.dl.all),
                pct(dl(0), self.dl.all),
                pct(dl(1), self.dl.all),
                pct(dl(2), self.dl.all),
                pct(held + dl(0), total),
                pct(held + dl(1), total),
                pct(held + dl(2), total),
            );
        }
        // How often the code alone cuts at the class read (the code read
        // replaces it, and cuts this share of the candidates reading it).
        let cuts = |r: &Reads, s: usize| {
            format!(
                "{:.1}% / {:.1}% / {:.1}%",
                100.0 * r.cut[s][0] as f64 / r.short[s].max(1) as f64,
                100.0 * r.cut[s][1] as f64 / r.short[s].max(1) as f64,
                100.0 * r.cut[s][2] as f64 / r.short[s].max(1) as f64
            )
        };
        let s62 = SHORT.iter().position(|&m| m == 62).expect("62");
        let _ = writeln!(
            out,
            "  at <=62: class reads held cut by the code (pow2 / six / exact): after buckets {}, before {}",
            cuts(&self.class_after, s62),
            cuts(&self.class_before, s62)
        );
        out
    }
}

/// The index-wide cost: per [`SHORT`], the grouped (term, group) lists of
/// at most that many postings (not of a record carrying its lengths), their
/// postings, and the bytes of their codes: 4 bits a posting, a byte a
/// posting, and 4 bits only where the list's codes differ (a byte a list
/// for its lowest code and width, then each posting's code less it at
/// that width, 0 to 4 bits). Also how many codes would cut nothing a
/// class does not (every posting's length under 16).
pub fn sizes(segments: &[Segment<'static>], blob_bytes: u64) -> String {
    let n = SHORT.len();
    let mut lists = vec![0u64; n];
    let mut postings = vec![0u64; n];
    let mut fixed4 = vec![0u64; n];
    let mut byte = vec![0u64; n];
    let mut adaptive = vec![0u64; n];
    let mut kinds = vec![[0u64; 4]; n];
    let mut lengths: Vec<u32> = Vec::new();
    for segment in segments {
        let geometry = &segment.docs.geometry;
        for item in segment.dictionary().iter() {
            let (_, entry) = item.expect("a term");
            let found = segment.resolve(entry).expect("a record");
            let p = &found.postings;
            let Form::Grouped(dir) = &p.form else {
                continue;
            };
            if p.lengths.is_some() {
                continue;
            }
            for e in dir.entries(&geometry.groups).expect("a directory") {
                if e.count > SHORT[n - 1] {
                    continue;
                }
                let g = e.index as usize;
                lengths.clear();
                let bytes = p.container(&e).all().expect("a container");
                for_each_local(&e, bytes, geometry.starts(g), |local| {
                    let rank = segment.docs.rank_in(g, local).expect("a document");
                    lengths.push(segment.lengths.get(rank).expect("a length"));
                })
                .expect("members");
                let codes: Vec<u32> = lengths.iter().map(|&l| 32 - l.leading_zeros()).collect();
                let lo = *codes.iter().min().unwrap_or(&0);
                let hi = *codes.iter().max().unwrap_or(&0);
                let width = match hi - lo {
                    0 => 0,
                    1 => 1,
                    2..=3 => 2,
                    _ => 4,
                };
                let c = u64::from(e.count);
                for (s, max) in SHORT.iter().enumerate() {
                    if e.count <= *max {
                        lists[s] += 1;
                        postings[s] += c;
                        fixed4[s] += c.div_ceil(2);
                        byte[s] += c;
                        adaptive[s] += 1 + (c * width).div_ceil(8);
                        kinds[s][usize::from(e.kind).min(3)] += 1;
                    }
                }
            }
        }
        eprintln!(
            "inline-length sizes: a segment of {} documents done",
            segment.documents
        );
    }
    // Keep the six-bit code's helpers in this census honest: a byte holds
    // codes up to 255 (lengths to 4,032 to six significant bits).
    debug_assert_eq!(rounded_length(round_length(4032)), 4032);
    let mut out = String::new();
    let gb = |b: u64| b as f64 / 1e9;
    let pct = |b: u64| 100.0 * b as f64 / blob_bytes as f64;
    let _ = writeln!(
        out,
        "inline-length sizes: index {:.2} GB; short<= lists postings | 4 bits | byte | adaptive (GB, % of index) | kinds grid/ef/paged/array",
        gb(blob_bytes)
    );
    for s in 0..n {
        let _ = writeln!(
            out,
            "  {:>4} {:>11} {:>12} | {:.3} {:+.2}% | {:.3} {:+.2}% | {:.3} {:+.2}% | {:?}",
            SHORT[s],
            lists[s],
            postings[s],
            gb(fixed4[s]),
            pct(fixed4[s]),
            gb(byte[s]),
            pct(byte[s]),
            gb(adaptive[s]),
            pct(adaptive[s]),
            kinds[s]
        );
    }
    out
}
