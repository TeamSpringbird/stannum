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
//! holds documents in has a first page `f`, a span of `p` pages to its last
//! (all 256 but at a heap's or a segment's edges), and per page a width
//! `w[i]`, the largest line pointer offset of the page's documents (0 for a
//! page without one). Page `i` of the span owns slots `start[i] ..
//! start[i] + w[i]`, `start[i]` the sum of the widths before it, so tuple
//! `(block, offset)` is slot `start[block % 256 - f] + offset - 1` of group
//! `block / 256`. A page holding its rows at offsets `1..=n` (every page of
//! a heap that was only inserted into) costs `n` slots, one per document,
//! as TIN sizes its page masks to each page's rows. The groups' slots are
//! numbered in a row, so slots sort as ctids do; a slot's ctid is a short
//! search in its group's page starts, which only a candidate's ctid needs.
//! A term's postings are a set of slots ([`super::postings`]), so two
//! terms' sets in a group are bitmaps of the same shape and combine word by
//! word.
//!
//! ```text
//! docset   := groups varint,
//!             (group u32le, first u8, pages - 1 u8, rank_base u32le,
//!              kind u8)*,
//!             body* (per group, in order)
//! body     := counts: a byte per page, page i holding offsets 1..=count[i]
//!                     (its width count[i])
//!           | grid:   a u16le width per page, then the group's slots as a
//!                     bitmap of whole u64le words
//! dl       := block varint (documents per block), blocks varint,
//!             (bits_at u32le, base u24le, width u8)* per block,
//!             per block its lengths less its base, packed at its width
//!             (byte-aligned), by rank
//! liveness := dead varint, then when dead > 0 a bitmap of u64le words over
//!             document ranks, a set bit naming a dead document
//! ```
//!
//! A document's rank is its position in ctid order, which is the order of
//! its slot; `rank_base` is the number of documents in earlier groups.

use super::bits;
use super::blob::Bytes;
use crate::{Error, Result, Tid, varint};

/// Heap blocks per group.
pub const GROUP_PAGES: u32 = 256;
/// The largest line pointer offset an 8 KiB heap page can hold.
pub const MAX_OFFSET: u16 = 291;
/// Bytes of one directory entry of the document set.
const ENTRY: usize = 11;
const KIND_GRID: u8 = 0;
const KIND_COUNTS: u8 = 1;

/// One group of heap blocks the segment holds documents in. Its pages'
/// widths are the [`Geometry`]'s ([`Geometry::starts`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group {
    /// `block / 256` of its blocks.
    pub id: u32,
    /// Its first page (`block % 256`), and the pages from there to its last.
    pub first: u8,
    pub pages: u16,
    /// The slot its first page's first offset maps to.
    pub slot_base: u32,
    /// Documents in earlier groups.
    pub rank_base: u32,
    /// Its slots: the sum of its pages' widths.
    slot_count: u32,
    /// Where its page starts (`pages + 1` of them) and its word hints begin
    /// in the geometry's tables.
    starts_at: u32,
    hints_at: u32,
}

impl Group {
    /// Slots in the group.
    #[inline]
    pub const fn slots(&self) -> u32 {
        self.slot_count
    }

    /// 64-bit words of a bitmap over its slots.
    #[inline]
    pub const fn words(&self) -> usize {
        (self.slot_count as usize).div_ceil(64)
    }

    /// Bytes of a grid over its slots: whole words.
    pub const fn grid_bytes(&self) -> usize {
        self.words() * 8
    }
}

/// The groups of a segment, mapping ctids to slots and back.
#[derive(Clone, Debug, Default)]
pub struct Geometry {
    pub groups: Vec<Group>,
    /// Slots in every group together.
    pub slots: u32,
    pub documents: u32,
    /// Per group, the local slot each page of its span starts at, then its
    /// slot count: `pages + 1` entries, ascending.
    starts: Vec<u32>,
    /// Per group, per 64-slot word of a bitmap over it, the page (of its
    /// span) the word's first slot is on: where a slot's page search starts.
    hints: Vec<u8>,
}

