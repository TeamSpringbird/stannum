//! Sorted, prefix-compressed term dictionary.
//!
//! Terms are ordered by their UTF-8 bytes, which is the order TINQL ranges
//! (`a TO cat`) and prefixes (`brew*`) need. Terms are grouped into blocks of
//! `BLOCK_TERMS`; a block index of first terms supports binary search and each
//! block is decoded sequentially from its first (uncompressed) term.
//!
//! ```text
//! stream := count varint, block_count varint, index_len varint, index, blocks
//! index  := (block_offset varint, first_len varint, first_bytes)* block_count
//! entry  := shared varint, suffix_len varint, suffix, df varint, max_tf_bucket u8,
//!           postings_offset varint, postings_len varint,
//!           payload_offset varint, payload_len varint
//! ```

use crate::reader::Reader;
use crate::{Error, Result, varint};

pub const BLOCK_TERMS: usize = 64;

/// Location of a byte range in a segment's postings or payload area.
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
}

#[derive(Default, Debug)]
pub struct DictionaryBuilder {
    count: usize,
    index: Vec<u8>,
    blocks: Vec<u8>,
    last: Vec<u8>,
}

impl DictionaryBuilder {
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
            0
        } else {
            common_prefix(&self.last, term.as_bytes())
        };
        let suffix = &term.as_bytes()[shared..];
        varint::put(&mut self.blocks, shared as u64);
        varint::put(&mut self.blocks, suffix.len() as u64);
        self.blocks.extend_from_slice(suffix);
        varint::put(&mut self.blocks, u64::from(entry.df));
        self.blocks.push(entry.max_tf_bucket);
        varint::put(&mut self.blocks, entry.postings.offset);
        varint::put(&mut self.blocks, u64::from(entry.postings.len));
        varint::put(&mut self.blocks, entry.payload.offset);
        varint::put(&mut self.blocks, u64::from(entry.payload.len));
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

/// A parsed dictionary. The block index is decoded eagerly; blocks are decoded
/// on demand.
#[derive(Clone, Debug)]
pub struct Dictionary<'a> {
    count: usize,
    index: Vec<(&'a [u8], usize)>,
    blocks: &'a [u8],
}

