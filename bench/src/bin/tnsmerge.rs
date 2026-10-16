// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Merges a dump's `TNS1` segments into fewer, as a background merge
//! would, to weigh what the segment count costs a query.
//!
//! ```text
//! cargo run -p bench --release --bin tnsmerge -- --in DUMP_DIR --out OUT_DIR \
//!     --segments K [--jobs N] [--reuse] [--check] [--plan] [--stream]
//! cargo run -p bench --release --bin tnsmerge -- --in DUMP_DIR --verify
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
//! `--stream` merges as the extension does (`merge::merge_streaming`):
//! inputs read through their files a range at a time, what is built term
//! by term spilled to files beside the output, and the output written as it
//! is laid out, so memory stays a few bytes per document rather than the
//! inputs' and the output's bytes. `--check` then reads each output back
//! whole. `--verify` checks every segment of `--in` and merges nothing.
//!
//! The output's manifest copies the source's settings; each merged segment
//! takes the largest generation of its run. `ids.tsv` is linked to the
//! source's.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use segment::Tid;
use segment::dead::DeadDocs;
use segment::tinshape::merge::{
    Input, SourceInput, merge, merge_afresh, merge_streaming, merge_streaming_afresh,
};
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
    stream: bool,
    verify: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: tnsmerge --in DIR --out DIR --segments K [--jobs N] [--reuse] [--check] [--plan] [--stream]\n       tnsmerge --in DIR --verify"
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
        stream: false,
        verify: false,
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
            "--stream" => a.stream = true,
            "--verify" => a.verify = true,
            _ => usage(),
        }
    }
    if a.verify && !a.input.as_os_str().is_empty() {
        return a;
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

/// A segment file read a range at a time.
struct FileSource {
    file: std::fs::File,
    len: u64,
}

impl FileSource {
    fn open(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let len = file.metadata().map_err(|e| e.to_string())?.len();
        Ok(Self { file, len })
    }

    /// The segment's documents, from its header and document set alone.
    fn documents(&self) -> segment::Result<segment::tinshape::docs::DocSet> {
        use segment::source::Source as _;
        use segment::tinshape::index::{HEADER_MAX, Header};
        let head = self.read(0, (HEADER_MAX as u64).min(self.len) as usize)?;
        let header = Header::parse(&head, self.len)?;
        let area = segment::tinshape::segment::Area::DocSet as usize;
        segment::tinshape::docs::DocSet::decode(&self.read(header.at(area), header.len(area))?)
    }
}

impl segment::source::Source for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read(&self, offset: u64, len: usize) -> segment::Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let mut out = vec![0u8; len];
        self.file
            .read_exact_at(&mut out, offset)
            .map_err(|_| segment::Error::Truncated)?;
        Ok(out)
    }
}

/// A spill in three files beside the output, removed when dropped.
struct FileSpill {
    paths: [PathBuf; 3],
    files: [Option<std::io::BufWriter<std::fs::File>>; 3],
}

impl FileSpill {
    fn new(dir: &Path, name: &str) -> Self {
        Self {
            paths: [0, 1, 2].map(|i| dir.join(format!(".{name}.spill{i}"))),
            files: [None, None, None],
        }
    }

    fn slot(area: segment::tinshape::segment::Area) -> usize {
        use segment::tinshape::segment::Area;
        match area {
            Area::TermMap => 0,
            Area::Postings => 1,
            _ => 2,
        }
    }
}

impl segment::tinshape::segment::Spill for FileSpill {
    fn append(
        &mut self,
        area: segment::tinshape::segment::Area,
        bytes: &[u8],
    ) -> segment::Result<()> {
        use std::io::Write;
        let i = Self::slot(area);
        if self.files[i].is_none() {
            let file =
                std::fs::File::create(&self.paths[i]).map_err(|_| segment::Error::Truncated)?;
            self.files[i] = Some(std::io::BufWriter::with_capacity(1 << 20, file));
        }
        let file = self.files[i].as_mut().expect("just opened");
        file.write_all(bytes).map_err(|_| segment::Error::Truncated)
    }

    fn replay(
        &mut self,
        area: segment::tinshape::segment::Area,
        out: &mut dyn FnMut(&[u8]) -> segment::Result<()>,
    ) -> segment::Result<()> {
        use std::io::{Read, Write};
        let i = Self::slot(area);
        let Some(mut file) = self.files[i].take() else {
            return Ok(());
        };
        file.flush().map_err(|_| segment::Error::Truncated)?;
        drop(file);
        let mut file =
            std::fs::File::open(&self.paths[i]).map_err(|_| segment::Error::Truncated)?;
        let mut chunk = vec![0u8; 1 << 20];
        loop {
            let read = file
                .read(&mut chunk)
                .map_err(|_| segment::Error::Truncated)?;
            if read == 0 {
                return Ok(());
            }
            out(&chunk[..read])?;
        }
    }
}