impl Geometry {
    /// The grid of `tids`, which must be strictly increasing: each page's
    /// width is its largest offset.
    pub fn of(tids: &[Tid]) -> Result<Self> {
        let mut geometry = Self::default();
        let mut previous: Option<Tid> = None;
        // The group at hand: its id, first page and first rank, and its
        // pages' widths so far.
        let mut open: Option<(u32, u8, u32)> = None;
        let mut widths: Vec<u16> = Vec::with_capacity(GROUP_PAGES as usize);
        for (rank, tid) in tids.iter().enumerate() {
            if tid.offset == 0 || tid.offset > MAX_OFFSET {
                return Err(Error::InvalidTid);
            }
            if previous.is_some_and(|p| p >= *tid) {
                return Err(Error::Unordered);
            }
            previous = Some(*tid);
            let id = tid.block / GROUP_PAGES;
            let page = tid.block % GROUP_PAGES;
            let first = match open {
                Some((open_id, first, _)) if open_id == id => first,
                _ => {
                    if let Some((id, first, rank_base)) = open {
                        geometry.push_group(id, first, &widths, rank_base)?;
                    }
                    open = Some((id, page as u8, rank as u32));
                    widths.clear();
                    page as u8
                }
            };
            let at = (page - u32::from(first)) as usize;
            if widths.len() <= at {
                widths.resize(at + 1, 0);
            }
            widths[at] = widths[at].max(tid.offset);
        }
        if let Some((id, first, rank_base)) = open {
            geometry.push_group(id, first, &widths, rank_base)?;
        }
        geometry.documents = tids.len() as u32;
        Ok(geometry)
    }

    /// Appends group `id` holding `rank_base` documents before it, its span
    /// starting at page `first` with a page per width. Groups come in
    /// ascending order; `documents` is the caller's to set.
    pub fn push_group(&mut self, id: u32, first: u8, widths: &[u16], rank_base: u32) -> Result<()> {
        if widths.is_empty()
            || u32::from(first) + widths.len() as u32 > GROUP_PAGES
            || widths.iter().any(|w| *w > MAX_OFFSET)
        {
            return Err(Error::Corrupt("group geometry"));
        }
        if self.groups.last().is_some_and(|g| g.id >= id) {
            return Err(Error::Corrupt("group order"));
        }
        let starts_at = self.starts.len();
        let mut slots = 0u32;
        for width in widths {
            self.starts.push(slots);
            slots += u32::from(*width);
        }
        self.starts.push(slots);
        let hints_at = self.hints.len();
        let starts = &self.starts[starts_at..];
        let mut page = 0usize;
        for word in 0..slots.div_ceil(64) {
            while starts[page + 1] <= word * 64 {
                page += 1;
            }
            self.hints.push(page as u8);
        }
        let slot_base = self.slots;
        self.slots = slot_base
            .checked_add(slots)
            .ok_or(Error::Corrupt("grid exceeds 32 bits"))?;
        self.groups.push(Group {
            id,
            first,
            pages: widths.len() as u16,
            slot_base,
            rank_base,
            slot_count: slots,
            starts_at: starts_at as u32,
            hints_at: hints_at as u32,
        });
        Ok(())
    }

    /// Group `index`'s page starts: the local slot of each page of its span,
    /// then its slot count.
    #[inline]
    pub fn starts(&self, index: usize) -> &[u32] {
        let group = &self.groups[index];
        &self.starts[group.starts_at as usize..][..usize::from(group.pages) + 1]
    }

    /// The width of page `page` (of its span) of group `index`.
    #[inline]
    pub fn width(&self, index: usize, page: usize) -> u32 {
        let starts = self.starts(index);
        starts[page + 1] - starts[page]
    }

    /// Whether group `index` here and group `other_index` of `other` lie on
    /// the same pages at the same widths, so their slots are the same.
    pub fn same_slots(&self, index: usize, other: &Geometry, other_index: usize) -> bool {
        let (a, b) = (&self.groups[index], &other.groups[other_index]);
        (a.id, a.first) == (b.id, b.first) && self.starts(index) == other.starts(other_index)
    }

    /// The local slot of `(page, offset)` in group `index`, `page` being
    /// `block % 256` and `offset` within the page's width.
    #[inline]
    pub fn local(&self, index: usize, page: u32, offset: u16) -> u32 {
        let first = u32::from(self.groups[index].first);
        self.starts(index)[(page - first) as usize] + u32::from(offset) - 1
    }

