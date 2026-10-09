// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

use super::SpanPositionFilter;

#[derive(Debug, Clone, PartialEq)]
pub enum SpanExpr {
    /// Empty interval stream: matches nothing.
    Empty,
    Term(usize),
    Ordered(Vec<SpanExpr>),
    Unordered(Vec<SpanExpr>),
    Or(Vec<SpanExpr>),
    AtLeast {
        min: u32,
        children: Vec<SpanExpr>,
    },
    MaxGaps {
        max_gaps: u32,
        inner: Box<SpanExpr>,
    },
    GapsInRange {
        min_gaps: u32,
        max_gaps: u32,
        inner: Box<SpanExpr>,
    },
    MaxWidth {
        max_width: u32,
        inner: Box<SpanExpr>,
    },
    WithinPositions {
        inner: Box<SpanExpr>,
        lo: u32,
        hi: u32,
    },
    PositionFilter {
        inner: Box<SpanExpr>,
        filter: SpanPositionFilter,
    },
    Containing {
        big: Box<SpanExpr>,
        little: Box<SpanExpr>,
    },
    ContainedBy {
        little: Box<SpanExpr>,
        big: Box<SpanExpr>,
    },
    NotContaining {
        big: Box<SpanExpr>,
        little: Box<SpanExpr>,
    },
    NotContainedBy {
        little: Box<SpanExpr>,
        big: Box<SpanExpr>,
    },
    Overlapping {
        a: Box<SpanExpr>,
        b: Box<SpanExpr>,
    },
    NonOverlapping {
        a: Box<SpanExpr>,
        b: Box<SpanExpr>,
    },
    Before {
        a: Box<SpanExpr>,
        b: Box<SpanExpr>,
    },
    After {
        a: Box<SpanExpr>,
        b: Box<SpanExpr>,
    },
}

impl SpanExpr {
    pub fn needs_doc_length(&self) -> bool {
        match self {
            Self::Empty | Self::Term(_) => false,
            Self::Ordered(children) | Self::Unordered(children) | Self::Or(children) => {
                children.iter().any(Self::needs_doc_length)
            }
            Self::AtLeast { children, .. } => children.iter().any(Self::needs_doc_length),
            Self::MaxGaps { inner, .. }
            | Self::GapsInRange { inner, .. }
            | Self::MaxWidth { inner, .. }
            | Self::WithinPositions { inner, .. } => inner.needs_doc_length(),
            Self::PositionFilter { inner, filter } => {
                filter.needs_doc_length() || inner.needs_doc_length()
            }
            Self::Containing { big, little }
            | Self::ContainedBy {
                little: big,
                big: little,
            }
            | Self::NotContaining { big, little }
            | Self::NotContainedBy {
                little: big,
                big: little,
            }
            | Self::Overlapping { a: big, b: little }
            | Self::NonOverlapping { a: big, b: little }
            | Self::Before { a: big, b: little }
            | Self::After { a: big, b: little } => {
                big.needs_doc_length() || little.needs_doc_length()
            }
        }
    }

    pub fn all_terms_required(&self) -> bool {
        match self {
            Self::Empty => false,
            Self::Term(_) => true,
            Self::Ordered(children) | Self::Unordered(children) => {
                children.iter().all(Self::all_terms_required)
            }
            Self::Or(_) => false,
            Self::AtLeast { min, children } => {
                *min as usize == children.len() && children.iter().all(Self::all_terms_required)
            }
            Self::MaxGaps { inner, .. }
            | Self::GapsInRange { inner, .. }
            | Self::MaxWidth { inner, .. }
            | Self::WithinPositions { inner, .. }
            | Self::PositionFilter { inner, .. } => inner.all_terms_required(),
            Self::Containing { big, little }
            | Self::ContainedBy {
                little: big,
                big: little,
            }
            | Self::Overlapping { a: big, b: little }
            | Self::Before { a: big, b: little }
            | Self::After { a: big, b: little } => {
                big.all_terms_required() && little.all_terms_required()
            }
            Self::NotContaining { .. }
            | Self::NotContainedBy { .. }
            | Self::NonOverlapping { .. } => false,
        }
    }

