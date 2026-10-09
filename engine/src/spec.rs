// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

//! The tokenizer settings an index stores in its meta page, as bytes: what
//! a reader of a dumped index needs to analyze queries as the build did.
//! The values are also the extension's reloption enumeration values.

use tokenizer::{
    Folding, GraphemeMode, LongTokenMode, LongTokenSpec, PositionGapMode, Stemmer,
    TokenizerPipelineSpec, TokenizerSpec,
};

pub const TOKENIZER_UNICODE: i32 = 0;
pub const TOKENIZER_WHITESPACE: i32 = 1;
pub const FOLDING_PRESERVE: i32 = 0;
pub const FOLDING_FOLD: i32 = 1;
pub const LONG_TRUNCATE: i32 = 0;
pub const LONG_DISCARD: i32 = 1;
pub const LONG_SPLIT: i32 = 2;
pub const GRAPHEME_DISCARD: i32 = 0;
pub const GRAPHEME_EMOJI: i32 = 1;
pub const GRAPHEME_RETAIN: i32 = 2;
pub const GAPS_COLLAPSE: i32 = 0;
pub const GAPS_PRESERVE: i32 = 1;

/// The supported stemmers in their stored order: a spec stores a stemmer as
/// its position here plus one, zero for none. Append only.
const STEMMERS: [Stemmer; 18] = [
    Stemmer::Arabic,
    Stemmer::Danish,
    Stemmer::Dutch,
    Stemmer::English,
    Stemmer::Finnish,
    Stemmer::French,
    Stemmer::German,
    Stemmer::Greek,
    Stemmer::Hungarian,
    Stemmer::Italian,
    Stemmer::Norwegian,
    Stemmer::Portuguese,
    Stemmer::Romanian,
    Stemmer::Russian,
    Stemmer::Spanish,
    Stemmer::Swedish,
    Stemmer::Tamil,
    Stemmer::Turkish,
];

/// Bits of spec byte 0 below the stemmer; the tokenizer uses the lowest.
const STEMMER_SHIFT: u32 = 3;

/// Serialized tokenizer settings stored in the index meta page, so scans and
/// inserts analyze text exactly as the build did. Byte 0 holds the tokenizer
/// in its low bits and the stemmer above [`STEMMER_SHIFT`]; an index built
/// before stemming existed has zero there, no stemmer.
pub const SPEC_BYTES: usize = 8;

pub fn encode_spec(spec: &TokenizerPipelineSpec) -> [u8; SPEC_BYTES] {
    let folding = |value: Folding| match value {
        Folding::Preserve => FOLDING_PRESERVE,
        Folding::Fold => FOLDING_FOLD,
    } as u8;
    let mut out = [0u8; SPEC_BYTES];
    let stemmer = spec.stemmer.map_or(0, |stemmer| {
        STEMMERS
            .iter()
            .position(|&known| known == stemmer)
            .expect("every stemmer has a stored code")
            + 1
    }) as u8;
    out[0] = match spec.tokenizer {
        TokenizerSpec::Unicode => TOKENIZER_UNICODE,
        TokenizerSpec::Whitespace => TOKENIZER_WHITESPACE,
    } as u8
        | stemmer << STEMMER_SHIFT;
    out[1] = folding(spec.case_folding);
    out[2] = folding(spec.accent_folding);
    out[3] = match spec.long_tokens.mode {
        LongTokenMode::Truncate => LONG_TRUNCATE,
        LongTokenMode::Discard => LONG_DISCARD,
        LongTokenMode::Split => LONG_SPLIT,
    } as u8;
    out[4..6].copy_from_slice(&(spec.long_tokens.max_bytes as u16).to_le_bytes());
    out[6] = match spec.graphemes {
        GraphemeMode::Discard => GRAPHEME_DISCARD,
        GraphemeMode::Emoji => GRAPHEME_EMOJI,
        GraphemeMode::Retain => GRAPHEME_RETAIN,
    } as u8;
    out[7] = match spec.position_gaps {
        PositionGapMode::Collapse => GAPS_COLLAPSE,
        PositionGapMode::Preserve => GAPS_PRESERVE,
    } as u8;
    out
}