    /// The page (of its span) holding local slot `local` of group `index`:
    /// from the page its word starts on, a step per page start in the word
    /// (pages are a few dozen slots wide). A slot past the group's (a
    /// corrupt container's) falls on its last page.
    #[inline]
    pub fn page_of(&self, index: usize, local: u32) -> usize {
        let group = &self.groups[index];
        let starts = self.starts(index);
        let last = starts.len() - 2;
        let hint = self
            .hints
            .get(group.hints_at as usize + local as usize / 64)
            .filter(|_| local < group.slot_count);
        let Some(&hint) = hint else {
            return last;
        };
        let mut page = usize::from(hint);
        while starts[page + 1] <= local {
            page += 1;
        }
        page
    }

    /// The index of the group holding heap block `block`, if any.
    pub fn group_index(&self, block: u32) -> Option<usize> {
        let id = block / GROUP_PAGES;
        self.groups.binary_search_by_key(&id, |g| g.id).ok()
    }

    /// The slot of `tid`, `None` when its group is not in the segment or its
    /// offset is beyond its page's width.
    pub fn slot_of(&self, tid: Tid) -> Option<u32> {
        let index = self.group_index(tid.block)?;
        let group = &self.groups[index];
        let page = (tid.block % GROUP_PAGES).checked_sub(u32::from(group.first))?;
        if tid.offset == 0
            || page >= u32::from(group.pages)
            || u32::from(tid.offset) > self.width(index, page as usize)
        {
            return None;
        }
        Some(group.slot_base + self.starts(index)[page as usize] + u32::from(tid.offset) - 1)
    }

    /// The group holding `slot`.
    pub fn group_of_slot(&self, slot: u32) -> usize {
        self.groups.partition_point(|g| g.slot_base <= slot) - 1
    }

    /// The ctid of slot `local` of group `index`.
    #[inline]
    pub fn tid_in(&self, index: usize, local: u32) -> Tid {
        let group = &self.groups[index];
        let page = self.page_of(index, local);
        Tid {
            block: group.id * GROUP_PAGES + u32::from(group.first) + page as u32,
            offset: (local - self.starts(index)[page]) as u16 + 1,
        }
    }

    /// Calls `visit` with the ctid of every slot set in `words`, a bitmap
    /// over group `index`'s slots, in order: the pages are stepped through
    /// with the members, not searched per member.
    pub fn each_tid(&self, index: usize, words: &[u64], mut visit: impl FnMut(Tid)) {
        let group = &self.groups[index];
        let starts = self.starts(index);
        let block = group.id * GROUP_PAGES + u32::from(group.first);
        let last = starts.len() - 2;
        let mut page = 0usize;
        for (w, word) in words.iter().enumerate() {
            let mut word = *word;
            while word != 0 {
                let local = (w * 64) as u32 + word.trailing_zeros();
                while page < last && starts[page + 1] <= local {
                    page += 1;
                }
                visit(Tid {
                    block: block + page as u32,
                    offset: (local - starts[page]) as u16 + 1,
                });
                word &= word - 1;
            }
        }
    }

    /// The ctid of `slot`.
    pub fn tid_of(&self, slot: u32) -> Tid {
        let index = self.group_of_slot(slot);
        self.tid_in(index, slot - self.groups[index].slot_base)
    }

    /// Bytes its tables hold on the heap.
    pub fn heap_bytes(&self) -> usize {
        self.groups.capacity() * std::mem::size_of::<Group>()
            + self.starts.capacity() * 4
            + self.hints.capacity()
    }
}

