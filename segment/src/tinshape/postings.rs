// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A term's postings: its documents as a set of ctid-grid slots (see
//! [`super::docs`]), a footer of per-block score metadata, and the TF tail.
//!
//! ```text
//! record  := single | sparse | grouped
//! single  := slot varint                    (df = 1; the bucket is the
//!                                            dictionary's max_tf_bucket)
//! sparse  := form u8 (1), footer_len varint, payload_len varint,
//!            footer, ef(df slots below the grid's slot count), tf
//! footer  := block*, one per `block_size` postings in posting order:
//!            last_gap varint (its last slot less the previous block's,
//!                             the first block's from zero),
//!            frontier_len u8 (1..=16),
//!            (bucket u8, min_length varint)* ascending in both: the
//!            block's impact frontier
//! grouped := form u8 (2, | 0x10 when the frontiers' lengths are rounded),
//!            directory_len varint (bytes of the groups count and the
//!                                  directory: where containers start),
//!            payload_len varint,
//!            groups varint,
//!            (index_gap varint, (count - 1) << 2 | kind varint,
//!             [len varint, paged groups only],
//!             group frontier)*,                   the group directory
//!            container*, tf
//! group frontier := head u8 = (n - 1) << 4 | first bucket,
//!            first length varint,
//!            ((length - previous length - 1) << 4
//!             | bucket - previous bucket - 1) varint, for each later pair:
//!            the group's impact frontier, its lengths rounded down to six
//!            significant bits when the form says so
//! container := grid:  the group's slots in whole u64le words, bit `local`
//!                     set for each member
//!            | ef:    ef(count local slots below the group's slot count)
//!            | paged: (pages - 1) u8, the pages of the group's span (a byte
//!                     each when fewer than 32, else a 256-bit mask), then
//!                     per page head u8 = kind << 6 | value and its offsets:
//!                     list   (value + 1 offsets, `offset - 1` packed at
//!                             the bits the page's width needs),
//!                     bitmap (value + 1 bytes, bit `offset - 1`),
//!                     run    (first and count, less one, packed so)
//! tf      := sparse: per block, its buckets packed at the block's width,
//!            each block's starting on a byte;
//!            grouped: per group in directory order, its buckets packed at
//!            the group's width, in a row (no padding between groups);
//!            a width is 0 bits when the largest bucket is 0, 1 when 1,
//!            2 when at most 3, else 4
//! ```
//!
//! The impact frontier is the set of (bucket, length) pairs no other
//! posting of the block (or group) dominates (a bucket at least as high
//! with a length at most as long): its best score under any `k1` and `b`
//! is the score of one of them, as Lucene's competitive impacts are.
//!
//! A grouped record's score metadata is per (term, group), as TIN keeps
//! one footer entry per term and aligned 256-page group (its probes, rounds
//! 2 and 7): the directory entry of a group holds its count, its
//! container's kind and its frontier, and its buckets follow the groups
//! before it in the TF tail; a reader reaches a group's entry by its
//! position in the directory, decoding nothing of the groups before it.
//! A sparse record (one Elias-Fano list over the segment) keeps the footer
//! of blocks of `block_size` postings.
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
/// Set in a grouped record's form when its frontiers' lengths are rounded
/// down to six significant bits ([`round_length`]).
pub const FORM_ROUNDED: u8 = 0x10;
/// Set in a grouped record's form when its directory ends in skip points
/// ([`Directory`]; every grouped record from page layout version 11 on).
pub const FORM_SKIPS: u8 = 0x20;

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
    /// Whether a grouped record's frontiers hold exact lengths or lengths
    /// rounded down to six significant bits.
    pub group_frontiers: GroupFrontiers,
}

/// [`Options::group_frontiers`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupFrontiers {
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
    /// Bytes of the directory's skip points (part of `payload`).
    pub skip_bytes: usize,
}

impl Stats {
    pub fn total(&self) -> usize {
        self.header + self.footer + self.payload + self.tf + self.lengths
    }
}

/// The TF tail width of a block whose largest bucket is `max`.
/// [`tf_width`] (adaptive) by bucket, read without a branch on it.
const TF_WIDTH: [u8; 16] = [0, 1, 2, 2, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4];

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
        GroupFrontiers::Exact => frontier(postings),
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

