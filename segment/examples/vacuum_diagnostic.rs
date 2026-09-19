//! Isolated VACUUM codec diagnostic; excludes PostgreSQL, WAL and readers.
//! Prepare eight interleaved sources outside the timed process:
//! `vacuum_diagnostic prepare DIR DOCS_PER_SOURCE TOKENS_PER_DOC VOCABULARY`
//! Then run `vacuum_diagnostic reference|direct|validate DIR OUTPUT` with
//! `DIAG_DELETION=0..4` (quarters of each source deleted). Timings include input
//! file reads and, for direct merging, dead-set construction. Compare alternating
//! separate processes; compare complete output files, not just timings.
use segment::Tid;
use segment::merge::{MergeInput, MergeLimits};
use segment::segment::{Segment, SegmentBuilder};
use segment::set::Cursor;
use std::collections::BTreeSet;
use std::error::Error;
use std::path::Path;
use std::time::Instant;

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() < 4 {
        return Err(
            "usage: vacuum_diagnostic prepare DIR DOCS TOKENS VOCAB | reference|direct DIR OUTPUT"
                .into(),
        );
    }
    let directory = Path::new(&args[2]);
    let realistic = std::env::var("DIAG_LAYOUT").is_ok_and(|layout| layout == "sql");
    let parts: u32 = std::env::var("DIAG_PARTS")
        .unwrap_or_else(|_| "8".into())
        .parse()?;
    if parts == 0 || parts > 128 {
        return Err("DIAG_PARTS must be 1..128".into());
    }
    let rows_per_block = if realistic { 16 } else { 100 };
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
        for part in 0..parts {
            let mut builder = SegmentBuilder::default();
            for doc in 0..docs {
                let id = if realistic {
                    part * docs + doc
                } else {
                    doc * parts + part
                };
                let tid = Tid::new(id / rows_per_block, (id % rows_per_block + 1) as u16)?;
                let terms = if realistic {
                    let hash = u64::from(id).wrapping_mul(0x9e3779b97f4a7c15);
                    let mut terms = vec![format!("w{}", id % vocabulary)];
                    for p in 0..tokens {
                        terms.push(if p % 2 == 0 { "common" } else { "filler" }.into());
                    }
                    terms.push(format!("{:016x}{:016x}", hash, hash.rotate_left(17)));
                    terms
                } else {
                    (0..tokens)
                        .map(|p| format!("term{:04}", (p + id) % vocabulary))
                        .collect::<Vec<_>>()
                };
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
    let deletion: u32 = std::env::var("DIAG_DELETION")
        .unwrap_or_else(|_| "0".into())
        .parse()?;
    if deletion > 4 {
        return Err("DIAG_DELETION must be 0..4".into());
    }
    let is_dead = |tid: Tid| {
        let id = tid.block * rows_per_block + u32::from(tid.offset) - 1;
        (if realistic { id } else { id / parts }) % 4 < deletion
    };
    let start = Instant::now();
    let out = match args[1].as_str() {
        "reference" => {
            let mut builder = SegmentBuilder::default();
            // Match production reconstruction: retain only one encoded input
            // at a time while the output builder accumulates all documents.
            for part in 0..parts {
                let bytes = std::fs::read(directory.join(format!("input-{part}.segment")))?;
                let source = Segment::parse(&bytes)?;
                for record in source.records(is_dead)? {
                    builder.add_record(&record)?;
                }
            }
            builder.finish()
        }
        "validate" => {
            for part in 0..parts {
                let bytes = std::fs::read(directory.join(format!("input-{part}.segment")))?;
                assert!(segment::verify::verify_segment(&bytes).findings.is_empty());
            }
            Vec::new()
        }
        "direct" => {
            let blobs = (0..parts)
                .map(|part| std::fs::read(directory.join(format!("input-{part}.segment"))))
                .collect::<Result<Vec<_>, _>>()?;
            let dead = blobs
                .iter()
                .map(|bytes| {
                    let source = Segment::parse(bytes).unwrap();
                    let mut cursor = source.documents().unwrap();
                    let mut dead = BTreeSet::new();
                    while let Some(tid) = cursor.current() {
                        if is_dead(tid) {
                            dead.insert(tid);
                        }
                        cursor.advance().unwrap();
                    }
                    dead
                })
                .collect::<Vec<_>>();
            let inputs = blobs
                .iter()
                .zip(&dead)
                .map(|(bytes, dead)| MergeInput { bytes, dead })
                .collect::<Vec<_>>();
            segment::merge::merge(
                &inputs,
                MergeLimits {
                    max_inputs: parts as usize,
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
