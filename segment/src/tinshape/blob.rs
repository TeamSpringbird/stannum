// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A segment's blob loaded on demand, a [`CHUNK`] at a time, and
//! [`Bytes`], a range of a blob either in memory or loaded on demand.
//!
//! The extension's ctid-native paths keep a [`LazyBlob`] per segment in a
//! backend. A query reads through [`Bytes::get`], which loads the chunks a
//! range covers from the segment's pages the first time any query asks for
//! them, so what a backend holds grows with the pages its queries read
//! (one entry of a common word's positions, one block of the DL sidecar),
//! not with the areas they fall in. Unread ranges stay zero pages the
//! allocator has not committed.
//!
//! A range counts as loaded only once its bytes are in place: a read that
//! fails, or unwinds (a query cancel raised inside a page read), leaves its
//! chunks unloaded, so the next query reads them again rather than taking
//! zeros for the segment's bytes.
//!
//! Soundness: the blob's bytes are a raw allocation. A slice is handed out
//! only over loaded chunks, and a load writes only chunks not loaded yet,
//! so no byte a live slice covers is ever written. Loaded chunks stay
//! loaded until the blob is dropped, which the borrow of every slice
//! outlives.

use std::cell::Cell;
use std::ptr::NonNull;

use crate::source::Source;
use crate::{Error, Result};

/// Bytes of a blob loaded at a time.
pub const CHUNK: usize = 8192;

/// Most bytes one read copies: a long run of missing chunks is read in
/// pieces of this size, so a large area costs no transient copy of itself.
const PIECE: usize = 128 * CHUNK;

/// A segment's blob as far as it has been read, loading on demand from its
/// source.
pub struct LazyBlob {
    data: NonNull<u8>,
    len: usize,
    /// Bit per [`CHUNK`]: loaded.
    chunks: Box<[Cell<u64>]>,
    loaded: Cell<usize>,
    source: Box<dyn Source>,
}

impl Drop for LazyBlob {
    fn drop(&mut self) {
        // SAFETY: `data` and `len` came from `Box::into_raw` in `new`.
        drop(unsafe {
            Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                self.data.as_ptr(),
                self.len,
            ))
        });
    }
}

impl std::fmt::Debug for LazyBlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyBlob")
            .field("len", &self.len)
            .field("loaded", &self.loaded.get())
            .finish()
    }
}

