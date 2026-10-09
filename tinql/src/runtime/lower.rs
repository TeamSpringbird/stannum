// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

//! Lowers a `tinql::Expr` into the shared runtime `Query` / `SpanQuery`
//! representation.
//!
//! The lowering pass also canonicalizes boolean structure:
//!
//! - contiguous AND chains become `Query::Conjunction`
//! - OR and AT LEAST become `Query::Disjunction { min, children }`
//! - duplicate terms are removed from flat AND chains and plain OR chains
//! - `MatchAll` is folded out of compound boolean nodes
//! - single-term phrases collapse to plain `Term` nodes

use std::cell::Cell;

use rustc_hash::FxHashMap;

use crate::limits::{
    MAX_AT_LEAST_COMBINATIONS, MAX_QUERY_REGEX_BYTES, MAX_SPAN_EXPANSION, MAX_SPAN_NESTING,
};

use super::{
    CompiledRegex, PositionFilterBound, Query, RangeBound, SimplificationProfile, SpanExpr,
    SpanLeafBoosts, SpanPositionFilter, SpanTermSlot, simplify,
};

#[derive(Debug, thiserror::Error)]
pub enum LowerError {
    #[error("MatchAll (*) is not valid inside a span/positional context")]
    MatchAllInSpanContext,
    #[error("{0}")]
    InvalidRegex(#[from] super::RegexError),
    #[error(
        "query nesting exceeds {limit} levels once lowered (a proximity operator or phrase \
         nests one level per operand)",
        limit = MAX_SPAN_NESTING
    )]
    NestingTooDeep,
    #[error(
        "AT LEAST {min} OF {operands} operands inside a proximity operator expands to more than \
         {limit} combinations",
        limit = MAX_AT_LEAST_COMBINATIONS
    )]
    TooManyCombinations { min: u32, operands: usize },
    #[error(
        "AT LEAST inside a proximity operator expands the query by more than {limit} operands",
        limit = MAX_SPAN_EXPANSION
    )]
    ExpansionTooLarge,
    #[error(
        "the query's regexes compile to more than {limit} MiB",
        limit = MAX_QUERY_REGEX_BYTES >> 20
    )]
    RegexesTooLarge,
}

impl LowerError {
    /// Whether the query is refused for exceeding a limit of
    /// [`crate::limits`] rather than for being invalid.
    #[must_use]
    pub const fn exceeds_limit(&self) -> bool {
        match self {
            Self::NestingTooDeep
            | Self::TooManyCombinations { .. }
            | Self::ExpansionTooLarge
            | Self::RegexesTooLarge => true,
            Self::InvalidRegex(error) => error.exceeds_limit(),
            Self::MatchAllInSpanContext => false,
        }
    }
}

pub fn lower(expr: &crate::Expr) -> Result<Query, LowerError> {
    lower_with_profile(expr, SimplificationProfile::Structural)
}

pub fn lower_with_profile(
    expr: &crate::Expr,
    profile: SimplificationProfile,
) -> Result<Query, LowerError> {
    REGEX_BYTES.set(0);
    Ok(simplify(lower_boolean(expr, 1)?, profile))
}

thread_local! {
    /// Bytes the regexes and wildcards compiled so far by the lowering
    /// under way take (see [`compile_regex`]).
    static REGEX_BYTES: Cell<usize> = const { Cell::new(0) };
}

/// Compiles `pattern`, within [`crate::limits::MAX_REGEX_BYTES`] and,
/// with the regexes the lowering compiled before it, within
/// [`MAX_QUERY_REGEX_BYTES`]. The total is counted as they are compiled, so
/// the memory is bounded before the query is refused.
fn compile_regex(pattern: &str) -> Result<CompiledRegex, LowerError> {
    let regex = CompiledRegex::new(pattern)?;
    let total = REGEX_BYTES.get().saturating_add(regex.memory_usage());
    REGEX_BYTES.set(total);
    if total > MAX_QUERY_REGEX_BYTES {
        return Err(LowerError::RegexesTooLarge);
    }
    Ok(regex)
}