    /// Calls `f` with the slot of every `Term` leaf outside an excluded
    /// operand, once per occurrence, left to right: the leaves a match's
    /// spans are built from ([`Self::for_each_term`]).
    pub fn for_each_positive_term(&self, f: &mut impl FnMut(usize)) {
        self.for_each_term(true, &mut |idx, included| {
            if included {
                f(idx);
            }
        });
    }

    /// Calls `f` with the slot of every `Term` leaf, once per occurrence, left
    /// to right, and whether it is included (`included` seeds the root). An
    /// excluded operand (`NotContaining`'s `little`, `NotContainedBy`'s
    /// `big`, `NonOverlapping`'s `b`) only filters its sibling's spans, so
    /// every leaf beneath it is excluded. Left to right is the order lowering
    /// writes the leaves in.
    pub(super) fn for_each_term(&self, included: bool, f: &mut impl FnMut(usize, bool)) {
        match self {
            Self::Empty => {}
            Self::Term(idx) => f(*idx, included),
            Self::Ordered(children)
            | Self::Unordered(children)
            | Self::Or(children)
            | Self::AtLeast { children, .. } => {
                for child in children {
                    child.for_each_term(included, f);
                }
            }
            Self::MaxGaps { inner, .. }
            | Self::GapsInRange { inner, .. }
            | Self::MaxWidth { inner, .. }
            | Self::WithinPositions { inner, .. }
            | Self::PositionFilter { inner, .. } => inner.for_each_term(included, f),
            Self::NotContaining {
                big: kept,
                little: excluded,
            }
            | Self::NotContainedBy {
                little: kept,
                big: excluded,
            }
            | Self::NonOverlapping {
                a: kept,
                b: excluded,
            } => {
                kept.for_each_term(included, f);
                excluded.for_each_term(false, f);
            }
            Self::Containing { big: a, little: b }
            | Self::ContainedBy { little: a, big: b }
            | Self::Overlapping { a, b }
            | Self::Before { a, b }
            | Self::After { a, b } => {
                a.for_each_term(included, f);
                b.for_each_term(included, f);
            }
        }
    }

    pub fn to_fast_path_root(
        &self,
    ) -> Option<(boldi_vigna::SpanQuery, Option<SpanPositionFilter>)> {
        match self {
            Self::PositionFilter { inner, filter } => {
                Some((inner.to_fast_path_body()?, Some(filter.clone())))
            }
            _ => Some((self.to_fast_path_body()?, None)),
        }
    }

