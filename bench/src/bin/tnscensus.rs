// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A census of a `TNS1` dump's postings containers, and what they would
//! cost under other container schemes (TIN's measured one among them).
//!
//! ```text
//! cargo run -p bench --release --bin tnscensus -- --dump DIR [--trace FILE] \
//!     [--segment FILE] [--out FILE]
//! ```
//!
//! For every (term, group) it takes the members' pages and offsets and
//! prices each container form: the grid at the group's width, a grid at
//! each page's own largest offset (`pgrid`), Elias-Fano, the current paged
//! container, a paged container with a free full page (`pagedx`), a mask
//! shared by every page (`shared`) and an estimate of TIN's container from
//! its probes (`tin`). Schemes choose among the forms per group, as the
//! encoder does, and per term between the groups and one sparse list.
//!
//! With `--trace`, each scheme also lays the records out again (only the
//! payload changes size) and counts the distinct payload pages each
//! disjunction's terms span, every group of every term read, under `score`
//! (terms of at least 10% of the documents elided) and `full_score`.
//! Read-only on the dump.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use rustc_hash::{FxHashMap, FxHashSet};
use segment::tinshape::bits;
use segment::tinshape::docs::Group;
use segment::tinshape::ef;
use segment::tinshape::postings::{Form, KIND_EF, KIND_GRID, KIND_PAGED};
use segment::tinshape::segment::{Area, Segment};

const PAGE_DATA: usize = 8192 - 24 - 8 - 4;
/// The encoder's defaults: a group of at least one member in 64 slots of a
/// term of at least 4,096 postings is a grid.
const GRID_DENSITY: u64 = 64;
const GRID_MIN_POSTINGS: u64 = 4096;
const LIST_MAX: usize = 64;

const SCHEMES: [&str; 19] = [
    "actual",
    "cur",
    "noforce",
    "force16",
    "force8",
    "force4",
    "cur+shared",
    "cur+pagedx",
    "pgrid",
    "pgrid+x",
    "noforce+x",
    "force8+x",
    "best",
    "tin",
    "tin2",
    "pm16",
    "pm32",
    "pg16",
    "pg32",
];
/// Schemes whose per-group container bytes are kept for the trace's
/// conjunctions and phrases (the lead's groups only).
const PAGE_SCHEMES: [usize; 8] = [1, 2, 9, 14, 15, 16, 17, 18];
const NPS: usize = PAGE_SCHEMES.len();
const STYLES: [&str; 3] = ["conjunction", "disjunction", "phrase"];
const S_ACTUAL: usize = 0;
const NS: usize = SCHEMES.len();

const DF_CLASSES: [&str; 8] = [
    "1",
    "2-8",
    "9-64",
    "65-4095",
    "4096-65535",
    "65536-999999",
    "1M-9.99M",
    ">=10M",
];

fn df_class(df: u32) -> usize {
    match df {
        1 => 0,
        2..=8 => 1,
        9..=64 => 2,
        65..=4095 => 3,
        4096..=65535 => 4,
        65536..=999_999 => 5,
        1_000_000..=9_999_999 => 6,
        _ => 7,
    }
}

const FORMS: [&str; 8] = [
    "single", "sparse", "grid", "ef", "paged", "shared", "pgrid", "pmask",
];
const NF: usize = FORMS.len();
const F_GRID: usize = 2;
const F_EF: usize = 3;
const F_PAGED: usize = 4;
const F_SHARED: usize = 5;
const F_PGRID: usize = 6;
const F_PMASK: usize = 7;

struct Args {
    dump: PathBuf,
    trace: Option<PathBuf>,
    only: Option<String>,
    out: Option<PathBuf>,
    /// Microbenchmark the decode of this segment's forced grids.
    decode: Option<String>,
}

fn usage() -> ! {
    eprintln!("usage: tnscensus --dump DIR [--trace FILE] [--segment FILE] [--out FILE]");
    std::process::exit(2)
}

fn args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        dump: PathBuf::new(),
        trace: None,
        only: None,
        out: None,
        decode: None,
    };
    let value = |it: &mut std::iter::Skip<std::env::Args>| it.next().unwrap_or_else(|| usage());
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dump" => a.dump = value(&mut it).into(),
            "--trace" => a.trace = Some(value(&mut it).into()),
            "--segment" => a.only = Some(value(&mut it)),
            "--out" => a.out = Some(value(&mut it).into()),
            "--decode" => a.decode = Some(value(&mut it)),
            _ => usage(),
        }
    }
    if a.dump.as_os_str().is_empty() {
        usage();
    }
    a
}

fn varint_len(v: u64) -> usize {
    (64 - v.max(1).leading_zeros() as usize).div_ceil(7)
}

/// A group's pages in the document set: per page its documents and its
/// largest offset; and its grid at per-page widths.
struct GroupPages {
    base: Vec<usize>,
    docs: Vec<u16>,
    max: Vec<u16>,
    pgrid: Vec<usize>,
}

impl GroupPages {
    fn of(segment: &Segment<'_>) -> Self {
        let geometry = &segment.docs.geometry;
        let mut g = GroupPages {
            base: Vec::with_capacity(geometry.groups.len()),
            docs: Vec::new(),
            max: Vec::new(),
            pgrid: Vec::with_capacity(geometry.groups.len()),
        };
        for (index, group) in geometry.groups.iter().enumerate() {
            let base = g.docs.len();
            g.base.push(base);
            let p = group.pages as usize;
            let w = u32::from(group.width);
            g.docs.resize(base + p, 0);
            g.max.resize(base + p, 0);
            for (i, word) in segment.docs.group_words(index).iter().enumerate() {
                let mut word = *word;
                while word != 0 {
                    let local = (i * 64) as u32 + word.trailing_zeros();
                    word &= word - 1;
                    let (page, offset) = ((local / w) as usize, (local % w + 1) as u16);
                    g.docs[base + page] += 1;
                    g.max[base + page] = g.max[base + page].max(offset);
                }
            }
            let bits: usize = g.max[base..base + p].iter().map(|m| *m as usize).sum();
            g.pgrid.push(bits.div_ceil(64) * 8);
        }
        g
    }
}

/// Every form's bytes for one (term, group).
#[derive(Clone, Copy, Default)]
struct Costs {
    n: u32,
    grid: usize,
    pgrid: usize,
    ef: usize,
    paged: usize,
    pagedx: usize,
    shared: Option<usize>,
    tin: usize,
    /// A page bitmap (or list) and per present page a bitmap at the
    /// group's width (a fixed stride, so a page's mask is found by its
    /// rank): TIN's two-level bitmap without head bytes.
    pmask: usize,
    /// TIN 1.0.4 as probed on an 18-rows-per-page heap (probe6 S1):
    /// about 20 B per (term, group) and per present page about 2.8 B for
    /// one member, 0.2 B per further member, at most a bitmap.
    tin2: usize,
}

