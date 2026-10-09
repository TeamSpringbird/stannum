// Copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

use crate::folder::CompiledFolder;
use crate::{Folding, Stemmer, Token};
use std::borrow::Cow;
use unicode_normalization::{UnicodeNormalization, is_nfc};

#[derive(Clone, Copy)]
pub(crate) enum NormalizerSpec {
    Fold(CompiledFolder),
    Stem(Stemmer, Folding),
}

impl NormalizerSpec {
    pub(crate) fn new(case: Folding, accent: Folding, stemmer: Option<Stemmer>) -> Self {
        match stemmer {
            Some(stemmer) => Self::Stem(stemmer, accent),
            None => Self::Fold(CompiledFolder::new(case, accent)),
        }
    }

    pub(crate) fn compile(self) -> CompiledNormalizer {
        match self {
            Self::Fold(folder) => CompiledNormalizer::Fold(folder),
            Self::Stem(stemmer, accent) => CompiledNormalizer::Stem {
                stemmer: stemmer.compile(),
                accent: CompiledFolder::new(Folding::Preserve, accent),
            },
        }
    }
}

pub(crate) enum CompiledNormalizer {
    Fold(CompiledFolder),
    Stem {
        stemmer: rust_stemmers::Stemmer,
        accent: CompiledFolder,
    },
}

impl CompiledNormalizer {
    pub(crate) fn apply(&self, token: &mut Token<'_>) {
        match self {
            Self::Fold(folder) => folder.apply(token),
            Self::Stem { stemmer, accent } => {
                CompiledFolder::Case.apply(token);
                // Snowball suffix rules must see the same accents for canonically
                // equivalent words; keep accent removal after stemming.
                if !is_nfc(&token.text) {
                    token.text = Cow::Owned(token.text.nfc().collect());
                }
                match &mut token.text {
                    Cow::Borrowed(text) => token.text = stemmer.stem(text),
                    Cow::Owned(text) => {
                        // A borrowed stem is unchanged. Keep the existing
                        // lowercase allocation instead of copying it again.
                        if let Cow::Owned(stemmed) = stemmer.stem(text) {
                            *text = stemmed;
                        }
                    }
                }
                accent.apply(token);
            }
        }
    }
}