/// Lowers `expr`, which sits `depth` levels deep in the lowered query.
fn lower_boolean(expr: &crate::Expr, depth: usize) -> Result<Query, LowerError> {
    crate::limits::check_stack();
    use crate::Expr;

    // The parser bounds an expression's height, so this only guards callers
    // that lower a tree built some other way.
    if depth > MAX_SPAN_NESTING {
        return Err(LowerError::NestingTooDeep);
    }
    let lower_all = |exprs: &[Expr]| {
        exprs
            .iter()
            .map(|expr| lower_boolean(expr, depth + 1))
            .collect::<Result<Vec<_>, _>>()
    };
    match expr {
        Expr::Term(_)
        | Expr::MatchAll
        | Expr::MatchNone
        | Expr::Fuzzy { .. }
        | Expr::Wildcard(_)
        | Expr::Regex(_)
        | Expr::Range { .. } => lower_leaf(expr),
        // A chain lowers to one flat node. The left-deep binary nodes the
        // parser once built lowered to nested ones, which simplification
        // flattened into exactly this.
        Expr::And(operands) => Ok(Query::Conjunction(lower_all(operands)?)),
        Expr::Or(operands) => Ok(Query::Disjunction {
            min: 1,
            children: lower_all(operands)?,
        }),
        Expr::AndNot { positive, negative } => Ok(Query::Conjunction(vec![
            lower_boolean(positive, depth + 1)?,
            Query::Not(Box::new(lower_boolean(negative, depth + 2)?)),
        ])),
        Expr::Alternatives(xs) => Ok(Query::Disjunction {
            min: 1,
            children: lower_all(xs)?,
        }),
        Expr::AtLeast { threshold, exprs } => {
            let min = resolve_threshold(threshold, exprs.len());
            let children = lower_all(exprs)?;
            Ok(Query::Disjunction { min, children })
        }
        Expr::Boost { factor, inner } => Ok(Query::Boost {
            factor: factor.0,
            inner: Box::new(lower_boolean(inner, depth + 1)?),
        }),
        // A single-term phrase is just a term — no span machinery needed.
        Expr::Phrase { elements, .. }
            if elements.len() == 1 && matches!(elements[0], crate::PhraseElement::Term(_)) =>
        {
            match &elements[0] {
                crate::PhraseElement::Term(s) => Ok(Query::Term(s.clone())),
                _ => unreachable!(),
            }
        }
        Expr::Phrase { .. }
        | Expr::Then { .. }
        | Expr::Near { .. }
        | Expr::Encloses { .. }
        | Expr::NotEncloses { .. }
        | Expr::EnclosedBy { .. }
        | Expr::NotEnclosedBy { .. }
        | Expr::Overlapping { .. }
        | Expr::NotOverlapping { .. }
        | Expr::Before { .. }
        | Expr::After { .. }
        | Expr::First { .. }
        | Expr::Last { .. }
        | Expr::Middle { .. }
        | Expr::Between { .. }
        | Expr::Within { .. } => lower_as_span(expr, depth),
    }
}

/// Lowers a leaf. Out of line, so that compiling a regex does not weigh on
/// every level of the recursion.
#[inline(never)]
fn lower_leaf(expr: &crate::Expr) -> Result<Query, LowerError> {
    use crate::Expr;

    match expr {
        Expr::Term(s) => Ok(Query::Term(s.clone())),
        Expr::MatchAll => Ok(Query::MatchAll),
        // The canonical match-nothing query; simplify folds it out of
        // enclosing boolean nodes without disturbing AT LEAST thresholds.
        Expr::MatchNone => Ok(Query::Not(Box::new(Query::MatchAll))),
        Expr::Fuzzy {
            term,
            prefix,
            distance,
        } => Ok(Query::Fuzzy {
            term: term.clone(),
            prefix: *prefix,
            distance: *distance,
        }),
        Expr::Wildcard(parts) => Ok(Query::Regex(compile_regex(&wildcard_parts_regex(parts))?)),
        Expr::Regex(pat) => Ok(Query::Regex(compile_regex(pat)?)),
        Expr::Range { lower, upper } => Ok(Query::Range {
            lower: convert_range_bound(lower),
            upper: convert_range_bound(upper),
        }),
        _ => unreachable!("lower_leaf takes leaves"),
    }
}

#[inline(never)]
fn lower_as_span(expr: &crate::Expr, depth: usize) -> Result<Query, LowerError> {
    let mut builder = SpanBuilder::new(depth);
    let span_expr = builder.lower_span_expr(expr)?;
    let leaf_boosts = builder.leaf_boosts(&span_expr);
    let (plain, resolved) = span_expr.expansion_size();
    if resolved.saturating_sub(plain) > MAX_SPAN_EXPANSION {
        return Err(LowerError::ExpansionTooLarge);
    }
    if let Some((span_query, position_filter)) = span_expr.to_fast_path_root() {
        Ok(Query::Span {
            term_slots: builder.term_slots,
            span_query,
            position_filter,
            leaf_boosts,
        })
    } else {
        Ok(Query::SpanExpr {
            term_slots: builder.term_slots,
            span_expr,
            leaf_boosts,
        })
    }
}

struct SpanBuilder {
    term_slots: Vec<SpanTermSlot>,
    intern_map: FxHashMap<SpanTermSlot, usize>,
    /// Levels above the node being lowered, the query's included.
    depth: usize,
    /// The product of the `^` factors around the operand being lowered.
    boost: Option<f32>,
    /// `boost` at each leaf, in the order the leaves are lowered.
    written_boosts: Vec<Option<f32>>,
}

impl SpanBuilder {
    fn new(depth: usize) -> Self {
        Self {
            term_slots: Vec::new(),
            intern_map: FxHashMap::default(),
            depth: depth.saturating_sub(1),
            boost: None,
            written_boosts: Vec::new(),
        }
    }

    /// Lowers `exprs` as operands nested `extra` levels below the node being
    /// lowered (a left-deep chain puts its first operand deepest).
    fn lower_nested(
        &mut self,
        exprs: &[crate::Expr],
        extra: usize,
    ) -> Result<Vec<SpanExpr>, LowerError> {
        self.depth += extra;
        let lowered = exprs
            .iter()
            .map(|expr| self.lower_span_expr(expr))
            .collect();
        self.depth -= extra;
        lowered
    }

