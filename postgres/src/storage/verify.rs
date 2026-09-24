// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! `stannum.verify_index`: walks a whole index and lists every inconsistency
//! instead of failing on the first one, in the spirit of `amcheck`.
//!
//! The walk reads pages directly, never through the per-backend caches, and
//! never calls the erroring readers: every decode goes through a `Result`
//! and becomes a finding. The relation is locked in `ShareLock` mode (or
//! `AccessShareLock` during recovery) so no fold, merge or VACUUM moves
//! pages underneath the walk, while readers proceed as usual.
//!
//! Checks, in order:
//!
//! 1. the meta page: page kind, layout version, tokenizer spec, directory
//!    entries (run shapes, page tables present, generations unique and below
//!    the next one), pending-free entries and the buffer state;
//! 2. every directory run and its page table: page kinds, chain length, byte
//!    counts, the page table listing exactly the chain's blocks;
//! 3. every segment blob through [`segment::verify::verify_segment`], plus
//!    its directory entry's document count and total length;
//! 4. every dead list: decodes, sorted, a subset of the document table;
//! 5. the write buffer chain and stream: page kinds, full pages before the
//!    tail, tail state matching the byte count, records framed, one record
//!    per counted document;
//! 6. pending-free runs: chains of run pages, nothing already FREE;
//! 7. page accounting: no page referenced twice, no referenced page marked
//!    FREE, no live document indexed in two places, and (as warnings) pages
//!    referenced by nothing;
//! 8. with `heap_check`: every live location points at a heap line pointer
//!    that exists, and every visible heap row with indexable tokens is in
//!    the index.

use std::collections::HashMap;
use std::ffi::c_void;

use pgrx::{FromDatum, pg_guard, pg_sys};
use segment::Tid;
use segment::verify::{Finding, Findings, verify_dead_list, verify_forward_stream, verify_segment};

use super::layout::{
    self, BufferState, CHAIN_CAPACITY, KIND_BUFFER, KIND_FREE, KIND_META, KIND_RUN, Meta, NONE, Run,
};
use super::{Buffer, blocks, decode_page_table, tid_of, tokenizer_for, tokens_of};

/// One row of `stannum.verify_index`.
pub struct Row {
    pub severity: String,
    pub location: String,
    pub message: String,
}

impl From<Finding> for Row {
    fn from(finding: Finding) -> Self {
        Self {
            severity: finding.severity.as_str().to_owned(),
            location: finding.location,
            message: finding.message,
        }
    }
}

fn kind_name(kind: u8) -> String {
    match kind {
        KIND_META => "meta".to_owned(),
        KIND_BUFFER => "buffer".to_owned(),
        KIND_RUN => "run".to_owned(),
        KIND_FREE => "FREE".to_owned(),
        other => format!("unknown kind {other}"),
    }
}

fn describe(tid: Tid) -> String {
    format!("({},{})", tid.block, tid.offset)
}

/// How many pages a run of `bytes` bytes occupies when written by `write_run`.
fn pages_for(bytes: u32) -> u32 {
    bytes.div_ceil(CHAIN_CAPACITY as u32).max(1)
}

/// A page read for the walk: its kind, chain link and data.
struct PageData {
    kind: u8,
    next: u32,
    data: Vec<u8>,
}

struct Checker {
    index: pg_sys::Relation,
    findings: Findings,
    nblocks: u32,
    /// Who references each page, by block number.
    owners: Vec<Option<String>>,
    /// Live locations with the label of the source holding them.
    live: Vec<(Tid, usize)>,
    labels: Vec<String>,
}

impl Checker {
    fn error(&mut self, location: impl Into<String>, message: impl std::fmt::Display) {
        self.findings.error(location, message);
    }

    fn warning(&mut self, location: impl Into<String>, message: impl std::fmt::Display) {
        self.findings.warning(location, message);
    }

