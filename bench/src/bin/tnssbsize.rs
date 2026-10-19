// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Lab (lane sparsebound): what per-(sparse term, group) bounds would cost
//! in a dumped `TNS1` segment. For every single and sparse record: its
//! postings, the distinct groups they fall in, and the bytes of
//!
//! - E1: one group frontier per (term, group), encoded as a grouped
//!   record's directory frontier (rounded lengths), plus a seek index of a
//!   three-byte entry per footer block;
//! - E1c: E1, but a group of one posting stores only its length's
//!   rounded code in a byte (its bucket is in the TF tail);
//! - E2: a rounded length code byte per posting;
//! - E3: a two-bit length class per posting;
//!
//! split by whether the record already carries its lengths inline (its
//! groups can be bounded exactly with no format change) and by df band.
//!
//! ```text
//! tnssbsize SEGMENT_FILE...
//! ```

use std::os::fd::AsRawFd;

use segment::tinshape::postings::{self, Form, GroupFrontiers};
use segment::tinshape::segment::Segment;

#[derive(Default, Clone, Copy)]
struct Tally {
    records: u64,
    postings: u64,
    pairs: u64,
    pairs_by_count: [u64; 3],
    blocks: u64,
    e1: u64,
    e1c: u64,
}

impl Tally {
    fn add(&mut self, o: &Tally) {
        self.records += o.records;
        self.postings += o.postings;
        self.pairs += o.pairs;
        for k in 0..3 {
            self.pairs_by_count[k] += o.pairs_by_count[k];
        }
        self.blocks += o.blocks;
        self.e1 += o.e1;
        self.e1c += o.e1c;
    }
}

const BANDS: [&str; 6] = ["1", "2-8", "9-64", "65-256", "257-4K", ">4K"];
fn band(df: u32) -> usize {
    match df {
        1 => 0,
        2..=8 => 1,
        9..=64 => 2,
        65..=256 => 3,
        257..=4096 => 4,
        _ => 5,
    }
}

fn map(path: &str) -> &'static [u8] {
    unsafe extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    }
    let file = std::fs::File::open(path).expect("segment file");
    let len = file.metadata().expect("metadata").len() as usize;
    // SAFETY: a read-only private mapping of the whole file, never unmapped.
    let ptr = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, file.as_raw_fd(), 0) };
    assert!(ptr as isize != -1, "mmap failed");
    // SAFETY: the mapping above.
    unsafe { std::slice::from_raw_parts(ptr, len) }
}

fn main() {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    let handles: Vec<_> = paths
        .into_iter()
        .map(|path| {
            std::thread::spawn(move || {
                // [inline?][band]
                let mut t = [[Tally::default(); 6]; 2];
                let blob = map(&path);
                let segment = Segment::parse(blob).expect("a TNS1 segment");
                let geometry = &segment.docs.geometry;
                let mut pairs: Vec<(u8, u32)> = Vec::new();
                let mut fbytes = Vec::new();
                for item in segment.dictionary().iter() {
                    let (term, entry) = item.expect("a term");
                    let found = segment
                        .resolve(entry)
                        .unwrap_or_else(|e| panic!("{term}: {e}"));
                    let p = &found.postings;
                    if matches!(p.form, Form::Grouped(_)) {
                        continue;
                    }
                    let single = matches!(p.form, Form::Single(_));
                    let footer = (!single).then(|| {
                        p.footer(geometry, segment.block_size, entry.max_tf_bucket, segment.adaptive_tf)
                            .expect("a footer")
                    });
                    let slots = p.slots(geometry).expect("slots");
                    let x = &mut t[usize::from(p.lengths.is_some())][band(entry.df)];
                    x.records += 1;
                    x.postings += slots.len() as u64;
                    x.blocks += u64::from(entry.df.div_ceil(segment.block_size));
                    let mut i = 0;
                    let mut index = 0u32;
                    while i < slots.len() {
                        let g = geometry.group_of_slot(slots[i]);
                        let base = geometry.groups[g].slot_base;
                        let end = base + geometry.groups[g].slots();
                        pairs.clear();
                        while i < slots.len() && slots[i] < end {
                            let rank = segment.docs.rank_in(g, slots[i] - base).expect("a document");
                            let length = match &p.lengths {
                                Some(l) => l.get(index).expect("inline length"),
                                None => segment.lengths.get(rank).expect("a length"),
                            };
                            let bucket = match &footer {
                                Some(f) => f.bucket(p.tf, index).expect("a bucket"),
                                None => entry.max_tf_bucket,
                            };
                            pairs.push((bucket, length));
                            index += 1;
                            i += 1;
                        }
                        let f = postings::group_frontier(pairs.iter().copied(), GroupFrontiers::Rounded);
                        fbytes.clear();
                        postings::put_group_frontier(&mut fbytes, &f);
                        x.pairs += 1;
                        x.pairs_by_count[match pairs.len() {
                            1 => 0,
                            2..=3 => 1,
                            _ => 2,
                        }] += 1;
                        x.e1 += fbytes.len() as u64;
                        x.e1c += if pairs.len() == 1 { 1 } else { fbytes.len() as u64 };
                    }
                }
                eprintln!("{path}: done");
                (blob.len() as u64, t)
            })
        })
        .collect();
    let mut total_bytes = 0u64;
    let mut t = [[Tally::default(); 6]; 2];
    for h in handles {
        let (bytes, s) = h.join().expect("a segment thread");
        total_bytes += bytes;
        for a in 0..2 {
            for b in 0..6 {
                t[a][b].add(&s[a][b]);
            }
        }
    }
    println!("segment bytes {total_bytes}");
    println!(
        "{:>6} {:>7} {:>10} {:>12} {:>12} {:>10} {:>28} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "inline", "df", "records", "postings", "pairs", "blocks", "pairs 1/2-3/4+", "E1", "E1+idx", "E1c+idx", "E2", "E3"
    );
    let mut all = [Tally::default(); 2];
    for a in 0..2 {
        for b in 0..6 {
            let x = t[a][b];
            all[a].add(&x);
            row(if a == 1 { "yes" } else { "no" }, BANDS[b], &x);
        }
        row(if a == 1 { "yes" } else { "no" }, "all", &all[a]);
    }
    let mut both = all[0];
    both.add(&all[1]);
    row("any", "all", &both);
    let pct = |v: u64| 100.0 * v as f64 / total_bytes as f64;
    for (label, x) in [("no inline lengths", all[0]), ("all sparse+single", both)] {
        println!(
            "{label}: E1+idx {:.3}%  E1c+idx {:.3}%  E2 {:.3}%  E3 {:.3}% of the index",
            pct(x.e1 + 3 * x.blocks),
            pct(x.e1c + 3 * x.blocks),
            pct(x.postings),
            pct(x.postings.div_ceil(4))
        );
    }
}

fn row(a: &str, b: &str, x: &Tally) {
    println!(
        "{:>6} {:>7} {:>10} {:>12} {:>12} {:>10} {:>28} {:>12} {:>12} {:>12} {:>12} {:>12}",
        a,
        b,
        x.records,
        x.postings,
        x.pairs,
        x.blocks,
        format!("{}/{}/{}", x.pairs_by_count[0], x.pairs_by_count[1], x.pairs_by_count[2]),
        x.e1,
        x.e1 + 3 * x.blocks,
        x.e1c + 3 * x.blocks,
        x.postings,
        x.postings.div_ceil(4)
    );
}
