// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! BM25 scores of (bucket, length) pairs, lanes at a time: a footer's
//! blocks are bounded by the score of each frontier pair, one `f32`
//! division per pair, a million pairs in a `full_score` disjunction at 150M
//! rows. Each lane runs the scorer's own operations in its own order
//! (`numerator[b] / (denominator[b] + factor * length)`, each correctly
//! rounded, no fused multiply-add), so the scores are bit for bit
//! [`crate::bm25::TermScorer::score_bucket`]'s; a vector division retires
//! eight of them in the time of one.

use super::{Level, level};
use segment::tf_bucket::BUCKET_COUNT;

/// Per pair `i`, `num[buckets[i]] / (den[buckets[i]] + factor * lengths[i] as f32)`
/// into `out[i]`. Buckets are below [`BUCKET_COUNT`].
#[inline]
pub fn score_pairs(
    num: &[f32; BUCKET_COUNT],
    den: &[f32; BUCKET_COUNT],
    factor: f32,
    buckets: &[u8],
    lengths: &[u32],
    out: &mut [f32],
) {
    check(buckets, lengths, out);
    run!(level(), score_pairs(num, den, factor, buckets, lengths, out))
}

/// [`score_pairs`] at `level`; panics if the CPU lacks it.
pub fn score_pairs_at(
    level: Level,
    num: &[f32; BUCKET_COUNT],
    den: &[f32; BUCKET_COUNT],
    factor: f32,
    buckets: &[u8],
    lengths: &[u32],
    out: &mut [f32],
) {
    assert!(level.supported(), "kernel level {} unsupported here", level.name());
    check(buckets, lengths, out);
    run!(level, score_pairs(num, den, factor, buckets, lengths, out))
}

#[inline(always)]
fn check(buckets: &[u8], lengths: &[u32], out: &mut [f32]) {
    assert_eq!(buckets.len(), lengths.len(), "a length per bucket");
    assert_eq!(buckets.len(), out.len(), "a score per pair");
    assert!(
        buckets.iter().all(|b| usize::from(*b) < BUCKET_COUNT),
        "valid buckets"
    );
}

/// The portable body: the scorer's expression, a pair at a time.
mod scalar {
    use segment::tf_bucket::BUCKET_COUNT;

    /// Pairs `from..`, one at a time.
    #[inline(always)]
    pub fn tail(
        num: &[f32; BUCKET_COUNT],
        den: &[f32; BUCKET_COUNT],
        factor: f32,
        buckets: &[u8],
        lengths: &[u32],
        out: &mut [f32],
        from: usize,
    ) {
        for i in from..buckets.len() {
            let b = usize::from(buckets[i]);
            out[i] = num[b] / (den[b] + factor * lengths[i] as f32);
        }
    }

    #[inline(always)]
    pub fn score_pairs(
        num: &[f32; BUCKET_COUNT],
        den: &[f32; BUCKET_COUNT],
        factor: f32,
        buckets: &[u8],
        lengths: &[u32],
        out: &mut [f32],
    ) {
        tail(num, den, factor, buckets, lengths, out, 0)
    }
}

/// NEON body: four pairs a pass, the tables read a lane at a time.
#[cfg(target_arch = "aarch64")]
mod neon {
    use super::scalar;
    use core::arch::aarch64::*;
    use segment::tf_bucket::BUCKET_COUNT;

    #[inline]
    pub fn score_pairs(
        num: &[f32; BUCKET_COUNT],
        den: &[f32; BUCKET_COUNT],
        factor: f32,
        buckets: &[u8],
        lengths: &[u32],
        out: &mut [f32],
    ) {
        let n = buckets.len();
        let full = n - n % 4;
        // SAFETY: NEON is baseline on aarch64; the loads and stores are of
        // four lanes at most `full` pairs in; buckets index the tables
        // (checked by the caller).
        unsafe {
            let f = vdupq_n_f32(factor);
            let mut i = 0;
            while i < full {
                let b = &buckets[i..i + 4];
                let (b0, b1, b2, b3) = (usize::from(b[0]), usize::from(b[1]), usize::from(b[2]), usize::from(b[3]));
                let nv = vld1q_f32([num[b0], num[b1], num[b2], num[b3]].as_ptr());
                let dv = vld1q_f32([den[b0], den[b1], den[b2], den[b3]].as_ptr());
                let l = vcvtq_f32_u32(vld1q_u32(lengths.as_ptr().add(i)));
                let d = vaddq_f32(dv, vmulq_f32(f, l));
                vst1q_f32(out.as_mut_ptr().add(i), vdivq_f32(nv, d));
                i += 4;
            }
        }
        scalar::tail(num, den, factor, buckets, lengths, out, full);
    }
}

