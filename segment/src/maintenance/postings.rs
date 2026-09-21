// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Forward-only posting and score-table decoding for maintenance.

use super::Window;
use crate::{
    Error, Result, Tid,
    pages::Offsets,
    postings::{BLOCK_POSTINGS, BlockBound, LIST_MAX},
    segment::Format,
    source::Source,
    tf_bucket::BUCKET_COUNT,
    tid::{MAX_BLOCK, MAX_OFFSET},
};

/// A posting and, on the last posting of a scored block, its stored bound.
/// Compare that bound with minima accumulated from document lengths and payload
/// buckets for **all** postings, including dead ones, before reusing metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PostingEntry {
    pub tid: Tid,
    pub completed_bound: Option<BlockBound>,
}

/// Two owned refill windows plus fixed-size page/bound state; no vector grows
/// with the term or its score table. Source caching is outside this bound.
/// Traversal validates stream counts, ordering, group framing, bitmap padding,
/// score-table framing, block endpoints and sparse block starts. It does not
/// prove document ownership or score minima: retain whole-input verification
/// until the caller performs those cross-stream checks too.
///
/// Consume through `None` to check exact exhaustion. Any error poisons the
/// cursor; no prefix produced by a failed traversal may be published.
pub struct PostingsCursor<'a, S: Source + ?Sized> {
    body: Window<'a, S>,
    bounds: Window<'a, S>,
    body_start: u64,
    count: u32,
    ordinal: u32,
    previous: Option<Tid>,
    grouped: bool,
    scored: bool,
    compact: bool,
    minima: [u32; BUCKET_COUNT],
    bound_last: Option<Tid>,
    previous_bound: Option<Tid>,
    sparse_start: u64,
    groups_left: u32,
    gid: Option<u32>,
    group_count: u32,
    group_seen: u32,
    group_end: u64,
    pages: [u8; 32],
    page_bit: u16,
    page_block: u32,
    offsets: Offsets,
    failed: bool,
}

fn minima<S: Source + ?Sized>(reader: &mut Window<'_, S>) -> Result<[u32; BUCKET_COUNT]> {
    let mask = reader.u32()?;
    if mask == 0 || mask >> BUCKET_COUNT != 0 {
        return Err(Error::Corrupt("block bound buckets"));
    }
    let mut out = [u32::MAX; BUCKET_COUNT];
    for (bucket, length) in out.iter_mut().enumerate() {
        if mask & (1 << bucket) != 0 {
            *length = reader.u32()?;
            if *length == u32::MAX {
                return Err(Error::Corrupt("block bound length"));
            }
        }
    }
    Ok(out)
}

impl<'a, S: Source + ?Sized> PostingsCursor<'a, S> {
    pub fn new(
        source: &'a S,
        start: u64,
        len: u64,
        format: Format,
        window_bytes: usize,
    ) -> Result<Self> {
        let end = start.checked_add(len).ok_or(Error::Truncated)?;
        let mut header = Window::new(source, start, end, window_bytes)?;
        let form = header.byte()?;
        if form > 7 || form & 6 == 6 {
            return Err(Error::Corrupt("unknown postings form"));
        }
        let count = header.u32()?;
        let grouped = form & 1 != 0;
        let scored = form & 6 != 0;
        let compact = form & 4 != 0;
        if scored
            && (format == Format::Lsg1
                || (compact && (format < Format::Lsg3 || count == 0 || count > BLOCK_POSTINGS))
                || (!compact && format >= Format::Lsg3 && count <= BLOCK_POSTINGS))
        {
            return Err(Error::Corrupt("postings bounds layout for format"));
        }
        let (bounds_start, bounds_end) = if form & 2 != 0 {
            let bytes = u64::from(header.u32()?);
            (
                header.at,
                header
                    .at
                    .checked_add(bytes)
                    .filter(|at| *at <= end)
                    .ok_or(Error::Truncated)?,
            )
        } else if compact {
            let at = header.at;
            minima(&mut header)?;
            (at, header.at)
        } else {
            (header.at, header.at)
        };
        drop(header);
        let mut body = Window::new(source, bounds_end, end, window_bytes)?;
        let groups_left = if grouped { body.u32()? } else { 0 };
        Ok(Self {
            body,
            bounds: Window::new(source, bounds_start, bounds_end, window_bytes)?,
            body_start: bounds_end,
            count,
            ordinal: 0,
            previous: None,
            grouped,
            scored,
            compact,
            minima: [u32::MAX; BUCKET_COUNT],
            bound_last: None,
            previous_bound: None,
            sparse_start: 0,
            groups_left,
            gid: None,
            group_count: 0,
            group_seen: 0,
            group_end: 0,
            pages: [0; 32],
            page_bit: 256,
            page_block: 0,
            offsets: Offsets::default(),
            failed: false,
        })
    }

