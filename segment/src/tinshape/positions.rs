// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A term's positions in TIN's shape (page layout 15): STN3's positions
//! stream ([`crate::payload`]) in blocks of [`SKIP_INTERVAL`] entries, each
//! block's numbers bit-packed at its own widths, so that any entry is read
//! without decoding the entries before it (TIN bit-packs its positions'
//! deltas per block too).
//!
//! ```text
//! positions := count varint, skip u32le * slots, block*
//! slots     := blocks - 1 (skip i: block i + 1's offset from block 0)
//! block     := widths u16le, [wc u8], plane * wc, firsts, gaps
//! widths    := wf | wd << 6 | min(wc, 15) << 12 (wc in the next byte
//!              when 15 or more)
//! plane     := ceil(m / 8) bytes: bit i of plane b is bit b of entry i's
//!              count of positions less one
//! firsts    := each entry's first position at wf bits
//! gaps      := each entry's later positions as (delta - 1), entry after
//!              entry, at wd bits
//! ```
//!
//! A block holds `m` entries: [`SKIP_INTERVAL`], fewer in the last one.
//! Packed values are [`super::bits`]' (least significant bit first), and
//! `firsts` and `gaps` each end on a byte.
//!
//! Entry `i`'s count is one more than its planes' bits `i`, and its gaps
//! start at the sum of the counts less one before it: per plane `b`, the
//! set bits below `i` times `2^b`. So an entry is found by a few
//! population counts, its first position by one read at `i * wf` bits and
//! its gaps by reads from there: no varint of another entry is decoded. Most
//! entries hold one position (`wc` is then often 0 or 1), which costs a bit
//! or none.

use super::bits::{self, BitWriter, packed_len};
use super::blob::Bytes;
use crate::payload::{PayloadBuilder, SKIP_INTERVAL};
use crate::{Error, Result, varint};

/// The count width a block's widths say in a byte of its own.
const WIDE: u32 = 15;

/// Bits needed for `x`.
fn width(x: u32) -> u32 {
    32 - x.leading_zeros()
}

/// The stream of a term in this shape, from its [`crate::payload`] stream.
pub fn encode(stream: &[u8]) -> Result<Vec<u8>> {
    let mut at = 0;
    let count = varint::get_u32(stream, &mut at)?;
    let blocks = count.div_ceil(SKIP_INTERVAL) as usize;
    let slots = blocks.saturating_sub(1);
    let data = stream.get(at + slots * 4..).ok_or(Error::Truncated)?;
    let mut out = Vec::with_capacity(data.len() + slots * 4 + 5);
    varint::put(&mut out, u64::from(count));
    let skips_at = out.len();
    out.resize(skips_at + slots * 4, 0);
    let data_at = out.len();
    let (mut counts, mut firsts, mut gaps) = (Vec::new(), Vec::new(), Vec::new());
    let mut p = 0;
    for b in 0..blocks {
        let m = (count - b as u32 * SKIP_INTERVAL).min(SKIP_INTERVAL);
        counts.clear();
        firsts.clear();
        gaps.clear();
        for _ in 0..m {
            let n = varint::get_u32(data, &mut p)?;
            if n == 0 {
                return Err(Error::Corrupt("positions entry"));
            }
            counts.push(n - 1);
            firsts.push(varint::get_u32(data, &mut p)?);
            for _ in 1..n {
                gaps.push(varint::get_u32(data, &mut p)?);
            }
        }
        if b > 0 {
            let skip = u32::try_from(out.len() - data_at)
                .map_err(|_| Error::Corrupt("positions length"))?;
            let s = skips_at + (b - 1) * 4;
            out[s..s + 4].copy_from_slice(&skip.to_le_bytes());
        }
        let most = |v: &[u32]| width(v.iter().copied().max().unwrap_or(0));
        let (wc, wf, wd) = (most(&counts), most(&firsts), most(&gaps));
        let head = wf | wd << 6 | wc.min(WIDE) << 12;
        out.extend_from_slice(&(head as u16).to_le_bytes());
        if wc >= WIDE {
            out.push(wc as u8);
        }
        let plane_len = m.div_ceil(8) as usize;
        for bit in 0..wc {
            let plane = counts
                .iter()
                .enumerate()
                .fold(0u32, |p, (i, c)| p | (c >> bit & 1) << i);
            out.extend_from_slice(&plane.to_le_bytes()[..plane_len]);
        }
        for (values, w) in [(&firsts, wf), (&gaps, wd)] {
            let mut writer = BitWriter::new();
            values.iter().for_each(|v| writer.put(*v, w));
            out.extend_from_slice(&writer.finish());
        }
    }
    if p != data.len() {
        return Err(Error::Corrupt("positions stream"));
    }
    Ok(out)
}

