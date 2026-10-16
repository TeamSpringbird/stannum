// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! What a merge streams through: its inputs' runs read a range at a time
//! ([`RunInput`]), what it builds term by term set aside in temporary files
//! ([`TempSpill`]), and the merged run written page by page as it is laid
//! out ([`RunWriter`]). A merge holds neither its inputs' bytes nor its
//! output's: at 150M rows a segment is 9 to 16 GiB, and an offline merge
//! of whole blobs peaked at 56 GB.

use std::cell::RefCell;

use pgrx::pg_sys;

use super::layout::{self, CHAIN_CAPACITY, KIND_RUN, NONE, Run};
use super::{Buffer, run_page, write_page, write_run};
use segment::tinshape::segment::{Area, Sink, Spill};

/// `whence` for an absolute seek, as in `<stdio.h>`.
const SEEK_SET: std::ffi::c_int = 0;

/// A published segment run read by range through its page table, for work
/// done without the meta lock: a page that is no longer the run's (retired
/// and reclaimed meanwhile) fails the read rather than reporting corruption,
/// and [`Self::failure`] says why, for the caller to judge once it knows
/// whether the run is still published (see `unlocked`).
pub(super) struct RunInput {
    index: pg_sys::Relation,
    run: Run,
    table: Vec<u32>,
    nblocks: u32,
    label: String,
    failure: RefCell<Option<String>>,
    /// Flips the first byte read (tests of corruption reporting).
    corrupt: bool,
}

impl RunInput {
    /// # Safety
    /// `index` stays open while the source is used; `table` lists the run's
    /// pages in order.
    pub(super) unsafe fn new(
        index: pg_sys::Relation,
        run: Run,
        table: Vec<u32>,
        label: String,
    ) -> Self {
        Self {
            index,
            run,
            table,
            nblocks: unsafe { super::blocks(index) },
            label,
            failure: RefCell::new(None),
            corrupt: false,
        }
    }

    #[cfg(feature = "pg_test")]
    pub(super) fn corrupt_first_byte(&mut self) {
        self.corrupt = true;
    }

    /// Why a read failed, if one did.
    pub(super) fn failure(&self) -> Option<String> {
        self.failure.borrow().clone()
    }

    fn fail(&self, message: String) -> segment::Error {
        self.failure.borrow_mut().get_or_insert(message);
        segment::Error::Corrupt("a run page is not the run's")
    }
}

impl segment::source::Source for RunInput {
    fn len(&self) -> u64 {
        self.run.bytes
    }

    fn read(&self, offset: u64, len: usize) -> segment::Result<Vec<u8>> {
        let end = offset
            .checked_add(len as u64)
            .filter(|end| *end <= self.run.bytes)
            .ok_or(segment::Error::Truncated)?;
        let mut out = Vec::with_capacity(len);
        let mut at = offset;
        while at < end {
            let page = (at / CHAIN_CAPACITY as u64) as usize;
            let within = (at % CHAIN_CAPACITY as u64) as usize;
            let block = *self.table.get(page).ok_or(segment::Error::Truncated)?;
            if block >= self.nblocks {
                return Err(self.fail(format!(
                    "Stannum {}: page {block} is beyond the end of the index",
                    self.label
                )));
            }
            // SAFETY: `index` is open (see `new`); one page lock at a time.
            let buffer = unsafe { Buffer::read(self.index, block, false) };
            let (_, data) = run_page(&buffer, &self.label).map_err(|message| self.fail(message))?;
            let take = ((end - at) as usize).min(data.len().saturating_sub(within));
            if take == 0 {
                return Err(self.fail(format!(
                    "Stannum {}: page {block} ends before byte {at}",
                    self.label
                )));
            }
            out.extend_from_slice(&data[within..within + take]);
            at += take as u64;
        }
        if self.corrupt && offset == 0 && !out.is_empty() {
            out[0] ^= 0xff;
        }
        Ok(out)
    }
}

/// A [`Spill`] in temporary files, one per area, which PostgreSQL removes
/// with the transaction that made them whether it commits, fails or the
/// server restarts. They count against `temp_file_limit`.
pub(super) struct TempSpill {
    files: [*mut pg_sys::BufFile; 3],
}

impl TempSpill {
    pub(super) fn new() -> Self {
        Self {
            files: [std::ptr::null_mut(); 3],
        }
    }

    fn slot(area: Area) -> usize {
        match area {
            Area::TermMap => 0,
            Area::Postings => 1,
            Area::Positions => 2,
            _ => unreachable!("only the term map, postings and positions spill"),
        }
    }
}