    fn to_fast_path_body(&self) -> Option<boldi_vigna::SpanQuery> {
        crate::limits::check_stack();
        match self {
            Self::Empty => Some(boldi_vigna::SpanQuery::Empty),
            Self::Term(idx) => Some(boldi_vigna::SpanQuery::Term(*idx)),
            Self::Ordered(children) => Some(boldi_vigna::SpanQuery::Ordered(
                children
                    .iter()
                    .map(Self::to_fast_path_body)
                    .collect::<Option<Vec<_>>>()?,
            )),
            Self::Unordered(children) => Some(boldi_vigna::SpanQuery::Unordered(
                children
                    .iter()
                    .map(Self::to_fast_path_body)
                    .collect::<Option<Vec<_>>>()?,
            )),
            Self::Or(children) => Some(boldi_vigna::SpanQuery::Or(
                children
                    .iter()
                    .map(Self::to_fast_path_body)
                    .collect::<Option<Vec<_>>>()?,
            )),
            Self::AtLeast { .. } => None,
            Self::MaxGaps { max_gaps, inner } => Some(boldi_vigna::SpanQuery::MaxGaps {
                max_gaps: *max_gaps,
                inner: Box::new(inner.to_fast_path_body()?),
            }),
            Self::GapsInRange {
                min_gaps,
                max_gaps,
                inner,
            } => Some(boldi_vigna::SpanQuery::GapsInRange {
                min_gaps: *min_gaps,
                max_gaps: *max_gaps,
                inner: Box::new(inner.to_fast_path_body()?),
            }),
            Self::MaxWidth { max_width, inner } => Some(boldi_vigna::SpanQuery::MaxWidth {
                max_width: *max_width,
                inner: Box::new(inner.to_fast_path_body()?),
            }),
            Self::WithinPositions { inner, lo, hi } => {
                Some(boldi_vigna::SpanQuery::WithinPositions {
                    inner: Box::new(inner.to_fast_path_body()?),
                    lo: *lo,
                    hi: *hi,
                })
            }
            Self::PositionFilter { inner, filter } => {
                if filter.needs_doc_length() {
                    return None;
                }
                let window = filter.resolve_window(0)?;
                Some(boldi_vigna::SpanQuery::WithinPositions {
                    inner: Box::new(inner.to_fast_path_body()?),
                    lo: window.lo,
                    hi: window.hi,
                })
            }
            Self::Containing { big, little } => Some(boldi_vigna::SpanQuery::Containing {
                big: Box::new(big.to_fast_path_body()?),
                little: Box::new(little.to_fast_path_body()?),
            }),
            Self::ContainedBy { little, big } => Some(boldi_vigna::SpanQuery::ContainedBy {
                little: Box::new(little.to_fast_path_body()?),
                big: Box::new(big.to_fast_path_body()?),
            }),
            Self::NotContaining { big, little } => Some(boldi_vigna::SpanQuery::NotContaining {
                big: Box::new(big.to_fast_path_body()?),
                little: Box::new(little.to_fast_path_body()?),
            }),
            Self::NotContainedBy { little, big } => Some(boldi_vigna::SpanQuery::NotContainedBy {
                little: Box::new(little.to_fast_path_body()?),
                big: Box::new(big.to_fast_path_body()?),
            }),
            Self::Overlapping { a, b } => Some(boldi_vigna::SpanQuery::Overlapping {
                a: Box::new(a.to_fast_path_body()?),
                b: Box::new(b.to_fast_path_body()?),
            }),
            Self::NonOverlapping { a, b } => Some(boldi_vigna::SpanQuery::NonOverlapping {
                a: Box::new(a.to_fast_path_body()?),
                b: Box::new(b.to_fast_path_body()?),
            }),
            Self::Before { a, b } => Some(boldi_vigna::SpanQuery::Before {
                a: Box::new(a.to_fast_path_body()?),
                b: Box::new(b.to_fast_path_body()?),
            }),
            Self::After { a, b } => Some(boldi_vigna::SpanQuery::After {
                a: Box::new(a.to_fast_path_body()?),
                b: Box::new(b.to_fast_path_body()?),
            }),
        }
    }

