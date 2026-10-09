// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A segment's documents: the ctid grid every posting set is addressed in,
//! the document set with its rank directory, the exact document lengths
//! (the DL sidecar) and the liveness bitmap.
//!
//! # The ctid grid
//!
//! Heap blocks are taken in groups of [`GROUP_PAGES`]. A group the segment
//! holds documents in has a width `w`, the largest line pointer offset of
//! those documents, and `256 * w` slots: tuple `(block, offset)` is slot
//! `(block % 256) * w + offset - 1` of group `block / 256`. The groups'
//! slots are numbered in a row, so a slot names a ctid and back with one
//! table of groups and a multiply, and slots sort as ctids do. A term's
//! postings are a set of slots ([`super::postings`]), so two terms' sets in a
//! group are bitmaps of the same shape and combine word by word.
//!
//! ```text
//! docset   := groups varint,
//!             (group u32le, width u16le, rank_base u32le, kind u8)*,
//!             body* (per group, in order)
//! body     := grid:   32 * width bytes, the group's slots as a bitmap
//!           | counts: 256 bytes, page p holding offsets 1..=count[p]
//! dl       := escapes varint, u16le per document by rank (0xffff: escaped),
//!             (rank u32le, length u32le)* for the escaped, by rank
//! liveness := dead varint, then when dead > 0 a bitmap of u64le words over
//!             document ranks, a set bit naming a dead document
//! ```
//!
//! A document's rank is its position in ctid order, which is the order of
//! its slot; `rank_base` is the number of documents in earlier groups.

use super::bits;
use crate::{Error, Result, Tid, varint};

/// Heap blocks per group.
pub const GROUP_PAGES: u32 = 256;
/// The largest line pointer offset an 8 KiB heap page can hold.
pub const MAX_OFFSET: u16 = 291;
/// Bytes of one directory entry of the document set.
const ENTRY: usize = 11;
/// A stored length that names an escaped one.
const ESCAPE: u16 = u16::MAX;

const KIND_GRID: u8 = 0;
const KIND_COUNTS: u8 = 1;

/// One group of heap blocks the segment holds documents in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group {
    /// `block / 256` of its blocks.
    pub id: u32,
    /// Its largest offset: slots per page.
    pub width: u16,
    /// The slot its first page's first offset maps to.
    pub slot_base: u32,
    /// Documents in earlier groups.
    pub rank_base: u32,
}

impl Group {
    /// Slots in the group.
    pub const fn slots(&self) -> u32 {
        GROUP_PAGES * self.width as u32
    }

    /// 64-bit words of a bitmap over its slots.
    pub const fn words(&self) -> usize {
        (GROUP_PAGES as usize * self.width as usize).div_ceil(64)
    }
}

/// The groups of a segment, mapping ctids to slots and back.
#[derive(Clone, Debug, Default)]
pub struct Geometry {
    pub groups: Vec<Group>,
    /// Slots in every group together.
    pub slots: u32,
    pub documents: u32,
}

impl Geometry {
    /// The grid of `tids`, which must be strictly increasing.
    pub fn of(tids: &[Tid]) -> Result<Self> {
        let mut groups: Vec<Group> = Vec::new();
        let mut previous: Option<Tid> = None;
        for (rank, tid) in tids.iter().enumerate() {
            if tid.offset == 0 || tid.offset > MAX_OFFSET {
                return Err(Error::InvalidTid);
            }
            if previous.is_some_and(|p| p >= *tid) {
                return Err(Error::Unordered);
            }
            previous = Some(*tid);
            let id = tid.block / GROUP_PAGES;
            match groups.last_mut() {
                Some(group) if group.id == id => group.width = group.width.max(tid.offset),
                _ => groups.push(Group {
                    id,
                    width: tid.offset,
                    slot_base: 0,
                    rank_base: rank as u32,
                }),
            }
        }
        let mut slots = 0u32;
        for group in &mut groups {
            group.slot_base = slots;
            slots = slots
                .checked_add(group.slots())
                .ok_or(Error::Corrupt("grid exceeds 32 bits"))?;
        }
        Ok(Self {
            groups,
            slots,
            documents: tids.len() as u32,
        })
    }

    /// The index of the group holding heap block `block`, if any.
    pub fn group_index(&self, block: u32) -> Option<usize> {
        let id = block / GROUP_PAGES;
        self.groups.binary_search_by_key(&id, |g| g.id).ok()
    }

