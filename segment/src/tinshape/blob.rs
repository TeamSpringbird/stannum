// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A segment's blob read on demand, and [`Bytes`], a range of a blob
//! either in memory or read on demand.
//!
//! The extension's ctid-native paths keep a [`LazyBlob`] per segment in a
//! backend. A query reads through [`Bytes::get`]. Within a span
//! ([`LazyBlob::open_span`]) a range on one page is read in place from the
//! page, which the source keeps pinned in shared buffers until the span
//! closes, and a range across pages is stitched into a buffer the span
//! frees: nothing a query reads is kept. Outside a span the range's
//! [`CHUNK`]s are copied in the first time any query asks for them and kept
//! (one entry of a common word's positions, one block of the DL sidecar),
//! so what a backend holds grows with the pages those reads touch, not with
//! the areas they fall in. Unread ranges stay zero pages the allocator has
//! not committed.
//!
//! At 150 million rows a segment is some 2.6 GB: the chunks queries copied
//! overflowed every backend's share of `stannum.reader_cache_mb`, so each
//! query copied what the last had, 15,000 pages of it (see
//! docs/architecture/tin-shape.md).
//!
//! A chunk counts as loaded only once its bytes are in place: a read that
//! fails, or unwinds (a query cancel raised inside a page read), leaves its
//! chunks unloaded, so the next query reads them again rather than taking
//! zeros for the segment's bytes.
//!
//! Soundness: a slice over loaded chunks lives as long as the blob: the
//! blob's bytes are a raw allocation, a load writes only chunks not loaded
//! yet, and loaded chunks stay loaded until the blob is dropped. A slice
//! read within a span (a pinned page, or the span's stitch buffers) lives
//! until the span closes: [`LazyBlob::close_span`] is `unsafe`, and its
//! caller drops whatever borrowed the span's slices first.

use std::cell::{Cell, RefCell};
use std::mem::MaybeUninit;
use std::ptr::NonNull;

use crate::source::Source;
use crate::{Error, Result};

/// Bytes of a blob loaded at a time.
pub const CHUNK: usize = 8192;

/// Most bytes one read copies: a long run of missing chunks is read in
/// pieces of this size, so a large area costs no transient copy of itself.
const PIECE: usize = 128 * CHUNK;

/// What a read through [`Bytes`] is of, for accounting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// The header, the term map, the document set and anything untagged.
    #[default]
    Other,
    /// A postings record's header and group directory.
    Record,
    /// A postings record's footer.
    Footer,
    /// A grouped record's containers.
    Container,
    /// A sparse record's list, or a single posting.
    Sparse,
    /// A record's TF tail.
    Tf,
    /// A rare term's inline lengths.
    InlineLengths,
    /// The DL sidecar.
    Lengths,
    /// Positions.
    Positions,
}

/// Kinds of [`Kind`].
pub const KINDS: usize = 9;

/// [`Kind`]s by number, as reports name them.
pub const KIND_NAMES: [&str; KINDS] = [
    "other",
    "record",
    "footer",
    "container",
    "sparse",
    "tf",
    "inline lengths",
    "lengths",
    "positions",
];

/// What a backend's blobs read, per [`Kind`], since [`reset_stats`]: only
/// the slow paths count, so a read served from a page pinned already costs
/// nothing to account.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KindStats {
    /// Bytes copied into blobs' chunks, kept across queries.
    pub copied: u64,
    /// Pages those copies read.
    pub copied_pages: u64,
    /// Pages pinned to read in place.
    pub pinned: u64,
    /// Bytes stitched across page boundaries into a span's buffers.
    pub stitched: u64,
    /// Reads stitched.
    pub stitches: u64,
}

const NO_STATS: KindStats = KindStats {
    copied: 0,
    copied_pages: 0,
    pinned: 0,
    stitched: 0,
    stitches: 0,
};

thread_local! {
    static STATS: RefCell<[KindStats; KINDS]> = const { RefCell::new([NO_STATS; KINDS]) };
}

/// What this thread's blobs read per [`Kind`] since [`reset_stats`].
pub fn stats() -> [KindStats; KINDS] {
    STATS.with_borrow(|s| *s)
}

pub fn reset_stats() {
    STATS.with_borrow_mut(|s| *s = [NO_STATS; KINDS]);
}

fn count(kind: Kind, add: impl FnOnce(&mut KindStats)) {
    STATS.with_borrow_mut(|s| add(&mut s[kind as usize]));
}

/// Pages a span's reads remember, direct-mapped by page. An entry holds
/// the span it was pinned in, so closing a span forgets them all without
/// touching them.
const PAGE_MEMO: usize = 1024;

/// A page a span pinned: its index, the span, its bytes and their length.
#[derive(Clone, Copy)]
struct PageRef {
    page: usize,
    span: u64,
    data: *const u8,
    len: usize,
}

