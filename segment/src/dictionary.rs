// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Sorted, prefix-compressed term dictionary.
//!
//! Terms are ordered by their UTF-8 bytes, which is the order TINQL ranges
//! (`a TO cat`) and prefixes (`brew*`) need. Terms are grouped into blocks of
//! `BLOCK_TERMS`; a block index of first terms supports binary search and each
//! block is decoded sequentially from its first (uncompressed) term. Blocks
//! are fetched one at a time through [`Blocks`], so a lookup over a paged
//! segment reads the index once and then a single block.
//!
//! ```text
//! stream := count varint, block_count varint, index_len varint, index, blocks
//! index  := (block_offset varint, first_len varint, first_bytes)* block_count
//! entry  := shared varint, suffix_len varint, suffix, df_bucket varint,
//!           postings_gap varint, postings_len varint,
//!           payload_gap varint, payload_len varint,
//!           (ordinals_gap varint, ordinals_len varint) from `LSG4` on
//! df_bucket := df << 4 | max_tf_bucket
//! gap    := zigzag(offset - previous_end): the extent's distance from the end
//!           of the previous entry's extent in the same area, 0 at a block start
//! ```
//!
//! A segment builder lays each term's streams out back to back in dictionary
//! order, so a gap is normally zero: one byte in place of an absolute offset
//! into an area of many megabytes. `LSG1` and `LSG2` dictionaries hold, in
//! place of `df_bucket` and the gaps, `df varint, max_tf_bucket u8` and the
//! absolute offsets; [`Dictionary::with_format`] reads them.

use crate::reader::Reader;
use crate::segment::Format;
use crate::{Error, Result, varint};

pub const BLOCK_TERMS: usize = 64;

