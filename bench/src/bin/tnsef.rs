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
//! `arrays` sets each sampled list against the same members as an array
//! container (sorted `u16`s, page layout 11), per container: the list
//! parsed and decoded (at the CPU's level) against the array widened, each
//! set in a cleared row, each intersected with candidates about as many
//! (half of them members; the list decoded and marked, the array marked,
//! merged and on aarch64 compared all-pairs), and each probed with an
//! eighth as many (the list's high bits sought, the array galloped). Take
//! the sample from a dump of layout 10 or earlier: it holds lists only.
//!
//! `census` counts a dump's group containers by kind and its Elias-Fano
//! lists by length (lists, members, bytes, and what arrays would take),
//! and its groups' largest slot count.
//!
//! ```text
//! cargo run --release -p bench --bin tnsef -- time --dump DIR [--segments 1]
//! tnsef sample --dump DIR --out FILE [--segments 1]
//! tnsef time --sample FILE [--group PREFIX]
//! tnsef count --sample FILE --group NAME --level L --mode vector|row --passes P
//! tnsef arrays --sample FILE [--group PREFIX]
//! tnsef census --dump DIR [--segments N]
//! ```

use std::hint::black_box;
use std::time::{Duration, Instant};

use engine::tinshape::kernels::{self, Level};
use segment::tinshape::bits;
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