const NO_PAGE: PageRef = PageRef {
    page: usize::MAX,
    span: 0,
    data: std::ptr::null(),
    len: 0,
};

/// Bytes of a stitch buffer: a range across pages is copied into the
/// current one, or into a buffer of its own when larger than a quarter.
const STITCH_BLOCK: usize = 64 * 1024;

/// Bytes of stitch buffers a blob keeps for its next span: a query
/// stitches some kilobytes per segment (a record's directory, a footer, a
/// sparse list), which allocating and freeing per span cost again each time
/// (a large one mapped and zeroed by the kernel).
const STITCH_KEEP: usize = 1 << 20;

type Buffer = Box<[MaybeUninit<u8>]>;

/// A span's stitch buffers. A buffer is never reallocated, so a slice of
/// one stays valid until the span closes; then the buffers are kept, up to
/// [`STITCH_KEEP`] bytes, for the next span. Nothing is zeroed: a stitch
/// writes every byte it hands out.
#[derive(Default)]
struct Stitches {
    /// Blocks of [`STITCH_BLOCK`] handed out in the span; the last is being
    /// filled, `used` bytes of it.
    blocks: Vec<Buffer>,
    used: usize,
    /// Buffers of their own handed out in the span.
    large: Vec<Buffer>,
    /// Kept for the next span.
    free_blocks: Vec<Buffer>,
    free_large: Vec<Buffer>,
    /// Bytes stitched in the span.
    bytes: usize,
}

impl Stitches {
    /// A buffer of `len` bytes, valid until [`Self::clear`], its contents
    /// unspecified: the caller writes them all before reading any.
    fn alloc(&mut self, len: usize) -> *mut u8 {
        self.bytes += len;
        if len > STITCH_BLOCK / 4 {
            let reuse = self
                .free_large
                .iter()
                .enumerate()
                .filter(|(_, b)| b.len() >= len)
                .min_by_key(|(_, b)| b.len())
                .map(|(i, _)| i);
            let mut own = match reuse {
                Some(i) => self.free_large.swap_remove(i),
                None => Box::new_uninit_slice(len.next_power_of_two()),
            };
            let at = own.as_mut_ptr().cast::<u8>();
            self.large.push(own);
            return at;
        }
        if self.blocks.is_empty() || self.used + len > STITCH_BLOCK {
            let block = self
                .free_blocks
                .pop()
                .unwrap_or_else(|| Box::new_uninit_slice(STITCH_BLOCK));
            self.blocks.push(block);
            self.used = 0;
        }
        let block = self.blocks.last_mut().expect("a buffer");
        // SAFETY: `used + len` lies within the buffer.
        let at = unsafe { block.as_mut_ptr().cast::<u8>().add(self.used) };
        self.used += len;
        at
    }

    /// Ends the span: its buffers become free, the largest dropped past
    /// [`STITCH_KEEP`] bytes.
    fn clear(&mut self) {
        self.bytes = 0;
        self.used = 0;
        self.free_blocks.append(&mut self.blocks);
        self.free_large.append(&mut self.large);
        let mut kept = self.free_blocks.len() * STITCH_BLOCK;
        while kept > STITCH_KEEP && self.free_blocks.pop().is_some() {
            kept -= STITCH_BLOCK;
        }
        self.free_large.sort_unstable_by_key(|b| b.len());
        let mut cut = self.free_large.len();
        for (i, b) in self.free_large.iter().enumerate() {
            if kept + b.len() > STITCH_KEEP {
                cut = i;
                break;
            }
            kept += b.len();
        }
        self.free_large.truncate(cut);
    }

    /// Bytes of the buffers held, in use or kept.
    fn capacity(&self) -> usize {
        (self.blocks.len() + self.free_blocks.len()) * STITCH_BLOCK
            + self
                .large
                .iter()
                .chain(&self.free_large)
                .map(|b| b.len())
                .sum::<usize>()
    }
}

/// A segment's blob as far as it has been read, loading on demand from its
/// source; within a span, read in place (see the module documentation).
pub struct LazyBlob {
    data: NonNull<u8>,
    len: usize,
    /// Bit per [`CHUNK`]: loaded.
    chunks: Box<[Cell<u64>]>,
    loaded: Cell<usize>,
    source: Box<dyn Source>,
    /// The source's bytes per page, zero when it pins none.
    page_len: usize,
    /// Open [`Self::open_span`]s.
    spans: Cell<u32>,
    /// The outermost span open or last opened, numbering them from 1.
    span: Cell<u64>,
    /// Pages the open span pinned, by page modulo [`PAGE_MEMO`].
    pages: Box<[Cell<PageRef>; PAGE_MEMO]>,
    stitches: RefCell<Stitches>,
}