/// Checks and steps over a group frontier at `at`; its largest bucket (the
/// last pair's).
#[inline]
fn skip_group_frontier(bytes: &[u8], at: &mut usize) -> Result<u8> {
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
    Ok(bucket as u8)
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
    pub fn new(bytes: &'b [u8], at: usize, rounded: bool) -> Self {
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

/// Bytes the varint of `value` takes.
fn varint_len(value: u64) -> usize {
    (64 - (value | 1).leading_zeros() as usize).div_ceil(7)
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
    // Payload: the smaller of one list over the grid and per-group
    // containers (with their directory's frontiers and their TF tail).
    let mut sparse = Vec::new();
    if options.sparse {
        ef::encode(slots, geometry.slots, &mut sparse);
    }
    let mut grouped_stats = Stats::default();
    let mut grouped_tf = Vec::new();
    let (grouped, directory_len) = encode_groups(
        geometry,
        slots,
        buckets,
        lengths,
        options,
        reuse,
        &mut grouped_stats,
        &mut grouped_tf,
    );
    // A sparse record's footer and TF tail, per block.
    let mut footer = Vec::new();
    let mut tf_bytes = Vec::new();
    let mut blocks = 0usize;
    if options.sparse && grouped_stats.forced == 0 {
        let mut tf = BitWriter::new();
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
            blocks += 1;
        }
    }
    // A term with a group dense enough to be a grid stays grouped. The
    // directory's frontiers and the footer do not count: they buy the
    // walks their bounds, and leaving them out keeps each term's form
    // what it was without them.
    let use_sparse = options.sparse
        && grouped_stats.forced == 0
        && sparse.len() <= grouped.len() - grouped_stats.frontier_bytes - grouped_stats.skip_bytes;
    let payload = if use_sparse { &sparse } else { &grouped };
    let mut inline_bytes = Vec::new();
    if inline {
        encode_inline_lengths(lengths, &mut inline_bytes);
    }
    let compact = use_sparse
        && inline
        && slots.len() as u64 <= u64::from(COMPACT_MAX)
        && slots.len() <= block;
    out.push(if use_sparse {
        FORM_SPARSE | if inline { FORM_LENGTHS } else { 0 } | if compact { FORM_COMPACT } else { 0 }
    } else {
        FORM_GROUPED
            | FORM_SKIPS
            | if inline { FORM_LENGTHS } else { 0 }
            | if options.group_frontiers == GroupFrontiers::Rounded {
                FORM_ROUNDED
            } else {
                0
            }
    });
    if !use_sparse {
        footer.clear();
        varint::put(out, directory_len as u64);
    } else if compact {
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
    let tf_bytes = if use_sparse { &tf_bytes } else { &grouped_tf };
    out.extend_from_slice(tf_bytes);
    stats.footer = footer.len();
    stats.payload = payload.len();
    stats.tf = tf_bytes.len();
    stats.sparse = use_sparse;
    if use_sparse {
        stats.blocks = blocks;
    } else {
        stats.kinds = grouped_stats.kinds;
        stats.kind_bytes = grouped_stats.kind_bytes;
        stats.forced = grouped_stats.forced;
        stats.frontier_bytes = grouped_stats.frontier_bytes;
        stats.skip_bytes = grouped_stats.skip_bytes;
        stats.frontiers = grouped_stats.frontiers;
    }
    stats
}

/// Encodes one group's members, `locals` its slots within the group and
/// `starts` its page starts; each page's offsets are packed at the bits its
/// own width needs.
fn encode_paged(locals: &[u32], starts: &[u32], out: &mut Vec<u8>) {
    let mut pages: Vec<(u32, Vec<u32>)> = Vec::new();
    let mut page = 0usize;
    for local in locals {
        while starts[page + 1] <= *local {
            page += 1;
        }
        let offset = local - starts[page];
        match pages.last_mut() {
            Some((p, offsets)) if *p == page as u32 => offsets.push(offset),
            _ => pages.push((page as u32, vec![offset])),
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
    for (page, offsets) in &pages {
        let page = *page as usize;
        let ob = bits::width_below(starts[page + 1] - starts[page]);
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

/// A grouped payload (its directory with each group's frontier, then its
/// containers) and the bytes of its group count and directory (where its
/// containers start), and in `tf` the record's TF tail: each group's
/// buckets at the group's width, in a row.
#[expect(clippy::too_many_arguments, reason = "the postings and the outputs")]
fn encode_groups<'r>(
    geometry: &Geometry,
    slots: &[u32],
    buckets: &[u8],
    lengths: &[u32],
    options: &Options,
    reuse: &mut dyn FnMut(usize) -> Option<Reused<'r>>,
    stats: &mut Stats,
    tf: &mut Vec<u8>,
) -> (Vec<u8>, usize) {
    let mut directory = Vec::new();
    let mut bodies = Vec::new();
    let mut groups = 0usize;
    let mut previous: Option<usize> = None;
    let mut at = 0usize;
    let mut index = geometry.group_of_slot(slots[0]);
    let mut locals = Vec::new();
    let mut candidate = Vec::new();
    let mut tf_bits = BitWriter::new();
    let mut skips = Vec::new();
    let mut tf_total = 0u64;
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
        let starts = geometry.starts(index);
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
            encode_paged(&locals, starts, &mut candidate);
            if candidate.len() < best {
                best = candidate.len();
                kind = KIND_PAGED;
            }
        }
        let gap = match previous {
            None => index,
            Some(p) => index - p - 1,
        };
        if groups > 0 && groups.is_multiple_of(DIRECTORY_SKIP) {
            // Where decoding stands before this entry: a skip point.
            put_skip(
                &mut skips,
                &Resume {
                    at: directory.len(),
                    after: previous.map_or(0, |p| p as u64 + 1),
                    body: bodies.len() as u64,
                    first: from as u64,
                    tf_bit: tf_total,
                },
            );
        }
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
        debug_assert_eq!(bodies.len() - before, best);
        // The group's frontier, from its postings (a copied container's
        // included: a merge's is the least length per bucket over the
        // inputs' live postings there), and its buckets at its width.
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
        // A group's buckets are always packed at the group's own width.
        let width = tf_width(pairs.last().expect("a pair").0, true);
        for bucket in &buckets[from..at] {
            tf_bits.put(u32::from(*bucket), width);
        }
        tf_total += (at - from) as u64 * u64::from(width);
        stats.kinds[kind as usize] += 1;
        stats.kind_bytes[kind as usize] += best;
        groups += 1;
    }
    *tf = tf_bits.finish();
    debug_assert_eq!(skips.len(), skip_count(groups) * SKIP_BYTES);
    stats.skip_bytes += skips.len();
    let mut out = Vec::with_capacity(directory.len() + skips.len() + bodies.len() + 4);
    varint::put(&mut out, groups as u64);
    out.extend_from_slice(&directory);
    out.extend_from_slice(&skips);
    let directory_len = out.len();
    out.extend_from_slice(&bodies);
    (out, directory_len)
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
    /// Where the group's frontier lies in the directory's bytes
    /// ([`Postings::directory`]).
    pub frontier: u32,
    /// Where the group's buckets start in the TF tail, in bits, and their
    /// width; the frontier's largest bucket.
    pub tf_bit: u32,
    pub width: u8,
    pub max_bucket: u8,
}

impl GroupEntry {
    /// Where posting `index` of the group (its `first` or later) has its
    /// bucket in the TF tail, in bits.
    #[inline]
    pub fn bucket_bit(&self, index: u32) -> usize {
        self.tf_bit as usize + (index - self.first) as usize * usize::from(self.width)
    }
}

/// A term's membership.
#[derive(Clone, Debug)]
pub enum Form<'a> {
    Single(u32),
    Sparse(Ef<'a>),
    /// The group directory, shared by the clones of a parsed record and
    /// decoded as far as asked for.
    Grouped(std::rc::Rc<Directory>),
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
    /// Whether a grouped record's frontiers' lengths are rounded
    /// ([`FORM_ROUNDED`]).
    pub rounded: bool,
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
                rounded: false,
            });
        }
        // The form and three varints.
        let record = bytes.window(0, 1 + 3 * 10)?;
        let mut directory_len = 0;
        let form = *record.first().ok_or(Error::Truncated)?;
        let mut at = 1;
        let grouped = form & 3 == FORM_GROUPED;
        let rounded = form & FORM_ROUNDED != 0;
        let skips = form & FORM_SKIPS != 0;
        if (rounded || skips) && !grouped {
            return Err(Error::Corrupt("postings form"));
        }
        let footer_len = if form & FORM_COMPACT != 0 {
            if form & FORM_LENGTHS == 0 || form & 3 != FORM_SPARSE || df > COMPACT_MAX {
                return Err(Error::Corrupt("compact postings"));
            }
            0
        } else if grouped {
            directory_len = varint::get_u32(record, &mut at)? as usize;
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
        let form = form & !(FORM_LENGTHS | FORM_COMPACT | FORM_ROUNDED | FORM_SKIPS);
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
        let tf_len = bytes.len() - tf_at;
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
                // The header gives the directory's length: its bytes are
                // read in one window and copied, and nothing of them is
                // stepped over ([`Directory`] decodes its entries as asked).
                let end = directory_len;
                if end == 0 || end > payload.len() {
                    return Err(Error::Corrupt("postings directory length"));
                }
                // Copied a page at a time: a directory spanning pages is
                // not stitched first (a second copy).
                let mut copy = Vec::with_capacity(end);
                payload.copy_into(0, end, &mut copy)?;
                let directory: Box<[u8]> = copy.into_boxed_slice();
                let mut at = 0;
                let groups = varint::get_u32(&directory, &mut at)? as usize;
                // An entry takes at least four bytes (a gap, a count and
                // kind, a frontier's head and first length).
                if groups == 0 || groups > geometry.groups.len() || end - at < 4 * groups {
                    return Err(Error::Corrupt("postings groups"));
                }
                super::segment::count_memo(|c| {
                    c.directories += 1;
                    c.directory_bytes += end as u64;
                });
                containers_at = end;
                Form::Grouped(std::rc::Rc::new(
                    Directory::new(
                        directory,
                        groups,
                        skips,
                        df,
                        (payload.len() - end) as u64,
                        tf_len as u64,
                    )
                    .ok_or(Error::Corrupt("postings groups"))?,
                ))
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
            rounded,
        })
    }

    /// The impact frontier a grouped record's directory holds for a group.
    #[inline]
    pub fn group_frontier(&self, entry: &GroupEntry) -> GroupFrontier<'_> {
        GroupFrontier::new(
            self.directory_bytes(),
            entry.frontier as usize,
            self.rounded,
        )
    }

    /// A grouped record's directory's bytes (its entries carry their
    /// groups' frontiers); none for another record.
    #[inline]
    pub fn directory_bytes(&self) -> &[u8] {
        match &self.form {
            Form::Grouped(dir) => dir.bytes(),
            _ => &[],
        }
    }

    /// The bucket of posting `index` of a grouped record (its group's
    /// entry found by the index), a single posting's (`max_bucket`, the
    /// dictionary's), or a sparse record's through its footer, decoded
    /// whole: for a reader of one posting, not a walk.
    pub fn bucket_at(
        &self,
        index: u32,
        geometry: &Geometry,
        block_size: u32,
        max_bucket: u8,
        adaptive_tf: bool,
    ) -> Result<u8> {
        match &self.form {
            Form::Single(_) => Ok(max_bucket),
            Form::Grouped(dir) => {
                self.bucket_in(dir.entry_of_posting(index, &geometry.groups)?, index)
            }
            Form::Sparse(_) => Footer::parse(self, geometry, block_size, max_bucket, adaptive_tf)?
                .bucket(self.tf, index),
        }
    }

    /// The bucket of posting `index` of a grouped record's group `entry`.
    #[inline]
    pub fn bucket_in(&self, entry: &GroupEntry, index: u32) -> Result<u8> {
        if entry.width == 0 {
            return Ok(0);
        }
        let bit = entry.bucket_bit(index);
        let bytes = self.tf.window(bit / 8, 8)?;
        Ok(bits::get_at(bytes, bit % 8, u32::from(entry.width))? as u8)
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
            Form::Grouped(dir) => {
                for b in 0..dir.len() {
                    let entry = dir.get(b, &geometry.groups)?;
                    let index = entry.index as usize;
                    let base = geometry.groups[index].slot_base;
                    let starts = geometry.starts(index);
                    for_each_local(entry, self.container(entry).all()?, starts, |local| {
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

    /// The footer, decoded (a grouped record's directory entries as its
    /// blocks, every one decoded).
    pub fn footer(
        &self,
        geometry: &Geometry,
        block_size: u32,
        max_bucket: u8,
        adaptive_tf: bool,
    ) -> Result<Footer> {
        Footer::parse(self, geometry, block_size, max_bucket, adaptive_tf)
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

/// Bytes a chunk of a directory scanned for its varints ([`Units`]) holds
/// at most.
const UNITS_CHUNK: usize = 2048;

/// Entries of a [`Directory`] between two skip points, decoded together the
/// first time one of them is asked for.
pub const DIRECTORY_SKIP: usize = 16;

/// Bytes of a skip point: five little-endian `u32`s.
const SKIP_BYTES: usize = 20;

/// Runs a walk decodes at once on reaching the run after one it decoded.
const SEQUENTIAL_RUNS: usize = 8;

/// Skip points of a directory of `groups` entries: one before each entry
/// past the first that a multiple of [`DIRECTORY_SKIP`] numbers.
#[inline]
fn skip_count(groups: usize) -> usize {
    groups.saturating_sub(1) / DIRECTORY_SKIP
}

/// Appends a skip point: where decoding stands before its entry, with the
/// entry's offset counted from the first entry (the group count before it
/// is written last).
fn put_skip(out: &mut Vec<u8>, at: &Resume) {
    for v in [at.at as u64, at.after, at.body, at.first, at.tf_bit] {
        let v = u32::try_from(v).expect("a directory's totals fit 32 bits");
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// A grouped record's directory: its bytes, copied when the record is
/// parsed (so the parsed record borrows nothing of the blob, and decoding
/// reads no page of it), and its entries, decoded [`DIRECTORY_SKIP`] at a
/// time as they are asked for.
///
/// An entry carries its group's impact frontier, so a directory takes about
/// ten bytes an entry; the record's header gives its length (where the
/// containers start), so the parse reads none of its entries. An entry's
/// totals (its container's offset, its first posting, its buckets' offset)
/// count every entry before it, so after the entries the directory holds a
/// skip point before every [`DIRECTORY_SKIP`]th: where decoding stands there
/// (the entry's offset, the group index after the entry before it, and the
/// totals), [`SKIP_BYTES`] each. A conjunction's walk asks a term for the
/// groups its rarest term holds, about a third of the entries it steps
/// over; reaching an entry decodes its own run of entries from the skip
/// point before it, and seeking a group reads the skip points' group
/// indexes, not the entries between (TIN's footer entries likewise locate
/// a group's container without those before it).
///
/// Each entry is checked as it is decoded against the record (its group
/// lies in the segment and holds its count, its container lies in the
/// payload and its buckets in the TF tail), and each run where it ends
/// against the next skip point, the last against the record's totals.
pub struct Directory {
    /// The directory's bytes, from its group count on: entries' frontier
    /// offsets index them.
    bytes: Box<[u8]>,
    /// A slot per entry, reserved when parsed and never moved: those of
    /// the runs marked in `done` are written, once, and read through shared
    /// references a walk holds while other runs are written. A walk asks
    /// for an entry several times per group (its group's bounds, frontier,
    /// buckets), so one decoded is a bit test and a load away.
    entries: Box<[std::cell::UnsafeCell<std::mem::MaybeUninit<GroupEntry>>]>,
    /// Runs of [`DIRECTORY_SKIP`] entries decoded, a bit each.
    done: Box<[std::cell::Cell<u64>]>,
    /// Entries decoded.
    decoded: std::cell::Cell<usize>,
    /// Where the entries start (after the group count), and where they end
    /// (the skip points start).
    head: usize,
    end: usize,
    /// Whether the directory has skip points (a record of page layout 10
    /// has none: its runs decode in order, from where the last one ended).
    skips: bool,
    resume: std::cell::Cell<Resume>,
    /// What the entries are checked against: the record's postings, its
    /// containers' bytes, and its TF tail's bytes.
    df: u32,
    containers: u64,
    tf_len: u64,
}

impl std::fmt::Debug for Directory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Directory")
            .field("len", &self.len())
            .field("bytes", &self.bytes.len())
            .field("decoded", &self.decoded.get())
            .finish()
    }
}

/// Where a directory's decoding stands: the next entry's offset in its
/// bytes, the group index after the last entry's (0 before the first), and
/// the totals over the entries so far: container bytes, postings, TF bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Resume {
    at: usize,
    after: u64,
    body: u64,
    first: u64,
    tf_bit: u64,
}

impl Directory {
    /// A directory of `len` entries in `bytes` (from its group count on,
    /// through its skip points if it has them); `None` if the bytes cannot
    /// hold them.
    fn new(
        bytes: Box<[u8]>,
        len: usize,
        skips: bool,
        df: u32,
        containers: u64,
        tf_len: u64,
    ) -> Option<Self> {
        let head = varint_len(len as u64);
        let skip_bytes = if skips {
            skip_count(len) * SKIP_BYTES
        } else {
            0
        };
        // An entry takes at least four bytes (a gap, a count and kind, a
        // frontier's head and first length).
        let end = bytes.len().checked_sub(skip_bytes)?;
        if end < head + 4 * len {
            return None;
        }
        Some(Self {
            entries: (0..len)
                .map(|_| std::cell::UnsafeCell::new(std::mem::MaybeUninit::uninit()))
                .collect(),
            done: (0..len.div_ceil(DIRECTORY_SKIP).div_ceil(64))
                .map(|_| std::cell::Cell::new(0))
                .collect(),
            decoded: std::cell::Cell::new(0),
            head,
            end,
            skips,
            resume: std::cell::Cell::new(Resume {
                at: head,
                ..Resume::default()
            }),
            bytes,
            df,
            containers,
            tf_len,
        })
    }

    /// Entries (groups the record holds).
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The directory's bytes, from its group count on.
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether run `r`'s entries are decoded.
    #[inline(always)]
    fn is_done(&self, r: usize) -> bool {
        // SAFETY: a run below the entries' runs, which `done` has a bit for.
        let word = unsafe { self.done.get_unchecked(r / 64) }.get();
        word >> (r % 64) & 1 == 1
    }

    /// Entry `b`, its run decoded first if it was not; `groups` is the
    /// segment's ([`Geometry::groups`]).
    #[inline]
    pub fn get(&self, b: usize, groups: &[Group]) -> Result<&GroupEntry> {
        if b < self.len() && self.is_done(b / DIRECTORY_SKIP) {
            // SAFETY: `b`'s run is decoded: its slots are written, once,
            // never again.
            return Ok(unsafe { (*self.entries.get_unchecked(b).get()).assume_init_ref() });
        }
        self.get_slow(b, groups)
    }

    #[inline(never)]
    fn get_slow(&self, b: usize, groups: &[Group]) -> Result<&GroupEntry> {
        if b >= self.len() {
            return Err(Error::Corrupt("group directory entry"));
        }
        let r = b / DIRECTORY_SKIP;
        if self.skips {
            // A walk reaching the run after one it decoded reads on, as a
            // rule: the runs after it too, a few, in one pass (a run's own
            // costs as much to set up as to decode); else the run alone.
            let k = if r > 0 && self.is_done(r - 1) {
                self.undecoded_from(r, SEQUENTIAL_RUNS)
            } else {
                1
            };
            self.decode_runs(groups, r, k)?;
        } else {
            // Without skip points, the runs before it first, in order.
            let first = (0..=r).find(|&k| !self.is_done(k)).unwrap_or(r);
            let k = (r + 1 - first).max(self.undecoded_from(first, SEQUENTIAL_RUNS));
            self.decode_runs(groups, first, k)?;
        }
        self.get(b, groups)
    }

    /// Decodes every entry not decoded yet: for a walk that will reach them
    /// all (a disjunction bounds every group of each term it scores).
    pub fn decode_all(&self, groups: &[Group]) -> Result<()> {
        let runs = self.len().div_ceil(DIRECTORY_SKIP);
        let mut r = 0;
        while self.decoded.get() < self.len() && r < runs {
            let k = self.undecoded_from(r, usize::MAX);
            if k > 0 {
                self.decode_runs(groups, r, k)?;
            }
            r += k.max(1);
        }
        Ok(())
    }

    /// The first entry from `from` on whose group index is `index` or more,
    /// or the entry count if none is: the entries of `from`'s run on, then
    /// the skip points' group indexes past it (the runs they pass over are
    /// not decoded), galloping.
    #[inline]
    pub fn seek(&self, index: u32, from: usize, groups: &[Group]) -> Result<usize> {
        let n = self.len();
        let mut b = from;
        while b < n {
            let r = b / DIRECTORY_SKIP;
            if !self.is_done(r) {
                self.get_slow(b, groups)?;
            }
            // Run `r`, or every entry from `b` on once all are decoded (a
            // disjunction's directory is decoded whole).
            let end = if self.decoded.get() == n {
                n
            } else {
                ((r + 1) * DIRECTORY_SKIP).min(n)
            };
            // SAFETY: runs `r..end.div_ceil(DIRECTORY_SKIP)` are decoded:
            // their slots are written, once, and laid out as their entries.
            let run = unsafe {
                std::slice::from_raw_parts(
                    self.entries.as_ptr().add(b).cast::<GroupEntry>(),
                    end - b,
                )
            };
            if let Some(i) = run.iter().position(|e| e.index >= index) {
                return Ok(b + i);
            }
            b = end;
            // Past a run not decoded, by the skip points (a decoded one is
            // scanned on).
            if self.skips && b < n && !self.is_done(b / DIRECTORY_SKIP) {
                b = self.skip_past(b / DIRECTORY_SKIP, index) * DIRECTORY_SKIP;
            }
        }
        Ok(n)
    }

    /// From run `r` (whose entries follow one below `index`), the furthest
    /// run whose skip point follows a group below `index`: the first entry
    /// at `index` or past it lies in that run, at the latest.
    #[inline]
    fn skip_past(&self, r: usize, index: u32) -> usize {
        let runs = skip_count(self.len()) + 1;
        let below = |r: usize| self.skip_after(r) <= u64::from(index);
        if r + 1 >= runs || !below(r + 1) {
            return r;
        }
        // `lo` passes, `hi` fails or is past the last run.
        let (mut lo, mut step) = (r + 1, 1);
        let mut hi = loop {
            let at = lo + step;
            if at >= runs || !below(at) {
                break at.min(runs);
            }
            lo = at;
            step *= 2;
        };
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if below(mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Field `k` of skip point `r` (one past a run's first entry), unchecked.
    #[inline(always)]
    fn skip_field(&self, r: usize, k: usize) -> u64 {
        let at = self.end + (r - 1) * SKIP_BYTES + 4 * k;
        u64::from(u32::from_le_bytes(
            self.bytes[at..at + 4].try_into().expect("four bytes"),
        ))
    }

    /// The group index after the last entry before run `r` (`r` past 0).
    #[inline(always)]
    fn skip_after(&self, r: usize) -> u64 {
        self.skip_field(r, 1)
    }

    /// Where decoding stands before run `r`: the start, or its skip point
    /// (whose offset must lie among the entries); without skip points,
    /// where the run before it ended (which must be decoded).
    fn start_of(&self, r: usize) -> Result<Resume> {
        if r == 0 {
            return Ok(Resume {
                at: self.head,
                ..Resume::default()
            });
        }
        if !self.skips {
            return Ok(self.resume.get());
        }
        let at = self.head + self.skip_field(r, 0) as usize;
        if at >= self.end {
            return Err(Error::Corrupt("group directory skip"));
        }
        Ok(Resume {
            at,
            after: self.skip_field(r, 1),
            body: self.skip_field(r, 2),
            first: self.skip_field(r, 3),
            tf_bit: self.skip_field(r, 4),
        })
    }

    /// The entry holding posting `index` (one of the record's).
    pub fn entry_of_posting(&self, index: u32, groups: &[Group]) -> Result<&GroupEntry> {
        // The last run whose first posting is `index` or before it, then
        // the last entry of it starting there or before.
        let runs = if self.skips {
            skip_count(self.len()) + 1
        } else {
            // Its runs' first entries, decoding in order.
            let mut r = 0;
            while (r + 1) * DIRECTORY_SKIP < self.len()
                && self.get((r + 1) * DIRECTORY_SKIP, groups)?.first <= index
            {
                r += 1;
            }
            return self.posting_in_run(r, index, groups);
        };
        let (mut lo, mut hi) = (0, runs);
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if self.skip_field(mid, 3) <= u64::from(index) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        self.posting_in_run(lo, index, groups)
    }

    /// The entry of run `r` holding posting `index`.
    fn posting_in_run(&self, r: usize, index: u32, groups: &[Group]) -> Result<&GroupEntry> {
        let from = r * DIRECTORY_SKIP;
        let to = (from + DIRECTORY_SKIP).min(self.len());
        let mut found = None;
        for b in from..to {
            let e = self.get(b, groups)?;
            if e.first > index {
                break;
            }
            found = Some(e);
        }
        match found {
            Some(e) if index < e.first + e.count => Ok(e),
            _ => Err(Error::Corrupt("posting index")),
        }
    }

    /// The entry of group `index`, if the record holds the group.
    pub fn find(&self, index: u32, groups: &[Group]) -> Result<Option<&GroupEntry>> {
        let b = self.seek(index, 0, groups)?;
        if b == self.len() {
            return Ok(None);
        }
        let entry = self.get(b, groups)?;
        Ok((entry.index == index).then_some(entry))
    }

    /// Every entry, in order (each run decoded as it is reached).
    pub fn each(&self, groups: &[Group], mut visit: impl FnMut(&GroupEntry)) -> Result<()> {
        for b in 0..self.len() {
            visit(self.get(b, groups)?);
        }
        Ok(())
    }

    /// Every entry, decoded.
    pub fn entries(&self, groups: &[Group]) -> Result<Vec<GroupEntry>> {
        let mut out = Vec::with_capacity(self.len());
        self.each(groups, |e| out.push(*e))?;
        Ok(out)
    }

    /// Entries decoded so far.
    #[inline]
    pub fn decoded(&self) -> usize {
        self.decoded.get()
    }

    /// Bytes the directory holds, roughly: its bytes, and its entries
    /// decoded so far (the slots past them untouched).
    pub fn held(&self) -> usize {
        self.bytes.len() + self.decoded() * std::mem::size_of::<GroupEntry>()
    }

    /// Decodes runs `r..r + k` (none of them decoded) in one pass, from the
    /// skip point before `r`, and checks where they end against the next
    /// skip point (or the last run against the record).
    fn decode_runs(&self, groups: &[Group], r: usize, k: usize) -> Result<()> {
        let from = r * DIRECTORY_SKIP;
        let n = (k * DIRECTORY_SKIP).min(self.len() - from);
        let last = from + n == self.len();
        let mut resume = self.start_of(r)?;
        let mut out = Slots {
            slots: &self.entries[from..from + n],
            len: 0,
        };
        // The runs end where the next skip point says (scanned exactly so),
        // or where the entries do.
        let next = if last || !self.skips {
            None
        } else {
            Some(self.start_of(r + k)?)
        };
        let end = next.map_or(self.end, |s| s.at);
        if end < resume.at {
            return Err(Error::Corrupt("group directory skip"));
        }
        let bytes = &self.bytes[..end];
        UNITS.with_borrow_mut(|units| {
            decode_entries(
                bytes,
                self.skips,
                &mut resume,
                n,
                &mut out,
                groups,
                self,
                units,
            )
        })?;
        match next {
            None if last => {
                // The last: the record's totals.
                if resume.first != u64::from(self.df)
                    || resume.body != self.containers
                    || resume.at != self.end
                {
                    return Err(Error::Corrupt("postings payload length"));
                }
                if resume.tf_bit.div_ceil(8) != self.tf_len {
                    return Err(Error::Corrupt("postings TF tail length"));
                }
            }
            None => self.resume.set(resume),
            Some(next) if resume != next => return Err(Error::Corrupt("group directory skip")),
            Some(_) => {}
        }
        super::segment::count_memo(|c| c.directory_entries += n as u64);
        for run in r..r + k {
            let word = &self.done[run / 64];
            word.set(word.get() | 1 << (run % 64));
        }
        self.decoded.set(self.decoded.get() + n);
        Ok(())
    }

    /// Runs not decoded from `r` on, up to `most`.
    #[inline]
    fn undecoded_from(&self, r: usize, most: usize) -> usize {
        let runs = self.len().div_ceil(DIRECTORY_SKIP);
        let mut k = 0;
        while k < most && r + k < runs && !self.is_done(r + k) {
            k += 1;
        }
        k
    }
}

/// The slots a run decodes into: written in order, none of them referred
/// to until the run is done.
struct Slots<'d> {
    slots: &'d [std::cell::UnsafeCell<std::mem::MaybeUninit<GroupEntry>>],
    len: usize,
}

impl Slots<'_> {
    #[inline(always)]
    fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    fn push(&mut self, entry: GroupEntry) {
        // SAFETY: a slot past the directory's `decoded`, which no reference
        // reaches (they are handed out below it only).
        unsafe { (*self.slots[self.len].get()).write(entry) };
        self.len += 1;
    }
}

thread_local! {
    /// Scratch of the directory scans ([`decode_entries`]).
    static UNITS: std::cell::RefCell<Units> = std::cell::RefCell::new(Units::default());
}

/// Decodes `want` entries of a directory's `bytes` from `resume` on, into
/// `out`, checking each against `dir`'s record; `resume` is left after them.
/// With `exact`, the entries wanted end where `bytes` do.
#[allow(clippy::too_many_arguments)]
fn decode_entries(
    bytes: &[u8],
    exact: bool,
    resume: &mut Resume,
    want: usize,
    out: &mut Slots<'_>,
    groups: &[Group],
    dir: &Directory,
    units: &mut Units,
) -> Result<()> {
    let target = out.len() + want;
    // Scanned as far as the entries wanted take at the directory's bytes per
    // entry and a quarter more (or, exact, to their end): the entry the scan
    // ends within is scanned again with the next chunk.
    let per = bytes.len().div_ceil(dir.len().max(1));
    while out.len() < target {
        let left = target - out.len();
        let start = resume.at;
        let scan = if exact {
            usize::MAX
        } else {
            left.saturating_mul(per + per.div_ceil(4))
                .saturating_add(32)
        };
        let end = bytes.len().min(start.saturating_add(scan.min(UNITS_CHUNK)));
        let before = out.len();
        decode_chunk(
            &bytes[start..end],
            start,
            resume,
            target,
            out,
            groups,
            dir,
            units,
        )?;
        if out.len() == before {
            // An entry longer than a chunk: a varint at a time.
            let mut p = resume.at;
            let previous = resume.after.checked_sub(1);
            let (entry, len) =
                decode_entry(bytes, &mut p, 0, previous, groups).map_err(|e| match e {
                    Error::Truncated => Error::Corrupt("group directory entry"),
                    e => e,
                })?;
            push_entry(resume, entry, len, p, out, dir)?;
        }
    }
    Ok(())
}

/// Appends `entry` (its container of `len` bytes, the entry ending at `end`
/// of the directory) after the entries so far, with their totals, checked
/// against the record.
#[inline(always)]
fn push_entry(
    resume: &mut Resume,
    entry: GroupEntry,
    len: u64,
    end: usize,
    out: &mut Slots<'_>,
    dir: &Directory,
) -> Result<()> {
    let count = u64::from(entry.count);
    let tf_bits = count * u64::from(entry.width);
    if resume.body + len > dir.containers
        || resume.first + count > u64::from(dir.df)
        || (resume.tf_bit + tf_bits).div_ceil(8) > dir.tf_len
    {
        return Err(Error::Corrupt("group directory entry"));
    }
    out.push(GroupEntry {
        at: resume.body as u32,
        first: resume.first as u32,
        tf_bit: resume.tf_bit as u32,
        ..entry
    });
    resume.after = u64::from(entry.index) + 1;
    resume.body += len;
    resume.first += count;
    resume.tf_bit += tf_bits;
    resume.at = end;
    Ok(())
}

/// The whole entries of one chunk of [`decode_entries`] (at offset `base`
/// of the directory), up to `target` in `out`.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn decode_chunk(
    bytes: &[u8],
    base: usize,
    resume: &mut Resume,
    target: usize,
    out: &mut Slots<'_>,
    groups: &[Group],
    dir: &Directory,
    units: &mut Units,
) -> Result<()> {
    units.scan(bytes);
    let n = units.n;
    let (at, first, step) = (&units.at[..], &units.first[..], &units.step[..]);
    // The scan's vectors hold `UNITS_SLACK` past its `n` varints, whose
    // starts rise strictly and lie within `bytes`.
    debug_assert!(at.len() >= n + UNITS_SLACK && first.len() >= n + UNITS_SLACK);
    debug_assert!(at[..n].windows(2).all(|w| w[0] < w[1]));
    debug_assert!(n == 0 || usize::from(at[n - 1]) <= bytes.len());
    let mut u = 0usize;
    while out.len() < target && u + 4 <= n {
        // SAFETY (each read of `at`, `first`, `step` and `bytes` below): `u
        // + 4 <= n` and `next < n`, and the entry's varints lie between
        // `at[u]` and `at[next]` (at most the bytes' length): each varint
        // read (the gap, the count and kind, a paged length) is followed by
        // another of the entry, so its second byte lies before `at[next]`
        // too; the deltas' eight first bytes lie within `n + 8`.
        let next = u + usize::from(unsafe { *step.get_unchecked(u) });
        if next >= n {
            break;
        }
        let f: [u8; 4] = unsafe { *first.as_ptr().add(u).cast::<[u8; 4]>() };
        let w: [u16; 4] = unsafe { [0, 1, 2, 3].map(|i| *at.get_unchecked(u + i)) };
        let (a0, a1, a2, a3) = (
            usize::from(w[0]),
            usize::from(w[1]),
            usize::from(w[2]),
            usize::from(w[3]),
        );
        let kind = f[1] & 3;
        let paged = usize::from(kind == KIND_PAGED);
        let head_at = if paged == 1 { a3 } else { a2 };
        let head = usize::from(if paged == 1 { f[3] } else { f[2] });
        let end = usize::from(unsafe { *at.get_unchecked(next) });
        // The gap, count and kind, and a paged length, of at most two bytes
        // each, no varint of five bytes or more (a frontier length past 32
        // bits) and at most eight pairs: else a varint at a time.
        let longest = (a1 - a0).max(a2 - a1).max((a3 - a2) * paged);
        let (entry, len) = if longest > 2 || head >= 0x80 || end > units.long_at {
            let mut p = a0;
            let decoded = decode_entry(bytes, &mut p, base, resume.after.checked_sub(1), groups)?;
            if p != end {
                return Err(Error::Corrupt("group directory entry"));
            }
            decoded
        } else {
            let index = resume.after + u64::from(unsafe { varint2(bytes, a0) });
            let Some(group) = groups.get(index as usize) else {
                return Err(Error::Corrupt("postings group index"));
            };
            let count = (unsafe { varint2(bytes, a1) } >> 2) + 1;
            if count > group.slots() || kind > KIND_PAGED {
                return Err(Error::Corrupt("postings group count"));
            }
            // Each kind's length, then the entry's: the kinds mix within a
            // directory, so a branch on them would miss.
            let grid = group.grid_bytes() as u64;
            let ef = ef::encoded_len_branchless(count, group.slots()) as u64;
            let len = if kind == KIND_GRID { grid } else { ef };
            let paged_len = u64::from(unsafe { varint2(bytes, a2) });
            let len = if paged == 1 { paged_len } else { len };
            // The frontier's deltas (at most seven) follow its head and first
            // length; each adds one more than its low four bits, in its first
            // byte, to the bucket: summed eight lanes of a word at once.
            const ONES: u64 = 0x0101_0101_0101_0101;
            let deltas = u + 4 + paged;
            let lanes =
                u64::from_le_bytes(unsafe { *first.as_ptr().add(deltas).cast::<[u8; 8]>() });
            let keep = (1u64 << (8 * (head >> 4))).wrapping_sub(1);
            let sum = (((lanes & (15 * ONES)) + ONES) & keep).wrapping_mul(ONES) >> 56;
            let max_bucket = (head & 15) + sum as usize;
            if max_bucket > 15 {
                return Err(Error::Corrupt("group frontier"));
            }
            let max_bucket = max_bucket as u8;
            (
                GroupEntry {
                    index: index as u32,
                    count,
                    kind,
                    at: 0,
                    len: len as u32,
                    first: 0,
                    frontier: (base + head_at) as u32,
                    tf_bit: 0,
                    width: TF_WIDTH[usize::from(max_bucket)],
                    max_bucket,
                },
                len,
            )
        };
        push_entry(resume, entry, len, base + end, out, dir)?;
        u = next;
    }
    Ok(())
}

/// The value of the varint of one or two bytes at `at`.
///
/// # Safety
///
/// `at + 1` lies within `bytes`.
#[inline(always)]
unsafe fn varint2(bytes: &[u8], at: usize) -> u32 {
    debug_assert!(at + 1 < bytes.len());
    // SAFETY: the caller's.
    let (b0, b1) = unsafe { (*bytes.get_unchecked(at), *bytes.get_unchecked(at + 1)) };
    let (b0, b1) = (u32::from(b0), u32::from(b1));
    (b0 & 0x7f) | ((b1 & 0x7f) << 7) & (bits::mask(b0 >= 0x80) as u32)
}

/// Decodes the directory entry at `at` of `bytes` (offset `base` of the
/// directory), of the group after `previous`: the entry without the totals
/// before it (its `at`, `first` and `tf_bit`, the caller's), and its
/// container's length. `Truncated` when the bytes end within it.
fn decode_entry(
    bytes: &[u8],
    at: &mut usize,
    base: usize,
    previous: Option<u64>,
    groups: &[Group],
) -> Result<(GroupEntry, u64)> {
    let gap = varint::get(bytes, at)?;
    let next = match previous {
        None => gap,
        Some(i) => i + 1 + gap,
    };
    if next >= groups.len() as u64 {
        return Err(Error::Corrupt("postings group index"));
    }
    let group = groups[next as usize];
    let packed = varint::get(bytes, at)?;
    let kind = (packed & 3) as u8;
    let count = (packed >> 2) + 1;
    if count > u64::from(group.slots()) {
        return Err(Error::Corrupt("postings group count"));
    }
    let len = match kind {
        KIND_GRID => group.grid_bytes() as u64,
        KIND_EF => ef::encoded_len(count as usize, group.slots()) as u64,
        KIND_PAGED => varint::get(bytes, at)?,
        _ => return Err(Error::Corrupt("postings group kind")),
    };
    let frontier = (base + *at) as u32;
    let max_bucket = skip_group_frontier(bytes, at)?;
    let width = tf_width(max_bucket, true);
    Ok((
        GroupEntry {
            index: next as u32,
            count: count as u32,
            kind,
            at: 0,
            len: len as u32,
            first: 0,
            frontier,
            tf_bit: 0,
            width: width as u8,
            max_bucket,
        },
        len,
    ))
}

/// Every entry of a grouped record's directory decoded a varint at a time,
/// from `bytes` (the directory's, from its group count on, through its skip
/// points, each checked against where decoding stands): the reference a
/// [`Directory`]'s runs are tested against.
#[doc(hidden)]
pub fn decode_directory_whole(bytes: &[u8], groups: &[Group]) -> Result<Vec<GroupEntry>> {
    let mut at = 0;
    let n = varint::get_u32(bytes, &mut at)? as usize;
    let head = at;
    let end = bytes
        .len()
        .checked_sub(skip_count(n) * SKIP_BYTES)
        .ok_or(Error::Corrupt("group directory length"))?;
    let mut out = Vec::with_capacity(n);
    let (mut previous, mut body, mut first, mut tf_bit) = (None, 0u64, 0u64, 0u64);
    let mut skips = Vec::new();
    for i in 0..n {
        if i > 0 && i.is_multiple_of(DIRECTORY_SKIP) {
            put_skip(
                &mut skips,
                &Resume {
                    at: at - head,
                    after: previous.map_or(0, |p: u64| p + 1),
                    body,
                    first,
                    tf_bit,
                },
            );
        }
        let (entry, len) = decode_entry(&bytes[..end], &mut at, 0, previous, groups)?;
        out.push(GroupEntry {
            at: body as u32,
            first: first as u32,
            tf_bit: tf_bit as u32,
            ..entry
        });
        previous = Some(u64::from(entry.index));
        body += len;
        first += u64::from(entry.count);
        tf_bit += u64::from(entry.count) * u64::from(entry.width);
    }
    if at != end || bytes[end..] != skips[..] {
        return Err(Error::Corrupt("group directory length"));
    }
    Ok(out)
}

/// A run of varints (a directory's) as where each starts, read without a
/// branch on their lengths: a varint starts after a byte below 0x80.
#[derive(Default)]
struct Units {
    /// Where each varint starts (scanned runs are at most [`UNITS_CHUNK`]
    /// bytes), and the end of the bytes when the last one ends there (as
    /// the start of one more): `n` of them.
    at: Vec<u16>,
    /// Each one's first byte (an entry's kind and a frontier's head are
    /// read there).
    first: Vec<u8>,
    /// Per varint, the varints an entry starting there takes: a walk from
    /// entry to entry adds them up, one dependent load an entry where
    /// reading the entry's bytes took several. An entry is a gap, a count
    /// and kind, then a paged container's length, then the frontier's head
    /// (its pairs less one in the high four bits), its first length and a
    /// delta per further pair: its length follows from the first bytes of
    /// its second to fourth varints. A head of nine pairs or more (above
    /// 0x7F) reads as one varint with the first length.
    step: Vec<u8>,
    n: usize,
    /// At or before where the first varint of five bytes or more lies
    /// (`usize::MAX` when none does): a chunk's last bytes are often past
    /// the directory's end, in containers, where such runs are common.
    long_at: usize,
}

/// `COMPACT[m]`: the positions of byte `m`'s set bits, ascending, a byte
/// each, the rest 0xFF (which a table lookup reads as zero).
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
static COMPACT: [u64; 256] = {
    let mut table = [u64::MAX; 256];
    let mut m = 0;
    while m < 256 {
        let mut k = 0;
        let mut bit = 0;
        while bit < 8 {
            if m & (1 << bit) != 0 {
                table[m] &= !(0xFF << (8 * k));
                table[m] |= (bit as u64) << (8 * k);
                k += 1;
            }
            bit += 1;
        }
        m += 1;
    }
    table
};

/// Slots the scan writes past what it keeps, and the steps read past it, at
/// most.
const UNITS_SLACK: usize = 32;

impl Units {
    /// Scans `bytes` (at most [`UNITS_CHUNK`] of them): where each varint
    /// starts, its first byte, the entry steps, and where a long varint lies.
    #[inline(never)]
    fn scan(&mut self, bytes: &[u8]) {
        debug_assert!(bytes.len() <= UNITS_CHUNK);
        let need = bytes.len() + UNITS_SLACK + 4;
        if self.at.len() < need {
            // Zeroed once: the scan writes past what it keeps.
            self.at.resize(need, 0);
            self.first.resize(need, 0);
            self.step.resize(need, 0);
        }
        let mut state = ScanState {
            k: 0,
            start: true,
            run: 0,
            long_at: usize::MAX,
        };
        #[cfg(target_arch = "aarch64")]
        // SAFETY: NEON is baseline on aarch64; the vectors are long enough
        // for the scan's writes past `k` (`UNITS_SLACK`).
        let j = unsafe { self.scan_neon(bytes, &mut state) };
        #[cfg(target_arch = "x86_64")]
        let j = if std::is_x86_feature_detected!("ssse3") {
            // SAFETY: the CPU has SSSE3 (asked above); the vectors are long
            // enough for the scan's writes past `k` (`UNITS_SLACK`).
            unsafe { self.scan_ssse3(bytes, &mut state) }
        } else {
            0
        };
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        let j = 0;
        self.scan_bytes(bytes, j, &mut state);
        let k = state.k;
        self.at[k] = bytes.len() as u16;
        self.first[k] = 0;
        self.n = k + usize::from(state.start);
        self.long_at = state.long_at;
        self.steps();
    }

    /// [`Self::step`] for every varint (from the first bytes, which past
    /// the last varint read as zero or whatever an earlier scan left: steps
    /// there are not taken).
    #[inline(always)]
    fn steps(&mut self) {
        #[cfg(target_arch = "aarch64")]
        // SAFETY: NEON is baseline on aarch64; the vectors hold
        // `UNITS_SLACK` past the varints, more than a step reads past them.
        unsafe {
            use core::arch::aarch64::*;
            let (first, step) = (self.first.as_ptr(), self.step.as_mut_ptr());
            for u in (0..self.n).step_by(16) {
                let second = vld1q_u8(first.add(u + 1));
                let paged = vceqq_u8(vandq_u8(second, vdupq_n_u8(3)), vdupq_n_u8(KIND_PAGED));
                let head = vbslq_u8(
                    paged,
                    vld1q_u8(first.add(u + 3)),
                    vld1q_u8(first.add(u + 2)),
                );
                // 4 + paged + (head >> 4) - (head >> 7), wrapping per lane.
                let more = vsubq_u8(vandq_u8(paged, vdupq_n_u8(1)), vshrq_n_u8::<7>(head));
                let lanes = vaddq_u8(vaddq_u8(vdupq_n_u8(4), vshrq_n_u8::<4>(head)), more);
                vst1q_u8(step.add(u), lanes);
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            // Eight at a time, a byte each in a word: kinds and heads are
            // small, so no lane carries into the next.
            const ONES: u64 = 0x0101_0101_0101_0101;
            let word =
                |at: usize| u64::from_le_bytes(self.first[at..at + 8].try_into().expect("8"));
            for u in (0..self.n).step_by(8) {
                let (second, third, fourth) = (word(u + 1), word(u + 2), word(u + 3));
                // A lane's kind is paged when its low two bits are 2: one if so.
                let kind = (second & (3 * ONES)) ^ (2 * ONES);
                let paged = !(kind | kind >> 1) & ONES;
                let head = (fourth & (paged * 0xFF)) | (third & !(paged * 0xFF));
                let step = 4 * ONES + paged + ((head >> 4) & (15 * ONES)) - ((head >> 7) & ONES);
                self.step[u..u + 8].copy_from_slice(&step.to_le_bytes());
            }
        }
    }

    /// The bytes from `from` on, one at a time.
    #[inline(always)]
    fn scan_bytes(&mut self, bytes: &[u8], from: usize, s: &mut ScanState) {
        for (j, &b) in bytes.iter().enumerate().skip(from) {
            // Written at every byte, kept at a varint's first: `k` moves on
            // past it only then.
            self.at[s.k] = j as u16;
            self.first[s.k] = b;
            s.k += usize::from(s.start);
            s.run = (s.run + 1) & !(bits::mask(s.start) as u32);
            if s.run >= 4 {
                s.long_at = s.long_at.min(j.saturating_sub(4));
            }
            s.start = b < 0x80;
        }
    }

    /// The bytes sixteen at a time; where it stopped.
    #[cfg(target_arch = "aarch64")]
    #[inline(always)]
    unsafe fn scan_neon(&mut self, bytes: &[u8], s: &mut ScanState) -> usize {
        // SAFETY: the caller's (see `scan`).
        unsafe {
            use core::arch::aarch64::*;
            let (at, first) = (self.at.as_mut_ptr(), self.first.as_mut_ptr());
            let mut k = s.k;
            // Whether the previous byte ended a varint (the first starts
            // one), and the continuation bytes the previous block ended with,
            // as its bits (bit 15 its last byte's).
            let mut carry = u32::from(s.start);
            let mut cont: u32 = 0;
            let mut long_at = s.long_at;
            let mut j = 0;
            while j + 16 <= bytes.len() {
                let p = bytes.as_ptr().add(j);
                let lo = u64::from_le_bytes(*p.cast::<[u8; 8]>());
                let hi = u64::from_le_bytes(*p.add(8).cast::<[u8; 8]>());
                let ends = varint_ends(lo) | varint_ends(hi) << 8;
                let starts = (ends << 1 | carry) & 0xFFFF;
                carry = ends >> 15;
                // Four continuation bytes in a row: a varint of five or more
                // (from bit 16 on this block's bytes, below it the last's).
                let c = cont | (!ends & 0xFFFF) << 16;
                let run = c & c >> 1 & c >> 2 & c >> 3;
                let run_at = (j + run.trailing_zeros() as usize).saturating_sub(16);
                long_at = long_at.min(if run == 0 { usize::MAX } else { run_at });
                cont = c >> 16;
                for (half, word) in [(0usize, lo), (8, hi)] {
                    // The positions of the varints starting in the half, and
                    // their first bytes.
                    let m = (starts >> half) & 0xFF;
                    let idx = vcreate_u8(COMPACT[m as usize]);
                    let pos = vaddq_u16(vmovl_u8(idx), vdupq_n_u16((j + half) as u16));
                    vst1q_u16(at.add(k), pos);
                    vst1_u8(first.add(k), vtbl1_u8(vcreate_u8(word), idx));
                    k += m.count_ones() as usize;
                }
                j += 16;
            }
            s.k = k;
            s.start = carry == 1;
            // The continuation bytes the last block ended with, for the next.
            s.run = (cont << 16).leading_ones();
            s.long_at = long_at;
            j
        }
    }
}

impl Units {
    /// [`Self::scan_neon`] with SSSE3 (`pshufb` the compacting table
    /// lookup, `pmovmskb` the bytes that end a varint); where it stopped.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "ssse3")]
    unsafe fn scan_ssse3(&mut self, bytes: &[u8], s: &mut ScanState) -> usize {
        use core::arch::x86_64::*;
        let (at, first) = (self.at.as_mut_ptr(), self.first.as_mut_ptr());
        let mut k = s.k;
        let mut carry = u32::from(s.start);
        let mut cont: u32 = 0;
        let mut long_at = s.long_at;
        let mut j = 0;
        while j + 16 <= bytes.len() {
            // SAFETY: 16 bytes from `j` lie within `bytes`; the writes at
            // `k` lie within the vectors' slack (see `scan`).
            unsafe {
                let raw = _mm_loadu_si128(bytes.as_ptr().add(j).cast());
                let ends = !(_mm_movemask_epi8(raw) as u32) & 0xFFFF;
                let starts = (ends << 1 | carry) & 0xFFFF;
                carry = ends >> 15;
                let c = cont | (!ends & 0xFFFF) << 16;
                let run = c & c >> 1 & c >> 2 & c >> 3;
                let run_at = (j + run.trailing_zeros() as usize).saturating_sub(16);
                long_at = long_at.min(if run == 0 { usize::MAX } else { run_at });
                cont = c >> 16;
                for half in [0usize, 8] {
                    let m = (starts >> half) & 0xFF;
                    let idx = _mm_cvtsi64_si128(COMPACT[m as usize] as i64);
                    let pos = _mm_add_epi16(
                        _mm_unpacklo_epi8(idx, _mm_setzero_si128()),
                        _mm_set1_epi16((j + half) as i16),
                    );
                    _mm_storeu_si128(at.add(k).cast(), pos);
                    // The half's lanes: past the first half, its indices
                    // move up by eight (an unused lane's 0xFF stays negative
                    // or wraps into a lane past `k`, unread).
                    let lanes = _mm_add_epi8(idx, _mm_set1_epi8(half as i8));
                    _mm_storel_epi64(first.add(k).cast(), _mm_shuffle_epi8(raw, lanes));
                    k += m.count_ones() as usize;
                }
            }
            j += 16;
        }
        s.k = k;
        s.start = carry == 1;
        s.run = (cont << 16).leading_ones();
        s.long_at = long_at;
        j
    }
}

/// Where [`Units::scan`] is in its bytes.
struct ScanState {
    k: usize,
    start: bool,
    run: u32,
    long_at: usize,
}

/// The bits of a little-endian word's bytes that end a varint (below 0x80):
/// bit `i` for byte `i`.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn varint_ends(word: u64) -> u32 {
    let t = (!word & 0x8080_8080_8080_8080) >> 7;
    (t.wrapping_mul(0x0102_0408_1020_4080) >> 56) as u32
}

/// Calls `visit` with every member of a group container, as local slots in
/// order; `starts` is the group's page starts ([`Geometry::starts`]: its
/// slot count last).
#[inline]
pub fn for_each_local(
    entry: &GroupEntry,
    bytes: &[u8],
    starts: &[u32],
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
            Ef::parse(bytes, entry.count as usize, group_slots(starts))?.for_each(visit);
            Ok(())
        }
        _ => paged_for_each(bytes, starts, visit),
    }
}

/// A group's slot count, from its page starts.
#[inline]
pub fn group_slots(starts: &[u32]) -> u32 {
    starts[starts.len() - 1]
}

/// Sets every member of a group container in `words`, a bitmap over the
/// group's slots.
#[inline]
pub fn or_into(entry: &GroupEntry, bytes: &[u8], starts: &[u32], words: &mut [u64]) -> Result<()> {
    if entry.kind == KIND_GRID {
        for (i, word) in words.iter_mut().enumerate() {
            *word |= bits::word(bytes, i);
        }
        return Ok(());
    }
    for_each_local(entry, bytes, starts, |local| {
        words[local as usize / 64] |= 1 << (local % 64);
    })
}

/// A paged container's members; each page's offsets are at the bits the
/// page's own width needs.
fn paged_for_each(bytes: &[u8], starts: &[u32], mut visit: impl FnMut(u32)) -> Result<()> {
    let span = starts.len() - 1;
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
        let page = usize::from(page);
        if page >= span {
            return Err(Error::Corrupt("paged page"));
        }
        let base = starts[page];
        let width = starts[page + 1] - base;
        let ob = bits::width_below(width);
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

/// A record's score metadata decoded whole: per block (a sparse record's
/// footer blocks; a grouped record's groups, one block per directory
/// entry), its last slot, its frontier and where its buckets start in the
/// TF tail.
#[derive(Clone, Debug, Default)]
pub struct Footer {
    pub block_size: u32,
    pub last: Vec<u32>,
    /// Block `b`'s frontier is `frontier[starts[b]..starts[b + 1]]`.
    pub starts: Vec<u32>,
    pub frontier: Vec<(u8, u32)>,
    /// Block `b`'s buckets start at bit `tf_at[b]` of the TF tail, packed
    /// at `widths[b]` bits.
    pub tf_at: Vec<u32>,
    pub widths: Vec<u8>,
    /// Where each block's footer entry starts within the footer (a sparse
    /// record), for page accounting.
    pub entry_at: Vec<u32>,
    /// A grouped record's blocks' first posting indexes (empty for blocks
    /// of `block_size` postings).
    pub firsts: Vec<u32>,
    /// A single posting's bucket, which the dictionary keeps.
    pub single: Option<u8>,
}

impl Footer {
    fn parse(
        postings: &Postings<'_>,
        geometry: &Geometry,
        block_size: u32,
        max_bucket: u8,
        adaptive: bool,
    ) -> Result<Self> {
        let df = postings.df;
        if let Form::Grouped(dir) = &postings.form {
            let entries = dir.entries(&geometry.groups)?;
            // Each group's entry, decoded whole. `last` holds each block's
            // group index (the geometry turns it into the group's last
            // slot): the walks bound grouped records from their directory
            // entries, not through this.
            let mut footer = Self {
                block_size,
                last: Vec::with_capacity(entries.len()),
                starts: Vec::with_capacity(entries.len() + 1),
                frontier: Vec::with_capacity(entries.len() * 2),
                tf_at: Vec::with_capacity(entries.len()),
                widths: Vec::with_capacity(entries.len()),
                entry_at: Vec::new(),
                firsts: Vec::with_capacity(entries.len()),
                single: None,
            };
            let mut largest = 0u8;
            for e in entries.iter() {
                footer.starts.push(footer.frontier.len() as u32);
                footer.frontier.extend(postings.group_frontier(e));
                footer.tf_at.push(e.tf_bit);
                footer.widths.push(e.width);
                footer.firsts.push(e.first);
                // The group's index: a reader with the geometry turns it
                // into the group's last slot ([`Self::group_ends`]).
                footer.last.push(e.index);
                largest = largest.max(e.max_bucket);
            }
            footer.starts.push(footer.frontier.len() as u32);
            if largest != max_bucket {
                return Err(Error::Corrupt("postings largest bucket"));
            }
            super::segment::count_memo(|c| {
                c.footer_blocks += entries.len() as u64;
                c.footer_bytes += dir.bytes().len() as u64;
            });
            return Ok(footer);
        }
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
                firsts: Vec::new(),
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
                firsts: Vec::new(),
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
            firsts: Vec::new(),
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
            footer.tf_at.push((tf * 8) as u32);
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

    /// The block holding posting `index`, and the posting's place in it.
    #[inline]
    fn block_of(&self, index: u32) -> Result<(usize, usize)> {
        if self.firsts.is_empty() {
            return Ok((
                (index / self.block_size) as usize,
                (index % self.block_size) as usize,
            ));
        }
        let b = self.firsts.partition_point(|f| *f <= index);
        if b == 0 {
            return Err(Error::Corrupt("posting index"));
        }
        Ok((b - 1, (index - self.firsts[b - 1]) as usize))
    }

    /// The bucket of posting `index`, from its record's TF tail.
    #[inline]
    pub fn bucket(&self, tf: Bytes<'_>, index: u32) -> Result<u8> {
        if let Some(bucket) = self.single {
            return Ok(bucket);
        }
        let (block, within) = self.block_of(index)?;
        let width = u32::from(
            *self
                .widths
                .get(block)
                .ok_or(Error::Corrupt("posting index"))?,
        );
        if width == 0 {
            return Ok(0);
        }
        let bit = self.tf_at[block] as usize + within * width as usize;
        let bytes = tf.window(bit / 8, 8)?;
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
    /// Whether the decoded blocks' frontiers are kept in `frontier`
    /// (`ends`); else (decoded for their bounds, [`Self::decode_bounds`])
    /// their entries' bytes are, in `raw`, each block's starting at
    /// `entries[b]`.
    kept: bool,
    raw: Vec<u8>,
    entries: Vec<u32>,
    /// Whether the footer is skimmed ([`Self::skim`]): its blocks' last
    /// slots, TF offsets and widths are kept, their frontiers not at all.
    skim: bool,
    /// A skimmed footer's blocks' largest buckets, or a footer's decoded
    /// for its bounds ([`Self::decode_bounds`]).
    maxb: Vec<u8>,
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

/// A pair's length whose varint starts with bytes `b0`, `b1`, when it takes
/// one byte or two (a length below 16,384, as a rule): its value and its
/// bytes, chosen by a [`bits::mask`], not a branch (one length takes one
/// byte, the next two, as the documents come); `None` for a longer one.
#[inline(always)]
fn short_length(b0: u8, b1: u8) -> Option<(u32, usize)> {
    if (b0 & b1) >= 0x80 {
        return None;
    }
    let two = b0 >> 7;
    let high = (u32::from(b1) << 7) & bits::mask(two == 1) as u32;
    Some((u32::from(b0 & 0x7f) | high, 1 + usize::from(two)))
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
        let length = if let Some((length, n)) = short_length(b0, b1) {
            at += 1 + n;
            length
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

/// A block's bound from its frontier: the best of `bound` over its pairs,
/// and 0 at least.
#[inline(always)]
fn fold_bound(pairs: &[(u8, u32)], bound: &mut impl FnMut(u8, u32) -> f32) -> f32 {
    pairs
        .iter()
        .map(|(bucket, length)| bound(*bucket, *length))
        .fold(0.0_f32, f32::max)
}

/// [`footer_entry_fast`] taking each pair's `bound` as it is read, rather
/// than keeping the pairs: the gap, the entry's length, its largest
/// bucket (its last pair's) and [`fold_bound`] of its pairs.
#[inline(always)]
fn footer_entry_bound(
    bytes: &[u8],
    bound: &mut impl FnMut(u8, u32) -> f32,
) -> Option<(u64, usize, u8, f32)> {
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
    let (mut best, mut last) = (0.0_f32, 0u8);
    for _ in 0..k {
        let bucket = b[at];
        if bucket > 15 {
            return None;
        }
        let (b0, b1) = (b[at + 1], b[at + 2]);
        // A length: one or two bytes as a rule, five at most.
        let length = if let Some((length, n)) = short_length(b0, b1) {
            at += 1 + n;
            length
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
                    return None;
                }
            }
            at += n;
            u32::try_from(value).ok()?
        };
        best = best.max(bound(bucket, length));
        last = bucket;
    }
    Some((gap, at, last, best))
}

/// [`footer_entry_fast`] keeping nothing of the pairs: the gap, the
/// entry's length and its largest bucket (its last pair's).
#[inline(always)]
fn footer_entry_skim(bytes: &[u8]) -> Option<(u64, usize, u8)> {
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
    let mut any = 0u8;
    let mut last = 0u8;
    for _ in 0..k {
        let bucket = b[at];
        any |= bucket;
        last = bucket;
        at += 1;
        if let Some((_, n)) = short_length(b[at], b[at + 1]) {
            at += n;
            continue;
        }
        // A length: five bytes at most, the last below 0x80.
        let mut n = 0;
        while b[at + n] >= 0x80 {
            n += 1;
            if n == 5 {
                return None;
            }
        }
        if n == 4 && b[at + 4] > 0x0f {
            return None;
        }
        at += n + 1;
    }
    if any > 15 {
        return None;
    }
    Some((gap, at, last))
}

impl<'a> LazyFooter<'a> {
    fn new(
        postings: &Postings<'a>,
        block_size: u32,
        max_bucket: u8,
        adaptive: bool,
    ) -> Result<Self> {
        if matches!(postings.form, Form::Grouped(_)) {
            // A grouped record's metadata is per group in its directory
            // (see the module's grammar): nothing to decode lazily.
            return Err(Error::Corrupt("a lazy footer of a grouped record"));
        }
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
            kept: true,
            raw: Vec::new(),
            entries: Vec::new(),
            skim: false,
            maxb: Vec::new(),
            release: false,
            owned: Vec::new(),
        };
        if matches!(postings.form, Form::Single(_)) || postings.compact {
            // Derived from the record (a block at most): decoded whole as
            // ever.
            let whole = Footer::parse(
                postings,
                &Geometry::default(),
                block_size,
                max_bucket,
                adaptive,
            )?;
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
            if self.kept {
                self.ends.reserve_exact(blocks);
                self.frontier.reserve_exact(
                    (self.src.len().saturating_sub(2 * blocks) / 2).min(blocks * 16),
                );
            } else {
                self.entries.reserve_exact(blocks);
                self.raw.reserve_exact(self.src.len());
                if self.skim {
                    self.maxb.reserve_exact(blocks);
                }
            }
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
        if self.skim {
            return self.skim_until(from, slot);
        }
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

    /// [`Self::decode_until`] of a skimmed footer: no pairs kept.
    fn skim_until(&mut self, from: usize, slot: u32) -> Result<()> {
        loop {
            let (window, window_at) = (self.window, self.window_at);
            let (mut at, mut next_last, mut next_tf) = (self.at, self.next_last, self.next_tf);
            let mut b = self.last.len();
            let full = bits::packed_len(self.block_size as usize, 1) as u64;
            let mut found = false;
            let start = at;
            let raw = self.raw.len();
            while b + 1 < self.blocks {
                let Some((gap, len, max)) =
                    window.get(at - window_at..).and_then(footer_entry_skim)
                else {
                    break;
                };
                let Ok(last) = u32::try_from(next_last + gap) else {
                    // Reported by the block's slow decode.
                    break;
                };
                next_last += gap;
                let width = tf_width(max, self.adaptive);
                self.last.push(last);
                self.entries.push((raw + (at - start)) as u32);
                self.maxb.push(max);
                self.tf_at.push(next_tf as u32);
                self.widths.push(width as u8);
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
            if at > start {
                self.raw
                    .extend_from_slice(&window[start - window_at..at - window_at]);
            }
            self.at = at;
            self.next_last = next_last;
            self.next_tf = next_tf;
            if found || self.last.len() >= self.blocks {
                return Ok(());
            }
            // The next entry alone: across a window's end, or the last.
            let (gap, len) = self.next_entry()?;
            let within = self.at - self.window_at;
            let entry = self.raw.len() as u32;
            self.raw
                .extend_from_slice(&self.window[within..within + len]);
            let max = self.frontier.last().map_or(0, |pair| pair.0);
            self.push_block((gap, len))?;
            self.entries.push(entry);
            self.maxb.push(max);
            self.frontier.clear();
            let b = self.last.len() - 1;
            if b >= from && self.last[b] >= slot {
                return Ok(());
            }
        }
    }

    /// Skims the footer from here on: its blocks are decoded for their last
    /// slots, TF offsets and widths, and their entries' bytes kept, as
    /// [`Self::decode_bounds`] keeps them: a block's frontier is read from
    /// them only when asked ([`Self::floor`], [`Self::floors`],
    /// [`Self::max_bucket`]). Before any block is decoded.
    pub fn skim(&mut self) {
        if self.last.is_empty() && self.frontier.is_empty() {
            self.skim = true;
            self.kept = false;
        }
    }

    /// Whether the footer is skimmed ([`Self::skim`]).
    #[inline]
    pub fn skimmed(&self) -> bool {
        self.skim
    }

    /// Decodes the next block's entry, reading windows as needed; false
    /// when every block is.
    #[inline(never)]
    fn decode_next(&mut self) -> Result<bool> {
        if self.last.len() >= self.blocks {
            return Ok(false);
        }
        let entry = self.next_entry()?;
        self.push_block(entry)
    }

    /// Parses the next block's entry (its pairs appended to the frontier),
    /// reading windows as needed: its bytes are the window's from
    /// [`Self::at`] on.
    fn next_entry(&mut self) -> Result<(u64, usize)> {
        let mut want = BRIDGE;
        Ok(loop {
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
        })
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
        if self.kept {
            self.ends.push(end as u32);
        }
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

    /// Decodes every block in one pass, for a reader that bounds every
    /// block (a disjunction bounds each group its terms hold), and pushes
    /// onto `bounds` each block's bound: the best `bound` of its frontier's
    /// pairs (0 at least), taken as each pair is read. The frontiers are
    /// not kept as pairs: their entries' bytes are (a few bytes a pair,
    /// where a pair kept takes eight), and a block's frontier is read from
    /// them only when asked ([`Self::floor`], [`Self::floors`]); most
    /// blocks' never are. Each block's largest bucket is kept
    /// ([`Self::max_bucket`]), a byte a block.
    /// The footer's pages are read and released as [`Self::ensure`] reads
    /// them. Blocks decoded already (a footer derived from its record)
    /// are bounded from their frontiers.
    #[inline(never)]
    pub fn decode_bounds(
        &mut self,
        mut bound: impl FnMut(u8, u32) -> f32,
        bounds: &mut Vec<f32>,
    ) -> Result<()> {
        bounds.reserve_exact(self.blocks);
        if !self.last.is_empty() || self.blocks == 0 {
            if self.blocks > 0 {
                self.ensure(self.blocks - 1)?;
            }
            for b in 0..self.blocks {
                bounds.push(fold_bound(self.frontier(b), &mut bound));
            }
            return Ok(());
        }
        self.kept = false;
        self.maxb.reserve_exact(self.blocks);
        let full = bits::packed_len(self.block_size as usize, 1) as u64;
        loop {
            // The entries well within the window, in one loop: each one's
            // pairs taken and dropped, its bytes copied with the rest after.
            let (window, window_at) = (self.window, self.window_at);
            let start = self.at;
            let (mut at, mut next_last, mut next_tf) = (self.at, self.next_last, self.next_tf);
            let mut b = self.last.len();
            let raw = self.raw.len();
            while b + 1 < self.blocks {
                let Some((gap, len, max, v)) = window
                    .get(at - window_at..)
                    .and_then(|bytes| footer_entry_bound(bytes, &mut bound))
                else {
                    break;
                };
                let Ok(last) = u32::try_from(next_last + gap) else {
                    // Reported by the block's slow decode.
                    break;
                };
                next_last += gap;
                let width = tf_width(max, self.adaptive);
                bounds.push(v);
                self.maxb.push(max);
                self.last.push(last);
                self.entries.push((raw + (at - start)) as u32);
                self.tf_at.push(next_tf as u32);
                self.widths.push(width as u8);
                next_tf += if width == 1 {
                    full
                } else {
                    bits::packed_len(self.block_size as usize, width) as u64
                };
                at += len;
                b += 1;
            }
            if at > start {
                self.raw
                    .extend_from_slice(&window[start - window_at..at - window_at]);
            }
            self.at = at;
            self.next_last = next_last;
            self.next_tf = next_tf;
            if self.last.len() >= self.blocks {
                return Ok(());
            }
            // The next entry alone: across a window's end, or the last.
            let (gap, len) = self.next_entry()?;
            let from = self.at - self.window_at;
            let entry = self.raw.len() as u32;
            self.raw.extend_from_slice(&self.window[from..from + len]);
            self.push_block((gap, len))?;
            self.entries.push(entry);
            bounds.push(fold_bound(&self.frontier, &mut bound));
            self.maxb
                .push(self.frontier.last().map_or(0, |pair| pair.0));
            self.frontier.clear();
        }
    }

    /// Block `b`'s frontier pairs, in order, read from its entry's bytes
    /// (a footer decoded for its bounds), which were checked as decoded.
    fn raw_pairs(&self, b: usize) -> impl Iterator<Item = (u8, u32)> + '_ {
        let raw = &self.raw[self.entries[b] as usize..];
        // The slot gap's bytes, then the pair count.
        let mut at = raw
            .iter()
            .position(|byte| *byte < 0x80)
            .map_or(0, |i| i + 1);
        let k = usize::from(raw[at]);
        at += 1;
        (0..k).map(move |_| {
            let bucket = raw[at];
            at += 1;
            let mut length = 0u32;
            let mut shift = 0;
            loop {
                let byte = raw[at];
                at += 1;
                length |= u32::from(byte & 0x7f) << shift;
                if byte < 0x80 {
                    break;
                }
                shift += 7;
            }
            (bucket, length)
        })
    }

    /// How short a document holding a posting of (decoded) block `b` with
    /// bucket `bucket` or more can be: the shortest length of the
    /// frontier's pairs at or above it (every posting is dominated by a
    /// frontier pair); 0 when none is.
    #[inline]
    pub fn floor(&self, b: usize, bucket: u8) -> u32 {
        if self.kept {
            self.frontier(b)
                .iter()
                .filter(|(at, _)| *at >= bucket)
                .map(|(_, length)| *length)
                .min()
                .unwrap_or(0)
        } else {
            self.raw_floor(b, bucket)
        }
    }

    #[inline(never)]
    fn raw_floor(&self, b: usize, bucket: u8) -> u32 {
        self.raw_pairs(b)
            .filter(|(at, _)| *at >= bucket)
            .map(|(_, length)| length)
            .min()
            .unwrap_or(0)
    }

    /// [`Self::floor`] of (decoded) block `b` at every bucket, from one
    /// read of its frontier.
    #[inline(never)]
    pub fn floors(&self, b: usize) -> [u32; 16] {
        let mut out = [u32::MAX; 16];
        let mut put = |(bucket, length): (u8, u32)| {
            let at = usize::from(bucket) % 16;
            out[at] = out[at].min(length);
        };
        if self.kept {
            self.frontier(b).iter().copied().for_each(&mut put);
        } else {
            self.raw_pairs(b).for_each(&mut put);
        }
        let mut least = u32::MAX;
        for floor in out.iter_mut().rev() {
            least = least.min(*floor);
            *floor = if least == u32::MAX { 0 } else { least };
        }
        out
    }

    /// Block `b`'s frontier pairs (decoded), each given to `visit`, kept as
    /// pairs or read from the block's entry.
    #[inline]
    pub fn each_pair(&self, b: usize, mut visit: impl FnMut(u8, u32)) {
        if self.kept {
            for (bucket, length) in self.frontier(b) {
                visit(*bucket, *length);
            }
        } else {
            for (bucket, length) in self.raw_pairs(b) {
                visit(bucket, length);
            }
        }
    }

    /// Whether the decoded blocks' frontiers are kept as pairs (not a
    /// footer decoded for its bounds), so [`Self::frontier`] reads them.
    #[inline]
    pub fn keeps_pairs(&self) -> bool {
        self.kept
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
    /// Not for a footer decoded for its bounds ([`Self::decode_bounds`]),
    /// which keeps no pairs: see [`Self::floor`].
    pub fn frontier(&self, b: usize) -> &[(u8, u32)] {
        debug_assert!(self.kept, "a frontier kept as pairs");
        let from = if b == 0 { 0 } else { self.ends[b - 1] as usize };
        &self.frontier[from..self.ends[b] as usize]
    }

    /// Block `b`'s largest bucket (decoded): its frontier's last pair's.
    #[inline]
    pub fn max_bucket(&self, b: usize) -> u8 {
        if self.skim || !self.kept && b < self.maxb.len() {
            self.maxb[b]
        } else if self.kept {
            self.frontier[self.ends[b] as usize - 1].0
        } else {
            self.raw_pairs(b).last().map_or(0, |(bucket, _)| bucket)
        }
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
            .footer(geometry, options.block_size, max, options.adaptive_tf)
            .unwrap();
        for (i, bucket) in buckets.iter().enumerate() {
            assert_eq!(
                footer.bucket(parsed.tf, i as u32).unwrap(),
                *bucket,
                "posting {i}"
            );
            assert_eq!(
                parsed
                    .bucket_at(
                        i as u32,
                        geometry,
                        options.block_size,
                        max,
                        options.adaptive_tf
                    )
                    .unwrap(),
                *bucket,
                "posting {i}"
            );
        }
        if let Form::Grouped(dir) = &parsed.form {
            let entries = &dir.entries(&geometry.groups).unwrap();
            assert_eq!(
                entries,
                &decode_directory_whole(dir.bytes(), &geometry.groups).unwrap(),
                "decoded a block at a time as a varint at a time"
            );
            check_group_frontiers(&parsed, entries, geometry, slots, buckets, lengths, options);
            assert_eq!(footer.blocks(), entries.len());
            assert!(stats.footer == 0 && stats.blocks == 0);
            return stats;
        }
        check_lazy(&parsed, &footer, options, max);
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
    /// posting is dominated by one of its pairs, every pair is a posting's,
    /// and the group's buckets lie at its entry's TF bits.
    fn check_group_frontiers(
        parsed: &Postings<'_>,
        entries: &[GroupEntry],
        geometry: &Geometry,
        slots: &[u32],
        buckets: &[u8],
        lengths: &[u32],
        options: &Options,
    ) {
        let rounded = options.group_frontiers == GroupFrontiers::Rounded;
        assert_eq!(parsed.rounded, rounded);
        for entry in entries {
            let front: Vec<(u8, u32)> = parsed.group_frontier(entry).collect();
            let group = geometry.groups[entry.index as usize];
            let members: Vec<usize> = (0..slots.len())
                .filter(|i| {
                    slots[*i] >= group.slot_base && slots[*i] < group.slot_base + group.slots()
                })
                .collect();
            assert_eq!(members.len(), entry.count as usize);
            assert_eq!(members[0], entry.first as usize);
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
            assert_eq!(entry.max_bucket, front.last().unwrap().0);
            assert_eq!(u32::from(entry.width), tf_width(entry.max_bucket, true));
            for i in &members {
                assert!(
                    front
                        .iter()
                        .any(|(fb, fl)| *fb >= buckets[*i] && *fl <= lengths[*i]),
                    "posting {i} uncovered by {front:?}"
                );
                assert_eq!(parsed.bucket_in(entry, *i as u32).unwrap(), buckets[*i]);
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
        assert_eq!(rounded_length(round_length(65)), 64);
        assert_eq!(rounded_length(round_length(300)), 296);
        for exact in [1u32, 63, 64, 128, 256, 4032] {
            assert_eq!(rounded_length(round_length(exact)), exact);
        }
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
                assert_eq!(
                    skip_group_frontier(&bytes, &mut at).unwrap(),
                    pairs.last().unwrap().0
                );
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
        let mut bytes = vec![(1 << 4) | 15];
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
            assert_eq!(l.tf_at(b) * 8, footer.tf_at[b]);
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
        // Decoded whole for its blocks' bounds, as a disjunction does.
        check_bounds(lazy(), parsed, footer);
        check_skim(lazy, parsed, footer);
    }

    /// A skimmed footer answers each block's last slot, TF offset, width,
    /// floors and largest bucket, and every bucket, as the whole footer
    /// does: asked for buckets first (decoding a block's prefix only), and
    /// sought block by block.
    fn check_skim<'p>(lazy: impl Fn() -> LazyFooter<'p>, parsed: &Postings<'_>, footer: &Footer) {
        let derived = matches!(parsed.form, Form::Single(_)) || parsed.compact;
        let mut l = lazy();
        l.skim();
        assert_eq!(l.skimmed(), !derived);
        for i in 0..parsed.df {
            assert_eq!(
                l.bucket(parsed.tf, i).unwrap(),
                footer.bucket(parsed.tf, i).unwrap(),
                "posting {i}"
            );
            let b = (i / footer.block_size) as usize;
            assert_eq!(
                l.decoded(),
                if derived { footer.blocks() } else { b + 1 },
                "posting {i} block {b} of {} bs {}",
                footer.blocks(),
                footer.block_size
            );
        }
        let mut l = lazy();
        l.skim();
        for b in 0..footer.blocks() {
            assert_eq!(l.seek(b, footer.last[b]).unwrap(), b, "block {b}");
            let front = footer.frontier_of(b);
            assert_eq!(l.last(b), footer.last[b]);
            assert_eq!(l.tf_at(b) * 8, footer.tf_at[b]);
            assert_eq!(l.width(b), u32::from(footer.widths[b]));
            assert_eq!(l.max_bucket(b), front.last().unwrap().0);
            for bucket in 0..16u8 {
                let want = front
                    .iter()
                    .filter(|(at, _)| *at >= bucket)
                    .map(|(_, length)| *length)
                    .min()
                    .unwrap_or(0);
                assert_eq!(l.floor(b, bucket), want, "floor of {b} at {bucket}");
                assert_eq!(l.floors(b)[usize::from(bucket)], want);
            }
        }
        assert_eq!(l.seek(0, u32::MAX).unwrap(), footer.blocks());
    }

    /// A stand-in for a pair's bound: rising with the bucket, falling with
    /// the length.
    pub(super) fn pair_bound(bucket: u8, length: u32) -> f32 {
        f32::from(bucket) * 1000.0 + 1e6 / (length as f32 + 1.0)
    }

    /// The bound [`LazyFooter::decode_bounds`] gives a block of `pairs`.
    pub(super) fn test_bound(pairs: &[(u8, u32)]) -> f32 {
        pairs
            .iter()
            .map(|(bucket, length)| pair_bound(*bucket, *length))
            .fold(0.0, f32::max)
    }

    /// The fast footer readers read an entry as [`footer_entry`] does, for
    /// pair lengths of one to five bytes and any mix of them.
    #[test]
    fn fast_entry_readers_agree_with_the_checked_one() {
        let lengths = [
            0u32,
            1,
            127,
            128,
            200,
            16_383,
            16_384,
            (1 << 21) - 1,
            1 << 21,
            1 << 28,
            u32::MAX,
        ];
        for (gap, k, from) in [
            (0u64, 1usize, 0usize),
            (300, 3, 4),
            (1 << 20, 16, 2),
            (5, 11, 7),
        ] {
            let mut bytes = Vec::new();
            crate::varint::put(&mut bytes, gap);
            bytes.push(k as u8);
            let mut want = Vec::new();
            for i in 0..k {
                let pair = (i as u8 % 16, lengths[(from + i) % lengths.len()]);
                bytes.push(pair.0);
                crate::varint::put(&mut bytes, u64::from(pair.1));
                want.push(pair);
            }
            let end = bytes.len();
            bytes.resize(end + ENTRY_MAX, 0xff);
            let mut pairs = Vec::new();
            assert_eq!(footer_entry(&bytes, &mut pairs).unwrap(), (gap, end));
            assert_eq!(pairs, want);
            let mut fast = Vec::new();
            assert_eq!(footer_entry_fast(&bytes, &mut fast), Some((gap, end)));
            assert_eq!(fast, want);
            let last = want.last().unwrap().0;
            let mut bound = pair_bound;
            assert_eq!(
                footer_entry_bound(&bytes, &mut bound),
                Some((gap, end, last, test_bound(&want)))
            );
            assert_eq!(footer_entry_skim(&bytes), Some((gap, end, last)));
        }
    }

    /// [`LazyFooter::decode_bounds`] decodes every block, bounds each from
    /// its frontier, and still answers each block's floors, largest bucket
    /// and buckets as the whole footer does.
    fn check_bounds(mut l: LazyFooter<'_>, parsed: &Postings<'_>, footer: &Footer) {
        let mut bounds = Vec::new();
        l.decode_bounds(pair_bound, &mut bounds).unwrap();
        assert_eq!(l.decoded(), footer.blocks());
        assert_eq!(bounds.len(), footer.blocks());
        for (b, bound) in bounds.iter().enumerate() {
            let front = footer.frontier_of(b);
            assert_eq!(*bound, test_bound(front), "bound of {b}");
            assert_eq!(l.last(b), footer.last[b]);
            assert_eq!(l.tf_at(b) * 8, footer.tf_at[b]);
            assert_eq!(l.width(b), u32::from(footer.widths[b]));
            assert_eq!(l.max_bucket(b), front.last().unwrap().0);
            for bucket in 0..16u8 {
                let want = front
                    .iter()
                    .filter(|(at, _)| *at >= bucket)
                    .map(|(_, length)| *length)
                    .min()
                    .unwrap_or(0);
                assert_eq!(l.floor(b, bucket), want, "floor of {b} at {bucket}");
                assert_eq!(l.floors(b)[usize::from(bucket)], want);
            }
        }
        for i in 0..parsed.df {
            assert_eq!(
                l.bucket(parsed.tf, i).unwrap(),
                footer.bucket(parsed.tf, i).unwrap()
            );
        }
        assert_eq!(l.seek(0, u32::MAX).unwrap(), footer.blocks());
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
                            group_frontiers: if (block_size + grid_density) % 2 == 0 {
                                GroupFrontiers::Rounded
                            } else {
                                GroupFrontiers::Exact
                            },
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

    /// A directory whose entries are longer than a few bytes (a frontier
    /// of several pairs per group), read in place over small pages: every
    /// entry is decoded once and every directory byte past the first
    /// window is copied about once, however far its count's estimate
    /// falls short, and the record is the one parsed from memory.
    #[test]
    fn long_directory_entries_are_read_once() {
        use super::super::blob::{self, LazyBlob, PinningSource};
        use super::super::segment::{memo_counts, reset_memo_counts};
        // 2,000 groups of six rows each; within a group, a longer row has
        // a higher bucket, so every row is on the group's frontier.
        let tids: Vec<Tid> = (0..2000u32)
            .flat_map(|g| {
                (1..=6u16).map(move |offset| Tid {
                    block: g * 256,
                    offset,
                })
            })
            .collect();
        let geometry = geometry(&tids);
        let slots: Vec<u32> = tids.iter().map(|t| geometry.slot_of(*t).unwrap()).collect();
        let buckets: Vec<u8> = (0..slots.len()).map(|i| (i % 6 * 2) as u8).collect();
        let lengths: Vec<u32> = (0..slots.len() as u32).map(|i| i % 6 * 40 + 3).collect();
        let options = Options {
            sparse: false,
            grid_min_postings: 0,
            ..Options::default()
        };
        let mut out = Vec::new();
        encode(&geometry, &slots, &buckets, &lengths, &options, &mut out);
        let df = slots.len() as u32;
        let whole = Postings::parse(&out, df, &geometry).unwrap();
        let Form::Grouped(dir) = &whole.form else {
            panic!("a grouped record");
        };
        let entries = decode_directory_whole(dir.bytes(), &geometry.groups).unwrap();
        assert_eq!(entries.len(), 2000);
        let directory = dir.bytes().len();
        assert!(
            directory > 2000 * 8,
            "entries of {} bytes",
            directory / 2000
        );
        let source = PinningSource::new(out.clone(), 64);
        let blob = LazyBlob::new(Box::new(source));
        blob.open_span();
        blob::reset_stats();
        reset_memo_counts();
        let parsed = Postings::parse(blob.bytes(), df, &geometry).unwrap();
        let Form::Grouped(read) = &parsed.form else {
            panic!("a grouped record");
        };
        assert_eq!(read.bytes(), dir.bytes());
        assert_eq!(parsed.containers_at, whole.containers_at);
        let counts = memo_counts();
        assert_eq!(
            (counts.directories, counts.directory_entries),
            (1, 0),
            "the directory copied, no entry decoded"
        );
        assert_eq!(read.entries(&geometry.groups).unwrap(), entries);
        assert_eq!(read.entries(&geometry.groups).unwrap(), entries);
        let counts = memo_counts();
        assert_eq!(
            (counts.directories, counts.directory_entries),
            (1, 2000),
            "every entry decoded once"
        );
        let stitched = blob::stats()[Kind::Record as usize].stitched as usize;
        assert!(
            stitched <= directory + 64,
            "stitched {stitched} bytes for a {directory}-byte directory"
        );
        drop(parsed);
        unsafe { blob.close_span() };
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
        let wrong_df = Postings::parse(&out, 9, &geometry)
            .and_then(|p| p.footer(&geometry, 128, 0, true).map(|_| ()));
        assert!(wrong_df.is_err());
        // A grouped record's entries are checked as they are decoded.
        let truncated = Postings::parse(&out[..out.len() - 1], 10, &geometry).and_then(|p| {
            if let Form::Grouped(dir) = &p.form {
                dir.entries(&geometry.groups)?;
            }
            Ok(())
        });
        assert!(truncated.is_err());
        assert!(Postings::parse(&[], 2, &geometry).is_err());
        assert!(Postings::parse(&[9, 0, 0], 2, &geometry).is_err());
    }

    /// A grouped record of `groups` groups of `per` postings each over a
    /// grid of 256-page groups, its header's fields, and its geometry.
    fn grouped_record(groups: u32, per: u32) -> (Vec<u8>, Geometry, u32) {
        let tids: Vec<Tid> = (0..groups * 256)
            .map(|b| Tid {
                block: b,
                offset: 1,
            })
            .collect();
        let geometry = geometry(&tids);
        let slots: Vec<u32> = tids
            .iter()
            .enumerate()
            .filter(|(i, _)| (*i as u32 % 256) < per)
            .map(|(_, t)| geometry.slot_of(*t).unwrap())
            .collect();
        let df = slots.len() as u32;
        let buckets: Vec<u8> = (0..df).map(|i| (i % 7) as u8).collect();
        let lengths: Vec<u32> = (0..df).map(|i| 1 + i % 13).collect();
        let mut out = Vec::new();
        let options = Options {
            sparse: false,
            ..Options::default()
        };
        let stats = encode(&geometry, &slots, &buckets, &lengths, &options, &mut out);
        assert!(!stats.sparse, "a grouped record");
        (out, geometry, df)
    }

    #[test]
    fn a_directory_decoded_whole_is_the_one_decoded_as_reached() {
        let (out, geometry, df) = grouped_record(700, 2);
        let groups = &geometry.groups;
        for first in [None, Some(0), Some(200), Some(699)] {
            let parsed = Postings::parse(&out[..], df, &geometry).unwrap();
            let Form::Grouped(dir) = &parsed.form else {
                panic!("a grouped record");
            };
            if let Some(b) = first {
                dir.get(b, groups).unwrap();
            }
            dir.decode_all(groups).unwrap();
            assert_eq!(dir.decoded(), 700);
            assert_eq!(
                dir.entries(groups).unwrap(),
                decode_directory_whole(dir.bytes(), groups).unwrap()
            );
            dir.decode_all(groups).unwrap();
        }
        // Its totals are checked as a block's are.
        let parsed = Postings::parse(&out[..out.len() - 1], df, &geometry).unwrap();
        let Form::Grouped(dir) = &parsed.form else {
            panic!("a grouped record");
        };
        assert!(dir.decode_all(groups).is_err());
    }

    /// `out`, a grouped record of `groups` groups, as page layout 10 wrote
    /// it: its directory without skip points, the flag clear.
    fn without_skips(out: &[u8], groups: usize) -> Vec<u8> {
        assert_eq!(out[0] & (FORM_LENGTHS | FORM_SKIPS), FORM_SKIPS);
        let mut at = 1;
        let directory_len = varint::get(out, &mut at).unwrap() as usize;
        let payload_len = varint::get(out, &mut at).unwrap() as usize;
        let skips = skip_count(groups) * SKIP_BYTES;
        let payload = &out[at..at + payload_len];
        let mut old = vec![out[0] & !FORM_SKIPS];
        varint::put(&mut old, (directory_len - skips) as u64);
        varint::put(&mut old, (payload_len - skips) as u64);
        old.extend_from_slice(&payload[..directory_len - skips]);
        old.extend_from_slice(&payload[directory_len..]);
        old.extend_from_slice(&out[at + payload_len..]);
        old
    }

    /// A directory without skip points (page layout 10) reads as the same
    /// directory with them, its runs decoded in order whatever is asked.
    #[test]
    fn directories_without_skip_points_read_the_same() {
        for (groups, per) in [(1, 2), (16, 3), (17, 2), (40, 3), (700, 2)] {
            let (out, geometry, df) = grouped_record(groups, per);
            let groups = groups as usize;
            let g = &geometry.groups;
            let old = without_skips(&out, groups);
            let new = Postings::parse(&out[..], df, &geometry).unwrap();
            let parsed = Postings::parse(&old[..], df, &geometry).unwrap();
            let (Form::Grouped(new), Form::Grouped(dir)) = (&new.form, &parsed.form) else {
                panic!("grouped records");
            };
            assert_eq!(
                new.bytes().len() - dir.bytes().len(),
                skip_count(groups) * SKIP_BYTES
            );
            let whole = new.entries(g).unwrap();
            let last = groups - 1;
            assert_eq!(*dir.get(last, g).unwrap(), whole[last]);
            assert_eq!(dir.decoded(), groups, "every run before the last");
            assert_eq!(dir.entries(g).unwrap(), whole);
            let fresh = Postings::parse(&old[..], df, &geometry).unwrap();
            let Form::Grouped(dir) = &fresh.form else {
                panic!("a grouped record");
            };
            for e in &whole {
                assert_eq!(dir.find(e.index, g).unwrap(), Some(e));
                assert_eq!(dir.entry_of_posting(e.first, g).unwrap(), e);
            }
            // Cut short, the last run's totals fail.
            let bad = Postings::parse(&old[..old.len() - 1], df, &geometry).unwrap();
            let Form::Grouped(dir) = &bad.form else {
                panic!("a grouped record");
            };
            assert!(dir.decode_all(g).is_err());
        }
    }

    /// A skip point rewritten with a total one off: the run before it, or
    /// the run it starts, fails its checks.
    #[test]
    fn a_wrong_skip_point_fails_its_runs() {
        let (out, geometry, df) = grouped_record(700, 2);
        let g = &geometry.groups;
        let mut at = 1;
        let directory_len = varint::get(&out, &mut at).unwrap() as usize;
        varint::get(&out, &mut at).unwrap();
        let skips_at = at + directory_len - skip_count(700) * SKIP_BYTES;
        for (point, field) in [(0, 0), (3, 1), (10, 2), (20, 3), (42, 4)] {
            let mut bad = out.clone();
            bad[skips_at + point * SKIP_BYTES + 4 * field] ^= 1;
            let parsed = Postings::parse(&bad[..], df, &geometry).unwrap();
            let Form::Grouped(dir) = &parsed.form else {
                panic!("a grouped record");
            };
            let r = point + 1;
            let before = dir.get(r * DIRECTORY_SKIP - 1, g).map(|_| ());
            let at = dir.get(r * DIRECTORY_SKIP, g).map(|_| ());
            assert!(
                before.is_err() || at.is_err() || dir.decode_all(g).is_err(),
                "point {point} field {field}"
            );
        }
    }

    #[test]
    fn a_grouped_header_gives_where_the_containers_start() {
        for (groups, per) in [(1, 2), (3, 5), (40, 3), (700, 2)] {
            let (out, geometry, df) = grouped_record(groups, per);
            assert_eq!(out[0] & 3, FORM_GROUPED);
            let mut at = 1;
            let directory_len = varint::get(&out, &mut at).unwrap() as usize;
            let payload_len = varint::get(&out, &mut at).unwrap() as usize;
            let parsed = Postings::parse(&out[..], df, &geometry).unwrap();
            assert_eq!(parsed.payload_at, at);
            assert_eq!(parsed.payload.len(), payload_len);
            assert_eq!(parsed.containers_at, directory_len);
            let Form::Grouped(dir) = &parsed.form else {
                panic!("a grouped record");
            };
            assert_eq!(dir.bytes().len(), directory_len);
            assert_eq!(dir.len(), groups as usize);
            assert_eq!(
                dir.entries(&geometry.groups).unwrap(),
                decode_directory_whole(dir.bytes(), &geometry.groups).unwrap()
            );
            // The length rewritten one byte shorter or longer: the parse or
            // the last entry's decode finds it (the totals no longer meet
            // the containers), as does a length past the payload or zero.
            for wrong in [directory_len - 1, directory_len + 1, payload_len + 1, 0] {
                let mut bad = vec![out[0]];
                varint::put(&mut bad, wrong as u64);
                bad.extend_from_slice(&out[1 + varint_len(directory_len as u64)..]);
                let read = Postings::parse(&bad[..], df, &geometry).and_then(|p| {
                    if let Form::Grouped(dir) = &p.form {
                        dir.entries(&geometry.groups)?;
                    }
                    Ok(())
                });
                assert!(
                    read.is_err(),
                    "{groups} groups, length {wrong} of {directory_len}"
                );
            }
        }
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
            .step_by(5)
            .map(|i| geometry.slot_of(tids[i]).unwrap())
            .collect();
        let n = members.len();
        let buckets: Vec<u8> = (0..n).map(|i| (i * 7 % 16) as u8).collect();
        let lengths: Vec<u32> = (0..n).map(|i| (i as u32 * 7919) % 5000 + 1).collect();
        // A sparse record (one list, footer blocks): the members are few
        // enough for one list to beat group containers.
        let options = Options {
            block_size: 4,
            grid_min_postings: u32::MAX,
            inline_lengths_max_df: 0,
            ..Options::default()
        };
        let mut out = Vec::new();
        encode(&geometry, &members, &buckets, &lengths, &options, &mut out);
        let parsed = Postings::parse(&out, n as u32, &geometry).unwrap();
        assert!(matches!(parsed.form, Form::Sparse(_)), "a sparse record");
        let whole = parsed.footer(&geometry, 4, 15, true).unwrap();
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
            assert_eq!(lazy.tf_at(b) * 8, whole.tf_at[b]);
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
            .step_by(5)
            .map(|i| geometry.slot_of(tids[i]).unwrap())
            .collect();
        let n = members.len();
        let buckets: Vec<u8> = (0..n).map(|i| (i * 5 % 16) as u8).collect();
        let lengths: Vec<u32> = (0..n).map(|i| (i as u32 * 7919) % 5000 + 1).collect();
        // A sparse record (one list, footer blocks): the members are few
        // enough for one list to beat group containers.
        let options = Options {
            block_size: 4,
            grid_min_postings: u32::MAX,
            inline_lengths_max_df: 0,
            ..Options::default()
        };
        let mut out = Vec::new();
        encode(&geometry, &members, &buckets, &lengths, &options, &mut out);
        let parsed = Postings::parse(&out, n as u32, &geometry).unwrap();
        assert!(matches!(parsed.form, Form::Sparse(_)), "a sparse record");
        let whole = parsed.footer(&geometry, 4, 15, true).unwrap();
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
            assert_eq!(lazy.tf_at(b) * 8, whole.tf_at[b]);
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

    /// Decoded whole for its bounds over a blob read in place, a footer
    /// releases each page of its own bytes as it reads past it, as
    /// decoding a block at a time does, and its blocks' frontiers stay
    /// readable after: they are not read from the released (poisoned)
    /// pages.
    #[test]
    fn lazy_footer_decoded_for_bounds_releases_its_pages_and_keeps_its_frontiers() {
        use super::super::blob::{LazyBlob, PinningSource};
        let tids: Vec<Tid> = (0..6000u32)
            .map(|i| Tid {
                block: i / 3,
                offset: (i % 3 + 1) as u16,
            })
            .collect();
        let geometry = Geometry::of(&tids).unwrap();
        let members: Vec<u32> = (0..6000)
            .step_by(7)
            .map(|i| geometry.slot_of(tids[i]).unwrap())
            .collect();
        let n = members.len();
        let buckets: Vec<u8> = (0..n).map(|i| (i * 7 % 16) as u8).collect();
        let lengths: Vec<u32> = (0..n).map(|i| (i as u32 * 7919) % 5000 + 1).collect();
        // A sparse record (one list, footer blocks): the members are few
        // enough for one list to beat group containers.
        let options = Options {
            block_size: 4,
            grid_min_postings: u32::MAX,
            inline_lengths_max_df: 0,
            ..Options::default()
        };
        let mut out = Vec::new();
        encode(&geometry, &members, &buckets, &lengths, &options, &mut out);
        let parsed = Postings::parse(&out, n as u32, &geometry).unwrap();
        assert!(matches!(parsed.form, Form::Sparse(_)), "a sparse record");
        let whole = parsed.footer(&geometry, 4, 15, true).unwrap();
        let source = PinningSource::new(out.clone(), 64);
        let blob = LazyBlob::new(Box::new(source.clone()));
        blob.open_span();
        let parsed = Postings::parse(blob.bytes(), n as u32, &geometry).unwrap();
        let mut lazy = parsed.lazy_footer(4, 15, true).unwrap();
        lazy.release_passed();
        let mut bounds = Vec::new();
        lazy.decode_bounds(super::tests::pair_bound, &mut bounds)
            .unwrap();
        assert!(
            source.0.early.get() > 10,
            "pages released as the footer read on"
        );
        assert_eq!(bounds.len(), whole.blocks());
        for (b, bound) in bounds.iter().enumerate() {
            let front = whole.frontier_of(b);
            assert_eq!(*bound, super::tests::test_bound(front));
            assert_eq!(lazy.max_bucket(b), front.last().unwrap().0);
            for bucket in 0..16u8 {
                let want = front
                    .iter()
                    .filter(|(at, _)| *at >= bucket)
                    .map(|(_, length)| *length)
                    .min()
                    .unwrap_or(0);
                assert_eq!(lazy.floor(b, bucket), want, "floor of {b} at {bucket}");
                assert_eq!(lazy.floors(b)[usize::from(bucket)], want);
            }
        }
        drop(lazy);
        drop(parsed);
        unsafe { blob.close_span() };
    }
}

#[cfg(test)]
mod lazy_directory_tests {
    use super::super::blob::{LazyBlob, PinningSource};
    use super::super::docs::Geometry;
    use super::*;
    use crate::Tid;
    use proptest::prelude::*;

    /// A small deterministic generator for the terms below.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    /// A grouped record's encoding over `groups` heap groups of up to four
    /// pages each, its members in each group at one of several densities
    /// (so grid, Elias-Fano and paged containers mix), buckets at random or
    /// rising with length (long frontiers), lengths of 6 to 31 bits; with
    /// its geometry and postings count.
    fn term(seed: u64, groups: u32, exact: bool) -> (Vec<u8>, Geometry, u32) {
        let mut rng = Rng(seed | 1);
        let mut tids = Vec::new();
        for g in 0..groups {
            let pages = 1 + rng.below(4) as u32;
            // At least two slots a group, its first and last members.
            let width = 2 + rng.below(39) as u16;
            for page in 0..pages {
                for offset in 1..=width {
                    tids.push(Tid {
                        block: g * 256 + page,
                        offset,
                    });
                }
            }
        }
        let geometry = Geometry::of(&tids).unwrap();
        let (mut slots, mut buckets, mut lengths) = (Vec::new(), Vec::new(), Vec::new());
        let bits = [6u32, 14, 20, 31][rng.below(4) as usize];
        let mut at = 0;
        for g in 0..groups as usize {
            let n = geometry.groups[g].slots() as usize;
            let density = [2u64, 20, 60, 100][rng.below(4) as usize];
            let rising = rng.below(4) == 0;
            for i in 0..n {
                if i == 0 || i + 1 == n || rng.below(100) < density {
                    slots.push(geometry.slot_of(tids[at + i]).unwrap());
                    if rising {
                        buckets.push((i % 16) as u8);
                        lengths.push(1 + (i as u32 % 16) * 3);
                    } else {
                        buckets.push(rng.below(16) as u8);
                        lengths.push(1 + rng.below(1 << bits) as u32);
                    }
                }
            }
            at += n;
        }
        let options = Options {
            sparse: false,
            grid_min_postings: 0,
            group_frontiers: if exact {
                GroupFrontiers::Exact
            } else {
                GroupFrontiers::Rounded
            },
            ..Options::default()
        };
        let mut out = Vec::new();
        encode(&geometry, &slots, &buckets, &lengths, &options, &mut out);
        (out, geometry, slots.len() as u32)
    }

    /// Reads `walk` (entry positions, in the order asked) from a freshly
    /// parsed directory and checks each against the whole decode: the
    /// entry, its frontier and its container; and that the runs decoded are
    /// those of the entries asked for and, read on to, a few after each.
    /// Then seeks every group from every run's first entry.
    fn check_walk(record: &[u8], geometry: &Geometry, df: u32, walk: &[usize]) {
        let whole_record = Postings::parse(record, df, geometry).unwrap();
        let Form::Grouped(dir) = &whole_record.form else {
            panic!("a grouped record");
        };
        let groups = &geometry.groups;
        let whole = decode_directory_whole(dir.bytes(), groups).unwrap();
        let parsed = Postings::parse(record, df, geometry).unwrap();
        let Form::Grouped(lazy) = &parsed.form else {
            panic!("a grouped record");
        };
        assert_eq!(lazy.len(), whole.len());
        assert_eq!(lazy.decoded(), 0, "nothing decoded by the parse");
        let mut runs = std::collections::BTreeSet::new();
        for &b in walk {
            let b = b % whole.len();
            runs.insert(b / DIRECTORY_SKIP);
            let entry = lazy.get(b, groups).unwrap();
            assert_eq!(*entry, whole[b], "entry {b}");
            assert!(
                parsed
                    .group_frontier(entry)
                    .eq(whole_record.group_frontier(&whole[b])),
                "frontier of entry {b}"
            );
            assert_eq!(
                parsed.container(entry).all().unwrap(),
                whole_record.container(&whole[b]).all().unwrap(),
                "container of entry {b}"
            );
            let decoded: usize = runs
                .iter()
                .map(|r| DIRECTORY_SKIP.min(whole.len() - r * DIRECTORY_SKIP))
                .sum();
            assert!(lazy.decoded() >= decoded, "runs {runs:?}");
            assert!(
                lazy.decoded() <= (decoded * SEQUENTIAL_RUNS).min(whole.len()),
                "runs {runs:?}"
            );
            for r in &runs {
                assert!(lazy.is_done(*r), "run {r}");
            }
        }
        let fresh = Postings::parse(record, df, geometry).unwrap();
        let Form::Grouped(dir) = &fresh.form else {
            panic!("a grouped record");
        };
        let last = whole.last().unwrap().index;
        for from in (0..whole.len()).step_by(DIRECTORY_SKIP) {
            for index in whole[from].index..=last + 1 {
                let want = from + whole[from..].partition_point(|e| e.index < index);
                assert_eq!(
                    dir.seek(index, from, groups).unwrap(),
                    want,
                    "{index} from {from}"
                );
            }
        }
        for e in &whole {
            assert_eq!(dir.find(e.index, groups).unwrap(), Some(e));
            for index in [e.first, e.first + e.count - 1] {
                assert_eq!(dir.entry_of_posting(index, groups).unwrap(), e);
            }
        }
        // And decoded whole.
        dir.decode_all(groups).unwrap();
        for from in (0..whole.len()).step_by(7) {
            for index in [whole[from].index, whole[from].index + 1, last, last + 1] {
                let want = from + whole[from..].partition_point(|e| e.index < index);
                assert_eq!(
                    dir.seek(index, from, groups).unwrap(),
                    want,
                    "{index} from {from}, whole"
                );
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

        /// A directory decoded a block at a time, in whatever order a walk
        /// asks for its entries (forward a step or a jump, back to one
        /// read), gives the entries, frontiers and containers decoding it
        /// whole a varint at a time does, and decodes no block past the
        /// furthest asked for.
        #[test]
        fn lazy_directories_read_as_whole_ones(
            seed in any::<u64>(),
            groups in 1u32..600,
            exact in any::<bool>(),
            steps in proptest::collection::vec((0u8..4, 0usize..400), 1..80),
        ) {
            let (record, geometry, df) = term(seed, groups, exact);
            let mut walk = Vec::new();
            let mut b = 0usize;
            for (kind, n) in steps {
                b = match kind {
                    0 => b + 1,
                    1 => b + n,
                    2 => walk.get(n % walk.len().max(1)).copied().unwrap_or(0),
                    _ => n,
                };
                walk.push(b);
            }
            check_walk(&record, &geometry, df, &walk);
            // And every entry in order.
            let all: Vec<usize> = (0..groups as usize).collect();
            check_walk(&record, &geometry, df, &all);
        }

        /// The varints an entry would take from each varint on, computed
        /// eight at a time, are those its kind and head give.
        #[test]
        fn steps_are_entry_lengths(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
            let mut units = Units::default();
            units.scan(&bytes);
            for u in 0..units.n {
                let f = |i: usize| units.first[u + i];
                let paged = u8::from(f(1) & 3 == KIND_PAGED);
                let head = if paged == 1 { f(3) } else { f(2) };
                let want = 4 + paged + (head >> 4) - (head >> 7);
                prop_assert_eq!(units.step[u], want, "varint {}", u);
            }
        }

        /// The division-free Elias-Fano length is `encoded_len`'s.
        #[test]
        fn branchless_ef_lengths_are_encoded_lengths(n in 1u32..100_000, universe in 1u32..u32::MAX) {
            let n = n.min(universe);
            prop_assert_eq!(
                ef::encoded_len_branchless(n, universe),
                ef::encoded_len(n as usize, universe)
            );
        }
    }

    /// The entries a varint-at-a-time path decodes within a block decoded
    /// otherwise without a branch on their lengths: frontiers of nine pairs
    /// or more (a head above 0x7F), counts of three bytes, and exact lengths
    /// of five bytes.
    #[test]
    fn unusual_entries_read_as_whole_ones() {
        // Group 0: 256 pages of 40 rows, every row a member (a count past
        // 4,096); group 1: sixteen rows, buckets and lengths rising (a
        // frontier of sixteen pairs); group 2: a length past 2^28; then 300
        // groups of two rows.
        let mut tids = Vec::new();
        for page in 0..256u32 {
            tids.extend((1..=40u16).map(|offset| Tid {
                block: page,
                offset,
            }));
        }
        tids.extend((1..=16u16).map(|offset| Tid { block: 256, offset }));
        for g in 2..302u32 {
            tids.extend((1..=2u16).map(|offset| Tid {
                block: g * 256,
                offset,
            }));
        }
        let geometry = Geometry::of(&tids).unwrap();
        let slots: Vec<u32> = tids.iter().map(|t| geometry.slot_of(*t).unwrap()).collect();
        let n = slots.len();
        let buckets: Vec<u8> = (0..n)
            .map(|i| match i {
                i if i < 10240 => (i % 3) as u8,
                i if i < 10256 => (i - 10240) as u8,
                10256 => 15,
                i => (i % 5) as u8,
            })
            .collect();
        let lengths: Vec<u32> = (0..n)
            .map(|i| match i {
                i if i < 10240 => 100,
                i if i < 10256 => 10 + (i - 10240) as u32 * 7,
                10256 => 1 << 30,
                i => 1 + i as u32 % 900,
            })
            .collect();
        let options = Options {
            sparse: false,
            grid_min_postings: 0,
            group_frontiers: GroupFrontiers::Exact,
            ..Options::default()
        };
        let mut out = Vec::new();
        encode(&geometry, &slots, &buckets, &lengths, &options, &mut out);
        let parsed = Postings::parse(&out, n as u32, &geometry).unwrap();
        let Form::Grouped(dir) = &parsed.form else {
            panic!("a grouped record");
        };
        let whole = decode_directory_whole(dir.bytes(), &geometry.groups).unwrap();
        assert!(whole[0].count > 4096, "a three-byte count");
        assert_eq!(
            parsed.group_frontier(&whole[1]).count(),
            16,
            "sixteen pairs"
        );
        assert!(
            parsed.group_frontier(&whole[2]).any(|(_, l)| l >= 1 << 28),
            "a five-byte length"
        );
        assert_eq!(dir.entries(&geometry.groups).unwrap(), whole);
        let walk: Vec<usize> = (0..whole.len()).rev().collect();
        check_walk(&out, &geometry, n as u32, &walk);
    }

    /// A directory parsed over pages read in place holds no page: once the
    /// span closes (its pages released and poisoned), its entries decode
    /// as from memory, and decoding them pins nothing.
    #[test]
    fn lazy_directories_read_no_page_after_the_parse() {
        let (record, geometry, df) = term(7, 900, false);
        let whole = {
            let parsed = Postings::parse(&record, df, &geometry).unwrap();
            let Form::Grouped(dir) = &parsed.form else {
                panic!("a grouped record");
            };
            decode_directory_whole(dir.bytes(), &geometry.groups).unwrap()
        };
        let source = PinningSource::new(record.clone(), 64);
        let blob = LazyBlob::new(Box::new(source.clone()));
        blob.set_keep(0);
        blob.open_span();
        let parsed = Postings::parse(blob.bytes(), df, &geometry).unwrap();
        assert!(parsed.is_detached(), "a grouped record keeps no slice");
        let Form::Grouped(dir) = &parsed.form else {
            panic!("a grouped record");
        };
        let dir = dir.clone();
        let pins = source.0.pins.get();
        let reads = source.0.reads.get();
        // SAFETY: the parsed record keeps no slice of the span's pages.
        unsafe { blob.close_span() };
        assert!(
            !source.0.released.borrow().is_empty(),
            "pages released, poisoned"
        );
        assert_eq!(dir.entries(&geometry.groups).unwrap(), whole);
        assert_eq!(source.0.pins.get(), pins, "no page pinned to decode");
        assert_eq!(source.0.reads.get(), reads, "no page read to decode");
        drop(parsed);
    }
}