impl Spill for TempSpill {
    fn append(&mut self, area: Area, bytes: &[u8]) -> segment::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let file = &mut self.files[Self::slot(area)];
        // SAFETY: the file is this spill's, created in this transaction.
        unsafe {
            if file.is_null() {
                *file = pg_sys::BufFileCreateTemp(false);
            }
            pg_sys::BufFileWrite(*file, bytes.as_ptr().cast(), bytes.len());
        }
        Ok(())
    }

    fn replay(
        &mut self,
        area: Area,
        out: &mut dyn FnMut(&[u8]) -> segment::Result<()>,
    ) -> segment::Result<()> {
        let file = self.files[Self::slot(area)];
        if file.is_null() {
            return Ok(());
        }
        let mut chunk = vec![0u8; 1 << 20];
        // SAFETY: as in `append`; BufFile reports I/O errors itself.
        unsafe {
            if pg_sys::BufFileSeek(file, 0, 0, SEEK_SET) != 0 {
                pgrx::error!("could not rewind a Stannum merge's temporary file");
            }
            loop {
                pgrx::check_for_interrupts!();
                let read = pg_sys::BufFileRead(file, chunk.as_mut_ptr().cast(), chunk.len());
                if read == 0 {
                    return Ok(());
                }
                out(&chunk[..read])?;
            }
        }
    }
}

impl Drop for TempSpill {
    fn drop(&mut self) {
        // Unwinding from an error, the transaction's abort closes the files.
        if std::thread::panicking() {
            return;
        }
        for file in self.files {
            if !file.is_null() {
                // SAFETY: opened by `append` and not closed since.
                unsafe { pg_sys::BufFileClose(file) };
            }
        }
    }
}

/// Writes a run front to back as its bytes arrive, a page at a time, with
/// its page table at the end. Each page is claimed when its predecessor
/// needs its block number for the link: claimed as an empty run page, so
/// that an allocation elsewhere, which takes only FREE pages, never takes
/// it, and written in full when its bytes are complete. Pages written and
/// never published are unreferenced, for VACUUM's orphan pass (see
/// `reclaim_orphans`), or freed by [`Self::discard`].
pub(super) struct RunWriter {
    index: pg_sys::Relation,
    /// The bytes of the page being filled, and its block once claimed.
    page: Vec<u8>,
    current: Option<u32>,
    blocks: Vec<u32>,
    bytes: u64,
}

impl RunWriter {
    pub(super) fn new(index: pg_sys::Relation) -> Self {
        Self {
            index,
            page: Vec::with_capacity(CHAIN_CAPACITY),
            current: None,
            blocks: Vec::new(),
            bytes: 0,
        }
    }

    /// A page claimed for the run.
    fn claim(&mut self) -> u32 {
        pgrx::check_for_interrupts!();
        // SAFETY: the writer's index is open; one page lock at a time.
        unsafe {
            let buffer = Buffer::allocate(self.index);
            write_page(
                self.index,
                &buffer,
                true,
                KIND_RUN,
                &layout::chain_payload(NONE, &[]),
            );
            let block = buffer.block();
            self.blocks.push(block);
            block
        }
    }

    /// Writes the page being filled, linked to `next`.
    fn put(&mut self, next: u32) {
        let block = match self.current {
            Some(block) => block,
            None => {
                let block = self.claim();
                self.current = Some(block);
                block
            }
        };
        // SAFETY: the page was claimed by this writer and is published
        // nowhere, so nothing else writes it.
        unsafe {
            let buffer = Buffer::read(self.index, block, true);
            write_page(
                self.index,
                &buffer,
                false,
                KIND_RUN,
                &layout::chain_payload(next, &self.page),
            );
        }
        self.page.clear();
    }

    /// The run and its page table.
    pub(super) fn finish(mut self) -> (Run, Run) {
        self.put(NONE);
        let pages = self.blocks.len() as u32;
        if u64::from(pages) != super::run_pages(self.bytes) as u64 {
            pgrx::error!("Stannum merge wrote {pages} pages for {} bytes", self.bytes);
        }
        let run = Run {
            first: self.blocks[0],
            blocks: pages,
            bytes: self.bytes,
            last: *self.blocks.last().expect("a run has a page"),
        };
        let mut table = Vec::with_capacity(self.blocks.len() * 4);
        for block in &self.blocks {
            table.extend_from_slice(&block.to_le_bytes());
        }
        // SAFETY: as in `claim`.
        (run, unsafe { write_run(self.index, &table) })
    }

    /// Frees the pages written so far, which nothing references.
    pub(super) fn discard(self) {
        // SAFETY: the pages are this writer's own and were never published.
        unsafe {
            let stamp = pg_sys::ReadNextTransactionId().into_inner();
            super::free_pages(self.index, &self.blocks, stamp);
        }
    }
}

impl Sink for RunWriter {
    fn write(&mut self, mut bytes: &[u8]) -> segment::Result<()> {
        if self.current.is_none() && !bytes.is_empty() {
            self.current = Some(self.claim());
        }
        self.bytes += bytes.len() as u64;
        while !bytes.is_empty() {
            if self.page.len() == CHAIN_CAPACITY {
                // More is coming, so the full page links to a successor.
                let next = self.claim();
                self.put(next);
                self.current = Some(next);
            }
            let take = (CHAIN_CAPACITY - self.page.len()).min(bytes.len());
            self.page.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
        }
        Ok(())
    }
}
