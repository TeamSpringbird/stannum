// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Replays the published Wikipedia OR-count queries over the real corpus with
//! the current TID postings and with ordinal containers. Not a PostgreSQL
//! latency benchmark: no buffer manager, visibility map or executor.
//!
//! cargo run -p segment --release --example ordinal_counts -- \
//!     data.csv queries.tsv postings.cache [rows] [repetitions]
//!
//! `queries.tsv`: id, exact count, default ms, page ms, space-separated terms.
//! The first run streams the CSV once, keeping postings only for query terms,
//! and writes them to the cache. Construction and verification are untimed.

use std::collections::HashMap;
use std::fs::File;
use std::hint::black_box;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::time::Instant;

use segment::ordinals::{self, Node, Ordinals};
use segment::pages::{self, Cursor as PageCursor};
use segment::postings::{Postings, PostingsBuilder};
use segment::set::{self, Cursor};
use segment::{Result, Tid};

/// Immutable segments of the vacuumed full-corpus index on AWS.
const SEGMENTS: usize = 9;
/// Rows per heap page of the published table: 100,000 rows fill 9,849 pages.
const ROWS_PER_PAGE: u32 = 10;

struct Query {
    id: u32,
    exact: u64,
    aws_default_ms: f64,
    aws_page_ms: f64,
    terms: Vec<usize>,
    text: String,
}

fn load_queries(path: &str) -> (Vec<Query>, Vec<String>) {
    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut names = Vec::new();
    let mut queries = Vec::new();
    for line in BufReader::new(File::open(path).expect("queries")).lines() {
        let line = line.expect("query line");
        let fields: Vec<&str> = line.split('\t').collect();
        let mut terms = Vec::new();
        for term in fields[4].split(' ') {
            let id = *ids.entry(term.to_string()).or_insert_with(|| {
                names.push(term.to_string());
                names.len() - 1
            });
            // Stannum's union sees each distinct term once.
            if !terms.contains(&id) {
                terms.push(id);
            }
        }
        queries.push(Query {
            id: fields[0].parse().expect("id"),
            exact: fields[1].parse().expect("count"),
            aws_default_ms: fields[2].parse().expect("ms"),
            aws_page_ms: fields[3].parse().expect("ms"),
            terms,
            text: fields[4].to_string(),
        });
    }
    (queries, names)
}

/// Global document ordinals per query term, in CSV order.
fn scan_corpus(path: &str, names: &[String], rows: u32) -> (u32, Vec<Vec<u32>>) {
    let ids: HashMap<&[u8], usize> = names
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_bytes(), i))
        .collect();
    let mut lists: Vec<Vec<u32>> = vec![Vec::new(); names.len()];
    let mut reader = BufReader::with_capacity(1 << 22, File::open(path).expect("corpus"));
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).expect("header");
    let mut document = 0u32;
    while document < rows {
        line.clear();
        if reader.read_until(b'\n', &mut line).expect("row") == 0 {
            break;
        }
        let body = line.iter().position(|b| *b == b',').map_or(0, |at| at + 1);
        for token in line[body..]
            .split(|b| !(b.is_ascii_alphanumeric() || *b >= 0x80))
            .filter(|t| !t.is_empty())
        {
            if let Some(term) = ids.get(token) {
                let list = &mut lists[*term];
                if list.last() != Some(&document) {
                    list.push(document);
                }
            }
        }
        document += 1;
        if document.is_multiple_of(500_000) {
            eprintln!("scanned {document} rows");
        }
    }
    (document, lists)
}

fn write_cache(path: &str, documents: u32, names: &[String], lists: &[Vec<u32>]) {
    let mut out = BufWriter::new(File::create(path).expect("cache"));
    out.write_all(&documents.to_le_bytes()).unwrap();
    out.write_all(&(names.len() as u32).to_le_bytes()).unwrap();
    for (name, list) in names.iter().zip(lists) {
        out.write_all(&(name.len() as u32).to_le_bytes()).unwrap();
        out.write_all(name.as_bytes()).unwrap();
        out.write_all(&(list.len() as u32).to_le_bytes()).unwrap();
        for ordinal in list {
            out.write_all(&ordinal.to_le_bytes()).unwrap();
        }
    }
}

fn read_cache(path: &str, names: &[String]) -> Option<(u32, Vec<Vec<u32>>)> {
    let mut bytes = Vec::new();
    File::open(path).ok()?.read_to_end(&mut bytes).ok()?;
    let mut at = 0;
    let next = |at: &mut usize| {
        let v = u32::from_le_bytes(bytes[*at..*at + 4].try_into().unwrap());
        *at += 4;
        v
    };
    let documents = next(&mut at);
    if next(&mut at) as usize != names.len() {
        return None;
    }
    let mut lists = Vec::new();
    for name in names {
        let len = next(&mut at) as usize;
        if &bytes[at..at + len] != name.as_bytes() {
            return None;
        }
        at += len;
        let count = next(&mut at) as usize;
        lists.push((0..count).map(|_| next(&mut at)).collect());
    }
    Some((documents, lists))
}