type Lists = Vec<(Ef<'static>, u32, u32)>;

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
            lists.push((ef, words, universe));
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
    for (ef, w, _) in lists {
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
        .flat_map(|g| g.2.iter().map(|(_, w, _)| *w as usize))
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
        let members: u64 = lists.iter().map(|(e, _, _)| e.len() as u64).sum();
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
    let members: u64 = lists.iter().map(|(e, _, _)| e.len() as u64).sum();
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

/// Intersections of ascending candidates with an array container (sorted
/// `u16` local slots) the walk could use instead of widening the array and
/// marking it ([`intersect_marked`] without the widening): a branch-free
/// merge, and on aarch64 an all-pairs compare of blocks of eight.
mod arrays {
    use segment::tinshape::bits;

    #[inline]
    pub fn value(bytes: &[u8], i: usize) -> u32 {
        u32::from(u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]))
    }

    /// Keeps the candidates in the array, through cleared `marks`.
    #[inline]
    pub fn intersect_marked(cands: &mut Vec<u32>, bytes: &[u8], n: usize, marks: &mut [u64]) {
        let bytes = &bytes[..2 * n];
        for c in bytes.chunks_exact(2) {
            let v = u16::from_le_bytes([c[0], c[1]]) as usize;
            marks[v / 64] |= 1 << (v % 64);
        }
        bits::retain(cands, |v| marks[v as usize / 64] >> (v % 64) & 1 == 1);
        for c in bytes.chunks_exact(2) {
            let v = u16::from_le_bytes([c[0], c[1]]) as usize;
            marks[v / 64] = 0;
        }
    }

    /// Keeps the candidates in the array by a branch-free merge: per
    /// candidate, how many of the array's next eight lie below it.
    #[inline]
    pub fn intersect_merge(cands: &mut Vec<u32>, bytes: &[u8], n: usize) {
        let mut padded = [u16::MAX; 8];
        let (mut j, mut kept) = (0usize, 0usize);
        for r in 0..cands.len() {
            let c = cands[r];
            loop {
                let window: [u16; 8] = if j + 8 <= n {
                    std::array::from_fn(|k| value(bytes, j + k) as u16)
                } else {
                    for (k, p) in padded.iter_mut().enumerate() {
                        *p = if j + k < n {
                            value(bytes, j + k) as u16
                        } else {
                            u16::MAX
                        };
                    }
                    padded
                };
                let below = window.iter().filter(|w| u32::from(**w) < c).count();
                j += below;
                if below < 8 || j >= n {
                    let hit = j < n && value(bytes, j) == c;
                    cands[kept] = c;
                    kept += usize::from(hit);
                    break;
                }
            }
        }
        cands.truncate(kept);
    }

    /// Keeps the candidates in the array by an all-pairs compare of blocks
    /// of eight: candidates narrowed to `u16`, against the array's block
    /// and its seven rotations (Schlegel et al.'s block compare).
    #[cfg(target_arch = "aarch64")]
    #[inline]
    pub fn intersect_allpairs(cands: &mut Vec<u32>, bytes: &[u8], n: usize) {
        use std::arch::aarch64::*;
        let m = cands.len();
        let (mut i, mut j, mut kept) = (0usize, 0usize, 0usize);
        let mut a_pad = [u16::MAX; 8];
        let mut b_pad = [u16::MAX - 1; 8];
        // SAFETY: loads read 8 values of a buffer of at least 8.
        unsafe {
            while i < m && j < n {
                let ab = m - i >= 8;
                for (k, a) in a_pad.iter_mut().enumerate() {
                    *a = cands.get(i + k).map_or(u16::MAX, |c| *c as u16);
                }
                let a = vld1q_u16(a_pad.as_ptr());
                let b = if j + 8 <= n {
                    vld1q_u16(bytes.as_ptr().add(2 * j).cast())
                } else {
                    for (k, b) in b_pad.iter_mut().enumerate() {
                        *b = if j + k < n {
                            value(bytes, j + k) as u16
                        } else {
                            u16::MAX - 1
                        };
                    }
                    vld1q_u16(b_pad.as_ptr())
                };
                let mut eq = vceqq_u16(a, b);
                eq = vorrq_u16(eq, vceqq_u16(a, vextq_u16(b, b, 1)));
                eq = vorrq_u16(eq, vceqq_u16(a, vextq_u16(b, b, 2)));
                eq = vorrq_u16(eq, vceqq_u16(a, vextq_u16(b, b, 3)));
                eq = vorrq_u16(eq, vceqq_u16(a, vextq_u16(b, b, 4)));
                eq = vorrq_u16(eq, vceqq_u16(a, vextq_u16(b, b, 5)));
                eq = vorrq_u16(eq, vceqq_u16(a, vextq_u16(b, b, 6)));
                eq = vorrq_u16(eq, vceqq_u16(a, vextq_u16(b, b, 7)));
                let a_last = a_pad[if ab { 7 } else { m - i - 1 }];
                let b_last = if j + 8 <= n {
                    value(bytes, j + 7)
                } else {
                    value(bytes, n - 1)
                } as u16;
                if vmaxvq_u16(eq) != 0 {
                    let mut lanes = [0u16; 8];
                    vst1q_u16(lanes.as_mut_ptr(), eq);
                    for k in 0..8.min(m - i) {
                        // A lane hit twice (the block kept) is kept once.
                        if lanes[k] != 0 {
                            cands[kept] = cands[i + k];
                            kept += 1;
                            lanes[k] = 0;
                        }
                    }
                }
                // The block whose last value is lower is done; both when equal.
                // Hits in a kept candidate block must not repeat: clear them
                // by advancing past the candidates matched (they are unique).
                if a_last <= b_last {
                    i += 8;
                }
                if b_last <= a_last {
                    j += 8;
                }
                if a_last > b_last {
                    // The candidate block stays: drop the hits recorded so
                    // they are not recorded again, by moving its start past
                    // the last candidate at or below b_last.
                    while i < m && cands[i] as u16 <= b_last {
                        i += 1;
                    }
                }
            }
        }
        cands.truncate(kept);
    }
}

