// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Replays a query trace against a dumped index, outside PostgreSQL.
//!
//! ```text
//! cargo run -p bench --release --bin replay -- --dump DIR --trace trace.tsv \
//!     [--k 10] [--style conjunction,phrase] [--limit N] [--repeat 3] \
//!     [--threads 1,8] [--cold] [--whatif] [--no-count] [--no-ranked] \
//!     [--out results.tsv] [--expect results.tsv] [--per-query queries.tsv]
//! ```
//!
//! Per query it runs the ranked top k of `stannum.score(ctid)` (the default
//! dense ratio, the index's BM25 parameters) and the count, as the
//! extension's custom scan does, and reports per style the latency (p50,
//! p99, mean of the median of the timed passes), single-thread and
//! `--threads` throughput, page touches by area of the blob (warm: readers
//! and caches kept across queries, as a long-lived backend keeps them;
//! `--cold`: fresh readers and caches per query), and what the walk pruned.
//! `--whatif` adds what tighter sub-block bounds would have pruned (see the
//! bench crate's `whatif` module).
//!
//! `--out` writes every answer (ids or heap locations with score bits, and
//! counts) so runs can be diffed; `--expect` compares against such a file,
//! from an earlier run or from PostgreSQL (`script/replay-oracle.py`), and
//! fails on any difference. Every row is taken as visible: a dump has no
//! heap, only the dead lists VACUUM published.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use bench::TraceQuery;
use bench::areas::{AREAS, Area};
use bench::dump::Dump;
use bench::paged::{Touches, take_touches};
use bench::replay::{Engine, collect_events};
use bench::whatif::{Analyzer, WhatIf};
use engine::walk::stats::Counters;

struct Options {
    dump: PathBuf,
    trace: PathBuf,
    k: usize,
    styles: Option<Vec<String>>,
    limit: Option<usize>,
    repeat: usize,
    threads: Vec<usize>,
    cold: bool,
    whatif: bool,
    ranked: bool,
    count: bool,
    out: Option<PathBuf>,
    expect: Option<PathBuf>,
    per_query: Option<PathBuf>,
}

fn usage() -> ! {
    eprintln!(
        "usage: replay --dump DIR --trace FILE [--k N] [--style S,..] [--limit N] [--repeat N] \
         [--threads N,..] [--cold] [--whatif] [--no-count] [--no-ranked] [--out FILE] \
         [--expect FILE] [--per-query FILE]"
    );
    std::process::exit(2)
}

fn options() -> Options {
    let mut args = std::env::args().skip(1);
    let mut options = Options {
        dump: PathBuf::new(),
        trace: PathBuf::new(),
        k: 10,
        styles: None,
        limit: None,
        repeat: 3,
        threads: vec![1],
        cold: false,
        whatif: false,
        ranked: true,
        count: true,
        out: None,
        expect: None,
        per_query: None,
    };
    let value = |args: &mut dyn Iterator<Item = String>| args.next().unwrap_or_else(|| usage());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dump" => options.dump = value(&mut args).into(),
            "--trace" => options.trace = value(&mut args).into(),
            "--k" => options.k = value(&mut args).parse().unwrap_or_else(|_| usage()),
            "--style" => {
                options.styles = Some(value(&mut args).split(',').map(str::to_owned).collect());
            }
            "--limit" => options.limit = Some(value(&mut args).parse().unwrap_or_else(|_| usage())),
            "--repeat" => options.repeat = value(&mut args).parse().unwrap_or_else(|_| usage()),
            "--threads" => {
                options.threads = value(&mut args)
                    .split(',')
                    .map(|n| n.parse().unwrap_or_else(|_| usage()))
                    .collect();
            }
            "--cold" => options.cold = true,
            "--whatif" => options.whatif = true,
            "--no-count" => options.count = false,
            "--no-ranked" => options.ranked = false,
            "--out" => options.out = Some(value(&mut args).into()),
            "--expect" => options.expect = Some(value(&mut args).into()),
            "--per-query" => options.per_query = Some(value(&mut args).into()),
            _ => usage(),
        }
    }
    if options.dump.as_os_str().is_empty() || options.trace.as_os_str().is_empty() {
        usage();
    }
    options.repeat = options.repeat.max(1);
    options
}

