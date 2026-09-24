// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Score bounds over a run of a term's documents.

use crate::tf_bucket::BUCKET_COUNT;

/// The tightest statement a scorer can make about every document in a run:
/// per term-frequency bucket, the shortest document with that bucket. BM25
/// rises with the bucket and falls with the length, so the best score any
/// member can reach is the best over the occupied buckets at their minima.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockBound {
    /// Per term-frequency bucket, the shortest document in the run with that
    /// bucket; `u32::MAX` where the bucket does not occur.
    pub min_len: [u32; BUCKET_COUNT],
}

impl BlockBound {
    /// A bound over nothing: no bucket occurs.
    pub const EMPTY: Self = Self {
        min_len: [u32::MAX; BUCKET_COUNT],
    };

    /// The buckets that occur with their shortest document.
    pub fn buckets(&self) -> impl Iterator<Item = (u8, u32)> + '_ {
        self.min_len
            .iter()
            .enumerate()
            .filter(|(_, len)| **len != u32::MAX)
            .map(|(bucket, len)| (bucket as u8, *len))
    }

    /// Largest bucket that occurs.
    pub fn max_tf_bucket(&self) -> u8 {
        self.buckets().map(|(bucket, _)| bucket).max().unwrap_or(0)
    }

    /// Shortest document in the run.
    pub fn shortest(&self) -> u32 {
        self.min_len.iter().copied().min().unwrap_or(u32::MAX)
    }

    /// The tighter of two bounds' minima per bucket, covering both runs.
    pub fn merge(&self, other: &Self) -> Self {
        let mut min_len = self.min_len;
        for (mine, theirs) in min_len.iter_mut().zip(&other.min_len) {
            *mine = (*mine).min(*theirs);
        }
        Self { min_len }
    }

    /// Adds one document with `bucket` and `len`. A length of `u32::MAX` is
    /// recorded one shorter, which only loosens the bound, so the value can
    /// mark absent buckets.
    pub fn add(&mut self, bucket: u8, len: u32) {
        let slot = &mut self.min_len[usize::from(bucket)];
        *slot = (*slot).min(len.min(u32::MAX - 1));
    }

    /// The bound over documents given as (bucket, document length).
    pub fn over(documents: &[(u8, u32)]) -> Self {
        let mut bound = Self::EMPTY;
        for (bucket, len) in documents {
            bound.add(*bucket, *len);
        }
        bound
    }
}