    /// Reads and validates one page on behalf of `owner`, recording the
    /// reference. `None` means a finding was recorded and the page is
    /// unusable for the caller.
    unsafe fn page(&mut self, block: u32, owner: &str) -> Option<PageData> {
        if block >= self.nblocks {
            self.error(
                owner.to_owned(),
                format!(
                    "references page {block} beyond the end of the index ({} pages)",
                    self.nblocks
                ),
            );
            return None;
        }
        if let Some(previous) = &self.owners[block as usize] {
            let previous = previous.clone();
            if previous != owner {
                self.error(
                    owner.to_owned(),
                    format!("page {block} is also referenced by {previous}"),
                );
            } else {
                self.error(
                    owner.to_owned(),
                    format!("chain loops back to page {block}"),
                );
            }
            return None;
        }
        self.owners[block as usize] = Some(owner.to_owned());
        let buffer = unsafe { Buffer::read(self.index, block, false) };
        let page = buffer.page();
        let kind = match layout::kind(page) {
            Ok(kind) => kind,
            Err(message) => {
                self.error(owner.to_owned(), format!("page {block}: {message}"));
                return None;
            }
        };
        if kind == KIND_FREE {
            self.error(
                owner.to_owned(),
                format!("page {block} is marked FREE but still referenced"),
            );
            return None;
        }
        if kind == KIND_META {
            if block != 0 {
                self.error(owner.to_owned(), format!("page {block} is a meta page"));
                return None;
            }
            return Some(PageData {
                kind,
                next: NONE,
                data: layout::payload(page).to_vec(),
            });
        }
        match layout::chain(page) {
            Ok((next, data)) => Some(PageData {
                kind,
                next,
                data: data.to_vec(),
            }),
            Err(message) => {
                self.error(owner.to_owned(), format!("page {block}: {message}"));
                None
            }
        }
    }

    /// Walks a run on behalf of `owner`. Directory runs (`exact`) must have
    /// every page but the last full, exactly `run.bytes` bytes and no link
    /// past the last page; pending runs are only required to be chains of
    /// run pages. Returns the bytes and the blocks visited, or `None` when
    /// the bytes could not be assembled.
    unsafe fn run(&mut self, run: Run, owner: &str, exact: bool) -> Option<(Vec<u8>, Vec<u32>)> {
        if run.is_empty() {
            self.error(owner.to_owned(), "run is empty");
            return None;
        }
        if exact && run.blocks != pages_for(run.bytes) {
            self.error(
                owner.to_owned(),
                format!(
                    "directory entry says {} pages for {} bytes; {} pages expected",
                    run.blocks,
                    run.bytes,
                    pages_for(run.bytes)
                ),
            );
        }
        let mut out = Vec::with_capacity(run.bytes as usize);
        let mut visited = Vec::with_capacity(run.blocks as usize);
        let mut block = run.first;
        let mut complete = true;
        for i in 0..run.blocks {
            pgrx::check_for_interrupts!();
            if block == NONE {
                let message = format!("chain ends after {i} of {} pages", run.blocks);
                if exact {
                    self.error(owner.to_owned(), message);
                } else {
                    self.warning(owner.to_owned(), format!("{message}; the rest leaks"));
                }
                complete = false;
                break;
            }
            let Some(page) = (unsafe { self.page(block, owner) }) else {
                complete = false;
                break;
            };
            visited.push(block);
            if page.kind != KIND_RUN {
                self.error(
                    owner.to_owned(),
                    format!(
                        "page {block} has kind {} instead of run",
                        kind_name(page.kind)
                    ),
                );
                complete = false;
                break;
            }
            if exact {
                let expected = if i + 1 < run.blocks {
                    CHAIN_CAPACITY
                } else {
                    run.bytes as usize - (i as usize) * CHAIN_CAPACITY
                };
                if page.data.len() != expected {
                    self.error(
                        owner.to_owned(),
                        format!(
                            "page {block} holds {} bytes; {expected} expected",
                            page.data.len()
                        ),
                    );
                    complete = false;
                }
            }
            let take = (run.bytes as usize)
                .saturating_sub(out.len())
                .min(page.data.len());
            out.extend_from_slice(&page.data[..take]);
            block = page.next;
        }
        if exact && complete && block != NONE {
            self.warning(
                owner.to_owned(),
                format!("last page links on to page {block}, which no reader follows"),
            );
        }
        if out.len() != run.bytes as usize {
            if complete {
                self.error(
                    owner.to_owned(),
                    format!("{} of {} bytes readable", out.len(), run.bytes),
                );
            }
            return None;
        }
        complete.then_some((out, visited))
    }