    /// The left-deep binary chain `node(node(a, b), c)` over `operands`,
    /// the shape an AND or OR chain has always had inside a span: an
    /// unordered conjunction's gaps and repeated-operand rules differ
    /// between `(a b) c` and `a b c`, so it is not flattened.
    fn lower_chain(
        &mut self,
        operands: &[crate::Expr],
        node: fn(Vec<SpanExpr>) -> SpanExpr,
    ) -> Result<SpanExpr, LowerError> {
        let lowered = self.lower_nested(operands, operands.len().saturating_sub(2))?;
        let mut lowered = lowered.into_iter();
        let first = lowered.next().expect("a chain has operands");
        Ok(lowered.fold(first, |acc, next| node(vec![acc, next])))
    }

    fn lower_span_expr(&mut self, expr: &crate::Expr) -> Result<SpanExpr, LowerError> {
        crate::limits::check_stack();
        self.depth += 1;
        if self.depth > MAX_SPAN_NESTING {
            return Err(LowerError::NestingTooDeep);
        }
        let lowered = self.lower_span_node(expr);
        self.depth -= 1;
        lowered
    }

    /// One written occurrence of `slot`, under the current boost. Every
    /// occurrence of a slot shares its interned index.
    fn leaf(&mut self, slot: SpanTermSlot) -> SpanExpr {
        self.written_boosts.push(self.boost);
        let term_slots = &mut self.term_slots;
        let idx = *self.intern_map.entry(slot).or_insert_with_key(|slot| {
            term_slots.push(slot.clone());
            term_slots.len() - 1
        });
        SpanExpr::Term(idx)
    }

    /// The written boosts of the leaves `span_expr` scores. Lowering writes
    /// leaves in the tree's left-to-right order, the order `for_each_term`
    /// walks, so the two line up one for one.
    fn leaf_boosts(&self, span_expr: &SpanExpr) -> SpanLeafBoosts {
        let mut written = self.written_boosts.iter();
        let mut included = Vec::new();
        span_expr.for_each_term(true, &mut |_, scored| {
            let boost = written.next().expect("lowering writes a boost per leaf");
            if scored {
                included.push(*boost);
            }
        });
        debug_assert!(
            written.next().is_none(),
            "every written leaf is in the tree"
        );
        if included.iter().all(Option::is_none) {
            SpanLeafBoosts::default()
        } else {
            SpanLeafBoosts(included.into())
        }
    }

    fn lower_span_node(&mut self, expr: &crate::Expr) -> Result<SpanExpr, LowerError> {
        use crate::Expr;

        match expr {
            Expr::Term(_)
            | Expr::MatchAll
            | Expr::MatchNone
            | Expr::Fuzzy { .. }
            | Expr::Wildcard(_)
            | Expr::Regex(_)
            | Expr::Range { .. }
            | Expr::Phrase { .. } => self.lower_span_leaf(expr),
            Expr::And(operands) => self.lower_chain(operands, SpanExpr::Unordered),
            Expr::Or(operands) => self.lower_chain(operands, SpanExpr::Or),
            _ => self.lower_span_operator(expr),
        }
    }

    /// Lowers a leaf or a phrase. Out of line, so that compiling a regex
    /// does not weigh on every level of the recursion.
    #[inline(never)]
    fn lower_span_leaf(&mut self, expr: &crate::Expr) -> Result<SpanExpr, LowerError> {
        use crate::Expr;

        match expr {
            Expr::Term(s) => Ok(self.leaf(SpanTermSlot::Term(s.clone()))),
            Expr::MatchAll => Err(LowerError::MatchAllInSpanContext),
            Expr::MatchNone => Ok(SpanExpr::Empty),
            Expr::Fuzzy {
                term,
                prefix,
                distance,
            } => Ok(self.leaf(SpanTermSlot::Fuzzy {
                term: term.clone(),
                prefix: *prefix,
                distance: *distance,
            })),
            Expr::Wildcard(parts) => Ok(self.leaf(SpanTermSlot::Regex(compile_regex(
                &wildcard_parts_regex(parts),
            )?))),
            Expr::Regex(pat) => Ok(self.leaf(SpanTermSlot::Regex(compile_regex(pat)?))),
            Expr::Range { lower, upper } => Ok(self.leaf(SpanTermSlot::Range {
                lower: convert_range_bound(lower),
                upper: convert_range_bound(upper),
            })),
            Expr::Phrase { elements, slop } => self.lower_phrase(elements, *slop),
            _ => unreachable!("lower_span_leaf takes leaves and phrases"),
        }
    }

