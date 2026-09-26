// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! The document table: a segment's tuple locations by ordinal.
//!
//! Documents are numbered `0..doc_count` in heap order. The table is two
//! arrays that together name a document's location:
//!
//! ```text
//! pages   := (block u32le, first u32le)* ascending by block; `first` is
//!            the ordinal of the block's first document
//! offsets := offset u16le per document, in ordinal order
//! ```
//!
//! A location is found from its ordinal by a binary search of the page
//! table and one offset read, and an ordinal from its location by the same
//! search and a binary search within the block's offsets, so both
//! directions cost a few reads however large the segment. Offsets are
//! fetched in windows of [`WINDOW`] documents, so a cursor sweeping a
//! stream reads the table sequentially rather than a block at a time. Term streams
//! ([`crate::ordinals`]) name documents by ordinal and are turned back into
//! locations through this table.

use std::rc::Rc;

use crate::ordinals::{Fetch, OrdinalCursor};
use crate::pages::{Offsets, Page};
use crate::set::{self, Cursor as _};
use crate::tid::MAX_OFFSET;
use crate::{Error, Result, Tid};

/// Bytes per entry of the page table.
pub const PAGE_ENTRY: usize = 8;

/// The page table over documents in heap order.
pub fn page_table(documents: impl Iterator<Item = Tid>) -> Vec<u8> {
    let mut pages = Vec::new();
    let mut previous = None;
    for (ordinal, tid) in documents.enumerate() {
        if previous != Some(tid.block) {
            pages.extend_from_slice(&tid.block.to_le_bytes());
            pages.extend_from_slice(&(ordinal as u32).to_le_bytes());
            previous = Some(tid.block);
        }
    }
    pages
}

/// The offsets array over documents in heap order.
pub fn offsets(documents: impl Iterator<Item = Tid>) -> Vec<u8> {
    let mut out = Vec::new();
    for tid in documents {
        out.extend_from_slice(&tid.offset.to_le_bytes());
    }
    out
}

/// A parsed page table: the heap blocks the documents span.
#[derive(Clone, Copy)]
pub struct PageTable<'a> {
    bytes: &'a [u8],
    doc_count: u32,
}

