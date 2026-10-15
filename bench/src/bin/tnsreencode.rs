// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Re-encodes dumped `TNS1` segments of page layout version 14, 13, 12, 11,
//! 10, 9, 8 or 6 to version 16, whose grouped records' directories are rows
//! of fixed width found by a group's rank and hold short lists as arrays,
//! whose groups of 16 members or more carry page masks, and whose DL
//! sidecar holds length classes (or, with `--keep-records`, records as they
//! were but for their headers and directories: no masks), and reports what
//! that costs. Version 14 has its records encoded again (step 5, with
//! `--fresh` or not; its containers copied, each group given its mask).
//! Version 13 (the DL sidecar's length classes, its
//! directories still varint entries) has its directories rewritten; with
//! `--keep-records` every other area, and the header's flags, are copied.
//!
//! ```text
//! cargo run -p bench --release --bin tnsreencode -- --in DUMP_DIR --out OUT_DIR \
//!     [--from 14|13|12|11|10|9|8|6] [--exact] [--segment FILE] [--grid-density N] [--grid-share N] \
//!     [--arrays N] [--jobs N] [--check] [--check-merge] [--fresh] [--census] [--keep-records]
//! ```
//!
//! Up to six steps per segment:
//!
//! 1. **The document set** (from 8 or 6). Versions 6 and 8 gave a group one
//!    width `w`,
//!    its largest offset, and slot `page * w + offset - 1`. The old set is
//!    rewritten as a version-9 set whose every page has the width `w`: the
//!    same slots, so every record, the DL sidecar and the liveness area
//!    stay valid as they are.
//! 2. With `--from 6`, grouped records lose their footer of blocks: their
//!    directory entries gain their group's frontier and their TF tail is
//!    packed per group (version 8). Each frontier is computed from the
//!    group's postings: buckets from the old TF tail (through the old
//!    footer), lengths from the DL sidecar. Everything else is copied.
//!    Their headers are written as version 10's.
//! 3. **Directory lengths** (from 9 or 8). Each grouped record's header
//!    gains its directory's length (version 10), found by stepping over
//!    the directory's entries; nothing else of the record changes.
//! 4. **Directory rows** (every source but 14). Each grouped record's
//!    directory of varint entries (versions 10 to 13, skip points left
//!    out) is rewritten as version 14's (`postings::encode_directory`): its
//!    presence, bases and rows, and its frontiers as they were; containers,
//!    inline lengths and TF tail are copied.
//! 5. **Renumbering.** The segment is merged alone with this build's
//!    `merge::merge`, which derives the grid from its documents (each
//!    page's width its largest offset) and encodes every record afresh, as
//!    a build of the same documents does, but for the containers of groups
//!    whose geometry and members it keeps, which it copies where they are
//!    what the options would choose (with `--fresh`, `merge::merge_afresh`:
//!    none is copied); this makes the Elias-Fano lists of at most
//!    `--arrays` members (default `Options::array_max`) array containers.
//! 6. **Length classes** (12 or earlier with `--keep-records`; step 5's
//!    merge writes them itself). The DL sidecar is encoded again with its
//!    class plane and the header flags it; every other area is copied.
//!
//! `--keep-records` skips step 5: each record keeps its encoding but for
//! the header and the directory (with `--check-merge`, the merge must give
//! the same bytes).
//!
//! `--grid-density N` encodes with `Options::grid_density` N (a writer
//! option: a group of a big term holding one slot in N is a grid) to
//! weigh other thresholds, `--grid-share N` with `Options::grid_min_share`
//! N (only a term in one document in N has its groups forced so), and
//! `--arrays N` with `Options::array_max` N
//! (0: no array containers). `--check` verifies each output with
//! `verify_segment`; `--check-merge`
//! also merges the output alone again (copying containers) and requires
//! the same bytes (with `--fresh`: that copying writes what a fresh
//! encoding does). Each
//! segment holds a few copies of itself in memory; `--jobs` re-encodes
//! that many at once (default 1).
//!
//! `--census` re-encodes nothing: per segment it reports the source at its
//! old slots (step 1, in memory) and the output already in `--out`: slots a
//! document, words a group, and group containers by kind.
//!
//! The output gets the source's manifest and a symlink to its `ids.tsv`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use segment::Tid;
use segment::dead::DeadDocs;
use segment::dictionary::{DictionaryBuilder, Extent, TermEntry};
use segment::tinshape::bits::BitWriter;
use segment::tinshape::docs::{GROUP_PAGES, Geometry, Lengths, encode_docset, encode_lengths};
use segment::tinshape::ef;
use segment::tinshape::merge::{Input, merge, merge_afresh};
use segment::tinshape::postings::{
    ARRAY_SLOTS_MAX, FORM_COMPACT, FORM_GROUPED, FORM_LENGTHS, FORM_ROUNDED, GroupEntry,
    GroupFrontiers, KIND_EF, KIND_GRID, KIND_PAGED, Options, encode_directory, for_each_local,
    group_frontier, put_group_frontier, tf_width,
};
use segment::tinshape::segment::{Area, FLAG_LENGTH_CLASSES, Segment};
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
    from: u8,
    mode: GroupFrontiers,
    grid_density: u32,
    grid_share: u32,
    array_max: u32,
    only: Option<String>,
    jobs: usize,
    check: bool,
    check_merge: bool,
    census: bool,
    keep_records: bool,
    fresh: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: tnsreencode --in DIR --out DIR [--from 14|13|12|11|10|9|8|6] [--exact] \
         [--segment FILE] [--grid-density N] [--grid-share N] [--arrays N] [--jobs N] [--check] \
         [--check-merge] [--fresh] [--census] [--keep-records]"
    );
    std::process::exit(2)
}