/// Per-page shape counts.
#[derive(Default, Clone)]
struct Shape {
    containers: u64,
    postings: u64,
    bytes: u64,
    pages: u64,
    full_pages: u64,
    run_pages: u64,
    single_pages: u64,
    /// Containers of 2+ pages whose pages' masks are all equal, and of
    /// those, the ones whose pages are all full.
    multi: u64,
    identical: u64,
    identical_full: u64,
    all_full: u64,
    /// Containers by pages present: 1, 2-3, 4-15, 16-63, 64-255, 256.
    by_pages: [u64; 6],
    /// Pages present by members on them: 1, 2, 3-4, 5-8, 9-16, 17+.
    by_members: [u64; 6],
}

fn pages_bucket(np: usize) -> usize {
    match np {
        1 => 0,
        2..=3 => 1,
        4..=15 => 2,
        16..=63 => 3,
        64..=255 => 4,
        _ => 5,
    }
}

fn members_bucket(n: usize) -> usize {
    match n {
        1 => 0,
        2 => 1,
        3..=4 => 2,
        5..=8 => 3,
        9..=16 => 4,
        _ => 5,
    }
}

/// Prices one group's members (`locals`, sorted) and counts their shape.
fn price(
    group: &Group,
    gp: &GroupPages,
    index: usize,
    locals: &[u32],
    shape: Option<&mut Shape>,
) -> Costs {
    let w = u32::from(group.width);
    let ob = bits::width_below(w);
    let base = gp.base[index];
    let n = locals.len();
    let mut c = Costs {
        n: n as u32,
        grid: group.grid_bytes(),
        pgrid: gp.pgrid[index],
        ef: ef::encoded_len(n, group.slots()),
        ..Costs::default()
    };
    let mut np = 0usize;
    let mut paged_pages = 0usize;
    let mut pagedx_pages = 0usize;
    let mut tin_pages = 0usize;
    let mut pmask_pages = 0usize;
    let mut tin2_pages = 0.0f64;
    let mut identical = true;
    let mut all_full = true;
    let mut first_page: (usize, usize) = (0, 0);
    let mut max_offset = 0u32;
    let (mut full_pages, mut run_pages, mut single_pages) = (0u64, 0u64, 0u64);
    let mut by_members = [0u64; 6];
    let mut i = 0;
    while i < n {
        let page = locals[i] / w;
        let start = i;
        while i < n && locals[i] / w == page {
            i += 1;
        }
        let offs = &locals[start..i];
        let k = offs.len();
        let first = offs[0] % w;
        let last = offs[k - 1] % w;
        max_offset = max_offset.max(last);
        let contiguous = (last - first) as usize + 1 == k;
        let full = k == gp.docs[base + page as usize] as usize;
        let list = (k <= LIST_MAX).then(|| bits::packed_len(k, ob));
        let bitmap = last as usize / 8 + 1;
        let run = contiguous.then(|| bits::packed_len(2, ob));
        let cur = [list, Some(bitmap), run]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(bitmap);
        paged_pages += 1 + cur;
        pagedx_pages += 1 + if full { 0 } else { cur };
        tin_pages += if contiguous && k > 1 {
            9
        } else {
            (w as usize).div_ceil(8)
        };
        pmask_pages += (w as usize).div_ceil(8);
        {
            let docs = gp.docs[base + page as usize] as f64;
            let list = 2.8 + 0.2 * (k as f64 - 1.0);
            let bitmap = 0.9 + (docs / 8.0).ceil();
            tin2_pages += list.min(bitmap);
        }
        if np == 0 {
            first_page = (start, i);
        } else if identical {
            let (a, b) = first_page;
            identical = b - a == k && locals[a..b].iter().zip(offs).all(|(x, y)| x % w == y % w);
        }
        all_full &= full;
        full_pages += u64::from(full);
        run_pages += u64::from(contiguous);
        single_pages += u64::from(k == 1);
        by_members[members_bucket(k)] += 1;
        np += 1;
    }
    let page_set = 1 + if np < 32 { np } else { 32 };
    c.paged = page_set + paged_pages;
    c.pagedx = page_set + pagedx_pages;
    c.pmask = page_set + pmask_pages;
    c.tin2 = (20.0 + tin2_pages).round() as usize;
    if np >= 2 && identical {
        c.shared = Some(page_set + 1 + max_offset as usize / 8 + 1);
    } else if np >= 2 && all_full {
        c.shared = Some(page_set + 1);
    }
    let tin_list = if n == np {
        (n as f64 * 1.85) as usize
    } else {
        (n as f64 * 3.3) as usize
    };
    c.tin = if np >= 2 && identical {
        40
    } else {
        tin_list.min(32 + tin_pages)
    };
    if let Some(s) = shape {
        s.containers += 1;
        s.postings += n as u64;
        s.pages += np as u64;
        s.full_pages += full_pages;
        s.run_pages += run_pages;
        s.single_pages += single_pages;
        if np >= 2 {
            s.multi += 1;
            s.identical += u64::from(identical);
            s.identical_full += u64::from(identical && all_full);
        }
        s.all_full += u64::from(all_full);
        s.by_pages[pages_bucket(np)] += 1;
        for (a, b) in s.by_members.iter_mut().zip(by_members) {
            *a += b;
        }
    }
    c
}

