// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Re-encodes a dump's `TNS1` postings records under other container
//! choices (`Options::grid_density`, `Options::grid_min_postings`), so a
//! replay can measure what forcing grids costs and saves.
//!
//! ```text
//! cargo run -p bench --release --bin tnsregrid -- --in DUMP_DIR --out OUT_DIR \
//!     [--grid-density D] [--grid-min-postings N] [--segment FILE] [--check]
//! ```
//!
//! Each record of at least `--min-df` postings (default 4,096, the smallest
//! a grid is forced for) is encoded afresh by `postings::encode` from its
//! slots, its buckets (the TF tail) and its documents' lengths (the DL
//! sidecar); smaller records, positions, the document set, the DL sidecar
//! and the liveness area are copied byte for byte and the term map's
//! extents move with the records. A merge would not do: it copies an
//! input's containers where their group is not dense, grids included.
//! `--check` re-encodes with the default options and requires every record
//! to come out byte for byte as it was.

use std::path::{Path, PathBuf};
use std::time::Instant;

use segment::dictionary::{DictionaryBuilder, Extent, TermEntry};
use segment::tinshape::postings::{Form, Options, encode};
use segment::tinshape::segment::{Area, Segment};
use segment::tinshape::verify::verify_segment;

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn get_varint(bytes: &[u8], at: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at).ok_or("truncated varint")?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("varint past 64 bits".into())
}

struct Args {
    input: PathBuf,
    output: Option<PathBuf>,
    only: Option<String>,
    grid_density: u32,
    grid_min_postings: u32,
    paged_density: u32,
    min_df: u32,
    check: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: tnsregrid --in DIR [--out DIR] [--grid-density D] [--grid-min-postings N] \
         [--paged-density D] [--min-df N] [--segment FILE] [--check]"
    );
    std::process::exit(2)
}

fn args() -> Args {
    let defaults = Options::default();
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        input: PathBuf::new(),
        output: None,
        only: None,
        grid_density: defaults.grid_density,
        grid_min_postings: defaults.grid_min_postings,
        paged_density: defaults.paged_density,
        min_df: defaults.grid_min_postings,
        check: false,
    };
    let value = |it: &mut std::iter::Skip<std::env::Args>| it.next().unwrap_or_else(|| usage());
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--in" => a.input = value(&mut it).into(),
            "--out" => a.output = Some(value(&mut it).into()),
            "--segment" => a.only = Some(value(&mut it)),
            "--grid-density" => a.grid_density = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--grid-min-postings" => {
                a.grid_min_postings = value(&mut it).parse().unwrap_or_else(|_| usage());
            }
            "--paged-density" => {
                a.paged_density = value(&mut it).parse().unwrap_or_else(|_| usage());
            }
            "--min-df" => a.min_df = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--check" => a.check = true,
            _ => usage(),
        }
    }
    if a.input.as_os_str().is_empty() {
        usage();
    }
    a
}

/// The blob's ten header fields after the magic.
fn header_fields(blob: &[u8]) -> Result<Vec<u64>, String> {
    if blob.get(..4) != Some(b"TNS1") {
        return Err("not a TNS1 blob".into());
    }
    let mut at = 4;
    (0..10).map(|_| get_varint(blob, &mut at)).collect()
}

#[derive(Default)]
struct Count {
    records: u64,
    changed: u64,
    before: u64,
    after: u64,
}

fn regrid(
    blob: &[u8],
    options: &Options,
    min_df: u32,
    count: &mut Count,
) -> Result<Vec<u8>, String> {
    let segment = Segment::parse(blob).map_err(|e| e.to_string())?;
    let mut fields = header_fields(blob)?;
    let geometry = &segment.docs.geometry;
    let postings_at = segment.area_at(Area::Postings);
    let options = Options {
        block_size: segment.block_size,
        adaptive_tf: segment.adaptive_tf,
        ..*options
    };
    let mut postings = Vec::with_capacity(fields[5] as usize);
    let mut dictionary = DictionaryBuilder::default();
    let (mut slots, mut buckets, mut lengths) = (Vec::new(), Vec::new(), Vec::new());
    let mut record_out = Vec::new();
    for item in segment.dictionary().iter() {
        let (term, entry) = item.map_err(|e| e.to_string())?;
        let from = postings_at + entry.ordinals.offset as usize;
        let record = &blob[from..from + entry.ordinals.len as usize];
        let at = postings.len();
        if entry.df >= min_df {
            let found = segment.resolve(entry).map_err(|e| format!("{term}: {e}"))?;
            let p = &found.postings;
            slots.clear();
            p.for_each_slot(geometry, |s| slots.push(s))
                .map_err(|e| format!("{term}: {e}"))?;
            buckets.clear();
            lengths.clear();
            let footer = match &p.form {
                Form::Single(_) => None,
                _ => Some(
                    p.footer(segment.block_size, entry.max_tf_bucket, segment.adaptive_tf)
                        .map_err(|e| format!("{term}: {e}"))?,
                ),
            };
            for (i, slot) in slots.iter().enumerate() {
                buckets.push(match &footer {
                    Some(f) => f
                        .bucket(p.tf, i as u32)
                        .map_err(|e| format!("{term}: {e}"))?,
                    None => entry.max_tf_bucket,
                });
                let rank = segment
                    .docs
                    .rank(*slot)
                    .ok_or("a posting outside the docs")?;
                lengths.push(segment.lengths.get(rank).map_err(|e| e.to_string())?);
            }
            record_out.clear();
            encode(
                geometry,
                &slots,
                &buckets,
                &lengths,
                &options,
                &mut record_out,
            );
            count.records += 1;
            count.before += record.len() as u64;
            count.after += record_out.len() as u64;
            count.changed += u64::from(record_out != record);
            postings.extend_from_slice(&record_out);
        } else {
            postings.extend_from_slice(record);
        }
        let moved = TermEntry {
            ordinals: Extent {
                offset: at as u64,
                len: (postings.len() - at) as u32,
            },
            ..entry
        };
        dictionary.push(&term, moved).map_err(|e| e.to_string())?;
    }
    if segment.bounds[7] != blob.len() {
        return Err("bytes past the liveness area".into());
    }
    let dictionary = dictionary.finish();
    let rest = &blob[segment.area_at(Area::Positions)..segment.bounds[7]];
    fields[4] = dictionary.len() as u64;
    fields[5] = postings.len() as u64;
    let mut new = Vec::with_capacity(64 + dictionary.len() + postings.len() + rest.len());
    new.extend_from_slice(b"TNS1");
    for f in &fields {
        put_varint(&mut new, *f);
    }
    new.extend_from_slice(&dictionary);
    new.extend_from_slice(&postings);
    new.extend_from_slice(rest);
    Ok(new)
}

