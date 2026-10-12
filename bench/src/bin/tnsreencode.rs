// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Re-encodes dumped `TNS1` segments whose grouped records keep a footer
//! of blocks (page layout version 6) to grouped records whose directory
//! entries carry their group's frontier and whose TF tail is packed per
//! group (page layout version 8), and reports what that costs.
//!
//! ```text
//! cargo run -p bench --release --bin tnsreencode -- --in DUMP_DIR --out OUT_DIR \
//!     [--exact] [--segment FILE] [--check-merge]
//! ```
//!
//! Only grouped records change: their footer goes, their directory gains
//! a frontier per entry, their TF tail is re-packed per group, and the
//! term map's extents move with them. Containers, inline lengths, sparse
//! and single records, positions, the document set, the DL sidecar and
//! the liveness area are copied byte for byte. Each frontier is computed
//! from the group's postings: buckets from the old TF tail (through the
//! old footer), lengths from the DL sidecar.
//!
//! `--check-merge` also merges each re-encoded segment alone with this build's
//! `merge::merge` (a fresh encoding of every record) and requires the
//! re-encoded blob to equal it byte for byte, and checks it with
//! `verify_segment`: for small dumps (it holds several copies in memory).
//!
//! The output gets the source's manifest and a symlink to its `ids.tsv`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use segment::dead::DeadDocs;
use segment::dictionary::{DictionaryBuilder, Extent, TermEntry};
use segment::tinshape::bits::BitWriter;
use segment::tinshape::ef;
use segment::tinshape::merge::{Input, merge};
use segment::tinshape::postings::{
    FORM_COMPACT, FORM_GROUPED, FORM_LENGTHS, FORM_ROUNDED, GroupEntry, GroupFrontiers, KIND_EF,
    KIND_GRID, KIND_PAGED, Options, for_each_local, group_frontier, put_group_frontier, tf_width,
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

    pub fn get(bytes: &[u8], at: &mut usize) -> Result<u64, String> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *bytes
                .get(*at)
                .ok_or_else(|| "truncated varint".to_owned())?;
            *at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("varint past 64 bits".to_owned())
    }
}

struct Args {
    input: PathBuf,
    output: PathBuf,
    mode: GroupFrontiers,
    only: Option<String>,
    check_merge: bool,
}

fn usage() -> ! {
    eprintln!("usage: tnsreencode --in DIR --out DIR [--exact] [--segment FILE] [--check-merge]");
    std::process::exit(2)
}

fn args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        input: PathBuf::new(),
        output: PathBuf::new(),
        mode: GroupFrontiers::Rounded,
        only: None,
        check_merge: false,
    };
    let value = |it: &mut std::iter::Skip<std::env::Args>| it.next().unwrap_or_else(|| usage());
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--in" => a.input = value(&mut it).into(),
            "--out" => a.output = value(&mut it).into(),
            "--exact" => a.mode = GroupFrontiers::Exact,
            "--segment" => a.only = Some(value(&mut it)),
            "--check-merge" => a.check_merge = true,
            _ => usage(),
        }
    }
    if a.input.as_os_str().is_empty() || a.output.as_os_str().is_empty() {
        usage();
    }
    a
}

/// What the output spends and saves.
#[derive(Default, Clone)]
struct Cost {
    blob: u64,
    grouped_terms: u64,
    footer_removed: u64,
    directory_before: u64,
    directory_after: u64,
    frontier_bytes: u64,
    frontiers: u64,
    pairs: u64,
    tf_before: u64,
    tf_after: u64,
    /// Frontiers by pair count, 1 to 16.
    by_pairs: [u64; 17],
}

/// The source blob's header fields, in order.
fn header_fields(blob: &[u8]) -> Result<(Vec<u64>, usize), String> {
    if blob.get(..4) != Some(b"TNS1") {
        return Err("not a TNS1 blob".into());
    }
    let mut at = 4;
    let mut fields = Vec::with_capacity(10);
    for _ in 0..10 {
        fields.push(varint::get(blob, &mut at)?);
    }
    Ok((fields, at))
}

/// A grouped record of page layout version 6, parsed: its footer blocks'
/// TF offsets and widths, its directory, and where its parts lie.
///
/// ```text
/// full    := form u8 (2 grouped), footer_len varint, payload_len varint,
///            [lengths_len varint], footer, payload, [lengths], tf
/// footer  := (last_gap varint, frontier_len u8, (bucket u8, length varint)*)*
/// payload := groups varint, (index_gap varint, (count - 1) << 2 | kind varint,
///            [len varint, paged])*, container*
/// tf      := per block of `block_size` postings, its buckets at the block's
///            width, each block starting on a byte
/// ```
struct OldRecord<'r> {
    entries: Vec<GroupEntry>,
    /// Per footer block: where its buckets start in `tf` (bytes) and their
    /// width.
    blocks: Vec<(usize, u32)>,
    containers: &'r [u8],
    lengths: &'r [u8],
    tf: &'r [u8],
    footer_bytes: usize,
    directory_bytes: usize,
}

