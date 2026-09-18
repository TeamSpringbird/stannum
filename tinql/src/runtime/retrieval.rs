//! Candidate retrieval from an immutable, in-memory inverted segment.
//!
//! This is the retrieval core, not a PostgreSQL access method or a durable file
//! format. The caller owns document identity, visibility and exact rechecks. An
//! unsupported query produces `All`, never an empty posting list. In particular,
//! complementing approximate candidates is unsafe in the presence of NOT.

use std::collections::{BTreeMap, BTreeSet};

use boldi_vigna::SpanQuery;

use super::{Query, SpanTermSlot, TokenizedDoc};

/// Opaque document identity. A PostgreSQL adapter must define its mapping to TIDs
/// and its handling of HOT chains and reused tuple locations before using this.
pub type DocumentId = u64;

/// Builds one immutable segment. Documents may arrive in any order; an identity
/// may be inserted only once. Replacements belong in a separate segment/lifecycle.
#[derive(Default)]
pub struct SegmentBuilder {
    documents: BTreeSet<DocumentId>,
    terms: BTreeMap<String, Vec<DocumentId>>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DuplicateDocument(pub DocumentId);

impl std::fmt::Display for DuplicateDocument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "document {} is already in this segment", self.0)
    }
}

impl std::error::Error for DuplicateDocument {}

impl SegmentBuilder {
    /// Index the same normalized tokens used by the exact evaluator. The caller
    /// must analyze queries with the same tokenizer configuration as documents.
    pub fn insert(
        &mut self,
        id: DocumentId,
        document: &TokenizedDoc,
    ) -> Result<(), DuplicateDocument> {
        if !self.documents.insert(id) {
            return Err(DuplicateDocument(id));
        }
        // One posting per document/term, independent of term frequency.
        for term in document.tokens().iter().collect::<BTreeSet<_>>() {
            self.terms.entry(term.clone()).or_default().push(id);
        }
        Ok(())
    }

    pub fn finish(mut self) -> Segment {
        for postings in self.terms.values_mut() {
            postings.sort_unstable();
        }
        Segment {
            document_count: self.documents.len(),
            terms: self.terms,
        }
    }
}

/// Sealed posting lists, strictly increasing and unique within each term.
/// No document text is retained or tokenized during candidate retrieval.
pub struct Segment {
    document_count: usize,
    terms: BTreeMap<String, Vec<DocumentId>>,
}

/// Every returned identity requires an exact query and visibility recheck.
/// `All` means the caller must fall back to its complete document source, even
/// when this segment contains no terms. `Ids` may be empty: that is a real miss.
pub enum Candidates<'a> {
    All,
    Ids(Box<dyn Iterator<Item = DocumentId> + 'a>),
}

impl Segment {
    pub fn document_count(&self) -> usize {
        self.document_count
    }

    pub fn document_frequency(&self, term: &str) -> usize {
        self.terms.get(term).map_or(0, Vec::len)
    }

    /// Streaming candidates in ascending identity order without duplicates.
    /// Iterator state grows with the query, not with the size of its result set.
    pub fn candidates(&self, query: &Query) -> Candidates<'_> {
        match query {
            Query::Term(term) => self.term(term),
            Query::And(a, b) => self.candidates(a).and(self.candidates(b)),
            Query::Or(a, b) => self.candidates(a).or(self.candidates(b)),
            Query::Conjunction(children) => children.iter().fold(Candidates::All, |ids, child| {
                ids.and(self.candidates(child))
            }),
            Query::Disjunction { min: 1, children } | Query::AtLeast { min: 1, children } => {
                children.iter().fold(Candidates::empty(), |ids, child| {
                    ids.or(self.candidates(child))
                })
            }
            Query::Boost { inner, .. } => self.candidates(inner),
            Query::Span {
                term_slots,
                span_query,
                ..
            } => self.span_candidates(span_query, term_slots),
            // A positional/fuzzy/regex/threshold restriction can be discarded
            // conservatively. Never drop such a branch from an OR union.
            Query::Not(_)
            | Query::MatchAll
            | Query::Regex(_)
            | Query::Range { .. }
            | Query::Fuzzy { .. }
            | Query::Disjunction { .. }
            | Query::AtLeast { .. }
            | Query::SpanExpr { .. } => Candidates::All,
        }
    }

    fn term(&self, term: &str) -> Candidates<'_> {
        Candidates::Ids(Box::new(
            self.terms.get(term).into_iter().flatten().copied(),
        ))
    }

    fn span_candidates(&self, query: &SpanQuery, slots: &[SpanTermSlot]) -> Candidates<'_> {
        match query {
            SpanQuery::Empty => Candidates::empty(),
            SpanQuery::Term(i) => match slots.get(*i) {
                Some(SpanTermSlot::Term(term)) => self.term(term),
                // Invalid slots still require the exact evaluator's error path.
                _ => Candidates::All,
            },
            SpanQuery::Ordered(children) | SpanQuery::Unordered(children) => {
                children.iter().fold(Candidates::All, |ids, child| {
                    ids.and(self.span_candidates(child, slots))
                })
            }
            SpanQuery::Or(children) => children.iter().fold(Candidates::empty(), |ids, child| {
                ids.or(self.span_candidates(child, slots))
            }),
            SpanQuery::MaxGaps { inner, .. }
            | SpanQuery::GapsInRange { inner, .. }
            | SpanQuery::MaxWidth { inner, .. }
            | SpanQuery::WithinPositions { inner, .. } => self.span_candidates(inner, slots),
            // Negative relations require only the retained side. Intersecting
            // both sides would discard precisely the matches they express.
            SpanQuery::NotContaining { big, .. } => self.span_candidates(big, slots),
            SpanQuery::NotContainedBy { little, .. } => self.span_candidates(little, slots),
            SpanQuery::NonOverlapping { a, .. } => self.span_candidates(a, slots),
            SpanQuery::Containing { big: a, little: b }
            | SpanQuery::ContainedBy { little: a, big: b }
            | SpanQuery::Overlapping { a, b }
            | SpanQuery::Before { a, b }
            | SpanQuery::After { a, b } => self
                .span_candidates(a, slots)
                .and(self.span_candidates(b, slots)),
        }
    }
}

