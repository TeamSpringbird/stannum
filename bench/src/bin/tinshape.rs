// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Converts a dumped index to TIN's shape, checks it against the dump,
//! reports its size against the ordinal format, and replays a trace's
//! counts (and, with `--ranked`, its ranked top k) over it.
//!
//! ```text
//! cargo run -p bench --release --bin tinshape -- --dump DIR --out DIR \
//!     [--block 128] [--no-paged] [--no-ef-groups] [--no-sparse] [--fixed-tf] [--grid-density N] \
//!     [--grid-min-postings N] [--inline-lengths MAXDF] [--subset ROWS [--rows-per-page R]] \
//!     [--no-verify] [--no-write] [--label NAME] \
//!     [--trace trace.tsv --expect pg.tsv [--ranked] [--k 10] [--repeat 3] \
//!      [--per-query FILE]]
//! ```
//!
//! The size report goes to `OUT/sizes-LABEL.md` and `.json`; the replay
//! prints per style the count (and ranked) latency, p50 and p99 of the
//! median of `--repeat` passes on one thread, and the pages each query
//! touches, by the areas TIN's EXPLAIN names, counted in pages of the
//! paged source's size. Every row is visible, as in the ordinal replay.
//!
//! `--subset ROWS` converts an STN3 segment of the dump's first ROWS
//! documents instead, rebuilt by STN3's builder (a small table's size
//! against STN3's), laid out `R` rows to a page with `--rows-per-page`.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use bench::dump::Dump;
use bench::replay::Engine;
use bench::tinshape::{Converted, Pages, convert, verify};
use engine::tinshape::{self as tin, NoTouch, Part, Touch as _};
use segment::tinshape::postings::Options;
use segment::tinshape::segment::Segment;

struct Args {
    dump: PathBuf,
    out: PathBuf,
    options: Options,
    verify: bool,
    write: bool,
    label: String,
    trace: Option<PathBuf>,
    expect: Option<PathBuf>,
    ranked: bool,
    k: usize,
    repeat: usize,
    per_query: Option<PathBuf>,
    /// Convert an STN3 segment of the dump's first this many documents.
    subset: Option<u32>,
    /// With --subset: lay the documents out this many rows to a page.
    rows_per_page: Option<u16>,
}

fn usage() -> ! {
    eprintln!(
        "usage: tinshape --dump DIR --out DIR [--block N] [--no-paged] [--no-ef-groups] \
         [--no-sparse] [--fixed-tf] [--grid-density N] [--grid-min-postings N] \
         [--subset ROWS [--rows-per-page R]] [--no-verify] [--no-write] [--label NAME] \
         [--trace FILE --expect FILE [--ranked] [--k N] [--repeat N] [--per-query FILE]]"
    );
    std::process::exit(2)
}

