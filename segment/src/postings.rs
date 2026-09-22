// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A term's tuple locations in heap order.
//!
//! Two body layouts share one stream header; the builder emits whichever is
//! smaller. Either may carry score bounds, which a term's postings do and the
//! document table does not: a table with one entry per block of
//! `BLOCK_POSTINGS` postings, or, when the stream is one block, a single
//! term bound without the per-block fields.
//!
//! ```text
//! stream  := form u8, count varint, [bounds_len varint, table | term_bound], body
//!            form bits: 1 grouped (else sparse), 2 table follows, 4 term bound follows
//! sparse  := (block_delta varint, offset varint)*        first block absolute
//! grouped := group_count varint, group*
//! group   := gid varint, count varint, page_bitmap[32], body_len varint, body
//!            gid is absolute for the first group, then (delta - 1)
//! body    := page*  one per set bit of page_bitmap, ascending
//! page    := 0x00, n varint, offset u16le * n     (n <= LIST_MAX)
//!          | 0x01, tuple_bitmap[37]               (bit offset-1 set)
//! table   := entry * ceil(count / BLOCK_POSTINGS)
//! entry   := term_bound,
//!            last_block varint (delta from the previous entry), last_offset varint,
//!            [sparse: start varint, byte offset into body, delta from the previous entry]
//! term_bound := buckets varint (bit b set: bucket b occurs in the block),
//!            min_len varint per set bit ascending
//! ```
//!
//! Groups cover 256 consecutive heap blocks, so intersecting two grouped terms
//! can skip whole groups and whole pages without decoding offsets. Every group
//! and page carries its count, so `seek` maintains the ordinal of the current
//! posting, which is how the parallel payload stream is addressed.
//!
//! A bound describes a run of postings: for every term-frequency bucket that
//! occurs in the run, the shortest document it occurs in. A BM25 contribution
//! never grows with the document length, so the best score in the run under
//! any parameters is the best of those (bucket, length) pairs: a ranked scan
//! bounds the run exactly without decoding it. A table entry's last location
//! tells it the range of tuples the bound covers, and sparse streams also
//! record where each block starts, so their `seek` jumps over whole blocks
//! through the table. A stream of at most `BLOCK_POSTINGS` postings is one
//! block, so `LSG3` writers store only the term bound (form bit 4): its last
//! location is the stream's last posting, which a cursor finds by walking the
//! stream once when first asked. `LSG2` writers stored the full table for
//! such streams (form bit 2); both are still read.

use crate::reader::Reader;
use crate::segment::Format;
use crate::set::Cursor;
use crate::tf_bucket::BUCKET_COUNT;
use crate::tid::MAX_OFFSET;
use crate::{Error, Result, Tid, varint};

pub const GROUP_BLOCKS: u32 = 256;
/// Postings per score-bound block.
pub const BLOCK_POSTINGS: u32 = 128;
const PAGE_BITMAP_BYTES: usize = 32;
/// 296 bits, enough for `MAX_OFFSET` (291) one-based offsets.
const TUPLE_BITMAP_BYTES: usize = 37;
/// A list of this many `u16` offsets is no larger than a tuple bitmap.
pub const LIST_MAX: usize = 18;
const FORM_SPARSE: u8 = 0;
const FORM_GROUPED: u8 = 1;
/// Set on a form byte when a bounds table precedes the body.
const FORM_BOUNDED: u8 = 2;
/// Set on a form byte when a single term bound precedes the body; the stream
/// then holds at most `BLOCK_POSTINGS` postings.
const FORM_TERM_BOUND: u8 = 4;
/// The form bits that describe bounds rather than the body layout.
const FORM_FLAGS: u8 = FORM_BOUNDED | FORM_TERM_BOUND;
const TAG_LIST: u8 = 0;
const TAG_BITMAP: u8 = 1;

/// Score bounds over one block of postings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockBound {
    /// Per term-frequency bucket, the shortest document in the block with
    /// that bucket; `u32::MAX` where the bucket does not occur.
    pub min_len: [u32; BUCKET_COUNT],
    /// The block's last posting; every posting of the block is at or before it.
    pub last: Tid,
}

impl BlockBound {
    /// The buckets that occur in the block with their shortest document.
    pub fn buckets(&self) -> impl Iterator<Item = (u8, u32)> + '_ {
        self.min_len
            .iter()
            .enumerate()
            .filter(|(_, len)| **len != u32::MAX)
            .map(|(bucket, len)| (bucket as u8, *len))
    }

    /// Largest bucket in the block.
    pub fn max_tf_bucket(&self) -> u8 {
        self.buckets().map(|(bucket, _)| bucket).max().unwrap_or(0)
    }

    /// Shortest document in the block.
    pub fn shortest(&self) -> u32 {
        self.min_len.iter().copied().min().unwrap_or(u32::MAX)
    }

    /// The tighter of two bounds' minima per bucket, covering both blocks.
    pub fn merge(&self, other: &Self) -> Self {
        let mut min_len = self.min_len;
        for (mine, theirs) in min_len.iter_mut().zip(&other.min_len) {
            *mine = (*mine).min(*theirs);
        }
        Self {
            min_len,
            last: self.last.max(other.last),
        }
    }

    /// The bound over postings given as (bucket, document length), ending at
    /// `last`. A length of `u32::MAX` is recorded one shorter, which only
    /// loosens the bound, so the value can mark absent buckets.
    pub fn over(postings: &[(u8, u32)], last: Tid) -> Self {
        let mut min_len = [u32::MAX; BUCKET_COUNT];
        for (bucket, len) in postings {
            let slot = &mut min_len[usize::from(*bucket)];
            *slot = (*slot).min((*len).min(u32::MAX - 1));
        }
        Self { min_len, last }
    }
}

/// Accumulates strictly increasing tuple locations for one term.
#[derive(Default, Debug)]
pub struct PostingsBuilder {
    tids: Vec<Tid>,
    /// Term-frequency bucket and document length per posting, when every
    /// posting was pushed with [`PostingsBuilder::push_scored`].
    scores: Vec<(u8, u32)>,
}

impl PostingsBuilder {
    pub fn push(&mut self, tid: Tid) -> Result<()> {
        Tid::new(tid.block, tid.offset)?;
        if self.tids.last().is_some_and(|last| *last >= tid) {
            return Err(Error::Unordered);
        }
        self.tids.push(tid);
        Ok(())
    }

    /// Pushes a posting with the inputs of its score, so the stream carries
    /// block bounds. A stream mixing `push` and `push_scored` carries none.
    pub fn push_scored(&mut self, tid: Tid, tf_bucket: u8, doc_len: u32) -> Result<()> {
        if tf_bucket > crate::payload::MAX_TF_BUCKET {
            return Err(Error::InvalidTfBucket);
        }
        self.push(tid)?;
        self.scores.push((tf_bucket, doc_len));
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.tids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tids.is_empty()
    }

    /// Encodes with whichever form is smaller for this list.
    pub fn finish(self) -> Vec<u8> {
        self.finish_as(Format::CURRENT)
    }

    /// Encodes in the layout of an earlier format, for compatibility tests:
    /// `LSG1` streams carry no bounds and `LSG2` streams a table however few
    /// blocks they have.
    pub(crate) fn finish_as(self, format: Format) -> Vec<u8> {
        let scores =
            (format.has_bounds() && !self.tids.is_empty() && self.scores.len() == self.tids.len())
                .then_some(self.scores.as_slice());
        let sparse = encode_sparse(&self.tids, scores, format);
        let grouped = encode_grouped(&self.tids, scores, format);
        if sparse.len() <= grouped.len() {
            sparse
        } else {
            grouped
        }
    }
}

/// Encodes one bound: the buckets that occur and the shortest document per
/// bucket.
fn encode_term_bound(out: &mut Vec<u8>, bound: &BlockBound) {
    let buckets = bound
        .buckets()
        .fold(0u64, |mask, (bucket, _)| mask | 1 << bucket);
    varint::put(out, buckets);
    for (_, len) in bound.buckets() {
        varint::put(out, u64::from(len));
    }
}

/// Encodes the bounds table: per block, the bound over the score inputs, the
/// last location and, for sparse streams, where the block starts.
fn encode_bounds(tids: &[Tid], scores: &[(u8, u32)], starts: Option<&[usize]>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut previous_block = 0u32;
    let mut previous_start = 0usize;
    let blocks = tids
        .chunks(BLOCK_POSTINGS as usize)
        .zip(scores.chunks(BLOCK_POSTINGS as usize));
    for (index, (block, block_scores)) in blocks.enumerate() {
        let last = block[block.len() - 1];
        encode_term_bound(&mut out, &BlockBound::over(block_scores, last));
        varint::put(&mut out, u64::from(last.block - previous_block));
        varint::put(&mut out, u64::from(last.offset));
        previous_block = last.block;
        if let Some(starts) = starts {
            varint::put(&mut out, (starts[index] - previous_start) as u64);
            previous_start = starts[index];
        }
    }
    out
}

