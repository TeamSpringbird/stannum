// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Reading a candidate's positions one slot at a time, dropping it at the
//! first adjacent pair of leaves that cannot match.
//!
//! In a matching interval of an ordered span every leaf sits a bounded
//! distance after the leaf before it: one position in a phrase, up to the
//! slop plus one under a gap budget, exactly the pinned gap for `"a _ b"`.
//! A candidate no positions of which keep some pair's distance cannot
//! match, so a plan reads the rarest slot first, then the rarer neighbour
//! of the leaves read so far, and tests each pair as its second slot
//! arrives. Only a candidate every pair of which passes has the rest of
//! its slots read and the solver run; the solver's answer is the result.

use crate::positions::TermPositions;
use crate::query::SpanQuery;

/// One read of a slot's positions, and the pair of leaves it completes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    /// The slot to read, unless it was read by an earlier step.
    pub slot: usize,
    /// The adjacent pair (by its earlier leaf's index) to test once the
    /// slot is read; `None` for the first step.
    pub pair: Option<usize>,
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
        let slots = plan.leaves.iter().max().map_or(0, |slot| slot + 1);
        let mut read = vec![false; slots];
        let cost = |read: &[bool], leaf: usize| {
            let slot = plan.leaves[leaf];
            if read[slot] { 0 } else { rarity(slot) }
        };
        let seed = (0..plan.leaves.len())
            .min_by_key(|&leaf| cost(&read, leaf))
            .expect("a leaf");
        read[plan.leaves[seed]] = true;
        plan.steps.push(Step {
            slot: plan.leaves[seed],
            pair: None,
        });
        let (mut left, mut right) = (seed, seed);
        while left > 0 || right + 1 < plan.leaves.len() {
            let leftward = left > 0
                && (right + 1 >= plan.leaves.len()
                    || cost(&read, left - 1) <= cost(&read, right + 1));
            let (leaf, pair) = if leftward {
                left -= 1;
                (left, left)
            } else {
                right += 1;
                (right, right - 1)
            };
            read[plan.leaves[leaf]] = true;
            plan.steps.push(Step {
                slot: plan.leaves[leaf],
                pair: Some(pair),
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
    /// earlier leaf lies the pair's distance before some position of the
    /// later one. Both leaves' slots must be read.
    pub fn pair_keeps(&self, pair: usize, positions: &impl TermPositions) -> bool {
        let (lo, hi) = self.gaps[pair];
        keeps_distance(
            positions.positions(self.leaves[pair]),
            positions.positions(self.leaves[pair + 1]),
            lo,
            hi,
        )
    }

    /// Adds the leaves of `query`; `cap` is the gap budget of the nearest
    /// enclosing filter, which bounds the junctions of an ordered sequence
    /// directly under it.
    fn visit(&mut self, query: &SpanQuery, cap: Option<(u32, u32)>) -> Option<()> {
        use SpanQuery::*;
        match query {
            Term(slot) => self.leaves.push(*slot),
            Ordered(children) => {
                // Children follow one another without overlapping, so a
                // junction is at least one position; a budget of `max`
                // uncovered positions over the sequence caps every junction,
                // and pins the one junction of a pair to an exact budget.
                let (lo, hi) = match cap {
                    None => (1, u32::MAX),
                    Some((min, max)) => {
                        let hi = max.saturating_add(1);
                        (
                            if min == max && children.len() == 2 {
                                hi
                            } else {
                                1
                            },
                            hi,
                        )
                    }
                };
                for child in children {
                    let first = self.leaves.len();
                    self.visit(child, None)?;
                    if first > 0 && first < self.leaves.len() {
                        self.gaps.push((lo, hi));
                    }
                }
            }
            MaxGaps { max_gaps, inner } => self.visit(inner, Some((0, *max_gaps)))?,
            GapsInRange {
                min_gaps,
                max_gaps,
                inner,
            } => self.visit(inner, Some((*min_gaps, *max_gaps)))?,
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
            if let Some(pair) = step.pair {
                if !plan.pair_keeps(pair, &positions.to_vec()) {
                    return Some(false);
                }
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
    fn reads_rarest_first_then_rarer_neighbour() {
        let plan = PhrasePlan::new(&phrase(&[0, 1, 2, 3], 0), |s| [50, 5, 40, 1][s]).unwrap();
        let slots: Vec<_> = plan.steps().iter().map(|s| (s.slot, s.pair)).collect();
        assert_eq!(slots, [(3, None), (2, Some(2)), (1, Some(1)), (0, Some(0))]);
        // A repeated word is read once and still tested at every pair.
        let plan = PhrasePlan::new(&phrase(&[0, 1, 0, 0, 1, 0], 0), |s| [9, 3][s]).unwrap();
        let slots: Vec<_> = plan.steps().iter().map(|s| (s.slot, s.pair)).collect();
        assert_eq!(
            slots,
            [
                (1, None),
                (0, Some(0)),
                (0, Some(1)),
                (0, Some(2)),
                (1, Some(3)),
                (0, Some(4))
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
