// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! `stannum.debug_bound_estimate`: what finer stored sub-block bounds would
//! save a ranked walk, measured without changing the walk.
//!
//! A chunk bound pairs each sub-block's largest bucket with the shortest
//! document of that bucket over the whole chunk, so a sub-block's bound is
//! usually far above any of its members. When the setting is on, the walk
//! runs exactly as it does otherwise, and for every sub-block it evaluates
//! it also computes, from the loaded chunk's members, the bound each of
//! these alternatives would have stored:
//!
//! - `A`: per sub-block per term, the shortest member (a `u32`) with the
//!   existing largest bucket;
//! - `A8`: the same, the length as a one-byte length class;
//! - `B`: per sub-block per term, the shortest member per bucket, the
//!   chunk's table at sub-block granularity; this is the sub-block's exact
//!   maximum contribution;
//! - `C`: that maximum, quantized upward to one byte on a log scale over
//!   the term's maximum score.
//!
//! It then counts the sub-blocks, chunks and scored candidates each would
//! have skipped at the threshold the walk actually had at that moment. A
//! valid upper bound never skips a document that enters the top k, so the
//! threshold sequence, and the counts, are those the alternative would see.

use std::cell::Cell;

use segment::length_class::{class_of, min_length};
use segment::ordinals::{SUB, SUBS, Words};
use segment::tf_bucket::{BUCKET_COUNT, TfBucket};

use crate::bm25::TermScorer;

/// The alternatives, in report order.
pub(crate) const ALTS: usize = 4;
pub(crate) const ALT_NAMES: [&str; ALTS] = ["A", "A8", "B", "C"];
const A: usize = 0;
const A8: usize = 1;
const B: usize = 2;
const C: usize = 3;

/// The ratio between adjacent representable values of alternative `C`:
/// 255 steps of 3.5% span four orders of magnitude below the term maximum.
const C_STEP: f64 = 0.965;

#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct Counters {
    /// Chunks whose members were tabled.
    pub chunks: i64,
    /// (chunk, term) pairs tabled.
    pub chunk_terms: i64,
    /// Over those pairs, sub-blocks holding a member of the term: the unit
    /// every alternative pays per.
    pub occupied_subs: i64,
    /// Sub-blocks the walk evaluated while pruning, with members.
    pub subs: i64,
    /// Candidates the walk scored that each alternative would have skipped.
    pub scored_skipped: [i64; ALTS],
    /// Evaluated sub-blocks each alternative would have skipped whole.
    pub subs_skipped: [i64; ALTS],
    /// Tabled chunks no sub-block of which passes under each alternative:
    /// they would not have been loaded.
    pub chunks_skipped: [i64; ALTS],
    /// Bytes each alternative would add over the tabled pairs.
    pub bytes: [i64; ALTS],
    /// Sub-blocks whose stored largest bucket disagreed with the members:
    /// a check of the estimator against the format, always zero.
    pub mismatches: i64,
}

thread_local! {
    static COUNTERS: Cell<Counters> = const { Cell::new(Counters {
        chunks: 0,
        chunk_terms: 0,
        occupied_subs: 0,
        subs: 0,
        scored_skipped: [0; ALTS],
        subs_skipped: [0; ALTS],
        chunks_skipped: [0; ALTS],
        bytes: [0; ALTS],
        mismatches: 0,
    }) };
}

pub(crate) fn reset() {
    COUNTERS.set(Counters::default());
}

pub(crate) fn counters() -> Counters {
    COUNTERS.get()
}

pub(crate) fn bump(update: impl FnOnce(&mut Counters)) {
    let mut counters = COUNTERS.get();
    update(&mut counters);
    COUNTERS.set(counters);
}

/// One term's members of one chunk, summarized per sub-block.
pub(crate) struct TermStats {
    /// One past the largest bucket; zero where the term has no member.
    pub max_bucket: [u8; SUBS],
    /// The shortest member; `u32::MAX` where there is none.
    pub min_len: [u32; SUBS],
    /// The shortest member per bucket; `u32::MAX` where the bucket is absent.
    pub min_len_by_bucket: Box<[[u32; BUCKET_COUNT]; SUBS]>,
}

impl TermStats {
    /// Summarizes the members set in `words`, ranked from `rank_base` in
    /// ascending order, with `bucket_of(rank)` and `len_of(low)`.
    pub(crate) fn gather(
        words: &Words,
        rank_base: u32,
        mut bucket_of: impl FnMut(u32) -> u8,
        mut len_of: impl FnMut(u32) -> u32,
    ) -> Self {
        let mut stats = Self {
            max_bucket: [0; SUBS],
            min_len: [u32::MAX; SUBS],
            min_len_by_bucket: Box::new([[u32::MAX; BUCKET_COUNT]; SUBS]),
        };
        let mut rank = rank_base;
        for (i, word) in words.iter().enumerate() {
            let mut word = *word;
            while word != 0 {
                let low = (i * 64) as u32 + word.trailing_zeros();
                word &= word - 1;
                let bucket = bucket_of(rank);
                let len = len_of(low);
                rank += 1;
                let sub = (low / SUB) as usize;
                stats.max_bucket[sub] = stats.max_bucket[sub].max(bucket + 1);
                stats.min_len[sub] = stats.min_len[sub].min(len);
                let slot = &mut stats.min_len_by_bucket[sub][usize::from(bucket)];
                *slot = (*slot).min(len);
            }
        }
        stats
    }

