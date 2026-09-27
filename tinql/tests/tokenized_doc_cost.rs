// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Cost of tokenizing short documents for exact evaluation, as a `==>`
//! recheck does for every row it reads. Compares the shipped
//! `tokenize_doc` with copies of two layouts: one heap string per token
//! plus a map of positions (the shipped layout without its interrupt
//! check), and distinct terms with per-token term numbers. Run with cargo
//! test --release -p tinql --test tokenized_doc_cost -- --ignored
//! --nocapture (STANNUM_RUNS: timed runs, default 9; the median is
//! reported).

use std::borrow::Cow;
use std::hint::black_box;
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;
use tinql::runtime::tokenize_doc;
use tokenizer::Tokenizer;
use tokenizer::presets::default_pipeline;

/// A string per token and a map of positions by term.
struct PerToken {
    tokens: Vec<String>,
    token_positions: Vec<u32>,
    positions: FxHashMap<String, Vec<u32>>,
}

fn per_token(text: &str) -> PerToken {
    let positioned: Vec<(String, u32)> = default_pipeline()
        .tokenize(text)
        .map(|token| (token.text.into_owned(), token.pos))
        .collect();
    let mut doc = PerToken {
        tokens: Vec::with_capacity(positioned.len()),
        token_positions: Vec::with_capacity(positioned.len()),
        positions: FxHashMap::default(),
    };
    for (token, pos) in positioned {
        doc.positions.entry(token.clone()).or_default().push(pos);
        doc.tokens.push(token);
        doc.token_positions.push(pos);
    }
    doc
}

/// Distinct terms with their positions, and each token as a term number.
struct Interned {
    terms: Vec<(String, Vec<u32>)>,
    ids: FxHashMap<String, u32>,
    token_terms: Vec<u32>,
    token_positions: Vec<u32>,
}

fn interned(text: &str) -> Interned {
    let mut doc = Interned {
        terms: Vec::new(),
        ids: FxHashMap::default(),
        token_terms: Vec::new(),
        token_positions: Vec::new(),
    };
    for token in default_pipeline().tokenize(text) {
        let text: Cow<'_, str> = token.text;
        let id = match doc.ids.get(&*text) {
            Some(id) => *id,
            None => {
                let id = doc.terms.len() as u32;
                let text = text.into_owned();
                doc.ids.insert(text.clone(), id);
                doc.terms.push((text, Vec::new()));
                id
            }
        };
        doc.terms[id as usize].1.push(token.pos);
        doc.token_terms.push(id);
        doc.token_positions.push(token.pos);
    }
    doc
}

/// `count` documents of 20 to 200 words from a 20,000-word vocabulary with
/// a skew toward the first words, some capitalized.
fn corpus(count: usize) -> Vec<String> {
    let mut state = 0x853c_49e6_748f_ea9bu64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let vocabulary: Vec<String> = (0..20_000)
        .map(|_| {
            let len = 2 + (next() % 10) as usize;
            (0..len)
                .map(|_| char::from(b'a' + (next() % 26) as u8))
                .collect()
        })
        .collect();
    (0..count)
        .map(|_| {
            let words = 20 + (next() % 181) as usize;
            let mut text = String::new();
            for _ in 0..words {
                let r = next();
                let index =
                    (((r & 0xffff) * ((r >> 16) & 0xffff)) as usize * vocabulary.len()) >> 32;
                let word = &vocabulary[index];
                if (r >> 40) & 15 == 0 {
                    let mut chars = word.chars();
                    text.push(chars.next().unwrap().to_ascii_uppercase());
                    text.push_str(chars.as_str());
                } else {
                    text.push_str(word);
                }
                text.push(' ');
            }
            text
        })
        .collect()
}

/// Builds one document's evaluation form, returning a size to keep it live.
type Layout = dyn Fn(&str) -> usize;

#[test]
#[ignore = "manual release-mode timing probe"]
fn tokenized_doc_cost() {
    let runs: usize = std::env::var("STANNUM_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9);
    let docs = corpus(100_000);
    let tokens: usize = docs
        .iter()
        .map(|doc| tokenize_doc(doc, default_pipeline()).len())
        .sum();
    eprintln!("{} documents, {tokens} tokens", docs.len());
    let layouts: [(&str, &Layout); 3] = [
        ("shipped tokenize_doc", &|doc| {
            black_box(tokenize_doc(doc, default_pipeline())).len()
        }),
        ("per-token strings, no check", &|doc| {
            let doc = black_box(per_token(doc));
            doc.tokens.len() + doc.positions.len() + doc.token_positions.len()
        }),
        ("interned terms", &|doc| {
            let doc = black_box(interned(doc));
            doc.token_terms.len() + doc.ids.len() + doc.terms.len() + doc.token_positions.len()
        }),
    ];
    let mut times: Vec<Vec<Duration>> = vec![Vec::new(); layouts.len()];
    // Interleaved, so drift affects every layout alike; run 0 warms up.
    for run in 0..=runs {
        for (i, (_, build)) in layouts.iter().enumerate() {
            let start = Instant::now();
            let mut total = 0;
            for doc in &docs {
                total += build(doc);
            }
            black_box(total);
            if run > 0 {
                times[i].push(start.elapsed());
            }
        }
    }
    for ((name, _), mut runs) in layouts.iter().zip(times) {
        runs.sort();
        let ns = |d: Duration| d.as_nanos() as f64 / docs.len() as f64;
        eprintln!(
            "{name}: median {:.0} ns/document (min {:.0}, max {:.0})",
            ns(runs[runs.len() / 2]),
            ns(runs[0]),
            ns(runs[runs.len() - 1]),
        );
    }
}