    /// The slot of `tid`, `None` when its group is not in the segment or its
    /// offset is beyond the group's width.
    pub fn slot_of(&self, tid: Tid) -> Option<u32> {
        let group = &self.groups[self.group_index(tid.block)?];
        if tid.offset == 0 || tid.offset > group.width {
            return None;
        }
        Some(
            group.slot_base
                + (tid.block % GROUP_PAGES) * u32::from(group.width)
                + u32::from(tid.offset)
                - 1,
        )
    }

    /// The group holding `slot`.
    pub fn group_of_slot(&self, slot: u32) -> usize {
        self.groups.partition_point(|g| g.slot_base <= slot) - 1
    }

    /// The ctid of slot `local` of group `index`.
    #[inline]
    pub fn tid_in(&self, index: usize, local: u32) -> Tid {
        let group = &self.groups[index];
        let width = u32::from(group.width);
        Tid {
            block: group.id * GROUP_PAGES + local / width,
            offset: (local % width) as u16 + 1,
        }
    }

    /// The ctid of `slot`.
    pub fn tid_of(&self, slot: u32) -> Tid {
        let index = self.group_of_slot(slot);
        self.tid_in(index, slot - self.groups[index].slot_base)
    }
}

/// Builds the document set of `tids` (strictly increasing).
pub fn encode_docset(geometry: &Geometry, tids: &[Tid]) -> Vec<u8> {
    let mut out = Vec::new();
    varint::put(&mut out, geometry.groups.len() as u64);
    let mut bodies = Vec::new();
    let mut at = 0usize;
    for group in &geometry.groups {
        let start = at;
        while at < tids.len() && tids[at].block / GROUP_PAGES == group.id {
            at += 1;
        }
        let members = &tids[start..at];
        // Pages that hold exactly offsets 1..=n need only n.
        let mut counts = [0u16; GROUP_PAGES as usize];
        let mut prefix = true;
        for tid in members {
            let page = (tid.block % GROUP_PAGES) as usize;
            if tid.offset != counts[page] + 1 {
                prefix = false;
            }
            counts[page] += 1;
        }
        let kind = if prefix && group.width <= 255 && 256 < 32 * group.width as usize {
            KIND_COUNTS
        } else {
            KIND_GRID
        };
        out.extend_from_slice(&group.id.to_le_bytes());
        out.extend_from_slice(&group.width.to_le_bytes());
        out.extend_from_slice(&group.rank_base.to_le_bytes());
        out.push(kind);
        if kind == KIND_COUNTS {
            bodies.extend(counts.iter().map(|c| *c as u8));
        } else {
            let mut grid = vec![0u8; 32 * group.width as usize];
            let width = u32::from(group.width);
            for tid in members {
                let local = (tid.block % GROUP_PAGES) * width + u32::from(tid.offset) - 1;
                grid[local as usize / 8] |= 1 << (local % 8);
            }
            bodies.extend_from_slice(&grid);
        }
    }
    out.extend_from_slice(&bodies);
    out
}

/// The document set decoded: the grid, a bitmap over every slot that holds
/// a document, and the rank of each bitmap word's first slot.
#[derive(Clone, Debug, Default)]
pub struct DocSet {
    pub geometry: Geometry,
    /// Per group, its slots as words (group `i`'s words start at
    /// `word_base[i]`).
    pub words: Vec<u64>,
    pub word_base: Vec<usize>,
    /// Documents before each word.
    word_rank: Vec<u32>,
}