impl<'a> PageTable<'a> {
    /// Checks the table's form against `doc_count` documents: whole entries,
    /// strictly ascending blocks, first ordinals ascending from zero and
    /// below the document count, and an empty table only for no documents.
    pub fn parse(bytes: &'a [u8], doc_count: u32) -> Result<Self> {
        if !bytes.len().is_multiple_of(PAGE_ENTRY) {
            return Err(Error::Corrupt("page table length"));
        }
        let table = Self { bytes, doc_count };
        if table.is_empty() != (doc_count == 0) {
            return Err(Error::Corrupt("page table for the document count"));
        }
        let mut previous: Option<(u32, u32)> = None;
        for i in 0..table.len() {
            let entry = (table.block(i), table.first(i));
            let ordered = match previous {
                None => entry.1 == 0,
                Some((block, first)) => block < entry.0 && first < entry.1,
            };
            if !ordered || entry.1 >= doc_count {
                return Err(Error::Corrupt("page table order"));
            }
            previous = Some(entry);
        }
        Ok(table)
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Entries in the table: blocks the documents span.
    pub fn len(&self) -> usize {
        self.bytes.len() / PAGE_ENTRY
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn doc_count(&self) -> u32 {
        self.doc_count
    }

    pub fn block(&self, i: usize) -> u32 {
        let at = i * PAGE_ENTRY;
        u32::from_le_bytes(self.bytes[at..at + 4].try_into().unwrap())
    }

    /// The ordinal of entry `i`'s first document.
    pub fn first(&self, i: usize) -> u32 {
        let at = i * PAGE_ENTRY + 4;
        u32::from_le_bytes(self.bytes[at..at + 4].try_into().unwrap())
    }

    /// One past the ordinal of entry `i`'s last document.
    pub fn end(&self, i: usize) -> u32 {
        if i + 1 < self.len() {
            self.first(i + 1)
        } else {
            self.doc_count
        }
    }

    /// The entry holding `ordinal`.
    pub fn entry_of(&self, ordinal: u32) -> Option<usize> {
        if ordinal >= self.doc_count {
            return None;
        }
        let (mut lo, mut hi) = (0usize, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.first(mid) <= ordinal {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo.checked_sub(1)
    }

    /// The entry for `block`, or where it would be inserted.
    pub fn find(&self, block: u32) -> std::result::Result<usize, usize> {
        let (mut lo, mut hi) = (0usize, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.block(mid).cmp(&block) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(mid),
            }
        }
        Err(lo)
    }

    /// `(block, first ordinal)` per entry.
    pub fn entries(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        (0..self.len()).map(|i| (self.block(i), self.first(i)))
    }
}

/// Documents per window of the offsets table a paged source hands out: 8 KiB.
pub const WINDOW: u32 = 4096;

/// A window of the offsets table, addressed by ordinal.
#[derive(Clone)]
pub struct OffsetWindow {
    bytes: Rc<[u8]>,
    base: u32,
}

impl OffsetWindow {
    fn covers(&self, ordinal: u32) -> bool {
        ordinal >= self.base && ((ordinal - self.base) as usize) * 2 + 2 <= self.bytes.len()
    }

    /// The offset of document `ordinal`, which the window must cover.
    fn get(&self, ordinal: u32) -> Result<u16> {
        let at = (ordinal.wrapping_sub(self.base)) as usize * 2;
        let offset = self
            .bytes
            .get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .ok_or(Error::Truncated)?;
        if offset == 0 || offset > MAX_OFFSET {
            return Err(Error::Corrupt("document offset"));
        }
        Ok(offset)
    }
}

/// The document table of one segment, with offsets fetched a window at a time.
pub struct DocTable<'a> {
    pages: PageTable<'a>,
    offsets: Box<dyn Fetch<'a> + 'a>,
    offsets_len: u64,
}

impl<'a> DocTable<'a> {
    /// A table over `pages` and an offsets area of `offsets_len` bytes.
    pub fn new(
        pages: PageTable<'a>,
        offsets: impl Fetch<'a> + 'a,
        offsets_len: u64,
    ) -> Result<Self> {
        if offsets_len != u64::from(pages.doc_count) * 2 {
            return Err(Error::Corrupt("document offsets length"));
        }
        Ok(Self {
            pages,
            offsets: Box::new(offsets),
            offsets_len,
        })
    }

    /// A table held entirely in memory.
    pub fn parse(pages: &'a [u8], offsets: &'a [u8], doc_count: u32) -> Result<Self> {
        Self::new(
            PageTable::parse(pages, doc_count)?,
            offsets,
            offsets.len() as u64,
        )
    }

    pub const fn pages(&self) -> &PageTable<'a> {
        &self.pages
    }

    pub const fn doc_count(&self) -> u32 {
        self.pages.doc_count
    }

    /// The window of the offsets table holding `ordinal`.
    pub fn window(&self, ordinal: u32) -> Result<OffsetWindow> {
        if ordinal >= self.doc_count() {
            return Err(Error::Corrupt("document ordinal out of range"));
        }
        let base = ordinal - ordinal % WINDOW;
        let end = base.saturating_add(WINDOW).min(self.doc_count());
        let from = u64::from(base) * 2;
        let to = u64::from(end) * 2;
        if to > self.offsets_len {
            return Err(Error::Truncated);
        }
        Ok(OffsetWindow {
            bytes: self.offsets.fetch_owned(from, (to - from) as usize)?,
            base,
        })
    }

    /// The offset of document `ordinal`, through `window`, refetched when it
    /// does not cover the ordinal.
    fn offset_at(&self, window: &mut Option<OffsetWindow>, ordinal: u32) -> Result<u16> {
        if !window.as_ref().is_some_and(|w| w.covers(ordinal)) {
            *window = Some(self.window(ordinal)?);
        }
        window.as_ref().expect("window loaded").get(ordinal)
    }

    /// The location of document `ordinal`.
    pub fn tid_at(&self, ordinal: u32) -> Result<Tid> {
        let i = self
            .pages
            .entry_of(ordinal)
            .ok_or(Error::Corrupt("document ordinal out of range"))?;
        Ok(Tid {
            block: self.pages.block(i),
            offset: self.window(ordinal)?.get(ordinal)?,
        })
    }

    /// The first ordinal in entry `i` whose offset is at least `offset`;
    /// the entry's end when there is none.
    fn partition(&self, i: usize, offset: u16) -> Result<u32> {
        let mut window = None;
        let (mut lo, mut hi) = (self.pages.first(i), self.pages.end(i));
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.offset_at(&mut window, mid)? < offset {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    /// The ordinal of the document at `tid`, if the segment holds it.
    pub fn ordinal_of(&self, tid: Tid) -> Result<Option<u32>> {
        let Ok(i) = self.pages.find(tid.block) else {
            return Ok(None);
        };
        let at = self.partition(i, tid.offset)?;
        if at < self.pages.end(i) && self.window(at)?.get(at)? == tid.offset {
            Ok(Some(at))
        } else {
            Ok(None)
        }
    }

    /// The ordinal of the first document at or after `tid`; the document
    /// count when there is none.
    pub fn lower_bound(&self, tid: Tid) -> Result<u32> {
        match self.pages.find(tid.block) {
            Ok(i) => self.partition(i, tid.offset),
            Err(i) if i < self.pages.len() => Ok(self.pages.first(i)),
            Err(_) => Ok(self.doc_count()),
        }
    }

    /// Every document in heap order.
    pub fn into_cursor(self) -> Result<DocCursor<'a>> {
        let mut cursor = DocCursor {
            docs: self,
            ordinal: 0,
            entry: 0,
            window: None,
        };
        cursor.position(0)?;
        Ok(cursor)
    }

    /// Every document as `Tid`s, for verification and small tables.
    pub fn to_vec(&self) -> Result<Vec<Tid>> {
        let mut out = Vec::with_capacity(self.doc_count() as usize);
        let mut window = None;
        for i in 0..self.pages.len() {
            let block = self.pages.block(i);
            let mut previous = 0;
            for ordinal in self.pages.first(i)..self.pages.end(i) {
                let offset = self.offset_at(&mut window, ordinal)?;
                if offset <= previous {
                    return Err(Error::Corrupt("document offset order"));
                }
                previous = offset;
                out.push(Tid { block, offset });
            }
        }
        Ok(out)
    }

    /// Resolves ordinals to locations, remembering the page-table entry and
    /// offsets window last used, for callers that walk a stream in order.
    pub fn resolver(&self) -> Resolver<'_, 'a> {
        Resolver {
            docs: self,
            entry: None,
            window: None,
        }
    }
}

/// The page-table entry holding `ordinal`, trying the entry after `hint`
/// first: walks in ordinal order mostly step to the next block.
fn entry_after(pages: &PageTable<'_>, hint: Option<usize>, ordinal: u32) -> Option<usize> {
    if let Some(i) = hint {
        for next in i..(i + 4).min(pages.len()) {
            if pages.first(next) <= ordinal && ordinal < pages.end(next) {
                return Some(next);
            }
        }
    }
    pages.entry_of(ordinal)
}

/// Ordinal-to-location lookups that remember where they were.
pub struct Resolver<'d, 'a> {
    docs: &'d DocTable<'a>,
    entry: Option<usize>,
    window: Option<OffsetWindow>,
}

impl Resolver<'_, '_> {
    pub fn tid_at(&mut self, ordinal: u32) -> Result<Tid> {
        let pages = &self.docs.pages;
        let i = entry_after(pages, self.entry, ordinal)
            .ok_or(Error::Corrupt("document ordinal out of range"))?;
        self.entry = Some(i);
        Ok(Tid {
            block: pages.block(i),
            offset: self.docs.offset_at(&mut self.window, ordinal)?,
        })
    }
}

/// A cursor over every document of a table, in heap order.
pub struct DocCursor<'a> {
    docs: DocTable<'a>,
    ordinal: u32,
    entry: usize,
    window: Option<OffsetWindow>,
}

impl<'a> DocCursor<'a> {
    pub const fn table(&self) -> &DocTable<'a> {
        &self.docs
    }

