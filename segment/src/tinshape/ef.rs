// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Elias-Fano coding of a strictly increasing list of `n` values below a
//! universe `u`: each value's low `l = floor(log2(u / n))` bits packed, then
//! its high part in unary, as bit `(value >> l) + i` of a bitmap.
//!
//! ```text
//! ef := lows  (n values of l bits, packed, padded to a byte)
//!       highs (n + ((u - 1) >> l) bits, padded to a byte)
//! ```
//!
//! `n` and `u` are known to the reader from elsewhere, so the encoding has
//! no header: it costs about `2 + log2(u / n)` bits per value.

use super::bits::{self, BitWriter};
use crate::{Error, Result};

/// The low-bit width for `n` values below `universe`.
pub fn low_width(n: usize, universe: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    let quotient = u64::from(universe) / n as u64;
    if quotient <= 1 {
        0
    } else {
        63 - quotient.leading_zeros()
    }
}

fn high_bits(n: usize, universe: u32, low: u32) -> usize {
    if n == 0 {
        return 0;
    }
    n + ((universe.max(1) - 1) >> low) as usize
}

/// Bytes the encoding of `n` values below `universe` takes.
pub fn encoded_len(n: usize, universe: u32) -> usize {
    let low = low_width(n, universe);
    bits::packed_len(n, low) + high_bits(n, universe, low).div_ceil(8)
}

/// Appends the encoding of `values`, strictly increasing and below
/// `universe`.
pub fn encode(values: &[u32], universe: u32, out: &mut Vec<u8>) {
    let n = values.len();
    if n == 0 {
        return;
    }
    debug_assert!(values.windows(2).all(|w| w[0] < w[1]));
    debug_assert!(*values.last().unwrap() < universe);
    let low = low_width(n, universe);
    let mask = if low == 0 { 0 } else { (1u32 << low) - 1 };
    let mut lows = BitWriter::new();
    for v in values {
        lows.put(v & mask, low);
    }
    out.extend_from_slice(&lows.finish());
    let mut highs = vec![0u8; high_bits(n, universe, low).div_ceil(8)];
    for (i, v) in values.iter().enumerate() {
        let at = (v >> low) as usize + i;
        highs[at / 8] |= 1 << (at % 8);
    }
    out.extend_from_slice(&highs);
}

/// An encoded list, borrowed.
#[derive(Clone, Copy, Debug)]
pub struct Ef<'a> {
    lows: &'a [u8],
    highs: &'a [u8],
    n: usize,
    low: u32,
}

impl<'a> Ef<'a> {
    /// The list of `n` values below `universe` at the start of `bytes`.
    pub fn parse(bytes: &'a [u8], n: usize, universe: u32) -> Result<Self> {
        let low = low_width(n, universe);
        let lows_len = bits::packed_len(n, low);
        let highs_len = high_bits(n, universe, low).div_ceil(8);
        if bytes.len() < lows_len + highs_len {
            return Err(Error::Truncated);
        }
        Ok(Self {
            lows: &bytes[..lows_len],
            highs: &bytes[lows_len..lows_len + highs_len],
            n,
            low,
        })
    }

    pub const fn len(&self) -> usize {
        self.n
    }

    pub const fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Calls `visit` with every value in order.
    #[inline]
    pub fn for_each(&self, mut visit: impl FnMut(u32)) {
        let mut i = 0usize;
        let words = self.highs.len().div_ceil(8);
        let mask = if self.low == 0 {
            0
        } else {
            (1u64 << self.low) - 1
        };
        for w in 0..words {
            let mut word = bits::word(self.highs, w);
            while word != 0 {
                if i >= self.n {
                    return;
                }
                let at = w * 64 + word.trailing_zeros() as usize;
                let high = (at - i) as u32;
                let low = if self.low == 0 {
                    0
                } else {
                    let bit = i * self.low as usize;
                    // Fast path for widths that fit a word read.
                    let first = bit / 8;
                    let mut buf = [0u8; 8];
                    let take = (self.lows.len() - first).min(8);
                    buf[..take].copy_from_slice(&self.lows[first..first + take]);
                    ((u64::from_le_bytes(buf) >> (bit % 8)) & mask) as u32
                };
                visit(high << self.low | low);
                i += 1;
                word &= word - 1;
            }
        }
    }

    /// The values, decoded.
    pub fn to_vec(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.n);
        self.for_each(|v| out.push(v));
        out
    }

    /// A cursor at the first value.
    pub fn cursor(&self) -> EfCursor<'a> {
        let mut cursor = EfCursor {
            ef: *self,
            index: 0,
            bit: 0,
            current: None,
        };
        cursor.load();
        cursor
    }
}

/// Reads an [`Ef`] list forward, with its rank.
#[derive(Clone, Debug)]
pub struct EfCursor<'a> {
    ef: Ef<'a>,
    /// Rank of the current value.
    index: usize,
    /// The highs bit of the current value.
    bit: usize,
    current: Option<u32>,
}

