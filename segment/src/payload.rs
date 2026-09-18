//! Per-posting term-frequency bucket and token positions.
//!
//! Entries are in the same order as the term's postings and are addressed by
//! posting ordinal, so this stream is only read by queries that need
//! positions or scores.
//!
//! ```text
//! stream := count varint, skip_count varint, skip_delta varint * skip_count, data
//! entry  := tf_bucket u8 (low four bits), n varint, position varint * n
//!           positions: first absolute, then (delta - 1)
//! ```
//!
//! The skip table holds the byte offset (relative to `data`) of every
//! `SKIP_INTERVAL`-th entry, stored as deltas.

use crate::reader::Reader;
use crate::{Error, Result, varint};

pub const SKIP_INTERVAL: u32 = 64;
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
        let mut out = Vec::with_capacity(self.data.len() + self.skips.len() * 3 + 8);
        varint::put(&mut out, u64::from(self.count));
        varint::put(&mut out, self.skips.len() as u64);
        let mut previous = 0;
        for skip in &self.skips {
            varint::put(&mut out, (skip - previous) as u64);
            previous = *skip;
        }
        out.extend_from_slice(&self.data);
        out
    }
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

/// Appends decoded positions to `into` and returns how many were read.
pub(crate) fn decode_positions(reader: &mut Reader<'_>, into: &mut Vec<u32>) -> Result<usize> {
    let n = reader.varint_u32()?;
    if n == 0 {
        return Err(Error::InvalidPositions);
    }
    let mut position = reader.varint_u32()?;
    into.push(position);
    for _ in 1..n {
        let delta = reader.varint_u32()?;
        position = position
            .checked_add(delta)
            .and_then(|p| p.checked_add(1))
            .ok_or(Error::Corrupt("position overflow"))?;
        into.push(position);
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
    skip_count: usize,
    data_at: usize,
}

impl<'a> Payload<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        let count = reader.varint_u32()?;
        let skip_count = reader.varint_u32()? as usize;
        let expected = (count as usize).div_ceil(SKIP_INTERVAL as usize);
        if skip_count != expected {
            return Err(Error::Corrupt("payload skip table size"));
        }
        let skips_at = reader.position();
        for _ in 0..skip_count {
            reader.varint()?;
        }
        Ok(Self {
            bytes,
            count,
            skips_at,
            skip_count,
            data_at: reader.position(),
        })
    }

    pub const fn count(&self) -> u32 {
        self.count
    }

    /// Byte position of the skip-table entry containing `ordinal`, and the
    /// ordinal that entry starts at.
    fn skip_to(&self, ordinal: u32) -> Result<(usize, u32)> {
        let slot = (ordinal / SKIP_INTERVAL) as usize;
        if slot >= self.skip_count {
            return Err(Error::Corrupt("payload ordinal out of range"));
        }
        let mut reader = Reader::at(self.bytes, self.skips_at);
        let mut offset = 0usize;
        for _ in 0..=slot {
            offset = offset
                .checked_add(reader.varint()? as usize)
                .ok_or(Error::Corrupt("payload skip overflow"))?;
        }
        let at = self
            .data_at
            .checked_add(offset)
            .filter(|at| *at <= self.bytes.len())
            .ok_or(Error::Truncated)?;
        Ok((at, slot as u32 * SKIP_INTERVAL))
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
        let forward_only = ordinal >= self.next_ordinal
            && ordinal - self.next_ordinal < SKIP_INTERVAL
            && ordinal / SKIP_INTERVAL == self.next_ordinal / SKIP_INTERVAL;
        if !forward_only {
            let (at, start) = self.payload.skip_to(ordinal)?;
            self.reader.seek(at)?;
            self.next_ordinal = start;
        }
        let mut scratch = Vec::new();
        while self.next_ordinal < ordinal {
            self.next_into(&mut scratch)?;
            scratch.clear();
        }
        Ok(())
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
        let entries = sample(3 * SKIP_INTERVAL + 5);
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
        // Tamper with the skip table so the second slot points past the data.
        let payload = Payload::parse(&bytes).unwrap();
        let second_skip_at = payload.skips_at + 1;
        bytes[second_skip_at] = 0x7f;
        let tampered = Payload::parse(&bytes).unwrap();
        assert!(tampered.get(64).is_err() || tampered.get(64).unwrap() != entries[64]);
    }
}
