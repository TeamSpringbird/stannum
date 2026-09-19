// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Cardinality estimation for a lowered [`Query`] from index statistics.
//!
//! The planner asks two questions about a `==>` predicate: how many rows will
//! match (selectivity) and how many candidates the index will hand back for
//! fetching (cost). Both are fractions of the indexed documents, derived from
//! per-term document frequencies under an independence model:
//!
//! * a term matches `df / N` of the documents;
//! * a conjunction matches the product of its children's fractions, capped
//!   by the rarest child (an intersection can never exceed its smallest input);
//! * a disjunction matches by inclusion-exclusion, `1 - Π(1 - fᵢ)`;
//! * a negation matches the complement, `1 - f`;
//! * `AT LEAST k OF` matches the probability that at least k independent
//!   children match, the tail of their Poisson binomial distribution;
//! * a phrase or proximity query is bounded by its rarest term and discounted
//!   once per additional slot, because positions constrain far more than
//!   co-occurrence but rarely to independence;
//! * an expansion (wildcard, regex, range, fuzzy) sums the frequencies of the
//!   terms it expands to when the dictionary resolves it within the expansion
//!   cap; past the cap the plan degrades to a rechecked universe, so the
//!   candidates are everything and the match fraction is a conservative prior.
//!
//! [`Statistics`] abstracts the source of frequencies so the arithmetic is
//! testable without an index; [`IndexStatistics`] sums them over the segments
//! of a live index.

use segment::index::{Expanded, Index, Window};
use segment::postings::Postings;
use segment::segment::Term;
use segment::set::Cursor;

use super::eval::FuzzyMatcher;
use super::{CompiledRegex, Query, RangeBound, SpanTermSlot};

/// Fraction of a phrase's rarest term expected to survive each additional
/// positional constraint.
pub const PHRASE_DISCOUNT: f64 = 0.5;

/// Match fraction assumed for an expansion past the cap, where the dictionary
/// gives no frequencies. Matches the planner's historical prior for `==>`.
pub const OVERFLOW_SELECTIVITY: f64 = 0.1;

/// Document frequencies for estimation.
pub trait Statistics {
    type Error;
    /// Documents in the index.
    fn documents(&self) -> Result<f64, Self::Error>;
    /// Documents containing `term`.
    fn document_frequency(&self, term: &str) -> Result<f64, Self::Error>;
    /// Sum of document frequencies over the terms in `window` accepted by
    /// `filter`, or `None` when the expansion exceeds the cap.
    fn expansion_frequency(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
    ) -> Result<Option<f64>, Self::Error>;
}

/// What the planner learns about a query, as fractions of the documents.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Estimate {
    /// Fraction of documents expected to match.
    pub selectivity: f64,
    /// Fraction of documents the index yields as candidates to fetch. Equal
    /// to `selectivity` for an exact plan; larger when a recheck is needed.
    pub candidates: f64,
    /// Whether the index plan is exact (see [`plan`](super::plan)).
    pub exact: bool,
}

impl Estimate {
    const fn exact(fraction: f64) -> Self {
        Self {
            selectivity: fraction,
            candidates: fraction,
            exact: true,
        }
    }

    /// The whole document universe, rechecked.
    const fn inexact_universe(selectivity: f64) -> Self {
        Self {
            selectivity,
            candidates: 1.0,
            exact: false,
        }
    }

    /// Fractions are clamped to `[0, 1]`; candidates never fall below matches.
    fn normalized(self) -> Self {
        let selectivity = clamp(self.selectivity);
        Self {
            selectivity,
            candidates: clamp(self.candidates).max(selectivity),
            exact: self.exact,
        }
    }
}

fn clamp(fraction: f64) -> f64 {
    if fraction.is_nan() {
        0.0
    } else {
        fraction.clamp(0.0, 1.0)
    }
}

// --- Combination arithmetic -------------------------------------------------

/// AND: the product of the fractions, capped by the smallest of them. An empty
/// conjunction is the universe.
pub fn conjunction(fractions: impl IntoIterator<Item = f64>) -> f64 {
    let mut product = 1.0;
    let mut smallest = 1.0_f64;
    for fraction in fractions {
        let fraction = clamp(fraction);
        product *= fraction;
        smallest = smallest.min(fraction);
    }
    product.min(smallest)
}

