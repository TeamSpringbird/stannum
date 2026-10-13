// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Elias-Fano decoding measured on a dump's own lists.
//!
//! `sample` takes the Elias-Fano group containers of the first `--segments`
//! segments (and the sparse lists of rare terms): a uniform sample of them
//! all, and samples grouped by low-bit width (the list's density, about
//! `2^-l` of its universe) and by length, written to a file with each list's
//! bytes copied together (so that timings are of decoding, not of cache
//! misses across the segment). `time` decodes each group by the portable
//! body and by every kernel level this CPU runs, into a vector (as a walk's
//! lists are) and into a cleared row (as a disjunction's are): per member,
//! the median of seven batches of at least 20 ms. `count` decodes one group
//! at one level `--passes` times, for `time -l`'s instructions retired (run
//! it with 0 passes too and take the difference). `time` and `count` take a
//! dump instead of a sample file (`--dump DIR [--segments N]`), sampling it
//! first (about a minute a segment at 150M rows).
//!
//! ```text
//! cargo run --release -p bench --bin tnsef -- time --dump DIR [--segments 1]
//! tnsef sample --dump DIR --out FILE [--segments 1]
//! tnsef time --sample FILE [--group PREFIX]
//! tnsef count --sample FILE --group NAME --level L --mode vector|row --passes P
//! ```

use std::hint::black_box;
use std::time::{Duration, Instant};

use engine::tinshape::kernels::{self, Level};
use segment::tinshape::ef::Ef;
use segment::tinshape::postings::{Form, KIND_EF};
use segment::tinshape::segment::Segment;

/// Lists per group at most, spread over the segments.
const KEEP: usize = 40_000;
/// Lists in the uniform sample.
const KEEP_ALL: usize = 200_000;

/// A list as sampled: its bytes, length, universe and row words.
#[derive(Clone)]
struct Sampled {
    bytes: Vec<u8>,
    n: u32,
    universe: u32,
    words: u32,
}

struct Group {
    name: String,
    /// Share of all container members, in percent.
    share: f64,
    lists: Vec<Sampled>,
}

fn thin(lists: Vec<Sampled>, keep: usize) -> Vec<Sampled> {
    let step = lists.len().div_ceil(keep).max(1);
    lists.into_iter().step_by(step).collect()
}

fn sample(args: &[String]) {
    let (mut dump, mut out, mut segments) = (String::new(), String::new(), 1usize);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dump" => dump = it.next().expect("a directory").clone(),
            "--out" => out = it.next().expect("a file").clone(),
            "--segments" => segments = it.next().expect("a count").parse().expect("a count"),
            _ => panic!("usage: tnsef sample --dump DIR --out FILE [--segments N]"),
        }
    }
    std::fs::write(&out, sampled(&dump, segments)).expect("the sample written");
    eprintln!("sample written to {out}");
}

