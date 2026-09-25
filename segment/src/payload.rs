// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Per-document token positions of one term.
//!
//! Entries are in the same order as the term's ordinal stream and are
//! addressed by the document's rank in it, so this stream is read only by
//! queries that need positions: phrases and other positional shapes. The
//! term-frequency bucket a score needs lives beside the member in the
//! ordinal stream.
//!
//! ```text
//! stream := count varint, skip u32le * slots, data
//! slots  := ceil(count / SKIP_INTERVAL) - 1, or 0 for an empty stream
//! entry  := n varint, position varint * n
//!           positions: first absolute, then (delta - 1)
//! ```
//!
//! Skip slot `i` holds the byte offset (relative to `data`) of entry
//! `(i + 1) * SKIP_INTERVAL`; entry 0 is at offset 0 and has no slot. Offsets
//! are fixed-width so a seek jumps to its slot in constant time; a ranked
//! scan seeks once per scored document. A stream of at most `SKIP_INTERVAL`
//! entries has no table at all.

use crate::reader::Reader;
use crate::{Error, Result, varint};

pub const SKIP_INTERVAL: u32 = 32;

#[derive(Default, Debug)]
pub struct PayloadBuilder {
    count: u32,
    skips: Vec<usize>,
    data: Vec<u8>,
}

impl PayloadBuilder {
    /// `positions` must be non-empty and strictly increasing.
    pub fn push(&mut self, positions: &[u32]) -> Result<()> {
        validate_positions(positions)?;
        if self.count.is_multiple_of(SKIP_INTERVAL) {
            self.skips.push(self.data.len());
        }
        self.count += 1;
        encode_positions(&mut self.data, positions);
        Ok(())
    }

    pub fn len(&self) -> u32 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() + self.skips.len() * 4 + 8);
        varint::put(&mut out, u64::from(self.count));
        for skip in self.skips.iter().skip(1) {
            out.extend_from_slice(&fixed_skip(*skip).to_le_bytes());
        }
        out.extend_from_slice(&self.data);
        out
    }
}

fn fixed_skip(skip: usize) -> u32 {
    u32::try_from(skip).expect("payload streams are far below 4 GiB")
}

pub fn validate_positions(positions: &[u32]) -> Result<()> {
    if positions.is_empty() || positions.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(Error::InvalidPositions);
    }
    Ok(())
}

pub(crate) fn encode_positions(out: &mut Vec<u8>, positions: &[u32]) {
    varint::put(out, positions.len() as u64);
    varint::put(out, u64::from(positions[0]));
    for pair in positions.windows(2) {
        varint::put(out, u64::from(pair[1] - pair[0] - 1));
    }
}

/// Skips the encoded position list at `at` without materializing it.
fn skip_positions(bytes: &[u8], at: &mut usize) -> Result<()> {
    let n = read_u32(bytes, at)?;
    if n == 0 {
        return Err(Error::InvalidPositions);
    }
    for _ in 0..n {
        read_u32(bytes, at)?;
    }
    Ok(())
}

/// The byte just past `entries` encoded position lists starting at `at`.
///
/// A seek within a skip slot passes over the entries before its target,
/// and for a frequent term those are most of what a phrase check reads.
/// Each entry's count is decoded, but its positions are only counted: a
/// varint ends at a byte without the high bit, so eight bytes at a time
/// are tested at once. The positions are not validated; an entry that is
/// decoded is, and so is every entry by `verify`.
fn skip_entries(bytes: &[u8], mut at: usize, entries: u32) -> Result<usize> {
    for _ in 0..entries {
        let n = read_u32(bytes, &mut at)?;
        if n == 0 {
            return Err(Error::InvalidPositions);
        }
        at = skip_varints(bytes, at, n)?;
    }
    Ok(at)
}

/// Bytes up to and including the `n`-th end of a varint among the eight
/// bytes of `word` (little-endian), or `None` when fewer end there.
#[inline(always)]
fn nth_end(word: u64, n: u32) -> Option<u32> {
    const ONES: u64 = 0x0101_0101_0101_0101;
    // One per byte that ends a varint, then per byte the ends up to it:
    // at most eight, so no byte carries into the next.
    let ends = (!word & HIGH) >> 7;
    let upto = ends.wrapping_mul(ONES);
    // Per byte, its high bit set once its count reaches `n` (`n` <= 8, and
    // a count plus 0x80 - n stays below 0x100).
    let reached = upto.wrapping_add(ONES * u64::from(0x80 - n)) & HIGH;
    (reached != 0).then(|| reached.trailing_zeros() / 8 + 1)
}

