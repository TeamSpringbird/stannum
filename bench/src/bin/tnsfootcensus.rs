// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Census of a dumped `TNS1` segment's records by form and df band: bytes of
//! headers, footers, directories, containers, inline lengths and TF tails,
//! and how many groups and footer blocks the terms hold, to model a footer
//! per (term, group).
//!
//! ```text
//! cargo run -p bench --release --bin tnsfootcensus -- SEGMENT_FILE...
//! ```

use segment::tinshape::postings::Form;
use segment::tinshape::segment::Segment;

#[derive(Default, Clone, Copy)]
struct Band {
    terms: u64,
    postings: u64,
    groups: u64,
    blocks: u64,
    header: u64,
    footer: u64,
    directory: u64,
    containers: u64,
    lengths: u64,
    tf: u64,
    /// Groups holding one posting, two to three, four to fifteen, more.
    group_counts: [u64; 4],
    /// Sparse-form (and single) terms and their postings.
    sparse_terms: u64,
    sparse_postings: u64,
}

fn band_of(df: u32) -> usize {
    match df {
        1 => 0,
        2..=8 => 1,
        9..=64 => 2,
        65..=256 => 3,
        257..=1024 => 4,
        1025..=4096 => 5,
        4097..=16384 => 6,
        16385..=65536 => 7,
        65537..=262144 => 8,
        262145..=1048576 => 9,
        _ => 10,
    }
}

const BANDS: [&str; 11] = [
    "1", "2-8", "9-64", "65-256", "257-1K", "1K-4K", "4K-16K", "16K-64K", "64K-256K", "256K-1M",
    ">1M",
];

fn main() {
    let mut bands = [Band::default(); 11];
    let mut documents = 0u64;
    let mut groups_total = 0u64;
    for path in std::env::args().skip(1) {
        let blob = std::fs::read(&path).expect("segment file");
        let segment = Segment::parse(&blob).expect("a TNS1 segment");
        documents += u64::from(segment.documents);
        groups_total += segment.docs.geometry.groups.len() as u64;
        for item in segment.dictionary().iter() {
            let (term, entry) = item.expect("a term");
            let found = segment
                .resolve(entry)
                .unwrap_or_else(|e| panic!("{term}: {e}"));
            let p = &found.postings;
            let b = &mut bands[band_of(entry.df)];
            b.terms += 1;
            b.postings += u64::from(entry.df);
            b.header += p.footer_at as u64;
            b.footer += p.footer.len() as u64;
            b.tf += p.tf.len() as u64;
            b.blocks += u64::from(entry.df.div_ceil(segment.block_size));
            if let Some(l) = &p.lengths {
                b.lengths += (p.tf_at - l.at) as u64;
            }
            match &p.form {
                Form::Grouped(dir) => {
                    let entries = dir
                        .entries(&segment.docs.geometry.groups)
                        .expect("a directory");
                    b.groups += entries.len() as u64;
                    b.directory += p.containers_at as u64;
                    b.containers += (p.payload.len() - p.containers_at) as u64;
                    for e in entries.iter() {
                        let k = match e.count {
                            1 => 0,
                            2..=3 => 1,
                            4..=15 => 2,
                            _ => 3,
                        };
                        b.group_counts[k] += 1;
                    }
                }
                Form::Sparse(_) => {
                    b.sparse_terms += 1;
                    b.sparse_postings += u64::from(entry.df);
                    b.containers += p.payload.len() as u64;
                }
                Form::Single(_) => {
                    b.sparse_terms += 1;
                    b.sparse_postings += 1;
                    b.containers += p.payload.len() as u64;
                }
            }
        }
        eprintln!("{path}: done");
    }
    println!("documents {documents} groups {groups_total}");
    println!(
        "{:>10} {:>9} {:>12} {:>11} {:>11} {:>10} {:>11} {:>11} {:>12} {:>10} {:>11} {:>8} {:>12} | groups by count 1 / 2-3 / 4-15 / 16+",
        "df",
        "terms",
        "postings",
        "groups",
        "blocks",
        "header",
        "footer",
        "directory",
        "containers",
        "lengths",
        "tf",
        "sparseT",
        "sparseP"
    );
    let mut total = Band::default();
    for (i, b) in bands.iter().enumerate() {
        println!(
            "{:>10} {:>9} {:>12} {:>11} {:>11} {:>10} {:>11} {:>11} {:>12} {:>10} {:>11} {:>8} {:>12} | {} / {} / {} / {}",
            BANDS[i],
            b.terms,
            b.postings,
            b.groups,
            b.blocks,
            b.header,
            b.footer,
            b.directory,
            b.containers,
            b.lengths,
            b.tf,
            b.sparse_terms,
            b.sparse_postings,
            b.group_counts[0],
            b.group_counts[1],
            b.group_counts[2],
            b.group_counts[3]
        );
        total.terms += b.terms;
        total.postings += b.postings;
        total.groups += b.groups;
        total.blocks += b.blocks;
        total.header += b.header;
        total.footer += b.footer;
        total.directory += b.directory;
        total.containers += b.containers;
        total.lengths += b.lengths;
        total.tf += b.tf;
        total.sparse_terms += b.sparse_terms;
        total.sparse_postings += b.sparse_postings;
        for k in 0..4 {
            total.group_counts[k] += b.group_counts[k];
        }
    }
    let b = total;
    println!(
        "{:>10} {:>9} {:>12} {:>11} {:>11} {:>10} {:>11} {:>11} {:>12} {:>10} {:>11} {:>8} {:>12} | {} / {} / {} / {}",
        "total",
        b.terms,
        b.postings,
        b.groups,
        b.blocks,
        b.header,
        b.footer,
        b.directory,
        b.containers,
        b.lengths,
        b.tf,
        b.sparse_terms,
        b.sparse_postings,
        b.group_counts[0],
        b.group_counts[1],
        b.group_counts[2],
        b.group_counts[3]
    );
}
