// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A disjunction's members weighed by the bounds of the terms holding them,
//! several at a time (see `Walk::or_group`): the sum, in the terms' order,
//! of each term's bound where the term holds the member, else `+0.0`, as
//! [`or_zero`] adds it. Lanes are words ([`word_sums`]: a word's members
//! weighed together by the terms holding any of them) or the members of one
//! word ([`member_sums`]). Each lane's sum is the same `f32` additions in the
//! same order as the one-at-a-time loop's, so the sums are bit for bit its.
//!
//! The portable bodies are those loops. The vector bodies test a vector of
//! words (or a term's word shifted by a vector of bit positions) against the
//! mask, widen each 64-bit lane's verdict to a 32-bit one (two vectors of
//! words make one of `f32` lanes), keep the term's bound under it and add.
//! The per-word sum of the one-at-a-time loop ran a chain of dependent
//! `f32` additions per term; here a chain runs across lanes at once.

use super::{Level, level};

/// `x` where `keep` holds, else `+0.0`, without a branch: `x`'s bits under
/// an all-ones or all-zeros mask. Added to a sum, it adds exactly what
/// `if keep { x } else { 0.0 }` would, for every `x` (NaN, infinities and
/// -0.0 included). LLVM lowers that select of a float on an integer
/// condition to a test and a jump on x86-64 below AVX-512, which the walks'
/// per-word bound sums mispredict.
///
/// LLVM folds the mask back into that select, so on x86-64 the mask passes
/// through an empty `asm!` first, which it cannot see through: it stays an
/// `and`. With AVX-512 (a masked move) and on aarch64 (`fcsel`) the select
/// has no branch already, and is left to LLVM.
#[inline(always)]
pub fn or_zero(x: f32, keep: bool) -> f32 {
    #[allow(unused_mut)]
    let mut mask = u32::from(keep).wrapping_neg();
    #[cfg(all(target_arch = "x86_64", not(target_feature = "avx512f")))]
    // SAFETY: a template of a comment only: `mask` is left as it is.
    unsafe {
        core::arch::asm!("/* {0:e} */", inout(reg) mask, options(pure, nomem, nostack, preserves_flags));
    }
    f32::from_bits(x.to_bits() & mask)
}

/// Adds to `sums[j]`, for each word `j` of `mask`, each term's bound where
/// `mask[j]` and word `j` of the term's row share a bit: `rows[t]` is term
/// `t`'s row as little-endian bytes (at least `mask.len() * 8` of them),
/// `bounds[t]` its bound.
#[inline]
pub fn word_sums(mask: &[u64], rows: &[&[u8]], bounds: &[f32], sums: &mut [f32]) {
    check_words(mask, rows, bounds, sums);
    run!(level(), word_sums(mask, rows, bounds, sums))
}

/// [`word_sums`] at `level`; panics if the CPU lacks it.
pub fn word_sums_at(level: Level, mask: &[u64], rows: &[&[u8]], bounds: &[f32], sums: &mut [f32]) {
    assert!(level.supported(), "kernel level {} unsupported here", level.name());
    check_words(mask, rows, bounds, sums);
    run!(level, word_sums(mask, rows, bounds, sums))
}

/// Adds to `sums[j]`, for each bit position `bits[j]` (below 64), each
/// term's bound where bit `bits[j]` of `held[t]`, the term's word, is set.
#[inline]
pub fn member_sums(held: &[u64], bits: &[u8], bounds: &[f32], sums: &mut [f32]) {
    check_members(held, bits, bounds, sums);
    run!(level(), member_sums(held, bits, bounds, sums))
}

/// [`member_sums`] at `level`; panics if the CPU lacks it.
pub fn member_sums_at(level: Level, held: &[u64], bits: &[u8], bounds: &[f32], sums: &mut [f32]) {
    assert!(level.supported(), "kernel level {} unsupported here", level.name());
    check_members(held, bits, bounds, sums);
    run!(level, member_sums(held, bits, bounds, sums))
}