/// One segment's encodings of every query term.
struct Segment {
    tids: Vec<Vec<u8>>,
    ordinals: Vec<Vec<u8>>,
}

fn build(documents: u32, lists: &[Vec<u32>]) -> Result<Vec<Segment>> {
    // Whole heap pages per segment, so no page spans two segments.
    let pages = documents.div_ceil(ROWS_PER_PAGE);
    let per_segment = pages.div_ceil(SEGMENTS as u32) * ROWS_PER_PAGE;
    let mut segments = Vec::new();
    for s in 0..SEGMENTS as u32 {
        let (first, last) = (s * per_segment, ((s + 1) * per_segment).min(documents));
        let mut segment = Segment {
            tids: Vec::new(),
            ordinals: Vec::new(),
        };
        for list in lists {
            let from = list.partition_point(|o| *o < first);
            let to = list.partition_point(|o| *o < last);
            let mut builder = PostingsBuilder::default();
            let mut local = Vec::with_capacity(to - from);
            for ordinal in &list[from..to] {
                builder.push(Tid {
                    block: ordinal / ROWS_PER_PAGE,
                    offset: (ordinal % ROWS_PER_PAGE) as u16 + 1,
                })?;
                local.push(ordinal - first);
            }
            segment.tids.push(builder.finish());
            segment.ordinals.push(ordinals::encode(&local));
        }
        segments.push(segment);
    }
    Ok(segments)
}

/// The production scalar count: collect every segment's union, sort, deduplicate.
fn materialize(segments: &[Segment], terms: &[usize]) -> Result<u64> {
    let mut tids = Vec::new();
    for segment in segments {
        let cursors = terms
            .iter()
            .map(|t| Ok(Box::new(Postings::parse(&segment.tids[*t])?.cursor()?) as Box<dyn Cursor>))
            .collect::<Result<Vec<_>>>()?;
        let mut cursor = set::Union::new(cursors);
        while let Some(tid) = cursor.current() {
            tids.push(tid);
            cursor.advance()?;
        }
    }
    tids.sort_unstable();
    tids.dedup();
    Ok(tids.len() as u64)
}