impl Drop for LazyBlob {
    fn drop(&mut self) {
        // SAFETY: `data` and `len` came from `Box::into_raw` in `new`.
        drop(unsafe {
            Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                self.data.as_ptr(),
                self.len,
            ))
        });
    }
}

impl std::fmt::Debug for LazyBlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyBlob")
            .field("len", &self.len)
            .field("loaded", &self.loaded.get())
            .finish()
    }
}

impl LazyBlob {
    /// A blob of `source`'s bytes, none loaded yet.
    pub fn new(source: Box<dyn Source>) -> Self {
        let len = usize::try_from(source.len()).expect("a segment fits in memory");
        let bytes = vec![0u8; len].into_boxed_slice();
        let data = NonNull::new(Box::into_raw(bytes).cast::<u8>()).expect("a boxed slice");
        let page_len = source.page_len();
        Self {
            data,
            len,
            chunks: (0..len.div_ceil(CHUNK).div_ceil(64))
                .map(|_| Cell::new(0))
                .collect(),
            loaded: Cell::new(0),
            source,
            page_len,
            spans: Cell::new(0),
            span: Cell::new(0),
            pages: Box::new([const { Cell::new(NO_PAGE) }; PAGE_MEMO]),
            stitches: RefCell::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes loaded so far.
    pub fn loaded(&self) -> usize {
        self.loaded.get()
    }

    /// Bytes the blob holds besides its loaded chunks: the chunk bitmap,
    /// the span's page memo and the stitch buffers it keeps.
    pub fn overhead(&self) -> usize {
        self.chunks.len() * 8
            + self.pages.len() * std::mem::size_of::<PageRef>()
            + self.stitches.borrow().capacity()
    }

    /// Bytes the open span stitched so far, freed when it closes.
    pub fn stitched(&self) -> usize {
        self.stitches.borrow().bytes
    }

    /// The whole blob as [`Bytes`].
    pub fn bytes(&self) -> Bytes<'_> {
        Bytes::Lazy {
            blob: self,
            at: 0,
            len: self.len,
            kind: Kind::Other,
        }
    }

    /// Opens a span: until it closes, reads are served in place from pages
    /// the source keeps pinned ([`Source::pinned_page`]) rather than copied
    /// into the blob. Spans nest.
    pub fn open_span(&self) {
        self.source.hold(true);
        if self.spans.get() == 0 {
            self.span.set(self.span.get() + 1);
        }
        self.spans.set(self.spans.get() + 1);
    }

    /// Closes the span [`Self::open_span`] opened; the outermost releases
    /// its pages and frees its stitch buffers.
    ///
    /// # Safety
    ///
    /// Nothing read from the blob within the outermost span is used after:
    /// the slices handed out there point into pages no longer pinned and
    /// buffers freed.
    pub unsafe fn close_span(&self) {
        let depth = self.spans.get().saturating_sub(1);
        self.spans.set(depth);
        if depth == 0 {
            // The page memo's entries name the span: none is served after.
            self.stitches.borrow_mut().clear();
        }
        self.source.hold(false);
    }

    fn is_loaded(&self, chunk: usize) -> bool {
        self.chunks[chunk / 64].get() >> (chunk % 64) & 1 == 1
    }

    /// Whether `[offset, offset + len)` is loaded.
    #[inline]
    pub fn has(&self, offset: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let end = offset.saturating_add(len).min(self.len);
        let first = offset / CHUNK;
        if end <= (first + 1) * CHUNK {
            return self.is_loaded(first);
        }
        (first..end.div_ceil(CHUNK)).all(|chunk| self.is_loaded(chunk))
    }

    /// Loads `[from, to)` where not loaded yet.
    pub fn ensure(&self, from: usize, to: usize) -> Result<()> {
        self.ensure_kind(from, to, Kind::Other)
    }

    fn ensure_kind(&self, from: usize, to: usize, kind: Kind) -> Result<()> {
        if from >= to {
            return Ok(());
        }
        if to > self.len {
            return Err(Error::Truncated);
        }
        let mut chunk = from / CHUNK;
        while chunk * CHUNK < to {
            if self.is_loaded(chunk) {
                chunk += 1;
                continue;
            }
            // A run of missing chunks, read a piece at a time.
            let first = chunk;
            while chunk * CHUNK < to
                && !self.is_loaded(chunk)
                && (chunk - first + 1) * CHUNK <= PIECE
            {
                chunk += 1;
            }
            let start = first * CHUNK;
            let end = (chunk * CHUNK).min(self.len);
            let read = self.source.read(start as u64, end - start)?;
            if read.len() != end - start {
                return Err(Error::Truncated);
            }
            // SAFETY: `start..end` lies within the allocation and covers only
            // chunks not loaded, which no slice handed out covers.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    read.as_ptr(),
                    self.data.as_ptr().add(start),
                    end - start,
                );
            }
            // Only now: a read that failed or unwound marked nothing.
            for c in first..chunk {
                let word = &self.chunks[c / 64];
                word.set(word.get() | 1 << (c % 64));
            }
            self.loaded.set(self.loaded.get() + (end - start));
            let pages = match self.page_len {
                0 => 0,
                n => ((end - 1) / n - start / n + 1) as u64,
            };
            count(kind, |s| {
                s.copied += (end - start) as u64;
                s.copied_pages += pages;
            });
        }
        Ok(())
    }

    /// `[from, to)`: within a span read in place, else loaded first where
    /// it is not.
    pub fn get(&self, from: usize, to: usize) -> Result<&[u8]> {
        self.get_kind(from, to, Kind::Other)
    }

    /// `[from, to)` of `kind`: a range on a page the open span pinned
    /// already, the common case, read here; anything else in
    /// [`Self::get_slow`].
    #[inline(always)]
    fn get_kind(&self, from: usize, to: usize, kind: Kind) -> Result<&[u8]> {
        if self.spans.get() > 0 && from < to && self.page_len > 0 {
            let n = self.page_len;
            let page = from / n;
            let known = self.pages[page % PAGE_MEMO].get();
            if known.page == page && known.span == self.span.get() && to <= page * n + known.len {
                // SAFETY: within a page the open span pinned, as in
                // `in_place`; the caller of `close_span` drops this slice
                // first.
                return Ok(unsafe {
                    std::slice::from_raw_parts(known.data.add(from - page * n), to - from)
                });
            }
        }
        self.get_slow(from, to, kind)
    }

    #[inline(never)]
    fn get_slow(&self, from: usize, to: usize, kind: Kind) -> Result<&[u8]> {
        if from > to {
            return Err(Error::Truncated);
        }
        if self.spans.get() > 0 && self.page_len > 0 {
            if to > self.len {
                return Err(Error::Truncated);
            }
            if from == to {
                return Ok(&[]);
            }
            if self.loaded.get() == 0 || !self.has(from, to - from) {
                return self.in_place(from, to, kind);
            }
        }
        self.ensure_kind(from, to, kind)?;
        // SAFETY: every chunk of the range is loaded (above), and loaded
        // chunks are never written again while `self` lives.
        Ok(unsafe { std::slice::from_raw_parts(self.data.as_ptr().add(from), to - from) })
    }

    /// Page `page` pinned within the open span, its bytes and their
    /// length; `None` when the source pins no more.
    #[inline]
    fn page(&self, page: usize, kind: Kind) -> Result<Option<(*const u8, usize)>> {
        let memo = &self.pages[page % PAGE_MEMO];
        let known = memo.get();
        if known.page == page && known.span == self.span.get() {
            return Ok(Some((known.data, known.len)));
        }
        match self.source.pinned_page((page * self.page_len) as u64) {
            None => Ok(None),
            Some(Err(error)) => Err(error),
            Some(Ok(span)) => {
                if span.pinned {
                    count(kind, |s| s.pinned += 1);
                }
                memo.set(PageRef {
                    page,
                    span: self.span.get(),
                    data: span.data,
                    len: span.len,
                });
                Ok(Some((span.data, span.len)))
            }
        }
    }

    /// `[from, to)` (non-empty, within the blob) within the open span: in
    /// place on its page, else stitched.
    fn in_place(&self, from: usize, to: usize, kind: Kind) -> Result<&[u8]> {
        let n = self.page_len;
        let first = from / n;
        if (to - 1) / n == first
            && let Some((data, len)) = self.page(first, kind)?
        {
            let within = from - first * n;
            if to - first * n > len {
                return Err(Error::Truncated);
            }
            // SAFETY: within the page's bytes, which the source keeps
            // pinned until the span closes; the caller of `close_span`
            // drops this slice first.
            return Ok(unsafe { std::slice::from_raw_parts(data.add(within), to - from) });
        }
        // Across pages, or past the pages the source pins: stitched.
        let out = self.stitches.borrow_mut().alloc(to - from);
        let mut at = from;
        while at < to {
            let page = at / n;
            let take = ((page + 1) * n).min(to) - at;
            match self.page(page, kind)? {
                Some((data, len)) => {
                    let within = at - page * n;
                    if within + take > len {
                        return Err(Error::Truncated);
                    }
                    // SAFETY: `take` bytes of a pinned page into the
                    // buffer's `at - from..`, distinct allocations.
                    unsafe {
                        std::ptr::copy_nonoverlapping(data.add(within), out.add(at - from), take);
                    }
                }
                None => {
                    let read = self.source.read(at as u64, take)?;
                    if read.len() != take {
                        return Err(Error::Truncated);
                    }
                    // SAFETY: as above.
                    unsafe {
                        std::ptr::copy_nonoverlapping(read.as_ptr(), out.add(at - from), take);
                    }
                }
            }
            at += take;
        }
        count(kind, |s| {
            s.stitched += (to - from) as u64;
            s.stitches += 1;
        });
        // SAFETY: the buffer holds `to - from` bytes, written above, and
        // lives until the span closes.
        Ok(unsafe { std::slice::from_raw_parts(out, to - from) })
    }
}