    fn position(&mut self, ordinal: u32) -> Result<()> {
        self.ordinal = ordinal;
        if let Some(i) = entry_after(&self.docs.pages, Some(self.entry), ordinal) {
            self.entry = i;
            self.docs.offset_at(&mut self.window, ordinal)?;
        }
        Ok(())
    }

    /// The ordinal of the current document; the count when exhausted.
    pub fn ordinal(&self) -> u32 {
        self.ordinal
    }

    /// The ordinal of `tid`, moving the cursor to it or past where it
    /// would be.
    pub fn rank(&mut self, tid: Tid) -> Result<Option<u32>> {
        self.seek(tid)?;
        Ok(self
            .current()
            .filter(|found| *found == tid)
            .map(|_| self.ordinal))
    }
}

impl set::Cursor for DocCursor<'_> {
    fn current(&self) -> Option<Tid> {
        if self.ordinal >= self.docs.doc_count() {
            return None;
        }
        let window = self.window.as_ref()?;
        window.get(self.ordinal).ok().map(|offset| Tid {
            block: self.docs.pages.block(self.entry),
            offset,
        })
    }

    fn advance(&mut self) -> Result<()> {
        if self.ordinal >= self.docs.doc_count() {
            return Ok(());
        }
        self.position(self.ordinal + 1)
    }

    fn seek(&mut self, target: Tid) -> Result<()> {
        // An exhausted cursor stays exhausted, as every other cursor does:
        // repositioning it to an earlier target resurrected the universe
        // under `NOT`, and an intersection probing it after the end then
        // subtracted documents that were never in the inner set.
        if self.current().is_none_or(|current| current >= target) {
            return Ok(());
        }
        let ordinal = self.docs.lower_bound(target)?;
        self.position(ordinal)
    }
}