    unsafe fn check_segment(&mut self, position: usize, entry: &layout::SegmentEntry) {
        let label = format!("segment generation {}", entry.generation);
        let run_owner = format!("{label} run");
        let table_owner = format!("{label} page table");
        let dead_owner = format!("{label} dead list");
        let source = self.labels.len();
        self.labels.push(label.clone());

        // The page table must list exactly the run's chain.
        let chain = unsafe { self.run(entry.run, &run_owner, true) };
        if entry.map.is_empty() {
            self.error(label.clone(), "directory entry has no page table");
        } else if let Some((bytes, _)) = unsafe { self.run(entry.map, &table_owner, true) } {
            let table = decode_page_table(&bytes);
            if bytes.len() % 4 != 0 {
                self.error(
                    table_owner.clone(),
                    format!("{} bytes is not a whole number of entries", bytes.len()),
                );
            }
            if table.len() != entry.run.blocks as usize {
                self.error(
                    table_owner.clone(),
                    format!(
                        "lists {} pages for a run of {}",
                        table.len(),
                        entry.run.blocks
                    ),
                );
            } else if let Some((_, visited)) = &chain
                && let Some(at) = table.iter().zip(visited).position(|(t, v)| t != v)
            {
                self.error(
                    table_owner.clone(),
                    format!(
                        "entry {at} is page {} but the chain visits page {}",
                        table[at], visited[at]
                    ),
                );
            }
        }

        let documents = match &chain {
            Some((bytes, _)) => {
                let report = verify_segment(bytes);
                for finding in report.findings {
                    self.findings.push(finding.within(&label));
                }
                if let Some(doc_count) = report.doc_count
                    && doc_count != entry.docs
                {
                    self.error(
                        label.clone(),
                        format!(
                            "directory entry {position} says {} documents but the segment holds {doc_count}",
                            entry.docs
                        ),
                    );
                }
                if let Some(total_length) = report.total_length
                    && total_length != entry.total_length
                {
                    self.error(
                        label.clone(),
                        format!(
                            "directory entry {position} says total length {} but the segment says {total_length}",
                            entry.total_length
                        ),
                    );
                }
                report.doc_count.map(|_| report.documents)
            }
            None => None,
        };

        let mut dead: Vec<Tid> = Vec::new();
        if !entry.dead.is_empty()
            && let Some((bytes, _)) = unsafe { self.run(entry.dead, &dead_owner, true) }
        {
            let decoded = segment::ordinals::Ordinals::parse(&bytes).and_then(|o| o.to_vec());
            match (&documents, decoded) {
                (Some(documents), decoded) => {
                    for finding in verify_dead_list(&bytes, documents.len() as u32) {
                        self.findings.push(finding.within(&label));
                    }
                    dead = decoded
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|ordinal| documents.get(ordinal as usize).copied())
                        .collect();
                }
                // Without a document table the ordinals name nothing.
                (None, Ok(_)) => {}
                (None, Err(error)) => self.error(dead_owner.clone(), error),
            }
        }
        if let Some(documents) = documents {
            for tid in documents {
                if dead.binary_search(&tid).is_err() {
                    self.live.push((tid, source));
                }
            }
        }
    }

    unsafe fn check_buffer(&mut self, state: &BufferState) {
        let owner = "write buffer";
        if state.head == NONE {
            self.error(owner, "buffer state has no head page");
            return;
        }
        let live_pages = pages_for(state.bytes) as usize;
        let mut stream = Vec::with_capacity(state.bytes as usize);
        let mut block = state.head;
        let mut pages = Vec::new();
        let mut complete = true;
        // Live pages first, then whatever stale pages still hang off the chain.
        let mut i = 0usize;
        while block != NONE && i < self.nblocks as usize {
            pgrx::check_for_interrupts!();
            let Some(page) = (unsafe { self.page(block, owner) }) else {
                complete = false;
                break;
            };
            pages.push(block);
            if page.kind != KIND_BUFFER {
                self.error(
                    owner,
                    format!(
                        "page {block} has kind {} instead of buffer",
                        kind_name(page.kind)
                    ),
                );
                complete = false;
                break;
            }
            if i < live_pages {
                let needed = (state.bytes as usize).saturating_sub(i * CHAIN_CAPACITY);
                if i + 1 < live_pages && page.data.len() != CHAIN_CAPACITY {
                    self.error(
                        owner,
                        format!(
                            "page {block} holds {} bytes but the buffer continues past it",
                            page.data.len()
                        ),
                    );
                    complete = false;
                } else if page.data.len() < needed.min(CHAIN_CAPACITY) {
                    self.error(
                        owner,
                        format!(
                            "page {block} holds {} bytes; {} needed",
                            page.data.len(),
                            needed.min(CHAIN_CAPACITY)
                        ),
                    );
                    complete = false;
                }
                let take = needed.min(page.data.len());
                stream.extend_from_slice(&page.data[..take]);
            }
            block = page.next;
            i += 1;
        }
        if pages.len() < live_pages && block == NONE {
            self.error(
                owner,
                format!(
                    "chain ends after {} pages but {} bytes need {live_pages}",
                    pages.len(),
                    state.bytes
                ),
            );
            complete = false;
        }
        if complete {
            let tail = pages[live_pages - 1];
            let tail_used = state.bytes - (live_pages as u32 - 1) * CHAIN_CAPACITY as u32;
            if state.tail != tail || state.tail_used != tail_used {
                self.error(
                    owner,
                    format!(
                        "buffer state says tail page {} with {} bytes used but {} bytes end on page {tail} at byte {tail_used}",
                        state.tail, state.tail_used, state.bytes
                    ),
                );
            }
        }
        if stream.len() != state.bytes as usize {
            return;
        }
        let report = verify_forward_stream(&stream);
        for finding in report.findings {
            self.findings.push(finding.within(owner));
        }
        if report.records != state.docs {
            self.error(
                owner,
                format!(
                    "buffer state says {} documents but the stream holds {}",
                    state.docs, report.records
                ),
            );
        }
        let source = self.labels.len();
        self.labels.push(owner.to_owned());
        for tid in report.tids {
            self.live.push((tid, source));
        }
    }

    unsafe fn check_meta(&mut self) -> Option<Meta> {
        let page = unsafe { self.page(0, "meta page") }?;
        if page.kind != KIND_META {
            self.error(
                "meta page",
                format!("page 0 has kind {} instead of meta", kind_name(page.kind)),
            );
            return None;
        }
        let meta = match Meta::decode(&page.data) {
            Ok(meta) => meta,
            Err(message) => {
                self.error("meta page", message);
                return None;
            }
        };
        if crate::options::decode_spec(&meta.spec).is_none() {
            self.error(
                "meta page",
                format!("tokenizer spec {:?} is unreadable", meta.spec),
            );
        }
        let mut generations: HashMap<u32, usize> = HashMap::new();
        for (position, entry) in meta.segments.iter().enumerate() {
            let location = format!("directory entry {position}");
            if entry.generation >= meta.next_generation {
                self.error(
                    location.clone(),
                    format!(
                        "generation {} is not below the next generation {}",
                        entry.generation, meta.next_generation
                    ),
                );
            }
            if let Some(other) = generations.insert(entry.generation, position) {
                self.error(
                    location.clone(),
                    format!(
                        "generation {} is also used by directory entry {other}",
                        entry.generation
                    ),
                );
            }
            if entry.docs == 0 {
                self.warning(location.clone(), "segment holds no documents");
            }
        }
        for (position, pending) in meta.pending.iter().enumerate() {
            let location = format!("pending entry {position}");
            if pending.run.is_empty() || pending.run.blocks == 0 {
                self.warning(location.clone(), "entry holds no pages");
            }
            if pending.xid == 0 {
                self.warning(location, "entry has no transaction id");
            }
        }
        let buffer = &meta.buffer;
        if buffer.docs > 0 && buffer.bytes == 0 {
            self.error(
                "meta page",
                format!(
                    "buffer state counts {} documents in zero bytes",
                    buffer.docs
                ),
            );
        }
        if buffer.docs == 0 && buffer.bytes > 0 {
            self.error(
                "meta page",
                format!(
                    "buffer state holds {} bytes but counts no documents",
                    buffer.bytes
                ),
            );
        }
        Some(meta)
    }

    unsafe fn check_pending(&mut self, meta: &Meta) {
        for (position, pending) in meta.pending.iter().enumerate() {
            if pending.run.is_empty() || pending.run.blocks == 0 {
                continue;
            }
            let owner = format!("pending entry {position}");
            unsafe { self.run(pending.run, &owner, false) };
        }
    }

    /// Pages nothing references: FREE is expected; anything else leaked.
    unsafe fn check_unreferenced(&mut self) {
        for block in 0..self.nblocks {
            pgrx::check_for_interrupts!();
            if self.owners[block as usize].is_some() {
                continue;
            }
            let buffer = unsafe { Buffer::read(self.index, block, false) };
            match layout::kind(buffer.page()) {
                Ok(KIND_FREE) => {}
                Ok(kind) => self.warning(
                    format!("page {block}"),
                    format!(
                        "{} page referenced by nothing; VACUUM reclaims it",
                        kind_name(kind)
                    ),
                ),
                Err(message) => self.warning(
                    format!("page {block}"),
                    format!("unreferenced and unreadable: {message}"),
                ),
            }
        }
    }

    /// Sorts the live locations and reports any indexed in two places.
    fn check_duplicates(&mut self) {
        self.live.sort_unstable();
        let mut duplicates = Vec::new();
        for pair in self.live.windows(2) {
            if pair[0].0 == pair[1].0 {
                duplicates.push((pair[0].0, pair[0].1, pair[1].1));
            }
        }
        for (tid, a, b) in duplicates {
            self.error(
                self.labels[b].clone(),
                format!(
                    "document {} is also live in {}",
                    describe(tid),
                    self.labels[a]
                ),
            );
        }
        self.live.dedup_by_key(|(tid, _)| *tid);
    }

    /// Every live location must point at a heap line pointer that exists.
    unsafe fn check_heap_pointers(&mut self, heap: pg_sys::Relation) {
        let heap_blocks = unsafe { blocks(heap) };
        let mut current: Option<(u32, pg_sys::Buffer, u16)> = None;
        let mut problems = 0usize;
        for i in 0..self.live.len() {
            pgrx::check_for_interrupts!();
            let (tid, source) = self.live[i];
            let label = self.labels[source].clone();
            if tid.block >= heap_blocks {
                self.error(
                    label,
                    format!(
                        "document {} points beyond the heap ({heap_blocks} pages)",
                        describe(tid)
                    ),
                );
                problems += 1;
                continue;
            }
            if current.is_none_or(|(block, _, _)| block != tid.block) {
                if let Some((_, buffer, _)) = current.take() {
                    unsafe { pg_sys::UnlockReleaseBuffer(buffer) };
                }
                let buffer = unsafe {
                    let buffer = pg_sys::ReadBuffer(heap, tid.block);
                    pg_sys::LockBuffer(buffer, pg_sys::BUFFER_LOCK_SHARE as i32);
                    buffer
                };
                let max = unsafe { pg_sys::PageGetMaxOffsetNumber(pg_sys::BufferGetPage(buffer)) };
                current = Some((tid.block, buffer, max));
            }
            let (_, buffer, max) = current.expect("page pinned above");
            if tid.offset > max {
                self.error(
                    label,
                    format!(
                        "document {} points past the last line pointer ({max}) of its heap page",
                        describe(tid)
                    ),
                );
                problems += 1;
                continue;
            }
            let flags = unsafe {
                let item = pg_sys::PageGetItemId(pg_sys::BufferGetPage(buffer), tid.offset);
                (*item).lp_flags()
            };
            if flags == pg_sys::LP_UNUSED {
                self.error(
                    label,
                    format!(
                        "document {} points at an unused heap line pointer",
                        describe(tid)
                    ),
                );
                problems += 1;
            }
            if problems > segment::verify::MAX_FINDINGS {
                break;
            }
        }
        if let Some((_, buffer, _)) = current.take() {
            unsafe { pg_sys::UnlockReleaseBuffer(buffer) };
        }
    }
}

