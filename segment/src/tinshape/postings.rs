// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A term's postings: its documents as a set of ctid-grid slots (see
//! [`super::docs`]), a footer of per-block score metadata, and the TF tail.
//!
//! ```text
//! record  := single | full
//! single  := slot varint                    (df = 1; the bucket is the
//!                                            dictionary's max_tf_bucket)
//! full    := form u8 (1 sparse, 2 grouped), footer_len varint,
//!            payload_len varint, footer, payload, tf
//! footer  := block*, one per `block_size` postings in posting order:
//!            last_gap varint (its last slot less the previous block's,
//!                             the first block's from zero),
//!            frontier_len u8 (1..=16),
//!            (bucket u8, min_length varint)* ascending in both: the
//!            block's impact frontier
//! payload := sparse:  ef(df slots below the grid's slot count)
//!          | grouped: groups varint,
//!                     (index_gap varint, (count - 1) << 2 | kind varint,
//!                      [len varint, paged groups only],
//!                      group frontier)*,
//!                     container*
//! group frontier := head u8 = (n - 1) << 4 | first bucket,
//!            first length varint,
//!            ((length - previous length - 1) << 4
//!             | bucket - previous bucket - 1) varint, for each later pair:
//!            the group's impact frontier, its lengths rounded down to six
//!            significant bits when the form says so
//! container := grid:  32 * width bytes, bit `local` set for each member
//!            | ef:    ef(count local slots below 256 * width)
//!            | paged: (pages - 1) u8, the pages (a byte each when fewer
//!                     than 32, else a 256-bit mask), then per page
//!                     head u8 = kind << 6 | value and its offsets:
//!                     list   (value + 1 offsets, `offset - 1` packed at
//!                             the bits the width needs),
//!                     bitmap (value + 1 bytes, bit `offset - 1`),
//!                     run    (first and count, less one, packed so)
//! tf      := per block, its buckets packed at the block's width: 0 bits
//!            when its largest bucket is 0, 1 when 1, 2 when at most 3,
//!            else 4
//! ```
//!
//! The impact frontier is the set of (bucket, length) pairs no other
//! posting of the block dominates (a bucket at least as high with a length
//! at most as long): the block's best score under any `k1` and `b` is the
//! score of one of them, as Lucene's competitive impacts are.
//!
//! A posting's index (its rank in slot order) addresses its bucket in the
//! TF tail and its positions in the positions area.

use super::bits::{self, BitWriter};
use super::blob::{Bytes, Kind};
use super::docs::{Geometry, Group};
use super::ef::{self, Ef};
use crate::{Error, Result, varint};

pub const FORM_SPARSE: u8 = 1;
pub const FORM_GROUPED: u8 = 2;
/// Set in a record's form when it carries [`InlineLengths`].
pub const FORM_LENGTHS: u8 = 0x80;
/// Set in a record's form when it has no footer: a sparse term of at most
/// [`COMPACT_MAX`] postings (and one block) carrying its lengths, whose
/// footer a reader derives (its last slot from the list, its frontier from
/// its buckets and lengths, its TF width from its largest bucket).
pub const FORM_COMPACT: u8 = 0x40;

/// Set in a grouped record's form when each directory entry ends with its
/// group's impact frontier ([`GroupFrontier`]), which a reader bounds the
/// group by before it reads the group's container.
pub const FORM_FRONTIERS: u8 = 0x20;
/// Set with [`FORM_FRONTIERS`] when the frontiers' lengths are rounded down
/// to six significant bits ([`round_length`]).
pub const FORM_ROUNDED: u8 = 0x10;

/// Postings a footer-less record holds at most.
pub const COMPACT_MAX: u32 = 8;

pub const KIND_GRID: u8 = 0;
pub const KIND_EF: u8 = 1;
pub const KIND_PAGED: u8 = 2;

const PAGE_LIST: u8 = 0;
const PAGE_BITMAP: u8 = 1;
const PAGE_RUN: u8 = 2;
/// Most offsets a page list holds.
const LIST_MAX: usize = 64;

/// What an encoder may choose from.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Postings per footer block.
    pub block_size: u32,
    /// Whether a group may use the paged container.
    pub paged: bool,
    /// Whether a group may use the Elias-Fano container.
    pub ef_groups: bool,
    /// Whether a whole term may be one Elias-Fano list.
    pub sparse: bool,
    /// Whether the TF tail packs each block at its own width (else 4 bits).
    pub adaptive_tf: bool,
    /// A group holding at least one slot in this many is a grid bitmap
    /// whatever else would be smaller, so a fold reads it word by word
    /// rather than decoding a member at a time; 0 chooses by size alone.
    pub grid_density: u32,
    /// ... but only for a term of at least this many postings: decoding a
    /// smaller term whole costs microseconds, and a small table, every
    /// term of which is small, would pay for grids it never needs.
    pub grid_min_postings: u32,
    /// A term of at most this many postings carries its documents' lengths
    /// in its record ([`InlineLengths`]): its top k reads no DL sidecar.
    pub inline_lengths_max_df: u32,
    /// ... in a segment of at least this many documents: a smaller one's DL
    /// sidecar is a few pages.
    pub inline_lengths_min_documents: u32,
    /// What a grouped record's directory holds of each group's postings
    /// beyond their count: their impact frontier, exact or rounded.
    pub group_frontiers: GroupFrontiers,
}

/// [`Options::group_frontiers`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupFrontiers {
    /// None (the directory of page layout version 6).
    Off,
    /// The frontier's exact lengths: at 150M rows 3% more frontier bytes
    /// than rounded and 0.1% fewer candidates examined.
    Exact,
    /// Its lengths rounded down to six significant bits, as TIN stores its
    /// per-group frontiers (the default).
    Rounded,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            block_size: 256,
            paged: true,
            ef_groups: true,
            sparse: true,
            adaptive_tf: true,
            grid_density: 64,
            grid_min_postings: 4096,
            inline_lengths_max_df: 64,
            inline_lengths_min_documents: 1 << 16,
            group_frontiers: GroupFrontiers::Rounded,
        }
    }
}

/// Bytes an encoded record spends where, and the containers it chose.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub header: usize,
    pub footer: usize,
    pub payload: usize,
    pub tf: usize,
    pub blocks: usize,
    pub sparse: bool,
    /// Groups per container kind: grid, ef, paged.
    pub kinds: [usize; 3],
    /// Bytes per container kind.
    pub kind_bytes: [usize; 3],
    /// Groups made grids by [`Options::grid_density`].
    pub forced: usize,
    /// Bytes of inline lengths ([`InlineLengths`]).
    pub lengths: usize,
    /// Bytes of the directory's group frontiers (part of `payload`), and
    /// how many it holds.
    pub frontier_bytes: usize,
    pub frontiers: usize,
}

impl Stats {
    pub fn total(&self) -> usize {
        self.header + self.footer + self.payload + self.tf + self.lengths
    }
}

/// The TF tail width of a block whose largest bucket is `max`.
pub const fn tf_width(max: u8, adaptive: bool) -> u32 {
    if !adaptive {
        return 4;
    }
    match max {
        0 => 0,
        1 => 1,
        2..=3 => 2,
        _ => 4,
    }
}

/// The impact frontier of postings `(bucket, length)`: ascending in bucket
/// and in length, no pair dominated by another.
pub fn frontier(postings: impl Iterator<Item = (u8, u32)>) -> Vec<(u8, u32)> {
    let mut best = [u32::MAX; 16];
    for (bucket, length) in postings {
        let slot = &mut best[bucket as usize & 15];
        *slot = (*slot).min(length);
    }
    // From the highest bucket down, keep a pair only if it is shorter than
    // every pair above it.
    let mut out = Vec::new();
    let mut shortest = u32::MAX;
    for bucket in (0..16).rev() {
        if best[bucket] < shortest {
            shortest = best[bucket];
            out.push((bucket as u8, shortest));
        }
    }
    out.reverse();
    out
}

/// `length` rounded down to six significant bits, as a code that orders
/// as lengths do: a length below 64 is its own code; a longer one, `m << s`
/// with `m` its six leading bits (32 to 63) and `s` at least 1, is
/// `64 + 32 * (s - 1) + m - 32`. A bound at the rounded length is a bound
/// at the length (a score falls as the length grows).
#[inline]
pub const fn round_length(length: u32) -> u32 {
    let bits = 32 - length.leading_zeros();
    if bits <= 6 {
        return length;
    }
    let s = bits - 6;
    64 + 32 * (s - 1) + (length >> s) - 32
}

/// The length [`round_length`]'s `code` stands for: the longest it rounds
/// down to, never above a length that gives the code.
#[inline]
pub const fn rounded_length(code: u32) -> u32 {
    if code < 64 {
        return code;
    }
    let s = (code - 64) / 32 + 1;
    let m = 32 + (code - 64) % 32;
    if s >= 27 { u32::MAX } else { m << s }
}

/// The impact frontier of a group's postings `(bucket, length)` as its
/// directory entry stores it: [`frontier`] of the lengths, or of their
/// [`round_length`] codes.
pub fn group_frontier(
    postings: impl Iterator<Item = (u8, u32)>,
    mode: GroupFrontiers,
) -> Vec<(u8, u32)> {
    match mode {
        GroupFrontiers::Rounded => frontier(postings.map(|(b, l)| (b, round_length(l)))),
        _ => frontier(postings),
    }
}

/// Appends a group frontier's encoding (see the module's grammar): pairs
/// ascending in bucket and strictly in length (or code), 1 to 16 of them.
pub fn put_group_frontier(out: &mut Vec<u8>, pairs: &[(u8, u32)]) {
    debug_assert!((1..=16).contains(&pairs.len()));
    let (b0, l0) = pairs[0];
    out.push(((pairs.len() as u8 - 1) << 4) | (b0 & 15));
    varint::put(out, u64::from(l0));
    for w in pairs.windows(2) {
        let ((pb, pl), (b, l)) = (w[0], w[1]);
        debug_assert!(b > pb && l > pl);
        varint::put(out, (u64::from(l - pl - 1) << 4) | u64::from(b - pb - 1));
    }
}

/// Checks and steps over a group frontier at `at`.
#[inline]
fn skip_group_frontier(bytes: &[u8], at: &mut usize) -> Result<()> {
    let head = *bytes.get(*at).ok_or(Error::Truncated)?;
    *at += 1;
    let mut bucket = u32::from(head & 15);
    let mut length = varint::get(bytes, at)?;
    for _ in 0..head >> 4 {
        let d = varint::get(bytes, at)?;
        bucket += 1 + (d & 15) as u32;
        length += 1 + (d >> 4);
    }
    if bucket > 15 || length > u64::from(u32::MAX) {
        return Err(Error::Corrupt("group frontier"));
    }
    Ok(())
}

/// A group's impact frontier, from its directory entry: (bucket, shortest
/// length) pairs ascending in both, every posting of the group dominated
/// by one (a bucket at least as high, a length at most as long). A length
/// rounded when stored is the rounded one, at most the true one.
#[derive(Clone, Debug)]
pub struct GroupFrontier<'b> {
    bytes: &'b [u8],
    at: usize,
    /// Pairs left to give; the head is read with the first.
    left: u8,
    started: bool,
    bucket: u8,
    code: u32,
    rounded: bool,
}

impl<'b> GroupFrontier<'b> {
    /// The frontier at `at` of a directory's bytes, checked when parsed.
    fn new(bytes: &'b [u8], at: usize, rounded: bool) -> Self {
        Self {
            bytes,
            at,
            left: 1,
            started: false,
            bucket: 0,
            code: 0,
            rounded,
        }
    }
}