fn parse_old<'r>(
    record: &'r [u8],
    df: u32,
    block_size: u32,
    adaptive: bool,
    geometry: &segment::tinshape::docs::Geometry,
) -> Result<Option<OldRecord<'r>>, String> {
    if df == 1 {
        return Ok(None);
    }
    let form = record[0];
    if form & 3 != FORM_GROUPED {
        return Ok(None);
    }
    if form & FORM_COMPACT != 0 {
        return Err("a compact grouped record".into());
    }
    let mut at = 1;
    let footer_len = varint::get(record, &mut at)? as usize;
    let payload_len = varint::get(record, &mut at)? as usize;
    let lengths_len = if form & FORM_LENGTHS != 0 {
        varint::get(record, &mut at)? as usize
    } else {
        0
    };
    let footer_at = at;
    let payload_at = footer_at + footer_len;
    let lengths_at = payload_at + payload_len;
    let tf_at = lengths_at + lengths_len;
    let footer = &record[footer_at..payload_at];
    let payload = &record[payload_at..lengths_at];
    let tf = &record[tf_at..];
    // The footer: a block's last slot is not needed, its width is.
    let blocks_n = df.div_ceil(block_size) as usize;
    let mut blocks = Vec::with_capacity(blocks_n);
    let mut f = 0usize;
    let mut tf_byte = 0usize;
    for b in 0..blocks_n {
        let _gap = varint::get(footer, &mut f)?;
        let k = footer[f] as usize;
        f += 1;
        let mut max = 0u8;
        for _ in 0..k {
            max = max.max(footer[f]);
            f += 1;
            varint::get(footer, &mut f)?;
        }
        let width = tf_width(max, adaptive);
        let n = if b + 1 == blocks_n {
            df - b as u32 * block_size
        } else {
            block_size
        };
        blocks.push((tf_byte, width));
        tf_byte += (n as usize * width as usize).div_ceil(8);
    }
    if f != footer.len() || tf_byte != tf.len() {
        return Err("old footer length".into());
    }
    // The directory.
    let mut d = 0usize;
    let groups = varint::get(payload, &mut d)? as usize;
    let mut entries = Vec::with_capacity(groups);
    let mut index: Option<u64> = None;
    let mut body = 0u64;
    let mut first = 0u64;
    for _ in 0..groups {
        let gap = varint::get(payload, &mut d)?;
        let next = match index {
            None => gap,
            Some(i) => i + 1 + gap,
        };
        index = Some(next);
        let group = geometry.groups[next as usize];
        let packed = varint::get(payload, &mut d)?;
        let kind = (packed & 3) as u8;
        let count = (packed >> 2) + 1;
        let len = match kind {
            KIND_GRID => group.grid_bytes() as u64,
            KIND_EF => ef::encoded_len(count as usize, group.slots()) as u64,
            KIND_PAGED => varint::get(payload, &mut d)?,
            _ => return Err("old group kind".into()),
        };
        entries.push(GroupEntry {
            index: next as u32,
            count: count as u32,
            kind,
            at: body as u32,
            len: len as u32,
            first: first as u32,
            frontier: 0,
            tf_bit: 0,
            width: 0,
            max_bucket: 0,
        });
        body += len;
        first += count;
    }
    if first != u64::from(df) || d as u64 + body != payload.len() as u64 {
        return Err("old directory length".into());
    }
    Ok(Some(OldRecord {
        entries,
        blocks,
        containers: &payload[d..],
        lengths: &record[lengths_at..tf_at],
        tf,
        footer_bytes: footer_len,
        directory_bytes: d,
    }))
}

impl OldRecord<'_> {
    /// Posting `index`'s bucket.
    fn bucket(&self, index: u32, block_size: u32) -> u8 {
        let (tf_byte, width) = self.blocks[(index / block_size) as usize];
        if width == 0 {
            return 0;
        }
        let bit = (index % block_size) as usize * width as usize;
        segment::tinshape::bits::get_at(&self.tf[tf_byte + bit / 8..], bit % 8, width)
            .expect("a bucket within the TF tail") as u8
    }
}

