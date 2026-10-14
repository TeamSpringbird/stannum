// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! What a dumped `TNS1` segment's group directories cost to read, per
//! entry, over its grouped records of at least `MIN_GROUPS` groups: parsing
//! a record (its directory copied, `Postings::parse`), then decoding every
//! entry of the parsed directory (`Directory::entries`), against decoding
//! them a varint at a time (`postings::decode_directory_whole`, the
//! reference, which the decoded entries are checked against first).
//! Instructions and cycles where the OS counts them (macOS).
//!
//! ```text
//! cargo run -p bench --release --bin tnsdirskim -- MIN_GROUPS ROUNDS SEGMENT_FILE...
//! ```

use segment::tinshape::postings::{self, Form, Postings};
use segment::tinshape::segment::Segment;

/// A record's bytes, df, directory bytes, entries, and sub-range codes flag.
type Record<'a> = (&'a [u8], u32, Vec<u8>, usize, bool);

fn main() {
    let mut args = std::env::args().skip(1);
    let min_groups: usize = args.next().expect("MIN_GROUPS").parse().expect("a count");
    let rounds: usize = args.next().expect("ROUNDS").parse().expect("a count");
    for path in args {
        let blob = std::fs::read(&path).expect("segment file");
        let segment = Segment::parse(&blob).expect("a TNS1 segment");
        let geometry = &segment.docs.geometry;
        let groups = &geometry.groups;
        // (record bytes, df, directory bytes, entries)
        // Per record: its bytes, df, directory bytes, entries and whether its
        // grids carry sub-range codes.
        let mut records: Vec<Record<'_>> = Vec::new();
        for item in segment.dictionary().iter() {
            let (term, entry) = item.expect("a term");
            let found = segment.resolve(entry).expect("a record");
            let p = &found.postings;
            let Form::Grouped(dir) = &p.form else {
                continue;
            };
            if dir.len() < min_groups {
                continue;
            }
            let whole =
                postings::decode_directory_whole(dir.bytes(), groups, dir.subs()).expect("decoded");
            assert_eq!(dir.entries(groups).expect("decoded"), whole, "{term}");
            let record = &blob[found.at..found.at + p.tf_at + p.tf.len()];
            records.push((
                record,
                entry.df,
                dir.bytes().to_vec(),
                dir.len(),
                dir.subs(),
            ));
        }
        let total: usize = records.iter().map(|r| r.3).sum();
        let bytes: usize = records.iter().map(|r| r.2.len()).sum();
        println!(
            "{path}: {} records, {total} entries, {bytes} directory bytes ({:.2} an entry); every entry as the reference's",
            records.len(),
            bytes as f64 / total as f64
        );
        let per = |f: &mut dyn FnMut() -> usize| {
            let (i0, c0) = counters();
            let t = std::time::Instant::now();
            let sink = f();
            let ns = t.elapsed().as_nanos() as f64 / total as f64;
            let (i1, c1) = counters();
            std::hint::black_box(sink);
            (
                ns,
                (i1 - i0) as f64 / total as f64,
                (c1 - c0) as f64 / total as f64,
            )
        };
        if let Some(spin) = std::env::var("SPIN")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        {
            // Parse and decode every entry, `SPIN` times, for a profiler.
            let mut sink = 0usize;
            for _ in 0..spin {
                for (record, df, _, _, _) in &records {
                    let p = Postings::parse(*record, *df, geometry).expect("parsed");
                    if let Form::Grouped(dir) = &p.form {
                        sink += dir.get(dir.len() - 1, groups).expect("decoded").first as usize;
                    }
                }
            }
            println!("spun {sink}");
            continue;
        }
        for round in 0..rounds {
            let parse = per(&mut || {
                records
                    .iter()
                    .map(|(record, df, _, _, _)| {
                        Postings::parse(*record, *df, geometry)
                            .expect("parsed")
                            .containers_at
                    })
                    .sum()
            });
            let lazy = per(&mut || {
                records
                    .iter()
                    .map(|(record, df, _, _, _)| {
                        let p = Postings::parse(*record, *df, geometry).expect("parsed");
                        let Form::Grouped(dir) = &p.form else {
                            unreachable!()
                        };
                        dir.get(dir.len() - 1, groups).expect("decoded").first as usize
                    })
                    .sum()
            });
            let whole = per(&mut || {
                records
                    .iter()
                    .map(|(_, _, dir, _, subs)| {
                        postings::decode_directory_whole(dir, groups, *subs)
                            .expect("decoded")
                            .len()
                    })
                    .sum()
            });
            println!(
                "  round {round}, per entry ns / instructions / cycles: parse (copy) {:.2} / {:.1} / {:.1}, parse and decode every entry {:.2} / {:.1} / {:.1}, a varint at a time {:.2} / {:.1} / {:.1}",
                parse.0, parse.1, parse.2, lazy.0, lazy.1, lazy.2, whole.0, whole.1, whole.2
            );
        }
    }
}

/// The process's instructions retired and cycles so far (macOS).
fn counters() -> (u64, u64) {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
            fn getpid() -> i32;
        }
        let mut info = [0u64; 2 + 40];
        // SAFETY: RUSAGE_INFO_V4 (4) into a buffer larger than the struct.
        if unsafe { proc_pid_rusage(getpid(), 4, info.as_mut_ptr()) } == 0 {
            return (info[2 + 29], info[2 + 30]);
        }
    }
    (0, 0)
}