fn zigzag(delta: i64) -> u64 {
    ((delta << 1) ^ (delta >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// Location of a byte range in a segment's postings, payload or ordinals area.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Extent {
    pub offset: u64,
    pub len: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermEntry {
    /// Number of documents containing the term in this segment.
    pub df: u32,
    /// Largest term-frequency bucket over those documents, for score bounds.
    pub max_tf_bucket: u8,
    pub postings: Extent,
    pub payload: Extent,
    /// The term's ordinal stream; empty before [`Format::Lsg4`].
    pub ordinals: Extent,
}

#[derive(Debug)]
pub struct DictionaryBuilder {
    format: Format,
    count: usize,
    index: Vec<u8>,
    blocks: Vec<u8>,
    last: Vec<u8>,
    /// Where the previous entry's extents ended, for the gaps.
    postings_end: u64,
    payload_end: u64,
    ordinals_end: u64,
}

impl Default for DictionaryBuilder {
    fn default() -> Self {
        Self::with_format(Format::CURRENT)
    }
}

impl DictionaryBuilder {
    /// A builder writing the entry layout of `format`, for compatibility
    /// tests; [`Default`] writes the current one.
    pub fn with_format(format: Format) -> Self {
        Self {
            format,
            count: 0,
            index: Vec::new(),
            blocks: Vec::new(),
            last: Vec::new(),
            postings_end: 0,
            payload_end: 0,
            ordinals_end: 0,
        }
    }

    /// Terms must arrive in strictly increasing byte order.
    pub fn push(&mut self, term: &str, entry: TermEntry) -> Result<()> {
        if term.is_empty() {
            return Err(Error::EmptyTerm);
        }
        if self.count > 0 && self.last.as_slice() >= term.as_bytes() {
            return Err(Error::Unordered);
        }
        if entry.max_tf_bucket > crate::payload::MAX_TF_BUCKET {
            return Err(Error::InvalidTfBucket);
        }
        let shared = if self.count.is_multiple_of(BLOCK_TERMS) {
            varint::put(&mut self.index, self.blocks.len() as u64);
            varint::put(&mut self.index, term.len() as u64);
            self.index.extend_from_slice(term.as_bytes());
            self.postings_end = 0;
            self.payload_end = 0;
            self.ordinals_end = 0;
            0
        } else {
            common_prefix(&self.last, term.as_bytes())
        };
        let suffix = &term.as_bytes()[shared..];
        varint::put(&mut self.blocks, shared as u64);
        varint::put(&mut self.blocks, suffix.len() as u64);
        self.blocks.extend_from_slice(suffix);
        match self.format {
            Format::Lsg1 | Format::Lsg2 => {
                varint::put(&mut self.blocks, u64::from(entry.df));
                self.blocks.push(entry.max_tf_bucket);
                varint::put(&mut self.blocks, entry.postings.offset);
                varint::put(&mut self.blocks, u64::from(entry.postings.len));
                varint::put(&mut self.blocks, entry.payload.offset);
                varint::put(&mut self.blocks, u64::from(entry.payload.len));
            }
            Format::Lsg3 | Format::Lsg4 | Format::Lsg5 => {
                varint::put(
                    &mut self.blocks,
                    u64::from(entry.df) << 4 | u64::from(entry.max_tf_bucket),
                );
                let gap = |offset: u64, end: u64| zigzag(offset.wrapping_sub(end) as i64);
                varint::put(
                    &mut self.blocks,
                    gap(entry.postings.offset, self.postings_end),
                );
                varint::put(&mut self.blocks, u64::from(entry.postings.len));
                varint::put(
                    &mut self.blocks,
                    gap(entry.payload.offset, self.payload_end),
                );
                varint::put(&mut self.blocks, u64::from(entry.payload.len));
                if self.format.has_ordinals() {
                    varint::put(
                        &mut self.blocks,
                        gap(entry.ordinals.offset, self.ordinals_end),
                    );
                    varint::put(&mut self.blocks, u64::from(entry.ordinals.len));
                }
            }
        }
        self.ordinals_end = entry
            .ordinals
            .offset
            .wrapping_add(u64::from(entry.ordinals.len));
        self.postings_end = entry
            .postings
            .offset
            .wrapping_add(u64::from(entry.postings.len));
        self.payload_end = entry
            .payload
            .offset
            .wrapping_add(u64::from(entry.payload.len));
        self.last.clear();
        self.last.extend_from_slice(term.as_bytes());
        self.count += 1;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.index.len() + self.blocks.len() + 16);
        varint::put(&mut out, self.count as u64);
        varint::put(&mut out, self.count.div_ceil(BLOCK_TERMS) as u64);
        varint::put(&mut out, self.index.len() as u64);
        out.extend_from_slice(&self.index);
        out.extend_from_slice(&self.blocks);
        out
    }
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Index entries held decoded, out of every `SAMPLE`: a segment of millions
/// of terms has hundreds of thousands of blocks, and a backend holds the
/// index of every segment it reads, so the rest are parsed from the index
/// bytes on demand, at most `SAMPLE` entries per lookup.
const SAMPLE: usize = 16;

/// The block index: where each block starts and its first term, decoded at
/// every `SAMPLE`th block and parsed between those on demand.
#[derive(Clone, Debug)]
pub struct DictionaryIndex<'a> {
    count: usize,
    /// Byte length of the header and index, so callers can locate the blocks.
    pub header_len: usize,
    /// The index entries as written.
    index_bytes: &'a [u8],
    blocks: usize,
    /// Every `SAMPLE`th entry: its first term, its block's offset and where
    /// the entry starts in `index_bytes`.
    samples: Vec<(&'a [u8], usize, usize)>,
}

impl<'a> DictionaryIndex<'a> {
    /// Parses the header and index from the start of a dictionary stream;
    /// `bytes` may be a prefix that ends anywhere after the index.
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        let count = reader.varint_u32()? as usize;
        let block_count = reader.varint_u32()? as usize;
        if block_count != count.div_ceil(BLOCK_TERMS) {
            return Err(Error::Corrupt("dictionary block count"));
        }
        let index_len = reader.varint_u32()? as usize;
        let index_bytes = reader.take(index_len)?;
        let header_len = reader.position();
        // Each index entry needs at least two varints. Do not reserve
        // from a claimed block count before reading those bytes.
        let mut samples = Vec::with_capacity(block_count.min(index_bytes.len() / 2) / SAMPLE + 1);
        let mut index_reader = Reader::new(index_bytes);
        let mut previous: Option<(&[u8], usize)> = None;
        for block in 0..block_count {
            let at = index_reader.position();
            let offset = index_reader.varint_u32()? as usize;
            let len = index_reader.varint_u32()? as usize;
            let first = index_reader.take(len)?;
            if previous.is_some_and(|(p, o)| p >= first || o >= offset) {
                return Err(Error::Corrupt("dictionary index order"));
            }
            if block.is_multiple_of(SAMPLE) {
                samples.push((first, offset, at));
            }
            previous = Some((first, offset));
        }
        if index_reader.remaining() != 0 {
            return Err(Error::Corrupt("dictionary index length"));
        }
        Ok(Self {
            count,
            header_len,
            index_bytes,
            blocks: block_count,
            samples,
        })
    }

    /// Entry `block`: its first term and its block's offset. Parsed forward
    /// from the nearest sample; the bytes were validated when parsed.
    fn entry(&self, block: usize) -> Option<(&'a [u8], usize)> {
        if block >= self.blocks {
            return None;
        }
        let (first, offset, at) = self.samples[block / SAMPLE];
        let mut entry = (first, offset);
        let mut reader = Reader::at(self.index_bytes, at);
        for _ in 0..block % SAMPLE + 1 {
            let offset = reader.varint_u32().ok()? as usize;
            let len = reader.varint_u32().ok()? as usize;
            entry = (reader.take(len).ok()?, offset);
        }
        Some(entry)
    }

    /// How many bytes of a stream are needed to parse the index: the header
    /// plus `index_len`, discoverable from the first few bytes.
    pub fn prefix_len(bytes: &[u8]) -> Result<usize> {
        let mut reader = Reader::new(bytes);
        reader.varint()?;
        reader.varint()?;
        let index_len = reader.varint_u32()? as usize;
        Ok(reader.position() + index_len)
    }

    pub const fn len(&self) -> usize {
        self.count
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub const fn blocks(&self) -> usize {
        self.blocks
    }

    /// First term of block `block`, as recorded in the index.
    pub fn block_first(&self, block: usize) -> Option<&'a [u8]> {
        self.entry(block).map(|(first, _)| first)
    }

    /// Index of the block that could contain `term`, if any block starts at or
    /// before it.
    fn block_for(&self, term: &[u8]) -> Option<usize> {
        // The last sample at or before the term, then the last entry at or
        // before it among the ones up to the next sample.
        let sample = self
            .samples
            .partition_point(|(first, _, _)| *first <= term)
            .checked_sub(1)?;
        let mut block = sample * SAMPLE;
        for next in block + 1..(block + SAMPLE).min(self.blocks) {
            match self.entry(next) {
                Some((first, _)) if first <= term => block = next,
                _ => break,
            }
        }
        Some(block)
    }

    fn block_terms(&self, block: usize) -> usize {
        (self.count - block * BLOCK_TERMS).min(BLOCK_TERMS)
    }

    /// Byte range of block `block` within the blocks area.
    fn block_range(&self, block: usize, blocks_len: usize) -> (usize, usize) {
        let start = self.entry(block).expect("block index is in range").1;
        let end = self
            .entry(block + 1)
            .map_or(blocks_len, |(_, offset)| offset);
        (start, end)
    }
}

/// Fetches ranges of the blocks area on demand.
pub trait BlockFetch {
    fn fetch_block(&self, offset: usize, len: usize) -> Result<&[u8]>;
}

/// Access to the blocks area, either in memory or fetched per block.
#[derive(Clone, Copy)]
pub enum Blocks<'a> {
    Slice(&'a [u8]),
    Lazy {
        len: usize,
        fetch: &'a dyn BlockFetch,
    },
}

impl<'a> Blocks<'a> {
    fn len(&self) -> usize {
        match self {
            Self::Slice(bytes) => bytes.len(),
            Self::Lazy { len, .. } => *len,
        }
    }

    fn block(&self, range: (usize, usize)) -> Result<&'a [u8]> {
        let (start, end) = range;
        if start > end || end > self.len() {
            return Err(Error::Corrupt("dictionary block bounds"));
        }
        match self {
            Self::Slice(bytes) => Ok(&bytes[start..end]),
            Self::Lazy { fetch, .. } => fetch.fetch_block(start, end - start),
        }
    }
}

/// A dictionary view: an index plus block access. Cheap to copy.
#[derive(Clone, Copy)]
pub struct Dictionary<'a> {
    index: &'a DictionaryIndex<'a>,
    blocks: Blocks<'a>,
    format: Format,
}

