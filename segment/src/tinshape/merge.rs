// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Merging segments in this shape: the inputs' live documents in ctid
//! order, and each term's postings from every input that holds it,
//! re-encoded over the merged grid.
//!
//! Nothing is renumbered in the sense the ordinal format meant: a posting
//! is a ctid, and the merged segment's grid is derived from the merged
//! documents. Every term's slot set, footer and TF tail are re-encoded and
//! its positions entries re-packed in the merged posting order. Copying a
//! group's containers unchanged when only one input holds the group is the
//! cheaper merge the format allows ([the TIN-shape guide](../../../docs/architecture/tin-shape.md)),
//! not done yet.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use super::postings::Options;
use super::segment::{Builder, Segment};
use crate::dead::DeadDocs;
use crate::dictionary::TermEntry;
use crate::payload::{Payload, PayloadBuilder};
use crate::{Error, Result, Tid};

/// One segment to merge and its dead documents (by rank).
pub struct Input<'a> {
    pub bytes: &'a [u8],
    pub dead: &'a DeadDocs,
}

/// A segment written in this shape, by a build or a merge, and what it
/// holds.
#[derive(Debug)]
pub struct Merged {
    pub blob: Vec<u8>,
    pub documents: u32,
    pub total_length: u64,
    pub terms: u64,
    /// One per term and document.
    pub postings: u64,
}

/// Terms between two calls of a merge's checkpoint.
const CHECKPOINT_TERMS: usize = 1024;

/// Merges `inputs` without their dead documents, calling `checkpoint`
/// every so often (it may fail to stop the merge). `None` when no input
/// holds a live document.
pub fn merge(
    inputs: &[Input<'_>],
    options: Options,
    mut checkpoint: impl FnMut() -> Result<()>,
) -> Result<Option<Merged>> {
    let segments = inputs
        .iter()
        .map(|input| Segment::parse(input.bytes))
        .collect::<Result<Vec<_>>>()?;
    // The live documents in ctid order, each with where it came from.
    let mut documents: Vec<(Tid, u32, usize, u32)> = Vec::new();
    for (i, segment) in segments.iter().enumerate() {
        checkpoint()?;
        for (rank, tid) in segment.docs.tids().into_iter().enumerate() {
            let rank = rank as u32;
            if !inputs[i].dead.contains(rank) {
                documents.push((tid, segment.lengths.get(rank)?, i, rank));
            }
        }
    }
    documents.sort_unstable_by_key(|d| d.0);
    if documents.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err(Error::Corrupt("a live document in two merge inputs"));
    }
    if documents.is_empty() {
        return Ok(None);
    }
    let mut map: Vec<Vec<u32>> = segments
        .iter()
        .map(|segment| vec![u32::MAX; segment.documents as usize])
        .collect();
    for (out, (_, _, input, rank)) in documents.iter().enumerate() {
        map[*input][*rank as usize] = out as u32;
    }
    let tids: Vec<Tid> = documents.iter().map(|d| d.0).collect();
    let lengths: Vec<u32> = documents.iter().map(|d| d.1).collect();
    let total_length = lengths.iter().map(|l| u64::from(*l)).sum();
    let count = documents.len() as u32;
    drop(documents);
    let mut builder = Builder::new(tids, lengths, options)?;

    let dictionaries: Vec<_> = segments.iter().map(Segment::dictionary).collect();
    let mut iters: Vec<_> = dictionaries.iter().map(|d| d.iter()).collect();
    let mut heads: Vec<Option<TermEntry>> = vec![None; segments.len()];
    let mut heap: BinaryHeap<Reverse<(String, usize)>> = BinaryHeap::new();
    for (i, iter) in iters.iter_mut().enumerate() {
        if let Some(item) = iter.next() {
            let (term, entry) = item?;
            heads[i] = Some(entry);
            heap.push(Reverse((term, i)));
        }
    }
    // One term's postings: (merged rank, bucket, positions extent).
    let mut postings: Vec<(u32, u8, u32, u32)> = Vec::new();
    let mut positions: Vec<u32> = Vec::new();
    let mut scratch: Vec<u32> = Vec::new();
    let mut holders: Vec<usize> = Vec::new();
    let (mut terms, mut written) = (0u64, 0u64);
    let mut since_checkpoint = 0usize;
    while let Some(Reverse((term, first))) = heap.pop() {
        holders.clear();
        holders.push(first);
        while heap.peek().is_some_and(|Reverse((next, _))| *next == term) {
            let Reverse((_, i)) = heap.pop().expect("peeked");
            holders.push(i);
        }
        postings.clear();
        positions.clear();
        for &i in &holders {
            let entry = heads[i].take().expect("a head per heap item");
            let segment = &segments[i];
            let found = segment.resolve(entry)?;
            let footer = found.postings.footer(
                segment.block_size,
                entry.max_tf_bucket,
                segment.adaptive_tf,
            )?;
            let (stream, _) = segment.positions(&entry)?;
            let payload = Payload::parse(stream)?;
            if payload.count() != entry.df {
                return Err(Error::Corrupt("positions entries against df"));
            }
            let mut cursor = payload.cursor();
            let mut index = 0u32;
            let mut failed = None;
            found
                .postings
                .for_each_slot(&segment.docs.geometry, |slot| {
                    if failed.is_some() {
                        return;
                    }
                    let step = (|| -> Result<()> {
                        let rank = segment
                            .docs
                            .rank(slot)
                            .ok_or(Error::Corrupt("posting outside the document set"))?;
                        let out = map[i][rank as usize];
                        if out == u32::MAX {
                            cursor.skip_entry()?;
                        } else {
                            scratch.clear();
                            cursor.next_into(&mut scratch)?;
                            let start = positions.len() as u32;
                            positions.extend_from_slice(&scratch);
                            postings.push((
                                out,
                                footer.bucket(found.postings.tf, index)?,
                                start,
                                scratch.len() as u32,
                            ));
                        }
                        index += 1;
                        Ok(())
                    })();
                    if let Err(error) = step {
                        failed = Some(error);
                    }
                })?;
            if let Some(error) = failed {
                return Err(error);
            }
            if index != entry.df {
                return Err(Error::Corrupt("postings against df"));
            }
            if let Some(item) = iters[i].next() {
                let (next, entry) = item?;
                if next <= term {
                    return Err(Error::Unordered);
                }
                heads[i] = Some(entry);
                heap.push(Reverse((next, i)));
            }
        }
        if !postings.is_empty() {
            postings.sort_unstable_by_key(|p| p.0);
            let ranks: Vec<u32> = postings.iter().map(|p| p.0).collect();
            let buckets: Vec<u8> = postings.iter().map(|p| p.1).collect();
            let mut payload = PayloadBuilder::default();
            for p in &postings {
                payload.push(&positions[p.2 as usize..(p.2 + p.3) as usize])?;
            }
            builder.add_term(&term, &ranks, &buckets, &payload.finish())?;
            terms += 1;
            written += ranks.len() as u64;
        }
        since_checkpoint += 1;
        if since_checkpoint >= CHECKPOINT_TERMS {
            since_checkpoint = 0;
            checkpoint()?;
        }
    }
    checkpoint()?;
    let (blob, _) = builder.finish(&[]);
    Ok(Some(Merged {
        blob,
        documents: count,
        total_length,
        terms,
        postings: written,
    }))
}