/// State of the heap scan: the sorted live locations and what was missing.
struct HeapScan {
    live: Vec<Tid>,
    tokenizer: std::rc::Rc<tokenizer::CompiledTokenizerPipeline>,
    missing: Vec<Tid>,
    rows: u64,
}

#[pg_guard]
unsafe extern "C-unwind" fn heap_callback(
    _index: pg_sys::Relation,
    tid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    _tuple_is_alive: bool,
    state: *mut c_void,
) {
    unsafe {
        let state = &mut *state.cast::<HeapScan>();
        state.rows += 1;
        if *isnull {
            return;
        }
        let text = String::from_datum(*values, false).expect("non-null indexed text");
        if tokens_of(&state.tokenizer, &text).is_empty() {
            // Empty documents are dropped at fold time, so their presence
            // in the index is not required.
            return;
        }
        let tid = tid_of(*tid);
        if state.live.binary_search(&tid).is_err()
            && state.missing.len() <= segment::verify::MAX_FINDINGS
        {
            state.missing.push(tid);
        }
    }
}

/// Every visible heap row with indexable tokens must be in the index.
unsafe fn check_heap_rows(checker: &mut Checker, heap: pg_sys::Relation, meta: &Meta) {
    let Some(_) = crate::options::decode_spec(&meta.spec) else {
        return;
    };
    let mut state = HeapScan {
        live: checker.live.iter().map(|(tid, _)| *tid).collect(),
        tokenizer: tokenizer_for(&meta.spec),
        missing: Vec::new(),
        rows: 0,
    };
    unsafe {
        let info = pg_sys::BuildIndexInfo(checker.index);
        (*info).ii_Concurrent = true;
        (*info).ii_Unique = false;
        (*info).ii_ExclusionOps = std::ptr::null_mut();
        (*info).ii_ExclusionProcs = std::ptr::null_mut();
        (*info).ii_ExclusionStrats = std::ptr::null_mut();
        let snapshot = pg_sys::RegisterSnapshot(pg_sys::GetTransactionSnapshot());
        let scan =
            pg_sys::table_beginscan_strat(heap, snapshot, 0, std::ptr::null_mut(), true, false);
        // The build scan ends `scan` itself, as amcheck relies on; only the
        // snapshot is ours to release.
        pg_sys::table_index_build_scan(
            heap,
            checker.index,
            info,
            true,
            false,
            Some(heap_callback),
            (&mut state as *mut HeapScan).cast(),
            scan,
        );
        pg_sys::UnregisterSnapshot(snapshot);
    }
    for tid in state.missing {
        checker.error(
            "heap",
            format!("visible row {} is not in the index", describe(tid)),
        );
    }
}

