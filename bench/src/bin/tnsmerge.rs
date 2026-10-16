// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Merges a dump's `TNS1` segments into fewer, as a background merge
//! would, to weigh what the segment count costs a query.
//!
//! ```text
//! cargo run -p bench --release --bin tnsmerge -- --in DUMP_DIR --out OUT_DIR \
//!     --segments K [--jobs N] [--reuse] [--check] [--plan]
//! ```
//!
//! The segments, in directory order (by generation, as a tiered merge
//! policy sees them: a build's segments overlap in ctid), are cut into `K`
//! runs of consecutive segments holding about as many documents each;
//! each run is merged with `merge::merge_afresh` (every group's containers
//! encoded afresh from its postings, as a build of the same documents
//! encodes them; `--reuse`: `merge::merge`, which copies the containers of
//! groups one input holds alone) under `Options::default()`, the
//! extension's encoding. Dead lists are applied: the merged segments have
//! none. `--check` verifies each output with `verify_segment`. `--plan`
//! prints the runs and each segment's ctid range, and merges nothing.
//!
//! The output's manifest copies the source's settings; each merged segment
//! takes the largest generation of its run. `ids.tsv` is linked to the
//! source's.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use segment::Tid;
use segment::dead::DeadDocs;
use segment::tinshape::merge::{Input, merge, merge_afresh};
use segment::tinshape::postings::Options;
use segment::tinshape::segment::Segment;
use segment::tinshape::verify::verify_segment;

struct Args {
    input: PathBuf,
    output: PathBuf,
    segments: usize,
    jobs: usize,
    reuse: bool,
    check: bool,
    plan: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: tnsmerge --in DIR --out DIR --segments K [--jobs N] [--reuse] [--check] [--plan]"
    );
    std::process::exit(2)
}

fn args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        input: PathBuf::new(),
        output: PathBuf::new(),
        segments: 0,
        jobs: 1,
        reuse: false,
        check: false,
        plan: false,
    };
    let value = |it: &mut std::iter::Skip<std::env::Args>| it.next().unwrap_or_else(|| usage());
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--in" => a.input = value(&mut it).into(),
            "--out" => a.output = value(&mut it).into(),
            "--segments" => a.segments = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--jobs" => a.jobs = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--reuse" => a.reuse = true,
            "--check" => a.check = true,
            "--plan" => a.plan = true,
            _ => usage(),
        }
    }
    if a.input.as_os_str().is_empty()
        || (a.output.as_os_str().is_empty() && !a.plan)
        || a.segments == 0
        || a.jobs == 0
    {
        usage();
    }
    a
}

/// A source segment from the manifest.
struct Source {
    generation: u32,
    documents: u64,
    name: String,
    dead: Option<String>,
    first: Tid,
    last: Tid,
}

fn dead_docs(dir: &Path, source: &Source, documents: u32) -> Result<DeadDocs, String> {
    let bytes = match &source.dead {
        Some(file) => std::fs::read(dir.join(file)).map_err(|e| format!("{file}: {e}"))?,
        None => segment::ordinals::encode(&[]),
    };
    DeadDocs::decode(&bytes, documents).map_err(|e| e.to_string())
}

/// Cuts `sources` (in directory order) into `k` runs of consecutive segments,
/// each closing once it holds its share of the documents.
fn runs(sources: &[Source], k: usize) -> Vec<Vec<usize>> {
    let total: u64 = sources.iter().map(|s| s.documents).sum();
    let mut out: Vec<Vec<usize>> = vec![Vec::new()];
    let mut held = 0u64;
    for (i, s) in sources.iter().enumerate() {
        let left = sources.len() - i;
        let runs_left = k - out.len();
        // Close the run at hand when adding this segment would overshoot
        // its share by more than leaving it short does, or when every
        // remaining run needs a segment of its own.
        let target = total * out.len() as u64 / k as u64;
        let current = out.last().expect("a run");
        if !current.is_empty()
            && runs_left > 0
            && (left <= runs_left || held + s.documents / 2 > target)
        {
            out.push(Vec::new());
        }
        out.last_mut().expect("a run").push(i);
        held += s.documents;
    }
    out
}