/// A range of a segment's bytes: in memory, or in a [`LazyBlob`] that
/// loads what is read.
#[derive(Clone, Copy)]
pub enum Bytes<'a> {
    Slice(&'a [u8]),
    Lazy {
        blob: &'a LazyBlob,
        at: usize,
        len: usize,
        /// What the range holds, for accounting.
        kind: Kind,
    },
}

impl std::fmt::Debug for Bytes<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Slice(bytes) => write!(f, "Bytes::Slice({} bytes)", <[u8]>::len(bytes)),
            Self::Lazy { at, len, .. } => write!(f, "Bytes::Lazy({len} bytes at {at})"),
        }
    }
}

impl Default for Bytes<'_> {
    fn default() -> Self {
        Self::Slice(&[])
    }
}

impl<'a> From<&'a [u8]> for Bytes<'a> {
    fn from(bytes: &'a [u8]) -> Self {
        Self::Slice(bytes)
    }
}

impl<'a, const N: usize> From<&'a [u8; N]> for Bytes<'a> {
    fn from(bytes: &'a [u8; N]) -> Self {
        Self::Slice(bytes)
    }
}

impl<'a> From<&'a Vec<u8>> for Bytes<'a> {
    fn from(bytes: &'a Vec<u8>) -> Self {
        Self::Slice(bytes)
    }
}

