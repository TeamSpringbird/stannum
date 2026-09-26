// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Reading a candidate's positions one slot at a time, dropping it at the
//! first pair of leaves that cannot match.
//!
//! In a matching interval of an ordered span every leaf sits a bounded
//! distance after the leaf before it: one position in a phrase, up to the
//! slop plus one under a gap budget, exactly the pinned gap for `"a _ b"`.
//! Any two leaves then lie within the sum of the bounds between them. A
//! candidate no positions of which keep some pair's distance cannot match,
//! so a plan reads the slots rarest first and tests each leaf, as its slot
//! arrives, against the nearest leaf read before it. Only a candidate every
//! pair of which passes has the rest of its slots read and the solver run;
//! the solver's answer is the result.
//!
//! Rarest first rather than outwards from the rarest leaf: a phrase's rare
//! words are mostly flanked by the commonest ones (`"a number i would"`),
//! whose position lists are the longest to reach and to read, and the two
//! rarest words at their distance are no less rare a pair.

use crate::positions::TermPositions;
use crate::query::SpanQuery;

/// One read of a slot's positions, and the pair of leaves it completes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    /// The slot to read, unless it was read by an earlier step.
    pub slot: usize,
    /// The pair of leaves, earlier first, to test once the slot is read;
    /// `None` for the first step.
    pub pair: Option<(usize, usize)>,
}

/// A span query's leaves in order, the distance every adjacent pair must
/// keep, and the order to read the slots in.
#[derive(Clone, Debug)]
pub struct PhrasePlan {
    /// The slot of each leaf, in the span's order.
    leaves: Vec<usize>,
    /// Per adjacent pair of leaves: the least and greatest distance from
    /// the earlier leaf's position to the later one's.
    gaps: Vec<(u32, u32)>,
    steps: Vec<Step>,
}

impl PhrasePlan {
    /// The plan of `query` with `rarity(slot)` the documents holding the
    /// slot's term, or `None` for a shape whose intervals are not spanned
    /// by their first and last leaf, or without a leaf.
    pub fn new(query: &SpanQuery, rarity: impl Fn(usize) -> u64) -> Option<Self> {
        let mut plan = Self {
            leaves: Vec::new(),
            gaps: Vec::new(),
            steps: Vec::new(),
        };
        plan.visit(query, None)?;
        if plan.leaves.is_empty() {
            return None;
        }
        debug_assert_eq!(plan.gaps.len() + 1, plan.leaves.len(), "a gap per junction");
        let slots = plan.leaves.iter().max().map_or(0, |slot| slot + 1);
        let mut read = vec![false; slots];
        let cost = |read: &[bool], leaf: usize| {
            let slot = plan.leaves[leaf];
            if read[slot] { 0 } else { rarity(slot) }
        };
        // Each step places the cheapest leaf left, a leaf whose slot is read
        // costing nothing, and tests it against the nearest leaf placed,
        // the earlier on a tie.
        let mut placed = vec![false; plan.leaves.len()];
        for _ in 0..plan.leaves.len() {
            let leaf = (0..plan.leaves.len())
                .filter(|&leaf| !placed[leaf])
                .min_by_key(|&leaf| cost(&read, leaf))
                .expect("a leaf left");
            let before = (0..leaf).rev().find(|&other| placed[other]);
            let after = (leaf + 1..plan.leaves.len()).find(|&other| placed[other]);
            let pair = match (before, after) {
                (Some(before), Some(after)) if after - leaf < leaf - before => Some((leaf, after)),
                (Some(before), _) => Some((before, leaf)),
                (None, Some(after)) => Some((leaf, after)),
                (None, None) => None,
            };
            placed[leaf] = true;
            read[plan.leaves[leaf]] = true;
            plan.steps.push(Step {
                slot: plan.leaves[leaf],
                pair,
            });
        }
        Some(plan)
    }

    /// The reads in order; each slot appears at its first read only if the
    /// caller skips slots read already.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Whether the candidate keeps `pair`'s distance: some position of the
    /// earlier leaf lies the sum of the distances between the two before
    /// some position of the later one. Both leaves' slots must be read.
    pub fn pair_keeps(&self, pair: (usize, usize), positions: &impl TermPositions) -> bool {
        let (earlier, later) = pair;
        let (lo, hi) = self.gaps[earlier..later]
            .iter()
            .fold((0u32, 0u32), |(lo, hi), gap| {
                (lo.saturating_add(gap.0), hi.saturating_add(gap.1))
            });
        keeps_distance(
            positions.positions(self.leaves[earlier]),
            positions.positions(self.leaves[later]),
            lo,
            hi,
        )
    }

