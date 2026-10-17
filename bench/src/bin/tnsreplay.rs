// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Replays a trace's ranked top k over a dump of an index whose segments
//! are already in TIN's shape (`TNS1`), as the extension's ranked scan
//! walks them natively, and reports per style the latency, the walk's
//! counters and the pages each query touches by TIN's areas.
//!
//! ```text
//! cargo run -p bench --release --bin tnsreplay -- --dump DIR --trace trace.tsv \
//!     [--expect pg.tsv] [--style S] [--k 10] [--repeat 3] [--limit N] \
//!     [--keep] [--threads N] [--seconds S] [--per-query FILE] [--out FILE] \
//!     [--per-segment FILE] [--order O] [--seed] [--warm FILE] [--timed-walk] \
//!     [--plan STYLE:MODE:N,...] [--perf-ctl CTL,ACK] [--cpus SETS]
//! ```
//!
//! Each segment is read into memory (`--mmap`: mapped from its file, so
//! only the pages the queries touch take memory, as a replay of a few
//! forms over a dump larger than memory needs) and kept across queries with its
//! decoded footers; after each query its parsed records are forgotten, as
//! the extension's in-place reads forget them when a span closes (`--keep`
//! keeps them, as a backend copying the blob would). The answer is the
//! engine's `Scorer::top_k` over native sources (the extension's path);
//! the counters and pages come from a second pass of the same walk, one
//! segment at a time into shared rows, which must give the same rows.
//! `--threads N --seconds S` runs N threads, each with its own segments,
//! over the trace for S seconds and reports queries per second.
//!
//! The second pass measures what the threshold costs across segments:
//! `--per-segment FILE` writes, per query and segment in the order walked,
//! the threshold the segment started from, the final one and the walk's
//! counters there; `--order` walks the segments in another order than the
//! engine's (largest first); `--seed` starts it from the final rows (found
//! again, they are not kept twice); `--timed-walk` times that pass, without
//! pages, instead of the extension's path. `--warm FILE` starts a
//! disjunction's walk from a warm-up instead (within the timed walk): the
//! `k` best of a ranked AND of its two scoring terms of highest bound over
//! the first segment, scored as OR rows and kept as seeded rows are; FILE
//! gets per query the threshold they give and the final one;
//! `--warm-untimed` takes it before the timed walks (a free warm-up).
//!
//! `--plan conjunction:score:300,disjunction:full:200,...` replays several
//! slices of the trace after one load (the first N queries of a style,
//! ranked as `score` or as `full`), each reported as its own style (a full
//! one as `STYLE+full`). A slice `STYLE:MODE:@FILE` takes instead the
//! queries of that style named in FILE's first column (a trace's lines, in
//! FILE's order), reported as `STYLE[+full]@STEM` (FILE's stem): a fixed
//! set of forms, such as the benchmark's slowest (`benchmarks/tail/`). The timed passes' instructions, cycles and branch
//! misses are counted in the process (Linux: `perf_event_open`, user space
//! only; macOS: `proc_pid_rusage`, no branch misses); `--perf-ctl CTL,ACK`
//! turns an outside `perf stat` or `perf record` (`-D -1 --control
//! fifo:CTL,ACK`) on around each timed pass only.
//!
//! `--cpus 2/2,10` measures what busy hyperthreads cost (a server's
//! backends share each core with another): for each set of CPUs (sets
//! split by `/`), one thread per CPU, pinned to it, each with its own
//! segments' readers over the one load, replays every slice of the trace
//! (or of `--plan`) at once, slice by slice: each thread the whole slice
//! from its own offset (thread `t` of `T` at query `t·n/T`), each query
//! an untimed pass, then `--repeat` timed ones (`--forget` before each).
//! A thread done with its slice keeps replaying it, uncounted, until every
//! thread is done, so every counted query ran beside busy siblings. Per
//! slice and set it reports the instructions, cycles and branch misses per
//! query (counted per thread), each thread's cycles per query, the core's
//! IPC (the threads' instructions over a thread's cycles), the set's
//! queries per gigacycle (each thread's queries over its own cycles,
//! summed: a core's throughput at a fixed clock) and the answers' digest
//! (the same for every set and build of a correct walk); then, per query, a
//! `pinq` line with its instructions and cycles per timed pass (the mean
//! over the set's threads), from which a tail of the slowest forms is
//! compared (`benchmarks/aws/htpair_summary.py`). `--per-query` gives the
//! same counts per query, per timed pass, in its `ins` and `cyc` columns. Sibling CPUs
//! are in `/sys/devices/system/cpu/cpuN/topology/thread_siblings_list`;
//! pinning and per-thread counts are Linux only (macOS counts the process).
//!
//! The dump comes from `script/dump-segments.py`; `--expect` takes
//! `script/replay-oracle.py postgres` output (`--ranked-only` suffices).
//! Every row is visible: there is no heap.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use bench::tinshape::Pages;
use engine::bm25::{DenseRatio, ScoreStopWords, TermSetEdit};
use engine::terms::ScoringPolicy;
use engine::tinshape::{self as tin, NoTouch, Part};
use engine::walk::{NativeSegment, Scorer, Source, TopRows, Visibility, WalkConfig};
use rustc_hash::{FxHashMap, FxHashSet};
use segment::Tid;
use segment::dead::DeadDocs;
use segment::index::Index;
use segment::tinshape::segment::Segment;
use tinql::runtime::{parse_tinql_to_query, parse_tinql_to_scoring_query};

struct Args {
    dump: PathBuf,
    trace: PathBuf,
    expect: Option<PathBuf>,
    style: Option<String>,
    k: usize,
    repeat: usize,
    limit: Option<usize>,
    keep: bool,
    /// Before every pass of a query (the answer's and each timed one),
    /// every segment forgets the records and footers it parsed: each pass
    /// pays for parsing its terms' metadata, as a backend does for a term
    /// its memos do not hold (see [`Reader::forget`]).
    forget: bool,
    /// Rank as `stannum.full_score(ctid)` does: no dense term elided.
    full: bool,
    memory: bool,
    seed: bool,
    /// `--seed-rank R`: seeded instead with the rows ranked `R-k+1..=R`
    /// (a true lower bound of the final threshold, below it from `R >= 2k`),
    /// to measure what a walk started from a lower threshold costs.
    seed_rank: Option<usize>,
    /// `--prepass SPEC`: some groups of one segment walked before the
    /// others (see [`Prepass`]).
    prepass: Option<Prepass>,
    /// `--tail-first P`: the extension path's tail-first walk
    /// (`WalkConfig::tail_first_permille`); the engine's default when unset.
    tail_first: u32,
    pin_ns: u64,
    /// Pages a walk keeps pinned between groups (`LazyBlob::set_keep`);
    /// the blob's default when unset.
    keep_pins: Option<usize>,
    threads: usize,
    seconds: f64,
    per_query: Option<PathBuf>,
    /// Per query and segment, what the instrumented pass did there and the
    /// threshold it started from.
    per_segment: Option<PathBuf>,
    /// `--rows-at FILE`: per answer row, its segment and group.
    rows_at: Option<PathBuf>,
    /// `--group-stats FILE`: per segment and group, its documents, their
    /// mean length and how many lie in each length class.
    group_stats: Option<PathBuf>,
    /// The order the instrumented pass walks the segments in (see
    /// [`SegmentOrder`]).
    order: SegmentOrder,
    /// Time the instrumented pass's walk (a segment at a time in `order`,
    /// seeded with `--seed`) rather than the extension's path.
    timed_walk: bool,
    /// Before a disjunction's walks, a ranked AND of its two highest-bound
    /// scoring terms over the first segment, its rows scored as OR rows and
    /// kept as `--seed` keeps the final ones (`--warm FILE` writes, per
    /// query, the threshold they give against the final one).
    warm: Option<PathBuf>,
    /// `--warm-untimed`: the warm-up taken before the timed walks, which
    /// time only the walk it starts (what a free warm-up would save).
    warm_untimed: bool,
    out: Option<PathBuf>,
    /// `--plan`: (style, full, queries) slices replayed after one load.
    plan: Vec<(String, bool, PlanSlice)>,
    /// `--perf-ctl CTL,ACK`: perf's control and ack FIFOs; counting is
    /// enabled around each timed pass only (not the load, the answer's
    /// pass or the instrumented one).
    perf_ctl: Option<(PathBuf, PathBuf)>,
    /// `--cpus`: sets of CPUs, each measured with one pinned thread per CPU.
    cpus: Vec<Vec<usize>>,
    /// `--mmap`: segments mapped from their files rather than read.
    mmap: bool,
}

/// `--prepass WHICH:KIND:N`: before the walks over the segments, the walk
/// over some groups of the largest (`big`) or the smallest (`small`)
/// segment; that segment's own walk then takes the other groups. KIND:
/// `stride` every Nth group, `head` / `tail` the first / last N per mille
/// of its groups. Exact: the two walks cover each group once.
/// Ascending ranges `[start, end)` of a segment's group indexes.
type Ranges = Vec<(u32, u32)>;

#[derive(Clone, Copy)]
struct Prepass {
    big: bool,
    kind: PrepassKind,
    n: u32,
}

#[derive(Clone, Copy)]
enum PrepassKind {
    Stride,
    Head,
    Tail,
}

impl Prepass {
    fn parse(spec: &str) -> Option<Self> {
        let f: Vec<&str> = spec.split(':').collect();
        let [which, kind, n] = f[..] else { return None };
        let big = match which {
            "big" => true,
            "small" => false,
            _ => return None,
        };
        let kind = match kind {
            "stride" => PrepassKind::Stride,
            "head" => PrepassKind::Head,
            "tail" => PrepassKind::Tail,
            _ => return None,
        };
        let n: u32 = n.parse().ok()?;
        (n > 0).then_some(Self { big, kind, n })
    }

    /// The sampled ranges of a segment of `groups` groups, and the others.
    fn split(&self, groups: u32) -> (Ranges, Ranges) {
        let sample: Vec<(u32, u32)> = match self.kind {
            PrepassKind::Stride => (0..groups)
                .step_by(self.n as usize)
                .map(|g| (g, g + 1))
                .collect(),
            PrepassKind::Head => {
                let m = (u64::from(groups) * u64::from(self.n)).div_ceil(1000) as u32;
                vec![(0, m.clamp(1, groups))]
            }
            PrepassKind::Tail => {
                let m = (u64::from(groups) * u64::from(self.n)).div_ceil(1000) as u32;
                vec![(groups - m.clamp(1, groups), groups)]
            }
        };
        let mut rest = Vec::new();
        let mut at = 0;
        for &(start, end) in &sample {
            if start > at {
                rest.push((at, start));
            }
            at = end;
        }
        if at < groups {
            rest.push((at, groups));
        }
        (sample, rest)
    }
}

/// Which of a style's queries a `--plan` slice replays.
enum PlanSlice {
    /// The first N.
    First(usize),
    /// Those named (a trace file's first column), in this order, reported
    /// with the label's suffix.
    Named(String, Vec<String>),
}

/// Orders of the instrumented pass's segments, to measure what an order
/// costs: the engine's own (`engine`), by generation (`gen`, `gen-desc`),
/// smallest first (`docs-asc`), or by how many of the final rows each
/// segment holds, most first (`final`, a ceiling no walk can know).
#[derive(Clone, Copy, PartialEq)]
enum SegmentOrder {
    Engine,
    Generation,
    GenerationDesc,
    DocsAscending,
    /// The smallest segment, then the engine's order.
    SmallFirst,
    /// By the scoring terms' summed record bounds in the segment.
    Bound,
    /// Fewest estimated matches first: the segment's documents times each
    /// scoring term's share of them, as if independent (`matches`); or the
    /// lowest such share alone, whatever the segment's size (`density`).
    Matches,
    Density,
    Final,
}

