// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Merging segments in this shape: the inputs' live documents in ctid
//! order, and each term's postings from every input that holds it,
//! re-encoded over the merged grid.
//!
//! Nothing is renumbered in the sense the ordinal format meant: a posting
//! is a ctid, and the merged segment's grid is derived from the merged
//! documents. A group only one input holds, with the input's geometry and
//! no dead document there, keeps each term's container as the input wrote
//! it ([`Builder::add_term_reusing`]); every other group is re-encoded over
//! the merged grid. Footers and TF tails are rebuilt and positions entries
//! re-packed in the merged posting order, since posting indexes interleave
//! ([the TIN-shape guide](../../../docs/architecture/tin-shape.md)).
//!
//! A merge streams: it reads its inputs through [`Source`]s, each area
//! through a window that moves forward with the term walk, and spills what
//! it builds term by term to a [`Spill`] before writing the blob to a
//! [`Sink`] front to back. What it holds is the documents' tables (a few
//! bytes a document per input and for the output), the term maps' indexes,
//! the term at hand's postings and positions, and a window per area and
//! input: not the segments' bytes.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use super::docs::{DocSet, Lengths};
use super::index::{HEADER_MAX, Header};
use super::postings::{Form, Options, Postings, Reused};
use super::segment::{Area, Builder, MemorySpill, Sink, Spill};
use crate::dead::DeadDocs;
use crate::dictionary::{BlockFetch, Blocks, Dictionary, DictionaryIndex, TermEntry};
use crate::payload::{Payload, PayloadBuilder};
use crate::source::Source;
use crate::{Error, Result, Tid};

/// One segment to merge and its dead documents (by rank).
pub struct Input<'a> {
    pub bytes: &'a [u8],
    pub dead: &'a DeadDocs,
}

/// One segment to merge, read through a source, and its dead documents (by
/// rank).
pub struct SourceInput<'a> {
    pub source: &'a dyn Source,
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
    /// Group containers offered to the builder as an input wrote them.
    pub reused: u64,
}

/// What a streamed merge wrote to its sink: [`Merged`] without the blob.
#[derive(Clone, Copy, Debug)]
pub struct Streamed {
    /// The blob's length.
    pub bytes: u64,
    pub documents: u32,
    pub total_length: u64,
    pub terms: u64,
    pub postings: u64,
    pub reused: u64,
}

/// Terms between two calls of a merge's checkpoint.
const CHECKPOINT_TERMS: usize = 1024;

/// Bytes the builder holds before it spills them; a few in the crate's
/// tests, so that every merge they make spills and reads back.
const SPILL_BYTES: usize = if cfg!(test) { 64 } else { 4 << 20 };

/// Bytes read ahead in an area of an input; a few in the crate's tests, so
/// that their merges move every window many times.
const WINDOW_BYTES: usize = if cfg!(test) { 16 } else { 1 << 20 };

