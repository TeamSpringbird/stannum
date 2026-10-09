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
//!                      [len varint, paged groups only])*,
//!                     container*
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
    let grouped = encode_groups(geometry, slots, options, reuse, &mut grouped_stats);
    // A term with a group dense enough to be a grid stays grouped.
    let use_sparse = options.sparse && grouped_stats.forced == 0 && sparse.len() <= grouped.len();
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
            | if compact { FORM_COMPACT } else { 0 },
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
    }
    stats
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
}

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
        let form = form & !(FORM_LENGTHS | FORM_COMPACT);
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
                // read a prefix of the payload, longer until it holds it.
                let mut want = DIRECTORY_PREFIX.min(payload.len());
                let (entries, at) = loop {
                    let directory = payload.window(0, want)?;
                    match parse_groups(directory, payload.len(), df, geometry) {
                        Err(Error::Truncated) if want < payload.len() => {
                            want = want.saturating_mul(4).min(payload.len());
                        }
                        parsed => break parsed?,
                    }
                };
                containers_at = at;
                Form::Grouped(entries.into())
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
}

/// Bytes of a grouped payload read first for its directory: a few groups'
/// entries take a few bytes each.
const DIRECTORY_PREFIX: usize = 4096;

/// A grouped payload's directory, from a prefix of the payload holding it
/// (`Truncated` when it does not), `payload_len` the whole payload's length.
fn parse_groups(
    payload: &[u8],
    payload_len: usize,
    df: u32,
    geometry: &Geometry,
) -> Result<(Vec<GroupEntry>, usize)> {
    let mut at = 0;
    let groups = varint::get_u32(payload, &mut at)? as usize;
    if groups > geometry.groups.len() {
        return Err(Error::Corrupt("postings groups"));
    }
    let mut entries = Vec::with_capacity(groups);
    let mut index: Option<u64> = None;
    let mut body = 0u64;
    let mut first = 0u64;
    for _ in 0..groups {
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
        entries.push(GroupEntry {
            index: next as u32,
            count: count as u32,
            kind,
            at: body as u32,
            len: len as u32,
            first: first as u32,
        });
        body += len;
        first += count;
    }
    if first != u64::from(df) || at as u64 + body != payload_len as u64 {
        return Err(Error::Corrupt("postings payload length"));
    }
    Ok((entries, at))
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
        let mut footer = Self {
            block_size,
            last: Vec::with_capacity(blocks),
            starts: Vec::with_capacity(blocks + 1),
            frontier: Vec::with_capacity(blocks * 2),
            tf_at: Vec::with_capacity(blocks),
            widths: Vec::with_capacity(blocks),
            entry_at: Vec::with_capacity(blocks),
            single: None,
        };
        let bytes = postings.footer.all()?;
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
        // A bit per slot, a bit per bucket.
        assert_eq!(stats.payload, 2 * 32 * 40 + 1 + 2 * (1 + 3));
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