fn usage() -> ! {
    eprintln!(
        "usage: tnsreplay --dump DIR --trace FILE [--expect FILE] [--style S] [--k N] \
         [--repeat N] [--limit N] [--memory [--keep]] [--forget] [--full] [--pin-ns N] [--keep-pins N] [--threads N --seconds S] [--per-query FILE] [--per-segment FILE] \
         [--order engine|gen|gen-desc|docs-asc|small-first|bound|matches|density|final] [--seed | --seed-rank R] [--prepass big|small:stride|head|tail:N] [--tail-first P] [--warm FILE [--warm-untimed]] [--timed-walk] [--out FILE] \
         [--plan STYLE:score|full:N|full:@FILE,...] [--perf-ctl CTL,ACK] [--cpus CPU,CPU/CPU,...] [--mmap]"
    );
    std::process::exit(2)
}

fn args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        dump: PathBuf::new(),
        trace: PathBuf::new(),
        expect: None,
        style: None,
        k: 10,
        repeat: 3,
        limit: None,
        keep: false,
        forget: false,
        full: false,
        memory: false,
        seed: false,
        seed_rank: None,
        prepass: None,
        tail_first: engine::walk::TAIL_FIRST_PERMILLE,
        pin_ns: 0,
        keep_pins: None,
        threads: 0,
        seconds: 20.0,
        per_query: None,
        per_segment: None,
        rows_at: None,
        group_stats: None,
        order: SegmentOrder::Engine,
        timed_walk: false,
        warm: None,
        warm_untimed: false,
        out: None,
        plan: Vec::new(),
        perf_ctl: None,
        cpus: Vec::new(),
        mmap: false,
    };
    let value = |it: &mut std::iter::Skip<std::env::Args>| it.next().unwrap_or_else(|| usage());
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dump" => a.dump = value(&mut it).into(),
            "--trace" => a.trace = value(&mut it).into(),
            "--expect" => a.expect = Some(value(&mut it).into()),
            "--style" => a.style = Some(value(&mut it)),
            "--k" => a.k = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--repeat" => a.repeat = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--limit" => a.limit = Some(value(&mut it).parse().unwrap_or_else(|_| usage())),
            "--keep" => a.keep = true,
            "--forget" => a.forget = true,
            "--full" => a.full = true,
            "--memory" => a.memory = true,
            "--mmap" => a.mmap = true,
            "--seed" => a.seed = true,
            "--tail-first" => a.tail_first = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--prepass" => {
                a.prepass = Some(Prepass::parse(&value(&mut it)).unwrap_or_else(|| usage()))
            }
            "--seed-rank" => {
                a.seed_rank = Some(value(&mut it).parse().unwrap_or_else(|_| usage()));
            }
            "--pin-ns" => a.pin_ns = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--keep-pins" => {
                a.keep_pins = Some(value(&mut it).parse().unwrap_or_else(|_| usage()));
            }
            "--threads" => a.threads = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--seconds" => a.seconds = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--per-query" => a.per_query = Some(value(&mut it).into()),
            "--timed-walk" => a.timed_walk = true,
            "--warm" => a.warm = Some(value(&mut it).into()),
            "--warm-untimed" => a.warm_untimed = true,
            "--per-segment" => a.per_segment = Some(value(&mut it).into()),
            "--rows-at" => a.rows_at = Some(value(&mut it).into()),
            "--group-stats" => a.group_stats = Some(value(&mut it).into()),
            "--order" => {
                a.order = match value(&mut it).as_str() {
                    "engine" => SegmentOrder::Engine,
                    "gen" => SegmentOrder::Generation,
                    "gen-desc" => SegmentOrder::GenerationDesc,
                    "docs-asc" => SegmentOrder::DocsAscending,
                    "small-first" => SegmentOrder::SmallFirst,
                    "bound" => SegmentOrder::Bound,
                    "matches" => SegmentOrder::Matches,
                    "density" => SegmentOrder::Density,
                    "final" => SegmentOrder::Final,
                    _ => usage(),
                }
            }
            "--out" => a.out = Some(value(&mut it).into()),
            "--plan" => {
                for slice in value(&mut it).split(',') {
                    let f: Vec<&str> = slice.split(':').collect();
                    let [style, mode, n] = f[..] else { usage() };
                    let full = match mode {
                        "score" => false,
                        "full" => true,
                        _ => usage(),
                    };
                    let pick = match n.strip_prefix('@') {
                        Some(path) => {
                            let path = std::path::Path::new(path);
                            let names = std::fs::read_to_string(path)
                                .unwrap_or_else(|e| {
                                    eprintln!("--plan {}: {e}", path.display());
                                    std::process::exit(2)
                                })
                                .lines()
                                .filter_map(|l| l.split('\t').next())
                                .filter(|n| !n.is_empty())
                                .map(str::to_owned)
                                .collect();
                            let stem = path.file_stem().map_or_else(
                                || "named".to_owned(),
                                |s| s.to_string_lossy().into_owned(),
                            );
                            PlanSlice::Named(stem, names)
                        }
                        None => PlanSlice::First(n.parse().unwrap_or_else(|_| usage())),
                    };
                    a.plan.push((style.to_owned(), full, pick));
                }
            }
            "--cpus" => {
                for set in value(&mut it).split('/') {
                    let cpus: Vec<usize> = set
                        .split(',')
                        .map(|c| c.parse().unwrap_or_else(|_| usage()))
                        .collect();
                    a.cpus.push(cpus);
                }
            }
            "--perf-ctl" => {
                let v = value(&mut it);
                let (ctl, ack) = v.split_once(',').unwrap_or_else(|| usage());
                a.perf_ctl = Some((ctl.into(), ack.into()));
            }
            _ => usage(),
        }
    }
    if a.dump.as_os_str().is_empty() || a.trace.as_os_str().is_empty() {
        usage();
    }
    a
}

/// The system allocator, counting on this thread the calls that ask it
/// for memory (an allocation, a growth) and the bytes asked: what a query
/// costs in `malloc`, `realloc` and the copies a growth makes.
struct Counting;

#[derive(Clone, Copy, Default)]
struct Allocs {
    allocs: u64,
    reallocs: u64,
    bytes: u64,
}

thread_local! {
    static ALLOCS: std::cell::Cell<Allocs> = const {
        std::cell::Cell::new(Allocs { allocs: 0, reallocs: 0, bytes: 0 })
    };
}

impl Allocs {
    fn now() -> Self {
        ALLOCS.with(std::cell::Cell::get)
    }

    fn since(self, from: Self) -> Self {
        Self {
            allocs: self.allocs - from.allocs,
            reallocs: self.reallocs - from.reallocs,
            bytes: self.bytes - from.bytes,
        }
    }

    fn count(realloc: bool, size: usize) {
        // `try_with`: a thread's own teardown frees after its locals are gone.
        let _ = ALLOCS.try_with(|c| {
            let mut a = c.get();
            if realloc {
                a.reallocs += 1;
            } else {
                a.allocs += 1;
            }
            a.bytes += size as u64;
            c.set(a);
        });
    }
}

// SAFETY: every call is the system allocator's, with its arguments.
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        Allocs::count(false, layout.size());
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        Allocs::count(false, layout.size());
        unsafe { std::alloc::System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new: usize) -> *mut u8 {
        Allocs::count(true, new);
        unsafe { std::alloc::System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// A dumped segment: its generation, its blob and its dead list.
type Dumped = (u32, &'static [u8], Option<Vec<u8>>);

/// The dump's manifest and blobs, loaded once and shared by every thread.
struct Loaded {
    spec: [u8; engine::spec::SPEC_BYTES],
    params: engine::bm25::Bm25Params,
    stop_words: Option<String>,
    segments: Vec<Dumped>,
}

/// The bytes of `path`, mapped read-only for the process's life (Unix).
fn map_file(path: &std::path::Path) -> Result<&'static [u8], String> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        unsafe extern "C" {
            fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
            -> *mut u8;
        }
        const PROT_READ: i32 = 1;
        const MAP_PRIVATE: i32 = 2;
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let len = file.metadata().map_err(|e| e.to_string())?.len() as usize;
        if len == 0 {
            return Ok(&[]);
        }
        // SAFETY: a fresh read-only private mapping of the whole file, never
        // unmapped; the dump's files are not written while it is replayed.
        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ,
                MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr as isize == -1 {
            return Err(format!("{}: mmap failed", path.display()));
        }
        // SAFETY: the mapping above, `len` bytes, valid for the process's life.
        Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err("--mmap needs a Unix".to_owned())
    }
}

fn load(dir: &std::path::Path, mapped: bool) -> Result<Loaded, String> {
    let manifest = std::fs::read_to_string(dir.join("manifest.tsv")).map_err(|e| e.to_string())?;
    let mut spec = None;
    let mut params = engine::bm25::Bm25Params::default_bm25();
    let mut stop_words = None;
    let mut segments = Vec::new();
    for line in manifest.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["spec", hex] => {
                let bytes: Vec<u8> = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
                    .collect();
                spec = Some(bytes.try_into().map_err(|_| "spec length")?);
            }
            ["k1", v] => params.k1 = v.parse::<f64>().map_err(|e| e.to_string())? as f32,
            ["b", v] => params.b = v.parse::<f64>().map_err(|e| e.to_string())? as f32,
            ["score_stop_words", v] => stop_words = Some((*v).to_owned()),
            ["buffer_docs", v] if *v != "0" => {
                eprintln!("warning: the write buffer holds {v} documents, not dumped");
            }
            ["segment", generation, _docs, blob, dead] => {
                let start = Instant::now();
                let bytes: &'static [u8] = if mapped {
                    map_file(&dir.join(blob))?
                } else {
                    Vec::leak(std::fs::read(dir.join(blob)).map_err(|e| format!("{blob}: {e}"))?)
                };
                eprintln!(
                    "{blob}: {} bytes read in {:.1} s",
                    bytes.len(),
                    start.elapsed().as_secs_f64()
                );
                let dead = match *dead {
                    "-" => None,
                    name => Some(std::fs::read(dir.join(name)).map_err(|e| e.to_string())?),
                };
                segments.push((generation.parse().map_err(|_| "generation")?, bytes, dead));
            }
            _ => {}
        }
    }
    Ok(Loaded {
        spec: spec.ok_or("no spec")?,
        params,
        stop_words,
        segments,
    })
}

/// Bytes of a run page after its header, special area and chain link, as
/// the extension's run pages hold them (`storage::layout::CHAIN_CAPACITY`).
const PAGE: usize = 8192 - 24 - 8 - 4;

/// Pages one span may hold pinned, as the extension's `PINNED_LIMIT`.
const PINNED_LIMIT: usize = 8192;

thread_local! {
    /// Pages pinned on this thread (a pin per page per span).
    static PINS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// PostgreSQL's private buffer refcounts as this thread's pins would
    /// use them (see [`RefCounts`]).
    static REFS: std::cell::RefCell<RefCounts> = std::cell::RefCell::new(RefCounts::default());
}