impl LazyBlob {
    /// A blob of `source`'s bytes, none loaded yet.
    pub fn new(source: Box<dyn Source>) -> Self {
        let len = usize::try_from(source.len()).expect("a segment fits in memory");
        let bytes = vec![0u8; len].into_boxed_slice();
        let data = NonNull::new(Box::into_raw(bytes).cast::<u8>()).expect("a boxed slice");
        Self {
            data,
            len,
            chunks: (0..len.div_ceil(CHUNK).div_ceil(64))
                .map(|_| Cell::new(0))
                .collect(),
            loaded: Cell::new(0),
            source,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes loaded so far.
    pub fn loaded(&self) -> usize {
        self.loaded.get()
    }

    /// Bytes the blob holds besides its loaded chunks: the chunk bitmap.
    pub fn overhead(&self) -> usize {
        self.chunks.len() * 8
    }

    /// The whole blob as [`Bytes`].
    pub fn bytes(&self) -> Bytes<'_> {
        Bytes::Lazy {
            blob: self,
            at: 0,
            len: self.len,
        }
    }

    fn is_loaded(&self, chunk: usize) -> bool {
        self.chunks[chunk / 64].get() >> (chunk % 64) & 1 == 1
    }

    /// Whether `[offset, offset + len)` is loaded.
    pub fn has(&self, offset: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let end = offset.saturating_add(len).min(self.len);
        (offset / CHUNK..end.div_ceil(CHUNK)).all(|chunk| self.is_loaded(chunk))
    }

    /// Loads `[from, to)` where not loaded yet.
    pub fn ensure(&self, from: usize, to: usize) -> Result<()> {
        if from >= to {
            return Ok(());
        }
        if to > self.len {
            return Err(Error::Truncated);
        }
        let mut chunk = from / CHUNK;
        while chunk * CHUNK < to {
            if self.is_loaded(chunk) {
                chunk += 1;
                continue;
            }
            // A run of missing chunks, read a piece at a time.
            let first = chunk;
            while chunk * CHUNK < to
                && !self.is_loaded(chunk)
                && (chunk - first + 1) * CHUNK <= PIECE
            {
                chunk += 1;
            }
            let start = first * CHUNK;
            let end = (chunk * CHUNK).min(self.len);
            let read = self.source.read(start as u64, end - start)?;
            if read.len() != end - start {
                return Err(Error::Truncated);
            }
            // SAFETY: `start..end` lies within the allocation and covers only
            // chunks not loaded, which no slice handed out covers.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    read.as_ptr(),
                    self.data.as_ptr().add(start),
                    end - start,
                );
            }
            // Only now: a read that failed or unwound marked nothing.
            for c in first..chunk {
                let word = &self.chunks[c / 64];
                word.set(word.get() | 1 << (c % 64));
            }
            self.loaded.set(self.loaded.get() + (end - start));
        }
        Ok(())
    }

    /// `[from, to)`, loaded first where it is not.
    pub fn get(&self, from: usize, to: usize) -> Result<&[u8]> {
        if from > to {
            return Err(Error::Truncated);
        }
        self.ensure(from, to)?;
        // SAFETY: every chunk of the range is loaded (above), and loaded
        // chunks are never written again while `self` lives.
        Ok(unsafe { std::slice::from_raw_parts(self.data.as_ptr().add(from), to - from) })
    }
}

/// A range of a segment's bytes: in memory, or in a [`LazyBlob`] that
/// loads what is read.
#[derive(Clone, Copy)]
pub enum Bytes<'a> {
    Slice(&'a [u8]),
    Lazy {
        blob: &'a LazyBlob,
        at: usize,
        len: usize,
    },
}

impl std::fmt::Debug for Bytes<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Slice(bytes) => write!(f, "Bytes::Slice({} bytes)", <[u8]>::len(bytes)),
            Self::Lazy { at, len, .. } => write!(f, "Bytes::Lazy({len} bytes at {at})"),
        }
    }
}

impl Default for Bytes<'_> {
    fn default() -> Self {
        Self::Slice(&[])
    }
}

impl<'a> From<&'a [u8]> for Bytes<'a> {
    fn from(bytes: &'a [u8]) -> Self {
        Self::Slice(bytes)
    }
}

impl<'a> From<&'a Vec<u8>> for Bytes<'a> {
    fn from(bytes: &'a Vec<u8>) -> Self {
        Self::Slice(bytes)
    }
}

impl<'a> Bytes<'a> {
    pub fn len(&self) -> usize {
        match self {
            Self::Slice(bytes) => <[u8]>::len(bytes),
            Self::Lazy { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes `[from, to)`; `Truncated` past the end.
    #[inline]
    pub fn get(&self, from: usize, to: usize) -> Result<&'a [u8]> {
        match *self {
            Self::Slice(bytes) => bytes.get(from..to).ok_or(Error::Truncated),
            Self::Lazy { blob, at, len } => {
                if from > to || to > len {
                    return Err(Error::Truncated);
                }
                blob.get(at + from, at + to)
            }
        }
    }

    /// Up to `max` bytes from `from`, fewer at the end.
    #[inline]
    pub fn window(&self, from: usize, max: usize) -> Result<&'a [u8]> {
        let len = self.len();
        if from > len {
            return Err(Error::Truncated);
        }
        self.get(from, from + max.min(len - from))
    }

    /// Every byte.
    pub fn all(&self) -> Result<&'a [u8]> {
        self.get(0, self.len())
    }