/// Walks the index and returns every finding; an empty result means clean.
///
/// # Safety
/// `index` is an open Stannum index relation the caller holds a lock on for
/// the duration of the call.
pub unsafe fn verify(index: pg_sys::Relation, heap_check: bool) -> Vec<Row> {
    let index_oid = unsafe { (*index).rd_id };
    let heap_oid = unsafe { pg_sys::IndexGetRelation(index_oid, false) };
    let lockmode = if unsafe { pg_sys::RecoveryInProgress() } {
        pg_sys::AccessShareLock
    } else {
        pg_sys::ShareLock
    } as pg_sys::LOCKMODE;
    // Heap before index, the order every other path takes.
    let heap = unsafe { pg_sys::table_open(heap_oid, lockmode) };
    unsafe { pg_sys::LockRelationOid(index_oid, lockmode) };
    let nblocks = unsafe { blocks(index) };
    let mut checker = Checker {
        index,
        findings: Findings::default(),
        nblocks,
        owners: vec![None; nblocks as usize],
        live: Vec::new(),
        labels: Vec::new(),
    };
    if nblocks > 0 {
        unsafe {
            if let Some(meta) = checker.check_meta() {
                for (position, entry) in meta.segments.iter().enumerate() {
                    pgrx::check_for_interrupts!();
                    checker.check_segment(position, entry);
                }
                checker.check_buffer(&meta.buffer);
                checker.check_pending(&meta);
                checker.check_unreferenced();
                checker.check_duplicates();
                if heap_check {
                    checker.check_heap_pointers(heap);
                    check_heap_rows(&mut checker, heap, &meta);
                }
            }
        }
    }
    unsafe { pg_sys::table_close(heap, pg_sys::NoLock as pg_sys::LOCKMODE) };
    checker
        .findings
        .finish()
        .into_iter()
        .map(Row::from)
        .collect()
}