/// Entries of PostgreSQL's private refcount array (`bufmgr.c`,
/// `REFCOUNT_ARRAY_ENTRIES`): a backend's pins past these go to a hash
/// table.
const REFCOUNT_ARRAY_ENTRIES: usize = 8;

/// A model of `bufmgr.c`'s private refcount bookkeeping (PostgreSQL 17
/// and 18): an array of eight entries, past it a hash table; a new pin
/// takes a free array entry, or moves one (round robin) into the hash
/// table; looking up a buffer not in the array searches the hash table
/// once anything overflowed; releasing an entry held in the hash table
/// removes it there. It counts the hash table operations the pins make,
/// and the most pages held at once.
#[derive(Default)]
struct RefCounts {
    array: [Option<u64>; REFCOUNT_ARRAY_ENTRIES],
    reserved: Option<usize>,
    clock: usize,
    hash: FxHashSet<u64>,
    held: u64,
    peak: u64,
    hash_ops: u64,
}

impl RefCounts {
    /// `ReservePrivateRefCountEntry`.
    fn reserve(&mut self) {
        if self.reserved.is_some() {
            return;
        }
        if let Some(free) = self.array.iter().position(Option::is_none) {
            self.reserved = Some(free);
            return;
        }
        let victim = self.clock % REFCOUNT_ARRAY_ENTRIES;
        self.clock += 1;
        let moved = self.array[victim].take().expect("a full array");
        self.hash.insert(moved);
        self.hash_ops += 1;
        self.reserved = Some(victim);
    }

    /// `GetPrivateRefCountEntry` without moving: whether `page` is held.
    fn lookup(&mut self, page: u64) -> bool {
        if self.array.contains(&Some(page)) {
            return true;
        }
        if self.hash.is_empty() {
            return false;
        }
        self.hash_ops += 1;
        self.hash.contains(&page)
    }

    /// A first pin of `page` (`PinBuffer` of a buffer not pinned).
    fn pin(&mut self, page: u64) {
        self.reserve();
        let found = self.lookup(page);
        debug_assert!(!found, "a page pinned once per span");
        let slot = self.reserved.take().expect("reserved above");
        self.array[slot] = Some(page);
        self.held += 1;
        self.peak = self.peak.max(self.held);
    }

    /// The release of `page`'s pin (`UnpinBuffer`, then
    /// `ForgetPrivateRefCountEntry`).
    fn unpin(&mut self, page: u64) {
        self.lookup(page);
        if let Some(slot) = self.array.iter().position(|e| *e == Some(page)) {
            self.array[slot] = None;
            self.reserved = Some(slot);
        } else {
            self.hash_ops += 1;
            self.hash.remove(&page);
        }
        self.held -= 1;
    }
}

/// Page `page` of the source at `source` as one key.
fn ref_key(source: *const PinSource, page: usize) -> u64 {
    ((source as u64) << 24) ^ page as u64
}

/// A blob in memory served as the extension's run source serves a
/// segment: within a hold span, a page is "pinned" (remembered in a hash
/// map until the span closes, at `pin_ns` of simulated buffer-manager
/// work), and read in place.
struct PinSource {
    bytes: &'static [u8],
    holding: std::cell::Cell<u32>,
    pinned: std::cell::RefCell<FxHashMap<usize, ()>>,
    pin_ns: u64,
}

impl segment::source::Source for PinSource {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read(&self, offset: u64, len: usize) -> segment::Result<Vec<u8>> {
        let at = offset as usize;
        self.bytes
            .get(at..at + len)
            .map(<[u8]>::to_vec)
            .ok_or(segment::Error::Truncated)
    }

    fn hold(&self, open: bool) {
        let depth = self.holding.get();
        if open {
            self.holding.set(depth + 1);
        } else {
            self.holding.set(depth.saturating_sub(1));
            if depth == 1 {
                let me = std::ptr::from_ref(self);
                REFS.with_borrow_mut(|refs| {
                    for (page, ()) in self.pinned.borrow_mut().drain() {
                        refs.unpin(ref_key(me, page));
                    }
                });
            }
        }
    }

    fn holding(&self) -> bool {
        self.holding.get() > 0
    }

    fn page_len(&self) -> usize {
        PAGE
    }

    fn pinned_page(&self, offset: u64) -> Option<segment::Result<segment::source::HeldSpan>> {
        if self.holding.get() == 0 {
            return None;
        }
        let page = offset as usize / PAGE;
        let start = page * PAGE;
        if start >= self.bytes.len() {
            return Some(Err(segment::Error::Truncated));
        }
        let mut pinned = self.pinned.borrow_mut();
        let fresh = !pinned.contains_key(&page);
        if fresh {
            if pinned.len() >= PINNED_LIMIT {
                return None;
            }
            pinned.insert(page, ());
            PINS.set(PINS.get() + 1);
            let me = std::ptr::from_ref(self);
            REFS.with_borrow_mut(|refs| refs.pin(ref_key(me, page)));
            if self.pin_ns > 0 {
                let until = Instant::now() + std::time::Duration::from_nanos(self.pin_ns);
                while Instant::now() < until {
                    std::hint::spin_loop();
                }
            }
        }
        Some(Ok(segment::source::HeldSpan {
            start: start as u64,
            data: self.bytes[start..].as_ptr(),
            len: PAGE.min(self.bytes.len() - start),
            pinned: fresh,
        }))
    }

    fn release_page(&self, offset: u64) {
        let page = offset as usize / PAGE;
        if self.pinned.borrow_mut().remove(&page).is_some() {
            let me = std::ptr::from_ref(self);
            REFS.with_borrow_mut(|refs| refs.unpin(ref_key(me, page)));
        }
    }
}

/// An empty term map's index, as the extension assembles its segments
/// over: terms are found through the reader and remembered.
const NO_TERMS: &[u8] = &[0, 0, 0];

/// A segment as one thread reads it: its term map reader (for statistics
/// and term lookups) and the segment the native walk reads, kept across
/// queries, either over the blob in memory (`--memory`) or, as the
/// extension reads it, over a lazy blob read in place within a span per
/// query.
struct Seg {
    label: String,
    generation: u32,
    reader: segment::tinshape::index::Reader<&'static [u8]>,
    segment: Segment<'static>,
    lazy: Option<&'static segment::tinshape::blob::LazyBlob>,
    terms: std::cell::RefCell<FxHashMap<String, Option<segment::dictionary::TermEntry>>>,
    dead: DeadDocs,
    keep: bool,
}

impl Seg {
    /// Runs `f` over the segment as the extension's `native_segment` does.
    fn with<R>(
        &self,
        names: &[String],
        f: impl FnOnce(&Segment<'static>) -> segment::Result<R>,
    ) -> segment::Result<R> {
        let Some(lazy) = self.lazy else {
            let result = f(&self.segment);
            if !self.keep {
                self.segment.forget_borrowed();
            }
            return result;
        };
        for name in names {
            let known = self.terms.borrow().get(name.as_str()).copied();
            let entry = match known {
                Some(entry) => entry,
                None => {
                    let entry = self.reader.term_entry(name)?;
                    self.terms.borrow_mut().insert(name.clone(), entry);
                    entry
                }
            };
            self.segment.remember(name, entry);
        }
        lazy.open_span();
        let result = f(&self.segment);
        self.segment.forget_borrowed();
        // SAFETY: nothing `f` returned borrows the span's pages; the
        // segment forgot the records that did.
        unsafe { lazy.close_span() };
        result
    }
}

impl NativeSegment for Seg {
    fn walk(
        &self,
        names: &[String],
        _positions: bool,
        walk: &mut dyn FnMut(&Segment<'_>) -> segment::Result<()>,
    ) -> Option<segment::Result<()>> {
        Some(self.with(names, |segment| walk(segment)))
    }
}

struct Reader {
    tokenizer: tokenizer::CompiledTokenizerPipeline,
    params: engine::bm25::Bm25Params,
    stop: Option<String>,
    /// Rank as `full_score` (set per query with `--plan`).
    full: std::cell::Cell<bool>,
    prepass: Option<Prepass>,
    /// `--tail-first P` ([`WalkConfig::tail_first_permille`]).
    tail_first: u32,
    segs: Vec<Seg>,
}

impl Reader {
    fn open(loaded: &Loaded, args: &Args) -> Result<Self, String> {
        let tokenizer = engine::spec::decode_spec(&loaded.spec)
            .ok_or("spec")?
            .compile()
            .map_err(|e| e.to_string())?;
        let mut segs = Vec::new();
        for (generation, blob, dead) in &loaded.segments {
            let reader = segment::tinshape::index::Reader::new(*blob).map_err(|e| e.to_string())?;
            let (segment, lazy) = if args.memory {
                (Segment::parse(blob).map_err(|e| e.to_string())?, None)
            } else {
                let docs = reader.docs().map_err(|e| e.to_string())?;
                let documents = docs.geometry.documents;
                let ranks: Vec<u32> = match dead {
                    Some(list) => {
                        let set = DeadDocs::decode(list, documents).map_err(|e| e.to_string())?;
                        (0..documents).filter(|r| set.contains(*r)).collect()
                    }
                    None => Vec::new(),
                };
                let liveness = std::rc::Rc::new(
                    segment::tinshape::docs::Liveness::decode(
                        &segment::tinshape::docs::encode_liveness(documents, &ranks),
                        &docs,
                    )
                    .map_err(|e| e.to_string())?,
                );
                let lazy: &'static segment::tinshape::blob::LazyBlob = Box::leak(Box::new(
                    segment::tinshape::blob::LazyBlob::new(Box::new(PinSource {
                        bytes: blob,
                        holding: Default::default(),
                        pinned: Default::default(),
                        pin_ns: args.pin_ns,
                    })),
                ));
                if let Some(keep) = args.keep_pins {
                    lazy.set_keep(keep);
                }
                let mut segment = Segment::assemble(
                    lazy.bytes(),
                    segment::dictionary::DictionaryIndex::parse(NO_TERMS)
                        .map_err(|e| e.to_string())?,
                    docs,
                    liveness,
                )
                .map_err(|e| e.to_string())?;
                segment.share_footers(Default::default());
                (segment, Some(lazy))
            };
            let dead = match dead {
                Some(list) => {
                    DeadDocs::decode(list, segment.documents).map_err(|e| e.to_string())?
                }
                None => DeadDocs::default(),
            };
            segs.push(Seg {
                label: format!("segment generation {generation}"),
                generation: *generation,
                reader,
                segment,
                lazy,
                terms: Default::default(),
                dead,
                keep: args.keep,
            });
        }
        Ok(Self {
            tokenizer,
            params: loaded.params,
            stop: loaded.stop_words.clone(),
            full: std::cell::Cell::new(args.full),
            prepass: args.prepass,
            tail_first: args.tail_first,
            segs,
        })
    }

    /// Every segment forgets the records and footers it parsed (`--forget`).
    fn forget(&self) {
        for seg in &self.segs {
            seg.segment.shed(0);
        }
    }