/// A term's positions stream in this shape, parsed. Its bytes may load on
/// demand ([`Bytes::Lazy`]): every read asks for the bytes it needs, so
/// checking a few candidates of a common word reads a few pages of its
/// stream, not all of it.
#[derive(Clone, Copy, Debug)]
pub struct Positions<'a> {
    pub bytes: Bytes<'a>,
    pub count: u32,
    skips_at: usize,
    pub data_at: usize,
}

/// A block of entries, parsed: where its parts lie in
/// [`Positions::bytes`], its widths and its count planes.
#[derive(Clone, Copy, Debug)]
pub struct Block {
    /// The block's number: it holds entries from `index * SKIP_INTERVAL`.
    pub index: u32,
    pub entries: u32,
    /// Where the block, its firsts and its gaps start.
    pub at: usize,
    pub firsts_at: usize,
    pub gaps_at: usize,
    /// Where its skip lies, if it has one.
    pub skip_at: Option<usize>,
    first_width: u32,
    gap_width: u32,
    count_width: u32,
    planes: [u32; 32],
}

impl Block {
    /// Its widths and planes, as `(offset, length)`.
    pub fn head_bytes(&self) -> (usize, usize) {
        (self.at, self.firsts_at - self.at)
    }
}

/// An entry located in its block: its count and first position, and where
/// its other positions lie.
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub count: u32,
    pub first: u32,
    first_at: usize,
    first_len: usize,
    /// Its gaps' width, and their place among the block's.
    pub width: u32,
    gaps_at: usize,
    gap: usize,
}

impl Entry {
    /// The bytes its first position lies in, as `(offset, length)`.
    pub fn first_bytes(&self) -> (usize, usize) {
        (self.first_at, self.first_len)
    }

    /// Values before its first gap in the bytes [`Self::gap_bytes`] gives
    /// (they start on the block's eight-value boundary at or before it).
    pub fn lead(&self) -> usize {
        self.gap % 8
    }

    /// Where its gaps lie: from the byte value `gap & !7` starts on (eight
    /// values are `width` whole bytes) to the byte its last gap ends in.
    pub fn gap_bytes(&self) -> (usize, usize) {
        let from = self.gaps_at + self.gap / 8 * self.width as usize;
        let last = self.gap + self.count as usize - 1;
        (from, self.gaps_at + packed_len(last, self.width))
    }

    /// Its positions into `out`, from `gaps`: the bytes from
    /// [`Self::gap_bytes`]'s start (at least to its end).
    pub fn decode(&self, gaps: &[u8], out: &mut Vec<u32>) -> Result<()> {
        out.clear();
        out.reserve(self.count as usize);
        let mut p = self.first;
        out.push(p);
        let lead = self.lead();
        for i in 0..self.count as usize - 1 {
            let gap = bits::get(gaps, lead + i, self.width)?;
            p = p
                .checked_add(gap)
                .and_then(|p| p.checked_add(1))
                .ok_or(Error::Corrupt("positions"))?;
            out.push(p);
        }
        Ok(())
    }
}

impl<'a> Positions<'a> {
    pub fn parse(bytes: impl Into<Bytes<'a>>) -> Result<Self> {
        let bytes = bytes.into();
        let mut at = 0;
        let count = varint::get_u32(bytes.window(0, 5)?, &mut at)?;
        let slots = count.div_ceil(SKIP_INTERVAL).saturating_sub(1) as usize;
        let data_at = at + slots * 4;
        if data_at > bytes.len() {
            return Err(Error::Truncated);
        }
        Ok(Self {
            bytes,
            count,
            skips_at: at,
            data_at,
        })
    }

    /// The block holding entry `index`.
    #[inline]
    pub fn block_of(&self, index: u32) -> Result<Block> {
        if index >= self.count {
            return Err(Error::Corrupt("positions index"));
        }
        self.block(index / SKIP_INTERVAL)
    }