/// A term's documents as tuple locations in heap order: an ordinal cursor
/// resolved through the document table.
pub struct TidCursor<'a> {
    ordinals: OrdinalCursor<'a>,
    docs: DocTable<'a>,
    /// The page-table entry of the current ordinal and the window holding
    /// its offset.
    entry: Option<usize>,
    window: Option<OffsetWindow>,
}

impl<'a> TidCursor<'a> {
    pub fn new(ordinals: OrdinalCursor<'a>, docs: DocTable<'a>) -> Result<Self> {
        let mut cursor = Self {
            ordinals,
            docs,
            entry: None,
            window: None,
        };
        cursor.locate()?;
        Ok(cursor)
    }

    /// Finds the current ordinal's page-table entry and offsets window,
    /// keeping those already held when they still apply.
    fn locate(&mut self) -> Result<()> {
        let Some(ordinal) = self.ordinals.current() else {
            return Ok(());
        };
        self.entry = Some(
            entry_after(&self.docs.pages, self.entry, ordinal)
                .ok_or(Error::Corrupt("ordinal beyond the document table"))?,
        );
        self.docs.offset_at(&mut self.window, ordinal)?;
        Ok(())
    }

    /// The current document's ordinal.
    pub fn ordinal(&self) -> Option<u32> {
        self.ordinals.current()
    }