impl Iterator for GroupFrontier<'_> {
    type Item = (u8, u32);

    #[inline]
    fn next(&mut self) -> Option<(u8, u32)> {
        if self.left == 0 {
            return None;
        }
        if self.started {
            let d = get_checked(self.bytes, &mut self.at);
            self.bucket += 1 + (d & 15) as u8;
            self.code += 1 + (d >> 4) as u32;
        } else {
            let head = self.bytes[self.at];
            self.at += 1;
            self.started = true;
            self.left = (head >> 4) + 1;
            self.bucket = head & 15;
            self.code = get_checked(self.bytes, &mut self.at) as u32;
        }
        self.left -= 1;
        let length = if self.rounded {
            rounded_length(self.code)
        } else {
            self.code
        };
        Some((self.bucket, length))
    }
}

/// A varint of a directory checked when it was parsed.
#[inline]
fn get_checked(bytes: &[u8], at: &mut usize) -> u64 {
    let b = bytes[*at];
    if b < 0x80 {
        *at += 1;
        return u64::from(b);
    }
    varint::get(bytes, at).expect("a frontier checked when parsed")
}

/// A group container an input segment holds that a merge may copy: its
/// kind and bytes (as [`Postings::container`] gives them), and the postings
/// of the input term that holds it.
///
/// A copy is what a fresh encoding writes when the output group has the
/// input's geometry (the same width, first page and span), its members are
/// exactly the input's there (no dead document, no other input), the
/// options are the same, and the two terms fall on the same side of
/// [`Options::grid_min_postings`]; the caller vouches for all but the last,
/// which the encoder checks, re-encoding the group when it fails.
#[derive(Clone, Copy, Debug)]
pub struct Reused<'a> {
    pub kind: u8,
    pub bytes: &'a [u8],
    pub input_df: u32,
}

/// Encodes a term's postings: `slots` strictly increasing, with each
/// posting's bucket and its document's length. Returns where the bytes went.
pub fn encode(
    geometry: &Geometry,
    slots: &[u32],
    buckets: &[u8],
    lengths: &[u32],
    options: &Options,
    out: &mut Vec<u8>,
) -> Stats {
    encode_reusing(geometry, slots, buckets, lengths, options, out, &mut |_| {
        None
    })
}

/// [`encode`], copying the containers `reuse` gives for a group (by its
/// index in `geometry`) where [`Reused`]'s conditions hold: the bytes are
/// those a fresh encoding writes.
pub fn encode_reusing<'r>(
    geometry: &Geometry,
    slots: &[u32],
    buckets: &[u8],
    lengths: &[u32],
    options: &Options,
    out: &mut Vec<u8>,
    reuse: &mut dyn FnMut(usize) -> Option<Reused<'r>>,
) -> Stats {
    assert!(!slots.is_empty() && slots.len() == buckets.len() && slots.len() == lengths.len());
    debug_assert!(slots.windows(2).all(|w| w[0] < w[1]));
    let mut stats = Stats::default();
    let start = out.len();
    let inline = slots.len() as u64 <= u64::from(options.inline_lengths_max_df)
        && geometry.documents >= options.inline_lengths_min_documents;
    if slots.len() == 1 {
        varint::put(out, u64::from(slots[0]));
        stats.payload = out.len() - start;
        if inline {
            let before = out.len();
            varint::put(out, u64::from(lengths[0]));
            stats.lengths = out.len() - before;
        }
        stats.sparse = true;
        return stats;
    }
    let block = options.block_size as usize;
    // Footer.
    let mut footer = Vec::new();
    let mut tf = BitWriter::new();
    let mut tf_bytes = Vec::new();
    let mut previous = 0u32;
    for (b, chunk) in slots.chunks(block).enumerate() {
        let from = b * block;
        let last = *chunk.last().expect("a nonempty chunk");
        varint::put(&mut footer, u64::from(last - previous));
        previous = last;
        let range = from..from + chunk.len();
        let front = frontier(
            buckets[range.clone()]
                .iter()
                .copied()
                .zip(lengths[range.clone()].iter().copied()),
        );
        footer.push(front.len() as u8);
        for (bucket, length) in &front {
            footer.push(*bucket);
            varint::put(&mut footer, u64::from(*length));
        }
        let max = front.last().expect("a nonempty frontier").0;
        let width = tf_width(max, options.adaptive_tf);
        for bucket in &buckets[range] {
            tf.put(u32::from(*bucket), width);
        }
        // Each block's buckets start on a byte.
        tf_bytes.extend_from_slice(&std::mem::take(&mut tf).finish());
        stats.blocks += 1;
    }
    // Payload: the smaller of one list over the grid and per-group
    // containers.
    let mut sparse = Vec::new();
    if options.sparse {
        ef::encode(slots, geometry.slots, &mut sparse);
    }
    let mut grouped_stats = Stats::default();
    let grouped = encode_groups(
        geometry,
        slots,
        buckets,
        lengths,
        options,
        reuse,
        &mut grouped_stats,
    );
    // A term with a group dense enough to be a grid stays grouped. The
    // directory's frontiers do not count: they buy the walks their group
    // bounds, and leaving them out keeps each term's form what it was
    // without them.
    let use_sparse = options.sparse
        && grouped_stats.forced == 0
        && sparse.len() <= grouped.len() - grouped_stats.frontier_bytes;
    let payload = if use_sparse { &sparse } else { &grouped };
    let mut inline_bytes = Vec::new();
    if inline {
        encode_inline_lengths(lengths, &mut inline_bytes);
    }
    let compact = use_sparse
        && inline
        && slots.len() as u64 <= u64::from(COMPACT_MAX)
        && slots.len() <= block;
    out.push(
        if use_sparse {
            FORM_SPARSE
        } else {
            FORM_GROUPED
        } | if inline { FORM_LENGTHS } else { 0 }
            | if compact { FORM_COMPACT } else { 0 }
            | if use_sparse {
                0
            } else {
                frontier_form(options.group_frontiers)
            },
    );
    if compact {
        footer.clear();
    } else {
        varint::put(out, footer.len() as u64);
    }
    varint::put(out, payload.len() as u64);
    if inline {
        varint::put(out, inline_bytes.len() as u64);
    }
    stats.header = out.len() - start;
    out.extend_from_slice(&footer);
    out.extend_from_slice(payload);
    out.extend_from_slice(&inline_bytes);
    stats.lengths = inline_bytes.len();
    out.extend_from_slice(&tf_bytes);
    stats.footer = footer.len();
    stats.payload = payload.len();
    stats.tf = tf_bytes.len();
    stats.sparse = use_sparse;
    if !use_sparse {
        stats.kinds = grouped_stats.kinds;
        stats.kind_bytes = grouped_stats.kind_bytes;
        stats.forced = grouped_stats.forced;
        stats.frontier_bytes = grouped_stats.frontier_bytes;
        stats.frontiers = grouped_stats.frontiers;
    }
    stats
}

/// The form bits of a grouped record's directory frontiers.
const fn frontier_form(mode: GroupFrontiers) -> u8 {
    match mode {
        GroupFrontiers::Off => 0,
        GroupFrontiers::Exact => FORM_FRONTIERS,
        GroupFrontiers::Rounded => FORM_FRONTIERS | FORM_ROUNDED,
    }
}

/// Encodes one group's members, `locals` its slots within the group.
fn encode_paged(locals: &[u32], width: u32, out: &mut Vec<u8>) {
    let ob = bits::width_below(width);
    let mut pages: Vec<(u32, Vec<u32>)> = Vec::new();
    for local in locals {
        let (page, offset) = (local / width, local % width);
        match pages.last_mut() {
            Some((p, offsets)) if *p == page => offsets.push(offset),
            _ => pages.push((page, vec![offset])),
        }
    }
    out.push((pages.len() - 1) as u8);
    if pages.len() < 32 {
        out.extend(pages.iter().map(|(p, _)| *p as u8));
    } else {
        let mut mask = [0u8; 32];
        for (p, _) in &pages {
            mask[*p as usize / 8] |= 1 << (p % 8);
        }
        out.extend_from_slice(&mask);
    }
    for (_, offsets) in &pages {
        let n = offsets.len();
        let list = (n <= LIST_MAX).then(|| bits::packed_len(n, ob));
        let bitmap = (*offsets.last().expect("a member") as usize) / 8 + 1;
        let contiguous = offsets.windows(2).all(|w| w[1] == w[0] + 1);
        let run = contiguous.then(|| bits::packed_len(2, ob));
        let best = [list, Some(bitmap), run]
            .into_iter()
            .flatten()
            .min()
            .expect("a bitmap always fits");
        if run == Some(best) {
            out.push(PAGE_RUN << 6);
            let mut w = BitWriter::new();
            w.put(offsets[0], ob);
            w.put((n - 1) as u32, ob);
            out.extend_from_slice(&w.finish());
        } else if list == Some(best) {
            out.push(PAGE_LIST << 6 | (n - 1) as u8);
            let mut w = BitWriter::new();
            for offset in offsets {
                w.put(*offset, ob);
            }
            out.extend_from_slice(&w.finish());
        } else {
            out.push(PAGE_BITMAP << 6 | (bitmap - 1) as u8);
            let mut bytes = vec![0u8; bitmap];
            for offset in offsets {
                bytes[*offset as usize / 8] |= 1 << (offset % 8);
            }
            out.extend_from_slice(&bytes);
        }
    }
}

fn encode_groups<'r>(
    geometry: &Geometry,
    slots: &[u32],
    buckets: &[u8],
    lengths: &[u32],
    options: &Options,
    reuse: &mut dyn FnMut(usize) -> Option<Reused<'r>>,
    stats: &mut Stats,
) -> Vec<u8> {
    let mut directory = Vec::new();
    let mut bodies = Vec::new();
    let mut groups = 0usize;
    let mut previous: Option<usize> = None;
    let mut at = 0usize;
    let mut index = geometry.group_of_slot(slots[0]);
    let mut locals = Vec::new();
    let mut candidate = Vec::new();
    while at < slots.len() {
        while geometry.groups[index].slot_base + geometry.groups[index].slots() <= slots[at] {
            index += 1;
        }
        let group = geometry.groups[index];
        locals.clear();
        let from = at;
        while at < slots.len() && slots[at] < group.slot_base + group.slots() {
            locals.push(slots[at] - group.slot_base);
            at += 1;
        }
        let width = u32::from(group.width);
        let mut kind = KIND_GRID;
        let mut best = group.grid_bytes();
        let dense = options.grid_density > 0
            && locals.len() as u64 * u64::from(options.grid_density) >= u64::from(group.slots());
        let forced = dense && slots.len() as u64 >= u64::from(options.grid_min_postings);
        if forced {
            stats.forced += 1;
        }
        // An input's container, where its term was forced as this one is.
        let reused = reuse(index).filter(|r| {
            !dense || (u64::from(r.input_df) >= u64::from(options.grid_min_postings)) == forced
        });
        if let Some(r) = reused {
            kind = r.kind;
            best = r.bytes.len();
        }
        if options.ef_groups && !forced && reused.is_none() {
            let len = ef::encoded_len(locals.len(), group.slots());
            if len < best {
                best = len;
                kind = KIND_EF;
            }
        }
        if options.paged && !forced && reused.is_none() {
            candidate.clear();
            encode_paged(&locals, width, &mut candidate);
            if candidate.len() < best {
                best = candidate.len();
                kind = KIND_PAGED;
            }
        }
        let gap = match previous {
            None => index,
            Some(p) => index - p - 1,
        };
        previous = Some(index);
        varint::put(&mut directory, gap as u64);
        varint::put(
            &mut directory,
            ((locals.len() as u64 - 1) << 2) | u64::from(kind),
        );
        let before = bodies.len();
        if let Some(r) = reused {
            if kind == KIND_PAGED {
                varint::put(&mut directory, r.bytes.len() as u64);
            }
            bodies.extend_from_slice(r.bytes);
        } else {
            match kind {
                KIND_GRID => {
                    let mut grid = vec![0u8; group.grid_bytes()];
                    for local in &locals {
                        grid[*local as usize / 8] |= 1 << (local % 8);
                    }
                    bodies.extend_from_slice(&grid);
                }
                KIND_EF => ef::encode(&locals, group.slots(), &mut bodies),
                _ => {
                    varint::put(&mut directory, candidate.len() as u64);
                    bodies.extend_from_slice(&candidate);
                }
            }
        }
        if options.group_frontiers != GroupFrontiers::Off {
            // Computed from the group's postings, a copied container's
            // included: a merge's is the least length per bucket over the
            // inputs' live postings there.
            let pairs = group_frontier(
                buckets[from..at]
                    .iter()
                    .copied()
                    .zip(lengths[from..at].iter().copied()),
                options.group_frontiers,
            );
            let before = directory.len();
            put_group_frontier(&mut directory, &pairs);
            stats.frontier_bytes += directory.len() - before;
            stats.frontiers += 1;
        }
        debug_assert_eq!(bodies.len() - before, best);
        stats.kinds[kind as usize] += 1;
        stats.kind_bytes[kind as usize] += best;
        groups += 1;
    }
    let mut out = Vec::with_capacity(directory.len() + bodies.len() + 4);
    varint::put(&mut out, groups as u64);
    out.extend_from_slice(&directory);
    out.extend_from_slice(&bodies);
    out
}

