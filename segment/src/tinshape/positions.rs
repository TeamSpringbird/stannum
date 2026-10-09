// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A term's positions in TIN's shape: STN3's positions stream
//! ([`crate::payload`]), entries in posting order with a skip every
//! [`SKIP_INTERVAL`] entries, and two optional tables: for a term in at
//! least one document in [`SUB_SHARE`] (and of at least [`SUB_MIN`]
//! postings) an offset every [`SUB_INTERVAL`] entries, and for a term most
//! of whose documents hold it once a mask per block naming those entries,
//! whose count is then left out.
//!
//! ```text
//! positions := count << 2 | masks << 1 | subs varint, skip u32le * slots,
//!              [sub u16le * (SKIP_INTERVAL / SUB_INTERVAL - 1) * blocks],
//!              [mask u32le * blocks],
//!              entry*
//! entry     := [n varint], position varint * n
//!              positions: first absolute, then (delta - 1); n absent when
//!              the entry's mask bit says n = 1
//! slots     := ceil(count / SKIP_INTERVAL) - 1 (0 for an empty stream)
//! blocks    := ceil(count / SKIP_INTERVAL)
//! ```
//!
//! Sub `j` of block `b` is the offset of entry
//! `b * SKIP_INTERVAL + (j + 1) * SUB_INTERVAL` from the start of entry
//! `b * SKIP_INTERVAL`, or `0xffff` when there is no such entry or it lies
//! too far. A phrase checks the positions of a few candidates of a common
//! word, far apart in its posting order: from a skip it decoded up to 31
//! entries to reach one, from a sub at most 7. Bit `i` of block `b`'s mask
//! says entry `b * SKIP_INTERVAL + i` holds one position: a byte saved for
//! most postings, at a bit each.

use super::blob::Bytes;
use crate::payload::{PayloadBuilder, SKIP_INTERVAL};
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

/// Skips `n` varints from byte `p`, each ending at a byte below 0x80,
/// counted eight bytes at a time.
#[inline]
fn skip_varints(bytes: &[u8], mut p: usize, mut n: u32) -> Result<usize> {
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
    let blocks = count.div_ceil(SKIP_INTERVAL) as usize;
    let slots = blocks.saturating_sub(1);
    let data = stream.get(at + slots * 4..).ok_or(Error::Truncated)?;
    // Each entry's count and where its positions lie in `data`.
    let mut entries: Vec<(u32, usize, usize)> = Vec::with_capacity(count as usize);
    let mut p = 0usize;
    for _ in 0..count {
        let n = varint::get_u32(data, &mut p)?;
        let from = p;
        p = skip_varints(data, p, n)?;
        entries.push((n, from, p));
    }
    if p != data.len() {
        return Err(Error::Corrupt("positions stream"));
    }
    let ones = entries.iter().filter(|e| e.0 == 1).count();
    let masked = ones > 4 * blocks;
    let subbed =
        count >= SUB_MIN && u64::from(count) * u64::from(SUB_SHARE) >= u64::from(documents);
    let mut body = Vec::with_capacity(data.len());
    let mut skips = Vec::with_capacity(slots * 4);
    let mut subs = Vec::new();
    let mut masks = Vec::new();
    let mut block_at = 0usize;
    let mut mask = 0u32;
    for (i, (n, from, to)) in entries.iter().enumerate() {
        let within = i as u32 % SKIP_INTERVAL;
        if within == 0 {
            block_at = body.len();
            mask = 0;
            if i > 0 {
                let skip = u32::try_from(body.len()).map_err(|_| Error::Corrupt("positions"))?;
                skips.extend_from_slice(&skip.to_le_bytes());
            }
        } else if subbed && within.is_multiple_of(SUB_INTERVAL) {
            let sub = u16::try_from(body.len() - block_at)
                .ok()
                .filter(|s| *s != NO_SUB)
                .unwrap_or(NO_SUB);
            subs.extend_from_slice(&sub.to_le_bytes());
        }
        if masked && *n == 1 {
            mask |= 1 << within;
        } else {
            varint::put(&mut body, u64::from(*n));
        }
        body.extend_from_slice(&data[*from..*to]);
        if within == SKIP_INTERVAL - 1 || i + 1 == count as usize {
            if subbed {
                for _ in (within / SUB_INTERVAL) as usize..SUBS {
                    subs.extend_from_slice(&NO_SUB.to_le_bytes());
                }
            }
            masks.extend_from_slice(&mask.to_le_bytes());
        }
    }
    let mut out = Vec::with_capacity(body.len() + skips.len() + subs.len() + masks.len() + 5);
    varint::put(
        &mut out,
        u64::from(count) << 2 | u64::from(masked) << 1 | u64::from(subbed),
    );
    out.extend_from_slice(&skips);
    out.extend_from_slice(&subs);
    if masked {
        out.extend_from_slice(&masks);
    }
    out.extend_from_slice(&body);
    Ok(out)
}