    pub fn count(&self) -> u32 {
        self.count
    }
    pub fn has_bounds(&self) -> bool {
        self.scored
    }
    pub fn is_grouped(&self) -> bool {
        self.grouped
    }
    /// Owned encoded bytes, excluding fixed-size state and source caches.
    pub fn retained_bytes(&self) -> usize {
        self.body.bytes.len() + self.bounds.bytes.len()
    }

    /// One checkpoint before each posting (and the final exhaustion check).
    /// Per-call decoding work is bounded by one group header, page and bound.
    pub fn next_with(
        &mut self,
        mut checkpoint: impl FnMut() -> Result<()>,
    ) -> Result<Option<PostingEntry>> {
        if self.failed {
            return Err(Error::Corrupt("maintenance cursor failed"));
        }
        let result = checkpoint().and_then(|_| self.next_inner());
        self.failed = result.is_err();
        result
    }

    fn next_inner(&mut self) -> Result<Option<PostingEntry>> {
        if self.ordinal == self.count {
            if self.grouped {
                self.finish_group()?;
                if self.groups_left != 0
                    || self.pages.iter().any(|b| *b != 0)
                    || !self.offsets.is_empty()
                {
                    return Err(Error::Corrupt("posting count mismatch"));
                }
            }
            if self.body.at != self.body.end || self.bounds.at != self.bounds.end {
                return Err(Error::Corrupt("postings trailing bytes"));
            }
            self.body.bytes = Box::default();
            self.bounds.bytes = Box::default();
            return Ok(None);
        }
        if self.scored && self.ordinal.is_multiple_of(BLOCK_POSTINGS) {
            self.read_bound()?;
        }
        let tid = if self.grouped {
            self.grouped_tid()?
        } else {
            let block = self
                .previous
                .map_or(0, |p| p.block)
                .checked_add(self.body.u32()?)
                .ok_or(Error::Corrupt("block overflow"))?;
            let offset = u16::try_from(self.body.u32()?).map_err(|_| Error::InvalidTid)?;
            Tid::new(block, offset)?
        };
        if self.previous.is_some_and(|p| p >= tid) {
            return Err(Error::Unordered);
        }
        self.previous = Some(tid);
        self.ordinal += 1;
        let completed_bound = if self.scored
            && (self.ordinal.is_multiple_of(BLOCK_POSTINGS) || self.ordinal == self.count)
        {
            if self.bound_last.is_some_and(|last| last != tid) {
                return Err(Error::Corrupt("block bound endpoint"));
            }
            Some(BlockBound {
                min_len: self.minima,
                last: tid,
            })
        } else {
            None
        };
        Ok(Some(PostingEntry {
            tid,
            completed_bound,
        }))
    }

    fn read_bound(&mut self) -> Result<()> {
        self.minima = minima(&mut self.bounds)?;
        if !self.compact {
            let block = self
                .previous_bound
                .map_or(0, |p| p.block)
                .checked_add(self.bounds.u32()?)
                .ok_or(Error::Corrupt("block overflow"))?;
            let offset = u16::try_from(self.bounds.u32()?).map_err(|_| Error::InvalidTid)?;
            let last = Tid::new(block, offset)?;
            if self.previous_bound.is_some_and(|p| p >= last) {
                return Err(Error::Corrupt("block bounds not increasing"));
            }
            self.previous_bound = Some(last);
            self.bound_last = Some(last);
            if !self.grouped {
                self.sparse_start = self
                    .sparse_start
                    .checked_add(self.bounds.varint()?)
                    .ok_or(Error::Corrupt("block start overflow"))?;
                if self.sparse_start != self.body.at - self.body_start {
                    return Err(Error::Corrupt("block start mismatch"));
                }
            }
        }
        Ok(())
    }