/// One group of a grouped record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupEntry {
    /// Index of the group in the segment's geometry.
    pub index: u32,
    pub count: u32,
    pub kind: u8,
    /// Where the container starts within the payload, and its length.
    pub at: u32,
    pub len: u32,
    /// Postings in earlier groups.
    pub first: u32,
    /// Where the group's frontier lies in [`Postings::frontiers`]
    /// ([`NO_FRONTIER`] in a record without them).
    pub frontier: u32,
}

/// [`GroupEntry::frontier`] of a record whose directory holds none.
pub const NO_FRONTIER: u32 = u32::MAX;

/// A term's membership.
#[derive(Clone, Debug)]
pub enum Form<'a> {
    Single(u32),
    Sparse(Ef<'a>),
    /// The group directory, shared by the clones of a parsed record.
    Grouped(std::rc::Rc<[GroupEntry]>),
}

/// A term's postings record, parsed.
#[derive(Clone, Debug)]
pub struct Postings<'a> {
    pub df: u32,
    pub form: Form<'a>,
    /// The footer's bytes, and where they start in the record: read when
    /// the footer is decoded ([`Self::footer`]), which a reader keeps.
    pub footer: Bytes<'a>,
    pub footer_at: usize,
    /// The payload's bytes (an ef list or the grouped directory and
    /// containers), and where they start in the record. Loaded on demand
    /// for a record over a lazy blob: a grouped record's containers are
    /// read a group at a time ([`Self::container`]).
    pub payload: Bytes<'a>,
    pub payload_at: usize,
    /// Where the grouped containers start within the payload.
    pub containers_at: usize,
    /// The TF tail, read a posting's bits at a time ([`Footer::bucket`]).
    pub tf: Bytes<'a>,
    pub tf_at: usize,
    /// The documents' lengths, for a term of few postings.
    pub lengths: Option<InlineLengths<'a>>,
    /// Whether the record has no footer ([`FORM_COMPACT`]).
    pub compact: bool,
    /// A grouped record's directory bytes when its entries carry their
    /// groups' frontiers ([`FORM_FRONTIERS`]), copied once when parsed so
    /// the parsed record borrows nothing; whether their lengths are rounded.
    pub frontiers: Option<std::rc::Rc<[u8]>>,
    pub frontiers_rounded: bool,
}

/// A rare term's documents' lengths, in posting order: a base, a width
/// and the lengths less the base packed at it.
///
/// ```text
/// lengths := base varint, width u8, (length - base) packed at width
/// ```
#[derive(Clone, Copy, Debug)]
pub struct InlineLengths<'a> {
    base: u32,
    width: u32,
    packed: &'a [u8],
    /// Where they start in the record.
    pub at: usize,
}

impl<'a> InlineLengths<'a> {
    fn parse(bytes: &'a [u8], at: usize, df: u32) -> Result<Self> {
        let mut p = 0;
        let base = varint::get_u32(bytes, &mut p)?;
        let width = u32::from(*bytes.get(p).ok_or(Error::Truncated)?);
        p += 1;
        if width > 32 || bytes.len() - p != bits::packed_len(df as usize, width) {
            return Err(Error::Corrupt("inline lengths"));
        }
        Ok(Self {
            base,
            width,
            packed: &bytes[p..],
            at,
        })
    }

    /// The length of posting `index`'s document.
    #[inline]
    pub fn get(&self, index: u32) -> Result<u32> {
        Ok(self.base + bits::get(self.packed, index as usize, self.width)?)
    }

    /// Where posting `index`'s length lies in the record.
    pub fn at_of(&self, index: u32) -> usize {
        self.at + (index as usize * self.width as usize) / 8
    }
}

fn encode_inline_lengths(lengths: &[u32], out: &mut Vec<u8>) {
    let base = lengths.iter().copied().min().unwrap_or(0);
    let range = lengths.iter().map(|l| l - base).max().unwrap_or(0);
    let width = 32 - range.leading_zeros();
    varint::put(out, u64::from(base));
    out.push(width as u8);
    let mut w = BitWriter::new();
    for l in lengths {
        w.put(l - base, width);
    }
    out.extend_from_slice(&w.finish());
}

impl<'a> Postings<'a> {
    /// Whether the parsed record holds no slice of the blob, only ranges
    /// it reads when asked ([`Bytes::is_lazy`]): a grouped record without
    /// inline lengths. Such a record may be kept beyond the span it was
    /// parsed in (see [`super::blob::LazyBlob::close_span`]).
    pub fn is_detached(&self) -> bool {
        matches!(self.form, Form::Grouped(_))
            && self.lengths.is_none()
            && self.footer.is_lazy()
            && self.payload.is_lazy()
            && self.tf.is_lazy()
    }

    /// The ranges of the record (offsets from its start) the parsed record
    /// keeps slices of: a single posting's record, a sparse term's list,
    /// inline lengths. None for a detached record ([`Self::is_detached`]).
    pub fn borrowed(&self) -> [Option<(usize, usize)>; 2] {
        let lengths = self.lengths.as_ref().map(|l| (l.at, self.tf_at));
        match self.form {
            Form::Single(_) => [Some((0, self.tf_at)), None],
            Form::Sparse(_) => [
                Some((self.payload_at, self.payload_at + self.payload.len())),
                lengths,
            ],
            Form::Grouped(_) => [lengths, None],
        }
    }

    /// Parses a record of a term with `df` postings in `geometry`: its
    /// header, footer, inline lengths and group directory (or sparse list)
    /// are read, its containers and TF tail only when asked for.
    pub fn parse(record: impl Into<Bytes<'a>>, df: u32, geometry: &Geometry) -> Result<Self> {
        let bytes = record.into().tag(Kind::Record);
        if df == 0 {
            return Err(Error::Corrupt("a term without postings"));
        }
        if df == 1 {
            let record = bytes.tag(Kind::Sparse).all()?;
            let mut at = 0;
            let slot = varint::get_u32(record, &mut at)?;
            let lengths = if at < record.len() {
                let from = at;
                let length = varint::get_u32(record, &mut at)?;
                Some(InlineLengths {
                    base: length,
                    width: 0,
                    packed: &[],
                    at: from,
                })
            } else {
                None
            };
            if slot >= geometry.slots || at != record.len() {
                return Err(Error::Corrupt("single posting"));
            }
            return Ok(Self {
                compact: false,
                frontiers: None,
                frontiers_rounded: false,
                lengths,
                df,
                form: Form::Single(slot),
                footer: Bytes::default(),
                footer_at: 0,
                payload: Bytes::Slice(&record[..at]),
                payload_at: 0,
                containers_at: 0,
                tf: Bytes::default(),
                tf_at: at,
            });
        }
        // The form and three varints.
        let record = bytes.window(0, 1 + 3 * 10)?;
        let form = *record.first().ok_or(Error::Truncated)?;
        let mut at = 1;
        let footer_len = if form & FORM_COMPACT != 0 {
            if form & FORM_LENGTHS == 0 || form & 3 != FORM_SPARSE || df > COMPACT_MAX {
                return Err(Error::Corrupt("compact postings"));
            }
            0
        } else {
            varint::get_u32(record, &mut at)? as usize
        };
        let payload_len = varint::get_u32(record, &mut at)? as usize;
        let lengths_len = if form & FORM_LENGTHS != 0 {
            varint::get_u32(record, &mut at)? as usize
        } else {
            0
        };
        let compact = form & FORM_COMPACT != 0;
        let with_frontiers = form & FORM_FRONTIERS != 0;
        let frontiers_rounded = form & FORM_ROUNDED != 0;
        if (with_frontiers && form & 3 != FORM_GROUPED) || (frontiers_rounded && !with_frontiers) {
            return Err(Error::Corrupt("postings frontiers"));
        }
        let form = form & !(FORM_LENGTHS | FORM_COMPACT | FORM_FRONTIERS | FORM_ROUNDED);
        let footer_at = at;
        let payload_at = footer_at + footer_len;
        let lengths_at = payload_at + payload_len;
        let tf_at = lengths_at + lengths_len;
        if tf_at > bytes.len() {
            return Err(Error::Truncated);
        }
        let footer = bytes.sub(footer_at, payload_at)?.tag(Kind::Footer);
        let payload = bytes.sub(payload_at, lengths_at)?;
        let lengths = if lengths_len > 0 {
            Some(InlineLengths::parse(
                bytes.tag(Kind::InlineLengths).get(lengths_at, tf_at)?,
                lengths_at,
                df,
            )?)
        } else {
            None
        };
        let mut containers_at = 0;
        let mut frontiers = None;
        let form = match form {
            FORM_SPARSE => {
                if payload.len() != ef::encoded_len(df as usize, geometry.slots) {
                    return Err(Error::Corrupt("sparse postings length"));
                }
                Form::Sparse(Ef::parse(
                    payload.tag(Kind::Sparse).all()?,
                    df as usize,
                    geometry.slots,
                )?)
            }
            FORM_GROUPED => {
                // The directory's length is known only once it is parsed:
                // read a prefix of the payload, at least as long as its
                // group count says it must be, longer until it holds it.
                let mut want = DIRECTORY_PREFIX.min(payload.len());
                let typical = if with_frontiers {
                    DIRECTORY_ENTRY_TYPICAL + FRONTIER_TYPICAL
                } else {
                    DIRECTORY_ENTRY_TYPICAL
                };
                let (entries, at) = loop {
                    let directory = payload.window(0, want)?;
                    let mut at = 0;
                    let groups = (varint::get(directory, &mut at).unwrap_or(0) as usize)
                        .min(geometry.groups.len());
                    let least = at.saturating_add(groups.saturating_mul(typical));
                    if least > want && want < payload.len() {
                        want = least.min(payload.len());
                        continue;
                    }
                    match parse_groups(directory, payload.len(), df, geometry, with_frontiers) {
                        Err(Error::Truncated) if want < payload.len() => {
                            want = want.saturating_mul(2).min(payload.len());
                        }
                        Ok((entries, at)) => {
                            if with_frontiers {
                                frontiers = Some(std::rc::Rc::from(&directory[..at]));
                            }
                            break (entries, at);
                        }
                        Err(error) => return Err(error),
                    }
                };
                containers_at = at;
                Form::Grouped(entries)
            }
            _ => return Err(Error::Corrupt("postings form")),
        };
        Ok(Self {
            df,
            form,
            footer,
            footer_at,
            payload,
            payload_at,
            containers_at,
            tf: bytes.sub(tf_at, bytes.len())?.tag(Kind::Tf),
            tf_at,
            lengths,
            compact,
            frontiers,
            frontiers_rounded,
        })
    }

