// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Little-endian bit packing: value `i` of width `w` occupies bits
//! `i * w .. (i + 1) * w` of the byte string, least significant bit first.

use crate::{Error, Result};

/// Appends fixed-width values to a byte string.
#[derive(Default)]
pub struct BitWriter {
    pub bytes: Vec<u8>,
    acc: u64,
    filled: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends the low `width` bits of `value`; `width` at most 32.
    pub fn put(&mut self, value: u32, width: u32) {
        debug_assert!(width <= 32);
        if width == 0 {
            return;
        }
        debug_assert!(width == 32 || value >> width == 0);
        self.acc |= u64::from(value) << self.filled;
        self.filled += width;
        while self.filled >= 8 {
            self.bytes.push(self.acc as u8);
            self.acc >>= 8;
            self.filled -= 8;
        }
    }

    /// Sets bit `at` (relative to where this writer started), growing the
    /// string as needed; only for writers that use [`Self::set`] alone.
    pub fn set(&mut self, at: usize) {
        let byte = at / 8;
        if self.bytes.len() <= byte {
            self.bytes.resize(byte + 1, 0);
        }
        self.bytes[byte] |= 1 << (at % 8);
    }

    /// The bytes, the last one padded with zero bits.
    pub fn finish(mut self) -> Vec<u8> {
        if self.filled > 0 {
            self.bytes.push(self.acc as u8);
        }
        self.bytes
    }
}

/// Bytes needed for `count` values of `width` bits.
pub const fn packed_len(count: usize, width: u32) -> usize {
    (count * width as usize).div_ceil(8)
}

/// Value `index` of width `width` (at most 32) from a packed string.
#[inline]
pub fn get(bytes: &[u8], index: usize, width: u32) -> Result<u32> {
    if width == 0 {
        return Ok(0);
    }
    get_at(bytes, index * width as usize, width)
}

/// The `width`-bit value at bit `bit` of `bytes`.
#[inline]
pub fn get_at(bytes: &[u8], bit: usize, width: u32) -> Result<u32> {
    if width == 0 {
        return Ok(0);
    }
    let first = bit / 8;
    // One unaligned load where eight bytes remain: a value of at most 32
    // bits at a bit offset below 8 fits in them.
    if let Some(chunk) = bytes.get(first..first + 8) {
        let value = u64::from_le_bytes(chunk.try_into().expect("eight bytes")) >> (bit % 8);
        return Ok((value & ((1u64 << width) - 1)) as u32);
    }
    let last = (bit + width as usize - 1) / 8;
    if last >= bytes.len() {
        return Err(Error::Truncated);
    }
    // Fewer than eight bytes remain: assembled a byte at a time (a slice
    // copy here is a call to memcpy).
    let mut word = 0u64;
    for (i, b) in bytes[first..=last].iter().enumerate() {
        word |= u64::from(*b) << (8 * i);
    }
    let value = word >> (bit % 8);
    Ok((value & ((1u64 << width) - 1)) as u32)
}

/// The little-endian word `i` of a byte string, zero past its end.
#[inline]
pub fn word(bytes: &[u8], i: usize) -> u64 {
    let at = i * 8;
    if let Some(chunk) = bytes.get(at..at + 8) {
        u64::from_le_bytes(chunk.try_into().expect("eight bytes"))
    } else if at < bytes.len() {
        let mut word = 0u64;
        for (i, b) in bytes[at..].iter().enumerate() {
            word |= u64::from(*b) << (8 * i);
        }
        word
    } else {
        0
    }
}

/// All ones where `keep` holds, else zero: for choosing between values
/// without a branch, where the choice follows data a branch predictor
/// cannot guess.
///
/// On x86-64 the mask passes through an empty `asm!`, which LLVM cannot
/// see through. A mask it can see it folds into a select, and a select in
/// a loop whose condition comes from memory it lowers to a test and a
/// jump on x86-64 (`x86-cmov-conversion`), which mispredicts about every
/// other time on such data. Elsewhere (aarch64's `csel`) it is left to
/// LLVM.
#[inline(always)]
pub fn mask(keep: bool) -> u64 {
    #[allow(unused_mut)]
    let mut mask = u64::from(keep).wrapping_neg();
    #[cfg(target_arch = "x86_64")]
    // SAFETY: a template of a comment only: `mask` is left as it is.
    unsafe {
        core::arch::asm!("/* {0} */", inout(reg) mask, options(pure, nomem, nostack, preserves_flags));
    }
    mask
}

/// Keeps the values `keep` holds, in order, as [`Vec::retain`] does but
/// without a branch on each value's fate: every value is written after
/// those kept so far, which count it only if it is kept.
#[inline]
pub fn retain(values: &mut Vec<u32>, mut keep: impl FnMut(u32) -> bool) {
    let mut kept = 0;
    for read in 0..values.len() {
        let value = values[read];
        let k = keep(value);
        values[kept] = value;
        kept += usize::from(k);
    }
    values.truncate(kept);
}

