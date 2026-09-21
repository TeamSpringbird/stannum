// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Isolate current-codec iteration from the multi-term count_paths experiment.
//! cargo run -p segment --release --example decode_postings -- 12000 9 4
//! Arguments: populated pages, odd repetitions, passes per timed sample.
use std::hint::black_box;
use std::time::Instant;

use segment::postings::{Postings, PostingsBuilder};
use segment::set::Cursor;
use segment::{Result, Tid};

fn fixture(pages: u32, offsets: u16, stride: u32) -> Result<(Vec<u8>, Vec<Tid>)> {
    let mut builder = PostingsBuilder::default();
    let mut expected = Vec::new();
    for page in 0..pages {
        for offset in 1..=offsets {
            let tid = Tid::new(page.checked_mul(stride).expect("block overflow"), offset)?;
            builder.push(tid)?;
            expected.push(tid);
        }
    }
    Ok((builder.finish(), expected))
}

fn verify(bytes: &[u8], expected: &[Tid]) -> Result<()> {
    let postings = Postings::parse(bytes)?;
    let mut scalar = postings.cursor()?;
    for &tid in expected {
        assert_eq!(scalar.current(), Some(tid));
        scalar.advance()?;
    }
    assert_eq!(scalar.current(), None);
    let mut pages = postings.pages()?;
    let mut actual = Vec::new();
    while let Some(page) = pages.current() {
        actual.extend(page.offsets.iter().map(|offset| Tid {
            block: page.block,
            offset,
        }));
        pages.advance()?;
    }
    assert_eq!(actual, expected);
    Ok(())
}

// Scalar and expanded pages consume the same TIDs. Page-count deliberately avoids
// expansion; its count is comparable, but it performs a different amount of work.
fn consume(bytes: &[u8], mode: usize) -> Result<(u64, u64)> {
    let postings = Postings::parse(bytes)?;
    let (mut count, mut checksum) = (0, 0u64);
    if mode == 0 {
        let mut cursor = postings.cursor()?;
        while let Some(tid) = cursor.current() {
            count += 1;
            checksum = checksum.wrapping_add((u64::from(tid.block) << 16) | u64::from(tid.offset));
            cursor.advance()?;
        }
    } else {
        let mut cursor = postings.pages()?;
        while let Some(page) = cursor.current() {
            if mode == 1 {
                for offset in page.offsets.iter() {
                    count += 1;
                    checksum =
                        checksum.wrapping_add((u64::from(page.block) << 16) | u64::from(offset));
                }
            } else {
                count += u64::from(page.offsets.count());
            }
            cursor.advance()?;
        }
    }
    Ok((count, checksum))
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let pages: u32 = args.get(1).map_or(12000, |x| x.parse().expect("pages"));
    let repeats: usize = args.get(2).map_or(9, |x| x.parse().expect("repetitions"));
    let passes: usize = args.get(3).map_or(4, |x| x.parse().expect("passes"));
    assert!(pages > 0 && repeats >= 3 && repeats % 2 == 1 && passes > 0);
    verify(&PostingsBuilder::default().finish(), &[])?;
    let mut edge = PostingsBuilder::default();
    let last = Tid::new(u32::MAX - 1, 291)?;
    edge.push(last)?;
    verify(&edge.finish(), &[last])?;
    println!(
        "fixture,codec,mode,pages,offsets,stride,encoded_bytes,postings,repetitions,passes,median_ms,min_ms,max_ms,samples_ms"
    );
    for (label, offsets, stride) in [
        ("sparse-far-pages", 1, 257),
        ("sparse-contiguous-pages", 1, 1),
        ("four-per-page", 4, 1),
        ("eight-per-page", 8, 1),
        ("list-boundary", 18, 1),
        ("bitmap-boundary", 19, 1),
        ("dense", 64, 1),
        ("full-page", 291, 1),
    ] {
        let (bytes, expected) = fixture(pages, offsets, stride)?;
        verify(&bytes, &expected)?;
        let checksum = expected.iter().fold(0u64, |sum, tid| {
            sum.wrapping_add((u64::from(tid.block) << 16) | u64::from(tid.offset))
        });
        let answers = [
            (expected.len() as u64, checksum),
            (expected.len() as u64, checksum),
            (expected.len() as u64, 0),
        ];
        // Warm all three paths before rotating their order each repetition.
        for (mode, answer) in answers.iter().enumerate() {
            assert_eq!(consume(&bytes, mode)?, *answer);
        }
        let mut samples: [Vec<f64>; 3] = std::array::from_fn(|_| Vec::new());
        for repetition in 0..repeats {
            for index in 0..3 {
                let mode = (index + repetition) % 3;
                let start = Instant::now();
                let mut actual = (0, 0);
                for _ in 0..passes {
                    actual = black_box(consume(black_box(&bytes), mode)?);
                }
                let elapsed = start.elapsed().as_secs_f64() * 1000. / passes as f64;
                assert_eq!(actual, answers[mode]);
                samples[mode].push(elapsed);
            }
        }
        let codec = if bytes[0] & 1 == 0 {
            "sparse"
        } else {
            "grouped"
        };
        for (mode, mut values) in ["scalar", "page-expand", "page-count"]
            .into_iter()
            .zip(samples)
        {
            let raw = values
                .iter()
                .map(|x| format!("{x:.6}"))
                .collect::<Vec<_>>()
                .join(";");
            values.sort_by(f64::total_cmp);
            println!(
                "{label},{codec},{mode},{pages},{offsets},{stride},{},{},{repeats},{passes},{:.6},{:.6},{:.6},{raw}",
                bytes.len(),
                expected.len(),
                values[repeats / 2],
                values[0],
                values[repeats - 1]
            );
        }
    }
    Ok(())
}
