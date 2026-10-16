// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Census of a dump's grouped containers by density: per container kind and
//! band of a group's members per slot (below 1 in 64, 1 in 64, 1 in 32, ...
//! 1 in 2 or more) how many groups, members and bytes, and for grids what
//! the group would take as the smallest list form (Elias-Fano, or an array
//! of at most `Options::array_max` members) and as the paged container
//! (TIN's page mask with a list or bitmap per page). Also how many groups
//! hold 2,048 or more members, and the slots a group spans.
//!
//! ```text
//! cargo run -p bench --release --bin tnsmidcensus -- --dump DIR [--terms FILE] [--trace FILE] [--segment FILE]
//! ```
//!
//! Each segment is mapped, not read, so a 15 GB segment costs only the
//! pages its dictionary and payloads take. `--terms FILE` writes, per
//! segment and term holding a grid, the term's df and per band its grids,
//! their bytes and their smallest other form's bytes (to weigh by a trace);
//! with `--trace FILE` (a replay trace: id, style, query), every term of
//! its queries instead, grid or not, with its record's payload bytes.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;

use segment::tinshape::ef;
use segment::tinshape::postings::{
    Form, KIND_ARRAY, KIND_EF, KIND_GRID, KIND_PAGED, Options, paged_len,
};
use segment::tinshape::segment::Segment;

const BANDS: usize = 7;
const BAND_NAMES: [&str; BANDS] = ["<1/64", "1/64", "1/32", "1/16", "1/8", "1/4", ">=1/2"];
const KIND_NAMES: [&str; 4] = ["grid", "ef", "paged", "array"];

/// The band of `count` members over `slots`: 0 below one in 64, then one
/// band per doubling up to one in two or more.
fn band(count: u32, slots: u32) -> usize {
    let r = u64::from(count) * 64;
    let s = u64::from(slots.max(1));
    if r < s {
        return 0;
    }
    // 1 + floor(log2(r / s)), at most BANDS - 1.
    let q = r / s;
    (1 + (63 - q.leading_zeros()) as usize).min(BANDS - 1)
}

#[derive(Default, Clone, Copy)]
struct Cell {
    groups: u64,
    members: u64,
    bytes: u64,
    /// Grids: the smallest list form (ef or array), the paged form, the
    /// smaller of the two, and the group's non-empty pages and slots.
    list: u64,
    paged: u64,
    best: u64,
    pages: u64,
    slots: u64,
}

impl Cell {
    fn add(&mut self, o: &Cell) {
        self.groups += o.groups;
        self.members += o.members;
        self.bytes += o.bytes;
        self.list += o.list;
        self.paged += o.paged;
        self.best += o.best;
        self.pages += o.pages;
        self.slots += o.slots;
    }
}

#[derive(Default)]
struct Totals {
    cells: [[Cell; BANDS]; 4],
    file_bytes: u64,
    documents: u64,
    groups: u64,
    slots: u64,
    big_groups: u64,
    big_members: u64,
    max_slots: u32,
    sparse_bytes: u64,
    tf_bytes: u64,
    record_bytes: u64,
}

/// A read-only mapping of a file.
struct Map {
    ptr: *mut u8,
    len: usize,
}

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

impl Map {
    fn open(path: &std::path::Path) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        // PROT_READ 1, MAP_PRIVATE 2 on Linux and macOS.
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, file.as_raw_fd(), 0) };
        if ptr as isize == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { ptr, len })
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        unsafe { munmap(self.ptr, self.len) };
    }
}

