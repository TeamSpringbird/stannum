// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Re-encodes dumped `TNS1` segments written without group frontiers (page
//! layout version 6) to the directory of version 7, each grouped record's
//! directory entries ending with their group's impact frontier, and
//! reports what that costs.
//!
//! ```text
//! cargo run -p bench --release --bin tnsreencode -- --in DUMP_DIR \
//!     [--exact OUT_DIR] [--rounded OUT_DIR] [--segment FILE] [--check-merge]
//! ```
//!
//! Only grouped records change: their form gains the frontier bits, their
//! payload length grows by the frontiers, and the term map's extents move
//! with them. Footers, containers, inline lengths, TF tails, positions,
//! the document set, the DL sidecar and the liveness area are copied byte
//! for byte, so a re-encoded dump differs from its source in nothing a
//! walk reads but the directories. Each frontier is computed from the
//! group's postings: buckets from the TF tail, lengths from the DL sidecar.
//!
//! `--check-merge` also merges each segment alone with this build's
//! `merge::merge` (a fresh encoding of every record) and requires the
//! re-encoded blob to equal it byte for byte, and checks it with
//! `verify_segment`: for small dumps (it holds several copies in memory).
//!
//! Both outputs get the source's manifest and a symlink to its `ids.tsv`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use segment::dead::DeadDocs;
use segment::dictionary::{DictionaryBuilder, Extent, TermEntry};
use segment::tinshape::merge::{Input, merge};
use segment::tinshape::postings::{
    FORM_FRONTIERS, FORM_ROUNDED, Footer, Form, GroupFrontiers, KIND_PAGED, Options,
    for_each_local, group_frontier, put_group_frontier,
};
use segment::tinshape::segment::{Area, Segment};
use segment::tinshape::verify::verify_segment;

/// The segment crate's unsigned LEB128 integers.
mod varint {
    pub fn put(out: &mut Vec<u8>, mut value: u64) {
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

    pub fn get(bytes: &[u8], at: &mut usize) -> Result<u64, &'static str> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *bytes.get(*at).ok_or("truncated varint")?;
            *at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("varint past 64 bits")
    }
}

struct Args {
    input: PathBuf,
    exact: Option<PathBuf>,
    rounded: Option<PathBuf>,
    only: Option<String>,
    check_merge: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: tnsreencode --in DIR [--exact DIR] [--rounded DIR] [--segment FILE] [--check-merge]"
    );
    std::process::exit(2)
}

fn args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        input: PathBuf::new(),
        exact: None,
        rounded: None,
        only: None,
        check_merge: false,
    };
    let value = |it: &mut std::iter::Skip<std::env::Args>| it.next().unwrap_or_else(|| usage());
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--in" => a.input = value(&mut it).into(),
            "--exact" => a.exact = Some(value(&mut it).into()),
            "--rounded" => a.rounded = Some(value(&mut it).into()),
            "--segment" => a.only = Some(value(&mut it)),
            "--check-merge" => a.check_merge = true,
            _ => usage(),
        }
    }
    if a.input.as_os_str().is_empty() {
        usage();
    }
    a
}

/// What one output spends.
#[derive(Default, Clone)]
struct Cost {
    blob: u64,
    frontier_bytes: u64,
    frontiers: u64,
    pairs: u64,
    /// Frontiers by pair count, 1 to 16.
    by_pairs: [u64; 17],
}

/// One output: its mode, the postings and term map it builds.
struct Out {
    mode: GroupFrontiers,
    postings: Vec<u8>,
    dictionary: DictionaryBuilder,
    cost: Cost,
}

impl Out {
    fn new(mode: GroupFrontiers) -> Self {
        Self {
            mode,
            postings: Vec::new(),
            dictionary: DictionaryBuilder::default(),
            cost: Cost::default(),
        }
    }
}

/// The source blob's header fields, in order.
fn header_fields(blob: &[u8]) -> Result<(Vec<u64>, usize), String> {
    if blob.get(..4) != Some(b"TNS1") {
        return Err("not a TNS1 blob".into());
    }
    let mut at = 4;
    let mut fields = Vec::with_capacity(10);
    for _ in 0..10 {
        fields.push(varint::get(blob, &mut at).map_err(|e| e.to_string())?);
    }
    Ok((fields, at))
}

