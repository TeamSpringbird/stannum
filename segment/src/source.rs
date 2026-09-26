// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Where a segment's bytes come from.
//!
//! A segment on disk spans many pages; a query touches a few extents of it.
//! [`Source`] lets the reader ask for exactly those extents. In-memory
//! sources hand out borrowed slices; page-backed sources copy the pages that
//! cover a range.

use std::rc::Rc;

use crate::{Error, Result};

/// Slots a source may hold a page in, one per table read through
/// [`Source::held_span`], so two tables read in step do not evict each
/// other's page.
pub const HELD_SLOTS: usize = 2;

/// The bytes of a page a source holds pinned: `len` bytes from offset
/// `start` of the source, at `data`.
#[derive(Clone, Copy, Debug)]
pub struct HeldSpan {
    pub start: u64,
    pub data: *const u8,
    pub len: usize,
    /// Whether the page was pinned by this call rather than held already.
    pub pinned: bool,
}

/// Most pages a [`HeldRange`] spans: a bitmap chunk's 8 KiB of words
/// starts anywhere on a page, so it touches three pages at most.
pub const HELD_PIECES: usize = 3;

/// A range of a source read in place from pages the source holds pinned:
/// up to [`HELD_PIECES`] contiguous pieces, one per page, in order. The
/// last piece runs on to its page's end, so the bytes just past the range
/// on that page (a chunk's first bucket nibbles, say) are readable too:
/// [`HeldRange::end`] bytes from the range's start in all.
///
/// The pointers are valid only while the source keeps the pages pinned:
/// until the next call on the slot that handed the range out, or until the
/// outermost [`Source::hold`] span closes. Every read is `unsafe` for that
/// reason; callers wrap it in a type whose construction carries the
/// contract.
#[derive(Clone, Copy, Debug)]
pub struct HeldRange {
    pieces: usize,
    /// Per piece, the offset from the range's start just past it.
    ends: [usize; HELD_PIECES],
    /// Per piece, its first byte.
    data: [*const u8; HELD_PIECES],
    /// Bytes of the pages newly pinned for the range, for accounting.
    pub pinned_bytes: usize,
}

impl Default for HeldRange {
    fn default() -> Self {
        Self {
            pieces: 0,
            ends: [0; HELD_PIECES],
            data: [std::ptr::null(); HELD_PIECES],
            pinned_bytes: 0,
        }
    }
}

impl HeldRange {
    /// Appends a piece of `len` bytes at `data`; false when the range has
    /// [`HELD_PIECES`] already.
    pub fn push(&mut self, data: *const u8, len: usize) -> bool {
        if self.pieces == HELD_PIECES {
            return false;
        }
        self.ends[self.pieces] = self.end() + len;
        self.data[self.pieces] = data;
        self.pieces += 1;
        true
    }

    /// Limits the bytes readable to the first `end`, which must not fall
    /// below the range's length.
    pub fn clip(&mut self, end: usize) {
        while self.pieces > 1 && self.ends[self.pieces - 2] >= end {
            self.pieces -= 1;
        }
        if self.pieces > 0 {
            self.ends[self.pieces - 1] = self.ends[self.pieces - 1].min(end);
        }
    }

    /// Bytes readable from the range's start: its length and the rest of
    /// its last page.
    #[inline]
    pub fn end(&self) -> usize {
        match self.pieces {
            0 => 0,
            n => self.ends[n - 1],
        }
    }

    /// The first piece: its first byte and length.
    #[inline]
    pub fn head(&self) -> (*const u8, usize) {
        (self.data[0], self.ends[0])
    }

    /// Where byte `at` of the range lies, and the bytes of its piece from
    /// there on; `at` must be below [`HeldRange::end`].
    #[inline]
    pub fn locate(&self, at: usize) -> (*const u8, usize) {
        let mut start = 0;
        for piece in 0..self.pieces {
            let end = self.ends[piece];
            if at < end {
                // SAFETY: pointer arithmetic within the piece.
                return (unsafe { self.data[piece].add(at - start) }, end - at);
            }
            start = end;
        }
        panic!("byte {at} beyond a held range of {} bytes", self.end());
    }