    /// Bytes `[from, to)` as a range of their own, nothing loaded.
    pub fn sub(&self, from: usize, to: usize) -> Result<Bytes<'a>> {
        if from > to || to > self.len() {
            return Err(Error::Truncated);
        }
        Ok(match *self {
            Self::Slice(bytes) => Self::Slice(&bytes[from..to]),
            Self::Lazy { blob, at, .. } => Self::Lazy {
                blob,
                at: at + from,
                len: to - from,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    /// A source over `bytes` whose reads fail (or panic, as a PostgreSQL
    /// error raised inside a page read unwinds) while `fail` is set.
    #[derive(Clone)]
    struct Flaky(Rc<FlakyState>);

    struct FlakyState {
        bytes: Vec<u8>,
        fail: Cell<Option<bool>>,
        reads: Cell<usize>,
    }

    impl Source for Flaky {
        fn len(&self) -> u64 {
            self.0.bytes.len() as u64
        }

        fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
            let state = &self.0;
            state.reads.set(state.reads.get() + 1);
            match state.fail.get() {
                Some(true) => panic!("canceling statement due to user request"),
                Some(false) => Err(Error::Truncated),
                None => Ok(state.bytes[offset as usize..offset as usize + len].to_vec()),
            }
        }
    }

    fn flaky(len: usize) -> (Flaky, LazyBlob) {
        let source = Flaky(Rc::new(FlakyState {
            bytes: (0..len).map(|i| (i * 7 + 1) as u8).collect(),
            fail: Cell::new(None),
            reads: Cell::new(0),
        }));
        let blob = LazyBlob::new(Box::new(source.clone()));
        (source, blob)
    }

    #[test]
    fn a_failed_read_leaves_its_range_unloaded() {
        let (source, blob) = flaky(5 * CHUNK + 100);
        source.0.fail.set(Some(false));
        assert!(blob.get(100, 100 + 3 * CHUNK).is_err());
        assert!(!blob.has(100, 1));
        assert_eq!(blob.loaded(), 0);
        source.0.fail.set(None);
        assert_eq!(
            blob.get(100, 100 + 3 * CHUNK).unwrap(),
            &source.0.bytes[100..100 + 3 * CHUNK]
        );
    }

    #[test]
    fn a_read_that_unwinds_leaves_its_range_unloaded() {
        let (source, blob) = flaky(5 * CHUNK + 100);
        source.0.fail.set(Some(true));
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            blob.get(CHUNK + 5, 5 * CHUNK + 5).map(<[u8]>::to_vec)
        }));
        assert!(unwound.is_err());
        assert!(!blob.has(CHUNK + 5, 1));
        source.0.fail.set(None);
        assert_eq!(blob.get(0, blob.len()).unwrap(), source.0.bytes.as_slice());
        assert_eq!(blob.loaded(), source.0.bytes.len());
    }

    #[test]
    fn a_long_run_is_read_in_pieces_and_once() {
        let (source, blob) = flaky(3 * PIECE + 10);
        assert_eq!(blob.get(0, blob.len()).unwrap(), source.0.bytes.as_slice());
        assert_eq!(source.0.reads.get(), 4);
        blob.get(5, PIECE).unwrap();
        assert_eq!(source.0.reads.get(), 4);
    }

    #[test]
    fn only_the_chunks_read_are_loaded() {
        let (source, blob) = flaky(64 * CHUNK);
        let bytes = blob.bytes().sub(10 * CHUNK, 40 * CHUNK).unwrap();
        // A slice handed out earlier stays valid while later loads write
        // other chunks.
        let early = bytes.get(0, 10).unwrap();
        assert_eq!(
            bytes.window(5 * CHUNK + 3, 4).unwrap(),
            &source.0.bytes[15 * CHUNK + 3..15 * CHUNK + 7]
        );
        assert_eq!(early, &source.0.bytes[10 * CHUNK..10 * CHUNK + 10]);
        assert_eq!(blob.loaded(), 2 * CHUNK);
        assert!(bytes.get(0, 30 * CHUNK + 1).is_err());
        assert_eq!(bytes.window(30 * CHUNK - 2, 10).unwrap().len(), 2);
    }
}