/// A scheme's choice for one group: (form, bytes, whether its directory
/// entry carries a length).
fn choose(scheme: usize, c: &Costs, df: u32, slots: u32) -> (usize, usize) {
    let dense_at = |d: u64| u64::from(c.n) * d >= u64::from(slots);
    let big = u64::from(df) >= GRID_MIN_POSTINGS;
    let forced = |d: u64| big && dense_at(d);
    let shared = c.shared.unwrap_or(usize::MAX);
    let min = |options: &[(usize, usize)]| *options.iter().min_by_key(|o| o.1).unwrap();
    let all = [(F_GRID, c.grid), (F_EF, c.ef), (F_PAGED, c.paged)];
    match SCHEMES[scheme] {
        "cur" => {
            if forced(GRID_DENSITY) {
                (F_GRID, c.grid)
            } else {
                min(&all)
            }
        }
        "noforce" => min(&all),
        "force16" | "force8" | "force4" => {
            let d = match SCHEMES[scheme] {
                "force16" => 16,
                "force8" => 8,
                _ => 4,
            };
            if forced(d) {
                (F_GRID, c.grid)
            } else {
                min(&all)
            }
        }
        "cur+shared" => {
            if forced(GRID_DENSITY) {
                min(&[(F_GRID, c.grid), (F_SHARED, shared)])
            } else {
                min(&[all[0], all[1], all[2], (F_SHARED, shared)])
            }
        }
        "cur+pagedx" => {
            if forced(GRID_DENSITY) {
                min(&[(F_GRID, c.grid), (F_SHARED, shared)])
            } else {
                min(&[
                    (F_GRID, c.grid),
                    (F_EF, c.ef),
                    (F_PAGED, c.pagedx),
                    (F_SHARED, shared),
                ])
            }
        }
        "pgrid" => {
            if forced(GRID_DENSITY) {
                (F_PGRID, c.pgrid)
            } else {
                min(&[(F_PGRID, c.pgrid), (F_EF, c.ef), (F_PAGED, c.paged)])
            }
        }
        "pgrid+x" => {
            if forced(GRID_DENSITY) {
                min(&[(F_PGRID, c.pgrid), (F_SHARED, shared)])
            } else {
                min(&[
                    (F_PGRID, c.pgrid),
                    (F_EF, c.ef),
                    (F_PAGED, c.pagedx),
                    (F_SHARED, shared),
                ])
            }
        }
        "noforce+x" => min(&[
            (F_GRID, c.grid),
            (F_EF, c.ef),
            (F_PAGED, c.pagedx),
            (F_SHARED, shared),
        ]),
        "force8+x" => {
            if forced(8) {
                min(&[(F_GRID, c.grid), (F_SHARED, shared)])
            } else {
                min(&[
                    (F_GRID, c.grid),
                    (F_EF, c.ef),
                    (F_PAGED, c.pagedx),
                    (F_SHARED, shared),
                ])
            }
        }
        "best" => min(&[
            (F_GRID, c.grid),
            (F_PGRID, c.pgrid),
            (F_EF, c.ef),
            (F_PAGED, c.pagedx),
            (F_SHARED, shared),
        ]),
        "tin" => (F_PAGED, c.tin),
        // Terms under about 250 postings live in TIN's footer at about
        // 6.5 B per posting (probe6 S1: 5.6 to 9.9).
        "tin2" => (
            F_PAGED,
            if df < 256 {
                (f64::from(c.n) * 6.5).round() as usize
            } else {
                c.tin2
            },
        ),
        "pm16" | "pm32" | "pg16" | "pg32" => {
            let (d, form, bytes) = match SCHEMES[scheme] {
                "pm16" => (16, F_PMASK, c.pmask),
                "pm32" => (32, F_PMASK, c.pmask),
                "pg16" => (16, F_PAGED, c.paged),
                _ => (32, F_PAGED, c.paged),
            };
            if forced(d) {
                (F_GRID, c.grid)
            } else if forced(GRID_DENSITY) {
                (form, bytes)
            } else {
                min(&[all[0], all[1], all[2], (F_PMASK, c.pmask)])
            }
        }
        _ => unreachable!(),
    }
}

/// Whether a scheme's group form is forced (no sparse list for its term).
fn forced_in(scheme: usize, c: &Costs, df: u32, slots: u32) -> bool {
    let big = u64::from(df) >= GRID_MIN_POSTINGS;
    let d = match SCHEMES[scheme] {
        "cur" | "cur+shared" | "cur+pagedx" | "pgrid" | "pgrid+x" => GRID_DENSITY,
        "force16" => 16,
        "force8" | "force8+x" => 8,
        "force4" => 4,
        "tin" | "tin2" => return true,
        "pm16" | "pm32" | "pg16" | "pg32" => GRID_DENSITY,
        _ => return false,
    };
    big && u64::from(c.n) * d >= u64::from(slots)
}

/// Totals for one scheme.
#[derive(Default, Clone)]
struct Total {
    payload: u64,
    directory: u64,
    by_class: [u64; 8],
    by_form: [u64; NF],
    forms: [u64; NF],
}

/// Where a trace term's payload lies under each scheme, per segment.
#[derive(Clone, Copy, Default)]
struct Extent {
    at: u64,
    len: u64,
}

struct TermAt {
    df: u64,
    /// Per segment, per scheme.
    at: Vec<[Extent; NS]>,
}

/// A trace term in the segment at hand: its groups and, per page scheme,
/// where each group's container starts within its payload (a prefix sum,
/// one more entry than groups).
#[derive(Default, Clone)]
struct SegTerm {
    present: bool,
    df: u32,
    groups: Vec<u32>,
    at: [Vec<u32>; NPS],
    sparse: [bool; NPS],
}