/// The bounds a scored stream carries: a single term bound when it is one
/// block, else the table. Returns the form bit and the encoded bytes.
fn encode_scored(
    tids: &[Tid],
    scores: &[(u8, u32)],
    starts: Option<&[usize]>,
    format: Format,
) -> (u8, Vec<u8>) {
    if format >= Format::Lsg3 && tids.len() <= BLOCK_POSTINGS as usize {
        let mut out = Vec::new();
        encode_term_bound(&mut out, &BlockBound::over(scores, tids[tids.len() - 1]));
        (FORM_TERM_BOUND, out)
    } else {
        (FORM_BOUNDED, encode_bounds(tids, scores, starts))
    }
}

fn encode_header(form: u8, count: usize, bounds: Option<&(u8, Vec<u8>)>) -> Vec<u8> {
    let mut out = vec![bounds.map_or(form, |(bit, _)| form | bit)];
    varint::put(&mut out, count as u64);
    if let Some((bit, bounds)) = bounds {
        if *bit == FORM_BOUNDED {
            varint::put(&mut out, bounds.len() as u64);
        }
        out.extend_from_slice(bounds);
    }
    out
}

fn encode_sparse(tids: &[Tid], scores: Option<&[(u8, u32)]>, format: Format) -> Vec<u8> {
    let mut body = Vec::new();
    let mut starts = Vec::new();
    let mut last_block = 0u32;
    for (ordinal, tid) in tids.iter().enumerate() {
        if (ordinal as u32).is_multiple_of(BLOCK_POSTINGS) {
            starts.push(body.len());
        }
        varint::put(&mut body, u64::from(tid.block - last_block));
        varint::put(&mut body, u64::from(tid.offset));
        last_block = tid.block;
    }
    let bounds = scores.map(|scores| encode_scored(tids, scores, Some(&starts), format));
    let mut out = encode_header(FORM_SPARSE, tids.len(), bounds.as_ref());
    out.extend_from_slice(&body);
    out
}

fn encode_grouped(tids: &[Tid], scores: Option<&[(u8, u32)]>, format: Format) -> Vec<u8> {
    let bounds = scores.map(|scores| encode_scored(tids, scores, None, format));
    let mut out = encode_header(FORM_GROUPED, tids.len(), bounds.as_ref());
    let groups = tids.chunk_by(|a, b| a.group() == b.group());
    varint::put(&mut out, groups.clone().count() as u64);
    let mut previous_gid: Option<u32> = None;
    for group in groups {
        let gid = group[0].group();
        match previous_gid {
            None => varint::put(&mut out, u64::from(gid)),
            Some(previous) => varint::put(&mut out, u64::from(gid - previous - 1)),
        }
        previous_gid = Some(gid);
        varint::put(&mut out, group.len() as u64);
        let mut page_bitmap = [0u8; PAGE_BITMAP_BYTES];
        let mut body = Vec::new();
        for page in group.chunk_by(|a, b| a.block == b.block) {
            let bit = page[0].page_bit();
            page_bitmap[usize::from(bit / 8)] |= 1 << (bit % 8);
            if page.len() <= LIST_MAX {
                body.push(TAG_LIST);
                varint::put(&mut body, page.len() as u64);
                for tid in page {
                    body.extend_from_slice(&tid.offset.to_le_bytes());
                }
            } else {
                body.push(TAG_BITMAP);
                let mut tuple_bitmap = [0u8; TUPLE_BITMAP_BYTES];
                for tid in page {
                    let bit = usize::from(tid.offset - 1);
                    tuple_bitmap[bit / 8] |= 1 << (bit % 8);
                }
                body.extend_from_slice(&tuple_bitmap);
            }
        }
        out.extend_from_slice(&page_bitmap);
        varint::put(&mut out, body.len() as u64);
        out.extend_from_slice(&body);
    }
    out
}

/// Where a stream's bytes come from. A paged source hands out windows, so a
/// cursor reads the header and bounds table once and then only the bytes of
/// the blocks and groups it visits: a frequent term's postings run to
/// megabytes per segment, and a pruned ranked walk skips most of them.
#[derive(Clone, Copy)]
enum Bytes<'a> {
    Whole(&'a [u8]),
    Ranged {
        areas: &'a dyn crate::segment::AreaFetch,
        /// The stream's offset in the postings area.
        base: u64,
        len: usize,
    },
}

impl std::fmt::Debug for Bytes<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Whole(bytes) => write!(f, "Whole({} bytes)", bytes.len()),
            Self::Ranged { base, len, .. } => write!(f, "Ranged({base}, {len} bytes)"),
        }
    }
}

impl<'a> Bytes<'a> {
    const fn len(&self) -> usize {
        match self {
            Self::Whole(bytes) => bytes.len(),
            Self::Ranged { len, .. } => *len,
        }
    }

    /// `len` bytes from `at`, which must lie within the stream.
    fn range(&self, at: usize, len: usize) -> Result<&'a [u8]> {
        let end = at.checked_add(len).ok_or(Error::Truncated)?;
        match self {
            Self::Whole(bytes) => bytes.get(at..end).ok_or(Error::Truncated),
            Self::Ranged {
                areas,
                base,
                len: total,
            } => {
                if end > *total {
                    return Err(Error::Truncated);
                }
                areas.postings_range(base.checked_add(at as u64).ok_or(Error::Truncated)?, len)
            }
        }
    }
}

/// Bytes of a ranged stream fetched to parse its header: the form, count,
/// and either the table length or a term bound of every bucket.
const PROBE: usize = 16 + BUCKET_COUNT * 5;

/// Bytes a ranged stream fetches at a time, aligned within the stream so
/// that every cursor over the same bytes, in this query or a later one,
/// asks its source for the same windows and finds them cached.
const WINDOW: usize = 16 * 1024;

/// Bounds-checked sequential reads over a stream, fetched a window at a
/// time; the whole of an in-memory stream is one window. The API of
/// [`Reader`], over either source.
#[derive(Clone, Copy)]
struct Stream<'a> {
    source: Bytes<'a>,
    /// The stream's length.
    end: usize,
    at: usize,
    window: &'a [u8],
    window_at: usize,
    window_end: usize,
}

impl std::fmt::Debug for Stream<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Stream({:?} at {})", self.source, self.at)
    }
}

impl<'a> Stream<'a> {
    const fn at(source: Bytes<'a>, at: usize) -> Self {
        Self {
            source,
            end: source.len(),
            at,
            window: &[],
            window_at: 0,
            window_end: 0,
        }
    }

    const fn position(&self) -> usize {
        self.at
    }

    const fn remaining(&self) -> usize {
        self.end - self.at
    }

    /// The next `len` bytes, without consuming them.
    #[inline]
    fn need(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(len).ok_or(Error::Truncated)?;
        if end > self.end {
            return Err(Error::Truncated);
        }
        if self.at < self.window_at || end > self.window_end {
            self.fetch(len, end)?;
        }
        let from = self.at - self.window_at;
        Ok(&self.window[from..from + len])
    }

    /// Fetches a window holding the `len` bytes at the position.
    #[cold]
    fn fetch(&mut self, len: usize, end: usize) -> Result<()> {
        {
            let (start, span) = match self.source {
                Bytes::Whole(_) => (0, self.source.len()),
                Bytes::Ranged { .. } => {
                    let start = self.at - self.at % WINDOW;
                    let span = WINDOW.min(self.source.len() - start);
                    // An item across a window boundary is fetched on its own.
                    if end > start + span {
                        (self.at, len)
                    } else {
                        (start, span)
                    }
                }
            };
            self.window = self.source.range(start, span)?;
            self.window_at = start;
            self.window_end = start + span;
        }
        Ok(())
    }

    #[inline]
    fn u8(&mut self) -> Result<u8> {
        let byte = self.need(1)?[0];
        self.at += 1;
        Ok(byte)
    }

    #[inline]
    fn varint(&mut self) -> Result<u64> {
        // Decoded in place while the window holds a whole value's bytes, so
        // the hot loops over a stream cost what they did over one slice.
        let from = self.at.wrapping_sub(self.window_at);
        if self.at >= self.window_at && from + varint::MAX_LEN <= self.window.len() {
            let mut at = from;
            let value = varint::get(self.window, &mut at)?;
            self.at += at - from;
            return Ok(value);
        }
        let bytes = self.need(varint::MAX_LEN.min(self.remaining()))?;
        let mut at = 0;
        let value = varint::get(bytes, &mut at)?;
        self.at += at;
        Ok(value)
    }

