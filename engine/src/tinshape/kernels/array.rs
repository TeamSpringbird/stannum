// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Array containers: a group's local slots as sorted little-endian `u16`s
//! ([`segment::tinshape::postings::KIND_ARRAY`]). Nothing to decode: a walk
//! widens them into its list (eight at a time on NEON and SSE2), sets them
//! in a row ([`super::rows`]), or probes them in place by galloping, where an Elias-Fano list
//! of the same members takes a setup and a member at a time below
//! [`super::ef::SHORT`] members (about 6 to 35 ns a container on the M4 at
//! 3 to 40 members, against 3 to 10 widened).
//!
//! The widening is inlined at every level, with no dispatch: on x86-64 by
//! SSE2, which every x86-64 CPU has. Most arrays a walk widens hold a few
//! members, and a call into an AVX2 body chosen at run time (a
//! `target_feature` function cannot be inlined into its caller) cost more
//! than the widening: about 17 ns an array under eight members on Sapphire
//! Rapids, against 5 on the M4.

use super::Level;

/// The value at index `i` of an array's bytes.
#[inline(always)]
fn at(bytes: &[u8], i: usize) -> u32 {
    u32::from(u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]))
}

/// Appends the array's values (`bytes.len() / 2` of them), in order.
#[inline]
pub fn array_decode(bytes: &[u8], out: &mut Vec<u32>) {
    let n = bytes.len() / 2;
    out.reserve(n + LANES);
    #[cfg(target_arch = "aarch64")]
    neon::decode(bytes, n, out);
    #[cfg(target_arch = "x86_64")]
    sse2::decode(bytes, n, out);
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    out.extend((0..n).map(|i| at(bytes, i)));
}

/// [`array_decode`] at `level`: the portable body at [`Level::Scalar`], else
/// the vector body (NEON, or SSE2 on x86-64 at any other level).
#[inline]
pub fn array_decode_at(level: Level, bytes: &[u8], out: &mut Vec<u32>) {
    if level == Level::Scalar {
        let n = bytes.len() / 2;
        out.extend((0..n).map(|i| at(bytes, i)));
    } else {
        array_decode(bytes, out);
    }
}

/// Values a vector body widens at a time (and may store past the end, into
/// the room [`array_decode`] reserves).
const LANES: usize = 8;

#[cfg(target_arch = "aarch64")]
mod neon {
    use core::arch::aarch64::*;

