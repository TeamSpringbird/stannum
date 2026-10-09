// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

//! A query's scoring terms: the terms it names and those its expansions
//! bring, each with its boost, and the scorer of each term the statistics
//! retain.

use std::collections::BTreeSet;

use segment::index::{Expanded, Index, Window};
use tinql::runtime::{CompiledRegex, FuzzyMatcher, Query, RangeBound, SpanTermSlot, range_matches};

use crate::bm25::{
    Bm25Error, Bm25Params, DenseRatio, ScoreStopWords, ScoringTermInput, TermScorer, TermSetEdit,
    compile_scoring_terms,
};
use crate::segment_error;

/// A query's expansions bring more terms to score than the limit, given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooManyTerms(pub usize);

/// Why a query's scorers could not be built.
#[derive(Debug)]
pub enum ScoringError {
    /// Past `stannum.max_expansion_terms`.
    TooManyTerms(usize),
    /// The BM25 parameters do not make a scorer.
    Parameters(Bm25Error),
}

/// How a query's terms are scored.
pub struct ScoringPolicy<'a> {
    pub params: Bm25Params,
    /// `full_score`: no term is elided.
    pub full: bool,
    pub dense: DenseRatio,
    pub edit: &'a TermSetEdit,
    pub stop: Option<&'a ScoreStopWords<'a>>,
    /// `stannum.max_expansion_terms`.
    pub max_expansion_terms: usize,
}

/// The scorer of each scoring term of `scoring` over `segments`, the first
/// `immutable` of which are immutable segments (the rest the write buffer),
/// in the order [`compile_scoring_terms`] gives them. Statistics include
/// dead documents until their segment is rewritten, and buffered documents
/// immediately; elision uses immutable segments only.
pub fn term_scorers(
    scoring: &Query,
    segments: &[&dyn Index],
    immutable: usize,
    policy: &ScoringPolicy<'_>,
) -> Result<Vec<(String, TermScorer)>, ScoringError> {
    let mut collected = Collected::default();
    collect_score_terms(scoring, 1.0, false, &mut collected);
    let owned = collected
        .resolve(policy.max_expansion_terms, |expansion, limit| {
            expansion.expand_in(segments, limit)
        })
        .map_err(|TooManyTerms(limit)| ScoringError::TooManyTerms(limit))?;
    let terms = compile_scoring_terms(inputs_of(&owned), policy.edit, policy.stop);
    let is_immutable = |i: usize| i < immutable;
    let total_docs: u64 = segments.iter().map(|s| u64::from(s.document_count())).sum();
    let immutable_docs: u64 = segments
        .iter()
        .enumerate()
        .filter(|(i, _)| is_immutable(*i))
        .map(|(_, s)| u64::from(s.document_count()))
        .sum();
    let total_length: u64 = segments.iter().map(|s| s.total_length()).sum();
    let average_length = if total_docs == 0 {
        1.0
    } else {
        total_length as f32 / total_docs as f32
    };
    let mut scorers = Vec::new();
    for term in terms {
        // A term costs a dictionary lookup per source, and an expansion can
        // bring thousands of them.
        crate::check_interrupts();
        let mut total_df = 0u64;
        let mut immutable_df = 0u64;
        for (i, segment) in segments.iter().enumerate() {
            let df = segment_error(segment.term(term.text())).map_or(0, |t| u64::from(t.df()));
            total_df += df;
            if is_immutable(i) {
                immutable_df += df;
            }
        }
        let ratio = (!policy.full).then_some(policy.dense);
        if !term.is_retained(total_df, immutable_df, immutable_docs, ratio) {
            continue;
        }
        let scorer = TermScorer::from_statistics(
            total_docs,
            total_df,
            term.boost(),
            policy.params,
            average_length,
        )
        .map_err(ScoringError::Parameters)?;
        scorers.push((term.text().to_owned(), scorer));
    }
    Ok(scorers)
}

