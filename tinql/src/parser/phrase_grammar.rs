// Copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

use pest_derive::Parser;

#[derive(Parser)]
#[grammar = "parser/phrase.pest"]
pub(crate) struct PhraseContentParser;

// Re-export Rule under a distinct name
pub(crate) use self::Rule as PhraseRule;