/// OR: inclusion-exclusion under independence, `1 - Π(1 - fᵢ)`, which never
/// exceeds the sum and never falls below the largest child.
pub fn disjunction(fractions: impl IntoIterator<Item = f64>) -> f64 {
    let mut none_match = 1.0;
    for fraction in fractions {
        none_match *= 1.0 - clamp(fraction);
    }
    1.0 - none_match
}

/// NOT: the complement.
pub fn complement(fraction: f64) -> f64 {
    1.0 - clamp(fraction)
}

/// AT LEAST `min` OF: the probability that at least `min` of independent
/// events occur (the Poisson binomial tail), which is the disjunction for
/// `min == 1` and the product for `min == n`. Zero is the universe; more
/// than the child count is unsatisfiable.
pub fn at_least(fractions: impl IntoIterator<Item = f64>, min: usize) -> f64 {
    if min == 0 {
        return 1.0;
    }
    let fractions: Vec<f64> = fractions.into_iter().map(clamp).collect();
    if min > fractions.len() {
        return 0.0;
    }
    // exactly[j]: probability that exactly j of the events seen so far occur.
    let mut exactly = vec![0.0; fractions.len() + 1];
    exactly[0] = 1.0;
    for (seen, fraction) in fractions.iter().enumerate() {
        for j in (0..=seen + 1).rev() {
            let with = if j > 0 {
                exactly[j - 1] * fraction
            } else {
                0.0
            };
            exactly[j] = exactly[j] * (1.0 - fraction) + with;
        }
    }
    exactly[min..].iter().sum::<f64>().min(1.0)
}

/// Phrase or proximity: bounded by the rarest slot, discounted once per
/// additional slot. An empty phrase matches nothing.
pub fn phrase(fractions: impl IntoIterator<Item = f64>) -> f64 {
    let mut rarest: Option<f64> = None;
    let mut slots = 0usize;
    for fraction in fractions {
        let fraction = clamp(fraction);
        rarest = Some(rarest.map_or(fraction, |r| r.min(fraction)));
        slots += 1;
    }
    match rarest {
        Some(rarest) => rarest * PHRASE_DISCOUNT.powi(slots.saturating_sub(1) as i32),
        None => 0.0,
    }
}

/// Expansion within the cap: the summed frequencies as a fraction, capped at
/// the universe (expanded terms may share documents).
pub fn expansion(summed_frequency: f64, documents: f64) -> f64 {
    if documents <= 0.0 {
        0.0
    } else {
        clamp(summed_frequency / documents)
    }
}

// --- Query walk ---------------------------------------------------------------

/// Estimates `query` over `stats`. An index with no documents matches nothing.
pub fn estimate<S: Statistics + ?Sized>(query: &Query, stats: &S) -> Result<Estimate, S::Error> {
    let documents = stats.documents()?;
    if documents <= 0.0 {
        return Ok(Estimate::exact(0.0));
    }
    Estimator { stats, documents }.query(query)
}

struct Estimator<'s, S: ?Sized> {
    stats: &'s S,
    documents: f64,
}