    /// Lowers an operator other than an AND or OR chain.
    fn lower_span_operator(&mut self, expr: &crate::Expr) -> Result<SpanExpr, LowerError> {
        use crate::Expr;

        match expr {
            Expr::AndNot { positive, negative } => {
                let big = self.lower_span_expr(positive)?;
                let little = self.lower_span_expr(negative)?;
                Ok(SpanExpr::NotContaining {
                    big: Box::new(big),
                    little: Box::new(little),
                })
            }
            Expr::Alternatives(xs) => {
                let children = xs
                    .iter()
                    .map(|e| self.lower_span_expr(e))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(SpanExpr::Or(children))
            }
            Expr::AtLeast { threshold, exprs } => {
                let min = resolve_threshold(threshold, exprs.len());
                if super::span_expr::at_least_combinations(min, exprs.len())
                    > MAX_AT_LEAST_COMBINATIONS
                {
                    return Err(LowerError::TooManyCombinations {
                        min,
                        operands: exprs.len(),
                    });
                }
                let children = exprs
                    .iter()
                    .map(|expr| self.lower_span_expr(expr))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(SpanExpr::AtLeast { min, children })
            }
            Expr::Then { left, right, gap } => {
                let l = self.lower_span_expr(left)?;
                let r = self.lower_span_expr(right)?;
                Ok(SpanExpr::MaxGaps {
                    max_gaps: *gap,
                    inner: Box::new(SpanExpr::Ordered(vec![l, r])),
                })
            }
            Expr::Near { left, right, gap } => {
                let l = self.lower_span_expr(left)?;
                let r = self.lower_span_expr(right)?;
                Ok(SpanExpr::MaxGaps {
                    max_gaps: *gap,
                    inner: Box::new(SpanExpr::Unordered(vec![l, r])),
                })
            }
            Expr::Encloses { big, little } => {
                let b = self.lower_span_expr(big)?;
                let l = self.lower_span_expr(little)?;
                Ok(SpanExpr::Containing {
                    big: Box::new(b),
                    little: Box::new(l),
                })
            }
            Expr::NotEncloses { big, little } => {
                let b = self.lower_span_expr(big)?;
                let l = self.lower_span_expr(little)?;
                Ok(SpanExpr::NotContaining {
                    big: Box::new(b),
                    little: Box::new(l),
                })
            }
            Expr::EnclosedBy { little, big } => {
                let l = self.lower_span_expr(little)?;
                let b = self.lower_span_expr(big)?;
                Ok(SpanExpr::ContainedBy {
                    little: Box::new(l),
                    big: Box::new(b),
                })
            }
            Expr::NotEnclosedBy { little, big } => {
                let l = self.lower_span_expr(little)?;
                let b = self.lower_span_expr(big)?;
                Ok(SpanExpr::NotContainedBy {
                    little: Box::new(l),
                    big: Box::new(b),
                })
            }
            Expr::Overlapping { a, b } => {
                let qa = self.lower_span_expr(a)?;
                let qb = self.lower_span_expr(b)?;
                Ok(SpanExpr::Overlapping {
                    a: Box::new(qa),
                    b: Box::new(qb),
                })
            }
            Expr::NotOverlapping { a, b } => {
                let qa = self.lower_span_expr(a)?;
                let qb = self.lower_span_expr(b)?;
                Ok(SpanExpr::NonOverlapping {
                    a: Box::new(qa),
                    b: Box::new(qb),
                })
            }
            Expr::Before { a, b } => {
                let qa = self.lower_span_expr(a)?;
                let qb = self.lower_span_expr(b)?;
                Ok(SpanExpr::Before {
                    a: Box::new(qa),
                    b: Box::new(qb),
                })
            }
            Expr::After { a, b } => {
                let qa = self.lower_span_expr(a)?;
                let qb = self.lower_span_expr(b)?;
                Ok(SpanExpr::After {
                    a: Box::new(qa),
                    b: Box::new(qb),
                })
            }
            Expr::First { bound, inner } => Ok(SpanExpr::PositionFilter {
                inner: Box::new(self.lower_span_expr(inner)?),
                filter: SpanPositionFilter::First(PositionFilterBound::from(bound)),
            }),
            Expr::Last { bound, inner } => Ok(SpanExpr::PositionFilter {
                inner: Box::new(self.lower_span_expr(inner)?),
                filter: SpanPositionFilter::Last(PositionFilterBound::from(bound)),
            }),
            Expr::Middle { percent, inner } => Ok(SpanExpr::PositionFilter {
                inner: Box::new(self.lower_span_expr(inner)?),
                filter: SpanPositionFilter::Middle { percent: *percent },
            }),
            Expr::Between { lo, hi, inner } => Ok(SpanExpr::PositionFilter {
                inner: Box::new(self.lower_span_expr(inner)?),
                filter: SpanPositionFilter::Between { lo: *lo, hi: *hi },
            }),
            Expr::Within { width, inner } => {
                let sq = self.lower_span_expr(inner)?;
                Ok(SpanExpr::MaxWidth {
                    max_width: *width,
                    inner: Box::new(sq),
                })
            }
            // The factor stays out of the tree: it weighs the leaves beneath
            // it (`leaf_boosts`), and matching never sees it.
            Expr::Boost { factor, inner } => {
                let outer = self.boost;
                self.boost = Some(outer.unwrap_or(1.0) * factor.0);
                let lowered = self.lower_span_expr(inner);
                self.boost = outer;
                lowered
            }
            _ => unreachable!("lower_span_node dispatches leaves and chains elsewhere"),
        }
    }

