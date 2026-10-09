// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A term's positions in TIN's shape: STN3's positions stream
//! ([`crate::payload`]), entries in posting order with a skip every
//! [`SKIP_INTERVAL`] entries, and for a term in at least one document in
//! [`SUB_SHARE`] (and of at least [`SUB_MIN`] postings) a finer table, an
//! offset every [`SUB_INTERVAL`] entries.
//!
//! ```text
//! positions := count << 1 | subs varint, skip u32le * slots,
//!              [sub u16le * (SKIP_INTERVAL / SUB_INTERVAL - 1) * blocks],
//!              data
//! slots     := ceil(count / SKIP_INTERVAL) - 1 (0 for an empty stream)
//! blocks    := ceil(count / SKIP_INTERVAL), the subs present when the
//!              head's low bit is set
//! ```
//!
//! Sub `j` of block `b` is the offset of entry
//! `b * SKIP_INTERVAL + (j + 1) * SUB_INTERVAL` from the start of entry
//! `b * SKIP_INTERVAL`, or `0xffff` when there is no such entry or it lies
//! too far. A phrase checks the
//! positions of a few candidates of a common word, far apart in its posting
//! order: from a skip it decoded up to 31 entries to reach one, from a sub
//! at most 7.

use crate::payload::SKIP_INTERVAL;
use crate::{Error, Result, varint};

/// Entries between two subs.
pub const SUB_INTERVAL: u32 = 8;

/// A term in at least one document in this many carries subs.
pub const SUB_SHARE: u32 = 16;

/// Postings a term needs for its positions to carry subs, whatever its
/// share.
pub const SUB_MIN: u32 = 4096;

/// Subs per block of [`SKIP_INTERVAL`] entries.
const SUBS: usize = (SKIP_INTERVAL / SUB_INTERVAL) as usize - 1;

const NO_SUB: u16 = u16::MAX;

/// Skips one entry from byte `p`: its count, then that many varints (each
/// ends at a byte below 0x80, counted eight bytes at a time).
#[inline]
pub fn skip_entry(bytes: &[u8], p: usize) -> Result<usize> {
    let mut p = p;
    let mut n = varint::get_u32(bytes, &mut p)?;
    while n > 0 {
        if let Some(chunk) = bytes.get(p..p + 8) {
            let word = u64::from_le_bytes(chunk.try_into().expect("eight bytes"));
            let mut ends = !word & 0x8080_8080_8080_8080;
            let c = ends.count_ones();
            if c < n {
                n -= c;
                p += 8;
                continue;
            }
            for _ in 1..n {
                ends &= ends - 1;
            }
            return Ok(p + ends.trailing_zeros() as usize / 8 + 1);
        }
        let b = *bytes.get(p).ok_or(Error::Truncated)?;
        p += 1;
        if b < 0x80 {
            n -= 1;
        }
    }
    Ok(p)
}

