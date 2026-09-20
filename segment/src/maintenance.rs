// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Bounded, forward-only maintenance decoding. Query readers keep their existing
//! borrowed-slice guarantees. This module is not yet wired into the merger.
//!
//! The payload cursor validates every position, bucket and skip offset, including
//! entries a caller intends to delete. It does not validate posting ownership,
//! dictionary extents, document lengths or score bounds; callers must retain the
//! whole-input verifier until those checks have streaming equivalents. Next,
//! pair this cursor with bounded postings/dictionary cursors, then feed positions
//! to an incremental output codec. Source caches and callback allocations are
//! outside the cursor's bound; this is not a total merge memory budget. Legacy
//! LSG1 construction scans its variable-width skip table once to locate data;
//! that scan is bounded in memory but currently has no cancellation callback.

use crate::{Error, Result, segment::Format, source::Source, tf_bucket::TfBucket};

struct Window<'a, S: Source + ?Sized> {
    source: &'a S,
    at: u64,
    end: u64,
    size: usize,
    bytes: Box<[u8]>,
    next: usize,
}

impl<'a, S: Source + ?Sized> Window<'a, S> {
    fn new(source: &'a S, start: u64, end: u64, size: usize) -> Result<Self> {
        if size == 0 {
            return Err(Error::Corrupt("zero maintenance window"));
        }
        if start > end || end > source.len() {
            return Err(Error::Truncated);
        }
        Ok(Self {
            source,
            at: start,
            end,
            size,
            bytes: Box::default(),
            next: 0,
        })
    }

    fn byte(&mut self) -> Result<u8> {
        if self.at == self.end {
            return Err(Error::Truncated);
        }
        if self.next == self.bytes.len() {
            // Release the old window before asking the source for another one.
            self.bytes = Box::default();
            let len = (self.end - self.at).min(self.size as u64) as usize;
            let bytes = self.source.read(self.at, len)?;
            if bytes.len() != len {
                return Err(Error::Truncated);
            }
            self.bytes = bytes.into_boxed_slice();
            self.next = 0;
        }
        let byte = self.bytes[self.next];
        self.next += 1;
        self.at += 1;
        Ok(byte)
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0;
        for shift in (0..=63).step_by(7) {
            let byte = self.byte()?;
            if shift == 63 && byte > 1 {
                return Err(Error::Corrupt("varint exceeds 64 bits"));
            }
            value |= u64::from(byte & 127) << shift;
            if byte & 128 == 0 {
                return Ok(value);
            }
        }
        Err(Error::Corrupt("varint exceeds 64 bits"))
    }

    fn u32(&mut self) -> Result<u32> {
        u32::try_from(self.varint()?).map_err(|_| Error::Corrupt("value exceeds 32 bits"))
    }

    fn fixed(&mut self) -> Result<u64> {
        Ok(u32::from_le_bytes([self.byte()?, self.byte()?, self.byte()?, self.byte()?]) as u64)
    }
}

/// Summary returned only after an entry has been completely validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PayloadEntry {
    pub tf_bucket: u8,
    pub positions: u32,
}

/// A payload extent read with at most two `window_bytes` buffers, regardless of
/// entry count, skip-table size or positions per document. No positions Vec is
/// allocated. The source may itself retain bytes; use a bounded source for a
/// bounded executor. A failure poisons the cursor; callbacks may have observed
/// partial input, so callers must discard partial output on error.
pub struct PayloadCursor<'a, S: Source + ?Sized> {
    skips: Window<'a, S>,
    data: Window<'a, S>,
    data_start: u64,
    count: u32,
    ordinal: u32,
    interval: u32,
    format: Format,
    skip_offset: u64,
    failed: bool,
}