impl DocSet {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut at = 0usize;
        let count = varint::get_u32(bytes, &mut at)? as usize;
        let end = count
            .checked_mul(ENTRY)
            .and_then(|len| at.checked_add(len))
            .ok_or(Error::Truncated)?;
        let directory = bytes.get(at..end).ok_or(Error::Truncated)?;
        at = end;
        let mut geometry = Geometry::default();
        let mut words = Vec::new();
        let mut word_base = Vec::with_capacity(count);
        let mut word_rank = Vec::new();
        let mut rank = 0u32;
        for entry in directory.chunks_exact(ENTRY) {
            let id = u32::from_le_bytes(entry[0..4].try_into().expect("four bytes"));
            let width = u16::from_le_bytes(entry[4..6].try_into().expect("two bytes"));
            let rank_base = u32::from_le_bytes(entry[6..10].try_into().expect("four bytes"));
            let kind = entry[10];
            if width == 0 || width > MAX_OFFSET || rank_base != rank {
                return Err(Error::Corrupt("document set directory"));
            }
            if geometry.groups.last().is_some_and(|g| g.id >= id) {
                return Err(Error::Corrupt("document set order"));
            }
            let group = Group {
                id,
                width,
                slot_base: geometry.slots,
                rank_base,
            };
            geometry.slots = geometry
                .slots
                .checked_add(group.slots())
                .ok_or(Error::Corrupt("grid exceeds 32 bits"))?;
            let base = words.len();
            word_base.push(base);
            words.resize(base + group.words(), 0u64);
            let grid = &mut words[base..];
            match kind {
                KIND_GRID => {
                    let body = bytes
                        .get(at..at + 32 * width as usize)
                        .ok_or(Error::Truncated)?;
                    at += body.len();
                    for (i, word) in grid.iter_mut().enumerate() {
                        *word = bits::word(body, i);
                    }
                }
                KIND_COUNTS => {
                    let body = bytes.get(at..at + 256).ok_or(Error::Truncated)?;
                    at += 256;
                    for (page, &n) in body.iter().enumerate() {
                        if u16::from(n) > width {
                            return Err(Error::Corrupt("document set page count"));
                        }
                        let first = page * width as usize;
                        for local in first..first + n as usize {
                            grid[local / 64] |= 1 << (local % 64);
                        }
                    }
                }
                _ => return Err(Error::Corrupt("document set kind")),
            }
            for word in grid.iter() {
                word_rank.push(rank);
                rank += word.count_ones();
            }
            geometry.groups.push(group);
        }
        if at != bytes.len() {
            return Err(Error::Corrupt("document set length"));
        }
        geometry.documents = rank;
        Ok(Self {
            geometry,
            words,
            word_base,
            word_rank,
        })
    }

    /// The rank of the document at slot `local` of group `index`, `None`
    /// when no document is there.
    #[inline]
    pub fn rank_in(&self, index: usize, local: u32) -> Option<u32> {
        let word = self.word_base[index] + local as usize / 64;
        let bits = self.words[word];
        let bit = local % 64;
        if bits >> bit & 1 == 0 {
            return None;
        }
        Some(self.word_rank[word] + (bits & ((1u64 << bit) - 1)).count_ones())
    }

    /// The rank of the document at `slot`.
    pub fn rank(&self, slot: u32) -> Option<u32> {
        let index = self.geometry.group_of_slot(slot);
        self.rank_in(index, slot - self.geometry.groups[index].slot_base)
    }

    /// Every document's ctid, in order.
    pub fn tids(&self) -> Vec<Tid> {
        let mut out = Vec::with_capacity(self.geometry.documents as usize);
        for (index, group) in self.geometry.groups.iter().enumerate() {
            let base = self.word_base[index];
            for w in 0..group.words() {
                let mut word = self.words[base + w];
                while word != 0 {
                    let local = (w * 64) as u32 + word.trailing_zeros();
                    out.push(self.geometry.tid_in(index, local));
                    word &= word - 1;
                }
            }
        }
        out
    }

    /// The group `index`'s slots holding documents.
    pub fn group_words(&self, index: usize) -> &[u64] {
        let base = self.word_base[index];
        &self.words[base..base + self.geometry.groups[index].words()]
    }
}

/// Encodes exact document lengths, by rank.
pub fn encode_lengths(lengths: &[u32]) -> Vec<u8> {
    let escaped: Vec<(u32, u32)> = lengths
        .iter()
        .enumerate()
        .filter(|(_, l)| **l >= u32::from(ESCAPE))
        .map(|(rank, l)| (rank as u32, *l))
        .collect();
    let mut out = Vec::with_capacity(lengths.len() * 2 + escaped.len() * 8 + 4);
    varint::put(&mut out, escaped.len() as u64);
    for length in lengths {
        let stored = if *length >= u32::from(ESCAPE) {
            ESCAPE
        } else {
            *length as u16
        };
        out.extend_from_slice(&stored.to_le_bytes());
    }
    for (rank, length) in escaped {
        out.extend_from_slice(&rank.to_le_bytes());
        out.extend_from_slice(&length.to_le_bytes());
    }
    out
}