impl<'a> Dictionary<'a> {
    /// A view over blocks in the current entry layout.
    pub const fn new(index: &'a DictionaryIndex<'a>, blocks: Blocks<'a>) -> Self {
        Self::with_format(index, blocks, Format::CURRENT)
    }

    /// A view over blocks in the entry layout of `format`.
    pub const fn with_format(
        index: &'a DictionaryIndex<'a>,
        blocks: Blocks<'a>,
        format: Format,
    ) -> Self {
        Self {
            index,
            blocks,
            format,
        }
    }

    pub const fn len(&self) -> usize {
        self.index.count
    }

    pub const fn is_empty(&self) -> bool {
        self.index.count == 0
    }

    pub const fn index(&self) -> &'a DictionaryIndex<'a> {
        self.index
    }

    /// Every term of block `block` in order, checking that the block's bytes
    /// are consumed exactly; a verifier walks blocks one by one so a problem
    /// in one block does not hide the others.
    pub fn block(&self, block: usize) -> Result<Vec<(String, TermEntry)>> {
        Ok(self
            .block_sizes(block)?
            .into_iter()
            .map(|(term, entry, _)| (term, entry))
            .collect())
    }

    /// As [`Dictionary::block`], with the encoded byte length of each entry,
    /// for size accounting.
    pub fn block_sizes(&self, block: usize) -> Result<Vec<(String, TermEntry, usize)>> {
        if block >= self.index.blocks() {
            return Err(Error::Corrupt("dictionary block out of range"));
        }
        let mut walker = self.walker(block)?;
        let mut out = Vec::with_capacity(walker.remaining_in_block);
        while walker.remaining_in_block > 0 {
            let before = walker.reader.position();
            let entry = walker.step()?;
            let term = String::from_utf8(walker.term.clone())
                .map_err(|_| Error::Corrupt("dictionary term is not UTF-8"))?;
            out.push((term, entry, walker.reader.position() - before));
        }
        if walker.reader.remaining() != 0 {
            return Err(Error::Corrupt("dictionary block length"));
        }
        Ok(out)
    }

