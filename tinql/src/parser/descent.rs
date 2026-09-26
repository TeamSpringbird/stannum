// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

//! Recursive-descent parser for the query grammar.
//!
//! This is a direct transliteration of the PEG in
//! `pest_parser/grammar.pest` (kept, with the pest parser generated from it,
//! as a differential-testing oracle): the same ordered choices, the same
//! backtracking, the same whitespace rules, the same builder checks in the
//! same order. What it adds is control over recursion. pest recursed through
//! about ten rules and roughly 4.5 KiB of stack (release) per level of
//! nested brackets and had no depth limit, so 5,000 nested parentheses
//! overflowed a PostgreSQL backend's 8 MiB stack and aborted it. Here:
//!
//! - every bracket level and every operator applied to another operator's
//!   result counts against [`MAX_NESTING`], every term against
//!   [`MAX_TERMS`], and passing either ends the parse with an error;
//! - a rule returns only whether it matched and pushes what it built onto a
//!   heap stack, and building happens in out-of-line functions, so the frames
//!   on the recursive path stay small (about 1 KiB per bracket level
//!   in a release build);
//! - a `MATCHES` pattern is scanned in linear time, where pest backtracked
//!   exponentially over unclosed groups and quadratically over unclosed
//!   classes.
//!
//! Syntax errors take precedence over the builder's findings (a number out of
//! range, an empty phrase, ...): the latter are deferred until the whole
//! input has parsed, the first one found winning, and forgotten when the
//! construct that found them is backtracked over, as pest built only the
//! final parse tree.

use pest::Parser as _;

use crate::ImplicitOp;
use crate::ast::*;
use crate::error::ParseError;
use crate::limits::{MAX_NESTING, MAX_TERMS};
use crate::util::{classify_term, unescape_phrase_term};

use super::phrase_grammar::{PhraseContentParser, PhraseRule};

/// Parses a whole query.
pub(crate) fn parse(input: &str, implicit_op: ImplicitOp) -> Result<Expr, ParseError> {
    Parser::new(input, implicit_op, 0, 0, 0).query()
}

/// An expression and its height: 0 for a leaf, one more than its tallest
/// operand for any other node.
struct Built {
    expr: Expr,
    depth: usize,
}

/// A limit was passed; the error is in [`Parser::stopped`].
struct Stop;

/// Whether a rule matched (it pushed what it built), or a stop.
type R<T = bool> = Result<T, Stop>;

/// A point to backtrack to.
#[derive(Clone, Copy)]
struct Mark {
    pos: usize,
    had_deferred: bool,
    terms: usize,
    built: usize,
}

#[derive(Clone, Copy)]
enum RelOp {
    Encloses,
    NotEncloses,
    EnclosedBy,
    NotEnclosedBy,
    Overlapping,
    NotOverlapping,
    Before,
    After,
}

#[derive(Clone, Copy)]
enum PosFilter {
    /// `IN FIRST n WORDS` / `IN FIRST n%`: digits and whether a percentage.
    First((usize, usize), bool),
    Last((usize, usize), bool),
    Middle((usize, usize)),
    Between((usize, usize), (usize, usize)),
}

/// Top-level keywords: none of these can be a bare search term.
const KEYWORDS: &[&str] = &[
    "AND",
    "OR",
    "NOT",
    "TO",
    "IN",
    "WITHIN",
    "THEN",
    "NEAR",
    "ALL",
    "AT",
    "MATCHES",
    "CONTAINS",
    "ENCLOSES",
    "ENCLOSED",
    "OVERLAPPING",
    "BEFORE",
    "AFTER",
];

const NO_CLASS_CLOSE: usize = usize::MAX;

fn is_ws(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

/// A byte of a word: anything but whitespace and `()[]"~^`. Bytes of
/// multi-byte characters are all word bytes.
fn is_word_byte(byte: u8) -> bool {
    !is_ws(byte) && !matches!(byte, b'(' | b')' | b'[' | b']' | b'"' | b'~' | b'^')
}

struct Parser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    implicit_op: ImplicitOp,
    /// Where `src` starts in the text the user wrote: phrase alternatives are
    /// re-parsed from a copy wrapped in brackets.
    base: usize,
    /// What the rules matched so far built, innermost last.
    built: Vec<Built>,
    /// Brackets currently open, including those of enclosing parsers.
    nesting: usize,
    /// Terms built so far, including those of enclosing parsers.
    terms: usize,
    /// The furthest position at which something was expected and missing.
    furthest: usize,
    /// The first builder finding, reported if the whole input parses.
    deferred: Option<ParseError>,
    /// The limit that stopped the parse.
    stopped: Option<ParseError>,
    /// For each position, where a `MATCHES` character class whose body
    /// starts there closes ([`NO_CLASS_CLOSE`] if it never does). Built on
    /// first use.
    class_close: Option<Vec<usize>>,
}