    fn lower_phrase(
        &mut self,
        elements: &[crate::PhraseElement],
        slop: Option<u32>,
    ) -> Result<SpanExpr, LowerError> {
        // Each child carries the gap pinned between it and the previous
        // child. Gaps before the first child or after the last have no
        // anchoring pair and are ignored.
        let mut children: Vec<(u32, SpanExpr)> = Vec::new();
        let mut pending_gap: u32 = 0;

        // Levels the phrase's shape puts between it and its first word: two
        // for a flat budgeted sequence, two per word when every gap is
        // pinned in its own nested pair (see below).
        let words = elements
            .iter()
            .filter(|elem| !matches!(elem, crate::PhraseElement::Gap(_)))
            .count();
        let interior_gap = elements
            .iter()
            .skip_while(|elem| matches!(elem, crate::PhraseElement::Gap(_)))
            .scan(false, |gap_seen, elem| {
                let pinned = *gap_seen && !matches!(elem, crate::PhraseElement::Gap(_));
                *gap_seen |= matches!(elem, crate::PhraseElement::Gap(n) if *n > 0);
                Some(pinned)
            })
            .any(|pinned| pinned);
        let extra = match words {
            0 | 1 => 0,
            _ if interior_gap && slop.is_none() => 2 * (words - 1),
            _ => 2,
        };
        if self.depth + extra + 1 > MAX_SPAN_NESTING {
            return Err(LowerError::NestingTooDeep);
        }

        for elem in elements {
            match elem {
                crate::PhraseElement::Term(s) => {
                    children.push((pending_gap, self.leaf(SpanTermSlot::Term(s.clone()))));
                    pending_gap = 0;
                }
                crate::PhraseElement::Gap(n) => {
                    pending_gap = pending_gap.saturating_add(*n);
                }
                crate::PhraseElement::Alternatives(exprs) => {
                    // One more level for the alternatives' OR.
                    let alts = self.lower_nested(exprs, extra + 1)?;
                    children.push((pending_gap, SpanExpr::Or(alts)));
                    pending_gap = 0;
                }
            }
        }

        if children.is_empty() {
            // Every element tokenized away ("...", emoji-only phrases): the
            // analyzed phrase is empty and matches nothing.
            return Ok(SpanExpr::Empty);
        }

        if children.len() == 1 {
            return Ok(children.into_iter().next().unwrap().1);
        }

        let total_gaps = children
            .iter()
            .skip(1)
            .fold(0u32, |acc, (gap, _)| acc.saturating_add(*gap));

        if total_gaps == 0 {
            // No interior gaps: slop is a total budget over the whole phrase.
            let flat = children.into_iter().map(|(_, child)| child).collect();
            return Ok(SpanExpr::MaxGaps {
                max_gaps: slop.unwrap_or(0),
                inner: Box::new(SpanExpr::Ordered(flat)),
            });
        }

        if let Some(slop) = slop {
            // Gaps combined with slop: the pinned gaps are the floor and the
            // slop adds a total tolerance on top of them.
            let flat = children.into_iter().map(|(_, child)| child).collect();
            return Ok(SpanExpr::GapsInRange {
                min_gaps: total_gaps,
                max_gaps: total_gaps.saturating_add(slop),
                inner: Box::new(SpanExpr::Ordered(flat)),
            });
        }

        // Exact gaps: pin every adjacency. Left-nested pairs keep each gap at
        // its own slot ("a _ b __ c" requires exactly one position between a
        // and b and exactly two between b and c), which one shared budget
        // over the flat sequence cannot express.
        let mut iter = children.into_iter();
        let mut acc = iter.next().expect("length checked above").1;
        for (gap, child) in iter {
            acc = SpanExpr::GapsInRange {
                min_gaps: gap,
                max_gaps: gap,
                inner: Box::new(SpanExpr::Ordered(vec![acc, child])),
            };
        }
        Ok(acc)
    }
}

/// Translates a wildcard pattern to its anchored-regex equivalent: `*` -> `.*`
/// (zero or more characters), `?` -> `.` (exactly one), literals escaped. The
/// sub-tokenize pass normally performs this conversion with tokenizer-folded
/// literals before lowering runs; this handles expressions lowered without
/// sub-tokenization, preserving the written literal text.
fn wildcard_parts_regex(parts: &[crate::WildcardPart]) -> String {
    let mut pattern = String::new();
    for part in parts {
        match part {
            crate::WildcardPart::Literal(s) => pattern.push_str(&regex_syntax::escape(s)),
            crate::WildcardPart::Any => pattern.push_str(".*"),
            crate::WildcardPart::Single => pattern.push('.'),
        }
    }
    pattern
}

fn convert_range_bound(b: &crate::RangeBound) -> RangeBound {
    match b {
        crate::RangeBound::Open => RangeBound::Open,
        crate::RangeBound::Term(s) => RangeBound::Term(s.clone()),
    }
}