fn args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        input: PathBuf::new(),
        output: PathBuf::new(),
        from: 8,
        mode: GroupFrontiers::Rounded,
        grid_density: Options::default().grid_density,
        grid_share: Options::default().grid_min_share,
        array_max: Options::default().array_max,
        only: None,
        jobs: 1,
        check: false,
        check_merge: false,
        census: false,
        keep_records: false,
        fresh: false,
    };
    let value = |it: &mut std::iter::Skip<std::env::Args>| it.next().unwrap_or_else(|| usage());
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--in" => a.input = value(&mut it).into(),
            "--out" => a.output = value(&mut it).into(),
            "--from" => a.from = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--exact" => a.mode = GroupFrontiers::Exact,
            "--grid-density" => a.grid_density = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--grid-share" => a.grid_share = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--arrays" => a.array_max = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--segment" => a.only = Some(value(&mut it)),
            "--jobs" => a.jobs = value(&mut it).parse().unwrap_or_else(|_| usage()),
            "--check" => a.check = true,
            "--check-merge" => a.check_merge = true,
            "--census" => a.census = true,
            "--keep-records" => a.keep_records = true,
            "--fresh" => a.fresh = true,
            _ => usage(),
        }
    }
    if a.input.as_os_str().is_empty()
        || a.output.as_os_str().is_empty()
        || !matches!(a.from, 6 | 8 | 9 | 10 | 11 | 12 | 13 | 14)
        || (a.keep_records && matches!(a.from, 6 | 14))
        || a.jobs == 0
    {
        usage();
    }
    a
}

