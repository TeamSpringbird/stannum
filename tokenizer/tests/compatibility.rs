use tokenizer::{
    Folding, GraphemeMode, LongTokenMode, PositionGapMode, Tokenizer, TokenizerPipelineSpec,
    TokenizerSpec,
};

fn tokens(spec: TokenizerPipelineSpec, text: &str) -> Vec<(String, u32)> {
    spec.compile()
        .unwrap()
        .tokenize(text)
        .map(|token| (token.text.into_owned(), token.pos))
        .collect()
}

#[test]
fn unicode_boundaries_cover_scripts_numbers_apostrophes_hyphens_and_urls() {
    let actual = tokens(
        TokenizerPipelineSpec::default(),
        "Éclair E\u{301}clair Ελληνικά 東京 👩‍💻 3.14 1,234 can't wi-fi https://Example.com/a",
    );
    let expected = [
        "eclair",
        "eclair",
        "ελληνικα",
        "東",
        "京",
        "👩‍💻",
        "3.14",
        "1,234",
        "can't",
        "wi",
        "fi",
        "https",
        "example.com",
        "a",
    ];
    assert_eq!(
        actual,
        expected
            .into_iter()
            .enumerate()
            .map(|(i, text)| (text.into(), i as u32))
            .collect::<Vec<_>>()
    );
    let whitespace = TokenizerPipelineSpec {
        tokenizer: TokenizerSpec::Whitespace,
        ..Default::default()
    };
    assert_eq!(
        tokens(whitespace, "Can't wi-fi https://Example.com/a 1,234"),
        vec![
            ("can't".into(), 0),
            ("wi-fi".into(), 1),
            ("https://example.com/a".into(), 2),
            ("1,234".into(), 3)
        ]
    );
}

#[test]
fn folding_modes_pin_lowercasing_and_canonical_accent_removal() {
    for (case_folding, accent_folding, expected) in [
        (Folding::Fold, Folding::Fold, ["e", "e", "i", "σ", "ß"]),
        (Folding::Preserve, Folding::Fold, ["E", "E", "I", "Σ", "ß"]),
        (
            Folding::Fold,
            Folding::Preserve,
            ["é", "e\u{301}", "i\u{307}", "σ", "ß"],
        ),
        (
            Folding::Preserve,
            Folding::Preserve,
            ["É", "E\u{301}", "İ", "Σ", "ß"],
        ),
    ] {
        let spec = TokenizerPipelineSpec {
            case_folding,
            accent_folding,
            ..Default::default()
        };
        let actual: Vec<_> = tokens(spec, "É E\u{301} İ Σ ß")
            .into_iter()
            .map(|(term, _)| term)
            .collect();
        assert_eq!(actual, expected);
    }
}

#[test]
fn grapheme_modes_keep_emoji_sequences_together() {
    for (graphemes, expected) in [
        (GraphemeMode::Discard, vec!["a"]),
        (GraphemeMode::Emoji, vec!["👩‍💻", "🇬🇧", "a"]),
        (GraphemeMode::Retain, vec!["→", "👩‍💻", "🇬🇧", "a"]),
    ] {
        let spec = TokenizerPipelineSpec {
            graphemes,
            ..Default::default()
        };
        let actual: Vec<_> = tokens(spec, "→ 👩‍💻 🇬🇧 a")
            .into_iter()
            .map(|(term, _)| term)
            .collect();
        assert_eq!(actual, expected);
    }
}

#[test]
fn every_long_token_and_gap_mode_applies_after_folding() {
    for mode in [
        LongTokenMode::Split,
        LongTokenMode::Truncate,
        LongTokenMode::Discard,
    ] {
        for gaps in [PositionGapMode::Preserve, PositionGapMode::Collapse] {
            let mut spec = TokenizerPipelineSpec::default();
            spec.long_tokens.mode = mode;
            spec.long_tokens.max_bytes = 4;
            spec.position_gaps = gaps;
            let expected = match mode {
                LongTokenMode::Split => {
                    vec![("eeee".into(), 0), ("e".into(), 1), ("tail".into(), 2)]
                }
                LongTokenMode::Truncate => vec![("eeee".into(), 0), ("tail".into(), 1)],
                LongTokenMode::Discard => {
                    vec![("tail".into(), u32::from(gaps == PositionGapMode::Preserve))]
                }
            };
            assert_eq!(tokens(spec, "ééééé tail"), expected);
        }
    }
}