fn resolve_threshold(threshold: &crate::AtLeastThreshold, n: usize) -> u32 {
    match threshold {
        crate::AtLeastThreshold::Count(c) => *c,
        crate::AtLeastThreshold::Percent(p) => (*p as u64 * n as u64).div_ceil(100) as u32,
        crate::AtLeastThreshold::All => n as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ImplicitOp;
    use tokenizer::presets::default_pipeline;

    fn parse(input: &str) -> crate::Expr {
        crate::parse(input, ImplicitOp::And).expect("query should parse")
    }

    fn parse_and_lower(input: &str) -> Query {
        let expr = super::super::subtokenize::sub_tokenize(parse(input), default_pipeline())
            .expect("query should sub-tokenize");
        lower(&expr).expect("query should lower")
    }

    /// The slots of a lowered span's included leaves, one per written
    /// occurrence, left to right.
    fn positive_leaves(input: &str) -> Vec<String> {
        let query = parse_and_lower(input);
        let mut leaves = Vec::new();
        let term_slots = match &query {
            Query::Span {
                term_slots,
                span_query,
                ..
            } => {
                span_query.for_each_positive_term(&mut |idx| leaves.push(idx));
                term_slots
            }
            Query::SpanExpr {
                term_slots,
                span_expr,
                ..
            } => {
                span_expr.for_each_positive_term(&mut |idx| leaves.push(idx));
                term_slots
            }
            other => panic!("{input} lowers to {other:?}, not a span"),
        };
        leaves
            .into_iter()
            .map(|idx| match &term_slots[idx] {
                SpanTermSlot::Term(term) => term.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn span_positive_leaves_are_written_occurrences_outside_excluded_sides() {
        let to_be = ["to", "be", "or", "not", "to", "be"];
        assert_eq!(positive_leaves("\"to be or not to be\""), to_be);
        assert_eq!(positive_leaves("\"to be or not to be\"~2"), to_be);
        assert_eq!(positive_leaves("be NEAR/1 be"), ["be", "be"]);
        assert_eq!(positive_leaves("[be or] NEAR/1 be"), ["be", "or", "be"]);
        assert_eq!(
            positive_leaves("(AT LEAST 2 OF [be be or]) NEAR/3 to"),
            ["be", "be", "or", "to"]
        );
        assert_eq!(
            positive_leaves("(to NEAR/3 not) NOT ENCLOSES be"),
            ["to", "not"]
        );
        assert_eq!(
            positive_leaves("or NOT ENCLOSED BY \"to be or not\""),
            ["or"]
        );
        assert_eq!(positive_leaves("be NOT OVERLAPPING \"to be\""), ["be"]);
        assert_eq!(positive_leaves("(be AND NOT or) NEAR/3 to"), ["be", "to"]);
        assert_eq!(
            positive_leaves("(be NEAR/3 be) NOT ENCLOSES (or NEAR/1 be)"),
            ["be", "be"]
        );
    }

    /// A lowered span's included leaves as scoring reads them, left to right:
    /// `term^factor` for a leaf with a written boost.
    fn boosted_leaves(query: &Query) -> Vec<String> {
        let mut leaves = Vec::new();
        query.for_each_positive_span_slot(&mut |_, slot, boost| {
            let SpanTermSlot::Term(term) = slot else {
                panic!("{slot:?} is not a term slot");
            };
            leaves.push(match boost {
                Some(factor) => format!("{term}^{factor}"),
                None => term.clone(),
            });
        });
        leaves
    }

    #[test]
    fn span_leaf_boosts_weigh_the_operand_they_are_written_on() {
        let leaves = |input| boosted_leaves(&parse_and_lower(input));
        assert_eq!(leaves("ipa^3 NEAR/5 hoppy"), ["ipa^3", "hoppy"]);
        assert_eq!(leaves("\"craft [beer^2 ale]\""), ["craft", "beer^2", "ale"]);
        assert_eq!(
            leaves("(stout^2 OR porter) NEAR/3 chocolate"),
            ["stout^2", "porter", "chocolate"]
        );
        assert_eq!(leaves("beer^2 IN FIRST 100 WORDS"), ["beer^2"]);
        assert_eq!(leaves("beer^0 NEAR/3 tasting"), ["beer^0", "tasting"]);
        // Factors multiply down to the leaf; a repeated word weighs per
        // occurrence, each with its own boost.
        assert_eq!(
            leaves("(ipa^2 NEAR/1 pale)^3 THEN/4 ale"),
            ["ipa^6", "pale^3", "ale"]
        );
        assert_eq!(leaves("beer^2 NEAR/3 beer"), ["beer^2", "beer"]);
        // An excluded side only filters: its boosts weigh nothing.
        assert_eq!(
            leaves("(ipa^2 NEAR/3 hoppy) NOT ENCLOSES stout^5"),
            ["ipa^2", "hoppy"]
        );
        // The dynamic form (AT LEAST has no fast path) reads them the same way.
        let at_least = parse_and_lower("(AT LEAST 2 OF [be^2 be or]) NEAR/3 to");
        assert!(matches!(at_least, Query::SpanExpr { .. }));
        assert_eq!(boosted_leaves(&at_least), ["be^2", "be", "or", "to"]);
    }

    #[test]
    fn unboosted_span_leaves_lower_without_boosts() {
        let unboosted = |input| match parse_and_lower(input) {
            Query::Span { leaf_boosts, .. } | Query::SpanExpr { leaf_boosts, .. } => {
                leaf_boosts == SpanLeafBoosts::default()
            }
            Query::Boost { inner, .. } => matches!(
                *inner,
                Query::Span { leaf_boosts, .. } if leaf_boosts == SpanLeafBoosts::default()
            ),
            other => panic!("{input} lowers to {other:?}"),
        };
        assert!(unboosted("ipa NEAR/5 hoppy"));
        assert!(unboosted("(ipa NEAR/5 hoppy)^3"));
        assert!(unboosted("\"craft beer\"^2"));
        assert!(unboosted("ipa NOT ENCLOSES stout^5"));
    }

    #[test]
    fn top_level_runtime_position_filter_stays_on_fast_path() {
        let query = parse_and_lower("beer IN LAST 25%");
        assert!(matches!(query, Query::Span { .. }));
    }

    fn match_nothing() -> Query {
        Query::Not(Box::new(Query::MatchAll))
    }

    // NOT of a zero-token term excludes nothing, so the positive side of the
    // conjunction survives untouched.
    #[test]
    fn zero_token_negation_keeps_the_positive_side() {
        assert_eq!(
            parse_and_lower("beer AND NOT ,"),
            Query::Term("beer".into()),
        );
        assert_eq!(
            parse_and_lower("beer AND NOT ..."),
            Query::Term("beer".into()),
        );
        // As a positive conjunct the empty term matches nothing, and so does
        // the conjunction.
        assert_eq!(parse_and_lower("beer AND ,"), match_nothing());
    }

    // Comma-separated alternatives are the documented OR; the separator's
    // empty residue is dropped rather than poisoning the disjunction.
    #[test]
    fn comma_alternatives_lower_to_the_documented_or() {
        let expected = Query::Disjunction {
            min: 1,
            children: vec![Query::Term("beer".into()), Query::Term("wine".into())],
        };
        assert_eq!(parse_and_lower(r#"["beer", "wine"]"#), expected);
        assert_eq!(parse_and_lower("[beer,wine]"), expected);
    }

    // Empty alternatives never reduce an AT LEAST threshold.
    #[test]
    fn at_least_threshold_survives_comma_residue() {
        assert_eq!(
            parse_and_lower("AT LEAST 2 OF [beer , wine , stout]"),
            Query::Disjunction {
                min: 2,
                children: vec![
                    Query::Term("beer".into()),
                    Query::Term("wine".into()),
                    Query::Term("stout".into()),
                ],
            },
        );
        assert_eq!(
            parse_and_lower("AT LEAST 2 OF [beer~1, wine, water]"),
            Query::Disjunction {
                min: 2,
                children: vec![
                    Query::Fuzzy {
                        term: "beer".into(),
                        prefix: 1,
                        distance: 1,
                    },
                    Query::Term("wine".into()),
                    Query::Term("water".into()),
                ],
            },
        );
    }

    // An input that parses or analyzes to the empty query lowers to
    // match-nothing rather than erroring or matching the corpus.
    #[test]
    fn empty_and_zero_token_queries_lower_to_match_nothing() {
        assert_eq!(parse_and_lower(""), match_nothing());
        assert_eq!(parse_and_lower("..."), match_nothing());
        assert_eq!(parse_and_lower(".~1"), match_nothing());
        assert_eq!(parse_and_lower("@*"), match_nothing());
        // A phrase whose every element tokenizes away is an empty span —
        // the span-context spelling of match-nothing, not an error.
        assert!(matches!(
            parse_and_lower("\"...\""),
            Query::Span {
                span_query: boldi_vigna::SpanQuery::Empty,
                ..
            }
        ));
    }

    // Phrase gaps are exact: each adjacency is pinned at its own slot, which
    // one shared max-gaps budget cannot express.
    #[test]
    fn phrase_gaps_lower_to_exact_pinned_pairs() {
        let Query::Span { span_query, .. } = parse_and_lower("\"alpha _ gamma\"") else {
            panic!("expected fast-path span lowering");
        };
        assert_eq!(
            span_query,
            boldi_vigna::SpanQuery::GapsInRange {
                min_gaps: 1,
                max_gaps: 1,
                inner: Box::new(boldi_vigna::SpanQuery::Ordered(vec![
                    boldi_vigna::SpanQuery::Term(0),
                    boldi_vigna::SpanQuery::Term(1),
                ])),
            },
        );

        // Multi-gap phrases pin each slot pairwise.
        let Query::Span { span_query, .. } = parse_and_lower("\"a _ b __ c\"") else {
            panic!("expected fast-path span lowering");
        };
        assert_eq!(
            span_query,
            boldi_vigna::SpanQuery::GapsInRange {
                min_gaps: 2,
                max_gaps: 2,
                inner: Box::new(boldi_vigna::SpanQuery::Ordered(vec![
                    boldi_vigna::SpanQuery::GapsInRange {
                        min_gaps: 1,
                        max_gaps: 1,
                        inner: Box::new(boldi_vigna::SpanQuery::Ordered(vec![
                            boldi_vigna::SpanQuery::Term(0),
                            boldi_vigna::SpanQuery::Term(1),
                        ])),
                    },
                    boldi_vigna::SpanQuery::Term(2),
                ])),
            },
        );

        // Slop keeps its budget meaning: without gaps it is a plain budget,
        // with gaps the pinned total becomes the floor.
        let Query::Span { span_query, .. } = parse_and_lower("\"alpha gamma\"~1") else {
            panic!("expected fast-path span lowering");
        };
        assert!(matches!(
            span_query,
            boldi_vigna::SpanQuery::MaxGaps { max_gaps: 1, .. }
        ));
        let Query::Span { span_query, .. } = parse_and_lower("\"alpha _ gamma\"~1") else {
            panic!("expected fast-path span lowering");
        };
        assert!(matches!(
            span_query,
            boldi_vigna::SpanQuery::GapsInRange {
                min_gaps: 1,
                max_gaps: 2,
                ..
            }
        ));
    }

    #[test]
    fn lowering_rejects_invalid_regex() {
        let err = lower(&crate::Expr::Regex("(".into())).expect_err("regex should fail");
        assert!(matches!(err, LowerError::InvalidRegex(_)));
    }

    #[test]
    fn lowered_regex_carries_compiled_full_term_matcher() {
        let query = parse_and_lower("MATCHES alp[a-z]*");
        let Query::Regex(regex) = query else {
            panic!("expected regex query");
        };

        assert_eq!(regex.source(), "alp[a-z]*");
        assert!(regex.is_match("alpha"));
        assert!(!regex.is_match("xalpha"));
    }

    #[test]
    fn nested_runtime_position_filter_uses_span_expr_fallback() {
        let query = parse_and_lower("(beer IN LAST 25%) BEFORE wine");
        let Query::SpanExpr {
            term_slots,
            span_expr,
            ..
        } = query
        else {
            panic!("expected advanced span expression lowering");
        };

        assert_eq!(
            term_slots,
            vec![
                SpanTermSlot::Term("beer".into()),
                SpanTermSlot::Term("wine".into()),
            ]
        );
        assert!(matches!(
            span_expr,
            SpanExpr::Before {
                a,
                b,
            } if matches!(
                a.as_ref(),
                SpanExpr::PositionFilter {
                    filter: SpanPositionFilter::Last(PositionFilterBound::Percent(25)),
                    ..
                }
            ) && matches!(b.as_ref(), SpanExpr::Term(1))
        ));
    }

    #[test]
    fn at_least_inside_span_context_uses_span_expr_fallback() {
        let query = parse_and_lower("(AT LEAST 2 OF [quick, brown, fox]) WITHIN 3");
        let Query::SpanExpr { span_expr, .. } = query else {
            panic!("expected advanced span expression lowering");
        };

        assert!(matches!(
            span_expr,
            SpanExpr::MaxWidth { max_width: 3, inner }
                if matches!(
                    inner.as_ref(),
                    SpanExpr::AtLeast { min: 2, children } if children.len() == 3
                )
        ));
    }

    /// `AT LEAST n OF [k operands]` inside a proximity operator is matched
    /// as the disjunction of every n-operand combination, built again for
    /// each candidate document. A query whose combinations cannot be built
    /// in bounded time and memory is refused when it is lowered, before any
    /// document is read; below the bound it lowers as before.
    #[test]
    fn at_least_expansions_inside_spans_are_bounded() {
        let words = |prefix: &str, count: usize| {
            (0..count)
                .map(|i| format!("{prefix}{i}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let lowered = |input: &str| {
            let expr = super::super::subtokenize::sub_tokenize(parse(input), default_pipeline())
                .expect("query should sub-tokenize");
            lower(&expr)
        };
        let refused = [
            // C(30, 15), about 155 million combinations.
            format!("(AT LEAST 15 OF [{}]) NEAR/5 x", words("t", 30)),
            // C(1000, 999) is only 1,000 combinations, but of 999 operands
            // each: about a million copied operands.
            format!("(AT LEAST 999 OF [{}]) NEAR/5 x", words("t", 1000)),
            // Two operands of four at every level, twelve levels deep: six
            // combinations each, but each level copies the one below it
            // three times, 3^12 copies of the innermost.
            {
                let mut query = "a".to_owned();
                for level in 0..12 {
                    query = format!("AT LEAST 2 OF [({query}) b{level} c{level} d{level}]");
                }
                format!("({query}) NEAR/5 x")
            },
        ];
        for query in &refused {
            let error = lowered(query)
                .expect_err("query should be refused")
                .to_string();
            assert!(
                error.contains("AT LEAST") && error.contains("more than"),
                "{}: {error}",
                &query[..query.len().min(60)]
            );
        }

        for query in [
            format!("(AT LEAST 2 OF [{}]) NEAR/5 x", words("t", 100)),
            format!("(AT LEAST 6 OF [{}]) NEAR/5 x", words("t", 16)),
            format!("(AT LEAST 1 OF [{}]) NEAR/5 x", words("t", 5000)),
            format!("(AT LEAST 5000 OF [{}]) NEAR/5 x", words("t", 5000)),
        ] {
            assert!(
                matches!(lowered(&query), Ok(Query::SpanExpr { .. })),
                "{}",
                &query[..60]
            );
        }
    }

    /// Each regex is bounded on its own, and a query's regexes and
    /// wildcards together: 10,000 of them, each within the bound, would
    /// still take gigabytes.
    #[test]
    fn the_regexes_of_a_query_are_bounded_together() {
        let lowered = |input: &str| {
            let expr = super::super::subtokenize::sub_tokenize(parse(input), default_pipeline())
                .expect("query should sub-tokenize");
            lower(&expr)
        };
        // About 0.56 MB each.
        let regexes = |count: usize| {
            (0..count)
                .map(|i| format!("MATCHES \\w{{10}}{i}"))
                .collect::<Vec<_>>()
                .join(" OR ")
        };
        for query in [regexes(600), format!("({}) NEAR/5 x", regexes(600))] {
            let Err(error) = lowered(&query) else {
                panic!("{}...: should be refused", &query[..60]);
            };
            let error = error.to_string();
            assert!(error.contains("regexes compile to more than"), "{error}");
        }
        assert!(lowered(&regexes(100)).is_ok());
        // 10,000 wildcards are a few kilobytes each.
        let wildcards = (0..10_000)
            .map(|i| format!("w{i}*"))
            .collect::<Vec<_>>()
            .join(" OR ");
        assert!(lowered(&wildcards).is_ok());
    }
}