impl<'a> Bytes<'a> {
    pub fn len(&self) -> usize {
        match self {
            Self::Slice(bytes) => <[u8]>::len(bytes),
            Self::Lazy { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the range borrows no bytes: a range of a [`LazyBlob`] reads
    /// its bytes when asked, so it may be kept beyond the span a read of it
    /// was made in.
    pub fn is_lazy(&self) -> bool {
        matches!(self, Self::Lazy { .. })
    }

    /// The range tagged as holding `kind`, for accounting.
    #[must_use]
    pub fn tag(self, kind: Kind) -> Self {
        match self {
            Self::Slice(_) => self,
            Self::Lazy { blob, at, len, .. } => Self::Lazy {
                blob,
                at,
                len,
                kind,
            },
        }
    }

    /// Bytes `[from, to)`; `Truncated` past the end.
    #[inline(always)]
    pub fn get(&self, from: usize, to: usize) -> Result<&'a [u8]> {
        match *self {
            Self::Slice(bytes) => bytes.get(from..to).ok_or(Error::Truncated),
            Self::Lazy {
                blob,
                at,
                len,
                kind,
            } => {
                if from > to || to > len {
                    return Err(Error::Truncated);
                }
                blob.get_kind(at + from, at + to, kind)
            }
        }
    }

    /// Up to `max` bytes from `from`, fewer at the end.
    #[inline]
    pub fn window(&self, from: usize, max: usize) -> Result<&'a [u8]> {
        let len = self.len();
        if from > len {
            return Err(Error::Truncated);
        }
        self.get(from, from + max.min(len - from))
    }

    /// Where the page holding byte `from` of the range ends, as an offset
    /// in the range (its length at most): bytes `from..page_end(from)` are
    /// read in place within a span, on one page, with nothing stitched. A
    /// range in memory is one page.
    #[inline]
    pub fn page_end(&self, from: usize) -> usize {
        match *self {
            Self::Slice(bytes) => <[u8]>::len(bytes),
            Self::Lazy { blob, at, len, .. } => {
                let n = blob.page_len;
                if n == 0 {
                    return len;
                }
                ((at + from) / n * n + n - at).min(len)
            }
        }
    }

    /// Every byte.
    pub fn all(&self) -> Result<&'a [u8]> {
        self.get(0, self.len())
    }

    /// Bytes `[from, to)` as a range of their own, nothing loaded.
    pub fn sub(&self, from: usize, to: usize) -> Result<Bytes<'a>> {
        if from > to || to > self.len() {
            return Err(Error::Truncated);
        }
        Ok(match *self {
            Self::Slice(bytes) => Self::Slice(&bytes[from..to]),
            Self::Lazy { blob, at, kind, .. } => Self::Lazy {
                blob,
                at: at + from,
                len: to - from,
                kind,
            },
        })
    }
}

/// A source over bytes in pages of a given length that pins pages as a
/// page-backed source does ([`Source::pinned_page`]), for tests of reads in
/// place: a page is copied out when pinned and poisoned (every byte 0xA5)
/// when its span closes, so a slice used past its span reads garbage
/// rather than the right bytes by luck.
#[derive(Clone)]
pub struct PinningSource(pub std::rc::Rc<PinState>);