/// The production page count: offset masks per heap page, unioned across segments.
fn page_masks(segments: &[Segment], terms: &[usize]) -> Result<u64> {
    let sources = segments
        .iter()
        .map(|segment| {
            let cursors = terms
                .iter()
                .map(|t| Postings::parse(&segment.tids[*t])?.pages())
                .collect::<Result<Vec<_>>>()?;
            Ok(Box::new(pages::Union::new(cursors)) as Box<dyn PageCursor>)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut cursor = pages::Union::new(sources);
    let mut count = 0;
    while let Some(page) = cursor.current() {
        count += u64::from(page.offsets.count());
        cursor.advance()?;
    }
    Ok(count)
}

/// Fold each segment's ordinal streams a chunk at a time and count members
/// that are not dead. `visible` stands in for the visibility map: one bit per
/// heap page, read a word at a time; a clear bit would send that page's
/// members to the heap.
fn fold(segments: &[Segment], terms: &[usize], dead: &[Vec<u32>], visible: &[u64]) -> Result<u64> {
    let heap_checks: u32 = visible.iter().map(|word| (!word).count_ones()).sum();
    assert_eq!(heap_checks, 0, "the clean fixture has no page to recheck");
    let node = Node::Or((0..terms.len()).map(Node::Term).collect());
    let mut count = 0;
    for (segment, dead) in segments.iter().zip(dead) {
        let streams = terms
            .iter()
            .map(|t| {
                let stream = Ordinals::parse(&segment.ordinals[*t])?;
                Ok((stream.count() > 0).then_some(stream))
            })
            .collect::<Result<Vec<_>>>()?;
        ordinals::for_each_chunk(&node, &streams, |key, words, members| {
            let low = u32::from(key) << 16;
            let from = dead.partition_point(|ordinal| *ordinal < low);
            let gone = dead[from..]
                .iter()
                .take_while(|ordinal| **ordinal < low.saturating_add(ordinals::CHUNK))
                .filter(|ordinal| {
                    let bit = (**ordinal - low) as usize;
                    words[bit / 64] >> (bit % 64) & 1 == 1
                })
                .count();
            count += u64::from(members) - gone as u64;
            Ok(())
        })?;
    }
    Ok(count)
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    sorted[((sorted.len() as f64 * q).ceil() as usize).clamp(1, sorted.len()) - 1]
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (csv, tsv, cache) = (&args[1], &args[2], &args[3]);
    let rows: u32 = args.get(4).map_or(u32::MAX, |x| x.parse().expect("rows"));
    let repeats: usize = args.get(5).map_or(5, |x| x.parse().expect("repetitions"));
    let (queries, names) = load_queries(tsv);
    let (documents, lists) = read_cache(cache, &names).unwrap_or_else(|| {
        let scanned = scan_corpus(csv, &names, rows);
        write_cache(cache, scanned.0, &names, &scanned.1);
        scanned
    });
    let start = Instant::now();
    let segments = build(documents, &lists)?;
    let tid_bytes: usize = segments.iter().flat_map(|s| &s.tids).map(Vec::len).sum();
    let ordinal_bytes: usize = segments
        .iter()
        .flat_map(|s| &s.ordinals)
        .map(Vec::len)
        .sum();
    eprintln!(
        "{documents} documents, {} terms, {SEGMENTS} segments, built in {:.1}s; TID postings {tid_bytes} bytes, ordinal containers {ordinal_bytes} bytes",
        names.len(),
        start.elapsed().as_secs_f64()
    );
    let dead: Vec<Vec<u32>> = segments.iter().map(|_| Vec::new()).collect();
    let visible = vec![!0u64; (documents.div_ceil(ROWS_PER_PAGE) as usize).div_ceil(64)];

    println!(
        "id\tterms\texact\tcount\tagrees\taws_default_ms\taws_page_ms\tmaterialize_ms\tpage_ms\tfold_ms\ttext"
    );
    let (mut disagreements, mut totals, mut folds) = (0, [0f64; 5], Vec::new());
    for query in &queries {
        let expected = materialize(&segments, &query.terms)?;
        assert_eq!(page_masks(&segments, &query.terms)?, expected);
        assert_eq!(fold(&segments, &query.terms, &dead, &visible)?, expected);
        let agrees = expected == query.exact || documents < 5_000_000;
        disagreements += usize::from(!agrees);
        let mut samples: [Vec<f64>; 3] = Default::default();
        for repetition in 0..repeats {
            for i in 0..3 {
                let strategy = (i + repetition) % 3;
                // The slow paths need fewer samples to be stable.
                if strategy < 2 && repetition >= 3 {
                    continue;
                }
                let start = Instant::now();
                let count = black_box(match strategy {
                    0 => materialize(black_box(&segments), &query.terms)?,
                    1 => page_masks(black_box(&segments), &query.terms)?,
                    _ => fold(black_box(&segments), &query.terms, &dead, &visible)?,
                });
                samples[strategy].push(start.elapsed().as_secs_f64() * 1000.);
                assert_eq!(count, expected);
            }
        }
        let [a, b, c] = samples.each_mut().map(|s| median(s));
        for (total, value) in
            totals
                .iter_mut()
                .zip([query.aws_default_ms, query.aws_page_ms, a, b, c])
        {
            *total += value;
        }
        folds.push(c);
        println!(
            "{}\t{}\t{}\t{expected}\t{agrees}\t{:.3}\t{:.3}\t{a:.3}\t{b:.3}\t{c:.4}\t{}",
            query.id,
            query.terms.len(),
            query.exact,
            query.aws_default_ms,
            query.aws_page_ms,
            query.text
        );
    }
    // Back-to-back repetitions keep a query's streams in cache. A pass over
    // every query in turn, as a server sees them, reads them from memory.
    let mut passes = Vec::new();
    for _ in 0..repeats {
        let start = Instant::now();
        for query in &queries {
            black_box(fold(black_box(&segments), &query.terms, &dead, &visible)?);
        }
        passes.push(start.elapsed().as_secs_f64() * 1000. / queries.len() as f64);
    }
    eprintln!(
        "ordinal fold, queries in turn (ms per query): median pass {:.3}",
        median(&mut passes)
    );
    folds.sort_by(f64::total_cmp);
    eprintln!(
        "summed medians over {} queries (ms): AWS default {:.0}, AWS page {:.0}, materialize {:.0}, page masks {:.0}, ordinal fold {:.1}",
        queries.len(),
        totals[0],
        totals[1],
        totals[2],
        totals[3],
        totals[4]
    );
    eprintln!(
        "ordinal fold per query (ms): mean {:.3}, p50 {:.3}, p95 {:.3}, p99 {:.3}, max {:.3}",
        totals[4] / queries.len() as f64,
        percentile(&folds, 0.5),
        percentile(&folds, 0.95),
        percentile(&folds, 0.99),
        folds[folds.len() - 1]
    );
    eprintln!("counts differing from the AWS exact counts: {disagreements}");
    Ok(())
}