/// The pages of a chain starting at `first`: up to `limit` pages of `kind`,
/// stopping without complaint at the end of the chain, at a page beyond the
/// index, of another kind or unreadable. Returns the pages and the block the
/// walk stopped at: `NONE` when the chain ended, otherwise the page it could
/// not use. Used without the meta lock, where a chain retired and reclaimed
/// meanwhile is not an error but a reason to check the directory again.
///
/// # Safety
/// `index` is an open index relation.
pub(super) unsafe fn chain_pages(
    index: pg_sys::Relation,
    first: u32,
    limit: u32,
    kind: u8,
) -> (Vec<u32>, u32) {
    let nblocks = unsafe { blocks(index) };
    let mut pages = Vec::new();
    let mut block = first;
    while pages.len() < limit as usize {
        pgrx::check_for_interrupts!();
        if block == NONE || block >= nblocks {
            break;
        }
        let buffer = unsafe { Buffer::read(index, block, false) };
        if layout::kind(buffer.page()) != Ok(kind) {
            break;
        }
        let Ok((next, _)) = layout::chain(buffer.page()) else {
            break;
        };
        pages.push(block);
        block = next;
    }
    (pages, block)
}

/// Every page `meta` references, by the ownership rules of the checker:
/// page 0, each directory entry's run through its page table, the page-table
/// and dead-list chains, the whole write-buffer chain including stale pages
/// past the live bytes, and pending runs up to their recorded lengths, or
/// as far as their chains go. Directory entries and pending runs also present
/// in `already` are skipped, so a caller extending an earlier result under
/// the meta lock only walks what changed. `Err` names a directory chain that
/// could not be followed: corruption, or a race with a retirement when the
/// caller holds no lock; the caller decides which.
///
/// # Safety
/// `index` is an open index relation of at least `nblocks` pages.
pub(super) unsafe fn referenced_pages(
    index: pg_sys::Relation,
    meta: &Meta,
    nblocks: u32,
    already: Option<&Meta>,
) -> Result<Vec<bool>, String> {
    let mut referenced = vec![false; nblocks as usize];
    // Pages past `nblocks` were extended after the caller's capture and are
    // no candidates for anything, but chains written since may run through
    // them; they are read up to the relation's current extent. The checker
    // reports references beyond the actual end of the index.
    let extent = unsafe { blocks(index) };
    let mut mark = |block: u32| {
        if block < nblocks {
            referenced[block as usize] = true;
        }
    };
    mark(0);
    let whole = |run: Run, owner: &str| -> Result<(Vec<u32>, Vec<u8>), String> {
        let mut bytes = Vec::with_capacity(run.bytes as usize);
        let mut pages = Vec::with_capacity(run.blocks as usize);
        let mut block = run.first;
        for i in 0..run.blocks {
            pgrx::check_for_interrupts!();
            if block == NONE || block >= extent {
                return Err(format!(
                    "{owner}: chain ends after {i} of {} pages",
                    run.blocks
                ));
            }
            let buffer = unsafe { Buffer::read(index, block, false) };
            if layout::kind(buffer.page()) != Ok(KIND_RUN) {
                return Err(format!("{owner}: page {block} is not a run page"));
            }
            let (next, data) =
                layout::chain(buffer.page()).map_err(|message| format!("{owner}: {message}"))?;
            let take = (run.bytes as usize)
                .saturating_sub(bytes.len())
                .min(data.len());
            bytes.extend_from_slice(&data[..take]);
            pages.push(block);
            block = next;
        }
        Ok((pages, bytes))
    };
    for entry in &meta.segments {
        if already.is_some_and(|earlier| earlier.segments.contains(entry)) {
            continue;
        }
        let label = format!("segment generation {}", entry.generation);
        if entry.map.is_empty() {
            return Err(format!("{label} has no page table"));
        }
        let (pages, bytes) = whole(entry.map, &format!("{label} page table"))?;
        for block in pages {
            mark(block);
        }
        let table = decode_page_table(&bytes);
        if table.len() != entry.run.blocks as usize {
            return Err(format!(
                "{label} page table lists {} pages for a run of {}",
                table.len(),
                entry.run.blocks
            ));
        }
        for block in table {
            mark(block);
        }
        if !entry.dead.is_empty() {
            let (pages, _) = whole(entry.dead, &format!("{label} dead list"))?;
            for block in pages {
                mark(block);
            }
        }
    }
    let (pages, ended) = unsafe { chain_pages(index, meta.buffer.head, extent, KIND_BUFFER) };
    if ended != NONE {
        return Err(format!(
            "write buffer chain cannot continue at page {ended}"
        ));
    }
    for block in pages {
        mark(block);
    }
    for pending in &meta.pending {
        if already.is_some_and(|earlier| earlier.pending.contains(pending)) {
            continue;
        }
        let (pages, _) =
            unsafe { chain_pages(index, pending.run.first, pending.run.blocks, KIND_RUN) };
        for block in pages {
            mark(block);
        }
    }
    Ok(referenced)
}