/// One query's measurements.
#[derive(Default)]
struct Record {
    ranked: Option<String>,
    count: Option<String>,
    ranked_ns: Vec<u64>,
    count_ns: Vec<u64>,
    ranked_touches: Touches,
    count_touches: Touches,
    cold_ranked: Touches,
    cold_count: Touches,
    path: &'static str,
    scored: usize,
    threshold: Option<f32>,
    counters: Counters,
    chunk_loads: i64,
    positions: (i64, i64),
    whatif: WhatIf,
}

fn answer_line(dump: &Dump, rows: &[(f32, segment::Tid)]) -> String {
    let mut line = String::new();
    for (n, (score, tid)) in rows.iter().enumerate() {
        if n > 0 {
            line.push(',');
        }
        match dump.ids.as_ref().and_then(|ids| ids.get(tid)) {
            Some(id) => line.push_str(id),
            None => write!(line, "({},{})", tid.block, tid.offset).expect("a string"),
        }
        write!(line, ":{:08x}", score.to_bits()).expect("a string");
    }
    line
}

fn median(values: &[u64]) -> u64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied().unwrap_or(0)
}

fn percentile(values: &mut [u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let at = ((values.len() as f64 - 1.0) * p).round() as usize;
    values[at]
}

/// Runs `queries` once each on this thread's `engine`, measuring.
fn pass(
    engine: &Engine<'_>,
    queries: &[TraceQuery],
    records: &mut [Record],
    options: &Options,
    analyzer: &mut Option<Analyzer<'_>>,
    first: bool,
) -> Result<(), String> {
    for (query, record) in queries.iter().zip(records.iter_mut()) {
        if options.ranked {
            engine::walk::reset_counters();
            engine::walk::stats::reset();
            let events = analyzer
                .as_ref()
                .filter(|_| first)
                .map(|_| collect_events());
            take_touches();
            let started = Instant::now();
            let answer = engine.ranked(&query.text, options.k);
            let elapsed = started.elapsed().as_nanos() as u64;
            record.ranked_touches = take_touches();
            engine::walk::stats::observe(None);
            let answer = answer.map_err(|error| format!("{}: {error}", query.name))?;
            record.ranked_ns.push(elapsed);
            let line = answer_line(engine.dump, &answer.rows);
            if first {
                record.path = answer.path.name();
                record.scored = answer.scored;
                record.threshold =
                    (answer.rows.len() == options.k).then(|| answer.rows[options.k - 1].0);
                record.counters = engine::walk::stats::counters();
                record.chunk_loads = engine::walk::chunk_loads();
                record.positions = (
                    engine::walk::position_checks(),
                    engine::walk::position_reads(),
                );
                if let (Some(analyzer), Some(events)) = (analyzer.as_mut(), events) {
                    record.whatif = analyzer.judge(
                        &events.borrow(),
                        |index| engine.source_of(index as *const ()),
                        &answer.terms,
                        engine::walk::stats::counters().candidates,
                    )?;
                }
                record.ranked = Some(line);
            } else if record.ranked.as_deref() != Some(line.as_str()) {
                return Err(format!(
                    "{}: the ranked answer changed between passes",
                    query.name
                ));
            }
        }
        if options.count {
            take_touches();
            let started = Instant::now();
            let count = engine.count(&query.text);
            let elapsed = started.elapsed().as_nanos() as u64;
            record.count_touches = take_touches();
            let count = count.map_err(|error| format!("{}: {error}", query.name))?;
            record.count_ns.push(elapsed);
            let line = count.to_string();
            if first {
                record.count = Some(line);
            } else if record.count.as_deref() != Some(line.as_str()) {
                return Err(format!("{}: the count changed between passes", query.name));
            }
        }
    }
    Ok(())
}

/// Each query from fresh readers and empty caches, for its cold touches.
fn cold_pass(
    dump: &Dump,
    queries: &[TraceQuery],
    records: &mut [Record],
    options: &Options,
) -> Result<(), String> {
    for (query, record) in queries.iter().zip(records.iter_mut()) {
        if options.ranked {
            segment::cache::clear();
            take_touches();
            let engine = Engine::open(dump)?;
            engine.ranked(&query.text, options.k)?;
            record.cold_ranked = take_touches();
        }
        if options.count {
            segment::cache::clear();
            take_touches();
            let engine = Engine::open(dump)?;
            engine.count(&query.text)?;
            record.cold_count = take_touches();
        }
    }
    Ok(())
}

/// Queries a second over `threads` threads, each with its own readers and
/// caches as a backend has, after one untimed pass per thread.
fn throughput(
    dump: &Dump,
    queries: &[TraceQuery],
    options: &Options,
    threads: usize,
    ranked: bool,
) -> f64 {
    let next = AtomicUsize::new(0);
    let total = queries.len() * options.repeat.saturating_sub(1).max(1);
    let barrier = std::sync::Barrier::new(threads + 1);
    let elapsed = std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let engine = Engine::open(dump).expect("the dump opened once already");
                let run = |query: &TraceQuery| {
                    if ranked {
                        let _ = engine.ranked(&query.text, options.k);
                    } else {
                        let _ = engine.count(&query.text);
                    }
                };
                queries.iter().for_each(run);
                barrier.wait();
                loop {
                    let n = next.fetch_add(1, Ordering::Relaxed);
                    if n >= total {
                        break;
                    }
                    run(&queries[n % queries.len()]);
                }
                barrier.wait();
            });
        }
        barrier.wait();
        let started = Instant::now();
        barrier.wait();
        started.elapsed()
    });
    total as f64 / elapsed.as_secs_f64()
}

