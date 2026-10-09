// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Which part of a segment blob a byte belongs to, finer than the reader's
//! own read accounting: the ordinals area is split into each stream's head
//! (count, chunk directory and bounds), its chunks' members, the members'
//! term-frequency nibbles, and the short streams written as delta lists.
//! The split is what a comparison with TIN's page touches needs, where
//! postings footers, payloads and term-frequency tails are counted apart.

use segment::Result;
use segment::segment::Segment;

/// An area of a segment, or a blob beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Area {
    /// The blob header.
    Header,
    /// The term dictionary and its block index.
    Dictionary,
    /// Ordinal stream heads: count, chunk directory and bounds.
    OrdinalHeads,
    /// Ordinal chunk members: arrays and bitmaps.
    OrdinalChunks,
    /// Term-frequency nibbles beside the chunk members.
    Nibbles,
    /// Streams of at most 64 documents: bound, deltas and nibbles together.
    OrdinalLists,
    /// Positions: payload headers, skip tables and position data.
    Positions,
    /// The document table's heap offsets.
    Documents,
    /// Document lengths.
    Lengths,
    /// Document length classes.
    Classes,
    /// The document table's page table.
    PageTable,
    /// A dead list, read beside the segment.
    DeadList,
}

/// Number of [`Area`]s.
pub const AREAS: usize = 12;

impl Area {
    pub const ALL: [Area; AREAS] = [
        Area::Header,
        Area::Dictionary,
        Area::OrdinalHeads,
        Area::OrdinalChunks,
        Area::Nibbles,
        Area::OrdinalLists,
        Area::Positions,
        Area::Documents,
        Area::Lengths,
        Area::Classes,
        Area::PageTable,
        Area::DeadList,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Area::Header => "header",
            Area::Dictionary => "dictionary",
            Area::OrdinalHeads => "ordinal_heads",
            Area::OrdinalChunks => "ordinal_chunks",
            Area::Nibbles => "tf_nibbles",
            Area::OrdinalLists => "ordinal_lists",
            Area::Positions => "positions",
            Area::Documents => "doc_table",
            Area::Lengths => "lengths",
            Area::Classes => "length_classes",
            Area::PageTable => "page_table",
            Area::DeadList => "dead_list",
        }
    }

    /// The closest of TIN's page-touch categories (Metadata, Term Map,
    /// Postings Footer, Postings Payload, Postings TF Tail, DL Sidecar,
    /// Positions, Liveness Bitmap); `None` where TIN has no counterpart:
    /// TIN's postings carry document identifiers, Stannum's carry ordinals
    /// that the document table maps to heap locations.
    pub fn tin(self) -> Option<&'static str> {
        Some(match self {
            Area::Header => "Metadata",
            Area::Dictionary => "Term Map",
            Area::OrdinalHeads => "Postings Footer",
            Area::OrdinalChunks | Area::OrdinalLists => "Postings Payload",
            Area::Nibbles => "Postings TF Tail",
            Area::Lengths | Area::Classes => "DL Sidecar",
            Area::Positions => "Positions",
            Area::DeadList => "Liveness Bitmap",
            Area::Documents | Area::PageTable => return None,
        })
    }
}

/// Section starts of a blob and the ordinal spans within its ordinals area.
pub struct AreaMap {
    dictionary_at: u64,
    ordinals_at: u64,
    payload_at: u64,
    offsets_at: u64,
    lengths_at: u64,
    classes_at: u64,
    pages_at: u64,
    /// Within the ordinals area: (blob offset, area) at which each span
    /// starts, ascending; a span runs to the next one's start.
    spans: Vec<(u64, Area)>,
}

fn varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

impl AreaMap {
    /// The map of `blob`, a segment in the current format.
    pub fn of(blob: &[u8]) -> Result<Self> {
        let mut at = 4;
        let corrupt = || segment::Error::Corrupt("segment header");
        let doc_count = varint(blob, &mut at).ok_or_else(corrupt)?;
        let _total_length = varint(blob, &mut at).ok_or_else(corrupt)?;
        let dictionary_len = varint(blob, &mut at).ok_or_else(corrupt)?;
        let ordinals_len = varint(blob, &mut at).ok_or_else(corrupt)?;
        let payload_len = varint(blob, &mut at).ok_or_else(corrupt)?;
        let _pages_len = varint(blob, &mut at).ok_or_else(corrupt)?;
        let dictionary_at = at as u64;
        let ordinals_at = dictionary_at + dictionary_len;
        let payload_at = ordinals_at + ordinals_len;
        let offsets_at = payload_at + payload_len;
        let lengths_at = offsets_at + doc_count * 2;
        let classes_at = lengths_at + doc_count * 4;
        let pages_at = classes_at + doc_count;
        let mut spans = Vec::new();
        let segment = Segment::parse(blob)?;
        for item in segment.dictionary()?.iter() {
            let (_, entry) = item?;
            let start = ordinals_at + entry.ordinals.offset;
            let len = entry.ordinals.len as usize;
            let stream = &blob[start as usize..start as usize + len];
            Self::stream_spans(stream, start, &mut spans).ok_or_else(corrupt)?;
        }
        spans.sort_unstable_by_key(|(offset, _)| *offset);
        Ok(Self {
            dictionary_at,
            ordinals_at,
            payload_at,
            offsets_at,
            lengths_at,
            classes_at,
            pages_at,
            spans,
        })
    }

    /// The spans of one term's stream, which starts at blob offset `start`.
    fn stream_spans(stream: &[u8], start: u64, out: &mut Vec<(u64, Area)>) -> Option<()> {
        let mut at = 0;
        let count = varint(stream, &mut at)?;
        if count <= segment::ordinals::LIST_MAX as u64 {
            out.push((start, Area::OrdinalLists));
            return Some(());
        }
        out.push((start, Area::OrdinalHeads));
        let chunks = varint(stream, &mut at)? as usize;
        let bounds_len = varint(stream, &mut at)? as usize;
        let directory = at;
        let chunks_at = directory + chunks * 8 + bounds_len;
        for c in 0..chunks {
            let entry = stream.get(directory + c * 8..directory + c * 8 + 8)?;
            let cardinality = usize::from(u16::from_le_bytes([entry[2], entry[3]])) + 1;
            let offset = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]);
            let bitmap = offset & (1 << 31) != 0;
            let chunk = chunks_at + (offset & !(1 << 31)) as usize;
            let members = if bitmap { 8192 } else { cardinality * 2 };
            out.push((start + chunk as u64, Area::OrdinalChunks));
            out.push((start + (chunk + members) as u64, Area::Nibbles));
        }
        Some(())
    }

    /// The area of blob offset `offset`.
    pub fn area_of(&self, offset: u64) -> Area {
        if offset < self.dictionary_at {
            Area::Header
        } else if offset < self.ordinals_at {
            Area::Dictionary
        } else if offset < self.payload_at {
            let at = self.spans.partition_point(|(start, _)| *start <= offset);
            match at {
                0 => Area::OrdinalHeads,
                n => self.spans[n - 1].1,
            }
        } else if offset < self.offsets_at {
            Area::Positions
        } else if offset < self.lengths_at {
            Area::Documents
        } else if offset < self.classes_at {
            Area::Lengths
        } else if offset < self.pages_at {
            Area::Classes
        } else {
            Area::PageTable
        }
    }
}
