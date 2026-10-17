// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! What per-sub-range bound codes would add to a dump's index (lane
//! subrange): for every grouped record's grid containers, per sub-range
//! size (64, 256, 1,024 slots) and encoding, the bytes the codes would
//! take, in total and for terms above a document frequency.
//!
//! Encodings (see `engine::tinshape::census`): `st` one u8 length code per
//! sub-range and frontier pair of the group (R10's layout); `bl` a bucket
//! and a length code, 12 bits a sub-range; `mb` a bucket, 4 bits a
//! sub-range. Each container's codes round up to whole bytes.
//!
//! ```text
//! cargo run -p bench --release --bin tnssubsize -- DIR
//! ```

use std::path::PathBuf;

use segment::tinshape::postings::{Form, GroupFrontier, KIND_GRID};
use segment::tinshape::segment::Segment;

const SHIFTS: [u32; 3] = [6, 8, 10];
/// Document frequency floors, as fractions of the segment's documents.
const DF: [f64; 4] = [0.0, 0.01, 0.05, 0.10];

#[derive(Default)]
struct Totals {
    file_bytes: u64,
    grids: [u64; 4],
    pairs: [u64; 4],
    /// Per df floor, size, encoding.
    bytes: [[[u64; 3]; 3]; 4],
}

/// A read-only mapping of a file (only the pages read are paged in).
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

fn census(path: &std::path::Path, t: &mut Totals) {
    let map = Map::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let blob = map.bytes();
    let name = path.display().to_string();
    let segment = Segment::parse(blob).unwrap_or_else(|e| panic!("{name}: {e}"));
    let groups = &segment.docs.geometry.groups;
    let docs = f64::from(segment.documents);
    t.file_bytes += blob.len() as u64;
    for item in segment.dictionary().iter() {
        let (term, entry) = item.unwrap_or_else(|e| panic!("{name}: {e}"));
        let found = segment
            .resolve(entry)
            .unwrap_or_else(|e| panic!("{name} {term}: {e}"));
        let p = &found.postings;
        let Form::Grouped(dir) = &p.form else {
            continue;
        };
        let df = entry.df as f64 / docs;
        let entries = dir
            .entries(groups)
            .unwrap_or_else(|e| panic!("{term}: {e}"));
        for e in entries.iter().filter(|e| e.kind == KIND_GRID) {
            let slots = groups[e.index as usize].slots();
            let pairs =
                GroupFrontier::new(dir.bytes(), e.frontier as usize, p.rounded).count() as u64;
            for (d, floor) in DF.iter().enumerate() {
                if df < *floor {
                    continue;
                }
                t.grids[d] += 1;
                t.pairs[d] += pairs;
                for (k, shift) in SHIFTS.iter().enumerate() {
                    let subs = u64::from(slots.div_ceil(1 << shift));
                    t.bytes[d][k][0] += subs * pairs;
                    t.bytes[d][k][1] += (subs * 12).div_ceil(8);
                    t.bytes[d][k][2] += subs.div_ceil(2);
                }
            }
        }
    }
}

fn main() {
    let dir: PathBuf = std::env::args().nth(1).expect("a dump directory").into();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("a dump directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "segment"))
        .collect();
    paths.sort();
    let mut t = Totals::default();
    for p in &paths {
        census(p, &mut t);
        eprintln!("{} done", p.display());
    }
    println!("segments {:.3} GB", t.file_bytes as f64 / 1e9);
    for (d, floor) in DF.iter().enumerate() {
        println!(
            "df >= {:.0}%: grids {} pairs {} ({:.2} a grid)",
            floor * 100.0,
            t.grids[d],
            t.pairs[d],
            t.pairs[d] as f64 / t.grids[d].max(1) as f64
        );
        for (k, shift) in SHIFTS.iter().enumerate() {
            let line: Vec<String> = ["st", "bl", "mb"]
                .iter()
                .enumerate()
                .map(|(e, n)| {
                    let b = t.bytes[d][k][e];
                    format!(
                        "{n} {:.1} MB ({:+.3}%)",
                        b as f64 / 1e6,
                        100.0 * b as f64 / t.file_bytes as f64
                    )
                })
                .collect();
            println!("  s{}: {}", 1u32 << shift, line.join("  "));
        }
    }
}