/// Overwrites raw bytes of an index page in shared buffers without WAL, so
/// tests can corrupt an index deliberately.
///
/// # Safety
/// `index` is an open index relation; `block` exists.
#[cfg(feature = "pg_test")]
pub unsafe fn corrupt_page(index: pg_sys::Relation, block: u32, at: usize, bytes: &[u8]) {
    use super::layout::PAGE_SIZE;
    if at + bytes.len() > PAGE_SIZE {
        pgrx::error!("corruption does not fit in a page");
    }
    unsafe {
        let buffer = Buffer::read(index, block, true);
        let page =
            std::slice::from_raw_parts_mut(pg_sys::BufferGetPage(buffer.0).cast::<u8>(), PAGE_SIZE);
        page[at..at + bytes.len()].copy_from_slice(bytes);
        pg_sys::MarkBufferDirty(buffer.0);
    }
}

/// The kind of every page of the index, for tests that pick a page to corrupt.
///
/// # Safety
/// `index` is an open index relation.
#[cfg(feature = "pg_test")]
pub unsafe fn page_kinds(index: pg_sys::Relation) -> Vec<(u32, String)> {
    unsafe {
        (0..blocks(index))
            .map(|block| {
                let buffer = Buffer::read(index, block, false);
                let kind = layout::kind(buffer.page()).map_or_else(|m| m.to_owned(), kind_name);
                (block, kind)
            })
            .collect()
    }
}