#[inline(always)]
fn check_words(mask: &[u64], rows: &[&[u8]], bounds: &[f32], sums: &mut [f32]) {
    assert_eq!(rows.len(), bounds.len(), "a bound per row");
    assert_eq!(mask.len(), sums.len(), "a sum per word");
    for row in rows {
        assert!(row.len() >= mask.len() * 8, "a row of the mask's words");
    }
}

#[inline(always)]
fn check_members(held: &[u64], bits: &[u8], bounds: &[f32], sums: &mut [f32]) {
    assert_eq!(held.len(), bounds.len(), "a bound per held word");
    assert_eq!(bits.len(), sums.len(), "a sum per member");
    assert!(bits.iter().all(|b| *b < 64), "bit positions of a word");
}

/// The portable bodies: the one-at-a-time loops, the reference every level
/// is tested against.
mod scalar {
    use super::or_zero;

    /// Words `from..` of the inputs, one at a time.
    #[inline(always)]
    pub fn word_tail(mask: &[u64], rows: &[&[u8]], bounds: &[f32], sums: &mut [f32], from: usize) {
        for (row, &bound) in rows.iter().zip(bounds) {
            for j in from..mask.len() {
                let w = u64::from_le_bytes(row[j * 8..j * 8 + 8].try_into().expect("eight bytes"));
                sums[j] += or_zero(bound, w & mask[j] != 0);
            }
        }
    }

    /// Members `from..` of the inputs, one at a time.
    #[inline(always)]
    pub fn member_tail(held: &[u64], bits: &[u8], bounds: &[f32], sums: &mut [f32], from: usize) {
        for (&h, &bound) in held.iter().zip(bounds) {
            for j in from..bits.len() {
                sums[j] += or_zero(bound, h >> bits[j] & 1 == 1);
            }
        }
    }

    #[inline(always)]
    pub fn word_sums(mask: &[u64], rows: &[&[u8]], bounds: &[f32], sums: &mut [f32]) {
        word_tail(mask, rows, bounds, sums, 0)
    }

    #[inline(always)]
    pub fn member_sums(held: &[u64], bits: &[u8], bounds: &[f32], sums: &mut [f32]) {
        member_tail(held, bits, bounds, sums, 0)
    }
}

/// NEON bodies: four lanes of `f32` from two vectors of two words.
#[cfg(target_arch = "aarch64")]
mod neon {
    use super::scalar;
    use core::arch::aarch64::*;

    /// The low halves of two vectors of 64-bit verdicts as four 32-bit ones.
    #[inline(always)]
    fn narrow(k0: uint64x2_t, k1: uint64x2_t) -> uint32x4_t {
        // SAFETY: NEON is baseline on aarch64; register operations only.
        unsafe { vuzp1q_u32(vreinterpretq_u32_u64(k0), vreinterpretq_u32_u64(k1)) }
    }

    #[inline]
    pub fn word_sums(mask: &[u64], rows: &[&[u8]], bounds: &[f32], sums: &mut [f32]) {
        let n = mask.len();
        let full = n - n % 4;
        // SAFETY: NEON is baseline on aarch64; every load is of four words
        // (32 bytes) at most `full` words in, which every row holds (checked
        // by the caller), and every store of four sums.
        unsafe {
            let mut j = 0;
            while j < full {
                let m0 = vld1q_u64(mask.as_ptr().add(j));
                let m1 = vld1q_u64(mask.as_ptr().add(j + 2));
                let mut acc = vld1q_f32(sums.as_ptr().add(j));
                for (row, &bound) in rows.iter().zip(bounds) {
                    let p = row.as_ptr().add(j * 8);
                    let r0 = vreinterpretq_u64_u8(vld1q_u8(p));
                    let r1 = vreinterpretq_u64_u8(vld1q_u8(p.add(16)));
                    let held = narrow(vtstq_u64(r0, m0), vtstq_u64(r1, m1));
                    let v = vandq_u32(held, vdupq_n_u32(bound.to_bits()));
                    acc = vaddq_f32(acc, vreinterpretq_f32_u32(v));
                }
                vst1q_f32(sums.as_mut_ptr().add(j), acc);
                j += 4;
            }
        }
        scalar::word_tail(mask, rows, bounds, sums, full);
    }