/// AVX2 body: eight pairs a pass, the tables read by two permutes and a
/// blend; a length of 2^31 or more (none on any corpus, but a signed
/// conversion would miss it) sends the pass to the portable body.
#[cfg(target_arch = "x86_64")]
mod avx2 {
    use super::scalar;
    use core::arch::x86_64::*;
    use segment::tf_bucket::BUCKET_COUNT;

    /// `table[idx]` per lane of `idx` (each below 16).
    #[target_feature(enable = "avx2")]
    #[inline]
    fn gather(table: &[f32; BUCKET_COUNT], idx: __m256i) -> __m256 {
        // SAFETY: the two loads read the table's 16 values.
        let (lo, hi) = unsafe {
            (
                _mm256_loadu_ps(table.as_ptr()),
                _mm256_loadu_ps(table.as_ptr().add(8)),
            )
        };
        let from_hi = _mm256_castsi256_ps(_mm256_cmpgt_epi32(idx, _mm256_set1_epi32(7)));
        _mm256_blendv_ps(
            _mm256_permutevar8x32_ps(lo, idx),
            _mm256_permutevar8x32_ps(hi, idx),
            from_hi,
        )
    }

    #[target_feature(enable = "avx2")]
    pub fn score_pairs(
        num: &[f32; BUCKET_COUNT],
        den: &[f32; BUCKET_COUNT],
        factor: f32,
        buckets: &[u8],
        lengths: &[u32],
        out: &mut [f32],
    ) {
        let n = buckets.len();
        let full = n - n % 8;
        let f = _mm256_set1_ps(factor);
        // SAFETY: the loads are of eight buckets and eight lengths, the
        // stores of eight scores, at most `full` pairs in.
        unsafe {
            let mut i = 0;
            while i < full {
                let l = _mm256_loadu_si256(lengths.as_ptr().add(i).cast());
                if _mm256_movemask_ps(_mm256_castsi256_ps(l)) != 0 {
                    scalar::tail(num, den, factor, &buckets[..i + 8], lengths, out, i);
                    i += 8;
                    continue;
                }
                let idx = _mm256_cvtepu8_epi32(_mm_loadl_epi64(buckets.as_ptr().add(i).cast()));
                let d = _mm256_add_ps(gather(den, idx), _mm256_mul_ps(f, _mm256_cvtepi32_ps(l)));
                _mm256_storeu_ps(out.as_mut_ptr().add(i), _mm256_div_ps(gather(num, idx), d));
                i += 8;
            }
        }
        scalar::tail(num, den, factor, buckets, lengths, out, full);
    }
}

/// AVX-512 machines run the AVX2 body.
#[cfg(target_arch = "x86_64")]
mod avx512 {
    pub use super::avx2::score_pairs;
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn table() -> impl Strategy<Value = [f32; BUCKET_COUNT]> {
        prop::array::uniform16(prop_oneof![
            8 => 0.0_f32..100.0,
            1 => Just(0.0_f32),
            1 => Just(f32::INFINITY),
            1 => Just(f32::NAN),
            1 => Just(f32::MAX),
        ])
    }

    fn length() -> impl Strategy<Value = u32> {
        prop_oneof![
            8 => 0u32..100_000,
            1 => any::<u32>(),
            1 => Just(u32::MAX),
            1 => Just(1u32 << 31),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn scores_agree(
            num in table(),
            den in table(),
            factor in prop_oneof![8 => 0.0_f32..1.0, 1 => Just(0.0_f32), 1 => Just(f32::INFINITY)],
            pairs in prop::collection::vec((0u8..16, length()), 0..40),
        ) {
            let buckets: Vec<u8> = pairs.iter().map(|p| p.0).collect();
            let lengths: Vec<u32> = pairs.iter().map(|p| p.1).collect();
            let mut want = vec![0.0_f32; pairs.len()];
            scalar::score_pairs(&num, &den, factor, &buckets, &lengths, &mut want);
            for level in Level::supported_levels() {
                let mut have = vec![0.0_f32; pairs.len()];
                score_pairs_at(level, &num, &den, factor, &buckets, &lengths, &mut have);
                for (i, (h, w)) in have.iter().zip(&want).enumerate() {
                    prop_assert!(
                        h.to_bits() == w.to_bits() || (h.is_nan() && w.is_nan()),
                        "{:?} pair {}: {:?} vs {:?}", level, i, h, w
                    );
                }
            }
        }
    }
}