    fn scorer(&self, text: &str) -> Result<Scorer, String> {
        let query = parse_tinql_to_query(text, &self.tokenizer).map_err(|e| e.to_string())?;
        let scoring =
            parse_tinql_to_scoring_query(text, &self.tokenizer).map_err(|e| e.to_string())?;
        let edit = TermSetEdit::from_bound_arrays(None, None).map_err(|e| e.to_string())?;
        let stop = self.stop.as_deref().and_then(ScoreStopWords::from_csv);
        let policy = ScoringPolicy {
            params: self.params,
            full: self.full.get(),
            dense: DenseRatio::new(Some(DenseRatio::DEFAULT)),
            edit: &edit,
            stop: stop.as_ref(),
            max_expansion_terms: 65_536,
        };
        let indexes: Vec<&dyn Index> = self.segs.iter().map(|s| &s.reader as &dyn Index).collect();
        let terms = engine::terms::term_scorers(&scoring, &indexes, indexes.len(), &policy)
            .map_err(|_| "scoring terms".to_owned())?;
        Ok(Scorer { terms, query })
    }

    /// The extension's path: `Scorer::top_k` over native sources.
    fn ranked(&self, scorer: &Scorer, k: usize) -> Option<Vec<(f32, Tid)>> {
        let sources: Vec<Source<'_>> = self
            .segs
            .iter()
            .map(|s| Source {
                index: &s.reader,
                label: &s.label,
                dead: &s.dead,
                key: Some((1, s.generation)),
                native: Some(s as &dyn NativeSegment),
            })
            .collect();
        let config = WalkConfig {
            warmup_chunks: 256,
            warmup_min_matches: 4.0,
            seed: None,
            tail_first_permille: self.tail_first,
        };
        let top = scorer.top_k(&sources, k, false, &config, |_| AllVisible, |_| true)?;
        if top.zero_fill || !top.native || top.ordinal {
            return None;
        }
        Some(top.rows)
    }

    /// The same walk, a segment at a time in `order`, with pages and
    /// counters, and per segment what the walk did there.
    fn instrumented(
        &self,
        scorer: &Scorer,
        k: usize,
        pages: &mut Pages,
        seed: Option<&[(f32, Tid)]>,
        order: SegmentOrder,
        answer: &[(f32, Tid)],
    ) -> Option<Instrumented> {
        let mut names = Vec::new();
        let node = tin::lower(&scorer.query, &mut names)?;
        let visit = self.schedule(self.visit(&names, scorer, order, answer));
        let mut top = TopRows::new(k, false);
        // A measurement aid: the final rows kept from the start, so the
        // walk prunes against the final threshold throughout. A walk finding
        // one of them again must not keep it twice (the bar would rise past
        // the final threshold): they are not visible to it.
        for row in seed.unwrap_or(&[]) {
            top.push(engine::walk::Ranked(row.0, row.1));
        }
        let mut visibility = Unseeded(seed.unwrap_or(&[]).iter().map(|row| row.1).collect());
        let mut total = tin::RankedAnswer::default();
        let mut work = Vec::with_capacity(visit.len());
        for (i, groups) in visit {
            let seg = &self.segs[i];
            let (bar, pages0, pins0) = (top.bar(), pages.total(), PINS.get());
            pages.touch_meta(i);
            let a = seg
                .with(&names, |segment| {
                    tin::top_k_into_groups(
                        segment,
                        &node,
                        &names,
                        &scorer.terms,
                        &mut top,
                        &mut visibility,
                        &groups,
                        &mut Offset {
                            pages: &mut *pages,
                            base: i << 40,
                        },
                    )
                })
                .ok()?;
            work.push(SegmentWork {
                generation: seg.generation,
                documents: seg.segment.documents,
                theta: bar.map(|b| b.0),
                groups: a.windows,
                pruned: a.windows_pruned,
                examined: a.candidates,
                scored: a.scored,
                pages: (pages.total() - pages0) as u64,
                pins: PINS.get() - pins0,
            });
            total.scored += a.scored;
            total.candidates += a.candidates;
            total.position_checks += a.position_checks;
            total.class_cuts += a.class_cuts;
            total.windows += a.windows;
            total.windows_pruned += a.windows_pruned;
        }
        Some((top.into_rows(), total, work))
    }

    /// The instrumented pass's walk without pages or counters, to time it
    /// (`--timed-walk`): seeded or not, in `order`.
    fn walked(
        &self,
        scorer: &Scorer,
        k: usize,
        seed: Option<&[(f32, Tid)]>,
        order: SegmentOrder,
        answer: &[(f32, Tid)],
    ) -> Option<Vec<(f32, Tid)>> {
        let mut names = Vec::new();
        let node = tin::lower(&scorer.query, &mut names)?;
        let visit = self.schedule(self.visit(&names, scorer, order, answer));
        let mut top = TopRows::new(k, false);
        for row in seed.unwrap_or(&[]) {
            top.push(engine::walk::Ranked(row.0, row.1));
        }
        let mut visibility = Unseeded(seed.unwrap_or(&[]).iter().map(|row| row.1).collect());
        for (i, groups) in visit {
            self.segs[i]
                .with(&names, |segment| {
                    tin::top_k_into_groups(
                        segment,
                        &node,
                        &names,
                        &scorer.terms,
                        &mut top,
                        &mut visibility,
                        &groups,
                        &mut NoTouch,
                    )
                })
                .ok()?;
        }
        Some(top.into_rows())
    }

    /// The warm-up rows of a disjunction (`--warm`): the `k` best of a
    /// ranked AND of its two scoring terms of highest bound (their segment's
    /// largest bucket at length 0) over the first segment `order` visits,
    /// each scored as the OR scores it (every scoring term), with what the
    /// AND did. Empty for any other shape, or with fewer than two of the
    /// terms in that segment.
    fn warm(
        &self,
        scorer: &Scorer,
        k: usize,
        order: SegmentOrder,
        answer: &[(f32, Tid)],
    ) -> Option<(Vec<(f32, Tid)>, tin::RankedAnswer)> {
        let mut names = Vec::new();
        let node = tin::lower(&scorer.query, &mut names)?;
        if !matches!(node, tin::Node::Or(_)) {
            return Some(Default::default());
        }
        let first = *self.visit(&names, scorer, order, answer).first()?;
        let seg = &self.segs[first];
        let mut weighed: Vec<(f32, usize)> = Vec::new();
        for (i, (name, term)) in scorer.terms.iter().enumerate() {
            if let Ok(Some(entry)) = seg.reader.term_entry(name)
                && let Some(bucket) = segment::tf_bucket::TfBucket::new(entry.max_tf_bucket)
            {
                weighed.push((term.bound_through(bucket, 0), i));
            }
        }
        if weighed.len() < 2 {
            return Some(Default::default());
        }
        weighed.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        let pair: Vec<(String, engine::bm25::TermScorer)> = weighed[..2]
            .iter()
            .map(|&(_, i)| scorer.terms[i].clone())
            .collect();
        let and = tin::Node::And(
            pair.iter()
                .map(|(name, _)| {
                    tin::Node::Term(names.iter().position(|n| n == name).expect("a query term"))
                })
                .collect(),
        );
        seg.with(&names, |segment| {
            let mut top = TopRows::new(k, false);
            let a = tin::top_k_into(
                segment,
                &and,
                &names,
                &pair,
                &mut top,
                &mut AllVisible,
                &mut NoTouch,
            )?;
            let mut rows = Vec::with_capacity(k);
            for (_, tid) in top.into_rows() {
                if let Some(score) = tin::score_at(segment, &scorer.terms, tid)? {
                    rows.push((score, tid));
                }
            }
            Ok((rows, a))
        })
        .ok()
    }

    /// The walks of a query: per walk, a segment and the ranges of its
    /// groups it takes (every group when empty), the prepass first.
    fn schedule(&self, visit: Vec<usize>) -> Vec<(usize, Vec<(u32, u32)>)> {
        let mut walks: Vec<(usize, Vec<(u32, u32)>)> =
            visit.iter().map(|&i| (i, Vec::new())).collect();
        let Some(p) = self.prepass else {
            return walks;
        };
        let pick = if p.big {
            visit
                .iter()
                .copied()
                .max_by_key(|&i| (self.segs[i].segment.documents, std::cmp::Reverse(i)))
        } else {
            visit
                .iter()
                .copied()
                .min_by_key(|&i| (self.segs[i].segment.documents, i))
        };
        let Some(pick) = pick else { return walks };
        let groups = self.segs[pick].segment.docs.geometry.groups.len() as u32;
        let (sample, rest) = p.split(groups);
        if rest.is_empty() {
            return walks;
        }
        for walk in &mut walks {
            if walk.0 == pick {
                walk.1 = rest.clone();
            }
        }
        walks.insert(0, (pick, sample));
        walks
    }

    /// The segments in `order`.
    fn visit(
        &self,
        names: &[String],
        scorer: &Scorer,
        order: SegmentOrder,
        answer: &[(f32, Tid)],
    ) -> Vec<usize> {
        let mut visit: Vec<usize> = (0..self.segs.len()).collect();
        visit.sort_by_key(|&i| std::cmp::Reverse(self.segs[i].segment.documents));
        match order {
            SegmentOrder::SmallFirst => {
                if let Some(smallest) = visit.pop() {
                    visit.insert(0, smallest);
                }
            }
            SegmentOrder::Bound => {
                let bound: Vec<f32> = self
                    .segs
                    .iter()
                    .map(|seg| {
                        let mut sum = 0.0_f32;
                        for (name, term) in &scorer.terms {
                            if let Ok(Some(entry)) = seg.reader.term_entry(name)
                                && let Some(bucket) =
                                    segment::tf_bucket::TfBucket::new(entry.max_tf_bucket)
                            {
                                sum += term.bound_through(bucket, 0);
                            }
                        }
                        sum
                    })
                    .collect();
                visit.sort_by(|a, b| bound[*b].total_cmp(&bound[*a]));
            }
            SegmentOrder::Matches | SegmentOrder::Density => {
                let estimate: Vec<f64> = self
                    .segs
                    .iter()
                    .map(|seg| {
                        let docs = f64::from(seg.segment.documents.max(1));
                        let mut share = 1.0_f64;
                        for (name, _) in &scorer.terms {
                            let df = match seg.reader.term_entry(name) {
                                Ok(Some(entry)) => entry.df,
                                _ => 0,
                            };
                            share *= f64::from(df) / docs;
                        }
                        if order == SegmentOrder::Matches {
                            share * docs
                        } else {
                            share
                        }
                    })
                    .collect();
                // Stable: ties keep the engine's order.
                visit.sort_by(|a, b| estimate[*a].total_cmp(&estimate[*b]));
            }
            SegmentOrder::Engine => {}
            SegmentOrder::Generation => visit.sort_by_key(|&i| self.segs[i].generation),
            SegmentOrder::GenerationDesc => {
                visit.sort_by_key(|&i| std::cmp::Reverse(self.segs[i].generation));
            }
            SegmentOrder::DocsAscending => visit.reverse(),
            SegmentOrder::Final => {
                let held: Vec<usize> = (0..self.segs.len())
                    .map(|i| {
                        self.segs[i]
                            .with(names, |segment| {
                                let mut held = 0;
                                for row in answer {
                                    held += usize::from(
                                        tin::score_at(segment, &scorer.terms, row.1)?.is_some(),
                                    );
                                }
                                Ok(held)
                            })
                            .unwrap_or(0)
                    })
                    .collect();
                // Stable: segments holding as many rows keep the engine's order.
                visit.sort_by_key(|&i| std::cmp::Reverse(held[i]));
            }
        }
        visit
    }
}

/// The instrumented pass's rows, its counters, and per segment what it did.
type Instrumented = (Vec<(f32, Tid)>, tin::RankedAnswer, Vec<SegmentWork>);