    /// The current document's rank in the term's stream, which addresses
    /// its positions; the count once exhausted.
    pub fn rank(&self) -> u32 {
        self.ordinals.rank()
    }

    /// The current document's term-frequency bucket, when the stream
    /// stores buckets.
    pub fn bucket(&self) -> Option<u8> {
        self.ordinals.bucket()
    }

    pub fn count(&self) -> u32 {
        self.ordinals.count()
    }

    pub const fn table(&self) -> &DocTable<'a> {
        &self.docs
    }
}

impl set::Cursor for TidCursor<'_> {
    fn current(&self) -> Option<Tid> {
        let ordinal = self.ordinals.current()?;
        let i = self.entry?;
        let offset = self.window.as_ref()?.get(ordinal).ok()?;
        Some(Tid {
            block: self.docs.pages.block(i),
            offset,
        })
    }

    fn advance(&mut self) -> Result<()> {
        self.ordinals.advance()?;
        self.locate()
    }

    fn seek(&mut self, target: Tid) -> Result<()> {
        if self.current().is_none_or(|current| current >= target) {
            return Ok(());
        }
        let ordinal = self.docs.lower_bound(target)?;
        self.ordinals.seek(ordinal)?;
        self.locate()
    }
}

/// A term's documents a heap page at a time: each page-table entry the
/// stream touches, with the offsets of the members on that block.
pub struct PageCursor<'a> {
    ordinals: OrdinalCursor<'a>,
    docs: DocTable<'a>,
    entry: Option<usize>,
    window: Option<OffsetWindow>,
    page: Option<Page>,
}

impl<'a> PageCursor<'a> {
    pub fn new(ordinals: OrdinalCursor<'a>, docs: DocTable<'a>) -> Result<Self> {
        let mut cursor = Self {
            ordinals,
            docs,
            entry: None,
            window: None,
            page: None,
        };
        cursor.gather()?;
        Ok(cursor)
    }

    /// Collects the members on the current ordinal's block, leaving the
    /// ordinal cursor on the first member past it.
    fn gather(&mut self) -> Result<()> {
        let Some(first) = self.ordinals.current() else {
            self.page = None;
            return Ok(());
        };
        let pages = &self.docs.pages;
        let i = entry_after(pages, self.entry, first)
            .ok_or(Error::Corrupt("ordinal beyond the document table"))?;
        self.entry = Some(i);
        let end = pages.end(i);
        let mut page = Page {
            block: pages.block(i),
            offsets: Offsets::default(),
        };
        while let Some(ordinal) = self.ordinals.current().filter(|o| *o < end) {
            page.offsets
                .insert(self.docs.offset_at(&mut self.window, ordinal)?);
            self.ordinals.advance()?;
        }
        self.page = Some(page);
        Ok(())
    }
}