/// The gate's microbenchmark: per sampled group, an Elias-Fano container
/// (parsed and decoded at the CPU's level, as a walk loads one) against the
/// same members as sorted `u16`: decoded into a vector, into a cleared row,
/// intersected with candidates of about its length (half its members, half
/// not; the current path decodes then marks), and probed with candidates
/// of an eighth of its length. ns per container, median of seven batches.
fn arrays_bench(args: &[String]) {
    let (mut path, mut prefix) = (String::new(), String::new());
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--sample" => path = it.next().expect("a file").clone(),
            "--group" => prefix = it.next().expect("a prefix").clone(),
            _ => panic!("usage: tnsef arrays --sample FILE [--group PREFIX]"),
        }
    }
    let groups = load(&path, None);
    let mut row = vec![0u64; widest(&groups)];
    let mut marks = vec![0u64; widest(&groups)];
    let mut out: Vec<u32> = Vec::with_capacity(1 << 16);
    let mut cands: Vec<u32> = Vec::with_capacity(1 << 16);
    println!(
        "ns per container (EF parsed + decoded at {}; u16 kernels at the same width)",
        kernels::level().name()
    );
    println!(
        "{:<9} {:>6} {:>6} | decode ef u16 | row ef u16 | and: ef+marked u16marked u16merge u16allpairs | probe: ef u16",
        "group", "lists", "mean n"
    );
    // Sparse lists are over a segment's slots, not a group's: no array holds them.
    for (name, _, lists) in groups
        .iter()
        .filter(|g| g.0.starts_with(&prefix) && g.0 != "sparse")
    {
        if lists.is_empty() {
            continue;
        }
        // Each list as its EF bytes, and as u16, and its candidate sets.
        struct Case {
            ef: Vec<u8>,
            n: usize,
            universe: u32,
            words: usize,
            u16s: Vec<u8>,
            and: Vec<u32>,
            probe: Vec<u32>,
        }
        let cases: Vec<Case> = lists
            .iter()
            .map(|(ef, words, universe)| {
                let values = ef.to_vec();
                let set: std::collections::HashSet<u32> = values.iter().copied().collect();
                let universe = *universe;
                let mut and = Vec::new();
                for (i, v) in values.iter().enumerate() {
                    if i % 2 == 0 {
                        and.push(*v);
                    } else if let Some(o) =
                        (v + 1..v + 64).find(|o| !set.contains(o) && *o < universe)
                    {
                        and.push(o);
                    }
                }
                and.sort_unstable();
                and.dedup();
                let probe: Vec<u32> = values
                    .iter()
                    .step_by(8)
                    .flat_map(|v| [*v, v + 1])
                    .filter(|v| *v < universe)
                    .collect::<std::collections::BTreeSet<u32>>()
                    .into_iter()
                    .collect();
                Case {
                    ef: [ef.lows(), ef.highs()].concat(),
                    n: ef.len(),
                    universe,
                    words: *words as usize,
                    u16s: values
                        .iter()
                        .flat_map(|v| (*v as u16).to_le_bytes())
                        .collect(),
                    and,
                    probe,
                }
            })
            .collect();
        // Check the kernels against the reference first.
        for c in &cases {
            let ef = Ef::parse(&c.ef, c.n, c.universe).expect("a list");
            let want = ef.to_vec();
            out.clear();
            kernels::array_decode(&c.u16s, &mut out);
            assert_eq!(out, want, "decode");
            let reference = |cs: &[u32]| -> Vec<u32> {
                cs.iter()
                    .copied()
                    .filter(|v| want.binary_search(v).is_ok())
                    .collect()
            };
            let r = reference(&c.and);
            cands.clear();
            cands.extend_from_slice(&c.and);
            arrays::intersect_marked(&mut cands, &c.u16s, c.n, &mut marks);
            assert_eq!(cands, r, "marked");
            cands.clear();
            cands.extend_from_slice(&c.and);
            arrays::intersect_merge(&mut cands, &c.u16s, c.n);
            assert_eq!(cands, r, "merge");
            #[cfg(target_arch = "aarch64")]
            {
                cands.clear();
                cands.extend_from_slice(&c.and);
                arrays::intersect_allpairs(&mut cands, &c.u16s, c.n);
                assert_eq!(cands, r, "allpairs");
            }
            cands.clear();
            cands.extend_from_slice(&c.probe);
            kernels::array_retain(&c.u16s, &mut cands);
            assert_eq!(cands, reference(&c.probe), "probe");
        }
        let per = |f: &mut dyn FnMut() -> u64| median_ns(f) / cases.len() as f64;
        let ef_decode = per(&mut || {
            let mut s = 0u64;
            for c in &cases {
                out.clear();
                let ef = Ef::parse(black_box(&c.ef), c.n, c.universe).expect("a list");
                kernels::ef_decode(ef, &mut out);
                s += u64::from(out[out.len() - 1]);
            }
            s
        });
        let u16_decode = per(&mut || {
            let mut s = 0u64;
            for c in &cases {
                out.clear();
                kernels::array_decode(black_box(&c.u16s), &mut out);
                s += u64::from(out[out.len() - 1]);
            }
            s
        });
        let ef_row = per(&mut || {
            let mut s = 0u64;
            for c in &cases {
                let row = &mut row[..c.words];
                let ef = Ef::parse(black_box(&c.ef), c.n, c.universe).expect("a list");
                kernels::ef_into_row(ef, row);
                s += row[0];
                row.fill(0);
            }
            s
        });
        let u16_row = per(&mut || {
            let mut s = 0u64;
            for c in &cases {
                let row = &mut row[..c.words];
                kernels::array_into_row(black_box(&c.u16s), row);
                s += row[0];
                row.fill(0);
            }
            s
        });
        let ef_and = per(&mut || {
            let mut s = 0u64;
            for c in &cases {
                cands.clear();
                cands.extend_from_slice(&c.and);
                out.clear();
                let ef = Ef::parse(black_box(&c.ef), c.n, c.universe).expect("a list");
                kernels::ef_decode(ef, &mut out);
                bits::intersect_marked(&mut cands, &out, &mut marks);
                s += cands.len() as u64;
            }
            s
        });
        let mut u16_and = |f: fn(&mut Vec<u32>, &[u8], usize, &mut [u64])| {
            per(&mut || {
                let mut s = 0u64;
                for c in &cases {
                    cands.clear();
                    cands.extend_from_slice(&c.and);
                    f(&mut cands, black_box(&c.u16s), c.n, &mut marks);
                    s += cands.len() as u64;
                }
                s
            })
        };
        let u16_marked = u16_and(arrays::intersect_marked);
        let u16_merge = u16_and(|c, b, n, _| arrays::intersect_merge(c, b, n));
        #[cfg(target_arch = "aarch64")]
        let u16_allpairs = u16_and(|c, b, n, _| arrays::intersect_allpairs(c, b, n));
        #[cfg(not(target_arch = "aarch64"))]
        let u16_allpairs = f64::NAN;
        let ef_probe = per(&mut || {
            let mut s = 0u64;
            for c in &cases {
                cands.clear();
                cands.extend_from_slice(&c.probe);
                let ef = Ef::parse(black_box(&c.ef), c.n, c.universe).expect("a list");
                ef.retain_members(&mut cands);
                s += cands.len() as u64;
            }
            s
        });
        let u16_probe = per(&mut || {
            let mut s = 0u64;
            for c in &cases {
                cands.clear();
                cands.extend_from_slice(&c.probe);
                kernels::array_retain(black_box(&c.u16s), &mut cands);
                s += cands.len() as u64;
            }
            s
        });
        let mean_n = cases.iter().map(|c| c.n).sum::<usize>() as f64 / cases.len() as f64;
        println!(
            "{name:<9} {:>6} {mean_n:>6.1} | decode {ef_decode:6.2} {u16_decode:6.2} | row {ef_row:6.2} {u16_row:6.2} \
             | and {ef_and:6.2} {u16_marked:6.2} {u16_merge:6.2} {u16_allpairs:6.2} | probe {ef_probe:6.2} {u16_probe:6.2}",
            cases.len()
        );
    }
}