pub fn decode_spec(bytes: &[u8; SPEC_BYTES]) -> Option<TokenizerPipelineSpec> {
    let folding = |value: u8| match i32::from(value) {
        FOLDING_PRESERVE => Some(Folding::Preserve),
        FOLDING_FOLD => Some(Folding::Fold),
        _ => None,
    };
    let stemmer = match usize::from(bytes[0] >> STEMMER_SHIFT) {
        0 => None,
        code => Some(*STEMMERS.get(code - 1)?),
    };
    let spec = TokenizerPipelineSpec {
        tokenizer: match i32::from(bytes[0] & ((1 << STEMMER_SHIFT) - 1)) {
            TOKENIZER_UNICODE => TokenizerSpec::Unicode,
            TOKENIZER_WHITESPACE => TokenizerSpec::Whitespace,
            _ => return None,
        },
        case_folding: folding(bytes[1])?,
        accent_folding: folding(bytes[2])?,
        long_tokens: LongTokenSpec {
            mode: match i32::from(bytes[3]) {
                LONG_TRUNCATE => LongTokenMode::Truncate,
                LONG_DISCARD => LongTokenMode::Discard,
                LONG_SPLIT => LongTokenMode::Split,
                _ => return None,
            },
            max_bytes: usize::from(u16::from_le_bytes([bytes[4], bytes[5]])),
        },
        graphemes: match i32::from(bytes[6]) {
            GRAPHEME_DISCARD => GraphemeMode::Discard,
            GRAPHEME_EMOJI => GraphemeMode::Emoji,
            GRAPHEME_RETAIN => GraphemeMode::Retain,
            _ => return None,
        },
        position_gaps: match i32::from(bytes[7]) {
            GAPS_COLLAPSE => PositionGapMode::Collapse,
            GAPS_PRESERVE => PositionGapMode::Preserve,
            _ => return None,
        },
        stemmer,
    };
    spec.validate().ok()?;
    Some(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_bytes_round_trip_every_setting() {
        let spec = TokenizerPipelineSpec {
            tokenizer: TokenizerSpec::Whitespace,
            case_folding: Folding::Preserve,
            accent_folding: Folding::Fold,
            long_tokens: LongTokenSpec {
                mode: LongTokenMode::Discard,
                max_bytes: 2_692,
            },
            graphemes: GraphemeMode::Retain,
            position_gaps: PositionGapMode::Collapse,
            stemmer: None,
        };
        assert_eq!(decode_spec(&encode_spec(&spec)), Some(spec));
        let default = TokenizerPipelineSpec::stannum_default();
        assert_eq!(decode_spec(&encode_spec(&default)), Some(default));
        assert_eq!(decode_spec(&[2, 0, 0, 0, 0, 1, 0, 0]), None);
        assert_eq!(decode_spec(&[0, 0, 0, 0, 1, 0, 0, 0]), None);
        // Past the last stored stemmer.
        assert_eq!(decode_spec(&[19 << 3, 1, 1, 2, 0, 1, 1, 1]), None);
    }

    #[test]
    fn spec_bytes_round_trip_every_stemmer() {
        let mut spec = TokenizerPipelineSpec {
            tokenizer: TokenizerSpec::Whitespace,
            ..TokenizerPipelineSpec::stannum_default()
        };
        for stemmer in STEMMERS {
            spec.stemmer = Some(stemmer);
            assert_eq!(decode_spec(&encode_spec(&spec)), Some(spec), "{stemmer}");
        }
        // An index built before stemming existed reads as unstemmed: its
        // byte 0 was the tokenizer alone.
        let before = [1, 1, 1, 2, 0, 1, 1, 1];
        assert_eq!(
            decode_spec(&before).map(|spec| (spec.tokenizer, spec.stemmer)),
            Some((TokenizerSpec::Whitespace, None))
        );
        assert_eq!(
            encode_spec(&TokenizerPipelineSpec::stannum_default()),
            [0, 1, 1, 2, 0, 1, 1, 1]
        );
    }
}
