// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

use super::Window;
use crate::{
    Error, Result,
    dictionary::{BLOCK_TERMS, Extent, TermEntry},
    segment::Format,
    source::Source,
};

/// Forward-only dictionary validation without retaining its block index.
/// Retains two encoded windows and at most two terms, each capped by the caller's
/// `max_term_bytes`. Source caches and callback allocations are outside this bound.
/// Cross-stream ownership, extent overlap and score validation remain the caller's
/// responsibility. Call through `None` before publishing output.
pub struct DictionaryCursor<'a, S: Source + ?Sized> {
    index: Window<'a, S>,
    data: Window<'a, S>,
    blocks_start: u64,
    count: u32,
    ordinal: u32,
    term: Vec<u8>,
    scratch: Vec<u8>,
    max_term_bytes: usize,
    format: Format,
    postings_end: u64,
    payload_end: u64,
    ordinals_end: u64,
    failed: bool,
}

impl<'a, S: Source + ?Sized> DictionaryCursor<'a, S> {
    pub fn new(
        source: &'a S,
        start: u64,
        len: u64,
        format: Format,
        window_bytes: usize,
        max_term_bytes: usize,
    ) -> Result<Self> {
        let end = start.checked_add(len).ok_or(Error::Truncated)?;
        let mut header = Window::new(source, start, end, window_bytes)?;
        let count = header.u32()?;
        if header.u32()? != count.div_ceil(BLOCK_TERMS as u32) {
            return Err(Error::Corrupt("dictionary block count"));
        }
        let index_len = u64::from(header.u32()?);
        let index_start = header.at;
        let blocks_start = index_start
            .checked_add(index_len)
            .filter(|v| *v <= end)
            .ok_or(Error::Truncated)?;
        drop(header);
        Ok(Self {
            index: Window::new(source, index_start, blocks_start, window_bytes)?,
            data: Window::new(source, blocks_start, end, window_bytes)?,
            blocks_start,
            count,
            ordinal: 0,
            term: Vec::new(),
            scratch: Vec::new(),
            max_term_bytes,
            format,
            postings_end: 0,
            payload_end: 0,
            ordinals_end: 0,
            failed: false,
        })
    }

    /// Allocated cursor byte buffers, including retained term capacity.
    pub fn retained_bytes(&self) -> usize {
        self.index.bytes.len()
            + self.data.bytes.len()
            + self.term.capacity()
            + self.scratch.capacity()
    }

    /// Checkpoints run at each entry and each decoded term byte, so even a large
    /// term can be cancelled. Any error poisons the cursor, including callback errors.
    pub fn next_with(
        &mut self,
        mut checkpoint: impl FnMut() -> Result<()>,
        mut visit: impl FnMut(&str, TermEntry) -> Result<()>,
    ) -> Result<Option<()>> {
        if self.failed {
            return Err(Error::Corrupt("maintenance cursor failed"));
        }
        let result = self.next_inner(&mut checkpoint, &mut visit);
        self.failed = result.is_err();
        result
    }

    fn next_inner(
        &mut self,
        checkpoint: &mut impl FnMut() -> Result<()>,
        visit: &mut impl FnMut(&str, TermEntry) -> Result<()>,
    ) -> Result<Option<()>> {
        checkpoint()?;
        if self.ordinal == self.count {
            if self.index.at != self.index.end || self.data.at != self.data.end {
                return Err(Error::Corrupt("dictionary trailing bytes"));
            }
            self.index.bytes = Box::default();
            self.data.bytes = Box::default();
            self.term = Vec::new();
            self.scratch = Vec::new();
            return Ok(None);
        }
        let block_start = self.ordinal.is_multiple_of(BLOCK_TERMS as u32);
        if block_start {
            if u64::from(self.index.u32()?) != self.data.at - self.blocks_start {
                return Err(Error::Corrupt("dictionary block offset"));
            }
            let len = self.index.u32()? as usize;
            self.read_index_term(len, checkpoint)?;
            self.postings_end = 0;
            self.payload_end = 0;
            self.ordinals_end = 0;
        }
        let shared = self.data.u32()? as usize;
        let suffix = self.data.u32()? as usize;
        if shared > self.term.len() || (block_start && shared != 0) {
            return Err(Error::Corrupt("dictionary shared prefix"));
        }
        let len = shared
            .checked_add(suffix)
            .filter(|v| *v <= self.max_term_bytes)
            .ok_or(Error::Corrupt("maintenance term limit"))?;
        // At block starts compare index bytes directly, then reuse scratch for decoding.
        if block_start {
            if len != self.scratch.len() {
                return Err(Error::Corrupt("dictionary index term"));
            }
            for i in 0..suffix {
                checkpoint()?;
                if self.data.byte()? != self.scratch[i] {
                    return Err(Error::Corrupt("dictionary index term"));
                }
            }
        } else {
            self.scratch.clear();
            self.scratch.reserve_exact(len);
            self.scratch.extend_from_slice(&self.term[..shared]);
            for _ in 0..suffix {
                checkpoint()?;
                self.scratch.push(self.data.byte()?);
            }
        }
        if self.scratch.is_empty() || (self.ordinal > 0 && self.term >= self.scratch) {
            return Err(Error::Corrupt("dictionary term order"));
        }
        let term = std::str::from_utf8(&self.scratch)
            .map_err(|_| Error::Corrupt("dictionary term is not UTF-8"))?;
        let (df, max_tf_bucket) = if self.format >= Format::Lsg3 {
            let packed = self.data.varint()?;
            (
                u32::try_from(packed >> 4).map_err(|_| Error::Corrupt("dictionary df"))?,
                (packed & 15) as u8,
            )
        } else {
            (self.data.u32()?, self.data.byte()?)
        };
        if max_tf_bucket > 15 {
            return Err(Error::InvalidTfBucket);
        }
        let postings = Self::extent(&mut self.data, self.format, &mut self.postings_end)?;
        let payload = Self::extent(&mut self.data, self.format, &mut self.payload_end)?;
        let ordinals = if self.format.has_ordinals() {
            Self::extent(&mut self.data, self.format, &mut self.ordinals_end)?
        } else {
            Extent::default()
        };
        visit(
            term,
            TermEntry {
                df,
                max_tf_bucket,
                postings,
                payload,
                ordinals,
            },
        )?;
        std::mem::swap(&mut self.term, &mut self.scratch);
        self.ordinal += 1;
        Ok(Some(()))
    }

