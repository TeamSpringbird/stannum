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

/// Combinations one `AT LEAST n OF [k operands]` inside a proximity
/// operator, relation or positional filter may expand to. There it is
/// matched as the disjunction of every `n`-operand combination, C(k, n) of
/// them, built again for each candidate document: C(30, 15) is 155 million.
/// Outside a span context `AT LEAST` is counted, not expanded, and has no
/// such limit.
pub const MAX_AT_LEAST_COMBINATIONS: usize = 10_000;

/// Operands the expansion of `AT LEAST` inside a span context may add to
/// the query's span expression: each operand is copied into every
/// combination that includes it, and an expanded `AT LEAST` among the
/// operands of another is copied whole, so the copies multiply. A single
/// expansion of terms within [`MAX_AT_LEAST_COMBINATIONS`] that takes at
/// most nine at a time (or has at most 16 operands) stays within it;
/// `AT LEAST 999 OF` a thousand terms, a thousand combinations of 999
/// operands, does not.
pub const MAX_SPAN_EXPANSION: usize = 100_000;

/// Bytes the automaton of one regex, or of one wildcard, may take once
/// compiled: each of its forward and reverse NFAs (the regex engine's
/// default is 10 MiB). `\w` is every Unicode word character, so the pattern
/// decides the size: `\w{20}\w*`, a term of at least 20 word characters,
/// fits; `\w{100}`, about 5.6 MiB in all, does not.
pub const MAX_REGEX_BYTES: usize = 2 << 20;

/// Bytes the compiled regexes and wildcards of one query may take together,
/// counted as they are compiled. A query may name [`MAX_TERMS`] of them:
/// 10,000 prefix wildcards take about 70 MiB, a hundred `\w{10}` about
/// 56 MiB.
pub const MAX_QUERY_REGEX_BYTES: usize = 256 << 20;

static STACK_CHECK: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Installs `check`, which every recursive pass over a query calls once per
/// level: the parser per bracket, sub-tokenization, lowering,
/// simplification, estimation, display and evaluation per node. The limits
/// above keep those passes well inside a backend's stack; `check` is the
/// backstop for a stack that is smaller or already deeper than expected.
/// It must not return when the stack is nearly exhausted: the extension
/// installs PostgreSQL's `check_stack_depth`, whose ERROR reaches Rust as a
/// panic and unwinds out of the pass. The first installation wins.
pub fn set_stack_check(check: fn()) {
    let _ = STACK_CHECK.set(check);
}

/// Calls the installed stack check, if any: for passes over a query outside
/// this crate, such as the ranked walk's reading of its shape.
#[inline]
pub fn check_stack() {
    if let Some(check) = STACK_CHECK.get() {
        check();
    }
}