fn census(
    name: &str,
    blob: &[u8],
    t: &mut Totals,
    terms: &mut Option<std::fs::File>,
    wanted: &Option<std::collections::HashSet<String>>,
) {
    let options = Options::default();
    let segment = Segment::parse(blob).unwrap_or_else(|e| panic!("{name}: {e}"));
    let geometry = &segment.docs.geometry;
    t.file_bytes += blob.len() as u64;
    t.documents += u64::from(segment.documents);
    t.groups += geometry.groups.len() as u64;
    t.slots += u64::from(geometry.slots);
    t.max_slots = t
        .max_slots
        .max(geometry.groups.iter().map(|g| g.slots()).max().unwrap_or(0));
    let mut locals = Vec::new();
    let mut line = String::new();
    for item in segment.dictionary().iter() {
        let (term, entry) = item.unwrap_or_else(|e| panic!("{name}: {e}"));
        let found = segment
            .resolve(entry)
            .unwrap_or_else(|e| panic!("{name} {term}: {e}"));
        let p = &found.postings;
        t.tf_bytes += p.tf.len() as u64;
        t.record_bytes += (p.tf_at + p.tf.len()) as u64;
        let want = wanted.as_ref().map(|w| w.contains(&*term));
        let Form::Grouped(dir) = &p.form else {
            t.sparse_bytes += p.payload.len() as u64;
            if want == Some(true)
                && let Some(f) = terms.as_mut()
            {
                let mut line = format!("{name}\t{term}\t{}\ts\t{}", entry.df, p.payload.len());
                for _ in 0..BANDS {
                    line.push_str("\t0\t0\t0");
                }
                line.push('\n');
                f.write_all(line.as_bytes()).expect("terms file");
            }
            continue;
        };
        let entries = dir
            .entries(&geometry.groups)
            .unwrap_or_else(|e| panic!("{term}: {e}"));
        let mut mine = [[0u64; 3]; BANDS];
        let mut any_grid = false;
        for e in &entries {
            let group = &geometry.groups[e.index as usize];
            let b = band(e.count, group.slots());
            if e.count >= 2048 {
                t.big_groups += 1;
                t.big_members += u64::from(e.count);
            }
            let mut cell = Cell {
                groups: 1,
                members: u64::from(e.count),
                bytes: u64::from(e.len),
                ..Cell::default()
            };
            if e.kind == KIND_GRID {
                any_grid = true;
                let bytes = p.container(e).all().expect("a container");
                locals.clear();
                for (w, chunk) in bytes.chunks(8).enumerate() {
                    let mut word = [0u8; 8];
                    word[..chunk.len()].copy_from_slice(chunk);
                    let mut word = u64::from_le_bytes(word);
                    while word != 0 {
                        locals.push(w as u32 * 64 + word.trailing_zeros());
                        word &= word - 1;
                    }
                }
                debug_assert_eq!(locals.len(), e.count as usize);
                let starts = geometry.starts(e.index as usize);
                let list = if locals.len() as u64 <= u64::from(options.array_max) {
                    2 * locals.len()
                } else {
                    ef::encoded_len(locals.len(), group.slots())
                } as u64;
                let paged = paged_len(&locals, starts) as u64;
                let mut pages = 0u64;
                let mut page = usize::MAX;
                for l in &locals {
                    let p = starts.partition_point(|s| *s <= *l) - 1;
                    if p != page {
                        pages += 1;
                        page = p;
                    }
                }
                cell.list = list;
                cell.paged = paged;
                cell.best = list.min(paged);
                cell.pages = pages;
                cell.slots = u64::from(group.slots());
                mine[b][0] += 1;
                mine[b][1] += cell.bytes;
                mine[b][2] += cell.best;
            }
            let k = usize::from(e.kind & 3);
            debug_assert!(matches!(
                e.kind,
                KIND_GRID | KIND_EF | KIND_PAGED | KIND_ARRAY
            ));
            t.cells[k][b].add(&cell);
        }
        if want.unwrap_or(any_grid)
            && let Some(f) = terms.as_mut()
        {
            line.clear();
            let _ = write!(line, "{name}\t{term}\t{}\tg\t{}", entry.df, p.payload.len());
            for m in &mine {
                let _ = write!(line, "\t{}\t{}\t{}", m[0], m[1], m[2]);
            }
            line.push('\n');
            f.write_all(line.as_bytes()).expect("terms file");
        }
    }
}