    #[inline]
    fn varint_u32(&mut self) -> Result<u32> {
        u32::try_from(self.varint()?).map_err(|_| Error::Corrupt("value exceeds 32 bits"))
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let slice = self.need(len)?;
        self.at += len;
        Ok(slice)
    }

    fn skip(&mut self, len: usize) -> Result<()> {
        // Skipped bytes need not be fetched.
        let end = self.at.checked_add(len).ok_or(Error::Truncated)?;
        if end > self.end {
            return Err(Error::Truncated);
        }
        self.at = end;
        Ok(())
    }

    /// Moves to an absolute position that must not exceed the end of input.
    fn seek(&mut self, at: usize) -> Result<()> {
        if at > self.end {
            return Err(Error::Truncated);
        }
        self.at = at;
        Ok(())
    }
}

/// The form, count, bounds table location and body offset of a stream.
type Head = (u8, u32, Option<(usize, usize)>, usize);

/// A parsed stream header. Parsing reads only the header; bodies are validated
/// as cursors traverse them.
#[derive(Clone, Copy, Debug)]
pub struct Postings<'a> {
    source: Bytes<'a>,
    /// The stream up to its body: the header and any bounds table.
    head: &'a [u8],
    form: u8,
    count: u32,
    /// Where the bounds table is, when the stream carries one.
    bounds: Option<(usize, usize)>,
    body_at: usize,
}

impl<'a> Postings<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let (form, count, bounds, body_at) = Self::parse_head(bytes, bytes.len())?;
        Ok(Self {
            source: Bytes::Whole(bytes),
            head: &bytes[..body_at],
            form,
            count,
            bounds,
            body_at,
        })
    }

    /// A stream of `len` bytes at `base` in a source's postings area, read a
    /// window at a time. Only the header and bounds table are fetched here.
    pub(crate) fn open(
        areas: &'a dyn crate::segment::AreaFetch,
        base: u64,
        len: usize,
    ) -> Result<Self> {
        let source = Bytes::Ranged { areas, base, len };
        // The form, count and table length or term bound: a bounded prefix.
        let probe = source.range(0, PROBE.min(len))?;
        let (form, count, bounds, body_at) = Self::parse_head(probe, len)?;
        Ok(Self {
            source,
            head: source.range(0, body_at)?,
            form,
            count,
            bounds,
            body_at,
        })
    }

    /// The header fields from the stream's first bytes, of which `bytes`
    /// holds at least the header and term bound; `len` is the whole stream.
    fn parse_head(bytes: &[u8], len: usize) -> Result<Head> {
        let mut reader = Reader::new(bytes);
        let form = reader.u8()?;
        let layout = form & !FORM_FLAGS;
        if (layout != FORM_SPARSE && layout != FORM_GROUPED)
            || (form & FORM_BOUNDED != 0 && form & FORM_TERM_BOUND != 0)
        {
            return Err(Error::Corrupt("unknown postings form"));
        }
        let count = reader.varint_u32()?;
        let bounds = if form & FORM_BOUNDED != 0 {
            let table = reader.varint_u32()? as usize;
            let at = reader.position();
            if at.checked_add(table).is_none_or(|end| end > len) {
                return Err(Error::Truncated);
            }
            Some((at, table))
        } else if form & FORM_TERM_BOUND != 0 {
            if count == 0 || count > BLOCK_POSTINGS {
                return Err(Error::Corrupt("term bound on a stream of several blocks"));
            }
            let at = reader.position();
            decode_term_bound(&mut reader)?;
            Some((at, reader.position() - at))
        } else {
            None
        };
        let body_at = match bounds {
            Some((at, table)) => at + table,
            None => reader.position(),
        };
        Ok((form, count, bounds, body_at))
    }

    /// Number of postings, from the header.
    pub const fn count(&self) -> u32 {
        self.count
    }

    pub const fn is_grouped(&self) -> bool {
        self.form & !FORM_FLAGS == FORM_GROUPED
    }

    /// True when the stream carries per-block score bounds.
    pub const fn has_bounds(&self) -> bool {
        self.bounds.is_some()
    }

    /// True when the bounds are a single term bound rather than a table.
    pub const fn has_term_bound(&self) -> bool {
        self.form & FORM_TERM_BOUND != 0
    }

    /// Bytes of the bounds table, for size accounting.
    pub const fn bounds_len(&self) -> usize {
        match self.bounds {
            Some((_, len)) => len,
            None => 0,
        }
    }

    /// Bytes of the body after the header and bounds table.
    pub const fn body_len(&self) -> usize {
        self.source.len() - self.body_at
    }

    fn bounds_table(&self) -> Option<Bounds<'a>> {
        self.bounds.map(|(at, len)| Bounds {
            reader: Reader::new(&self.head[at..at + len]),
            blocks: self.count.div_ceil(BLOCK_POSTINGS),
            starts: if self.is_grouped() || self.has_term_bound() {
                None
            } else {
                Some(Vec::new())
            },
            body_len: self.source.len() - self.body_at,
            entries: Vec::new(),
            stream: *self,
            compact: self.has_term_bound(),
            term_last: None,
        })
    }

    fn grouped(&self) -> Result<GroupedCursor<'a>> {
        let mut reader = Stream::at(self.source, self.body_at);
        let groups_left = reader.varint_u32()?;
        Ok(GroupedCursor {
            reader,
            total: self.count,
            groups_left,
            previous_gid: None,
            gid: 0,
            group_count: 0,
            page_bitmap: [0; PAGE_BITMAP_BYTES],
            body_end: 0,
            next_bit: 0,
            group_consumed: 0,
            block: 0,
            offsets: Vec::new(),
            index: 0,
            current: None,
            ordinal: 0,
            seen: 0,
            bounds: None,
        })
    }

    /// Whether masks amortize their fixed five-word cost. Grouped encoding
    /// alone is not enough: it can also win for many sparsely occupied pages.
    /// Read group headers only, skipping offset bodies. Four tuples per page
    /// is a conservative crossover for the bulk count path.
    pub fn prefers_pages(&self) -> Result<bool> {
        if !self.is_grouped() {
            return Ok(false);
        }
        let mut groups = self.grouped()?;
        if u64::from(self.count) >= u64::from(groups.groups_left) * u64::from(GROUP_BLOCKS) * 4 {
            return Ok(self.count != 0);
        }
        let mut pages = 0u64;
        while groups.enter_group()? {
            pages += groups
                .page_bitmap
                .iter()
                .map(|b| u64::from(b.count_ones()))
                .sum::<u64>();
            groups.reader.seek(groups.body_end)?;
        }
        Ok(pages != 0 && u64::from(self.count) >= pages * 4)
    }

    /// Reads dense pages as bitmaps without expanding them into tuple IDs.
    pub fn pages(&self) -> Result<Box<dyn crate::pages::Cursor + 'a>> {
        if self.is_grouped() {
            let mut pages = GroupedPages {
                inner: self.grouped()?,
                current: None,
            };
            crate::pages::Cursor::advance(&mut pages)?;
            Ok(Box::new(pages))
        } else {
            Ok(Box::new(crate::pages::Rows::new(self.cursor_with(false)?)?))
        }
    }

    pub fn cursor(&self) -> Result<PostingsCursor<'a>> {
        self.cursor_with(true)
    }

    /// A cursor, with or without its bounds attached.
    fn cursor_with(&self, with_bounds: bool) -> Result<PostingsCursor<'a>> {
        let bounds = if with_bounds {
            self.bounds_table()
        } else {
            None
        };
        let mut cursor = if self.is_grouped() {
            let mut grouped = self.grouped()?;
            grouped.bounds = bounds;
            PostingsCursor::Grouped(grouped)
        } else {
            PostingsCursor::Sparse(SparseCursor {
                reader: Stream::at(self.source, self.body_at),
                body_at: self.body_at,
                total: self.count,
                remaining: self.count,
                last_block: 0,
                current: None,
                ordinal: 0,
                bounds,
            })
        };
        cursor.start()?;
        Ok(cursor)
    }

    /// Decodes every posting. Fails on the first malformed byte.
    pub fn to_vec(&self) -> Result<Vec<Tid>> {
        let mut cursor = self.cursor()?;
        // The count is untrusted until the stream has been decoded.
        // Bound speculative allocation by bytes actually present.
        let mut out = Vec::with_capacity((self.count as usize).min(self.source.len()));
        while let Some(tid) = cursor.current() {
            out.push(tid);
            cursor.advance()?;
        }
        if out.len() != self.count as usize {
            return Err(Error::Corrupt("posting count mismatch"));
        }
        Ok(out)
    }
}

