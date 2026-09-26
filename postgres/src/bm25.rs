// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

//! Standalone scoring policy and production-compatible BM25 arithmetic.

use std::collections::BTreeMap;

use rustc_hash::FxHashSet;
use segment::bound::BlockBound;
use thiserror::Error;

use crate::tf_bucket::{BUCKET_COUNT, TfBucket};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Bm25Params {
    pub(crate) k1: f32,
    pub(crate) b: f32,
}

impl Bm25Params {
    pub(crate) const DEFAULT_K1: f32 = 1.2;
    pub(crate) const DEFAULT_B: f32 = 0.75;
    pub(crate) const K1_MAX: f32 = 1.0e4;

    #[must_use]
    pub(crate) const fn default_bm25() -> Self {
        Self {
            k1: Self::DEFAULT_K1,
            b: Self::DEFAULT_B,
        }
    }

    #[must_use]
    pub(crate) fn is_valid(self) -> bool {
        (0.0..=Self::K1_MAX).contains(&self.k1) && (0.0..=1.0).contains(&self.b)
    }

    pub(crate) fn checked(self) -> Result<Self, Bm25Error> {
        self.is_valid().then_some(self).ok_or(Bm25Error::Parameters)
    }
}

impl Default for Bm25Params {
    fn default() -> Self {
        Self::default_bm25()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Bm25Overrides {
    pub(crate) k1: Option<f32>,
    pub(crate) b: Option<f32>,
}

impl Bm25Overrides {
    #[must_use]
    pub(crate) fn resolve(self, defaults: Bm25Params) -> Bm25Params {
        Bm25Params {
            k1: self.k1.unwrap_or(defaults.k1),
            b: self.b.unwrap_or(defaults.b),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DenseRatio(f32);

impl DenseRatio {
    pub(crate) const DEFAULT: f32 = 0.10;

    #[must_use]
    pub(crate) fn new(value: Option<f32>) -> Self {
        Self(value.unwrap_or(Self::DEFAULT))
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) const fn value(self) -> f32 {
        self.0
    }

    #[must_use]
    pub(crate) fn is_valid(self) -> bool {
        self.0.is_finite() && self.0 >= 0.0
    }

    /// Uses immutable-only statistics and the production `f64` comparison.
    #[must_use]
    pub(crate) fn elides(self, pinned: bool, immutable_df: u64, immutable_docs: u64) -> bool {
        !pinned
            && immutable_df > 0
            && immutable_df as f64 >= f64::from(self.0) * immutable_docs as f64
    }
}

#[derive(Debug)]
pub(crate) struct ScoreStopWords<'a> {
    terms: FxHashSet<&'a str>,
}

impl<'a> ScoreStopWords<'a> {
    #[must_use]
    pub(crate) fn from_csv(csv: &'a str) -> Option<Self> {
        let terms = csv
            .split(',')
            .map(str::trim)
            .filter(|term| !term.is_empty())
            .collect::<FxHashSet<_>>();
        (!terms.is_empty()).then_some(Self { terms })
    }

    #[must_use]
    pub(crate) fn contains(&self, term: &str) -> bool {
        self.terms.contains(term)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum TermSetEdit {
    #[default]
    None,
    Add(Vec<String>),
    Replace(Vec<String>),
}

impl TermSetEdit {
    pub(crate) fn from_bound_arrays(
        add: Option<Vec<String>>,
        replace: Option<Vec<String>>,
    ) -> Result<Self, TermSetEditError> {
        match (add, replace) {
            (Some(_), Some(_)) => Err(TermSetEditError::ConflictingEdits),
            (Some(terms), None) => Ok(Self::normalize(false, terms)),
            (None, Some(terms)) => Ok(Self::normalize(true, terms)),
            (None, None) => Ok(Self::None),
        }
    }

    /// Applies the caller's index tokenizer to raw edit elements.
    #[must_use]
    pub(crate) fn analyzed_with<F, I>(&self, mut analyze: F) -> Self
    where
        F: FnMut(&str) -> I,
        I: IntoIterator<Item = String>,
    {
        let (replace, raw) = match self {
            Self::None => return Self::None,
            Self::Add(raw) => (false, raw),
            Self::Replace(raw) => (true, raw),
        };
        let terms = raw.iter().flat_map(|element| analyze(element)).collect();
        Self::normalize(replace, terms)
    }

    fn normalize(replace: bool, terms: Vec<String>) -> Self {
        if terms.is_empty() {
            Self::None
        } else if replace {
            Self::Replace(terms)
        } else {
            Self::Add(terms)
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum TermSetEditError {
    #[error("term_add and term_replace cannot both be non-NULL")]
    ConflictingEdits,
}

/// One occurrence collected from the lowered query before term-set edits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScoringTermInput<'a> {
    pub(crate) text: &'a str,
    pub(crate) boost: f32,
    /// True for an explicit boost node, including an explicit `^1.0`.
    pub(crate) explicitly_boosted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ScoringTerm {
    text: String,
    boost: f32,
    pinned: bool,
}

impl ScoringTerm {
    #[must_use]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub(crate) const fn boost(&self) -> f32 {
        self.boost
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) const fn pinned(&self) -> bool {
        self.pinned
    }

    /// Terms absent from every source never score. Dense filtering is optional
    /// so `full_score` can retain the complete, non-stopped program.
    #[must_use]
    pub(crate) fn is_retained(
        &self,
        total_df: u64,
        immutable_df: u64,
        immutable_docs: u64,
        dense_ratio: Option<DenseRatio>,
    ) -> bool {
        total_df > 0
            && dense_ratio
                .is_none_or(|ratio| !ratio.elides(self.pinned, immutable_df, immutable_docs))
    }
}

/// Applies stop words and analyzed term-set edits, returning lexical order.
/// Repeated query occurrences add their boosts. Edit terms are idempotent,
/// pin existing query terms without changing their weight, and enter at 1.0.
#[must_use]
pub(crate) fn compile_scoring_terms<'query>(
    query_terms: impl IntoIterator<Item = ScoringTermInput<'query>>,
    edit: &TermSetEdit,
    stop_words: Option<&ScoreStopWords<'_>>,
) -> Vec<ScoringTerm> {
    let mut terms = BTreeMap::<String, ScoringTerm>::new();

    if !matches!(edit, TermSetEdit::Replace(_)) {
        for input in query_terms {
            if stop_words.is_some_and(|stop| stop.contains(input.text)) {
                continue;
            }
            let entry = terms
                .entry(input.text.to_owned())
                .or_insert_with(|| ScoringTerm {
                    text: input.text.to_owned(),
                    boost: 0.0,
                    pinned: false,
                });
            entry.boost += input.boost;
            entry.pinned |= input.explicitly_boosted;
        }
    }

    let edited_terms = match edit {
        TermSetEdit::None => &[][..],
        TermSetEdit::Add(terms) | TermSetEdit::Replace(terms) => terms,
    };
    for text in edited_terms {
        if stop_words.is_some_and(|stop| stop.contains(text)) {
            continue;
        }
        let entry = terms.entry(text.clone()).or_insert_with(|| ScoringTerm {
            text: text.clone(),
            boost: 1.0,
            pinned: true,
        });
        entry.pinned = true;
    }

    terms.into_values().collect()
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum Bm25Error {
    #[error("invalid BM25 parameters")]
    Parameters,
    #[error("average document length must be finite and positive")]
    AverageDocumentLength,
    #[error("term boost must be finite and non-negative")]
    Boost,
}

/// Canonical production IDF: `ln(1 + (N - df + 0.5) / (df + 0.5))`.
/// Stale statistics with `df > N` are floored at zero.
#[must_use]
pub(crate) fn bm25_idf(total_docs: u64, df: u64) -> f64 {
    let inner = 1.0 + (total_docs as f64 - df as f64 + 0.5) / (df as f64 + 0.5);
    if inner <= 1.0 { 0.0 } else { inner.ln() }
}

/// Precomputed one-term scorer. Its field construction and score expression
/// deliberately preserve production's `f32` operation order.
#[derive(Clone, Debug)]
pub(crate) struct TermScorer {
    numerator: [f32; BUCKET_COUNT],
    denominator_constant: [f32; BUCKET_COUNT],
    document_length_factor: f32,
    /// Whether the score is proven never to fall as the bucket rises, at
    /// every document length; see [`rises_with_the_bucket`].
    rises: bool,
}

/// Whether `numerator[i] / (denominator[i] + x)`, each sum and the quotient
/// rounded to `f32`, never falls from one bucket to the next for any
/// length term `x >= 0` (itself an `f32`, shared by both buckets).
///
/// For buckets `i < j` with numerators `n`, `n'` and denominators `d`,
/// `d'`, rounding is monotonic, so the scores keep their order when the
/// unrounded quotients `n' / fl(d' + x)` and `n / fl(d + x)` do. Each
/// rounded sum lies within a factor `1 ± u` of the exact one, `u = 2^-24`,
/// so it suffices that `n' (d + x)(1 - u) >= n (d' + x)(1 + u)`: linear in
/// `x`, it holds for every `x >= 0` when it holds at `x = 0` and its slope
/// is not negative, that is when `n' d (1 - u) >= n d' (1 + u)` and
/// `n' (1 - u) >= n (1 + u)`. Products of two `f32` are exact in `f64`; the
/// margin of four `u` absorbs the `f64` rounding of the differences. A
/// numerator that is not finite fails the test, and the caller falls back
/// to the running best.
fn rises_with_the_bucket(
    numerator: &[f32; BUCKET_COUNT],
    denominator: &[f32; BUCKET_COUNT],
) -> bool {
    let u = f64::from(f32::EPSILON) / 2.0;
    let keeps = |higher: f64, lower: f64| higher - lower >= 4.0 * u * (higher + lower);
    numerator
        .windows(2)
        .zip(denominator.windows(2))
        .all(|(n, d)| {
            let (lower_n, higher_n) = (f64::from(n[0]), f64::from(n[1]));
            let (lower_d, higher_d) = (f64::from(d[0]), f64::from(d[1]));
            keeps(higher_n * lower_d, lower_n * higher_d) && keeps(higher_n, lower_n)
        })
}

impl TermScorer {
    pub(crate) fn from_statistics(
        total_docs: u64,
        df: u64,
        boost: f32,
        params: Bm25Params,
        average_document_length: f32,
    ) -> Result<Self, Bm25Error> {
        Self::new(
            bm25_idf(total_docs, df) as f32,
            boost,
            params,
            average_document_length,
        )
    }

    pub(crate) fn new(
        idf: f32,
        boost: f32,
        params: Bm25Params,
        average_document_length: f32,
    ) -> Result<Self, Bm25Error> {
        params.checked()?;
        if !average_document_length.is_finite() || average_document_length <= 0.0 {
            return Err(Bm25Error::AverageDocumentLength);
        }
        if !boost.is_finite() || boost < 0.0 {
            return Err(Bm25Error::Boost);
        }

        let multiplier = idf * boost;
        let k1_plus_one = params.k1 + 1.0;
        let k1_one_minus_b = params.k1 * (1.0 - params.b);
        let document_length_factor = params.k1 * params.b / average_document_length;
        let mut numerator = [0.0; BUCKET_COUNT];
        let mut denominator_constant = [0.0; BUCKET_COUNT];
        for bucket in 0..BUCKET_COUNT {
            let tf = TfBucket::new(bucket as u8)
                .expect("bucket table index must fit in four bits")
                .representative_count() as f32;
            numerator[bucket] = multiplier * tf * k1_plus_one;
            denominator_constant[bucket] = tf + k1_one_minus_b;
        }
        let rises = rises_with_the_bucket(&numerator, &denominator_constant);
        Ok(Self {
            numerator,
            denominator_constant,
            document_length_factor,
            rises,
        })
    }

    #[must_use]
    pub(crate) fn score_bucket(&self, bucket: TfBucket, document_length: u32) -> f32 {
        let index = usize::from(bucket.value());
        let denominator =
            self.denominator_constant[index] + self.document_length_factor * document_length as f32;
        self.numerator[index] / denominator
    }

    /// An upper bound on the score of a document of at least
    /// `document_length` whose bucket is at most `bucket`: the best of
    /// [`Self::score_bucket`] over the buckets up to it, at that length.
    ///
    /// The score never rises with the length, every operation being
    /// correctly rounded and monotonic, but it may fall as the bucket
    /// rises: with k1 at or near zero, or with `b = 1` and a length term
    /// far below the frequency, the exact score barely grows from one
    /// bucket to the next and rounding leaves the higher bucket an ulp
    /// below the lower one. Where the scorer is proven to rise with the
    /// bucket at every length this is the score at `bucket`; otherwise it
    /// is the running best over the buckets up to it.
    #[must_use]
    pub(crate) fn bound_through(&self, bucket: TfBucket, document_length: u32) -> f32 {
        if self.rises {
            return self.score_bucket(bucket, document_length);
        }
        (0..=bucket.value())
            .map(|lower| {
                self.score_bucket(
                    TfBucket::new(lower).expect("a bucket below a valid one"),
                    document_length,
                )
            })
            .fold(0.0_f32, f32::max)
    }

    #[must_use]
    pub(crate) fn score_count(&self, term_frequency: u32, document_length: u32) -> f32 {
        self.score_bucket(TfBucket::from_count(term_frequency), document_length)
    }

    /// An upper bound on the score of every document a block bound covers:
    /// the best of `score_bucket(bucket, min_len)` over the block's buckets
    /// and their shortest documents.
    ///
    /// The score is computed with correctly rounded `f32` operations, each
    /// monotonic in its operands, so it never increases with the document
    /// length: the score at a bucket's shortest document covers every longer
    /// document with that bucket. The bound is attained by one of them, so
    /// it is the block's exact maximum.
    #[must_use]
    pub(crate) fn bound(&self, block: &BlockBound) -> f32 {
        let mut bound = 0.0_f32;
        for (bucket, min_len) in block.buckets() {
            let bucket = TfBucket::new(bucket).expect("block bounds hold valid buckets");
            bound = bound.max(self.score_bucket(bucket, min_len));
        }
        bound
    }

    /// Conjunction members share one document length. Every matching document
    /// is at least `min_length` long and also respects its bucket's minimum.
    pub(crate) fn bound_with_min_length(&self, block: &BlockBound, min_length: u32) -> f32 {
        block
            .buckets()
            .map(|(bucket, length)| {
                self.score_bucket(
                    TfBucket::new(bucket).expect("valid block bucket"),
                    length.max(min_length),
                )
            })
            .fold(0.0_f32, f32::max)
    }

    /// An upper bound on the score of a document of `length` in `block`:
    /// the best score over the buckets that occur, at that length. The walk
    /// bounds a sub-block at [`Self::bound_through`] its largest bucket
    /// instead, which is at least this maximum; the tests below check that.
    #[cfg(test)]
    pub(crate) fn bound_for_length(&self, block: &BlockBound, length: u32) -> f32 {
        let mut bound = 0.0_f32;
        for (bucket, _) in block.buckets() {
            let bucket = TfBucket::new(bucket).expect("block bounds hold valid buckets");
            bound = bound.max(self.score_bucket(bucket, length));
        }
        bound
    }

    /// Per bucket, an upper bound on the score of any document in `block`
    /// whose bucket is at most that bucket and whose length is at least
    /// `min_length`: the best score over the buckets up to it, each at its
    /// own shortest document. A sub-block bound at its largest bucket and
    /// the chunk's shortest document paired a high term frequency with a
    /// document that never carried it; this table pairs each bucket with
    /// the shortest document that does.
    pub(crate) fn bounds_by_bucket(
        &self,
        block: &BlockBound,
        min_length: u32,
    ) -> [f32; segment::tf_bucket::BUCKET_COUNT] {
        let mut table = [0.0_f32; segment::tf_bucket::BUCKET_COUNT];
        let mut best = 0.0_f32;
        for (bucket, slot) in table.iter_mut().enumerate() {
            let len = block.min_len[bucket];
            if len != u32::MAX {
                let bucket = TfBucket::new(bucket as u8).expect("bucket within the count");
                best = best.max(self.score_bucket(bucket, len.max(min_length)));
            }
            *slot = best;
        }
        table
    }
}

/// Sums already ordered term contributions with production's left-to-right
/// `f32` fold. Callers own the canonical term ordering.
#[must_use]
pub(crate) fn sum_scores_in_order(scores: impl IntoIterator<Item = f32>) -> f32 {
    let mut total = 0.0_f32;
    for score in scores {
        total += score;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_parameter_and_override_domains() {
        assert_eq!(Bm25Params::default(), Bm25Params { k1: 1.2, b: 0.75 });
        assert!(Bm25Params { k1: 0.0, b: 0.0 }.is_valid());
        assert!(Bm25Params { k1: 1.0e4, b: 1.0 }.is_valid());
        for invalid in [
            Bm25Params { k1: -0.1, b: 0.5 },
            Bm25Params {
                k1: 1.0e4_f32.next_up(),
                b: 0.5,
            },
            Bm25Params {
                k1: f32::NAN,
                b: 0.5,
            },
            Bm25Params {
                k1: f32::INFINITY,
                b: 0.5,
            },
            Bm25Params { k1: 1.2, b: -0.1 },
            Bm25Params {
                k1: 1.2,
                b: 1.0_f32.next_up(),
            },
            Bm25Params {
                k1: 1.2,
                b: f32::NAN,
            },
        ] {
            assert!(!invalid.is_valid(), "{invalid:?}");
            assert_eq!(invalid.checked(), Err(Bm25Error::Parameters));
        }

        let defaults = Bm25Params { k1: 2.0, b: 0.4 };
        assert_eq!(
            Bm25Overrides {
                k1: Some(3.0),
                b: None
            }
            .resolve(defaults),
            Bm25Params { k1: 3.0, b: 0.4 }
        );
    }

    #[test]
    fn idf_matches_known_values_and_floors_stale_stats() {
        assert_eq!(bm25_idf(0, 0).to_bits(), std::f64::consts::LN_2.to_bits());
        assert_eq!(bm25_idf(10, 11).to_bits(), 0.0_f64.to_bits());
        assert_eq!(bm25_idf(1, 1).to_bits(), 0.28768207245178085_f64.to_bits());
        assert_eq!(
            bm25_idf(100, 10).to_bits(),
            2.2637452596777816_f64.to_bits()
        );
    }

    #[test]
    fn dense_elision_uses_immutable_counts_and_explicit_pins() {
        let ratio = DenseRatio::new(None);
        assert!(ratio.is_valid());
        assert_eq!(ratio.value(), 0.1);
        // 0.1 is widened from its f32 representation before multiplication,
        // exactly as production does, so 10/100 is microscopically below it.
        assert!(!ratio.elides(false, 10, 100));
        assert!(ratio.elides(false, 11, 100));
        assert!(!ratio.elides(true, 100, 100));
        assert!(!ratio.elides(false, 0, 0));
        assert!(!DenseRatio::new(Some(1.5)).elides(false, 100, 100));
        assert!(!DenseRatio::new(Some(f32::NAN)).is_valid());
        assert!(!DenseRatio::new(Some(-0.1)).is_valid());
    }

    #[test]
    fn stop_words_are_exact_trimmed_and_deduplicated() {
        let words = ScoreStopWords::from_csv(" the,rare , ,\twalnut\n,the ").unwrap();
        assert!(words.contains("the"));
        assert!(words.contains("rare"));
        assert!(words.contains("walnut"));
        assert!(!words.contains("Rare"));
        assert!(ScoreStopWords::from_csv(" , \t,\n").is_none());
    }

    #[test]
    fn term_edits_validate_and_analyze() {
        assert_eq!(
            TermSetEdit::from_bound_arrays(Some(vec!["x".into()]), Some(vec!["y".into()])),
            Err(TermSetEditError::ConflictingEdits)
        );
        assert_eq!(
            TermSetEdit::from_bound_arrays(Some(Vec::new()), None).unwrap(),
            TermSetEdit::None
        );
        let raw = TermSetEdit::Add(vec!["alpha beta".into(), "...".into()]);
        assert_eq!(
            raw.analyzed_with(|value| {
                value
                    .split_whitespace()
                    .filter(|token| token.chars().any(char::is_alphanumeric))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            }),
            TermSetEdit::Add(vec!["alpha".into(), "beta".into()])
        );
    }

    #[test]
    fn policy_applies_multiplicity_pins_edits_and_stop_words() {
        let stop_csv = "stopped,blocked";
        let stop = ScoreStopWords::from_csv(stop_csv).unwrap();
        let query = [
            ScoringTermInput {
                text: "rare",
                boost: 1.0,
                explicitly_boosted: false,
            },
            ScoringTermInput {
                text: "rare",
                boost: 2.5,
                explicitly_boosted: true,
            },
            ScoringTermInput {
                text: "dense",
                boost: 1.0,
                explicitly_boosted: false,
            },
            ScoringTermInput {
                text: "stopped",
                boost: 9.0,
                explicitly_boosted: true,
            },
        ];
        let edit = TermSetEdit::Add(vec!["dense".into(), "added".into(), "blocked".into()]);
        let terms = compile_scoring_terms(query, &edit, Some(&stop));

        assert_eq!(
            terms.iter().map(ScoringTerm::text).collect::<Vec<_>>(),
            ["added", "dense", "rare"]
        );
        assert_eq!(terms[0].boost(), 1.0);
        assert!(terms[0].pinned());
        assert_eq!(terms[1].boost(), 1.0);
        assert!(terms[1].pinned());
        assert_eq!(terms[2].boost(), 3.5);
        assert!(terms[2].pinned());

        let replaced = compile_scoring_terms(
            [ScoringTermInput {
                text: "ignored",
                boost: 1.0,
                explicitly_boosted: false,
            }],
            &TermSetEdit::Replace(vec!["replacement".into(), "replacement".into()]),
            None,
        );
        assert_eq!(replaced.len(), 1);
        assert_eq!(replaced[0].text(), "replacement");
        assert_eq!(replaced[0].boost(), 1.0);
        assert!(replaced[0].pinned());
    }

    #[test]
    fn retained_terms_distinguish_full_dense_and_absent_terms() {
        let plain = ScoringTerm {
            text: "x".into(),
            boost: 1.0,
            pinned: false,
        };
        let pinned = ScoringTerm {
            pinned: true,
            ..plain.clone()
        };
        let ratio = Some(DenseRatio::new(Some(0.25)));
        assert!(!plain.is_retained(0, 0, 100, ratio));
        assert!(!plain.is_retained(30, 30, 100, ratio));
        assert!(plain.is_retained(30, 30, 100, None));
        assert!(pinned.is_retained(30, 30, 100, ratio));
    }

    #[test]
    fn scorer_matches_exact_production_fixtures() {
        let scorer =
            TermScorer::from_statistics(100, 10, 0.7, Bm25Params::default(), 80.0).unwrap();
        let fixtures = [
            (1, 1, 1_076_504_444_u32),
            (5, 17, 1_078_667_167_u32),
            (20, 80, 1_079_147_600_u32),
            (42, 250, 1_078_943_558_u32),
            (u32::MAX, 1_024, 1_079_969_742_u32),
        ];
        for (tf, dl, expected_bits) in fixtures {
            assert_eq!(
                scorer.score_count(tf, dl).to_bits(),
                expected_bits,
                "tf={tf} dl={dl}"
            );
        }
    }

    #[test]
    fn bound_dominates_every_score_it_covers() {
        let scorers = [
            TermScorer::from_statistics(100, 10, 0.7, Bm25Params::default(), 80.0).unwrap(),
            TermScorer::from_statistics(100_000, 22_000, 1.0, Bm25Params::default(), 333.7)
                .unwrap(),
            TermScorer::from_statistics(5, 5, 2.5, Bm25Params { k1: 0.0, b: 1.0 }, 1.0).unwrap(),
            TermScorer::from_statistics(1 << 30, 3, 1.0, Bm25Params { k1: 1e4, b: 0.0 }, 7.5)
                .unwrap(),
            TermScorer::from_statistics(1000, 1, 3.0, Bm25Params { k1: 0.3, b: 0.99 }, 0.01)
                .unwrap(),
        ];
        let lengths: Vec<u32> = (1..300)
            .chain((300..5_000).step_by(37))
            .chain([65_535, 1 << 20, u32::MAX / 2, u32::MAX - 1])
            .collect();
        for scorer in &scorers {
            // Blocks holding one bucket at one shortest length: every longer
            // document with that bucket scores at most the bound, which the
            // shortest attains.
            for bucket in 0..BUCKET_COUNT as u8 {
                for &min_len in &lengths {
                    let block = BlockBound::over(&[(bucket, min_len)]);
                    let bound = scorer.bound(&block);
                    let bucket = TfBucket::new(bucket).unwrap();
                    assert!(bound >= 0.0);
                    assert_eq!(scorer.score_bucket(bucket, min_len), bound);
                    for &length in lengths.iter().filter(|length| **length >= min_len) {
                        let score = scorer.score_bucket(bucket, length);
                        assert!(
                            score <= bound,
                            "bucket {bucket:?} length {length} scores {score} above bound {bound} at min_len {min_len}"
                        );
                    }
                }
            }
            // A mixed block: each posting is covered by its own bucket's entry.
            let postings: Vec<(u8, u32)> = lengths
                .iter()
                .enumerate()
                .map(|(i, len)| ((i % BUCKET_COUNT) as u8, *len))
                .collect();
            let block = BlockBound::over(&postings);
            let bound = scorer.bound(&block);
            for (bucket, len) in &postings {
                let score = scorer.score_bucket(TfBucket::new(*bucket).unwrap(), *len);
                assert!(score <= bound, "{bucket} {len}: {score} above {bound}");
                // The bound at the document's own length covers it too.
                for min_length in [1, *len / 2, *len] {
                    let joint = scorer.bound_with_min_length(&block, min_length);
                    assert!(
                        score <= joint && joint <= bound,
                        "{bucket} {len} {min_length}"
                    );
                }
                let for_length = scorer.bound_for_length(&block, *len);
                assert!(score <= for_length, "{bucket} {len}");
            }
            assert!(
                postings
                    .iter()
                    .any(|(b, l)| scorer.score_bucket(TfBucket::new(*b).unwrap(), *l) == bound)
            );
        }
    }

    /// With k1 = 0 the score is `fl(fl(m tf) 1) / tf`, which rounding
    /// leaves an ulp lower for three occurrences than for one: the
    /// ten-document table of 'w w w' and nine 'w'. A bound at the higher
    /// bucket alone fell below the lower bucket's score.
    #[test]
    fn bounds_through_a_bucket_cover_every_bucket_below_it() {
        let flat =
            TermScorer::from_statistics(10, 10, 1.0, Bm25Params { k1: 0.0, b: 0.75 }, 1.2).unwrap();
        let (one, three) = (TfBucket::from_count(1), TfBucket::from_count(3));
        assert!(flat.score_bucket(three, 3) < flat.score_bucket(one, 3));
        assert!(!flat.rises);
        assert_eq!(flat.bound_through(three, 3), flat.score_bucket(one, 3));
        // The default parameters rise with the bucket, so the walk's bound
        // stays one division.
        for (docs, df, avg) in [
            (10, 10, 1.2),
            (1_000_000, 3, 250.0),
            (1 << 30, 1 << 29, 1.0),
        ] {
            for boost in [0.25, 1.0, 3.0] {
                let scorer =
                    TermScorer::from_statistics(docs, df, boost, Bm25Params::default(), avg)
                        .unwrap();
                assert!(scorer.rises, "{docs} {df} {avg} {boost}");
            }
        }
        let lengths: Vec<u32> = (0..70)
            .chain([
                100,
                777,
                1_626,
                3_405,
                7_132,
                14_938,
                31_288,
                100_000,
                1 << 24,
            ])
            .collect();
        let mut inverted = 0;
        let mut checked = 0;
        for k1 in [0.0, 0.001, 0.01, 0.1, 0.5, 1.2, 3.0, 1e4] {
            for b in [0.0, 0.25, 0.75, 1.0] {
                for (docs, df) in [(10, 10), (10, 1), (100, 7), (100_000, 999), (1 << 31, 3)] {
                    for avg in [0.5, 1.0, 3.7, 100.0, 5_000.0] {
                        let scorer =
                            TermScorer::from_statistics(docs, df, 1.0, Bm25Params { k1, b }, avg)
                                .unwrap();
                        for &length in &lengths {
                            let mut best = 0.0_f32;
                            for bucket in 0..BUCKET_COUNT as u8 {
                                let bucket = TfBucket::new(bucket).unwrap();
                                let score = scorer.score_bucket(bucket, length);
                                inverted += usize::from(score < best);
                                best = best.max(score);
                                let bound = scorer.bound_through(bucket, length);
                                assert!(
                                    bound >= best,
                                    "k1 {k1} b {b} N {docs} df {df} avg {avg} len {length} {bucket:?}"
                                );
                                if scorer.rises {
                                    assert_eq!(bound, score, "k1 {k1} b {b} {bucket:?}");
                                }
                                checked += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(inverted > 0 && checked > inverted);
    }

    #[test]
    fn rejects_invalid_scorer_inputs() {
        let params = Bm25Params::default();
        assert_eq!(
            TermScorer::new(1.0, 1.0, params, 0.0).unwrap_err(),
            Bm25Error::AverageDocumentLength
        );
        assert_eq!(
            TermScorer::new(1.0, -1.0, params, 1.0).unwrap_err(),
            Bm25Error::Boost
        );
    }

    #[test]
    fn score_folding_is_left_to_right_f32() {
        let scores = [f32::MAX, -f32::MAX, 1.0];
        assert_eq!(sum_scores_in_order(scores), 1.0);
        assert_eq!(sum_scores_in_order(scores.into_iter().rev()), 0.0);
    }
}