/// The high bit of every byte of a word: set on a varint's bytes but its last.
const HIGH: u64 = 0x8080_8080_8080_8080;

/// A varint that fits `u32`, with a fast path for the one-byte values most
/// positions and counts are.
#[inline(always)]
fn read_u32(bytes: &[u8], at: &mut usize) -> Result<u32> {
    if let Some(&byte) = bytes.get(*at)
        && byte < 0x80
    {
        *at += 1;
        return Ok(u32::from(byte));
    }
    varint::get_u32(bytes, at)
}

/// The byte just past `n` varints starting at `at`.
#[inline]
fn skip_varints(bytes: &[u8], mut at: usize, mut n: u32) -> Result<usize> {
    while n > 0 {
        if let Some(chunk) = bytes.get(at..at + 8) {
            let word = u64::from_le_bytes(chunk.try_into().expect("eight bytes"));
            if n <= 8
                && let Some(len) = nth_end(word, n)
            {
                return Ok(at + len as usize);
            }
            n -= (!word & HIGH).count_ones().min(n);
            at += 8;
            continue;
        }
        let byte = *bytes.get(at).ok_or(Error::Truncated)?;
        at += 1;
        if byte < 0x80 {
            n -= 1;
        }
    }
    Ok(at)
}

/// Appends decoded positions to `into` and returns how many were read.
pub(crate) fn decode_positions(reader: &mut Reader<'_>, into: &mut Vec<u32>) -> Result<usize> {
    visit_positions(reader, |position| into.push(position))
}

fn visit_positions(reader: &mut Reader<'_>, visit: impl FnMut(u32)) -> Result<usize> {
    visit_varints(|| reader.varint_u32(), visit)
}

/// Decodes the entry at `at` of `bytes`, as [`visit_positions`] does.
#[inline]
fn visit_entry(bytes: &[u8], at: &mut usize, mut visit: impl FnMut(u32)) -> Result<usize> {
    // Most entries of a frequent term are a few one-byte positions: read
    // them from one word.
    let start = *at;
    let n = read_u32(bytes, at)?;
    if (1..=8).contains(&n)
        && let Some(chunk) = bytes.get(*at..*at + 8)
    {
        let word = u64::from_le_bytes(chunk.try_into().expect("eight bytes"));
        if word & HIGH & (u64::MAX >> (64 - 8 * n)) == 0 {
            // At most 127 + 7 * 128: no overflow.
            let mut position = (word & 0x7f) as u32;
            visit(position);
            for i in 1..n {
                position += ((word >> (8 * i)) & 0x7f) as u32 + 1;
                visit(position);
            }
            *at += n as usize;
            return Ok(n as usize);
        }
    }
    *at = start;
    visit_varints(|| read_u32(bytes, at), visit)
}

/// Decodes one entry from its varints in turn.
#[inline(always)]
fn visit_varints(
    mut next: impl FnMut() -> Result<u32>,
    mut visit: impl FnMut(u32),
) -> Result<usize> {
    let n = next()?;
    if n == 0 {
        return Err(Error::InvalidPositions);
    }
    let mut position = next()?;
    visit(position);
    for _ in 1..n {
        let delta = next()?;
        position = position
            .checked_add(delta)
            .and_then(|p| p.checked_add(1))
            .ok_or(Error::Corrupt("position overflow"))?;
        visit(position);
    }
    Ok(n as usize)
}

/// A decoded entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub positions: Vec<u32>,
}

/// Where a stream's bytes come from. A paged source hands out ranges, so a
/// cursor reads the header and skip table once and then only the spans of
/// entries it visits; a frequent term's positions run to megabytes and a
/// phrase touches a sliver of them.
#[derive(Clone, Copy)]
enum Bytes<'a> {
    Whole(&'a [u8]),
    Ranged {
        areas: &'a dyn crate::segment::AreaFetch,
        /// The stream's offset in the payload area.
        base: u64,
        len: usize,
    },
}

