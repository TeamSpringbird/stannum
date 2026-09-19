// Copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

use pest_derive::Parser;

#[derive(Parser)]
#[grammar = "parser/pest_parser/phrase.pest"]
pub(super) struct PhraseContentParser;

// Re-export Rule under a distinct name
pub(super) use self::Rule as PhraseRule;