impl crate::pages::Cursor for PageCursor<'_> {
    fn current(&self) -> Option<Page> {
        self.page
    }

    fn advance(&mut self) -> Result<()> {
        if self.page.is_some() {
            self.gather()?;
        }
        Ok(())
    }

    fn seek(&mut self, block: u32) -> Result<()> {
        if self.page.is_none_or(|page| page.block >= block) {
            return Ok(());
        }
        let ordinal = self.docs.lower_bound(Tid { block, offset: 1 })?;
        self.ordinals.seek(ordinal)?;
        self.gather()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::set::collect;

    fn table(tids: &[Tid]) -> (Vec<u8>, Vec<u8>) {
        (
            page_table(tids.iter().copied()),
            offsets(tids.iter().copied()),
        )
    }

    fn tid(block: u32, offset: u16) -> Tid {
        Tid::new(block, offset).unwrap()
    }

    #[test]
    fn an_exhausted_document_cursor_stays_exhausted() {
        let tids = [tid(0, 1), tid(0, 2), tid(3, 1)];
        let (pages, offsets) = table(&tids);
        let docs = DocTable::parse(&pages, &offsets, tids.len() as u32).unwrap();
        let mut cursor = docs.into_cursor().unwrap();
        cursor.seek(tid(9, 1)).unwrap();
        assert_eq!(cursor.current(), None);
        cursor.seek(tid(0, 1)).unwrap();
        assert_eq!(cursor.current(), None);
        assert_eq!(cursor.rank(tid(0, 2)).unwrap(), None);
    }

    #[test]
    fn maps_both_ways() {
        let tids = [tid(0, 1), tid(0, 3), tid(2, 1), tid(2, 2), tid(7, 291)];
        let (pages, offsets) = table(&tids);
        let docs = DocTable::parse(&pages, &offsets, tids.len() as u32).unwrap();
        for (ordinal, expected) in tids.iter().enumerate() {
            assert_eq!(docs.tid_at(ordinal as u32).unwrap(), *expected);
            assert_eq!(docs.ordinal_of(*expected).unwrap(), Some(ordinal as u32));
            assert_eq!(docs.lower_bound(*expected).unwrap(), ordinal as u32);
        }
        assert_eq!(docs.ordinal_of(tid(0, 2)).unwrap(), None);
        assert_eq!(docs.ordinal_of(tid(1, 1)).unwrap(), None);
        assert_eq!(docs.lower_bound(tid(0, 2)).unwrap(), 1);
        assert_eq!(docs.lower_bound(tid(1, 1)).unwrap(), 2);
        assert_eq!(docs.lower_bound(tid(7, 5)).unwrap(), 4);
        assert_eq!(docs.lower_bound(tid(8, 1)).unwrap(), 5);
        assert!(docs.tid_at(5).is_err());
        assert_eq!(docs.to_vec().unwrap(), tids);
        let fresh = || DocTable::parse(&pages, &offsets, tids.len() as u32).unwrap();
        assert_eq!(collect(fresh().into_cursor().unwrap()).unwrap(), tids);
        let mut cursor = fresh().into_cursor().unwrap();
        cursor.seek(tid(2, 2)).unwrap();
        assert_eq!(cursor.current(), Some(tid(2, 2)));
        assert_eq!(cursor.ordinal(), 3);
        assert_eq!(cursor.rank(tid(7, 291)).unwrap(), Some(4));
        assert_eq!(cursor.rank(tid(7, 290)).unwrap(), None);
        assert_eq!(cursor.current(), Some(tid(7, 291)));
        assert_eq!(cursor.rank(tid(8, 1)).unwrap(), None);
        assert_eq!(cursor.current(), None);
        let mut resolver = docs.resolver();
        for (ordinal, expected) in tids.iter().enumerate() {
            assert_eq!(resolver.tid_at(ordinal as u32).unwrap(), *expected);
        }
    }

    #[test]
    fn empty_table() {
        let docs = DocTable::parse(&[], &[], 0).unwrap();
        assert_eq!(docs.lower_bound(tid(0, 1)).unwrap(), 0);
        assert_eq!(docs.into_cursor().unwrap().current(), None);
        assert!(DocTable::parse(&[], &[0, 0], 1).is_err());
        assert!(PageTable::parse(&[0, 0, 0, 0, 1, 0, 0, 0], 2).is_err());
    }
}

/// Every cursor over documents treats exhaustion as final and never moves
/// backwards: a seek to a target at or before its position, or any seek or
/// advance once it is exhausted, leaves it where it is. A document cursor
/// sought back to an earlier target after its end came back to life
/// (7eb033f) and the universe under `NOT` subtracted documents the inner
/// set never held. Each cursor is driven by random advances and seeks,
/// backwards, past the end and after it, against a model over its members;
/// tables span several 65,536-ordinal chunks.
#[cfg(test)]
mod exhaustion {
    use super::*;
    use crate::ordinals::{Ordinals, encode};
    use crate::pages::Cursor as _;
    use crate::set::{AtLeast, Cursor, Difference, Intersection, Slice, Union};
    use proptest::prelude::*;