impl std::fmt::Debug for Bytes<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Whole(bytes) => write!(f, "Whole({} bytes)", bytes.len()),
            Self::Ranged { base, len, .. } => write!(f, "Ranged({base}, {len} bytes)"),
        }
    }
}

/// Most skip slots per fetched span of a ranged stream: about 2,048
/// entries. A cursor that jumps fetches one slot; one that sweeps doubles
/// its span up to this, so scoring a candidate costs a page or two of
/// payload and a phrase over a frequent term still reads in long runs.
const SPAN_SLOTS: usize = 64;

#[derive(Clone, Copy, Debug)]
pub struct Payload<'a> {
    source: Bytes<'a>,
    /// The stream up to its first entry: the count and the skip table.
    head: &'a [u8],
    len: usize,
    count: u32,
    skips_at: usize,
    data_at: usize,
}

impl<'a> Payload<'a> {
    /// Opens a stream of `len` bytes at `base` of a paged source's payload
    /// area, reading only its header and skip table.
    pub(crate) fn open(
        areas: &'a dyn crate::segment::AreaFetch,
        base: u64,
        len: usize,
    ) -> Result<Self> {
        // The count decides how long the skip table is.
        let probe = areas.payload_range(base, len.min(8))?;
        let count = Reader::new(probe).varint_u32()?;
        let head_len = Self::parse(probe).map_or_else(
            |_| {
                let slots = (count as usize).div_ceil(SKIP_INTERVAL as usize);
                // Count varint, then the table.
                (10 + slots * 4).min(len)
            },
            |parsed| parsed.data_at,
        );
        let head = areas.payload_range(base, head_len)?;
        let parsed = Self::parse(head)?;
        Ok(Self {
            source: Bytes::Ranged { areas, base, len },
            head: &head[..parsed.data_at],
            len,
            ..parsed
        })
    }

    /// Parses a stream held whole.
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        let count = reader.varint_u32()?;
        let slots = (count as usize)
            .div_ceil(SKIP_INTERVAL as usize)
            .saturating_sub(1);
        let skips_at = reader.position();
        reader.skip(slots * 4)?;
        let data_at = reader.position();
        Ok(Self {
            source: Bytes::Whole(bytes),
            head: &bytes[..data_at],
            len: bytes.len(),
            count,
            skips_at,
            data_at,
        })
    }

    pub const fn count(&self) -> u32 {
        self.count
    }

    /// Bytes of the skip table, for size accounting.
    pub const fn skip_table_len(&self) -> usize {
        self.data_at - self.skips_at
    }

    /// Bytes of the entries after the header and skip table.
    pub const fn data_len(&self) -> usize {
        self.len - self.data_at
    }

    /// Byte position of the skip-table entry containing `ordinal`, and the
    /// ordinal that entry starts at.
    fn skip_to(&self, ordinal: u32) -> Result<(usize, u32)> {
        if ordinal >= self.count {
            return Err(Error::Corrupt("payload ordinal out of range"));
        }
        let slot = (ordinal / SKIP_INTERVAL) as usize;
        let offset = match slot.checked_sub(1) {
            Some(slot) => {
                let at = self.skips_at + slot * 4;
                let bytes = &self.head[at..at + 4];
                u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize
            }
            None => 0,
        };
        let at = self
            .data_at
            .checked_add(offset)
            .filter(|at| *at <= self.len)
            .ok_or(Error::Truncated)?;
        Ok((at, slot as u32 * SKIP_INTERVAL))
    }

    /// Where skip slot `slot` starts in the stream; past the last, its end.
    fn slot_at(&self, slot: usize) -> Result<usize> {
        if slot >= (self.count as usize).div_ceil(SKIP_INTERVAL as usize) {
            return Ok(self.len);
        }
        self.skip_to(slot as u32 * SKIP_INTERVAL).map(|(at, _)| at)
    }

    pub fn cursor(&self) -> PayloadCursor<'a> {
        let reader = match self.source {
            Bytes::Whole(bytes) => Reader::at(bytes, self.data_at),
            Bytes::Ranged { .. } => Reader::new(&[]),
        };
        PayloadCursor {
            payload: *self,
            reader,
            owned: std::rc::Rc::from(Vec::new()),
            held: None,
            owned_at: 0,
            slots: (0, 0),
            span_at: 0,
            next_ordinal: 0,
            in_place: InPlace::Off,
        }
    }

    /// Random access to one entry.
    pub fn get(&self, ordinal: u32) -> Result<Entry> {
        let mut cursor = self.cursor();
        cursor.seek(ordinal)?;
        cursor.next_entry()
    }
}