    fn read_index_term(
        &mut self,
        len: usize,
        checkpoint: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        if len > self.max_term_bytes {
            return Err(Error::Corrupt("maintenance term limit"));
        }
        self.scratch.clear();
        self.scratch.reserve_exact(len);
        let input = &mut self.index;
        for _ in 0..len {
            checkpoint()?;
            self.scratch.push(input.byte()?);
        }
        Ok(())
    }

    fn extent(data: &mut Window<'a, S>, format: Format, end: &mut u64) -> Result<Extent> {
        let encoded = data.varint()?;
        let offset = if format >= Format::Lsg3 {
            let gap = ((encoded >> 1) as i64) ^ -((encoded & 1) as i64);
            end.checked_add_signed(gap)
                .ok_or(Error::Corrupt("dictionary extent gap"))?
        } else {
            encoded
        };
        let len = data.u32()?;
        *end = offset
            .checked_add(u64::from(len))
            .ok_or(Error::Corrupt("dictionary extent overflow"))?;
        Ok(Extent { offset, len })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictionary::{DictionaryBuilder, OwnedDictionary};
    use std::cell::Cell;

    fn build(format: Format, count: u32) -> Vec<u8> {
        let mut builder = DictionaryBuilder::with_format(format);
        for i in 0..count {
            builder
                .push(
                    &format!("café{i:06}"),
                    TermEntry {
                        df: i + 1,
                        max_tf_bucket: (i % 16) as u8,
                        postings: Extent {
                            offset: u64::from(count - i) * 3,
                            len: 3,
                        },
                        payload: Extent {
                            offset: u64::from(i) * 7,
                            len: 7,
                        },
                        ordinals: Default::default(),
                    },
                )
                .unwrap();
        }
        builder.finish()
    }
    fn validate(bytes: &[u8], format: Format) -> Result<()> {
        let mut cursor = DictionaryCursor::new(bytes, 0, bytes.len() as u64, format, 3, 128)?;
        while cursor.next_with(|| Ok(()), |_, _| Ok(()))?.is_some() {}
        Ok(())
    }

    #[test]
    fn matches_query_reader_across_formats_blocks_and_windows() {
        for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] {
            for count in [0, 1, 63, 64, 65, 129] {
                let bytes = build(format, count);
                let expected: Vec<_> = OwnedDictionary::parse_format(&bytes, format)
                    .unwrap()
                    .view()
                    .iter()
                    .collect::<Result<_>>()
                    .unwrap();
                for window in [1, 2, 7, 113] {
                    let mut cursor =
                        DictionaryCursor::new(&bytes, 0, bytes.len() as u64, format, window, 128)
                            .unwrap();
                    let mut actual = Vec::new();
                    while cursor
                        .next_with(
                            || Ok(()),
                            |term, entry| {
                                actual.push((term.to_owned(), entry));
                                Ok(())
                            },
                        )
                        .unwrap()
                        .is_some()
                    {
                        assert!(cursor.retained_bytes() <= 2 * window + 256);
                    }
                    assert_eq!(actual, expected);
                    assert_eq!(cursor.retained_bytes(), 0);
                }
            }
        }
    }