    pub fn resolve(&self, search_doc_len: u32) -> boldi_vigna::SpanQuery {
        crate::limits::check_stack();
        match self {
            Self::Empty => boldi_vigna::SpanQuery::Empty,
            Self::Term(idx) => boldi_vigna::SpanQuery::Term(*idx),
            Self::Ordered(children) => boldi_vigna::SpanQuery::Ordered(
                children
                    .iter()
                    .map(|child| child.resolve(search_doc_len))
                    .collect(),
            ),
            Self::Unordered(children) => boldi_vigna::SpanQuery::Unordered(
                children
                    .iter()
                    .map(|child| child.resolve(search_doc_len))
                    .collect(),
            ),
            Self::Or(children) => {
                let children =
                    nonempty_children(children.iter().map(|child| child.resolve(search_doc_len)));
                match children.len() {
                    0 => boldi_vigna::SpanQuery::Empty,
                    1 => children.into_iter().next().unwrap(),
                    _ => boldi_vigna::SpanQuery::Or(children),
                }
            }
            Self::AtLeast { min, children } => {
                let children =
                    nonempty_children(children.iter().map(|child| child.resolve(search_doc_len)));
                at_least_query(*min, children)
            }
            Self::MaxGaps { max_gaps, inner } => boldi_vigna::SpanQuery::MaxGaps {
                max_gaps: *max_gaps,
                inner: Box::new(inner.resolve(search_doc_len)),
            },
            Self::GapsInRange {
                min_gaps,
                max_gaps,
                inner,
            } => boldi_vigna::SpanQuery::GapsInRange {
                min_gaps: *min_gaps,
                max_gaps: *max_gaps,
                inner: Box::new(inner.resolve(search_doc_len)),
            },
            Self::MaxWidth { max_width, inner } => boldi_vigna::SpanQuery::MaxWidth {
                max_width: *max_width,
                inner: Box::new(inner.resolve(search_doc_len)),
            },
            Self::WithinPositions { inner, lo, hi } => boldi_vigna::SpanQuery::WithinPositions {
                inner: Box::new(inner.resolve(search_doc_len)),
                lo: *lo,
                hi: *hi,
            },
            Self::PositionFilter { inner, filter } => match filter.resolve_window(search_doc_len) {
                Some(window) => boldi_vigna::SpanQuery::WithinPositions {
                    inner: Box::new(inner.resolve(search_doc_len)),
                    lo: window.lo,
                    hi: window.hi,
                },
                None => boldi_vigna::SpanQuery::Empty,
            },
            Self::Containing { big, little } => boldi_vigna::SpanQuery::Containing {
                big: Box::new(big.resolve(search_doc_len)),
                little: Box::new(little.resolve(search_doc_len)),
            },
            Self::ContainedBy { little, big } => boldi_vigna::SpanQuery::ContainedBy {
                little: Box::new(little.resolve(search_doc_len)),
                big: Box::new(big.resolve(search_doc_len)),
            },
            Self::NotContaining { big, little } => boldi_vigna::SpanQuery::NotContaining {
                big: Box::new(big.resolve(search_doc_len)),
                little: Box::new(little.resolve(search_doc_len)),
            },
            Self::NotContainedBy { little, big } => boldi_vigna::SpanQuery::NotContainedBy {
                little: Box::new(little.resolve(search_doc_len)),
                big: Box::new(big.resolve(search_doc_len)),
            },
            Self::Overlapping { a, b } => boldi_vigna::SpanQuery::Overlapping {
                a: Box::new(a.resolve(search_doc_len)),
                b: Box::new(b.resolve(search_doc_len)),
            },
            Self::NonOverlapping { a, b } => boldi_vigna::SpanQuery::NonOverlapping {
                a: Box::new(a.resolve(search_doc_len)),
                b: Box::new(b.resolve(search_doc_len)),
            },
            Self::Before { a, b } => boldi_vigna::SpanQuery::Before {
                a: Box::new(a.resolve(search_doc_len)),
                b: Box::new(b.resolve(search_doc_len)),
            },
            Self::After { a, b } => boldi_vigna::SpanQuery::After {
                a: Box::new(a.resolve(search_doc_len)),
                b: Box::new(b.resolve(search_doc_len)),
            },
        }
    }
}

impl SpanExpr {
    /// Nodes this expression has, and nodes [`Self::resolve`] builds from
    /// it at most, saturating: an `AT LEAST` that expands contributes its
    /// disjunction, a conjunction per combination, and a copy of each
    /// operand per combination that includes it. Resolving drops operands
    /// that resolve to nothing, which only shrinks the expansion.
    pub(crate) fn expansion_size(&self) -> (usize, usize) {
        crate::limits::check_stack();
        let sum = |children: &[Self]| {
            children
                .iter()
                .map(Self::expansion_size)
                .fold((1usize, 1usize), |(plain, resolved), (p, r)| {
                    (plain.saturating_add(p), resolved.saturating_add(r))
                })
        };
        match self {
            Self::Empty | Self::Term(_) => (1, 1),
            Self::Ordered(children) | Self::Unordered(children) | Self::Or(children) => {
                sum(children)
            }
            Self::AtLeast { min, children } => {
                let (plain, resolved) = sum(children);
                let combinations = at_least_combinations(*min, children.len());
                if combinations == 0 {
                    return (plain, resolved);
                }
                // Each operand is in C(k - 1, n - 1) of the combinations.
                let copies = binomial(children.len() - 1, *min as usize - 1);
                let operands = resolved - 1;
                (
                    plain,
                    operands
                        .saturating_mul(copies)
                        .saturating_add(combinations)
                        .saturating_add(1),
                )
            }
            Self::MaxGaps { inner, .. }
            | Self::GapsInRange { inner, .. }
            | Self::MaxWidth { inner, .. }
            | Self::WithinPositions { inner, .. }
            | Self::PositionFilter { inner, .. } => {
                let (plain, resolved) = inner.expansion_size();
                (plain.saturating_add(1), resolved.saturating_add(1))
            }
            Self::Containing { big: a, little: b }
            | Self::ContainedBy { little: a, big: b }
            | Self::NotContaining { big: a, little: b }
            | Self::NotContainedBy { little: a, big: b }
            | Self::Overlapping { a, b }
            | Self::NonOverlapping { a, b }
            | Self::Before { a, b }
            | Self::After { a, b } => {
                let (pa, ra) = a.expansion_size();
                let (pb, rb) = b.expansion_size();
                (
                    pa.saturating_add(pb).saturating_add(1),
                    ra.saturating_add(rb).saturating_add(1),
                )
            }
        }
    }
}

