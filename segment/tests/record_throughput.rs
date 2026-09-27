// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Throughput of grouping tokenized documents by term, as inserts and index
//! builds do, over a fixed synthetic corpus. Run with cargo test --release
//! -p segment --test record_throughput -- --ignored --nocapture
//! (STANNUM_RUNS: timed runs, default 7; the median is reported).

use std::borrow::Cow;
use std::hash::{DefaultHasher, Hasher};
use std::time::{Duration, Instant};

use segment::forward::ForwardRecord;
use segment::segment::SegmentBuilder;
use segment::tid::Tid;
use tokenizer::Tokenizer;
use tokenizer::presets::default_pipeline;

/// `count` documents of 20 to 400 words from a 50,000-word vocabulary with
/// a skew toward the first words, some capitalized, with punctuation.
fn corpus(count: usize) -> Vec<String> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let vocabulary: Vec<String> = (0..50_000)
        .map(|_| {
            let len = 2 + (next() % 10) as usize;
            (0..len)
                .map(|_| char::from(b'a' + (next() % 26) as u8))
                .collect()
        })
        .collect();
    (0..count)
        .map(|_| {
            let words = 20 + (next() % 381) as usize;
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
                text.push_str(match (r >> 44) & 31 {
                    0 => ". ",
                    1 => ", ",
                    _ => " ",
                });
            }
            text
        })
        .collect()
}

fn tokens(text: &str) -> impl Iterator<Item = (Cow<'_, str>, u32)> {
    default_pipeline()
        .tokenize(text)
        .map(|token| (token.text, token.pos))
}

fn tid(n: usize) -> Tid {
    Tid::new(n as u32 / 100 + 1, (n % 100) as u16 + 1).unwrap()
}

fn median(mut runs: Vec<Duration>) -> (Duration, Duration, Duration) {
    runs.sort();
    (runs[0], runs[runs.len() / 2], runs[runs.len() - 1])
}

fn report(what: &str, runs: Vec<Duration>, bytes: usize) {
    let (min, mid, max) = median(runs);
    let rate = |d: Duration| bytes as f64 / d.as_secs_f64() / 1e6;
    eprintln!(
        "{what}: median {:.1} ms ({:.1} MB/s), min {:.1} ms, max {:.1} ms",
        mid.as_secs_f64() * 1e3,
        rate(mid),
        min.as_secs_f64() * 1e3,
        max.as_secs_f64() * 1e3,
    );
}

#[test]
#[ignore = "manual release-mode timing probe"]
fn record_throughput() {
    let runs: usize = std::env::var("STANNUM_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7);
    let docs = corpus(50_000);
    let bytes: usize = docs.iter().map(String::len).sum();
    let token_count: usize = docs.iter().map(|doc| tokens(doc).count()).sum();
    eprintln!(
        "{} documents, {bytes} bytes, {token_count} tokens",
        docs.len()
    );

    let time = |work: &dyn Fn() -> usize| {
        let mut checksum = 0;
        let mut out = Vec::new();
        for run in 0..=runs {
            let start = Instant::now();
            checksum = std::hint::black_box(work());
            // The first run warms caches and is not timed.
            if run > 0 {
                out.push(start.elapsed());
            }
        }
        (out, checksum)
    };

    let (tokenize, _) = time(&|| docs.iter().map(|doc| tokens(doc).count()).sum());
    report("tokenize only", tokenize, bytes);

    let (insert, encoded) = time(&|| {
        let mut hasher = DefaultHasher::new();
        for (n, doc) in docs.iter().enumerate() {
            let record = ForwardRecord::from_token_stream(tid(n), tokens(doc)).unwrap();
            let mut out = Vec::new();
            record.encode(&mut out).unwrap();
            hasher.write(&out);
        }
        hasher.finish() as usize
    });
    report("forward record + encode (insert)", insert, bytes);

    let (build, built) = time(&|| {
        let mut builder = SegmentBuilder::default();
        for (n, doc) in docs.iter().enumerate() {
            builder.add_token_stream(tid(n), tokens(doc)).unwrap();
        }
        builder.document_count()
    });
    report("segment builder add (build)", build, bytes);
    let mut builder = SegmentBuilder::default();
    for (n, doc) in docs.iter().enumerate() {
        builder.add_token_stream(tid(n), tokens(doc)).unwrap();
    }
    let mut hasher = DefaultHasher::new();
    hasher.write(&builder.finish());
    // Equal across changes to grouping: the same records and segment.
    eprintln!(
        "hashes: records {encoded:016x}, segment {:016x} ({built} documents)",
        hasher.finish()
    );
}