    /// The impact frontier the directory holds for a group, if it holds
    /// frontiers ([`FORM_FRONTIERS`]).
    #[inline]
    pub fn group_frontier(&self, entry: &GroupEntry) -> Option<GroupFrontier<'_>> {
        let bytes = self.frontiers.as_deref()?;
        (entry.frontier != NO_FRONTIER).then(|| {
            GroupFrontier::new(bytes, entry.frontier as usize, self.frontiers_rounded)
        })
    }

    /// The container of a grouped record's entry, not read yet (see
    /// [`Bytes::all`]). The directory was checked to lie within the payload.
    pub fn container(&self, entry: &GroupEntry) -> Bytes<'a> {
        let at = self.containers_at + entry.at as usize;
        self.payload
            .sub(at, at + entry.len as usize)
            .expect("a container within its payload")
            .tag(Kind::Container)
    }

    /// Calls `visit` with every slot, in order.
    pub fn for_each_slot(&self, geometry: &Geometry, mut visit: impl FnMut(u32)) -> Result<()> {
        match &self.form {
            Form::Single(slot) => visit(*slot),
            Form::Sparse(list) => list.for_each(visit),
            Form::Grouped(entries) => {
                for entry in entries.iter() {
                    let group = &geometry.groups[entry.index as usize];
                    let base = group.slot_base;
                    for_each_local(entry, self.container(entry).all()?, group, |local| {
                        visit(base + local);
                    })?;
                }
            }
        }
        Ok(())
    }

    /// Every slot, in order.
    pub fn slots(&self, geometry: &Geometry) -> Result<Vec<u32>> {
        let mut out = Vec::with_capacity(self.df as usize);
        self.for_each_slot(geometry, |slot| out.push(slot))?;
        if out.len() != self.df as usize {
            return Err(Error::Corrupt("postings count"));
        }
        Ok(out)
    }

    /// The footer, decoded.
    pub fn footer(&self, block_size: u32, max_bucket: u8, adaptive_tf: bool) -> Result<Footer> {
        Footer::parse(self, block_size, max_bucket, adaptive_tf)
    }

    /// The footer, to be decoded a block at a time as a reader reaches its
    /// blocks ([`LazyFooter`]); its bytes are read now.
    pub fn lazy_footer(
        &self,
        block_size: u32,
        max_bucket: u8,
        adaptive_tf: bool,
    ) -> Result<LazyFooter<'a>> {
        LazyFooter::new(self, block_size, max_bucket, adaptive_tf)
    }
}

/// Bytes of a grouped payload read first for its directory: a few groups'
/// entries take a few bytes each.
const DIRECTORY_PREFIX: usize = 4096;

/// Bytes a directory entry takes as a rule (a one-byte gap, a count and
/// kind of one or two bytes, a paged container's length): a directory of
/// more groups than a prefix holds is read in one window this long per
/// group, rather than parsed from the start again in windows four times
/// longer each.
const DIRECTORY_ENTRY_TYPICAL: usize = 3;

/// ... and a group frontier, as a rule (a head, a length, a later pair).
const FRONTIER_TYPICAL: usize = 3;

/// A grouped payload's directory, from a prefix of the payload holding it
/// (`Truncated` when it does not), `payload_len` the whole payload's length.
fn parse_groups(
    payload: &[u8],
    payload_len: usize,
    df: u32,
    geometry: &Geometry,
    frontiers: bool,
) -> Result<(std::rc::Rc<[GroupEntry]>, usize)> {
    let mut at = 0;
    let groups = varint::get_u32(payload, &mut at)? as usize;
    if groups > geometry.groups.len() {
        return Err(Error::Corrupt("postings groups"));
    }
    // Filled in place: the directory is kept shared, and a vector copied
    // into it allocated and copied it twice per parse.
    let mut entries = std::rc::Rc::<[GroupEntry]>::new_uninit_slice(groups);
    let slots = std::rc::Rc::get_mut(&mut entries).expect("just allocated");
    let mut index: Option<u64> = None;
    let mut body = 0u64;
    let mut first = 0u64;
    for slot in slots.iter_mut() {
        let gap = varint::get(payload, &mut at)?;
        let next = match index {
            None => gap,
            Some(i) => i + 1 + gap,
        };
        if next >= geometry.groups.len() as u64 {
            return Err(Error::Corrupt("postings group index"));
        }
        index = Some(next);
        let group = geometry.groups[next as usize];
        let packed = varint::get(payload, &mut at)?;
        let kind = (packed & 3) as u8;
        let count = (packed >> 2) + 1;
        if count > u64::from(group.slots()) {
            return Err(Error::Corrupt("postings group count"));
        }
        let len = match kind {
            KIND_GRID => group.grid_bytes() as u64,
            KIND_EF => ef::encoded_len(count as usize, group.slots()) as u64,
            KIND_PAGED => varint::get(payload, &mut at)?,
            _ => return Err(Error::Corrupt("postings group kind")),
        };
        let frontier = if frontiers {
            let from = at as u32;
            skip_group_frontier(payload, &mut at)?;
            from
        } else {
            NO_FRONTIER
        };
        slot.write(GroupEntry {
            index: next as u32,
            count: count as u32,
            kind,
            at: body as u32,
            len: len as u32,
            first: first as u32,
            frontier,
        });
        body += len;
        first += count;
    }
    if first != u64::from(df) || at as u64 + body != payload_len as u64 {
        return Err(Error::Corrupt("postings payload length"));
    }
    super::segment::count_memo(|c| {
        c.directories += 1;
        c.directory_entries += groups as u64;
        c.directory_bytes += at as u64;
    });
    // SAFETY: the loop wrote every one of the `groups` entries (an error
    // returns before this, dropping the slice as uninitialized memory,
    // which `GroupEntry`, plain data, permits).
    Ok((unsafe { entries.assume_init() }, at))
}

/// Calls `visit` with every member of a group container, as local slots in
/// order.
#[inline]
pub fn for_each_local(
    entry: &GroupEntry,
    bytes: &[u8],
    group: &Group,
    mut visit: impl FnMut(u32),
) -> Result<()> {
    match entry.kind {
        KIND_GRID => {
            let words = bytes.len().div_ceil(8);
            for w in 0..words {
                let mut word = bits::word(bytes, w);
                while word != 0 {
                    visit((w * 64) as u32 + word.trailing_zeros());
                    word &= word - 1;
                }
            }
            Ok(())
        }
        KIND_EF => {
            Ef::parse(bytes, entry.count as usize, group.slots())?.for_each(visit);
            Ok(())
        }
        _ => paged_for_each(bytes, u32::from(group.width), visit),
    }
}

/// Sets every member of a group container in `words`, a bitmap over the
/// group's slots.
#[inline]
pub fn or_into(entry: &GroupEntry, bytes: &[u8], group: &Group, words: &mut [u64]) -> Result<()> {
    if entry.kind == KIND_GRID {
        for (i, word) in words.iter_mut().enumerate() {
            *word |= bits::word(bytes, i);
        }
        return Ok(());
    }
    for_each_local(entry, bytes, group, |local| {
        words[local as usize / 64] |= 1 << (local % 64);
    })
}

fn paged_for_each(bytes: &[u8], width: u32, mut visit: impl FnMut(u32)) -> Result<()> {
    let ob = bits::width_below(width);
    let pages = *bytes.first().ok_or(Error::Truncated)? as usize + 1;
    let mut at = 1;
    let mut list = [0u8; 256];
    let page_list: &[u8] = if pages < 32 {
        let l = bytes.get(at..at + pages).ok_or(Error::Truncated)?;
        at += pages;
        l
    } else {
        let mask = bytes.get(at..at + 32).ok_or(Error::Truncated)?;
        at += 32;
        let mut n = 0;
        for p in 0..256 {
            if mask[p / 8] >> (p % 8) & 1 == 1 {
                list[n] = p as u8;
                n += 1;
            }
        }
        if n != pages {
            return Err(Error::Corrupt("paged mask count"));
        }
        &list[..n]
    };
    let page_list: Vec<u8> = page_list.to_vec();
    for page in page_list {
        let base = u32::from(page) * width;
        let head = *bytes.get(at).ok_or(Error::Truncated)?;
        at += 1;
        let value = (head & 63) as usize;
        match head >> 6 {
            PAGE_LIST => {
                let n = value + 1;
                let len = bits::packed_len(n, ob);
                let body = bytes.get(at..at + len).ok_or(Error::Truncated)?;
                at += len;
                for i in 0..n {
                    let offset = bits::get(body, i, ob)?;
                    if offset >= width {
                        return Err(Error::Corrupt("paged offset"));
                    }
                    visit(base + offset);
                }
            }
            PAGE_BITMAP => {
                let len = value + 1;
                let body = bytes.get(at..at + len).ok_or(Error::Truncated)?;
                at += len;
                for (i, byte) in body.iter().enumerate() {
                    let mut byte = *byte;
                    while byte != 0 {
                        let offset = (i * 8) as u32 + byte.trailing_zeros();
                        if offset >= width {
                            return Err(Error::Corrupt("paged offset"));
                        }
                        visit(base + offset);
                        byte &= byte - 1;
                    }
                }
            }
            PAGE_RUN => {
                let len = bits::packed_len(2, ob);
                let body = bytes.get(at..at + len).ok_or(Error::Truncated)?;
                at += len;
                let first = bits::get(body, 0, ob)?;
                let n = bits::get(body, 1, ob)? + 1;
                if first + n > width {
                    return Err(Error::Corrupt("paged run"));
                }
                for offset in first..first + n {
                    visit(base + offset);
                }
            }
            _ => return Err(Error::Corrupt("paged page kind")),
        }
    }
    if at != bytes.len() {
        return Err(Error::Corrupt("paged length"));
    }
    Ok(())
}

/// A record's footer decoded: per block, its last slot, its frontier and
/// where its buckets start in the TF tail.
#[derive(Clone, Debug, Default)]
pub struct Footer {
    pub block_size: u32,
    pub last: Vec<u32>,
    /// Block `b`'s frontier is `frontier[starts[b]..starts[b + 1]]`.
    pub starts: Vec<u32>,
    pub frontier: Vec<(u8, u32)>,
    /// Block `b`'s buckets start at byte `tf_at[b]` of the TF tail, packed
    /// at `widths[b]` bits.
    pub tf_at: Vec<u32>,
    pub widths: Vec<u8>,
    /// Where each block's footer entry starts within the footer, for page
    /// accounting.
    pub entry_at: Vec<u32>,
    /// A single posting's bucket, which the dictionary keeps.
    pub single: Option<u8>,
}