/// An entry the tables locate: its index, where it starts, and the table
/// bytes read to find it as `(offset, length)` pairs (length 0: none).
pub type Located = (u32, usize, [(usize, usize); 2]);

/// A term's positions stream in this shape, parsed. Its bytes may load on
/// demand ([`Bytes::Lazy`]): every read asks for the bytes it needs, an
/// entry at a time, so checking a few candidates of a common word reads a
/// few pages of its stream, not all of it.
#[derive(Clone, Copy, Debug)]
pub struct Positions<'a> {
    pub bytes: Bytes<'a>,
    pub count: u32,
    skips_at: usize,
    /// Where the subs and the masks start, when present.
    subs_at: Option<usize>,
    masks_at: Option<usize>,
    pub data_at: usize,
}

/// Most bytes of a varint.
const VARINT_MAX: usize = 5;

impl<'a> Positions<'a> {
    pub fn parse(bytes: impl Into<Bytes<'a>>) -> Result<Self> {
        let bytes = bytes.into();
        let mut at = 0;
        let head = varint::get(bytes.window(0, 10)?, &mut at)?;
        let count = u32::try_from(head >> 2).map_err(|_| Error::Corrupt("positions count"))?;
        let blocks = count.div_ceil(SKIP_INTERVAL) as usize;
        let slots = blocks.saturating_sub(1);
        let skips_at = at;
        let mut data_at = skips_at + slots * 4;
        let subs_at = (head & 1 == 1).then_some(data_at);
        if subs_at.is_some() {
            data_at += blocks * SUBS * 2;
        }
        let masks_at = (head & 2 == 2).then_some(data_at);
        if masks_at.is_some() {
            data_at += blocks * 4;
        }
        if data_at > bytes.len() {
            return Err(Error::Truncated);
        }
        Ok(Self {
            bytes,
            count,
            skips_at,
            subs_at,
            masks_at,
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
            let skip = self.bytes.get(s, s + 4)?;
            at += u32::from_le_bytes(skip.try_into().expect("four bytes")) as usize;
        }
        let j = ((index % SKIP_INTERVAL) / SUB_INTERVAL) as usize;
        if j > 0
            && let Some(subs_at) = self.subs_at
        {
            let s = subs_at + (slot * SUBS + j - 1) * 2;
            reads[1] = (s, 2);
            let sub = u16::from_le_bytes(self.bytes.get(s, s + 2)?.try_into().expect("two bytes"));
            if sub != NO_SUB {
                entry += j as u32 * SUB_INTERVAL;
                at += usize::from(sub);
            }
        }
        Ok((entry, at, reads))
    }

    /// Where entry `index`'s mask lies, for page accounting.
    pub fn mask_at(&self, index: u32) -> Option<usize> {
        self.masks_at
            .map(|m| m + (index / SKIP_INTERVAL) as usize * 4)
    }

    /// The count of positions of entry `index`, starting at byte `p`, and
    /// where its positions start.
    #[inline]
    fn head(&self, index: u32, p: usize) -> Result<(u32, usize)> {
        if let Some(m) = self.masks_at {
            let at = m + (index / SKIP_INTERVAL) as usize * 4;
            let mask =
                u32::from_le_bytes(self.bytes.get(at, at + 4)?.try_into().expect("four bytes"));
            if mask >> (index % SKIP_INTERVAL) & 1 == 1 {
                return Ok((1, p));
            }
        }
        let window = self.bytes.window(p, VARINT_MAX)?;
        let mut q = 0;
        let n = varint::get_u32(window, &mut q)?;
        Ok((n, p + q))
    }

    /// The bytes from `p` that hold `n` varints: at most five each.
    #[inline]
    fn varints(&self, p: usize, n: u32) -> Result<&'a [u8]> {
        self.bytes
            .window(p, (n as usize).saturating_mul(VARINT_MAX))
    }

