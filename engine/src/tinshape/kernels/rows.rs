// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Rows: a group's members, ascending, set as bits in a row of words over
//! the group's slots (bit `v % 64` of word `v / 64`), as a disjunction takes
//! a term's members and a fold an Elias-Fano or array container's.
//!
//! Setting a member's bit by reading its word, ORing the bit in and storing
//! it back makes every member wait on the store of the one before when the
//! two share a word: at one member in 16 slots, about four do, and the
//! store-to-load forwarding costs 1.1 ns a member on the M4. Two bodies
//! avoid that, for a row clear from the first member's bit on (a cleared
//! row, or one set so far by smaller members):
//!
//! - portable ([`RowPut`]): the word at hand's bits kept in a register,
//!   started afresh at each new word, and the word stored after each
//!   member, never read (0.5 ns a member on the M4: a select and an OR
//!   carried from member to member);
//! - NEON (aarch64, inlined): eight members at a time, at byte grain: each
//!   member's byte and bit, the bits of members sharing a byte ORed together
//!   in three shifted steps (sorted members of one byte are adjacent lanes),
//!   the last byte's bits carried to the next eight, and a byte stored per
//!   member, so the last store to a byte holds all its bits (0.33 ns a
//!   member at densities 1/4 to 1/64, 0.42 at 1/128).
//!
//! On x86-64 the portable body runs: baseline x86-64 has no variable shift
//! per lane, and a body chosen at run time cannot be inlined into the walks
//! (see [`super::array`]).

/// Sets ascending values in a row whose bits from the first value's on are
/// clear: the word at hand's bits kept in a register, the word stored after
/// each value (a value in a new word starts from none), so that no store
/// waits on a load of the word the last one stored.
pub(crate) struct RowPut {
    w: usize,
    bits: u64,
}

impl RowPut {
    /// For a row cleared from its first value's word on.
    #[inline(always)]
    pub(crate) fn new() -> Self {
        Self {
            w: usize::MAX,
            bits: 0,
        }
    }

    /// For a row clear from bit `first` on, keeping the bits below it in
    /// its word.
    #[inline(always)]
    pub(crate) fn at(row: &[u64], first: u32) -> Self {
        let w = first as usize / 64;
        Self { w, bits: row[w] }
    }

    #[inline(always)]
    pub(crate) fn put(&mut self, row: &mut [u64], v: u32) {
        let w = v as usize / 64;
        let kept = if w == self.w { self.bits } else { 0 };
        self.bits = kept | 1 << (v % 64);
        self.w = w;
        row[w] = self.bits;
    }

    /// [`Self::put`] of a value within the row.
    ///
    /// # Safety
    /// `v / 64 < row.len()`.
    #[inline(always)]
    pub(crate) unsafe fn put_unchecked(&mut self, row: &mut [u64], v: u32) {
        let w = v as usize / 64;
        let kept = if w == self.w { self.bits } else { 0 };
        self.bits = kept | 1 << (v % 64);
        self.w = w;
        // SAFETY: the caller's.
        unsafe { *row.get_unchecked_mut(w) = self.bits };
    }
}

/// Values the NEON body takes at a time; shorter lists take the portable one.
const LANES: usize = 8;

/// Sets bit `v` of `row` for each value `v` of `values` (ascending, without
/// repeats), the row's bits from the first value's on being clear (bits
/// below it are kept). Panics if a value is past the row.
#[inline]
pub fn set_ascending(values: &[u32], row: &mut [u64]) {
    let Some(&last) = values.last() else {
        return;
    };
    assert!((last as usize) < 64 * row.len(), "a value past the row");
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    if values.len() >= LANES && neon::fits(row) {
        // SAFETY: every value is below the last, within the row, which
        // `fits`; NEON is baseline on aarch64.
        unsafe { neon::set_u32(values, row) };
        return;
    }
    let mut put = RowPut::at(row, values[0]);
    for &v in values {
        // SAFETY: `v <= last`, within the row.
        unsafe { put.put_unchecked(row, v) };
    }
}