/// The DL sidecar: exact lengths by document rank.
#[derive(Clone, Copy, Debug)]
pub struct Lengths<'a> {
    /// Where the stored lengths start within the sidecar's bytes.
    pub stored_at: usize,
    stored: &'a [u8],
    escaped: &'a [u8],
}

impl<'a> Lengths<'a> {
    pub fn parse(bytes: &'a [u8], documents: u32) -> Result<Self> {
        let mut at = 0;
        let escapes = varint::get_u32(bytes, &mut at)? as usize;
        let stored_len = documents as usize * 2;
        let stored = bytes.get(at..at + stored_len).ok_or(Error::Truncated)?;
        let escaped = &bytes[at + stored_len..];
        if escaped.len() != escapes * 8 {
            return Err(Error::Corrupt("length escapes"));
        }
        Ok(Self {
            stored_at: at,
            stored,
            escaped,
        })
    }

    /// The length of the document at `rank`.
    #[inline]
    pub fn get(&self, rank: u32) -> Result<u32> {
        let at = rank as usize * 2;
        let stored = u16::from_le_bytes(
            self.stored
                .get(at..at + 2)
                .ok_or(Error::Corrupt("length rank"))?
                .try_into()
                .expect("two bytes"),
        );
        if stored != ESCAPE {
            return Ok(u32::from(stored));
        }
        let entries = self.escaped.len() / 8;
        let entry = |i: usize| {
            let e = &self.escaped[i * 8..i * 8 + 8];
            (
                u32::from_le_bytes(e[0..4].try_into().expect("four bytes")),
                u32::from_le_bytes(e[4..8].try_into().expect("four bytes")),
            )
        };
        let (mut lo, mut hi) = (0, entries);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match entry(mid).0.cmp(&rank) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(entry(mid).1),
            }
        }
        Err(Error::Corrupt("escaped length missing"))
    }
}

