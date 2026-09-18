//! Timing probe for the scorer's per-row lookup sequence; run with
//! `cargo test -p segment --release --test scoring_cost -- --ignored --nocapture`.
use segment::Tid;
use segment::segment::{Segment, SegmentBuilder};
use std::time::Instant;

#[test]
#[ignore]
fn per_row_lookup_cost() {
    let mut builder = SegmentBuilder::default();
    let docs = 10_000u32;
    for n in 1..=docs {
        let mut text = String::from("common ");
        for k in 0..(8 + n % 7) {
            text.push_str(if k % 3 == 0 { "filler " } else { "pad " });
        }
        if n % 100 == 0 {
            text.push_str("rare ");
        }
        let tokens: Vec<(&str, u32)> = text
            .split_whitespace()
            .enumerate()
            .map(|(i, w)| (w, i as u32 + 1))
            .collect();
        builder
            .add_document(Tid::new(n / 60, (n % 60 + 1) as u16).unwrap(), tokens)
            .unwrap();
    }
    let bytes = builder.finish();
    let segment = Segment::parse(&bytes).unwrap();
    let terms = ["common", "rare"];
    let start = Instant::now();
    let mut documents = segment.documents().unwrap();
    let mut readers: Vec<_> = terms
        .iter()
        .map(|t| {
            let term = segment.term(t).unwrap().unwrap();
            (term.cursor().unwrap(), term.payload().unwrap().cursor())
        })
        .collect();
    let lengths = segment.lengths();
    let mut positions = Vec::new();
    let mut total = 0u64;
    for n in 1..=docs {
        let tid = Tid::new(n / 60, (n % 60 + 1) as u16).unwrap();
        let ordinal = documents.rank(tid).unwrap().unwrap();
        total += u64::from(lengths.get(ordinal).unwrap());
        for (postings, payload) in readers.iter_mut() {
            if let Some(p) = postings.rank(tid).unwrap() {
                payload.seek(p).unwrap();
                positions.clear();
                total += u64::from(payload.next_into(&mut positions).unwrap());
            }
        }
    }
    let elapsed = start.elapsed();
    eprintln!(
        "{} rows in {:?} = {:.0} ns/row (checksum {total})",
        docs,
        elapsed,
        elapsed.as_nanos() as f64 / f64::from(docs)
    );
    // Same, but a fresh cursor per row as the first implementation did.
    let start = Instant::now();
    for n in 1..=docs {
        let tid = Tid::new(n / 60, (n % 60 + 1) as u16).unwrap();
        let mut documents = segment.documents().unwrap();
        documents.rank(tid).unwrap().unwrap();
        for t in terms {
            let term = segment.term(t).unwrap().unwrap();
            let mut c = term.cursor().unwrap();
            if let Some(p) = c.rank(tid).unwrap() {
                term.payload().unwrap().get(p).unwrap();
            }
        }
    }
    eprintln!(
        "fresh cursors: {:.0} ns/row",
        start.elapsed().as_nanos() as f64 / f64::from(docs)
    );
}
