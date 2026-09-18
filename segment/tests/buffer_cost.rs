//! Cost of turning the write buffer's forward stream into a queryable index.
//! Run with LEAD_DOCS=/path/to/documents.csv cargo test --release -p segment
//! --test buffer_cost -- --ignored --nocapture

use std::time::Instant;

use segment::forward::{ForwardRecord, records};
use segment::index::MutableIndex;
use segment::tid::Tid;

#[test]
#[ignore]
fn buffer_index_cost() {
    let path = std::env::var("LEAD_DOCS").expect("LEAD_DOCS");
    let count: usize = std::env::var("LEAD_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(593);
    let text = std::fs::read_to_string(path).unwrap();
    let mut stream = Vec::new();
    let mut n = 0u32;
    let mut tokens_total = 0usize;
    for line in text.lines().take(count) {
        let body = line.split_once(',').map(|(_, b)| b).unwrap_or(line);
        let tokens: Vec<(&str, u32)> = body
            .split(' ')
            .filter(|t| !t.is_empty())
            .enumerate()
            .map(|(i, t)| (t, i as u32 + 1))
            .collect();
        tokens_total += tokens.len();
        let tid = Tid::new(n / 100 + 1, (n % 100) as u16 + 1).unwrap();
        n += 1;
        let record = ForwardRecord::from_tokens(tid, tokens).unwrap();
        record.encode(&mut stream).unwrap();
    }
    println!("{n} docs, {tokens_total} tokens, {} bytes", stream.len());
    let start = Instant::now();
    let decoded: Vec<ForwardRecord> = records(&stream).map(|r| r.unwrap()).collect();
    let decode = start.elapsed();
    drop(decoded);
    let start = Instant::now();
    let index = MutableIndex::default();
    let mut at = 0;
    while at < stream.len() {
        at += index.add_encoded(&stream[at..]).unwrap();
    }
    let add = start.elapsed();
    println!("decode alone {decode:?}, add_encoded (decode + index) {add:?}");
}