impl<'a, S: Source + ?Sized> PayloadCursor<'a, S> {
    pub fn new(
        source: &'a S,
        start: u64,
        len: u64,
        format: Format,
        window_bytes: usize,
    ) -> Result<Self> {
        let end = start.checked_add(len).ok_or(Error::Truncated)?;
        let mut header = Window::new(source, start, end, window_bytes)?;
        let count = header.u32()?;
        let interval = if format == Format::Lsg1 { 64 } else { 32 };
        let slots = count.div_ceil(interval);
        let slots = if format == Format::Lsg3 {
            slots.saturating_sub(1)
        } else {
            if header.u32()? != slots {
                return Err(Error::Corrupt("payload skip table size"));
            }
            slots
        };
        let skips_start = header.at;
        if format == Format::Lsg1 {
            for _ in 0..slots {
                header.varint()?;
            }
        } else {
            header.at = header
                .at
                .checked_add(u64::from(slots) * 4)
                .filter(|at| *at <= end)
                .ok_or(Error::Truncated)?;
        }
        let data_start = header.at;
        drop(header);
        Ok(Self {
            skips: Window::new(source, skips_start, data_start, window_bytes)?,
            data: Window::new(source, data_start, end, window_bytes)?,
            data_start,
            count,
            ordinal: 0,
            interval,
            format,
            skip_offset: 0,
            failed: false,
        })
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    /// Owned encoded bytes retained now; excludes the source and callback.
    pub fn retained_bytes(&self) -> usize {
        self.skips.bytes.len() + self.data.bytes.len()
    }

    /// Visits positions without retaining them, and validates even when `visit`
    /// ignores them (for example, a dead posting). Returning `None` certifies the
    /// entire extent, including exact exhaustion. Call until `None`, not merely
    /// `count` times. The callback can return an error to abort long entries.
    pub fn next_with(
        &mut self,
        visit: impl FnMut(u32) -> Result<()>,
    ) -> Result<Option<PayloadEntry>> {
        if self.failed {
            return Err(Error::Corrupt("maintenance cursor failed"));
        }
        let result = self.next_inner(visit);
        self.failed = result.is_err();
        result
    }

    fn next_inner(
        &mut self,
        mut visit: impl FnMut(u32) -> Result<()>,
    ) -> Result<Option<PayloadEntry>> {
        if self.ordinal == self.count {
            if self.data.at != self.data.end || self.skips.at != self.skips.end {
                return Err(Error::Corrupt("payload trailing bytes"));
            }
            self.data.bytes = Box::default();
            self.skips.bytes = Box::default();
            return Ok(None);
        }
        if self.ordinal.is_multiple_of(self.interval)
            && (self.ordinal != 0 || self.format != Format::Lsg3)
        {
            self.skip_offset = if self.format == Format::Lsg1 {
                self.skip_offset
                    .checked_add(self.skips.varint()?)
                    .ok_or(Error::Corrupt("payload skip overflow"))?
            } else {
                self.skips.fixed()?
            };
            if self.skip_offset != self.data.at - self.data_start {
                return Err(Error::Corrupt("payload skip offset"));
            }
        }
        let tf_bucket = self.data.byte()?;
        if TfBucket::new(tf_bucket).is_none() {
            return Err(Error::InvalidTfBucket);
        }
        let positions = self.data.u32()?;
        if positions == 0 {
            return Err(Error::InvalidPositions);
        }
        if TfBucket::from_count(positions).value() != tf_bucket {
            return Err(Error::Corrupt("payload frequency bucket"));
        }
        let mut position = self.data.u32()?;
        visit(position)?;
        for _ in 1..positions {
            position = position
                .checked_add(self.data.u32()?)
                .and_then(|p| p.checked_add(1))
                .ok_or(Error::Corrupt("position overflow"))?;
            visit(position)?;
        }
        self.ordinal += 1;
        Ok(Some(PayloadEntry {
            tf_bucket,
            positions,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::{Payload, PayloadBuilder};
    use std::cell::Cell;

    struct Tracked {
        bytes: Vec<u8>,
        max_read: Cell<usize>,
        total_read: Cell<usize>,
    }
    impl Source for Tracked {
        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }
        fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
            self.max_read.set(self.max_read.get().max(len));
            self.total_read.set(self.total_read.get() + len);
            self.bytes
                .get(offset as usize..offset as usize + len)
                .map(|s| s.to_vec())
                .ok_or(Error::Truncated)
        }
    }
    fn tracked(bytes: Vec<u8>) -> Tracked {
        Tracked {
            bytes,
            max_read: Cell::new(0),
            total_read: Cell::new(0),
        }
    }
    fn build(format: Format, count: u32, frequency: u32) -> Vec<u8> {
        let mut builder = PayloadBuilder::default();
        let positions: Vec<_> = (0..frequency).map(|i| i * 129).collect();
        for _ in 0..count {
            builder
                .push(TfBucket::from_count(frequency).value(), &positions)
                .unwrap();
        }
        builder.finish_as(format)
    }
    fn validate(bytes: &[u8], format: Format, window: usize) -> Result<()> {
        let mut cursor = PayloadCursor::new(bytes, 0, bytes.len() as u64, format, window)?;
        while cursor.next_with(|_| Ok(()))?.is_some() {}
        Ok(())
    }

    #[test]
    fn matches_query_reader_across_formats_and_refill_boundaries() {
        for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] {
            for count in [0, 1, 31, 32, 33, 64, 65, 257] {
                let bytes = build(format, count, 7);
                let reference = Payload::parse_format(&bytes, format).unwrap();
                for window in [1, 2, 3, 7, 32, 4096] {
                    let source = tracked(bytes.clone());
                    let mut cursor =
                        PayloadCursor::new(&source, 0, bytes.len() as u64, format, window).unwrap();
                    assert_eq!(cursor.count(), count);
                    for ordinal in 0..count {
                        let mut positions = Vec::new();
                        let entry = cursor
                            .next_with(|p| {
                                positions.push(p);
                                Ok(())
                            })
                            .unwrap()
                            .unwrap();
                        let expected = reference.get(ordinal).unwrap();
                        assert_eq!(positions, expected.positions);
                        assert_eq!(entry.tf_bucket, expected.tf_bucket);
                        assert_eq!(entry.positions as usize, positions.len());
                        assert!(cursor.retained_bytes() <= 2 * window);
                    }
                    assert_eq!(cursor.next_with(|_| Ok(())).unwrap(), None);
                    assert_eq!(cursor.retained_bytes(), 0);
                    assert!(source.max_read.get() <= window);
                }
            }
        }
    }

    #[test]
    fn a_large_common_term_and_single_large_entry_have_constant_retention() {
        for (count, frequency) in [(100_000, 3), (1, 500_000)] {
            let source = tracked(build(Format::Lsg3, count, frequency));
            let mut cursor =
                PayloadCursor::new(&source, 0, source.len(), Format::Lsg3, 127).unwrap();
            let mut entries = 0;
            let mut positions = 0u64;
            while cursor
                .next_with(|_| {
                    positions += 1;
                    Ok(())
                })
                .unwrap()
                .is_some()
            {
                entries += 1;
                assert!(cursor.retained_bytes() <= 254);
            }
            assert_eq!(entries, count);
            assert_eq!(positions, u64::from(count) * u64::from(frequency));
            assert!(source.max_read.get() <= 127);
            assert!(source.total_read.get() >= source.bytes.len());
            assert_eq!(cursor.retained_bytes(), 0);
        }
    }

    #[test]
    fn validates_ignored_dead_entries_and_poisoned_cursors_cannot_resume() {
        // Two positions: u32::MAX followed by delta zero overflows.
        let mut bytes = vec![1, 1, 2];
        crate::varint::put(&mut bytes, u64::from(u32::MAX));
        bytes.push(0);
        let mut cursor =
            PayloadCursor::new(&bytes, 0, bytes.len() as u64, Format::Lsg3, 1).unwrap();
        assert_eq!(
            cursor.next_with(|_| Ok(())),
            Err(Error::Corrupt("position overflow"))
        );
        assert_eq!(
            cursor.next_with(|_| Ok(())),
            Err(Error::Corrupt("maintenance cursor failed"))
        );
        for bytes in [vec![1, 16, 1, 0], vec![1, 0, 0], vec![1, 0, 2, 0, 0]] {
            assert!(validate(&bytes, Format::Lsg3, 1).is_err());
        }
    }

    #[test]
    fn rejects_corrupt_skip_offsets_sizes_truncation_and_trailing_bytes() {
        for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] {
            let bytes = build(format, 65, 3);
            for cut in 0..bytes.len() {
                assert!(validate(&bytes[..cut], format, 3).is_err());
            }
            let mut extra = bytes.clone();
            extra.push(0);
            assert!(validate(&extra, format, 1).is_err());
            let mut wrong_skip = bytes.clone();
            let skips_at = if format == Format::Lsg3 { 1 } else { 2 };
            wrong_skip[skips_at] ^= 1;
            assert!(validate(&wrong_skip, format, 1).is_err());
            if format != Format::Lsg3 {
                let mut wrong_size = bytes.clone();
                wrong_size[1] += 1;
                assert!(validate(&wrong_size, format, 1).is_err());
            }
        }
        assert!(validate(&[255; 11], Format::Lsg3, 1).is_err());
        assert!(validate(&[128, 128, 128, 128, 16], Format::Lsg3, 1).is_err());
    }

    proptest::proptest! {
        #[test]
        fn arbitrary_extents_never_panic(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512), window in 1usize..64) {
            for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] {
                let _ = validate(&bytes, format, window);
            }
        }
    }

    #[test]
    fn extent_bounds_source_failures_and_callback_abort_are_errors() {
        let bytes = build(Format::Lsg3, 1, 3);
        assert!(PayloadCursor::new(&bytes, u64::MAX, 2, Format::Lsg3, 1).is_err());
        assert!(PayloadCursor::new(&bytes, 0, bytes.len() as u64 + 1, Format::Lsg3, 1).is_err());
        assert!(PayloadCursor::new(&bytes, 0, bytes.len() as u64, Format::Lsg3, 0).is_err());
        let mut wrapped = vec![255; 9];
        wrapped.extend_from_slice(&bytes);
        wrapped.extend_from_slice(&[255; 9]);
        let mut cursor =
            PayloadCursor::new(&wrapped, 9, bytes.len() as u64, Format::Lsg3, 2).unwrap();
        assert!(
            cursor
                .next_with(|_| Err(Error::Corrupt("cancelled")))
                .is_err()
        );
        assert!(cursor.next_with(|_| Ok(())).is_err());
        let mut cursor =
            PayloadCursor::new(&wrapped, 9, bytes.len() as u64, Format::Lsg3, 2).unwrap();
        assert_eq!(cursor.next_with(|_| Ok(())).unwrap().unwrap().positions, 3);
        assert_eq!(cursor.next_with(|_| Ok(())).unwrap(), None);
        struct Short;
        impl Source for Short {
            fn len(&self) -> u64 {
                100
            }
            fn read(&self, _: u64, _: usize) -> Result<Vec<u8>> {
                Ok(vec![])
            }
        }
        assert!(PayloadCursor::new(&Short, 0, 10, Format::Lsg3, 2).is_err());
    }
}