fn copy_manifest(input: &Path, output: &Path) -> Result<(), String> {
    std::fs::create_dir_all(output).map_err(|e| e.to_string())?;
    std::fs::copy(input.join("manifest.tsv"), output.join("manifest.tsv"))
        .map_err(|e| e.to_string())?;
    let ids = input.join("ids.tsv");
    let link = output.join("ids.tsv");
    if ids.exists() && !link.exists() {
        std::os::unix::fs::symlink(&ids, &link).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn main() {
    let args = args();
    let options = if args.check {
        Options::default()
    } else {
        Options {
            grid_density: args.grid_density,
            grid_min_postings: args.grid_min_postings,
            paged_density: args.paged_density,
            ..Options::default()
        }
    };
    let manifest = std::fs::read_to_string(args.input.join("manifest.tsv")).unwrap_or_else(|e| {
        eprintln!("manifest: {e}");
        std::process::exit(1)
    });
    if let Some(out) = &args.output {
        copy_manifest(&args.input, out).unwrap_or_else(|e| {
            eprintln!("{}: {e}", out.display());
            std::process::exit(1)
        });
    }
    let (mut source, mut written) = (0u64, 0u64);
    let mut total = Count::default();
    let mut ok = true;
    for line in manifest.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let ["segment", _, _, name, dead] = f.as_slice() else {
            continue;
        };
        if args.only.as_deref().is_some_and(|o| o != *name) {
            continue;
        }
        if *dead != "-" {
            eprintln!("{name}: has a dead list, which is copied as is");
            if let Some(out) = &args.output {
                let _ = std::fs::copy(args.input.join(dead), out.join(dead));
            }
        }
        let start = Instant::now();
        let blob = std::fs::read(args.input.join(name)).unwrap_or_else(|e| {
            eprintln!("{name}: {e}");
            std::process::exit(1)
        });
        let mut count = Count::default();
        let new = regrid(&blob, &options, args.min_df, &mut count).unwrap_or_else(|e| {
            eprintln!("{name}: {e}");
            std::process::exit(1)
        });
        source += blob.len() as u64;
        written += new.len() as u64;
        println!(
            "{name}: {} -> {} bytes ({:+.2}%), {} records re-encoded ({} -> {} bytes), {} changed, {:.1} s",
            blob.len(),
            new.len(),
            100.0 * (new.len() as f64 / blob.len() as f64 - 1.0),
            count.records,
            count.before,
            count.after,
            count.changed,
            start.elapsed().as_secs_f64()
        );
        if args.check {
            let same = new == blob;
            println!("{name}: identical to the source {same}");
            ok &= same && count.changed == 0;
        } else {
            let clean = verify_segment(&new).is_clean();
            println!("{name}: verifies {clean}");
            ok &= clean;
        }
        if let Some(out) = &args.output {
            std::fs::write(out.join(name), &new).unwrap_or_else(|e| {
                eprintln!("{name}: {e}");
                std::process::exit(1)
            });
        }
        total.records += count.records;
        total.changed += count.changed;
        total.before += count.before;
        total.after += count.after;
    }
    println!(
        "total: {source} -> {written} bytes ({:+.2}%), {} records re-encoded ({} -> {} bytes), {} changed",
        100.0 * (written as f64 / source.max(1) as f64 - 1.0),
        total.records,
        total.before,
        total.after,
        total.changed
    );
    if !ok {
        std::process::exit(1);
    }
}