    #[test]
    fn rejects_truncation_trailing_bytes_wrong_index_and_term_limits() {
        for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] {
            let bytes = build(format, 65);
            for cut in 0..bytes.len() {
                assert!(validate(&bytes[..cut], format).is_err(), "cut {cut}");
            }
            let mut extra = bytes.clone();
            extra.push(0);
            assert!(validate(&extra, format).is_err());
            // Header values all fit in one byte for this fixture. Offset of first block must be zero.
            let mut bad = bytes.clone();
            bad[3] = 1;
            assert!(validate(&bad, format).is_err());
            let mut bad = bytes.clone();
            bad[5] = b'x';
            assert!(validate(&bad, format).is_err());
            let mut cursor =
                DictionaryCursor::new(&bytes, 0, bytes.len() as u64, format, 1, 2).unwrap();
            assert!(cursor.next_with(|| Ok(()), |_, _| Ok(())).is_err());
            assert!(cursor.next_with(|| Ok(()), |_, _| Ok(())).is_err());
        }
    }

    #[test]
    fn cancellation_and_callback_failures_poison_cursor() {
        let bytes = build(Format::Lsg3, 65);
        for stop in 0..30 {
            let mut cursor =
                DictionaryCursor::new(&bytes, 0, bytes.len() as u64, Format::Lsg3, 2, 128).unwrap();
            let mut calls = 0;
            let mut checkpoint = || {
                calls += 1;
                if calls > stop {
                    Err(Error::Corrupt("cancelled"))
                } else {
                    Ok(())
                }
            };
            while cursor.next_with(&mut checkpoint, |_, _| Ok(())).is_ok() {}
            assert!(cursor.next_with(|| Ok(()), |_, _| Ok(())).is_err());
        }
        let mut cursor =
            DictionaryCursor::new(&bytes, 0, bytes.len() as u64, Format::Lsg3, 2, 128).unwrap();
        assert!(
            cursor
                .next_with(|| Ok(()), |_, _| Err(Error::Corrupt("callback")))
                .is_err()
        );
        assert!(cursor.next_with(|| Ok(()), |_, _| Ok(())).is_err());
    }

    #[test]
    fn large_dictionary_does_not_retain_index_or_prior_blocks() {
        struct Tracked {
            bytes: Vec<u8>,
            max: Cell<usize>,
        }
        impl Source for Tracked {
            fn len(&self) -> u64 {
                self.bytes.len() as u64
            }
            fn read(&self, at: u64, len: usize) -> Result<Vec<u8>> {
                self.max.set(self.max.get().max(len));
                self.bytes
                    .get(at as usize..at as usize + len)
                    .map(|s| s.to_vec())
                    .ok_or(Error::Truncated)
            }
        }
        let source = Tracked {
            bytes: build(Format::Lsg3, 100_000),
            max: Cell::new(0),
        };
        let mut cursor =
            DictionaryCursor::new(&source, 0, source.len(), Format::Lsg3, 113, 128).unwrap();
        let mut count = 0;
        while cursor
            .next_with(|| Ok(()), |_, _| Ok(()))
            .unwrap()
            .is_some()
        {
            count += 1;
            assert!(cursor.retained_bytes() <= 226 + 256);
        }
        assert_eq!(count, 100_000);
        assert!(source.max.get() <= 113);
    }

    #[test]
    fn rejects_bad_extents_utf8_and_short_sources() {
        let bytes = build(Format::Lsg3, 1);
        assert!(DictionaryCursor::new(&bytes, u64::MAX, 2, Format::Lsg3, 1, 128).is_err());
        assert!(
            DictionaryCursor::new(&bytes, 0, bytes.len() as u64, Format::Lsg3, 0, 128).is_err()
        );
        let mut invalid = bytes.clone();
        // Change both copies of the first term so index agreement still holds.
        let first = invalid.iter().position(|b| *b == b'c').unwrap();
        let second = invalid.iter().rposition(|b| *b == b'c').unwrap();
        invalid[first] = 255;
        invalid[second] = 255;
        assert!(validate(&invalid, Format::Lsg3).is_err());
        let mut builder = DictionaryBuilder::with_format(Format::Lsg1);
        builder
            .push(
                "a",
                TermEntry {
                    df: 1,
                    max_tf_bucket: 0,
                    postings: Extent {
                        offset: u64::MAX,
                        len: 1,
                    },
                    payload: Extent::default(),
                    ordinals: Default::default(),
                },
            )
            .unwrap();
        assert!(validate(&builder.finish(), Format::Lsg1).is_err());
        struct Short;
        impl Source for Short {
            fn len(&self) -> u64 {
                100
            }
            fn read(&self, _: u64, _: usize) -> Result<Vec<u8>> {
                Ok(vec![])
            }
        }
        assert!(DictionaryCursor::new(&Short, 0, 100, Format::Lsg3, 3, 128).is_err());
    }

    proptest::proptest! {
        #[test]
        fn arbitrary_bytes_do_not_panic(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512)) {
            for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] { let _ = validate(&bytes, format); }
        }
    }
}
