// Copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

use std::borrow::Cow;
use std::fmt::Display;

mod compiled;
mod folder;
mod long_tokens;
mod normalizer;
pub mod presets;
mod source_spans;
mod spec;
mod stemmer;
pub mod tokenizers;

pub use compiled::CompiledTokenizerPipeline;
pub use source_spans::{SourceSpanIter, SourceSpans};
pub use spec::{
    Folding, GraphemeMode, LongTokenMode, LongTokenSpec, MIN_TOKEN_BYTES, PositionGapMode,
    TokenizerPipelineSpec, TokenizerPipelineSpecError, TokenizerSpec,
};
pub use stemmer::Stemmer;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Classification {
    #[default]
    Unknown = 0,
    Word,
    Punctuation,
    Whitespace,
    Symbol,
    Other,
    Emoji,
    Grapheme,
}

#[derive(Debug, PartialEq)]
pub struct Token<'a> {
    pub text: Cow<'a, str>,
    pub pos: u32,
    pub classification: Classification,
    came_from_split: bool,
}

impl Display for Token<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.text)
    }
}

impl<'a> Token<'a> {
    pub fn new(text: &'a str, pos: u32) -> Self {
        Self {
            text: Cow::Borrowed(text),
            pos,
            classification: Classification::default(),
            came_from_split: false,
        }
    }

    pub fn with_classification(text: &'a str, pos: u32, classification: Classification) -> Self {
        Self {
            text: Cow::Borrowed(text),
            pos,
            classification,
            came_from_split: false,
        }
    }

    pub fn came_from_split(&self) -> bool {
        self.came_from_split
    }

    pub(crate) fn mark_split(&mut self) {
        self.came_from_split = true;
    }
}

/// A token iterator that accounts for positions consumed by discarded tokens.
pub trait TokenStream<'text>: Iterator<Item = Token<'text>> {
    /// Final position count, including preserved gaps and split continuations.
    /// Call only after `next()` returns `None`; intermediate counts are unspecified.
    /// With collapsed gaps, this equals the number of emitted tokens.
    fn positions_consumed(&self) -> u32;
}

/// A [`Tokenizer`] converts a string into a stream of [`Token`]s.
pub trait Tokenizer {
    type Iter<'tokenizer, 'text>: TokenStream<'text>
    where
        Self: 'tokenizer;

    /// Tokenize the given text into a stream of tokens.
    fn tokenize<'tokenizer, 'text>(
        &'tokenizer self,
        text: &'text str,
    ) -> Self::Iter<'tokenizer, 'text>;

    /// Normalize query pattern literals, preserving their morphology.
    fn tokenize_literal<'tokenizer, 'text>(
        &'tokenizer self,
        text: &'text str,
    ) -> Self::Iter<'tokenizer, 'text> {
        self.tokenize(text)
    }
}

impl<T: Tokenizer> Tokenizer for &T {
    type Iter<'tokenizer, 'text>
        = T::Iter<'tokenizer, 'text>
    where
        Self: 'tokenizer;

    fn tokenize<'tokenizer, 'text>(
        &'tokenizer self,
        text: &'text str,
    ) -> Self::Iter<'tokenizer, 'text> {
        (*self).tokenize(text)
    }

    fn tokenize_literal<'tokenizer, 'text>(
        &'tokenizer self,
        text: &'text str,
    ) -> Self::Iter<'tokenizer, 'text> {
        (*self).tokenize_literal(text)
    }
}