    fn finish_group(&self) -> Result<()> {
        if self.gid.is_some()
            && (self.group_seen != self.group_count || self.body.at != self.group_end)
        {
            return Err(Error::Corrupt("group count or body length mismatch"));
        }
        Ok(())
    }

    fn grouped_tid(&mut self) -> Result<Tid> {
        if let Some(offset) = self.offsets.pop_first() {
            return Tid::new(self.page_block, offset);
        }
        if self.pages.iter().all(|b| *b == 0) {
            self.finish_group()?;
            if self.groups_left == 0 {
                return Err(Error::Corrupt("posting count mismatch"));
            }
            self.groups_left -= 1;
            let delta = self.body.u32()?;
            let gid = match self.gid {
                None => delta,
                Some(previous) => previous
                    .checked_add(delta)
                    .and_then(|g| g.checked_add(1))
                    .ok_or(Error::Corrupt("group id overflow"))?,
            };
            if u64::from(gid) * 256 > u64::from(MAX_BLOCK) {
                return Err(Error::Corrupt("group beyond block range"));
            }
            self.gid = Some(gid);
            self.group_count = self.body.u32()?;
            if self.group_count == 0 || self.group_count > self.count - self.ordinal {
                return Err(Error::Corrupt("group count mismatch"));
            }
            for byte in &mut self.pages {
                *byte = self.body.byte()?;
            }
            if self.pages.iter().all(|b| *b == 0) {
                return Err(Error::Corrupt("empty group"));
            }
            let length = self.body.varint()?;
            self.group_end = self
                .body
                .at
                .checked_add(length)
                .filter(|end| *end <= self.body.end)
                .ok_or(Error::Truncated)?;
            self.group_seen = 0;
            self.page_bit = 0;
        }
        while self.page_bit < 256
            && self.pages[usize::from(self.page_bit / 8)] & (1 << (self.page_bit % 8)) == 0
        {
            self.page_bit += 1;
        }
        if self.page_bit == 256 {
            return Err(Error::Corrupt("group page bitmap"));
        }
        self.pages[usize::from(self.page_bit / 8)] &= !(1 << (self.page_bit % 8));
        self.page_block = self.gid.expect("group entered") * 256 + u32::from(self.page_bit);
        self.page_bit += 1;
        if self.body.at >= self.group_end {
            return Err(Error::Corrupt("page beyond group body"));
        }
        self.offsets = match self.body.byte()? {
            0 => {
                let count = self.body.u32()?;
                if count == 0 || count as usize > LIST_MAX {
                    return Err(Error::Corrupt("offset list length"));
                }
                let mut offsets = Offsets::default();
                let mut previous = 0;
                for _ in 0..count {
                    let offset = u16::from_le_bytes([self.body.byte()?, self.body.byte()?]);
                    if offset == 0 || offset > MAX_OFFSET || offset <= previous {
                        return Err(Error::Corrupt("offset list not increasing"));
                    }
                    offsets.insert(offset);
                    previous = offset;
                }
                offsets
            }
            1 => {
                let mut bytes = [0; 37];
                for byte in &mut bytes {
                    *byte = self.body.byte()?;
                }
                let offsets = Offsets::from_bitmap(&bytes)?;
                if offsets.is_empty() {
                    return Err(Error::Corrupt("empty tuple bitmap"));
                }
                offsets
            }
            _ => return Err(Error::Corrupt("unknown page tag")),
        };
        if self.body.at > self.group_end {
            return Err(Error::Corrupt("page beyond group body"));
        }
        self.group_seen += self.offsets.count();
        if self.group_seen > self.group_count {
            return Err(Error::Corrupt("group count mismatch"));
        }
        Tid::new(
            self.page_block,
            self.offsets.pop_first().expect("nonempty page checked"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postings::{Postings, PostingsBuilder};
    use std::cell::Cell;

    struct Tracked {
        bytes: Vec<u8>,
        max_read: Cell<usize>,
    }
    impl Source for Tracked {
        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }
        fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
            self.max_read.set(self.max_read.get().max(len));
            self.bytes
                .get(offset as usize..offset as usize + len)
                .map(|s| s.to_vec())
                .ok_or(Error::Truncated)
        }
    }
    fn build(tids: &[Tid], format: Format, scored: bool) -> Vec<u8> {
        let mut builder = PostingsBuilder::default();
        for tid in tids {
            if scored {
                builder
                    .push_scored(*tid, (tid.offset % 16) as u8, tid.block % 100 + 1)
                    .unwrap();
            } else {
                builder.push(*tid).unwrap();
            }
        }
        builder.finish_as(format)
    }
    fn validate(bytes: &[u8], format: Format, window: usize) -> Result<()> {
        let mut cursor = PostingsCursor::new(bytes, 0, bytes.len() as u64, format, window)?;
        while cursor.next_with(|| Ok(()))?.is_some() {}
        Ok(())
    }
    fn sparse(count: u32) -> Vec<Tid> {
        (0..count).map(|i| Tid::new(i * 1009, 1).unwrap()).collect()
    }
    fn dense(blocks: u32) -> Vec<Tid> {
        (0..blocks)
            .flat_map(|block| {
                (1..=if block % 2 == 0 { 291 } else { 3 })
                    .map(move |offset| Tid::new(block, offset).unwrap())
            })
            .collect()
    }

    #[test]
    fn matches_existing_rows_and_bounds_in_all_formats_and_windows() {
        for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] {
            for tids in [
                sparse(0),
                sparse(1),
                sparse(128),
                sparse(129),
                sparse(301),
                dense(3),
                dense(260),
            ] {
                for scored in [false, true] {
                    let bytes = build(&tids, format, scored);
                    let parsed = Postings::parse(&bytes).unwrap();
                    let expected_bounds = parsed.cursor().unwrap().block_bounds().unwrap();
                    for window in [1, 2, 7, 127, 4096] {
                        let source = Tracked {
                            bytes: bytes.clone(),
                            max_read: Cell::new(0),
                        };
                        let mut cursor =
                            PostingsCursor::new(&source, 0, source.len(), format, window).unwrap();
                        assert_eq!(cursor.count() as usize, tids.len());
                        assert_eq!(cursor.has_bounds(), parsed.has_bounds());
                        assert_eq!(cursor.is_grouped(), parsed.is_grouped());
                        let mut rows = Vec::new();
                        let mut bounds = Vec::new();
                        while let Some(entry) = cursor.next_with(|| Ok(())).unwrap() {
                            rows.push(entry.tid);
                            if let Some(bound) = entry.completed_bound {
                                bounds.push(bound);
                            }
                            assert!(cursor.retained_bytes() <= 2 * window);
                        }
                        assert_eq!(rows, tids);
                        assert_eq!(bounds, expected_bounds);
                        assert_eq!(cursor.retained_bytes(), 0);
                        assert!(source.max_read.get() <= window);
                    }
                }
            }
        }
    }