/// What the output spends and saves.
#[derive(Default, Clone)]
struct Cost {
    blob: u64,
    grouped_terms: u64,
    /// Grouped records given a directory length (step 3), and the bytes
    /// their headers grew by.
    lengthened: u64,
    length_bytes: u64,
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
    /// Step 4: grouped records, their entries, and their directories'
    /// bytes as varint entries and as rows.
    rowed: u64,
    rowed_entries: u64,
    rows_before: u64,
    rows_after: u64,
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
        let starts = geometry.starts(index);
        let bytes = &old.containers[e.at as usize..(e.at + e.len) as usize];
        let mut failed = None;
        let mut i = e.first;
        for_each_local(e, bytes, starts, |local| {
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
    varint::put(out, directory.len() as u64);
    varint::put(out, (directory.len() + old.containers.len()) as u64);
    if !old.lengths.is_empty() {
        varint::put(out, old.lengths.len() as u64);
    }
    out.extend_from_slice(&directory);
    out.extend_from_slice(old.containers);
    out.extend_from_slice(old.lengths);
    out.extend_from_slice(&tf);
}

/// Step 2: re-encodes one blob's grouped records from version 6 to 8.
fn reencode_v6(blob: &[u8], mode: GroupFrontiers, cost: &mut Cost) -> Result<Vec<u8>, String> {
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

/// Bytes of a version-8 or -9 grouped directory at the start of `payload`
/// (its group count and entries), stepped over a varint at a time.
fn directory_len(payload: &[u8]) -> Result<usize, String> {
    let mut at = 0;
    let groups = varint::get(payload, &mut at)?;
    for _ in 0..groups {
        varint::get(payload, &mut at)?;
        let packed = varint::get(payload, &mut at)?;
        if packed & 3 == u64::from(KIND_PAGED) {
            varint::get(payload, &mut at)?;
        }
        let head = *payload.get(at).ok_or("truncated frontier")?;
        at += 1;
        for _ in 0..=head >> 4 {
            varint::get(payload, &mut at)?;
        }
    }
    Ok(at)
}

/// Step 3: `blob`'s grouped records of version 8 or 9 with their headers
/// given their directory's length (version 10); every other byte of the
/// records is copied.
fn add_directory_lengths(blob: &[u8], cost: &mut Cost) -> Result<Vec<u8>, String> {
    let segment = Segment::parse(blob).map_err(|e| e.to_string())?;
    let (fields, _) = header_fields(blob)?;
    let postings_at = segment.area_at(Area::Postings);
    let mut out_postings = Vec::with_capacity(fields[5] as usize + fields[5] as usize / 100);
    let mut dictionary = DictionaryBuilder::default();
    for item in segment.dictionary().iter() {
        let (term, entry) = item.map_err(|e| e.to_string())?;
        let from = postings_at + entry.ordinals.offset as usize;
        let record = &blob[from..from + entry.ordinals.len as usize];
        let at = out_postings.len();
        if entry.df > 1 && record[0] & 3 == FORM_GROUPED {
            let form = record[0];
            let mut p = 1;
            let payload_len = varint::get(record, &mut p)? as usize;
            if form & FORM_LENGTHS != 0 {
                varint::get(record, &mut p)?;
            }
            let payload = record
                .get(p..p + payload_len)
                .ok_or_else(|| format!("{term}: payload past the record"))?;
            let len = directory_len(payload).map_err(|e| format!("{term}: {e}"))?;
            if len > payload_len {
                return Err(format!("{term}: directory past the payload"));
            }
            out_postings.push(form);
            varint::put(&mut out_postings, len as u64);
            out_postings.extend_from_slice(&record[1..]);
            cost.lengthened += 1;
            cost.length_bytes += (out_postings.len() - at - record.len()) as u64;
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
    if segment.bounds[7] != blob.len() {
        return Err("bytes past the liveness area".into());
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

impl Cost {
    fn add(&mut self, other: &Cost) {
        self.blob += other.blob;
        self.grouped_terms += other.grouped_terms;
        self.lengthened += other.lengthened;
        self.length_bytes += other.length_bytes;
        self.footer_removed += other.footer_removed;
        self.directory_before += other.directory_before;
        self.directory_after += other.directory_after;
        self.frontier_bytes += other.frontier_bytes;
        self.frontiers += other.frontiers;
        self.pairs += other.pairs;
        self.tf_before += other.tf_before;
        self.tf_after += other.tf_after;
        for (a, b) in self.by_pairs.iter_mut().zip(other.by_pairs) {
            *a += b;
        }
        self.rowed += other.rowed;
        self.rowed_entries += other.rowed_entries;
        self.rows_before += other.rows_before;
        self.rows_after += other.rows_after;
    }
}

/// Area names, in the blob's order after its header.
const AREAS: [&str; 6] = [
    "term map",
    "postings",
    "positions",
    "document set",
    "lengths",
    "liveness",
];

/// The bytes of each area of `blob`, in [`AREAS`] order.
fn area_sizes(blob: &[u8]) -> Result<[u64; 6], String> {
    let (fields, _) = header_fields(blob)?;
    Ok(std::array::from_fn(|i| fields[4 + i]))
}

/// The ctids and the grid of a version-6 or -8 document set:
///
/// ```text
/// docset := groups varint, (group u32le, width u16le, first u8, pages - 1 u8,
///            rank_base u32le, kind u8)*, body* (per group, in order)
/// body   := grid:   pages * width slots as whole u64le words, slot
///                   page * width + offset - 1
///         | counts: a byte per page, page p holding offsets 1..=count[p]
/// ```
///
/// Every page of the returned grid has its group's old width, so its slots
/// are the old slots.
fn legacy_docset(bytes: &[u8]) -> Result<(Geometry, Vec<Tid>), String> {
    const ENTRY: usize = 13;
    let mut at = 0usize;
    let count = varint::get(bytes, &mut at)? as usize;
    let directory = bytes
        .get(at..at + count * ENTRY)
        .ok_or("truncated document set")?;
    at += count * ENTRY;
    let mut geometry = Geometry::default();
    let mut tids = Vec::new();
    for entry in directory.chunks_exact(ENTRY) {
        let id = u32::from_le_bytes(entry[0..4].try_into().expect("four bytes"));
        let width = u16::from_le_bytes([entry[4], entry[5]]);
        let first = entry[6];
        let pages = usize::from(entry[7]) + 1;
        let rank_base = u32::from_le_bytes(entry[8..12].try_into().expect("four bytes"));
        if rank_base as usize != tids.len() || width == 0 {
            return Err("document set directory".into());
        }
        let block = |page: usize| id * GROUP_PAGES + u32::from(first) + page as u32;
        match entry[12] {
            0 => {
                let slots = pages * usize::from(width);
                let len = slots.div_ceil(64) * 8;
                let body = bytes.get(at..at + len).ok_or("truncated document set")?;
                at += len;
                for local in 0..slots {
                    if body[local / 8] >> (local % 8) & 1 == 1 {
                        let (page, offset) =
                            (local / usize::from(width), local % usize::from(width));
                        tids.push(Tid {
                            block: block(page),
                            offset: offset as u16 + 1,
                        });
                    }
                }
            }
            1 => {
                let body = bytes.get(at..at + pages).ok_or("truncated document set")?;
                at += pages;
                for (page, n) in body.iter().enumerate() {
                    tids.extend((1..=u16::from(*n)).map(|offset| Tid {
                        block: block(page),
                        offset,
                    }));
                }
            }
            _ => return Err("document set kind".into()),
        }
        geometry
            .push_group(id, first, &vec![width; pages], rank_base)
            .map_err(|e| e.to_string())?;
    }
    if at != bytes.len() {
        return Err("document set length".into());
    }
    geometry.documents = tids.len() as u32;
    Ok((geometry, tids))
}

/// Step 1: `blob` with its version-6 or -8 document set rewritten as a
/// version-9 set of the same slots.
fn regrid(blob: &[u8]) -> Result<Vec<u8>, String> {
    let (mut fields, header) = header_fields(blob)?;
    let mut bounds = [header; 7];
    for i in 0..6 {
        bounds[i + 1] = bounds[i] + fields[4 + i] as usize;
    }
    if bounds[6] != blob.len() {
        return Err("areas against the blob's length".into());
    }
    let (geometry, tids) = legacy_docset(&blob[bounds[3]..bounds[4]])?;
    if tids.len() as u64 != fields[0] {
        return Err("document set against the header's documents".into());
    }
    let docset = encode_docset(&geometry, &tids);
    fields[7] = docset.len() as u64;
    let mut out = Vec::with_capacity(blob.len());
    out.extend_from_slice(b"TNS1");
    for f in &fields {
        varint::put(&mut out, *f);
    }
    out.extend_from_slice(&blob[header..bounds[3]]);
    out.extend_from_slice(&docset);
    out.extend_from_slice(&blob[bounds[4]..]);
    Ok(out)
}

/// Step 6: `blob` (layout 12 or earlier, but for its directories) with its
/// DL sidecar's class plane; nothing else changes.
fn add_length_classes(blob: &[u8]) -> Result<Vec<u8>, String> {
    let (mut fields, header) = header_fields(blob)?;
    if fields[3] & FLAG_LENGTH_CLASSES != 0 {
        return Err("the DL sidecar holds length classes already".into());
    }
    let mut bounds = [header; 7];
    for i in 0..6 {
        bounds[i + 1] = bounds[i] + fields[4 + i] as usize;
    }
    if bounds[6] != blob.len() {
        return Err("areas against the blob's length".into());
    }
    let documents = u32::try_from(fields[0]).map_err(|_| "document count")?;
    let old =
        Lengths::parse(&blob[bounds[4]..bounds[5]], documents, false).map_err(|e| e.to_string())?;
    let lengths = (0..documents)
        .map(|rank| old.get(rank))
        .collect::<Result<Vec<u32>, _>>()
        .map_err(|e| e.to_string())?;
    let sidecar = encode_lengths(&lengths, true);
    fields[3] |= FLAG_LENGTH_CLASSES;
    fields[8] = sidecar.len() as u64;
    let mut out = Vec::with_capacity(blob.len() + sidecar.len() - (bounds[5] - bounds[4]) + 8);
    out.extend_from_slice(b"TNS1");
    for f in &fields {
        varint::put(&mut out, *f);
    }
    out.extend_from_slice(&blob[header..bounds[4]]);
    out.extend_from_slice(&sidecar);
    out.extend_from_slice(&blob[bounds[5]..]);
    Ok(out)
}

/// A version 10 to 12 group directory (`directory`, from its group count
/// on, its skip points after its entries left out) of a record with
/// `containers` container bytes, as version 14's: its entries with their
/// totals, and their frontiers' bytes as they were.
fn legacy_entries(
    directory: &[u8],
    containers: usize,
    geometry: &Geometry,
) -> Result<(Vec<GroupEntry>, Vec<u8>), String> {
    let mut at = 0;
    let n = varint::get(directory, &mut at)? as usize;
    let mut entries = Vec::with_capacity(n);
    let mut frontiers = Vec::new();
    let (mut index, mut body, mut first, mut tf_bit) = (0u64, 0u64, 0u64, 0u64);
    for i in 0..n {
        let gap = varint::get(directory, &mut at)?;
        index = if i == 0 { gap } else { index + 1 + gap };
        let group = geometry
            .groups
            .get(index as usize)
            .ok_or("a directory entry past the groups")?;
        let packed = varint::get(directory, &mut at)?;
        let (kind, count) = ((packed & 3) as u8, (packed >> 2) + 1);
        let len = match kind {
            KIND_GRID => group.grid_bytes() as u64,
            KIND_EF => ef::encoded_len(count as usize, group.slots()) as u64,
            KIND_PAGED => varint::get(directory, &mut at)?,
            _ if group.slots() <= ARRAY_SLOTS_MAX => 2 * count,
            _ => return Err("an array of a group past 65,536 slots".into()),
        };
        // The frontier: its head, its first length, a delta per later pair.
        let from = at;
        let head = *directory.get(at).ok_or("truncated frontier")?;
        at += 1;
        let mut bucket = u64::from(head & 15);
        varint::get(directory, &mut at)?;
        for _ in 0..head >> 4 {
            bucket += 1 + (varint::get(directory, &mut at)? & 15);
        }
        let max_bucket = u8::try_from(bucket)
            .ok()
            .filter(|b| *b < 16)
            .ok_or("frontier bucket")?;
        let width = tf_width(max_bucket, true);
        entries.push(GroupEntry {
            index: index as u32,
            count: count as u32,
            kind,
            at: body as u32,
            len: len as u32,
            first: first as u32,
            frontier: frontiers.len() as u32,
            tf_bit: tf_bit as u32,
            width: width as u8,
            max_bucket,
        });
        frontiers.extend_from_slice(&directory[from..at]);
        body += len;
        first += count;
        tf_bit += count * u64::from(width);
    }
    if body != containers as u64 {
        return Err("directory against its containers".into());
    }
    Ok((entries, frontiers))
}

/// Step 4: `blob`'s grouped records with their directories of varint
/// entries rewritten as rows (version 14); every other byte of the records
/// is copied.
fn directory_rows(blob: &[u8], cost: &mut Cost) -> Result<Vec<u8>, String> {
    /// Set in a version 11 or 12 grouped record's form: its directory
    /// ends in skip points.
    const FORM_SKIPS: u8 = 0x20;
    let segment = Segment::parse(blob).map_err(|e| e.to_string())?;
    let geometry = &segment.docs.geometry;
    let (fields, _) = header_fields(blob)?;
    let postings_at = segment.area_at(Area::Postings);
    let mut out_postings = Vec::with_capacity(fields[5] as usize + fields[5] as usize / 20);
    let mut dictionary = DictionaryBuilder::default();
    for item in segment.dictionary().iter() {
        let (term, entry) = item.map_err(|e| e.to_string())?;
        let from = postings_at + entry.ordinals.offset as usize;
        let record = &blob[from..from + entry.ordinals.len as usize];
        let at = out_postings.len();
        if entry.df > 1 && record[0] & 3 == FORM_GROUPED {
            let form = record[0];
            let mut p = 1;
            let directory_len = varint::get(record, &mut p)? as usize;
            let payload_len = varint::get(record, &mut p)? as usize;
            let lengths_len = if form & FORM_LENGTHS != 0 {
                Some(varint::get(record, &mut p)?)
            } else {
                None
            };
            let payload = record
                .get(p..p + payload_len)
                .ok_or_else(|| format!("{term}: payload past the record"))?;
            let rest = &record[p + payload_len..];
            let containers = payload
                .get(directory_len..)
                .ok_or_else(|| format!("{term}: directory past the payload"))?;
            let (entries, frontiers) =
                legacy_entries(&payload[..directory_len], containers.len(), geometry)
                    .map_err(|e| format!("{term}: {e}"))?;
            let mut directory = Vec::new();
            encode_directory(geometry.groups.len(), &entries, &frontiers, &mut directory);
            out_postings.push(form & !FORM_SKIPS);
            varint::put(&mut out_postings, directory.len() as u64);
            varint::put(
                &mut out_postings,
                (directory.len() + containers.len()) as u64,
            );
            if let Some(len) = lengths_len {
                varint::put(&mut out_postings, len);
            }
            out_postings.extend_from_slice(&directory);
            out_postings.extend_from_slice(containers);
            out_postings.extend_from_slice(rest);
            cost.rowed += 1;
            cost.rowed_entries += entries.len() as u64;
            cost.rows_before += directory_len as u64;
            cost.rows_after += directory.len() as u64;
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
    if segment.bounds[7] != blob.len() {
        return Err("bytes past the liveness area".into());
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
    Ok(new)
}

/// Step 5: `blob` merged alone, every record encoded over the grid of its
/// documents: afresh (no container copied) when `fresh`.
fn renumber(blob: &[u8], args: &Args, fresh: bool) -> Result<Vec<u8>, String> {
    let (fields, _) = header_fields(blob)?;
    let none = DeadDocs::decode(&segment::ordinals::encode(&[]), fields[0] as u32)
        .map_err(|e| e.to_string())?;
    let inputs = [Input {
        bytes: blob,
        dead: &none,
    }];
    let options = Options {
        group_frontiers: args.mode,
        grid_density: args.grid_density,
        grid_min_share: args.grid_share,
        array_max: args.array_max,
        ..Options::default()
    };
    let merged = if fresh {
        merge_afresh(&inputs, options, || Ok(()))
    } else {
        merge(&inputs, options, || Ok(()))
    };
    Ok(merged
        .map_err(|e| e.to_string())?
        .ok_or("no live document")?
        .blob)
}

/// One segment, re-encoded: its line of the report, its version-6 costs,
/// and whether its checks passed.
fn one(args: &Args, name: &str, cost: &mut Cost) -> Result<(String, bool), String> {
    let start = Instant::now();
    let blob = std::fs::read(args.input.join(name)).map_err(|e| e.to_string())?;
    let before = area_sizes(&blob)?;
    let source = blob.len();
    let mut new = if args.from >= 9 { blob } else { regrid(&blob)? };
    if args.from == 6 {
        new = reencode_v6(&new, args.mode, cost)?;
    } else if args.from < 10 {
        new = add_directory_lengths(&new, cost)?;
    }
    if args.from < 14 {
        new = directory_rows(&new, cost)?;
    }
    let new = if args.keep_records {
        new
    } else {
        renumber(&new, args, args.fresh)?
    };
    let new = if header_fields(&new)?.0[3] & FLAG_LENGTH_CLASSES == 0 {
        add_length_classes(&new)?
    } else {
        new
    };
    let after = area_sizes(&new)?;
    let mut line = format!(
        "{name}: {source} -> {} bytes ({:+.3}%)",
        new.len(),
        100.0 * (new.len() as f64 / source as f64 - 1.0)
    );
    for (i, area) in AREAS.iter().enumerate() {
        line += &format!("; {area} {} -> {}", before[i], after[i]);
    }
    let mut ok = true;
    if args.check || args.check_merge {
        let clean = verify_segment(&new).is_clean();
        line += &format!("; verifies {clean}");
        ok &= clean;
    }
    if args.check_merge {
        let again = renumber(&new, args, false)?;
        let same = again == new;
        line += &format!("; equal to its own merge {same}");
        if !same {
            explain_difference(&new, &again);
        }
        ok &= same;
    }
    std::fs::write(args.output.join(name), &new).map_err(|e| e.to_string())?;
    line += &format!("; {:.1} s", start.elapsed().as_secs_f64());
    Ok((line, ok))
}

/// A segment's grid and containers, for `--census`.
#[derive(Default, Clone, Copy)]
struct Census {
    documents: u64,
    groups: u64,
    slots: u64,
    /// 64-bit words of a bitmap over each group, summed.
    words: u64,
    /// Group containers by kind (grid, Elias-Fano, paged): how many, their
    /// members and their bytes; grids of groups a term was forced into.
    kinds: [(u64, u64, u64); 4],
    sparse_terms: u64,
    sparse_bytes: u64,
}

impl Census {
    fn of(blob: &[u8]) -> Result<Self, String> {
        let segment = Segment::parse(blob).map_err(|e| e.to_string())?;
        let geometry = &segment.docs.geometry;
        let mut c = Census {
            documents: u64::from(segment.documents),
            groups: geometry.groups.len() as u64,
            slots: u64::from(geometry.slots),
            words: geometry.groups.iter().map(|g| g.words() as u64).sum(),
            ..Census::default()
        };
        for item in segment.dictionary().iter() {
            let (term, entry) = item.map_err(|e| e.to_string())?;
            let found = segment.resolve(entry).map_err(|e| format!("{term}: {e}"))?;
            match &found.postings.form {
                segment::tinshape::postings::Form::Grouped(dir) => {
                    let entries = dir
                        .entries(&segment.docs.geometry.groups)
                        .map_err(|e| format!("{term}: {e}"))?;
                    for e in entries.iter() {
                        let k = &mut c.kinds[usize::from(e.kind & 3)];
                        k.0 += 1;
                        k.1 += u64::from(e.count);
                        k.2 += u64::from(e.len);
                    }
                }
                _ => {
                    c.sparse_terms += 1;
                    c.sparse_bytes += found.postings.payload.len() as u64;
                }
            }
        }
        Ok(c)
    }

    fn add(&mut self, o: &Census) {
        self.documents += o.documents;
        self.groups += o.groups;
        self.slots += o.slots;
        self.words += o.words;
        for (a, b) in self.kinds.iter_mut().zip(o.kinds) {
            a.0 += b.0;
            a.1 += b.1;
            a.2 += b.2;
        }
        self.sparse_terms += o.sparse_terms;
        self.sparse_bytes += o.sparse_bytes;
    }

    fn line(&self) -> String {
        let kinds = ["grid", "ef", "paged", "array"]
            .iter()
            .zip(self.kinds)
            .map(|(name, (n, members, bytes))| format!("{name} {n} ({members} members, {bytes} B)"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "documents {} groups {} slots {} ({:.3} a document) words {} ({:.1} a group); containers: {kinds}; sparse terms {} ({} B)",
            self.documents,
            self.groups,
            self.slots,
            self.slots as f64 / self.documents.max(1) as f64,
            self.words,
            self.words as f64 / self.groups.max(1) as f64,
            self.sparse_terms,
            self.sparse_bytes,
        )
    }
}

/// `--census`: each segment of the source at its old slots (step 1 only, in
/// memory, when it is of version 6 or 8) and as re-encoded in the output.
fn census(args: &Args, names: &[String]) {
    let mut totals = [Census::default(); 2];
    for name in names {
        let blob = std::fs::read(args.input.join(name)).unwrap_or_else(|e| {
            eprintln!("{name}: {e}");
            std::process::exit(1)
        });
        // A source already of version 9 (a dump of a fresh build) is
        // counted as it is.
        let old = match regrid(&blob) {
            Ok(regridded) => Census::of(&regridded),
            Err(_) => Census::of(&blob),
        };
        drop(blob);
        let new = std::fs::read(args.output.join(name))
            .map_err(|e| e.to_string())
            .and_then(|b| Census::of(&b));
        match (old, new) {
            (Ok(old), Ok(new)) => {
                println!("{name} source: {}", old.line());
                println!("{name} output: {}", new.line());
                totals[0].add(&old);
                totals[1].add(&new);
            }
            (old, new) => {
                eprintln!("{name}: {:?} / {:?}", old.err(), new.err());
                std::process::exit(1)
            }
        }
    }
    println!("source: {}", totals[0].line());
    println!("output: {}", totals[1].line());
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
    let mut names = Vec::new();
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
        names.push(name.to_string());
    }
    if args.census {
        census(&args, &names);
        return;
    }
    let queue = Mutex::new(names.into_iter());
    let totals = Mutex::new((Cost::default(), [[0u64; 6]; 2], true));
    std::thread::scope(|scope| {
        for _ in 0..args.jobs {
            scope.spawn(|| {
                loop {
                    let Some(name) = queue.lock().expect("queue").next() else {
                        return;
                    };
                    let mut cost = Cost::default();
                    match one(&args, &name, &mut cost) {
                        Ok((line, ok)) => {
                            println!("{line}");
                            let mut t = totals.lock().expect("totals");
                            t.0.add(&cost);
                            t.2 &= ok;
                            let blob = std::fs::read(args.input.join(&name)).ok();
                            let new = std::fs::read(args.output.join(&name)).ok();
                            if let (Some(b), Some(n)) = (blob, new)
                                && let (Ok(b), Ok(n)) = (area_sizes(&b), area_sizes(&n))
                            {
                                for i in 0..6 {
                                    t.1[0][i] += b[i];
                                    t.1[1][i] += n[i];
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("{name}: {e}");
                            std::process::exit(1)
                        }
                    }
                }
            });
        }
    });
    let (cost, areas, ok) = totals.into_inner().expect("totals");
    let (before, after): (u64, u64) = (areas[0].iter().sum(), areas[1].iter().sum());
    println!(
        "areas, source -> output: {}",
        AREAS
            .iter()
            .enumerate()
            .map(|(i, a)| format!(
                "{a} {} -> {} ({:+.2}%)",
                areas[0][i],
                areas[1][i],
                100.0 * (areas[1][i] as f64 / areas[0][i].max(1) as f64 - 1.0)
            ))
            .collect::<Vec<_>>()
            .join("; ")
    );
    println!(
        "areas together {before} -> {after} ({:+.3}%)",
        100.0 * (after as f64 / before.max(1) as f64 - 1.0)
    );
    println!(
        "directories as rows: {} grouped records, {} entries: {} -> {} B ({:+.2} B an entry)",
        cost.rowed,
        cost.rowed_entries,
        cost.rows_before,
        cost.rows_after,
        (cost.rows_after as f64 - cost.rows_before as f64) / cost.rowed_entries.max(1) as f64
    );
    if args.from != 6 {
        println!(
            "grouped records given a directory length: {}, headers +{} B ({:.2} B each)",
            cost.lengthened,
            cost.length_bytes,
            cost.length_bytes as f64 / cost.lengthened.max(1) as f64
        );
    }
    if args.from == 6 {
        let c = &cost;
        println!(
            "{:?}, version 6 to 8: grouped terms {}: footer -{} B, directory {} -> {} B (frontiers {} in {} B, {:.2} B each, {:.2} pairs), TF {} -> {} B; by pairs {:?}",
            args.mode,
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
    }
    if !ok {
        std::process::exit(1);
    }
}
