// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Where the bytes of a segment go: per section, and per term grouped by
//! document frequency, split into dictionary entry, postings header, bounds
//! table, postings body, payload header, payload skip table and payload data.
//!
//! ```text
//! script/dump-segments.py --dbname db --index documents_body_idx --out /tmp/blobs
//! cargo run -p segment --release --example breakdown -- [--reencode] /tmp/blobs/*.segment
//! ```
//!
//! With `--reencode`, every blob is also rebuilt through the current writer
//! and the rebuilt blob is broken down the same way, so an old blob and the
//! current format can be compared on the same documents.

use std::collections::BTreeMap;

use segment::segment::{Sections, Segment, SegmentBuilder};

#[derive(Clone, Copy, Default)]
struct Bytes {
    terms: usize,
    dictionary: usize,
    postings_header: usize,
    bounds: usize,
    postings_body: usize,
    payload_header: usize,
    skips: usize,
    payload_data: usize,
    grouped: usize,
}

impl Bytes {
    fn total(&self) -> usize {
        self.dictionary
            + self.postings_header
            + self.bounds
            + self.postings_body
            + self.payload_header
            + self.skips
            + self.payload_data
    }

    fn add(&mut self, other: &Bytes) {
        self.terms += other.terms;
        self.dictionary += other.dictionary;
        self.postings_header += other.postings_header;
        self.bounds += other.bounds;
        self.postings_body += other.postings_body;
        self.payload_header += other.payload_header;
        self.skips += other.skips;
        self.payload_data += other.payload_data;
        self.grouped += other.grouped;
    }
}

#[derive(Default)]
struct Breakdown {
    blobs: usize,
    bytes: usize,
    sections: Sections,
    by_df: BTreeMap<&'static str, Bytes>,
}

const BUCKETS: [&str; 3] = ["df=1", "df=2-127", "df=128+"];

fn bucket(df: u32) -> &'static str {
    match df {
        1 => BUCKETS[0],
        2..=127 => BUCKETS[1],
        _ => BUCKETS[2],
    }
}

impl Breakdown {
    fn add_blob(&mut self, bytes: &[u8]) -> segment::Result<()> {
        let segment = Segment::parse(bytes)?;
        let sections = segment.sections();
        self.blobs += 1;
        self.bytes += bytes.len();
        self.sections.header += sections.header;
        self.sections.dictionary += sections.dictionary;
        self.sections.postings += sections.postings;
        self.sections.payload += sections.payload;
        self.sections.docs += sections.docs;
        self.sections.lengths += sections.lengths;
        self.sections.ordinals += sections.ordinals;
        self.sections.pages += sections.pages;
        let dictionary = segment.dictionary()?;
        for block in 0..dictionary.index().blocks() {
            for (_, entry, entry_len) in dictionary.block_sizes(block)? {
                let term = segment.resolve(entry)?;
                let postings = term.postings()?;
                let payload = term.payload()?;
                let mut bytes = Bytes {
                    terms: 1,
                    dictionary: entry_len,
                    bounds: postings.bounds_len(),
                    postings_body: postings.body_len(),
                    skips: payload.skip_table_len(),
                    payload_data: payload.data_len(),
                    grouped: usize::from(postings.is_grouped()),
                    ..Bytes::default()
                };
                bytes.postings_header =
                    entry.postings.len as usize - bytes.bounds - bytes.postings_body;
                bytes.payload_header =
                    entry.payload.len as usize - bytes.skips - bytes.payload_data;
                self.by_df.entry(bucket(entry.df)).or_default().add(&bytes);
            }
        }
        Ok(())
    }

    fn print(&self, label: &str) {
        println!("{label}: {} blob(s), {} bytes", self.blobs, self.bytes);
        let share = |n: usize| 100.0 * n as f64 / self.bytes.max(1) as f64;
        println!("  {:<12} {:>12} {:>7}", "section", "bytes", "share");
        for (name, n) in [
            ("header", self.sections.header),
            ("dictionary", self.sections.dictionary),
            ("postings", self.sections.postings),
            ("payload", self.sections.payload),
            ("docs", self.sections.docs),
            ("lengths", self.sections.lengths),
            ("ordinals", self.sections.ordinals),
            ("pages", self.sections.pages),
        ] {
            println!("  {name:<12} {n:>12} {:>6.1}%", share(n));
        }
        println!(
            "  {:<9} {:>8} {:>8} {:>11} {:>9} {:>9} {:>10} {:>8} {:>8} {:>10} {:>11}",
            "terms",
            "count",
            "grouped",
            "dictionary",
            "post_hdr",
            "bounds",
            "post_body",
            "pay_hdr",
            "skips",
            "pay_data",
            "total"
        );
        let mut all = Bytes::default();
        for name in BUCKETS {
            let b = self.by_df.get(name).copied().unwrap_or_default();
            all.add(&b);
            print_row(name, &b);
        }
        print_row("all", &all);
    }
}

fn print_row(name: &str, b: &Bytes) {
    println!(
        "  {:<9} {:>8} {:>8} {:>11} {:>9} {:>9} {:>10} {:>8} {:>8} {:>10} {:>11}",
        name,
        b.terms,
        b.grouped,
        b.dictionary,
        b.postings_header,
        b.bounds,
        b.postings_body,
        b.payload_header,
        b.skips,
        b.payload_data,
        b.total()
    );
}

fn reencode(bytes: &[u8]) -> segment::Result<Vec<u8>> {
    let segment = Segment::parse(bytes)?;
    let mut builder = SegmentBuilder::default();
    for record in segment.records(|_| false)? {
        builder.add_record(&record)?;
    }
    Ok(builder.finish())
}

fn main() {
    let mut paths = Vec::new();
    let mut with_reencode = false;
    for argument in std::env::args().skip(1) {
        if argument == "--reencode" {
            with_reencode = true;
        } else {
            paths.push(argument);
        }
    }
    if paths.is_empty() {
        eprintln!("usage: breakdown [--reencode] BLOB...");
        std::process::exit(2);
    }
    let mut original = Breakdown::default();
    let mut rebuilt = Breakdown::default();
    for path in &paths {
        let bytes = std::fs::read(path).unwrap_or_else(|error| {
            eprintln!("{path}: {error}");
            std::process::exit(1);
        });
        let magic = String::from_utf8_lossy(&bytes[..bytes.len().min(4)]).into_owned();
        if let Err(error) = original.add_blob(&bytes) {
            eprintln!("{path}: {error}");
            std::process::exit(1);
        }
        let mut line = format!("{path}: {magic}, {} bytes", bytes.len());
        if with_reencode {
            let again = reencode(&bytes).unwrap_or_else(|error| {
                eprintln!("{path}: re-encoding: {error}");
                std::process::exit(1);
            });
            line.push_str(&format!(" -> re-encoded {} bytes", again.len()));
            rebuilt
                .add_blob(&again)
                .expect("the current writer's output parses");
        }
        println!("{line}");
    }
    original.print("as stored");
    if with_reencode {
        rebuilt.print("re-encoded with the current writer");
    }
}
