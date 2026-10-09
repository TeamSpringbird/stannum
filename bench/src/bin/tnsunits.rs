// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! How the units a query reads fall on the extension's pages, over a dump
//! of `TNS1` segments: per kind of unit (a record's header and group
//! directory, a sparse list, a container, a footer and a footer block's
//! entry, a TF block, a DL block and a DL header, a positions block of 32
//! entries), how many there are, how many span two pages or more (a read in
//! place then stitches it, a copy and a second pin), and what padding the
//! writer would add so that no unit of at most `--small` bytes spans a
//! page: per straddling small unit, the bytes left on its first page.
//!
//! ```text
//! cargo run -p bench --release --bin tnsunits -- --dump DIR [--small 1024] [--segments N]
//! ```

use std::path::PathBuf;

use bench::paged::PAGE_DATA;
use segment::tinshape::positions::Positions;
use segment::tinshape::postings::Form;
use segment::tinshape::segment::{Area, Segment};

const KINDS: [&str; 10] = [
    "record header+directory",
    "sparse list",
    "container",
    "footer (whole)",
    "footer block entry",
    "TF block",
    "DL block",
    "DL header",
    "positions block (32 entries)",
    "positions stream (whole)",
];

#[derive(Default, Clone, Copy)]
struct Tally {
    units: u64,
    bytes: u64,
    straddling: u64,
    straddling_bytes: u64,
    small_straddling: u64,
    /// Padding to start every straddling small unit on the next page.
    padding: u64,
    /// Units of at most `small` bytes.
    small: u64,
}

struct Units {
    small: usize,
    tallies: [Tally; KINDS.len()],
}

impl Units {
    fn add(&mut self, kind: usize, at: usize, len: usize) {
        if len == 0 {
            return;
        }
        let t = &mut self.tallies[kind];
        t.units += 1;
        t.bytes += len as u64;
        let small = len <= self.small;
        t.small += u64::from(small);
        let first = at / PAGE_DATA;
        let last = (at + len - 1) / PAGE_DATA;
        if last > first {
            t.straddling += 1;
            t.straddling_bytes += len as u64;
            if small {
                t.small_straddling += 1;
                t.padding += ((first + 1) * PAGE_DATA - at) as u64;
            }
        }
    }
}

fn main() {
    let mut dump = PathBuf::new();
    let mut small = 1024usize;
    let mut limit = usize::MAX;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dump" => dump = it.next().expect("a directory").into(),
            "--small" => small = it.next().expect("bytes").parse().expect("bytes"),
            "--segments" => limit = it.next().expect("a count").parse().expect("a count"),
            _ => panic!("usage: tnsunits --dump DIR [--small BYTES] [--segments N]"),
        }
    }
    let manifest = std::fs::read_to_string(dump.join("manifest.tsv")).expect("a manifest");
    let mut units = Units {
        small,
        tallies: [Tally::default(); KINDS.len()],
    };
    let mut blob_bytes = 0u64;
    let mut segments = 0usize;
    for line in manifest.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let ["segment", generation, _, blob, _] = f.as_slice() else {
            continue;
        };
        if segments == limit {
            break;
        }
        segments += 1;
        let bytes = std::fs::read(dump.join(blob)).expect("a blob");
        blob_bytes += bytes.len() as u64;
        let segment = Segment::parse(&bytes).expect("a TNS1 segment");
        scan(&segment, &mut units);
        eprintln!("segment {generation}: {} bytes", bytes.len());
    }
    println!(
        "{segments} segments, {:.2} GB; pages of {PAGE_DATA} bytes; small: at most {small} bytes",
        blob_bytes as f64 / 1e9
    );
    println!(
        "{:<30} {:>12} {:>10} {:>12} {:>9} {:>10} {:>12} {:>9}",
        "unit", "units", "mean B", "straddling", "%", "small %", "padding MB", "% blob"
    );
    for (kind, t) in KINDS.iter().zip(&units.tallies) {
        if t.units == 0 {
            continue;
        }
        println!(
            "{:<30} {:>12} {:>10.1} {:>12} {:>8.2}% {:>9.2}% {:>12.1} {:>8.3}%",
            kind,
            t.units,
            t.bytes as f64 / t.units as f64,
            t.straddling,
            100.0 * t.straddling as f64 / t.units as f64,
            100.0 * t.small as f64 / t.units as f64,
            t.padding as f64 / 1e6,
            100.0 * t.padding as f64 / blob_bytes as f64
        );
    }
}

fn scan(segment: &Segment<'_>, units: &mut Units) {
    let postings_at = segment.area_at(Area::Postings);
    for item in segment.dictionary().iter() {
        let (_, entry) = item.expect("a term map entry");
        let term = segment.resolve(entry).expect("a postings record");
        let p = &term.postings;
        match &p.form {
            Form::Grouped(entries) => {
                units.add(0, term.at, p.payload_at + p.containers_at);
                let containers = term.at + p.payload_at + p.containers_at;
                for e in entries.iter() {
                    units.add(2, containers + e.at as usize, e.len as usize);
                }
            }
            Form::Sparse(_) => {
                units.add(0, term.at, p.payload_at);
                units.add(1, term.at + p.payload_at, p.payload.len());
            }
            Form::Single(_) => {}
        }
        if !matches!(p.form, Form::Single(_)) && !p.compact {
            let footer = p
                .footer(segment.block_size, entry.max_tf_bucket, segment.adaptive_tf)
                .expect("a footer");
            let footer_at = term.at + p.footer_at;
            units.add(3, footer_at, p.footer.len());
            let blocks = footer.blocks();
            for b in 0..blocks {
                let end = if b + 1 < blocks {
                    footer.entry_at[b + 1] as usize
                } else {
                    p.footer.len()
                };
                units.add(
                    4,
                    footer_at + footer.entry_at[b] as usize,
                    end - footer.entry_at[b] as usize,
                );
                let tf_end = if b + 1 < blocks {
                    footer.tf_at[b + 1] as usize
                } else {
                    p.tf.len()
                };
                units.add(
                    5,
                    term.at + p.tf_at + footer.tf_at[b] as usize,
                    tf_end - footer.tf_at[b] as usize,
                );
            }
        }
        let _ = postings_at;
        let (stream, at) = segment.positions(&entry).expect("a positions stream");
        units.add(9, at, stream.len());
        let positions = Positions::parse(stream).expect("positions");
        let blocks = positions.count.div_ceil(32);
        let mut starts = Vec::with_capacity(blocks as usize + 1);
        for k in 0..blocks {
            starts.push(positions.locate(k * 32).expect("a block").1);
        }
        starts.push(stream.len());
        for w in starts.windows(2) {
            units.add(8, at + w[0], w[1] - w[0]);
        }
    }
    // The DL sidecar: per block of 256 documents its header and its bits.
    let lengths_at = segment.area_at(Area::Lengths);
    let documents = segment.documents;
    let block = 256u32;
    let blocks = documents.div_ceil(block);
    let data_end = segment.area_at(Area::Liveness);
    for b in 0..blocks {
        let (header, data) = segment.lengths.at(b * block);
        units.add(7, lengths_at + header, 8);
        let next = if b + 1 < blocks {
            lengths_at + segment.lengths.at((b + 1) * block).1
        } else {
            data_end
        };
        units.add(6, lengths_at + data, next - (lengths_at + data));
    }
}