/// Per group of an old grouped record, the (bucket, length) of each
/// posting, in posting order.
fn group_postings(
    segment: &Segment<'_>,
    old: &OldRecord<'_>,
    out: &mut Vec<Vec<(u8, u32)>>,
) -> Result<(), String> {
    let geometry = &segment.docs.geometry;
    out.resize_with(old.entries.len(), Vec::new);
    for (e, held) in old.entries.iter().zip(out.iter_mut()) {
        held.clear();
        let index = e.index as usize;
        let group = &geometry.groups[index];
        let bytes = &old.containers[e.at as usize..(e.at + e.len) as usize];
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
                Ok((
                    old.bucket(i, segment.block_size),
                    segment.lengths.get(rank)?,
                ))
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

/// A grouped record re-encoded: its directory with frontiers, the
/// containers copied, its inline lengths copied, its TF tail per group.
fn reencode_record(
    old: &OldRecord<'_>,
    groups: &[Vec<(u8, u32)>],
    mode: GroupFrontiers,
    cost: &mut Cost,
    out: &mut Vec<u8>,
) {
    let entries = &old.entries;
    let mut directory = Vec::with_capacity(entries.len() * 8 + 8);
    varint::put(&mut directory, entries.len() as u64);
    let mut tf = BitWriter::new();
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
        let width = tf_width(pairs.last().expect("a pair").0, true);
        for (bucket, _) in held {
            tf.put(u32::from(*bucket), width);
        }
    }
    let tf = tf.finish();
    cost.grouped_terms += 1;
    cost.footer_removed += old.footer_bytes as u64;
    cost.directory_before += old.directory_bytes as u64;
    cost.directory_after += directory.len() as u64;
    cost.tf_before += old.tf.len() as u64;
    cost.tf_after += tf.len() as u64;
    let form = FORM_GROUPED
        | if old.lengths.is_empty() {
            0
        } else {
            FORM_LENGTHS
        }
        | match mode {
            GroupFrontiers::Rounded => FORM_ROUNDED,
            GroupFrontiers::Exact => 0,
        };
    out.push(form);
    varint::put(out, (directory.len() + old.containers.len()) as u64);
    if !old.lengths.is_empty() {
        varint::put(out, old.lengths.len() as u64);
    }
    out.extend_from_slice(&directory);
    out.extend_from_slice(old.containers);
    out.extend_from_slice(old.lengths);
    out.extend_from_slice(&tf);
}

/// Re-encodes one blob.
fn reencode(blob: &[u8], mode: GroupFrontiers, cost: &mut Cost) -> Result<Vec<u8>, String> {
    let segment = Segment::parse(blob).map_err(|e| e.to_string())?;
    let (fields, _) = header_fields(blob)?;
    let postings_at = segment.area_at(Area::Postings);
    let mut groups: Vec<Vec<(u8, u32)>> = Vec::new();
    let mut out_postings = Vec::with_capacity(fields[5] as usize + fields[5] as usize / 20);
    let mut dictionary = DictionaryBuilder::default();
    for item in segment.dictionary().iter() {
        let (term, entry) = item.map_err(|e| e.to_string())?;
        let from = postings_at + entry.ordinals.offset as usize;
        let record = &blob[from..from + entry.ordinals.len as usize];
        let at = out_postings.len();
        let old = parse_old(
            record,
            entry.df,
            segment.block_size,
            segment.adaptive_tf,
            &segment.docs.geometry,
        )
        .map_err(|e| format!("{term}: {e}"))?;
        if let Some(old) = old {
            group_postings(&segment, &old, &mut groups).map_err(|e| format!("{term}: {e}"))?;
            reencode_record(&old, &groups, mode, cost, &mut out_postings);
        } else {
            out_postings.extend_from_slice(record);
        }
        let moved = TermEntry {
            ordinals: Extent {
                offset: at as u64,
                len: (out_postings.len() - at) as u32,
            },
            ..entry
        };
        dictionary.push(&term, moved).map_err(|e| e.to_string())?;
    }
    let rest = &blob[segment.area_at(Area::Positions)..segment.bounds[7]];
    let dictionary = dictionary.finish();
    let mut fields = fields.clone();
    fields[4] = dictionary.len() as u64;
    fields[5] = out_postings.len() as u64;
    let mut new = Vec::with_capacity(64 + dictionary.len() + out_postings.len() + rest.len());
    new.extend_from_slice(b"TNS1");
    for f in &fields {
        varint::put(&mut new, *f);
    }
    new.extend_from_slice(&dictionary);
    new.extend_from_slice(&out_postings);
    new.extend_from_slice(rest);
    cost.blob += new.len() as u64;
    if segment.bounds[7] != blob.len() {
        return Err("bytes past the liveness area".into());
    }
    Ok(new)
}

/// Where a re-encoded blob first differs from a fresh build's: the byte,
/// the area, and the term whose record holds it.
fn explain_difference(new: &[u8], built: &[u8]) {
    let at = new
        .iter()
        .zip(built)
        .position(|(a, b)| a != b)
        .unwrap_or(new.len().min(built.len()));
    println!(
        "  first difference at byte {at} (lengths {} / {})",
        new.len(),
        built.len()
    );
    for (label, blob) in [("re-encoded", new), ("built", built)] {
        let Ok(segment) = Segment::parse(blob) else {
            println!("  {label}: does not parse");
            continue;
        };
        let areas = [
            "header",
            "term map",
            "postings",
            "positions",
            "document set",
            "lengths",
            "liveness",
        ];
        let area = (0..7).find(|a| at < segment.bounds[a + 1]).unwrap_or(6);
        println!(
            "  {label}: in the {} area (bounds {:?})",
            areas[area], segment.bounds
        );
        if area == 2 {
            let base = segment.area_at(Area::Postings);
            for item in segment.dictionary().iter() {
                let Ok((term, entry)) = item else { break };
                let from = base + entry.ordinals.offset as usize;
                let to = from + entry.ordinals.len as usize;
                if (from..to).contains(&at) {
                    let record = &blob[from..to];
                    println!(
                        "  {label}: term {term:?} df {} max bucket {} record {}..{} ({} bytes), form {:#x}, first bytes {:?}",
                        entry.df,
                        entry.max_tf_bucket,
                        from,
                        to,
                        to - from,
                        record[0],
                        &record[..record.len().min(24)]
                    );
                    break;
                }
            }
        }
    }
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
    copy_manifest(&args.input, &args.output).unwrap_or_else(|e| {
        eprintln!("{}: {e}", args.output.display());
        std::process::exit(1)
    });
    let mut cost = Cost::default();
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
            let _ = std::fs::copy(args.input.join(dead), args.output.join(dead));
        }
        let start = Instant::now();
        let blob = std::fs::read(args.input.join(name)).unwrap_or_else(|e| {
            eprintln!("{name}: {e}");
            std::process::exit(1)
        });
        source += blob.len() as u64;
        let new = match reencode(&blob, args.mode, &mut cost) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{name}: {e}");
                std::process::exit(1)
            }
        };
        println!(
            "{name}: {} -> {} bytes ({:+.3}%)",
            blob.len(),
            new.len(),
            100.0 * (new.len() as f64 / blob.len() as f64 - 1.0)
        );
        if args.check_merge {
            let documents = header_fields(&blob).map(|(f, _)| f[0] as u32).unwrap_or(0);
            let none = DeadDocs::decode(&segment::ordinals::encode(&[]), documents).ok();
            let merged = none.as_ref().and_then(|dead| {
                merge(
                    &[Input { bytes: &new, dead }],
                    Options {
                        group_frontiers: args.mode,
                        ..Options::default()
                    },
                    || Ok(()),
                )
                .ok()
                .flatten()
            });
            let same = merged.as_ref().is_some_and(|m| m.blob == new);
            let clean = verify_segment(&new).is_clean();
            println!("{name}: equal to a merge's {same}, verifies {clean}");
            if let (false, Some(m)) = (same, merged.as_ref()) {
                explain_difference(&new, &m.blob);
            }
            ok &= same && clean;
        }
        std::fs::write(args.output.join(name), new).unwrap_or_else(|e| {
            eprintln!("{name}: {e}");
            std::process::exit(1)
        });
        eprintln!("{name}: {:.1} s", start.elapsed().as_secs_f64());
    }
    let c = &cost;
    println!("source {source} bytes");
    println!(
        "{:?}: blobs {} bytes ({:+} = {:+.3}%); grouped terms {}: footer -{} B, directory {} -> {} B (frontiers {} in {} B, {:.2} B each, {:.2} pairs), TF {} -> {} B; by pairs {:?}",
        args.mode,
        c.blob,
        c.blob as i64 - source as i64,
        100.0 * (c.blob as f64 / source as f64 - 1.0),
        c.grouped_terms,
        c.footer_removed,
        c.directory_before,
        c.directory_after,
        c.frontiers,
        c.frontier_bytes,
        c.frontier_bytes as f64 / c.frontiers.max(1) as f64,
        c.pairs as f64 / c.frontiers.max(1) as f64,
        c.tf_before,
        c.tf_after,
        &c.by_pairs[1..],
    );
    if !ok {
        std::process::exit(1);
    }
}