    /// Block `b`, parsed (`b` below the block count).
    #[inline]
    pub fn block(&self, b: u32) -> Result<Block> {
        let first = b * SKIP_INTERVAL;
        debug_assert!(first < self.count);
        let mut at = self.data_at;
        let mut skip_at = None;
        if b > 0 {
            let s = self.skips_at + (b as usize - 1) * 4;
            skip_at = Some(s);
            let skip = self.bytes.get(s, s + 4)?;
            at += u32::from_le_bytes(skip.try_into().expect("four bytes")) as usize;
        }
        let head = self.bytes.window(at, 3)?;
        let widths = u16::from_le_bytes(
            head.get(..2)
                .ok_or(Error::Truncated)?
                .try_into()
                .expect("two bytes"),
        );
        let (first_width, gap_width) = (u32::from(widths & 63), u32::from(widths >> 6 & 63));
        let mut count_width = u32::from(widths >> 12);
        let mut planes_at = at + 2;
        if count_width == WIDE {
            count_width = u32::from(*head.get(2).ok_or(Error::Truncated)?);
            planes_at += 1;
        }
        if first_width > 32 || gap_width > 32 || count_width > 32 {
            return Err(Error::Corrupt("positions widths"));
        }
        let entries = (self.count - first).min(SKIP_INTERVAL);
        let plane_len = entries.div_ceil(8) as usize;
        let firsts_at = planes_at + count_width as usize * plane_len;
        let mut planes = [0u32; 32];
        if count_width > 0 {
            let bytes = self.bytes.get(planes_at, firsts_at)?;
            for (plane, b) in planes.iter_mut().zip(bytes.chunks_exact(plane_len)) {
                let mut word = [0u8; 4];
                word[..plane_len].copy_from_slice(b);
                *plane = u32::from_le_bytes(word);
            }
        }
        Ok(Block {
            index: b,
            entries,
            at,
            firsts_at,
            gaps_at: firsts_at + packed_len(entries as usize, first_width),
            skip_at,
            first_width,
            gap_width,
            count_width,
            planes,
        })
    }

    /// Entry `index`, of `block`, located: its count and first position
    /// read.
    #[inline]
    pub fn entry(&self, block: &Block, index: u32) -> Result<Entry> {
        let i = index.wrapping_sub(block.index * SKIP_INTERVAL);
        if i >= block.entries {
            return Err(Error::Corrupt("positions index"));
        }
        let below = (1u32 << i) - 1;
        let (mut extra, mut gap) = (0u32, 0usize);
        for (b, plane) in block.planes[..block.count_width as usize]
            .iter()
            .enumerate()
        {
            extra |= (plane >> i & 1) << b;
            gap += ((plane & below).count_ones() as usize) << b;
        }
        let bit = i as usize * block.first_width as usize;
        let first_at = block.firsts_at + bit / 8;
        let first = bits::get_at(self.bytes.window(first_at, 8)?, bit % 8, block.first_width)?;
        Ok(Entry {
            count: extra
                .checked_add(1)
                .ok_or(Error::Corrupt("positions count"))?,
            first,
            first_at,
            first_len: (bit % 8 + block.first_width as usize).div_ceil(8),
            width: block.gap_width,
            gaps_at: block.gaps_at,
            gap,
        })
    }

    /// Decodes entry `index` into `out`.
    pub fn read_entry(&self, index: u32, out: &mut Vec<u32>) -> Result<()> {
        let block = self.block_of(index)?;
        let entry = self.entry(&block, index)?;
        let (from, to) = entry.gap_bytes();
        entry.decode(self.bytes.get(from, to)?, out)
    }