    #[test]
    fn large_common_term_and_bounds_keep_constant_owned_bytes() {
        let tids = dense(4096);
        let source = Tracked {
            bytes: build(&tids, Format::Lsg3, true),
            max_read: Cell::new(0),
        };
        let mut cursor = PostingsCursor::new(&source, 0, source.len(), Format::Lsg3, 113).unwrap();
        let mut count = 0;
        while let Some(entry) = cursor.next_with(|| Ok(())).unwrap() {
            assert_eq!(entry.tid, tids[count]);
            count += 1;
            assert!(cursor.retained_bytes() <= 226);
        }
        assert_eq!(count, tids.len());
        assert!(count > 500_000);
        assert!(source.max_read.get() <= 113);
    }

    #[test]
    fn truncation_and_trailing_data_fail_for_sparse_grouped_and_legacy() {
        for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] {
            for tids in [sparse(129), dense(2)] {
                let bytes = build(&tids, format, true);
                for cut in 0..bytes.len() {
                    assert!(validate(&bytes[..cut], format, 3).is_err(), "cut {cut}");
                }
                let mut extra = bytes;
                extra.push(0);
                assert!(validate(&extra, format, 1).is_err());
            }
        }
    }

    #[test]
    fn rejects_invalid_sparse_tids_forms_counts_and_bounds() {
        for bytes in [
            vec![8, 0],
            vec![6, 0],
            vec![0, 1, 0, 0],
            vec![0, 2, 0, 1, 0, 1],
            vec![0, 0, 0, 1],
            vec![4, 0, 1, 1],
        ] {
            assert!(validate(&bytes, Format::Lsg3, 1).is_err());
        }
        let tids = sparse(129);
        let mut wrong = build(&tids, Format::Lsg3, true);
        // Header: form, 2-byte count, bounds length. Decode to the first
        // table entry independently and mutate its endpoint or sparse start.
        let mut reader = crate::reader::Reader::new(&wrong);
        reader.u8().unwrap();
        reader.varint().unwrap();
        reader.varint().unwrap();
        reader.varint().unwrap();
        reader.varint().unwrap(); // one bucket + length
        let endpoint = reader.position();
        reader.varint().unwrap();
        reader.varint().unwrap();
        let start = reader.position();
        wrong[start] = 1;
        assert!(validate(&wrong, Format::Lsg3, 1).is_err());
        wrong = build(&tids, Format::Lsg3, true);
        wrong[endpoint] ^= 1;
        assert!(validate(&wrong, Format::Lsg3, 1).is_err());
        let compact = build(&sparse(1), Format::Lsg3, true);
        assert!(validate(&compact, Format::Lsg2, 1).is_err());
        assert!(validate(&compact, Format::Lsg1, 1).is_err());
    }

    #[test]
    fn validates_group_counts_extents_page_lists_and_bitmap_padding() {
        // One group, one page, bitmap containing offset 1. Force grouped
        // framing so malformed data does not depend on builder heuristics.
        let mut bytes = vec![1, 1, 1, 0, 1];
        bytes.push(1);
        bytes.extend_from_slice(&[0; 31]);
        bytes.push(38);
        bytes.push(1);
        bytes.push(1);
        bytes.extend_from_slice(&[0; 36]);
        assert!(validate(&bytes, Format::Lsg3, 1).is_ok());
        for (at, value) in [(1, 0), (2, 2), (4, 2), (5, 0), (37, 37), (38, 2), (75, 128)] {
            let mut wrong = bytes.clone();
            wrong[at] = value;
            assert!(validate(&wrong, Format::Lsg3, 1).is_err(), "at {at}");
        }
        let mut list = bytes[..37].to_vec();
        list.extend_from_slice(&[6, 0, 2, 1, 0, 2, 0]);
        list[1] = 2;
        list[4] = 2;
        assert!(validate(&list, Format::Lsg3, 1).is_ok());
        let last = list.len() - 2;
        list[last] = 1;
        assert!(validate(&list, Format::Lsg3, 1).is_err());
    }

    #[test]
    fn every_checkpoint_can_cancel_and_errors_poison_the_cursor() {
        let bytes = build(&dense(2), Format::Lsg3, true);
        let count = Postings::parse(&bytes).unwrap().count();
        for cancel_at in 0..=count {
            let mut cursor =
                PostingsCursor::new(&bytes, 0, bytes.len() as u64, Format::Lsg3, 3).unwrap();
            for _ in 0..cancel_at {
                cursor.next_with(|| Ok(())).unwrap();
            }
            assert_eq!(
                cursor.next_with(|| Err(Error::Corrupt("cancelled"))),
                Err(Error::Corrupt("cancelled"))
            );
            assert_eq!(
                cursor.next_with(|| Ok(())),
                Err(Error::Corrupt("maintenance cursor failed"))
            );
        }
    }

    proptest::proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512), window in 1usize..64) {
            for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] { let _ = validate(&bytes, format, window); }
        }
    }
}