/// Times loading a forced grid's members into a row as the OR walk does
/// (`kernels::load` of the grid, or clearing the row and setting each
/// member of the group's Elias-Fano list), the same followed by a visit of
/// every set bit (the walk's candidate loop at its cheapest), and a count
/// fold against another term's row (`and_count_bytes`, or a bit test per
/// list member). Containers: up to 20,000 forced grids of the segment, by
/// density band.
fn decode_bench(dump: &std::path::Path, name: &str) {
    use engine::tinshape::kernels;
    use segment::tinshape::ef::Ef;
    let blob = std::fs::read(dump.join(name)).expect("segment");
    let segment = Segment::parse(&blob).expect("parse");
    let geometry = &segment.docs.geometry;
    // (density band, group words, grid bytes, ef bytes, members, slots,
    // paged bytes, group index)
    type Sample = (usize, usize, Vec<u8>, Vec<u8>, u32, u32, Vec<u8>, usize);
    let mut samples: Vec<Sample> = Vec::new();
    let mut per_band = [0usize; 4];
    let mut seen = 0u64;
    for item in segment.dictionary().iter() {
        let (_, entry) = item.expect("term map");
        if u64::from(entry.df) < GRID_MIN_POSTINGS {
            continue;
        }
        let found = segment.resolve(entry).expect("record");
        let Form::Grouped(entries) = &found.postings.form else {
            continue;
        };
        for e in entries.iter() {
            if e.kind != KIND_GRID {
                continue;
            }
            seen += 1;
            if !seen.is_multiple_of(7) {
                continue;
            }
            let group = &geometry.groups[e.index as usize];
            let bytes = found
                .postings
                .container(e)
                .all()
                .expect("container")
                .to_vec();
            let n = e.count;
            // Bands of members per slot: <1/32, <1/16, <1/8, >=1/8.
            let density = f64::from(n) / f64::from(group.slots());
            let band = if density < 1.0 / 32.0 {
                0
            } else if density < 1.0 / 16.0 {
                1
            } else if density < 1.0 / 8.0 {
                2
            } else {
                3
            };
            if per_band[band] >= 5000 {
                continue;
            }
            per_band[band] += 1;
            let mut locals = Vec::with_capacity(n as usize);
            for (w, c) in bytes.chunks(8).enumerate() {
                let mut word = 0u64;
                for (i, b) in c.iter().enumerate() {
                    word |= u64::from(*b) << (8 * i);
                }
                while word != 0 {
                    locals.push((w * 64) as u32 + word.trailing_zeros());
                    word &= word - 1;
                }
            }
            let mut efb = Vec::new();
            ef::encode(&locals, group.slots(), &mut efb);
            let mut paged = Vec::new();
            segment::tinshape::postings::encode_paged(&locals, u32::from(group.width), &mut paged);
            samples.push((
                band,
                group.words(),
                bytes,
                efb,
                n,
                group.slots(),
                paged,
                e.index as usize,
            ));
        }
        if per_band.iter().all(|c| *c >= 5000) {
            break;
        }
    }
    let max_words = samples.iter().map(|s| s.1).max().unwrap_or(0);
    let mut row = vec![0u64; max_words];
    let other: Vec<u64> = (0..max_words as u64)
        .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15) & i.wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
        .collect();
    println!("decode bench: {name}, level {}", kernels::level().name());
    for band in 0..4 {
        let set: Vec<_> = samples.iter().filter(|s| s.0 == band).collect();
        if set.is_empty() {
            continue;
        }
        let members: u64 = set.iter().map(|s| u64::from(s.4)).sum();
        let grid_bytes: u64 = set.iter().map(|s| s.2.len() as u64).sum();
        let ef_bytes: u64 = set.iter().map(|s| s.3.len() as u64).sum();
        let paged_bytes: u64 = set.iter().map(|s| s.6.len() as u64).sum();
        let entry_of = |s: &Sample, kind: u8| segment::tinshape::postings::GroupEntry {
            index: s.7 as u32,
            count: s.4,
            kind,
            at: 0,
            len: 0,
            first: 0,
        };
        let reps = 20;
        let time = |f: &mut dyn FnMut() -> u64| {
            let mut best = f64::MAX;
            let mut sink = 0u64;
            for _ in 0..5 {
                let t = Instant::now();
                for _ in 0..reps {
                    sink = sink.wrapping_add(f());
                }
                best = best.min(t.elapsed().as_secs_f64() / reps as f64);
            }
            std::hint::black_box(sink);
            best * 1e9 / set.len() as f64
        };
        let grid_load = time(&mut || {
            let mut x = 0u64;
            for s in &set {
                let r = &mut row[..s.1];
                kernels::load(r, &s.2);
                x = x.wrapping_add(std::hint::black_box(r[0]));
            }
            x
        });
        let ef_load = time(&mut || {
            let mut x = 0u64;
            for s in &set {
                let r = &mut row[..s.1];
                r.fill(0);
                let list = Ef::parse(&s.3, s.4 as usize, s.5).expect("ef");
                let (mut cw, mut bits) = (0usize, 0u64);
                list.for_each(|l| {
                    let w = l as usize / 64;
                    let kept = if w == cw { bits } else { 0 };
                    bits = kept | 1 << (l % 64);
                    cw = w;
                    r[w] = bits;
                });
                x = x.wrapping_add(std::hint::black_box(r[0]));
            }
            x
        });
        let mut list: Vec<u32> = Vec::with_capacity(1 << 16);
        let ef_row_kernel = time(&mut || {
            let mut x = 0u64;
            for s in &set {
                let r = &mut row[..s.1];
                r.fill(0);
                let e = entry_of(s, KIND_EF);
                kernels::container_row(&e, &s.3, &geometry.groups[s.7], r).expect("ef");
                x = x.wrapping_add(std::hint::black_box(r[0]));
            }
            x
        });
        let paged_row = time(&mut || {
            let mut x = 0u64;
            for s in &set {
                let r = &mut row[..s.1];
                r.fill(0);
                let e = entry_of(s, KIND_PAGED);
                kernels::container_row(&e, &s.6, &geometry.groups[s.7], r).expect("paged");
                x = x.wrapping_add(std::hint::black_box(r[0]));
            }
            x
        });
        let ef_list = time(&mut || {
            let mut x = 0u64;
            for s in &set {
                list.clear();
                let e = entry_of(s, KIND_EF);
                kernels::container_locals(&e, &s.3, &geometry.groups[s.7], &mut list).expect("ef");
                x = x.wrapping_add(std::hint::black_box(list.len() as u64));
            }
            x
        });
        let paged_list = time(&mut || {
            let mut x = 0u64;
            for s in &set {
                list.clear();
                let e = entry_of(s, KIND_PAGED);
                kernels::container_locals(&e, &s.6, &geometry.groups[s.7], &mut list)
                    .expect("paged");
                x = x.wrapping_add(std::hint::black_box(list.len() as u64));
            }
            x
        });
        let visit = |r: &[u64]| {
            let mut x = 0u64;
            for (i, w) in r.iter().enumerate() {
                let mut w = *w;
                while w != 0 {
                    x = x.wrapping_add((i * 64) as u64 + u64::from(w.trailing_zeros()));
                    w &= w - 1;
                }
            }
            x
        };
        let grid_visit = time(&mut || {
            let mut x = 0u64;
            for s in &set {
                let r = &mut row[..s.1];
                kernels::load(r, &s.2);
                x = x.wrapping_add(visit(r));
            }
            x
        });
        let ef_visit = time(&mut || {
            let mut x = 0u64;
            for s in &set {
                let r = &mut row[..s.1];
                r.fill(0);
                let list = Ef::parse(&s.3, s.4 as usize, s.5).expect("ef");
                let (mut cw, mut bits) = (0usize, 0u64);
                list.for_each(|l| {
                    let w = l as usize / 64;
                    let kept = if w == cw { bits } else { 0 };
                    bits = kept | 1 << (l % 64);
                    cw = w;
                    r[w] = bits;
                });
                x = x.wrapping_add(visit(r));
            }
            x
        });
        let grid_count = time(&mut || {
            let mut x = 0u64;
            for s in &set {
                x = x.wrapping_add(kernels::and_count_bytes(&other[..s.1], &s.2));
            }
            x
        });
        let ef_count = time(&mut || {
            let mut x = 0u64;
            for s in &set {
                let list = Ef::parse(&s.3, s.4 as usize, s.5).expect("ef");
                list.for_each(|l| x += other[l as usize / 64] >> (l % 64) & 1);
            }
            x
        });
        let m = members as f64 / set.len() as f64;
        println!(
            "  band {band}: {} containers, {:.0} members, grid {:.0} B ef {:.0} B ({:.2}x) paged {:.0} B | ns/container: load grid {:.0} ef-scalar {:.0} ef-kernel {:.0} paged {:.0} | list ef-kernel {:.0} paged {:.0} | load+visit grid {:.0} ef {:.0} | count grid {:.0} ef {:.0} | ef ns/member load {:.2} visit-delta {:.2}",
            set.len(),
            m,
            grid_bytes as f64 / set.len() as f64,
            ef_bytes as f64 / set.len() as f64,
            grid_bytes as f64 / ef_bytes as f64,
            paged_bytes as f64 / set.len() as f64,
            grid_load,
            ef_load,
            ef_row_kernel,
            paged_row,
            ef_list,
            paged_list,
            grid_visit,
            ef_visit,
            grid_count,
            ef_count,
            ef_load / m,
            (ef_visit - grid_visit) / m,
        );
    }
}