/// [`set_ascending`] of an array container's values: `bytes.len() / 2`
/// little-endian `u16`s.
#[inline]
pub fn set_ascending_u16(bytes: &[u8], row: &mut [u64]) {
    let n = bytes.len() / 2;
    if n == 0 {
        return;
    }
    let value = |i: usize| u32::from(u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]));
    let last = value(n - 1);
    assert!((last as usize) < 64 * row.len(), "a value past the row");
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    if n >= LANES && neon::fits(row) {
        // SAFETY: as in `set_ascending`.
        unsafe { neon::set_u16(&bytes[..2 * n], row) };
        return;
    }
    let mut put = RowPut::at(row, value(0));
    for i in 0..n {
        // SAFETY: as in `set_ascending`.
        unsafe { put.put_unchecked(row, value(i)) };
    }
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
mod neon {
    use core::arch::aarch64::*;

    /// A byte index no value's reaches: rows of fewer than `2^19` bits.
    const NONE: u16 = u16::MAX;

    /// Whether the row's byte indexes fit 16-bit lanes below [`NONE`].
    #[inline(always)]
    pub fn fits(row: &[u64]) -> bool {
        row.len() * 8 < usize::from(NONE)
    }

    /// The byte stores of eight values at a time, and what is carried from
    /// one eight to the next: the last value's byte index and that byte's
    /// bits so far, in every lane.
    struct Bytes {
        out: *mut u8,
        at: uint16x8_t,
        bits: uint16x8_t,
    }

    impl Bytes {
        /// Starting at value `first`'s byte, its bits below `first` kept.
        ///
        /// # Safety
        /// `out` is the row's bytes, and `first` is within it.
        #[inline(always)]
        unsafe fn new(out: *mut u8, first: u32) -> Self {
            let at = (first / 8) as u16;
            // SAFETY: the caller's; NEON is baseline on aarch64.
            unsafe {
                let byte = *out.add(usize::from(at));
                Self {
                    out,
                    at: vdupq_n_u16(at),
                    bits: vdupq_n_u16(u16::from(byte)),
                }
            }
        }

        /// Sets eight ascending values, given as their byte indexes and
        /// their bit positions in those bytes.
        ///
        /// # Safety
        /// Every byte index is within the row, and at or after the last
        /// one set.
        #[inline(always)]
        unsafe fn eight(&mut self, at: uint16x8_t, low: uint16x8_t) {
            // SAFETY: eight lanes stored into eight; the byte stores within
            // the row (the caller's); NEON is baseline on aarch64.
            unsafe {
                let none = vdupq_n_u16(NONE);
                let zero = vdupq_n_u16(0);
                let mut bits = vshlq_u16(vdupq_n_u16(1), vreinterpretq_s16_u16(low));
                // Lane k ORs in lane k - s's bits where the two share a byte
                // (then so do the lanes between them): after steps of 1, 2 and
                // 4, a lane holds the bits of every lane before it in its byte.
                let s1 = vceqq_u16(at, vextq_u16::<7>(none, at));
                bits = vorrq_u16(bits, vandq_u16(vextq_u16::<7>(zero, bits), s1));
                let s2 = vceqq_u16(at, vextq_u16::<6>(none, at));
                bits = vorrq_u16(bits, vandq_u16(vextq_u16::<6>(zero, bits), s2));
                let s4 = vceqq_u16(at, vextq_u16::<4>(none, at));
                bits = vorrq_u16(bits, vandq_u16(vextq_u16::<4>(zero, bits), s4));
                // The carried byte's bits, into the lanes in that byte.
                bits = vorrq_u16(bits, vandq_u16(self.bits, vceqq_u16(at, self.at)));
                self.at = vdupq_laneq_u16::<7>(at);
                self.bits = vdupq_laneq_u16::<7>(bits);
                let (mut ats, mut values) = ([0u16; 8], [0u16; 8]);
                vst1q_u16(ats.as_mut_ptr(), at);
                vst1q_u16(values.as_mut_ptr(), bits);
                for k in 0..8 {
                    *self.out.add(usize::from(ats[k])) = values[k] as u8;
                }
            }
        }
    }

    /// The rest of the values after the last eight, a byte read and stored
    /// per value (the eights' stores before it).
    ///
    /// # Safety
    /// Each value is within the row whose bytes are `out`.
    #[inline(always)]
    unsafe fn tail(out: *mut u8, values: impl Iterator<Item = u32>) {
        for v in values {
            // SAFETY: the caller's.
            unsafe { *out.add(v as usize / 8) |= 1 << (v % 8) };
        }
    }

