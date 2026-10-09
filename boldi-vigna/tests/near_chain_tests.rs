// Copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

//! Chains of NEAR, tinql's left-deep `t0 NEAR/n t1 NEAR/n t2 …`:
//! `MAXGAPS(n, UNORDERED(MAXGAPS(n, UNORDERED(…)), t))`. Under a gap filter an
//! Unordered compiles to its disjoint form, one ordered walk per child order,
//! and those walks share each child that holds a further Unordered rather
//! than compiling it once per order (wp-m4.9-gate.md §U6.13). The shared
//! compile must answer exactly as private copies would: the explicit
//! `MAXGAPS(n, OR(ORDERED(a, b), ORDERED(b, a)))` expansion of every level.

use boldi_vigna::*;
use proptest::prelude::*;

fn collect(query: &SpanQuery, positions: &impl TermPositions) -> Vec<Interval> {
    let mut solver = SpanSolver::new(query).unwrap();
    solver.intervals(positions).collect()
}

fn near(max_gaps: u32, left: SpanQuery, right: SpanQuery) -> SpanQuery {
    SpanQuery::MaxGaps {
        max_gaps,
        inner: Box::new(SpanQuery::Unordered(vec![left, right])),
    }
}

/// A chain over terms `0..gaps.len() + 1`, level `i` joining term `i + 1`
/// under `gaps[i]`, the chain on the left of a level where `left[i]` holds.
fn chain(gaps: &[u32], left: &[bool]) -> SpanQuery {
    (gaps.iter().zip(left).enumerate()).fold(
        SpanQuery::Term(0),
        |chain, (level, (&max_gaps, &left))| {
            let term = SpanQuery::Term(level + 1);
            if left {
                near(max_gaps, chain, term)
            } else {
                near(max_gaps, term, chain)
            }
        },
    )
}

/// `query` with every NEAR written as the disjoint form private copies
/// compile to: both orders of its two children, each copy its own.
fn private_copies(query: &SpanQuery) -> SpanQuery {
    match query {
        SpanQuery::MaxGaps { max_gaps, inner } => match inner.as_ref() {
            SpanQuery::Unordered(children) => {
                let [a, b] = [&children[0], &children[1]].map(private_copies);
                SpanQuery::MaxGaps {
                    max_gaps: *max_gaps,
                    inner: Box::new(SpanQuery::Or(vec![
                        SpanQuery::Ordered(vec![a.clone(), b.clone()]),
                        SpanQuery::Ordered(vec![b, a]),
                    ])),
                }
            }
            other => SpanQuery::MaxGaps {
                max_gaps: *max_gaps,
                inner: Box::new(private_copies(other)),
            },
        },
        other => other.clone(),
    }
}

/// The 66-term chain U6.3's audit runs (`dd1 NEAR/99 w000 … NEAR/99 w064`):
/// it compiles (2^65 states when each order compiled its own copy of the
/// chain below) and answers a document holding every term in order, and
/// none missing one.
#[test]
fn a_66_term_near_chain_compiles_and_answers() {
    let query = chain(&[99; 65], &[true; 65]);
    let in_order: Vec<Vec<u32>> = (0..66).map(|position| vec![position]).collect();
    assert_eq!(collect(&query, &in_order), [Interval::new(0, 65)]);
    let mut missing = in_order.clone();
    missing[40].clear();
    assert!(collect(&query, &missing).is_empty());
    let reversed: Vec<Vec<u32>> = (0..66).rev().map(|position| vec![position]).collect();
    assert_eq!(collect(&query, &reversed), [Interval::new(0, 65)]);
}

fn positions_strategy(terms: usize) -> impl Strategy<Value = Vec<Vec<u32>>> {
    proptest::collection::vec(
        proptest::collection::btree_set(0u32..40, 0..6)
            .prop_map(|positions| positions.into_iter().collect::<Vec<_>>()),
        terms,
    )
}

fn chain_case() -> impl Strategy<Value = (Vec<u32>, Vec<bool>, Vec<Vec<u32>>)> {
    (1usize..=5).prop_flat_map(|depth| {
        (
            proptest::collection::vec(0u32..4, depth),
            proptest::collection::vec(any::<bool>(), depth),
            positions_strategy(depth + 1),
        )
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// A chain's shared compile answers exactly as its private copies.
    #[test]
    fn a_near_chain_answers_as_its_private_copies((gaps, left, positions) in chain_case()) {
        let query = chain(&gaps, &left);
        prop_assert_eq!(
            collect(&query, &positions),
            collect(&private_copies(&query), &positions),
            "{}", query
        );
    }
}