impl<'a> Parser<'a> {
    fn new(
        src: &'a str,
        implicit_op: ImplicitOp,
        base: usize,
        nesting: usize,
        terms: usize,
    ) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            implicit_op,
            base,
            built: Vec::new(),
            nesting,
            terms,
            furthest: 0,
            deferred: None,
            stopped: None,
            class_close: None,
        }
    }

    // ── Bookkeeping ─────────────────────────────────────────────────

    fn mark(&self) -> Mark {
        Mark {
            pos: self.pos,
            had_deferred: self.deferred.is_some(),
            terms: self.terms,
            built: self.built.len(),
        }
    }

    fn reset(&mut self, mark: Mark) {
        self.pos = mark.pos;
        if !mark.had_deferred {
            self.deferred = None;
        }
        self.terms = mark.terms;
        self.built.truncate(mark.built);
    }

    /// Records a builder finding; the first one wins.
    fn defer(&mut self, error: ParseError) {
        if self.deferred.is_none() {
            self.deferred = Some(error);
        }
    }

    fn stop(&mut self, error: ParseError) -> Stop {
        self.stopped = Some(error);
        Stop
    }

    /// Notes that something expected at `pos` was not there.
    fn missing(&mut self, pos: usize) {
        self.furthest = self.furthest.max(pos);
    }

    fn syntax_error(&self) -> ParseError {
        let found = match self.src.get(self.furthest..).and_then(|s| s.chars().next()) {
            Some(c) => format!("'{c}'"),
            None => "end of input".into(),
        };
        ParseError::Expected {
            expected: "a term, operator or bracket".into(),
            pos: self.base + self.furthest,
            found,
        }
    }

    fn push(&mut self, expr: Expr, depth: usize) -> R<()> {
        if depth > MAX_NESTING {
            let error = ParseError::NestingTooDeep {
                pos: self.base + self.pos,
            };
            return Err(self.stop(error));
        }
        self.built.push(Built { expr, depth });
        Ok(())
    }

    fn pop(&mut self) -> Built {
        self.built.pop().expect("a rule built a node")
    }

    /// Opens the bracket at the current position.
    fn enter(&mut self) -> R<()> {
        crate::limits::check_stack();
        self.nesting += 1;
        if self.nesting > MAX_NESTING {
            let error = ParseError::NestingTooDeep {
                pos: self.base + self.pos,
            };
            return Err(self.stop(error));
        }
        self.pos += 1;
        Ok(())
    }

    /// Counts `count` terms found at `at`.
    fn count_terms(&mut self, count: usize, at: usize) -> R<()> {
        self.terms += count;
        if self.terms > MAX_TERMS {
            let error = ParseError::TooManyTerms {
                pos: self.base + at,
            };
            return Err(self.stop(error));
        }
        Ok(())
    }

    #[inline(never)]
    fn leaf(&mut self, expr: Expr, at: usize) -> R {
        self.count_terms(1, at)?;
        self.push(expr, 0)?;
        Ok(true)
    }

    /// Replaces the top node with `node` of it.
    #[inline(never)]
    fn wrap(&mut self, node: impl FnOnce(Box<Expr>) -> Expr) -> R<()> {
        let inner = self.pop();
        self.push(node(Box::new(inner.expr)), inner.depth + 1)
    }

    /// Replaces the top two nodes with `node` of them.
    #[inline(never)]
    fn binary(&mut self, node: impl FnOnce(Box<Expr>, Box<Expr>) -> Expr) -> R<()> {
        let right = self.pop();
        let left = self.pop();
        let depth = left.depth.max(right.depth) + 1;
        self.push(node(Box::new(left.expr), Box::new(right.expr)), depth)
    }

    /// Replaces the top `count` nodes with their left-associative AND (or
    /// OR) chain, flat.
    #[inline(never)]
    fn chain(&mut self, count: usize, or: bool) -> R<()> {
        if count < 2 {
            return Ok(());
        }
        let operands = self.built.split_off(self.built.len() - count);
        let mut operands = operands.into_iter();
        let first = operands.next().expect("a chain has operands");
        let (mut exprs, mut depth) = match first.expr {
            Expr::Or(exprs) if or => (exprs, first.depth),
            Expr::And(exprs) if !or => (exprs, first.depth),
            expr => (vec![expr], first.depth + 1),
        };
        for operand in operands {
            depth = depth.max(operand.depth + 1);
            exprs.push(operand.expr);
        }
        let expr = if or {
            Expr::Or(exprs)
        } else {
            Expr::And(exprs)
        };
        self.push(expr, depth)
    }

    /// A number the grammar accepted as digits, or 0 with the finding
    /// deferred when it does not fit in `u32`.
    fn number(&mut self, (start, end): (usize, usize)) -> u32 {
        let text = &self.src[start..end];
        text.parse().unwrap_or_else(|_| {
            self.defer(ParseError::NumberOutOfRange {
                text: text.to_string(),
                pos: self.base + start,
            });
            0
        })
    }

    // ── Terminals ───────────────────────────────────────────────────

    fn byte(&self, at: usize) -> Option<u8> {
        self.bytes.get(at).copied()
    }

    /// The end of the whitespace run starting at `at`.
    fn ws_end(&self, mut at: usize) -> usize {
        while self.byte(at).is_some_and(is_ws) {
            at += 1;
        }
        at
    }

    fn skip_ws(&mut self) {
        self.pos = self.ws_end(self.pos);
    }

    /// The end of the run of one or more ASCII digits at `at`.
    fn digits_end(&self, at: usize) -> Option<usize> {
        let mut end = at;
        while self.byte(end).is_some_and(|b| b.is_ascii_digit()) {
            end += 1;
        }
        (end > at).then_some(end)
    }

    /// The end of keyword `word` at `at`: the word, not followed by a
    /// character that could continue it.
    fn keyword_end(&self, at: usize, word: &str) -> Option<usize> {
        let end = at + word.len();
        let continues = self
            .byte(end)
            .is_some_and(|b| !is_ws(b) && !b"()[]\"/\\~^".contains(&b));
        (self.bytes[at..].starts_with(word.as_bytes()) && !continues).then_some(end)
    }

    fn eat_keyword(&mut self, word: &str) -> bool {
        match self.keyword_end(self.pos, word) {
            Some(end) => {
                self.pos = end;
                true
            }
            None => {
                self.missing(self.pos);
                false
            }
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.byte(self.pos) == Some(byte) {
            self.pos += 1;
            true
        } else {
            self.missing(self.pos);
            false
        }
    }

    fn is_keyword_at(&self, at: usize) -> bool {
        KEYWORDS
            .iter()
            .any(|word| self.keyword_end(at, word).is_some())
    }

    /// The end of the word at `at`.
    fn word_end(&self, at: usize) -> Option<usize> {
        let mut end = at;
        while self.byte(end).is_some_and(is_word_byte) {
            end += 1;
        }
        (end > at).then_some(end)
    }

    /// Whether an operand can start at `at` (for the implicit operator).
    fn atom_starts(&self, at: usize) -> bool {
        self.byte(at)
            .is_some_and(|b| !is_ws(b) && !matches!(b, b')' | b']' | b':' | b'~' | b'^'))
    }

    // ── Top level ───────────────────────────────────────────────────

    fn query(mut self) -> Result<Expr, ParseError> {
        self.skip_ws();
        let matched = match self.or_expr() {
            Ok(matched) => matched,
            Err(Stop) => return Err(self.stopped.take().expect("a stop records its limit")),
        };
        self.skip_ws();
        if !matched || self.pos != self.bytes.len() {
            self.missing(self.pos);
            return Err(self.syntax_error());
        }
        match self.deferred.take() {
            Some(error) => Err(error),
            None => Ok(self.pop().expr),
        }
    }

    // ── Boolean tier ────────────────────────────────────────────────
    //
    // These rules and the ones they call down to `base` are the recursive
    // path: keep their locals few and small, and build out of line.

    fn or_expr(&mut self) -> R {
        let mut count = self.and_expr()?;
        if count == 0 {
            return Ok(false);
        }
        loop {
            let mark = self.mark();
            if self.separator("OR") {
                let more = self.and_expr()?;
                if more > 0 {
                    count += more;
                    continue;
                }
            }
            self.reset(mark);
            break;
        }
        self.chain(count, true)?;
        Ok(true)
    }

    /// Skips whitespace, `keyword`, whitespace.
    fn separator(&mut self, keyword: &str) -> bool {
        self.skip_ws();
        if !self.eat_keyword(keyword) {
            return false;
        }
        self.skip_ws();
        true
    }

    /// An `and_expr`: pushes the operands it contributes to the enclosing OR
    /// chain (one AND chain, or with [`ImplicitOp::Or`] one per run of
    /// explicitly AND-ed operands) and returns how many (0: no match).
    fn and_expr(&mut self) -> R<usize> {
        if !self.andnot_expr()? {
            return Ok(0);
        }
        let mut count = 1;
        // Whether each operand after the first follows an explicit AND.
        let mut explicit = Vec::new();
        loop {
            let mark = self.mark();
            self.skip_ws();
            let Some(sep) = self.and_sep() else {
                self.reset(mark);
                break;
            };
            self.skip_ws();
            if !self.andnot_expr()? {
                self.reset(mark);
                break;
            }
            count += 1;
            explicit.push(sep);
        }
        self.group_and(count, explicit)
    }

    /// Replaces the top `count` operands of an `and_expr` with what it
    /// contributes to the OR chain; returns how many.
    #[inline(never)]
    fn group_and(&mut self, count: usize, explicit: Vec<bool>) -> R<usize> {
        if count == 1 {
            return Ok(1);
        }
        match self.implicit_op {
            ImplicitOp::And => {
                self.chain(count, false)?;
                Ok(1)
            }
            ImplicitOp::Or => {
                // Juxtaposed operands are OR-ed; runs joined by an explicit
                // AND become one AND chain each:
                // `a b AND c d` -> [a, (b AND c), d].
                let operands = self.built.split_off(self.built.len() - count);
                let mut groups = 0;
                let mut run = 0;
                let starts = std::iter::once(false).chain(explicit);
                for (operand, joined) in operands.into_iter().zip(starts) {
                    if !joined && run > 0 {
                        self.chain(run, false)?;
                        groups += 1;
                        run = 0;
                    }
                    self.built.push(operand);
                    run += 1;
                }
                self.chain(run, false)?;
                Ok(groups + 1)
            }
        }
    }

    /// `AND` not followed by `NOT` (explicit, `true`), or juxtaposition
    /// before an operand (implicit, `false`).
    fn and_sep(&mut self) -> Option<bool> {
        if let Some(end) = self.keyword_end(self.pos, "AND") {
            let after = self.ws_end(end);
            if self.keyword_end(after, "NOT").is_none() {
                self.pos = after;
                return Some(true);
            }
        }
        if self.keyword_end(self.pos, "OR").is_none()
            && self.keyword_end(self.pos, "AND").is_none()
            && self.atom_starts(self.pos)
        {
            return Some(false);
        }
        self.missing(self.pos);
        None
    }

    fn andnot_expr(&mut self) -> R {
        if !self.pos_expr()? {
            return Ok(false);
        }
        loop {
            let mark = self.mark();
            if self.andnot_sep() && self.pos_expr()? {
                self.binary(|positive, negative| Expr::AndNot { positive, negative })?;
                continue;
            }
            self.reset(mark);
            return Ok(true);
        }
    }

    /// Whitespace, `AND NOT` (unless it starts `NOT ENCLOSES`, `NOT ENCLOSED
    /// BY` or `NOT OVERLAPPING`), whitespace.
    fn andnot_sep(&mut self) -> bool {
        self.skip_ws();
        let Some(end) = self.keyword_end(self.pos, "AND") else {
            self.missing(self.pos);
            return false;
        };
        let at = self.ws_end(end);
        let Some(end) = self.keyword_end(at, "NOT") else {
            self.missing(at);
            return false;
        };
        let at = self.ws_end(end);
        if ["ENCLOSES", "ENCLOSED", "OVERLAPPING"]
            .iter()
            .any(|word| self.keyword_end(at, word).is_some())
        {
            return false;
        }
        self.pos = at;
        true
    }

    /// An alternatives element: no implicit operator.
    fn alt_or_expr(&mut self) -> R {
        if !self.alt_and_expr()? {
            return Ok(false);
        }
        let mut count = 1;
        loop {
            let mark = self.mark();
            if self.separator("OR") && self.alt_and_expr()? {
                count += 1;
                continue;
            }
            self.reset(mark);
            break;
        }
        self.chain(count, true)?;
        Ok(true)
    }

    fn alt_and_expr(&mut self) -> R {
        if !self.andnot_expr()? {
            return Ok(false);
        }
        let mut count = 1;
        loop {
            let mark = self.mark();
            if self.alt_and_sep() && self.andnot_expr()? {
                count += 1;
                continue;
            }
            self.reset(mark);
            break;
        }
        self.chain(count, false)?;
        Ok(true)
    }

    /// Whitespace, `AND` not followed by `NOT`, whitespace.
    fn alt_and_sep(&mut self) -> bool {
        self.skip_ws();
        let Some(end) = self.keyword_end(self.pos, "AND") else {
            return false;
        };
        let after = self.ws_end(end);
        if self.keyword_end(after, "NOT").is_some() {
            return false;
        }
        self.pos = after;
        true
    }

    // ── Positional filter tier ──────────────────────────────────────

    fn pos_expr(&mut self) -> R {
        if !self.rel_expr()? {
            return Ok(false);
        }
        self.pos_suffix()?;
        Ok(true)
    }

    /// An optional positional filter on the top node.
    #[inline(never)]
    fn pos_suffix(&mut self) -> R<()> {
        let mark = self.mark();
        self.skip_ws();
        let Some(filter) = self.pos_filter() else {
            self.reset(mark);
            return Ok(());
        };
        let bound = |parser: &mut Self, digits, percent| {
            let n = parser.number(digits);
            if percent {
                PositionBound::Percent(n)
            } else {
                PositionBound::Absolute(n)
            }
        };
        match filter {
            PosFilter::First(digits, percent) => {
                let bound = bound(self, digits, percent);
                self.wrap(|inner| Expr::First { bound, inner })
            }
            PosFilter::Last(digits, percent) => {
                let bound = bound(self, digits, percent);
                self.wrap(|inner| Expr::Last { bound, inner })
            }
            PosFilter::Middle(digits) => {
                let percent = self.number(digits);
                self.wrap(|inner| Expr::Middle { percent, inner })
            }
            PosFilter::Between(lo, hi) => {
                let lo = self.number(lo);
                let hi = self.number(hi);
                self.wrap(|inner| Expr::Between { lo, hi, inner })
            }
        }
    }

    fn pos_filter(&mut self) -> Option<PosFilter> {
        let Some(end) = self.keyword_end(self.pos, "IN") else {
            self.missing(self.pos);
            return None;
        };
        let at = self.ws_end(end);
        if let Some((end, digits, percent)) = self.first_or_last(at, "FIRST") {
            self.pos = end;
            return Some(PosFilter::First(digits, percent));
        }
        if let Some((end, digits, percent)) = self.first_or_last(at, "LAST") {
            self.pos = end;
            return Some(PosFilter::Last(digits, percent));
        }
        // `MIDDLE`, whitespace, digits, `%`, with no whitespace inside
        // after the keyword's.
        if let Some(end) = self.keyword_end(at, "MIDDLE") {
            let start = self.ws_end(end);
            if start > end
                && let Some(digits_end) = self.digits_end(start)
                && self.byte(digits_end) == Some(b'%')
            {
                self.pos = digits_end + 1;
                return Some(PosFilter::Middle((start, digits_end)));
            }
        }
        // `WORDS lo TO hi`, whitespace optional between the parts.
        if let Some(end) = self.keyword_end(at, "WORDS") {
            let lo_start = self.ws_end(end);
            if let Some(lo_end) = self.digits_end(lo_start) {
                let at = self.ws_end(lo_end);
                if let Some(end) = self.keyword_end(at, "TO") {
                    let hi_start = self.ws_end(end);
                    if let Some(hi_end) = self.digits_end(hi_start) {
                        self.pos = hi_end;
                        return Some(PosFilter::Between((lo_start, lo_end), (hi_start, hi_end)));
                    }
                }
            }
        }
        self.missing(at);
        None
    }

    /// `FIRST`/`LAST`, whitespace, digits, then `%` or whitespace and
    /// `WORDS`: the end, the digits, and whether a percentage.
    fn first_or_last(&self, at: usize, word: &str) -> Option<(usize, (usize, usize), bool)> {
        let end = self.keyword_end(at, word)?;
        let start = self.ws_end(end);
        if start == end {
            return None;
        }
        let digits_end = self.digits_end(start)?;
        if self.byte(digits_end) == Some(b'%') {
            return Some((digits_end + 1, (start, digits_end), true));
        }
        let words = self.ws_end(digits_end);
        if words == digits_end {
            return None;
        }
        let end = self.keyword_end(words, "WORDS")?;
        Some((end, (start, digits_end), false))
    }

    // ── Relation tier ───────────────────────────────────────────────

    fn rel_expr(&mut self) -> R {
        if !self.span_expr()? {
            return Ok(false);
        }
        let mark = self.mark();
        if let Some(op) = self.rel_op()
            && self.span_expr()?
        {
            self.join_rel(op)?;
            return Ok(true);
        }
        self.reset(mark);
        Ok(true)
    }

    #[inline(never)]
    fn join_rel(&mut self, op: RelOp) -> R<()> {
        self.binary(|l, r| match op {
            RelOp::Encloses => Expr::Encloses { big: l, little: r },
            RelOp::NotEncloses => Expr::NotEncloses { big: l, little: r },
            RelOp::EnclosedBy => Expr::EnclosedBy { little: l, big: r },
            RelOp::NotEnclosedBy => Expr::NotEnclosedBy { little: l, big: r },
            RelOp::Overlapping => Expr::Overlapping { a: l, b: r },
            RelOp::NotOverlapping => Expr::NotOverlapping { a: l, b: r },
            RelOp::Before => Expr::Before { a: l, b: r },
            RelOp::After => Expr::After { a: l, b: r },
        })
    }

    /// Whitespace, a relation operator, whitespace.
    fn rel_op(&mut self) -> Option<RelOp> {
        self.skip_ws();
        let at = self.pos;
        let two = |parser: &Self, first: &str, second: &str| {
            let end = parser.keyword_end(at, first)?;
            parser.keyword_end(parser.ws_end(end), second)
        };
        let three = |parser: &Self, first: &str, second: &str, third: &str| {
            let end = two(parser, first, second)?;
            parser.keyword_end(parser.ws_end(end), third)
        };
        let found = [
            (two(self, "NOT", "ENCLOSES"), RelOp::NotEncloses),
            (three(self, "NOT", "ENCLOSED", "BY"), RelOp::NotEnclosedBy),
            (two(self, "NOT", "OVERLAPPING"), RelOp::NotOverlapping),
            (self.keyword_end(at, "ENCLOSES"), RelOp::Encloses),
            (two(self, "ENCLOSED", "BY"), RelOp::EnclosedBy),
            (self.keyword_end(at, "OVERLAPPING"), RelOp::Overlapping),
            (self.keyword_end(at, "BEFORE"), RelOp::Before),
            (self.keyword_end(at, "AFTER"), RelOp::After),
        ]
        .into_iter()
        .find_map(|(end, op)| end.map(|end| (end, op)));
        match found {
            Some((end, op)) => {
                self.pos = self.ws_end(end);
                Some(op)
            }
            None => {
                self.missing(at);
                None
            }
        }
    }

    // ── Span tier ───────────────────────────────────────────────────

    fn span_expr(&mut self) -> R {
        if !self.atom_expr()? {
            return Ok(false);
        }
        loop {
            let mark = self.mark();
            // The builder reads the gap before building the right side.
            if let Some((then, gap)) = self.span_op()
                && self.atom_expr()?
            {
                self.join_span(then, gap)?;
                continue;
            }
            self.reset(mark);
            return Ok(true);
        }
    }

    #[inline(never)]
    fn join_span(&mut self, then: bool, gap: u32) -> R<()> {
        self.binary(|left, right| {
            if then {
                Expr::Then { left, right, gap }
            } else {
                Expr::Near { left, right, gap }
            }
        })
    }

    /// Whitespace, `THEN/n` or `NEAR/n` (no whitespace inside), whitespace:
    /// whether `THEN`, and `n`.
    fn span_op(&mut self) -> Option<(bool, u32)> {
        self.skip_ws();
        let at = self.pos;
        let (then, end) = match self.keyword_end(at, "THEN") {
            Some(end) => (true, end),
            None => (false, self.keyword_end(at, "NEAR")?),
        };
        if self.byte(end) != Some(b'/') {
            return None;
        }
        let digits_end = self.digits_end(end + 1)?;
        let gap = self.number((end + 1, digits_end));
        self.pos = self.ws_end(digits_end);
        Some((then, gap))
    }

    // ── Atom tier ───────────────────────────────────────────────────

    fn atom_expr(&mut self) -> R {
        if !self.primary()? {
            return Ok(false);
        }
        self.within_suffix()?;
        Ok(true)
    }

    /// An optional `WITHIN n` on the top node.
    #[inline(never)]
    fn within_suffix(&mut self) -> R<()> {
        let mark = self.mark();
        self.skip_ws();
        if let Some(end) = self.keyword_end(self.pos, "WITHIN") {
            let start = self.ws_end(end);
            if let Some(digits_end) = self.digits_end(start) {
                self.pos = digits_end;
                let width = self.number((start, digits_end));
                return self.wrap(|inner| Expr::Within { width, inner });
            }
        }
        self.reset(mark);
        Ok(())
    }

    fn primary(&mut self) -> R {
        if !self.base()? {
            return Ok(false);
        }
        self.boost_suffix()?;
        Ok(true)
    }

    /// An optional `^factor` on the top node.
    #[inline(never)]
    fn boost_suffix(&mut self) -> R<()> {
        let base_end = self.pos;
        let mark = self.mark();
        self.skip_ws();
        let boost_start = self.pos;
        let number_end = match self.byte(boost_start) {
            Some(b'^') => self.number_end(boost_start + 1),
            _ => None,
        };
        let Some(end) = number_end else {
            self.reset(mark);
            return Ok(());
        };
        self.pos = end;
        // The grammar skips whitespace before `^`; the builder rejects it.
        if base_end != boost_start {
            self.defer(ParseError::Expected {
                expected: "'^' adjacent to expression (no space)".into(),
                pos: self.base + boost_start,
                found: "whitespace before '^'".into(),
            });
            return Ok(());
        }
        let text = &self.src[boost_start + 1..end];
        // The number syntax guarantees `parse` succeeds; it can still
        // overflow to +inf, and BM25's f32 score fold needs the factor
        // inside the documented boost domain.
        let factor: f32 = text.parse().expect("boost number syntax");
        if !(0.0..=BoostFactor::MAX).contains(&factor) {
            self.defer(ParseError::BoostOutOfRange {
                text: text.to_string(),
                pos: self.base + boost_start + 1,
            });
        }
        let factor = BoostFactor(factor);
        self.wrap(|inner| Expr::Boost { factor, inner })
    }

    /// The end of a boost number at `at`: digits, an optional fraction, an
    /// optional exponent.
    fn number_end(&self, at: usize) -> Option<usize> {
        let mut end = self.digits_end(at)?;
        if self.byte(end) == Some(b'.')
            && let Some(fraction_end) = self.digits_end(end + 1)
        {
            end = fraction_end;
        }
        if matches!(self.byte(end), Some(b'e' | b'E')) {
            let mut digits = end + 1;
            if matches!(self.byte(digits), Some(b'+' | b'-')) {
                digits += 1;
            }
            if let Some(exponent_end) = self.digits_end(digits) {
                end = exponent_end;
            }
        }
        Some(end)
    }

    // ── Primary ─────────────────────────────────────────────────────

    fn base(&mut self) -> R {
        // The ordered choice, dispatched on the first character where that
        // decides the alternative.
        match self.byte(self.pos) {
            Some(b'(') => self.grouped(),
            Some(b'[') => self.alternatives_expr(),
            Some(b'"') => self.phrase(),
            _ => Ok(self.all_of()?
                || self.at_least()?
                || self.matches_expr()?
                || self.contains_expr()?
                || self.word_primary()?),
        }
    }

    fn grouped(&mut self) -> R {
        let mark = self.mark();
        self.enter()?;
        self.skip_ws();
        let matched = self.or_expr()? && {
            self.skip_ws();
            self.eat(b')')
        };
        self.nesting -= 1;
        if !matched {
            self.reset(mark);
        }
        Ok(matched)
    }

    /// `[` elements `]`: pushes the elements; returns where the bracket
    /// opens and how many.
    fn alternatives(&mut self) -> R<Option<(usize, usize)>> {
        let mark = self.mark();
        let open = self.pos;
        if self.byte(open) != Some(b'[') {
            self.missing(open);
            return Ok(None);
        }
        self.enter()?;
        self.skip_ws();
        let mut count = 0;
        if self.alt_or_expr()? {
            count = 1;
            loop {
                let element = self.mark();
                self.skip_ws();
                if !self.alt_or_expr()? {
                    self.reset(element);
                    break;
                }
                count += 1;
            }
        }
        self.skip_ws();
        let closed = self.eat(b']');
        self.nesting -= 1;
        if !closed {
            self.reset(mark);
            return Ok(None);
        }
        Ok(Some((open, count)))
    }

    fn alternatives_expr(&mut self) -> R {
        let Some((open, count)) = self.alternatives()? else {
            return Ok(false);
        };
        self.finish_list(open, count, None)?;
        Ok(true)
    }

    /// Replaces the top `count` elements of the list opened at `open` with
    /// the list: alternatives, or `AT LEAST`/`ALL OF` at `threshold`. The
    /// builder rejects an empty list and splits bare words on commas.
    #[inline(never)]
    fn finish_list(
        &mut self,
        open: usize,
        count: usize,
        threshold: Option<AtLeastThreshold>,
    ) -> R<()> {
        if count == 0 {
            self.defer(ParseError::EmptyAlternatives {
                pos: self.base + open,
            });
        }
        let items = self.built.split_off(self.built.len() - count);
        let depth = items.iter().map(|item| item.depth).max().unwrap_or(0) + 1;
        let exprs: Vec<Expr> = items.into_iter().map(|item| item.expr).collect();
        // Splitting on commas turns one counted word into several terms.
        let leaves = |exprs: &[Expr]| exprs.iter().filter(|expr| is_leaf(expr)).count();
        let before = leaves(&exprs);
        let exprs = split_comma_separated_alternatives(exprs);
        self.count_terms(leaves(&exprs).saturating_sub(before), open)?;
        let expr = match threshold {
            None => Expr::Alternatives(exprs),
            Some(threshold) => Expr::AtLeast { threshold, exprs },
        };
        self.push(expr, depth)
    }

    fn all_of(&mut self) -> R {
        let mark = self.mark();
        if !self.eat_keyword("ALL") || !self.separator("OF") {
            self.reset(mark);
            return Ok(false);
        }
        let Some((open, count)) = self.alternatives()? else {
            self.reset(mark);
            return Ok(false);
        };
        self.finish_list(open, count, Some(AtLeastThreshold::All))?;
        Ok(true)
    }

    fn at_least(&mut self) -> R {
        let mark = self.mark();
        let Some(threshold) = self.at_least_head() else {
            self.reset(mark);
            return Ok(false);
        };
        let Some((open, count)) = self.alternatives()? else {
            self.reset(mark);
            return Ok(false);
        };
        self.finish_list(open, count, Some(threshold))?;
        Ok(true)
    }

    /// `AT LEAST n OF` or `AT LEAST n% OF` and the whitespace after it.
    #[inline(never)]
    fn at_least_head(&mut self) -> Option<AtLeastThreshold> {
        if !self.eat_keyword("AT") || !self.separator("LEAST") {
            return None;
        }
        let digits_start = self.pos;
        let Some(digits_end) = self.digits_end(digits_start) else {
            self.missing(digits_start);
            return None;
        };
        self.pos = digits_end;
        let percent = self.byte(digits_end) == Some(b'%');
        if percent {
            self.pos += 1;
        }
        if !self.separator("OF") {
            return None;
        }
        // The builder reads the count before the list.
        let n = self.number((digits_start, digits_end));
        Some(if percent {
            AtLeastThreshold::Percent(n)
        } else {
            AtLeastThreshold::Count(n)
        })
    }

    // ── Phrase ──────────────────────────────────────────────────────

    #[inline(never)]
    fn phrase(&mut self) -> R {
        let open = self.pos;
        let body_start = open + 1;
        let mut end = body_start;
        loop {
            match self.byte(end) {
                None | Some(b'"') => break,
                // An escape takes the next character, whatever it is; the
                // bytes of a multi-byte character are never `\` or `"`.
                Some(b'\\') if end + 1 < self.bytes.len() => end += 2,
                Some(_) => end += 1,
            }
        }
        if self.byte(end) != Some(b'"') {
            self.missing(end);
            return Ok(false);
        }
        let body_end = end;
        end += 1;
        let mut slop_digits = None;
        if self.byte(end) == Some(b'~')
            && let Some(digits_end) = self.digits_end(end + 1)
        {
            slop_digits = Some((end + 1, digits_end));
            end = digits_end;
        }
        self.pos = end;

        // The builder reads the slop before the content.
        let slop = slop_digits.map(|digits| self.number(digits));
        let (elements, depth) = self.phrase_content(body_start, body_end, open)?;
        self.push(Expr::Phrase { elements, slop }, depth)?;
        Ok(true)
    }

    /// The elements of the phrase body `start..end`, and the height of the
    /// phrase (0 without alternatives).
    fn phrase_content(
        &mut self,
        start: usize,
        end: usize,
        open: usize,
    ) -> R<(Vec<PhraseElement>, usize)> {
        let body = &self.src[start..end];
        if body.is_empty() {
            self.defer(ParseError::EmptyPhrase {
                pos: self.base + open,
            });
            return Ok((Vec::new(), 0));
        }
        let Ok(pairs) = PhraseContentParser::parse(PhraseRule::phrase_content, body) else {
            self.defer(ParseError::Expected {
                expected: "valid phrase content".into(),
                pos: self.base + start,
                found: "invalid phrase content".into(),
            });
            return Ok((Vec::new(), 0));
        };
        let content = pairs.into_iter().next().expect("phrase content pair");
        let mut elements = Vec::new();
        let mut depth = 0;
        for child in content.into_inner() {
            if child.as_rule() == PhraseRule::EOI {
                continue;
            }
            let Some(variant) = child.into_inner().next() else {
                continue;
            };
            match variant.as_rule() {
                PhraseRule::phrase_word => {
                    self.count_terms(1, start + variant.as_span().start())?;
                    let text = variant.as_str();
                    elements.push(PhraseElement::Term(if text.contains('\\') {
                        unescape_phrase_term(text)
                    } else {
                        text.to_string()
                    }));
                }
                PhraseRule::phrase_gap => {
                    elements.push(PhraseElement::Gap(variant.as_str().len() as u32));
                }
                PhraseRule::phrase_alternatives => {
                    let body = variant
                        .into_inner()
                        .find(|p| p.as_rule() == PhraseRule::phrase_alt_body)
                        .expect("phrase alternatives body");
                    let offset = start + body.as_span().start();
                    let (exprs, height) = self.phrase_alternatives(body.as_str(), offset)?;
                    depth = depth.max(height + 1);
                    elements.push(PhraseElement::Alternatives(exprs));
                }
                _ => {}
            }
        }
        if elements.is_empty() {
            self.defer(ParseError::EmptyPhrase {
                pos: self.base + open,
            });
        }
        Ok((elements, depth))
    }

    /// Parses the alternatives `[text]` of a phrase, `text` found at
    /// `offset`, as an alternatives list (no emptiness check, no comma
    /// splitting, and anything after its closing bracket ignored, as the
    /// pest builder did). Its findings are this parser's findings.
    fn phrase_alternatives(&mut self, text: &str, offset: usize) -> R<(Vec<Expr>, usize)> {
        let synthetic = format!("[{text}]");
        let mut sub = Parser::new(
            &synthetic,
            self.implicit_op,
            self.base + offset - 1,
            self.nesting,
            self.terms,
        );
        let parsed = match sub.alternatives() {
            Ok(parsed) => parsed,
            Err(Stop) => {
                let error = sub.stopped.take().expect("a stop records its limit");
                return Err(self.stop(error));
            }
        };
        self.terms = sub.terms;
        match parsed {
            Some(_) => {
                if let Some(error) = sub.deferred.take() {
                    self.defer(error);
                }
                let depth = sub.built.iter().map(|item| item.depth).max().unwrap_or(0);
                Ok((sub.built.into_iter().map(|item| item.expr).collect(), depth))
            }
            None => {
                self.defer(ParseError::Expected {
                    expected: "expression".into(),
                    pos: sub.base + sub.furthest,
                    found: "unexpected input".into(),
                });
                Ok((Vec::new(), 0))
            }
        }
    }

    // ── MATCHES (regex) ─────────────────────────────────────────────

    #[inline(never)]
    fn matches_expr(&mut self) -> R {
        let Some(keyword_end) = self.keyword_end(self.pos, "MATCHES") else {
            return Ok(false);
        };
        let start = self.ws_end(keyword_end);
        if start == keyword_end {
            self.missing(start);
            return Ok(false);
        }
        let end = self.pattern_end(start);
        if end == start {
            self.missing(start);
            return Ok(false);
        }
        let at = self.pos;
        self.pos = end;
        let expr = Expr::Regex(unescape_pattern(&self.src[start..end]));
        self.leaf(expr, at)
    }

    /// The end of the `MATCHES` pattern starting at `start`.
    ///
    /// The grammar's pattern is one or more atoms: `\` and any character, a
    /// group `(` atoms `)`, a class `[` ... `]` (any characters but
    /// unescaped `]` and whitespace inside), or any character but whitespace,
    /// `)` and `]`, which also takes a `(` or `[` that does not open a
    /// complete group or class. Atoms stop at whitespace, `)` or `]`. A
    /// group left open when they stop cannot close later either, so the
    /// pattern ends there, and a `)` ends it when no group is open. That
    /// makes the extent one linear scan, where pest's backtracking over the
    /// same rules took time exponential in the unclosed groups.
    fn pattern_end(&mut self, start: usize) -> usize {
        let mut open_groups = 0usize;
        let mut at = start;
        while let Some(byte) = self.byte(at) {
            match byte {
                b']' => break,
                b')' if open_groups == 0 => break,
                b')' => {
                    open_groups -= 1;
                    at += 1;
                }
                b'(' => {
                    open_groups += 1;
                    at += 1;
                }
                b'[' => {
                    at = match self.class_close(at + 1) {
                        Some(close) => close + 1,
                        None => at + 1,
                    };
                }
                b'\\' if at + 1 < self.bytes.len() => at += 2,
                byte if is_ws(byte) => break,
                _ => at += 1,
            }
        }
        at
    }

    /// Where the character class whose body starts at `at` closes.
    fn class_close(&mut self, at: usize) -> Option<usize> {
        let bytes = self.bytes;
        let table = self.class_close.get_or_insert_with(|| {
            // Right to left, so a failing class costs O(1) however many
            // unclosed `[` precede it (pest rescanned from each).
            let n = bytes.len();
            let mut close = vec![NO_CLASS_CLOSE; n + 2];
            for i in (0..n).rev() {
                close[i] = match bytes[i] {
                    b']' => i,
                    byte if is_ws(byte) => NO_CLASS_CLOSE,
                    b'\\' if i + 1 < n => close[i + 2],
                    _ => close[i + 1],
                };
            }
            close
        });
        let close = table[at];
        (close != NO_CLASS_CLOSE).then_some(close)
    }

    // ── CONTAINS and words ──────────────────────────────────────────

    #[inline(never)]
    fn contains_expr(&mut self) -> R {
        let mark = self.mark();
        let Some(keyword_end) = self.keyword_end(self.pos, "CONTAINS") else {
            return Ok(false);
        };
        let start = self.ws_end(keyword_end);
        if start == keyword_end {
            self.missing(start);
            return Ok(false);
        }
        self.pos = start;
        let matched = self.word_primary()?;
        if !matched {
            self.reset(mark);
        }
        Ok(matched)
    }

    #[inline(never)]
    fn word_primary(&mut self) -> R {
        let start = self.pos;
        let Some(word_end) = self.word_end(start) else {
            self.missing(start);
            return Ok(false);
        };

        // word, whitespace, TO, whitespace, word: a range.
        let to = self.ws_end(word_end);
        if to > word_end
            && let Some(to_end) = self.keyword_end(to, "TO")
        {
            let upper_start = self.ws_end(to_end);
            if upper_start > to_end
                && let Some(upper_end) = self.word_end(upper_start)
            {
                self.pos = upper_end;
                let lower = self.range_bound(start, word_end);
                let upper = self.range_bound(upper_start, upper_end);
                return self.leaf(Expr::Range { lower, upper }, start);
            }
        }

        // word~N or word~P:N: fuzzy.
        if self.byte(word_end) == Some(b'~')
            && let Some(first_end) = self.digits_end(word_end + 1)
        {
            let first = (word_end + 1, first_end);
            let mut end = first_end;
            let mut second = None;
            if self.byte(first_end) == Some(b':')
                && let Some(second_end) = self.digits_end(first_end + 1)
            {
                second = Some((first_end + 1, second_end));
                end = second_end;
            }
            self.pos = end;
            let (prefix, distance) = match second {
                Some(second) => {
                    let prefix = self.number(first);
                    (prefix, self.number(second))
                }
                None => (1, self.number(first)),
            };
            let term = self.src[start..word_end].to_string();
            return self.leaf(
                Expr::Fuzzy {
                    term,
                    prefix,
                    distance,
                },
                start,
            );
        }

        // Any other word, unless it is a keyword.
        if self.is_keyword_at(start) {
            self.missing(start);
            return Ok(false);
        }
        self.pos = word_end;
        let word = &self.src[start..word_end];
        let expr = if word == "*" {
            Expr::MatchAll
        } else {
            classify_term(word)
        };
        self.leaf(expr, start)
    }

    /// A range bound: `*` (open) or a plain term. A wildcard has no defined
    /// range semantics and would otherwise be silently stripped by analysis.
    fn range_bound(&mut self, start: usize, end: usize) -> RangeBound {
        let text = &self.src[start..end];
        if text == "*" {
            return RangeBound::Open;
        }
        if matches!(classify_term(text), Expr::Wildcard(_)) {
            self.defer(ParseError::WildcardInRangeBound {
                text: text.to_string(),
                pos: self.base + start,
            });
        }
        RangeBound::Term(text.to_string())
    }
}