/// The stream of a term in this shape, from its [`crate::payload`] stream,
/// in a segment of `documents` documents.
pub fn encode(stream: &[u8], documents: u32) -> Result<Vec<u8>> {
    let mut at = 0;
    let count = varint::get_u32(stream, &mut at)?;
    if count < SUB_MIN || u64::from(count) * u64::from(SUB_SHARE) < u64::from(documents) {
        let mut out = Vec::with_capacity(stream.len() + 1);
        varint::put(&mut out, u64::from(count) << 1);
        out.extend_from_slice(&stream[at..]);
        return Ok(out);
    }
    let slots = (count.div_ceil(SKIP_INTERVAL) - 1) as usize;
    let data_at = at + slots * 4;
    let data = stream.get(data_at..).ok_or(Error::Truncated)?;
    let blocks = count.div_ceil(SKIP_INTERVAL) as usize;
    let mut subs = Vec::with_capacity(blocks * SUBS * 2);
    let mut p = 0usize;
    let mut block_at = 0usize;
    for entry in 0..count {
        let within = entry % SKIP_INTERVAL;
        if within == 0 {
            block_at = p;
        } else if within.is_multiple_of(SUB_INTERVAL) {
            let sub = u16::try_from(p - block_at)
                .ok()
                .filter(|s| *s != NO_SUB)
                .unwrap_or(NO_SUB);
            subs.extend_from_slice(&sub.to_le_bytes());
        }
        p = skip_entry(data, p)?;
        if within == SKIP_INTERVAL - 1 || entry + 1 == count {
            // Pad the block's missing subs.
            let written = (within / SUB_INTERVAL) as usize;
            for _ in written..SUBS {
                subs.extend_from_slice(&NO_SUB.to_le_bytes());
            }
        }
    }
    if p != data.len() || subs.len() != blocks * SUBS * 2 {
        return Err(Error::Corrupt("positions stream"));
    }
    let mut out = Vec::with_capacity(stream.len() + subs.len() + 1);
    varint::put(&mut out, u64::from(count) << 1 | 1);
    out.extend_from_slice(&stream[at..data_at]);
    out.extend_from_slice(&subs);
    out.extend_from_slice(data);
    Ok(out)
}

/// An entry the tables locate: its index, where it starts, and the table
/// bytes read to find it as `(offset, length)` pairs (length 0: none).
pub type Located = (u32, usize, [(usize, usize); 2]);

/// A term's positions stream in this shape, parsed.
#[derive(Clone, Copy, Debug)]
pub struct Positions<'a> {
    pub bytes: &'a [u8],
    pub count: u32,
    skips_at: usize,
    /// Where the subs start, when present.
    subs_at: Option<usize>,
    pub data_at: usize,
}