    fn walker(&self, block: usize) -> Result<Walker<'a>> {
        let range = self.index.block_range(block, self.blocks.len());
        Ok(Walker {
            dictionary: *self,
            block,
            reader: Reader::new(self.blocks.block(range)?),
            remaining_in_block: self.index.block_terms(block),
            term: Vec::new(),
            postings_end: 0,
            payload_end: 0,
            ordinals_end: 0,
        })
    }

    pub fn get(&self, term: &str) -> Result<Option<TermEntry>> {
        let Some(block) = self.index.block_for(term.as_bytes()) else {
            return Ok(None);
        };
        let mut walker = self.walker(block)?;
        while walker.remaining_in_block > 0 {
            let entry = walker.step()?;
            match walker.term.as_slice().cmp(term.as_bytes()) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => return Ok(Some(entry)),
                std::cmp::Ordering::Greater => return Ok(None),
            }
        }
        Ok(None)
    }

    /// All terms at or after `start`, in order.
    pub fn iter_from(&self, start: &str) -> Iter<'a> {
        let block = if self.index.count == 0 {
            None
        } else {
            Some(self.index.block_for(start.as_bytes()).unwrap_or(0))
        };
        let mut iter = Iter {
            walker: None,
            pending: None,
        };
        let Some(block) = block else {
            return iter;
        };
        iter.walker = match self.walker(block) {
            Ok(walker) => Some(walker),
            Err(error) => {
                iter.pending = Some(Err(error));
                return iter;
            }
        };
        // Skip entries below `start` inside the first block.
        while let Some(walker) = iter.walker.as_mut() {
            if walker.remaining_in_block == 0 {
                iter.walker = match walker.next_block() {
                    Ok(next) => next,
                    Err(error) => {
                        iter.pending = Some(Err(error));
                        return iter;
                    }
                };
                continue;
            }
            match walker.step() {
                Ok(entry) => {
                    if walker.term.as_slice() >= start.as_bytes() {
                        iter.pending = Some(Ok((walker.term.clone(), entry)));
                        break;
                    }
                }
                Err(error) => {
                    iter.pending = Some(Err(error));
                    break;
                }
            }
        }
        iter
    }

    pub fn iter(&self) -> Iter<'a> {
        self.iter_from("")
    }

    /// Terms starting with `prefix`.
    pub fn prefix(&self, prefix: &str) -> impl Iterator<Item = Result<(String, TermEntry)>> + 'a {
        let prefix = prefix.to_owned();
        self.iter_from(&prefix).take_while(move |item| match item {
            Ok((term, _)) => term.starts_with(&prefix),
            Err(_) => true,
        })
    }

    /// Terms in the inclusive range; `None` is an open bound.
    pub fn range(
        &self,
        lower: Option<&str>,
        upper: Option<&str>,
    ) -> impl Iterator<Item = Result<(String, TermEntry)>> + 'a {
        let upper = upper.map(str::to_owned);
        self.iter_from(lower.unwrap_or(""))
            .take_while(move |item| match item {
                Ok((term, _)) => upper.as_deref().is_none_or(|upper| term.as_str() <= upper),
                Err(_) => true,
            })
    }
}