/// Merges `inputs` without their dead documents, calling `checkpoint`
/// every so often (it may fail to stop the merge). `None` when no input
/// holds a live document.
pub fn merge(
    inputs: &[Input<'_>],
    options: Options,
    checkpoint: impl FnMut() -> Result<()>,
) -> Result<Option<Merged>> {
    merge_in_memory(inputs, options, true, checkpoint)
}

/// [`merge`], every record encoded afresh from its postings: no input's
/// container is copied (a re-encoder's check that copying writes the
/// bytes a fresh encoding does).
pub fn merge_afresh(
    inputs: &[Input<'_>],
    options: Options,
    checkpoint: impl FnMut() -> Result<()>,
) -> Result<Option<Merged>> {
    merge_in_memory(inputs, options, false, checkpoint)
}

/// [`merge`] reading through sources and writing the blob to `out`,
/// spilling to `spill` meanwhile. Nothing is written to `out` when no input
/// holds a live document.
pub fn merge_streaming(
    inputs: &[SourceInput<'_>],
    options: Options,
    spill: &mut dyn Spill,
    out: &mut dyn Sink,
    checkpoint: impl FnMut() -> Result<()>,
) -> Result<Option<Streamed>> {
    merge_in(inputs, options, true, spill, out, checkpoint)
}

/// [`merge_streaming`], every record encoded afresh (see [`merge_afresh`]).
pub fn merge_streaming_afresh(
    inputs: &[SourceInput<'_>],
    options: Options,
    spill: &mut dyn Spill,
    out: &mut dyn Sink,
    checkpoint: impl FnMut() -> Result<()>,
) -> Result<Option<Streamed>> {
    merge_in(inputs, options, false, spill, out, checkpoint)
}

fn merge_in_memory(
    inputs: &[Input<'_>],
    options: Options,
    reuse: bool,
    checkpoint: impl FnMut() -> Result<()>,
) -> Result<Option<Merged>> {
    let sources: Vec<SourceInput<'_>> = inputs
        .iter()
        .map(|input| SourceInput {
            source: &input.bytes,
            dead: input.dead,
        })
        .collect();
    let mut blob = Vec::new();
    let merged = merge_in(
        &sources,
        options,
        reuse,
        &mut MemorySpill::default(),
        &mut blob,
        checkpoint,
    )?;
    Ok(merged.map(|merged| Merged {
        blob,
        documents: merged.documents,
        total_length: merged.total_length,
        terms: merged.terms,
        postings: merged.postings,
        reused: merged.reused,
    }))
}

/// The bytes of an area an input read last: `bytes` from `start`.
#[derive(Default)]
struct Window {
    start: u64,
    bytes: Vec<u8>,
}

/// The windows of an input, by the area each walks.
const TERM_MAP: usize = 0;
const POSTINGS: usize = 1;
const POSITIONS: usize = 2;

/// An input as a merge reads it: its header and document set, and a window
/// per area it walks.
struct Reader<'a> {
    source: &'a dyn Source,
    header: Header,
    docs: DocSet,
    windows: [Window; 3],
}

impl<'a> Reader<'a> {
    fn open(source: &'a dyn Source) -> Result<Self> {
        let total = source.len();
        let head = source.read(0, (HEADER_MAX as u64).min(total) as usize)?;
        let header = Header::parse(&head, total)?;
        let docs = DocSet::decode(&Self::area_of(source, &header, Area::DocSet)?)?;
        if docs.geometry.documents != header.documents {
            return Err(Error::Corrupt("document set count"));
        }
        Ok(Self {
            source,
            header,
            docs,
            windows: Default::default(),
        })
    }

    fn area_of(source: &dyn Source, header: &Header, area: Area) -> Result<Vec<u8>> {
        source.read(header.at(area as usize), header.len(area as usize))
    }

    /// The area's bytes, read whole.
    fn area(&self, area: Area) -> Result<Vec<u8>> {
        Self::area_of(self.source, &self.header, area)
    }

    /// Where `area` starts and ends.
    fn bounds(&self, area: Area) -> (u64, u64) {
        (
            self.header.at(area as usize),
            self.header.at(area as usize + 1),
        )
    }

    /// Makes `len` bytes at `offset` within `area` available to
    /// [`Self::slice`] through window `window`, reading ahead.
    fn load(&mut self, window: usize, area: Area, offset: u64, len: usize) -> Result<()> {
        let (start, end) = self.bounds(area);
        let from = start.checked_add(offset).ok_or(Error::Truncated)?;
        let to = from.checked_add(len as u64).ok_or(Error::Truncated)?;
        if to > end {
            return Err(Error::Truncated);
        }
        let held = &self.windows[window];
        if from >= held.start && to <= held.start + held.bytes.len() as u64 {
            return Ok(());
        }
        let take = (len.max(WINDOW_BYTES) as u64).min(end - from) as usize;
        self.windows[window] = Window {
            start: from,
            bytes: self.source.read(from, take)?,
        };
        Ok(())
    }

    /// Bytes [`Self::load`] made available.
    fn slice(&self, window: usize, area: Area, offset: u64, len: usize) -> &[u8] {
        let held = &self.windows[window];
        let at = (self.bounds(area).0 + offset - held.start) as usize;
        &held.bytes[at..at + len]
    }
}

/// One block of a term map's blocks area, as a [`Dictionary`] fetches it.
struct OneBlock<'a> {
    start: usize,
    bytes: &'a [u8],
}

impl BlockFetch for OneBlock<'_> {
    fn fetch_block(&self, offset: usize, len: usize) -> Result<&[u8]> {
        if offset != self.start || len != self.bytes.len() {
            return Err(Error::Corrupt("dictionary block bounds"));
        }
        Ok(self.bytes)
    }
}

/// An input's terms in order, a block of its term map at a time.
struct Terms<'i> {
    index: DictionaryIndex<'i>,
    /// Where the blocks area starts within the term map, and its length.
    blocks_at: u64,
    blocks_len: usize,
    block: usize,
    entries: std::vec::IntoIter<(String, TermEntry)>,
}

impl<'i> Terms<'i> {
    /// The head of an input's term map: its header and index.
    fn head(reader: &Reader<'_>) -> Result<Vec<u8>> {
        let at = reader.header.at(Area::TermMap as usize);
        let len = reader.header.len(Area::TermMap as usize);
        let first = reader.source.read(at, len.min(32))?;
        let head = DictionaryIndex::prefix_len(&first)?;
        if head > len {
            return Err(Error::Truncated);
        }
        reader.source.read(at, head)
    }

    fn new(head: &'i [u8], reader: &Reader<'_>) -> Result<Self> {
        let index = DictionaryIndex::parse(head)?;
        let blocks_len = reader
            .header
            .len(Area::TermMap as usize)
            .checked_sub(index.header_len)
            .ok_or(Error::Truncated)?;
        Ok(Self {
            blocks_at: index.header_len as u64,
            index,
            blocks_len,
            block: 0,
            entries: Vec::new().into_iter(),
        })
    }

    fn next(&mut self, reader: &mut Reader<'_>) -> Result<Option<(String, TermEntry)>> {
        loop {
            if let Some(item) = self.entries.next() {
                return Ok(Some(item));
            }
            if self.block >= self.index.blocks() {
                return Ok(None);
            }
            let (start, end) = self
                .index
                .block_extent(self.block, self.blocks_len)
                .filter(|(start, end)| start <= end && *end <= self.blocks_len)
                .ok_or(Error::Corrupt("dictionary block bounds"))?;
            let at = self.blocks_at + start as u64;
            reader.load(TERM_MAP, Area::TermMap, at, end - start)?;
            let one = OneBlock {
                start,
                bytes: reader.slice(TERM_MAP, Area::TermMap, at, end - start),
            };
            let blocks = Blocks::Lazy {
                len: self.blocks_len,
                fetch: &one,
            };
            self.entries = Dictionary::new(&self.index, blocks)
                .block(self.block)?
                .into_iter();
            self.block += 1;
        }
    }
}

fn merge_in(
    inputs: &[SourceInput<'_>],
    options: Options,
    reuse: bool,
    spill: &mut dyn Spill,
    out: &mut dyn Sink,
    mut checkpoint: impl FnMut() -> Result<()>,
) -> Result<Option<Streamed>> {
    let mut readers = inputs
        .iter()
        .map(|input| Reader::open(input.source))
        .collect::<Result<Vec<_>>>()?;
    // The live documents in ctid order, each with where it came from.
    let mut documents: Vec<(Tid, u32, u32, u32)> = Vec::new();
    // Per input, per group of its own, whether a document there is dead.
    let mut dead_in: Vec<Vec<bool>> = Vec::with_capacity(readers.len());
    for (i, reader) in readers.iter().enumerate() {
        checkpoint()?;
        let geometry = &reader.docs.geometry;
        let area = reader.area(Area::Lengths)?;
        let lengths = Lengths::parse(
            area.as_slice(),
            reader.header.documents,
            reader.header.length_classes,
        )?;
        let mut dead = vec![false; geometry.groups.len()];
        for (rank, tid) in reader.docs.tids().into_iter().enumerate() {
            let rank = rank as u32;
            if !inputs[i].dead.contains(rank) {
                documents.push((tid, lengths.get(rank)?, i as u32, rank));
            } else if let Some(group) = geometry.group_index(tid.block) {
                dead[group] = true;
            }
        }
        dead_in.push(dead);
    }
    documents.sort_unstable_by_key(|d| d.0);
    if documents.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err(Error::Corrupt("a live document in two merge inputs"));
    }
    if documents.is_empty() {
        return Ok(None);
    }
    let mut map: Vec<Vec<u32>> = readers
        .iter()
        .map(|reader| vec![u32::MAX; reader.header.documents as usize])
        .collect();
    for (out, (_, _, input, rank)) in documents.iter().enumerate() {
        map[*input as usize][*rank as usize] = out as u32;
    }
    let tids: Vec<Tid> = documents.iter().map(|d| d.0).collect();
    let lengths: Vec<u32> = documents.iter().map(|d| d.1).collect();
    let total_length = lengths.iter().map(|l| u64::from(*l)).sum();
    let count = documents.len() as u32;
    drop(documents);
    let mut builder = Builder::new(tids, lengths, options)?;
    // Per group of the output, the input (and its group) whose containers
    // it may keep: the only input with a live document there, with the
    // same pages at the same widths, and no dead document there.
    let owners: Vec<Option<(usize, u32)>> = builder
        .geometry()
        .groups
        .iter()
        .enumerate()
        .map(|(o, out)| {
            let mut owner = None;
            for (i, reader) in readers.iter().enumerate() {
                let geometry = &reader.docs.geometry;
                let Ok(g) = geometry.groups.binary_search_by_key(&out.id, |g| g.id) else {
                    continue;
                };
                if owner.is_some() {
                    return None;
                }
                if dead_in[i][g] || !geometry.same_slots(g, builder.geometry(), o) {
                    return None;
                }
                owner = Some((i, g as u32));
            }
            owner
        })
        .collect();
    drop(dead_in);

    let term_heads = readers
        .iter()
        .map(Terms::head)
        .collect::<Result<Vec<_>>>()?;
    let mut walks = readers
        .iter()
        .zip(&term_heads)
        .map(|(reader, head)| Terms::new(head, reader))
        .collect::<Result<Vec<_>>>()?;
    let mut heads: Vec<Option<TermEntry>> = vec![None; readers.len()];
    let mut heap: BinaryHeap<Reverse<(String, usize)>> = BinaryHeap::new();
    for (i, walk) in walks.iter_mut().enumerate() {
        if let Some((term, entry)) = walk.next(&mut readers[i])? {
            heads[i] = Some(entry);
            heap.push(Reverse((term, i)));
        }
    }
    // One term's postings: (merged rank, bucket, positions extent).
    let mut postings: Vec<(u32, u8, u32, u32)> = Vec::new();
    let mut positions: Vec<u32> = Vec::new();
    let mut scratch: Vec<u32> = Vec::new();
    // The inputs holding the term at hand, with its entry in each.
    let mut holders: Vec<(usize, TermEntry)> = Vec::new();
    let (mut terms, mut written, mut reused) = (0u64, 0u64, 0u64);
    let mut since_checkpoint = 0usize;
    while let Some(Reverse((term, first))) = heap.pop() {
        holders.clear();
        holders.push((first, heads[first].take().expect("a head per heap item")));
        while heap.peek().is_some_and(|Reverse((next, _))| *next == term) {
            let Reverse((_, i)) = heap.pop().expect("peeked");
            holders.push((i, heads[i].take().expect("a head per heap item")));
        }
        // Load each holder's record and positions, then step its walk on:
        // the term map reads through a window of its own.
        for &(i, entry) in &holders {
            let reader = &mut readers[i];
            reader.load(
                POSTINGS,
                Area::Postings,
                entry.ordinals.offset,
                entry.ordinals.len as usize,
            )?;
            reader.load(
                POSITIONS,
                Area::Positions,
                entry.payload.offset,
                entry.payload.len as usize,
            )?;
            if let Some((next, entry)) = walks[i].next(reader)? {
                if next <= term {
                    return Err(Error::Unordered);
                }
                heads[i] = Some(entry);
                heap.push(Reverse((next, i)));
            }
        }
        postings.clear();
        positions.clear();
        // The holders' records of the term at hand, by input.
        let mut records: Vec<Option<(Postings<'_>, u32)>> = vec![None; readers.len()];
        for &(i, entry) in &holders {
            let reader = &readers[i];
            let geometry = &reader.docs.geometry;
            let record = reader.slice(
                POSTINGS,
                Area::Postings,
                entry.ordinals.offset,
                entry.ordinals.len as usize,
            );
            let found = Postings::parse(record, entry.df, geometry)?;
            let footer = found.footer(
                geometry,
                reader.header.block_size,
                entry.max_tf_bucket,
                reader.header.adaptive_tf,
            )?;
            let stream = reader.slice(
                POSITIONS,
                Area::Positions,
                entry.payload.offset,
                entry.payload.len as usize,
            );
            let stream = super::positions::Positions::parse(stream)?.payload()?;
            let payload = Payload::parse(&stream)?;
            if payload.count() != entry.df {
                return Err(Error::Corrupt("positions entries against df"));
            }
            let mut cursor = payload.cursor();
            let mut index = 0u32;
            let mut failed = None;
            found.for_each_slot(geometry, |slot| {
                if failed.is_some() {
                    return;
                }
                let step = (|| -> Result<()> {
                    let rank = reader
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
                            footer.bucket(found.tf, index)?,
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
            records[i] = Some((found, entry.df));
        }
        if !postings.is_empty() {
            postings.sort_unstable_by_key(|p| p.0);
            let ranks: Vec<u32> = postings.iter().map(|p| p.0).collect();
            let buckets: Vec<u8> = postings.iter().map(|p| p.1).collect();
            let mut payload = PayloadBuilder::default();
            for p in &postings {
                payload.push(&positions[p.2 as usize..(p.2 + p.3) as usize])?;
            }
            builder.add_term_reusing(&term, &ranks, &buckets, &payload.finish(), &mut |out| {
                if !reuse {
                    return None;
                }
                let (i, g) = owners[out]?;
                let (postings, _) = records[i].as_ref()?;
                let Form::Grouped(dir) = &postings.form else {
                    return None;
                };
                let b = dir.find(g)?;
                let entry = dir.entry_of(b, g, &readers[i].docs.geometry.groups).ok()?;
                reused += 1;
                Some(Reused {
                    kind: entry.kind,
                    bytes: postings.container(&entry).all().ok()?,
                })
            })?;
            terms += 1;
            written += ranks.len() as u64;
            if builder.held() >= SPILL_BYTES {
                builder.spill(spill)?;
            }
        }
        since_checkpoint += 1;
        if since_checkpoint >= CHECKPOINT_TERMS {
            since_checkpoint = 0;
            checkpoint()?;
        }
    }
    drop(map);
    checkpoint()?;
    let (bytes, _) = builder.finish_into(&[], spill, out)?;
    Ok(Some(Streamed {
        bytes,
        documents: count,
        total_length,
        terms,
        postings: written,
        reused,
    }))
}