impl Footer {
    fn parse(
        postings: &Postings<'_>,
        block_size: u32,
        max_bucket: u8,
        adaptive: bool,
    ) -> Result<Self> {
        let df = postings.df;
        if postings.compact
            && let (Form::Sparse(list), Some(lengths)) = (&postings.form, &postings.lengths)
        {
            if df > block_size {
                return Err(Error::Corrupt("compact postings"));
            }
            let mut last = 0;
            list.for_each(|v| last = v);
            let width = tf_width(max_bucket, adaptive);
            let tf = postings.tf.all()?;
            let mut pairs = Vec::with_capacity(df as usize);
            for i in 0..df as usize {
                let bucket = bits::get(tf, i, width)? as u8;
                pairs.push((bucket, lengths.get(i as u32)?));
            }
            let frontier = frontier(pairs.into_iter());
            if frontier.last().map(|f| f.0) != Some(max_bucket)
                || postings.tf.len() != bits::packed_len(df as usize, width)
            {
                return Err(Error::Corrupt("compact postings"));
            }
            return Ok(Self {
                block_size,
                last: vec![last],
                starts: vec![0, frontier.len() as u32],
                frontier,
                tf_at: vec![0],
                widths: vec![width as u8],
                entry_at: vec![0],
                single: None,
            });
        }
        if let Form::Single(slot) = postings.form {
            return Ok(Self {
                block_size,
                last: vec![slot],
                starts: vec![0, 1],
                frontier: vec![(max_bucket, 0)],
                tf_at: vec![0],
                widths: vec![0],
                entry_at: vec![0],
                single: Some(max_bucket),
            });
        }
        let blocks = df.div_ceil(block_size) as usize;
        let bytes = postings.footer.all()?;
        // A block's entry takes at least two bytes (its last slot and its
        // frontier's length), a frontier pair at least two (bucket, length):
        // room for that many pairs, so the vector is not grown and copied
        // as it fills (a common word's footer holds tens of thousands).
        let pairs = (bytes.len().saturating_sub(2 * blocks) / 2).min(blocks * 16);
        let mut footer = Self {
            block_size,
            last: Vec::with_capacity(blocks),
            starts: Vec::with_capacity(blocks + 1),
            frontier: Vec::with_capacity(pairs),
            tf_at: Vec::with_capacity(blocks),
            widths: Vec::with_capacity(blocks),
            entry_at: Vec::with_capacity(blocks),
            single: None,
        };
        let mut at = 0usize;
        let mut last = 0u64;
        let mut tf = 0u64;
        for b in 0..blocks {
            footer.entry_at.push(at as u32);
            last += varint::get(bytes, &mut at)?;
            footer
                .last
                .push(u32::try_from(last).map_err(|_| Error::Corrupt("footer slot"))?);
            let k = *bytes.get(at).ok_or(Error::Truncated)? as usize;
            at += 1;
            if k == 0 || k > 16 {
                return Err(Error::Corrupt("footer frontier"));
            }
            footer.starts.push(footer.frontier.len() as u32);
            let mut max = 0u8;
            for _ in 0..k {
                let bucket = *bytes.get(at).ok_or(Error::Truncated)?;
                at += 1;
                if bucket > 15 {
                    return Err(Error::InvalidTfBucket);
                }
                let length = varint::get_u32(bytes, &mut at)?;
                footer.frontier.push((bucket, length));
                max = max.max(bucket);
            }
            let width = tf_width(max, adaptive);
            let n = if b + 1 == blocks {
                df - b as u32 * block_size
            } else {
                block_size
            };
            footer.tf_at.push(tf as u32);
            footer.widths.push(width as u8);
            tf += bits::packed_len(n as usize, width) as u64;
        }
        footer.starts.push(footer.frontier.len() as u32);
        super::segment::count_memo(|c| {
            c.footer_blocks += blocks as u64;
            c.footer_bytes += at as u64;
        });
        if at != bytes.len() || tf != postings.tf.len() as u64 {
            return Err(Error::Corrupt("footer length"));
        }
        Ok(footer)
    }

    pub fn blocks(&self) -> usize {
        self.last.len()
    }

    pub fn frontier_of(&self, block: usize) -> &[(u8, u32)] {
        &self.frontier[self.starts[block] as usize..self.starts[block + 1] as usize]
    }

    /// The bucket of posting `index`, from its record's TF tail.
    #[inline]
    pub fn bucket(&self, tf: Bytes<'_>, index: u32) -> Result<u8> {
        if let Some(bucket) = self.single {
            return Ok(bucket);
        }
        let block = (index / self.block_size) as usize;
        let within = (index % self.block_size) as usize;
        let width = u32::from(
            *self
                .widths
                .get(block)
                .ok_or(Error::Corrupt("posting index"))?,
        );
        if width == 0 {
            return Ok(0);
        }
        let bit = within * width as usize;
        let bytes = tf.window(self.tf_at[block] as usize + bit / 8, 8)?;
        Ok(bits::get_at(bytes, bit % 8, width)? as u8)
    }
}

/// A record's footer decoded a block at a time, as far as a reader has
/// reached: its blocks' entries are variable-length and in a row, so
/// reaching block `b` decodes every entry before it, and nothing after it.
/// The decoded blocks are structures of arrays, as [`Footer`]'s: per block
/// its last slot (so a run of blocks' last slots is a run of words), where
/// its frontier ends and where its buckets start in the TF tail.
///
/// The bytes are read as far as the blocks decoded, a page at a time and
/// in place over a blob read within a span ([`Bytes::page_end`]): an entry
/// across a page boundary is stitched alone. Nothing is kept beyond the
/// reader: a walk decodes per query what it reaches. The window read in
/// place is a slice of its page, which a walk releasing pages as it goes
/// must hold while the footer has bytes on it to decode
/// ([`LazyFooter::borrowed`]); a page holding only footer bytes is
/// released once the footer has read past it
/// ([`LazyFooter::release_passed`]).
#[derive(Clone, Debug)]
pub struct LazyFooter<'a> {
    block_size: u32,
    /// The record's postings and its blocks.
    df: u32,
    blocks: usize,
    adaptive: bool,
    /// The footer's bytes, and the window of them read last and where it
    /// starts in the footer.
    src: Bytes<'a>,
    window: &'a [u8],
    window_at: usize,
    /// Whether a window was read.
    started: bool,
    /// Where the next block's entry starts in the footer.
    at: usize,
    /// The last slot and TF offset after the blocks decoded.
    next_last: u64,
    next_tf: u64,
    /// The TF tail's length, checked once every block is decoded.
    tf_len: u64,
    /// Per decoded block: its last slot; where its frontier ends in
    /// `frontier` (it starts where the block before it ends); where its
    /// buckets start in the TF tail, and their width.
    last: Vec<u32>,
    ends: Vec<u32>,
    frontier: Vec<(u8, u32)>,
    tf_at: Vec<u32>,
    widths: Vec<u8>,
    /// A single posting's bucket, which the dictionary keeps.
    single: Option<u8>,
    /// Whether pages the footer's reads pinned for bytes only it reads are
    /// released once it has read past them ([`Self::release_passed`]), and
    /// those pages, still pinned.
    release: bool,
    owned: Vec<usize>,
}

/// Most bytes a footer block's entry takes: a slot gap, the frontier's
/// length and sixteen pairs.
const ENTRY_MAX: usize = 10 + 1 + 16 * (1 + 5);

/// Bytes read first across a page boundary for the entry there: as a rule
/// it is a few bytes, and the rest of the window is stitched for nothing.
const BRIDGE: usize = 16;

/// A footer block's entry at the start of `bytes`, its pairs appended to
/// `pairs`: the gap to its last slot, and its length.
fn footer_entry(bytes: &[u8], pairs: &mut Vec<(u8, u32)>) -> Result<(u64, usize)> {
    let from = pairs.len();
    let entry = (|| {
        let mut at = 0;
        let gap = varint::get(bytes, &mut at)?;
        let k = *bytes.get(at).ok_or(Error::Truncated)? as usize;
        at += 1;
        if k == 0 || k > 16 {
            return Err(Error::Corrupt("footer frontier"));
        }
        for _ in 0..k {
            let bucket = *bytes.get(at).ok_or(Error::Truncated)?;
            at += 1;
            if bucket > 15 {
                return Err(Error::InvalidTfBucket);
            }
            pairs.push((bucket, varint::get_u32(bytes, &mut at)?));
        }
        Ok((gap, at))
    })();
    if entry.is_err() {
        pairs.truncate(from);
    }
    entry
}

/// [`footer_entry`] of an entry that starts at least [`ENTRY_MAX`] bytes
/// before the end of `bytes`, so it lies wholly within them: `None`, with
/// nothing appended, for anything a corrupt entry could hold (decoded again
/// by [`footer_entry`] for its error) or a slot gap longer than five bytes,
/// which no `u32` slot needs.
#[inline(always)]
fn footer_entry_fast(bytes: &[u8], pairs: &mut Vec<(u8, u32)>) -> Option<(u64, usize)> {
    let b: &[u8; ENTRY_MAX] = bytes.get(..ENTRY_MAX)?.try_into().ok()?;
    let mut at = 0;
    let mut gap = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = b[at];
        at += 1;
        gap |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            break;
        }
        shift += 7;
        if shift >= 35 {
            return None;
        }
    }
    let k = usize::from(b[at]);
    at += 1;
    if k.wrapping_sub(1) >= 16 {
        return None;
    }
    let from = pairs.len();
    let mut any = 0u8;
    for _ in 0..k {
        let bucket = b[at];
        any |= bucket;
        let (b0, b1) = (b[at + 1], b[at + 2]);
        // A length: one or two bytes as a rule, five at most.
        let length = if b0 < 0x80 {
            at += 2;
            u32::from(b0)
        } else if b1 < 0x80 {
            at += 3;
            u32::from(b0 & 0x7f) | u32::from(b1) << 7
        } else {
            at += 1;
            let mut value = 0u64;
            let mut n = 0;
            loop {
                let byte = b[at + n];
                value |= u64::from(byte & 0x7f) << (7 * n);
                n += 1;
                if byte < 0x80 {
                    break;
                }
                if n == 5 {
                    pairs.truncate(from);
                    return None;
                }
            }
            at += n;
            match u32::try_from(value) {
                Ok(v) => v,
                Err(_) => {
                    pairs.truncate(from);
                    return None;
                }
            }
        };
        pairs.push((bucket, length));
    }
    if any > 15 {
        pairs.truncate(from);
        return None;
    }
    Some((gap, at))
}

impl<'a> LazyFooter<'a> {
    fn new(
        postings: &Postings<'a>,
        block_size: u32,
        max_bucket: u8,
        adaptive: bool,
    ) -> Result<Self> {
        let df = postings.df;
        let blocks = df.div_ceil(block_size.max(1)) as usize;
        let mut footer = Self {
            block_size,
            df,
            blocks,
            adaptive,
            src: postings.footer,
            window: &[],
            window_at: 0,
            started: false,
            at: 0,
            next_last: 0,
            next_tf: 0,
            tf_len: postings.tf.len() as u64,
            last: Vec::new(),
            ends: Vec::new(),
            frontier: Vec::new(),
            tf_at: Vec::new(),
            widths: Vec::new(),
            single: None,
            release: false,
            owned: Vec::new(),
        };
        if matches!(postings.form, Form::Single(_)) || postings.compact {
            // Derived from the record (a block at most): decoded whole as
            // ever.
            let whole = Footer::parse(postings, block_size, max_bucket, adaptive)?;
            footer.blocks = whole.blocks();
            footer.last = whole.last;
            footer.ends = whole.starts[1..].to_vec();
            footer.frontier = whole.frontier;
            footer.tf_at = whole.tf_at;
            footer.widths = whole.widths;
            footer.single = whole.single;
            footer.src = Bytes::default();
        }
        Ok(footer)
    }

    /// Reads a window from the next entry on: to the end of its page, or,
    /// when the window read last reaches that already (the entry crosses
    /// the page boundary), `want` bytes past it, stitched.
    #[cold]
    #[inline(never)]
    fn read_window(&mut self, want: usize) -> Result<()> {
        let (from, read, len) = (self.at, self.read(), self.src.len());
        if read >= len && self.started {
            return Err(Error::Truncated);
        }
        let end = self.src.page_end(from);
        let to = if end > read || !self.started {
            end
        } else {
            (read.max(from) + want).min(len)
        };
        let blob = self.src.blob().filter(|_| self.release);
        let mark = blob.map_or(0, |b| b.pin_mark());
        self.window = self.src.get(from, to)?;
        self.window_at = from;
        if let Some(blob) = blob {
            self.pass(blob, mark);
        }
        if !self.started {
            self.started = true;
            // Room for every block, so the vectors are not grown and copied
            // as they fill (a common word's footer holds tens of thousands);
            // what is never decoded is never touched. A frontier pair takes
            // two bytes or more.
            let blocks = self.blocks;
            self.last.reserve_exact(blocks);
            self.ends.reserve_exact(blocks);
            self.frontier
                .reserve_exact((self.src.len().saturating_sub(2 * blocks) / 2).min(blocks * 16));
            self.tf_at.reserve_exact(blocks);
            self.widths.reserve_exact(blocks);
        }
        Ok(())
    }

