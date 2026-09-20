// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Experimental output spilling. The budget covers only accumulated encoded
//! postings/payload, not retained inputs, metadata or per-term codec scratch.
use super::*;
use segment::merge::{MergeError, OutputSink};
use segment::source::Source;

struct TempFile(*mut pg_sys::BufFile);
impl TempFile {
    fn new() -> Self {
        Self(unsafe { pg_sys::BufFileCreateTemp(false) })
    }
    fn append(&mut self, bytes: &[u8]) {
        unsafe { pg_sys::BufFileWrite(self.0, bytes.as_ptr().cast(), bytes.len()) };
        race_point("spill:written");
    }
    fn read(&mut self, at: usize, bytes: &mut [u8]) -> segment::Result<()> {
        if unsafe { pg_sys::BufFileSeek(self.0, 0, at as _, 0) } != 0 {
            pgrx::error!("Stannum temporary merge output seek failed");
        }
        let count = unsafe { pg_sys::BufFileRead(self.0, bytes.as_mut_ptr().cast(), bytes.len()) };
        if count != bytes.len() {
            pgrx::error!("Stannum temporary merge output is truncated");
        }
        race_point("spill:read");
        Ok(())
    }
}
impl Drop for TempFile {
    fn drop(&mut self) {
        // Closing flushes buffered writes and can raise another PostgreSQL
        // error (for example after temp_file_limit was hit). Never do that
        // while unwinding: the ResourceOwner will close the underlying files
        // and the transaction memory context owns BufFile's allocation.
        if !std::thread::panicking() {
            unsafe { pg_sys::BufFileClose(self.0) };
        }
    }
}

enum Area {
    Memory(Vec<u8>),
    Disk { file: TempFile, len: usize },
}
impl Area {
    fn len(&self) -> usize {
        match self {
            Self::Memory(v) => v.len(),
            Self::Disk { len, .. } => *len,
        }
    }
    fn spill(&mut self) {
        if let Self::Memory(bytes) = self {
            let mut file = TempFile::new();
            file.append(bytes);
            let len = bytes.len();
            *self = Self::Disk { file, len };
        }
    }
    fn append(&mut self, bytes: &[u8]) {
        match self {
            Self::Memory(v) => v.extend_from_slice(bytes),
            Self::Disk { file, len } => {
                file.append(bytes);
                *len += bytes.len();
            }
        }
    }
    fn read(&mut self, at: usize, out: &mut [u8]) -> segment::Result<()> {
        match self {
            Self::Memory(v) => {
                out.copy_from_slice(&v[at..at + out.len()]);
                Ok(())
            }
            Self::Disk { file, .. } => file.read(at, out),
        }
    }
}

pub(super) struct SpillSink {
    postings: Area,
    payload: Area,
    budget: usize,
}
impl SpillSink {
    pub(super) fn new(budget: usize) -> Self {
        Self {
            postings: Area::Memory(Vec::new()),
            payload: Area::Memory(Vec::new()),
            budget,
        }
    }
}
impl OutputSink for SpillSink {
    type Output = Output;
    fn lengths(&self) -> (usize, usize) {
        (self.postings.len(), self.payload.len())
    }
    fn append(&mut self, postings: &[u8], payload: &[u8]) -> Result<(), MergeError> {
        // Reserve exact capacities, charged together. Once spilled, neither
        // area comes back into RAM, including during final index publication.
        if let (Area::Memory(a), Area::Memory(b)) = (&mut self.postings, &mut self.payload) {
            let needed_a = a
                .len()
                .checked_add(postings.len())
                .ok_or(MergeError::Limit("output bytes"))?;
            let needed_b = b
                .len()
                .checked_add(payload.len())
                .ok_or(MergeError::Limit("output bytes"))?;
            let capacity = needed_a
                .max(a.capacity())
                .checked_add(needed_b.max(b.capacity()))
                .ok_or(MergeError::Limit("output bytes"))?;
            if capacity <= self.budget {
                a.try_reserve_exact(postings.len())
                    .map_err(|_| MergeError::Allocation)?;
                b.try_reserve_exact(payload.len())
                    .map_err(|_| MergeError::Allocation)?;
            }
            if capacity > self.budget || a.capacity().saturating_add(b.capacity()) > self.budget {
                self.postings.spill();
                self.payload.spill();
                race_point("spill:ready");
            }
        }
        self.postings.append(postings);
        self.payload.append(payload);
        Ok(())
    }
    fn finish(
        self,
        header: Vec<u8>,
        dictionary: Vec<u8>,
        documents: Vec<u8>,
        lengths: Vec<u8>,
        limit: usize,
    ) -> Result<Output, MergeError> {
        let areas = [
            Area::Memory(header),
            Area::Memory(dictionary),
            self.postings,
            self.payload,
            Area::Memory(documents),
            Area::Memory(lengths),
        ];
        let mut total = 0usize;
        for area in &areas {
            total = total
                .checked_add(area.len())
                .ok_or(MergeError::Limit("output bytes"))?;
        }
        if total > limit {
            return Err(MergeError::Limit("output bytes"));
        }
        Ok(Output {
            areas: RefCell::new(areas),
            len: total,
        })
    }
}