    /// Bytes each alternative would store for this term's chunk, and the
    /// sub-blocks it occupies.
    fn bytes(&self) -> ([i64; ALTS], i64) {
        let mut bytes = [0i64; ALTS];
        let mut occupied = 0;
        for sub in 0..SUBS {
            if self.max_bucket[sub] == 0 {
                continue;
            }
            occupied += 1;
            bytes[A] += 4;
            bytes[A8] += 1;
            bytes[C] += 1;
            // A bucket mask, then a varint per occupied bucket.
            bytes[B] += 2;
            for len in self.min_len_by_bucket[sub] {
                if len != u32::MAX {
                    bytes[B] += varint_len(u64::from(len));
                }
            }
        }
        (bytes, occupied)
    }
}

fn varint_len(value: u64) -> i64 {
    let bits = 64 - value.leading_zeros().min(63);
    i64::from(bits.div_ceil(7).max(1))
}

/// `exact` rounded up to a representable value of alternative `C`: the
/// term's maximum scaled by a power of [`C_STEP`], the largest power at or
/// above the ratio. Never below `exact`, and never above the maximum.
pub(crate) fn quantize_c(exact: f32, term_max: f32) -> f32 {
    if exact <= 0.0 || term_max <= 0.0 {
        return 0.0;
    }
    if exact >= term_max {
        return term_max;
    }
    let ratio = f64::from(exact) / f64::from(term_max);
    let steps = (ratio.ln() / C_STEP.ln()).floor().clamp(0.0, 254.0) as i32;
    let mut value = (f64::from(term_max) * C_STEP.powi(steps)) as f32;
    while value < exact {
        value = value.next_up();
    }
    value.min(term_max)
}

/// The alternatives' bounds for one term over one chunk's sub-blocks.
pub(crate) struct AltTables {
    pub bounds: [[f32; SUBS]; ALTS],
}

/// The alternative bounds per present term. `conjunction` carries the
/// chunk-level shared length floor the walk bounded with; under a
/// conjunction every term's member is one document, so the shortest
/// member per sub-block over the terms floors every term's bound where an
/// alternative stores lengths, and `C`, which stores none, keeps today's
/// bound as a ceiling.
pub(crate) fn tables(
    stats: &[TermStats],
    scorers: &[&TermScorer],
    term_max: &[f32],
    conjunction: Option<u32>,
) -> Vec<AltTables> {
    let mut floors_a = [0u32; SUBS];
    let mut floors_a8 = [0u32; SUBS];
    if conjunction.is_some() {
        for sub in 0..SUBS {
            for term in stats {
                if term.max_bucket[sub] != 0 {
                    floors_a[sub] = floors_a[sub].max(term.min_len[sub]);
                    floors_a8[sub] = floors_a8[sub].max(min_length(class_of(term.min_len[sub])));
                }
            }
        }
    }
    let mut out = Vec::with_capacity(stats.len());
    for ((term, scorer), max) in stats.iter().zip(scorers).zip(term_max) {
        let mut bounds = [[0.0_f32; SUBS]; ALTS];
        for sub in 0..SUBS {
            if term.max_bucket[sub] == 0 {
                continue;
            }
            let top = TfBucket::new(term.max_bucket[sub] - 1).expect("a member's bucket");
            let min_len = term.min_len[sub];
            bounds[A][sub] = scorer.score_bucket(top, min_len.max(floors_a[sub]));
            bounds[A8][sub] =
                scorer.score_bucket(top, min_length(class_of(min_len)).max(floors_a8[sub]));
            let mut exact = 0.0_f32;
            let mut b = 0.0_f32;
            for (bucket, len) in term.min_len_by_bucket[sub].iter().enumerate() {
                if *len == u32::MAX {
                    continue;
                }
                let bucket = TfBucket::new(bucket as u8).expect("bucket within the count");
                exact = exact.max(scorer.score_bucket(bucket, *len));
                b = b.max(scorer.score_bucket(bucket, (*len).max(floors_a[sub])));
            }
            bounds[B][sub] = b;
            let mut c = quantize_c(exact, *max);
            if let Some(floor) = conjunction {
                c = c.min(scorer.score_bucket(top, floor));
            }
            bounds[C][sub] = c;
        }
        out.push(AltTables { bounds });
    }
    let mut bytes = [0i64; ALTS];
    let mut occupied = 0;
    for term in stats {
        let (mine, subs) = term.bytes();
        for (total, b) in bytes.iter_mut().zip(mine) {
            *total += b;
        }
        occupied += subs;
    }
    bump(|c| {
        c.chunks += 1;
        c.chunk_terms += stats.len() as i64;
        c.occupied_subs += occupied;
        for (total, b) in c.bytes.iter_mut().zip(bytes) {
            *total += b;
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantized_c_covers_and_stays_within_a_step() {
        let max = 7.25_f32;
        let mut exact = max;
        while exact > 1e-5 {
            let q = quantize_c(exact, max);
            assert!(q >= exact, "{exact} -> {q}");
            assert!(q <= max);
            assert!(
                exact < max * 1e-4 || q <= exact / C_STEP as f32 * 1.0001,
                "{exact} -> {q}"
            );
            exact *= 0.9;
        }
        assert_eq!(quantize_c(0.0, max), 0.0);
        assert_eq!(quantize_c(max * 2.0, max), max);
    }

    #[test]
    fn varint_lengths() {
        assert_eq!(varint_len(0), 1);
        assert_eq!(varint_len(127), 1);
        assert_eq!(varint_len(128), 2);
        assert_eq!(varint_len(1 << 14), 3);
    }
}
