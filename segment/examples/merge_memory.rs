// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Isolated-process memory probe. Prepare files in a separate process first.
//! `merge_memory prepare DIR DOCS_PER_SEGMENT TOKENS_PER_DOC VOCABULARY`
//! `merge_memory reference|direct DIR OUTPUT`
//! Wrap the latter commands with the platform's /usr/bin/time peak-RSS option.
//! Includes file I/O, decoding and encoding; excludes PostgreSQL/WAL/locks.
use segment::Tid;
use segment::merge::{MergeInput, MergeLimits};
use segment::segment::{Segment, SegmentBuilder};
use std::collections::BTreeSet;
use std::error::Error;
use std::path::Path;
use std::time::Instant;

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() < 4 {
        return Err(
            "usage: merge_memory prepare DIR DOCS TOKENS VOCAB | reference|direct DIR OUTPUT"
                .into(),
        );
    }
    let directory = Path::new(&args[2]);
    if args[1] == "prepare" {
        if args.len() != 6 {
            return Err("prepare needs DOCS TOKENS VOCAB".into());
        }
        let docs: u32 = args[3].parse()?;
        let tokens: u32 = args[4].parse()?;
        let vocabulary: u32 = args[5].parse()?;
        if docs == 0 || docs > 100_000 || tokens == 0 || tokens > 10_000 || vocabulary == 0 {
            return Err("fixture bounds: docs 1..100000, tokens 1..10000, vocabulary > 0".into());
        }
        std::fs::create_dir(directory)?;
        for part in 0..8 {
            let mut builder = SegmentBuilder::default();
            for doc in 0..docs {
                let id = doc * 8 + part;
                let tid = Tid::new(id / 100, (id % 100 + 1) as u16)?;
                let terms = (0..tokens)
                    .map(|p| format!("term{:04}", (p + id) % vocabulary))
                    .collect::<Vec<_>>();
                builder.add_document(
                    tid,
                    terms
                        .iter()
                        .enumerate()
                        .map(|(p, t)| (t.as_str(), p as u32 + 1)),
                )?;
            }
            std::fs::write(
                directory.join(format!("input-{part}.segment")),
                builder.finish(),
            )?;
        }
        return Ok(());
    }
    let start = Instant::now();
    let out = match args[1].as_str() {
        "reference" => {
            let mut builder = SegmentBuilder::default();
            // Match production reconstruction: retain only one encoded input
            // at a time while the output builder accumulates all documents.
            for part in 0..8 {
                let bytes = std::fs::read(directory.join(format!("input-{part}.segment")))?;
                let source = Segment::parse(&bytes)?;
                for record in source.records(|_| false)? {
                    builder.add_record(&record)?;
                }
            }
            builder.finish()
        }
        "direct" => {
            let blobs = (0..8)
                .map(|part| std::fs::read(directory.join(format!("input-{part}.segment"))))
                .collect::<Result<Vec<_>, _>>()?;
            let dead = BTreeSet::new();
            let inputs = blobs
                .iter()
                .map(|bytes| MergeInput { bytes, dead: &dead })
                .collect::<Vec<_>>();
            segment::merge::merge(
                &inputs,
                MergeLimits {
                    max_inputs: 8,
                    max_input_bytes: u32::MAX as usize,
                    max_documents: u32::MAX as usize,
                    max_output_bytes: u32::MAX as usize,
                },
                || Ok(()),
            )?
        }
        _ => return Err("unknown mode".into()),
    };
    println!(
        "{} elapsed_ms={:.3} bytes={}",
        args[1],
        start.elapsed().as_secs_f64() * 1000.,
        out.len()
    );
    std::fs::write(&args[3], out)?;
    Ok(())
}