/// What a [`PinningSource`] holds and has done.
pub struct PinState {
    pub bytes: Vec<u8>,
    pub page_len: usize,
    /// Open hold spans.
    pub holding: Cell<u32>,
    /// Pages pinned in the open span, by page.
    pub pinned: RefCell<std::collections::BTreeMap<usize, Box<[u8]>>>,
    /// Pages released and poisoned, kept so a stale slice stays readable.
    pub released: RefCell<Vec<Box<[u8]>>>,
    /// Pages pinned in all, and copying reads made.
    pub pins: Cell<usize>,
    pub reads: Cell<usize>,
    /// Most pages a span pins; past it `pinned_page` is `None`.
    pub limit: Cell<usize>,
    /// While set, pins and reads fail (`false`) or panic (`true`).
    pub fail: Cell<Option<bool>>,
}

impl PinningSource {
    pub fn new(bytes: Vec<u8>, page_len: usize) -> Self {
        assert!(page_len > 0);
        Self(std::rc::Rc::new(PinState {
            bytes,
            page_len,
            holding: Cell::new(0),
            pinned: RefCell::default(),
            released: RefCell::default(),
            pins: Cell::new(0),
            reads: Cell::new(0),
            limit: Cell::new(usize::MAX),
            fail: Cell::new(None),
        }))
    }

    fn failing(&self) -> Result<()> {
        match self.0.fail.get() {
            Some(true) => panic!("canceling statement due to user request"),
            Some(false) => Err(Error::Truncated),
            None => Ok(()),
        }
    }
}