fn main() {
    let args = args();
    if let Some(name) = &args.decode {
        decode_bench(&args.dump, name);
        return;
    }
    let manifest = std::fs::read_to_string(args.dump.join("manifest.tsv")).unwrap_or_else(|e| {
        eprintln!("manifest: {e}");
        std::process::exit(1)
    });
    let mut segments = Vec::new();
    let mut spec_hex = None;
    for line in manifest.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["segment", _, docs, name, _] => {
                if args.only.as_deref().is_none_or(|o| o == *name) {
                    segments.push(((*name).to_owned(), docs.parse::<u64>().unwrap_or(0)));
                }
            }
            ["spec", hex] => spec_hex = Some((*hex).to_owned()),
            _ => {}
        }
    }
    // Trace terms, per query: (style, terms).
    let mut queries: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut name_at: FxHashMap<String, usize> = FxHashMap::default();
    if let Some(trace) = &args.trace {
        let hex = spec_hex.expect("a spec");
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
            .collect();
        let spec = <[u8; engine::spec::SPEC_BYTES]>::try_from(bytes).expect("spec bytes");
        let tokenizer = engine::spec::decode_spec(&spec)
            .expect("spec")
            .compile()
            .expect("tokenizer");
        for q in bench::read_trace(trace).expect("trace") {
            let Some(style) = STYLES.iter().position(|s| *s == q.style) else {
                continue;
            };
            let query = tinql::runtime::parse_tinql_to_query(&q.text, &tokenizer).expect("query");
            let mut terms = Vec::new();
            if engine::tinshape::lower(&query, &mut terms).is_none() {
                continue;
            }
            let ids = terms
                .into_iter()
                .map(|t| {
                    *name_at.entry(t.clone()).or_insert_with(|| {
                        names.push(t);
                        names.len() - 1
                    })
                })
                .collect();
            queries.push((style, ids));
        }
        eprintln!("{} queries, {} terms", queries.len(), names.len());
    }
    // Per style, per term, per page scheme: the pages read over the trace
    // (conjunctions and phrases: the lead whole, the others in the lead's
    // groups; disjunctions are counted at the end from the extents).
    let mut style_pages: Vec<Vec<[u64; NPS]>> =
        (0..3).map(|_| vec![[0u64; NPS]; names.len()]).collect();
    // Per term, per page scheme, bytes by form (for attributing pages).
    let mut term_form: Vec<[[u64; NF]; NPS]> = vec![[[0u64; NF]; NPS]; names.len()];
    let mut term_at: Vec<TermAt> = names
        .iter()
        .map(|_| TermAt {
            df: 0,
            at: vec![[Extent::default(); NS]; segments.len()],
        })
        .collect();

    let mut totals = vec![Total::default(); NS];
    let mut postings_by_class = [0u64; 8];
    let mut terms_by_class = [0u64; 8];
    let mut postings = 0u64;
    let mut record_bytes = 0u64;
    // Shapes by the actual container kind: forced grid, other grid, ef,
    // paged; and of sparse terms' groups.
    let mut shapes: BTreeMap<&str, Shape> = BTreeMap::new();
    // Doc set census.
    let (mut groups_n, mut slots_n, mut docs_n, mut pages_n, mut full_pages_n) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut pgrid_bits = 0u64;
    let mut docs_total = 0u64;
    let mut mismatch = 0u64;
    let mut locals: Vec<u32> = Vec::new();
    // Forced grids by density band (<1/32, <1/16, <1/8, >=1/8):
    // containers, postings, grid bytes, ef bytes, pages present.
    let mut bands = [(0u64, 0u64, 0u64, 0u64, 0u64); 4];
    let mut costs: Vec<(usize, Costs)> = Vec::new();

    for (si, (name, docs)) in segments.iter().enumerate() {
        let start = Instant::now();
        docs_total += docs;
        let blob = std::fs::read(args.dump.join(name)).unwrap_or_else(|e| {
            eprintln!("{name}: {e}");
            std::process::exit(1)
        });
        let segment = Segment::parse(&blob).unwrap_or_else(|e| {
            eprintln!("{name}: {e}");
            std::process::exit(1)
        });
        let geometry = &segment.docs.geometry;
        let gp = GroupPages::of(&segment);
        for (index, group) in geometry.groups.iter().enumerate() {
            groups_n += 1;
            slots_n += u64::from(group.slots());
            let b = gp.base[index];
            for page in 0..group.pages as usize {
                let d = gp.docs[b + page];
                if d > 0 {
                    pages_n += 1;
                    docs_n += u64::from(d);
                    full_pages_n += u64::from(d == gp.max[b + page]);
                }
                pgrid_bits += u64::from(gp.max[b + page]);
            }
        }
        let postings_at = segment.area_at(Area::Postings) as u64;
        // Each scheme's running record offset in the postings area.
        let mut offset = [0u64; NS];
        let mut seg_terms: Vec<SegTerm> = (0..names.len()).map(|_| SegTerm::default()).collect();
        let mut group_idx: Vec<u32> = Vec::new();
        let mut group_bytes: Vec<Vec<u32>> = (0..NPS).map(|_| Vec::new()).collect();
        let mut sparse_chosen: [bool; NPS];
        for item in segment.dictionary().iter() {
            let (term, entry) = item.expect("term map");
            let found = segment.resolve(entry).expect("record");
            let p = &found.postings;
            let df = entry.df;
            let class = df_class(df);
            terms_by_class[class] += 1;
            postings_by_class[class] += u64::from(df);
            postings += u64::from(df);
            let record_len = u64::from(entry.ordinals.len);
            record_bytes += record_len;
            // The record's payload as it is: directory and containers.
            let (actual_payload, actual_directory) = match &p.form {
                Form::Single(_) => (0u64, 0u64),
                Form::Sparse(_) => (p.payload.len() as u64, 0),
                Form::Grouped(_) => (
                    (p.payload.len() - p.containers_at) as u64,
                    p.containers_at as u64,
                ),
            };
            let trace = name_at.get(&term).copied();
            let mut chosen = [(0u64, 0u64); NS];
            group_idx.clear();
            for g in group_bytes.iter_mut() {
                g.clear();
            }
            sparse_chosen = [false; NPS];
            match &p.form {
                Form::Single(_) => {
                    totals[S_ACTUAL].forms[0] += 1;
                }
                _ => {
                    // Members by group.
                    costs.clear();
                    let mut actual_kinds: Vec<(u8, usize, bool)> = Vec::new();
                    let mut slots_of_term: Vec<u32> = Vec::new();
                    p.for_each_slot(geometry, |s| slots_of_term.push(s))
                        .expect("slots");
                    assert_eq!(slots_of_term.len(), df as usize, "{term}");
                    if let Form::Grouped(entries) = &p.form {
                        for e in entries.iter() {
                            actual_kinds.push((e.kind, e.len as usize, false));
                        }
                    }
                    let mut at = 0usize;
                    let mut index = geometry.group_of_slot(slots_of_term[0]);
                    let mut gi = 0usize;
                    while at < slots_of_term.len() {
                        while geometry.groups[index].slot_base + geometry.groups[index].slots()
                            <= slots_of_term[at]
                        {
                            index += 1;
                        }
                        let group = &geometry.groups[index];
                        locals.clear();
                        while at < slots_of_term.len()
                            && slots_of_term[at] < group.slot_base + group.slots()
                        {
                            locals.push(slots_of_term[at] - group.slot_base);
                            at += 1;
                        }
                        let key = match actual_kinds.get(gi) {
                            Some((KIND_GRID, _, _)) => {
                                let forced = u64::from(df) >= GRID_MIN_POSTINGS
                                    && locals.len() as u64 * GRID_DENSITY
                                        >= u64::from(group.slots());
                                if forced { "grid forced" } else { "grid" }
                            }
                            Some((KIND_EF, _, _)) => "ef",
                            Some((KIND_PAGED, _, _)) => "paged",
                            Some(_) => "other",
                            None => "sparse-term",
                        };
                        if key == "grid forced" {
                            let d = locals.len() as f64 / f64::from(group.slots());
                            let band = if d < 1.0 / 32.0 {
                                0
                            } else if d < 1.0 / 16.0 {
                                1
                            } else if d < 1.0 / 8.0 {
                                2
                            } else {
                                3
                            };
                            let pages = {
                                let w = u32::from(group.width);
                                let mut np = 0u64;
                                let mut last = u32::MAX;
                                for l in &locals {
                                    if l / w != last {
                                        np += 1;
                                        last = l / w;
                                    }
                                }
                                np
                            };
                            bands[band].0 += 1;
                            bands[band].1 += locals.len() as u64;
                            bands[band].2 += group.grid_bytes() as u64;
                            bands[band].3 += ef::encoded_len(locals.len(), group.slots()) as u64;
                            bands[band].4 += pages;
                        }
                        let shape = shapes.entry(key).or_default();
                        if let Some((_, len, _)) = actual_kinds.get(gi) {
                            shape.bytes += *len as u64;
                        }
                        let c = price(group, &gp, index, &locals, Some(shape));
                        costs.push((index, c));
                        gi += 1;
                    }
                    let slots = geometry.slots;
                    let sparse_len = ef::encoded_len(df as usize, slots) as u64;
                    for s in 1..NS {
                        let mut payload = 0u64;
                        let mut directory = varint_len(costs.len() as u64) as u64;
                        let mut any_forced = false;
                        let mut previous: Option<usize> = None;
                        let mut by_form = [0u64; NF];
                        let mut forms = [0u64; NF];
                        let ps = PAGE_SCHEMES.iter().position(|&x| x == s);
                        for (index, c) in &costs {
                            let gslots = geometry.groups[*index].slots();
                            let (form, bytes) = choose(s, c, df, gslots);
                            if trace.is_some() {
                                if let Some(ps) = ps {
                                    group_bytes[ps].push(bytes as u32);
                                }
                                if s == 1 {
                                    group_idx.push(*index as u32);
                                }
                            }
                            any_forced |= forced_in(s, c, df, gslots);
                            payload += bytes as u64;
                            by_form[form] += bytes as u64;
                            forms[form] += 1;
                            let gap = match previous {
                                None => *index,
                                Some(p) => index - p - 1,
                            };
                            previous = Some(*index);
                            let kind_bits = if s <= 5 { 2 } else { 3 };
                            directory += varint_len(gap as u64) as u64
                                + varint_len(u64::from(c.n - 1) << kind_bits) as u64;
                            if form == F_PAGED || form == F_SHARED {
                                directory += varint_len(bytes as u64) as u64;
                            }
                        }
                        let sparse_ok = SCHEMES[s] != "tin" && !any_forced;
                        let t = &mut totals[s];
                        if sparse_ok && sparse_len <= payload + directory {
                            chosen[s] = (sparse_len, 0);
                            t.by_form[1] += sparse_len;
                            t.forms[1] += 1;
                            if let (Some(tr), Some(ps)) = (trace, ps) {
                                sparse_chosen[ps] = true;
                                term_form[tr][ps][1] += sparse_len;
                            }
                        } else {
                            if let (Some(tr), Some(ps)) = (trace, ps) {
                                for f in 0..NF {
                                    term_form[tr][ps][f] += by_form[f];
                                }
                            }
                            chosen[s] = (payload, directory);
                            for f in 0..NF {
                                t.by_form[f] += by_form[f];
                                t.forms[f] += forms[f];
                            }
                        }
                    }
                    // Actual.
                    let t = &mut totals[S_ACTUAL];
                    match &p.form {
                        Form::Sparse(_) => {
                            t.by_form[1] += actual_payload;
                            t.forms[1] += 1;
                        }
                        Form::Grouped(entries) => {
                            for e in entries.iter() {
                                let f = match e.kind {
                                    KIND_GRID => F_GRID,
                                    KIND_EF => F_EF,
                                    _ => F_PAGED,
                                };
                                t.by_form[f] += u64::from(e.len);
                                t.forms[f] += 1;
                            }
                        }
                        Form::Single(_) => {}
                    }
                    if chosen[1].0 + chosen[1].1 != actual_payload + actual_directory {
                        mismatch += 1;
                        if mismatch <= 5 {
                            eprintln!(
                                "{term}: modeled cur {}+{} vs actual {}+{}",
                                chosen[1].0, chosen[1].1, actual_payload, actual_directory
                            );
                        }
                    }
                }
            }
            chosen[S_ACTUAL] = (actual_payload, actual_directory);
            for s in 0..NS {
                let t = &mut totals[s];
                t.payload += chosen[s].0;
                t.directory += chosen[s].1;
                t.by_class[class] += chosen[s].0 + chosen[s].1;
            }
            // Layout: the record's start, and where its containers begin.
            let containers_in_record = match &p.form {
                Form::Single(_) => 0,
                Form::Sparse(_) => p.payload_at as u64,
                Form::Grouped(_) => (p.payload_at + p.containers_at) as u64,
            };
            for s in 0..NS {
                let (pay, dir) = chosen[s];
                if let Some(t) = trace {
                    let at =
                        postings_at + offset[s] + (containers_in_record - actual_directory) + dir;
                    term_at[t].at[si][s] = Extent {
                        at,
                        len: if matches!(p.form, Form::Single(_)) {
                            0
                        } else {
                            pay
                        },
                    };
                }
                offset[s] += record_len - actual_payload - actual_directory + pay + dir;
            }
            if let Some(t) = trace {
                term_at[t].df += u64::from(df);
                let st = &mut seg_terms[t];
                st.present = true;
                st.df = df;
                st.groups = group_idx.clone();
                st.sparse = sparse_chosen;
                for ps in 0..NPS {
                    let mut at = Vec::with_capacity(group_bytes[ps].len() + 1);
                    let mut sum = 0u32;
                    at.push(0);
                    for b in &group_bytes[ps] {
                        sum += b;
                        at.push(sum);
                    }
                    st.at[ps] = at;
                }
            }
        }
        // Conjunctions and phrases over this segment: the lead (fewest
        // postings here) is read whole; every other term's containers are
        // read in the groups the lead holds (a sparse list whole).
        for (style, q) in &queries {
            if *style == 1 || q.iter().any(|&t| !seg_terms[t].present) {
                continue;
            }
            let lead = *q.iter().min_by_key(|&&t| seg_terms[t].df).expect("terms");
            for (ps, &s) in PAGE_SCHEMES.iter().enumerate() {
                for &t in q {
                    let st = &seg_terms[t];
                    let ext = term_at[t].at[si][s];
                    let mut set: FxHashSet<u64> = FxHashSet::default();
                    if ext.len == 0 {
                        continue;
                    }
                    if t == lead || st.sparse[ps] || st.groups.is_empty() {
                        let first = ext.at / PAGE_DATA as u64;
                        let last = (ext.at + ext.len - 1) / PAGE_DATA as u64;
                        set.extend(first..=last);
                    } else {
                        let lg = &seg_terms[lead].groups;
                        let (mut i, mut j) = (0usize, 0usize);
                        while i < lg.len() && j < st.groups.len() {
                            if lg[i] < st.groups[j] {
                                i += 1;
                            } else if lg[i] > st.groups[j] {
                                j += 1;
                            } else {
                                let (a, b) = (st.at[ps][j], st.at[ps][j + 1]);
                                if b > a {
                                    let first = (ext.at + u64::from(a)) / PAGE_DATA as u64;
                                    let last = (ext.at + u64::from(b) - 1) / PAGE_DATA as u64;
                                    set.extend(first..=last);
                                }
                                i += 1;
                                j += 1;
                            }
                        }
                    }
                    style_pages[*style][t][ps] += set.len() as u64;
                }
            }
        }
        drop(seg_terms);
        eprintln!(
            "{name}: {:.1} s, {} groups",
            start.elapsed().as_secs_f64(),
            geometry.groups.len()
        );
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "segments {} documents {docs_total} postings {postings}",
        segments.len()
    );
    let _ = writeln!(
        out,
        "postings records {record_bytes} bytes; modeled-cur mismatches {mismatch}"
    );
    let _ = writeln!(
        out,
        "docset: groups {groups_n} slots {slots_n} docs {docs_n} ({:.3} slots/doc) pages {pages_n} ({:.1} docs/page, {:.1} pages/group), full pages {full_pages_n} ({:.1}%), pgrid bits {pgrid_bits} ({:.3}/doc)",
        slots_n as f64 / docs_n as f64,
        docs_n as f64 / pages_n as f64,
        pages_n as f64 / groups_n as f64,
        100.0 * full_pages_n as f64 / pages_n as f64,
        pgrid_bits as f64 / docs_n as f64,
    );
    let _ = writeln!(out, "\nterms and postings by df class");
    for c in 0..8 {
        let _ = writeln!(
            out,
            "  {:>13} terms {:>10} postings {:>13} ({:.1}%)",
            DF_CLASSES[c],
            terms_by_class[c],
            postings_by_class[c],
            100.0 * postings_by_class[c] as f64 / postings as f64
        );
    }
    let _ = writeln!(
        out,
        "\nschemes: payload (containers/lists) + directory, B/posting, vs actual"
    );
    let base = (totals[S_ACTUAL].payload + totals[S_ACTUAL].directory) as f64;
    for s in 0..NS {
        let t = &totals[s];
        let all = (t.payload + t.directory) as f64;
        let _ = writeln!(
            out,
            "  {:>11} payload {:>14} dir {:>12} total {:>14} {:.3} B/posting {:+.1}%",
            SCHEMES[s],
            t.payload,
            t.directory,
            t.payload + t.directory,
            all / postings as f64,
            100.0 * (all / base - 1.0)
        );
    }
    let _ = writeln!(out, "\nbytes by form (count) per scheme");
    for s in 0..NS {
        let t = &totals[s];
        let mut line = format!("  {:>11}", SCHEMES[s]);
        for (f, form) in FORMS.iter().enumerate() {
            if t.forms[f] > 0 {
                let _ = write!(line, " {form} {} ({})", t.by_form[f], t.forms[f]);
            }
        }
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out, "\npayload+directory bytes by df class per scheme");
    let _ = write!(out, "  {:>11}", "");
    for c in DF_CLASSES {
        let _ = write!(out, " {c:>13}");
    }
    let _ = writeln!(out);
    for s in 0..NS {
        let _ = write!(out, "  {:>11}", SCHEMES[s]);
        for c in 0..8 {
            let _ = write!(out, " {:>13}", totals[s].by_class[c]);
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "\ncontainer shapes by actual kind");
    for (k, s) in &shapes {
        let _ = writeln!(
            out,
            "  {k}: containers {} postings {} bytes {} pages {} ({:.1}/container, {:.2} members/page); \
             pages full {:.1}% run {:.1}% single {:.1}%; of {} multi-page containers identical masks {:.2}% (all full {:.2}%); all pages full {:.2}%; \
             by pages [1,2-3,4-15,16-63,64-255,256] {:?}; pages by members [1,2,3-4,5-8,9-16,17+] {:?}",
            s.containers,
            s.postings,
            s.bytes,
            s.pages,
            s.pages as f64 / s.containers.max(1) as f64,
            s.postings as f64 / s.pages.max(1) as f64,
            100.0 * s.full_pages as f64 / s.pages.max(1) as f64,
            100.0 * s.run_pages as f64 / s.pages.max(1) as f64,
            100.0 * s.single_pages as f64 / s.pages.max(1) as f64,
            s.multi,
            100.0 * s.identical as f64 / s.multi.max(1) as f64,
            100.0 * s.identical_full as f64 / s.multi.max(1) as f64,
            100.0 * s.all_full as f64 / s.containers.max(1) as f64,
            s.by_pages,
            s.by_members,
        );
    }
    let _ = writeln!(
        out,
        "\nforced grids by density band: containers, postings, grid bytes, ef bytes, pages present"
    );
    for (b, label) in ["<1/32", "1/32-1/16", "1/16-1/8", ">=1/8"]
        .iter()
        .enumerate()
    {
        let (c, n, g, e, pg) = bands[b];
        let _ = writeln!(
            out,
            "  {label:>10} containers {c} postings {n} grid {g} ef {e} pages {pg} ({:.0} members/container, {:.2}/page)",
            n as f64 / c.max(1) as f64,
            n as f64 / pg.max(1) as f64
        );
    }
    if !queries.is_empty() {
        let missing = term_at.iter().filter(|t| t.df == 0).count();
        let _ = writeln!(
            out,
            "\ndisjunction payload pages per query (distinct; every group of each present term), {} queries, {} of {} terms absent",
            queries.iter().filter(|(st, _)| *st == 1).count(),
            missing,
            names.len()
        );
        let nq: Vec<usize> = (0..3)
            .map(|st| queries.iter().filter(|(s, _)| *s == st).count())
            .collect();
        let class_of = |t: usize| df_class(term_at[t].df.min(u64::from(u32::MAX)) as u32);
        // Pages per query by df class and by form, per style and page scheme.
        let mut by_class: Vec<Vec<[f64; 9]>> = (0..3).map(|_| vec![[0f64; 9]; NPS]).collect();
        let mut by_form_pages: Vec<Vec<[f64; NF]>> =
            (0..3).map(|_| vec![[0f64; NF]; NPS]).collect();
        for style in [0usize, 2] {
            for ps in 0..NPS {
                for t in 0..names.len() {
                    let pages = style_pages[style][t][ps] as f64 / nq[style].max(1) as f64;
                    by_class[style][ps][class_of(t)] += pages;
                    by_class[style][ps][8] += pages;
                    let tf = &term_form[t][ps];
                    let total: u64 = tf.iter().sum();
                    if total > 0 {
                        for f in 0..NF {
                            by_form_pages[style][ps][f] += pages * tf[f] as f64 / total as f64;
                        }
                    }
                }
            }
        }
        for (ps, &s) in PAGE_SCHEMES.iter().enumerate() {
            for (_, q) in queries.iter().filter(|(st, _)| *st == 1) {
                for &t in q {
                    let ta = &term_at[t];
                    if ta.df as f64 >= 0.10 * docs_total as f64 {
                        continue;
                    }
                    let mut pages = 0u64;
                    for per in ta.at.iter() {
                        let e = per[s];
                        if e.len == 0 {
                            continue;
                        }
                        pages +=
                            (e.at + e.len - 1) / PAGE_DATA as u64 - e.at / PAGE_DATA as u64 + 1;
                    }
                    let pages = pages as f64 / nq[1].max(1) as f64;
                    by_class[1][ps][class_of(t)] += pages;
                    by_class[1][ps][8] += pages;
                    let tf = &term_form[t][ps];
                    let total: u64 = tf.iter().sum();
                    if total > 0 {
                        for f in 0..NF {
                            by_form_pages[1][ps][f] += pages * tf[f] as f64 / total as f64;
                        }
                    }
                }
            }
        }
        for style in 0..3 {
            let _ = writeln!(
                out,
                "\n{} payload pages per query by the term's df class ({} queries; score: terms of 10% of the documents elided; AND/phrase: lead whole, others in the lead's groups)",
                STYLES[style], nq[style]
            );
            let _ = write!(out, "  {:>11}", "");
            for c in DF_CLASSES {
                let _ = write!(out, " {c:>13}");
            }
            let _ = writeln!(out, " {:>13}", "total");
            for (ps, &s) in PAGE_SCHEMES.iter().enumerate() {
                let _ = write!(out, "  {:>11}", SCHEMES[s]);
                for c in 0..9 {
                    let _ = write!(out, " {:>13.1}", by_class[style][ps][c]);
                }
                let _ = writeln!(out);
            }
            let _ = writeln!(
                out,
                "  by form (pages attributed by the term's bytes per form):"
            );
            for (ps, &s) in PAGE_SCHEMES.iter().enumerate() {
                let mut line = format!("  {:>11}", SCHEMES[s]);
                for f in 0..NF {
                    if by_form_pages[style][ps][f] > 0.05 {
                        let _ = write!(line, " {} {:.1}", FORMS[f], by_form_pages[style][ps][f]);
                    }
                }
                let _ = writeln!(out, "{line}");
            }
        }
        for (label, elide) in [("score", true), ("full_score", false)] {
            let mut line = format!("  {label:>10}");
            for s in 0..NS {
                let mut sum = 0u64;
                for (_, q) in queries.iter().filter(|(st, _)| *st == 1) {
                    let mut set: FxHashSet<(usize, u64)> = FxHashSet::default();
                    for &t in q {
                        let ta = &term_at[t];
                        if elide && ta.df as f64 >= 0.10 * docs_total as f64 {
                            continue;
                        }
                        for (si, per) in ta.at.iter().enumerate() {
                            let e = per[s];
                            if e.len == 0 {
                                continue;
                            }
                            let first = e.at / PAGE_DATA as u64;
                            let last = (e.at + e.len - 1) / PAGE_DATA as u64;
                            for page in first..=last {
                                set.insert((si, page));
                            }
                        }
                    }
                    sum += set.len() as u64;
                }
                let _ = write!(
                    line,
                    " {}={:.0}",
                    SCHEMES[s],
                    sum as f64 / nq[1].max(1) as f64
                );
            }
            let _ = writeln!(out, "{line}");
        }
    }
    print!("{out}");
    if let Some(path) = &args.out {
        std::fs::write(path, &out).expect("out");
    }
}