/// Positioned at one posting (or exhausted), with that posting's ordinal.
#[derive(Clone, Debug)]
pub enum PostingsCursor<'a> {
    Sparse(SparseCursor<'a>),
    Grouped(GroupedCursor<'a>),
}

impl<'a> PostingsCursor<'a> {
    fn start(&mut self) -> Result<()> {
        match self {
            Self::Sparse(cursor) => cursor.load_next(),
            Self::Grouped(cursor) => cursor.open_page_at_or_after(0),
        }
    }

    /// Zero-based index of the current posting within its stream.
    pub fn ordinal(&self) -> u32 {
        match self {
            Self::Sparse(cursor) => cursor.ordinal,
            Self::Grouped(cursor) => cursor.ordinal,
        }
    }

    /// Ordinal of `tid` if present. Leaves the cursor at or after `tid`.
    pub fn rank(&mut self, tid: Tid) -> Result<Option<u32>> {
        self.seek(tid)?;
        Ok((self.current() == Some(tid)).then(|| self.ordinal()))
    }

    fn bounds_mut(&mut self) -> Option<&mut Bounds<'a>> {
        match self {
            Self::Sparse(cursor) => cursor.bounds.as_mut(),
            Self::Grouped(cursor) => cursor.bounds.as_mut(),
        }
    }

    /// True when the stream carries per-block score bounds.
    pub fn has_bounds(&self) -> bool {
        match self {
            Self::Sparse(cursor) => cursor.bounds.is_some(),
            Self::Grouped(cursor) => cursor.bounds.is_some(),
        }
    }

    /// Bounds of the block holding the first posting at or after `target`
    /// that the cursor has not passed: the current block when `target` is at
    /// or before the current posting. `None` when no such posting exists or
    /// the stream carries no bounds. The cursor does not move.
    pub fn bound_at(&mut self, target: Tid) -> Result<Option<BlockBound>> {
        let Some(current) = self.current() else {
            return Ok(None);
        };
        let target = target.max(current);
        let mut block = self.ordinal() / BLOCK_POSTINGS;
        self.resolve_term_last()?;
        let Some(bounds) = self.bounds_mut() else {
            return Ok(None);
        };
        loop {
            let Some(entry) = bounds.entry(block)? else {
                return Ok(None);
            };
            if entry.last >= target {
                return Ok(Some(entry));
            }
            block += 1;
        }
    }

    /// Every block's bounds, in order; empty when the stream carries none.
    pub fn block_bounds(&mut self) -> Result<Vec<BlockBound>> {
        let mut all = Vec::new();
        self.block_bounds_into(&mut all)?;
        Ok(all)
    }

    /// Reuse caller scratch while checking and decoding every block bound.
    /// Scratch is cleared first; on error it can contain a decoded prefix.
    pub(crate) fn block_bounds_into(&mut self, all: &mut Vec<BlockBound>) -> Result<()> {
        all.clear();
        self.resolve_term_last()?;
        let Some(bounds) = self.bounds_mut() else {
            return Ok(());
        };
        // A corrupt count can imply millions of bounds in a tiny stream.
        for block in 0..bounds.blocks {
            all.push(bounds.entry(block)?.expect("block index is in range"));
        }
        Ok(())
    }

    /// A term bound stores no last location: find the stream's last posting
    /// by walking a fresh cursor once, so the bound covers exactly the
    /// stream. The walk is short, since such a stream is one block.
    fn resolve_term_last(&mut self) -> Result<()> {
        let stream = match self.bounds_mut() {
            Some(bounds) if bounds.compact && bounds.term_last.is_none() => bounds.stream,
            _ => return Ok(()),
        };
        let mut probe = stream.cursor_with(false)?;
        let mut last = None;
        while let Some(current) = probe.current() {
            last = Some(current);
            probe.advance()?;
        }
        let last = last.ok_or(Error::Corrupt("term bound on an empty stream"))?;
        self.bounds_mut().expect("checked above").term_last = Some(last);
        Ok(())
    }
}

impl Cursor for PostingsCursor<'_> {
    fn current(&self) -> Option<Tid> {
        match self {
            Self::Sparse(cursor) => cursor.current,
            Self::Grouped(cursor) => cursor.current,
        }
    }

    fn advance(&mut self) -> Result<()> {
        match self {
            Self::Sparse(cursor) => {
                if cursor.current.is_some() {
                    cursor.ordinal += 1;
                    cursor.load_next()?;
                }
                Ok(())
            }
            Self::Grouped(cursor) => cursor.advance(),
        }
    }

    fn seek(&mut self, target: Tid) -> Result<()> {
        match self {
            Self::Sparse(cursor) => cursor.seek(target),
            Self::Grouped(cursor) => cursor.seek(target),
        }
    }
}

/// Decodes a term bound: the bucket mask and the shortest document per
/// bucket that occurs.
fn decode_term_bound(reader: &mut Reader<'_>) -> Result<[u32; BUCKET_COUNT]> {
    let buckets = reader.varint_u32()?;
    if buckets == 0 || buckets >> BUCKET_COUNT != 0 {
        return Err(Error::Corrupt("block bound buckets"));
    }
    let mut min_len = [u32::MAX; BUCKET_COUNT];
    for (bucket, len) in min_len.iter_mut().enumerate() {
        if buckets & (1 << bucket) != 0 {
            *len = reader.varint_u32()?;
            if *len == u32::MAX {
                return Err(Error::Corrupt("block bound length"));
            }
        }
    }
    Ok(min_len)
}

/// The bounds table, decoded one entry at a time as the cursor moves.
#[derive(Clone, Debug)]
struct Bounds<'a> {
    /// Positioned at the next undecoded entry.
    reader: Reader<'a>,
    blocks: u32,
    /// Sparse streams with a table only: byte offset of each block's first
    /// posting.
    starts: Option<Vec<usize>>,
    body_len: usize,
    entries: Vec<BlockBound>,
    /// The stream, so a term bound's last location can be found.
    stream: Postings<'a>,
    /// True for a term bound: one entry, without a stored last location.
    compact: bool,
    /// A term bound's last location once found (see
    /// [`PostingsCursor::resolve_term_last`]).
    term_last: Option<Tid>,
}

