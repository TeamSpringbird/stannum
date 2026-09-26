// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! The recursive-descent parser against the pest parser it replaced: on
//! inputs well inside the limits both must accept the same queries, build
//! the same trees, and report the same builder findings.

use proptest::prelude::*;

use crate::ImplicitOp;
use crate::ast::Expr;
use crate::error::ParseError;

use super::{descent, pest_parser};

fn parse_both(
    input: &str,
    implicit_op: ImplicitOp,
) -> (Result<Expr, ParseError>, Result<Expr, ParseError>) {
    (
        descent::parse(input, implicit_op),
        pest_parser::parse(input, implicit_op),
    )
}

/// Agreement on one input: the same tree, or both an error. A builder
/// finding (anything but a syntax error) must be the same finding, except
/// that inside phrase alternatives pest reported positions relative to its
/// re-parse and the descent parser reports them in the query.
fn assert_agree(input: &str) {
    for implicit_op in [ImplicitOp::And, ImplicitOp::Or] {
        let (descent, pest) = parse_both(input, implicit_op);
        match (&descent, &pest) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "trees differ for {input:?}"),
            (Err(_), Err(ParseError::Expected { .. })) => {}
            (Err(a), Err(b)) => {
                assert_eq!(
                    std::mem::discriminant(a),
                    std::mem::discriminant(b),
                    "findings differ for {input:?}: {a} vs {b}"
                );
                if !input.contains('"') {
                    assert_eq!(
                        a.to_string(),
                        b.to_string(),
                        "findings differ for {input:?}"
                    );
                }
            }
            _ => panic!("parsers disagree on {input:?}: descent {descent:?}, pest {pest:?}"),
        }
    }
}

const FRAGMENTS: &[&str] = &[
    // words and word-like terms
    "a",
    "b",
    "beer",
    "x1",
    "*",
    "a*",
    "b?r",
    r"\*",
    "a,b",
    ",",
    "47,000",
    "/p",
    r"\",
    ":tag",
    "a:b",
    "é",
    "日本",
    "-foo",
    "wi-fi",
    "a.b",
    // keywords, some lowercase
    "AND",
    "OR",
    "NOT",
    "TO",
    "IN",
    "WITHIN",
    "THEN/2",
    "NEAR/1",
    "THEN/99999999999",
    "THEN",
    "NEAR",
    "ALL",
    "AT",
    "LEAST",
    "OF",
    "MATCHES",
    "CONTAINS",
    "ENCLOSES",
    "ENCLOSED",
    "BY",
    "OVERLAPPING",
    "BEFORE",
    "AFTER",
    "FIRST",
    "LAST",
    "MIDDLE",
    "WORDS",
    "and",
    "or",
    "at",
    "least",
    "of",
    "all",
    "ANDx",
    "AND/x",
    "OR(",
    "TO/",
    // numbers
    "3",
    "25%",
    "50%",
    "99999999999",
    "0",
    "1:2",
    // punctuation
    "(",
    ")",
    "[",
    "]",
    "\"",
    "~2",
    "~1:2",
    "~0:2",
    "~99999999999",
    "~",
    "^2",
    "^1.5",
    "^1e5",
    "^1E+5",
    "^1.",
    "^",
    "^99999",
    "_",
    r#"\""#,
    r"\ ",
    "%",
    ":",
    // regex fragments
    "MATCHES a(b",
    "MATCHES [a)]",
    "MATCHES (a]",
    r"MATCHES \)x",
    r"MATCHES [\]]",
    "MATCHES a\"b",
    "MATCHES ((a)",
    "MATCHES [[a]",
    "MATCHES a[",
    r"MATCHES a\ b",
    // phrases
    "\"big bad\"",
    "\"a _ b\"",
    "\"[x y] z\"",
    "\"[x (y] z\"",
    "\"\"",
    "\"[]\"",
    "\"a\"~2",
    "\"[a,b] c\"",
    r#""[a \] b]""#,
    "\"[a~99999999999] b\"",
    "\"[(a b) c] d\"",
];

const SEPARATORS: &[&str] = &[" ", " ", " ", "", "  ", "\t", "\n"];

fn soup() -> impl Strategy<Value = String> {
    prop::collection::vec(
        (
            prop::sample::select(FRAGMENTS),
            prop::sample::select(SEPARATORS),
        ),
        1..14,
    )
    .prop_map(|parts| {
        parts
            .into_iter()
            .map(|(fragment, separator)| format!("{fragment}{separator}"))
            .collect()
    })
}