/// What the instrumented pass did in one segment: the threshold it
/// started from (`None` before `k` rows were kept), the groups it visited
/// and of those skipped by their bounds, the candidates it examined and
/// scored, the distinct pages it touched and the pages it pinned.
struct SegmentWork {
    generation: u32,
    documents: u32,
    theta: Option<f32>,
    groups: u64,
    pruned: u64,
    examined: u64,
    scored: u64,
    pages: u64,
    pins: u64,
}

/// Pages of segment `i` counted apart from another segment's.
struct Offset<'p> {
    pages: &'p mut Pages,
    base: usize,
}

impl tin::Touch for Offset<'_> {
    fn touch(&mut self, part: Part, at: usize, len: usize) {
        self.pages.touch(part, self.base + at, len);
    }
}

trait Meta {
    fn touch_meta(&mut self, i: usize);
}

impl Meta for Pages {
    fn touch_meta(&mut self, i: usize) {
        use engine::tinshape::Touch as _;
        self.touch(Part::Metadata, i << 40, 1);
    }
}

struct AllVisible;

impl Visibility for AllVisible {
    fn visible(&mut self, _: Tid) -> bool {
        true
    }
}

/// Every row but the seeded ones (see [`Reader::instrumented`]).
struct Unseeded(FxHashSet<Tid>);

impl Visibility for Unseeded {
    fn visible(&mut self, tid: Tid) -> bool {
        !self.0.contains(&tid)
    }
}

fn percentile(values: &mut [u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[((values.len() as f64 - 1.0) * p).round() as usize]
}

/// Ids of `tids` from the dump's `ids.tsv`, streamed once.
fn resolve_ids(dir: &std::path::Path, tids: &FxHashSet<Tid>) -> FxHashMap<Tid, String> {
    use std::io::BufRead as _;
    let mut out = FxHashMap::default();
    let Ok(file) = std::fs::File::open(dir.join("ids.tsv")) else {
        return out;
    };
    for line in std::io::BufReader::with_capacity(1 << 20, file).lines() {
        let Ok(line) = line else { break };
        let mut f = line.splitn(3, '\t');
        let (Some(b), Some(o), Some(id)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let tid = Tid {
            block: b.parse().unwrap_or(0),
            offset: o.parse().unwrap_or(0),
        };
        if tids.contains(&tid) {
            out.insert(tid, id.to_owned());
        }
    }
    out
}

fn answer_line(ids: &FxHashMap<Tid, String>, rows: &[(f32, Tid)]) -> String {
    let mut line = String::new();
    for (n, (score, tid)) in rows.iter().enumerate() {
        if n > 0 {
            line.push(',');
        }
        match ids.get(tid) {
            Some(id) => line.push_str(id),
            None => write!(line, "({},{})", tid.block, tid.offset).expect("a string"),
        }
        write!(line, ":{:08x}", score.to_bits()).expect("a string");
    }
    line
}

struct Row {
    name: String,
    style: String,
    ns: u64,
    rows: Vec<(f32, Tid)>,
    pages: Pages,
    answer: tin::RankedAnswer,
    pins: u64,
    /// The most pages held pinned at once in the instrumented pass, and
    /// the private refcount hash table operations its pins made.
    peak: u64,
    hash_ops: u64,
    /// The instrumented pass's reads by kind of structure.
    kinds: [segment::tinshape::blob::KindStats; segment::tinshape::blob::KINDS],
    /// What the answer's pass parsed of the terms' metadata.
    memo: segment::tinshape::segment::MemoCounts,
    /// The last timed pass's allocations: building the query's scorer,
    /// then walking it.
    scorer_allocs: Allocs,
    walk_allocs: Allocs,
    /// Instructions, cycles and branch misses of the timed passes, all of
    /// them.
    timed: [u64; 3],
    /// The instrumented pass, per segment in the order walked.
    segments: Vec<SegmentWork>,
}

fn throughput(args: &Args, loaded: &Loaded, trace: &[bench::TraceQuery]) -> Result<(), String> {
    let seconds = args.seconds;
    let total = std::sync::atomic::AtomicU64::new(0);
    let per_style = std::sync::Mutex::new(FxHashMap::<String, (u64, u64)>::default());
    let start = std::sync::Barrier::new(args.threads);
    std::thread::scope(|s| {
        for t in 0..args.threads {
            let (total, per_style, start) = (&total, &per_style, &start);
            s.spawn(move || {
                let reader = Reader::open(loaded, args).expect("open");
                // Warm: one pass over a slice of the trace.
                let n = trace.len();
                for q in trace
                    .iter()
                    .skip(t * n / args.threads)
                    .take(n / args.threads)
                {
                    if let Ok(scorer) = reader.scorer(&q.text) {
                        std::hint::black_box(reader.ranked(&scorer, args.k));
                    }
                }
                start.wait();
                let cpu0 = process_cpu();
                let began = Instant::now();
                let mut i = t * 7919 % n;
                let mut done = 0u64;
                let mut local = FxHashMap::<String, (u64, u64)>::default();
                while began.elapsed().as_secs_f64() < seconds {
                    let q = &trace[i % n];
                    i += 1;
                    let at = Instant::now();
                    if let Ok(scorer) = reader.scorer(&q.text) {
                        std::hint::black_box(reader.ranked(&scorer, args.k));
                    }
                    let e = local.entry(q.style.clone()).or_default();
                    e.0 += 1;
                    e.1 += at.elapsed().as_nanos() as u64;
                    done += 1;
                }
                let cpu1 = process_cpu();
                if t == 0 {
                    println!(
                        "cpu window: user {:.3} s, sys {:.3} s, {done} queries (thread 0; process-wide)",
                        cpu1.0 - cpu0.0,
                        cpu1.1 - cpu0.1
                    );
                }
                total.fetch_add(done, std::sync::atomic::Ordering::Relaxed);
                let mut all = per_style.lock().expect("lock");
                for (k, v) in local {
                    let e = all.entry(k).or_default();
                    e.0 += v.0;
                    e.1 += v.1;
                }
            });
        }
    });
    let done = total.load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "throughput: {} threads, {:.1} s: {} queries, {:.1} QPS",
        args.threads,
        seconds,
        done,
        done as f64 / seconds
    );
    for (style, (n, ns)) in per_style.lock().expect("lock").iter() {
        println!(
            "  {style:<12} {n:>8} queries, mean {:.1} µs",
            *ns as f64 / *n as f64 / 1e3
        );
    }
    Ok(())
}

/// The window bounds census per style (`--features census`, with
/// `STANNUM_WIN_CENSUS` set): the answer pass's walks only.
#[cfg(feature = "census")]
fn print_census(census: &std::collections::BTreeMap<String, engine::tinshape::census::Tally>) {
    use engine::tinshape::census::SHIFTS;
    if census.values().all(|t| t.walks == 0) {
        return;
    }
    let pct = |a: u64, b: u64| 100.0 * a as f64 / b.max(1) as f64;
    println!("\nwindow census (answer pass): per style, then per window size");
    for (style, t) in census {
        println!(
            "census {style}: walks {} groups {} pruned {} ({:.1}%) folds {} in pruned groups {} ({:.1}%); entries {}",
            t.walks,
            t.groups,
            t.pruned,
            pct(t.pruned, t.groups),
            t.folds,
            t.pruned_folds,
            pct(t.pruned_folds, t.folds),
            t.entries
        );
        for (k, shift) in SHIFTS.iter().enumerate() {
            println!(
                "census {style} w{}: checks {} ({:.1}% of folds) cut windows {} ({:.1}%) groups {} ({:.1}% of groups) folds {} ({:.1}% of folds, {:.1}% of pruned folds) violations {} windows {} ({:.3} an entry); final-threshold ceiling {:.1}% of folds, {:.1}% of pruned folds",
                1u32 << shift,
                t.checks[k],
                pct(t.checks[k], t.folds),
                t.cut_windows[k],
                pct(t.cut_windows[k], t.checks[k]),
                t.cut_groups[k],
                pct(t.cut_groups[k], t.groups),
                t.cut_folds[k],
                pct(t.cut_folds[k], t.folds),
                pct(t.cut_folds[k], t.pruned_folds),
                t.violations[k],
                t.windows[k],
                t.windows[k] as f64 / t.entries.max(1) as f64,
                pct(t.final_folds[k], t.folds),
                pct(t.final_folds[k], t.pruned_folds)
            );
        }
    }
}

/// What one thread of a `--cpus` set counted over one slice.
#[derive(Clone, Copy, Default)]
struct PinnedSlice {
    queries: u64,
    unsupported: u64,
    /// Instructions, cycles and branch misses of the timed passes.
    counts: [u64; 3],
    /// From the slice's start to this thread's last counted query.
    wall_ns: u64,
    /// The answers: each query's rows hashed, the hashes summed.
    digest: u64,
}

/// A thread's slice: its totals and, per query of the slice (by its place
/// in it), the timed passes' instructions, cycles and branch misses.
type PinnedRun = (PinnedSlice, Vec<[u64; 3]>);

/// Pins this thread to `cpu` (Linux: `sched_setaffinity`); false where it
/// cannot.
fn pin_to(cpu: usize) -> bool {
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" {
            fn sched_setaffinity(pid: i32, size: usize, mask: *const u64) -> i32;
        }
        let mut mask = [0u64; 16];
        if cpu >= 64 * mask.len() {
            return false;
        }
        mask[cpu / 64] |= 1 << (cpu % 64);
        // SAFETY: this thread (0), a mask of the size given.
        unsafe { sched_setaffinity(0, std::mem::size_of_val(&mask), mask.as_ptr()) == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cpu;
        false
    }
}

/// One thread of a `--cpus` set: every slice, each started with the set's
/// other threads (see the module's documentation).
fn pinned_thread(
    args: &Args,
    reader: &Reader,
    trace: &[bench::TraceQuery],
    fulls: &[bool],
    slices: &[(String, std::ops::Range<usize>)],
    (t, threads): (usize, usize),
    (start, done): (&std::sync::Barrier, &[std::sync::atomic::AtomicUsize]),
) -> Result<Vec<PinnedRun>, String> {
    use std::sync::atomic::Ordering;
    let mut out = Vec::with_capacity(slices.len());
    for (n, (_, range)) in slices.iter().enumerate() {
        let len = range.len();
        let offset = t * len / threads;
        let mut slice = PinnedSlice::default();
        // Per query of the slice (by its place in it): the timed passes' counts.
        let mut per_query = vec![[0u64; 3]; len];
        start.wait();
        let began = Instant::now();
        let mut i = 0usize;
        loop {
            if i == len {
                slice.wall_ns = began.elapsed().as_nanos() as u64;
                done[n].fetch_add(1, Ordering::AcqRel);
            }
            let counted = i < len;
            if !counted && done[n].load(Ordering::Acquire) >= threads {
                break;
            }
            let at = range.start + (offset + i) % len;
            i += 1;
            reader.full.set(fulls[at]);
            let text = &trace[at].text;
            let Ok(scorer) = reader.scorer(text) else {
                slice.unsupported += counted as u64;
                continue;
            };
            if args.forget {
                reader.forget();
            }
            let Some(answer) = reader.ranked(&scorer, args.k) else {
                slice.unsupported += counted as u64;
                continue;
            };
            let c0 = process_counters();
            for _ in 0..args.repeat {
                if args.forget {
                    reader.forget();
                }
                let scorer = reader.scorer(text)?;
                std::hint::black_box(reader.ranked(&scorer, args.k));
            }
            let c1 = process_counters();
            if counted {
                slice.queries += 1;
                for (k, count) in slice.counts.iter_mut().enumerate() {
                    *count += c1[k] - c0[k];
                    per_query[at - range.start][k] = c1[k] - c0[k];
                }
                let mut h = rustc_hash::FxHasher::default();
                for (score, tid) in &answer {
                    std::hash::Hash::hash(&(score.to_bits(), tid.block, tid.offset), &mut h);
                }
                slice.digest = slice.digest.wrapping_add(std::hash::Hasher::finish(&h));
            }
        }
        out.push((slice, per_query));
    }
    Ok(out)
}