    /// `N` bytes at `at`, gathered across pieces where they straddle two.
    ///
    /// # Safety
    ///
    /// The pages must still be pinned (see [`HeldRange`]), and
    /// `at + N <= self.end()`.
    #[inline]
    pub unsafe fn read<const N: usize>(&self, at: usize) -> [u8; N] {
        let (data, left) = self.locate(at);
        if left >= N {
            // SAFETY: `N` bytes of the piece, pinned per the contract.
            return unsafe { data.cast::<[u8; N]>().read_unaligned() };
        }
        let mut out = [0u8; N];
        for (i, byte) in out.iter_mut().enumerate() {
            let (data, _) = self.locate(at + i);
            // SAFETY: as above, a byte at a time.
            *byte = unsafe { *data };
        }
        out
    }

    /// Hands `visit` the bytes `from..to` of the range as contiguous runs,
    /// in order, with each run's offset in the range.
    ///
    /// # Safety
    ///
    /// As [`HeldRange::read`], with `to <= self.end()`.
    pub unsafe fn runs(&self, from: usize, to: usize, mut visit: impl FnMut(usize, &[u8])) {
        let mut at = from;
        while at < to {
            let (data, left) = self.locate(at);
            let take = left.min(to - at);
            // SAFETY: `take` bytes of one piece, pinned per the contract.
            visit(at, unsafe { std::slice::from_raw_parts(data, take) });
            at += take;
        }
    }
}

pub trait Source {
    /// Total bytes available.
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies `len` bytes starting at `offset`.
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>>;

    /// The same range shared: a source that copies page by page fills the
    /// shared allocation directly, where going through a vector copied
    /// every cached chunk twice.
    fn read_shared(&self, offset: u64, len: usize) -> Result<std::rc::Rc<[u8]>> {
        self.read(offset, len).map(std::rc::Rc::from)
    }

    /// A borrowed view of the range, when the source is contiguous in memory.
    fn slice(&self, offset: u64, len: usize) -> Option<&[u8]> {
        let _ = (offset, len);
        None
    }

    /// Opens (`true`) or closes (`false`) a span within which the source
    /// may keep a page per [`Source::held_span`] slot pinned between reads.
    /// Spans nest; closing the outermost releases every page held.
    fn hold(&self, open: bool) {
        let _ = open;
    }

    /// The outermost [`Source::hold`] span open or last opened, as a number
    /// no other outermost span of any source in the process shares: slots
    /// from [`Source::held_slot`] and the pages read through them belong
    /// to the span that handed them out, and a reader may check it still
    /// is the one open. Zero for a source that holds nothing.
    fn hold_generation(&self) -> u64 {
        0
    }

    /// The page covering `offset`, held pinned in `slot` in place of the
    /// page held there: for a table read a few bytes at a time in ascending
    /// order, where a copied window per read cost a buffer lookup and a
    /// copy for one value. `None` outside a [`Source::hold`] span and for a
    /// source that holds nothing.
    ///
    /// The span's bytes stay valid until the next call on `slot` or until
    /// the outermost hold span closes, whichever comes first; the caller
    /// must not read through `data` after either.
    fn held_span(&self, slot: usize, offset: u64) -> Option<Result<HeldSpan>> {
        let _ = (slot, offset);
        None
    }

    /// A fresh slot beyond the [`HELD_SLOTS`] of the tables, for one reader
    /// of [`Source::held_range`] or [`Source::held_span`] within the open
    /// [`Source::hold`] span: a walked term's chunks, its bucket nibbles,
    /// a phrase slot's positions. The slots end with the outermost span.
    /// `None` outside a span, for a source that holds nothing, or once the
    /// source's bound on slots is reached; the caller then copies.
    fn held_slot(&self) -> Option<usize> {
        None
    }

    /// The pages covering `len` bytes at `offset`, held pinned in `slot` in
    /// place of what it held there; a page it held already is kept rather
    /// than pinned again. `None` outside a [`Source::hold`] span, for a
    /// source that holds nothing, and for a range over more than
    /// [`HELD_PIECES`] pages. The range is valid as a [`HeldSpan`] is.
    fn held_range(&self, slot: usize, offset: u64, len: usize) -> Option<Result<HeldRange>> {
        let _ = (slot, offset, len);
        None
    }
}

fn check(total: u64, offset: u64, len: usize) -> Result<usize> {
    let end = offset.checked_add(len as u64).ok_or(Error::Truncated)?;
    if end > total {
        return Err(Error::Truncated);
    }
    Ok(offset as usize)
}

impl Source for [u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let at = check(Source::len(self), offset, len)?;
        Ok(self[at..at + len].to_vec())
    }
    fn slice(&self, offset: u64, len: usize) -> Option<&[u8]> {
        let at = check(Source::len(self), offset, len).ok()?;
        Some(&self[at..at + len])
    }
}

