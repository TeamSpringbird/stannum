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
//!            directory_len varint (bytes of the directory: where the
//!                                  containers start),
//!            payload_len varint,
//!            directory, container*, tf
//! directory := groups varint,
//!            widths u8 * 4 (bits of a row's deltas of postings, container
//!                           bytes, TF bits and frontier bytes; at most 32),
//!            presence,
//!            bases, one per block of 32 rows (rows 32k on):
//!              (postings, container bytes, TF bits, frontier bytes before
//!               the block's first row) u32le each,
//!            rows, groups + 1 of them packed at 6 + the widths' sum bits
//!              (at most 120): kind (2 bits), the frontier's largest bucket
//!              (4), then each total's delta from its block's base; the
//!              last row (kind and bucket 0) holds the record's totals,
//!            group frontier*, one per group in order
//! presence := groups * 16 > the segment's groups:
//!              a bitmap over the segment's groups, bit g of u64le word
//!              g / 64 set for each group held, then per word the groups
//!              held in the words before it
//!            | else the groups' indexes, ascending
//!            (indexes and ranks u16le; u32le in a segment of more than
//!             65,536 groups)
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
//!            | array: count u16le local slots, ascending (a group of at
//!                     most 65,536 slots; kind 3, page layout 11)
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
//! container's kind and offset, where its buckets start in the TF tail and
//! where its frontier starts; a reader finds a group's entry by the group's
//! rank in the record's presence (or among its group indexes) and reads its
//! row, decoding nothing of the groups before it ([`Directory`]).
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

/// Postings a footer-less record holds at most.
pub const COMPACT_MAX: u32 = 8;

pub const KIND_GRID: u8 = 0;
pub const KIND_EF: u8 = 1;
pub const KIND_PAGED: u8 = 2;
/// A group's local slots as sorted little-endian `u16`s (page layout 11):
/// two bytes a member, read with no decoding.
pub const KIND_ARRAY: u8 = 3;