    #[inline]
    pub fn member_sums(held: &[u64], bits: &[u8], bounds: &[f32], sums: &mut [f32]) {
        let n = bits.len();
        let full = n - n % 4;
        // SAFETY: NEON is baseline on aarch64; the loads and stores are of
        // four lanes at most `full` members in.
        unsafe {
            let one = vdupq_n_u64(1);
            let mut j = 0;
            while j < full {
                // Each member's bit as a one-bit mask: `1 << bits[j]`.
                let b = vmovl_u16(vget_low_u16(vmovl_u8(vld1_u8(bits.as_ptr().add(j).cast()))));
                let s0 = vreinterpretq_s64_u64(vmovl_u32(vget_low_u32(b)));
                let s1 = vreinterpretq_s64_u64(vmovl_u32(vget_high_u32(b)));
                let (b0, b1) = (vshlq_u64(one, s0), vshlq_u64(one, s1));
                let mut acc = vld1q_f32(sums.as_ptr().add(j));
                for (&h, &bound) in held.iter().zip(bounds) {
                    let hv = vdupq_n_u64(h);
                    let k = narrow(vtstq_u64(hv, b0), vtstq_u64(hv, b1));
                    let v = vandq_u32(k, vdupq_n_u32(bound.to_bits()));
                    acc = vaddq_f32(acc, vreinterpretq_f32_u32(v));
                }
                vst1q_f32(sums.as_mut_ptr().add(j), acc);
                j += 4;
            }
        }
        scalar::member_tail(held, bits, bounds, sums, full);
    }
}

/// AVX2 bodies: eight lanes of `f32` from two vectors of four words. A
/// shuffle takes the low halves of the 64-bit verdicts in the order
/// 0, 1, 4, 5, 2, 3, 6, 7 (within each 128-bit half); the sums are
/// permuted into that order on load and back on store.
#[cfg(target_arch = "x86_64")]
mod avx2 {
    use super::scalar;
    use core::arch::x86_64::*;

    #[target_feature(enable = "avx2")]
    #[inline]
    fn order() -> __m256i {
        _mm256_setr_epi32(0, 1, 4, 5, 2, 3, 6, 7)
    }

    /// The low halves of two vectors of 64-bit "no bit shared" verdicts as
    /// eight 32-bit ones, in [`order`].
    #[target_feature(enable = "avx2")]
    #[inline]
    fn narrow(z0: __m256i, z1: __m256i) -> __m256 {
        _mm256_shuffle_ps::<0b10_00_10_00>(_mm256_castsi256_ps(z0), _mm256_castsi256_ps(z1))
    }

    #[target_feature(enable = "avx2")]
    pub fn word_sums(mask: &[u64], rows: &[&[u8]], bounds: &[f32], sums: &mut [f32]) {
        let n = mask.len();
        let full = n - n % 8;
        let zero = _mm256_setzero_si256();
        // SAFETY: every load is of eight words (64 bytes) at most `full`
        // words in, which every row holds (checked by the caller), and
        // every load and store of sums of eight lanes.
        unsafe {
            let mut j = 0;
            while j < full {
                let m0 = _mm256_loadu_si256(mask.as_ptr().add(j).cast());
                let m1 = _mm256_loadu_si256(mask.as_ptr().add(j + 4).cast());
                let mut acc =
                    _mm256_permutevar8x32_ps(_mm256_loadu_ps(sums.as_ptr().add(j)), order());
                for (row, &bound) in rows.iter().zip(bounds) {
                    let p = row.as_ptr().add(j * 8);
                    let r0 = _mm256_loadu_si256(p.cast());
                    let r1 = _mm256_loadu_si256(p.add(32).cast());
                    let z0 = _mm256_cmpeq_epi64(_mm256_and_si256(r0, m0), zero);
                    let z1 = _mm256_cmpeq_epi64(_mm256_and_si256(r1, m1), zero);
                    let v = _mm256_andnot_ps(narrow(z0, z1), _mm256_set1_ps(bound));
                    acc = _mm256_add_ps(acc, v);
                }
                _mm256_storeu_ps(sums.as_mut_ptr().add(j), _mm256_permutevar8x32_ps(acc, order()));
                j += 8;
            }
        }
        scalar::word_tail(mask, rows, bounds, sums, full);
    }