impl<S: Statistics + ?Sized> Estimator<'_, S> {
    fn fraction(&self, frequency: f64) -> f64 {
        clamp(frequency / self.documents)
    }

    fn term(&self, term: &str) -> Result<Estimate, S::Error> {
        Ok(Estimate::exact(
            self.fraction(self.stats.document_frequency(term)?),
        ))
    }

    fn expanded(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
    ) -> Result<Estimate, S::Error> {
        Ok(match self.stats.expansion_frequency(window, filter)? {
            Some(sum) => Estimate::exact(expansion(sum, self.documents)),
            None => Estimate::inexact_universe(OVERFLOW_SELECTIVITY),
        })
    }

    fn regex(&self, regex: &CompiledRegex) -> Result<Estimate, S::Error> {
        match regex.pure_prefix() {
            Some(prefix) => self.expanded(Window::Prefix(&prefix), &|_| true),
            None => self.expanded(Window::All, &|term| regex.is_match(term)),
        }
    }

    fn range(&self, lower: &RangeBound, upper: &RangeBound) -> Result<Estimate, S::Error> {
        fn bound(bound: &RangeBound) -> Option<&str> {
            match bound {
                RangeBound::Open => None,
                RangeBound::Term(term) => Some(term.as_str()),
            }
        }
        self.expanded(Window::Range(bound(lower), bound(upper)), &|_| true)
    }

    fn fuzzy(&self, term: &str, prefix: u32, distance: u32) -> Result<Estimate, S::Error> {
        let matcher = FuzzyMatcher::new(term, prefix, distance);
        let fixed: String = term.chars().take(prefix as usize).collect();
        self.expanded(Window::Prefix(&fixed), &|candidate| {
            matcher.is_match(candidate)
        })
    }

    fn slot(&self, slot: &SpanTermSlot) -> Result<Estimate, S::Error> {
        match slot {
            SpanTermSlot::Term(term) => self.term(term),
            SpanTermSlot::Regex(regex) => self.regex(regex),
            SpanTermSlot::Range { lower, upper } => self.range(lower, upper),
            SpanTermSlot::Fuzzy {
                term,
                prefix,
                distance,
            } => self.fuzzy(term, *prefix, *distance),
        }
    }

    fn children(&self, children: &[Query]) -> Result<Vec<Estimate>, S::Error> {
        children.iter().map(|child| self.query(child)).collect()
    }

    fn and(children: &[Estimate]) -> Estimate {
        let exact = children.iter().all(|child| child.exact);
        Estimate {
            selectivity: conjunction(children.iter().map(|child| child.selectivity)),
            candidates: conjunction(children.iter().map(|child| child.candidates)),
            exact,
        }
        .normalized()
    }

    fn or(children: &[Estimate]) -> Estimate {
        let exact = children.iter().all(|child| child.exact);
        Estimate {
            selectivity: disjunction(children.iter().map(|child| child.selectivity)),
            candidates: disjunction(children.iter().map(|child| child.candidates)),
            exact,
        }
        .normalized()
    }

    fn at_least(min: u32, children: &[Estimate]) -> Estimate {
        let min = min as usize;
        if min == 1 {
            return Self::or(children);
        }
        let exact = children.iter().all(|child| child.exact);
        Estimate {
            selectivity: at_least(children.iter().map(|child| child.selectivity), min),
            candidates: at_least(children.iter().map(|child| child.candidates), min),
            exact,
        }
        .normalized()
    }

    /// Complementing a superset is unsound, so `NOT` over an inexact child
    /// degrades to the rechecked universe, as the plan does.
    fn not(inner: Estimate) -> Estimate {
        let selectivity = complement(inner.selectivity);
        if inner.exact {
            Estimate::exact(selectivity)
        } else {
            Estimate::inexact_universe(selectivity)
        }
    }

    fn span(&self, slots: &[SpanTermSlot]) -> Result<Estimate, S::Error> {
        let slots = slots
            .iter()
            .map(|slot| self.slot(slot))
            .collect::<Result<Vec<_>, _>>()?;
        if slots.iter().any(|slot| !slot.exact) {
            // An overflowed slot leaves the plan a rechecked universe; the
            // resolvable slots still bound the matches.
            let resolvable = slots.iter().filter(|slot| slot.exact);
            let bound = phrase(resolvable.map(|slot| slot.selectivity));
            let selectivity = if slots.iter().all(|slot| !slot.exact) {
                OVERFLOW_SELECTIVITY
            } else {
                bound
            };
            return Ok(Estimate::inexact_universe(selectivity).normalized());
        }
        Ok(Estimate::exact(phrase(slots.iter().map(|slot| slot.selectivity))).normalized())
    }

    fn query(&self, query: &Query) -> Result<Estimate, S::Error> {
        Ok(match query {
            Query::Term(term) => self.term(term)?,
            Query::And(left, right) => Self::and(&[self.query(left)?, self.query(right)?]),
            Query::Or(left, right) => Self::or(&[self.query(left)?, self.query(right)?]),
            Query::Conjunction(children) => Self::and(&self.children(children)?),
            Query::Disjunction { min, children } | Query::AtLeast { min, children } => {
                Self::at_least(*min, &self.children(children)?)
            }
            Query::Not(inner) => Self::not(self.query(inner)?),
            Query::MatchAll => Estimate::exact(1.0),
            Query::Regex(regex) => self.regex(regex)?,
            Query::Range { lower, upper } => self.range(lower, upper)?,
            Query::Fuzzy {
                term,
                prefix,
                distance,
            } => self.fuzzy(term, *prefix, *distance)?,
            Query::Boost { inner, .. } => self.query(inner)?,
            Query::Span { term_slots, .. } | Query::SpanExpr { term_slots, .. } => {
                self.span(term_slots)?
            }
        })
    }
}