/// A query node that scores every dictionary term it expands to, as TIN does.
pub enum Expansion<'a> {
    Regex(&'a CompiledRegex),
    Range(&'a RangeBound, &'a RangeBound),
    Fuzzy {
        term: &'a str,
        prefix: u32,
        distance: u32,
    },
}

impl Expansion<'_> {
    fn matcher(&self) -> Box<dyn Fn(&str) -> bool + '_> {
        match self {
            Self::Regex(regex) => Box::new(move |candidate| regex.is_match(candidate)),
            Self::Range(lower, upper) => {
                Box::new(move |candidate| range_matches(candidate, lower, upper))
            }
            Self::Fuzzy {
                term,
                prefix,
                distance,
            } => {
                let matcher = FuzzyMatcher::new(term, *prefix, *distance);
                Box::new(move |candidate| matcher.is_match(candidate))
            }
        }
    }

    /// Every matching term across the given indexes, or `None` when there
    /// are more than `limit`.
    pub fn expand_in(&self, segments: &[&dyn Index], limit: usize) -> Option<Vec<String>> {
        let mut found = BTreeSet::new();
        let matcher = self.matcher();
        for segment in segments {
            let expanded = match self {
                Self::Regex(regex) => match regex.pure_prefix() {
                    Some(prefix) => segment.expand(Window::Prefix(&prefix), &|_| true, limit),
                    None => segment.expand(Window::All, &*matcher, limit),
                },
                Self::Range(lower, upper) => {
                    fn bound(bound: &RangeBound) -> Option<&str> {
                        match bound {
                            RangeBound::Open => None,
                            RangeBound::Term(term) => Some(term.as_str()),
                        }
                    }
                    segment.expand(Window::Range(bound(lower), bound(upper)), &|_| true, limit)
                }
                Self::Fuzzy { term, prefix, .. } => {
                    let fixed: String = term.chars().take(*prefix as usize).collect();
                    segment.expand(Window::Prefix(&fixed), &*matcher, limit)
                }
            };
            match segment_error(expanded) {
                Expanded::Terms(terms) => found.extend(terms.into_iter().map(|(t, _)| t)),
                Expanded::Overflow => return None,
            }
            if found.len() > limit {
                return None;
            }
        }
        Some(found.into_iter().collect())
    }

    /// Every term of `universe` that matches, or `None` when there are more
    /// than `limit`.
    pub fn expand_over(&self, universe: &BTreeSet<&str>, limit: usize) -> Option<Vec<String>> {
        let matcher = self.matcher();
        let found: Vec<String> = universe
            .iter()
            .filter(|term| matcher(term))
            .take(limit.saturating_add(1))
            .map(|term| (*term).to_owned())
            .collect();
        (found.len() <= limit).then_some(found)
    }
}

/// Scoring inputs gathered from a query before expansions are resolved.
#[derive(Default)]
pub struct Collected<'a> {
    terms: Vec<ScoringTermInput<'a>>,
    expansions: Vec<(Expansion<'a>, f32, bool)>,
}

impl<'a> Collected<'a> {
    /// Resolves expansions through `expand` and returns owned inputs.
    /// `expand` is given the terms the expansions may still bring and
    /// returns `None` past them: every term an expansion brings is scored,
    /// so their number is bounded by `limit` (`stannum.max_expansion_terms`),
    /// past which the query fails, [`TooManyTerms`], rather than scoring
    /// some of them.
    pub fn resolve(
        self,
        limit: usize,
        mut expand: impl FnMut(&Expansion<'a>, usize) -> Option<Vec<String>>,
    ) -> Result<Vec<(String, f32, bool)>, TooManyTerms> {
        let mut out: Vec<(String, f32, bool)> = self
            .terms
            .iter()
            .map(|input| (input.text.to_owned(), input.boost, input.explicitly_boosted))
            .collect();
        let mut expanded = 0usize;
        for (expansion, boost, explicit) in &self.expansions {
            let Some(terms) = expand(expansion, limit - expanded) else {
                return Err(TooManyTerms(limit));
            };
            expanded += terms.len();
            for term in terms {
                out.push((term, *boost, *explicit));
            }
        }
        Ok(out)
    }
}

pub fn inputs_of(owned: &[(String, f32, bool)]) -> impl Iterator<Item = ScoringTermInput<'_>> {
    owned
        .iter()
        .map(|(text, boost, explicit)| ScoringTermInput {
            text,
            boost: *boost,
            explicitly_boosted: *explicit,
        })
}