impl EfCursor<'_> {
    fn load(&mut self) {
        if self.index >= self.ef.n {
            self.current = None;
            return;
        }
        let total = self.ef.highs.len() * 8;
        let mut at = self.bit;
        loop {
            if at >= total {
                self.current = None;
                return;
            }
            let word = bits::word(self.ef.highs, at / 64) >> (at % 64);
            if word != 0 {
                at += word.trailing_zeros() as usize;
                break;
            }
            at = (at / 64 + 1) * 64;
        }
        self.bit = at;
        let high = (at - self.index) as u32;
        let low = bits::get(self.ef.lows, self.index, self.ef.low).unwrap_or(0);
        self.current = Some(high << self.ef.low | low);
    }

    pub fn current(&self) -> Option<u32> {
        self.current
    }

    /// The rank of the current value in the list.
    pub fn rank(&self) -> usize {
        self.index
    }

    pub fn advance(&mut self) {
        if self.current.is_none() {
            return;
        }
        self.index += 1;
        self.bit += 1;
        self.load();
    }

    /// Moves to the first value at or after `target`.
    pub fn seek(&mut self, target: u32) {
        while let Some(current) = self.current {
            if current >= target {
                return;
            }
            // Skip whole words of the highs whose values all fall short:
            // the next value's high part is at least the zeros passed.
            let high_target = (target >> self.ef.low) as usize;
            let floor_bit = high_target + self.index;
            if floor_bit > self.bit + 64 {
                // Count the ones (values) between here and the word holding
                // `floor_bit`'s neighborhood; each one passed is a value
                // whose high part is below the target's.
                let mut at = self.bit + 1;
                let mut index = self.index + 1;
                while at / 64 < floor_bit.min(self.ef.highs.len() * 8) / 64 {
                    let word = bits::word(self.ef.highs, at / 64) >> (at % 64);
                    let ones = word.count_ones() as usize;
                    // A value at bit `p` with rank `r` has high part
                    // `p - r`; the word's last value has the largest, at
                    // most `next_at - (index + ones)`. Skip the word only
                    // when that falls short of the target's high part.
                    let next_at = (at / 64 + 1) * 64;
                    if next_at - (index + ones) >= high_target {
                        break;
                    }
                    index += ones;
                    at = next_at;
                }
                if index > self.index + 1 {
                    // Resume from the word boundary with the rank there.
                    self.index = index;
                    self.bit = at;
                    self.load();
                    continue;
                }
            }
            self.advance();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn round_trip(values: &[u32], universe: u32) {
        let mut out = Vec::new();
        encode(values, universe, &mut out);
        assert_eq!(out.len(), encoded_len(values.len(), universe));
        let ef = Ef::parse(&out, values.len(), universe).unwrap();
        assert_eq!(ef.to_vec(), values);
        let mut cursor = ef.cursor();
        for (i, v) in values.iter().enumerate() {
            assert_eq!(cursor.current(), Some(*v));
            assert_eq!(cursor.rank(), i);
            cursor.advance();
        }
        assert_eq!(cursor.current(), None);
    }

    #[test]
    fn edge_cases() {
        round_trip(&[], 1);
        round_trip(&[0], 1);
        round_trip(&[5], 6);
        round_trip(&[0, 1, 2, 3], 4);
        round_trip(&[u32::MAX - 1], u32::MAX);
        round_trip(&[0, u32::MAX - 1], u32::MAX);
        let dense: Vec<u32> = (0..1000).collect();
        round_trip(&dense, 1000);
        let sparse: Vec<u32> = (0..100).map(|i| i * 99_991).collect();
        round_trip(&sparse, 100 * 99_991);
    }

    #[test]
    fn size_is_near_two_plus_log_ratio() {
        // 1,000 values in a universe of 1,000,000: l = 9, about 11 bits each.
        let len = encoded_len(1000, 1_000_000);
        assert!(len * 8 <= 1000 * 12, "{len}");
    }

    proptest! {
        #[test]
        fn round_trips(mut values in prop::collection::btree_set(0u32..200_000, 0..400), extra in 1u32..1000) {
            let values: Vec<u32> = std::mem::take(&mut values).into_iter().collect();
            let universe = values.last().map_or(1, |v| v + extra);
            round_trip(&values, universe);
        }

        #[test]
        fn seeks(values in prop::collection::btree_set(0u32..100_000, 1..400), targets in prop::collection::vec(0u32..110_000, 1..20)) {
            let values: Vec<u32> = values.into_iter().collect();
            let universe = values.last().unwrap() + 1;
            let mut out = Vec::new();
            encode(&values, universe, &mut out);
            let ef = Ef::parse(&out, values.len(), universe).unwrap();
            let mut targets = targets;
            targets.sort_unstable();
            let mut cursor = ef.cursor();
            for target in targets {
                cursor.seek(target);
                let expected = values.partition_point(|v| *v < target);
                prop_assert_eq!(cursor.current(), values.get(expected).copied());
                if cursor.current().is_some() {
                    prop_assert_eq!(cursor.rank(), expected);
                }
            }
        }
    }
}