fn mean_touches(
    records: &[&Record],
    pick: impl Fn(&Record) -> &Touches,
) -> ([f64; AREAS], f64, f64) {
    let n = records.len().max(1) as f64;
    let mut areas = [0.0; AREAS];
    let mut accesses = 0.0;
    let mut pages = 0.0;
    for record in records {
        let touches = pick(record);
        for (slot, value) in areas.iter_mut().zip(touches.accesses) {
            *slot += value as f64 / n;
        }
        accesses += touches.total_accesses() as f64 / n;
        pages += touches.pages as f64 / n;
    }
    (areas, accesses, pages)
}

fn touch_table(
    out: &mut String,
    title: &str,
    styles: &[(String, Vec<&Record>)],
    pick: &dyn Fn(&Record) -> &Touches,
) {
    let _ = writeln!(
        out,
        "\n{title}: page touches per query, mean (accesses; distinct pages last)"
    );
    let _ = write!(out, "{:<12}", "style");
    for area in Area::ALL {
        let _ = write!(out, " {:>9}", short(area));
    }
    let _ = writeln!(out, " {:>9} {:>9}", "total", "distinct");
    for (style, records) in styles {
        let (areas, accesses, pages) = mean_touches(records, pick);
        let _ = write!(out, "{style:<12}");
        for value in areas {
            let _ = write!(out, " {value:>9.1}");
        }
        let _ = writeln!(out, " {accesses:>9.1} {pages:>9.1}");
    }
    // The same, folded into TIN's categories.
    let _ = writeln!(
        out,
        "  as TIN's categories (Metadata, Term Map, Footer, Payload, TF Tail, DL Sidecar, Positions, Liveness; doc table apart):"
    );
    let tin = [
        "Metadata",
        "Term Map",
        "Postings Footer",
        "Postings Payload",
        "Postings TF Tail",
        "DL Sidecar",
        "Positions",
        "Liveness Bitmap",
    ];
    for (style, records) in styles {
        let (areas, _, _) = mean_touches(records, pick);
        let mut sums = [0.0; 8];
        let mut apart = 0.0;
        for (area, value) in Area::ALL.iter().zip(areas) {
            match area.tin() {
                Some(name) => {
                    sums[tin.iter().position(|t| *t == name).expect("a category")] += value
                }
                None => apart += value,
            }
        }
        let _ = write!(out, "  {style:<10}");
        for (name, value) in tin.iter().zip(sums) {
            let _ = write!(out, " {name}={value:.1}");
        }
        let _ = writeln!(out, " DocTable={apart:.1}");
    }
}

fn short(area: Area) -> &'static str {
    match area {
        Area::Header => "header",
        Area::Dictionary => "dict",
        Area::OrdinalHeads => "ord.head",
        Area::OrdinalChunks => "ord.chunk",
        Area::Nibbles => "tf.nibble",
        Area::OrdinalLists => "ord.list",
        Area::Positions => "positions",
        Area::Documents => "doc.table",
        Area::Lengths => "lengths",
        Area::Classes => "classes",
        Area::PageTable => "page.tbl",
        Area::DeadList => "dead",
    }
}

