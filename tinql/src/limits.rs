// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Size and nesting limits on a query.
//!
//! The query front end runs inside a PostgreSQL backend, whose stack is
//! 8 MiB and which has no stack-overflow handler for Rust code: a query deep
//! enough to overflow it aborts the backend and restarts every session. Every
//! recursive pass over a query (the parser, sub-tokenization, lowering,
//! simplification, estimation, display, drop, the extension's own walks) is
//! bounded by these limits instead, and a query past them is rejected with an
//! error naming the limit.
//!
//! PlanetScale TIN 1.0.3 (PostgreSQL 18.6, measured 2026-09-26), whose parser
//! Stannum's derives from, answers 3,000 plain words, a 3,000-term OR chain
//! and 1,000 levels of nested parentheses, and crashes its server at 10,000
//! words, 10,000 OR terms and 5,000 levels. The limits accept everything TIN
//! answers there and turn what crashed it into an error.

/// Levels of nesting a query may have: brackets (parentheses, alternatives,
/// `AT LEAST`/`ALL OF` lists, alternatives inside phrases) opened inside one
/// another, and operators applied to the result of another operator. A flat
/// chain `a b c` or `a OR b OR c` is one level however long it is; `AND NOT`,
/// proximity and relation chains nest one level per operator.
///
/// Measured at this depth (aarch64): parsing 1,000 nested parentheses takes
/// about 0.95 MiB of stack in a release build and 1.25 MiB unoptimized (pest
/// took about 4.5 MiB release, and overflowed 8 MiB unoptimized past about
/// 500 levels); parsing, sub-tokenizing, lowering, simplifying, displaying
/// and estimating the deepest shapes (1,000 nested alternatives, 1,000
/// nested OR groups, a 1,000-operand `AND NOT` chain) take at most 1.3 MiB
/// release and 6.1 MiB unoptimized, and a 1,000-operand `THEN` chain, whose
/// span nests twice as deep, 2.1 MiB release.
pub const MAX_NESTING: usize = 1_000;

/// Search terms a query may name: words, wildcards, regexes, ranges, fuzzy
/// terms and `*`, counting each word of a phrase, before and after
/// sub-tokenization splits written terms into analyzed tokens.
pub const MAX_TERMS: usize = 10_000;

/// Levels of nesting a query's lowered span expression may have. Within a
/// proximity operator an AND or OR chain keeps its left-deep binary shape,
/// whose gaps and repeated-term semantics differ from a flat operand list,
/// and a phrase with pinned gaps nests one level per word; this bounds both.
pub const MAX_SPAN_NESTING: usize = 2 * MAX_NESTING;