impl<'a> Candidates<'a> {
    fn empty() -> Self {
        Self::Ids(Box::new(std::iter::empty()))
    }

    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::All, ids) | (ids, Self::All) => ids,
            (Self::Ids(a), Self::Ids(b)) => {
                let (mut a, mut b) = (a.peekable(), b.peekable());
                Self::Ids(Box::new(std::iter::from_fn(move || {
                    loop {
                        match a.peek()?.cmp(b.peek()?) {
                            std::cmp::Ordering::Less => {
                                a.next();
                            }
                            std::cmp::Ordering::Greater => {
                                b.next();
                            }
                            std::cmp::Ordering::Equal => {
                                b.next();
                                return a.next();
                            }
                        }
                    }
                })))
            }
        }
    }

    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Self::All,
            (Self::Ids(a), Self::Ids(b)) => {
                let (mut a, mut b) = (a.peekable(), b.peekable());
                Self::Ids(Box::new(std::iter::from_fn(move || {
                    match (a.peek(), b.peek()) {
                        (Some(x), Some(y)) if x == y => {
                            b.next();
                            a.next()
                        }
                        (Some(x), Some(y)) if x < y => a.next(),
                        (Some(_), Some(_)) | (None, Some(_)) => b.next(),
                        (Some(_), None) => a.next(),
                        (None, None) => None,
                    }
                })))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{evaluate, parse_tinql_to_query_default, tokenize_doc};
    use tokenizer::presets::default_pipeline;

    fn document(text: &str) -> TokenizedDoc {
        tokenize_doc(text, default_pipeline())
    }

    fn query(text: &str) -> Query {
        parse_tinql_to_query_default(text).unwrap_or_else(|error| panic!("{text}: {error}"))
    }

    fn ids(segment: &Segment, text: &str) -> Vec<DocumentId> {
        match segment.candidates(&query(text)) {
            Candidates::Ids(ids) => ids.collect(),
            Candidates::All => panic!("expected selective candidates for {text}"),
        }
    }

    #[test]
    fn postings_are_sorted_unique_and_do_not_retain_document_text() {
        let mut builder = SegmentBuilder::default();
        builder.insert(9, &document("Beer beer WINE")).unwrap();
        builder.insert(1, &document("beer")).unwrap();
        builder.insert(4, &document("wine")).unwrap();
        assert_eq!(
            builder.insert(9, &document("poison")),
            Err(DuplicateDocument(9))
        );
        let segment = builder.finish();
        assert_eq!(segment.document_count(), 3);
        assert_eq!(segment.document_frequency("beer"), 2);
        assert_eq!(ids(&segment, "BEER"), vec![1, 9]);
        assert_eq!(ids(&segment, "beer AND wine"), vec![9]);
        assert_eq!(ids(&segment, "beer OR wine"), vec![1, 4, 9]);
        assert!(ids(&segment, "poison").is_empty());
        assert!(ids(&segment, "absent AND wine").is_empty());
    }

    #[test]
    fn phrase_candidates_require_rechecking_positions() {
        let mut builder = SegmentBuilder::default();
        builder.insert(1, &document("alpha beta")).unwrap();
        builder.insert(2, &document("beta alpha")).unwrap();
        builder.insert(3, &document("alpha")).unwrap();
        let segment = builder.finish();
        assert_eq!(ids(&segment, "\"alpha beta\""), vec![1, 2]);
        assert!(
            !evaluate(&query("\"alpha beta\""), &document("beta alpha"))
                .unwrap()
                .matched
        );
    }

    #[test]
    fn unsupported_is_distinct_from_missing_and_never_loses_an_or_branch() {
        let mut builder = SegmentBuilder::default();
        builder.insert(1, &document("beer wine")).unwrap();
        builder.insert(2, &document("wine")).unwrap();
        let segment = builder.finish();
        assert!(matches!(
            segment.candidates(&query("beer OR win*")),
            Candidates::All
        ));
        assert_eq!(ids(&segment, "beer AND win*"), vec![1]);
        // NOT is evaluated by the reference evaluator, not by complementing
        // possibly incomplete postings or other approximate candidate sets.
        let not = Query::Not(Box::new(query("beer")));
        assert!(matches!(segment.candidates(&not), Candidates::All));
        assert!(matches!(
            segment.candidates(&Query::Or(Box::new(query("absent")), Box::new(not))),
            Candidates::All
        ));
        assert!(ids(&segment, "absent").is_empty());
    }

    #[test]
    fn streaming_union_does_not_collect_its_inputs() {
        use std::cell::Cell;
        let reads = Cell::new(0);
        let a = Candidates::Ids(Box::new(
            (0..1_000_000)
                .step_by(2)
                .inspect(|_| reads.set(reads.get() + 1)),
        ));
        let b = Candidates::Ids(Box::new(
            (1..1_000_000)
                .step_by(2)
                .inspect(|_| reads.set(reads.get() + 1)),
        ));
        let Candidates::Ids(mut union) = a.or(b) else {
            unreachable!()
        };
        assert_eq!(union.next(), Some(0));
        assert!(reads.get() <= 2);
    }

    #[test]
    fn streaming_algebra_agrees_with_independent_sets() {
        for left in 0..64_u64 {
            for right in 0..64_u64 {
                let a: BTreeSet<_> = (0..6).filter(|i| left & (1 << i) != 0).collect();
                let b: BTreeSet<_> = (0..6).filter(|i| right & (1 << i) != 0).collect();
                let make = |set: &BTreeSet<DocumentId>| {
                    Candidates::Ids(Box::new(
                        set.iter().copied().collect::<Vec<_>>().into_iter(),
                    ))
                };
                let Candidates::Ids(and) = make(&a).and(make(&b)) else {
                    unreachable!()
                };
                let Candidates::Ids(or) = make(&a).or(make(&b)) else {
                    unreachable!()
                };
                assert_eq!(
                    and.collect::<Vec<_>>(),
                    a.intersection(&b).copied().collect::<Vec<_>>()
                );
                assert_eq!(
                    or.collect::<Vec<_>>(),
                    a.union(&b).copied().collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn exhaustive_small_corpus_candidates_cover_reference_matches() {
        // Every sequence of length 0..4 over three tokens, plus normalization cases.
        let mut texts = vec![String::new(), "BEER, wine!".into(), "jalapeño beer".into()];
        for length in 1..=4 {
            for mut value in 0..3_usize.pow(length) {
                let mut words = Vec::new();
                for _ in 0..length {
                    words.push(["beer", "wine", "craft"][value % 3]);
                    value /= 3;
                }
                texts.push(words.join(" "));
            }
        }
        let documents: Vec<_> = texts.iter().map(|s| document(s)).collect();
        let mut builder = SegmentBuilder::default();
        for (id, doc) in documents.iter().enumerate() {
            builder.insert(id as u64, doc).unwrap();
        }
        let segment = builder.finish();
        let queries = [
            "beer",
            "missing",
            "beer AND wine",
            "beer OR wine",
            "beer AND NOT wine",
            "(beer OR wine) AND craft",
            "beer OR win*",
            "beer AND win*",
            "beer^2",
            "\"beer wine\"",
            "\"beer beer\"",
            "beer THEN/1 wine",
            "beer NEAR/1 wine",
            "beer NOT ENCLOSES wine",
            "beer NOT ENCLOSED BY wine",
            "beer NOT OVERLAPPING wine",
            "beer ENCLOSES wine",
            "beer ENCLOSED BY wine",
            "beer OVERLAPPING wine",
            "beer BEFORE wine",
            "beer AFTER wine",
            "beer IN FIRST 25%",
            "beer IN LAST 25%",
            "beer~1",
            "beer OR beer~1",
            "beer AND (wine OR craft)",
            "AT LEAST 2 OF [beer, wine, craft]",
            "*",
            "",
            "jalapeño",
        ];
        for text in queries {
            let q = query(text);
            let candidates: BTreeSet<_> = match segment.candidates(&q) {
                Candidates::All => (0..documents.len() as u64).collect(),
                Candidates::Ids(ids) => ids.collect(),
            };
            for (id, doc) in documents.iter().enumerate() {
                if evaluate(&q, doc).unwrap().matched {
                    assert!(
                        candidates.contains(&(id as u64)),
                        "false negative: {text} on {:?}",
                        texts[id]
                    );
                }
            }
        }
    }
}