/// Sequential reader that can jump to an ordinal through the skip table.
#[derive(Clone, Debug)]
pub struct PayloadCursor<'a> {
    payload: Payload<'a>,
    /// Over the whole stream; unused for a ranged one.
    reader: Reader<'a>,
    /// The loaded span of a ranged stream, owned: it is read once and
    /// replaced by the next, so a sweep of a frequent term holds one span.
    owned: std::rc::Rc<[u8]>,
    /// The loaded span in place instead, as its first byte and length, on
    /// a page the source holds pinned (see [`PayloadCursor::hold_in_place`]).
    held: Option<(*const u8, usize)>,
    owned_at: usize,
    /// The loaded span of a ranged stream as (first skip slot, slots), and
    /// where it starts in the stream.
    slots: (usize, usize),
    span_at: usize,
    next_ordinal: u32,
    /// Whether spans may be read in place, and the slot they are held in.
    in_place: InPlace,
}

/// A cursor's reading of spans in place (see [`PayloadCursor::hold_in_place`]).
#[derive(Clone, Copy, Debug)]
enum InPlace {
    Off,
    /// Allowed, no slot handed out yet.
    Allowed,
    /// Held in this slot of the source.
    Slot(usize),
}

impl PayloadCursor<'_> {
    /// Reads the spans a ranged stream loads in place from the page the
    /// source holds pinned for the cursor, where the source holds pages
    /// and a span lies on one page; the rest are copied as before. A
    /// phrase check loads a span per candidate slot it reads, and copying
    /// it through the read cache was most of the reading.
    ///
    /// # Safety
    ///
    /// Once a span is loaded within a source's
    /// [`crate::source::Source::hold`] span, the cursor must not be used
    /// after that span closes.
    pub unsafe fn hold_in_place(&mut self) {
        if matches!(self.payload.source, Bytes::Ranged { .. }) {
            self.in_place = InPlace::Allowed;
        }
    }

    /// Whether the loaded span is read in place.
    pub fn is_held(&self) -> bool {
        self.held.is_some()
    }

    /// The loaded span of a ranged stream.
    #[inline]
    fn span(&self) -> &[u8] {
        match self.held {
            // SAFETY: the page stays pinned until the next load on the
            // cursor's slot, which replaces `held`, or the end of the hold
            // span, after which `hold_in_place`'s contract bars any use.
            Some((data, len)) => unsafe { std::slice::from_raw_parts(data, len) },
            None => &self.owned,
        }
    }

    /// Loads skip slots `slot..slot + count`, bytes `start..` of the
    /// stream, in place when the source holds a page for the cursor: as
    /// many of the slots as end on the page `start` is on, at least one.
    /// `Some(Ok(0))` where the first slot runs over its page's end: the
    /// caller copies it alone, and the next load is in place again. `None`
    /// where the source holds no page; the caller copies them all.
    fn load_held(
        &mut self,
        areas: &dyn crate::segment::AreaFetch,
        base: u64,
        slot: usize,
        count: usize,
        start: usize,
    ) -> Option<Result<usize>> {
        let held = match self.in_place {
            InPlace::Off => return None,
            InPlace::Slot(held) => held,
            InPlace::Allowed => {
                let held = areas.held_slot()?;
                self.in_place = InPlace::Slot(held);
                held
            }
        };
        let first = match self.payload.slot_at(slot + 1) {
            Ok(end) => end,
            Err(error) => return Some(Err(error)),
        };
        if first <= start {
            return Some(Ok(0));
        }
        let range = match areas.payload_held(held, base + start as u64, 1)? {
            Ok(range) => range,
            Err(error) => return Some(Err(error)),
        };
        let (data, on_page) = range.head();
        if on_page < first - start {
            return Some(Ok(0));
        }
        // The most slots of the `count` wanted that end on the page: the
        // first ends there, so search the rest.
        let (mut take, mut end) = (1, first);
        let (mut lo, mut hi) = (2, count);
        while lo <= hi {
            let mid = (lo + hi) / 2;
            let at = match self.payload.slot_at(slot + mid) {
                Ok(at) => at,
                Err(error) => return Some(Err(error)),
            };
            if at - start <= on_page {
                (take, end) = (mid, at);
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
        }
        self.held = Some((data, end - start));
        Some(Ok(take))
    }

    /// Ordinal the next `next()` call will decode.
    pub const fn next_ordinal(&self) -> u32 {
        self.next_ordinal
    }

    /// Makes the reader cover the entry at `next_ordinal`: nothing to do for
    /// a whole stream; a ranged one fetches the span of skip slots holding it.
    fn load(&mut self) -> Result<()> {
        let Bytes::Ranged { areas, base, .. } = self.payload.source else {
            return Ok(());
        };
        let slot = (self.next_ordinal / SKIP_INTERVAL) as usize;
        let (first, count) = self.slots;
        if count != 0 && slot >= first && slot < first + count {
            return Ok(());
        }
        let sequential = count != 0 && slot == first + count;
        let count = if sequential {
            (count * 2).min(SPAN_SLOTS)
        } else {
            1
        };
        let start = self.payload.slot_at(slot)?;
        let end = self.payload.slot_at(slot + count)?;
        if end < start {
            return Err(Error::Corrupt("payload skip order"));
        }
        self.held = None;
        let count = match self.load_held(areas, base, slot, count, start) {
            Some(Ok(0)) => {
                let end = self.payload.slot_at(slot + 1)?;
                self.owned = areas.payload_range_owned(base + start as u64, end - start)?;
                1
            }
            Some(taken) => taken?,
            None => {
                self.owned = areas.payload_range_owned(base + start as u64, end - start)?;
                count
            }
        };
        self.owned_at = 0;
        self.slots = (slot, count);
        self.span_at = start;
        Ok(())
    }

    fn ranged(&self) -> bool {
        matches!(self.payload.source, Bytes::Ranged { .. })
    }

    fn set_position(&mut self, at: usize) -> Result<()> {
        if self.ranged() {
            if at > self.span().len() {
                return Err(Error::Truncated);
            }
            self.owned_at = at;
            Ok(())
        } else {
            self.reader.seek(at)
        }
    }

    /// Decodes at the position, over the whole stream or the loaded span:
    /// `f` reads from the bytes at the offset it is given and moves it on.
    fn decode<R>(&mut self, f: impl FnOnce(&[u8], &mut usize) -> Result<R>) -> Result<R> {
        let (value, at) = {
            let (bytes, mut at) = match self.payload.source {
                Bytes::Whole(bytes) => (bytes, self.reader.position()),
                Bytes::Ranged { .. } => (self.span(), self.owned_at),
            };
            (f(bytes, &mut at)?, at)
        };
        self.set_position(at)?;
        Ok(value)
    }

    /// Positions so the next decode returns entry `ordinal`.
    pub fn seek(&mut self, ordinal: u32) -> Result<()> {
        if ordinal >= self.payload.count {
            return Err(Error::Corrupt("payload ordinal out of range"));
        }
        let interval = SKIP_INTERVAL;
        let forward_only = ordinal >= self.next_ordinal
            && ordinal - self.next_ordinal < interval
            && ordinal / interval == self.next_ordinal / interval;
        if !forward_only {
            let (at, start) = self.payload.skip_to(ordinal)?;
            self.next_ordinal = start;
            self.load()?;
            self.set_position(at - self.span_at)?;
        }
        if self.next_ordinal < ordinal {
            // The target shares a skip slot with the next entry, so the
            // loaded span holds every entry in between.
            self.load()?;
            let skip = ordinal - self.next_ordinal;
            self.decode(|bytes, at| {
                *at = skip_entries(bytes, *at, skip)?;
                Ok(())
            })?;
            self.next_ordinal = ordinal;
        }
        Ok(())
    }

    /// Skips the next entry.
    pub fn skip_entry(&mut self) -> Result<()> {
        if self.next_ordinal >= self.payload.count {
            return Err(Error::Corrupt("payload read past end"));
        }
        self.load()?;
        self.decode(skip_positions)?;
        self.next_ordinal += 1;
        Ok(())
    }

    /// Decodes the next entry, appending its positions to `positions`.
    pub fn next_into(&mut self, positions: &mut Vec<u32>) -> Result<()> {
        if self.next_ordinal >= self.payload.count {
            return Err(Error::Corrupt("payload read past end"));
        }
        self.load()?;
        self.decode(|bytes, at| visit_entry(bytes, at, |position| positions.push(position)))?;
        self.next_ordinal += 1;
        Ok(())
    }

    /// Validate every position and return its count without materializing it.
    /// Unlike `skip_entry`, this checks cumulative position overflow as well.
    pub(crate) fn next_count(&mut self) -> Result<usize> {
        if self.next_ordinal >= self.payload.count {
            return Err(Error::Corrupt("payload read past end"));
        }
        self.load()?;
        let count = self.decode(|bytes, at| visit_entry(bytes, at, |_| {}))?;
        self.next_ordinal += 1;
        Ok(count)
    }

    pub fn next_entry(&mut self) -> Result<Entry> {
        let mut positions = Vec::new();
        self.next_into(&mut positions)?;
        Ok(Entry { positions })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n: u32) -> Vec<Entry> {
        (0..n)
            .map(|i| Entry {
                positions: (0..=(i % 5)).map(|k| i * 7 + k * (k + 1) + 1).collect(),
            })
            .collect()
    }

    fn build(entries: &[Entry]) -> Vec<u8> {
        let mut builder = PayloadBuilder::default();
        for entry in entries {
            builder.push(&entry.positions).unwrap();
        }
        builder.finish()
    }

    /// A paged payload area holding one stream after some padding, counting
    /// the bytes it hands out.
    struct Area {
        bytes: Vec<u8>,
        fetched: std::cell::Cell<usize>,
    }

    impl crate::segment::AreaFetch for Area {
        fn ordinals_bytes(&self, _: u64, _: usize) -> Result<&[u8]> {
            unreachable!()
        }
        fn payload_bytes(&self, _: crate::dictionary::Extent) -> Result<&[u8]> {
            unreachable!()
        }
        fn doc_table(&self) -> Result<crate::docs::DocTable<'_>> {
            unreachable!()
        }
        fn length(&self, _: u32) -> Result<u32> {
            unreachable!()
        }
        fn length_class(&self, _: u32) -> Result<u8> {
            unreachable!()
        }
        fn ranged_payloads(&self) -> bool {
            true
        }
        fn payload_range(&self, offset: u64, len: usize) -> Result<&[u8]> {
            self.fetched.set(self.fetched.get() + len);
            self.bytes
                .get(offset as usize..offset as usize + len)
                .ok_or(Error::Truncated)
        }
    }

    #[test]
    fn ranged_streams_read_like_whole_ones_and_fetch_only_what_they_visit() {
        // Several spans of skip slots, with a short last one.
        let entries = sample(5 * SPAN_SLOTS as u32 * SKIP_INTERVAL + 77);
        let stream = build(&entries);
        let mut bytes = vec![0xAB; 13];
        bytes.extend_from_slice(&stream);
        bytes.extend_from_slice(&[0xCD; 9]);
        let area = Area {
            bytes,
            fetched: std::cell::Cell::new(0),
        };
        let payload = Payload::open(&area, 13, stream.len()).unwrap();
        assert_eq!(payload.count(), entries.len() as u32);
        assert_eq!(
            payload.data_len(),
            Payload::parse(&stream).unwrap().data_len()
        );
        let mut cursor = payload.cursor();
        for entry in &entries {
            assert_eq!(&cursor.next_entry().unwrap(), entry);
        }
        assert!(cursor.next_entry().is_err());
        // Backwards, across spans, to the last entry, and within a slot.
        for ordinal in [
            entries.len() as u32 - 1,
            3,
            2 * SPAN_SLOTS as u32 * SKIP_INTERVAL - 1,
            2 * SPAN_SLOTS as u32 * SKIP_INTERVAL,
            2 * SPAN_SLOTS as u32 * SKIP_INTERVAL + 5,
            0,
        ] {
            cursor.seek(ordinal).unwrap();
            assert_eq!(
                cursor.next_entry().unwrap(),
                entries[ordinal as usize],
                "{ordinal}"
            );
        }
        // One lookup costs the head and one span, not the stream.
        area.fetched.set(0);
        let payload = Payload::open(&area, 13, stream.len()).unwrap();
        assert_eq!(payload.get(40).unwrap(), entries[40]);
        assert!(
            area.fetched.get() < stream.len() / 3,
            "{}",
            area.fetched.get()
        );
        // A stream shorter than its table says is an error, not a panic.
        assert!(Payload::open(&area, 13, 6).is_err());
    }

    #[test]
    fn random_and_sequential_access_agree_across_skip_boundaries() {
        let entries = sample(6 * SKIP_INTERVAL + 5);
        let bytes = build(&entries);
        let payload = Payload::parse(&bytes).unwrap();
        assert_eq!(payload.count(), entries.len() as u32);
        for (ordinal, entry) in entries.iter().enumerate() {
            assert_eq!(
                &payload.get(ordinal as u32).unwrap(),
                entry,
                "ordinal {ordinal}"
            );
        }
        let mut cursor = payload.cursor();
        for entry in &entries {
            assert_eq!(&cursor.next_entry().unwrap(), entry);
        }
        assert!(cursor.next_entry().is_err());
        // Backwards, forwards within a slot, and far forwards.
        for ordinal in [190u32, 5, 6, 70, 69, 63, 64, 0, 196] {
            cursor.seek(ordinal).unwrap();
            assert_eq!(
                &cursor.next_entry().unwrap(),
                &entries[ordinal as usize],
                "seek {ordinal}"
            );
        }
        assert!(cursor.seek(entries.len() as u32).is_err());
        assert!(payload.get(u32::MAX).is_err());
    }

    #[test]
    fn counting_rejects_cumulative_overflow_even_when_each_varint_fits() {
        // One entry of two positions. The second delta is representable,
        // but adding it and the implicit one overflows u32.
        let mut bytes = vec![1, 2];
        varint::put(&mut bytes, u64::from(u32::MAX));
        varint::put(&mut bytes, 0);
        let payload = Payload::parse(&bytes).unwrap();
        let mut cursor = payload.cursor();
        assert_eq!(
            cursor.next_count(),
            Err(Error::Corrupt("position overflow"))
        );
        assert_eq!(cursor.next_ordinal(), 0);
        assert_eq!(payload.cursor().skip_entry(), Ok(()));
    }

    #[test]
    fn counted_positions_match_materialized_decode_and_failures() {
        // Exercise skip boundaries, seeks, overflow and truncation.
        {
            let mut entries = sample(70);
            entries[3].positions = vec![0, u32::MAX];
            let bytes = build(&entries);
            let compare = |bytes: &[u8]| {
                let Ok(payload) = Payload::parse(bytes) else {
                    return;
                };
                for start in [None, Some(0), Some(31), Some(32), Some(64), Some(69)] {
                    let mut count = payload.cursor();
                    let mut decode = payload.cursor();
                    if let Some(start) = start {
                        let a = count.seek(start);
                        let b = decode.seek(start);
                        assert_eq!(a, b);
                        if a.is_err() {
                            continue;
                        }
                    }
                    for _ in 0..=entries.len() {
                        let mut positions = Vec::new();
                        let expected = decode.next_into(&mut positions).map(|()| positions.len());
                        assert_eq!(count.next_count(), expected);
                        assert_eq!(count.next_ordinal(), decode.next_ordinal());
                        if expected.is_err() {
                            break;
                        }
                    }
                }
            };
            compare(&bytes);
            for at in 0..bytes.len() {
                compare(&bytes[..at]);
                for value in [0, 0x80, 0xff] {
                    let mut corrupted = bytes.clone();
                    corrupted[at] = value;
                    compare(&corrupted);
                }
            }
        }
    }

    /// Random position lists with deltas of every varint width.
    fn random_entries(seed: u64, n: usize) -> Vec<Entry> {
        let mut state = seed | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        (0..n)
            .map(|_| {
                let len = 1 + (next() % if next() % 4 == 0 { 40 } else { 4 }) as usize;
                let mut position = 0u32;
                let positions = (0..len)
                    .map(|i| {
                        let width = [3, 7, 14, 21, 28][(next() % 5) as usize];
                        let step = (next() % (1 << width)) as u32;
                        position = if i == 0 { step } else { position + step + 1 };
                        position
                    })
                    .collect();
                Entry { positions }
            })
            .collect()
    }

    #[test]
    fn skipping_entries_lands_where_decoding_them_does() {
        for seed in 1..40 {
            let entries = random_entries(seed, 50);
            let mut data = Vec::new();
            let mut ends = vec![0];
            for entry in &entries {
                encode_positions(&mut data, &entry.positions);
                ends.push(data.len());
            }
            for (ordinal, entry) in entries.iter().enumerate() {
                let mut at = ends[ordinal];
                let mut decoded = Vec::new();
                let n = visit_entry(&data, &mut at, |p| decoded.push(p)).unwrap();
                assert_eq!(
                    (n, &decoded, at),
                    (entry.positions.len(), &entry.positions, ends[ordinal + 1])
                );
            }
            for from in 0..entries.len() {
                for count in 0..=(entries.len() - from).min(33) {
                    assert_eq!(
                        skip_entries(&data, ends[from], count as u32),
                        Ok(ends[from + count]),
                        "seed {seed} from {from} count {count}"
                    );
                    // Cut short, it fails rather than reading past the end.
                    if count > 0 {
                        let cut = &data[..ends[from + count] - 1];
                        assert!(skip_entries(cut, ends[from], count as u32).is_err());
                    }
                }
            }
            // A zero count is invalid, as when decoding.
            assert_eq!(skip_entries(&[0], 0, 1), Err(Error::InvalidPositions));
        }
    }

    #[test]
    fn short_streams_have_no_skip_table_and_long_ones_omit_the_zero_slot() {
        for n in [0, 1, 31, 32, 33, 64, 65] {
            let entries = sample(n);
            let bytes = build(&entries);
            let payload = Payload::parse(&bytes).unwrap();
            let slots = (n as usize)
                .div_ceil(SKIP_INTERVAL as usize)
                .saturating_sub(1);
            assert_eq!(payload.skip_table_len(), slots * 4, "{n} entries");
            assert_eq!(
                payload.data_len() + payload.skip_table_len() + 1,
                bytes.len()
            );
            for (ordinal, entry) in entries.iter().enumerate().rev() {
                assert_eq!(&payload.get(ordinal as u32).unwrap(), entry);
            }
        }
    }

    #[test]
    fn empty_payload_round_trips() {
        let bytes = build(&[]);
        let payload = Payload::parse(&bytes).unwrap();
        assert_eq!(payload.count(), 0);
        assert!(payload.get(0).is_err());
        assert!(payload.cursor().next_entry().is_err());
    }

    #[test]
    fn builder_validates_input() {
        let mut builder = PayloadBuilder::default();
        assert_eq!(builder.push(&[]), Err(Error::InvalidPositions));
        assert_eq!(builder.push(&[3, 3]), Err(Error::InvalidPositions));
        assert_eq!(builder.push(&[4, 3]), Err(Error::InvalidPositions));
        builder.push(&[0]).unwrap();
        builder.push(&[1, u32::MAX]).unwrap();
        let bytes = builder.finish();
        let payload = Payload::parse(&bytes).unwrap();
        assert_eq!(payload.get(1).unwrap().positions, vec![1, u32::MAX]);
    }

    #[test]
    fn corruption_is_detected() {
        let entries = sample(100);
        let mut bytes = build(&entries);
        assert!(
            Payload::parse(&bytes[..bytes.len() - 1])
                .and_then(|p| p.get(99))
                .is_err()
        );
        // Tamper with the skip table so the slot for entry 32 points past the data.
        let payload = Payload::parse(&bytes).unwrap();
        assert_eq!(payload.skip_table_len(), 3 * 4);
        let second_skip_at = payload.skips_at + 1;
        bytes[second_skip_at] = 0x7f;
        let tampered = Payload::parse(&bytes).unwrap();
        let probe = SKIP_INTERVAL;
        assert!(
            tampered.get(probe).is_err() || tampered.get(probe).unwrap() != entries[probe as usize]
        );
    }
}