    /// Has the footer release each page its reads pinned that holds
    /// nothing but its bytes once it has read past it, rather than leave
    /// it to the walk's next [`super::blob::LazyBlob::release_since`]: a
    /// walk whose first bound lies far into a common term's footer reads
    /// hundreds of its pages within one group. For a walk that releases
    /// pages as it goes (nothing else reads a footer's pages).
    pub fn release_passed(&mut self) {
        self.release = true;
    }

    /// After a read: notes the pages it pinned (since `mark`) that hold
    /// footer bytes only, and releases those noted before the window.
    #[inline(never)]
    fn pass(&mut self, blob: &super::blob::LazyBlob, mark: usize) {
        let (n, Some(base)) = (blob.page_len(), self.src.offset()) else {
            return;
        };
        if n == 0 {
            return;
        }
        let end = base + self.src.len();
        let mut i = mark;
        while let Some(page) = blob.pinned_page(i) {
            if page * n >= base && (page + 1) * n <= end {
                self.owned.push(page);
            }
            i += 1;
        }
        let window = base + self.window_at;
        let mut k = 0;
        while k < self.owned.len() {
            let page = self.owned[k];
            if (page + 1) * n <= window {
                // SAFETY: the page holds only footer bytes, which only this
                // footer reads, and the window, its one slice of a page,
                // starts past it.
                unsafe { blob.release_pinned(page) };
                self.owned.swap_remove(k);
            } else {
                k += 1;
            }
        }
    }

    /// The record's blocks.
    #[inline]
    pub fn blocks(&self) -> usize {
        self.blocks
    }

    /// Blocks decoded so far.
    #[inline]
    pub fn decoded(&self) -> usize {
        self.last.len()
    }

    /// Footer bytes parsed so far.
    pub fn parsed(&self) -> usize {
        self.at
    }

    /// Footer bytes read so far.
    pub fn read(&self) -> usize {
        self.window_at + self.window.len()
    }

    /// The bytes of the blob the footer keeps a slice of and has yet to
    /// decode, as `(from, to)` offsets of the blob: those of the window
    /// read last past the entries decoded, when the window was read in
    /// place (one page) from a lazy blob. Decoding reads on from that
    /// slice, so its page must stay pinned until the footer is past it
    /// ([`super::blob::LazyBlob::release_since`]'s `held`); a stitched
    /// window lives until its span closes.
    #[inline]
    pub fn borrowed(&self) -> Option<(usize, usize)> {
        let (from, to) = (self.at, self.read());
        if from >= to || to > self.src.page_end(self.window_at) {
            return None;
        }
        let base = self.src.offset()?;
        Some((base + from, base + to))
    }

    #[inline]
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// A single posting's bucket, which the dictionary keeps.
    #[inline]
    pub fn single(&self) -> Option<u8> {
        self.single
    }

    /// Decodes blocks until block `from` or a later one ends at `slot` or
    /// later, or every block is: those whose entries lie well within the
    /// window read in one loop, the others (and the last block, whose
    /// lengths are checked) one at a time.
    #[inline(never)]
    fn decode_until(&mut self, from: usize, slot: u32) -> Result<()> {
        loop {
            let (window, window_at) = (self.window, self.window_at);
            let (mut at, mut next_last, mut next_tf) = (self.at, self.next_last, self.next_tf);
            let mut b = self.last.len();
            let full = bits::packed_len(self.block_size as usize, 1) as u64;
            let mut found = false;
            while b + 1 < self.blocks {
                let Some((gap, len)) = window
                    .get(at - window_at..)
                    .and_then(|bytes| footer_entry_fast(bytes, &mut self.frontier))
                else {
                    break;
                };
                let end = self.frontier.len();
                let Ok(last) = u32::try_from(next_last + gap) else {
                    // Reported by the block's slow decode.
                    let start = if b == 0 { 0 } else { self.ends[b - 1] as usize };
                    self.frontier.truncate(start);
                    break;
                };
                next_last += gap;
                let width = tf_width(self.frontier[end - 1].0, self.adaptive);
                self.last.push(last);
                self.ends.push(end as u32);
                self.tf_at.push(next_tf as u32);
                self.widths.push(width as u8);
                // A whole block's buckets: `block_size` at `width` bits.
                next_tf += if width == 1 {
                    full
                } else {
                    bits::packed_len(self.block_size as usize, width) as u64
                };
                at += len;
                b += 1;
                if b > from && last >= slot {
                    found = true;
                    break;
                }
            }
            self.at = at;
            self.next_last = next_last;
            self.next_tf = next_tf;
            if found || !self.decode_next()? {
                return Ok(());
            }
            let b = self.last.len() - 1;
            if b >= from && self.last[b] >= slot {
                return Ok(());
            }
        }
    }

    /// Decodes the next block's entry, reading windows as needed; false
    /// when every block is.
    #[inline(never)]
    fn decode_next(&mut self) -> Result<bool> {
        if self.last.len() >= self.blocks {
            return Ok(false);
        }
        let mut want = BRIDGE;
        let entry = loop {
            let parsed = match self.window.get(self.at - self.window_at..) {
                Some(bytes) if self.started => footer_entry(bytes, &mut self.frontier),
                _ => Err(Error::Truncated),
            };
            match parsed {
                Ok(entry) => break entry,
                Err(Error::Truncated) => {
                    // The entry runs past the window: read on.
                    self.read_window(want)?;
                    want = (want * 2).min(ENTRY_MAX);
                }
                Err(error) => return Err(error),
            }
        };
        self.push_block(entry)
    }

    /// Records the block whose entry, at the next block's place, was just
    /// decoded (`(gap, length)`, its pairs appended to the frontier).
    #[inline(always)]
    fn push_block(&mut self, (gap, len): (u64, usize)) -> Result<bool> {
        let b = self.last.len();
        let next_last = self.next_last + gap;
        let last = u32::try_from(next_last).map_err(|_| Error::Corrupt("footer slot"))?;
        let n = if b + 1 == self.blocks {
            self.df - b as u32 * self.block_size
        } else {
            self.block_size
        };
        let end = self.frontier.len();
        let max = self.frontier[end - 1].0;
        let width = tf_width(max, self.adaptive);
        let tf_at = self.next_tf;
        let next_tf = tf_at + bits::packed_len(n as usize, width) as u64;
        let at = self.at + len;
        if b + 1 == self.blocks && (at != self.src.len() || next_tf != self.tf_len) {
            return Err(Error::Corrupt("footer length"));
        }
        self.last.push(last);
        self.ends.push(end as u32);
        self.tf_at.push(tf_at as u32);
        self.widths.push(width as u8);
        self.next_last = next_last;
        self.next_tf = next_tf;
        self.at = at;
        Ok(true)
    }

    /// Decodes blocks through `b` (which must be below [`Self::blocks`]).
    #[inline]
    pub fn ensure(&mut self, b: usize) -> Result<()> {
        if self.last.len() <= b {
            self.decode_until(b, 0)?;
            if self.last.len() <= b {
                return Err(Error::Corrupt("footer block"));
            }
        }
        Ok(())
    }

    /// The first block from `from` on whose last slot is `slot` or later,
    /// decoding as far as it; [`Self::blocks`] when there is none. Blocks
    /// are decoded only up to the one returned.
    #[inline]
    pub fn seek(&mut self, from: usize, slot: u32) -> Result<usize> {
        if let Some(found) = self
            .last
            .get(from..)
            .and_then(|rest| rest.iter().position(|l| *l >= slot))
        {
            return Ok(from + found);
        }
        self.decode_until(from, slot)?;
        let b = self.last.len().wrapping_sub(1);
        Ok(if b < self.blocks && b >= from && self.last[b] >= slot {
            b
        } else {
            self.blocks
        })
    }

    /// Block `b`'s last slot (decoded).
    #[inline]
    pub fn last(&self, b: usize) -> u32 {
        self.last[b]
    }

    /// The last slots of the blocks decoded.
    #[inline]
    pub fn lasts(&self) -> &[u32] {
        &self.last
    }

    /// Block `b`'s frontier (decoded): its (bucket, shortest length) pairs.
    #[inline]
    pub fn frontier(&self, b: usize) -> &[(u8, u32)] {
        let from = if b == 0 { 0 } else { self.ends[b - 1] as usize };
        &self.frontier[from..self.ends[b] as usize]
    }

    /// Block `b`'s largest bucket (decoded): its frontier's last pair's.
    #[inline]
    pub fn max_bucket(&self, b: usize) -> u8 {
        self.frontier[self.ends[b] as usize - 1].0
    }

    /// Where block `b`'s buckets start in the TF tail (decoded).
    #[inline]
    pub fn tf_at(&self, b: usize) -> u32 {
        self.tf_at[b]
    }

    /// The width of block `b`'s buckets in the TF tail (decoded).
    #[inline]
    pub fn width(&self, b: usize) -> u32 {
        u32::from(self.widths[b])
    }

    /// The bucket of posting `index`, from its record's TF tail, decoding
    /// the footer through its block.
    #[inline]
    pub fn bucket(&mut self, tf: Bytes<'_>, index: u32) -> Result<u8> {
        if let Some(bucket) = self.single {
            return Ok(bucket);
        }
        let block = (index / self.block_size) as usize;
        if block >= self.blocks {
            return Err(Error::Corrupt("posting index"));
        }
        let width = match self.widths.get(block) {
            Some(width) => u32::from(*width),
            None => {
                self.ensure(block)?;
                u32::from(self.widths[block])
            }
        };
        if width == 0 {
            return Ok(0);
        }
        let bit = (index % self.block_size) as usize * width as usize;
        let bytes = tf.window(self.tf_at[block] as usize + bit / 8, 8)?;
        Ok(bits::get_at(bytes, bit % 8, width)? as u8)
    }
}