fn merge_run(
    args: &Args,
    sources: &[Source],
    run: &[usize],
) -> Result<(u32, String, String), String> {
    let start = Instant::now();
    let blobs: Vec<Vec<u8>> = run
        .iter()
        .map(|&i| std::fs::read(args.input.join(&sources[i].name)).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    let read = start.elapsed().as_secs_f64();
    let deads: Vec<DeadDocs> = run
        .iter()
        .zip(&blobs)
        .map(|(&i, blob)| {
            let documents = Segment::parse(blob).map_err(|e| e.to_string())?.documents;
            dead_docs(&args.input, &sources[i], documents)
        })
        .collect::<Result<_, _>>()?;
    let inputs: Vec<Input<'_>> = blobs
        .iter()
        .zip(&deads)
        .map(|(bytes, dead)| Input { bytes, dead })
        .collect();
    let merged = if args.reuse {
        merge(&inputs, Options::default(), || Ok(()))
    } else {
        merge_afresh(&inputs, Options::default(), || Ok(()))
    }
    .map_err(|e| e.to_string())?
    .ok_or("no live document")?;
    let merging = start.elapsed().as_secs_f64() - read;
    drop(inputs);
    let source_bytes: u64 = blobs.iter().map(|b| b.len() as u64).sum();
    drop(blobs);
    let generation = run
        .iter()
        .map(|&i| sources[i].generation)
        .max()
        .expect("a run");
    let name = format!("gen{generation}.segment");
    let mut line = format!(
        "{name} <- {}: {} documents, {} terms, {} postings, {} reused; {source_bytes} -> {} bytes ({:+.3}%); read {read:.1} s, merge {merging:.1} s",
        run.iter()
            .map(|&i| sources[i].name.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        merged.documents,
        merged.terms,
        merged.postings,
        merged.reused,
        merged.blob.len(),
        100.0 * (merged.blob.len() as f64 / source_bytes as f64 - 1.0),
    );
    if args.check {
        let t = Instant::now();
        let clean = verify_segment(&merged.blob).is_clean();
        line += &format!("; verifies {clean} ({:.1} s)", t.elapsed().as_secs_f64());
        if !clean {
            return Err(line);
        }
    }
    std::fs::write(args.output.join(&name), &merged.blob).map_err(|e| e.to_string())?;
    line += &format!("; total {:.1} s", start.elapsed().as_secs_f64());
    let manifest = format!("segment\t{generation}\t{}\t{name}\t-", merged.documents);
    Ok((generation, manifest, line))
}

fn main() {
    let args = args();
    let manifest = std::fs::read_to_string(args.input.join("manifest.tsv")).unwrap_or_else(|e| {
        eprintln!("manifest: {e}");
        std::process::exit(1)
    });
    let mut header = Vec::new();
    let mut sources = Vec::new();
    for line in manifest.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let ["segment", generation, documents, name, dead] = f.as_slice() else {
            header.push(line.to_string());
            continue;
        };
        let blob = std::fs::read(args.input.join(name)).unwrap_or_else(|e| {
            eprintln!("{name}: {e}");
            std::process::exit(1)
        });
        let segment = Segment::parse(&blob).unwrap_or_else(|e| {
            eprintln!("{name}: {e}");
            std::process::exit(1)
        });
        let tids = segment.docs.tids();
        sources.push(Source {
            generation: generation.parse().unwrap_or_else(|_| usage()),
            documents: documents.parse().unwrap_or_else(|_| usage()),
            name: name.to_string(),
            dead: (*dead != "-").then(|| dead.to_string()),
            first: *tids.first().expect("a document"),
            last: *tids.last().expect("a document"),
        });
    }
    for s in &sources {
        let overlaps = sources
            .iter()
            .filter(|o| o.name != s.name && o.first <= s.last && s.first <= o.last)
            .count();
        println!(
            "{}: gen {} {} documents, ctid ({},{})..({},{}), overlapping {overlaps} others",
            s.name,
            s.generation,
            s.documents,
            s.first.block,
            s.first.offset,
            s.last.block,
            s.last.offset,
        );
    }
    let runs = runs(&sources, args.segments.min(sources.len()));
    for run in &runs {
        println!(
            "run of {} documents: {}",
            run.iter().map(|&i| sources[i].documents).sum::<u64>(),
            run.iter()
                .map(|&i| sources[i].name.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    if args.plan {
        return;
    }
    std::fs::create_dir_all(&args.output).unwrap_or_else(|e| {
        eprintln!("{}: {e}", args.output.display());
        std::process::exit(1)
    });
    let queue = Mutex::new(runs.iter().collect::<Vec<_>>().into_iter());
    let lines = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..args.jobs {
            scope.spawn(|| {
                loop {
                    let Some(run) = queue.lock().expect("queue").next() else {
                        return;
                    };
                    match merge_run(&args, &sources, run) {
                        Ok((generation, entry, line)) => {
                            println!("{line}");
                            lines.lock().expect("lines").push((generation, entry));
                        }
                        Err(e) => {
                            eprintln!("merge failed: {e}");
                            std::process::exit(1)
                        }
                    }
                }
            });
        }
    });
    // Directory order: by generation, as the source lists its segments.
    let mut entries = lines.into_inner().expect("lines");
    entries.sort();
    let mut out = header.join("\n");
    for (_, entry) in entries {
        out += "\n";
        out += &entry;
    }
    out += "\n";
    std::fs::write(args.output.join("manifest.tsv"), out).unwrap_or_else(|e| {
        eprintln!("manifest: {e}");
        std::process::exit(1)
    });
    let ids = args.input.join("ids.tsv");
    let link = args.output.join("ids.tsv");
    if ids.exists() && !link.exists() {
        let target = std::fs::canonicalize(&ids).unwrap_or(ids);
        let _ = std::os::unix::fs::symlink(target, link);
    }
    println!("MERGE-DONE");
}