struct Walker<'a> {
    dictionary: Dictionary<'a>,
    block: usize,
    reader: Reader<'a>,
    remaining_in_block: usize,
    term: Vec<u8>,
    /// Where the previous entry's extents ended, for the gaps.
    postings_end: u64,
    payload_end: u64,
    ordinals_end: u64,
}

impl<'a> Walker<'a> {
    fn step(&mut self) -> Result<TermEntry> {
        let shared = self.reader.varint_u32()? as usize;
        if shared > self.term.len() {
            return Err(Error::Corrupt("dictionary shared prefix"));
        }
        let suffix_len = self.reader.varint_u32()? as usize;
        let suffix = self.reader.take(suffix_len)?;
        let previous = std::mem::take(&mut self.term);
        self.term.extend_from_slice(&previous[..shared]);
        self.term.extend_from_slice(suffix);
        if self.term.is_empty() || (!previous.is_empty() && previous >= self.term) {
            return Err(Error::Corrupt("dictionary term order"));
        }
        let mut ordinals = Extent::default();
        let (df, max_tf_bucket, postings, payload) = match self.dictionary.format {
            Format::Lsg1 | Format::Lsg2 => {
                let df = self.reader.varint_u32()?;
                let max_tf_bucket = self.reader.u8()?;
                let postings = Extent {
                    offset: self.reader.varint()?,
                    len: self.reader.varint_u32()?,
                };
                let payload = Extent {
                    offset: self.reader.varint()?,
                    len: self.reader.varint_u32()?,
                };
                (df, max_tf_bucket, postings, payload)
            }
            Format::Lsg3 | Format::Lsg4 | Format::Lsg5 => {
                let df_bucket = self.reader.varint()?;
                let df =
                    u32::try_from(df_bucket >> 4).map_err(|_| Error::Corrupt("dictionary df"))?;
                let max_tf_bucket = (df_bucket & 0xf) as u8;
                let offset = |end: u64, gap: i64| {
                    end.checked_add_signed(gap)
                        .ok_or(Error::Corrupt("dictionary extent gap"))
                };
                let postings_gap = unzigzag(self.reader.varint()?);
                let postings = Extent {
                    offset: offset(self.postings_end, postings_gap)?,
                    len: self.reader.varint_u32()?,
                };
                let payload_gap = unzigzag(self.reader.varint()?);
                let payload = Extent {
                    offset: offset(self.payload_end, payload_gap)?,
                    len: self.reader.varint_u32()?,
                };
                if self.dictionary.format.has_ordinals() {
                    let ordinals_gap = unzigzag(self.reader.varint()?);
                    ordinals = Extent {
                        offset: offset(self.ordinals_end, ordinals_gap)?,
                        len: self.reader.varint_u32()?,
                    };
                }
                (df, max_tf_bucket, postings, payload)
            }
        };
        if max_tf_bucket > crate::payload::MAX_TF_BUCKET {
            return Err(Error::Corrupt("dictionary bucket"));
        }
        self.postings_end = postings.offset.wrapping_add(u64::from(postings.len));
        self.payload_end = payload.offset.wrapping_add(u64::from(payload.len));
        self.ordinals_end = ordinals.offset.wrapping_add(u64::from(ordinals.len));
        self.remaining_in_block -= 1;
        Ok(TermEntry {
            df,
            max_tf_bucket,
            postings,
            payload,
            ordinals,
        })
    }