    /// # Safety
    /// The values ascend and lie within the row, which [`fits`]; there is
    /// at least one.
    #[inline(always)]
    pub unsafe fn set_u32(values: &[u32], row: &mut [u64]) {
        let out = row.as_mut_ptr().cast::<u8>();
        // SAFETY: the caller's; each load reads eight of the values.
        unsafe {
            let mut bytes = Bytes::new(out, values[0]);
            let seven = vdupq_n_u16(7);
            let mut eights = values.chunks_exact(8);
            for e in &mut eights {
                let a = vld1q_u32(e.as_ptr());
                let b = vld1q_u32(e.as_ptr().add(4));
                let at = vcombine_u16(vshrn_n_u32::<3>(a), vshrn_n_u32::<3>(b));
                let low = vandq_u16(vcombine_u16(vmovn_u32(a), vmovn_u32(b)), seven);
                bytes.eight(at, low);
            }
            tail(out, eights.remainder().iter().copied());
        }
    }

    /// # Safety
    /// As [`set_u32`], of `bytes.len() / 2` little-endian `u16`s.
    #[inline(always)]
    pub unsafe fn set_u16(bytes: &[u8], row: &mut [u64]) {
        let out = row.as_mut_ptr().cast::<u8>();
        let n = bytes.len() / 2;
        // SAFETY: the caller's; each load reads 16 of the `2n` bytes.
        unsafe {
            let first = u32::from(u16::from_le_bytes([bytes[0], bytes[1]]));
            let mut bytes_out = Bytes::new(out, first);
            let seven = vdupq_n_u16(7);
            let mut i = 0;
            while i + 8 <= n {
                let v = vld1q_u16(bytes.as_ptr().add(2 * i).cast());
                bytes_out.eight(vshrq_n_u16::<3>(v), vandq_u16(v, seven));
                i += 8;
            }
            tail(
                out,
                (i..n).map(|i| u32::from(u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]))),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn sorted(mut v: Vec<u32>) -> Vec<u32> {
        v.sort_unstable();
        v.dedup();
        v
    }

    fn expected(base: &[u64], values: &[u32]) -> Vec<u64> {
        let mut row = base.to_vec();
        for v in values {
            row[*v as usize / 64] |= 1 << (v % 64);
        }
        row
    }

    proptest! {
        /// Setting into a row clear from the first value's bit on (bits
        /// below it random) gives the bits ORed in one at a time, for dense
        /// runs (many values a byte) and sparse ones, in one call or split
        /// in two calls (the second continuing the first's words).
        #[test]
        fn sets_as_one_at_a_time(
            raw in proptest::collection::vec(0u32..4096, 0..600),
            dense in proptest::collection::vec(0u32..300, 0..300),
            split in 0usize..600,
            below in any::<u64>(),
        ) {
            let words = 4096 / 64;
            for values in [sorted(raw.clone()), sorted(dense.iter().map(|v| v + 1000).collect())] {
                let mut base = vec![0u64; words];
                if let Some(&first) = values.first() {
                    let w = first as usize / 64;
                    for x in &mut base[..w] {
                        *x = below;
                    }
                    base[w] = below & ((1u64 << (first % 64)) - 1);
                }
                let want = expected(&base, &values);
                let mut row = base.clone();
                set_ascending(&values, &mut row);
                prop_assert_eq!(&row, &want);
                let mut row = base.clone();
                let s = split.min(values.len());
                set_ascending(&values[..s], &mut row);
                set_ascending(&values[s..], &mut row);
                prop_assert_eq!(&row, &want);
                let bytes: Vec<u8> = values.iter().flat_map(|v| (*v as u16).to_le_bytes()).collect();
                let mut row = base.clone();
                set_ascending_u16(&bytes, &mut row);
                prop_assert_eq!(&row, &want);
            }
        }
    }

    #[test]
    #[should_panic(expected = "a value past the row")]
    fn a_value_past_the_row_panics() {
        let mut row = vec![0u64; 2];
        set_ascending(&[1, 2, 3, 4, 5, 6, 7, 8, 128], &mut row);
    }
}