impl<'a> Dictionary<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        let count = reader.varint_u32()? as usize;
        let block_count = reader.varint_u32()? as usize;
        if block_count != count.div_ceil(BLOCK_TERMS) {
            return Err(Error::Corrupt("dictionary block count"));
        }
        let index_len = reader.varint_u32()? as usize;
        let index_bytes = reader.take(index_len)?;
        let blocks = reader.take(reader.remaining())?;
        let mut index = Vec::with_capacity(block_count);
        let mut index_reader = Reader::new(index_bytes);
        let mut previous: Option<&[u8]> = None;
        for _ in 0..block_count {
            let offset = index_reader.varint_u32()? as usize;
            let len = index_reader.varint_u32()? as usize;
            let first = index_reader.take(len)?;
            if offset > blocks.len()
                || previous.is_some_and(|p| p >= first)
                || index.last().is_some_and(|(_, o)| *o >= offset)
            {
                return Err(Error::Corrupt("dictionary index order"));
            }
            index.push((first, offset));
            previous = Some(first);
        }
        if index_reader.remaining() != 0 {
            return Err(Error::Corrupt("dictionary index length"));
        }
        Ok(Self {
            count,
            index,
            blocks,
        })
    }

    pub const fn len(&self) -> usize {
        self.count
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Index of the block that could contain `term`, if any block starts at or
    /// before it.
    fn block_for(&self, term: &[u8]) -> Option<usize> {
        self.index
            .partition_point(|(first, _)| *first <= term)
            .checked_sub(1)
    }

    fn block_terms(&self, block: usize) -> usize {
        (self.count - block * BLOCK_TERMS).min(BLOCK_TERMS)
    }

    fn walker(&self, block: usize) -> Walker<'a, '_> {
        Walker {
            dictionary: self,
            block,
            reader: Reader::at(self.blocks, self.index[block].1),
            remaining_in_block: self.block_terms(block),
            term: Vec::new(),
        }
    }

    pub fn get(&self, term: &str) -> Result<Option<TermEntry>> {
        let Some(block) = self.block_for(term.as_bytes()) else {
            return Ok(None);
        };
        let mut walker = self.walker(block);
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
    pub fn iter_from(&self, start: &str) -> Iter<'a, '_> {
        let block = self.block_for(start.as_bytes());
        let mut iter = Iter {
            walker: block.map(|block| self.walker(block)),
            pending: None,
        };
        if block.is_none() && self.count > 0 {
            iter.walker = Some(self.walker(0));
        }
        // Skip entries below `start` inside the first block.
        while let Some(walker) = iter.walker.as_mut() {
            if walker.remaining_in_block == 0 {
                iter.walker = walker.next_block();
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

    pub fn iter(&self) -> Iter<'a, '_> {
        self.iter_from("")
    }

    /// Terms starting with `prefix`.
    pub fn prefix(&self, prefix: &str) -> impl Iterator<Item = Result<(String, TermEntry)>> {
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
    ) -> impl Iterator<Item = Result<(String, TermEntry)>> {
        let upper = upper.map(str::to_owned);
        self.iter_from(lower.unwrap_or(""))
            .take_while(move |item| match item {
                Ok((term, _)) => upper.as_deref().is_none_or(|upper| term.as_str() <= upper),
                Err(_) => true,
            })
    }
}

struct Walker<'a, 'd> {
    dictionary: &'d Dictionary<'a>,
    block: usize,
    reader: Reader<'a>,
    remaining_in_block: usize,
    term: Vec<u8>,
}

impl<'a, 'd> Walker<'a, 'd> {
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
        let df = self.reader.varint_u32()?;
        let max_tf_bucket = self.reader.u8()?;
        if max_tf_bucket > crate::payload::MAX_TF_BUCKET {
            return Err(Error::Corrupt("dictionary bucket"));
        }
        let postings = Extent {
            offset: self.reader.varint()?,
            len: self.reader.varint_u32()?,
        };
        let payload = Extent {
            offset: self.reader.varint()?,
            len: self.reader.varint_u32()?,
        };
        self.remaining_in_block -= 1;
        Ok(TermEntry {
            df,
            max_tf_bucket,
            postings,
            payload,
        })
    }

    fn next_block(&self) -> Option<Walker<'a, 'd>> {
        let block = self.block + 1;
        (block < self.dictionary.index.len()).then(|| self.dictionary.walker(block))
    }
}

pub struct Iter<'a, 'd> {
    walker: Option<Walker<'a, 'd>>,
    pending: Option<Result<(Vec<u8>, TermEntry)>>,
}

impl Iterator for Iter<'_, '_> {
    type Item = Result<(String, TermEntry)>;

    fn next(&mut self) -> Option<Self::Item> {
        let item = if let Some(pending) = self.pending.take() {
            pending
        } else {
            loop {
                let walker = self.walker.as_mut()?;
                if walker.remaining_in_block == 0 {
                    self.walker = walker.next_block();
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
        let dictionary = Dictionary::parse(&bytes).unwrap();
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
    fn empty_dictionary_and_unicode_terms() {
        let bytes = build(&[]);
        let dictionary = Dictionary::parse(&bytes).unwrap();
        assert!(dictionary.is_empty());
        assert_eq!(dictionary.get("x").unwrap(), None);
        assert_eq!(dictionary.iter().count(), 0);
        let mut terms = vec!["café", "cafés", "日本", "日本語", "🍺", "z"];
        terms.sort_unstable();
        let bytes = build(&terms);
        let dictionary = Dictionary::parse(&bytes).unwrap();
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
        assert!(Dictionary::parse(&bytes[..bytes.len() - 1]).is_ok_and(|d| d.get("w069").is_err()));
        assert!(Dictionary::parse(&bytes[..5]).is_err());
        let mut tampered = bytes.clone();
        // Break UTF-8 in the last term's suffix byte.
        let last = tampered.len() - 8;
        tampered[last] = 0xff;
        let dictionary = Dictionary::parse(&tampered).unwrap();
        assert!(dictionary.iter().any(|item| item.is_err()));
    }
}
