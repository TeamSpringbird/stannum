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
        // The lows are read as a stream: a 64-bit buffer refilled four
        // bytes at a time.
        let low_bits = self.low;
        let mut acc = 0u64;
        let mut avail = 0u32;
        let mut next = 0usize;
        let lows = self.lows;
        for w in 0..words {
            let mut word = bits::word(self.highs, w);
            while word != 0 {
                if i >= self.n {
                    return;
                }
                let at = w * 64 + word.trailing_zeros() as usize;
                let high = (at - i) as u32;
                let low = if low_bits == 0 {
                    0
                } else {
                    if avail < low_bits {
                        if let Some(four) = lows.get(next..next + 4) {
                            acc |= u64::from(u32::from_le_bytes(four.try_into().expect("four")))
                                << avail;
                            avail += 32;
                            next += 4;
                        } else {
                            while avail < low_bits && next < lows.len() {
                                acc |= u64::from(lows[next]) << avail;
                                avail += 8;
                                next += 1;
                            }
                        }
                    }
                    let low = (acc & mask) as u32;
                    acc >>= low_bits;
                    avail -= low_bits.min(avail);
                    low
                };
                visit(high << low_bits | low);
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

    /// Keeps the values of `values` (ascending, distinct) the list holds.
    ///
    /// The highs are passed a word at a time to each value's high bucket
    /// (its high part is the number of zeros before it), and only the lows
    /// of that bucket's members are read: for a few values against a long
    /// list, a small part of what decoding the list reads.
    pub fn retain_members(&self, values: &mut Vec<u32>) {
        let (highs, lows, low, n) = (self.highs, self.lows, self.low, self.n);
        let total = highs.len() * 8;
        let mask = if low == 0 { 0 } else { (1u32 << low) - 1 };
        // The highs bit the scan stands at, and the ones before it (the
        // rank of the next member); the zeros before it are the difference.
        let mut bit = 0usize;
        let mut index = 0usize;
        values.retain(|&v| {
            let h = (v >> low) as usize;
            // Move to bucket `h`'s first bit, `h` zeros in.
            while bit - index < h {
                if bit >= total {
                    return false;
                }
                let need = h - (bit - index);
                let off = bit % 64;
                let word = bits::word(highs, bit / 64) >> off;
                let ones = word.count_ones() as usize;
                let zeros = 64 - off - ones;
                if zeros < need {
                    index += ones;
                    bit += 64 - off;
                    continue;
                }
                let p = select(!word, need - 1) as usize;
                index += (word & ((1u64 << p) - 1)).count_ones() as usize;
                bit += p + 1;
            }
            // The bucket's members, until its closing zero.
            let want = v & mask;
            while index < n && bit < total && highs[bit / 8] >> (bit % 8) & 1 == 1 {
                let have = bits::get(lows, index, low).unwrap_or(0);
                if have >= want {
                    return have == want;
                }
                index += 1;
                bit += 1;
            }
            false
        });
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

/// The position of the set bit of rank `k` (from 0) in `x`, which has more
/// than `k`: the byte by prefix sums of byte counts, then the bit.
#[inline]
fn select(x: u64, k: usize) -> u32 {
    const ONES: u64 = 0x0101_0101_0101_0101;
    let mut c = x - ((x >> 1) & 0x5555_5555_5555_5555);
    c = (c & 0x3333_3333_3333_3333) + ((c >> 2) & 0x3333_3333_3333_3333);
    c = (c + (c >> 4)) & 0x0f0f_0f0f_0f0f_0f0f;
    // Byte `i` of `prefix`: the set bits of bytes `0..=i`.
    let prefix = c.wrapping_mul(ONES);
    let mut byte = 0u32;
    while ((prefix >> (8 * byte)) & 0xff) as usize <= k {
        byte += 1;
    }
    let before = if byte == 0 {
        0
    } else {
        ((prefix >> (8 * (byte - 1))) & 0xff) as usize
    };
    let mut b = (x >> (8 * byte)) & 0xff;
    for _ in before..k {
        b &= b - 1;
    }
    8 * byte + b.trailing_zeros()
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

impl<'a> EfCursor<'a> {
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

    /// The list the cursor reads.
    pub fn list(&self) -> Ef<'a> {
        self.ef
    }

    /// The rank of the current value in the list.
    pub fn rank(&self) -> usize {
        self.index
    }

    /// Calls `visit` with the current value and each after it below `end`,
    /// and stops at the first at or past it: [`Self::advance`] in one loop,
    /// the highs word and the lows read in place.
    #[inline]
    pub fn drain_below(&mut self, end: u32, mut visit: impl FnMut(u32)) {
        let Some(mut value) = self.current else {
            return;
        };
        let (highs, lows, low, n) = (self.ef.highs, self.ef.lows, self.ef.low, self.ef.n);
        let total = highs.len() * 8;
        let mut index = self.index;
        let mut bit = self.bit;
        while value < end {
            visit(value);
            index += 1;
            if index >= n {
                self.index = n;
                self.bit = bit + 1;
                self.current = None;
                return;
            }
            let mut at = bit + 1;
            let mut word = if at < total {
                bits::word(highs, at / 64) >> (at % 64)
            } else {
                0
            };
            while word == 0 {
                at = (at / 64 + 1) * 64;
                if at >= total {
                    self.index = index;
                    self.bit = at;
                    self.current = None;
                    return;
                }
                word = bits::word(highs, at / 64);
            }
            at += word.trailing_zeros() as usize;
            bit = at;
            let high = (at - index) as u32;
            value = high << low | bits::get(lows, index, low).unwrap_or(0);
        }
        self.index = index;
        self.bit = bit;
        self.current = Some(value);
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
    ///
    /// Values whose high part falls short of the target's are passed by
    /// their high bits alone (a word of them at a time where every one of a
    /// word does), and only values in the target's high bucket have their
    /// low bits read.
    pub fn seek(&mut self, target: u32) {
        match self.current {
            Some(current) if current < target => {}
            _ => return,
        }
        let high_target = (target >> self.ef.low) as usize;
        let highs = self.ef.highs;
        let total = highs.len() * 8;
        // The next value's rank and the highs bit to look from.
        let mut index = self.index + 1;
        let mut at = self.bit + 1;
        'words: loop {
            if index >= self.ef.n || at >= total {
                self.index = index.min(self.ef.n);
                self.current = None;
                return;
            }
            let next_at = (at / 64 + 1) * 64;
            let mut word = bits::word(highs, at / 64) >> (at % 64);
            let ones = word.count_ones() as usize;
            // A value at bit `p` with rank `r` has high part `p - r`; the
            // word's last has the largest, at most `next_at - index - ones`.
            if ones == 0 || next_at - (index + ones) < high_target {
                index += ones;
                at = next_at;
                continue;
            }
            while word != 0 {
                let p = at + word.trailing_zeros() as usize;
                if p - index >= high_target {
                    self.index = index;
                    self.bit = p;
                    self.load();
                    break 'words;
                }
                index += 1;
                word &= word - 1;
            }
            at = next_at;
        }
        // Values of the target's high bucket with lower low bits.
        while let Some(current) = self.current {
            if current >= target {
                return;
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

    #[test]
    fn drains_runs() {
        let values: Vec<u32> = (0..3000u32).map(|i| i * 7 + i % 5).collect();
        let mut out = Vec::new();
        encode(&values, 30_000, &mut out);
        let ef = Ef::parse(&out, values.len(), 30_000).unwrap();
        let mut cursor = ef.cursor();
        let mut seen = Vec::new();
        for end in [0, 1, 50, 51, 999, 10_000, 20_996, 21_005, 30_000] {
            cursor.drain_below(end, |v| seen.push(v));
            let rank = cursor.rank();
            assert_eq!(seen.len(), rank);
            assert_eq!(cursor.current(), values.get(rank).copied());
            assert!(cursor.current().is_none_or(|c| c >= end));
        }
        assert_eq!(seen, values);
    }

    fn check_retain(values: &[u32], universe: u32, probes: &[u32]) {
        let mut out = Vec::new();
        encode(values, universe, &mut out);
        let ef = Ef::parse(&out, values.len(), universe).unwrap();
        let mut kept = probes.to_vec();
        ef.retain_members(&mut kept);
        let expected: Vec<u32> = probes
            .iter()
            .copied()
            .filter(|p| values.binary_search(p).is_ok())
            .collect();
        assert_eq!(
            kept, expected,
            "values {values:?} universe {universe} probes {probes:?}"
        );
    }

    #[test]
    fn selects() {
        for x in [1u64, u64::MAX, 0x8000_0000_0000_0000, 0xf0f0_0000_ff00_0101] {
            let mut bits = Vec::new();
            for i in 0..64 {
                if x >> i & 1 == 1 {
                    bits.push(i);
                }
            }
            for (k, b) in bits.iter().enumerate() {
                assert_eq!(select(x, k), *b, "{x:#x} {k}");
            }
        }
    }

    #[test]
    fn retains_members() {
        check_retain(&[], 1, &[0]);
        check_retain(&[0], 1, &[0]);
        check_retain(&[5], 6, &[0, 4, 5]);
        let dense: Vec<u32> = (0..1000).collect();
        check_retain(&dense, 1000, &[0, 63, 64, 500, 999]);
        let every_third: Vec<u32> = (0..2000).map(|i| i * 3).collect();
        let probes: Vec<u32> = (0..6100).step_by(7).collect();
        check_retain(&every_third, 6001, &probes);
        // Long runs of empty buckets, then a crowded one.
        let mut clustered: Vec<u32> = (0..40).map(|i| i * 4_000).collect();
        clustered.extend(160_001..160_100);
        check_retain(
            &clustered,
            170_000,
            &[0, 3_999, 4_000, 80_000, 160_000, 160_050, 169_999],
        );
    }

    proptest! {
        #[test]
        fn retains(values in prop::collection::btree_set(0u32..50_000, 0..600), extra in 1u32..100, probes in prop::collection::btree_set(0u32..50_100, 0..80), pick in prop::collection::vec(any::<prop::sample::Index>(), 0..40)) {
            let values: Vec<u32> = values.into_iter().collect();
            let universe = values.last().map_or(1, |v| v + extra);
            let mut probes: std::collections::BTreeSet<u32> = probes;
            if !values.is_empty() {
                probes.extend(pick.iter().map(|i| values[i.index(values.len())]));
            }
            let probes: Vec<u32> = probes.into_iter().collect();
            check_retain(&values, universe, &probes);
        }

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
