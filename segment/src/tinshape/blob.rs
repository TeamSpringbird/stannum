// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A segment's blob loaded as far as its queries have read it: the
//! extension's ctid-native paths keep one per segment in a backend and
//! load ranges of it from the segment's pages on demand. Unread ranges stay
//! zero pages the allocator has not committed.
//!
//! A range counts as loaded only once its bytes are in place: a read that
//! fails, or unwinds (a query cancel raised inside a page read), leaves its
//! chunks unloaded, so the next query reads them again rather than taking
//! zeros for the segment's bytes.

use crate::source::Source;
use crate::{Error, Result};

/// Bytes of a blob loaded at a time.
pub const CHUNK: usize = 8192;

/// Most bytes one read copies: a long run of missing chunks is read in
/// pieces of this size, so a large area costs no transient copy of itself.
const PIECE: usize = 128 * CHUNK;

/// A segment's blob as far as it has been read.
pub struct Blob {
    bytes: Box<[u8]>,
    /// Bit per [`CHUNK`]: loaded.
    chunks: Vec<u64>,
    loaded: usize,
}

impl Blob {
    /// An empty blob of `len` bytes.
    pub fn new(len: usize) -> Self {
        Self {
            bytes: vec![0u8; len].into_boxed_slice(),
            chunks: vec![0u64; len.div_ceil(CHUNK).div_ceil(64)],
            loaded: 0,
        }
    }

    /// The blob's bytes; only loaded ranges hold the segment's.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Bytes loaded so far.
    pub fn loaded(&self) -> usize {
        self.loaded
    }

    fn is_loaded(&self, chunk: usize) -> bool {
        self.chunks[chunk / 64] >> (chunk % 64) & 1 == 1
    }

    /// Whether `[offset, offset + len)` is loaded.
    pub fn has(&self, offset: u64, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let end = (offset as usize).saturating_add(len).min(self.bytes.len());
        (offset as usize / CHUNK..end.div_ceil(CHUNK)).all(|chunk| self.is_loaded(chunk))
    }

    /// Loads `[offset, offset + len)` from `source` where not loaded yet.
    pub fn ensure(&mut self, source: &dyn Source, offset: u64, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let end = (offset as usize)
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(Error::Truncated)?;
        let mut chunk = offset as usize / CHUNK;
        while chunk * CHUNK < end {
            if self.is_loaded(chunk) {
                chunk += 1;
                continue;
            }
            // A run of missing chunks, read a piece at a time.
            let first = chunk;
            while chunk * CHUNK < end
                && !self.is_loaded(chunk)
                && (chunk - first + 1) * CHUNK <= PIECE
            {
                chunk += 1;
            }
            let from = first * CHUNK;
            let to = (chunk * CHUNK).min(self.bytes.len());
            let read = source.read(from as u64, to - from)?;
            if read.len() != to - from {
                return Err(Error::Truncated);
            }
            self.bytes[from..to].copy_from_slice(&read);
            // Only now: a read that failed or unwound marked nothing.
            for c in first..chunk {
                self.chunks[c / 64] |= 1 << (c % 64);
            }
            self.loaded += to - from;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A source over `bytes` whose reads fail (or panic, as a PostgreSQL
    /// error raised inside a page read unwinds) while `fail` is set.
    struct Flaky {
        bytes: Vec<u8>,
        fail: Cell<Option<bool>>,
        reads: Cell<usize>,
    }

    impl Source for Flaky {
        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }

        fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
            self.reads.set(self.reads.get() + 1);
            match self.fail.get() {
                Some(true) => panic!("canceling statement due to user request"),
                Some(false) => Err(Error::Truncated),
                None => Ok(self.bytes[offset as usize..offset as usize + len].to_vec()),
            }
        }
    }

    fn flaky(len: usize) -> Flaky {
        Flaky {
            bytes: (0..len).map(|i| (i * 7 + 1) as u8).collect(),
            fail: Cell::new(None),
            reads: Cell::new(0),
        }
    }

    #[test]
    fn a_failed_read_leaves_its_range_unloaded() {
        let source = flaky(5 * CHUNK + 100);
        let mut blob = Blob::new(source.bytes.len());
        source.fail.set(Some(false));
        assert!(blob.ensure(&source, 100, 3 * CHUNK).is_err());
        assert!(!blob.has(100, 1));
        assert_eq!(blob.loaded(), 0);
        source.fail.set(None);
        blob.ensure(&source, 100, 3 * CHUNK).unwrap();
        assert_eq!(
            &blob.bytes()[100..100 + 3 * CHUNK],
            &source.bytes[100..100 + 3 * CHUNK]
        );
    }

    #[test]
    fn a_read_that_unwinds_leaves_its_range_unloaded() {
        let source = flaky(5 * CHUNK + 100);
        let mut blob = Blob::new(source.bytes.len());
        source.fail.set(Some(true));
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            blob.ensure(&source, CHUNK as u64 + 5, 4 * CHUNK)
        }));
        assert!(unwound.is_err());
        assert!(!blob.has(CHUNK as u64 + 5, 1));
        source.fail.set(None);
        blob.ensure(&source, 0, source.bytes.len()).unwrap();
        assert_eq!(blob.bytes(), source.bytes.as_slice());
        assert_eq!(blob.loaded(), source.bytes.len());
    }

    #[test]
    fn a_long_run_is_read_in_pieces() {
        let source = flaky(3 * PIECE + 10);
        let mut blob = Blob::new(source.bytes.len());
        blob.ensure(&source, 0, source.bytes.len()).unwrap();
        assert_eq!(blob.bytes(), source.bytes.as_slice());
        assert_eq!(source.reads.get(), 4);
        // Loaded ranges are not read again.
        blob.ensure(&source, 5, PIECE).unwrap();
        assert_eq!(source.reads.get(), 4);
    }
}