impl Drop for LazyFooter<'_> {
    /// Counts what was decoded ([`super::segment::MemoCounts`]); a derived
    /// footer was counted as it was decoded whole.
    fn drop(&mut self) {
        if self.at > 0 {
            let (blocks, bytes) = (self.last.len() as u64, self.at as u64);
            super::segment::count_memo(|c| {
                c.footers_decoded += 1;
                c.footer_blocks += blocks;
                c.footer_bytes += bytes;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::docs::Geometry;
    use super::*;
    use crate::Tid;
    use proptest::prelude::*;

    fn geometry(tids: &[Tid]) -> Geometry {
        Geometry::of(tids).unwrap()
    }

    fn round_trip(
        geometry: &Geometry,
        slots: &[u32],
        buckets: &[u8],
        lengths: &[u32],
        options: &Options,
    ) -> Stats {
        let mut out = Vec::new();
        let stats = encode(geometry, slots, buckets, lengths, options, &mut out);
        assert_eq!(stats.total(), out.len());
        let parsed = Postings::parse(&out, slots.len() as u32, geometry).unwrap();
        assert_eq!(parsed.slots(geometry).unwrap(), slots);
        let max = *buckets.iter().max().unwrap();
        let footer = parsed
            .footer(options.block_size, max, options.adaptive_tf)
            .unwrap();
        for (i, bucket) in buckets.iter().enumerate() {
            assert_eq!(
                footer.bucket(parsed.tf, i as u32).unwrap(),
                *bucket,
                "posting {i}"
            );
        }
        check_lazy(&parsed, &footer, options, max);
        check_group_frontiers(&parsed, geometry, slots, buckets, lengths, options);
        // Every posting is covered by its block's frontier, and the last
        // slot of each block is right.
        for b in 0..footer.blocks() {
            let from = b * options.block_size as usize;
            let to = (from + options.block_size as usize).min(slots.len());
            assert_eq!(footer.last[b], slots[to - 1]);
            let front = footer.frontier_of(b);
            for i in from..to {
                assert!(
                    front.iter().any(
                        |(fb, fl)| *fb >= buckets[i] && (*fl <= lengths[i] || slots.len() == 1)
                    ),
                    "posting {i} uncovered by {front:?}"
                );
            }
            if slots.len() > 1 {
                // Every frontier pair is attained.
                for (fb, fl) in front {
                    assert!((from..to).any(|i| buckets[i] == *fb && lengths[i] == *fl));
                }
            }
        }
        stats
    }

    /// A grouped record's directory holds, per group, the frontier of the
    /// group's postings (of their rounded lengths when rounded): every
    /// posting is dominated by one of its pairs, and every pair is a
    /// posting's.
    fn check_group_frontiers(
        parsed: &Postings<'_>,
        geometry: &Geometry,
        slots: &[u32],
        buckets: &[u8],
        lengths: &[u32],
        options: &Options,
    ) {
        let Form::Grouped(entries) = &parsed.form else {
            assert!(parsed.frontiers.is_none());
            return;
        };
        let rounded = options.group_frontiers == GroupFrontiers::Rounded;
        assert_eq!(parsed.frontiers_rounded, rounded);
        for entry in entries.iter() {
            let Some(front) = parsed.group_frontier(entry) else {
                assert_eq!(options.group_frontiers, GroupFrontiers::Off);
                assert_eq!(entry.frontier, NO_FRONTIER);
                continue;
            };
            assert_ne!(options.group_frontiers, GroupFrontiers::Off);
            let front: Vec<(u8, u32)> = front.collect();
            let group = geometry.groups[entry.index as usize];
            let members: Vec<usize> = (0..slots.len())
                .filter(|i| {
                    slots[*i] >= group.slot_base && slots[*i] < group.slot_base + group.slots()
                })
                .collect();
            assert_eq!(members.len(), entry.count as usize);
            let stored = |l: u32| {
                if rounded {
                    rounded_length(round_length(l))
                } else {
                    l
                }
            };
            let expected: Vec<(u8, u32)> = group_frontier(
                members.iter().map(|i| (buckets[*i], lengths[*i])),
                options.group_frontiers,
            )
            .into_iter()
            .map(|(b, v)| (b, if rounded { rounded_length(v) } else { v }))
            .collect();
            assert_eq!(front, expected, "group {}", entry.index);
            for i in &members {
                assert!(
                    front
                        .iter()
                        .any(|(fb, fl)| *fb >= buckets[*i] && *fl <= lengths[*i]),
                    "posting {i} uncovered by {front:?}"
                );
            }
            for (fb, fl) in &front {
                assert!(
                    members
                        .iter()
                        .any(|i| buckets[*i] == *fb && stored(lengths[*i]) == *fl)
                );
            }
        }
    }

    #[test]
    fn rounded_lengths_keep_six_significant_bits() {
        let mut previous = 0;
        for length in (1..100_000u32).chain([u32::MAX - 1, u32::MAX]) {
            let code = round_length(length);
            assert!(code >= previous, "codes order as lengths do");
            previous = code;
            let back = rounded_length(code);
            assert!(back <= length, "{length} rounds up to {back}");
            let bits = 32 - length.leading_zeros();
            assert!(u64::from(length - back) < 1u64 << bits.saturating_sub(6));
            assert_eq!(round_length(back), code);
        }
        for exact in [1u32, 63, 64, 65, 126, 128, 256, 4032] {
            if exact < 64 || exact.count_ones() == 1 || exact == 4032 {
                assert_eq!(rounded_length(round_length(exact)), exact);
            }
        }
        assert_eq!(rounded_length(round_length(65)), 64);
        assert_eq!(rounded_length(round_length(300)), 296);
    }

    #[test]
    fn group_frontiers_round_trip() {
        // Pairs of every shape through the codec: deltas of one and of
        // several bytes, all sixteen buckets.
        let cases: Vec<Vec<(u8, u32)>> = vec![
            vec![(0, 1)],
            vec![(15, u32::MAX)],
            vec![(0, 1), (1, 2), (2, 3)],
            vec![(3, 100), (9, 100_000), (15, 4_000_000_000)],
            (0..16).map(|b| (b, u32::from(b) * 1000 + 7)).collect(),
        ];
        for pairs in cases {
            for rounded in [false, true] {
                let mut bytes = vec![0xaa];
                put_group_frontier(&mut bytes, &pairs);
                let mut at = 1;
                skip_group_frontier(&bytes, &mut at).unwrap();
                assert_eq!(at, bytes.len());
                let back: Vec<(u8, u32)> = GroupFrontier::new(&bytes, 1, rounded).collect();
                let want: Vec<(u8, u32)> = pairs
                    .iter()
                    .map(|(b, v)| (*b, if rounded { rounded_length(*v) } else { *v }))
                    .collect();
                assert_eq!(back, want);
                // Truncated anywhere, it fails to parse.
                for cut in 1..bytes.len() {
                    let mut at = 1;
                    assert!(skip_group_frontier(&bytes[..cut], &mut at).is_err());
                }
            }
        }
        // A bucket past 15 is refused.
        let mut bytes = Vec::new();
        bytes.push((1 << 4) | 15);
        varint::put(&mut bytes, 5);
        varint::put(&mut bytes, 0);
        let mut at = 0;
        assert!(skip_group_frontier(&bytes, &mut at).is_err());
    }

    /// The footer decoded a block at a time agrees with the whole one:
    /// sought block by block, asked for a bucket first, and sought past
    /// its end.
    fn check_lazy(parsed: &Postings<'_>, footer: &Footer, options: &Options, max: u8) {
        let lazy = || {
            parsed
                .lazy_footer(options.block_size, max, options.adaptive_tf)
                .unwrap()
        };
        let mut l = lazy();
        assert_eq!(l.blocks(), footer.blocks());
        // A footer derived from its record (a single posting, a compact
        // record) is decoded whole; any other, nothing yet.
        let derived = matches!(parsed.form, Form::Single(_)) || parsed.compact;
        let start = l.decoded();
        assert_eq!(start, if derived { footer.blocks() } else { 0 });
        for b in 0..footer.blocks() {
            assert_eq!(l.seek(b, footer.last[b]).unwrap(), b, "block {b}");
            assert_eq!(l.last(b), footer.last[b]);
            let front = l.frontier(b).to_vec();
            assert_eq!(front, footer.frontier_of(b), "frontier of {b}");
            assert_eq!(l.max_bucket(b), footer.frontier_of(b).last().unwrap().0);
            assert_eq!(l.tf_at(b), footer.tf_at[b]);
            assert_eq!(l.width(b), u32::from(footer.widths[b]));
            // A block is decoded only once the walk reaches it.
            assert_eq!(l.decoded(), start.max(b + 1));
        }
        assert_eq!(l.seek(0, u32::MAX).unwrap(), footer.blocks());
        assert_eq!(l.seek(0, 0).unwrap(), 0);
        // A bucket asked for first decodes its block's prefix only.
        let df = parsed.df;
        let mut l = lazy();
        let i = df - 1;
        assert_eq!(
            l.bucket(parsed.tf, i).unwrap(),
            footer.bucket(parsed.tf, i).unwrap()
        );
        for i in 0..df {
            assert_eq!(
                l.bucket(parsed.tf, i).unwrap(),
                footer.bucket(parsed.tf, i).unwrap(),
                "posting {i}"
            );
        }
        // Sought from the start for each block's first slot.
        let mut l = lazy();
        let mut from = 0;
        for b in 0..footer.blocks() {
            let first = if b == 0 { 0 } else { footer.last[b - 1] + 1 };
            from = l.seek(from, first).unwrap();
            assert_eq!(from, b);
        }
    }

    fn all_options() -> Vec<Options> {
        let mut out = Vec::new();
        for block_size in [1, 3, 128] {
            for (paged, ef_groups, sparse) in [
                (true, true, true),
                (false, false, false),
                (true, false, false),
                (false, true, false),
                (false, false, true),
            ] {
                for adaptive_tf in [true, false] {
                    for grid_density in [0, 4] {
                        out.push(Options {
                            block_size,
                            paged,
                            ef_groups,
                            sparse,
                            adaptive_tf,
                            grid_density,
                            grid_min_postings: 0,
                            inline_lengths_max_df: if adaptive_tf { 8 } else { 0 },
                            inline_lengths_min_documents: 0,
                            group_frontiers: [
                                GroupFrontiers::Off,
                                GroupFrontiers::Exact,
                                GroupFrontiers::Rounded,
                            ][(block_size + grid_density) as usize % 3],
                        });
                    }
                }
            }
        }
        out
    }

    fn check_all(tids: &[Tid], members: &[Tid], buckets: &[u8]) {
        let geometry = geometry(tids);
        let slots: Vec<u32> = members
            .iter()
            .map(|t| geometry.slot_of(*t).unwrap())
            .collect();
        let lengths: Vec<u32> = (0..members.len())
            .map(|i| (i as u32 * 7919) % 300 + 1)
            .collect();
        for options in all_options() {
            round_trip(&geometry, &slots, buckets, &lengths, &options);
        }
    }

    #[test]
    fn frontier_keeps_undominated_pairs() {
        assert_eq!(frontier([(0, 10)].into_iter()), vec![(0, 10)]);
        assert_eq!(
            frontier([(0, 10), (1, 20), (2, 5)].into_iter()),
            vec![(2, 5)]
        );
        assert_eq!(
            frontier([(0, 3), (1, 20), (2, 50)].into_iter()),
            vec![(0, 3), (1, 20), (2, 50)]
        );
        assert_eq!(frontier([(3, 9), (3, 4), (0, 4)].into_iter()), vec![(3, 4)]);
    }

    #[test]
    fn edge_offsets_and_groups() {
        let tids = [
            Tid {
                block: 0,
                offset: 1,
            },
            Tid {
                block: 0,
                offset: 291,
            },
            Tid {
                block: 7,
                offset: 2,
            },
            Tid {
                block: 300,
                offset: 1,
            },
            Tid {
                block: 1 << 20,
                offset: 291,
            },
        ];
        // One posting at offset 1, one at 291, single-document groups.
        check_all(&tids, &tids[..1], &[3]);
        check_all(&tids, &tids[1..2], &[0]);
        check_all(&tids, &tids[3..4], &[15]);
        check_all(&tids, &tids, &[0, 1, 2, 15, 4]);
        check_all(&tids, &[tids[0], tids[1]], &[0, 0]);
    }

    #[test]
    fn directories_longer_than_the_first_window() {
        // A document in each of 3,000 groups: a directory of some 6 KB, past
        // the 4 KiB read first, is read in one window sized by its count.
        let tids: Vec<Tid> = (0..3000u32)
            .map(|g| Tid {
                block: g * 256,
                offset: 1,
            })
            .collect();
        let geometry = geometry(&tids);
        let slots: Vec<u32> = tids.iter().map(|t| geometry.slot_of(*t).unwrap()).collect();
        let buckets = vec![1u8; slots.len()];
        let lengths: Vec<u32> = (0..slots.len() as u32).map(|i| i % 50 + 1).collect();
        let options = Options {
            sparse: false,
            grid_min_postings: 0,
            ..Options::default()
        };
        round_trip(&geometry, &slots, &buckets, &lengths, &options);
    }

    #[test]
    fn very_dense_terms() {
        // Every document of 512 full pages of 40 tuples: grids win.
        let tids: Vec<Tid> = (0..512u32)
            .flat_map(|b| {
                (1..=40u16).map(move |o| Tid {
                    block: b,
                    offset: o,
                })
            })
            .collect();
        let buckets: Vec<u8> = (0..tids.len()).map(|i| (i % 3 == 0) as u8).collect();
        let geometry = geometry(&tids);
        let slots: Vec<u32> = tids.iter().map(|t| geometry.slot_of(*t).unwrap()).collect();
        let lengths = vec![50; tids.len()];
        let grids = Options {
            paged: false,
            ..Options::default()
        };
        let stats = round_trip(&geometry, &slots, &buckets, &lengths, &grids);
        assert!(!stats.sparse);
        assert_eq!(stats.kinds, [2, 0, 0]);
        // A bit per slot, a bit per bucket; per group a directory entry and
        // a frontier of one pair, (1, 50), of two bytes.
        assert_eq!(stats.payload, 2 * 32 * 40 + 1 + 2 * (1 + 3) + 2 * 2);
        assert_eq!((stats.frontiers, stats.frontier_bytes), (2, 4));
        assert_eq!(stats.tf, tids.len() / 8);
        // Whole pages are runs, smaller still, when size alone decides; the
        // default keeps groups this dense as grids.
        let by_size = Options {
            grid_density: 0,
            ..Options::default()
        };
        let stats = round_trip(&geometry, &slots, &buckets, &lengths, &by_size);
        assert_eq!(stats.kinds, [0, 0, 2]);
        let stats = round_trip(&geometry, &slots, &buckets, &lengths, &Options::default());
        assert_eq!((stats.kinds, stats.forced), ([2, 0, 0], 2));
        check_all(&tids, &tids, &buckets);
        // Every tenth document, and runs of whole pages.
        let tenth: Vec<Tid> = tids.iter().step_by(10).copied().collect();
        check_all(&tids, &tenth, &vec![2; tenth.len()]);
        let runs: Vec<Tid> = tids.iter().filter(|t| t.block % 3 == 0).copied().collect();
        check_all(&tids, &runs, &vec![0; runs.len()]);
    }

    #[test]
    fn rejects_corruption() {
        let tids: Vec<Tid> = (0..10u32)
            .map(|b| Tid {
                block: b,
                offset: 1,
            })
            .collect();
        let geometry = geometry(&tids);
        let slots: Vec<u32> = tids.iter().map(|t| geometry.slot_of(*t).unwrap()).collect();
        let mut out = Vec::new();
        encode(
            &geometry,
            &slots,
            &[0; 10],
            &[1; 10],
            // Dense enough for a grid, whatever its size.
            &Options {
                grid_min_postings: 0,
                ..Options::default()
            },
            &mut out,
        );
        let wrong_df =
            Postings::parse(&out, 9, &geometry).and_then(|p| p.footer(128, 0, true).map(|_| ()));
        assert!(wrong_df.is_err());
        assert!(Postings::parse(&out[..out.len() - 1], 10, &geometry).is_err());
        assert!(Postings::parse(&[], 2, &geometry).is_err());
        assert!(Postings::parse(&[9, 0, 0], 2, &geometry).is_err());
    }

    proptest! {
        #[test]
        fn random_terms(
            docs in prop::collection::btree_set((0u32..1200, 1u16..=60), 2..600),
            pick in prop::collection::vec(any::<bool>(), 600),
            buckets in prop::collection::vec(0u8..16, 600),
        ) {
            let tids: Vec<Tid> = docs.into_iter().map(|(block, offset)| Tid { block, offset }).collect();
            let members: Vec<Tid> = tids.iter().zip(&pick).filter(|(_, p)| **p).map(|(t, _)| *t).collect();
            prop_assume!(!members.is_empty());
            let buckets: Vec<u8> = buckets[..members.len()].iter().map(|b| if b % 3 == 0 { *b } else { 0 }).collect();
            check_all(&tids, &members, &buckets);
        }
    }
}

#[cfg(test)]
mod lazy_footer_tests {
    use super::super::docs::Geometry;
    use super::*;
    use crate::Tid;

    #[test]
    fn lazy_footer_rejects_corruption() {
        let tids: Vec<Tid> = (0..10u32)
            .map(|b| Tid {
                block: b * 3,
                offset: 1,
            })
            .collect();
        let geometry = Geometry::of(&tids).unwrap();
        let slots: Vec<u32> = tids.iter().map(|t| geometry.slot_of(*t).unwrap()).collect();
        let options = Options {
            block_size: 2,
            grid_min_postings: u32::MAX,
            inline_lengths_max_df: 0,
            ..Options::default()
        };
        let mut out = Vec::new();
        encode(&geometry, &slots, &[1; 10], &[5; 10], &options, &mut out);
        let parsed = Postings::parse(&out, 10, &geometry).unwrap();
        let bytes = parsed.footer.all().unwrap().to_vec();
        let with = |footer: &[u8]| {
            let mut p = parsed.clone();
            p.footer = Bytes::Slice(footer);
            p.lazy_footer(2, 1, true)
                .and_then(|mut l| l.seek(0, u32::MAX))
        };
        assert_eq!(with(&bytes).unwrap(), 5);
        // Truncated, one byte too many, a frontier of no pairs.
        assert!(with(&bytes[..bytes.len() - 1]).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(with(&longer).is_err());
        let mut empty = bytes.clone();
        let mut k = 0;
        varint::get(&bytes, &mut k).unwrap();
        empty[k] = 0;
        assert!(with(&empty).is_err());
        // A prefix decodes before the corruption past it shows.
        let mut l = {
            let mut p = parsed.clone();
            p.footer = Bytes::Slice(&bytes[..bytes.len() - 1]);
            p.lazy_footer(2, 1, true).unwrap()
        };
        assert_eq!(l.seek(0, slots[0]).unwrap(), 0);
    }

    /// Over a blob read in place, a footer's pages are pinned only as far
    /// as its blocks are decoded, an entry across a page boundary is
    /// stitched alone, and every block agrees with the whole footer.
    #[test]
    fn lazy_footer_reads_in_place_as_far_as_reached() {
        use super::super::blob::{self, LazyBlob, PinningSource};
        let tids: Vec<Tid> = (0..4000u32)
            .map(|i| Tid {
                block: i / 3,
                offset: (i % 3 + 1) as u16,
            })
            .collect();
        let geometry = Geometry::of(&tids).unwrap();
        let members: Vec<u32> = (0..4000)
            .step_by(2)
            .map(|i| geometry.slot_of(tids[i]).unwrap())
            .collect();
        let n = members.len();
        let buckets: Vec<u8> = (0..n).map(|i| (i * 7 % 16) as u8).collect();
        let lengths: Vec<u32> = (0..n).map(|i| (i as u32 * 7919) % 5000 + 1).collect();
        let options = Options {
            block_size: 4,
            grid_min_postings: u32::MAX,
            inline_lengths_max_df: 0,
            ..Options::default()
        };
        let mut out = Vec::new();
        encode(&geometry, &members, &buckets, &lengths, &options, &mut out);
        let whole = Postings::parse(&out, n as u32, &geometry)
            .unwrap()
            .footer(4, 15, true)
            .unwrap();
        let footer_len = {
            let p = Postings::parse(&out, n as u32, &geometry).unwrap();
            p.footer.len()
        };
        assert!(footer_len > 20 * 64, "a footer of many pages");
        let source = PinningSource::new(out.clone(), 64);
        let blob = LazyBlob::new(Box::new(source.clone()));
        blob.open_span();
        let parsed = Postings::parse(blob.bytes(), n as u32, &geometry).unwrap();
        blob::reset_stats();
        let mut lazy = parsed.lazy_footer(4, 15, true).unwrap();
        let pins = source.0.pins.get();
        lazy.ensure(0).unwrap();
        assert!(
            source.0.pins.get() - pins <= 2,
            "the first block's pages only"
        );
        let reached = whole.blocks() / 3;
        lazy.ensure(reached).unwrap();
        let pinned = source.0.pins.get() - pins;
        assert!(
            pinned * 64 < footer_len / 2,
            "{pinned} pages pinned of a {footer_len}-byte footer"
        );
        for b in 0..whole.blocks() {
            assert_eq!(lazy.seek(b, whole.last[b]).unwrap(), b);
            assert_eq!(lazy.frontier(b), whole.frontier_of(b));
            assert_eq!(lazy.tf_at(b), whole.tf_at[b]);
        }
        for i in 0..n as u32 {
            assert_eq!(lazy.bucket(parsed.tf, i).unwrap(), buckets[i as usize]);
        }
        // Stitched: only entries across page boundaries, a few bytes each.
        let stitched = blob::stats()[Kind::Footer as usize].stitched as usize;
        assert!(
            stitched < footer_len / 2,
            "stitched {stitched} of {footer_len}"
        );
        drop(lazy);
        drop(parsed);
        unsafe { blob.close_span() };
    }
    /// A lazy footer read on a block at a time, every page pinned since a
    /// mark released (none kept, poisoned) between blocks but for the
    /// window it reports borrowed, decodes the same blocks as whole.
    #[test]
    fn lazy_footer_reads_on_with_only_its_borrowed_window_held() {
        use super::super::blob::{LazyBlob, PinningSource};
        let tids: Vec<Tid> = (0..4000u32)
            .map(|i| Tid {
                block: i / 3,
                offset: (i % 3 + 1) as u16,
            })
            .collect();
        let geometry = Geometry::of(&tids).unwrap();
        let members: Vec<u32> = (0..4000)
            .step_by(3)
            .map(|i| geometry.slot_of(tids[i]).unwrap())
            .collect();
        let n = members.len();
        let buckets: Vec<u8> = (0..n).map(|i| (i * 5 % 16) as u8).collect();
        let lengths: Vec<u32> = (0..n).map(|i| (i as u32 * 7919) % 5000 + 1).collect();
        let options = Options {
            block_size: 4,
            grid_min_postings: u32::MAX,
            inline_lengths_max_df: 0,
            ..Options::default()
        };
        let mut out = Vec::new();
        encode(&geometry, &members, &buckets, &lengths, &options, &mut out);
        let whole = Postings::parse(&out, n as u32, &geometry)
            .unwrap()
            .footer(4, 15, true)
            .unwrap();
        let source = PinningSource::new(out.clone(), 64);
        let blob = LazyBlob::new(Box::new(source.clone()));
        blob.set_keep(0);
        blob.open_span();
        let parsed = Postings::parse(blob.bytes(), n as u32, &geometry).unwrap();
        let mark = blob.pin_mark();
        let mut lazy = parsed.lazy_footer(4, 15, true).unwrap();
        let mut held_at_most = 0;
        for b in 0..whole.blocks() {
            assert_eq!(lazy.seek(b, whole.last[b]).unwrap(), b);
            assert_eq!(lazy.frontier(b), whole.frontier_of(b));
            assert_eq!(lazy.tf_at(b), whole.tf_at[b]);
            let held: Vec<(usize, usize)> = lazy.borrowed().into_iter().collect();
            if let Some((from, to)) = held.first() {
                assert!(from < to && (to - 1) / 64 == from / 64, "one page");
            }
            // Keep 0: anything pinned since is released, but the window.
            if blob.over_keep(mark) {
                unsafe { blob.release_since(mark, &held) };
                held_at_most = held_at_most.max(blob.pinned_since(mark));
            }
        }
        assert!(
            source.0.early.get() > 10,
            "pages released as the footer read on"
        );
        assert!(held_at_most <= 1, "{held_at_most} pages held");
        drop(lazy);
        drop(parsed);
        unsafe { blob.close_span() };
    }
}