/// A sample file's bytes: the groups of the first `segments` of `dump`.
fn sampled(dump: &str, segments: usize) -> Vec<u8> {
    let dump = std::path::PathBuf::from(dump);
    let manifest = std::fs::read_to_string(dump.join("manifest.tsv")).expect("a manifest");
    let mut all: Vec<Sampled> = Vec::new();
    let mut sparse: Vec<Sampled> = Vec::new();
    for line in manifest
        .lines()
        .filter(|l| l.starts_with("segment\t"))
        .take(segments)
    {
        let blob = line.split('\t').nth(3).expect("a blob");
        let bytes = std::fs::read(dump.join(blob)).expect("a blob");
        let segment = Segment::parse(&bytes).expect("a TNS1 segment");
        let geometry = &segment.docs.geometry;
        let copy = |ef: &Ef<'_>, universe: u32, words: u32| Sampled {
            bytes: [ef.lows(), ef.highs()].concat(),
            n: ef.len() as u32,
            universe,
            words,
        };
        for item in segment.dictionary().iter() {
            let (_, entry) = item.expect("a term map entry");
            let term = segment.resolve(entry).expect("a postings record");
            let p = term.postings;
            match &p.form {
                Form::Grouped(dir) => {
                    let entries = dir.entries(&geometry.groups).expect("a directory");
                    for e in entries.iter().filter(|e| e.kind == KIND_EF) {
                        let group = geometry.groups[e.index as usize];
                        let body = p.container(e).all().expect("a container");
                        let ef = Ef::parse(body, e.count as usize, group.slots()).expect("a list");
                        all.push(copy(&ef, group.slots(), group.words() as u32));
                    }
                }
                Form::Sparse(list) => {
                    sparse.push(copy(list, geometry.slots, geometry.slots.div_ceil(64)));
                }
                Form::Single(_) => {}
            }
        }
        eprintln!(
            "{blob}: {} containers, {} sparse lists so far",
            all.len(),
            sparse.len()
        );
    }
    let total: u64 = all.iter().map(|s| u64::from(s.n)).sum();
    let width = |s: &Sampled| segment::tinshape::ef::low_width(s.n as usize, s.universe);
    let share = |lists: &[Sampled]| {
        100.0 * lists.iter().map(|s| u64::from(s.n)).sum::<u64>() as f64 / total as f64
    };
    let mut groups = Vec::new();
    for l in 0..32 {
        let lists: Vec<Sampled> = all.iter().filter(|s| width(s) == l).cloned().collect();
        if !lists.is_empty() {
            groups.push(Group {
                name: format!("l{l}"),
                share: share(&lists),
                lists: thin(lists, KEEP),
            });
        }
    }
    for (name, lo, hi) in [
        ("n1-7", 1, 8),
        ("n8-15", 8, 16),
        ("n16-31", 16, 32),
        ("n32-63", 32, 64),
        ("n64-127", 64, 128),
        ("n128-511", 128, 512),
        ("n512+", 512, u32::MAX),
    ] {
        let lists: Vec<Sampled> = all
            .iter()
            .filter(|s| (lo..hi).contains(&s.n))
            .cloned()
            .collect();
        groups.push(Group {
            name: name.into(),
            share: share(&lists),
            lists: thin(lists, KEEP),
        });
    }
    groups.push(Group {
        name: "sparse".into(),
        share: share(&sparse),
        lists: thin(sparse, KEEP),
    });
    let containers = all.len();
    groups.push(Group {
        name: "all".into(),
        share: 100.0,
        lists: thin(all, KEEP_ALL),
    });
    let mut file = Vec::new();
    for g in &groups {
        file.extend_from_slice(format!("{}\t{}\t{}\n", g.name, g.share, g.lists.len()).as_bytes());
        for s in &g.lists {
            for v in [s.n, s.universe, s.words, s.bytes.len() as u32] {
                file.extend_from_slice(&v.to_le_bytes());
            }
            file.extend_from_slice(&s.bytes);
        }
    }
    eprintln!(
        "{containers} containers, {total} members; {} groups",
        groups.len()
    );
    file
}