    #[inline(always)]
    pub fn decode(bytes: &[u8], n: usize, out: &mut Vec<u32>) {
        let start = out.len();
        let src = bytes.as_ptr();
        // SAFETY: each load reads 16 of the `2n` bytes (`i + 8 <= n`); the
        // stores fall within the `n + LANES` values reserved past `start`.
        // NEON is baseline on aarch64.
        unsafe {
            let dst = out.as_mut_ptr().add(start);
            let mut i = 0;
            while i + 8 <= n {
                let v = vld1q_u16(src.add(2 * i).cast());
                vst1q_u32(dst.add(i), vmovl_u16(vget_low_u16(v)));
                vst1q_u32(dst.add(i + 4), vmovl_high_u16(v));
                i += 8;
            }
            while i < n {
                dst.add(i).write(super::at(bytes, i));
                i += 1;
            }
            out.set_len(start + n);
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod sse2 {
    use core::arch::x86_64::*;

    #[inline(always)]
    pub fn decode(bytes: &[u8], n: usize, out: &mut Vec<u32>) {
        let start = out.len();
        let src = bytes.as_ptr();
        // SAFETY: as NEON's (each load 16 of the `2n` bytes, the stores
        // within the room reserved); SSE2 is part of x86-64. Interleaving
        // with zero widens a vector's low and high four values.
        unsafe {
            let dst = out.as_mut_ptr().add(start);
            let zero = _mm_setzero_si128();
            let mut i = 0;
            while i + 8 <= n {
                let v = _mm_loadu_si128(src.add(2 * i).cast());
                _mm_storeu_si128(dst.add(i).cast(), _mm_unpacklo_epi16(v, zero));
                _mm_storeu_si128(dst.add(i + 4).cast(), _mm_unpackhi_epi16(v, zero));
                i += 8;
            }
            while i < n {
                dst.add(i).write(super::at(bytes, i));
                i += 1;
            }
            out.set_len(start + n);
        }
    }
}

/// Sets bit `v` of `row` for each value `v` of the array, the row's bits
/// from its first value's on being clear (as a cleared row's are; see
/// [`super::rows`]). Panics if a value is past the row.
#[inline]
pub fn array_into_row(bytes: &[u8], row: &mut [u64]) {
    super::rows::set_ascending_u16(bytes, row);
}

/// The least index from `from` whose value is at least `target`, or the
/// array's length: galloping from `from`, then a binary search of the last
/// step, without a branch per halving.
#[inline]
pub fn array_seek(bytes: &[u8], from: usize, target: u32) -> usize {
    let n = bytes.len() / 2;
    if from >= n || at(bytes, from) >= target {
        return from.min(n);
    }
    // `lo` holds a value below the target; `hi` one at least it, or `n`.
    let (mut lo, mut step) = (from, 1usize);
    let mut hi = from + 1;
    while hi < n && at(bytes, hi) < target {
        lo = hi;
        step *= 2;
        hi = lo + step;
    }
    let hi = hi.min(n);
    // The answer lies in `lo + 1 ..= hi`.
    let (mut base, mut len) = (lo + 1, hi - lo);
    while len > 1 {
        let half = len / 2;
        let probe = base + half - 1;
        base = if at(bytes, probe) < target {
            base + half
        } else {
            base
        };
        len -= half;
    }
    base
}

/// Keeps the candidates (ascending, without repeats) the array holds, by
/// galloping each from where the last one ended: the walk's path for an
/// array much longer than its candidates.
#[inline]
pub fn array_retain(bytes: &[u8], cands: &mut Vec<u32>) {
    let n = bytes.len() / 2;
    let (mut from, mut kept) = (0usize, 0usize);
    for r in 0..cands.len() {
        let c = cands[r];
        from = array_seek(bytes, from, c);
        cands[kept] = c;
        kept += usize::from(from < n && at(bytes, from) == c);
    }
    cands.truncate(kept);
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn bytes_of(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|v| (*v as u16).to_le_bytes())
            .collect()
    }

    fn sorted(mut v: Vec<u16>) -> Vec<u32> {
        v.sort_unstable();
        v.dedup();
        v.into_iter().map(u32::from).collect()
    }

    proptest! {
        /// Every level widens as the portable body does, after values
        /// already in the vector.
        #[test]
        fn decode_at_every_level(values in proptest::collection::vec(any::<u16>(), 0..80)) {
            let values = sorted(values);
            let bytes = bytes_of(&values);
            for level in Level::supported_levels() {
                let mut out = vec![7u32];
                array_decode_at(level, &bytes, &mut out);
                prop_assert_eq!(&out[1..], &values[..], "{}", level.name());
                prop_assert_eq!(out[0], 7);
            }
        }

        /// Seeking finds the least index at or above the target from any
        /// start, and retaining keeps the candidates the array holds.
        #[test]
        fn seek_and_retain(
            values in proptest::collection::vec(any::<u16>(), 0..200),
            cands in proptest::collection::vec(any::<u16>(), 0..60),
            from in 0usize..210,
            target in any::<u16>(),
        ) {
            let values = sorted(values);
            let bytes = bytes_of(&values);
            let want = (from..values.len())
                .find(|&i| values[i] >= u32::from(target))
                .unwrap_or(values.len())
                .max(from.min(values.len()));
            prop_assert_eq!(array_seek(&bytes, from, u32::from(target)), want);
            let mut cands = sorted(cands);
            let kept: Vec<u32> = cands
                .iter()
                .copied()
                .filter(|c| values.binary_search(c).is_ok())
                .collect();
            array_retain(&bytes, &mut cands);
            prop_assert_eq!(cands, kept);
            let mut row = vec![0u64; 1024];
            array_into_row(&bytes, &mut row);
            let set: Vec<u32> = (0..65536u32)
                .filter(|v| row[*v as usize / 64] >> (v % 64) & 1 == 1)
                .collect();
            prop_assert_eq!(set, values);
        }
    }
}
