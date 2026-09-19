// Copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

use thiserror::Error;

/// An error produced during query parsing.
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("expected {expected} (at byte {pos}), found {found}")]
    Expected {
        expected: String,
        pos: usize,
        found: String,
    },

    #[error("empty alternatives (at byte {pos})")]
    EmptyAlternatives { pos: usize },

    #[error("empty phrase (at byte {pos})")]
    EmptyPhrase { pos: usize },

    #[error(
        "number \"{text}\" is out of range (at byte {pos}): must fit in an unsigned 32-bit integer"
    )]
    NumberOutOfRange { text: String, pos: usize },

    #[error(
        "range bound \"{text}\" contains a wildcard (at byte {pos}): bounds must be plain terms"
    )]
    WildcardInRangeBound { text: String, pos: usize },

    #[error(
        "boost factor \"{text}\" is out of range (at byte {pos}): must be a finite value of at most {max}",
        max = crate::ast::BoostFactor::MAX
    )]
    BoostOutOfRange { text: String, pos: usize },
}