    #[target_feature(enable = "avx2")]
    pub fn member_sums(held: &[u64], bits: &[u8], bounds: &[f32], sums: &mut [f32]) {
        let n = bits.len();
        let full = n - n % 8;
        let zero = _mm256_setzero_si256();
        let one = _mm256_set1_epi64x(1);
        // SAFETY: the loads of bit positions are of eight bytes, and the
        // loads and stores of sums of eight lanes, at most `full` members in.
        unsafe {
            let mut j = 0;
            while j < full {
                // Each member's bit as a one-bit mask: `1 << bits[j]`.
                let b = _mm_loadl_epi64(bits.as_ptr().add(j).cast());
                let b0 = _mm256_sllv_epi64(one, _mm256_cvtepu8_epi64(b));
                let b1 = _mm256_sllv_epi64(one, _mm256_cvtepu8_epi64(_mm_srli_si128::<4>(b)));
                let mut acc =
                    _mm256_permutevar8x32_ps(_mm256_loadu_ps(sums.as_ptr().add(j)), order());
                for (&h, &bound) in held.iter().zip(bounds) {
                    let hv = _mm256_set1_epi64x(h as i64);
                    let z0 = _mm256_cmpeq_epi64(_mm256_and_si256(hv, b0), zero);
                    let z1 = _mm256_cmpeq_epi64(_mm256_and_si256(hv, b1), zero);
                    let v = _mm256_andnot_ps(narrow(z0, z1), _mm256_set1_ps(bound));
                    acc = _mm256_add_ps(acc, v);
                }
                _mm256_storeu_ps(sums.as_mut_ptr().add(j), _mm256_permutevar8x32_ps(acc, order()));
                j += 8;
            }
        }
        scalar::member_tail(held, bits, bounds, sums, full);
    }
}

/// AVX-512 machines run the AVX2 bodies: eight lanes of `f32` are one
/// vector either way.
#[cfg(target_arch = "x86_64")]
mod avx512 {
    pub use super::avx2::{member_sums, word_sums};
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Bounds, with the values the walks never produce but `or_zero` must
    /// still add exactly.
    fn bound() -> impl Strategy<Value = f32> {
        prop_oneof![
            8 => 0.0_f32..40.0,
            1 => Just(0.0_f32),
            1 => Just(-0.0_f32),
            1 => Just(f32::INFINITY),
            1 => Just(f32::NAN),
            1 => Just(-1.5_f32),
            1 => Just(f32::MAX),
        ]
    }

    fn word(density: u32) -> impl Strategy<Value = u64> {
        any::<u64>().prop_map(move |x| match density {
            0 => 0,
            1 => x & x.rotate_left(17) & x.rotate_left(31),
            2 => x & x.rotate_left(7),
            _ => x,
        })
    }

