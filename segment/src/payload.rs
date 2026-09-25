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

/// Skips one encoded position list without materializing it.
pub(crate) fn skip_positions(reader: &mut Reader<'_>) -> Result<()> {
    let n = reader.varint_u32()?;
    if n == 0 {
        return Err(Error::InvalidPositions);
    }
    for _ in 0..n {
        reader.varint_u32()?;
    }
    Ok(())
}

/// Appends decoded positions to `into` and returns how many were read.
pub(crate) fn decode_positions(reader: &mut Reader<'_>, into: &mut Vec<u32>) -> Result<usize> {
    visit_positions(reader, |position| into.push(position))
}

fn visit_positions(reader: &mut Reader<'_>, mut visit: impl FnMut(u32)) -> Result<usize> {
    let n = reader.varint_u32()?;
    if n == 0 {
        return Err(Error::InvalidPositions);
    }
    let mut position = reader.varint_u32()?;
    visit(position);
    for _ in 1..n {
        let delta = reader.varint_u32()?;
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
            owned_at: 0,
            slots: (0, 0),
            span_at: 0,
            next_ordinal: 0,
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
    owned_at: usize,
    /// The loaded span of a ranged stream as (first skip slot, slots), and
    /// where it starts in the stream.
    slots: (usize, usize),
    span_at: usize,
    next_ordinal: u32,
}

impl PayloadCursor<'_> {
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
        let t0 = diag::ticks();
        let start = self.payload.slot_at(slot)?;
        let end = self.payload.slot_at(slot + count)?;
        if end < start {
            return Err(Error::Corrupt("payload skip order"));
        }
        self.owned = areas.payload_range_owned(base + start as u64, end - start)?;
        diag::add(2, 1);
        diag::add(3, (end - start) as u64);
        diag::add(9, diag::ticks() - t0);
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
            if at > self.owned.len() {
                return Err(Error::Truncated);
            }
            self.owned_at = at;
            Ok(())
        } else {
            self.reader.seek(at)
        }
    }

    /// Decodes at the position, over the whole stream or the loaded span.
    fn decode<R>(&mut self, f: impl FnOnce(&mut Reader<'_>) -> Result<R>) -> Result<R> {
        if self.ranged() {
            let mut reader = Reader::at(&self.owned, self.owned_at);
            let value = f(&mut reader)?;
            let at = reader.position();
            self.owned_at = at;
            Ok(value)
        } else {
            let mut reader = self.reader;
            let value = f(&mut reader)?;
            self.reader = reader;
            Ok(value)
        }
    }

    /// Positions so the next decode returns entry `ordinal`.
    pub fn seek(&mut self, ordinal: u32) -> Result<()> {
        if ordinal >= self.payload.count {
            return Err(Error::Corrupt("payload ordinal out of range"));
        }
        let t0 = diag::ticks();
        diag::add(0, 1);
        let interval = SKIP_INTERVAL;
        let forward_only = ordinal >= self.next_ordinal
            && ordinal - self.next_ordinal < interval
            && ordinal / interval == self.next_ordinal / interval;
        if !forward_only {
            diag::add(1, 1);
            let (at, start) = self.payload.skip_to(ordinal)?;
            self.next_ordinal = start;
            self.load()?;
            self.set_position(at - self.span_at)?;
        }
        while self.next_ordinal < ordinal {
            let n = self.decode(|r| {
                let mut probe = *r;
                probe.varint_u32()
            })?;
            diag::add(4, 1);
            diag::add(5, u64::from(n));
            self.skip_entry()?;
        }
        diag::add(8, diag::ticks() - t0);
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
        let t0 = diag::ticks();
        self.load()?;
        let n = self.decode(|reader| decode_positions(reader, positions))?;
        self.next_ordinal += 1;
        diag::add(6, 1);
        diag::add(7, n as u64);
        diag::add(10, diag::ticks() - t0);
        Ok(())
    }

    /// Validate every position and return its count without materializing it.
    /// Unlike `skip_entry`, this checks cumulative position overflow as well.
    pub(crate) fn next_count(&mut self) -> Result<usize> {
        if self.next_ordinal >= self.payload.count {
            return Err(Error::Corrupt("payload read past end"));
        }
        self.load()?;
        let count = self.decode(|reader| visit_positions(reader, |_| {}))?;
        self.next_ordinal += 1;
        Ok(count)
    }

    pub fn next_entry(&mut self) -> Result<Entry> {
        let mut positions = Vec::new();
        self.next_into(&mut positions)?;
        Ok(Entry { positions })
    }
}

/// Diagnostic counters (throwaway).
pub mod diag {
    use std::cell::Cell;
    pub const N: usize = 12;
    pub const NAMES: [&str; N] = [
        "seeks", "jumps", "loads", "load_bytes", "skipped_entries", "skipped_positions",
        "lists", "positions", "seek_ticks", "load_ticks", "decode_ticks", "check_ticks",
    ];
    thread_local! { pub static C: [Cell<u64>; N] = const { [const { Cell::new(0) }; N] }; }
    #[inline]
    pub fn add(i: usize, v: u64) {
        C.with(|c| c[i].set(c[i].get() + v));
    }
    pub fn reset() {
        C.with(|c| c.iter().for_each(|x| x.set(0)));
    }
    pub fn report() -> String {
        let freq = freq();
        C.with(|c| {
            NAMES
                .iter()
                .zip(c.iter())
                .map(|(n, v)| {
                    if n.ends_with("ticks") {
                        format!("{n}_us={:.0}", v.get() as f64 * 1e6 / freq as f64)
                    } else {
                        format!("{n}={}", v.get())
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
    }
    #[inline]
    pub fn ticks() -> u64 {
        #[cfg(target_arch = "aarch64")]
        {
            let t: u64;
            unsafe { std::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) t) };
            t
        }
        #[cfg(not(target_arch = "aarch64"))]
        0
    }
    fn freq() -> u64 {
        #[cfg(target_arch = "aarch64")]
        {
            let t: u64;
            unsafe { std::arch::asm!("mrs {}, cntfrq_el0", out(reg) t) };
            t
        }
        #[cfg(not(target_arch = "aarch64"))]
        1
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