/// The combinations `AT LEAST min OF` `operands` operands expands to inside
/// a span context, saturating past [`crate::limits::MAX_AT_LEAST_COMBINATIONS`]:
/// C(operands, min), or zero when it is matched without expanding (a
/// threshold of at most one, or of every operand, or none can meet).
pub(crate) fn at_least_combinations(min: u32, operands: usize) -> usize {
    let min = min as usize;
    if min <= 1 || min >= operands {
        return 0;
    }
    binomial(operands, min)
}

/// C(`n`, `k`) for `k <= n`, or `usize::MAX` once it passes
/// [`crate::limits::MAX_AT_LEAST_COMBINATIONS`].
fn binomial(n: usize, k: usize) -> usize {
    // C(n, i) grows with i up to n / 2, so the product can stop as soon as
    // it passes the limit, and stays small enough not to overflow.
    let cap = crate::limits::MAX_AT_LEAST_COMBINATIONS as u128;
    let take = k.min(n - k) as u128;
    let n = n as u128;
    let mut combinations: u128 = 1;
    for i in 0..take {
        combinations = combinations * (n - i) / (i + 1);
        if combinations > cap {
            return usize::MAX;
        }
    }
    combinations as usize
}

fn nonempty_children(
    children: impl IntoIterator<Item = boldi_vigna::SpanQuery>,
) -> Vec<boldi_vigna::SpanQuery> {
    children
        .into_iter()
        .filter(|child| !matches!(child, boldi_vigna::SpanQuery::Empty))
        .collect()
}

fn at_least_query(min: u32, children: Vec<boldi_vigna::SpanQuery>) -> boldi_vigna::SpanQuery {
    if min == 0 {
        return boldi_vigna::SpanQuery::Empty;
    }

    let min = min as usize;
    if min > children.len() {
        return boldi_vigna::SpanQuery::Empty;
    }
    if children.is_empty() {
        return boldi_vigna::SpanQuery::Empty;
    }
    if min == 1 {
        return match children.len() {
            1 => children.into_iter().next().unwrap(),
            _ => boldi_vigna::SpanQuery::Or(children),
        };
    }
    if min == children.len() {
        return match children.len() {
            1 => children.into_iter().next().unwrap(),
            _ => boldi_vigna::SpanQuery::Unordered(children),
        };
    }

    let mut combinations = Vec::new();
    let mut current = Vec::with_capacity(min);
    collect_combinations(&children, min, 0, &mut current, &mut combinations);
    match combinations.len() {
        0 => boldi_vigna::SpanQuery::Empty,
        1 => combinations.into_iter().next().unwrap(),
        _ => boldi_vigna::SpanQuery::Or(combinations),
    }
}