    fn same(have: &[f32], want: &[f32]) -> bool {
        have.len() == want.len()
            && have
                .iter()
                .zip(want)
                .all(|(h, w)| h.to_bits() == w.to_bits() || (h.is_nan() && w.is_nan()))
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn word_sums_agree(
            words in 0usize..21,
            terms in 0usize..9,
            density in 0u32..4,
            seed in any::<u64>(),
            bounds in prop::collection::vec(bound(), 0..9),
            start in prop::collection::vec(bound(), 0..21),
            at in 0usize..8,
        ) {
            let terms = terms.min(bounds.len());
            let bounds = &bounds[..terms];
            let mut s = seed | 1;
            let mut next = || { s ^= s << 13; s ^= s >> 7; s ^= s << 17; s };
            let dense = |x: u64| match density { 0 => 0, 1 => x & x.rotate_left(17) & x.rotate_left(31), 2 => x & x.rotate_left(7), _ => x };
            let mask: Vec<u64> = (0..words).map(|_| dense(next())).collect();
            // Rows as bytes, each starting `at` bytes into its buffer (unaligned).
            let buffers: Vec<Vec<u8>> = (0..terms)
                .map(|_| {
                    let mut b = vec![0u8; at];
                    for _ in 0..words { b.extend_from_slice(&dense(next()).to_le_bytes()); }
                    b
                })
                .collect();
            let rows: Vec<&[u8]> = buffers.iter().map(|b| &b[at..]).collect();
            let mut want: Vec<f32> = (0..words).map(|j| start.get(j).copied().unwrap_or(0.0)).collect();
            let init = want.clone();
            scalar::word_sums(&mask, &rows, bounds, &mut want);
            for level in Level::supported_levels() {
                let mut have = init.clone();
                word_sums_at(level, &mask, &rows, bounds, &mut have);
                prop_assert!(same(&have, &want), "{:?}: {:?} vs {:?}", level, have, want);
            }
        }

        #[test]
        fn member_sums_agree(
            members in 0usize..40,
            held in prop::collection::vec(word(3), 0..9),
            bounds in prop::collection::vec(bound(), 0..9),
            seed in any::<u64>(),
            start in prop::collection::vec(bound(), 0..40),
        ) {
            let terms = held.len().min(bounds.len());
            let (held, bounds) = (&held[..terms], &bounds[..terms]);
            let mut s = seed | 1;
            let bits: Vec<u8> = (0..members).map(|_| { s ^= s << 13; s ^= s >> 7; s ^= s << 17; (s >> 32) as u8 % 64 }).collect();
            let mut want: Vec<f32> = (0..members).map(|j| start.get(j).copied().unwrap_or(0.0)).collect();
            let init = want.clone();
            scalar::member_sums(held, &bits, bounds, &mut want);
            for level in Level::supported_levels() {
                let mut have = init.clone();
                member_sums_at(level, held, &bits, bounds, &mut have);
                prop_assert!(same(&have, &want), "{:?}: {:?} vs {:?}", level, have, want);
            }
        }
    }

    #[test]
    fn or_zero_adds_what_the_branchy_select_adds() {
        let xs = [
            0.0_f32,
            -0.0,
            f32::MIN_POSITIVE,
            f32::from_bits(1),
            -f32::from_bits(1),
            f32::from_bits(0x007f_ffff),
            -f32::from_bits(0x007f_ffff),
            1.0,
            -1.5,
            f32::MAX,
            f32::MIN,
            f32::EPSILON,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            -f32::NAN,
            f32::from_bits(0x7f80_0001),
            f32::from_bits(0x7fbf_ffff),
            f32::from_bits(0xffc0_1234),
            f32::from_bits(0xff80_0001),
        ];
        for &x in &xs {
            for keep in [false, true] {
                let branchy = if keep { x } else { 0.0 };
                assert_eq!(or_zero(x, keep).to_bits(), branchy.to_bits(), "{x:?} {keep}");
                for &acc in &xs {
                    let want = acc + branchy;
                    let got = acc + or_zero(x, keep);
                    assert!(
                        got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()),
                        "{acc:?} + {x:?} {keep}: {got:?} vs {want:?}"
                    );
                }
            }
        }
    }
}