impl Drop for FileSpill {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A file written front to back.
struct FileSink(std::io::BufWriter<std::fs::File>);

impl segment::tinshape::segment::Sink for FileSink {
    fn write(&mut self, bytes: &[u8]) -> segment::Result<()> {
        use std::io::Write;
        self.0
            .write_all(bytes)
            .map_err(|_| segment::Error::Truncated)
    }
}

/// [`merge_run`] streaming (see the module).
fn merge_run_streaming(
    args: &Args,
    sources: &[Source],
    run: &[usize],
) -> Result<(u32, String, String), String> {
    use std::io::Write;
    let start = Instant::now();
    let files: Vec<FileSource> = run
        .iter()
        .map(|&i| FileSource::open(&args.input.join(&sources[i].name)))
        .collect::<Result<_, _>>()?;
    let deads: Vec<DeadDocs> = run
        .iter()
        .zip(&files)
        .map(|(&i, file)| {
            let documents = file
                .documents()
                .map_err(|e| e.to_string())?
                .geometry
                .documents;
            dead_docs(&args.input, &sources[i], documents)
        })
        .collect::<Result<_, _>>()?;
    let inputs: Vec<SourceInput<'_>> = files
        .iter()
        .zip(&deads)
        .map(|(source, dead)| SourceInput { source, dead })
        .collect();
    let generation = run
        .iter()
        .map(|&i| sources[i].generation)
        .max()
        .expect("a run");
    let name = format!("gen{generation}.segment");
    let path = args.output.join(&name);
    let mut spill = FileSpill::new(&args.output, &name);
    let mut sink = FileSink(std::io::BufWriter::with_capacity(
        1 << 20,
        std::fs::File::create(&path).map_err(|e| e.to_string())?,
    ));
    let merged = if args.reuse {
        merge_streaming(
            &inputs,
            Options::default(),
            &mut spill,
            &mut sink,
            || Ok(()),
        )
    } else {
        merge_streaming_afresh(
            &inputs,
            Options::default(),
            &mut spill,
            &mut sink,
            || Ok(()),
        )
    }
    .map_err(|e| e.to_string())?
    .ok_or("no live document")?;
    sink.0.flush().map_err(|e| e.to_string())?;
    drop(sink);
    drop(spill);
    let merging = start.elapsed().as_secs_f64();
    let source_bytes: u64 = files.iter().map(|f| f.len).sum();
    let mut line = format!(
        "{name} <- {}: {} documents, {} terms, {} postings, {} reused; {source_bytes} -> {} bytes ({:+.3}%); streamed, merge {merging:.1} s",
        run.iter()
            .map(|&i| sources[i].name.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        merged.documents,
        merged.terms,
        merged.postings,
        merged.reused,
        merged.bytes,
        100.0 * (merged.bytes as f64 / source_bytes as f64 - 1.0),
    );
    if args.check {
        let t = Instant::now();
        let blob = std::fs::read(&path).map_err(|e| e.to_string())?;
        let clean = verify_segment(&blob).is_clean();
        line += &format!("; verifies {clean} ({:.1} s)", t.elapsed().as_secs_f64());
        if !clean {
            return Err(line);
        }
    }
    line += &format!("; total {:.1} s", start.elapsed().as_secs_f64());
    let manifest = format!("segment\t{generation}\t{}\t{name}\t-", merged.documents);
    Ok((generation, manifest, line))
}

fn merge_run(
    args: &Args,
    sources: &[Source],
    run: &[usize],
) -> Result<(u32, String, String), String> {
    if args.stream {
        return merge_run_streaming(args, sources, run);
    }
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
        if args.verify {
            let blob = std::fs::read(args.input.join(name)).unwrap_or_else(|e| {
                eprintln!("{name}: {e}");
                std::process::exit(1)
            });
            let t = Instant::now();
            let clean = verify_segment(&blob).is_clean();
            println!(
                "{name}: {} bytes, verifies {clean} ({:.1} s)",
                blob.len(),
                t.elapsed().as_secs_f64()
            );
            if !clean {
                std::process::exit(1);
            }
            continue;
        }
        // The documents alone, read through the file: a segment can be
        // larger than the memory a streamed merge needs.
        let tids = FileSource::open(&args.input.join(name))
            .and_then(|file| file.documents().map_err(|e| e.to_string()))
            .unwrap_or_else(|e| {
                eprintln!("{name}: {e}");
                std::process::exit(1)
            })
            .tids();
        sources.push(Source {
            generation: generation.parse().unwrap_or_else(|_| usage()),
            documents: documents.parse().unwrap_or_else(|_| usage()),
            name: name.to_string(),
            dead: (*dead != "-").then(|| dead.to_string()),
            first: *tids.first().expect("a document"),
            last: *tids.last().expect("a document"),
        });
    }
    if args.verify {
        println!("VERIFY-DONE");
        return;
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
