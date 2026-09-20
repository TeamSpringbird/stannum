// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Execution POC over current encoded postings, not a PostgreSQL latency benchmark.
//! cargo run -p segment --release --example count_paths -- 12000 7
//! Arguments: heap pages, repetitions. Construction and exact-ID checks are untimed.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};
use std::hint::black_box;
use std::time::Instant;

use segment::pages::{self, Cursor as PageCursor};
use segment::postings::{Postings, PostingsBuilder};
use segment::set::{self, Cursor};
use segment::{Result, Tid};

type Inputs = Vec<Vec<Vec<u8>>>;
type Rows<'a> = Box<dyn Cursor + 'a>;

struct HeapUnion<'a> {
    inputs: Vec<Rows<'a>>,
    heads: BinaryHeap<Reverse<(Tid, usize)>>,
}

impl<'a> HeapUnion<'a> {
    fn new(inputs: Vec<Rows<'a>>) -> Self {
        let heads = inputs
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.current().map(|tid| Reverse((tid, i))))
            .collect();
        Self { inputs, heads }
    }
}

impl Cursor for HeapUnion<'_> {
    fn seek(&mut self, target: Tid) -> Result<()> {
        self.heads.clear();
        for (i, input) in self.inputs.iter_mut().enumerate() {
            input.seek(target)?;
            if let Some(tid) = input.current() {
                self.heads.push(Reverse((tid, i)));
            }
        }
        Ok(())
    }
    fn current(&self) -> Option<Tid> {
        self.heads.peek().map(|Reverse((tid, _))| *tid)
    }
    fn advance(&mut self) -> Result<()> {
        let Some(current) = self.current() else {
            return Ok(());
        };
        while self
            .heads
            .peek()
            .is_some_and(|Reverse((tid, _))| *tid == current)
        {
            let Reverse((_, i)) = self.heads.pop().unwrap();
            self.inputs[i].advance()?;
            if let Some(tid) = self.inputs[i].current() {
                self.heads.push(Reverse((tid, i)));
            }
        }
        Ok(())
    }
}

fn segment_rows(inputs: &Inputs, heap_terms: bool) -> Result<Vec<Rows<'_>>> {
    inputs
        .iter()
        .map(|terms| {
            let cursors = terms
                .iter()
                .map(|bytes| Ok(Box::new(Postings::parse(bytes)?.cursor()?) as Rows<'_>))
                .collect::<Result<Vec<_>>>()?;
            Ok(if heap_terms {
                Box::new(HeapUnion::new(cursors)) as Rows<'_>
            } else {
                Box::new(set::Union::new(cursors)) as Rows<'_>
            })
        })
        .collect()
}

fn scalar_rows(inputs: &Inputs, heap_terms: bool) -> Result<HeapUnion<'_>> {
    Ok(HeapUnion::new(segment_rows(inputs, heap_terms)?))
}