fn is_leaf(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Term(_)
            | Expr::MatchAll
            | Expr::Fuzzy { .. }
            | Expr::Wildcard(_)
            | Expr::Regex(_)
            | Expr::Range { .. }
    )
}

/// Unescapes `\` + whitespace in a `MATCHES` pattern; every other `\X` is
/// regex syntax and passes through.
fn unescape_pattern(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    let mut pattern = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(ws) if ws.is_ascii_whitespace() => pattern.push(ws),
                Some(other) => {
                    pattern.push('\\');
                    pattern.push(other);
                }
                None => pattern.push('\\'),
            }
        } else {
            pattern.push(c);
        }
    }
    pattern
}

/// Alternatives lists document commas as element separators
/// (`[beer, ale, lager]` means `or(beer, ale, lager)`), but the word rule
/// deliberately admits `,` as a term character so free text keeps parsing.
/// Re-split bare-word elements on separator commas here so `[beer,wine]` and
/// `[beer, wine]` both mean the documented OR instead of a phrase. A comma
/// between two digits is a numeric separator, not a list separator — the same
/// distinction word segmentation makes — so `[47,000 48,000]` keeps its two
/// numeric terms.
pub(super) fn split_comma_separated_alternatives(items: Vec<Expr>) -> Vec<Expr> {
    items
        .into_iter()
        .flat_map(|item| match item {
            Expr::Term(word) if word.contains(',') => {
                let pieces: Vec<Expr> = split_separator_commas(&word)
                    .into_iter()
                    .map(|piece| classify_term(&piece))
                    .collect();
                if pieces.is_empty() {
                    // Pure separator residue like a bare "," element:
                    // analysis-empty, matches nothing.
                    vec![Expr::MatchNone]
                } else {
                    pieces
                }
            }
            other => vec![other],
        })
        .collect()
}

/// Splits `word` at every comma that is not flanked by digits on both sides,
/// dropping empty pieces.
fn split_separator_commas(word: &str) -> Vec<String> {
    let bytes = word.as_bytes();
    let mut pieces = Vec::new();
    let mut start = 0;
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte != b',' {
            continue;
        }
        let numeric_separator = idx > 0
            && bytes[idx - 1].is_ascii_digit()
            && bytes.get(idx + 1).is_some_and(u8::is_ascii_digit);
        if numeric_separator {
            continue;
        }
        if start < idx {
            pieces.push(word[start..idx].to_string());
        }
        start = idx + 1;
    }
    if start < word.len() {
        pieces.push(word[start..].to_string());
    }
    pieces
}