fn args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        dump: PathBuf::new(),
        out: PathBuf::new(),
        options: Options::default(),
        verify: true,
        write: true,
        label: String::new(),
        trace: None,
        expect: None,
        ranked: false,
        k: 10,
        repeat: 3,
        per_query: None,
        subset: None,
        rows_per_page: None,
    };
    let value = |it: &mut dyn Iterator<Item = String>| it.next().unwrap_or_else(|| usage());
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dump" => a.dump = value(&mut it).into(),
            "--out" => a.out = value(&mut it).into(),
            "--block" => a.options.block_size = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--no-paged" => a.options.paged = false,
            "--no-ef-groups" => a.options.ef_groups = false,
            "--no-sparse" => a.options.sparse = false,
            "--fixed-tf" => a.options.adaptive_tf = false,
            "--inline-lengths" => {
                a.options.inline_lengths_max_df = value(&mut it).parse().unwrap_or_else(|_| usage())
            }
            "--grid-min-postings" => {
                a.options.grid_min_postings = value(&mut it).parse().unwrap_or_else(|_| usage())
            }
            "--grid-density" => {
                a.options.grid_density = value(&mut it).parse().unwrap_or_else(|_| usage())
            }
            "--no-verify" => a.verify = false,
            "--no-write" => a.write = false,
            "--label" => a.label = value(&mut it),
            "--trace" => a.trace = Some(value(&mut it).into()),
            "--expect" => a.expect = Some(value(&mut it).into()),
            "--ranked" => a.ranked = true,
            "--k" => a.k = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--repeat" => a.repeat = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--per-query" => a.per_query = Some(value(&mut it).into()),
            "--subset" => a.subset = Some(value(&mut it).parse().unwrap_or_else(|_| usage())),
            "--rows-per-page" => {
                a.rows_per_page = Some(value(&mut it).parse().unwrap_or_else(|_| usage()))
            }
            _ => usage(),
        }
    }
    if a.dump.as_os_str().is_empty() || a.out.as_os_str().is_empty() {
        usage();
    }
    if a.label.is_empty() {
        let o = &a.options;
        a.label = format!(
            "b{}{}{}{}{}{}",
            o.block_size,
            if o.grid_density > 0 {
                format!("-g{}", o.grid_density)
            } else {
                String::new()
            },
            if o.paged { "" } else { "-nopaged" },
            if o.ef_groups { "" } else { "-noefg" },
            if o.sparse { "" } else { "-nosparse" },
            if o.adaptive_tf { "" } else { "-fixedtf" },
        );
    }
    a.repeat = a.repeat.max(1);
    a
}

/// Document-frequency buckets of the size report.
const DF_BUCKETS: [(u32, u32, &str); 8] = [
    (1, 1, "1"),
    (2, 7, "2-7"),
    (8, 63, "8-63"),
    (64, 511, "64-511"),
    (512, 4095, "512-4K"),
    (4096, 32767, "4K-32K"),
    (32768, 262_143, "32K-256K"),
    (262_144, u32::MAX, "256K+"),
];

#[derive(Default, Clone, Copy)]
struct Bucket {
    terms: u64,
    postings: u64,
    payload: u64,
    footer: u64,
    header: u64,
    tf: u64,
    stn3: u64,
    positions: u64,
}

fn mb(bytes: u64) -> String {
    format!("{:.2}", bytes as f64 / 1e6)
}