/// Builds the document set of `tids` (strictly increasing) over
/// `geometry`, which holds each of them.
pub fn encode_docset(geometry: &Geometry, tids: &[Tid]) -> Vec<u8> {
    let mut out = Vec::new();
    varint::put(&mut out, geometry.groups.len() as u64);
    let mut bodies = Vec::new();
    let mut at = 0usize;
    for (index, group) in geometry.groups.iter().enumerate() {
        let start = at;
        while at < tids.len() && tids[at].block / GROUP_PAGES == group.id {
            at += 1;
        }
        let members = &tids[start..at];
        let widths: Vec<u32> = (0..usize::from(group.pages))
            .map(|page| geometry.width(index, page))
            .collect();
        // Pages that hold exactly offsets 1..=width need only their width.
        let mut counts = vec![0u32; widths.len()];
        let mut prefix = true;
        for tid in members {
            let page = (tid.block % GROUP_PAGES - u32::from(group.first)) as usize;
            if u32::from(tid.offset) != counts[page] + 1 {
                prefix = false;
            }
            counts[page] += 1;
        }
        let kind = if prefix && counts == widths && widths.iter().all(|w| *w <= 255) {
            KIND_COUNTS
        } else {
            KIND_GRID
        };
        out.extend_from_slice(&group.id.to_le_bytes());
        out.push(group.first);
        out.push((group.pages - 1) as u8);
        out.extend_from_slice(&group.rank_base.to_le_bytes());
        out.push(kind);
        if kind == KIND_COUNTS {
            bodies.extend(widths.iter().map(|w| *w as u8));
        } else {
            bodies.extend(widths.iter().flat_map(|w| (*w as u16).to_le_bytes()));
            let mut grid = vec![0u8; group.grid_bytes()];
            for tid in members {
                let local = geometry.local(index, tid.block % GROUP_PAGES, tid.offset);
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
        let mut widths: Vec<u16> = Vec::with_capacity(GROUP_PAGES as usize);
        for entry in directory.chunks_exact(ENTRY) {
            let id = u32::from_le_bytes(entry[0..4].try_into().expect("four bytes"));
            let first = entry[4];
            let pages = usize::from(entry[5]) + 1;
            let rank_base = u32::from_le_bytes(entry[6..10].try_into().expect("four bytes"));
            let kind = entry[10];
            if rank_base != rank {
                return Err(Error::Corrupt("document set directory"));
            }
            widths.clear();
            match kind {
                KIND_COUNTS => {
                    let body = bytes.get(at..at + pages).ok_or(Error::Truncated)?;
                    at += pages;
                    widths.extend(body.iter().map(|n| u16::from(*n)));
                }
                KIND_GRID => {
                    let body = bytes.get(at..at + pages * 2).ok_or(Error::Truncated)?;
                    at += pages * 2;
                    widths.extend(
                        body.chunks_exact(2)
                            .map(|w| u16::from_le_bytes([w[0], w[1]])),
                    );
                }
                _ => return Err(Error::Corrupt("document set kind")),
            }
            geometry.push_group(id, first, &widths, rank_base)?;
            let index = geometry.groups.len() - 1;
            let group = geometry.groups[index];
            let base = words.len();
            word_base.push(base);
            words.resize(base + group.words(), 0u64);
            let grid = &mut words[base..];
            if kind == KIND_GRID {
                let body = bytes
                    .get(at..at + group.grid_bytes())
                    .ok_or(Error::Truncated)?;
                at += body.len();
                for (i, word) in grid.iter_mut().enumerate() {
                    *word = bits::word(body, i);
                }
                if !group.slots().is_multiple_of(64)
                    && grid.last().is_some_and(|w| w >> (group.slots() % 64) != 0)
                {
                    return Err(Error::Corrupt("document set past its slots"));
                }
            } else {
                // Every page holds offsets 1..=width: every slot is a document.
                let slots = group.slots() as usize;
                grid[..slots / 64].fill(u64::MAX);
                if !slots.is_multiple_of(64) {
                    grid[slots / 64] = (1u64 << (slots % 64)) - 1;
                }
            }
            for word in grid.iter() {
                word_rank.push(rank);
                rank += word.count_ones();
            }
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

    /// Bytes the decoded set holds on the heap.
    pub fn heap_bytes(&self) -> usize {
        self.words.capacity() * 8
            + self.word_base.capacity() * std::mem::size_of::<usize>()
            + self.word_rank.capacity() * 4
            + self.geometry.heap_bytes()
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
        for index in 0..self.geometry.groups.len() {
            self.geometry
                .each_tid(index, self.group_words(index), |tid| out.push(tid));
        }
        out
    }

    /// The group `index`'s slots holding documents.
    pub fn group_words(&self, index: usize) -> &[u64] {
        let base = self.word_base[index];
        &self.words[base..base + self.geometry.groups[index].words()]
    }
}

/// Documents per DL sidecar block: each block packs its lengths less its
/// shortest at the width the longest needs.
pub const DL_BLOCK: u32 = 256;

/// The largest base a block header holds (24 bits); a block whose shortest
/// length is above it packs the rest at a wider width.
const DL_BASE_MAX: u32 = (1 << 24) - 1;

/// Encodes exact document lengths, by rank.
pub fn encode_lengths(lengths: &[u32]) -> Vec<u8> {
    let blocks = lengths.len().div_ceil(DL_BLOCK as usize);
    let mut out = Vec::with_capacity(8 + blocks * 8 + lengths.len());
    varint::put(&mut out, u64::from(DL_BLOCK));
    varint::put(&mut out, blocks as u64);
    let mut headers = Vec::with_capacity(blocks * 8);
    let mut data = Vec::new();
    for chunk in lengths.chunks(DL_BLOCK as usize) {
        let base = chunk.iter().copied().min().unwrap_or(0).min(DL_BASE_MAX);
        let range = chunk.iter().map(|l| l - base).max().unwrap_or(0);
        let width = 32 - range.leading_zeros();
        headers.extend_from_slice(&(data.len() as u32).to_le_bytes());
        headers.extend_from_slice(&(base | width << 24).to_le_bytes());
        let mut w = bits::BitWriter::new();
        for l in chunk {
            w.put(l - base, width);
        }
        data.extend_from_slice(&w.finish());
    }
    out.extend_from_slice(&headers);
    out.extend_from_slice(&data);
    out
}

/// The DL sidecar: exact lengths by document rank. Its bytes may load on
/// demand ([`Bytes::Lazy`]): a length reads its block's header and the
/// bytes its bits fall in.
#[derive(Clone, Copy, Debug)]
pub struct Lengths<'a> {
    /// Where the block headers and the packed lengths start within the
    /// sidecar's bytes.
    pub headers_at: usize,
    pub data_at: usize,
    documents: u32,
    block: u32,
    headers: Bytes<'a>,
    data: Bytes<'a>,
}

impl<'a> Lengths<'a> {
    pub fn parse(bytes: impl Into<Bytes<'a>>, documents: u32) -> Result<Self> {
        let bytes = bytes.into();
        let mut at = 0;
        let prefix = bytes.window(0, 20)?;
        let block = varint::get_u32(prefix, &mut at)?;
        let blocks = varint::get_u32(prefix, &mut at)? as usize;
        if block == 0 || blocks != documents.div_ceil(block) as usize {
            return Err(Error::Corrupt("length blocks"));
        }
        let headers = bytes.sub(at, at + blocks * 8)?;
        let headers_at = at;
        let data_at = at + blocks * 8;
        Ok(Self {
            headers_at,
            data_at,
            documents,
            block,
            headers,
            data: bytes.sub(data_at, bytes.len())?,
        })
    }

    /// Block `b`'s header: where its bits start, its base and its width.
    #[inline]
    fn header(&self, b: usize) -> Result<(usize, u32, u32)> {
        let h = self
            .headers
            .get(b * 8, b * 8 + 8)
            .map_err(|_| Error::Corrupt("length rank"))?;
        let at = u32::from_le_bytes(h[0..4].try_into().expect("four bytes")) as usize;
        let packed = u32::from_le_bytes(h[4..8].try_into().expect("four bytes"));
        Ok((at, packed & DL_BASE_MAX, packed >> 24))
    }

    /// The length of the document at `rank`.
    #[inline]
    pub fn get(&self, rank: u32) -> Result<u32> {
        if rank >= self.documents {
            return Err(Error::Corrupt("length rank"));
        }
        let (at, base, width) = self.header((rank / self.block) as usize)?;
        if width == 0 {
            // No bits to read, but the block must still start in the data.
            return if at <= self.data.len() {
                Ok(base)
            } else {
                Err(Error::Truncated)
            };
        }
        let bit = (rank % self.block) as usize * width as usize;
        let from = at + bit / 8;
        let bytes = self.data.window(from, 8)?;
        Ok(base + bits::get_at(bytes, bit % 8, width)?)
    }

    /// Where `rank`'s block header and its packed length lie within the
    /// sidecar's bytes, for page accounting.
    pub fn at(&self, rank: u32) -> (usize, usize) {
        let b = (rank / self.block) as usize;
        let (at, _, width) = self.header(b).unwrap_or((0, 0, 0));
        let bit = (rank % self.block) as usize * width as usize;
        (self.headers_at + b * 8, self.data_at + at + bit / 8)
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
    /// Bytes its dead groups' bitmaps hold on the heap.
    pub fn heap_bytes(&self) -> usize {
        self.groups.capacity() * std::mem::size_of::<Option<Box<[u64]>>>()
            + self
                .groups
                .iter()
                .flatten()
                .map(|words| words.len() * 8)
                .sum::<usize>()
    }

    /// Whether the document at `slot` is dead.
    pub fn is_dead(&self, geometry: &Geometry, slot: u32) -> bool {
        if self.dead == 0 {
            return false;
        }
        let group = geometry.group_of_slot(slot);
        let local = slot - geometry.groups[group].slot_base;
        self.groups[group]
            .as_deref()
            .is_some_and(|dead| dead[local as usize / 64] >> (local % 64) & 1 == 1)
    }

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

    /// Pages of `rows` rows each at offsets `1..=rows` (0: an empty page),
    /// from block `first`.
    fn heap(first: u32, rows: &[u16]) -> Vec<Tid> {
        rows.iter()
            .enumerate()
            .flat_map(|(page, &n)| {
                (1..=n).map(move |offset| Tid {
                    block: first + page as u32,
                    offset,
                })
            })
            .collect()
    }

    #[test]
    fn each_page_has_its_own_width() {
        // A slot per document where pages hold rows 1..=n, whatever n.
        let rows = [3u16, 0, 1, 64, 0, 0, 291, 2, 63, 65, 1];
        let tids = heap(250, &rows);
        let geometry = Geometry::of(&tids).unwrap();
        assert_eq!(geometry.groups.len(), 2);
        assert_eq!(geometry.slots, tids.len() as u32);
        let g = &geometry.groups[0];
        assert_eq!((g.first, g.pages, g.slots()), (250, 4, 68));
        assert_eq!(geometry.starts(0), &[0, 3, 3, 4, 68]);
        assert_eq!(geometry.width(0, 1), 0);
        assert_eq!(
            geometry.slot_of(Tid {
                block: 251,
                offset: 1
            }),
            None
        );
        assert_eq!(
            geometry.slot_of(Tid {
                block: 252,
                offset: 2
            }),
            None
        );
        assert_eq!(
            geometry.slot_of(Tid {
                block: 252,
                offset: 1
            }),
            Some(3)
        );
        assert_eq!(
            geometry.slot_of(Tid {
                block: 249,
                offset: 1
            }),
            None
        );
        let g = &geometry.groups[1];
        assert_eq!((g.first, g.pages, g.slot_base), (0, 5, 68));
        // The first group as counts (a byte per page); the second has a
        // page of 291 rows, which no byte holds, so it is a grid: a u16
        // width per page and 422 slots in 7 words.
        assert_eq!(
            encode_docset(&geometry, &tids).len(),
            1 + 2 * ENTRY + 4 + 5 * 2 + 7 * 8
        );
        check(&tids);
        // Pages with holes, and a page wider than its rows.
        let mut holes = heap(512, &[5, 0, 7, 2]);
        holes.retain(|t| t.offset != 3);
        holes.push(Tid {
            block: 516,
            offset: 40,
        });
        let geometry = Geometry::of(&holes).unwrap();
        assert_eq!(geometry.starts(0), &[0, 5, 5, 12, 14, 54]);
        check(&holes);
        // Spans whose slots end on, before and past a word's end.
        for n in [63u16, 64, 65, 127, 128, 129] {
            check(&heap(7, &[n]));
            check(&heap(7, &[1, n, 0, n]));
        }
    }

    #[test]
    fn page_search_from_every_slot() {
        let rows: Vec<u16> = (0..256u32).map(|p| ((p * 37) % 23) as u16).collect();
        let tids = heap(1024, &rows);
        let geometry = Geometry::of(&tids).unwrap();
        for local in 0..geometry.groups[0].slots() {
            let page = geometry.page_of(0, local);
            let starts = geometry.starts(0);
            assert!(starts[page] <= local && local < starts[page + 1]);
        }
        check(&tids);
    }

    #[test]
    fn lengths_round_trip() {
        let mut lengths = vec![1, 65_534, 65_535, 70_000, 3, u32::MAX, 0, (1 << 24) + 5];
        lengths.extend((0..1000u32).map(|i| (i * 7919) % 4096 + 1));
        lengths.extend([7; 300]);
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

        /// Heap pages holding rows `1..=n`, `n` random per page, some with
        /// holes (a row deleted and pruned).
        #[test]
        fn random_heaps(pages in prop::collection::btree_map(0u32..1200, (1u16..=291, any::<u64>()), 1..60)) {
            let mut tids = Vec::new();
            for (block, (n, holes)) in pages {
                let n = if holes % 5 == 0 { n } else { n % 40 + 1 };
                tids.extend((1..=n).filter(|o| holes % 3 != 0 || (holes >> (o % 64)) & 1 == 0).map(|offset| Tid { block, offset }));
            }
            prop_assume!(!tids.is_empty());
            let geometry = Geometry::of(&tids).unwrap();
            prop_assert!(geometry.slots as usize >= tids.len());
            check(&tids);
        }
    }
}
