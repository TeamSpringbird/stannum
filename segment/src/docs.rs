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