/// Encodes the liveness bitmap of `documents` documents, `dead` the ranks
/// of the dead ones, ascending.
pub fn encode_liveness(documents: u32, dead: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    varint::put(&mut out, dead.len() as u64);
    if dead.is_empty() {
        return out;
    }
    let mut words = vec![0u64; (documents as usize).div_ceil(64)];
    for rank in dead {
        words[*rank as usize / 64] |= 1 << (rank % 64);
    }
    for word in words {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out
}

/// Dead documents in slot space: per group holding any, a bitmap over its
/// slots, so a posting set's words are cleared word by word.
#[derive(Clone, Debug, Default)]
pub struct Liveness {
    pub dead: u32,
    /// Per group, its dead slots, `None` when every document is live.
    pub groups: Vec<Option<Box<[u64]>>>,
}

impl Liveness {
    /// Every document live.
    pub fn all_live(docs: &DocSet) -> Self {
        Self {
            dead: 0,
            groups: vec![None; docs.geometry.groups.len()],
        }
    }

    /// Decodes a stored liveness bitmap into slot space through the
    /// document set: a VACUUM publishes it, every reader decodes it once.
    pub fn decode(bytes: &[u8], docs: &DocSet) -> Result<Self> {
        let mut at = 0;
        let dead = varint::get_u32(bytes, &mut at)?;
        let mut groups: Vec<Option<Box<[u64]>>> = vec![None; docs.geometry.groups.len()];
        if dead == 0 {
            if at != bytes.len() {
                return Err(Error::Corrupt("liveness length"));
            }
            return Ok(Self { dead, groups });
        }
        let documents = docs.geometry.documents as usize;
        let words = &bytes[at..];
        if words.len() != documents.div_ceil(64) * 8 {
            return Err(Error::Corrupt("liveness length"));
        }
        let mut found = 0u32;
        for (index, group) in docs.geometry.groups.iter().enumerate() {
            let slots = docs.group_words(index);
            let mut rank = group.rank_base as usize;
            for (w, word) in slots.iter().enumerate() {
                let mut word = *word;
                while word != 0 {
                    let bit = word.trailing_zeros() as usize;
                    if bits::word(words, rank / 64) >> (rank % 64) & 1 == 1 {
                        let map = groups[index]
                            .get_or_insert_with(|| vec![0u64; group.words()].into_boxed_slice());
                        map[w] |= 1 << bit;
                        found += 1;
                    }
                    rank += 1;
                    word &= word - 1;
                }
            }
        }
        if found != dead {
            return Err(Error::Corrupt("liveness count"));
        }
        Ok(Self { dead, groups })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn tids(pairs: &[(u32, u16)]) -> Vec<Tid> {
        pairs
            .iter()
            .map(|&(block, offset)| Tid { block, offset })
            .collect()
    }

    fn check(tids: &[Tid]) {
        let geometry = Geometry::of(tids).unwrap();
        let bytes = encode_docset(&geometry, tids);
        let docs = DocSet::decode(&bytes).unwrap();
        assert_eq!(docs.geometry.groups, geometry.groups);
        assert_eq!(docs.geometry.slots, geometry.slots);
        assert_eq!(docs.tids(), tids);
        let mut previous = None;
        for (rank, tid) in tids.iter().enumerate() {
            let slot = geometry.slot_of(*tid).unwrap();
            assert!(previous.is_none_or(|p| p < slot), "slots sort as ctids");
            previous = Some(slot);
            assert_eq!(geometry.tid_of(slot), *tid);
            assert_eq!(docs.rank(slot), Some(rank as u32));
        }
    }

    #[test]
    fn grids_of_edge_offsets() {
        check(&tids(&[(0, 1)]));
        check(&tids(&[(0, 291)]));
        check(&tids(&[(0, 1), (0, 291), (255, 1), (256, 2), (1 << 20, 7)]));
        // A page holding 1..=n everywhere is stored as counts.
        let full: Vec<Tid> = (0..256u32)
            .flat_map(|b| {
                (1..=20u16).map(move |o| Tid {
                    block: b,
                    offset: o,
                })
            })
            .collect();
        let geometry = Geometry::of(&full).unwrap();
        assert_eq!(encode_docset(&geometry, &full).len(), 1 + ENTRY + 256);
        check(&full);
        assert!(Geometry::of(&tids(&[(0, 0)])).is_err());
        assert!(Geometry::of(&tids(&[(0, 292)])).is_err());
        assert!(Geometry::of(&tids(&[(0, 2), (0, 1)])).is_err());
    }

    #[test]
    fn lengths_escape() {
        let lengths = [1, 65_534, 65_535, 70_000, 3, u32::MAX];
        let bytes = encode_lengths(&lengths);
        let parsed = Lengths::parse(&bytes, lengths.len() as u32).unwrap();
        for (rank, length) in lengths.iter().enumerate() {
            assert_eq!(parsed.get(rank as u32).unwrap(), *length);
        }
        assert!(parsed.get(lengths.len() as u32).is_err());
    }

    #[test]
    fn liveness_maps_ranks_to_slots() {
        let tids = tids(&[(0, 1), (0, 3), (1, 2), (300, 5)]);
        let geometry = Geometry::of(&tids).unwrap();
        let docs = DocSet::decode(&encode_docset(&geometry, &tids)).unwrap();
        let live = Liveness::decode(&encode_liveness(4, &[]), &docs).unwrap();
        assert!(live.groups.iter().all(Option::is_none));
        let live = Liveness::decode(&encode_liveness(4, &[1, 3]), &docs).unwrap();
        assert_eq!(live.dead, 2);
        for (rank, tid) in tids.iter().enumerate() {
            let slot = geometry.slot_of(*tid).unwrap();
            let index = geometry.group_of_slot(slot);
            let local = slot - geometry.groups[index].slot_base;
            let dead = live.groups[index]
                .as_ref()
                .is_some_and(|w| w[local as usize / 64] >> (local % 64) & 1 == 1);
            assert_eq!(dead, rank == 1 || rank == 3);
        }
        assert!(Liveness::decode(&encode_liveness(4, &[1]), &docs).is_ok());
        let mut wrong = encode_liveness(4, &[1]);
        wrong[0] = 2;
        assert!(Liveness::decode(&wrong, &docs).is_err());
    }

    proptest! {
        #[test]
        fn random_document_sets(set in prop::collection::btree_set((0u32..2000, 1u16..=291), 1..500)) {
            let tids: Vec<Tid> = set.into_iter().map(|(block, offset)| Tid { block, offset }).collect();
            check(&tids);
        }
    }
}