/// Per group of a grouped record, the (bucket, length) of each posting,
/// in posting order.
fn group_postings(
    segment: &Segment<'_>,
    postings: &segment::tinshape::postings::Postings<'_>,
    footer: &Footer,
    out: &mut Vec<Vec<(u8, u32)>>,
) -> Result<(), String> {
    let Form::Grouped(entries) = &postings.form else {
        unreachable!("grouped only");
    };
    let geometry = &segment.docs.geometry;
    out.resize_with(entries.len(), Vec::new);
    for (e, held) in entries.iter().zip(out.iter_mut()) {
        held.clear();
        let index = e.index as usize;
        let group = &geometry.groups[index];
        let bytes = postings.container(e).all().map_err(|x| x.to_string())?;
        let mut failed = None;
        let mut i = e.first;
        for_each_local(e, bytes, group, |local| {
            if failed.is_some() {
                return;
            }
            let step = (|| -> segment::Result<(u8, u32)> {
                let rank = segment
                    .docs
                    .rank_in(index, local)
                    .ok_or(segment::Error::Corrupt("posting outside the document set"))?;
                Ok((footer.bucket(postings.tf, i)?, segment.lengths.get(rank)?))
            })();
            match step {
                Ok(p) => held.push(p),
                Err(x) => failed = Some(x),
            }
            i += 1;
        })
        .map_err(|x| x.to_string())?;
        if let Some(x) = failed {
            return Err(x.to_string());
        }
        if held.len() != e.count as usize {
            return Err(format!(
                "group {} holds {} not {}",
                e.index,
                held.len(),
                e.count
            ));
        }
    }
    Ok(())
}

/// A grouped record with its directory's frontiers, the rest copied.
fn reencode_record(
    record: &[u8],
    postings: &segment::tinshape::postings::Postings<'_>,
    groups: &[Vec<(u8, u32)>],
    mode: GroupFrontiers,
    cost: &mut Cost,
    out: &mut Vec<u8>,
) {
    let Form::Grouped(entries) = &postings.form else {
        unreachable!("grouped only");
    };
    let mut directory = Vec::with_capacity(entries.len() * 6 + 8);
    varint::put(&mut directory, entries.len() as u64);
    let mut previous: Option<u32> = None;
    for (e, held) in entries.iter().zip(groups) {
        let gap = match previous {
            None => e.index,
            Some(p) => e.index - p - 1,
        };
        previous = Some(e.index);
        varint::put(&mut directory, u64::from(gap));
        varint::put(
            &mut directory,
            (u64::from(e.count - 1) << 2) | u64::from(e.kind),
        );
        if e.kind == KIND_PAGED {
            varint::put(&mut directory, u64::from(e.len));
        }
        let pairs = group_frontier(held.iter().copied(), mode);
        let before = directory.len();
        put_group_frontier(&mut directory, &pairs);
        cost.frontier_bytes += (directory.len() - before) as u64;
        cost.frontiers += 1;
        cost.pairs += pairs.len() as u64;
        cost.by_pairs[pairs.len()] += 1;
    }
    let payload_end = postings.payload_at + postings.payload.len();
    let containers = &record[postings.payload_at + postings.containers_at..payload_end];
    let form = record[0]
        | match mode {
            GroupFrontiers::Rounded => FORM_FRONTIERS | FORM_ROUNDED,
            _ => FORM_FRONTIERS,
        };
    out.push(form);
    varint::put(out, (postings.payload_at - postings.footer_at) as u64);
    varint::put(out, (directory.len() + containers.len()) as u64);
    if let Some(lengths) = &postings.lengths {
        varint::put(out, (postings.tf_at - lengths.at) as u64);
    }
    out.extend_from_slice(&record[postings.footer_at..postings.payload_at]);
    out.extend_from_slice(&directory);
    out.extend_from_slice(containers);
    out.extend_from_slice(&record[payload_end..]);
}