// --- Index-backed statistics --------------------------------------------------

/// Frequencies summed over the sources of an index (its segments and write
/// buffer), each expanded under the same cap the plan uses.
pub struct IndexStatistics<'a> {
    pub sources: Vec<(&'a dyn Index, Option<Postings<'a>>)>,
    pub max_expansion: usize,
}

impl Statistics for IndexStatistics<'_> {
    type Error = segment::Error;

    fn documents(&self) -> Result<f64, Self::Error> {
        Ok(self
            .sources
            .iter()
            .map(|(source, dead)| {
                f64::from(
                    source
                        .document_count()
                        .saturating_sub(dead.as_ref().map_or(0, Postings::count)),
                )
            })
            .sum())
    }

    fn document_frequency(&self, term: &str) -> Result<f64, Self::Error> {
        let mut sum = 0.0;
        for (source, dead) in &self.sources {
            if let Some(term) = source.term(term)? {
                sum += live_frequency(*source, dead.as_ref(), &term)?;
            }
        }
        Ok(sum)
    }

    fn expansion_frequency(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
    ) -> Result<Option<f64>, Self::Error> {
        let mut sum = 0.0;
        for (source, dead) in &self.sources {
            match source.expand(window, filter, self.max_expansion)? {
                Expanded::Terms(terms) => {
                    for (_, term) in terms {
                        sum += live_frequency(*source, dead.as_ref(), &term)?;
                    }
                }
                Expanded::Overflow => return Ok(None),
            }
        }
        Ok(Some(sum))
    }
}