/// Mostly well-formed queries, nested a few levels.
fn query() -> impl Strategy<Value = String> {
    let leaf = prop::sample::select(
        &[
            "beer",
            "wine",
            "a",
            "b*",
            "beer~1",
            "a TO c",
            "*",
            "\"craft beer\"",
            "\"a _ b\"",
            "\"[x y] z\"",
            "MATCHES h.p",
            "CONTAINS ale",
            "[x, y]",
            "x^2",
            "ale^1.5",
        ][..],
    )
    .prop_map(str::to_string);
    leaf.prop_recursive(4, 48, 4, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("{a} {b}")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("{a} AND {b}")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("{a} OR {b}")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("{a} AND NOT {b}")),
            (inner.clone(), inner.clone(), 0u32..4)
                .prop_map(|(a, b, n)| format!("{a} THEN/{n} {b}")),
            (inner.clone(), inner.clone(), 0u32..4)
                .prop_map(|(a, b, n)| format!("{a} NEAR/{n} {b}")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("{a} ENCLOSES {b}")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("{a} NOT ENCLOSED BY {b}")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("{a} BEFORE {b}")),
            inner.clone().prop_map(|a| format!("({a})")),
            inner.clone().prop_map(|a| format!("({a}) WITHIN 3")),
            inner.clone().prop_map(|a| format!("({a})^2")),
            inner.clone().prop_map(|a| format!("{a} IN FIRST 5 WORDS")),
            inner.clone().prop_map(|a| format!("{a} IN LAST 25%")),
            inner.clone().prop_map(|a| format!("({a}) IN MIDDLE 50%")),
            inner.clone().prop_map(|a| format!("({a}) IN WORDS 2 TO 9")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("[{a} {b}]")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("AT LEAST 1 OF [{a} {b}]")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("ALL OF [{a}, {b}]")),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 20_000, ..ProptestConfig::default() })]

    #[test]
    fn descent_agrees_with_pest_on_token_soup(input in soup()) {
        assert_agree(&input);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 5_000, ..ProptestConfig::default() })]

    #[test]
    fn descent_agrees_with_pest_on_queries(input in query()) {
        assert_agree(&input);
    }

    #[test]
    fn descent_agrees_with_pest_on_mangled_queries(
        input in query(),
        cut in any::<prop::sample::Index>(),
        insert in prop::sample::select(FRAGMENTS),
    ) {
        // Splice a fragment in at a character boundary: mostly near-misses.
        let boundaries: Vec<usize> =
            input.char_indices().map(|(i, _)| i).chain([input.len()]).collect();
        let at = boundaries[cut.index(boundaries.len())];
        let mangled = format!("{}{insert}{}", &input[..at], &input[at..]);
        assert_agree(&mangled);
    }
}

#[test]
fn descent_agrees_with_pest_on_known_edges() {
    for input in [
        "a",
        "a b",
        "(a b) c",
        "a (b c)",
        "(a OR b) OR c",
        "a OR (b OR c)",
        "a b AND c d",
        "a AND NOT b AND NOT c",
        "a AND NOT ENCLOSES b",
        "x TO MATCHES",
        "MATCHES TO y",
        "ALL TO x",
        "AND TO b",
        "OR~2",
        "CONTAINS MATCHES TO x",
        "a~1:",
        "beer ^2",
        "beer^ 2",
        "a THEN/5b",
        "a IN FIRST 3 WORDS",
        "a IN FIRST 3WORDS",
        "a IN WORDS 1 TO 5",
        "a IN WORDS1 TO 5",
        "a IN MIDDLE 50%",
        "a IN MIDDLE 50",
        "[a (b]",
        "\\(foo",
        "MATCHES [)[]x",
        "MATCHES ([a)b",
        "(MATCHES a)",
        "[MATCHES a]",
        "MATCHES a)",
        "\"[a \\] b]\"",
        "\"[a,b] c\"",
        "\"[] c\"",
        "[]",
        "\"\"",
        "a\"b\"",
        "\"a\"b",
        "\"a\"~2b",
        "a^2.5e3",
        "a^1.x",
        "(a)^2",
        "[a]^2",
        "\"a b\"^2",
        "AT LEAST 2 OF [a b c]",
        "AT LEAST 50% OF [a b]",
        "AT LEAST 99999999999 OF [a]",
        "AT LEAST 2 [a]",
        "ALL OF [a,b]",
        "ALL OF [,]",
        "at least 3 of [a b]",
        "beer :tag",
        "beer AND :tag",
        "a~99999999999",
        "a TO b*",
        "* TO *",
        "a WITHIN 99999999999",
        "(a b) WITHIN 3 WITHIN 4",
        "a b c d e f g h",
        "a OR b c OR d",
        "[a AND b OR c AND NOT d]",
        "[a b AND NOT c]",
    ] {
        assert_agree(input);
    }
}