/// Boolean NOT contributes nothing to scoring, nor does the excluded side of
/// a negative span relation. A span weighs each written occurrence of a
/// term, times the boosts written on that operand. Wildcards, regexes, ranges and fuzzy terms score every term
/// they expand to with the node's boost.
pub fn collect_score_terms<'a>(
    query: &'a Query,
    boost: f32,
    explicitly_boosted: bool,
    out: &mut Collected<'a>,
) {
    // tinql bounds a query's nesting (tinql::limits); this turns a walk that
    // still runs out of stack into the host's error rather than an abort
    // (the extension's is PostgreSQL's ERROR).
    tinql::limits::check_stack();
    let mut push = |text: &'a str| {
        out.terms.push(ScoringTermInput {
            text,
            boost,
            explicitly_boosted,
        });
    };
    match query {
        Query::Term(text) => push(text),
        Query::Fuzzy {
            term,
            prefix,
            distance,
        } => out.expansions.push((
            Expansion::Fuzzy {
                term,
                prefix: *prefix,
                distance: *distance,
            },
            boost,
            explicitly_boosted,
        )),
        Query::Regex(regex) => {
            out.expansions
                .push((Expansion::Regex(regex), boost, explicitly_boosted))
        }
        Query::Range { lower, upper } => {
            out.expansions
                .push((Expansion::Range(lower, upper), boost, explicitly_boosted))
        }
        Query::Span { .. } | Query::SpanExpr { .. } => {
            // Each written occurrence outside an excluded side, weighted by
            // the boost written on it (Lead e3ed2f4, from TIN).
            query.for_each_positive_span_slot(&mut |_, slot, leaf_boost| {
                let boost = boost * leaf_boost.unwrap_or(1.0);
                let explicitly_boosted = explicitly_boosted || leaf_boost.is_some();
                match slot {
                    SpanTermSlot::Term(text) => out.terms.push(ScoringTermInput {
                        text,
                        boost,
                        explicitly_boosted,
                    }),
                    SpanTermSlot::Regex(regex) => {
                        out.expansions
                            .push((Expansion::Regex(regex), boost, explicitly_boosted))
                    }
                    SpanTermSlot::Range { lower, upper } => out.expansions.push((
                        Expansion::Range(lower, upper),
                        boost,
                        explicitly_boosted,
                    )),
                    SpanTermSlot::Fuzzy {
                        term,
                        prefix,
                        distance,
                    } => out.expansions.push((
                        Expansion::Fuzzy {
                            term,
                            prefix: *prefix,
                            distance: *distance,
                        },
                        boost,
                        explicitly_boosted,
                    )),
                }
            });
        }
        Query::And(left, right) | Query::Or(left, right) => {
            collect_score_terms(left, boost, explicitly_boosted, out);
            collect_score_terms(right, boost, explicitly_boosted, out);
        }
        Query::Conjunction(children)
        | Query::Disjunction { children, .. }
        | Query::AtLeast { children, .. } => {
            for child in children {
                collect_score_terms(child, boost, explicitly_boosted, out);
            }
        }
        Query::Not(_) | Query::MatchAll => {}
        Query::Boost { factor, inner } => {
            collect_score_terms(inner, boost * *factor, true, out);
        }
    }
}

/// Distinct tokens of a corpus, the expansion universe without a dictionary.
pub fn corpus_universe(tokenized: &[Vec<String>]) -> BTreeSet<&str> {
    tokenized
        .iter()
        .flat_map(|tokens| tokens.iter().map(String::as_str))
        .collect()
}