impl Source for &[u8] {
    fn len(&self) -> u64 {
        <[u8] as Source>::len(self)
    }
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        <[u8] as Source>::read(self, offset, len)
    }
    fn slice(&self, offset: u64, len: usize) -> Option<&[u8]> {
        <[u8] as Source>::slice(self, offset, len)
    }
}

impl Source for Vec<u8> {
    fn len(&self) -> u64 {
        <[u8] as Source>::len(self)
    }
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        <[u8] as Source>::read(self, offset, len)
    }
    fn slice(&self, offset: u64, len: usize) -> Option<&[u8]> {
        <[u8] as Source>::slice(self, offset, len)
    }
}

impl Source for Rc<Vec<u8>> {
    fn len(&self) -> u64 {
        <[u8] as Source>::len(self)
    }
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        <[u8] as Source>::read(self, offset, len)
    }
    fn slice(&self, offset: u64, len: usize) -> Option<&[u8]> {
        <[u8] as Source>::slice(self, offset, len)
    }
}

impl Source for Box<dyn Source> {
    fn len(&self) -> u64 {
        (**self).len()
    }
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        (**self).read(offset, len)
    }
    fn read_shared(&self, offset: u64, len: usize) -> Result<Rc<[u8]>> {
        (**self).read_shared(offset, len)
    }
    fn slice(&self, offset: u64, len: usize) -> Option<&[u8]> {
        (**self).slice(offset, len)
    }
    fn hold(&self, open: bool) {
        (**self).hold(open);
    }
    fn hold_generation(&self) -> u64 {
        (**self).hold_generation()
    }
    fn held_span(&self, slot: usize, offset: u64) -> Option<Result<HeldSpan>> {
        (**self).held_span(slot, offset)
    }
    fn held_slot(&self) -> Option<usize> {
        (**self).held_slot()
    }
    fn held_range(&self, slot: usize, offset: u64, len: usize) -> Option<Result<HeldRange>> {
        (**self).held_range(slot, offset, len)
    }
}

/// A source that serves fixed-size pages and copies the ones a range covers.
/// Page-backed storage implements [`PageSource::page`]; everything else is
/// shared.
pub trait PageSource {
    /// Bytes of data per page.
    fn page_len(&self) -> usize;
    /// Number of pages.
    fn pages(&self) -> u64;
    /// Total data bytes (the last page may be partial).
    fn data_len(&self) -> u64;
    /// The data of page `index`, shared so a cache hit costs no copy.
    fn page(&self, index: u64) -> Result<Rc<[u8]>>;
}

impl<P: PageSource> Source for P {
    fn len(&self) -> u64 {
        self.data_len()
    }

    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        check(self.data_len(), offset, len)?;
        let page_len = self.page_len() as u64;
        let mut out = Vec::with_capacity(len);
        let mut at = offset;
        let end = offset + len as u64;
        while at < end {
            let index = at / page_len;
            let within = (at % page_len) as usize;
            let page = self.page(index)?;
            let take = ((end - at) as usize).min(page.len().saturating_sub(within));
            if take == 0 {
                return Err(Error::Truncated);
            }
            out.extend_from_slice(&page[within..within + take]);
            at += take as u64;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Paged(Vec<u8>);

    impl PageSource for Paged {
        fn page_len(&self) -> usize {
            7
        }
        fn pages(&self) -> u64 {
            (self.0.len() as u64).div_ceil(7)
        }
        fn data_len(&self) -> u64 {
            self.0.len() as u64
        }
        fn page(&self, index: u64) -> Result<Rc<[u8]>> {
            let start = index as usize * 7;
            Ok(Rc::from(&self.0[start..(start + 7).min(self.0.len())]))
        }
    }

    #[test]
    fn paged_reads_match_contiguous_reads() {
        let data: Vec<u8> = (0..100).collect();
        let paged = Paged(data.clone());
        assert_eq!(paged.pages(), 15);
        for (offset, len) in [(0, 0), (0, 7), (3, 10), (6, 2), (93, 7), (99, 1), (50, 50)] {
            assert_eq!(
                paged.read(offset, len).unwrap(),
                data.read(offset, len).unwrap()
            );
        }
        assert!(paged.read(94, 7).is_err());
        assert!(data.as_slice().read(100, 1).is_err());
        assert_eq!(data.slice(10, 5), Some(&data[10..15]));
        assert!(paged.slice(0, 1).is_none());
    }
}