fn report(args: &Args, converted: &[Converted], dump: &Dump) -> String {
    let mut md = String::new();
    let mut json = String::from("{\n");
    let o = &args.options;
    let _ = writeln!(
        md,
        "# TIN-shape size report: {}\n\nDump `{}`; options: block size {}, paged containers {}, \
         group Elias-Fano {}, whole-term Elias-Fano {}, adaptive TF widths {}.\n",
        args.label,
        dump.dir.display(),
        o.block_size,
        o.paged,
        o.ef_groups,
        o.sparse,
        o.adaptive_tf
    );
    // Totals over segments.
    let mut s = segment::tinshape::segment::BuildStats::default();
    let (
        mut stn3_dict,
        mut stn3_ord,
        mut stn3_pay,
        mut stn3_offsets,
        mut stn3_lengths,
        mut stn3_classes,
        mut stn3_pages,
        mut stn3_header,
    ) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut tns_total, mut stn3_total, mut documents, mut tokens) = (0u64, 0u64, 0u64, 0u64);
    let mut buckets = [Bucket::default(); DF_BUCKETS.len()];
    let mut dense: Vec<(u32, u64, u64)> = Vec::new();
    for c in converted {
        let t = &c.stats;
        s.terms += t.terms;
        s.postings += t.postings;
        s.header += t.header;
        s.dictionary += t.dictionary;
        s.record_headers += t.record_headers;
        s.footer += t.footer;
        s.payload += t.payload;
        s.tf += t.tf;
        s.inline_lengths += t.inline_lengths;
        s.positions += t.positions;
        s.docset += t.docset;
        s.lengths += t.lengths;
        s.liveness += t.liveness;
        s.blocks += t.blocks;
        s.sparse_terms += t.sparse_terms;
        s.single_terms += t.single_terms;
        for k in 0..3 {
            s.kinds[k] += t.kinds[k];
            s.kind_bytes[k] += t.kind_bytes[k];
        }
        let x = &c.stn3;
        stn3_header += x.header as u64;
        stn3_dict += x.dictionary as u64;
        stn3_ord += x.ordinals as u64;
        stn3_pay += x.payload as u64;
        stn3_offsets += x.offsets as u64;
        stn3_lengths += x.lengths as u64;
        stn3_classes += x.classes as u64;
        stn3_pages += x.pages as u64;
        tns_total += c.blob.len() as u64;
        stn3_total += (x.header
            + x.dictionary
            + x.ordinals
            + x.payload
            + x.offsets
            + x.lengths
            + x.classes
            + x.pages) as u64;
        documents += u64::from(c.documents);
        tokens += c.tokens;
        for term in &c.terms {
            let i = DF_BUCKETS
                .iter()
                .position(|(lo, hi, _)| term.df >= *lo && term.df <= *hi)
                .expect("a bucket");
            let b = &mut buckets[i];
            b.terms += 1;
            b.postings += u64::from(term.df);
            b.payload += term.tns.payload as u64;
            b.footer += term.tns.footer as u64;
            b.header += term.tns.header as u64;
            b.tf += term.tns.tf as u64;
            b.stn3 += u64::from(term.stn3_ordinals);
            b.positions += u64::from(term.positions);
            if u64::from(term.df) * 4 >= u64::from(c.documents) {
                dense.push((term.df, term.tns.payload as u64, u64::from(c.documents)));
            }
        }
    }
    let postings = s.postings;
    let _ = writeln!(md, "## Bytes per area\n");
    let _ = writeln!(md, "| area | STN3 (MB) | TNS1 (MB) | TNS1 / STN3 |");
    let _ = writeln!(md, "| --- | ---: | ---: | ---: |");
    let tns_postings = s.record_headers + s.footer + s.payload + s.tf + s.inline_lengths;
    let rows: Vec<(&str, u64, u64)> = vec![
        ("header / metadata", stn3_header, s.header),
        ("term map (dictionary)", stn3_dict, s.dictionary),
        (
            "postings: all (STN3 ordinal streams incl. bounds and nibbles)",
            stn3_ord,
            tns_postings,
        ),
        ("  postings payload (ctid sets)", 0, s.payload),
        (
            "  postings footer (record headers, group directories, block frontiers)",
            0,
            s.record_headers + s.footer,
        ),
        ("  TF tail", 0, s.tf),
        ("  inline lengths (rare terms)", 0, s.inline_lengths),
        ("positions", stn3_pay, s.positions),
        (
            "document table / document set",
            stn3_offsets + stn3_pages,
            s.docset,
        ),
        (
            "lengths (DL sidecar; STN3 lengths + classes)",
            stn3_lengths + stn3_classes,
            s.lengths,
        ),
        ("liveness bitmap (empty: no dead rows)", 0, s.liveness),
        ("total", stn3_total, tns_total),
    ];
    for (name, a, b) in &rows {
        let ratio = if *a > 0 {
            format!("{:.3}", *b as f64 / *a as f64)
        } else {
            String::new()
        };
        let _ = writeln!(
            md,
            "| {name} | {} | {} | {ratio} |",
            if *a > 0 { mb(*a) } else { String::new() },
            mb(*b)
        );
    }
    let _ = writeln!(json, "  \"label\": \"{}\",", args.label);
    let _ = writeln!(
        json,
        "  \"options\": {{\"block_size\": {}, \"paged\": {}, \"ef_groups\": {}, \"sparse\": {}, \"adaptive_tf\": {}}},",
        o.block_size, o.paged, o.ef_groups, o.sparse, o.adaptive_tf
    );
    let _ = writeln!(
        json,
        "  \"documents\": {documents}, \"tokens\": {tokens}, \"postings\": {postings}, \"terms\": {},",
        s.terms
    );
    let _ = writeln!(
        json,
        "  \"stn3\": {{\"header\": {stn3_header}, \"dictionary\": {stn3_dict}, \"ordinals\": {stn3_ord}, \"positions\": {stn3_pay}, \"offsets\": {stn3_offsets}, \"pages\": {stn3_pages}, \"lengths\": {stn3_lengths}, \"classes\": {stn3_classes}, \"total\": {stn3_total}}},"
    );
    let _ = writeln!(
        json,
        "  \"tns1\": {{\"header\": {}, \"dictionary\": {}, \"record_headers\": {}, \"footer\": {}, \"payload\": {}, \"tf\": {}, \"positions\": {}, \"docset\": {}, \"lengths\": {}, \"liveness\": {}, \"total\": {tns_total}, \"blocks\": {}, \"single_terms\": {}, \"sparse_terms\": {}, \"group_kinds\": {:?}, \"group_kind_bytes\": {:?}}},",
        s.header,
        s.dictionary,
        s.record_headers,
        s.footer,
        s.payload,
        s.tf,
        s.positions,
        s.docset,
        s.lengths,
        s.liveness,
        s.blocks,
        s.single_terms,
        s.sparse_terms,
        s.kinds,
        s.kind_bytes
    );
    let per_pair = |bytes: u64| bytes as f64 / postings as f64;
    let per_token = |bytes: u64| bytes as f64 / tokens as f64;
    let _ = writeln!(
        md,
        "\n{documents} documents, {} terms, {postings} (term, document) pairs, {tokens} tokens.\n",
        s.terms
    );
    let _ = writeln!(md, "| per unit | STN3 | TNS1 | TIN (published/observed) |");
    let _ = writeln!(md, "| --- | ---: | ---: | ---: |");
    let _ = writeln!(
        md,
        "| bytes per (term, document) pair | {:.2} | {:.2} | ~8.3 |",
        per_pair(stn3_total),
        per_pair(tns_total)
    );
    let _ = writeln!(
        md,
        "| bytes per token (incl. positions) | {:.2} | {:.2} | ~3.4 |",
        per_token(stn3_total),
        per_token(tns_total)
    );
    let _ = writeln!(
        md,
        "| bytes per pair, without positions | {:.2} | {:.2} | |",
        per_pair(stn3_total - stn3_pay),
        per_pair(tns_total - s.positions)
    );
    let _ = writeln!(
        md,
        "| bytes per document | {:.1} | {:.1} | ~340 (SE 150M) |",
        stn3_total as f64 / documents as f64,
        tns_total as f64 / documents as f64
    );
    // 150M extrapolation: the ratio to STN3's measured 150M size, and a
    // per-unit projection.
    let ratio = tns_total as f64 / stn3_total as f64;
    let _ = writeln!(md, "\n## 150M extrapolation\n");
    let _ = writeln!(
        md,
        "- By ratio to STN3's measured 47 GB: {:.3} x 47 GB = **{:.1} GB** (TIN 50.7 GB).",
        ratio,
        47.0 * ratio
    );
    let linear = (tns_total - s.dictionary) as f64 * 150e6 / documents as f64
        + s.dictionary as f64 * (150e6 / documents as f64).powf(0.6);
    let linear_stn3 = (stn3_total - stn3_dict) as f64 * 150e6 / documents as f64
        + stn3_dict as f64 * (150e6 / documents as f64).powf(0.6);
    let _ = writeln!(
        md,
        "- Per unit (everything but the term map linear in documents, the term map by Heaps' law with exponent 0.6): TNS1 **{:.1} GB**, STN3 {:.1} GB by the same rule (measured: 47 GB).",
        linear / 1e9,
        linear_stn3 / 1e9
    );
    let _ = writeln!(
        json,
        "  \"extrapolated_150m_gb\": {{\"ratio_to_stn3\": {:.2}, \"per_unit\": {:.2}, \"per_unit_stn3\": {:.2}}},",
        47.0 * ratio,
        linear / 1e9,
        linear_stn3 / 1e9
    );
    // By document frequency.
    let _ = writeln!(md, "\n## Bits per posting by document frequency\n");
    let _ = writeln!(
        md,
        "Payload is the ctid set alone (TIN's \"bits per posting\"); footer adds the record header, group directory and block frontiers. STN3's ordinal stream includes its 4-bit bucket nibbles and chunk bounds; \"STN3 - nibbles\" takes 4 bits per posting off.\n"
    );
    let _ = writeln!(
        md,
        "| df | terms | postings | payload | +footer | TF tail | TNS1 postings total | STN3 ordinals | STN3 - nibbles | positions |"
    );
    let _ = writeln!(
        md,
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    let _ = writeln!(json, "  \"by_df\": [");
    for (i, (_, _, name)) in DF_BUCKETS.iter().enumerate() {
        let b = buckets[i];
        if b.postings == 0 {
            continue;
        }
        let bits = |bytes: u64| bytes as f64 * 8.0 / b.postings as f64;
        let _ = writeln!(
            md,
            "| {name} | {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} |",
            b.terms,
            b.postings,
            bits(b.payload),
            bits(b.payload + b.footer + b.header),
            bits(b.tf),
            bits(b.payload + b.footer + b.header + b.tf),
            bits(b.stn3),
            bits(b.stn3) - 4.0,
            bits(b.positions)
        );
        let _ = writeln!(
            json,
            "    {{\"df\": \"{name}\", \"terms\": {}, \"postings\": {}, \"payload\": {}, \"footer\": {}, \"header\": {}, \"tf\": {}, \"stn3_ordinals\": {}, \"positions\": {}}}{}",
            b.terms,
            b.postings,
            b.payload,
            b.footer,
            b.header,
            b.tf,
            b.stn3,
            b.positions,
            if i + 1 < DF_BUCKETS.len() { "," } else { "" }
        );
    }
    let _ = writeln!(json, "  ],");
    dense.sort_by_key(|d| std::cmp::Reverse(d.0));
    let _ = writeln!(
        md,
        "\nDense terms (df at least a quarter of the documents): payload bits per posting\n"
    );
    let _ = writeln!(md, "| df | share | payload bits/posting |");
    let _ = writeln!(md, "| ---: | ---: | ---: |");
    for (df, payload, docs) in dense.iter().take(12) {
        let _ = writeln!(
            md,
            "| {df} | {:.0}% | {:.2} |",
            *df as f64 * 100.0 / *docs as f64,
            *payload as f64 * 8.0 / *df as f64
        );
    }
    let _ = writeln!(
        md,
        "\nGroup containers chosen: grid {} ({} MB), Elias-Fano {} ({} MB), paged {} ({} MB); whole-term Elias-Fano terms {}; single-posting terms {}; footer blocks {}.",
        s.kinds[0],
        mb(s.kind_bytes[0]),
        s.kinds[1],
        mb(s.kind_bytes[1]),
        s.kinds[2],
        mb(s.kind_bytes[2]),
        s.sparse_terms,
        s.single_terms,
        s.blocks
    );
    let _ = writeln!(
        json,
        "  \"dense_terms\": [{}]",
        dense
            .iter()
            .take(12)
            .map(|(df, p, _)| format!("[{df}, {p}]"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    json.push_str("}\n");
    let _ = std::fs::write(args.out.join(format!("sizes-{}.json", args.label)), &json);
    md
}

fn percentile(values: &mut [u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[((values.len() as f64 - 1.0) * p).round() as usize]
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

/// A ranked answer with the walk's candidates scored, windows and windows
/// pruned.
type RankedRun = (Vec<(f32, segment::Tid)>, [u64; 5]);

fn ranked(
    segments: &[Segment<'_>],
    node: &tin::Node,
    names: &[String],
    scorers: &[(String, engine::bm25::TermScorer)],
    k: usize,
    touch: &mut impl tin::Touch,
) -> Result<RankedRun, String> {
    let mut rows = Vec::new();
    let mut counts = [0u64; 5];
    for segment in segments {
        let answer =
            tin::top_k(segment, node, names, scorers, k, touch).map_err(|e| e.to_string())?;
        rows.extend(answer.rows);
        for (c, v) in counts.iter_mut().zip([
            answer.scored,
            answer.windows,
            answer.windows_pruned,
            answer.candidates,
            answer.position_checks,
        ]) {
            *c += v;
        }
    }
    rows.sort_by(engine::walk::rank);
    rows.truncate(k);
    Ok((rows, counts))
}

fn replay(args: &Args, dump: &Dump, blobs: &[Vec<u8>]) -> Result<bool, String> {
    let trace = bench::read_trace(args.trace.as_ref().expect("a trace"))?;
    let engine = Engine::open(dump)?;
    let segments: Vec<Segment<'_>> = blobs
        .iter()
        .map(|b| Segment::parse(b).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    let expected: rustc_hash::FxHashMap<(String, String), String> = match &args.expect {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .lines()
            .filter_map(|line| {
                let f: Vec<&str> = line.splitn(4, '\t').collect();
                (f.len() == 4).then(|| ((f[0].to_owned(), f[2].to_owned()), f[3].to_owned()))
            })
            .collect(),
        None => Default::default(),
    };
    struct Row {
        style: String,
        count_ns: u64,
        ranked_ns: u64,
        count_pages: Pages,
        ranked_pages: Pages,
        scored: u64,
        windows: u64,
        pruned: u64,
        candidates: u64,
        checks: u64,
    }
    let mut rows: Vec<Row> = Vec::new();
    let (mut same, mut differ, mut unsupported) = (0usize, 0usize, 0usize);
    let mut per_query = String::from(
        "name\tstyle\tcount_us\tranked_us\tcount_pages\tranked_pages\tscored\tcandidates\tchecks\n",
    );
    for q in &trace {
        let query = engine.parse(&q.text)?;
        let mut names = Vec::new();
        let Some(node) = tin::lower(&query, &mut names) else {
            unsupported += 1;
            eprintln!("unsupported: {} {}", q.name, q.text);
            continue;
        };
        let mut row = Row {
            style: q.style.clone(),
            count_ns: 0,
            ranked_ns: 0,
            count_pages: Pages::default(),
            ranked_pages: Pages::default(),
            scored: 0,
            windows: 0,
            pruned: 0,
            candidates: 0,
            checks: 0,
        };
        // Count: a recorded pass for pages and the answer, then timed ones.
        let mut total = 0u64;
        for segment in &segments {
            row.count_pages.touch(Part::Metadata, 0, 1);
            total += tin::count(segment, &node, &names, &mut row.count_pages)
                .map_err(|e| e.to_string())?;
        }
        let mut times = Vec::with_capacity(args.repeat);
        for _ in 0..args.repeat {
            let start = Instant::now();
            let mut t = 0u64;
            for segment in &segments {
                t += tin::count(segment, &node, &names, &mut NoTouch).map_err(|e| e.to_string())?;
            }
            times.push(start.elapsed().as_nanos() as u64);
            std::hint::black_box(t);
        }
        row.count_ns = percentile(&mut times, 0.5);
        let answer = total.to_string();
        if let Some(want) = expected.get(&(q.name.clone(), "count".to_owned())) {
            if *want == answer {
                same += 1;
            } else {
                differ += 1;
                eprintln!("DIFF {} count: expected {want}, got {answer}", q.name);
            }
        }
        if args.ranked {
            let scorer = engine.scorer(&q.text)?;
            let mut snode = names.clone();
            let _ = &mut snode;
            for segment in &segments {
                let _ = segment;
                row.ranked_pages.touch(Part::Metadata, 0, 1);
            }
            let (top, [scored, windows, pruned, candidates, checks]) = ranked(
                &segments,
                &node,
                &names,
                &scorer.terms,
                args.k,
                &mut row.ranked_pages,
            )?;
            row.scored = scored;
            row.windows = windows;
            row.pruned = pruned;
            row.candidates = candidates;
            row.checks = checks;
            let mut times = Vec::with_capacity(args.repeat);
            for _ in 0..args.repeat {
                let start = Instant::now();
                let r = ranked(
                    &segments,
                    &node,
                    &names,
                    &scorer.terms,
                    args.k,
                    &mut NoTouch,
                )?;
                times.push(start.elapsed().as_nanos() as u64);
                std::hint::black_box(r);
            }
            row.ranked_ns = percentile(&mut times, 0.5);
            let line = answer_line(dump, &top);
            if let Some(want) = expected.get(&(q.name.clone(), "ranked".to_owned())) {
                if *want == line {
                    same += 1;
                } else {
                    differ += 1;
                    eprintln!(
                        "DIFF {} ranked\n  expected {want}\n  got      {line}",
                        q.name
                    );
                }
            }
        }
        let _ = writeln!(
            per_query,
            "{}\t{}\t{:.1}\t{:.1}\t{}\t{}\t{}\t{}\t{}",
            q.name,
            q.style,
            row.count_ns as f64 / 1e3,
            row.ranked_ns as f64 / 1e3,
            row.count_pages.total(),
            row.ranked_pages.total(),
            row.scored,
            row.candidates,
            row.checks
        );
        rows.push(row);
    }
    if let Some(path) = &args.per_query {
        std::fs::write(path, per_query).map_err(|e| e.to_string())?;
    }
    println!("expect: {same} equal, {differ} different; {unsupported} queries unsupported");
    let mut styles: Vec<String> = rows.iter().map(|r| r.style.clone()).collect();
    styles.sort();
    styles.dedup();
    println!(
        "latency per query, microseconds (median of {} passes), one thread",
        args.repeat
    );
    println!(
        "{:<12} {:>7} | {:>9} {:>9} {:>9} | {:>9} {:>9} {:>9}",
        "style",
        "queries",
        "count p50",
        "count p99",
        "count mean",
        "rank p50",
        "rank p99",
        "rank mean"
    );
    for style in &styles {
        let of: Vec<&Row> = rows.iter().filter(|r| &r.style == style).collect();
        let mut c: Vec<u64> = of.iter().map(|r| r.count_ns).collect();
        let mut r: Vec<u64> = of.iter().map(|r| r.ranked_ns).collect();
        let mean = |v: &[u64]| v.iter().sum::<u64>() as f64 / v.len().max(1) as f64 / 1e3;
        println!(
            "{:<12} {:>7} | {:>9.1} {:>9.1} {:>9.1} | {:>9.1} {:>9.1} {:>9.1}",
            style,
            of.len(),
            percentile(&mut c, 0.5) as f64 / 1e3,
            percentile(&mut c, 0.99) as f64 / 1e3,
            mean(&c),
            percentile(&mut r, 0.5) as f64 / 1e3,
            percentile(&mut r, 0.99) as f64 / 1e3,
            mean(&r)
        );
    }
    for (kind, pick) in [("count", 0), ("ranked", 1)] {
        if pick == 1 && !args.ranked {
            continue;
        }
        println!("\n{kind}: distinct pages touched per query, mean, by TIN's areas");
        let mut header = format!("{:<12}", "style");
        for part in Part::ALL {
            let _ = write!(header, " {:>16}", part.name());
        }
        println!("{header} {:>8}", "total");
        for style in &styles {
            let of: Vec<&Row> = rows.iter().filter(|r| &r.style == style).collect();
            let mut line = format!("{style:<12}");
            let mut total = 0.0;
            for part in Part::ALL {
                let v = of
                    .iter()
                    .map(|r| {
                        if pick == 0 {
                            r.count_pages.distinct(part)
                        } else {
                            r.ranked_pages.distinct(part)
                        }
                    })
                    .sum::<usize>() as f64
                    / of.len().max(1) as f64;
                total += v;
                let _ = write!(line, " {v:>16.1}");
            }
            println!("{line} {total:>8.1}");
        }
    }
    if args.ranked {
        println!(
            "\nranked walk per style, summed: candidates examined, scored exactly, positions checked; windows, windows pruned"
        );
        for style in &styles {
            let of: Vec<&Row> = rows.iter().filter(|r| &r.style == style).collect();
            println!(
                "{:<12} examined {:>10} scored {:>10} checked {:>9} windows {:>9} pruned {:>9}",
                style,
                of.iter().map(|r| r.candidates).sum::<u64>(),
                of.iter().map(|r| r.scored).sum::<u64>(),
                of.iter().map(|r| r.checks).sum::<u64>(),
                of.iter().map(|r| r.windows).sum::<u64>(),
                of.iter().map(|r| r.pruned).sum::<u64>()
            );
        }
    }
    Ok(differ == 0)
}

fn run() -> Result<bool, String> {
    let args = args();
    let dump = Dump::open(&args.dump)?;
    std::fs::create_dir_all(&args.out).map_err(|e| e.to_string())?;
    let mut converted = Vec::new();
    // TNS_LIST_TERMS=MIN,MAX,EVERY prints every EVERY-th lowercase term of
    // MIN to MAX postings, with its df, and stops: a lab aid for building
    // single-term traces by document frequency.
    if let Ok(spec) = std::env::var("TNS_LIST_TERMS") {
        let v: Vec<u32> = spec.split(',').map(|x| x.parse().unwrap()).collect();
        let reader = segment::segment::Reader::parse(&dump.segments[0].blob).unwrap();
        let dictionary = reader.dictionary().unwrap();
        let mut i = 0u32;
        for block in 0..dictionary.index().blocks() {
            for (term, entry, _) in dictionary.block_sizes(block).unwrap() {
                if entry.df >= v[0]
                    && entry.df <= v[1]
                    && term.chars().all(|c| c.is_ascii_lowercase())
                {
                    i += 1;
                    if i % v[2] == 0 {
                        println!("{term}	{}", entry.df);
                    }
                }
            }
        }
        return Ok(true);
    }
    let mut dump = dump;
    if let Some(n) = args.subset {
        for dumped in &mut dump.segments {
            let blob = bench::tinshape::subset(dumped, n, args.rows_per_page)?;
            dumped.documents = n.min(dumped.documents);
            dumped.blob = std::sync::Arc::new(blob);
            dumped.dead = None;
        }
    }
    for dumped in &dump.segments {
        let start = Instant::now();
        let c = convert(dumped, args.options)?;
        eprintln!(
            "segment generation {}: {} documents converted in {:.1} s, {} -> {} bytes",
            dumped.generation,
            c.documents,
            start.elapsed().as_secs_f64(),
            dumped.blob.len(),
            c.blob.len()
        );
        if args.verify {
            let start = Instant::now();
            let postings = verify(dumped, &c.blob)?;
            eprintln!(
                "  verified {postings} postings exactly (ctids, buckets, positions, lengths) in {:.1} s",
                start.elapsed().as_secs_f64()
            );
        }
        if args.write {
            let path = args
                .out
                .join(format!("gen{}-{}.tns", dumped.generation, args.label));
            std::fs::write(&path, &c.blob).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        converted.push(c);
    }
    let md = report(&args, &converted, &dump);
    std::fs::write(args.out.join(format!("sizes-{}.md", args.label)), &md)
        .map_err(|e| e.to_string())?;
    print!("{md}");
    if args.trace.is_some() {
        let blobs: Vec<Vec<u8>> = converted.into_iter().map(|c| c.blob).collect();
        return replay(&args, &dump, &blobs);
    }
    Ok(true)
}

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("tinshape: {error}");
            std::process::exit(1);
        }
    }
}