fn run() -> Result<bool, String> {
    let options = options();
    let loaded = Instant::now();
    let dump = Dump::open(&options.dump)?;
    let mut queries = bench::read_trace(&options.trace)?;
    if let Some(styles) = &options.styles {
        queries.retain(|q| styles.contains(&q.style));
    }
    if let Some(limit) = options.limit {
        queries.truncate(limit);
    }
    eprintln!(
        "dump: {} segments, {} documents, {:.1} MB; {} queries; loaded in {:.1} s",
        dump.segments.len(),
        dump.documents(),
        dump.bytes() as f64 / 1e6,
        queries.len(),
        loaded.elapsed().as_secs_f64()
    );
    if dump.buffer_docs > 0 {
        eprintln!(
            "warning: the index's write buffer holds {} documents the dump lacks; scores will differ",
            dump.buffer_docs
        );
    }
    engine::set_blocks_probe(|| bench::paged::touches_so_far() as i64);
    let engine = Engine::open(&dump)?;
    let mut analyzer = if options.whatif {
        Some(Analyzer::new(&dump)?)
    } else {
        None
    };
    let mut records: Vec<Record> = queries.iter().map(|_| Record::default()).collect();
    for round in 0..options.repeat {
        pass(
            &engine,
            &queries,
            &mut records,
            &options,
            &mut analyzer,
            round == 0,
        )?;
    }
    // The first pass warms the caches; the others are timed.
    for record in &mut records {
        if options.repeat > 1 {
            if !record.ranked_ns.is_empty() {
                record.ranked_ns.remove(0);
            }
            if !record.count_ns.is_empty() {
                record.count_ns.remove(0);
            }
        }
    }
    if options.cold {
        cold_pass(&dump, &queries, &mut records, &options)?;
    }

    // Answers.
    let mut lines = String::new();
    for (query, record) in queries.iter().zip(&records) {
        if let Some(line) = &record.ranked {
            let _ = writeln!(lines, "{}\t{}\tranked\t{line}", query.name, query.style);
        }
        if let Some(line) = &record.count {
            let _ = writeln!(lines, "{}\t{}\tcount\t{line}", query.name, query.style);
        }
    }
    if let Some(path) = &options.out {
        std::fs::write(path, &lines).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    let mut matched = true;
    if let Some(path) = &options.expect {
        let expected = std::fs::read_to_string(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let expected: rustc_hash::FxHashMap<(String, String), String> = expected
            .lines()
            .filter_map(|line| {
                let mut f = line.splitn(4, '\t');
                let (name, _style, kind, answer) =
                    (f.next()?, f.next()?, f.next()?, f.next().unwrap_or(""));
                Some(((name.to_owned(), kind.to_owned()), answer.to_owned()))
            })
            .collect();
        let (mut same, mut differ, mut missing) = (0, 0, 0);
        for line in lines.lines() {
            let mut f = line.splitn(4, '\t');
            let (name, _style, kind, answer) = (
                f.next().unwrap(),
                f.next().unwrap(),
                f.next().unwrap(),
                f.next().unwrap_or(""),
            );
            match expected.get(&(name.to_owned(), kind.to_owned())) {
                Some(want) if want == answer => same += 1,
                Some(want) => {
                    differ += 1;
                    if differ <= 20 {
                        eprintln!("DIFF {name} {kind}\n  expected {want}\n  replayed {answer}");
                    }
                }
                None => missing += 1,
            }
        }
        eprintln!(
            "expect: {same} equal, {differ} different, {missing} not in {}",
            path.display()
        );
        matched = differ == 0 && missing == 0;
    }

    // Per style.
    let mut by_style: Vec<(String, Vec<&Record>)> = Vec::new();
    let mut style_queries: Vec<(String, Vec<TraceQuery>)> = Vec::new();
    for (query, record) in queries.iter().zip(&records) {
        match by_style.iter().position(|(s, _)| *s == query.style) {
            Some(at) => {
                by_style[at].1.push(record);
                style_queries[at].1.push(query.clone());
            }
            None => {
                by_style.push((query.style.clone(), vec![record]));
                style_queries.push((query.style.clone(), vec![query.clone()]));
            }
        }
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "latency per query, microseconds (median of {} timed passes); qps on one thread",
        options.repeat.saturating_sub(1).max(1)
    );
    let _ = writeln!(
        out,
        "{:<12} {:>7} | {:>9} {:>9} {:>9} {:>9} | {:>9} {:>9} {:>9} {:>9}",
        "style",
        "queries",
        "rank p50",
        "rank p99",
        "rank mean",
        "rank qps",
        "count p50",
        "count p99",
        "count mean",
        "count qps"
    );
    for (style, records) in &by_style {
        let mut ranked: Vec<u64> = records.iter().map(|r| median(&r.ranked_ns)).collect();
        let mut count: Vec<u64> = records.iter().map(|r| median(&r.count_ns)).collect();
        let rank_mean = ranked.iter().sum::<u64>() as f64 / ranked.len().max(1) as f64;
        let count_mean = count.iter().sum::<u64>() as f64 / count.len().max(1) as f64;
        let _ = writeln!(
            out,
            "{style:<12} {:>7} | {:>9.1} {:>9.1} {:>9.1} {:>9.0} | {:>9.1} {:>9.1} {:>9.1} {:>9.0}",
            records.len(),
            percentile(&mut ranked, 0.5) as f64 / 1e3,
            percentile(&mut ranked, 0.99) as f64 / 1e3,
            rank_mean / 1e3,
            if rank_mean > 0.0 {
                1e9 / rank_mean
            } else {
                0.0
            },
            percentile(&mut count, 0.5) as f64 / 1e3,
            percentile(&mut count, 0.99) as f64 / 1e3,
            count_mean / 1e3,
            if count_mean > 0.0 {
                1e9 / count_mean
            } else {
                0.0
            },
        );
    }
    let threads: Vec<usize> = options.threads.iter().copied().filter(|n| *n > 1).collect();
    if !threads.is_empty() {
        let _ = writeln!(
            out,
            "\nthroughput, queries a second, each thread with its own readers and caches"
        );
        for (style, queries) in &style_queries {
            for &n in &threads {
                let ranked = if options.ranked {
                    throughput(&dump, queries, &options, n, true)
                } else {
                    0.0
                };
                let count = if options.count {
                    throughput(&dump, queries, &options, n, false)
                } else {
                    0.0
                };
                let _ = writeln!(
                    out,
                    "{style:<12} {n:>3} threads: ranked {ranked:>9.0}  count {count:>9.0}"
                );
            }
        }
    }
    if options.ranked {
        touch_table(&mut out, "ranked, warm", &by_style, &|r: &Record| {
            &r.ranked_touches
        });
    }
    if options.count {
        touch_table(&mut out, "count, warm", &by_style, &|r: &Record| {
            &r.count_touches
        });
    }
    if options.cold {
        if options.ranked {
            touch_table(&mut out, "ranked, cold", &by_style, &|r: &Record| {
                &r.cold_ranked
            });
        }
        if options.count {
            touch_table(&mut out, "count, cold", &by_style, &|r: &Record| {
                &r.cold_count
            });
        }
    }
    if options.ranked {
        let _ = writeln!(
            out,
            "\nranked walk per style, summed over queries (threshold: mean k-th score where k rows were found)"
        );
        let _ = writeln!(
            out,
            "{:<12} {:>8} {:>8} {:>8} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}  paths",
            "style",
            "chunks",
            "ch.bound",
            "ch.subs",
            "loads",
            "subs",
            "subs.pr",
            "examined",
            "classes",
            "lengths",
            "scored",
            "pos.chk",
            "threshold"
        );
        for (style, records) in &by_style {
            let mut c = Counters::default();
            let (mut loads, mut scored, mut positions) = (0i64, 0usize, 0i64);
            let (mut threshold, mut thresholds) = (0.0f64, 0usize);
            let mut paths: Vec<(&str, usize)> = Vec::new();
            for r in records {
                c.chunks += r.counters.chunks;
                c.chunks_pruned_by_bound += r.counters.chunks_pruned_by_bound;
                c.chunks_pruned_by_subs += r.counters.chunks_pruned_by_subs;
                c.subs += r.counters.subs;
                c.subs_pruned += r.counters.subs_pruned;
                c.candidates += r.counters.candidates;
                c.class_reads += r.counters.class_reads;
                c.length_reads += r.counters.length_reads;
                loads += r.chunk_loads;
                scored += r.scored;
                positions += r.positions.0;
                if let Some(t) = r.threshold {
                    threshold += f64::from(t);
                    thresholds += 1;
                }
                match paths.iter_mut().find(|(p, _)| *p == r.path) {
                    Some(entry) => entry.1 += 1,
                    None => paths.push((r.path, 1)),
                }
            }
            let paths: Vec<String> = paths.iter().map(|(p, n)| format!("{p}={n}")).collect();
            let _ = writeln!(
                out,
                "{style:<12} {:>8} {:>8} {:>8} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9.3}  {}",
                c.chunks,
                c.chunks_pruned_by_bound,
                c.chunks_pruned_by_subs,
                loads,
                c.subs,
                c.subs_pruned,
                c.candidates,
                c.class_reads,
                c.length_reads,
                scored,
                positions,
                threshold / thresholds.max(1) as f64,
                paths.join(" ")
            );
        }
    }
    if options.whatif {
        let _ = writeln!(
            out,
            "\nwhat if sub-block bounds were tighter: of the sub-blocks with candidates the walk judged and kept,"
        );
        let _ = writeln!(
            out,
            "those a tighter bound skips, and the candidates the walk examined in them (of all it examined in kept sub-blocks)"
        );
        let _ = writeln!(
            out,
            "{:<12} {:>9} {:>9} {:>9} | {:>9} {:>21} | {:>9} {:>21} | {:>8}",
            "style",
            "visited",
            "pruned",
            "kept",
            "own: skip",
            "own: candidates",
            "exact:skip",
            "exact: candidates",
            "mismatch"
        );
        for (style, records) in &by_style {
            let mut total = WhatIf::default();
            for r in records {
                total.add(&r.whatif);
            }
            let kept = total.visited - total.pruned;
            let all = total.kept_examined.max(1) as f64;
            let _ = writeln!(
                out,
                "{style:<12} {:>9} {:>9} {:>9} | {:>9} {:>12} ({:>5.1}%) | {:>9} {:>12} ({:>5.1}%) | {:>8}",
                total.visited,
                total.pruned,
                kept,
                total.own,
                total.own_examined,
                100.0 * total.own_examined as f64 / all,
                total.exact,
                total.exact_examined,
                100.0 * total.exact_examined as f64 / all,
                total.mismatches
            );
        }
    }
    print!("{out}");

    if let Some(path) = &options.per_query {
        let mut text = String::from(
            "name\tstyle\tranked_us\tcount_us\tpath\tscored\tthreshold\tchunks\tchunks_pruned\tsubs\tsubs_pruned\twhatif_own\twhatif_exact\tranked_pages\tcount_pages",
        );
        for area in Area::ALL {
            let _ = write!(text, "\tranked_{}", area.name());
        }
        for area in Area::ALL {
            let _ = write!(text, "\tcount_{}", area.name());
        }
        text.push('\n');
        for (query, r) in queries.iter().zip(&records) {
            let _ = write!(
                text,
                "{}\t{}\t{:.1}\t{:.1}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                query.name,
                query.style,
                median(&r.ranked_ns) as f64 / 1e3,
                median(&r.count_ns) as f64 / 1e3,
                r.path,
                r.scored,
                r.threshold.map_or(String::new(), |t| t.to_string()),
                r.counters.chunks,
                r.counters.chunks_pruned_by_bound + r.counters.chunks_pruned_by_subs,
                r.counters.subs,
                r.counters.subs_pruned,
                r.whatif.own,
                r.whatif.exact,
                r.ranked_touches.total_accesses(),
                r.count_touches.total_accesses(),
            );
            for value in r
                .ranked_touches
                .accesses
                .iter()
                .chain(&r.count_touches.accesses)
            {
                let _ = write!(text, "\t{value}");
            }
            text.push('\n');
        }
        std::fs::write(path, text).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(matched)
}

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("replay: {error}");
            std::process::exit(2);
        }
    }
}