    /// The [`crate::payload`] stream it was encoded from.
    pub fn payload(&self) -> Result<Vec<u8>> {
        let mut builder = PayloadBuilder::default();
        let mut out = Vec::new();
        for b in 0..self.count.div_ceil(SKIP_INTERVAL) {
            let block = self.block(b)?;
            for index in b * SKIP_INTERVAL..b * SKIP_INTERVAL + block.entries {
                let entry = self.entry(&block, index)?;
                let (from, to) = entry.gap_bytes();
                entry.decode(self.bytes.get(from, to)?, &mut out)?;
                builder.push(&out)?;
            }
        }
        Ok(builder.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn stream(entries: &[Vec<u32>]) -> Vec<u8> {
        let mut b = PayloadBuilder::default();
        for e in entries {
            b.push(e).unwrap();
        }
        b.finish()
    }

    /// Every entry read alone, in any order, and the stream back.
    fn check(entries: &[Vec<u32>]) -> Vec<u8> {
        let original = stream(entries);
        let encoded = encode(&original).unwrap();
        let positions = Positions::parse(&encoded[..]).unwrap();
        assert_eq!(positions.count, entries.len() as u32);
        assert_eq!(positions.payload().unwrap(), original);
        let mut out = Vec::new();
        for index in (0..entries.len() as u32).rev() {
            positions.read_entry(index, &mut out).unwrap();
            assert_eq!(out, entries[index as usize], "entry {index}");
        }
        assert!(positions.block_of(entries.len() as u32).is_err());
        encoded
    }

    #[test]
    fn empty_and_single() {
        check(&[]);
        check(&[vec![0]]);
        check(&[vec![u32::MAX]]);
        check(&[vec![0, u32::MAX]]);
        check(&[vec![7, 8, 9, 10]]);
    }

    #[test]
    fn every_block_shape() {
        for count in [1u32, 31, 32, 33, 64, 100, 1000] {
            for spread in [1u32, 2, 7] {
                let entries: Vec<Vec<u32>> = (0..count)
                    .map(|i| {
                        let n = if i % spread == 0 { i % 9 + 1 } else { 1 };
                        (0..n).map(|p| p * (i % 300 + 1) + i % 5).collect()
                    })
                    .collect();
                check(&entries);
            }
        }
    }

    #[test]
    fn wide_counts_and_positions() {
        // An entry of 70,000 positions (count width 17, in a byte of its
        // own) among single ones; gaps of every width.
        let mut entries: Vec<Vec<u32>> = (0..40).map(|i| vec![i * 1000]).collect();
        entries[5] = (0..70_000).collect();
        entries[33] = (0..33)
            .map(|b| if b == 32 { u32::MAX } else { (1u32 << b) - 1 })
            .collect();
        let encoded = check(&entries);
        let positions = Positions::parse(&encoded[..]).unwrap();
        assert_eq!(positions.block(0).unwrap().count_width, 17);
    }

    #[test]
    fn single_positions_cost_no_count() {
        let entries: Vec<Vec<u32>> = (0..32).map(|i| vec![i]).collect();
        let encoded = check(&entries);
        // Count 1 + widths 2 + 32 firsts of 5 bits.
        assert_eq!(encoded.len(), 1 + 2 + 20);
    }

    /// Read in place from pages far smaller than its blocks, every entry is
    /// decoded as from memory (reads across a page's end are stitched).
    #[test]
    fn reads_in_place_across_page_ends() {
        use super::super::blob::{LazyBlob, PinningSource};
        let entries: Vec<Vec<u32>> = (0..500u32)
            .map(|i| {
                let n = match i % 9 {
                    0 => 40,
                    1 | 2 => 3,
                    _ => 1,
                };
                (0..n).map(|p| p * (i % 500 + 1) + i % 3).collect()
            })
            .collect();
        let encoded = encode(&stream(&entries)).unwrap();
        for page_len in [7, 64, 1000] {
            let blob = LazyBlob::new(Box::new(PinningSource::new(encoded.clone(), page_len)));
            blob.open_span();
            let positions = Positions::parse(blob.bytes()).unwrap();
            let mut out = Vec::new();
            for index in 0..entries.len() as u32 {
                positions.read_entry(index, &mut out).unwrap();
                assert_eq!(
                    out, entries[index as usize],
                    "{index} in pages of {page_len}"
                );
            }
            // SAFETY: no slice of the span is used past here.
            unsafe { blob.close_span() };
        }
    }

    #[test]
    fn rejects_corrupt_widths() {
        let mut encoded = encode(&stream(&[vec![3, 9]])).unwrap();
        encoded[1] = 63;
        assert!(
            Positions::parse(&encoded[..])
                .unwrap()
                .read_entry(0, &mut Vec::new())
                .is_err()
        );
    }

    fn entries() -> impl Strategy<Value = Vec<Vec<u32>>> {
        let entry = (
            prop_oneof![Just(1usize), 1usize..4, 1usize..80],
            prop_oneof![Just(1u32), 1u32..16, 1u32..2000, 1u32..1 << 30],
            0u32..5000,
        )
            .prop_map(|(n, step, first)| {
                let mut p = first;
                let mut out = vec![p];
                for k in 1..n {
                    let gap = 1 + (k as u32).wrapping_mul(2_654_435_761) % step;
                    match p.checked_add(gap) {
                        Some(next) => p = next,
                        None => break,
                    }
                    out.push(p);
                }
                out
            });
        proptest::collection::vec(entry, 0..200)
    }

    proptest! {
        #[test]
        fn round_trips(entries in entries()) {
            check(&entries);
        }
    }
}