    /// Adds the leaves of `query` and the gaps between them; `cap` is the
    /// range of gaps the filters directly over `query` allow its interval,
    /// which bounds the junctions of an ordered sequence directly under
    /// them.
    ///
    /// Each junction of an ordered sequence lies between the last leaf of
    /// one child and the first leaf of the next, whatever the nesting
    /// inside either, so its gap is pushed between the two children's
    /// leaves: `gaps[i]` stays the distance from leaf `i` to leaf `i + 1`.
    /// Pushed after the child instead, and whenever any leaf came before,
    /// an ordered child not first in the whole span pushed a gap before its
    /// own first leaf and its parent's junction landed after the child's
    /// inner gaps: `a THEN/1 "b c"` tested (a, b) at the phrase's distance
    /// and dropped "a x b c".
    fn visit(&mut self, query: &SpanQuery, cap: Option<(u32, u32)>) -> Option<()> {
        use SpanQuery::*;
        match query {
            Term(slot) => self.leaves.push(*slot),
            Ordered(children) => {
                // Children follow one another without overlapping, so a
                // junction is at least one position. The interval's gaps
                // are the uncovered positions between its children, the
                // sum over its junctions of each distance less one: a
                // budget of `max` caps every junction, and of a pair's one
                // junction the budget's least is a floor too. A child's own
                // gaps are its own, bounded only by filters over it.
                let (lo, hi) = match cap {
                    None => (1, u32::MAX),
                    Some((min, max)) => (
                        if children.len() == 2 {
                            min.saturating_add(1)
                        } else {
                            1
                        },
                        max.saturating_add(1),
                    ),
                };
                for (index, child) in children.iter().enumerate() {
                    if index > 0 {
                        self.gaps.push((lo, hi));
                    }
                    let first = self.leaves.len();
                    self.visit(child, None)?;
                    // A child without a leaf would leave its junction's gap
                    // between the wrong leaves.
                    if self.leaves.len() == first {
                        return None;
                    }
                }
            }
            // Filters over filters keep the interval only when every one
            // does: their ranges intersect.
            MaxGaps { max_gaps, inner } => self.visit(inner, Some(within(cap, 0, *max_gaps)))?,
            GapsInRange {
                min_gaps,
                max_gaps,
                inner,
            } => self.visit(inner, Some(within(cap, *min_gaps, *max_gaps)))?,
            // These keep an interval or drop it; its gaps are the inner's.
            MaxWidth { inner, .. } | WithinPositions { inner, .. } => self.visit(inner, cap)?,
            Empty
            | Unordered(_)
            | Or(_)
            | NotContaining { .. }
            | NotContainedBy { .. }
            | NonOverlapping { .. }
            | Containing { .. }
            | ContainedBy { .. }
            | Overlapping { .. }
            | Before { .. }
            | After { .. } => return None,
        }
        Some(())
    }
}

/// The gap range `min..=max` of a filter under the filters' `cap`.
fn within(cap: Option<(u32, u32)>, min: u32, max: u32) -> (u32, u32) {
    match cap {
        None => (min, max),
        Some((outer_min, outer_max)) => (outer_min.max(min), outer_max.min(max)),
    }
}