/// Keep planning bounded: count dead hits exactly for short posting lists,
/// and use the segment's live fraction for common terms. Buffered sources
/// have no dead list and retain their original frequencies.
fn live_frequency(
    index: &dyn Index,
    dead: Option<&Postings<'_>>,
    term: &Term<'_>,
) -> segment::Result<f64> {
    let Some(dead) = dead.filter(|dead| dead.count() > 0) else {
        return Ok(f64::from(term.df()));
    };
    let documents = index.document_count();
    if documents == 0 || dead.count() >= documents {
        return Ok(0.0);
    }
    if term.df() <= 1024 {
        let mut postings = term.cursor()?;
        let mut deleted = dead.cursor()?;
        let mut live = 0u32;
        while let Some(tid) = postings.current() {
            deleted.seek(tid)?;
            live += u32::from(deleted.current() != Some(tid));
            postings.advance()?;
        }
        Ok(f64::from(live))
    } else {
        Ok(f64::from(term.df()) * f64::from(documents - dead.count()) / f64::from(documents))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::convert::Infallible;

    use super::*;
    use crate::runtime::parse_tinql_to_query_default;

    /// A dictionary of frequencies over `documents` documents.
    struct Table {
        documents: f64,
        frequencies: BTreeMap<&'static str, f64>,
        cap: usize,
    }

    impl Table {
        fn new(documents: f64, entries: &[(&'static str, f64)]) -> Self {
            Self {
                documents,
                frequencies: entries.iter().copied().collect(),
                cap: 1024,
            }
        }
    }

    impl Statistics for Table {
        type Error = Infallible;
        fn documents(&self) -> Result<f64, Infallible> {
            Ok(self.documents)
        }
        fn document_frequency(&self, term: &str) -> Result<f64, Infallible> {
            Ok(self.frequencies.get(term).copied().unwrap_or(0.0))
        }
        fn expansion_frequency(
            &self,
            window: Window<'_>,
            filter: &dyn Fn(&str) -> bool,
        ) -> Result<Option<f64>, Infallible> {
            let matched: Vec<f64> = self
                .frequencies
                .iter()
                .filter(|(term, _)| match window {
                    Window::Prefix(prefix) => term.starts_with(prefix),
                    Window::Range(lower, upper) => {
                        lower.is_none_or(|l| **term >= l) && upper.is_none_or(|u| **term <= u)
                    }
                    Window::All => true,
                })
                .filter(|(term, _)| filter(term))
                .map(|(_, df)| *df)
                .collect();
            Ok((matched.len() <= self.cap).then(|| matched.iter().sum()))
        }
    }

    fn corpus() -> Table {
        Table::new(
            1000.0,
            &[
                ("rare", 10.0),
                ("common", 900.0),
                ("half", 500.0),
                ("fifth", 200.0),
                ("brewer", 30.0),
                ("brewery", 20.0),
                ("brewhouse", 5.0),
            ],
        )
    }

    fn run(query: &str) -> Estimate {
        let query = parse_tinql_to_query_default(query).unwrap();
        estimate(&query, &corpus()).unwrap()
    }

    fn close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn conjunction_is_the_capped_product() {
        close(conjunction([0.5, 0.2]), 0.1);
        close(conjunction([0.01, 1.0]), 0.01);
        close(conjunction([]), 1.0);
        close(conjunction([0.0, 0.9]), 0.0);
        // Clamped inputs cannot inflate the result.
        close(conjunction([2.0, 0.5]), 0.5);
    }

    #[test]
    fn disjunction_is_inclusion_exclusion() {
        close(disjunction([0.5, 0.2]), 0.6);
        close(disjunction([0.5, 0.5, 0.5]), 0.875);
        close(disjunction([]), 0.0);
        close(disjunction([1.0, 0.3]), 1.0);
        // Never exceeds the sum, never falls below the largest child.
        assert!(disjunction([0.3, 0.3]) <= 0.6);
        assert!(disjunction([0.3, 0.3]) >= 0.3);
    }

    #[test]
    fn complement_and_at_least() {
        close(complement(0.2), 0.8);
        close(complement(-1.0), 1.0);
        // At least one is the disjunction; all of them is the product.
        close(at_least([0.1, 0.5, 0.3], 1), disjunction([0.1, 0.5, 0.3]));
        close(at_least([0.1, 0.5, 0.3], 2), 0.2);
        close(at_least([0.1, 0.5, 0.3], 3), 0.015);
        close(at_least([0.1, 0.5, 0.3], 4), 0.0);
        close(at_least([0.1], 0), 1.0);
        close(at_least([], 1), 0.0);
    }

    #[test]
    fn phrase_is_the_discounted_rarest_slot() {
        close(phrase([0.5, 0.01]), 0.005);
        close(phrase([0.02, 0.5, 0.9]), 0.005);
        close(phrase([0.3]), 0.3);
        close(phrase([]), 0.0);
    }

    #[test]
    fn expansion_is_a_capped_sum() {
        close(expansion(55.0, 1000.0), 0.055);
        close(expansion(5000.0, 1000.0), 1.0);
        close(expansion(5.0, 0.0), 0.0);
    }

    #[test]
    fn terms_and_boolean_shapes() {
        close(run("rare").selectivity, 0.01);
        close(run("missing").selectivity, 0.0);
        close(run("half AND fifth").selectivity, 0.1);
        close(run("half fifth").selectivity, 0.1);
        close(run("half OR fifth").selectivity, 0.6);
        close(run("common AND NOT half").selectivity, 0.45);
        close(run("rare AND common").selectivity, 0.009);
        assert!(run("half AND fifth").exact);
    }

    #[test]
    fn phrases_are_bounded_by_the_rarest_term() {
        let phrase = run("\"common rare\"");
        close(phrase.selectivity, 0.005);
        assert!(phrase.exact);
        close(run("half NEAR/3 rare").selectivity, 0.005);
    }

    #[test]
    fn expansions_sum_over_the_dictionary() {
        let prefix = run("brew*");
        close(prefix.selectivity, 0.055);
        assert!(prefix.exact);
        close(run("brewer~1").selectivity, 0.05);
    }

    #[test]
    fn overflowed_expansions_are_a_rechecked_universe() {
        let mut table = corpus();
        table.cap = 1;
        let query = parse_tinql_to_query_default("brew*").unwrap();
        let overflow = estimate(&query, &table).unwrap();
        assert_eq!(overflow, Estimate::inexact_universe(OVERFLOW_SELECTIVITY));
        // NOT over a superset cannot be complemented: the exclusion keeps the
        // positive side's candidates, all of them rechecked.
        let negated = estimate(
            &parse_tinql_to_query_default("common AND NOT brew*").unwrap(),
            &table,
        )
        .unwrap();
        close(negated.selectivity, 0.81);
        close(negated.candidates, 0.9);
        assert!(!negated.exact);
        let narrowed = estimate(
            &parse_tinql_to_query_default("rare AND brew*").unwrap(),
            &table,
        )
        .unwrap();
        close(narrowed.candidates, 0.01);
        close(narrowed.selectivity, 0.001);
        assert!(!narrowed.exact);
    }

    #[test]
    fn empty_index_matches_nothing() {
        let table = Table::new(0.0, &[]);
        let query = parse_tinql_to_query_default("anything OR common").unwrap();
        assert_eq!(estimate(&query, &table).unwrap(), Estimate::exact(0.0));
    }

    #[test]
    fn match_all_and_at_least() {
        close(run("*").selectivity, 1.0);
        close(run("AT LEAST 2 OF [half fifth rare]").selectivity, 0.105);
    }
    #[test]
    fn index_statistics_subtract_known_deaths_and_preserve_buffer() {
        use segment::{
            Tid, forward::ForwardRecord, index::MutableIndex, postings::PostingsBuilder,
        };
        let index = MutableIndex::default();
        let buffer = MutableIndex::default();
        let mut dead = PostingsBuilder::default();
        for n in 0..2000 {
            let tid = Tid::new(n, 1).unwrap();
            let mut tokens = vec![("common", 0)];
            if n < 20 {
                tokens.push(("rare", 1));
            }
            index
                .add_record(ForwardRecord::from_tokens(tid, tokens).unwrap())
                .unwrap();
            if n < 10 || (100..590).contains(&n) {
                dead.push(tid).unwrap();
            }
        }
        buffer
            .add_record(
                ForwardRecord::from_tokens(Tid::new(2001, 1).unwrap(), [("rare", 0)]).unwrap(),
            )
            .unwrap();
        let bytes = dead.finish();
        let stats = IndexStatistics {
            sources: vec![
                (&index, Some(Postings::parse(&bytes).unwrap())),
                (&buffer, None),
            ],
            max_expansion: 10,
        };
        close(stats.documents().unwrap(), 1501.0);
        close(stats.document_frequency("rare").unwrap(), 11.0);
        close(stats.document_frequency("common").unwrap(), 1500.0);
        close(stats.document_frequency("missing").unwrap(), 0.0);
        close(
            stats
                .expansion_frequency(Window::Prefix("ra"), &|_| true)
                .unwrap()
                .unwrap(),
            11.0,
        );
        close(
            stats
                .expansion_frequency(Window::All, &|_| true)
                .unwrap()
                .unwrap(),
            1511.0,
        );
        let capped = IndexStatistics {
            max_expansion: 1,
            ..stats
        };
        assert_eq!(
            capped.expansion_frequency(Window::All, &|_| true).unwrap(),
            None
        );
    }

    #[test]
    fn wholly_dead_and_empty_sources_estimate_zero() {
        use segment::{
            Tid, forward::ForwardRecord, index::MutableIndex, postings::PostingsBuilder,
        };
        let index = MutableIndex::default();
        let empty = MutableIndex::default();
        let tid = Tid::new(0, 1).unwrap();
        index
            .add_record(ForwardRecord::from_tokens(tid, [("gone", 0)]).unwrap())
            .unwrap();
        let mut dead = PostingsBuilder::default();
        dead.push(tid).unwrap();
        let bytes = dead.finish();
        let stats = IndexStatistics {
            sources: vec![
                (&index, Some(Postings::parse(&bytes).unwrap())),
                (&empty, None),
            ],
            max_expansion: 10,
        };
        close(stats.documents().unwrap(), 0.0);
        close(stats.document_frequency("gone").unwrap(), 0.0);
        close(
            estimate(&parse_tinql_to_query_default("gone").unwrap(), &stats)
                .unwrap()
                .selectivity,
            0.0,
        );
    }
}