/// `--cpus`: each set's pinned threads replay each slice at once (see the
/// module's documentation).
fn pinned(
    args: &Args,
    loaded: &Loaded,
    trace: &[bench::TraceQuery],
    fulls: &[bool],
) -> Result<(), String> {
    // Slices: runs of one style label, in the plan's order.
    let mut slices: Vec<(String, std::ops::Range<usize>)> = Vec::new();
    for (i, q) in trace.iter().enumerate() {
        match slices.last_mut() {
            Some((style, range)) if *style == q.style => range.end = i + 1,
            _ => slices.push((q.style.clone(), i..i + 1)),
        }
    }
    for set in &args.cpus {
        let threads = set.len();
        let start = std::sync::Barrier::new(threads);
        let done: Vec<std::sync::atomic::AtomicUsize> =
            slices.iter().map(|_| Default::default()).collect();
        let (slices, start, done) = (&slices, &start, &done);
        let per_thread = std::thread::scope(|s| {
            let handles: Vec<_> = set
                .iter()
                .enumerate()
                .map(|(t, &cpu)| {
                    s.spawn(move || {
                        if !pin_to(cpu) {
                            eprintln!("warning: thread {t} not pinned to CPU {cpu}");
                        }
                        let reader = Reader::open(loaded, args)?;
                        pinned_thread(
                            args,
                            &reader,
                            trace,
                            fulls,
                            slices,
                            (t, threads),
                            (start, done),
                        )
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("a pinned thread panicked"))
                .collect::<Result<Vec<_>, String>>()
        })?;
        let label = set
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(",");
        for (n, (style, _)) in slices.iter().enumerate() {
            let all: Vec<PinnedSlice> = per_thread.iter().map(|t| t[n].0).collect();
            let queries = all.iter().map(|s| s.queries).sum::<u64>().max(1) as f64;
            let sum = |k: usize| all.iter().map(|s| s.counts[k]).sum::<u64>() as f64;
            let (ins, cyc, miss) = (sum(0), sum(1).max(1.0), sum(2));
            // Each thread's queries over its own cycles, summed: what the
            // core finishes per gigacycle of its clock.
            let rate: f64 = all
                .iter()
                .map(|s| s.queries as f64 * 1e9 / s.counts[1].max(1) as f64)
                .sum();
            let wall = all.iter().map(|s| s.wall_ns).max().unwrap_or(0).max(1) as f64 / 1e9;
            // Every thread answers every query of the slice once, counted.
            let digest = match all.iter().all(|s| s.digest == all[0].digest) {
                true => format!("{:016x}", all[0].digest),
                false => "threads-differ".to_owned(),
            };
            let per: Vec<String> = all
                .iter()
                .map(|s| format!("{:.3}", s.counts[1] as f64 / s.queries.max(1) as f64 / 1e6))
                .collect();
            println!(
                "pinned {label:<8} {style:<18} threads {} queries {queries} unsupported {} \
                 ins/q {:.3}M cyc/q {:.3}M bmiss/q {:.2}K ipc {:.3} core-ipc {:.3} q/Gcyc {rate:.3} \
                 qps {:.2} digest {digest} thread-cyc/q {}",
                all.len(),
                all.iter().map(|s| s.unsupported).sum::<u64>(),
                ins / queries / 1e6,
                cyc / queries / 1e6,
                miss / queries / 1e3,
                ins / cyc,
                ins / (cyc / all.len() as f64),
                queries / wall,
                per.join(","),
            );
        }
        // Per query: its timed passes' instructions and cycles, per pass,
        // the mean over the threads that answered it.
        for (n, (style, range)) in slices.iter().enumerate() {
            for (j, q) in trace[range.clone()].iter().enumerate() {
                let counted: Vec<[u64; 3]> = per_thread
                    .iter()
                    .map(|t| t[n].1[j])
                    .filter(|c| c[1] > 0)
                    .collect();
                if counted.is_empty() {
                    continue;
                }
                let per = (counted.len() * args.repeat.max(1)) as f64;
                let sum = |k: usize| counted.iter().map(|c| c[k]).sum::<u64>() as f64 / per;
                println!(
                    "pinq {label:<8} {style:<18} {} ins {:.0} cyc {:.0}",
                    q.name,
                    sum(0),
                    sum(1),
                );
            }
        }
    }
    Ok(())
}

fn run() -> Result<bool, String> {
    let args = args();
    let mut trace = bench::read_trace(&args.trace)?;
    if let Some(style) = &args.style {
        trace.retain(|q| &q.style == style);
    }
    if let Some(n) = args.limit {
        trace.truncate(n);
    }
    let mut fulls = vec![args.full; trace.len()];
    if !args.plan.is_empty() {
        let mut sliced = Vec::new();
        fulls.clear();
        for (style, full, pick) in &args.plan {
            let of_style = || trace.iter().filter(|q| &q.style == style);
            let (picked, suffix): (Vec<&bench::TraceQuery>, &str) = match pick {
                PlanSlice::First(n) => (of_style().take(*n).collect(), ""),
                PlanSlice::Named(stem, names) => {
                    let by_name: FxHashMap<&str, &bench::TraceQuery> =
                        of_style().map(|q| (q.name.as_str(), q)).collect();
                    let picked = names
                        .iter()
                        .map(|n| {
                            by_name.get(n.as_str()).copied().ok_or_else(|| {
                                format!("--plan {style}:@{stem}: no {style} query {n}")
                            })
                        })
                        .collect::<Result<_, _>>()?;
                    (picked, stem.as_str())
                }
            };
            for q in picked {
                let mut q = q.clone();
                if *full {
                    q.style.push_str("+full");
                }
                if !suffix.is_empty() {
                    q.style.push('@');
                    q.style.push_str(suffix);
                }
                sliced.push(q);
                fulls.push(*full);
            }
        }
        trace = sliced;
    }
    let loaded = load(&args.dump, args.mmap)?;
    if args.threads > 0 {
        throughput(&args, &loaded, &trace)?;
        return Ok(true);
    }
    if !args.cpus.is_empty() {
        return pinned(&args, &loaded, &trace, &fulls).map(|()| true);
    }
    let start = Instant::now();
    let reader = Reader::open(&loaded, &args)?;
    if let Some(path) = &args.group_stats {
        let mut text =
            String::from("generation\tgroup\tid\tdocuments\tmean_length\tc0\tc1\tc2\tc3\n");
        for seg in &reader.segs {
            let docs = &seg.segment.docs;
            let groups = &docs.geometry.groups;
            for (g, group) in groups.iter().enumerate() {
                let from = group.rank_base;
                let to = groups
                    .get(g + 1)
                    .map_or(seg.segment.documents, |next| next.rank_base);
                let (mut sum, mut classes) = (0u64, [0u32; 4]);
                for rank in from..to {
                    sum += u64::from(seg.segment.lengths.get(rank).unwrap_or(0));
                    classes[usize::from(seg.segment.lengths.class(rank).unwrap_or(0))] += 1;
                }
                let n = to - from;
                let _ = writeln!(
                    text,
                    "{}\t{g}\t{}\t{n}\t{:.1}\t{}\t{}\t{}\t{}",
                    seg.generation,
                    group.id,
                    sum as f64 / f64::from(n.max(1)),
                    classes[0],
                    classes[1],
                    classes[2],
                    classes[3]
                );
            }
        }
        std::fs::write(path, text).map_err(|e| e.to_string())?;
    }
    eprintln!(
        "{} segments opened in {:.1} s",
        reader.segs.len(),
        start.elapsed().as_secs_f64()
    );
    let mut perf = PerfCtl::open(args.perf_ctl.as_ref()).map_err(|e| format!("--perf-ctl: {e}"))?;
    let mut rows = Vec::new();
    let mut warm_out = String::from(
        "name\tstyle\twarm_rows\ttheta_warm\ttheta_final\twarm_in_final\tand_examined\tand_scored\n",
    );
    let mut unsupported = 0usize;
    let mut mismatched = 0usize;
    #[cfg(feature = "census")]
    let mut census = std::collections::BTreeMap::<String, engine::tinshape::census::Tally>::new();
    for (n, q) in trace.iter().enumerate() {
        reader.full.set(fulls[n]);
        let scorer = reader.scorer(&q.text)?;
        // Warm pass (also the answer, and what it decoded of the terms'
        // metadata), then timed passes.
        segment::tinshape::segment::reset_memo_counts();
        if args.forget {
            reader.forget();
        }
        #[cfg(feature = "census")]
        engine::tinshape::census::take();
        let Some(answer) = reader.ranked(&scorer, args.k) else {
            unsupported += 1;
            continue;
        };
        #[cfg(feature = "census")]
        census
            .entry(q.style.clone())
            .or_default()
            .add(&engine::tinshape::census::take());
        let memo = segment::tinshape::segment::memo_counts();
        // `--seed-rank R`: the rows ranked R-k+1..=R (or the last k of
        // fewer, when at least 2k match), found untimed.
        let ranked_seed: Option<Vec<(f32, Tid)>> = args.seed_rank.and_then(|r| {
            let deep = reader.ranked(&scorer, r)?;
            (deep.len() >= 2 * args.k).then(|| deep[deep.len() - args.k..].to_vec())
        });
        let seed_rows: Option<&[(f32, Tid)]> = ranked_seed
            .as_deref()
            .or(args.seed.then_some(answer.as_slice()));
        let mut times = Vec::with_capacity(args.repeat);
        let (mut scorer_allocs, mut walk_allocs) = (Allocs::default(), Allocs::default());
        let untimed = (args.warm.is_some() && args.warm_untimed).then(|| {
            reader
                .warm(&scorer, args.k, args.order, &answer)
                .unwrap_or_default()
        });
        let counters0 = process_counters();
        for _ in 0..args.repeat {
            if args.forget {
                reader.forget();
            }
            perf.send(b"enable\n");
            let at = Instant::now();
            let a0 = Allocs::now();
            let scorer = reader.scorer(&q.text)?;
            let a1 = Allocs::now();
            if args.timed_walk && args.warm.is_some() {
                let warm = match &untimed {
                    Some(warm) => (warm.0.clone(), Default::default()),
                    None => reader
                        .warm(&scorer, args.k, args.order, &answer)
                        .unwrap_or_default(),
                };
                std::hint::black_box(reader.walked(
                    &scorer,
                    args.k,
                    Some(warm.0.as_slice()),
                    args.order,
                    &answer,
                ));
            } else if args.timed_walk {
                std::hint::black_box(
                    reader.walked(&scorer, args.k, seed_rows, args.order, &answer),
                );
            } else {
                std::hint::black_box(reader.ranked(&scorer, args.k));
            }
            let a2 = Allocs::now();
            times.push(at.elapsed().as_nanos() as u64);
            perf.send(b"disable\n");
            (scorer_allocs, walk_allocs) = (a1.since(a0), a2.since(a1));
        }
        let counters1 = process_counters();
        let timed: [u64; 3] = std::array::from_fn(|i| counters1[i] - counters0[i]);
        let mut pages = Pages::default();
        let pins0 = PINS.get();
        let hash0 = REFS.with_borrow_mut(|refs| {
            refs.peak = refs.held;
            refs.hash_ops
        });
        segment::tinshape::blob::reset_stats();
        let warm = match &args.warm {
            Some(_) => {
                let (rows, a) = reader
                    .warm(&scorer, args.k, args.order, &answer)
                    .ok_or("a warm-up failed")?;
                let theta = |r: &[(f32, Tid)]| {
                    let mut s: Vec<f32> = r.iter().map(|x| x.0).collect();
                    s.sort_by(|a, b| b.total_cmp(a));
                    s.get(args.k - 1).copied()
                };
                let fmt = |t: Option<f32>| t.map_or("-".to_owned(), |t| t.to_string());
                let kept = rows
                    .iter()
                    .filter(|r| answer.iter().any(|x| x.1 == r.1))
                    .count();
                let _ = writeln!(
                    warm_out,
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    q.name,
                    q.style,
                    rows.len(),
                    fmt(theta(&rows)),
                    fmt(theta(&answer)),
                    kept,
                    a.candidates,
                    a.scored
                );
                Some(rows)
            }
            None => None,
        };
        let (direct, counters, segments) = reader
            .instrumented(
                &scorer,
                args.k,
                &mut pages,
                warm.as_deref().or(seed_rows),
                args.order,
                &answer,
            )
            .ok_or("an instrumented walk failed")?;
        if direct != answer && !args.seed {
            mismatched += 1;
            eprintln!("instrumented walk differs: {}", q.name);
        }
        rows.push(Row {
            name: q.name.clone(),
            style: q.style.clone(),
            ns: percentile(&mut times, 0.5),
            rows: answer,
            pages,
            answer: counters,
            pins: PINS.get() - pins0,
            peak: REFS.with_borrow(|refs| refs.peak),
            hash_ops: REFS.with_borrow(|refs| refs.hash_ops) - hash0,
            kinds: segment::tinshape::blob::stats(),
            memo,
            scorer_allocs,
            walk_allocs,
            timed,
            segments,
        });
        if (n + 1) % 500 == 0 {
            eprintln!("{} of {}", n + 1, trace.len());
        }
    }
    let _ = NoTouch;
    if let Some(path) = &args.warm {
        std::fs::write(path, &warm_out).map_err(|e| e.to_string())?;
    }
    // Answers and the comparison.
    let tids: FxHashSet<Tid> = rows
        .iter()
        .flat_map(|r| r.rows.iter().map(|x| x.1))
        .collect();
    let ids = resolve_ids(&args.dump, &tids);
    let mut out = String::new();
    for r in &rows {
        let _ = writeln!(
            out,
            "{}\t{}\tranked\t{}",
            r.name,
            r.style,
            answer_line(&ids, &r.rows)
        );
    }
    if let Some(path) = &args.out {
        std::fs::write(path, &out).map_err(|e| e.to_string())?;
    }
    let (mut same, mut differ) = (0usize, 0usize);
    if let Some(path) = &args.expect {
        let expected: FxHashMap<String, String> = std::fs::read_to_string(path)
            .map_err(|e| e.to_string())?
            .lines()
            .filter_map(|line| {
                let f: Vec<&str> = line.splitn(4, '\t').collect();
                (f.len() == 4 && f[2] == "ranked").then(|| (f[0].to_owned(), f[3].to_owned()))
            })
            .collect();
        for r in &rows {
            if let Some(want) = expected.get(&r.name) {
                let got = answer_line(&ids, &r.rows);
                if *want == got {
                    same += 1;
                } else {
                    differ += 1;
                    if differ <= 10 {
                        eprintln!("DIFF {}\n  expected {want}\n  got      {got}", r.name);
                    }
                }
            }
        }
        println!("expect: {same} equal, {differ} different");
    }
    println!(
        "{} queries, {unsupported} not walked natively (zero fill or streamed), {mismatched} instrumented mismatches",
        rows.len()
    );
    #[cfg(feature = "census")]
    print_census(&census);
    let mut styles: Vec<String> = rows.iter().map(|r| r.style.clone()).collect();
    styles.sort();
    styles.dedup();
    println!(
        "\nranked latency, µs (median of {} passes), one thread{}",
        args.repeat,
        if args.keep {
            ", records kept"
        } else {
            ", records forgotten per query"
        }
    );
    println!(
        "{:<12} {:>7} {:>9} {:>9} {:>9}",
        "style", "queries", "p50", "p99", "mean"
    );
    for style in &styles {
        let mut t: Vec<u64> = rows
            .iter()
            .filter(|r| &r.style == style)
            .map(|r| r.ns)
            .collect();
        let mean = t.iter().sum::<u64>() as f64 / t.len().max(1) as f64 / 1e3;
        println!(
            "{:<12} {:>7} {:>9.1} {:>9.1} {:>9.1}",
            style,
            t.len(),
            percentile(&mut t, 0.5) as f64 / 1e3,
            percentile(&mut t, 0.99) as f64 / 1e3,
            mean
        );
    }
    println!("\ndistinct pages per query, mean, by TIN's areas (accesses in parentheses)");
    let mut header = format!("{:<12}", "style");
    for part in Part::ALL {
        let _ = write!(header, " {:>20}", part.name());
    }
    println!("{header} {:>8}", "total");
    for style in &styles {
        let of: Vec<&Row> = rows.iter().filter(|r| &r.style == style).collect();
        let n = of.len().max(1) as f64;
        let mut line = format!("{style:<12}");
        let mut total = 0.0;
        for part in Part::ALL {
            let d = of.iter().map(|r| r.pages.distinct(part)).sum::<usize>() as f64 / n;
            let a = of
                .iter()
                .map(|r| r.pages.accesses.get(&part).copied().unwrap_or(0))
                .sum::<u64>() as f64
                / n;
            total += d;
            let _ = write!(line, " {:>20}", format!("{d:.1} ({a:.0})"));
        }
        println!("{line} {total:>8.1}");
    }
    println!(
        "\nreads per query by area, mean: reads, spanning two pages (%), page switches, back to an earlier page"
    );
    for style in &styles {
        let rs: Vec<&Row> = rows.iter().filter(|r| &r.style == style).collect();
        let n = rs.len().max(1) as f64;
        let mut line = format!("{style:<12}");
        for part in Part::ALL {
            let sum = |f: fn(&bench::tinshape::ReadOrder) -> u64| -> f64 {
                rs.iter()
                    .filter_map(|r| r.pages.order.get(&part))
                    .map(f)
                    .sum::<u64>() as f64
            };
            let reads = sum(|o| o.reads);
            if reads == 0.0 {
                continue;
            }
            let _ = write!(
                line,
                "  {}: {:.0} ({:.2}%) sw {:.0} back {:.0}",
                part.name(),
                reads / n,
                100.0 * sum(|o| o.straddles) / reads,
                sum(|o| o.switches) / n,
                sum(|o| o.back) / n,
            );
        }
        println!("{line}");
    }
    println!("\nstitched per query by kind, mean: reads, bytes");
    for style in &styles {
        let rs: Vec<&Row> = rows.iter().filter(|r| &r.style == style).collect();
        let n = rs.len().max(1) as f64;
        let mut line = format!("{style:<12}");
        for (k, name) in segment::tinshape::blob::KIND_NAMES.iter().enumerate() {
            let reads = rs.iter().map(|r| r.kinds[k].stitches).sum::<u64>() as f64 / n;
            let bytes = rs.iter().map(|r| r.kinds[k].stitched).sum::<u64>() as f64 / n;
            if reads > 0.0 {
                let _ = write!(line, "  {name}: {reads:.1} ({:.1} KB)", bytes / 1024.0);
            }
        }
        println!("{line}");
    }
    println!("\npinned per query by kind, mean");
    for style in &styles {
        let rs: Vec<&Row> = rows.iter().filter(|r| &r.style == style).collect();
        let n = rs.len().max(1) as f64;
        let mut line = format!("{style:<12}");
        for (k, name) in segment::tinshape::blob::KIND_NAMES.iter().enumerate() {
            let pins = rs.iter().map(|r| r.kinds[k].pinned).sum::<u64>() as f64 / n;
            if pins > 0.0 {
                let _ = write!(line, "  {name}: {pins:.1}");
            }
        }
        println!("{line}");
    }
    println!("\npages pinned per query (in place), mean");
    for style in &styles {
        let of: Vec<&Row> = rows.iter().filter(|r| &r.style == style).collect();
        println!(
            "{:<12} {:>10.1}",
            style,
            of.iter().map(|r| r.pins).sum::<u64>() as f64 / of.len().max(1) as f64
        );
    }
    println!(
        "\npins held at once per query (mean, p50, max), and private refcount hash operations per query (mean)"
    );
    for style in &styles {
        let of: Vec<&Row> = rows.iter().filter(|r| &r.style == style).collect();
        let n = of.len().max(1) as f64;
        let mut peaks: Vec<u64> = of.iter().map(|r| r.peak).collect();
        peaks.sort_unstable();
        println!(
            "{:<12} peak {:>8.1} {:>6} {:>6}  hash ops {:>10.1}",
            style,
            peaks.iter().sum::<u64>() as f64 / n,
            peaks.get(peaks.len() / 2).copied().unwrap_or(0),
            peaks.last().copied().unwrap_or(0),
            of.iter().map(|r| r.hash_ops).sum::<u64>() as f64 / n,
        );
    }
    println!(
        "\nallocations per query (a timed pass), mean: building the scorer, then the walk: \
         allocations, growths (realloc), KB asked"
    );
    for style in &styles {
        let of: Vec<&Row> = rows.iter().filter(|r| &r.style == style).collect();
        let n = of.len().max(1) as f64;
        let m = |f: &dyn Fn(&Row) -> u64| of.iter().map(|r| f(r)).sum::<u64>() as f64 / n;
        println!(
            "{:<12} scorer {:>8.1} {:>7.1} {:>8.1} KB  walk {:>9.1} {:>8.1} {:>9.1} KB",
            style,
            m(&|r| r.scorer_allocs.allocs),
            m(&|r| r.scorer_allocs.reallocs),
            m(&|r| r.scorer_allocs.bytes) / 1024.0,
            m(&|r| r.walk_allocs.allocs),
            m(&|r| r.walk_allocs.reallocs),
            m(&|r| r.walk_allocs.bytes) / 1024.0,
        );
    }
    println!(
        "\ninstructions, cycles and branch misses of the timed passes, all queries (where the OS counts them)"
    );
    for style in &styles {
        let of: Vec<&Row> = rows.iter().filter(|r| &r.style == style).collect();
        println!(
            "{:<12} timed instructions {:.3}G cycles {:.3}G branch misses {:.3}M",
            style,
            of.iter().map(|r| r.timed[0]).sum::<u64>() as f64 / 1e9,
            of.iter().map(|r| r.timed[1]).sum::<u64>() as f64 / 1e9,
            of.iter().map(|r| r.timed[2]).sum::<u64>() as f64 / 1e6,
        );
    }
    println!(
        "\nwalk per query, mean: examined, scored, position checks, windows, pruned, ruled out by length class"
    );
    for style in &styles {
        let of: Vec<&Row> = rows.iter().filter(|r| &r.style == style).collect();
        let n = of.len().max(1) as f64;
        let m = |f: &dyn Fn(&tin::RankedAnswer) -> u64| {
            of.iter().map(|r| f(&r.answer)).sum::<u64>() as f64 / n
        };
        println!(
            "{:<12} examined {:>10.0} scored {:>10.0} checks {:>8.0} windows {:>8.0} pruned {:>8.0} class cuts {:>8.0}",
            style,
            m(&|a| a.candidates),
            m(&|a| a.scored),
            m(&|a| a.position_checks),
            m(&|a| a.windows),
            m(&|a| a.windows_pruned),
            m(&|a| a.class_cuts),
        );
    }
    println!(
        "\nterm metadata per query, mean (answer pass): records parsed / kept, footers decoded / kept, \
         footer blocks decoded, reached, used, in the whole footers, footer KB parsed, in the whole footers; directories parsed, entries read, reached, \
         loaded, directory KB parsed"
    );
    for style in &styles {
        let of: Vec<&Row> = rows.iter().filter(|r| &r.style == style).collect();
        let n = of.len().max(1) as f64;
        let m = |f: &dyn Fn(&segment::tinshape::segment::MemoCounts) -> u64| {
            of.iter().map(|r| f(&r.memo)).sum::<u64>() as f64 / n
        };
        println!(
            "{:<12} records {:.1} / {:.1} footers {:.1} / {:.1} blocks {:.0} reached {:.0} used {:.0} whole {:.0} footer KB {:.1} whole {:.1}  \
             dirs {:.1} entries {:.0} reached {:.0} loaded {:.0} dir KB {:.1}",
            style,
            m(&|c| c.records_parsed),
            m(&|c| c.records_kept),
            m(&|c| c.footers_decoded),
            m(&|c| c.footers_kept),
            m(&|c| c.footer_blocks),
            m(&|c| c.blocks_reached),
            m(&|c| c.blocks_used),
            m(&|c| c.footer_blocks_whole),
            m(&|c| c.footer_bytes) / 1024.0,
            m(&|c| c.footer_bytes_whole) / 1024.0,
            m(&|c| c.directories),
            m(&|c| c.directory_entries),
            m(&|c| c.entries_reached),
            m(&|c| c.entries_used),
            m(&|c| c.directory_bytes) / 1024.0,
        );
    }
    if let Some(path) = &args.per_query {
        let mut text = String::from(
            "name\tstyle\tus\texamined\tscored\tchecks\tpages\tdl\tpayload\tfooter\ttf\tpositions\tpins\tallocs\treallocs\tdlclass\tclasscuts\tins\tcyc\n",
        );
        for r in &rows {
            let _ = writeln!(
                text,
                "{}\t{}\t{:.1}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.0}\t{:.0}",
                r.name,
                r.style,
                r.ns as f64 / 1e3,
                r.answer.candidates,
                r.answer.scored,
                r.answer.position_checks,
                r.pages.total(),
                r.pages.distinct(Part::DlSidecar),
                r.pages.distinct(Part::Payload),
                r.pages.distinct(Part::Footer),
                r.pages.distinct(Part::TfTail),
                r.pages.distinct(Part::Positions),
                r.pins,
                r.walk_allocs.allocs,
                r.walk_allocs.reallocs,
                r.pages.distinct(Part::DlClasses),
                r.answer.class_cuts,
                r.timed[0] as f64 / args.repeat.max(1) as f64,
                r.timed[1] as f64 / args.repeat.max(1) as f64,
            );
        }
        std::fs::write(path, text).map_err(|e| e.to_string())?;
    }
    if let Some(path) = &args.rows_at {
        // Where each answer row lies: its segment and group there.
        let mut text =
            String::from("name\tstyle\trank\tscore\tblock\toffset\tgeneration\tgroup\tgroups\n");
        for r in &rows {
            for (rank, (score, tid)) in r.rows.iter().enumerate() {
                let at = reader.segs.iter().find_map(|seg| {
                    let geometry = &seg.segment.docs.geometry;
                    let slot = geometry.slot_of(*tid)?;
                    seg.segment.docs.rank(slot)?;
                    Some((
                        seg.generation,
                        geometry.group_of_slot(slot),
                        geometry.groups.len(),
                    ))
                });
                let (generation, group, groups) = at.unwrap_or((0, 0, 0));
                let _ = writeln!(
                    text,
                    "{}\t{}\t{}\t{score}\t{}\t{}\t{generation}\t{group}\t{groups}",
                    r.name,
                    r.style,
                    rank + 1,
                    tid.block,
                    tid.offset
                );
            }
        }
        std::fs::write(path, text).map_err(|e| e.to_string())?;
    }
    if let Some(path) = &args.per_segment {
        let mut text = String::from(
            "name\tstyle\tstep\tgeneration\tdocuments\ttheta\tfinal\tgroups\tpruned\texamined\tscored\tpages\tpins\n",
        );
        for r in &rows {
            // The final threshold: the k-th row's score (none with fewer).
            let last = (r.rows.len() == args.k).then(|| r.rows[args.k - 1].0);
            let show = |t: Option<f32>| t.map_or_else(|| "-".to_owned(), |t| t.to_string());
            for (step, w) in r.segments.iter().enumerate() {
                let _ = writeln!(
                    text,
                    "{}\t{}\t{step}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    r.name,
                    r.style,
                    w.generation,
                    w.documents,
                    show(w.theta),
                    show(last),
                    w.groups,
                    w.pruned,
                    w.examined,
                    w.scored,
                    w.pages,
                    w.pins,
                );
            }
        }
        std::fs::write(path, text).map_err(|e| e.to_string())?;
    }
    Ok(differ == 0 && mismatched == 0)
}

/// perf's control FIFO (`--perf-ctl`): each command waits for perf's ack,
/// so the counters are on exactly while a timed pass runs.
struct PerfCtl(Option<(std::fs::File, std::fs::File)>);

impl PerfCtl {
    fn open(paths: Option<&(PathBuf, PathBuf)>) -> std::io::Result<Self> {
        Ok(Self(match paths {
            Some((ctl, ack)) => Some((
                std::fs::OpenOptions::new().write(true).open(ctl)?,
                std::fs::File::open(ack)?,
            )),
            None => None,
        }))
    }

    fn send(&mut self, command: &[u8]) {
        use std::io::{Read, Write};
        if let Some((ctl, ack)) = &mut self.0 {
            ctl.write_all(command).expect("perf control FIFO");
            let mut reply = [0u8; 5];
            ack.read_exact(&mut reply).expect("perf ack FIFO");
        }
    }
}

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("tnsreplay: {error}");
            std::process::exit(1);
        }
    }
}