/// Keeps the members of `list` that are in `other`, as [`intersect_sorted`]
/// does, through `marks`, a bitmap over the values (all clear on entry, and
/// left clear): `other`'s values are marked, `list` is kept by its marks
/// ([`retain`]), and the marks are cleared again. A bit set, a bit tested
/// and a word cleared per value, none a branch, where a merge (even
/// [`intersect_sorted`]'s) compares each member with a stretch of `other`.
/// Values must lie below `marks.len() * 64`, and `other`'s must rise.
///
/// A word's marks are gathered in a register while `other`'s values stay
/// in it, and the word stored at each value: setting each mark in memory
/// read the word the store before had just written, one store forwarded
/// after another (most of a container's values share a word with the one
/// before), where now only the register carries from value to value.
#[inline]
pub fn intersect_marked(list: &mut Vec<u32>, other: &[u32], marks: &mut [u64]) {
    debug_assert!(other.windows(2).all(|w| w[0] < w[1]));
    let (mut at, mut word) = (usize::MAX, 0u64);
    for &v in other {
        let w = v as usize / 64;
        // The marks so far of `w`'s word: none when `v` starts it.
        word = (word & mask(w == at)) | 1 << (v % 64);
        marks[w] = word;
        at = w;
    }
    retain(list, |v| marks[v as usize / 64] >> (v % 64) & 1 == 1);
    for &v in other {
        marks[v as usize / 64] = 0;
    }
}

/// Values of `other` a step of [`intersect_sorted`] compares at once.
const STRIDE: usize = 8;

/// Keeps the members of `list` that are in `other`; both ascending and
/// without repeats.
///
/// A merge, but with `other` passed by counting how many of its next
/// [`STRIDE`] values lie below the member at hand (a compare the compiler
/// does in vector registers) rather than one value at a time, and the
/// member kept as [`retain`] keeps: neither how far `other` runs past a
/// member nor whether it holds it is a branch. A one-at-a-time merge
/// branches on both, and on x86-64 mispredicts about once a member. The
/// one branch left, another stride when all of one lies below, is rare:
/// `other` holds a few times as many values as `list` at most, as a rule
/// (the walk probes a longer one in place). Near its end, fewer than a
/// stride of values are counted masked.
#[inline]
pub fn intersect_sorted(list: &mut Vec<u32>, other: &[u32]) {
    let m = other.len();
    if m == 0 {
        list.clear();
        return;
    }
    /// How many of `window` (ascending) lie below `value`.
    #[inline(always)]
    fn below(window: &[u32; STRIDE], value: u32) -> usize {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; the loads read the window's
        // eight values.
        unsafe {
            // Those below are a prefix: the trailing ones of the lanes'
            // mask (two bits a lane after the pack), a bit scan, where a
            // sum is a population count, which baseline x86-64 lacks (a
            // multiply sequence, longer on the path from one member to the
            // next). Unsigned order by signed compares of biased values.
            use core::arch::x86_64::{
                _mm_cmpgt_epi32, _mm_loadu_si128, _mm_movemask_epi8, _mm_packs_epi32,
                _mm_set1_epi32, _mm_xor_si128,
            };
            let bias = _mm_set1_epi32(i32::MIN);
            let v = _mm_xor_si128(_mm_set1_epi32(value as i32), bias);
            let p = window.as_ptr();
            let lo = _mm_xor_si128(_mm_loadu_si128(p.cast()), bias);
            let hi = _mm_xor_si128(_mm_loadu_si128(p.add(4).cast()), bias);
            let lt = _mm_packs_epi32(_mm_cmpgt_epi32(v, lo), _mm_cmpgt_epi32(v, hi));
            let mask = _mm_movemask_epi8(lt) as u32;
            ((!mask).trailing_zeros() / 2) as usize
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            window.iter().map(|&o| u32::from(o < value)).sum::<u32>() as usize
        }
    }
    // `other[..j]` lies below the member at hand.
    let mut j = 0;
    let mut kept = 0;
    for read in 0..list.len() {
        let value = list[read];
        while let Some(window) = other.get(j..j + STRIDE) {
            let n = below(window.try_into().expect("a stride"), value);
            j += n;
            if n < STRIDE {
                break;
            }
        }
        if j + STRIDE > m {
            // Less than a stride left: the last stride counted (what lies
            // before `j` in it is below `value` already), or with fewer
            // values than a stride, each where it lies before the end
            // (read clamped to it, so no lane branches).
            if let Some(last) = m.checked_sub(STRIDE) {
                j = last + below(other[last..].try_into().expect("a stride"), value);
            } else {
                j = 0;
                for t in 0..STRIDE - 1 {
                    j += usize::from((t < m) & (other[t.min(m - 1)] < value));
                }
            }
        }
        list[kept] = value;
        // Past the end, `other`'s last value lies below `value`.
        kept += usize::from(other[j.min(m - 1)] == value);
    }
    list.truncate(kept);
}