impl<'a> Positions<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let mut at = 0;
        let head = varint::get(bytes, &mut at)?;
        let count = u32::try_from(head >> 1).map_err(|_| Error::Corrupt("positions count"))?;
        let blocks = count.div_ceil(SKIP_INTERVAL) as usize;
        let slots = blocks.saturating_sub(1);
        let skips_at = at;
        let mut data_at = skips_at + slots * 4;
        let subs_at = (head & 1 == 1).then_some(data_at);
        if subs_at.is_some() {
            data_at += blocks * SUBS * 2;
        }
        if data_at > bytes.len() {
            return Err(Error::Truncated);
        }
        Ok(Self {
            bytes,
            count,
            skips_at,
            subs_at,
            data_at,
        })
    }

    /// The nearest entry at or before `index` the tables locate, and where
    /// it starts in [`Self::bytes`]; with the table bytes read, as
    /// `(offset, length)` pairs, for page accounting.
    #[inline]
    pub fn locate(&self, index: u32) -> Result<Located> {
        if index >= self.count {
            return Err(Error::Corrupt("positions index"));
        }
        let slot = (index / SKIP_INTERVAL) as usize;
        let mut reads = [(0, 0); 2];
        let mut entry = slot as u32 * SKIP_INTERVAL;
        let mut at = self.data_at;
        if slot > 0 {
            let s = self.skips_at + (slot - 1) * 4;
            reads[0] = (s, 4);
            let skip = self.bytes.get(s..s + 4).ok_or(Error::Truncated)?;
            at += u32::from_le_bytes(skip.try_into().expect("four bytes")) as usize;
        }
        let j = ((index % SKIP_INTERVAL) / SUB_INTERVAL) as usize;
        if j > 0
            && let Some(subs_at) = self.subs_at
        {
            let s = subs_at + (slot * SUBS + j - 1) * 2;
            reads[1] = (s, 2);
            let sub = u16::from_le_bytes(
                self.bytes
                    .get(s..s + 2)
                    .ok_or(Error::Truncated)?
                    .try_into()
                    .expect("two bytes"),
            );
            if sub != NO_SUB {
                entry += j as u32 * SUB_INTERVAL;
                at += usize::from(sub);
            }
        }
        Ok((entry, at, reads))
    }

    /// Decodes the entry at byte `at` into `out`; where the next starts.
    #[inline]
    pub fn read_entry(&self, at: usize, out: &mut Vec<u32>) -> Result<usize> {
        let mut p = at;
        let n = varint::get_u32(self.bytes, &mut p)?;
        out.clear();
        let mut previous: Option<u32> = None;
        for _ in 0..n {
            let v = varint::get_u32(self.bytes, &mut p)?;
            let position = match previous {
                None => v,
                Some(q) => q + v + 1,
            };
            out.push(position);
            previous = Some(position);
        }
        Ok(p)
    }

    /// The stream without its subs: the [`crate::payload`] stream it was
    /// encoded from.
    pub fn payload(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bytes.len());
        varint::put(&mut out, u64::from(self.count));
        let skips_end = self.subs_at.unwrap_or(self.data_at);
        out.extend_from_slice(&self.bytes[self.skips_at..skips_end]);
        out.extend_from_slice(&self.bytes[self.data_at..]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::PayloadBuilder;

    fn stream(entries: &[Vec<u32>]) -> Vec<u8> {
        let mut b = PayloadBuilder::default();
        for e in entries {
            b.push(e).unwrap();
        }
        b.finish()
    }

    #[test]
    fn locates_every_entry_with_and_without_subs() {
        for count in [1u32, 31, 32, 33, 100, SUB_MIN - 1, SUB_MIN, SUB_MIN + 37] {
            let entries: Vec<Vec<u32>> = (0..count)
                .map(|i| (0..(i % 7 + 1)).map(|p| p * (i % 300 + 1)).collect())
                .collect();
            let original = stream(&entries);
            // A share of one in 16 of `16 * SUB_MIN` documents.
            let encoded = encode(&original, SUB_SHARE * SUB_MIN).unwrap();
            let subs = count >= SUB_MIN;
            assert_eq!(encoded.len() > original.len() + 1, subs);
            // Too rare a share: no subs, whatever the count.
            let rare = encode(&original, SUB_SHARE * count + 1).unwrap();
            let rare = Positions::parse(&rare).unwrap();
            assert!(rare.subs_at.is_none());
            assert_eq!(rare.payload(), original);
            let positions = Positions::parse(&encoded).unwrap();
            assert_eq!(positions.payload(), original);
            let mut out = Vec::new();
            for index in (0..count).step_by(1 + count as usize / 500) {
                let (mut entry, mut at, _) = positions.locate(index).unwrap();
                assert!(entry <= index && index - entry < SKIP_INTERVAL);
                if count >= SUB_MIN {
                    assert!(index - entry < SUB_INTERVAL);
                }
                while entry < index {
                    at = skip_entry(positions.bytes, at).unwrap();
                    entry += 1;
                }
                positions.read_entry(at, &mut out).unwrap();
                assert_eq!(out, entries[index as usize]);
            }
            assert!(positions.locate(count).is_err());
        }
    }

    #[test]
    fn long_blocks_fall_back_to_skips() {
        // Entries of 3,000 two-byte positions: a block of 32 is far over
        // 64 KiB, so its later subs cannot be held.
        let entries: Vec<Vec<u32>> = (0..SUB_MIN + 64)
            .map(|i| {
                if i < 64 {
                    (0..3000).map(|p| p * 300).collect()
                } else {
                    vec![i]
                }
            })
            .collect();
        let encoded = encode(&stream(&entries), 0).unwrap();
        let positions = Positions::parse(&encoded).unwrap();
        let mut out = Vec::new();
        for index in [0, 7, 8, 9, 31, 33, 40, 63, 64, 100] {
            let (mut entry, mut at, _) = positions.locate(index).unwrap();
            while entry < index {
                at = skip_entry(positions.bytes, at).unwrap();
                entry += 1;
            }
            positions.read_entry(at, &mut out).unwrap();
            assert_eq!(out, entries[index as usize]);
        }
    }
}