fn collect_combinations(
    children: &[boldi_vigna::SpanQuery],
    choose: usize,
    start: usize,
    current: &mut Vec<boldi_vigna::SpanQuery>,
    out: &mut Vec<boldi_vigna::SpanQuery>,
) {
    crate::limits::check_stack();
    if current.len() == choose {
        out.push(boldi_vigna::SpanQuery::Unordered(current.clone()));
        if out.len().is_multiple_of(segment::INTERRUPT_INTERVAL) {
            segment::check_interrupts("at_least:combine");
        }
        return;
    }

    let remaining = choose - current.len();
    for idx in start..=children.len() - remaining {
        current.push(children[idx].clone());
        collect_combinations(children, choose, idx + 1, current, out);
        current.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::PositionFilterBound;

    #[test]
    fn nested_absolute_filters_can_still_fast_path() {
        let expr = SpanExpr::Before {
            a: Box::new(SpanExpr::PositionFilter {
                inner: Box::new(SpanExpr::Term(0)),
                filter: SpanPositionFilter::First(PositionFilterBound::Absolute(3)),
            }),
            b: Box::new(SpanExpr::Term(1)),
        };

        assert!(expr.to_fast_path_root().is_some());
    }

    #[test]
    fn nested_doc_relative_filters_require_advanced_path() {
        let expr = SpanExpr::Before {
            a: Box::new(SpanExpr::PositionFilter {
                inner: Box::new(SpanExpr::Term(0)),
                filter: SpanPositionFilter::Last(PositionFilterBound::Absolute(3)),
            }),
            b: Box::new(SpanExpr::Term(1)),
        };

        assert!(expr.to_fast_path_root().is_none());
    }

    #[test]
    fn resolves_at_least_two_to_pairwise_unordered_disjunction() {
        let expr = SpanExpr::AtLeast {
            min: 2,
            children: vec![SpanExpr::Term(0), SpanExpr::Term(1), SpanExpr::Term(2)],
        };

        assert_eq!(
            expr.resolve(10),
            boldi_vigna::SpanQuery::Or(vec![
                boldi_vigna::SpanQuery::Unordered(vec![
                    boldi_vigna::SpanQuery::Term(0),
                    boldi_vigna::SpanQuery::Term(1),
                ]),
                boldi_vigna::SpanQuery::Unordered(vec![
                    boldi_vigna::SpanQuery::Term(0),
                    boldi_vigna::SpanQuery::Term(2),
                ]),
                boldi_vigna::SpanQuery::Unordered(vec![
                    boldi_vigna::SpanQuery::Term(1),
                    boldi_vigna::SpanQuery::Term(2),
                ]),
            ])
        );
    }

    /// The size lowering bounds is the size resolving builds.
    #[test]
    fn expansion_size_counts_the_resolved_nodes() {
        fn nodes(query: &boldi_vigna::SpanQuery) -> usize {
            match query {
                boldi_vigna::SpanQuery::Or(children)
                | boldi_vigna::SpanQuery::Unordered(children) => {
                    1 + children.iter().map(nodes).sum::<usize>()
                }
                boldi_vigna::SpanQuery::MaxGaps { inner, .. } => 1 + nodes(inner),
                _ => 1,
            }
        }
        let at_least = |min, children: Vec<SpanExpr>| SpanExpr::AtLeast { min, children };
        let terms = |count: usize| (0..count).map(SpanExpr::Term).collect::<Vec<_>>();
        for operands in 1..9 {
            for min in 0..=operands as u32 + 1 {
                let flat = at_least(min, terms(operands));
                let nested = SpanExpr::MaxGaps {
                    max_gaps: 3,
                    inner: Box::new(at_least(
                        2,
                        vec![flat.clone(), SpanExpr::Term(20), SpanExpr::Term(21)],
                    )),
                };
                // Exact when the threshold expands; otherwise resolving may
                // collapse a node or two.
                let exact = at_least_combinations(min, operands) > 0;
                for expr in [flat, nested] {
                    let (bound, built) = (expr.expansion_size().1, nodes(&expr.resolve(10)));
                    assert!(bound >= built && (!exact || bound == built), "{expr:?}");
                }
            }
        }
        assert_eq!(at_least_combinations(15, 30), usize::MAX);
        assert_eq!(at_least_combinations(6, 16), 8008);
        assert_eq!(at_least_combinations(999, 1000), 1000);
        assert_eq!(at_least_combinations(1, 1000), 0);
        assert_eq!(at_least_combinations(1000, 1000), 0);
    }

    #[test]
    fn empty_runtime_filter_resolves_to_empty_query() {
        let expr = SpanExpr::PositionFilter {
            inner: Box::new(SpanExpr::Term(0)),
            filter: SpanPositionFilter::Middle { percent: 0 },
        };

        assert_eq!(expr.resolve(10), boldi_vigna::SpanQuery::Empty);
    }
}