/// Bands of container lengths the census reports, by most members.
const BANDS: [u32; 7] = [4, 8, 16, 32, 64, 128, u32::MAX];

/// Per band of Elias-Fano container lengths: containers, members, their
/// bytes as Elias-Fano and as sorted `u16` local slots.
#[derive(Clone, Copy, Default)]
struct Band {
    lists: u64,
    members: u64,
    ef: u64,
    u16s: u64,
}

/// Over every segment of a dump: Elias-Fano group containers by length
/// band (lists, members, bytes as Elias-Fano and as `u16` arrays), every
/// container kind's count and bytes, the segments' bytes, and the largest
/// group's slots (what a `u16` local slot must hold).
fn census(args: &[String]) {
    let (mut dump, mut segments) = (String::new(), usize::MAX);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dump" => dump = it.next().expect("a directory").clone(),
            "--segments" => segments = it.next().expect("a count").parse().expect("a count"),
            _ => panic!("usage: tnsef census --dump DIR [--segments N]"),
        }
    }
    let dump = std::path::PathBuf::from(dump);
    let manifest = std::fs::read_to_string(dump.join("manifest.tsv")).expect("a manifest");
    let mut bands = [Band::default(); BANDS.len()];
    let mut kinds = [(0u64, 0u64, 0u64); 4];
    let (mut blob_bytes, mut max_slots, mut over_u16, mut groups_total) = (0u64, 0u32, 0u64, 0u64);
    let mut sparse = (0u64, 0u64);
    for line in manifest
        .lines()
        .filter(|l| l.starts_with("segment\t"))
        .take(segments)
    {
        let blob = line.split('\t').nth(3).expect("a blob");
        let bytes = std::fs::read(dump.join(blob)).expect("a blob");
        blob_bytes += bytes.len() as u64;
        let segment = Segment::parse(&bytes).expect("a TNS1 segment");
        let geometry = &segment.docs.geometry;
        for g in &geometry.groups {
            max_slots = max_slots.max(g.slots());
            over_u16 += u64::from(g.slots() > 1 << 16);
            groups_total += 1;
        }
        for item in segment.dictionary().iter() {
            let (_, entry) = item.expect("a term map entry");
            let term = segment.resolve(entry).expect("a postings record");
            let p = term.postings;
            match &p.form {
                Form::Grouped(dir) => {
                    let entries = dir.entries(&geometry.groups).expect("a directory");
                    for e in entries.iter() {
                        let len = u64::from(e.len);
                        let k = &mut kinds[usize::from(e.kind & 3)];
                        k.0 += 1;
                        k.1 += u64::from(e.count);
                        k.2 += len;
                        if e.kind == KIND_EF {
                            let b = BANDS.iter().position(|m| e.count <= *m).expect("a band");
                            let band = &mut bands[b];
                            band.lists += 1;
                            band.members += u64::from(e.count);
                            band.ef += len;
                            band.u16s += 2 * u64::from(e.count);
                        }
                    }
                }
                Form::Sparse(list) => {
                    sparse.0 += 1;
                    sparse.1 += list.len() as u64;
                }
                Form::Single(_) => {}
            }
        }
        eprintln!("{blob}: done");
    }
    println!(
        "segments {:.3} GB; groups {groups_total}, largest {max_slots} slots, {over_u16} over 65536",
        blob_bytes as f64 / 1e9
    );
    for (name, k) in ["grid", "ef", "paged", "array"].iter().zip(kinds) {
        println!(
            "{name:<6} containers {:>11} members {:>13} bytes {:>14} ({:.2} B/member)",
            k.0,
            k.1,
            k.2,
            k.2 as f64 / k.1.max(1) as f64
        );
    }
    println!("sparse lists {} members {}", sparse.0, sparse.1);
    let ef_members: u64 = bands.iter().map(|b| b.members).sum();
    let (mut lists, mut members, mut ef, mut u16s) = (0u64, 0u64, 0u64, 0u64);
    println!(
        "ef band        lists       members (% of ef)      ef bytes     u16 bytes  ratio | \
         cumulative: lists members (%), u16 less ef bytes (% of segments)"
    );
    for (b, band) in bands.iter().enumerate() {
        lists += band.lists;
        members += band.members;
        ef += band.ef;
        u16s += band.u16s;
        let name = if BANDS[b] == u32::MAX {
            "all".to_string()
        } else {
            BANDS[b].to_string()
        };
        println!(
            "<= {name:<5} {:>11} {:>13} ({:>5.1}%) {:>13} {:>13} {:>5.2}x | {lists:>11} {members:>13} \
             ({:>5.1}%) {:>+14} ({:+.2}%)",
            band.lists,
            band.members,
            100.0 * band.members as f64 / ef_members.max(1) as f64,
            band.ef,
            band.u16s,
            band.u16s as f64 / band.ef.max(1) as f64,
            100.0 * members as f64 / ef_members.max(1) as f64,
            u16s as i64 - ef as i64,
            100.0 * (u16s as f64 - ef as f64) / blob_bytes as f64
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("sample") => sample(&args[1..]),
        Some("time") => time(&args[1..]),
        Some("count") => count(&args[1..]),
        Some("census") => census(&args[1..]),
        Some("arrays") => arrays_bench(&args[1..]),
        _ => panic!("usage: tnsef sample|time|count|census ..."),
    }
}
