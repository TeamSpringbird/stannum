//! Per-posting term-frequency bucket and token positions.
//!
//! Entries are in the same order as the term's postings and are addressed by
//! posting ordinal, so this stream is only read by queries that need
//! positions or scores.
//!
//! ```text
//! stream := count varint, skip u32le * slots, data
//! slots  := ceil(count / SKIP_INTERVAL) - 1, or 0 for an empty stream
//! entry  := tf_bucket u8 (low four bits), n varint, position varint * n
//!           positions: first absolute, then (delta - 1)
//! ```
//!
//! Skip slot `i` holds the byte offset (relative to `data`) of entry
//! `(i + 1) * SKIP_INTERVAL`; entry 0 is at offset 0 and has no slot. Offsets
//! are fixed-width so a seek jumps to its slot in constant time; a ranked
//! scan seeks once per scored document. A stream of at most `SKIP_INTERVAL`
//! entries has no table at all.
//!
//! Earlier segment formats are still read through [`Payload::parse_format`]:
//! `LSG2` streams count their slots explicitly and include the zero slot for
//! entry 0 (`count, skip_count varint, skip u32le * skip_count, data`); `LSG1`
//! streams hold one skip per 64 entries as varint deltas, so a seek walks
//! the table from its start.

use crate::reader::Reader;
use crate::segment::Format;
use crate::{Error, Result, varint};

pub const SKIP_INTERVAL: u32 = 32;
const LEGACY_SKIP_INTERVAL: u32 = 64;
pub const MAX_TF_BUCKET: u8 = crate::tf_bucket::BUCKET_MAX;

#[derive(Default, Debug)]
pub struct PayloadBuilder {
    count: u32,
    skips: Vec<usize>,
    data: Vec<u8>,
}

impl PayloadBuilder {
    /// `positions` must be non-empty and strictly increasing.
    pub fn push(&mut self, tf_bucket: u8, positions: &[u32]) -> Result<()> {
        if tf_bucket > MAX_TF_BUCKET {
            return Err(Error::InvalidTfBucket);
        }
        validate_positions(positions)?;
        if self.count.is_multiple_of(SKIP_INTERVAL) {
            self.skips.push(self.data.len());
        }
        self.count += 1;
        self.data.push(tf_bucket);
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
        self.finish_as(Format::CURRENT)
    }

    /// Encodes in the layout of an earlier format, for compatibility tests.
    pub(crate) fn finish_as(self, format: Format) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() + self.skips.len() * 4 + 8);
        varint::put(&mut out, u64::from(self.count));
        match format {
            Format::Lsg1 => {
                // One varint delta per 64 entries: every other fixed-width slot.
                let skips: Vec<usize> = self.skips.iter().copied().step_by(2).collect();
                varint::put(&mut out, skips.len() as u64);
                let mut previous = 0;
                for skip in skips {
                    varint::put(&mut out, (skip - previous) as u64);
                    previous = skip;
                }
            }
            Format::Lsg2 => {
                varint::put(&mut out, self.skips.len() as u64);
                for skip in &self.skips {
                    out.extend_from_slice(&fixed_skip(*skip).to_le_bytes());
                }
            }
            Format::Lsg3 => {
                for skip in self.skips.iter().skip(1) {
                    out.extend_from_slice(&fixed_skip(*skip).to_le_bytes());
                }
            }
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
    pub tf_bucket: u8,
    pub positions: Vec<u32>,
}

#[derive(Clone, Copy, Debug)]
pub struct Payload<'a> {
    bytes: &'a [u8],
    count: u32,
    skips_at: usize,
    data_at: usize,
    /// Entries per skip; 64 with varint deltas in the `LSG1` layout.
    interval: u32,
    format: Format,
}

