//! A term's tuple locations in heap order.
//!
//! Two forms share one stream header; the builder emits whichever is smaller:
//!
//! ```text
//! stream  := form u8, count varint, body
//! sparse  := (block_delta varint, offset varint)*        first block absolute
//! grouped := group_count varint, group*
//! group   := gid varint, count varint, page_bitmap[32], body_len varint, body
//!            gid is absolute for the first group, then (delta - 1)
//! body    := page*  one per set bit of page_bitmap, ascending
//! page    := 0x00, n varint, offset u16le * n     (n <= LIST_MAX)
//!          | 0x01, tuple_bitmap[37]               (bit offset-1 set)
//! ```
//!
//! Groups cover 256 consecutive heap blocks, so intersecting two grouped terms
//! can skip whole groups and whole pages without decoding offsets. Every group
//! and page carries its count, so `seek` maintains the ordinal of the current
//! posting, which is how the parallel payload stream is addressed.

use crate::reader::Reader;
use crate::set::Cursor;
use crate::tid::MAX_OFFSET;
use crate::{Error, Result, Tid, varint};

pub const GROUP_BLOCKS: u32 = 256;
const PAGE_BITMAP_BYTES: usize = 32;
/// 296 bits, enough for `MAX_OFFSET` (291) one-based offsets.
const TUPLE_BITMAP_BYTES: usize = 37;
/// A list of this many `u16` offsets is no larger than a tuple bitmap.
pub const LIST_MAX: usize = 18;
const FORM_SPARSE: u8 = 0;
const FORM_GROUPED: u8 = 1;
const TAG_LIST: u8 = 0;
const TAG_BITMAP: u8 = 1;

/// Accumulates strictly increasing tuple locations for one term.
#[derive(Default, Debug)]
pub struct PostingsBuilder {
    tids: Vec<Tid>,
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

    pub fn len(&self) -> usize {
        self.tids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tids.is_empty()
    }

    /// Encodes with whichever form is smaller for this list.
    pub fn finish(self) -> Vec<u8> {
        let sparse = encode_sparse(&self.tids);
        let grouped = encode_grouped(&self.tids);
        if sparse.len() <= grouped.len() {
            sparse
        } else {
            grouped
        }
    }
}

fn encode_sparse(tids: &[Tid]) -> Vec<u8> {
    let mut out = vec![FORM_SPARSE];
    varint::put(&mut out, tids.len() as u64);
    let mut last_block = 0u32;
    for tid in tids {
        varint::put(&mut out, u64::from(tid.block - last_block));
        varint::put(&mut out, u64::from(tid.offset));
        last_block = tid.block;
    }
    out
}

fn encode_grouped(tids: &[Tid]) -> Vec<u8> {
    let mut out = vec![FORM_GROUPED];
    varint::put(&mut out, tids.len() as u64);
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

/// A parsed stream header. Parsing reads only the header; bodies are validated
/// as cursors traverse them.
#[derive(Clone, Copy, Debug)]
pub struct Postings<'a> {
    bytes: &'a [u8],
    form: u8,
    count: u32,
    body_at: usize,
}

impl<'a> Postings<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        let form = reader.u8()?;
        if form != FORM_SPARSE && form != FORM_GROUPED {
            return Err(Error::Corrupt("unknown postings form"));
        }
        let count = reader.varint_u32()?;
        Ok(Self {
            bytes,
            form,
            count,
            body_at: reader.position(),
        })
    }

    /// Number of postings, from the header.
    pub const fn count(&self) -> u32 {
        self.count
    }

    pub const fn is_grouped(&self) -> bool {
        self.form == FORM_GROUPED
    }

    pub fn cursor(&self) -> Result<PostingsCursor<'a>> {
        let mut cursor = match self.form {
            FORM_SPARSE => PostingsCursor::Sparse(SparseCursor {
                reader: Reader::at(self.bytes, self.body_at),
                remaining: self.count,
                last_block: 0,
                current: None,
                ordinal: 0,
            }),
            _ => {
                let mut reader = Reader::at(self.bytes, self.body_at);
                let groups_left = reader.varint_u32()?;
                PostingsCursor::Grouped(GroupedCursor {
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
                })
            }
        };
        cursor.start()?;
        Ok(cursor)
    }

    /// Decodes every posting. Fails on the first malformed byte.
    pub fn to_vec(&self) -> Result<Vec<Tid>> {
        let mut cursor = self.cursor()?;
        let mut out = Vec::with_capacity(self.count as usize);
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

impl PostingsCursor<'_> {
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
            Self::Sparse(cursor) => {
                while cursor.current.is_some_and(|current| current < target) {
                    cursor.ordinal += 1;
                    cursor.load_next()?;
                }
                Ok(())
            }
            Self::Grouped(cursor) => cursor.seek(target),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SparseCursor<'a> {
    reader: Reader<'a>,
    remaining: u32,
    last_block: u32,
    current: Option<Tid>,
    ordinal: u32,
}

impl SparseCursor<'_> {
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
    reader: Reader<'a>,
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

    fn page_reader(&self) -> Result<Reader<'a>> {
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

    /// Decodes the page at the read position into `offsets`.
    fn decode_page(&mut self) -> Result<()> {
        let mut reader = self.page_reader()?;
        self.offsets.clear();
        match reader.u8()? {
            TAG_LIST => {
                let n = reader.varint_u32()?;
                if n == 0 || n as usize > LIST_MAX {
                    return Err(Error::Corrupt("offset list length"));
                }
                for _ in 0..n {
                    let offset = reader.u16_le()?;
                    if offset == 0
                        || offset > MAX_OFFSET
                        || self.offsets.last().is_some_and(|last| *last >= offset)
                    {
                        return Err(Error::Corrupt("offset list not increasing"));
                    }
                    self.offsets.push(offset);
                }
            }
            TAG_BITMAP => {
                let bitmap = reader.take(TUPLE_BITMAP_BYTES)?;
                for (byte_index, byte) in bitmap.iter().enumerate() {
                    let mut bits = *byte;
                    while bits != 0 {
                        let bit = bits.trailing_zeros() as usize;
                        bits &= bits - 1;
                        let offset = (byte_index * 8 + bit + 1) as u16;
                        if offset > MAX_OFFSET {
                            return Err(Error::Corrupt("tuple bitmap offset"));
                        }
                        self.offsets.push(offset);
                    }
                }
                if self.offsets.is_empty() {
                    return Err(Error::Corrupt("empty tuple bitmap"));
                }
            }
            _ => return Err(Error::Corrupt("unknown page tag")),
        }
        if reader.position() > self.body_end {
            return Err(Error::Corrupt("page beyond group body"));
        }
        self.reader = reader;
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

    #[test]
    fn empty_list_round_trips() {
        let bytes = build(&[]);
        let postings = Postings::parse(&bytes).unwrap();
        assert_eq!(postings.count(), 0);
        assert_eq!(postings.to_vec().unwrap(), Vec::<Tid>::new());
        let mut cursor = postings.cursor().unwrap();
        assert_eq!(cursor.current(), None);
        cursor.seek(tid(5, 5)).unwrap();
        assert_eq!(cursor.current(), None);
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
        let bytes = build(&tids);
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
}