fn page_rows(inputs: &Inputs) -> Result<pages::Union<Box<dyn PageCursor + '_>>> {
    let sources = inputs
        .iter()
        .map(|terms| {
            let cursors = terms
                .iter()
                .map(|bytes| Postings::parse(bytes)?.pages())
                .collect::<Result<Vec<_>>>()?;
            Ok(Box::new(pages::Union::new(cursors)) as Box<dyn PageCursor>)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(pages::Union::new(sources))
}

fn materialized(inputs: &Inputs) -> Result<Vec<Tid>> {
    let mut tids = Vec::new();
    for mut cursor in segment_rows(inputs, false)? {
        while let Some(tid) = cursor.current() {
            tids.push(tid);
            cursor.advance()?;
        }
    }
    tids.sort_unstable();
    tids.dedup();
    Ok(tids)
}

// Both counts and heap-page grouping must agree, not merely final cardinality.
fn scalar_count(mut cursor: impl Cursor) -> Result<(u64, u64)> {
    let (mut count, mut pages, mut previous) = (0, 0, None);
    while let Some(tid) = cursor.current() {
        count += 1;
        if previous != Some(tid.block) {
            pages += 1;
            previous = Some(tid.block);
        }
        cursor.advance()?;
    }
    Ok((count, pages))
}

fn measure(inputs: &Inputs, strategy: usize) -> Result<(u64, u64)> {
    match strategy {
        0 => {
            let tids = materialized(inputs)?;
            let mut pages = 0;
            let mut previous = None;
            for tid in &tids {
                if previous != Some(tid.block) {
                    pages += 1;
                    previous = Some(tid.block);
                }
            }
            Ok((tids.len() as u64, pages))
        }
        1 => scalar_count(scalar_rows(inputs, false)?),
        2 => {
            let mut cursor = page_rows(inputs)?;
            let (mut count, mut pages) = (0, 0);
            while let Some(page) = cursor.current() {
                count += u64::from(page.offsets.count());
                pages += 1;
                cursor.advance()?;
            }
            Ok((count, pages))
        }
        3 => scalar_count(scalar_rows(inputs, true)?),
        _ => unreachable!(),
    }
}

fn verify(inputs: &Inputs, expected: &[Tid]) -> Result<()> {
    assert_eq!(materialized(inputs)?, expected);
    for heap_terms in [false, true] {
        let mut cursor = scalar_rows(inputs, heap_terms)?;
        let mut actual = Vec::new();
        while let Some(tid) = cursor.current() {
            actual.push(tid);
            cursor.advance()?;
        }
        assert_eq!(actual, expected);
    }
    let mut cursor = page_rows(inputs)?;
    let mut actual = Vec::new();
    while let Some(page) = cursor.current() {
        actual.extend(page.offsets.iter().map(|offset| Tid {
            block: page.block,
            offset,
        }));
        cursor.advance()?;
    }
    assert_eq!(actual, expected);
    Ok(())
}

fn fixture(
    pages: u32,
    width: usize,
    per_page: u16,
    percent: u64,
    overlap: bool,
) -> Result<(Inputs, Vec<Tid>)> {
    let mut builders: Vec<Vec<PostingsBuilder>> = (0..9)
        .map(|_| (0..width).map(|_| PostingsBuilder::default()).collect())
        .collect();
    let mut expected = BTreeSet::new();
    let mut inserted = 0u64;
    for block in 0..pages {
        let source = ((u64::from(block) * 9) / u64::from(pages)) as usize;
        for offset in 1..=per_page {
            let tid = Tid { block, offset };
            for term in 0..width {
                let mut hash = (u64::from(block) * 317 + u64::from(offset))
                    .wrapping_add(term as u64 * 0x9e3779b9);
                hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
                hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d049bb133111eb);
                hash ^= hash >> 31;
                if hash % 100 < percent {
                    inserted += 1;
                    builders[source][term].push(tid)?;
                    if overlap && source < 8 && block % 17 == 0 {
                        builders[source + 1][term].push(tid)?;
                    }
                    expected.insert(tid);
                }
            }
        }
    }
    let nominal = u64::from(pages) * u64::from(per_page) * width as u64 * percent / 100;
    assert!(
        inserted.abs_diff(nominal) <= nominal / 4 + 64,
        "fixture frequency differs from requested density"
    );
    Ok((
        builders
            .into_iter()
            .map(|terms| terms.into_iter().map(PostingsBuilder::finish).collect())
            .collect(),
        expected.into_iter().collect(),
    ))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let pages = args.get(1).map_or(12000, |x| x.parse().expect("pages"));
    let repeats: usize = args.get(2).map_or(7, |x| x.parse().expect("repetitions"));
    assert!(pages >= 9 && repeats >= 3);
    let empty = PostingsBuilder::default().finish();
    verify(&vec![vec![empty.clone()], vec![empty]], &[])?;
    let mut edge = PostingsBuilder::default();
    let last = Tid::new(u32::MAX - 1, 291)?;
    edge.push(last)?;
    let edge = edge.finish();
    let duplicates = vec![vec![edge.clone(), edge.clone()], vec![edge]];
    verify(&duplicates, &[last])?;
    for heap_terms in [false, true] {
        let mut cursor = scalar_rows(&duplicates, heap_terms)?;
        cursor.seek(last)?;
        assert_eq!(cursor.current(), Some(last));
        cursor.advance()?;
        cursor.seek(last)?;
        assert_eq!(cursor.current(), None);
    }
    println!(
        "fixture,strategy,pages,terms,offsets_per_page,term_percent,default_path,encoded_bytes,matches,median_ms,min_ms,max_ms"
    );
    for (label, width, offsets, percent, overlap) in [
        ("rare-short", 2, 8, 1, false),
        ("sparse-wide", 24, 8, 1, false),
        ("common-short", 2, 8, 50, false),
        ("common-wide", 24, 8, 50, false),
        ("dense-wide", 24, 64, 50, false),
        ("overlapping-segments", 8, 8, 30, true),
    ] {
        let (inputs, expected) = fixture(pages, width, offsets, percent, overlap)?;
        verify(&inputs, &expected)?;
        let mut prefer_pages = false;
        for bytes in inputs.iter().flatten() {
            prefer_pages |= Postings::parse(bytes)?.prefers_pages()?;
        }
        let default_path = if prefer_pages {
            "page-bitmaps"
        } else {
            "materialize-sort"
        };
        let answer = measure(&inputs, 0)?;
        let bytes: usize = inputs.iter().flatten().map(Vec::len).sum();
        let mut samples: [Vec<f64>; 4] = std::array::from_fn(|_| Vec::new());
        for repetition in 0..repeats {
            for i in 0..4 {
                let strategy = (i + repetition) % 4;
                let start = Instant::now();
                let actual = black_box(measure(black_box(&inputs), strategy)?);
                let elapsed = start.elapsed().as_secs_f64() * 1000.;
                assert_eq!(actual, answer);
                samples[strategy].push(elapsed);
            }
        }
        for (name, mut values) in [
            "materialize-sort",
            "stream-heap-segments",
            "page-bitmaps",
            "stream-heap-terms",
        ]
        .into_iter()
        .zip(samples)
        {
            values.sort_by(f64::total_cmp);
            println!(
                "{label},{name},{pages},{width},{offsets},{percent},{default_path},{bytes},{},{:.6},{:.6},{:.6}",
                expected.len(),
                values[repeats / 2],
                values[0],
                values[repeats - 1]
            );
        }
    }
    Ok(())
}