/// Instructions, cycles and branch misses this thread ran so far in user
/// space (Linux: a `perf_event_open` group per thread, opened on its
/// first use; macOS:
/// `proc_pid_rusage`'s instructions and cycles, as `time -l` reports them,
/// no branch misses); zero where neither counts them. Taken around the
/// timed passes only, they leave out loading the dump and the
/// instrumented pass.
fn process_counters() -> [u64; 3] {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
            fn getpid() -> i32;
        }
        // `struct rusage_info_v4`: a 16-byte UUID, then 64-bit fields, of
        // which `ri_instructions` and `ri_cycles` are the 30th and 31st.
        let mut info = [0u64; 2 + 40];
        // SAFETY: RUSAGE_INFO_V4 (4) into a buffer larger than the struct.
        if unsafe { proc_pid_rusage(getpid(), 4, info.as_mut_ptr()) } == 0 {
            return [info[2 + 29], info[2 + 30], 0];
        }
    }
    #[cfg(target_os = "linux")]
    if let Some(counts) = linux_counters::counts() {
        return counts;
    }
    [0; 3]
}

/// The Linux side of [`process_counters`]: this thread's hardware
/// instructions (the group's leader), cycles and branch misses in user
/// space, read together.
#[cfg(target_os = "linux")]
mod linux_counters {
    use std::cell::OnceCell;