/// Re-encodes one blob into each output's mode; the blobs, in `outs`
/// order.
fn reencode(blob: &[u8], outs: &mut [Out]) -> Result<Vec<Vec<u8>>, String> {
    let segment = Segment::parse(blob).map_err(|e| e.to_string())?;
    let (fields, _) = header_fields(blob)?;
    let postings_at = segment.area_at(Area::Postings);
    let mut groups: Vec<Vec<(u8, u32)>> = Vec::new();
    for out in outs.iter_mut() {
        out.postings.clear();
        out.postings
            .reserve(fields[5] as usize + fields[5] as usize / 20);
        out.dictionary = DictionaryBuilder::default();
    }
    for item in segment.dictionary().iter() {
        let (term, entry) = item.map_err(|e| e.to_string())?;
        let found = segment.resolve(entry).map_err(|e| format!("{term}: {e}"))?;
        let from = postings_at + entry.ordinals.offset as usize;
        let record = &blob[from..from + entry.ordinals.len as usize];
        let grouped = matches!(found.postings.form, Form::Grouped(_));
        if grouped && found.postings.frontiers.is_some() {
            return Err(format!("{term}: the source already holds frontiers"));
        }
        if grouped {
            let footer = found
                .postings
                .footer(segment.block_size, entry.max_tf_bucket, segment.adaptive_tf)
                .map_err(|e| e.to_string())?;
            group_postings(&segment, &found.postings, &footer, &mut groups)
                .map_err(|e| format!("{term}: {e}"))?;
        }
        for out in outs.iter_mut() {
            let at = out.postings.len();
            if grouped {
                reencode_record(
                    record,
                    &found.postings,
                    &groups,
                    out.mode,
                    &mut out.cost,
                    &mut out.postings,
                );
            } else {
                out.postings.extend_from_slice(record);
            }
            let moved = TermEntry {
                ordinals: Extent {
                    offset: at as u64,
                    len: (out.postings.len() - at) as u32,
                },
                ..entry
            };
            out.dictionary
                .push(&term, moved)
                .map_err(|e| e.to_string())?;
        }
    }
    let rest = &blob[segment.area_at(Area::Positions)..segment.bounds[7]];
    let mut blobs = Vec::with_capacity(outs.len());
    for out in outs.iter_mut() {
        let dictionary = std::mem::take(&mut out.dictionary).finish();
        let mut fields = fields.clone();
        fields[4] = dictionary.len() as u64;
        fields[5] = out.postings.len() as u64;
        let mut new = Vec::with_capacity(64 + dictionary.len() + out.postings.len() + rest.len());
        new.extend_from_slice(b"TNS1");
        for f in &fields {
            varint::put(&mut new, *f);
        }
        new.extend_from_slice(&dictionary);
        new.extend_from_slice(&out.postings);
        new.extend_from_slice(rest);
        out.cost.blob += new.len() as u64;
        blobs.push(new);
    }
    if segment.bounds[7] != blob.len() {
        return Err("bytes past the liveness area".into());
    }
    Ok(blobs)
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
    let manifest = std::fs::read_to_string(args.input.join("manifest.tsv")).unwrap_or_else(|e| {
        eprintln!("manifest: {e}");
        std::process::exit(1)
    });
    let mut outs = Vec::new();
    let mut dirs = Vec::new();
    for (dir, mode) in [
        (&args.exact, GroupFrontiers::Exact),
        (&args.rounded, GroupFrontiers::Rounded),
    ] {
        if let Some(dir) = dir {
            copy_manifest(&args.input, dir).unwrap_or_else(|e| {
                eprintln!("{}: {e}", dir.display());
                std::process::exit(1)
            });
            outs.push(Out::new(mode));
            dirs.push(dir.clone());
        } else if args.check_merge {
            outs.push(Out::new(mode));
            dirs.push(PathBuf::new());
        }
    }
    let mut source = 0u64;
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
            for dir in dirs.iter().filter(|d| !d.as_os_str().is_empty()) {
                let _ = std::fs::copy(args.input.join(dead), dir.join(dead));
            }
        }
        let start = Instant::now();
        let blob = std::fs::read(args.input.join(name)).unwrap_or_else(|e| {
            eprintln!("{name}: {e}");
            std::process::exit(1)
        });
        source += blob.len() as u64;
        let blobs = match reencode(&blob, &mut outs) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{name}: {e}");
                std::process::exit(1)
            }
        };
        for ((new, out), dir) in blobs.iter().zip(&outs).zip(&dirs) {
            let mode = format!("{:?}", out.mode).to_lowercase();
            println!(
                "{name} {mode}: {} -> {} bytes (+{:.3}%)",
                blob.len(),
                new.len(),
                100.0 * (new.len() as f64 / blob.len() as f64 - 1.0)
            );
            if args.check_merge {
                let documents = header_fields(&blob).map(|(f, _)| f[0] as u32).unwrap_or(0);
                let none = DeadDocs::decode(&segment::ordinals::encode(&[]), documents).ok();
                let merged = none.as_ref().and_then(|dead| {
                    merge(
                        &[Input { bytes: &blob, dead }],
                        Options {
                            group_frontiers: out.mode,
                            ..Options::default()
                        },
                        || Ok(()),
                    )
                    .ok()
                    .flatten()
                });
                let same = merged.as_ref().is_some_and(|m| m.blob == *new);
                let clean = verify_segment(new).is_clean();
                println!("{name} {mode}: equal to a merge's {same}, verifies {clean}");
                ok &= same && clean;
            }
            if !dir.as_os_str().is_empty() {
                std::fs::write(dir.join(name), new).unwrap_or_else(|e| {
                    eprintln!("{name}: {e}");
                    std::process::exit(1)
                });
            }
        }
        eprintln!("{name}: {:.1} s", start.elapsed().as_secs_f64());
    }
    println!("source {source} bytes");
    for out in &outs {
        let c = &out.cost;
        println!(
            "{:?}: blobs {} bytes (+{} = +{:.3}%), frontiers {} in {} bytes ({:.2} B each, {:.2} pairs), by pairs {:?}",
            out.mode,
            c.blob,
            c.blob as i64 - source as i64,
            100.0 * (c.blob as f64 / source as f64 - 1.0),
            c.frontiers,
            c.frontier_bytes,
            c.frontier_bytes as f64 / c.frontiers.max(1) as f64,
            c.pairs as f64 / c.frontiers.max(1) as f64,
            &c.by_pairs[1..],
        );
    }
    if !ok {
        std::process::exit(1);
    }
}