fn report(t: &Totals) {
    println!(
        "segments {:.2} GB, documents {}, groups {} ({:.0} slots a group, largest {}), records {:.2} GB, \
         sparse payload {:.2} GB, TF tails {:.2} GB; groups of >= 2,048 members {} ({} members)",
        t.file_bytes as f64 / 1e9,
        t.documents,
        t.groups,
        t.slots as f64 / t.groups.max(1) as f64,
        t.max_slots,
        t.record_bytes as f64 / 1e9,
        t.sparse_bytes as f64 / 1e9,
        t.tf_bytes as f64 / 1e9,
        t.big_groups,
        t.big_members,
    );
    println!(
        "kind\tband\tgroups\tmembers\tbytes\tB/member\tlist\tpaged\tbest\tsaved\tpages\tslots"
    );
    for (k, row) in t.cells.iter().enumerate() {
        let mut all = Cell::default();
        for (b, c) in row.iter().enumerate() {
            all.add(c);
            if c.groups == 0 {
                continue;
            }
            print_cell(KIND_NAMES[k], BAND_NAMES[b], c, k == 0);
        }
        print_cell(KIND_NAMES[k], "all", &all, k == 0);
    }
}

fn print_cell(kind: &str, band: &str, c: &Cell, grid: bool) {
    let per = c.bytes as f64 / c.members.max(1) as f64;
    if grid {
        println!(
            "{kind}\t{band}\t{}\t{}\t{}\t{per:.3}\t{}\t{}\t{}\t{}\t{}\t{}",
            c.groups,
            c.members,
            c.bytes,
            c.list,
            c.paged,
            c.best,
            c.bytes as i64 - c.best as i64,
            c.pages,
            c.slots
        );
    } else {
        println!(
            "{kind}\t{band}\t{}\t{}\t{}\t{per:.3}",
            c.groups, c.members, c.bytes
        );
    }
}

fn main() {
    let mut dump = None;
    let mut terms_path = None;
    let mut only = None;
    let mut trace: Option<PathBuf> = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dump" => dump = it.next().map(PathBuf::from),
            "--terms" => terms_path = it.next().map(PathBuf::from),
            "--segment" => only = it.next(),
            "--trace" => trace = it.next().map(PathBuf::from),
            _ => {
                eprintln!(
                    "usage: tnsmidcensus --dump DIR [--terms FILE] [--trace FILE] [--segment FILE]"
                );
                std::process::exit(2)
            }
        }
    }
    let dump = dump.unwrap_or_else(|| {
        eprintln!("--dump DIR");
        std::process::exit(2)
    });
    let manifest = std::fs::read_to_string(dump.join("manifest.tsv")).expect("manifest.tsv");
    let mut terms = terms_path.map(|p| std::fs::File::create(p).expect("terms file"));
    let wanted = trace.map(|p| {
        let text = std::fs::read_to_string(p).expect("trace");
        let mut set = std::collections::HashSet::new();
        for line in text.lines() {
            let Some(query) = line.split('\t').nth(2) else {
                continue;
            };
            for word in query.split(|c: char| !c.is_alphanumeric()) {
                if !word.is_empty() && !["AND", "OR", "NOT"].contains(&word) {
                    set.insert(word.to_lowercase());
                }
            }
        }
        set
    });
    let mut totals = Totals::default();
    for line in manifest.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let ["segment", _, _, name, _] = f.as_slice() else {
            continue;
        };
        if only.as_deref().is_some_and(|o| o != *name) {
            continue;
        }
        let start = std::time::Instant::now();
        let map = Map::open(&dump.join(name)).expect("segment file");
        let mut t = Totals::default();
        census(name, map.bytes(), &mut t, &mut terms, &wanted);
        eprintln!("{name}: {:.0} s", start.elapsed().as_secs_f64());
        println!("== {name}");
        report(&t);
        for (a, b) in totals.cells.iter_mut().zip(&t.cells) {
            for (x, y) in a.iter_mut().zip(b) {
                x.add(y);
            }
        }
        totals.file_bytes += t.file_bytes;
        totals.documents += t.documents;
        totals.groups += t.groups;
        totals.slots += t.slots;
        totals.big_groups += t.big_groups;
        totals.big_members += t.big_members;
        totals.max_slots = totals.max_slots.max(t.max_slots);
        totals.sparse_bytes += t.sparse_bytes;
        totals.tf_bytes += t.tf_bytes;
        totals.record_bytes += t.record_bytes;
    }
    println!("== all");
    report(&totals);
}