    /// `struct perf_event_attr` up to `PERF_ATTR_SIZE_VER0` (64 bytes).
    #[repr(C)]
    #[derive(Default)]
    struct Attr {
        kind: u32,
        size: u32,
        config: u64,
        sample_period: u64,
        sample_type: u64,
        read_format: u64,
        flags: u64,
        wakeup_events: u32,
        bp_type: u32,
        config1: u64,
    }

    unsafe extern "C" {
        fn syscall(number: i64, ...) -> i64;
        fn read(fd: i32, buffer: *mut u64, bytes: usize) -> isize;
    }

    #[cfg(target_arch = "x86_64")]
    const PERF_EVENT_OPEN: i64 = 298;
    #[cfg(target_arch = "aarch64")]
    const PERF_EVENT_OPEN: i64 = 241;
    /// `PERF_COUNT_HW_INSTRUCTIONS`, `_CPU_CYCLES`, `_BRANCH_MISSES`.
    const EVENTS: [u64; 3] = [1, 0, 5];
    /// `exclude_kernel` and `exclude_hv`.
    const USER_ONLY: u64 = 1 << 5 | 1 << 6;
    /// `PERF_FORMAT_GROUP`.
    const GROUP: u64 = 1 << 3;

    fn open(config: u64, leader: i32) -> Option<i32> {
        let attr = Attr {
            size: std::mem::size_of::<Attr>() as u32,
            config,
            read_format: GROUP,
            flags: USER_ONLY,
            ..Attr::default() // kind 0: PERF_TYPE_HARDWARE
        };
        // SAFETY: perf_event_open(attr, this thread, any CPU, leader, 0)
        // reads the attribute only during the call.
        let fd = unsafe { syscall(PERF_EVENT_OPEN, &raw const attr, 0i32, -1i32, leader, 0u64) };
        (fd >= 0).then_some(fd as i32)
    }

    thread_local! {
        /// This thread's group leader, opened on its first count.
        static LEADER: OnceCell<Option<i32>> = const { OnceCell::new() };
    }

    pub(super) fn counts() -> Option<[u64; 3]> {
        let leader = LEADER.with(|cell| {
            *cell.get_or_init(|| {
                let leader = open(EVENTS[0], -1)?;
                for config in &EVENTS[1..] {
                    open(*config, leader)?;
                }
                Some(leader)
            })
        })?;
        // `{ nr, values[nr] }`, in the order the events were opened.
        let mut group = [0u64; 4];
        // SAFETY: at most the buffer's 32 bytes.
        let got = unsafe { read(leader, group.as_mut_ptr(), 32) };
        (got == 32 && group[0] == 3).then(|| [group[1], group[2], group[3]])
    }
}

/// The process's user and system CPU seconds so far (`getrusage`).
fn process_cpu() -> (f64, f64) {
    #[repr(C)]
    #[derive(Default)]
    struct Timeval {
        sec: i64,
        usec: i64,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Rusage {
        utime: Timeval,
        stime: Timeval,
        rest: [i64; 14],
    }
    unsafe extern "C" {
        fn getrusage(who: i32, usage: *mut Rusage) -> i32;
    }
    let mut usage = Rusage::default();
    // SAFETY: RUSAGE_SELF into a struct laid out as the C one (timeval's
    // microseconds are padded to eight bytes on 64-bit targets).
    unsafe { getrusage(0, &mut usage) };
    let t = |v: &Timeval| v.sec as f64 + v.usec as f64 * 1e-6;
    (t(&usage.utime), t(&usage.stime))
}