    fn next_block(&self) -> Result<Option<Walker<'a>>> {
        let block = self.block + 1;
        if block >= self.dictionary.index.blocks() {
            return Ok(None);
        }
        self.dictionary.walker(block).map(Some)
    }
}

pub struct Iter<'a> {
    walker: Option<Walker<'a>>,
    pending: Option<Result<(Vec<u8>, TermEntry)>>,
}

impl Iterator for Iter<'_> {
    type Item = Result<(String, TermEntry)>;

    fn next(&mut self) -> Option<Self::Item> {
        let item = if let Some(pending) = self.pending.take() {
            pending
        } else {
            loop {
                let walker = self.walker.as_mut()?;
                if walker.remaining_in_block == 0 {
                    match walker.next_block() {
                        Ok(next) => self.walker = next,
                        Err(error) => {
                            self.walker = None;
                            return Some(Err(error));
                        }
                    }
                    continue;
                }
                break walker.step().map(|entry| (walker.term.clone(), entry));
            }
        };
        Some(match item {
            Ok((term, entry)) => match String::from_utf8(term) {
                Ok(term) => Ok((term, entry)),
                Err(_) => {
                    self.walker = None;
                    Err(Error::Corrupt("dictionary term is not UTF-8"))
                }
            },
            Err(error) => {
                self.walker = None;
                Err(error)
            }
        })
    }
}

/// Convenience for tests and in-memory callers: a dictionary over one slice.
pub struct OwnedDictionary<'a> {
    index: DictionaryIndex<'a>,
    blocks: Blocks<'a>,
    format: Format,
}