impl Source for PinningSource {
    fn len(&self) -> u64 {
        self.0.bytes.len() as u64
    }

    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.0.reads.set(self.0.reads.get() + 1);
        self.failing()?;
        let at = offset as usize;
        self.0
            .bytes
            .get(at..at + len)
            .map(<[u8]>::to_vec)
            .ok_or(Error::Truncated)
    }

    fn hold(&self, open: bool) {
        let state = &self.0;
        if open {
            state.holding.set(state.holding.get() + 1);
            return;
        }
        let depth = state.holding.get().saturating_sub(1);
        state.holding.set(depth);
        if depth == 0 {
            let mut pinned = state.pinned.borrow_mut();
            let mut released = state.released.borrow_mut();
            for (_, mut page) in std::mem::take(&mut *pinned) {
                page.fill(0xA5);
                released.push(page);
            }
        }
    }

    fn holding(&self) -> bool {
        self.0.holding.get() > 0
    }

    fn page_len(&self) -> usize {
        self.0.page_len
    }

    fn pinned_page(&self, offset: u64) -> Option<Result<crate::source::HeldSpan>> {
        let state = &self.0;
        if state.holding.get() == 0 {
            return None;
        }
        let page = offset as usize / state.page_len;
        let start = page * state.page_len;
        if start >= state.bytes.len() {
            return Some(Err(Error::Truncated));
        }
        let mut pinned = state.pinned.borrow_mut();
        let fresh = !pinned.contains_key(&page);
        if fresh {
            if pinned.len() >= state.limit.get() {
                return None;
            }
            if let Err(error) = self.failing() {
                return Some(Err(error));
            }
            let end = (start + state.page_len).min(state.bytes.len());
            pinned.insert(page, state.bytes[start..end].into());
            state.pins.set(state.pins.get() + 1);
        }
        let data = &pinned[&page];
        Some(Ok(crate::source::HeldSpan {
            start: start as u64,
            data: data.as_ptr(),
            len: data.len(),
            pinned: fresh,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn pinning(len: usize, page_len: usize) -> (PinningSource, LazyBlob) {
        let bytes: Vec<u8> = (0..len).map(|i| (i * 13 + 5) as u8).collect();
        let source = PinningSource::new(bytes, page_len);
        let blob = LazyBlob::new(Box::new(source.clone()));
        (source, blob)
    }

    #[test]
    fn a_span_reads_in_place_and_copies_nothing() {
        let (source, blob) = pinning(10 * 100 + 37, 100);
        let state = &source.0;
        blob.open_span();
        let bytes = blob.bytes();
        // Within a page: a slice of the pinned page itself.
        let within = bytes.get(205, 290).unwrap();
        assert_eq!(within, &state.bytes[205..290]);
        assert_eq!(
            within.as_ptr(),
            state.pinned.borrow()[&2][5..].as_ptr(),
            "read in place"
        );
        // Across pages: stitched, the same bytes.
        assert_eq!(bytes.get(150, 420).unwrap(), &state.bytes[150..420]);
        assert_eq!(bytes.window(1000, 100).unwrap(), &state.bytes[1000..1037]);
        assert_eq!(blob.loaded(), 0, "a span copies nothing into the blob");
        assert_eq!(state.reads.get(), 0);
        // A page read again in the span is the same pin.
        let pins = state.pins.get();
        bytes.get(210, 220).unwrap();
        assert_eq!(state.pins.get(), pins);
        assert!(blob.stitched() >= 270);
        unsafe { blob.close_span() };
        assert!(
            state.pinned.borrow().is_empty(),
            "the span's pages released"
        );
        assert_eq!(state.holding.get(), 0);
        assert_eq!(blob.stitched(), 0);
        // Outside a span the old way: chunks copied and kept.
        assert_eq!(bytes.get(205, 290).unwrap(), &state.bytes[205..290]);
        assert!(blob.loaded() > 0);
        assert_eq!(state.pins.get(), pins);
    }

    #[test]
    fn page_end_bounds_reads_in_place() {
        let (source, blob) = pinning(10 * 100 + 37, 100);
        let range = blob.bytes().sub(150, 900).unwrap();
        assert_eq!(range.page_end(0), 50);
        assert_eq!(range.page_end(49), 50);
        assert_eq!(range.page_end(50), 150);
        assert_eq!(range.page_end(745), 750);
        let all = blob.bytes();
        assert_eq!(all.page_end(1000), 1037);
        blob.open_span();
        let stitched = blob.stitched();
        assert_eq!(
            range.get(50, range.page_end(50)).unwrap(),
            &source.0.bytes[200..300]
        );
        assert_eq!(blob.stitched(), stitched, "read in place");
        unsafe { blob.close_span() };
        let memory = [1u8, 2, 3];
        assert_eq!(Bytes::from(&memory).page_end(1), 3);
    }

    #[test]
    fn a_span_past_its_pin_bound_stitches_by_copying() {
        let (source, blob) = pinning(1000, 100);
        let state = &source.0;
        state.limit.set(2);
        blob.open_span();
        let bytes = blob.bytes();
        for (from, to) in [(10, 20), (110, 120), (210, 220), (250, 450), (990, 1000)] {
            assert_eq!(bytes.get(from, to).unwrap(), &state.bytes[from..to]);
        }
        assert_eq!(state.pinned.borrow().len(), 2);
        assert!(state.reads.get() > 0);
        assert_eq!(blob.loaded(), 0);
        unsafe { blob.close_span() };
        assert!(state.pinned.borrow().is_empty());
    }

    #[test]
    fn a_failed_or_unwound_read_in_a_span_keeps_nothing() {
        let (source, blob) = pinning(1000, 100);
        let state = &source.0;
        blob.open_span();
        let bytes = blob.bytes();
        state.fail.set(Some(false));
        assert!(bytes.get(150, 420).is_err());
        state.fail.set(Some(true));
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            bytes.get(510, 520).map(<[u8]>::to_vec)
        }));
        assert!(unwound.is_err());
        // A walk unwinding closes its span (the extension's guard).
        unsafe { blob.close_span() };
        assert!(state.pinned.borrow().is_empty());
        assert_eq!(state.holding.get(), 0);
        state.fail.set(None);
        assert_eq!(blob.loaded(), 0);
        blob.open_span();
        assert_eq!(bytes.get(150, 420).unwrap(), &state.bytes[150..420]);
        assert_eq!(bytes.get(510, 520).unwrap(), &state.bytes[510..520]);
        unsafe { blob.close_span() };
    }

    /// Stitch buffers outlive their span for the next one's stitches, and a
    /// page remembered in one span is pinned afresh in the next.
    #[test]
    fn spans_reuse_stitch_buffers_and_repin_pages() {
        let (source, blob) = pinning(64 * 1000, 1000);
        let state = &source.0;
        let bytes = blob.bytes();
        let mut previous: Option<(usize, usize)> = None;
        for round in 0..4 {
            blob.open_span();
            // Small stitches into a shared block, and one of its own.
            let small = bytes.get(990, 1010).unwrap();
            assert_eq!(small, &state.bytes[990..1010]);
            let large = bytes.get(1500, 1500 + 40_000).unwrap();
            assert_eq!(large, &state.bytes[1500..41_500]);
            let within = bytes.get(5, 15).unwrap();
            assert_eq!(within, &state.bytes[5..15]);
            let at = (small.as_ptr() as usize, large.as_ptr() as usize);
            if let Some(previous) = previous {
                assert_eq!(previous, at, "round {round}: the buffers reused");
            }
            previous = Some(at);
            let pins = state.pins.get();
            unsafe { blob.close_span() };
            assert!(state.pinned.borrow().is_empty());
            blob.open_span();
            // The page memo from the closed span is not served: the page is
            // pinned again (a poisoned stale copy would read 0xA5).
            assert_eq!(bytes.get(5, 15).unwrap(), &state.bytes[5..15]);
            assert_eq!(state.pins.get(), pins + 1);
            unsafe { blob.close_span() };
        }
        assert!(blob.overhead() >= 40_000, "kept buffers count as held");
        assert_eq!(blob.loaded(), 0);
    }

    #[test]
    fn spans_nest_and_the_outermost_releases() {
        let (source, blob) = pinning(1000, 100);
        let state = &source.0;
        blob.open_span();
        blob.open_span();
        let bytes = blob.bytes();
        let inner = bytes.get(10, 20).unwrap();
        unsafe { blob.close_span() };
        assert_eq!(state.pinned.borrow().len(), 1);
        assert_eq!(
            inner,
            &state.bytes[10..20],
            "still pinned in the outer span"
        );
        unsafe { blob.close_span() };
        assert!(state.pinned.borrow().is_empty());
    }

    #[test]
    fn chunks_copied_outside_a_span_are_read_from_the_copy_within_one() {
        let (source, blob) = pinning(1000, 100);
        let state = &source.0;
        let bytes = blob.bytes();
        let kept = bytes.get(10, 20).unwrap();
        blob.open_span();
        assert_eq!(bytes.get(10, 20).unwrap().as_ptr(), kept.as_ptr());
        assert_eq!(state.pins.get(), 0);
        unsafe { blob.close_span() };
        assert_eq!(kept, &state.bytes[10..20]);
    }

    /// A source over `bytes` whose reads fail (or panic, as a PostgreSQL
    /// error raised inside a page read unwinds) while `fail` is set.
    #[derive(Clone)]
    struct Flaky(Rc<FlakyState>);

    struct FlakyState {
        bytes: Vec<u8>,
        fail: Cell<Option<bool>>,
        reads: Cell<usize>,
    }

    impl Source for Flaky {
        fn len(&self) -> u64 {
            self.0.bytes.len() as u64
        }

        fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
            let state = &self.0;
            state.reads.set(state.reads.get() + 1);
            match state.fail.get() {
                Some(true) => panic!("canceling statement due to user request"),
                Some(false) => Err(Error::Truncated),
                None => Ok(state.bytes[offset as usize..offset as usize + len].to_vec()),
            }
        }
    }

    fn flaky(len: usize) -> (Flaky, LazyBlob) {
        let source = Flaky(Rc::new(FlakyState {
            bytes: (0..len).map(|i| (i * 7 + 1) as u8).collect(),
            fail: Cell::new(None),
            reads: Cell::new(0),
        }));
        let blob = LazyBlob::new(Box::new(source.clone()));
        (source, blob)
    }

    #[test]
    fn a_failed_read_leaves_its_range_unloaded() {
        let (source, blob) = flaky(5 * CHUNK + 100);
        source.0.fail.set(Some(false));
        assert!(blob.get(100, 100 + 3 * CHUNK).is_err());
        assert!(!blob.has(100, 1));
        assert_eq!(blob.loaded(), 0);
        source.0.fail.set(None);
        assert_eq!(
            blob.get(100, 100 + 3 * CHUNK).unwrap(),
            &source.0.bytes[100..100 + 3 * CHUNK]
        );
    }

    #[test]
    fn a_read_that_unwinds_leaves_its_range_unloaded() {
        let (source, blob) = flaky(5 * CHUNK + 100);
        source.0.fail.set(Some(true));
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            blob.get(CHUNK + 5, 5 * CHUNK + 5).map(<[u8]>::to_vec)
        }));
        assert!(unwound.is_err());
        assert!(!blob.has(CHUNK + 5, 1));
        source.0.fail.set(None);
        assert_eq!(blob.get(0, blob.len()).unwrap(), source.0.bytes.as_slice());
        assert_eq!(blob.loaded(), source.0.bytes.len());
    }

    #[test]
    fn a_long_run_is_read_in_pieces_and_once() {
        let (source, blob) = flaky(3 * PIECE + 10);
        assert_eq!(blob.get(0, blob.len()).unwrap(), source.0.bytes.as_slice());
        assert_eq!(source.0.reads.get(), 4);
        blob.get(5, PIECE).unwrap();
        assert_eq!(source.0.reads.get(), 4);
    }

    #[test]
    fn only_the_chunks_read_are_loaded() {
        let (source, blob) = flaky(64 * CHUNK);
        let bytes = blob.bytes().sub(10 * CHUNK, 40 * CHUNK).unwrap();
        // A slice handed out earlier stays valid while later loads write
        // other chunks.
        let early = bytes.get(0, 10).unwrap();
        assert_eq!(
            bytes.window(5 * CHUNK + 3, 4).unwrap(),
            &source.0.bytes[15 * CHUNK + 3..15 * CHUNK + 7]
        );
        assert_eq!(early, &source.0.bytes[10 * CHUNK..10 * CHUNK + 10]);
        assert_eq!(blob.loaded(), 2 * CHUNK);
        assert!(bytes.get(0, 30 * CHUNK + 1).is_err());
        assert_eq!(bytes.window(30 * CHUNK - 2, 10).unwrap().len(), 2);
    }
}
