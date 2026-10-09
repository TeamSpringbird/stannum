// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Stemmed tokenization against TIN 1.0.4's recorded answers
//! (`conformance/cases/stemming.yaml`, `stemming.tokenize.*`): every
//! documented language, and the order of lowercasing, stemming and accent
//! folding.

use tokenizer::{Folding, Stemmer, Tokenizer, TokenizerPipelineSpec};

fn stems(text: &str, code: &str, accent_folding: Folding) -> Vec<String> {
    let spec = TokenizerPipelineSpec {
        accent_folding,
        stemmer: Some(code.parse::<Stemmer>().expect("a documented code")),
        ..TokenizerPipelineSpec::stannum_default()
    };
    spec.compile()
        .expect("a valid specification")
        .tokenize(text)
        .map(|token| token.text.into_owned())
        .collect()
}

#[test]
fn every_documented_language_stems_as_tin_1_0_4() {
    let recorded: [(&str, &str, &[&str]); 18] = [
        ("ar", "المكتبات يكتبون", &["مكتب", "يكتب"]),
        ("da", "løbende løber hestene", &["løb", "løb", "hest"]),
        (
            "de",
            "Häuser laufen gelaufen Katzen",
            &["haus", "lauf", "gelauf", "katz"],
        ),
        ("el", "ανθρώπους ανθρώπων", &["ανθρωπ", "ανθρωπ"]),
        ("en", "running runs ponies", &["run", "run", "poni"]),
        ("es", "hablando hablar gatos", &["habl", "habl", "gat"]),
        ("fi", "taloissa taloja kissat", &["talo", "talo", "kis"]),
        (
            "fr",
            "chanter chantons chanté continuellement",
            &["chant", "chanton", "chant", "continuel"],
        ),
        ("hu", "házakban kertek", &["haz", "kert"]),
        ("it", "parlare parlando gatti", &["parl", "parl", "gatt"]),
        (
            "nl",
            "lopen loopt gelopen huizen",
            &["lop", "loopt", "gelop", "huiz"],
        ),
        ("no", "hestene løpende bøker", &["hest", "løp", "bøk"]),
        ("pt", "falando falar gatos", &["fal", "fal", "gat"]),
        ("ro", "cântând pisicile", &["cant", "pisic"]),
        ("ru", "бегущий бегать кошки", &["бегущ", "бега", "кошк"]),
        ("sv", "springande hästarna", &["spring", "hast"]),
        ("ta", "பள்ளிக்கூடங்கள்", &["பளளககடம"]),
        ("tr", "kitaplar evlerden", &["kitap", "ev"]),
    ];
    for (code, text, expected) in recorded {
        assert_eq!(stems(text, code, Folding::Fold), expected, "{code}");
    }
}

#[test]
fn stemming_follows_lowercasing_and_precedes_accent_folding() {
    let text = "Running CAFÉS Naïvely résumés";
    assert_eq!(
        stems(text, "en", Folding::Fold),
        ["run", "cafe", "naiv", "resume"]
    );
    assert_eq!(
        stems(text, "en", Folding::Preserve),
        ["run", "café", "naïv", "résumé"]
    );
    assert_eq!(
        stems(
            "runs running runner ran easily connection connected generously ponies",
            "en",
            Folding::Fold
        ),
        [
            "run", "run", "runner", "ran", "easili", "connect", "connect", "generous", "poni"
        ]
    );
}

#[test]
fn codes_outside_the_list_are_refused_with_tins_message() {
    for code in ["hy", "xx", "english", "EN", ""] {
        let error = code.parse::<Stemmer>().expect_err(code);
        assert_eq!(
            error.to_string(),
            format!("unknown stemmer language code: {code}")
        );
    }
    let spec = TokenizerPipelineSpec {
        case_folding: Folding::Preserve,
        stemmer: Some(Stemmer::English),
        ..TokenizerPipelineSpec::stannum_default()
    };
    assert_eq!(
        spec.compile()
            .err()
            .map(|error| error.to_string())
            .as_deref(),
        Some("stemming requires case_folding = fold")
    );
}