/// Whether some position of `earlier` lies `lo..=hi` before some position
/// of `later`; both ascend.
fn keeps_distance(earlier: &[u32], later: &[u32], lo: u32, hi: u32) -> bool {
    let mut j = 0;
    for &at in earlier {
        let Some(least) = at.checked_add(lo) else {
            return false;
        };
        while later.get(j).is_some_and(|&next| next < least) {
            j += 1;
        }
        match later.get(j) {
            None => return false,
            Some(&next) if next - at <= hi => return true,
            Some(_) => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SpanSolver;

    fn phrase(slots: &[usize], slop: u32) -> SpanQuery {
        SpanQuery::MaxGaps {
            max_gaps: slop,
            inner: Box::new(SpanQuery::Ordered(
                slots.iter().map(|s| SpanQuery::Term(*s)).collect(),
            )),
        }
    }

    fn pinned(left: SpanQuery, gap: u32, slot: usize) -> SpanQuery {
        SpanQuery::GapsInRange {
            min_gaps: gap,
            max_gaps: gap,
            inner: Box::new(SpanQuery::Ordered(vec![left, SpanQuery::Term(slot)])),
        }
    }

    /// Runs the plan over `positions`, reading slots in order: the pair
    /// tests must never reject a candidate the solver accepts.
    fn plan_says(query: &SpanQuery, rarity: &[u64], positions: &[Vec<u32>]) -> Option<bool> {
        let plan = PhrasePlan::new(query, |slot| rarity[slot])?;
        let mut read = vec![false; positions.len()];
        for step in plan.steps() {
            read[step.slot] = true;
            if let Some(pair) = step.pair
                && !plan.pair_keeps(pair, &positions.to_vec())
            {
                return Some(false);
            }
        }
        assert!(
            plan.leaves.iter().all(|slot| read[*slot]),
            "every slot read before the solver"
        );
        Some(true)
    }

    fn solver_says(query: &SpanQuery, positions: &[Vec<u32>]) -> bool {
        let mut solver = SpanSolver::new(query).unwrap();
        solver.intervals(&positions.to_vec()).next().is_some()
    }

    #[test]
    fn reads_rarest_first_testing_the_nearest_leaf_read() {
        let plan = PhrasePlan::new(&phrase(&[0, 1, 2, 3], 0), |s| [50, 5, 40, 1][s]).unwrap();
        let slots: Vec<_> = plan.steps().iter().map(|s| (s.slot, s.pair)).collect();
        assert_eq!(
            slots,
            [
                (3, None),
                (1, Some((1, 3))),
                (2, Some((1, 2))),
                (0, Some((0, 1)))
            ]
        );
        // A pair two leaves apart keeps the sum of their distances.
        let positions = vec![vec![], vec![4], vec![], vec![6]];
        assert!(plan.pair_keeps((1, 3), &positions));
        let positions = vec![vec![], vec![4], vec![], vec![5]];
        assert!(!plan.pair_keeps((1, 3), &positions));
        // A repeated word is read once, and its other leaves are placed
        // before any slot more is read.
        let plan = PhrasePlan::new(&phrase(&[0, 1, 0, 0, 1, 0], 0), |s| [9, 3][s]).unwrap();
        let slots: Vec<_> = plan.steps().iter().map(|s| (s.slot, s.pair)).collect();
        assert_eq!(
            slots,
            [
                (1, None),
                (1, Some((1, 4))),
                (0, Some((0, 1))),
                (0, Some((1, 2))),
                (0, Some((2, 3))),
                (0, Some((4, 5)))
            ]
        );
    }

    #[test]
    fn unsupported_shapes_and_empty_spans_have_no_plan() {
        assert!(PhrasePlan::new(&SpanQuery::Empty, |_| 1).is_none());
        let unordered = SpanQuery::MaxGaps {
            max_gaps: 2,
            inner: Box::new(SpanQuery::Unordered(vec![
                SpanQuery::Term(0),
                SpanQuery::Term(1),
            ])),
        };
        assert!(PhrasePlan::new(&unordered, |_| 1).is_none());
        assert!(PhrasePlan::new(&SpanQuery::Term(0), |_| 1).is_some());
    }

    #[test]
    fn pair_tests_never_reject_what_the_solver_accepts() {
        let queries = [
            phrase(&[0, 1], 0),
            phrase(&[0, 1, 2], 0),
            phrase(&[0, 0, 1], 0),
            phrase(&[0, 1, 0], 1),
            phrase(&[0, 1, 2], 2),
            pinned(SpanQuery::Term(0), 1, 1),
            pinned(SpanQuery::Term(0), 0, 0),
            pinned(pinned(SpanQuery::Term(0), 1, 1), 2, 2),
            SpanQuery::GapsInRange {
                min_gaps: 1,
                max_gaps: 2,
                inner: Box::new(SpanQuery::Ordered(vec![
                    SpanQuery::Term(0),
                    SpanQuery::Term(1),
                    SpanQuery::Term(2),
                ])),
            },
            SpanQuery::MaxWidth {
                max_width: 3,
                inner: Box::new(phrase(&[0, 1, 2], 1)),
            },
        ];
        // Every subset of positions 0..6 per slot, three slots.
        let lists: Vec<Vec<u32>> = (1u32..64)
            .map(|mask| (0..6).filter(|p| mask & (1 << p) != 0).collect())
            .collect();
        let mut dropped = 0;
        let mut matched = 0;
        for query in &queries {
            for a in &lists {
                for b in &lists {
                    for c in lists.iter().step_by(7) {
                        let positions = vec![a.clone(), b.clone(), c.clone()];
                        let solver = solver_says(query, &positions);
                        for rarity in [[1, 2, 3], [3, 2, 1], [2, 3, 1]] {
                            let plan = plan_says(query, &rarity, &positions).unwrap();
                            assert!(plan || !solver, "{query:?} {positions:?} {rarity:?}");
                            dropped += usize::from(!plan);
                            matched += usize::from(solver);
                        }
                    }
                }
            }
        }
        assert!(dropped > 0 && matched > 0);
    }

    /// `a THEN/1 "b c"`: the outer junction between `a` and the phrase
    /// takes up to one gap, the phrase's own junction none.
    #[test]
    fn a_phrase_after_the_first_operand_keeps_its_own_junction() {
        let query = SpanQuery::MaxGaps {
            max_gaps: 1,
            inner: Box::new(SpanQuery::Ordered(vec![
                SpanQuery::Term(0),
                phrase(&[1, 2], 0),
            ])),
        };
        let plan = PhrasePlan::new(&query, |s| [1, 2, 3][s]).unwrap();
        assert_eq!(plan.gaps, [(1, 2), (1, 1)]);
        // "a x b c": a at 0, b at 2, c at 3.
        let positions = vec![vec![0], vec![2], vec![3]];
        assert!(solver_says(&query, &positions));
        assert_eq!(plan_says(&query, &[1, 2, 3], &positions), Some(true));
        // `"a x" THEN/2 "b c"`: the outer junction sits between leaves 1
        // and 2.
        let query = SpanQuery::MaxGaps {
            max_gaps: 2,
            inner: Box::new(SpanQuery::Ordered(vec![
                phrase(&[0, 1], 0),
                phrase(&[2, 3], 0),
            ])),
        };
        let plan = PhrasePlan::new(&query, |_| 1).unwrap();
        assert_eq!(plan.gaps, [(1, 1), (1, 3), (1, 1)]);
    }

    mod random {
        use super::*;
        use proptest::prelude::*;

        fn cases(default: u32) -> u32 {
            std::env::var("PROPTEST_CASES")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        }

        const SLOTS: usize = 4;

        /// Nested ordered spans as the query languages build them:
        /// phrases with slop and pinned gaps, THEN/NEAR budgets over
        /// phrase and group operands, width and position windows.
        fn span() -> impl Strategy<Value = SpanQuery> {
            let leaf = (0..SLOTS).prop_map(SpanQuery::Term);
            leaf.prop_recursive(4, 24, 3, |inner| {
                let ordered =
                    prop::collection::vec(inner.clone(), 2..4).prop_map(SpanQuery::Ordered);
                prop_oneof![
                    2 => ordered.clone(),
                    3 => (ordered.clone(), 0u32..4).prop_map(|(inner, max_gaps)| {
                        SpanQuery::MaxGaps { max_gaps, inner: Box::new(inner) }
                    }),
                    2 => (inner.clone(), inner.clone(), 0u32..3).prop_map(|(left, right, gap)| {
                        SpanQuery::GapsInRange {
                            min_gaps: gap,
                            max_gaps: gap,
                            inner: Box::new(SpanQuery::Ordered(vec![left, right])),
                        }
                    }),
                    1 => (ordered.clone(), 0u32..3, 0u32..3).prop_map(|(inner, min, extra)| {
                        SpanQuery::GapsInRange {
                            min_gaps: min,
                            max_gaps: min + extra,
                            inner: Box::new(inner),
                        }
                    }),
                    1 => (inner.clone(), 1u32..8).prop_map(|(inner, max_width)| {
                        SpanQuery::MaxWidth { max_width, inner: Box::new(inner) }
                    }),
                    1 => (inner, 1u32..10).prop_map(|(inner, n)| SpanQuery::first_n(inner, n)),
                ]
            })
        }

        fn positions() -> impl Strategy<Value = Vec<Vec<u32>>> {
            prop::collection::vec(
                prop::collection::btree_set(0u32..14, 0..6)
                    .prop_map(|set| set.into_iter().collect::<Vec<_>>()),
                SLOTS,
            )
        }

        proptest! {
            #![proptest_config(ProptestConfig { cases: cases(2000), ..ProptestConfig::default() })]

            #[test]
            fn pair_tests_never_reject_what_the_solver_accepts_in_nested_spans(
                query in span(),
                documents in prop::collection::vec(positions(), 1..8),
                rarity in prop::collection::vec(1u64..5, SLOTS),
            ) {
                // A lone term has no pairs to test.
                if PhrasePlan::new(&query, |slot| rarity[slot]).is_none() {
                    return Ok(());
                }
                for positions in &documents {
                    let solver = solver_says(&query, positions);
                    let plan = plan_says(&query, &rarity, positions).unwrap();
                    prop_assert!(plan || !solver, "{:?} {:?} {:?}", query, positions, rarity);
                }
            }
        }
    }

    #[test]
    fn distance_is_tested_from_the_earlier_leaf() {
        assert!(keeps_distance(&[3], &[4], 1, 1));
        assert!(!keeps_distance(&[4], &[3], 1, 1));
        assert!(!keeps_distance(&[3], &[5], 1, 1));
        assert!(keeps_distance(&[3], &[5], 1, 2));
        assert!(keeps_distance(&[1, 9], &[2, 4, 10], 1, 1));
        assert!(!keeps_distance(&[1, 9], &[3, 5, 11], 1, 1));
        assert!(keeps_distance(&[0], &[u32::MAX], 1, u32::MAX));
        assert!(!keeps_distance(&[u32::MAX], &[u32::MAX], 1, 1));
        assert!(!keeps_distance(&[], &[1], 1, 1));
    }
}
