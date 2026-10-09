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

/// An STN3 segment of the first `n` documents of a dumped one in ctid order,
/// as a table of that many rows (the heap's first pages) indexes them: the
/// same tokens, rebuilt by STN3's segment builder (for size reports: a
/// token sharing its position with another moves up by one).
pub fn subset(
    dumped: &DumpedSegment,
    n: u32,
    rows_per_page: Option<u16>,
) -> Result<Vec<u8>, String> {
    let blob: &[u8] = &dumped.blob;
    let reader = Reader::parse(blob).map_err(err)?;
    let sections = reader.sections();
    let n = n.min(reader.document_count());
    let mut tids = reader.doc_table().map_err(err)?.to_vec().map_err(err)?;
    // A denser heap: `rows_per_page` rows on every page, in rank order.
    if let Some(per) = rows_per_page {
        for (rank, tid) in tids.iter_mut().enumerate() {
            *tid = Tid {
                block: (rank / usize::from(per)) as u32,
                offset: (rank % usize::from(per)) as u16 + 1,
            };
        }
    }
    let payload_at = sections.header + sections.dictionary + sections.ordinals;
    let mut names: Vec<String> = Vec::new();
    let mut docs: Vec<Vec<(u32, u32)>> = vec![Vec::new(); n as usize];
    let dictionary = reader.dictionary().map_err(err)?;
    let mut positions = Vec::new();
    for block in 0..dictionary.index().blocks() {
        for (term, entry, _) in dictionary.block_sizes(block).map_err(err)? {
            let resolved = reader.resolve(entry).map_err(err)?;
            let mut cursor = resolved.ordinals().map_err(err)?.cursor().map_err(err)?;
            let from = payload_at + entry.payload.offset as usize;
            let payload =
                segment::payload::Payload::parse(&blob[from..from + entry.payload.len as usize])
                    .map_err(err)?;
            let mut entries = payload.cursor();
            let t = names.len() as u32;
            let mut used = false;
            while let Some(ordinal) = cursor.current() {
                if ordinal >= n {
                    break;
                }
                positions.clear();
                entries.next_into(&mut positions).map_err(err)?;
                for p in &positions {
                    docs[ordinal as usize].push((t, *p));
                }
                used = true;
                cursor.advance().map_err(err)?;
            }
            if used {
                names.push(term);
            }
        }
    }
    let mut builder = segment::segment::SegmentBuilder::default();
    for (rank, tokens) in docs.iter_mut().enumerate() {
        // STN3's builder takes one token per position; a few tokens share
        // one in the dump, and move up by one (a size measure).
        tokens.sort_unstable_by_key(|(t, p)| (*p, *t));
        let mut next = 0u32;
        for (_, p) in tokens.iter_mut() {
            *p = (*p).max(next);
            next = *p + 1;
        }
        builder
            .add_document(
                tids[rank],
                tokens
                    .iter()
                    .map(|(t, p)| (names[*t as usize].as_str(), *p)),
            )
            .map_err(err)?;
    }
    Ok(builder.finish())
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
            if let Some(inline) = &found.postings.lengths {
                for (i, slot) in slots.iter().enumerate() {
                    let rank = segment
                        .docs
                        .rank(*slot)
                        .ok_or("a posting without a document")?;
                    if inline.get(i as u32).map_err(err)?
                        != segment.lengths.get(rank).map_err(err)?
                    {
                        return Err(format!(
                            "term {term:?}: inline length of posting {i} differs"
                        ));
                    }
                }
            }
            let from = payload_at + entry.payload.offset as usize;
            let (positions, _) = segment.positions(&found.entry).map_err(err)?;
            let positions = segment::tinshape::positions::Positions::parse(positions)
                .map_err(err)?
                .payload()
                .map_err(err)?;
            if positions != dumped.blob[from..from + entry.payload.len as usize] {
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
    /// Per part, how its reads fall on pages, in the order made.
    pub order: BTreeMap<Part, ReadOrder>,
}

/// How a part's reads fall on pages: reads (touches), those spanning two
/// pages or more (a stitch and a second pin when read in place), reads on
/// another page than the part's last one, and of those the ones moving
/// back to an earlier page of the same blob (a read out of rank order).
/// Blobs are told apart by offsets past 2^40, as the replay counts them.
#[derive(Default, Clone, Copy)]
pub struct ReadOrder {
    pub reads: u64,
    pub straddles: u64,
    pub switches: u64,
    pub back: u64,
    last: Option<usize>,
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
        let order = self.order.entry(part).or_default();
        order.reads += 1;
        order.straddles += u64::from(last > first);
        match order.last {
            Some(previous) if previous == first => {}
            Some(previous) => {
                order.switches += 1;
                let same_blob = previous * PAGE_DATA >> 40 == at >> 40;
                order.back += u64::from(same_blob && first < previous);
            }
            None => order.switches += 1,
        }
        order.last = Some(last);
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