impl<'a> OwnedDictionary<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        Self::parse_format(bytes, Format::CURRENT)
    }

    pub fn parse_format(bytes: &'a [u8], format: Format) -> Result<Self> {
        let index = DictionaryIndex::parse(bytes)?;
        let blocks = Blocks::Slice(&bytes[index.header_len..]);
        Ok(Self {
            index,
            blocks,
            format,
        })
    }

    pub fn view(&self) -> Dictionary<'_> {
        Dictionary::with_format(&self.index, self.blocks, self.format)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(i: u32) -> TermEntry {
        TermEntry {
            df: i,
            max_tf_bucket: (i % 16) as u8,
            postings: Extent {
                offset: u64::from(i) * 100,
                len: i * 3,
            },
            payload: Extent {
                offset: u64::from(i) * 1000,
                len: i * 7,
            },
            ordinals: Extent {
                offset: u64::from(i) * 10,
                len: i * 2,
            },
        }
    }

    fn build(terms: &[&str]) -> Vec<u8> {
        let mut builder = DictionaryBuilder::default();
        for (i, term) in terms.iter().enumerate() {
            builder.push(term, entry(i as u32)).unwrap();
        }
        builder.finish()
    }

    #[test]
    fn lookups_ranges_and_prefixes_across_blocks() {
        let terms: Vec<String> = (0..300).map(|i| format!("term{i:04}")).collect();
        let refs: Vec<&str> = terms.iter().map(String::as_str).collect();
        let bytes = build(&refs);
        let owned = OwnedDictionary::parse(&bytes).unwrap();
        let dictionary = owned.view();
        assert_eq!(dictionary.len(), 300);
        for (i, term) in refs.iter().enumerate() {
            assert_eq!(
                dictionary.get(term).unwrap(),
                Some(entry(i as u32)),
                "{term}"
            );
        }
        assert_eq!(dictionary.get("term").unwrap(), None);
        assert_eq!(dictionary.get("a").unwrap(), None);
        assert_eq!(dictionary.get("term0063x").unwrap(), None);
        assert_eq!(dictionary.get("zzz").unwrap(), None);
        let all: Vec<String> = dictionary.iter().map(|r| r.unwrap().0).collect();
        assert_eq!(all, terms);
        let prefixed: Vec<String> = dictionary.prefix("term01").map(|r| r.unwrap().0).collect();
        assert_eq!(prefixed, terms[100..200]);
        let ranged: Vec<String> = dictionary
            .range(Some("term0060"), Some("term0070"))
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(ranged, terms[60..=70]);
        let open: Vec<String> = dictionary
            .range(None, Some("term0001"))
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(open, terms[..2]);
        let tail: Vec<String> = dictionary
            .range(Some("term0298"), None)
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(tail, terms[298..]);
        assert_eq!(dictionary.prefix("zzz").count(), 0);
        assert_eq!(dictionary.iter_from("a").count(), 300);
    }

    #[test]
    fn lazy_blocks_fetch_one_block_per_lookup() {
        let terms: Vec<String> = (0..300).map(|i| format!("term{i:04}")).collect();
        let refs: Vec<&str> = terms.iter().map(String::as_str).collect();
        let bytes = build(&refs);
        let prefix = DictionaryIndex::prefix_len(&bytes).unwrap();
        let index = DictionaryIndex::parse(&bytes[..prefix]).unwrap();
        assert_eq!(index.header_len, prefix);
        let area = &bytes[prefix..];
        struct Counting<'a> {
            area: &'a [u8],
            fetches: std::cell::Cell<usize>,
        }
        impl BlockFetch for Counting<'_> {
            fn fetch_block(&self, offset: usize, len: usize) -> Result<&[u8]> {
                self.fetches.set(self.fetches.get() + 1);
                Ok(&self.area[offset..offset + len])
            }
        }
        let counting = Counting {
            area,
            fetches: std::cell::Cell::new(0),
        };
        let fetches = &counting.fetches;
        let blocks = Blocks::Lazy {
            len: area.len(),
            fetch: &counting,
        };
        let dictionary = Dictionary::new(&index, blocks);
        assert_eq!(dictionary.get("term0130").unwrap(), Some(entry(130)));
        assert_eq!(fetches.get(), 1);
        // A miss inside the key space still costs exactly one block.
        assert_eq!(dictionary.get("term0130x").unwrap(), None);
        assert_eq!(fetches.get(), 2);
        // A miss below the first term costs nothing.
        assert_eq!(dictionary.get("nothing").unwrap(), None);
        assert_eq!(fetches.get(), 2);
        let ranged: Vec<String> = dictionary
            .range(Some("term0060"), Some("term0070"))
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(ranged, terms[60..=70]);
        assert_eq!(fetches.get(), 4);
        assert_eq!(dictionary.iter().count(), 300);
    }

    #[test]
    fn empty_dictionary_and_unicode_terms() {
        let bytes = build(&[]);
        let owned = OwnedDictionary::parse(&bytes).unwrap();
        let dictionary = owned.view();
        assert!(dictionary.is_empty());
        assert_eq!(dictionary.get("x").unwrap(), None);
        assert_eq!(dictionary.iter().count(), 0);
        let mut terms = vec!["café", "cafés", "日本", "日本語", "🍺", "z"];
        terms.sort_unstable();
        let bytes = build(&terms);
        let owned = OwnedDictionary::parse(&bytes).unwrap();
        let dictionary = owned.view();
        let all: Vec<String> = dictionary.iter().map(|r| r.unwrap().0).collect();
        assert_eq!(all, terms);
        let prefixed: Vec<String> = dictionary.prefix("日本").map(|r| r.unwrap().0).collect();
        assert_eq!(prefixed, ["日本", "日本語"]);
    }

    #[test]
    fn builder_rejects_bad_input_and_reader_rejects_corruption() {
        let mut builder = DictionaryBuilder::default();
        assert_eq!(builder.push("", entry(0)), Err(Error::EmptyTerm));
        builder.push("b", entry(0)).unwrap();
        assert_eq!(builder.push("b", entry(0)), Err(Error::Unordered));
        assert_eq!(builder.push("a", entry(0)), Err(Error::Unordered));
        assert_eq!(
            builder.push(
                "c",
                TermEntry {
                    max_tf_bucket: 16,
                    ..entry(0)
                }
            ),
            Err(Error::InvalidTfBucket)
        );
        let terms: Vec<String> = (0..70).map(|i| format!("w{i:03}")).collect();
        let refs: Vec<&str> = terms.iter().map(String::as_str).collect();
        let bytes = build(&refs);
        let truncated = &bytes[..bytes.len() - 1];
        assert!(OwnedDictionary::parse(truncated).is_ok_and(|d| d.view().get("w069").is_err()));
        assert!(OwnedDictionary::parse(&bytes[..5]).is_err());
        // Break UTF-8 in the last term's suffix byte: its entry follows the
        // five before it in the second block, and its suffix ("9" after the
        // shared "w06") follows the two one-byte varints of the entry.
        let owned = OwnedDictionary::parse(&bytes).unwrap();
        let sizes = owned.view().block_sizes(1).unwrap();
        assert_eq!(sizes.len(), 6);
        let last_at = owned.index.header_len
            + owned.index.entry(1).unwrap().1
            + sizes[..5].iter().map(|(_, _, n)| n).sum::<usize>();
        let mut tampered = bytes.clone();
        assert_eq!(tampered[last_at + 2], b'9');
        tampered[last_at + 2] = 0xff;
        let owned = OwnedDictionary::parse(&tampered).unwrap();
        assert!(owned.view().iter().any(|item| item.is_err()));
    }

    #[test]
    fn earlier_entry_layouts_are_still_read_and_the_current_one_is_smaller() {
        // Extents laid out back to back, as a segment builder writes them.
        let mut entries = Vec::new();
        let (mut postings_at, mut payload_at) = (1_000_000u64, 40_000_000u64);
        for i in 0..300u32 {
            let entry = TermEntry {
                df: 1 + i % 5,
                max_tf_bucket: (i % 16) as u8,
                postings: Extent {
                    offset: postings_at,
                    len: 3 + i,
                },
                payload: Extent {
                    offset: payload_at,
                    len: 10 + i,
                },
                ordinals: Default::default(),
            };
            postings_at += u64::from(entry.postings.len);
            payload_at += u64::from(entry.payload.len);
            entries.push((format!("term{i:04}"), entry));
        }
        let mut sizes = Vec::new();
        for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] {
            let mut builder = DictionaryBuilder::with_format(format);
            for (term, entry) in &entries {
                builder.push(term, *entry).unwrap();
            }
            let bytes = builder.finish();
            let owned = OwnedDictionary::parse_format(&bytes, format).unwrap();
            let all: Vec<(String, TermEntry)> = owned.view().iter().collect::<Result<_>>().unwrap();
            assert_eq!(all, entries, "{format}");
            assert_eq!(
                owned.view().get("term0299").unwrap(),
                Some(entries[299].1),
                "{format}"
            );
            sizes.push(bytes.len());
        }
        // LSG1 and LSG2 share the entry layout. Absolute offsets of three
        // and four bytes become one-byte gaps (except at block starts) and
        // df shares a byte with the bucket.
        assert_eq!(sizes[0], sizes[1]);
        assert!(sizes[2] + 300 * 5 <= sizes[1], "{sizes:?}");
        // Gaps are signed, so extents in any order still round-trip.
        let mut builder = DictionaryBuilder::default();
        let backwards: Vec<(String, TermEntry)> = entries
            .iter()
            .rev()
            .zip(&entries)
            .map(|((_, entry), (term, _))| (term.clone(), *entry))
            .collect();
        for (term, entry) in &backwards {
            builder.push(term, *entry).unwrap();
        }
        let bytes = builder.finish();
        let owned = OwnedDictionary::parse(&bytes).unwrap();
        let all: Vec<(String, TermEntry)> = owned.view().iter().collect::<Result<_>>().unwrap();
        assert_eq!(all, backwards);
    }
}
