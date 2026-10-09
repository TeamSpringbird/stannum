// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A dumped index converted to TIN's shape ([`segment::tinshape`]),
//! checked against the ordinal segments it came from, and measured: bytes
//! per area and per posting, and page touches as TIN's EXPLAIN names them.

use std::collections::BTreeMap;

use engine::tinshape::{Part, Touch};
use segment::Tid;
use segment::dead::DeadDocs;
use segment::segment::Reader;
use segment::tinshape::postings::{Options, Stats};
use segment::tinshape::segment::{BuildStats, Builder, Segment};

use crate::dump::DumpedSegment;
use crate::paged::PAGE_DATA;

/// One term's bytes in both formats.
#[derive(Clone, Copy, Debug, Default)]
pub struct TermBytes {
    pub df: u32,
    /// The ordinal stream (members, bucket nibbles, bounds).
    pub stn3_ordinals: u32,
    pub stn3_dictionary: u32,
    pub positions: u32,
    pub tns: Stats,
}

/// A segment converted, with what the conversion measured.
pub struct Converted {
    pub blob: Vec<u8>,
    pub stats: BuildStats,
    pub terms: Vec<TermBytes>,
    /// The STN3 blob's sections.
    pub stn3: segment::segment::Sections,
    pub documents: u32,
    pub tokens: u64,
}

fn err(e: segment::Error) -> String {
    e.to_string()
}

/// Converts a dumped STN3 segment.
pub fn convert(dumped: &DumpedSegment, options: Options) -> Result<Converted, String> {
    let blob: &[u8] = &dumped.blob;
    let reader = Reader::parse(blob).map_err(err)?;
    let sections = reader.sections();
    let documents = reader.document_count();
    let tids = reader.doc_table().map_err(err)?.to_vec().map_err(err)?;
    let mut lengths = Vec::with_capacity(tids.len());
    for o in 0..documents {
        lengths.push(reader.length_at(o).map_err(err)?);
    }
    let tokens = lengths.iter().map(|l| u64::from(*l)).sum();
    let dead = match &dumped.dead {
        Some(list) => {
            let decoded = DeadDocs::decode(list, documents).map_err(err)?;
            (0..documents).filter(|o| decoded.contains(*o)).collect()
        }
        None => Vec::new(),
    };
    let payload_at = sections.header + sections.dictionary + sections.ordinals;
    let mut builder = Builder::new(tids, lengths, options).map_err(err)?;
    let dictionary = reader.dictionary().map_err(err)?;
    let mut terms = Vec::with_capacity(dictionary.len());
    let mut ranks = Vec::new();
    let mut buckets = Vec::new();
    for block in 0..dictionary.index().blocks() {
        for (term, entry, entry_len) in dictionary.block_sizes(block).map_err(err)? {
            let resolved = reader.resolve(entry).map_err(err)?;
            let mut cursor = resolved.ordinals().map_err(err)?.cursor().map_err(err)?;
            ranks.clear();
            buckets.clear();
            while let Some(ordinal) = cursor.current() {
                ranks.push(ordinal);
                buckets.push(cursor.bucket().ok_or("a member without a bucket")?);
                cursor.advance().map_err(err)?;
            }
            let from = payload_at + entry.payload.offset as usize;
            let positions = &blob[from..from + entry.payload.len as usize];
            let stats = builder
                .add_term(&term, &ranks, &buckets, positions)
                .map_err(err)?;
            terms.push(TermBytes {
                df: entry.df,
                stn3_ordinals: entry.ordinals.len,
                stn3_dictionary: entry_len as u32,
                positions: entry.payload.len,
                tns: stats,
            });
        }
    }
    let (blob_out, stats) = builder.finish(&dead);
    Ok(Converted {
        blob: blob_out,
        stats,
        terms,
        stn3: sections,
        documents,
        tokens,
    })
}