    /// Skips entry `index`, starting at byte `p`; where the next starts.
    #[inline]
    pub fn skip(&self, index: u32, p: usize) -> Result<usize> {
        let (n, p) = self.head(index, p)?;
        Ok(p + skip_varints(self.varints(p, n)?, 0, n)?)
    }

    /// Decodes entry `index`, starting at byte `at`, into `out`; where the
    /// next starts.
    #[inline]
    pub fn read_entry(&self, index: u32, at: usize, out: &mut Vec<u32>) -> Result<usize> {
        let (n, p) = self.head(index, at)?;
        let bytes = self.varints(p, n)?;
        let mut q = 0;
        out.clear();
        let mut previous: Option<u32> = None;
        for _ in 0..n {
            let v = varint::get_u32(bytes, &mut q)?;
            let position = match previous {
                None => v,
                Some(q) => q + v + 1,
            };
            out.push(position);
            previous = Some(position);
        }
        Ok(p + q)
    }

    /// The [`crate::payload`] stream it was encoded from.
    pub fn payload(&self) -> Result<Vec<u8>> {
        let mut builder = PayloadBuilder::default();
        let mut out = Vec::new();
        let mut p = self.data_at;
        for index in 0..self.count {
            p = self.read_entry(index, p, &mut out)?;
            builder.push(&out)?;
        }
        Ok(builder.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(entries: &[Vec<u32>]) -> Vec<u8> {
        let mut b = PayloadBuilder::default();
        for e in entries {
            b.push(e).unwrap();
        }
        b.finish()
    }

    fn check(entries: &[Vec<u32>], documents: u32) -> Positions<'static> {
        let original = stream(entries);
        let encoded: &'static [u8] =
            Box::leak(encode(&original, documents).unwrap().into_boxed_slice());
        let positions = Positions::parse(encoded).unwrap();
        assert_eq!(positions.payload().unwrap(), original);
        let count = entries.len() as u32;
        let mut out = Vec::new();
        for index in (0..count).step_by(1 + count as usize / 500) {
            let (mut entry, mut at, _) = positions.locate(index).unwrap();
            assert!(entry <= index && index - entry < SKIP_INTERVAL);
            while entry < index {
                at = positions.skip(entry, at).unwrap();
                entry += 1;
            }
            positions.read_entry(index, at, &mut out).unwrap();
            assert_eq!(out, entries[index as usize]);
        }
        assert!(positions.locate(count).is_err());
        positions
    }

    #[test]
    fn locates_every_entry_with_and_without_tables() {
        for count in [1u32, 31, 32, 33, 100, SUB_MIN - 1, SUB_MIN, SUB_MIN + 37] {
            // Mostly single positions (masked), and mostly several (not).
            for spread in [7u32, 2] {
                let entries: Vec<Vec<u32>> = (0..count)
                    .map(|i| {
                        let n = if i % spread == 0 { i % 7 + 2 } else { 1 };
                        (0..n).map(|p| p * (i % 300 + 1)).collect()
                    })
                    .collect();
                let positions = check(&entries, SUB_SHARE * SUB_MIN);
                assert_eq!(positions.subs_at.is_some(), count >= SUB_MIN);
                let ones = entries.iter().filter(|e| e.len() == 1).count();
                let blocks = count.div_ceil(SKIP_INTERVAL) as usize;
                assert_eq!(positions.masks_at.is_some(), ones > 4 * blocks);
                // Too rare a share for subs, whatever the count.
                let rare = check(&entries, SUB_SHARE * count + 1);
                assert!(rare.subs_at.is_none());
            }
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
        check(&entries, 0);
    }
}