impl<'a> Payload<'a> {
    /// Parses the current layout.
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        Self::parse_format(bytes, Format::CURRENT)
    }

    /// Parses the `LSG1` layout: varint delta skips every 64 entries.
    pub fn parse_legacy(bytes: &'a [u8]) -> Result<Self> {
        Self::parse_format(bytes, Format::Lsg1)
    }

    /// Parses the layout written by segments of `format`.
    pub fn parse_format(bytes: &'a [u8], format: Format) -> Result<Self> {
        let interval = match format {
            Format::Lsg1 => LEGACY_SKIP_INTERVAL,
            Format::Lsg2 | Format::Lsg3 => SKIP_INTERVAL,
        };
        let mut reader = Reader::new(bytes);
        let count = reader.varint_u32()?;
        let slots = (count as usize).div_ceil(interval as usize);
        let slots = match format {
            Format::Lsg1 | Format::Lsg2 => {
                let skip_count = reader.varint_u32()? as usize;
                if skip_count != slots {
                    return Err(Error::Corrupt("payload skip table size"));
                }
                skip_count
            }
            Format::Lsg3 => slots.saturating_sub(1),
        };
        let skips_at = reader.position();
        match format {
            Format::Lsg1 => {
                for _ in 0..slots {
                    reader.varint()?;
                }
            }
            Format::Lsg2 | Format::Lsg3 => reader.skip(slots * 4)?,
        }
        Ok(Self {
            bytes,
            count,
            skips_at,
            data_at: reader.position(),
            interval,
            format,
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
        self.bytes.len() - self.data_at
    }

    /// Byte position of the skip-table entry containing `ordinal`, and the
    /// ordinal that entry starts at.
    fn skip_to(&self, ordinal: u32) -> Result<(usize, u32)> {
        if ordinal >= self.count {
            return Err(Error::Corrupt("payload ordinal out of range"));
        }
        let slot = (ordinal / self.interval) as usize;
        let fixed = |slot: usize| {
            let at = self.skips_at + slot * 4;
            let bytes = &self.bytes[at..at + 4];
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize
        };
        let offset = match self.format {
            Format::Lsg1 => {
                let mut reader = Reader::at(self.bytes, self.skips_at);
                let mut offset = 0usize;
                for _ in 0..=slot {
                    offset = offset
                        .checked_add(reader.varint()? as usize)
                        .ok_or(Error::Corrupt("payload skip overflow"))?;
                }
                offset
            }
            Format::Lsg2 => fixed(slot),
            Format::Lsg3 => match slot.checked_sub(1) {
                Some(slot) => fixed(slot),
                None => 0,
            },
        };
        let at = self
            .data_at
            .checked_add(offset)
            .filter(|at| *at <= self.bytes.len())
            .ok_or(Error::Truncated)?;
        Ok((at, slot as u32 * self.interval))
    }

    pub fn cursor(&self) -> PayloadCursor<'a> {
        PayloadCursor {
            payload: *self,
            reader: Reader::at(self.bytes, self.data_at),
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
    reader: Reader<'a>,
    next_ordinal: u32,
}

impl PayloadCursor<'_> {
    /// Ordinal the next `next()` call will decode.
    pub const fn next_ordinal(&self) -> u32 {
        self.next_ordinal
    }

    /// Positions so the next decode returns entry `ordinal`.
    pub fn seek(&mut self, ordinal: u32) -> Result<()> {
        if ordinal >= self.payload.count {
            return Err(Error::Corrupt("payload ordinal out of range"));
        }
        let interval = self.payload.interval;
        let forward_only = ordinal >= self.next_ordinal
            && ordinal - self.next_ordinal < interval
            && ordinal / interval == self.next_ordinal / interval;
        if !forward_only {
            let (at, start) = self.payload.skip_to(ordinal)?;
            self.reader.seek(at)?;
            self.next_ordinal = start;
        }
        while self.next_ordinal < ordinal {
            self.next_bucket()?;
        }
        Ok(())
    }

    /// Decodes the next entry's term-frequency bucket, skipping its positions.
    pub fn next_bucket(&mut self) -> Result<u8> {
        if self.next_ordinal >= self.payload.count {
            return Err(Error::Corrupt("payload read past end"));
        }
        let byte = self.reader.u8()?;
        if byte > MAX_TF_BUCKET {
            return Err(Error::Corrupt("payload bucket byte"));
        }
        skip_positions(&mut self.reader)?;
        self.next_ordinal += 1;
        Ok(byte)
    }

    /// Decodes the next entry, appending its positions to `positions` and
    /// returning its term-frequency bucket.
    pub fn next_into(&mut self, positions: &mut Vec<u32>) -> Result<u8> {
        if self.next_ordinal >= self.payload.count {
            return Err(Error::Corrupt("payload read past end"));
        }
        let byte = self.reader.u8()?;
        if byte > MAX_TF_BUCKET {
            return Err(Error::Corrupt("payload bucket byte"));
        }
        decode_positions(&mut self.reader, positions)?;
        self.next_ordinal += 1;
        Ok(byte)
    }

    /// Validate every position and return its count without materializing it.
    /// Unlike `next_bucket`, this checks cumulative position overflow as well.
    pub(crate) fn next_count(&mut self) -> Result<(u8, usize)> {
        if self.next_ordinal >= self.payload.count {
            return Err(Error::Corrupt("payload read past end"));
        }
        let byte = self.reader.u8()?;
        if byte > MAX_TF_BUCKET {
            return Err(Error::Corrupt("payload bucket byte"));
        }
        let count = visit_positions(&mut self.reader, |_| {})?;
        self.next_ordinal += 1;
        Ok((byte, count))
    }

    pub fn next_entry(&mut self) -> Result<Entry> {
        let mut positions = Vec::new();
        let tf_bucket = self.next_into(&mut positions)?;
        Ok(Entry {
            tf_bucket,
            positions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n: u32) -> Vec<Entry> {
        (0..n)
            .map(|i| Entry {
                tf_bucket: (i % 16) as u8,
                positions: (0..=(i % 5)).map(|k| i * 7 + k * (k + 1) + 1).collect(),
            })
            .collect()
    }

    fn build(entries: &[Entry]) -> Vec<u8> {
        let mut builder = PayloadBuilder::default();
        for entry in entries {
            builder.push(entry.tf_bucket, &entry.positions).unwrap();
        }
        builder.finish()
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

    /// Encodes entries in the `LSG1` layout, as that format's builder did.
    fn build_legacy(entries: &[Entry]) -> Vec<u8> {
        let mut data = Vec::new();
        let mut skips = Vec::new();
        for (ordinal, entry) in entries.iter().enumerate() {
            if (ordinal as u32).is_multiple_of(LEGACY_SKIP_INTERVAL) {
                skips.push(data.len());
            }
            data.push(entry.tf_bucket);
            encode_positions(&mut data, &entry.positions);
        }
        let mut out = Vec::new();
        varint::put(&mut out, entries.len() as u64);
        varint::put(&mut out, skips.len() as u64);
        let mut previous = 0;
        for skip in skips {
            varint::put(&mut out, (skip - previous) as u64);
            previous = skip;
        }
        out.extend_from_slice(&data);
        out
    }

    /// Encodes entries in the `LSG2` layout, as that format's builder did.
    fn build_v2(entries: &[Entry]) -> Vec<u8> {
        let mut data = Vec::new();
        let mut skips = Vec::new();
        for (ordinal, entry) in entries.iter().enumerate() {
            if (ordinal as u32).is_multiple_of(SKIP_INTERVAL) {
                skips.push(data.len() as u32);
            }
            data.push(entry.tf_bucket);
            encode_positions(&mut data, &entry.positions);
        }
        let mut out = Vec::new();
        varint::put(&mut out, entries.len() as u64);
        varint::put(&mut out, skips.len() as u64);
        for skip in skips {
            out.extend_from_slice(&skip.to_le_bytes());
        }
        out.extend_from_slice(&data);
        out
    }

    fn build_as(entries: &[Entry], format: Format) -> Vec<u8> {
        let mut builder = PayloadBuilder::default();
        for entry in entries {
            builder.push(entry.tf_bucket, &entry.positions).unwrap();
        }
        builder.finish_as(format)
    }

    #[test]
    fn counting_rejects_cumulative_overflow_even_when_each_varint_fits() {
        // LSG3: one entry, bucket zero, two positions. The second delta is
        // representable, but adding it and the implicit one overflows u32.
        let mut bytes = vec![1, 0, 2];
        varint::put(&mut bytes, u64::from(u32::MAX));
        varint::put(&mut bytes, 0);
        let payload = Payload::parse(&bytes).unwrap();
        let mut cursor = payload.cursor();
        assert_eq!(
            cursor.next_count(),
            Err(Error::Corrupt("position overflow"))
        );
        assert_eq!(cursor.next_ordinal(), 0);
        assert_eq!(payload.cursor().next_bucket(), Ok(0));
    }

    #[test]
    fn counted_positions_match_materialized_decode_and_failures() {
        // Exercise all layouts, skip boundaries, seeks, overflow and truncation.
        for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] {
            let mut entries = sample(70);
            entries[3].positions = vec![0, u32::MAX];
            let bytes = build_as(&entries, format);
            let compare = |bytes: &[u8]| {
                let Ok(payload) = Payload::parse_format(bytes, format) else {
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
                        let expected = decode
                            .next_into(&mut positions)
                            .map(|bucket| (bucket, positions.len()));
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
    fn earlier_layouts_are_still_read() {
        let entries = sample(3 * LEGACY_SKIP_INTERVAL + 5);
        for (format, bytes) in [
            (Format::Lsg1, build_legacy(&entries)),
            (Format::Lsg2, build_v2(&entries)),
        ] {
            // The compatibility writer reproduces the old layout exactly.
            assert_eq!(build_as(&entries, format), bytes, "{format}");
            let payload = Payload::parse_format(&bytes, format).unwrap();
            assert_eq!(payload.count(), entries.len() as u32);
            for (ordinal, entry) in entries.iter().enumerate() {
                assert_eq!(&payload.get(ordinal as u32).unwrap(), entry, "{ordinal}");
            }
            let mut cursor = payload.cursor();
            for ordinal in [190u32, 5, 6, 70, 69, 63, 64, 0, 196, 32, 31, 33] {
                cursor.seek(ordinal).unwrap();
                assert_eq!(&cursor.next_entry().unwrap(), &entries[ordinal as usize]);
            }
            assert!(Payload::parse_format(&build(&entries), format).is_err());
        }
        assert_eq!(build_as(&entries, Format::Lsg3), build(&entries));
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
        assert_eq!(builder.push(16, &[1]), Err(Error::InvalidTfBucket));
        assert_eq!(builder.push(1, &[]), Err(Error::InvalidPositions));
        assert_eq!(builder.push(1, &[3, 3]), Err(Error::InvalidPositions));
        assert_eq!(builder.push(1, &[4, 3]), Err(Error::InvalidPositions));
        builder.push(0, &[0]).unwrap();
        builder.push(15, &[1, u32::MAX]).unwrap();
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