impl Bounds<'_> {
    /// Decodes up to and including entry `block`; `None` when out of range.
    fn entry(&mut self, block: u32) -> Result<Option<BlockBound>> {
        if block >= self.blocks {
            return Ok(None);
        }
        while self.entries.len() <= block as usize {
            self.decode()?;
        }
        Ok(Some(self.entries[block as usize]))
    }

    fn decode(&mut self) -> Result<()> {
        let min_len = decode_term_bound(&mut self.reader)?;
        let previous = self.entries.last().copied();
        let last = if self.compact {
            self.term_last
                .expect("a term bound's last location is resolved before decoding")
        } else {
            let block = previous
                .map_or(0, |entry| entry.last.block)
                .checked_add(self.reader.varint_u32()?)
                .ok_or(Error::Corrupt("block overflow"))?;
            let offset = u16::try_from(self.reader.varint_u32()?).map_err(|_| Error::InvalidTid)?;
            let last = Tid::new(block, offset)?;
            if previous.is_some_and(|entry| entry.last >= last) {
                return Err(Error::Corrupt("block bounds not increasing"));
            }
            if let Some(starts) = self.starts.as_mut() {
                let previous_start = starts.last().copied();
                let start = previous_start
                    .unwrap_or(0)
                    .checked_add(self.reader.varint()? as usize)
                    .filter(|start| *start < self.body_len)
                    .ok_or(Error::Corrupt("block start beyond body"))?;
                if previous_start.is_some_and(|previous| previous >= start) {
                    return Err(Error::Corrupt("block starts not increasing"));
                }
                starts.push(start);
            }
            last
        };
        self.entries.push(BlockBound { min_len, last });
        if self.entries.len() == self.blocks as usize && self.reader.remaining() != 0 {
            return Err(Error::Corrupt("block bounds length"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SparseCursor<'a> {
    reader: Stream<'a>,
    body_at: usize,
    total: u32,
    remaining: u32,
    last_block: u32,
    current: Option<Tid>,
    ordinal: u32,
    bounds: Option<Bounds<'a>>,
}

impl SparseCursor<'_> {
    fn seek(&mut self, target: Tid) -> Result<()> {
        if self.bounds.as_ref().is_some_and(|b| b.starts.is_some()) {
            self.skip_blocks_before(target)?;
        }
        while self.current.is_some_and(|current| current < target) {
            self.ordinal += 1;
            self.load_next()?;
        }
        Ok(())
    }

    /// Jumps over whole blocks whose last posting is before `target`,
    /// through the bounds table alone: the body is read again only at the
    /// first block that can hold the target.
    fn skip_blocks_before(&mut self, target: Tid) -> Result<()> {
        if self.current.is_none() {
            return Ok(());
        }
        let bounds = self.bounds.as_mut().expect("bounded stream");
        let from = self.ordinal / BLOCK_POSTINGS;
        let mut block = from;
        let mut entry = bounds
            .entry(block)?
            .ok_or(Error::Corrupt("posting beyond block bounds"))?;
        while entry.last < target {
            match bounds.entry(block + 1)? {
                Some(next) => {
                    block += 1;
                    entry = next;
                }
                None => {
                    self.current = None;
                    self.remaining = 0;
                    self.ordinal = self.total;
                    return Ok(());
                }
            }
        }
        if block == from {
            return Ok(());
        }
        let previous = bounds
            .entry(block - 1)?
            .expect("decoded on the way to the target block");
        let start = bounds.starts.as_ref().expect("sparse bounds track starts")[block as usize];
        self.reader.seek(self.body_at + start)?;
        self.last_block = previous.last.block;
        self.ordinal = block * BLOCK_POSTINGS;
        self.remaining = self.total - self.ordinal;
        self.current = Some(previous.last);
        self.load_next()
    }

    fn load_next(&mut self) -> Result<()> {
        if self.remaining == 0 {
            self.current = None;
            return Ok(());
        }
        let delta = self.reader.varint_u32()?;
        let block = self
            .last_block
            .checked_add(delta)
            .ok_or(Error::Corrupt("block overflow"))?;
        let offset = self.reader.varint_u32()?;
        let offset = u16::try_from(offset).map_err(|_| Error::InvalidTid)?;
        let tid = Tid::new(block, offset)?;
        if self.current.is_some_and(|current| current >= tid) {
            return Err(Error::Corrupt("sparse postings not increasing"));
        }
        self.last_block = block;
        self.remaining -= 1;
        self.current = Some(tid);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct GroupedCursor<'a> {
    reader: Stream<'a>,
    total: u32,
    groups_left: u32,
    previous_gid: Option<u32>,
    gid: u32,
    group_count: u32,
    page_bitmap: [u8; PAGE_BITMAP_BYTES],
    body_end: usize,
    /// Next page bit to examine within the current group.
    next_bit: u16,
    /// Postings of this group in pages already passed (decoded or skipped).
    group_consumed: u32,
    block: u32,
    offsets: Vec<u16>,
    index: usize,
    current: Option<Tid>,
    ordinal: u32,
    /// Total postings accounted for across finished groups; checked at the end.
    seen: u32,
    bounds: Option<Bounds<'a>>,
}

struct GroupedPages<'a> {
    inner: GroupedCursor<'a>,
    current: Option<crate::pages::Page>,
}

impl crate::pages::Cursor for GroupedPages<'_> {
    fn current(&self) -> Option<crate::pages::Page> {
        self.current
    }
    fn advance(&mut self) -> Result<()> {
        let c = &mut self.inner;
        self.current = None;
        loop {
            if c.previous_gid.is_some() {
                if let Some(bit) = c.next_set_bit(c.next_bit) {
                    let offsets = c.decode_offsets()?;
                    c.next_bit = bit + 1;
                    c.group_consumed = c
                        .group_consumed
                        .checked_add(offsets.count())
                        .ok_or(Error::Corrupt("posting count overflow"))?;
                    let block = c.gid * GROUP_BLOCKS + u32::from(bit);
                    Tid::new(block, 1)?;
                    self.current = Some(crate::pages::Page { block, offsets });
                    return Ok(());
                }
                c.finish_group()?;
            }
            if !c.enter_group()? {
                if c.seen != c.total {
                    return Err(Error::Corrupt("posting count mismatch"));
                }
                // An exhausted cursor must remain exhausted on further advances.
                c.previous_gid = None;
                return Ok(());
            }
        }
    }
    fn seek(&mut self, block: u32) -> Result<()> {
        if self.current.is_none_or(|page| page.block >= block) {
            return Ok(());
        }
        let c = &mut self.inner;
        let target_gid = block / GROUP_BLOCKS;
        // Skip complete group bodies by their stored length/count.
        while c.gid < target_gid {
            c.reader.seek(c.body_end)?;
            c.group_consumed = c.group_count;
            c.finish_group()?;
            if !c.enter_group()? {
                if c.seen != c.total {
                    return Err(Error::Corrupt("posting count mismatch"));
                }
                c.previous_gid = None;
                self.current = None;
                return Ok(());
            }
        }
        if c.gid == target_gid {
            c.skip_pages_before((block % GROUP_BLOCKS) as u16)?;
        }
        self.advance()
    }
}

impl<'a> GroupedCursor<'a> {
    /// Reads the next group header. Returns false when no groups remain.
    fn enter_group(&mut self) -> Result<bool> {
        if self.groups_left == 0 {
            return Ok(false);
        }
        self.groups_left -= 1;
        let raw = self.reader.varint_u32()?;
        self.gid = match self.previous_gid {
            None => raw,
            Some(previous) => previous
                .checked_add(raw)
                .and_then(|gid| gid.checked_add(1))
                .ok_or(Error::Corrupt("group id overflow"))?,
        };
        if u64::from(self.gid) * u64::from(GROUP_BLOCKS) > u64::from(crate::tid::MAX_BLOCK) {
            return Err(Error::Corrupt("group beyond block range"));
        }
        self.previous_gid = Some(self.gid);
        self.group_count = self.reader.varint_u32()?;
        if self.group_count == 0 {
            return Err(Error::Corrupt("empty group"));
        }
        self.page_bitmap
            .copy_from_slice(self.reader.take(PAGE_BITMAP_BYTES)?);
        let body_len = self.reader.varint()? as usize;
        self.body_end = self
            .reader
            .position()
            .checked_add(body_len)
            .filter(|end| *end <= self.reader.position() + self.reader.remaining())
            .ok_or(Error::Truncated)?;
        self.next_bit = 0;
        self.group_consumed = 0;
        Ok(true)
    }

    fn finish_group(&mut self) -> Result<()> {
        if self.group_consumed != self.group_count {
            return Err(Error::Corrupt("group count mismatch"));
        }
        if self.reader.position() != self.body_end {
            return Err(Error::Corrupt("group body length mismatch"));
        }
        self.seen = self
            .seen
            .checked_add(self.group_count)
            .ok_or(Error::Corrupt("posting count overflow"))?;
        Ok(())
    }

    fn next_set_bit(&self, from: u16) -> Option<u16> {
        (from..GROUP_BLOCKS as u16)
            .find(|bit| self.page_bitmap[usize::from(bit / 8)] & (1 << (bit % 8)) != 0)
    }

    fn page_reader(&self) -> Result<Stream<'a>> {
        if self.reader.position() >= self.body_end {
            return Err(Error::Corrupt("page beyond group body"));
        }
        Ok(self.reader)
    }

    /// Number of postings on the page at the read position, skipping it.
    fn skip_page(&mut self) -> Result<u32> {
        let mut reader = self.page_reader()?;
        let n = match reader.u8()? {
            TAG_LIST => {
                let n = reader.varint_u32()?;
                if n == 0 || n as usize > LIST_MAX {
                    return Err(Error::Corrupt("offset list length"));
                }
                reader.skip(n as usize * 2)?;
                n
            }
            TAG_BITMAP => {
                let bitmap = reader.take(TUPLE_BITMAP_BYTES)?;
                bitmap.iter().map(|byte| byte.count_ones()).sum()
            }
            _ => return Err(Error::Corrupt("unknown page tag")),
        };
        if reader.position() > self.body_end {
            return Err(Error::Corrupt("page beyond group body"));
        }
        self.reader = reader;
        Ok(n)
    }

    fn decode_offsets(&mut self) -> Result<crate::pages::Offsets> {
        let mut reader = self.page_reader()?;
        let mut offsets = crate::pages::Offsets::default();
        match reader.u8()? {
            TAG_LIST => {
                let n = reader.varint_u32()?;
                if n == 0 || n as usize > LIST_MAX {
                    return Err(Error::Corrupt("offset list length"));
                }
                let mut previous = 0;
                for pair in reader.take(n as usize * 2)?.chunks_exact(2) {
                    let offset = u16::from_le_bytes([pair[0], pair[1]]);
                    if offset == 0 || offset > MAX_OFFSET || offset <= previous {
                        return Err(Error::Corrupt("offset list not increasing"));
                    }
                    offsets.insert(offset);
                    previous = offset;
                }
            }
            TAG_BITMAP => {
                offsets = crate::pages::Offsets::from_bitmap(reader.take(TUPLE_BITMAP_BYTES)?)?;
                if offsets.is_empty() {
                    return Err(Error::Corrupt("empty tuple bitmap"));
                }
            }
            _ => return Err(Error::Corrupt("unknown page tag")),
        }
        if reader.position() > self.body_end {
            return Err(Error::Corrupt("page beyond group body"));
        }
        self.reader = reader;
        Ok(offsets)
    }

    fn decode_page(&mut self) -> Result<()> {
        let offsets = self.decode_offsets()?;
        self.offsets.clear();
        self.offsets.extend(offsets.iter());
        Ok(())
    }

    /// Positions on the first posting of the first present page whose bit is at
    /// least `bit` in the current group, moving into later groups as needed.
    fn open_page_at_or_after(&mut self, mut bit: u16) -> Result<()> {
        loop {
            // `previous_gid` is set once any group has been entered.
            if self.previous_gid.is_some() {
                if let Some(found) = self.next_set_bit(bit) {
                    self.decode_page()?;
                    self.block = self.gid * GROUP_BLOCKS + u32::from(found);
                    self.index = 0;
                    self.next_bit = found + 1;
                    self.current = Some(Tid {
                        block: self.block,
                        offset: self.offsets[0],
                    });
                    return Ok(());
                }
                self.finish_group()?;
            }
            if !self.enter_group()? {
                if self.seen != self.total {
                    return Err(Error::Corrupt("posting count mismatch"));
                }
                self.current = None;
                return Ok(());
            }
            bit = 0;
        }
    }

    /// Skips pages whose bit is below `bit`, accounting for their postings.
    fn skip_pages_before(&mut self, bit: u16) -> Result<()> {
        while let Some(found) = self.next_set_bit(self.next_bit) {
            if found >= bit {
                return Ok(());
            }
            let n = self.skip_page()?;
            self.group_consumed += n;
            self.ordinal += n;
            self.next_bit = found + 1;
        }
        Ok(())
    }

    fn advance(&mut self) -> Result<()> {
        if self.current.is_none() {
            return Ok(());
        }
        self.index += 1;
        self.ordinal += 1;
        if self.index < self.offsets.len() {
            self.current = Some(Tid {
                block: self.block,
                offset: self.offsets[self.index],
            });
            return Ok(());
        }
        self.group_consumed += self.offsets.len() as u32;
        self.open_page_at_or_after(self.next_bit)
    }

    fn seek(&mut self, target: Tid) -> Result<()> {
        let Some(current) = self.current else {
            return Ok(());
        };
        if current >= target {
            return Ok(());
        }
        if target.block == self.block {
            let skipped =
                self.offsets[self.index..].partition_point(|offset| *offset < target.offset);
            self.index += skipped;
            self.ordinal += skipped as u32;
            if self.index < self.offsets.len() {
                self.current = Some(Tid {
                    block: self.block,
                    offset: self.offsets[self.index],
                });
                return Ok(());
            }
            self.group_consumed += self.offsets.len() as u32;
            return self.open_page_at_or_after(self.next_bit);
        }
        // Leave the current page entirely.
        let rest = (self.offsets.len() - self.index) as u32;
        self.ordinal += rest;
        self.group_consumed += self.offsets.len() as u32;
        if target.group() > self.gid {
            // Skip the remainder of this group and any whole groups before the target's.
            self.ordinal += self
                .group_count
                .checked_sub(self.group_consumed)
                .ok_or(Error::Corrupt("group count mismatch"))?;
            self.group_consumed = self.group_count;
            self.reader.seek(self.body_end)?;
            loop {
                self.finish_group()?;
                if !self.enter_group()? {
                    if self.seen != self.total {
                        return Err(Error::Corrupt("posting count mismatch"));
                    }
                    self.current = None;
                    return Ok(());
                }
                if self.gid >= target.group() {
                    break;
                }
                self.ordinal += self.group_count;
                self.group_consumed = self.group_count;
                self.reader.seek(self.body_end)?;
            }
            if self.gid > target.group() {
                return self.open_page_at_or_after(0);
            }
        }
        // Same group as the target: skip pages before its block.
        self.skip_pages_before(target.page_bit())?;
        self.open_page_at_or_after(target.page_bit())?;
        if self
            .current
            .is_some_and(|current| current.block == target.block)
        {
            let skipped = self
                .offsets
                .partition_point(|offset| *offset < target.offset);
            self.index = skipped;
            self.ordinal += skipped as u32;
            if self.index < self.offsets.len() {
                self.current = Some(Tid {
                    block: self.block,
                    offset: self.offsets[self.index],
                });
            } else {
                self.group_consumed += self.offsets.len() as u32;
                self.open_page_at_or_after(self.next_bit)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(block: u32, offset: u16) -> Tid {
        Tid::new(block, offset).unwrap()
    }

    fn build(tids: &[Tid]) -> Vec<u8> {
        let mut builder = PostingsBuilder::default();
        for tid in tids {
            builder.push(*tid).unwrap();
        }
        builder.finish()
    }

    /// Bucket and length derived from the location, so tests can predict them.
    fn score_of(tid: Tid) -> (u8, u32) {
        (
            (tid.block % 16) as u8,
            10 + (tid.block * 7 + u32::from(tid.offset)) % 50,
        )
    }

    fn build_scored(tids: &[Tid]) -> Vec<u8> {
        let mut builder = PostingsBuilder::default();
        for tid in tids {
            let (bucket, len) = score_of(*tid);
            builder.push_scored(*tid, bucket, len).unwrap();
        }
        builder.finish()
    }

    fn expected_bounds(tids: &[Tid]) -> Vec<BlockBound> {
        tids.chunks(BLOCK_POSTINGS as usize)
            .map(|block| {
                let scores: Vec<(u8, u32)> = block.iter().map(|t| score_of(*t)).collect();
                BlockBound::over(&scores, block[block.len() - 1])
            })
            .collect()
    }

    #[test]
    fn block_bound_reports_per_bucket_minima() {
        let bound = BlockBound::over(&[(3, 50), (0, 7), (3, 20), (15, 9)], tid(9, 9));
        assert_eq!(
            bound.buckets().collect::<Vec<_>>(),
            [(0, 7), (3, 20), (15, 9)]
        );
        assert_eq!(bound.max_tf_bucket(), 15);
        assert_eq!(bound.shortest(), 7);
        let other = BlockBound::over(&[(3, 10), (1, 3)], tid(4, 1));
        let merged = bound.merge(&other);
        assert_eq!(
            merged.buckets().collect::<Vec<_>>(),
            [(0, 7), (1, 3), (3, 10), (15, 9)]
        );
        assert_eq!(merged.last, tid(9, 9));
    }

    #[test]
    fn empty_list_round_trips() {
        let bytes = build(&[]);
        let postings = Postings::parse(&bytes).unwrap();
        assert_eq!(postings.count(), 0);
        assert!(!postings.has_bounds());
        assert_eq!(postings.to_vec().unwrap(), Vec::<Tid>::new());
        let mut cursor = postings.cursor().unwrap();
        assert_eq!(cursor.current(), None);
        cursor.seek(tid(5, 5)).unwrap();
        assert_eq!(cursor.current(), None);
        assert!(!cursor.has_bounds());
        assert_eq!(cursor.bound_at(tid(1, 1)).unwrap(), None);
        assert!(cursor.block_bounds().unwrap().is_empty());
    }

    #[test]
    fn builder_rejects_out_of_order_and_invalid_tids() {
        let mut builder = PostingsBuilder::default();
        builder.push(tid(3, 3)).unwrap();
        assert_eq!(builder.push(tid(3, 3)), Err(Error::Unordered));
        assert_eq!(builder.push(tid(2, 9)), Err(Error::Unordered));
        assert_eq!(
            builder.push(Tid {
                block: 4,
                offset: 0
            }),
            Err(Error::InvalidTid)
        );
        assert_eq!(
            builder.push_scored(tid(5, 1), 16, 1),
            Err(Error::InvalidTfBucket)
        );
    }

    #[test]
    fn dense_lists_choose_grouped_form_and_sparse_lists_do_not() {
        let rare: Vec<Tid> = (0..5).map(|i| tid(i * 100_000, 1)).collect();
        let dense: Vec<Tid> = (0..2000)
            .map(|i| tid(i / 40, (i % 40 + 1) as u16))
            .collect();
        assert!(!Postings::parse(&build(&rare)).unwrap().is_grouped());
        assert!(Postings::parse(&build(&dense)).unwrap().is_grouped());
        assert_eq!(
            Postings::parse(&build(&rare)).unwrap().to_vec().unwrap(),
            rare
        );
        assert_eq!(
            Postings::parse(&build(&dense)).unwrap().to_vec().unwrap(),
            dense
        );
        // Bounds do not change the choice of layout or the decoded postings.
        assert!(!Postings::parse(&build_scored(&rare)).unwrap().is_grouped());
        assert!(Postings::parse(&build_scored(&dense)).unwrap().is_grouped());
        assert_eq!(
            Postings::parse(&build_scored(&dense))
                .unwrap()
                .to_vec()
                .unwrap(),
            dense
        );
    }

    #[test]
    fn one_block_streams_carry_a_term_bound_in_both_layouts() {
        // Dense enough to be grouped, and a thinned copy that is sparse.
        let dense: Vec<Tid> = (0..BLOCK_POSTINGS)
            .map(|i| tid(i / 40, (i % 40 + 1) as u16))
            .collect();
        let sparse: Vec<Tid> = dense
            .iter()
            .step_by(3)
            .map(|t| tid(t.block * 500, t.offset))
            .collect();
        for (tids, grouped) in [(dense, true), (sparse, false)] {
            let bytes = build_scored(&tids);
            let postings = Postings::parse(&bytes).unwrap();
            assert_eq!(postings.is_grouped(), grouped);
            assert!(postings.has_bounds());
            assert!(postings.has_term_bound());
            assert_eq!(postings.to_vec().unwrap(), tids);
            // The bound is the block bound, with the stream's last posting.
            let expected = expected_bounds(&tids);
            assert_eq!(expected.len(), 1);
            let mut cursor = postings.cursor().unwrap();
            assert_eq!(cursor.block_bounds().unwrap(), expected);
            let mut cursor = postings.cursor().unwrap();
            for target in [tids[0], tids[tids.len() / 2], tids[tids.len() - 1]] {
                assert_eq!(cursor.bound_at(target).unwrap(), Some(expected[0]));
            }
            let last = tids[tids.len() - 1];
            let past = Tid {
                block: last.block,
                offset: last.offset + 1,
            };
            assert_eq!(cursor.bound_at(past).unwrap(), None);
            // A cursor moved past the middle still reports the one block,
            // and one that was exhausted reports nothing.
            cursor.seek(tids[tids.len() / 2]).unwrap();
            assert_eq!(cursor.bound_at(tids[0]).unwrap(), Some(expected[0]));
            assert_eq!(cursor.ordinal() as usize, tids.len() / 2);
            cursor.seek(past).unwrap();
            assert_eq!(cursor.current(), None);
            assert_eq!(cursor.bound_at(tids[0]).unwrap(), None);
            // The term bound is smaller than the table it replaces.
            let with_table = {
                let mut builder = PostingsBuilder::default();
                for tid in &tids {
                    let (bucket, len) = score_of(*tid);
                    builder.push_scored(*tid, bucket, len).unwrap();
                }
                builder.finish_as(Format::Lsg2)
            };
            assert!(bytes.len() < with_table.len());
            let old = Postings::parse(&with_table).unwrap();
            assert!(!old.has_term_bound() && old.has_bounds());
            assert_eq!(old.is_grouped(), grouped);
            assert_eq!(old.cursor().unwrap().block_bounds().unwrap(), expected);
        }
        // One posting more than a block gets the table.
        let spill: Vec<Tid> = (0..=BLOCK_POSTINGS).map(|i| tid(i * 7, 1)).collect();
        let bytes = build_scored(&spill);
        let postings = Postings::parse(&bytes).unwrap();
        assert!(postings.has_bounds() && !postings.has_term_bound());
        assert_eq!(postings.cursor().unwrap().block_bounds().unwrap().len(), 2);
    }

    #[test]
    fn bounds_describe_each_block_in_both_layouts() {
        let sparse: Vec<Tid> = (0..1000).map(|i| tid(i * 37, (i % 3 + 1) as u16)).collect();
        let dense: Vec<Tid> = (0..3000)
            .map(|i| tid(i / 40, (i % 40 + 1) as u16))
            .collect();
        for tids in [sparse, dense] {
            let bytes = build_scored(&tids);
            let postings = Postings::parse(&bytes).unwrap();
            assert!(postings.has_bounds());
            assert_eq!(postings.to_vec().unwrap(), tids);
            let mut cursor = postings.cursor().unwrap();
            assert!(cursor.has_bounds());
            let expected = expected_bounds(&tids);
            assert_eq!(cursor.block_bounds().unwrap(), expected);
            // A fresh cursor decodes entries on demand through `bound_at`.
            let mut cursor = postings.cursor().unwrap();
            for (i, block) in tids.chunks(BLOCK_POSTINGS as usize).enumerate() {
                for target in [block[0], block[block.len() / 2], block[block.len() - 1]] {
                    assert_eq!(cursor.bound_at(target).unwrap(), Some(expected[i]));
                }
                // A location just past a block's last posting resolves to the next block.
                let past = Tid {
                    block: block[block.len() - 1].block,
                    offset: block[block.len() - 1].offset + 1,
                };
                assert_eq!(cursor.bound_at(past).unwrap(), expected.get(i + 1).copied());
            }
            // Targets behind the cursor report the current block.
            cursor.seek(tids[200]).unwrap();
            assert_eq!(cursor.bound_at(tids[0]).unwrap(), Some(expected[1]));
            cursor.seek(tid(u32::MAX - 1, 1)).unwrap();
            assert_eq!(cursor.current(), None);
            assert_eq!(cursor.bound_at(tids[0]).unwrap(), None);
        }
    }

    #[test]
    fn sparse_seek_jumps_blocks_with_correct_ordinals() {
        let tids: Vec<Tid> = (0..1000).map(|i| tid(i * 37, (i % 3 + 1) as u16)).collect();
        let bytes = build_scored(&tids);
        let postings = Postings::parse(&bytes).unwrap();
        assert!(!postings.is_grouped());
        for (expected_ordinal, target) in tids.iter().enumerate().step_by(7) {
            let mut cursor = postings.cursor().unwrap();
            cursor.seek(*target).unwrap();
            assert_eq!(cursor.current(), Some(*target));
            assert_eq!(cursor.ordinal() as usize, expected_ordinal);
            let bumped = Tid {
                block: target.block,
                offset: target.offset + 1,
            };
            cursor.seek(bumped).unwrap();
            assert_eq!(cursor.current(), tids.get(expected_ordinal + 1).copied());
            // Walking on from a jump decodes the rest of the stream intact.
            let mut rest = Vec::new();
            while let Some(current) = cursor.current() {
                rest.push(current);
                cursor.advance().unwrap();
            }
            assert_eq!(rest, tids[expected_ordinal + 1..]);
        }
        let mut cursor = postings.cursor().unwrap();
        cursor.seek(tid(37 * 999 + 1, 1)).unwrap();
        assert_eq!(cursor.current(), None);
        assert_eq!(cursor.ordinal(), 1000);
    }

    #[test]
    fn grouped_seek_skips_groups_and_pages_with_correct_ordinals() {
        // Three groups: 0, 3 and 4; a bitmap page and list pages in each.
        let mut tids = Vec::new();
        for block in [0u32, 7, 255, 768, 770, 1024, 1279] {
            let per_page = if block % 2 == 0 { 40 } else { 3 };
            for offset in 1..=per_page {
                tids.push(tid(block, offset));
            }
        }
        for bytes in [build(&tids), build_scored(&tids)] {
            let postings = Postings::parse(&bytes).unwrap();
            assert!(postings.is_grouped());
            for (expected_ordinal, target) in tids.iter().enumerate() {
                let mut cursor = postings.cursor().unwrap();
                cursor.seek(*target).unwrap();
                assert_eq!(cursor.current(), Some(*target));
                assert_eq!(cursor.ordinal() as usize, expected_ordinal, "{target:?}");
                // Seeking to a location just past the target lands on the successor.
                let mut cursor = postings.cursor().unwrap();
                let bumped = Tid {
                    block: target.block,
                    offset: target.offset + 1,
                };
                cursor.seek(bumped).unwrap();
                let successor = tids.iter().find(|t| **t >= bumped).copied();
                assert_eq!(cursor.current(), successor, "successor of {target:?}");
                if successor.is_some() {
                    assert_eq!(cursor.ordinal() as usize, expected_ordinal + 1);
                }
            }
            // Seeking into an absent group between present ones.
            let mut cursor = postings.cursor().unwrap();
            cursor.seek(tid(300, 1)).unwrap();
            assert_eq!(cursor.current(), Some(tid(768, 1)));
            assert_eq!(cursor.rank(tid(768, 1)).unwrap(), Some(46));
            assert_eq!(cursor.rank(tid(769, 1)).unwrap(), None);
            assert_eq!(cursor.current(), Some(tid(770, 1)));
            cursor.seek(tid(9_999, 1)).unwrap();
            assert_eq!(cursor.current(), None);
        }
    }

    #[test]
    fn corrupt_streams_are_reported_not_trusted() {
        let dense: Vec<Tid> = (0..2000)
            .map(|i| tid(i / 40, (i % 40 + 1) as u16))
            .collect();
        let mut bytes = build(&dense);
        assert!(Postings::parse(&[]).is_err());
        assert!(Postings::parse(&[9]).is_err());
        // Truncation anywhere in the body surfaces as an error, never a short list.
        for cut in [bytes.len() - 1, bytes.len() / 2, 3] {
            let result = Postings::parse(&bytes[..cut]).and_then(|p| p.to_vec());
            assert!(result.is_err(), "cut at {cut}");
        }
        // A tampered page tag: the first page of the first group.
        let mut header = Reader::new(&bytes);
        header.u8().unwrap();
        for _ in 0..4 {
            header.varint().unwrap();
        }
        header.skip(PAGE_BITMAP_BYTES).unwrap();
        header.varint().unwrap();
        let tag_at = header.position();
        assert_eq!(bytes[tag_at], TAG_BITMAP);
        bytes[tag_at] = 7;
        assert!(Postings::parse(&bytes).unwrap().to_vec().is_err());
    }

    #[test]
    fn bounds_scratch_is_replaced_after_scans_and_errors() {
        let mut scratch = Vec::new();
        for count in [500, 1, 50] {
            let tids: Vec<_> = (0..count).map(|i| tid(i * 37, 1)).collect();
            let bytes = build_scored(&tids);
            let mut cursor = Postings::parse(&bytes).unwrap().cursor().unwrap();
            while cursor.current().is_some() {
                cursor.advance().unwrap();
            }
            cursor.block_bounds_into(&mut scratch).unwrap();
            assert_eq!(scratch, expected_bounds(&tids));
        }
        let tids: Vec<_> = (0..500).map(|i| tid(i * 37, 1)).collect();
        let mut corrupted = build_scored(&tids);
        let at = Postings::parse(&corrupted).unwrap().bounds.unwrap().0;
        corrupted[at] = 0;
        let mut cursor = Postings::parse(&corrupted).unwrap().cursor().unwrap();
        assert!(cursor.block_bounds_into(&mut scratch).is_err());
        assert!(scratch.is_empty());
        scratch = expected_bounds(&tids);
        let unbounded = build(&tids);
        Postings::parse(&unbounded)
            .unwrap()
            .cursor()
            .unwrap()
            .block_bounds_into(&mut scratch)
            .unwrap();
        assert!(scratch.is_empty());
    }

    #[test]
    fn corrupt_term_bounds_are_reported_not_trusted() {
        let tids: Vec<Tid> = (0..50).map(|i| tid(i * 37, 1)).collect();
        let bytes = build_scored(&tids);
        let postings = Postings::parse(&bytes).unwrap();
        let (at, len) = postings.bounds.unwrap();
        // An empty bucket set is impossible for a non-empty stream.
        let mut tampered = bytes.clone();
        tampered[at] = 0;
        assert!(Postings::parse(&tampered).is_err());
        // A term bound on a stream of several blocks is not a layout.
        let mut tampered = bytes.clone();
        tampered[1] = 200;
        assert!(Postings::parse(&tampered).is_err());
        // Both bound bits at once are not a layout either.
        let mut tampered = bytes.clone();
        tampered[0] |= FORM_BOUNDED;
        assert!(Postings::parse(&tampered).is_err());
        // A truncated bound fails to parse.
        assert!(Postings::parse(&bytes[..at + len - 1]).is_err());
        // A body that ends early is caught when the last posting is sought.
        let truncated = &bytes[..bytes.len() - 1];
        assert!(
            Postings::parse(truncated)
                .and_then(|p| p.cursor()?.block_bounds())
                .is_err()
        );
    }

    #[test]
    fn corrupt_bounds_are_reported_not_trusted() {
        let tids: Vec<Tid> = (0..500).map(|i| tid(i * 37, 1)).collect();
        let bytes = build_scored(&tids);
        let postings = Postings::parse(&bytes).unwrap();
        let (bounds_at, bounds_len) = postings.bounds.unwrap();
        // An empty bucket set is impossible for a non-empty block.
        let mut tampered = bytes.clone();
        tampered[bounds_at] = 0;
        let parsed = Postings::parse(&tampered).unwrap();
        assert!(parsed.cursor().unwrap().block_bounds().is_err());
        // Bounds that are not increasing: copy the first entry over the second.
        let mut cursor = postings.cursor().unwrap();
        let first = cursor.bound_at(tids[0]).unwrap().unwrap();
        assert_eq!(first.last, tids[127]);
        let mut tampered = bytes.clone();
        let entry_len = bounds_len / 4;
        tampered.copy_within(bounds_at..bounds_at + entry_len, bounds_at + entry_len);
        let parsed = Postings::parse(&tampered).unwrap();
        let mut cursor = parsed.cursor().unwrap();
        assert!(cursor.bound_at(tids[200]).is_err() || cursor.seek(tids[300]).is_err());
        // A truncated table fails to parse or to decode, never yields bounds.
        let truncated = &bytes[..bounds_at + bounds_len - 1];
        assert!(
            Postings::parse(truncated)
                .and_then(|p| p.cursor()?.block_bounds())
                .is_err()
        );
    }
    /// The postings area of a segment held in memory, handed out a range at
    /// a time, counting the fetches.
    struct RangedArea {
        bytes: Vec<u8>,
        fetches: std::cell::Cell<usize>,
        fetched: std::cell::Cell<usize>,
    }

    impl crate::segment::AreaFetch for RangedArea {
        fn postings_bytes(&self, _extent: crate::dictionary::Extent) -> Result<&[u8]> {
            unreachable!("ranged sources are not read whole")
        }
        fn payload_bytes(&self, _extent: crate::dictionary::Extent) -> Result<&[u8]> {
            unreachable!()
        }
        fn ranged_postings(&self) -> bool {
            true
        }
        fn postings_range(&self, offset: u64, len: usize) -> Result<&[u8]> {
            self.fetches.set(self.fetches.get() + 1);
            self.fetched.set(self.fetched.get() + len);
            let at = offset as usize;
            self.bytes.get(at..at + len).ok_or(Error::Truncated)
        }
        fn length(&self, _ordinal: u32) -> Result<u32> {
            unreachable!()
        }
    }

    #[test]
    fn ranged_streams_read_the_same_postings_through_windows() {
        // Sparse and grouped streams several windows long, at an offset.
        for (name, tids) in [
            (
                "sparse",
                (0..60_000u32)
                    .map(|i| Tid::new(i * 7 + 1, 1 + (i % 5) as u16).unwrap())
                    .collect::<Vec<_>>(),
            ),
            (
                "grouped",
                (0..200_000u32)
                    .map(|i| Tid::new(i / 40, 1 + (i % 40) as u16 * 3).unwrap())
                    .collect::<Vec<_>>(),
            ),
        ] {
            let encoded = build_scored(&tids);
            let whole = Postings::parse(&encoded).unwrap();
            assert_eq!(whole.is_grouped(), name == "grouped", "{name}");
            let mut bytes = vec![0xAA; 1_000];
            bytes.extend_from_slice(&encoded);
            bytes.extend_from_slice(&[0x55; 300]);
            let area = RangedArea {
                bytes,
                fetches: Default::default(),
                fetched: Default::default(),
            };
            let ranged = Postings::open(&area, 1_000, encoded.len()).unwrap();
            assert_eq!(ranged.count(), whole.count());
            assert_eq!(ranged.to_vec().unwrap(), tids, "{name}");
            // A seek near the end reads only the head and the tail's windows.
            area.fetches.set(0);
            area.fetched.set(0);
            let mut cursor = ranged.cursor().unwrap();
            let target = tids[tids.len() - 10];
            cursor.seek(target).unwrap();
            assert_eq!(cursor.current(), Some(target), "{name}");
            let mut whole_cursor = whole.cursor().unwrap();
            whole_cursor.seek(target).unwrap();
            assert_eq!(cursor.ordinal(), whole_cursor.ordinal(), "{name}");
            // A sparse stream seeks through its bounds table; a grouped one
            // reads each group header on the way, one window at a time.
            let windows = encoded.len().div_ceil(WINDOW);
            assert!(
                if name == "sparse" {
                    area.fetched.get() < encoded.len() / 2
                } else {
                    area.fetches.get() <= windows + 2
                },
                "{name}: fetched {} of {} bytes in {} fetches",
                area.fetched.get(),
                encoded.len(),
                area.fetches.get()
            );
            assert_eq!(
                ranged.cursor().unwrap().block_bounds().unwrap(),
                whole.cursor().unwrap().block_bounds().unwrap()
            );
        }
    }
}