pub(super) struct Output {
    areas: RefCell<[Area; 6]>,
    len: usize,
}
impl Source for &Output {
    fn len(&self) -> u64 {
        self.len as u64
    }
    fn read(&self, offset: u64, len: usize) -> segment::Result<Vec<u8>> {
        let end = offset
            .checked_add(len as u64)
            .ok_or(segment::Error::Truncated)?;
        if end > self.len as u64 {
            return Err(segment::Error::Truncated);
        }
        let mut out = vec![0; len];
        let mut at = offset as usize;
        let mut written = 0;
        for area in self.areas.borrow_mut().iter_mut() {
            if at >= area.len() {
                at -= area.len();
                continue;
            }
            let take = (area.len() - at).min(len - written);
            area.read(at, &mut out[written..written + take])?;
            written += take;
            at = 0;
            if written == len {
                break;
            }
        }
        Ok(out)
    }
}

/// Same reverse-linked run and page map as write_segment_run, with one page
/// of transfer storage. Do not construct a complete output Vec on publication.
pub(super) unsafe fn write(index: pg_sys::Relation, output: &Output) -> (Run, Run) {
    unsafe {
        let count = output.len.div_ceil(CHAIN_CAPACITY).max(1);
        let mut next = NONE;
        let mut pages = Vec::with_capacity(count);
        for i in (0..count).rev() {
            pgrx::check_for_interrupts!();
            let at = i * CHAIN_CAPACITY;
            let bytes = codec(output.read(at as u64, (output.len - at).min(CHAIN_CAPACITY)));
            let buffer = Buffer::allocate(index);
            write_page(
                index,
                &buffer,
                true,
                KIND_RUN,
                &layout::chain_payload(next, &bytes),
            );
            race_point("spill:page-written");
            next = buffer.block();
            pages.push(next);
        }
        let mut table = Vec::with_capacity(pages.len() * 4);
        for page in pages.into_iter().rev() {
            table.extend_from_slice(&page.to_le_bytes());
        }
        (
            Run {
                first: next,
                blocks: count as u32,
                bytes: output.len as u32,
            },
            write_run(index, &table),
        )
    }
}

#[cfg(feature = "pg_test")]
pub(super) fn check_spilled_output() {
    use segment::merge::{MergeInput, MergeLimits};
    use std::collections::BTreeSet;
    let mut blobs = Vec::new();
    for source in 0..3 {
        let mut builder = SegmentBuilder::default();
        for n in 0..120 {
            let tid = Tid {
                block: n,
                offset: source + 1,
            };
            builder
                .add_document(tid, [("common", 0), ("rare", 1), ("common", 2)])
                .unwrap();
        }
        blobs.push(builder.finish());
    }
    let dead = BTreeSet::from([Tid {
        block: 10,
        offset: 1,
    }]);
    let empty = BTreeSet::new();
    let inputs = blobs
        .iter()
        .enumerate()
        .map(|(i, bytes)| MergeInput {
            bytes,
            dead: if i == 0 { &dead } else { &empty },
        })
        .collect::<Vec<_>>();
    let limits = MergeLimits {
        max_inputs: 10,
        max_input_bytes: 1 << 20,
        max_output_bytes: 1 << 20,
        max_documents: 1000,
    };
    let expected = segment::merge::merge(&inputs, limits, || Ok(())).unwrap();
    for budget in [0, 1, 1024, 1 << 20] {
        let out =
            segment::merge::merge_into(&inputs, limits, SpillSink::new(budget), || Ok(())).unwrap();
        if budget <= 1 {
            assert!(matches!(out.areas.borrow()[2], Area::Disk { .. }));
        }
        assert_eq!((&out).read(0, out.len).unwrap(), expected);
        for at in (0..out.len).rev().step_by(37) {
            let n = (out.len - at).min(113);
            assert_eq!((&out).read(at as u64, n).unwrap(), expected[at..at + n]);
        }
        assert!((&out).read(out.len as u64, 1).is_err());
    }
    let mut limited = limits;
    limited.max_output_bytes = expected.len() - 1;
    assert!(segment::merge::merge_into(&inputs, limited, SpillSink::new(1), || Ok(())).is_err());
    let mut checkpoints = 0;
    assert!(matches!(
        segment::merge::merge_into(&inputs, limits, SpillSink::new(1), || {
            checkpoints += 1;
            if checkpoints > 850 {
                Err(MergeError::Cancelled)
            } else {
                Ok(())
            }
        }),
        Err(MergeError::Cancelled)
    ));
}
