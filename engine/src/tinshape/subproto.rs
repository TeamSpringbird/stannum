// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! R10 prototype (lab only): grid groups' sub-range codes computed from a
//! segment at load and kept beside it, so a walk can bound by them before
//! the format carries them.

use std::sync::OnceLock;

use rustc_hash::FxHashMap;
use segment::tinshape::postings::{Form, KIND_GRID, put_sub_codes};
use segment::tinshape::segment::Segment;

/// A record's codes: per directory entry, where its codes start (`u32::MAX`
/// for an entry without).
pub struct Record {
    pub offsets: Vec<u32>,
    pub codes: Vec<u8>,
}

/// A segment's records by their blob offset.
pub type Records = FxHashMap<usize, Record>;

/// Segments by (documents, total length).
static INSTALLED: OnceLock<FxHashMap<(u32, u64), Records>> = OnceLock::new();

/// Installs the prototype's codes for every segment (once).
pub fn install(segments: FxHashMap<(u32, u64), Records>) {
    let _ = INSTALLED.set(segments);
}

/// The codes of the record at `at` of `segment`, if installed.
pub fn record(segment: &Segment<'_>, at: usize) -> Option<&'static Record> {
    INSTALLED
        .get()?
        .get(&(segment.documents, segment.total_length))?
        .get(&at)
}

/// Every grouped record's grid entries' codes in `segment` (parsed whole).
pub fn build(segment: &Segment<'_>) -> Records {
    let geometry = &segment.docs.geometry;
    let mut out = Records::default();
    let mut locals = Vec::new();
    let mut postings = Vec::new();
    let mut buckets = Vec::new();
    for item in segment.dictionary().iter() {
        let (_, entry) = item.expect("a term");
        let term = segment.resolve(entry).expect("a record");
        let p = &term.postings;
        let Form::Grouped(dir) = &p.form else {
            continue;
        };
        let entries = dir.entries(&geometry.groups).expect("a directory");
        if !entries.iter().any(|e| e.kind == KIND_GRID) {
            continue;
        }
        let mut rec = Record {
            offsets: vec![u32::MAX; entries.len()],
            codes: Vec::new(),
        };
        for (b, e) in entries.iter().enumerate() {
            if e.kind != KIND_GRID {
                continue;
            }
            let group = &geometry.groups[e.index as usize];
            let bytes = p.container(e).all().expect("a container");
            locals.clear();
            for (w, chunk) in bytes.chunks(8).enumerate() {
                let mut eight = [0u8; 8];
                eight[..chunk.len()].copy_from_slice(chunk);
                let mut word = u64::from_le_bytes(eight);
                while word != 0 {
                    locals.push((w * 64) as u32 + word.trailing_zeros());
                    word &= word - 1;
                }
            }
            assert_eq!(locals.len() as u32, e.count);
            postings.clear();
            for (j, &local) in locals.iter().enumerate() {
                let bucket = p.bucket_in(e, e.first + j as u32).expect("a bucket");
                let rank = segment
                    .docs
                    .rank_in(e.index as usize, local)
                    .expect("a document");
                let length = segment.lengths.get(rank).expect("a length");
                postings.push((bucket, length));
            }
            buckets.clear();
            buckets.extend(p.group_frontier(e).map(|(bucket, _)| bucket));
            rec.offsets[b] = rec.codes.len() as u32;
            put_sub_codes(
                &buckets,
                group.slots(),
                &locals,
                postings.iter().copied(),
                &mut rec.codes,
            );
        }
        out.insert(term.at, rec);
    }
    out
}
