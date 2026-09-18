//! Where a segment's bytes come from.
//!
//! A segment on disk spans many pages; a query touches a few extents of it.
//! [`Source`] lets the reader ask for exactly those extents. In-memory
//! sources hand out borrowed slices; page-backed sources copy the pages that
//! cover a range.

use std::rc::Rc;

use crate::{Error, Result};

pub trait Source {
    /// Total bytes available.
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies `len` bytes starting at `offset`.
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>>;

    /// A borrowed view of the range, when the source is contiguous in memory.
    fn slice(&self, offset: u64, len: usize) -> Option<&[u8]> {
        let _ = (offset, len);
        None
    }
}

fn check(total: u64, offset: u64, len: usize) -> Result<usize> {
    let end = offset.checked_add(len as u64).ok_or(Error::Truncated)?;
    if end > total {
        return Err(Error::Truncated);
    }
    Ok(offset as usize)
}

impl Source for [u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let at = check(Source::len(self), offset, len)?;
        Ok(self[at..at + len].to_vec())
    }
    fn slice(&self, offset: u64, len: usize) -> Option<&[u8]> {
        let at = check(Source::len(self), offset, len).ok()?;
        Some(&self[at..at + len])
    }
}

impl Source for &[u8] {
    fn len(&self) -> u64 {
        <[u8] as Source>::len(self)
    }
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        <[u8] as Source>::read(self, offset, len)
    }
    fn slice(&self, offset: u64, len: usize) -> Option<&[u8]> {
        <[u8] as Source>::slice(self, offset, len)
    }
}

impl Source for Vec<u8> {
    fn len(&self) -> u64 {
        <[u8] as Source>::len(self)
    }
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        <[u8] as Source>::read(self, offset, len)
    }
    fn slice(&self, offset: u64, len: usize) -> Option<&[u8]> {
        <[u8] as Source>::slice(self, offset, len)
    }
}

impl Source for Rc<Vec<u8>> {
    fn len(&self) -> u64 {
        <[u8] as Source>::len(self)
    }
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        <[u8] as Source>::read(self, offset, len)
    }
    fn slice(&self, offset: u64, len: usize) -> Option<&[u8]> {
        <[u8] as Source>::slice(self, offset, len)
    }
}

impl Source for Box<dyn Source> {
    fn len(&self) -> u64 {
        (**self).len()
    }
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        (**self).read(offset, len)
    }
    fn slice(&self, offset: u64, len: usize) -> Option<&[u8]> {
        (**self).slice(offset, len)
    }
}

/// A source that serves fixed-size pages and copies the ones a range covers.
/// Page-backed storage implements [`PageSource::page`]; everything else is
/// shared.
pub trait PageSource {
    /// Bytes of data per page.
    fn page_len(&self) -> usize;
    /// Number of pages.
    fn pages(&self) -> u64;
    /// Total data bytes (the last page may be partial).
    fn data_len(&self) -> u64;
    /// The data of page `index`, shared so a cache hit costs no copy.
    fn page(&self, index: u64) -> Result<Rc<[u8]>>;
}

impl<P: PageSource> Source for P {
    fn len(&self) -> u64 {
        self.data_len()
    }

    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        check(self.data_len(), offset, len)?;
        let page_len = self.page_len() as u64;
        let mut out = Vec::with_capacity(len);
        let mut at = offset;
        let end = offset + len as u64;
        while at < end {
            let index = at / page_len;
            let within = (at % page_len) as usize;
            let page = self.page(index)?;
            let take = ((end - at) as usize).min(page.len().saturating_sub(within));
            if take == 0 {
                return Err(Error::Truncated);
            }
            out.extend_from_slice(&page[within..within + take]);
            at += take as u64;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Paged(Vec<u8>);

    impl PageSource for Paged {
        fn page_len(&self) -> usize {
            7
        }
        fn pages(&self) -> u64 {
            (self.0.len() as u64).div_ceil(7)
        }
        fn data_len(&self) -> u64 {
            self.0.len() as u64
        }
        fn page(&self, index: u64) -> Result<Rc<[u8]>> {
            let start = index as usize * 7;
            Ok(Rc::from(&self.0[start..(start + 7).min(self.0.len())]))
        }
    }

    #[test]
    fn paged_reads_match_contiguous_reads() {
        let data: Vec<u8> = (0..100).collect();
        let paged = Paged(data.clone());
        assert_eq!(paged.pages(), 15);
        for (offset, len) in [(0, 0), (0, 7), (3, 10), (6, 2), (93, 7), (99, 1), (50, 50)] {
            assert_eq!(
                paged.read(offset, len).unwrap(),
                data.read(offset, len).unwrap()
            );
        }
        assert!(paged.read(94, 7).is_err());
        assert!(data.as_slice().read(100, 1).is_err());
        assert_eq!(data.slice(10, 5), Some(&data[10..15]));
        assert!(paged.slice(0, 1).is_none());
    }
}