/// Bits needed to hold values below `bound` (at least one).
pub const fn width_below(bound: u32) -> u32 {
    if bound <= 2 {
        1
    } else {
        32 - (bound - 1).leading_zeros()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn widths() {
        assert_eq!(width_below(1), 1);
        assert_eq!(width_below(2), 1);
        assert_eq!(width_below(3), 2);
        assert_eq!(width_below(256), 8);
        assert_eq!(width_below(257), 9);
        assert_eq!(width_below(291), 9);
    }

    #[test]
    fn masks() {
        assert_eq!(mask(true), u64::MAX);
        assert_eq!(mask(false), 0);
    }

    /// What [`intersect_sorted`] keeps: the members of `list` in `other`.
    fn common(list: &[u32], other: &[u32]) -> Vec<u32> {
        list.iter()
            .copied()
            .filter(|v| other.binary_search(v).is_ok())
            .collect()
    }

    #[test]
    fn intersects_edges() {
        for (list, other) in [
            (vec![], vec![]),
            (vec![], vec![1, 2]),
            (vec![1, 2], vec![]),
            (vec![0], vec![0]),
            (vec![0, u32::MAX], vec![0, u32::MAX]),
            (vec![u32::MAX], vec![0, 7, u32::MAX - 1]),
            (vec![3, 4, 5], vec![5]),
            (vec![5], vec![3, 4, 5]),
            (vec![1, 3, 5, 7], vec![0, 2, 4, 6, 8]),
            ((0..100).collect(), (50..300).collect()),
            (
                (0..1000).step_by(3).collect(),
                (0..1000).step_by(5).collect(),
            ),
            (vec![2, 999_999], (0..100_000).map(|v| v * 10 + 9).collect()),
        ] {
            let mut kept = list.clone();
            intersect_sorted(&mut kept, &other);
            assert_eq!(kept, common(&list, &other), "{list:?} {other:?}");
        }
    }

    proptest! {
        #[test]
        fn retains_what_vec_retain_keeps(values in prop::collection::vec(any::<u32>(), 0..300), m in 1u32..9, r in 0u32..9) {
            let mut kept = values.clone();
            retain(&mut kept, |v| v % m == r % m);
            let mut want = values;
            want.retain(|v| v % m == r % m);
            prop_assert_eq!(kept, want);
        }

        #[test]
        fn intersects(list in prop::collection::btree_set(0u32..5_000, 0..300), other in prop::collection::btree_set(0u32..5_000, 0..1200), pick in prop::collection::vec(any::<prop::sample::Index>(), 0..60)) {
            let other: Vec<u32> = other.into_iter().collect();
            let mut list = list;
            if !other.is_empty() {
                list.extend(pick.iter().map(|i| other[i.index(other.len())]));
            }
            let list: Vec<u32> = list.into_iter().collect();
            let mut kept = list.clone();
            intersect_sorted(&mut kept, &other);
            prop_assert_eq!(&kept, &common(&list, &other));
            // The other way round: a long list against a short one.
            let mut kept = other.clone();
            intersect_sorted(&mut kept, &list);
            prop_assert_eq!(kept, common(&other, &list));
        }

        #[test]
        fn intersects_through_marks(list in prop::collection::btree_set(0u32..5_000, 0..300), other in prop::collection::btree_set(0u32..5_000, 0..1200), pick in prop::collection::vec(any::<prop::sample::Index>(), 0..60)) {
            let other: Vec<u32> = other.into_iter().collect();
            let mut list = list;
            if !other.is_empty() {
                list.extend(pick.iter().map(|i| other[i.index(other.len())]));
            }
            let list: Vec<u32> = list.into_iter().collect();
            let mut marks = vec![0u64; 5_000usize.div_ceil(64)];
            let mut kept = list.clone();
            intersect_marked(&mut kept, &other, &mut marks);
            prop_assert_eq!(&kept, &common(&list, &other));
            prop_assert!(marks.iter().all(|w| *w == 0), "marks left set");
            let mut kept = other.clone();
            intersect_marked(&mut kept, &list, &mut marks);
            prop_assert_eq!(kept, common(&other, &list));
            prop_assert!(marks.iter().all(|w| *w == 0), "marks left set");
        }

        #[test]
        fn round_trips(width in 0u32..=32, values in prop::collection::vec(any::<u32>(), 0..200)) {
            let mask = if width == 32 { u32::MAX } else { (1u32 << width) - 1 };
            let values: Vec<u32> = values.into_iter().map(|v| v & mask).collect();
            let mut writer = BitWriter::new();
            for v in &values {
                writer.put(*v, width);
            }
            let bytes = writer.finish();
            prop_assert_eq!(bytes.len(), packed_len(values.len(), width));
            for (i, v) in values.iter().enumerate() {
                prop_assert_eq!(get(&bytes, i, width).unwrap(), *v);
            }
        }
    }
}