    /// Draws from a seed, so that tables of a hundred thousand documents
    /// cost one value each to generate and shrink.
    struct Draw(u64);

    impl Draw {
        fn next(&mut self) -> u64 {
            // SplitMix64.
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// `count` document locations in heap order, a few to a block.
    fn locations(draw: &mut Draw, count: u32) -> Vec<Tid> {
        let mut tids = Vec::with_capacity(count as usize);
        let (mut block, mut offset) = (draw.below(3) as u32, 0u16);
        for _ in 0..count {
            if offset >= MAX_OFFSET - 1 || draw.below(4) == 0 {
                block += 1 + draw.below(3) as u32;
                offset = 0;
            }
            offset += 1 + draw.below(2) as u16;
            tids.push(Tid::new(block, offset).unwrap());
        }
        tids
    }

    /// A subset of `0..count` of density about one in `every`.
    fn members(draw: &mut Draw, count: u32, every: u64) -> Vec<u32> {
        (0..count).filter(|_| draw.below(every) == 0).collect()
    }

    #[derive(Clone, Copy, Debug)]
    enum Op {
        Advance,
        /// Seek to a location drawn from 0..=1 past the last block, so
        /// backwards, onto members, between them and after the end.
        Seek(u64),
    }

    fn ops() -> impl Strategy<Value = Vec<Op>> {
        prop::collection::vec(
            prop_oneof![2 => Just(Op::Advance), 3 => any::<u64>().prop_map(Op::Seek)],
            1..60,
        )
    }

    /// The target of `seed` among `tids`' blocks, past the last one too.
    fn target(tids: &[Tid], seed: u64) -> Tid {
        let last = tids.last().map_or(0, |tid| tid.block);
        let block = (seed % u64::from(last + 3)) as u32;
        let offset = ((seed >> 32) % u64::from(MAX_OFFSET)) as u16 + 1;
        Tid::new(block, offset).unwrap()
    }

    /// The model: the position among `expected`, which only moves forward.
    fn replay<T: Copy + Ord + std::fmt::Debug>(
        expected: &[T],
        ops: &[Op],
        at: impl Fn(u64) -> T,
        mut step: impl FnMut(Option<T>) -> Option<T>,
        label: &str,
    ) {
        let mut index = 0usize;
        let mut exhausted_at = None;
        for (n, op) in ops.iter().enumerate() {
            let (target, actual) = match *op {
                Op::Advance => {
                    index = (index + 1).min(expected.len());
                    (None, step(None))
                }
                Op::Seek(seed) => {
                    let target = at(seed);
                    if expected.get(index).is_some_and(|current| *current < target) {
                        index += expected[index..].partition_point(|member| *member < target);
                    }
                    (Some(target), step(Some(target)))
                }
            };
            let want = expected.get(index).copied();
            assert_eq!(
                actual, want,
                "{label}: op {n} {op:?} (target {target:?}) after exhaustion at {exhausted_at:?}"
            );
            if want.is_none() && exhausted_at.is_none() {
                exhausted_at = Some(n);
            }
        }
    }

    /// Drives a set cursor by `ops` against `expected`.
    fn check(mut cursor: impl Cursor, expected: &[Tid], tids: &[Tid], ops: &[Op], label: &str) {
        assert_eq!(
            cursor.current(),
            expected.first().copied(),
            "{label}: first"
        );
        replay(
            expected,
            ops,
            |seed| target(tids, seed),
            |op| {
                match op {
                    None => cursor.advance().unwrap(),
                    Some(target) => cursor.seek(target).unwrap(),
                }
                cursor.current()
            },
            label,
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

        #[test]
        fn exhausted_cursors_stay_exhausted(
            seed in any::<u64>(),
            size in prop_oneof![
                6 => 0u32..400,
                1 => 65_530u32..65_545,
                1 => 131_060u32..140_000,
            ],
            density in (1u64..40, 1u64..40),
            ops in ops(),
        ) {
            let mut draw = Draw(seed);
            let tids = locations(&mut draw, size);
            let pages = page_table(tids.iter().copied());
            let offsets = offsets(tids.iter().copied());
            let docs = || DocTable::parse(&pages, &offsets, size).unwrap();
            check(docs().into_cursor().unwrap(), &tids, &tids, &ops, "documents");

            let a = members(&mut draw, size, density.0);
            let b = members(&mut draw, size, density.1);
            let (a_bytes, b_bytes) = (encode(&a), encode(&b));
            let stream = |bytes| Ordinals::parse(bytes).unwrap().cursor().unwrap();
            let at = |ordinals: &[u32]| -> Vec<Tid> {
                ordinals.iter().map(|o| tids[*o as usize]).collect()
            };
            let (a_tids, b_tids) = (at(&a), at(&b));

            // The ordinal stream itself, sought by ordinal.
            let mut ordinals = stream(&a_bytes);
            replay(
                &a,
                &ops,
                |seed| (seed % u64::from(size + 2)) as u32,
                |op| {
                    match op {
                        None => ordinals.advance().unwrap(),
                        Some(target) => ordinals.seek(target).unwrap(),
                    }
                    ordinals.current()
                },
                "ordinals",
            );

            let tid_cursor = |bytes| TidCursor::new(stream(bytes), docs()).unwrap();
            check(tid_cursor(&a_bytes), &a_tids, &tids, &ops, "term");

            let both: Vec<Tid> = a_tids.iter().copied().filter(|t| b_tids.binary_search(t).is_ok()).collect();
            let either: Vec<Tid> = {
                let mut all: Vec<Tid> = a_tids.iter().chain(&b_tids).copied().collect();
                all.sort_unstable();
                all.dedup();
                all
            };
            let only_a: Vec<Tid> = a_tids.iter().copied().filter(|t| b_tids.binary_search(t).is_err()).collect();
            check(
                Intersection::new(vec![tid_cursor(&a_bytes), tid_cursor(&b_bytes)]).unwrap(),
                &both, &tids, &ops, "intersection",
            );
            check(Union::new(vec![tid_cursor(&a_bytes), tid_cursor(&b_bytes)]), &either, &tids, &ops, "union");
            check(
                AtLeast::new(vec![tid_cursor(&a_bytes), tid_cursor(&b_bytes)], 2).unwrap(),
                &both, &tids, &ops, "at least two",
            );
            check(
                Difference::new(tid_cursor(&a_bytes), tid_cursor(&b_bytes)).unwrap(),
                &only_a, &tids, &ops, "difference",
            );
            // The universe less a term, as `NOT` plans it, and a slice.
            let universe_less_b: Vec<Tid> =
                tids.iter().copied().filter(|t| b_tids.binary_search(t).is_err()).collect();
            check(
                Difference::new(docs().into_cursor().unwrap(), tid_cursor(&b_bytes)).unwrap(),
                &universe_less_b, &tids, &ops, "universe less a term",
            );
            check(Slice::new(&a_tids), &a_tids, &tids, &ops, "slice");

            // A term a heap page at a time, sought by block.
            let mut blocks: Vec<u32> = a_tids.iter().map(|t| t.block).collect();
            blocks.dedup();
            let mut page_cursor = PageCursor::new(stream(&a_bytes), docs()).unwrap();
            prop_assert_eq!(page_cursor.current().map(|p| p.block), blocks.first().copied());
            let last = tids.last().map_or(0, |tid| tid.block);
            replay(
                &blocks,
                &ops,
                |seed| (seed % u64::from(last + 3)) as u32,
                |op| {
                    match op {
                        None => page_cursor.advance().unwrap(),
                        Some(block) => page_cursor.seek(block).unwrap(),
                    }
                    page_cursor.current().map(|page| page.block)
                },
                "pages",
            );
        }
    }
}