/// Slots a group may have for its members to be an array container.
pub const ARRAY_SLOTS_MAX: u32 = 1 << 16;

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
    /// A group the encoder would store as an Elias-Fano list, of at most
    /// this many members, is an array of `u16` local slots instead
    /// ([`KIND_ARRAY`]): two bytes a member where the list takes 1.2 to 1.8
    /// here, but no decoding, so a walk takes its members as they are and
    /// probes them by search; 0 for none.
    pub array_max: u32,
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
            array_max: 64,
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
    /// Groups per container kind: grid, ef, paged, array.
    pub kinds: [usize; 4],
    /// Bytes per container kind.
    pub kind_bytes: [usize; 4],
    /// Groups made grids by [`Options::grid_density`].
    pub forced: usize,
    /// Bytes of inline lengths ([`InlineLengths`]).
    pub lengths: usize,
    /// Bytes of the directory's group frontiers (part of `payload`), and
    /// how many it holds.
    pub frontier_bytes: usize,
    pub frontiers: usize,
    /// Bytes of the directory but for its frontiers: its group count,
    /// widths, presence, bases and rows (part of `payload`).
    pub directory_bytes: usize,
    /// Bytes a directory entry took as varints before the directory had
    /// fixed-width rows (page layout 12): a group index gap, a count and
    /// kind, a paged container's length, and the group count's.
    pub entry_varints: usize,
    /// Bytes the array containers take less what their members would take
    /// as Elias-Fano lists (negative for a few lists of one to four).
    pub array_excess: isize,
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
///
/// A frontier is read as it lies, not checked first (the verifier checks
/// every one, [`Directory::check_entries`]): a corrupt one gives pairs
/// that are wrong but whose buckets are buckets, and ends at the
/// directory's end.
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
            let d = get_frontier(self.bytes, &mut self.at)?;
            self.bucket = self.bucket.wrapping_add(1 + (d & 15) as u8) & 15;
            self.code = self.code.wrapping_add(1 + (d >> 4) as u32);
        } else {
            let head = *self.bytes.get(self.at)?;
            self.at += 1;
            self.started = true;
            self.left = (head >> 4) + 1;
            self.bucket = head & 15;
            self.code = get_frontier(self.bytes, &mut self.at)? as u32;
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

/// A frontier's varint at `at`, `None` past the bytes or when malformed.
#[inline]
fn get_frontier(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let b = *bytes.get(*at)?;
    if b < 0x80 {
        *at += 1;
        return Some(u64::from(b));
    }
    varint::get(bytes, at).ok()
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
    // what it was without them. Its rows count as the varint entries they
    // replaced, and arrays as the Elias-Fano lists they replace, for the
    // same reason: they buy seeks and decoding.
    let grouped_len = grouped.len() as isize
        - grouped_stats.frontier_bytes as isize
        - grouped_stats.directory_bytes as isize
        + grouped_stats.entry_varints as isize
        - grouped_stats.array_excess;
    let use_sparse =
        options.sparse && grouped_stats.forced == 0 && sparse.len() as isize <= grouped_len;
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
        stats.directory_bytes = grouped_stats.directory_bytes;
        stats.entry_varints = grouped_stats.entry_varints;
        stats.frontiers = grouped_stats.frontiers;
        stats.array_excess = grouped_stats.array_excess;
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
/// containers) and the bytes of its directory (where its containers
/// start), and in `tf` the record's TF tail: each group's buckets at the
/// group's width, in a row.
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
    let mut entries: Vec<GroupEntry> = Vec::new();
    let mut frontiers = Vec::new();
    let mut bodies = Vec::new();
    let mut previous: Option<usize> = None;
    let mut at = 0usize;
    let mut index = geometry.group_of_slot(slots[0]);
    let mut locals = Vec::new();
    let mut candidate = Vec::new();
    let mut tf_bits = BitWriter::new();
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
        // A list of this many members is an array rather than Elias-Fano.
        let array =
            locals.len() as u64 <= u64::from(options.array_max) && group.slots() <= ARRAY_SLOTS_MAX;
        // An input's container, where its term was forced as this one is
        // and its list is the form these options give it.
        let reused = reuse(index).filter(|r| {
            let form = match r.kind {
                KIND_EF => !array,
                KIND_ARRAY => array,
                _ => true,
            };
            form && (!dense
                || (u64::from(r.input_df) >= u64::from(options.grid_min_postings)) == forced)
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
        if kind == KIND_EF && array {
            kind = KIND_ARRAY;
            best = 2 * locals.len();
        }
        if kind == KIND_ARRAY {
            stats.array_excess +=
                best as isize - ef::encoded_len(locals.len(), group.slots()) as isize;
        }
        let gap = match previous {
            None => index,
            Some(p) => index - p - 1,
        };
        previous = Some(index);
        let packed = ((locals.len() as u64 - 1) << 2) | u64::from(kind);
        stats.entry_varints += varint_len(gap as u64) + varint_len(packed);
        if kind == KIND_PAGED {
            stats.entry_varints += varint_len(best as u64);
        }
        let before = bodies.len();
        if let Some(r) = reused {
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
                KIND_ARRAY => {
                    for local in &locals {
                        bodies.extend_from_slice(&(*local as u16).to_le_bytes());
                    }
                }
                _ => bodies.extend_from_slice(&candidate),
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
        let frontier = frontiers.len();
        put_group_frontier(&mut frontiers, &pairs);
        stats.frontiers += 1;
        // A group's buckets are always packed at the group's own width.
        let max_bucket = pairs.last().expect("a pair").0;
        let width = tf_width(max_bucket, true);
        for bucket in &buckets[from..at] {
            tf_bits.put(u32::from(*bucket), width);
        }
        entries.push(GroupEntry {
            index: index as u32,
            count: locals.len() as u32,
            kind,
            at: u32::try_from(before).expect("a record's containers fit 32 bits"),
            len: best as u32,
            first: from as u32,
            frontier: frontier as u32,
            tf_bit: u32::try_from(tf_total).expect("a record's TF tail fits 32 bits"),
            width: width as u8,
            max_bucket,
        });
        tf_total += (at - from) as u64 * u64::from(width);
        stats.kinds[kind as usize] += 1;
        stats.kind_bytes[kind as usize] += best;
    }
    *tf = tf_bits.finish();
    stats.entry_varints += varint_len(entries.len() as u64);
    stats.frontier_bytes += frontiers.len();
    let mut out = Vec::with_capacity(bodies.len() + frontiers.len() + 16 * entries.len());
    stats.directory_bytes +=
        encode_directory(geometry.groups.len(), &entries, &frontiers, &mut out);
    let directory_len = out.len();
    out.extend_from_slice(&bodies);
    (out, directory_len)
}

/// One group of a grouped record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
        if rounded && !grouped {
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
        let form = form & !(FORM_LENGTHS | FORM_COMPACT | FORM_ROUNDED);
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
                // read in one window and copied ([`Directory`] reads its
                // entries in place).
                let end = directory_len;
                if end == 0 || end > payload.len() {
                    return Err(Error::Corrupt("postings directory length"));
                }
                // Copied a page at a time: a directory spanning pages is
                // not stitched first (a second copy).
                let mut copy = Vec::with_capacity(end + SLACK);
                payload.copy_into(0, end, &mut copy)?;
                super::segment::count_memo(|c| {
                    c.directories += 1;
                    c.directory_bytes += end as u64;
                });
                containers_at = end;
                Form::Grouped(std::rc::Rc::new(Directory::parse(
                    copy,
                    geometry.groups.len(),
                    df,
                    (payload.len() - end) as u64,
                    tf_len as u64,
                )?))
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
                self.bucket_in(&dir.entry_of_posting(index, &geometry.groups)?, index)
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
                let mut failed = Ok(());
                dir.each(&geometry.groups, |entry| {
                    if failed.is_err() {
                        return;
                    }
                    let index = entry.index as usize;
                    let base = geometry.groups[index].slot_base;
                    let starts = geometry.starts(index);
                    failed = self.container(entry).all().and_then(|bytes| {
                        for_each_local(entry, bytes, starts, |local| visit(base + local))
                    });
                })?;
                failed?;
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

/// Entries of a [`Directory`] per block of rows: each block starts with its
/// bases, the totals before its first row.
pub const DIRECTORY_BLOCK: usize = 32;

/// A directory's columns of totals, in a block's bases and a row's deltas:
/// postings, container bytes, TF bits and frontier bytes before the entry.
const FIRST: usize = 0;
const AT: usize = 1;
const TF: usize = 2;
const FRONTIER: usize = 3;

/// Bits of a row before its deltas: its container's kind (2) and its
/// frontier's largest bucket (4).
const ROW_HEAD: u32 = 6;

/// Most bits a row takes: one read of sixteen bytes holds it at any bit
/// offset. A row's deltas are bounded by a block's groups' slots (a group's
/// count and grid, at most 256 pages of 291 slots), so no record needs more.
const ROW_MAX: u32 = 120;

/// Bytes of a block's bases: four little-endian `u32`s.
const BASE_BYTES: usize = 16;

/// A record held in more than one group in this many has its presence as a
/// bitmap over the segment's groups (two bytes a group or fewer), else as
/// its group indexes.
const DENSE_SHARE: usize = 16;

/// Zero bytes a parsed directory's copy carries past its end, so that a
/// row's sixteen bytes and a bitmap word's eight read in one load anywhere.
const SLACK: usize = 16;

/// Bytes of a group index or a bitmap word's rank in a segment of `groups`
/// groups: two, four past 65,536.
#[inline]
const fn index_bytes(groups: usize) -> usize {
    if groups > 1 << 16 { 4 } else { 2 }
}

/// Whether a record of `len` groups in a segment of `groups` has a presence
/// bitmap.
#[inline]
const fn is_dense(len: usize, groups: usize) -> bool {
    len * DENSE_SHARE > groups
}

/// Appends a grouped record's directory (see the module's grammar) for a
/// segment of `groups` groups: `entries` in group order with their totals
/// (`first`, `at`, `tf_bit`, and `frontier` an offset into `frontiers`), the
/// last's ends being the record's (its postings, container bytes and TF
/// bits, and `frontiers`' length). Returns the bytes before the frontiers.
pub fn encode_directory(
    groups: usize,
    entries: &[GroupEntry],
    frontiers: &[u8],
    out: &mut Vec<u8>,
) -> usize {
    let n = entries.len();
    assert!(n > 0 && n <= groups);
    let last = entries[n - 1];
    let end = [
        u64::from(last.first + last.count),
        u64::from(last.at + last.len),
        u64::from(last.tf_bit) + u64::from(last.count) * u64::from(last.width),
        frontiers.len() as u64,
    ];
    let total = |r: usize| -> [u64; 4] {
        if r == n {
            return end;
        }
        let e = &entries[r];
        [
            u64::from(e.first),
            u64::from(e.at),
            u64::from(e.tf_bit),
            u64::from(e.frontier),
        ]
    };
    // Each column's width: its largest delta from a block's base.
    let mut widths = [0u32; 4];
    for r in 0..=n {
        let (v, base) = (total(r), total(r / DIRECTORY_BLOCK * DIRECTORY_BLOCK));
        for c in 0..4 {
            widths[c] = widths[c].max(64 - (v[c] - base[c]).leading_zeros());
        }
    }
    let row = ROW_HEAD + widths.iter().sum::<u32>();
    assert!(row <= ROW_MAX, "a directory row of {row} bits");
    let start = out.len();
    varint::put(out, n as u64);
    out.extend(widths.map(|w| w as u8));
    let wide = index_bytes(groups);
    let put_index = |out: &mut Vec<u8>, v: usize| {
        out.extend_from_slice(&(v as u32).to_le_bytes()[..wide]);
    };
    if is_dense(n, groups) {
        let mut words = vec![0u64; groups.div_ceil(64)];
        for e in entries {
            words[e.index as usize / 64] |= 1 << (e.index % 64);
        }
        for w in &words {
            out.extend_from_slice(&w.to_le_bytes());
        }
        let mut rank = 0;
        for w in &words {
            put_index(out, rank);
            rank += w.count_ones() as usize;
        }
    } else {
        for e in entries {
            put_index(out, e.index as usize);
        }
    }
    for b in (0..=n).step_by(DIRECTORY_BLOCK) {
        for v in total(b) {
            let v = u32::try_from(v).expect("a record's totals fit 32 bits");
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    let mut rows = BitWriter::new();
    for r in 0..=n {
        let (v, base) = (total(r), total(r / DIRECTORY_BLOCK * DIRECTORY_BLOCK));
        let head = entries
            .get(r)
            .map_or(0, |e| u32::from(e.kind) | u32::from(e.max_bucket) << 2);
        rows.put(head, ROW_HEAD);
        for c in 0..4 {
            rows.put((v[c] - base[c]) as u32, widths[c]);
        }
    }
    out.extend_from_slice(&rows.finish());
    let before = out.len() - start;
    out.extend_from_slice(frontiers);
    before
}

/// Where a directory finds its groups: a bitmap over the segment's groups
/// with each word's rank (the entries before it), or the groups' indexes.
#[derive(Clone, Copy, Debug)]
enum Presence {
    Bitmap {
        words: usize,
        ranks: usize,
        len: usize,
    },
    Indexes {
        at: usize,
    },
}

/// A grouped record's directory: its bytes, copied when the record is
/// parsed (so the parsed record borrows nothing of the blob, and reading an
/// entry reads no page of it), read in place.
///
/// An entry is a row of fixed width: a seek finds the entry of a group by
/// the group's rank in the record's presence bitmap (a popcount), or among
/// its group indexes, and the entry's fields are a row's deltas from its
/// block's bases, with its count and container length the next row's less
/// its own. Nothing of the entries before it is decoded (TIN's footer entry
/// likewise locates a group's container directly).
///
/// The presence and the record's totals (the last row's) are checked when
/// the directory is parsed; each entry is checked as it is read: its group
/// holds its count, its container's length is its kind's, and its buckets
/// and frontier lie within the record's.
pub struct Directory {
    /// The directory's bytes, from its group count on, and [`SLACK`] zero
    /// bytes past them: entries' frontier offsets index them.
    bytes: Box<[u8]>,
    end: usize,
    len: usize,
    presence: Presence,
    /// Bytes of a group index or a word's rank.
    wide: usize,
    bases: usize,
    rows: usize,
    row_bits: u32,
    /// Where each column's delta lies in a row, and its mask.
    shift: [u32; 4],
    mask: [u64; 4],
    /// What the entries are checked against: the record's postings,
    /// container bytes and TF bits.
    df: u32,
    containers: u64,
    tf_bits: u64,
    /// Entries read since [`Self::take_read`] was last called.
    read: std::cell::Cell<u64>,
    /// The entry read last, and its position (`usize::MAX` before the
    /// first): a walk asks for a group's entry several times (its bound,
    /// its container, its buckets), through the record's clones.
    last: std::cell::Cell<(usize, GroupEntry)>,
    /// The row after the entry read last (its position, head and totals):
    /// the next entry's, which a walk in order reads next.
    ahead: std::cell::Cell<(usize, u32, [u32; 4])>,
}

impl std::fmt::Debug for Directory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Directory")
            .field("len", &self.len)
            .field("bytes", &self.end)
            .field("presence", &self.presence)
            .finish()
    }
}

impl Directory {
    /// Parses a directory, `directory` its bytes from its group count on, of
    /// a record of `df` postings, `containers` container bytes and `tf_len`
    /// TF bytes, in a segment of `groups` groups.
    fn parse(
        mut directory: Vec<u8>,
        groups: usize,
        df: u32,
        containers: u64,
        tf_len: u64,
    ) -> Result<Self> {
        let corrupt = || Error::Corrupt("postings directory");
        let mut at = 0;
        let len = varint::get_u32(&directory, &mut at)? as usize;
        if len == 0 || len > groups {
            return Err(Error::Corrupt("postings groups"));
        }
        let widths = directory.get(at..at + 4).ok_or(Error::Truncated)?;
        at += 4;
        let mut shift = [0u32; 4];
        let mut mask = [0u64; 4];
        let mut row_bits = ROW_HEAD;
        for c in 0..4 {
            let w = u32::from(widths[c]);
            if w > 32 {
                return Err(corrupt());
            }
            shift[c] = row_bits;
            mask[c] = (1u64 << w) - 1;
            row_bits += w;
        }
        if row_bits > ROW_MAX {
            return Err(corrupt());
        }
        let wide = index_bytes(groups);
        let presence_at = at;
        let presence = if is_dense(len, groups) {
            let words = groups.div_ceil(64);
            at += words * (8 + wide);
            Presence::Bitmap {
                words: presence_at,
                ranks: presence_at + 8 * words,
                len: words,
            }
        } else {
            at += len * wide;
            Presence::Indexes { at: presence_at }
        };
        let bases = at;
        at += (len / DIRECTORY_BLOCK + 1) * BASE_BYTES;
        let rows = at;
        at += ((len + 1) * row_bits as usize).div_ceil(8);
        if at > directory.len() {
            return Err(Error::Truncated);
        }
        let end = directory.len();
        directory.resize(end + SLACK, 0);
        let dir = Self {
            bytes: directory.into_boxed_slice(),
            end,
            len,
            presence,
            wide,
            bases,
            rows,
            row_bits,
            shift,
            mask,
            df,
            containers,
            tf_bits: tf_len * 8,
            read: std::cell::Cell::new(0),
            last: std::cell::Cell::new((usize::MAX, GroupEntry::default())),
            ahead: std::cell::Cell::new((usize::MAX, 0, [0; 4])),
        };
        // The presence: every group in the segment, ranks counting the
        // words before (and so `len` groups), indexes rising.
        match presence {
            Presence::Bitmap { words, len: n, .. } => {
                let mut rank = 0;
                for w in 0..n {
                    if dir.rank_before(w) != rank {
                        return Err(corrupt());
                    }
                    rank += dir.word(words, w).count_ones() as usize;
                }
                let tail = groups % 64;
                if rank != len || (tail != 0 && dir.word(words, n - 1) >> tail != 0) {
                    return Err(corrupt());
                }
            }
            Presence::Indexes { .. } => {
                let mut next = 0;
                for b in 0..len {
                    let index = dir.index(b) as usize;
                    if index < next || index >= groups {
                        return Err(corrupt());
                    }
                    next = index + 1;
                }
            }
        }
        // The record's totals: the last row's.
        let (_, totals) = dir.row(len);
        if u64::from(totals[FIRST]) != u64::from(df)
            || u64::from(totals[AT]) != containers
            || u64::from(totals[TF]).div_ceil(8) != tf_len
            || rows + ((len + 1) * row_bits as usize).div_ceil(8) + totals[FRONTIER] as usize
                != dir.end
        {
            return Err(Error::Corrupt("postings directory totals"));
        }
        Ok(dir)
    }

    /// Entries (groups the record holds).
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The directory's bytes, from its group count on.
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.end]
    }

    /// Bytes the parsed directory holds.
    pub fn held(&self) -> usize {
        self.bytes.len()
    }

    /// Entries read since this was last called (for accounting).
    pub fn take_read(&self) -> u64 {
        self.read.replace(0)
    }

    /// Whether the presence is a bitmap (the record is in more than one
    /// group in [`DENSE_SHARE`]).
    pub fn is_dense(&self) -> bool {
        matches!(self.presence, Presence::Bitmap { .. })
    }

    /// The `N` bytes at `at`, read in one load.
    ///
    /// `at + N` must lie within the bytes and their slack: the parse checked
    /// that the presence, bases and rows lie within the directory, and each
    /// caller reads a word, rank, index, base or row of them (a row's or an
    /// index's load runs past it into the bytes after, or the slack).
    #[inline(always)]
    fn load<const N: usize>(&self, at: usize) -> [u8; N] {
        assert!(N <= SLACK);
        debug_assert!(at + N <= self.bytes.len());
        // SAFETY: within the bytes, as above.
        unsafe { std::ptr::read_unaligned(self.bytes.as_ptr().add(at).cast::<[u8; N]>()) }
    }

    #[inline(always)]
    fn word(&self, words: usize, w: usize) -> u64 {
        u64::from_le_bytes(self.load(words + 8 * w))
    }

    /// The little-endian `u32` (or `u16`, `wide` 2) at `at`: a group index
    /// or a word's rank.
    #[inline(always)]
    fn read_index(&self, at: usize) -> usize {
        let v = u32::from_le_bytes(self.load(at));
        if self.wide == 2 {
            (v & 0xffff) as usize
        } else {
            v as usize
        }
    }

    /// Entries in a bitmap's words before word `w`.
    #[inline(always)]
    fn rank_before(&self, w: usize) -> usize {
        let Presence::Bitmap { ranks, .. } = self.presence else {
            unreachable!("a bitmap's ranks")
        };
        self.read_index(ranks + w * self.wide)
    }

    /// Row `r` (the record's totals at `len`): its head (kind and largest
    /// bucket) and its totals.
    #[inline(always)]
    fn row(&self, r: usize) -> (u32, [u32; 4]) {
        debug_assert!(r <= self.len);
        let bit = r * self.row_bits as usize;
        let raw = u128::from_le_bytes(self.load(self.rows + bit / 8)) >> (bit % 8);
        let bases = u128::from_le_bytes(self.load(self.bases + r / DIRECTORY_BLOCK * BASE_BYTES));
        let total = |c: usize| {
            let delta = (raw >> self.shift[c]) as u64 & self.mask[c];
            ((bases >> (32 * c)) as u32).wrapping_add(delta as u32)
        };
        (raw as u32 & 63, [total(0), total(1), total(2), total(3)])
    }

    /// The group index of entry `b`.
    #[inline]
    pub fn index(&self, b: usize) -> u32 {
        debug_assert!(b < self.len);
        match self.presence {
            Presence::Indexes { at } => self.read_index(at + b * self.wide) as u32,
            Presence::Bitmap { words, len, .. } => {
                // The last word whose rank is `b` or less holds it.
                let (mut lo, mut hi) = (0, len);
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    if self.rank_before(mid) <= b {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                let k = b - self.rank_before(lo);
                (lo * 64) as u32 + bits::select(self.word(words, lo), k)
            }
        }
    }

    /// The first entry from `from` on whose group index is `group` or more,
    /// and that index; the entry count and `u32::MAX` when there is none.
    /// A bitmap's is found from `group` (its rank); group indexes are
    /// galloped from `from`.
    #[inline]
    pub fn seek(&self, group: u32, from: usize) -> (usize, u32) {
        let (b, index) = self.seek_any(group, from);
        if b >= from {
            return (b, index);
        }
        // Asked from past it: the entry asked from, as a gallop gives.
        if from < self.len {
            (from, self.index(from))
        } else {
            (self.len, u32::MAX)
        }
    }

    #[inline(always)]
    fn seek_any(&self, group: u32, from: usize) -> (usize, u32) {
        match self.presence {
            Presence::Bitmap { words, len, .. } => {
                let mut w = group as usize / 64;
                if w >= len {
                    return (self.len, u32::MAX);
                }
                let mut word = self.word(words, w) & (u64::MAX << (group % 64));
                while word == 0 {
                    w += 1;
                    if w == len {
                        return (self.len, u32::MAX);
                    }
                    word = self.word(words, w);
                }
                let bit = word.trailing_zeros();
                let below = self.word(words, w) & ((1u64 << bit) - 1);
                (
                    self.rank_before(w) + below.count_ones() as usize,
                    (w * 64) as u32 + bit,
                )
            }
            Presence::Indexes { .. } => {
                let n = self.len;
                if from >= n {
                    return (n, u32::MAX);
                }
                if self.index(from) >= group {
                    return (from, self.index(from));
                }
                // `lo` below `group`, `hi` at or past it (or `n`).
                let (mut lo, mut step) = (from, 1);
                let mut hi = loop {
                    let at = lo + step;
                    if at >= n {
                        break n;
                    }
                    if self.index(at) >= group {
                        break at;
                    }
                    lo = at;
                    step *= 2;
                };
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    if self.index(mid) >= group {
                        hi = mid;
                    } else {
                        lo = mid;
                    }
                }
                (hi, if hi < n { self.index(hi) } else { u32::MAX })
            }
        }
    }

    /// The entry of group `group`, if the record holds it (sought from the
    /// entry read last when that is of an earlier group).
    #[inline]
    pub fn find(&self, group: u32) -> Option<usize> {
        let (at, last) = self.last.get();
        let from = if at < self.len && last.index <= group {
            at
        } else {
            0
        };
        let (b, index) = self.seek(group, from);
        (index == group).then_some(b)
    }

    /// Entry `b` of group `index` (its [`Self::index`]), checked against
    /// the record and the segment's `groups`: as a rule the entry read last
    /// (a walk asks for a group's entry for each of its fields).
    #[inline(always)]
    pub fn entry_of(&self, b: usize, index: u32, groups: &[Group]) -> Result<GroupEntry> {
        let (at, last) = self.last.get();
        if at == b {
            return Ok(last);
        }
        self.read(b, index, groups)
    }

    /// [`Self::entry_of`], read from its rows.
    #[inline(never)]
    fn read(&self, b: usize, index: u32, groups: &[Group]) -> Result<GroupEntry> {
        if b >= self.len {
            return Err(Error::Corrupt("group directory entry"));
        }
        let (head, v) = match self.ahead.get() {
            (r, head, totals) if r == b => (head, totals),
            _ => self.row(b),
        };
        let (next_head, next) = self.row(b + 1);
        self.ahead.set((b + 1, next_head, next));
        let group = groups
            .get(index as usize)
            .ok_or(Error::Corrupt("postings group index"))?;
        let kind = (head & 3) as u8;
        let max_bucket = (head >> 2) as u8;
        let width = TF_WIDTH[usize::from(max_bucket)];
        let count = next[FIRST].wrapping_sub(v[FIRST]);
        let len = next[AT].wrapping_sub(v[AT]);
        // A grid's and an array's length follow from the group and the
        // count, read as they are; an Elias-Fano list or a paged container
        // checks its own when parsed (its exact length is the verifier's,
        // [`Self::check_entries`]). Without a branch on the kind: the kinds
        // mix within a directory.
        let slots = group.slots();
        let grid = group.grid_bytes() as u32;
        let expected = if kind == KIND_GRID { grid } else { len };
        let expected = if kind == KIND_ARRAY {
            2 * count
        } else {
            expected
        };
        let ok = count >= 1
            && count <= slots
            && len == expected
            && len > 0
            && (kind != KIND_ARRAY || slots <= ARRAY_SLOTS_MAX)
            && u64::from(next[TF].wrapping_sub(v[TF])) == u64::from(count) * u64::from(width)
            && u64::from(next[TF]) <= self.tf_bits
            && u64::from(next[AT]) <= self.containers
            && next[FIRST] <= self.df
            && next[FRONTIER] > v[FRONTIER];
        if !ok {
            return Err(Error::Corrupt("group directory entry"));
        }
        self.read.set(self.read.get() + 1);
        let frontiers = self.rows + ((self.len + 1) * self.row_bits as usize).div_ceil(8);
        let entry = GroupEntry {
            index,
            count,
            kind,
            at: v[AT],
            len,
            first: v[FIRST],
            frontier: (frontiers + v[FRONTIER] as usize) as u32,
            tf_bit: v[TF],
            width,
            max_bucket,
        };
        self.last.set((b, entry));
        Ok(entry)
    }

    /// Entry `b`, its group index found first: the entry read last as it
    /// was, the one after it by the next group its presence holds (a walk
    /// in order selects nothing).
    #[inline(always)]
    pub fn get(&self, b: usize, groups: &[Group]) -> Result<GroupEntry> {
        let (at, last) = self.last.get();
        if at == b {
            return Ok(last);
        }
        self.get_other(b, groups)
    }

    /// [`Self::get`] of an entry other than the one read last.
    #[inline(never)]
    fn get_other(&self, b: usize, groups: &[Group]) -> Result<GroupEntry> {
        let (at, last) = self.last.get();
        if b >= self.len {
            return Err(Error::Corrupt("group directory entry"));
        }
        let index = if at != usize::MAX && at + 1 == b {
            self.seek_any(last.index + 1, b).1
        } else {
            self.index(b)
        };
        self.entry_of(b, index, groups)
    }

    /// The entry holding posting `index` (one of the record's): the last
    /// block whose base is at it or before it, then the last row of it.
    pub fn entry_of_posting(&self, index: u32, groups: &[Group]) -> Result<GroupEntry> {
        let base = |k: usize| u32::from_le_bytes(self.load(self.bases + k * BASE_BYTES));
        let blocks = self.len.div_ceil(DIRECTORY_BLOCK);
        let (mut lo, mut hi) = (0, blocks);
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if base(mid) <= index {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let (mut lo, mut hi) = (
            lo * DIRECTORY_BLOCK,
            ((lo + 1) * DIRECTORY_BLOCK).min(self.len),
        );
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if self.row(mid).1[FIRST] <= index {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let e = self.get(lo, groups)?;
        if e.first <= index && index < e.first + e.count {
            Ok(e)
        } else {
            Err(Error::Corrupt("posting index"))
        }
    }

    /// Every entry, in order, its group index read from the presence in
    /// order (not sought).
    pub fn each(&self, groups: &[Group], mut visit: impl FnMut(&GroupEntry)) -> Result<()> {
        match self.presence {
            Presence::Indexes { .. } => {
                for b in 0..self.len {
                    visit(&self.get(b, groups)?);
                }
            }
            Presence::Bitmap { words, len, .. } => {
                let mut b = 0;
                for w in 0..len {
                    let mut word = self.word(words, w);
                    while word != 0 {
                        let index = (w * 64) as u32 + word.trailing_zeros();
                        visit(&self.entry_of(b, index, groups)?);
                        word &= word - 1;
                        b += 1;
                    }
                }
            }
        }
        Ok(())
    }

    /// Every entry, read.
    pub fn entries(&self, groups: &[Group]) -> Result<Vec<GroupEntry>> {
        let mut out = Vec::with_capacity(self.len);
        self.each(groups, |e| out.push(*e))?;
        Ok(out)
    }

    /// Every entry read and checked whole: an Elias-Fano container's length
    /// is its count's (a reader checks only that the list fits), and the
    /// frontier ends where the next begins (the last, where the directory
    /// does), its largest bucket the row's.
    pub fn check_entries(&self, groups: &[Group]) -> Result<()> {
        let entries = self.entries(groups)?;
        for (i, e) in entries.iter().enumerate() {
            let slots = groups[e.index as usize].slots();
            if e.kind == KIND_EF && e.len as usize != ef::encoded_len(e.count as usize, slots) {
                return Err(Error::Corrupt("group container length"));
            }
            let mut at = e.frontier as usize;
            let max = skip_group_frontier(self.bytes(), &mut at)?;
            let end = entries.get(i + 1).map_or(self.end, |n| n.frontier as usize);
            if max != e.max_bucket || at != end {
                return Err(Error::Corrupt("group frontier"));
            }
        }
        Ok(())
    }
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
        KIND_ARRAY => {
            let array = array_bytes(bytes, entry.count, group_slots(starts))?;
            for c in array.chunks_exact(2) {
                visit(u32::from(u16::from_le_bytes([c[0], c[1]])));
            }
            Ok(())
        }
        _ => paged_for_each(bytes, starts, visit),
    }
}

/// An array container's `2 * count` bytes, checked to hold its values and
/// to be of a group of at most [`ARRAY_SLOTS_MAX`] slots. Its values are
/// checked to rise and to lie below the group's slots by the verifier
/// (`verify_segment`), not here: a reader takes them as they are.
#[inline]
pub fn array_bytes(bytes: &[u8], count: u32, slots: u32) -> Result<&[u8]> {
    if slots > ARRAY_SLOTS_MAX {
        return Err(Error::Corrupt("array container's group"));
    }
    bytes.get(..2 * count as usize).ok_or(Error::Truncated)
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

/// The larger of a running bound `best` (never NaN) and `v`: `best` when
/// `v` is NaN, as [`f32::max`] gives, but one compare and select. On
/// x86-64 `f32::max` is that and three more instructions to order NaNs, on
/// the chain a fold of bounds carries from pair to pair (`maxss`,
/// `cmpunordss`, and a blend), which held 4% of a disjunction's cycles on
/// Sapphire Rapids; on aarch64 it is one `fmaxnm`, this a compare and a
/// select.
#[inline(always)]
pub fn higher(best: f32, v: f32) -> f32 {
    if v > best { v } else { best }
}

/// The smaller of a running bound `best` (never NaN) and `v`: `best` when
/// `v` is NaN, as [`f32::min`] gives. On x86-64 one `minss` (`v < best ?
/// v : best`) rather than `f32::min`'s NaN ordering (a `minss`, a
/// `cmpunordss` and a blend); written as a compare and select, LLVM made it
/// a branch on the data. On aarch64 `f32::min` is already one `fminnm`.
#[inline(always)]
pub fn lower(best: f32, v: f32) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::{_mm_cvtss_f32, _mm_min_ss, _mm_set_ss};
        // SAFETY: SSE is part of the x86-64 baseline.
        unsafe { _mm_cvtss_f32(_mm_min_ss(_mm_set_ss(v), _mm_set_ss(best))) }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        best.min(v)
    }
}

/// A block's bound from its frontier: the best of `bound` over its pairs,
/// and 0 at least.
#[inline(always)]
fn fold_bound(pairs: &[(u8, u32)], bound: &mut impl FnMut(u8, u32) -> f32) -> f32 {
    pairs
        .iter()
        .map(|(bucket, length)| bound(*bucket, *length))
        .fold(0.0_f32, higher)
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
        best = higher(best, bound(bucket, length));
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
            dir.check_entries(&geometry.groups).unwrap();
            for (b, e) in entries.iter().enumerate() {
                assert_eq!(dir.find(e.index), Some(b));
                assert_eq!(dir.get(b, &geometry.groups).unwrap(), *e);
            }
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
                            // Arrays for lists of at most 64, of 2, of any
                            // length, and none.
                            array_max: match (block_size, grid_density) {
                                (1, _) => 64,
                                (3, 0) => 2,
                                (3, _) => u32::MAX,
                                _ => 0,
                            },
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
            let stats = round_trip(&geometry, &slots, buckets, &lengths, &options);
            // Arrays weigh as the lists they replace in the choice of a
            // term's form: it is the same without them.
            let lists = Options {
                array_max: 0,
                ..options
            };
            let without = round_trip(&geometry, &slots, buckets, &lengths, &lists);
            assert_eq!(stats.sparse, without.sparse);
            assert_eq!(without.array_excess, 0);
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
    /// directory byte past the first window is copied about once, and the
    /// record is the one parsed from memory.
    #[test]
    fn long_directories_are_copied_once() {
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
        let entries = dir.entries(&geometry.groups).unwrap();
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
        assert_eq!(memo_counts().directories, 1, "the directory copied");
        assert_eq!(read.entries(&geometry.groups).unwrap(), entries);
        let stitched = blob::stats()[Kind::Record as usize].stitched as usize;
        assert!(
            stitched <= directory + 64,
            "stitched {stitched} bytes for a {directory}-byte directory"
        );
        drop(parsed);
        unsafe { blob.close_span() };
    }

    #[test]
    fn short_group_lists_are_arrays() {
        // 512 full pages of 40 tuples (two groups); a term in one document
        // of 150 in the first group and one in 37 of 100 pages of the
        // second: lists of 69 and 109 members, Elias-Fano by size.
        let tids: Vec<Tid> = (0..512u32)
            .flat_map(|b| {
                (1..=40u16).map(move |o| Tid {
                    block: b,
                    offset: o,
                })
            })
            .collect();
        let members: Vec<Tid> = tids[..256 * 40]
            .iter()
            .step_by(150)
            .chain(tids[256 * 40..356 * 40].iter().step_by(37))
            .copied()
            .collect();
        let geometry = geometry(&tids);
        let slots: Vec<u32> = members
            .iter()
            .map(|t| geometry.slot_of(*t).unwrap())
            .collect();
        let buckets = vec![1u8; slots.len()];
        let lengths = vec![50; slots.len()];
        let options = |array_max| Options {
            array_max,
            sparse: false,
            ..Options::default()
        };
        let ef = round_trip(&geometry, &slots, &buckets, &lengths, &options(0));
        assert_eq!(ef.kinds, [0, 2, 0, 0]);
        // The shorter list becomes an array of two bytes a member, the
        // longer stays a list.
        let some = round_trip(&geometry, &slots, &buckets, &lengths, &options(69));
        assert_eq!(some.kinds, [0, 1, 0, 1]);
        let first = ef::encoded_len(69, geometry.groups[0].slots());
        assert_eq!(some.kind_bytes[3], 2 * 69);
        assert_eq!(some.payload, ef.payload - first + 2 * 69);
        let all = round_trip(&geometry, &slots, &buckets, &lengths, &options(u32::MAX));
        assert_eq!(all.kinds, [0, 0, 0, 2]);
        // Each array's members, as the containers give them.
        let mut out = Vec::new();
        encode(
            &geometry,
            &slots,
            &buckets,
            &lengths,
            &options(u32::MAX),
            &mut out,
        );
        let parsed = Postings::parse(&out, slots.len() as u32, &geometry).unwrap();
        let Form::Grouped(dir) = &parsed.form else {
            panic!("grouped")
        };
        let mut locals = Vec::new();
        for e in dir.entries(&geometry.groups).unwrap() {
            assert_eq!((e.kind, e.len), (KIND_ARRAY, 2 * e.count));
            let bytes = parsed.container(&e).all().unwrap();
            let base = geometry.groups[e.index as usize].slot_base;
            for_each_local(&e, bytes, geometry.starts(e.index as usize), |l| {
                locals.push(base + l)
            })
            .unwrap();
            assert!(
                array_bytes(bytes, e.count + 1, 1 << 16).is_err(),
                "past the container"
            );
            assert!(
                array_bytes(bytes, e.count, (1 << 16) + 1).is_err(),
                "too many slots"
            );
        }
        assert_eq!(locals, slots);
        check_all(&tids, &members, &buckets);
        // What the arrays take beyond the lists, which the choice of the
        // term's form leaves out.
        let second = ef::encoded_len(109, geometry.groups[1].slots());
        assert_eq!(all.array_excess, (2 * 178 - first - second) as isize);
        assert_eq!(ef.array_excess, 0);
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
        assert_eq!(stats.kinds, [2, 0, 0, 0]);
        // A bit per slot, a bit per bucket. The directory: its count, four
        // widths, a presence bitmap of one word and its rank, one block's
        // bases, three rows of 6 + 15 + 12 + 15 + 3 bits (postings, grid
        // bytes and TF bits of a group of 10,240 slots, two frontier
        // bytes), and per group a frontier of one pair, (1, 50), of two
        // bytes.
        let rows = (3 * (6 + 15 + 12 + 15 + 3) as usize).div_ceil(8);
        assert_eq!(
            stats.payload,
            2 * 32 * 40 + 1 + 4 + (8 + 2) + 16 + rows + 2 * 2
        );
        assert_eq!((stats.frontiers, stats.frontier_bytes), (2, 4));
        assert_eq!(stats.tf, tids.len() / 8);
        // Whole pages are runs, smaller still, when size alone decides; the
        // default keeps groups this dense as grids.
        let by_size = Options {
            grid_density: 0,
            ..Options::default()
        };
        let stats = round_trip(&geometry, &slots, &buckets, &lengths, &by_size);
        assert_eq!(stats.kinds, [0, 0, 2, 0]);
        let stats = round_trip(&geometry, &slots, &buckets, &lengths, &Options::default());
        assert_eq!((stats.kinds, stats.forced), ([2, 0, 0, 0], 2));
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
                dir.entries(&geometry.groups).unwrap().len(),
                groups as usize
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
mod directory_tests {
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

    /// A term: its geometry, slots, buckets and lengths.
    struct Term {
        geometry: Geometry,
        slots: Vec<u32>,
        buckets: Vec<u8>,
        lengths: Vec<u32>,
    }

    /// A grouped term over `groups` heap groups of up to four pages each,
    /// held in about one group in `spread` (the others empty; 1 for every
    /// group), its members in each group at one of several densities (so
    /// grid, Elias-Fano, paged and array containers mix), buckets at random
    /// or rising with length (long frontiers), lengths of 6 to 31 bits.
    fn term(seed: u64, groups: u32, spread: u64) -> Term {
        let mut rng = Rng(seed | 1);
        let mut tids = Vec::new();
        let mut held = Vec::new();
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
            held.push(g == 0 || rng.below(spread) == 0);
        }
        let geometry = Geometry::of(&tids).unwrap();
        let (mut slots, mut buckets, mut lengths) = (Vec::new(), Vec::new(), Vec::new());
        let bits = [6u32, 14, 20, 31][rng.below(4) as usize];
        let mut at = 0;
        for (g, held) in held.iter().enumerate() {
            let n = geometry.groups[g].slots() as usize;
            let density = [2u64, 20, 60, 100][rng.below(4) as usize];
            let rising = rng.below(4) == 0;
            for i in 0..n {
                if *held && (i == 0 || i + 1 == n || rng.below(100) < density) {
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
        Term {
            geometry,
            slots,
            buckets,
            lengths,
        }
    }

    fn options(exact: bool) -> Options {
        Options {
            sparse: false,
            grid_min_postings: 0,
            group_frontiers: if exact {
                GroupFrontiers::Exact
            } else {
                GroupFrontiers::Rounded
            },
            ..Options::default()
        }
    }

    fn encoded(t: &Term, options: &Options) -> Vec<u8> {
        let mut out = Vec::new();
        encode(
            &t.geometry,
            &t.slots,
            &t.buckets,
            &t.lengths,
            options,
            &mut out,
        );
        out
    }

    /// Checks a grouped record's directory against its postings worked out
    /// by brute force: each entry's group, count and first posting, its
    /// container's members, buckets and frontier; every group of the
    /// segment sought from the start, from its entry and from between, and
    /// found; every posting's entry.
    fn check_directory(record: &[u8], t: &Term, options: &Options) {
        let geometry = &t.geometry;
        let groups = &geometry.groups;
        let df = t.slots.len() as u32;
        let parsed = Postings::parse(record, df, geometry).unwrap();
        let Form::Grouped(dir) = &parsed.form else {
            panic!("a grouped record");
        };
        // (group, first posting, count) per group held.
        let mut want: Vec<(u32, u32, u32)> = Vec::new();
        for (i, slot) in t.slots.iter().enumerate() {
            let g = geometry.group_of_slot(*slot) as u32;
            match want.last_mut() {
                Some(w) if w.0 == g => w.2 += 1,
                _ => want.push((g, i as u32, 1)),
            }
        }
        assert_eq!(dir.len(), want.len());
        assert_eq!(
            dir.is_dense(),
            want.len() * DENSE_SHARE > groups.len(),
            "presence"
        );
        let entries = dir.entries(groups).unwrap();
        dir.check_entries(groups).unwrap();
        for (b, (e, w)) in entries.iter().zip(&want).enumerate() {
            assert_eq!((e.index, e.first, e.count), *w, "entry {b}");
            assert_eq!(dir.index(b), e.index, "index of entry {b}");
            assert_eq!(dir.get(b, groups).unwrap(), *e, "entry {b}");
            let base = groups[e.index as usize].slot_base;
            let mut members = Vec::new();
            for_each_local(
                e,
                parsed.container(e).all().unwrap(),
                geometry.starts(e.index as usize),
                |l| members.push(base + l),
            )
            .unwrap();
            let range = e.first as usize..(e.first + e.count) as usize;
            assert_eq!(members, t.slots[range.clone()], "members of entry {b}");
            for i in range.clone() {
                assert_eq!(parsed.bucket_in(e, i as u32).unwrap(), t.buckets[i]);
                assert_eq!(dir.entry_of_posting(i as u32, groups).unwrap(), *e);
            }
            let pairs = group_frontier(
                t.buckets[range.clone()]
                    .iter()
                    .copied()
                    .zip(t.lengths[range].iter().copied()),
                options.group_frontiers,
            );
            let read: Vec<(u8, u32)> = parsed
                .group_frontier(e)
                .map(|(b, l)| match options.group_frontiers {
                    GroupFrontiers::Rounded => (b, round_length(l)),
                    GroupFrontiers::Exact => (b, l),
                })
                .collect();
            assert_eq!(read, pairs, "frontier of entry {b}");
            assert_eq!(e.max_bucket, pairs.last().unwrap().0);
        }
        for g in 0..=groups.len() as u32 + 1 {
            let b = want.partition_point(|w| w.0 < g);
            let index = want.get(b).map_or(u32::MAX, |w| w.0);
            for from in [0, b / 2, b.saturating_sub(1), b] {
                assert_eq!(dir.seek(g, from), (b, index), "group {g} from {from}");
            }
            assert_eq!(dir.find(g), (index == g).then_some(b), "group {g}");
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

        /// A directory's entries, seeks and finds are those its postings
        /// give, its presence a bitmap or group indexes.
        #[test]
        fn directories_read_as_their_postings(
            seed in any::<u64>(),
            groups in 1u32..600,
            spread in prop::sample::select(vec![1u64, 3, 15, 16, 17, 40]),
            exact in any::<bool>(),
        ) {
            let t = term(seed, groups, spread);
            let options = options(exact);
            check_directory(&encoded(&t, &options), &t, &options);
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

    /// Presence on either side of one group in sixteen: a bitmap past it,
    /// group indexes at it and below.
    #[test]
    fn presence_turns_to_a_bitmap_past_one_group_in_sixteen() {
        let tids: Vec<Tid> = (0..320u32)
            .map(|g| Tid {
                block: g * 256,
                offset: 1,
            })
            .collect();
        let geometry = Geometry::of(&tids).unwrap();
        for held in [19usize, 20, 21] {
            let slots: Vec<u32> = (0..held)
                .map(|i| geometry.slot_of(tids[i * 15]).unwrap())
                .collect();
            let t = Term {
                buckets: vec![1; held],
                lengths: (0..held as u32).map(|i| 1 + i).collect(),
                slots,
                geometry: geometry.clone(),
            };
            let options = options(false);
            let record = encoded(&t, &options);
            let parsed = Postings::parse(&record, held as u32, &geometry).unwrap();
            let Form::Grouped(dir) = &parsed.form else {
                panic!("a grouped record");
            };
            assert_eq!(dir.is_dense(), held * 16 > 320, "{held} groups");
            check_directory(&record, &t, &options);
        }
    }

    /// A segment of more than 65,536 groups: group indexes and ranks of
    /// four bytes.
    #[test]
    fn wide_segments_have_four_byte_indexes() {
        let tids: Vec<Tid> = (0..70_000u32)
            .map(|g| Tid {
                block: g * 256,
                offset: 1,
            })
            .collect();
        let geometry = Geometry::of(&tids).unwrap();
        for step in [7usize, 1000] {
            let slots: Vec<u32> = (0..tids.len())
                .step_by(step)
                .map(|i| geometry.slot_of(tids[i]).unwrap())
                .collect();
            let n = slots.len();
            let t = Term {
                buckets: (0..n).map(|i| (i % 5) as u8).collect(),
                lengths: (0..n as u32).map(|i| 1 + i % 77).collect(),
                slots,
                geometry: geometry.clone(),
            };
            let options = options(false);
            check_directory(&encoded(&t, &options), &t, &options);
        }
    }

    /// Entries the rows hold at their widest: a count past 4,096 (a whole
    /// group of 256 pages of 40 rows), a frontier of sixteen pairs, a
    /// length past 2^28, then 300 groups of two rows.
    #[test]
    fn unusual_entries_read_as_their_postings() {
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
        let t = Term {
            geometry,
            slots,
            buckets,
            lengths,
        };
        let options = options(true);
        let record = encoded(&t, &options);
        let parsed = Postings::parse(&record, n as u32, &t.geometry).unwrap();
        let Form::Grouped(dir) = &parsed.form else {
            panic!("a grouped record");
        };
        let entries = dir.entries(&t.geometry.groups).unwrap();
        assert!(entries[0].count > 4096, "a count past 4,096");
        assert_eq!(
            parsed.group_frontier(&entries[1]).count(),
            16,
            "sixteen pairs"
        );
        assert!(
            parsed
                .group_frontier(&entries[2])
                .any(|(_, l)| l >= 1 << 28),
            "a length past 2^28"
        );
        check_directory(&record, &t, &options);
    }

    /// Any byte of a directory before its frontiers corrupted: reading it
    /// fails or gives entries, but never panics; its totals and presence
    /// are checked by the parse, its rows as they are read.
    #[test]
    fn corrupt_directories_fail_without_panicking() {
        for (seed, groups, spread) in [(3u64, 90u32, 1u64), (5, 300, 30)] {
            let t = term(seed, groups, spread);
            let record = encoded(&t, &options(false));
            let df = t.slots.len() as u32;
            let g = &t.geometry.groups;
            let mut at = 1;
            let directory_len = varint::get(&record, &mut at).unwrap() as usize;
            varint::get(&record, &mut at).unwrap();
            let parsed = Postings::parse(&record, df, &t.geometry).unwrap();
            let Form::Grouped(dir) = &parsed.form else {
                panic!("a grouped record");
            };
            let entries = dir.entries(g).unwrap();
            let frontiers = entries[0].frontier as usize;
            let mut detected = 0;
            for i in 0..frontiers {
                for flip in [1u8, 0x80, 0xff] {
                    let mut bad = record.clone();
                    bad[at + i] ^= flip;
                    let read = Postings::parse(&bad, df, &t.geometry).and_then(|p| {
                        let Form::Grouped(dir) = &p.form else {
                            return Ok(());
                        };
                        for b in 0..dir.len() {
                            let index = dir.index(b);
                            let e = dir.entry_of(b, index, g)?;
                            p.container(&e).all()?;
                            p.bucket_in(&e, e.first + e.count - 1)?;
                            p.group_frontier(&e).for_each(drop);
                        }
                        for group in 0..g.len() as u32 {
                            dir.seek(group, 0);
                            dir.find(group);
                        }
                        dir.check_entries(g)?;
                        p.slots(&t.geometry)?;
                        Ok(())
                    });
                    detected += usize::from(read.is_err());
                }
            }
            assert!(
                detected > frontiers,
                "{detected} of {} detected",
                3 * frontiers
            );
            // Cut short, or its totals one off: the parse fails.
            assert!(Postings::parse(&record[..record.len() - 1], df, &t.geometry).is_err());
            assert!(Postings::parse(&record, df - 1, &t.geometry).is_err());
            assert!(directory_len > frontiers);
        }
    }

    /// A directory parsed over pages read in place holds no page: once the
    /// span closes (its pages released and poisoned), its entries read as
    /// from memory, and reading them pins nothing.
    #[test]
    fn directories_read_no_page_after_the_parse() {
        let t = term(7, 900, 1);
        let record = encoded(&t, &options(false));
        let df = t.slots.len() as u32;
        let whole = {
            let parsed = Postings::parse(&record, df, &t.geometry).unwrap();
            let Form::Grouped(dir) = &parsed.form else {
                panic!("a grouped record");
            };
            dir.entries(&t.geometry.groups).unwrap()
        };
        let source = PinningSource::new(record.clone(), 64);
        let blob = LazyBlob::new(Box::new(source.clone()));
        blob.set_keep(0);
        blob.open_span();
        let parsed = Postings::parse(blob.bytes(), df, &t.geometry).unwrap();
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
        assert_eq!(dir.entries(&t.geometry.groups).unwrap(), whole);
        assert_eq!(source.0.pins.get(), pins, "no page pinned to read");
        assert_eq!(source.0.reads.get(), reads, "no page read to read");
        drop(parsed);
    }
}