/// Checks a converted segment against its source, exactly: every term's
/// documents (as ctids), buckets, positions and the documents' lengths.
pub fn verify(dumped: &DumpedSegment, tns: &[u8]) -> Result<u64, String> {
    let reader = Reader::parse(&dumped.blob[..]).map_err(err)?;
    let segment = Segment::parse(tns).map_err(err)?;
    let documents = reader.document_count();
    if segment.documents != documents {
        return Err("document counts differ".into());
    }
    let tids = reader.doc_table().map_err(err)?.to_vec().map_err(err)?;
    if segment.docs.tids() != tids {
        return Err("document sets differ".into());
    }
    for (rank, _) in tids.iter().enumerate() {
        if segment.lengths.get(rank as u32).map_err(err)?
            != reader.length_at(rank as u32).map_err(err)?
        {
            return Err(format!("length of document {rank} differs"));
        }
    }
    let sections = reader.sections();
    let payload_at = sections.header + sections.dictionary + sections.ordinals;
    let geometry = &segment.docs.geometry;
    let dictionary = reader.dictionary().map_err(err)?;
    let mut postings = 0u64;
    let mut terms = 0usize;
    for block in 0..dictionary.index().blocks() {
        for (term, entry) in dictionary.block(block).map_err(err)? {
            terms += 1;
            let found = segment
                .term(&term)
                .map_err(err)?
                .ok_or_else(|| format!("term {term:?} missing"))?;
            if found.entry.df != entry.df || found.entry.max_tf_bucket != entry.max_tf_bucket {
                return Err(format!("term {term:?}: statistics differ"));
            }
            let slots = found.postings.slots(geometry).map_err(err)?;
            let footer = found
                .postings
                .footer(
                    segment.block_size,
                    found.entry.max_tf_bucket,
                    segment.adaptive_tf,
                )
                .map_err(err)?;
            let resolved = reader.resolve(entry).map_err(err)?;
            let mut cursor = resolved.ordinals().map_err(err)?.cursor().map_err(err)?;
            let mut i = 0usize;
            while let Some(ordinal) = cursor.current() {
                let want = tids[ordinal as usize];
                let got: Tid = geometry.tid_of(*slots.get(i).ok_or("too few postings")?);
                if got != want {
                    return Err(format!(
                        "term {term:?}: posting {i} is {got:?}, not {want:?}"
                    ));
                }
                let bucket = footer.bucket(found.postings.tf, i as u32).map_err(err)?;
                if Some(bucket) != cursor.bucket() {
                    return Err(format!("term {term:?}: bucket of posting {i} differs"));
                }
                i += 1;
                cursor.advance().map_err(err)?;
            }
            if i != slots.len() {
                return Err(format!("term {term:?}: too many postings"));
            }
            let from = payload_at + entry.payload.offset as usize;
            let (positions, _) = segment.positions(&found.entry).map_err(err)?;
            if positions != &dumped.blob[from..from + entry.payload.len as usize] {
                return Err(format!("term {term:?}: positions differ"));
            }
            postings += i as u64;
        }
    }
    if segment.dictionary().len() != terms {
        return Err("term counts differ".into());
    }
    Ok(postings)
}

/// Distinct pages and accesses per part, over a blob read in pages of
/// [`PAGE_DATA`] bytes, as the paged source counts STN3's.
#[derive(Default, Clone)]
pub struct Pages {
    pub pages: BTreeMap<Part, rustc_hash::FxHashSet<usize>>,
    pub accesses: BTreeMap<Part, u64>,
}

impl Touch for Pages {
    fn touch(&mut self, part: Part, at: usize, len: usize) {
        let first = at / PAGE_DATA;
        let last = (at + len.max(1) - 1) / PAGE_DATA;
        let set = self.pages.entry(part).or_default();
        for page in first..=last {
            set.insert(page);
        }
        *self.accesses.entry(part).or_default() += (last - first + 1) as u64;
    }
}

impl Pages {
    pub fn distinct(&self, part: Part) -> usize {
        self.pages.get(&part).map_or(0, |s| s.len())
    }
    pub fn total(&self) -> usize {
        self.pages.values().map(|s| s.len()).sum()
    }
}