type Lists = Vec<(Ef<'static>, u32)>;

/// The groups of a sample file, or of a dump sampled first: name, share of
/// members, lists.
fn load(path: &str, dump: Option<(&str, usize)>) -> Vec<(String, f64, Lists)> {
    let file = match dump {
        Some((dump, segments)) => sampled(dump, segments),
        None => std::fs::read(path).expect("a sample"),
    };
    let file: &'static [u8] = Box::leak(file.into_boxed_slice());
    let mut at = 0;
    let mut groups = Vec::new();
    let u32_at = |at: &mut usize| {
        let v = u32::from_le_bytes(file[*at..*at + 4].try_into().expect("four"));
        *at += 4;
        v
    };
    while at < file.len() {
        let end = at
            + file[at..]
                .iter()
                .position(|b| *b == b'\n')
                .expect("a header");
        let header = std::str::from_utf8(&file[at..end]).expect("a header");
        let f: Vec<&str> = header.split('\t').collect();
        let count: usize = f[2].parse().expect("a count");
        at = end + 1;
        let mut lists = Vec::with_capacity(count);
        for _ in 0..count {
            let (n, universe) = (u32_at(&mut at), u32_at(&mut at));
            let (words, len) = (u32_at(&mut at), u32_at(&mut at) as usize);
            let ef = Ef::parse(&file[at..at + len], n as usize, universe).expect("a list");
            at += len;
            lists.push((ef, words));
        }
        groups.push((f[0].to_string(), f[1].parse().expect("a share"), lists));
    }
    groups
}

fn median_ns(mut f: impl FnMut() -> u64) -> f64 {
    let mut calls = 1u64;
    loop {
        let t = Instant::now();
        for _ in 0..calls {
            black_box(f());
        }
        if t.elapsed() >= Duration::from_millis(20) {
            break;
        }
        calls *= 2;
    }
    let mut times: Vec<f64> = (0..7)
        .map(|_| {
            let t = Instant::now();
            for _ in 0..calls {
                black_box(f());
            }
            t.elapsed().as_nanos() as f64 / calls as f64
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[3]
}

/// One pass over `lists` at `level`, into a vector or into the row (or,
/// with `HARNESS` set, neither: the harness's own cost).
fn pass(level: Level, into_row: bool, lists: &Lists, out: &mut Vec<u32>, row: &mut [u64]) -> u64 {
    let mut sum = 0u64;
    let harness = std::env::var_os("HARNESS").is_some();
    for (ef, w) in lists {
        if harness {
            out.clear();
            sum += black_box(ef).len() as u64 + u64::from(*w);
        } else if into_row {
            let row = &mut row[..*w as usize];
            kernels::ef_into_row_at(level, *ef, row);
            sum += row[0];
        } else {
            out.clear();
            kernels::ef_decode_at(level, *ef, out);
            sum += u64::from(*out.last().unwrap_or(&0));
        }
    }
    sum
}

fn widest(groups: &[(String, f64, Lists)]) -> usize {
    groups
        .iter()
        .flat_map(|g| g.2.iter().map(|(_, w)| *w as usize))
        .max()
        .unwrap_or(0)
}

fn time(args: &[String]) {
    let (mut path, mut prefix) = (String::new(), String::new());
    let (mut dump, mut segments) = (None, 1usize);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--sample" => path = it.next().expect("a file").clone(),
            "--dump" => dump = Some(it.next().expect("a directory").clone()),
            "--segments" => segments = it.next().expect("a count").parse().expect("a count"),
            "--group" => prefix = it.next().expect("a prefix").clone(),
            _ => panic!(
                "usage: tnsef time --sample FILE | --dump DIR [--segments N] [--group PREFIX]"
            ),
        }
    }
    let groups = load(&path, dump.as_deref().map(|d| (d, segments)));
    let levels = Level::supported_levels();
    let mut row = vec![0u64; widest(&groups)];
    let mut out = Vec::with_capacity(1 << 16);
    println!(
        "ns per member, vector | row; speedup over scalar; levels {:?}",
        levels.iter().map(|l| l.name()).collect::<Vec<_>>()
    );
    for (name, share, lists) in groups.iter().filter(|g| g.0.starts_with(&prefix)) {
        let members: u64 = lists.iter().map(|(e, _)| e.len() as u64).sum();
        if members == 0 {
            continue;
        }
        let mut line = format!(
            "{name:<9} {share:>5.1}% of members {:>6} lists {:>6.1} mean n",
            lists.len(),
            members as f64 / lists.len() as f64
        );
        let mut base = (0.0, 0.0);
        for &level in &levels {
            let vector =
                median_ns(|| pass(level, false, lists, &mut out, &mut row)) / members as f64;
            row.fill(0);
            let rowed = median_ns(|| pass(level, true, lists, &mut out, &mut row)) / members as f64;
            if level == Level::Scalar {
                base = (vector, rowed);
                line += &format!(" | scalar {vector:.2} {rowed:.2}");
            } else {
                line += &format!(
                    " | {} {vector:.2} {rowed:.2} ({:.2}x {:.2}x)",
                    level.name(),
                    base.0 / vector,
                    base.1 / rowed
                );
            }
        }
        println!("{line}");
    }
}

fn count(args: &[String]) {
    let (mut path, mut group, mut level, mut into_row, mut passes) =
        (String::new(), String::new(), Level::Scalar, false, 1u64);
    let (mut dump, mut segments) = (None::<String>, 1usize);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--sample" => path = it.next().expect("a file").clone(),
            "--dump" => dump = Some(it.next().expect("a directory").clone()),
            "--segments" => segments = it.next().expect("a count").parse().expect("a count"),
            "--group" => group = it.next().expect("a name").clone(),
            "--level" => level = Level::parse(it.next().expect("a level")).expect("a level"),
            "--mode" => into_row = it.next().expect("a mode") == "row",
            "--passes" => passes = it.next().expect("a count").parse().expect("a count"),
            _ => panic!(
                "usage: tnsef count --sample FILE --group NAME --level L --mode vector|row --passes P"
            ),
        }
    }
    let groups = load(&path, dump.as_deref().map(|d| (d, segments)));
    let (_, _, lists) = groups.iter().find(|g| g.0 == group).expect("the group");
    let members: u64 = lists.iter().map(|(e, _)| e.len() as u64).sum();
    let mut row = vec![0u64; widest(&groups)];
    let mut out = Vec::with_capacity(1 << 16);
    let mut sum = 0u64;
    for _ in 0..passes {
        sum = sum.wrapping_add(pass(level, into_row, lists, &mut out, &mut row));
    }
    println!(
        "{group} {} lists {} members {} passes {passes} ({sum})",
        level.name(),
        lists.len(),
        members
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("sample") => sample(&args[1..]),
        Some("time") => time(&args[1..]),
        Some("count") => count(&args[1..]),
        _ => panic!("usage: tnsef sample|time|count ..."),
    }
}
