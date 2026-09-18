//! LDP2 storage: a write buffer of forward records folded into immutable
//! segments, all in ordinary index pages.
//!
//! Locking: the meta page (block 0) serializes every structural change.
//! Writers hold it exclusively for inserts, folds and publication. VACUUM
//! copies merge inputs under a shared lock and builds the output unlocked.
//! Readers hold it shared while copying the directory and the write buffer,
//! then read immutable segment runs without any lock beyond the per-page
//! content lock. Runs released by a merge or VACUUM wait on the meta page's
//! pending list until their transaction id is older than every snapshot, so
//! a reader holding an old directory never sees a reused page.
//!
//! Lock order: meta page, then buffer or run pages, then the relation
//! extension lock. No operation holds two run pages at once.
//!
//! WAL: every page change goes through the generic WAL API. New runs and
//! their directory entries are published in that order, so a crash between
//! the two leaks unreferenced pages rather than referencing unwritten ones.

pub mod layout;
pub mod verify;

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use layout::{
    BufferState, CHAIN_CAPACITY, KIND_BUFFER, KIND_FREE, KIND_META, KIND_RUN, MAX_PENDING,
    MAX_SEGMENTS, Meta, NONE, PAGE_SIZE, Pending, Run, SPECIAL_SIZE, SegmentEntry,
};
use pgrx::{
    FromDatum, GucContext, GucFlags, GucRegistry, GucSetting, PgLogLevel, PgRelation,
    PgSqlErrorCode, pg_sys,
};
use rustc_hash::FxHashMap;
use segment::Tid;
use segment::dictionary::TermEntry;
use segment::forward::ForwardRecord;
use segment::index::{Expanded, Index, MutableIndex, Window};
use segment::postings::{Postings, PostingsBuilder, PostingsCursor};
use segment::segment::{Lengths, Reader, Term};
use segment::segment::{Segment, SegmentBuilder};
use segment::set::{Cursor, Difference, Intersection};
use tinql::runtime::Query;
use tinql::runtime::plan::{Limits, plan};
use tokenizer::{CompiledTokenizerPipeline, Tokenizer};

/// Encoded forward-record bytes buffered before folding.
static WRITE_BUFFER_BYTES: GucSetting<i32> = GucSetting::<i32>::new(1024 * 1024);
/// Total input documents ordinary insert-side merges may rewrite per fold.
static MAX_MERGE_DOCS: GucSetting<i32> = GucSetting::<i32>::new(1024);
const BITMAP_BATCH: usize = 1024;

/// Documents the write buffer holds before folding into a segment.
static WRITE_BUFFER_DOCS: GucSetting<i32> = GucSetting::<i32>::new(512);
/// Documents an index build accumulates before writing a segment.
static BUILD_SEGMENT_DOCS: GucSetting<i32> = GucSetting::<i32>::new(32_768);
/// Hard bound on directory entries; the tiered policy normally stays well below it.
static MAX_SEGMENTS_GUC: GucSetting<i32> = GucSetting::<i32>::new(MAX_SEGMENTS as i32);
/// Segments a size tier holds before they merge into one segment of the next tier.
static MERGE_TIER_FACTOR: GucSetting<i32> = GucSetting::<i32>::new(8);
/// Smallest `stannum.merge_tier_factor` value; below it every fold would merge.
pub const MIN_MERGE_TIER_FACTOR: i32 = 2;
/// Largest `stannum.merge_tier_factor` value that still keeps tiers meaningful.
pub const MAX_MERGE_TIER_FACTOR: i32 = 64;

/// Registers the tunables. Low values exist so tests can drive folds,
/// merges and reclamation at small scale; the defaults are the intended ones.
pub fn init() {
    GucRegistry::define_int_guc(
        c"stannum.write_buffer_bytes",
        c"Encoded bytes buffered before folding into a Stannum segment",
        c"A fold happens before either the byte or document cap is exceeded. A single oversized document is allowed.",
        &WRITE_BUFFER_BYTES,
        1024,
        64 * 1024 * 1024,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"stannum.max_merge_docs",
        c"Document budget for ordinary merges performed by one inserting backend per fold",
        c"Larger merges wait for VACUUM. Directory overflow forces the minimum number of smallest entries to merge even above this budget; zero defers all ordinary merges.",
        &MAX_MERGE_DOCS,
        0,
        i32::MAX,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"stannum.write_buffer_docs",
        c"Documents buffered before folding into a Stannum segment",
        c"Lower values fold sooner, producing more and smaller segments.",
        &WRITE_BUFFER_DOCS,
        1,
        1_000_000,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"stannum.build_segment_docs",
        c"Documents an index build accumulates per Stannum segment",
        c"Bounds build memory; lower values write more segments.",
        &BUILD_SEGMENT_DOCS,
        1,
        10_000_000,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"stannum.max_segments",
        c"Most Stannum segments an index directory may hold",
        c"Tiered merges keep the count far lower; reaching this bound forces the smallest segments to merge. The on-disk directory holds at most 128 entries.",
        &MAX_SEGMENTS_GUC,
        1,
        MAX_SEGMENTS as i32,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"stannum.merge_tier_factor",
        c"Stannum segments per size tier before they merge",
        c"Segments are tiered by document count in powers of this factor; a tier holding this many segments merges them into one segment of the next tier.",
        &MERGE_TIER_FACTOR,
        MIN_MERGE_TIER_FACTOR,
        MAX_MERGE_TIER_FACTOR,
        GucContext::Userset,
        GucFlags::default(),
    );
}

fn max_segments() -> usize {
    (MAX_SEGMENTS_GUC.get().max(1) as usize).min(MAX_SEGMENTS)
}

fn merge_tier_factor() -> u32 {
    MERGE_TIER_FACTOR
        .get()
        .clamp(MIN_MERGE_TIER_FACTOR, MAX_MERGE_TIER_FACTOR) as u32
}

/// Reports index corruption: what was found and where, with the standard
/// advice. `stannum.verify_index` lists every problem rather than the first.
pub(crate) fn corrupt(message: impl std::fmt::Display) -> ! {
    pg_sys::panic::ErrorReport::new(
        PgSqlErrorCode::ERRCODE_INDEX_CORRUPTED,
        format!("{message}; REINDEX required"),
        "stannum",
    )
    .set_hint("Run SELECT * FROM stannum.verify_index('<index>') to list every problem.")
    .report(PgLogLevel::ERROR);
    unreachable!()
}

/// Page-layout results that carry no location of their own.
fn checked<T>(result: Result<T, &'static str>) -> T {
    result.unwrap_or_else(|message| corrupt(format!("Stannum index: {message}")))
}

/// Codec results from a source the caller cannot name more precisely.
fn codec<T>(result: segment::Result<T>) -> T {
    result.unwrap_or_else(|error| corrupt(format!("Stannum index data: {error}")))
}

/// Codec results from a named source, such as `segment generation 7`.
pub(crate) fn codec_in<T>(result: segment::Result<T>, what: &str) -> T {
    result.unwrap_or_else(|error| corrupt(format!("Stannum {what}: {error}")))
}

/// A directory entry's name in messages.
fn generation_label(generation: u32) -> String {
    format!("segment generation {generation}")
}

/// Owns a buffer pin and content lock; page borrows cannot outlive this guard.
struct Buffer(pg_sys::Buffer);

impl Buffer {
    /// # Safety
    /// `index` is a live index relation; `block` is an existing block.
    unsafe fn read(index: pg_sys::Relation, block: u32, exclusive: bool) -> Self {
        unsafe {
            let buffer = pg_sys::ReadBuffer(index, block);
            pg_sys::LockBuffer(
                buffer,
                if exclusive {
                    pg_sys::BUFFER_LOCK_EXCLUSIVE
                } else {
                    pg_sys::BUFFER_LOCK_SHARE
                } as i32,
            );
            Self(buffer)
        }
    }

    /// Takes a free page recorded in the FSM, or extends the relation.
    unsafe fn allocate(index: pg_sys::Relation) -> Self {
        unsafe {
            loop {
                let block = pg_sys::GetFreeIndexPage(index);
                if block == pg_sys::InvalidBlockNumber {
                    break;
                }
                let buffer = Self::read(index, block, true);
                if layout::kind(buffer.page()) == Ok(KIND_FREE) {
                    return buffer;
                }
                // The FSM is only a hint; never overwrite a page still in use.
            }
            pg_sys::LockRelationForExtension(index, pg_sys::ExclusiveLock as i32);
            let buffer = pg_sys::ReadBuffer(index, pg_sys::InvalidBlockNumber); // P_NEW
            pg_sys::LockBuffer(buffer, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
            pg_sys::UnlockRelationForExtension(index, pg_sys::ExclusiveLock as i32);
            Self(buffer)
        }
    }

    fn block(&self) -> u32 {
        unsafe { pg_sys::BufferGetBlockNumber(self.0) }
    }

    fn page(&self) -> &[u8] {
        // SAFETY: the guard pins this BLCKSZ allocation and holds its content lock.
        unsafe { std::slice::from_raw_parts(pg_sys::BufferGetPage(self.0).cast(), PAGE_SIZE) }
    }

    /// Validated kind of this page.
    fn kind(&self) -> u8 {
        layout::kind(self.page()).unwrap_or_else(|message| {
            corrupt(format!("Stannum index page {}: {message}", self.block()))
        })
    }

    /// Link and data of this chained page.
    fn chain(&self) -> (u32, &[u8]) {
        layout::chain(self.page()).unwrap_or_else(|message| {
            corrupt(format!("Stannum index page {}: {message}", self.block()))
        })
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe { pg_sys::UnlockReleaseBuffer(self.0) };
    }
}

/// Rewrites a page's payload through a generic WAL record.
///
/// # Safety
/// `buffer` holds an exclusive content lock on a page of `index`.
unsafe fn write_page(
    index: pg_sys::Relation,
    buffer: &Buffer,
    initialize: bool,
    kind: u8,
    payload: &[u8],
) {
    unsafe {
        if !is_permanent(index) {
            // Temporary relations use local buffers; unlogged main forks use
            // ordinary shared buffers. Neither needs WAL. Prepare the complete
            // image before entering the no-error critical section.
            let mut image = [0u64; PAGE_SIZE / std::mem::size_of::<u64>()];
            let raw = image.as_mut_ptr().cast::<std::ffi::c_char>();
            std::ptr::copy_nonoverlapping(pg_sys::BufferGetPage(buffer.0), raw, PAGE_SIZE);
            if initialize {
                pg_sys::PageInit(raw, PAGE_SIZE, SPECIAL_SIZE);
            }
            checked(layout::write(
                std::slice::from_raw_parts_mut(raw.cast(), PAGE_SIZE),
                kind,
                payload,
            ));
            pg_sys::CritSectionCount += 1;
            std::ptr::copy_nonoverlapping(raw, pg_sys::BufferGetPage(buffer.0), PAGE_SIZE);
            pg_sys::MarkBufferDirty(buffer.0);
            pg_sys::CritSectionCount -= 1;
            return;
        }
        let wal = pg_sys::GenericXLogStart(index);
        let raw = pg_sys::GenericXLogRegisterBuffer(
            wal,
            buffer.0,
            if initialize {
                pg_sys::GENERIC_XLOG_FULL_IMAGE as i32
            } else {
                0
            },
        );
        if initialize {
            pg_sys::PageInit(raw, PAGE_SIZE, SPECIAL_SIZE);
        }
        let page = std::slice::from_raw_parts_mut(raw.cast::<u8>(), PAGE_SIZE);
        checked(layout::write(page, kind, payload));
        pg_sys::GenericXLogFinish(wal);
    }
}

unsafe fn blocks(index: pg_sys::Relation) -> u32 {
    unsafe { pg_sys::RelationGetNumberOfBlocksInFork(index, pg_sys::ForkNumber::MAIN_FORKNUM) }
}

fn is_permanent(index: pg_sys::Relation) -> bool {
    unsafe { (*(*index).rd_rel).relpersistence.to_ne_bytes()[0] == b'p' }
}

/// Legacy zero-page indexes keep the reference path until REINDEX.
///
/// # Safety
/// `index` is a live index relation held open by the caller.
pub unsafe fn present(index: pg_sys::Relation) -> bool {
    unsafe {
        // A partitioned index is a catalog entry without storage.
        if (*(*index).rd_rel).relkind.to_ne_bytes()[0] != b'i' || blocks(index) == 0 {
            return false;
        }
        let meta = Buffer::read(index, 0, false);
        let kind = meta.kind();
        if kind != KIND_META {
            corrupt(format!(
                "Stannum index page 0 has kind {kind} instead of a meta page (unsupported format?)"
            ));
        }
        true
    }
}

unsafe fn read_meta(index: pg_sys::Relation, exclusive: bool) -> (Buffer, Meta) {
    unsafe {
        let buffer = Buffer::read(index, 0, exclusive);
        let kind = buffer.kind();
        if kind != KIND_META {
            corrupt(format!(
                "Stannum index page 0 has kind {kind} instead of a meta page (unsupported format?)"
            ));
        }
        let meta = Meta::decode(layout::payload(buffer.page()))
            .unwrap_or_else(|message| corrupt(format!("Stannum index meta page: {message}")));
        (buffer, meta)
    }
}

unsafe fn write_meta(index: pg_sys::Relation, buffer: &Buffer, meta: &Meta) {
    unsafe { write_page(index, buffer, false, KIND_META, &checked(meta.encode())) }
}

// --- Tokenizers ---------------------------------------------------------------

thread_local! {
    static TOKENIZERS: RefCell<HashMap<[u8; crate::options::SPEC_BYTES], Rc<CompiledTokenizerPipeline>>> =
        RefCell::new(HashMap::new());
}

/// The tokenizer an index was built with, compiled once per backend.
pub fn tokenizer_for(spec: &[u8; crate::options::SPEC_BYTES]) -> Rc<CompiledTokenizerPipeline> {
    TOKENIZERS.with_borrow_mut(|cache| {
        cache
            .entry(*spec)
            .or_insert_with(|| {
                let spec = crate::options::decode_spec(spec).unwrap_or_else(|| {
                    corrupt("Stannum index meta page: tokenizer settings are unreadable")
                });
                Rc::new(spec.compile().expect("decoded spec validated"))
            })
            .clone()
    })
}

/// # Safety
/// `index` is a live LDP2 index relation.
pub unsafe fn index_tokenizer(index: pg_sys::Relation) -> Rc<CompiledTokenizerPipeline> {
    let (_, meta) = unsafe { read_meta(index, false) };
    tokenizer_for(&meta.spec)
}

/// The tokenizer settings an index analyzes text with: the meta page's copy
/// for a segmented index, otherwise (a partitioned, temporary, unlogged or
/// legacy index, which has no LDP2 storage) its reloptions.
///
/// # Safety
/// `index` is a live index relation held open by the caller.
pub unsafe fn index_spec(index: pg_sys::Relation) -> [u8; crate::options::SPEC_BYTES] {
    unsafe {
        if present(index) {
            read_meta(index, false).1.spec
        } else {
            crate::options::encode_spec(&crate::options::tokenizer_spec(index))
        }
    }
}

thread_local! {
    /// Per-index tokenizer settings, keyed by index OID and validated
    /// against the relation's file number, which every rebuild changes.
    static SPECS: RefCell<HashMap<u32, (u32, [u8; crate::options::SPEC_BYTES])>> =
        RefCell::new(HashMap::new());
}

/// [`index_spec`] of the index with this OID, memoized per backend.
///
/// # Safety
/// `index_oid` names an index relation that the caller may open.
pub unsafe fn spec_by_oid(index_oid: pg_sys::Oid) -> [u8; crate::options::SPEC_BYTES] {
    unsafe {
        let index = pg_sys::index_open(index_oid, pg_sys::AccessShareLock as _);
        let file = (*index).rd_locator.relNumber.to_u32();
        let cached = SPECS.with_borrow(|specs| specs.get(&index_oid.to_u32()).copied());
        let spec = match cached {
            Some((at, spec)) if at == file => spec,
            _ => {
                let spec = index_spec(index);
                // Reloptions of an index without storage can change in place.
                if present(index) {
                    SPECS.with_borrow_mut(|specs| specs.insert(index_oid.to_u32(), (file, spec)));
                }
                spec
            }
        };
        pg_sys::index_close(index, pg_sys::AccessShareLock as _);
        spec
    }
}

/// The compiled tokenizer of the index with this OID, memoized per backend.
///
/// # Safety
/// `index_oid` names an index relation that the caller may open.
pub unsafe fn tokenizer_by_oid(index_oid: pg_sys::Oid) -> Rc<CompiledTokenizerPipeline> {
    tokenizer_for(&unsafe { spec_by_oid(index_oid) })
}

fn tokens_of(tokenizer: &CompiledTokenizerPipeline, text: &str) -> Vec<(String, u32)> {
    tokenizer
        .tokenize(text)
        .map(|token| (token.text.into_owned(), token.pos))
        .collect()
}

fn tid_of(pointer: pg_sys::ItemPointerData) -> Tid {
    let block = (u32::from(pointer.ip_blkid.bi_hi) << 16) | u32::from(pointer.ip_blkid.bi_lo);
    Tid::new(block, pointer.ip_posid)
        .unwrap_or_else(|_| pgrx::error!("invalid heap tuple location"))
}

fn pointer_of(tid: Tid) -> pg_sys::ItemPointerData {
    pg_sys::ItemPointerData {
        ip_blkid: pg_sys::BlockIdData {
            bi_hi: (tid.block >> 16) as u16,
            bi_lo: tid.block as u16,
        },
        ip_posid: tid.offset,
    }
}

// --- Runs ---------------------------------------------------------------------

/// Reads a whole run into memory. `what` names the run in error messages,
/// such as `segment generation 7 dead list`.
unsafe fn read_run(index: pg_sys::Relation, run: Run, what: &str) -> Vec<u8> {
    unsafe {
        let mut out = Vec::with_capacity(run.bytes as usize);
        let mut block = run.first;
        for i in 0..run.blocks {
            pgrx::check_for_interrupts!();
            if block == NONE {
                corrupt(format!(
                    "Stannum {what}: chain ends after {i} of {} pages",
                    run.blocks
                ));
            }
            let buffer = Buffer::read(index, block, false);
            expect_run_page(&buffer, what);
            let (next, data) = buffer.chain();
            let take = (run.bytes as usize - out.len()).min(data.len());
            out.extend_from_slice(&data[..take]);
            block = next;
        }
        if out.len() != run.bytes as usize {
            corrupt(format!(
                "Stannum {what}: {} of {} bytes readable",
                out.len(),
                run.bytes
            ));
        }
        out
    }
}

/// Fails unless the page is a run page of `what`.
fn expect_run_page(buffer: &Buffer, what: &str) {
    let kind = buffer.kind();
    if kind != KIND_RUN {
        corrupt(format!(
            "Stannum {what}: page {} has kind {kind} instead of a run page",
            buffer.block()
        ));
    }
}

/// Writes a blob as a new chain of run pages, last page first so each page
/// can carry its successor's block number.
unsafe fn write_run(index: pg_sys::Relation, data: &[u8]) -> Run {
    unsafe { write_run_with_map(index, data).0 }
}

/// Writes a run and returns its block numbers in order.
unsafe fn write_run_with_map(index: pg_sys::Relation, data: &[u8]) -> (Run, Vec<u32>) {
    unsafe {
        let chunks: Vec<&[u8]> = if data.is_empty() {
            vec![&[][..]]
        } else {
            data.chunks(CHAIN_CAPACITY).collect()
        };
        let mut next = NONE;
        let mut blocks = Vec::with_capacity(chunks.len());
        for chunk in chunks.iter().rev() {
            pgrx::check_for_interrupts!();
            let buffer = Buffer::allocate(index);
            write_page(
                index,
                &buffer,
                true,
                KIND_RUN,
                &layout::chain_payload(next, chunk),
            );
            next = buffer.block();
            blocks.push(next);
        }
        blocks.reverse();
        (
            Run {
                first: next,
                blocks: chunks.len() as u32,
                bytes: data.len() as u32,
            },
            blocks,
        )
    }
}

/// Writes a segment run and its page table; returns both runs.
unsafe fn write_segment_run(index: pg_sys::Relation, data: &[u8]) -> (Run, Run) {
    unsafe {
        let (run, blocks) = write_run_with_map(index, data);
        let mut table = Vec::with_capacity(blocks.len() * 4);
        for block in blocks {
            table.extend_from_slice(&block.to_le_bytes());
        }
        (run, write_run(index, &table))
    }
}

fn decode_page_table(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// Page tables by (index identity, segment generation).
type PageTables = HashMap<(u64, u32), Rc<Vec<u32>>>;

thread_local! {
    static PAGE_TABLES: RefCell<PageTables> = RefCell::new(HashMap::new());
}

/// A segment run's page table, cached per backend by index identity and
/// generation. Generations never repeat within an identity.
unsafe fn page_table(index: pg_sys::Relation, identity: u64, entry: &SegmentEntry) -> Rc<Vec<u32>> {
    let key = (identity, entry.generation);
    if let Some(table) = PAGE_TABLES.with_borrow(|tables| tables.get(&key).cloned()) {
        return table;
    }
    let label = generation_label(entry.generation);
    let table = Rc::new(decode_page_table(&unsafe {
        read_run(index, entry.map, &format!("{label} page table"))
    }));
    if table.len() != entry.run.blocks as usize {
        corrupt(format!(
            "Stannum {label}: page table lists {} pages for a run of {}",
            table.len(),
            entry.run.blocks
        ));
    }
    PAGE_TABLES.with_borrow_mut(|tables| {
        if tables.len() > 4096 {
            tables.clear();
        }
        tables.insert(key, table.clone());
    });
    table
}

/// Serves byte ranges of a segment run, one buffer pin per page touched and
/// copying only the bytes asked for. The reader above memoizes extents, so a
/// range is fetched once per backend for as long as the reader is cached.
///
/// The relation is looked up per read rather than held open, because a
/// reader is cached across statements and a relcache reference cannot
/// outlive the statement that took it.
pub struct RunSource {
    index_oid: pg_sys::Oid,
    run: Run,
    table: Rc<Vec<u32>>,
    /// The run's name in error messages.
    label: String,
}

impl segment::source::Source for RunSource {
    fn len(&self) -> u64 {
        u64::from(self.run.bytes)
    }

    fn read(&self, offset: u64, len: usize) -> segment::Result<Vec<u8>> {
        let end = offset
            .checked_add(len as u64)
            .filter(|end| *end <= u64::from(self.run.bytes))
            .ok_or(segment::Error::Truncated)?;
        let mut out = Vec::with_capacity(len);
        // SAFETY: the transaction still holds the lock the planner or scan
        // took on the index; the relcache reference is scoped to this read.
        unsafe {
            let index = pg_sys::RelationIdGetRelation(self.index_oid);
            if index.is_null() {
                pgrx::error!("Stannum index no longer exists");
            }
            let mut at = offset;
            while at < end {
                let page = (at / CHAIN_CAPACITY as u64) as usize;
                let within = (at % CHAIN_CAPACITY as u64) as usize;
                let block = match self.table.get(page) {
                    Some(block) => *block,
                    None => {
                        pg_sys::RelationClose(index);
                        return Err(segment::Error::Truncated);
                    }
                };
                let buffer = Buffer::read(index, block, false);
                expect_run_page(&buffer, &self.label);
                let (_, data) = buffer.chain();
                let take = ((end - at) as usize).min(data.len().saturating_sub(within));
                if take == 0 {
                    pg_sys::RelationClose(index);
                    return Err(segment::Error::Truncated);
                }
                out.extend_from_slice(&data[within..within + take]);
                at += take as u64;
            }
            pg_sys::RelationClose(index);
        }
        Ok(out)
    }
}

/// A reader over a page-backed run, shared between the cache and live views.
type SharedReader = Rc<Reader<Box<dyn segment::source::Source>>>;

/// Dictionary lookups memoized per backend: a term's entry, or its absence.
type TermMemo = Rc<RefCell<FxHashMap<String, Option<TermEntry>>>>;

/// Memoized lookups per segment before the memo is emptied.
const TERM_MEMO_LIMIT: usize = 4096;

/// A segment's dead list, decoded once per backend and dead run.
type DeadSet = Rc<BTreeSet<Tid>>;

/// A segment reader kept per backend with the bytes it has fetched, plus the
/// segment's dead list as of the directory entry it was last checked against.
struct CachedSegment {
    reader: SharedReader,
    /// Dictionary lookups made through this reader. Segments are immutable,
    /// so an answer stays right for as long as the generation exists.
    terms: TermMemo,
    dead_run: Run,
    dead: Option<Rc<Vec<u8>>>,
    /// `dead` decoded once per dead run, for scorers that test membership.
    dead_set: DeadSet,
}

/// An immutable segment as a query source: the shared reader plus the
/// backend's memo of its dictionary lookups. A query resolves each of its
/// terms in every segment several times (planning, statistics, scoring),
/// and every statement repeats that; walking a prefix-compressed dictionary
/// block each time costs more than the lookups it serves once the directory
/// holds a dozen segments. The memo answers repeats without the walk and
/// hands out the same `Term` the reader would.
struct MemoizedSegment {
    reader: SharedReader,
    terms: TermMemo,
}

impl Index for MemoizedSegment {
    fn document_count(&self) -> u32 {
        self.reader.document_count()
    }

    fn total_length(&self) -> u64 {
        self.reader.total_length()
    }

    fn term(&self, term: &str) -> segment::Result<Option<Term<'_>>> {
        if let Some(entry) = self.terms.borrow().get(term) {
            return entry.map(|entry| self.reader.resolve(entry)).transpose();
        }
        let found = self.reader.term(term)?;
        let mut memo = self.terms.borrow_mut();
        if memo.len() >= TERM_MEMO_LIMIT {
            memo.clear();
        }
        memo.insert(term.to_owned(), found.map(|found| found.entry));
        Ok(found)
    }

    fn expand(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
        limit: usize,
    ) -> segment::Result<Expanded<'_>> {
        Index::expand(&*self.reader, window, filter, limit)
    }

    fn documents(&self) -> segment::Result<PostingsCursor<'_>> {
        self.reader.documents()
    }

    fn lengths(&self) -> Lengths<'_> {
        self.reader.lengths()
    }
}

/// Cached readers by (index identity, segment generation).
type SegmentReaders = HashMap<(u64, u32), CachedSegment>;

/// Fetched bytes across cached readers before the cache is emptied.
const READER_CACHE_BYTES: usize = 64 * 1024 * 1024;

thread_local! {
    static SEGMENT_READERS: RefCell<SegmentReaders> = RefCell::new(HashMap::new());
}

/// The cached reader for a directory entry, created on first use. Readers
/// are immutable like their segments; only the dead list can change.
unsafe fn cached_segment(
    index: pg_sys::Relation,
    index_oid: pg_sys::Oid,
    identity: u64,
    entry: &SegmentEntry,
) -> (MemoizedSegment, Option<Rc<Vec<u8>>>, DeadSet) {
    let key = (identity, entry.generation);
    let found = SEGMENT_READERS.with_borrow(|readers| {
        readers.get(&key).map(|cached| {
            (
                MemoizedSegment {
                    reader: cached.reader.clone(),
                    terms: cached.terms.clone(),
                },
                (cached.dead_run == entry.dead)
                    .then(|| (cached.dead.clone(), cached.dead_set.clone())),
            )
        })
    });
    let (segment, dead) = match found {
        Some((segment, Some((dead, dead_set)))) => return (segment, dead, dead_set),
        Some((segment, None)) => (segment, None),
        None => {
            let label = generation_label(entry.generation);
            let source: Box<dyn segment::source::Source> = Box::new(RunSource {
                index_oid,
                run: entry.run,
                table: unsafe { page_table(index, identity, entry) },
                label: label.clone(),
            });
            let segment = MemoizedSegment {
                reader: Rc::new(codec_in(Reader::new(source), &label)),
                terms: Rc::default(),
            };
            (segment, None)
        }
    };
    let dead = dead.unwrap_or_else(|| {
        (!entry.dead.is_empty()).then(|| {
            Rc::new(unsafe {
                read_run(
                    index,
                    entry.dead,
                    &format!("{} dead list", generation_label(entry.generation)),
                )
            })
        })
    });
    let dead_set = Rc::new(match &dead {
        Some(bytes) => codec_in(
            Postings::parse(bytes).and_then(|p| p.to_vec()),
            &format!("{} dead list", generation_label(entry.generation)),
        )
        .into_iter()
        .collect(),
        None => BTreeSet::new(),
    });
    SEGMENT_READERS.with_borrow_mut(|readers| {
        readers.insert(
            key,
            CachedSegment {
                reader: segment.reader.clone(),
                terms: segment.terms.clone(),
                dead_run: entry.dead,
                dead: dead.clone(),
                dead_set: dead_set.clone(),
            },
        );
    });
    (segment, dead, dead_set)
}

/// Drops cached readers for segments no longer in the directory, and every
/// reader once the fetched bytes exceed the budget. Live views keep their
/// own references, so dropping here only releases what nothing else holds.
fn trim_reader_cache(identity: u64, meta: &Meta) {
    SEGMENT_READERS.with_borrow_mut(|readers| {
        readers.retain(|(id, generation), _| {
            *id != identity || meta.segments.iter().any(|e| e.generation == *generation)
        });
        let bytes: usize = readers.values().map(|c| c.reader.cached_bytes()).sum();
        if bytes > READER_CACHE_BYTES {
            readers.clear();
        }
    });
}

/// Queues a run for reclamation once no scan can still hold it.
///
/// Runs released at the same transaction horizon share one pending entry:
/// the new run's last page is linked ahead of the entry's chain. No reader
/// follows that link, because every reader stops at its own run's block
/// count or uses the page table, so the pages stay valid for old directories.
/// A full list is first drained of runs no snapshot can still read, and
/// otherwise the run joins the newest entry, so the list never overflows and
/// no page is leaked; reclamation of that entry just waits for the newer xid.
///
/// # Safety
/// The caller holds the meta page of `index` exclusively.
unsafe fn release(index: pg_sys::Relation, meta: &mut Meta, run: Run) {
    if run.is_empty() {
        return;
    }
    let xid = unsafe { pg_sys::ReadNextTransactionId() }.into_inner();
    if meta.pending.len() >= MAX_PENDING {
        unsafe { drain_pending(index, meta) };
    }
    let full = meta.pending.len() >= MAX_PENDING;
    match meta.pending.last_mut() {
        Some(last) if full || last.xid == xid => {
            unsafe { prepend_chain(index, run, last.run.first) };
            last.run = Run {
                first: run.first,
                blocks: last.run.blocks + run.blocks,
                bytes: last.run.bytes.saturating_add(run.bytes),
            };
            last.xid = xid;
        }
        _ => meta.pending.push(Pending { run, xid }),
    }
}

/// Points the last page of `run` at `next`, joining two chains of pages that
/// only the reclamation walk will ever follow across.
///
/// # Safety
/// The caller holds the meta page of `index` exclusively; `run` is a run of
/// `index` that no directory references any more.
unsafe fn prepend_chain(index: pg_sys::Relation, run: Run, next: u32) {
    unsafe {
        let what = format!("released run at page {}", run.first);
        let mut block = run.first;
        for i in 1..run.blocks {
            pgrx::check_for_interrupts!();
            let buffer = Buffer::read(index, block, false);
            expect_run_page(&buffer, &what);
            let (following, _) = buffer.chain();
            if following == NONE {
                corrupt(format!(
                    "Stannum {what}: chain ends after {i} of {} pages",
                    run.blocks
                ));
            }
            block = following;
        }
        let last = Buffer::read(index, block, true);
        expect_run_page(&last, &what);
        let (_, data) = last.chain();
        let payload = layout::chain_payload(next, data);
        write_page(index, &last, false, KIND_RUN, &payload);
    }
}

/// Marks the pages of every pending run that no snapshot can still read as
/// free and records them in the FSM; the rest stay on the list.
///
/// # Safety
/// The caller holds the meta page of `index` exclusively.
unsafe fn drain_pending(index: pg_sys::Relation, meta: &mut Meta) {
    unsafe {
        let mut still_pending = Vec::new();
        for pending in std::mem::take(&mut meta.pending) {
            let xid = pg_sys::TransactionId::from(pending.xid);
            if !pg_sys::GlobalVisCheckRemovableXid(index, xid) {
                still_pending.push(pending);
                continue;
            }
            let mut block = pending.run.first;
            for _ in 0..pending.run.blocks {
                pgrx::check_for_interrupts!();
                if block == NONE {
                    break;
                }
                let buffer = Buffer::read(index, block, true);
                if buffer.kind() != KIND_RUN {
                    break;
                }
                let (next, _) = buffer.chain();
                write_page(index, &buffer, false, KIND_FREE, &pending.xid.to_le_bytes());
                let freed = buffer.block();
                drop(buffer);
                pg_sys::RecordFreeIndexPage(index, freed);
                block = next;
            }
        }
        meta.pending = still_pending;
    }
}

// --- Write buffer index -------------------------------------------------------

/// The write buffer as an in-memory index that grows with the buffer. Appends
/// only extend the stream, so the index absorbs the bytes past `covered` on
/// each use; a fold or VACUUM rewrite starts a new epoch and a new index.
/// Block numbers of buffer pages are remembered so the tail is reached
/// without walking the chain from its head.
struct BufferIndex {
    identity: u64,
    epoch: u32,
    covered: usize,
    pages: Vec<u32>,
    index: Rc<MutableIndex>,
}

thread_local! {
    static BUFFER_INDEX: RefCell<Option<BufferIndex>> = const { RefCell::new(None) };
}

/// Reads buffer bytes `[from, to)` using and extending the page map.
unsafe fn read_buffer_range(
    index: pg_sys::Relation,
    pages: &mut Vec<u32>,
    from: usize,
    to: usize,
) -> Vec<u8> {
    unsafe {
        let mut out = Vec::with_capacity(to - from);
        let mut at = from;
        while at < to {
            pgrx::check_for_interrupts!();
            let page = at / CHAIN_CAPACITY;
            while pages.len() <= page {
                // Follow the chain from the last known page to discover the next.
                let last = *pages.last().expect("head is always known");
                let buffer = Buffer::read(index, last, false);
                let (next, _) = buffer.chain();
                if next == NONE {
                    corrupt(format!(
                        "Stannum write buffer: chain ends at page {last} before byte {to}"
                    ));
                }
                pages.push(next);
            }
            let buffer = Buffer::read(index, pages[page], false);
            expect_buffer_page(&buffer);
            let (_, data) = buffer.chain();
            let within = at % CHAIN_CAPACITY;
            let take = (to - at).min(data.len().saturating_sub(within));
            if take == 0 {
                corrupt(format!(
                    "Stannum write buffer: page {} holds {} bytes but byte {at} is expected on it",
                    pages[page],
                    data.len()
                ));
            }
            out.extend_from_slice(&data[within..within + take]);
            at += take;
        }
        out
    }
}

/// The buffer's index for the current state, extended with any records
/// appended since it was last used.
unsafe fn buffer_index(
    index: pg_sys::Relation,
    identity: u64,
    state: &BufferState,
) -> Rc<MutableIndex> {
    let cache = BUFFER_INDEX.with_borrow_mut(Option::take);
    let mut entry = match cache {
        Some(entry)
            if entry.identity == identity
                && entry.epoch == state.epoch
                && entry.covered <= state.bytes as usize =>
        {
            entry
        }
        _ => BufferIndex {
            identity,
            epoch: state.epoch,
            covered: 0,
            pages: vec![state.head],
            index: Rc::new(MutableIndex::default()),
        },
    };
    if entry.covered < state.bytes as usize {
        let tail = unsafe {
            read_buffer_range(index, &mut entry.pages, entry.covered, state.bytes as usize)
        };
        let mut at = 0;
        while at < tail.len() {
            at += codec_in(entry.index.add_encoded(&tail[at..]), "write buffer");
        }
        entry.covered = state.bytes as usize;
    }
    let result = entry.index.clone();
    BUFFER_INDEX.with_borrow_mut(|slot| *slot = Some(entry));
    result
}

/// What this backend's caches hold, for tests.
#[cfg(any(test, feature = "pg_test"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheProbe {
    pub cached_segments: usize,
    /// Memoized dictionary lookups over every cached segment.
    pub memoized_terms: usize,
    /// (index identity, buffer epoch, bytes covered, documents) of the
    /// cached buffer index, if any.
    pub buffer: Option<(u64, u32, usize, u32)>,
}

#[cfg(any(test, feature = "pg_test"))]
pub fn cache_probe() -> CacheProbe {
    let (cached_segments, memoized_terms) = SEGMENT_READERS.with_borrow(|readers| {
        (
            readers.len(),
            readers.values().map(|c| c.terms.borrow().len()).sum(),
        )
    });
    let buffer = BUFFER_INDEX.with_borrow(|slot| {
        slot.as_ref().map(|entry| {
            (
                entry.identity,
                entry.epoch,
                entry.covered,
                entry.index.document_count(),
            )
        })
    });
    CacheProbe {
        cached_segments,
        memoized_terms,
        buffer,
    }
}

unsafe fn dead_set(index: pg_sys::Relation, entry: &SegmentEntry) -> BTreeSet<Tid> {
    if entry.dead.is_empty() {
        return BTreeSet::new();
    }
    let what = format!("{} dead list", generation_label(entry.generation));
    let bytes = unsafe { read_run(index, entry.dead, &what) };
    codec_in(Postings::parse(&bytes).and_then(|p| p.to_vec()), &what)
        .into_iter()
        .collect()
}

fn encode_dead(dead: &BTreeSet<Tid>) -> Vec<u8> {
    let mut builder = PostingsBuilder::default();
    for tid in dead {
        builder
            .push(*tid)
            .expect("set iteration is ordered and unique");
    }
    builder.finish()
}

// --- Write buffer -------------------------------------------------------------

unsafe fn read_buffer_stream(index: pg_sys::Relation, state: &BufferState) -> Vec<u8> {
    unsafe {
        let mut out = Vec::with_capacity(state.bytes as usize);
        let mut block = state.head;
        while out.len() < state.bytes as usize {
            pgrx::check_for_interrupts!();
            if block == NONE {
                corrupt(format!(
                    "Stannum write buffer: chain ends after {} of {} bytes",
                    out.len(),
                    state.bytes
                ));
            }
            let buffer = Buffer::read(index, block, false);
            expect_buffer_page(&buffer);
            let (next, data) = buffer.chain();
            let take = (state.bytes as usize - out.len()).min(data.len());
            if take < data.len() && out.len() + take < state.bytes as usize {
                corrupt(format!(
                    "Stannum write buffer: page {block} holds {} bytes but the buffer continues past it",
                    data.len()
                ));
            }
            out.extend_from_slice(&data[..take]);
            block = next;
        }
        out
    }
}

/// Appends bytes to the write buffer, extending the chain as needed. The
/// caller holds the meta page exclusively and persists `state` afterwards.
unsafe fn append_to_buffer(index: pg_sys::Relation, state: &mut BufferState, mut data: &[u8]) {
    unsafe {
        while !data.is_empty() {
            pgrx::check_for_interrupts!();
            let tail = Buffer::read(index, state.tail, true);
            expect_buffer_page(&tail);
            let (next, existing) = tail.chain();
            let used = state.tail_used as usize;
            if used > existing.len() {
                corrupt(format!(
                    "Stannum write buffer: tail page {} holds {} bytes but {used} are in use",
                    state.tail,
                    existing.len()
                ));
            }
            if used == CHAIN_CAPACITY {
                let next_block = if next == NONE {
                    let fresh = Buffer::allocate(index);
                    write_page(
                        index,
                        &fresh,
                        true,
                        KIND_BUFFER,
                        &layout::chain_payload(NONE, &[]),
                    );
                    let block = fresh.block();
                    drop(fresh);
                    let payload = layout::chain_payload(block, existing);
                    write_page(index, &tail, false, KIND_BUFFER, &payload);
                    block
                } else {
                    next
                };
                state.tail = next_block;
                state.tail_used = 0;
                continue;
            }
            let take = data.len().min(CHAIN_CAPACITY - used);
            let mut merged = Vec::with_capacity(used + take);
            merged.extend_from_slice(&existing[..used]);
            merged.extend_from_slice(&data[..take]);
            let payload = layout::chain_payload(next, &merged);
            write_page(index, &tail, false, KIND_BUFFER, &payload);
            state.tail_used += take as u32;
            state.bytes += take as u32;
            data = &data[take..];
        }
        state.version = state.version.wrapping_add(1);
    }
}

/// Fails unless the page is a write-buffer page.
fn expect_buffer_page(buffer: &Buffer) {
    let kind = buffer.kind();
    if kind != KIND_BUFFER {
        corrupt(format!(
            "Stannum write buffer: page {} has kind {kind} instead of a buffer page",
            buffer.block()
        ));
    }
}

/// Rewrites the write buffer from its head with new contents.
unsafe fn replace_buffer(index: pg_sys::Relation, state: &mut BufferState, data: &[u8], docs: u32) {
    state.tail = state.head;
    state.tail_used = 0;
    state.bytes = 0;
    state.docs = docs;
    state.version = state.version.wrapping_add(1);
    state.epoch = state.epoch.wrapping_add(1);
    unsafe { append_to_buffer(index, state, data) };
}

// --- Segments -----------------------------------------------------------------

fn finish_builder(builder: SegmentBuilder) -> (Vec<u8>, u32, u64) {
    let docs = builder.document_count() as u32;
    let blob = builder.finish();
    let total_length = codec(Segment::parse(&blob)).total_length();
    (blob, docs, total_length)
}

/// A directory entry for a freshly written segment run, with the next
/// generation number. Generations never repeat within an index identity.
fn new_entry(meta: &mut Meta, run: Run, map: Run, docs: u32, total_length: u64) -> SegmentEntry {
    let generation = meta.next_generation;
    meta.next_generation = meta
        .next_generation
        .checked_add(1)
        .unwrap_or_else(|| pgrx::error!("Stannum segment generations exhausted; REINDEX required"));
    SegmentEntry {
        run,
        map,
        dead: Run::EMPTY,
        docs,
        total_length,
        generation,
    }
}

/// Queues every run of a retired directory entry for reclamation.
///
/// # Safety
/// The caller holds the meta page of `index` exclusively.
unsafe fn release_entry(index: pg_sys::Relation, meta: &mut Meta, entry: SegmentEntry) {
    unsafe {
        release(index, meta, entry.run);
        release(index, meta, entry.map);
        release(index, meta, entry.dead);
    }
}

/// Publishes a built segment: writes its run, appends a directory entry and
/// then spends the caller's merge budget and enforces the hard bound, so the directory
/// never leaves this function with more than `stannum.max_segments` entries.
unsafe fn add_segment(
    index: pg_sys::Relation,
    meta: &mut Meta,
    blob: &[u8],
    docs: u32,
    total_length: u64,
    budget: u64,
) {
    unsafe {
        let (run, map) = write_segment_run(index, blob);
        let entry = new_entry(meta, run, map, docs, total_length);
        meta.segments.push(entry);
        maintain(index, meta, budget);
    }
}

/// The size tier of a segment: how many times `factor` divides its document
/// count. Tier `t` holds counts in `[factor^t, factor^(t+1))`.
fn tier(docs: u32, factor: u32) -> u32 {
    let mut remaining = docs.max(1);
    let mut tier = 0;
    while remaining >= factor {
        remaining /= factor;
        tier += 1;
    }
    tier
}

/// The directory positions the merge policy combines next, if any.
///
/// Segments are tiered by document count in powers of `factor`, like the
/// levels of a log-structured merge tree. The lowest tier holding `factor` or
/// more segments merges into one segment that lands in the next tier up, so
/// every document is rewritten about once per tier and a merge touches only
/// a small run of similarly sized segments rather than the whole index. If no
/// tier is due but the directory still exceeds `limit`, the smallest entries
/// merge until it fits. `None` means the directory is in shape.
fn merge_candidates(docs: &[u32], factor: u32, limit: usize) -> Option<Vec<usize>> {
    let mut tiers: HashMap<u32, Vec<usize>> = HashMap::new();
    for (position, count) in docs.iter().enumerate() {
        tiers
            .entry(tier(*count, factor))
            .or_default()
            .push(position);
    }
    if let Some(members) = tiers
        .into_iter()
        .filter(|(_, members)| members.len() >= factor as usize)
        .min_by_key(|(tier, _)| *tier)
        .map(|(_, members)| members)
    {
        return Some(members.into_iter().take(factor as usize).collect());
    }
    if docs.len() > limit {
        let mut by_size: Vec<usize> = (0..docs.len()).collect();
        by_size.sort_by_key(|position| (docs[*position], *position));
        by_size.truncate((docs.len() - limit + 1).max(2));
        return Some(by_size);
    }
    None
}

/// The hard directory bound takes precedence over the work budget. Its
/// emergency merge includes only the smallest `len - limit + 1` entries;
/// normally just two. No fixed document ceiling can also guarantee space in
/// a fixed-size directory when every entry is already larger than that ceiling.
fn bounded_merge_candidates(
    docs: &[u32],
    factor: u32,
    limit: usize,
    budget: u64,
) -> Option<Vec<usize>> {
    if docs.len() > limit {
        let mut positions: Vec<usize> = (0..docs.len()).collect();
        positions.sort_by_key(|p| (docs[*p], *p));
        positions.truncate((docs.len() - limit + 1).max(2));
        return Some(positions);
    }
    let positions = merge_candidates(docs, factor, limit)?;
    let work: u64 = positions.iter().map(|p| u64::from(docs[*p])).sum();
    (work <= budget).then_some(positions)
}

/// Applies ordinary merges within the remaining budget and emergency merges
/// until the directory fits. Excess full tiers wait for VACUUM.
///
/// # Safety
/// The caller holds the meta page of `index` exclusively.
unsafe fn maintain(index: pg_sys::Relation, meta: &mut Meta, mut budget: u64) {
    let factor = merge_tier_factor();
    let limit = max_segments();
    loop {
        pgrx::check_for_interrupts!();
        let docs: Vec<u32> = meta.segments.iter().map(|entry| entry.docs).collect();
        match bounded_merge_candidates(&docs, factor, limit, budget) {
            Some(positions) => {
                let work: u64 = positions.iter().map(|p| u64::from(docs[*p])).sum();
                budget = budget.saturating_sub(work);
                unsafe { merge(index, meta, positions) };
            }
            None => break,
        }
    }
}

/// Rewrites the segments at `positions` into one, dropping dead documents.
/// The merged segment takes a fresh generation at the end of the directory;
/// the old runs go to the pending-free list.
///
/// # Safety
/// The caller holds the meta page of `index` exclusively.
unsafe fn merge(index: pg_sys::Relation, meta: &mut Meta, mut positions: Vec<usize>) {
    unsafe {
        positions.sort_unstable();
        let mut old = Vec::with_capacity(positions.len());
        for position in positions.into_iter().rev() {
            old.push(meta.segments.remove(position));
        }
        old.reverse();
        let mut builder = SegmentBuilder::default();
        for entry in &old {
            pgrx::check_for_interrupts!();
            let label = generation_label(entry.generation);
            let bytes = read_run(index, entry.run, &label);
            let segment = codec_in(Segment::parse(&bytes), &label);
            let dead = dead_set(index, entry);
            for record in codec_in(segment.records(|tid| dead.contains(&tid)), &label) {
                codec_in(builder.add_record(&record), &label);
            }
        }
        let (blob, docs, total_length) = finish_builder(builder);
        let (run, map) = write_segment_run(index, &blob);
        let entry = new_entry(meta, run, map, docs, total_length);
        meta.segments.push(entry);
        for entry in old {
            release_entry(index, meta, entry);
        }
    }
}

/// VACUUM owns maintenance scheduling; no preload library or worker slots
/// are required. Limit each call to the directory size captured on entry so
/// a steady stream of inserts cannot keep VACUUM here forever.
unsafe fn deferred_merges(index: pg_sys::Relation) {
    let attempts = unsafe { read_meta(index, false) }.1.segments.len();
    for _ in 0..attempts {
        pgrx::check_for_interrupts!();
        let (identity, entries, inputs) = unsafe {
            let (_guard, meta) = read_meta(index, false);
            let docs: Vec<u32> = meta.segments.iter().map(|entry| entry.docs).collect();
            let Some(positions) = merge_candidates(&docs, merge_tier_factor(), max_segments())
            else {
                return;
            };
            let entries: Vec<SegmentEntry> = positions.iter().map(|p| meta.segments[*p]).collect();
            let inputs: Vec<_> = entries
                .iter()
                .map(|entry| {
                    (
                        read_run(index, entry.run, &generation_label(entry.generation)),
                        dead_set(index, entry),
                    )
                })
                .collect();
            (meta.identity, entries, inputs)
        };
        // Own all input bytes before dropping the shared meta lock. VACUUM
        // need not hold a snapshot to protect pages during this CPU work.
        let mut builder = SegmentBuilder::default();
        for (entry, (bytes, dead)) in entries.iter().zip(&inputs) {
            pgrx::check_for_interrupts!();
            let label = generation_label(entry.generation);
            let segment = codec_in(Segment::parse(bytes), &label);
            for record in codec_in(segment.records(|tid| dead.contains(&tid)), &label) {
                codec_in(builder.add_record(&record), &label);
            }
        }
        let (blob, docs, total_length) = finish_builder(builder);
        unsafe {
            let (guard, mut meta) = read_meta(index, true);
            // An insert can force an emergency merge while we build. Match
            // complete entries, including dead-list identity, not positions.
            if meta.identity != identity
                || !entries.iter().all(|entry| meta.segments.contains(entry))
            {
                return;
            }
            let (run, map) = write_segment_run(index, &blob);
            meta.segments.retain(|entry| !entries.contains(entry));
            let entry = new_entry(&mut meta, run, map, docs, total_length);
            meta.segments.push(entry);
            for entry in entries {
                release_entry(index, &mut meta, entry);
            }
            write_meta(index, &guard, &meta);
        }
    }
}

/// Folds the write buffer into a new segment and empties it.
unsafe fn fold(index: pg_sys::Relation, meta: &mut Meta) {
    unsafe {
        if meta.buffer.docs == 0 {
            return;
        }
        let stream = read_buffer_stream(index, &meta.buffer);
        let mut builder = SegmentBuilder::default();
        for record in segment::forward::records(&stream) {
            codec_in(
                builder.add_record(&codec_in(record, "write buffer")),
                "write buffer",
            );
        }
        let (blob, docs, total_length) = finish_builder(builder);
        add_segment(
            index,
            meta,
            &blob,
            docs,
            total_length,
            MAX_MERGE_DOCS.get() as u64,
        );
        meta.buffer.tail = meta.buffer.head;
        meta.buffer.tail_used = 0;
        meta.buffer.bytes = 0;
        meta.buffer.docs = 0;
        meta.buffer.version = meta.buffer.version.wrapping_add(1);
        meta.buffer.epoch = meta.buffer.epoch.wrapping_add(1);
    }
}

// --- Build --------------------------------------------------------------------

/// # Safety
/// The caller owns an empty relation locked for index construction.
pub unsafe fn build_empty(index: pg_sys::Relation) {
    unsafe {
        if blocks(index) != 0 {
            pgrx::error!("Stannum index build requires an empty relation");
        }
        let meta_buffer = Buffer::allocate(index);
        let head_buffer = Buffer::allocate(index);
        if meta_buffer.block() != 0 || head_buffer.block() != 1 {
            pgrx::error!("unexpected Stannum index allocation");
        }
        write_page(
            index,
            &head_buffer,
            true,
            KIND_BUFFER,
            &layout::chain_payload(NONE, &[]),
        );
        drop(head_buffer);
        let meta = empty_meta(index);
        write_page(
            index,
            &meta_buffer,
            true,
            KIND_META,
            &checked(meta.encode()),
        );
    }
}

/// A fresh main/init fork always starts with a meta page and buffer head.
unsafe fn empty_meta(index: pg_sys::Relation) -> Meta {
    unsafe {
        let spec = crate::options::tokenizer_spec(index);
        let relnumber = u64::from((*index).rd_locator.relNumber.to_u32());
        let xid = u64::from(pg_sys::ReadNextTransactionId().into_inner());
        Meta {
            identity: (relnumber << 32) | xid,
            spec: crate::options::encode_spec(&spec),
            buffer: BufferState {
                version: 0,
                epoch: 0,
                head: 1,
                tail: 1,
                tail_used: 0,
                bytes: 0,
                docs: 0,
            },
            next_generation: 1,
            segments: Vec::new(),
            pending: Vec::new(),
        }
    }
}

/// Build the crash-reset image for an unlogged index. PostgreSQL has already
/// created the init fork. As with built-in AMs, WAL-log and fsync its contents
/// even though subsequent main-fork mutations are unlogged.
///
/// # Safety
/// The caller owns an unlogged index locked for construction.
pub unsafe fn build_init_fork(index: pg_sys::Relation) {
    unsafe {
        let meta = checked(empty_meta(index).encode());
        let head = layout::chain_payload(NONE, &[]);
        let smgr = pg_sys::RelationGetSmgr(index);
        for (block, kind, payload) in [
            (0, KIND_META, meta.as_slice()),
            (1, KIND_BUFFER, head.as_slice()),
        ] {
            // smgr may use direct I/O; ordinary Rust stack alignment is not
            // sufficient for the server's I/O alignment requirement.
            let raw = pg_sys::palloc_aligned(
                PAGE_SIZE,
                pg_sys::PG_IO_ALIGN_SIZE as usize,
                pg_sys::MCXT_ALLOC_ZERO as i32,
            )
            .cast::<std::ffi::c_char>();
            pg_sys::PageInit(raw, PAGE_SIZE, SPECIAL_SIZE);
            checked(layout::write(
                std::slice::from_raw_parts_mut(raw.cast(), PAGE_SIZE),
                kind,
                payload,
            ));
            pg_sys::PageSetChecksumInplace(raw, block);
            pg_sys::smgrextend(
                smgr,
                pg_sys::ForkNumber::INIT_FORKNUM,
                block,
                raw.cast(),
                true,
            );
            pg_sys::log_newpage(
                &mut (*index).rd_locator,
                pg_sys::ForkNumber::INIT_FORKNUM,
                block,
                raw,
                true,
            );
            pg_sys::pfree(raw.cast());
        }
        pg_sys::smgrimmedsync(smgr, pg_sys::ForkNumber::INIT_FORKNUM);
    }
}

/// Accumulates documents during `ambuild` and writes segments directly,
/// bypassing the write buffer.
pub struct Builder {
    tokenizer: Option<Rc<CompiledTokenizerPipeline>>,
    segment: SegmentBuilder,
}

impl Builder {
    /// # Safety
    /// `index` is a live relation that `build_empty` has initialized.
    pub unsafe fn new(index: pg_sys::Relation) -> Self {
        let tokenizer = unsafe { present(index) }.then(|| unsafe { index_tokenizer(index) });
        Self {
            tokenizer,
            segment: SegmentBuilder::default(),
        }
    }

    /// # Safety
    /// Pointers reference the first indexed datum, its null flag and a valid TID.
    pub unsafe fn add(
        &mut self,
        index: pg_sys::Relation,
        values: *mut pg_sys::Datum,
        isnull: *mut bool,
        tid: pg_sys::ItemPointer,
    ) {
        let Some(tokenizer) = self.tokenizer.clone() else {
            return;
        };
        unsafe {
            if *isnull {
                return;
            }
            let text = String::from_datum(*values, false).expect("non-null indexed text");
            let tokens = tokens_of(&tokenizer, &text);
            codec(self.segment.add_document(
                tid_of(*tid),
                tokens.iter().map(|(term, pos)| (term.as_str(), *pos)),
            ));
            if self.segment.document_count() >= BUILD_SEGMENT_DOCS.get().max(1) as usize {
                self.flush(index);
            }
        }
    }

    unsafe fn flush(&mut self, index: pg_sys::Relation) {
        if self.segment.document_count() == 0 {
            return;
        }
        let builder = std::mem::take(&mut self.segment);
        let (blob, docs, total_length) = finish_builder(builder);
        unsafe {
            let (meta_buffer, mut meta) = read_meta(index, true);
            add_segment(index, &mut meta, &blob, docs, total_length, u64::MAX);
            write_meta(index, &meta_buffer, &meta);
        }
    }

    /// # Safety
    /// `index` is the relation passed to `new`.
    pub unsafe fn finish(mut self, index: pg_sys::Relation) {
        if self.tokenizer.is_some() {
            unsafe { self.flush(index) };
        }
    }
}

// --- Insert -------------------------------------------------------------------

/// # Safety
/// `index` is live and locked for insertion. The pointers reference the first
/// indexed datum/null flag and a valid heap TID for the duration of this call.
pub unsafe fn insert(
    index: pg_sys::Relation,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    tid: pg_sys::ItemPointer,
) {
    unsafe {
        if *isnull || !present(index) {
            return;
        }
        let text = String::from_datum(*values, false).expect("non-null indexed text");
        let (meta_buffer, mut meta) = read_meta(index, true);
        let tokenizer = tokenizer_for(&meta.spec);
        let tokens = tokens_of(&tokenizer, &text);
        let record = codec(ForwardRecord::from_tokens(
            tid_of(*tid),
            tokens.iter().map(|(term, pos)| (term.as_str(), *pos)),
        ));
        let mut bytes = Vec::new();
        codec(record.encode(&mut bytes));
        if meta.buffer.docs > 0
            && (meta.buffer.bytes as usize + bytes.len() > WRITE_BUFFER_BYTES.get() as usize
                || meta.buffer.docs >= WRITE_BUFFER_DOCS.get().max(1) as u32)
        {
            fold(index, &mut meta);
        }
        append_to_buffer(index, &mut meta.buffer, &bytes);
        meta.buffer.docs += 1;
        write_meta(index, &meta_buffer, &meta);
    }
}

// --- Scan ---------------------------------------------------------------------

/// A queryable index (a cached segment reader or the buffer's in-memory
/// index) plus its dead list, if any.
pub type Source = (Box<dyn Index>, Option<Rc<Vec<u8>>>);

/// Everything a scan or a scorer needs from an index, captured under one
/// shared meta lock so the buffer and directory are mutually consistent.
/// Segment pages are fetched on demand through the readers; the view keeps
/// the index relation open for as long as it lives.
pub struct View {
    /// Immutable segments first, then the write buffer as in-memory segments.
    pub sources: Vec<Source>,
    /// How many leading entries of `sources` are immutable segments.
    pub immutable_sources: usize,
    /// A name per source for error messages: `segment generation 7` or
    /// `write buffer`.
    pub labels: Vec<String>,
    /// Each source's dead list as a set, decoded once per backend and dead
    /// run rather than once per statement; empty for the write buffer.
    pub dead_sets: Vec<DeadSet>,
}

/// # Safety
/// `index_oid` names a live LDP2 index the caller may open.
pub unsafe fn view(index_oid: pg_sys::Oid) -> View {
    unsafe {
        let relation = PgRelation::with_lock(index_oid, pg_sys::AccessShareLock as _);
        let index = relation.as_ptr();
        let (meta_buffer, meta) = read_meta(index, false);
        let mut sources: Vec<Source> = Vec::with_capacity(meta.segments.len() + 1);
        let mut labels = Vec::with_capacity(meta.segments.len() + 1);
        let mut dead_sets = Vec::with_capacity(meta.segments.len() + 1);
        trim_reader_cache(meta.identity, &meta);
        for entry in &meta.segments {
            pgrx::check_for_interrupts!();
            let (segment, dead, dead_set) = cached_segment(index, index_oid, meta.identity, entry);
            sources.push((Box::new(segment), dead));
            labels.push(generation_label(entry.generation));
            dead_sets.push(dead_set);
        }
        let immutable_sources = sources.len();
        if meta.buffer.docs > 0 {
            let buffer = buffer_index(index, meta.identity, &meta.buffer);
            sources.push((Box::new(buffer), None));
            labels.push("write buffer".to_owned());
            dead_sets.push(Rc::default());
        }
        // Segments are immutable; the buffer index was extended under the
        // shared meta lock, so a fold cannot rewrite pages underneath it.
        drop(meta_buffer);
        drop(relation);
        View {
            sources,
            immutable_sources,
            labels,
            dead_sets,
        }
    }
}

/// Adds every document matching all `queries` to `bitmap`, exact where the
/// plan is exact. Returns the number of candidates added.
///
/// # Safety
/// `index` is a live LDP2 index; `bitmap` is a valid, writable TID bitmap.
pub unsafe fn scan(
    index: pg_sys::Relation,
    queries: &[Query],
    bitmap: *mut pg_sys::TIDBitmap,
) -> i64 {
    unsafe {
        let view = view((*index).rd_id);
        let limits = Limits::default();
        let mut added = 0i64;
        let mut pending: Vec<pg_sys::ItemPointerData> = Vec::with_capacity(BITMAP_BATCH);
        let flush = |pending: &mut Vec<pg_sys::ItemPointerData>, recheck: bool| {
            if !pending.is_empty() {
                pg_sys::tbm_add_tuples(bitmap, pending.as_mut_ptr(), pending.len() as i32, recheck);
                pending.clear();
            }
        };

        for ((segment, dead_bytes), label) in view.sources.iter().zip(&view.labels) {
            pgrx::check_for_interrupts!();
            let mut exact = true;
            let mut cursors: Vec<Box<dyn Cursor>> = Vec::with_capacity(queries.len());
            for query in queries {
                let plan = plan(query, segment, &limits)
                    .unwrap_or_else(|error| pgrx::error!("Stannum query plan: {error}"));
                exact &= plan.exact;
                cursors.push(plan.cursor);
            }
            let mut cursor: Box<dyn Cursor> = if cursors.len() == 1 {
                cursors.pop().expect("one cursor")
            } else {
                Box::new(codec_in(Intersection::new(cursors), label))
            };
            if let Some(dead_bytes) = dead_bytes {
                let dead = codec_in(
                    Postings::parse(dead_bytes).and_then(|p| p.cursor()),
                    &format!("{label} dead list"),
                );
                cursor = Box::new(codec_in(Difference::new(cursor, dead), label));
            }
            while let Some(tid) = cursor.current() {
                pending.push(pointer_of(tid));
                added += 1;
                if pending.len() == BITMAP_BATCH {
                    pgrx::check_for_interrupts!();
                    flush(&mut pending, !exact);
                }
                codec_in(cursor.advance(), label);
            }
            flush(&mut pending, !exact);
        }

        added
    }
}

// --- VACUUM -------------------------------------------------------------------

/// Records dead tuples: per segment as a dead list, and by rewriting the write
/// buffer without them. Returns (live, removed) document counts.
///
/// # Safety
/// `index` is a live LDP2 index locked for VACUUM; the callback and state
/// satisfy PostgreSQL's bulk-delete contract.
pub unsafe fn bulk_delete(
    index: pg_sys::Relation,
    callback: pg_sys::IndexBulkDeleteCallback,
    state: *mut std::ffi::c_void,
) -> (u64, u64) {
    unsafe {
        let callback = callback.expect("VACUUM callback");
        let is_dead = |tid: Tid| callback(&mut pointer_of(tid), state);
        let (meta_buffer, mut meta) = read_meta(index, true);
        let (mut live, mut removed) = (0u64, 0u64);
        for i in 0..meta.segments.len() {
            pgrx::check_for_interrupts!();
            let entry = meta.segments[i];
            let label = generation_label(entry.generation);
            let bytes = read_run(index, entry.run, &label);
            let segment = codec_in(Segment::parse(&bytes), &label);
            let mut dead = dead_set(index, &entry);
            let before = dead.len();
            let mut documents = codec_in(segment.documents(), &label);
            while let Some(tid) = documents.current() {
                if dead.contains(&tid) {
                    // Already dead: nothing to report.
                } else if is_dead(tid) {
                    dead.insert(tid);
                    removed += 1;
                } else {
                    live += 1;
                }
                codec_in(documents.advance(), &label);
            }
            if dead.len() != before {
                let run = write_run(index, &encode_dead(&dead));
                let old = std::mem::replace(&mut meta.segments[i].dead, run);
                release(index, &mut meta, old);
            }
        }
        if meta.buffer.docs > 0 {
            let stream = read_buffer_stream(index, &meta.buffer);
            let mut kept = Vec::with_capacity(stream.len());
            let mut kept_docs = 0u32;
            let mut dropped = false;
            for record in segment::forward::records(&stream) {
                let record = codec_in(record, "write buffer");
                if is_dead(record.tid) {
                    removed += 1;
                    dropped = true;
                } else {
                    live += 1;
                    kept_docs += 1;
                    codec(record.encode(&mut kept));
                }
            }
            if dropped {
                replace_buffer(index, &mut meta.buffer, &kept, kept_docs);
            }
        }
        write_meta(index, &meta_buffer, &meta);
        (live, removed)
    }
}

/// Rewrites segments that are mostly dead and reclaims runs no scan can
/// still reference.
///
/// # Safety
/// `index` is a live LDP2 index locked for VACUUM.
pub unsafe fn cleanup(index: pg_sys::Relation) {
    unsafe {
        deferred_merges(index);
        let (meta_buffer, mut meta) = read_meta(index, true);
        for i in 0..meta.segments.len() {
            pgrx::check_for_interrupts!();
            let entry = meta.segments[i];
            if entry.dead.is_empty() {
                continue;
            }
            let label = generation_label(entry.generation);
            let dead_label = format!("{label} dead list");
            let dead_bytes = read_run(index, entry.dead, &dead_label);
            let dead_count = codec_in(Postings::parse(&dead_bytes), &dead_label).count();
            if u64::from(dead_count) * 2 < u64::from(entry.docs) {
                continue;
            }
            let dead: BTreeSet<Tid> = codec_in(
                Postings::parse(&dead_bytes).and_then(|p| p.to_vec()),
                &dead_label,
            )
            .into_iter()
            .collect();
            let bytes = read_run(index, entry.run, &label);
            let segment = codec_in(Segment::parse(&bytes), &label);
            let mut builder = SegmentBuilder::default();
            for record in codec_in(segment.records(|tid| dead.contains(&tid)), &label) {
                codec_in(builder.add_record(&record), &label);
            }
            let (blob, docs, total_length) = finish_builder(builder);
            let (run, map) = write_segment_run(index, &blob);
            meta.segments[i] = new_entry(&mut meta, run, map, docs, total_length);
            release_entry(index, &mut meta, entry);
        }
        drain_pending(index, &mut meta);
        write_meta(index, &meta_buffer, &meta);
        drop(meta_buffer);
        pg_sys::IndexFreeSpaceMapVacuum(index);
    }
}

/// Whether the planner may use segmented execution for this index.
/// Feedback settings do not prove this snapshot reached the primary before
/// index VACUUM/reclamation. Generic WAL has no removal-conflict record, so
/// recovery snapshots (including those surviving promotion) use heap scoring
/// and scan fallbacks. Keep this shared by the custom-path and score planners.
///
/// # Safety
/// `oid` names an index relation that the caller may open.
pub unsafe fn is_segmented(oid: pg_sys::Oid) -> bool {
    unsafe {
        if pg_sys::RecoveryInProgress()
            || (pg_sys::ActiveSnapshotSet() && (*pg_sys::GetActiveSnapshot()).takenDuringRecovery)
        {
            return false;
        }
        let index = pg_sys::index_open(oid, pg_sys::AccessShareLock as _);
        let segmented = present(index);
        pg_sys::index_close(index, pg_sys::AccessShareLock as _);
        segmented
    }
}

/// One row of `stannum.segment_info`, mirroring TIN's columns.
pub struct SegmentRow {
    pub ordinal: i64,
    pub kind: String,
    pub root_block: i64,
    pub docs: i64,
    pub dead_docs: i64,
    pub sum_doc_lengths: i64,
    pub total_pages: i64,
    pub generation: i64,
}

/// The directory as rows: immutable segments first, then the write buffer.
///
/// # Safety
/// `index` is a live LDP2 index.
pub unsafe fn segment_rows(index: pg_sys::Relation) -> Vec<SegmentRow> {
    unsafe {
        let (_, meta) = read_meta(index, false);
        let mut rows = Vec::with_capacity(meta.segments.len() + 1);
        for (ordinal, entry) in meta.segments.iter().enumerate() {
            let dead = if entry.dead.is_empty() {
                0
            } else {
                let what = format!("{} dead list", generation_label(entry.generation));
                let bytes = read_run(index, entry.dead, &what);
                i64::from(codec_in(Postings::parse(&bytes), &what).count())
            };
            rows.push(SegmentRow {
                ordinal: ordinal as i64,
                kind: "immutable".to_owned(),
                root_block: i64::from(entry.run.first),
                docs: i64::from(entry.docs),
                dead_docs: dead,
                sum_doc_lengths: entry.total_length as i64,
                total_pages: i64::from(entry.run.blocks + entry.dead.blocks),
                generation: i64::from(entry.generation),
            });
        }
        if meta.buffer.docs > 0 {
            let stream = read_buffer_stream(index, &meta.buffer);
            let lengths: u64 = segment::forward::records(&stream)
                .map(|record| u64::from(codec_in(record, "write buffer").doc_len))
                .sum();
            rows.push(SegmentRow {
                ordinal: meta.segments.len() as i64,
                kind: "mutable".to_owned(),
                root_block: i64::from(meta.buffer.head),
                docs: i64::from(meta.buffer.docs),
                dead_docs: 0,
                sum_doc_lengths: lengths as i64,
                total_pages: i64::from(meta.buffer.bytes.div_ceil(CHAIN_CAPACITY as u32).max(1)),
                generation: i64::from(meta.buffer.version),
            });
        }
        rows
    }
}

/// Identifies the contents of an index for planner memoization: a fold, a
/// merge, a rebuild or any write to the buffer changes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    identity: u64,
    buffer_version: u32,
    buffer_epoch: u32,
    next_generation: u32,
}

/// The stamp of the index with this OID, or `None` when it has no LDP2
/// storage to read.
///
/// # Safety
/// `index_oid` names an index relation that the caller may open.
pub unsafe fn stamp(index_oid: pg_sys::Oid) -> Option<Stamp> {
    unsafe {
        let relation = PgRelation::with_lock(index_oid, pg_sys::AccessShareLock as _);
        let index = relation.as_ptr();
        if !present(index) {
            return None;
        }
        let (_, meta) = read_meta(index, false);
        Some(Stamp {
            identity: meta.identity,
            buffer_version: meta.buffer.version,
            buffer_epoch: meta.buffer.epoch,
            next_generation: meta.next_generation,
        })
    }
}

/// Segment and buffer document counts from the directory, for statistics.
///
/// # Safety
/// `index` is a live LDP2 index.
pub unsafe fn document_count(index: pg_sys::Relation) -> u64 {
    let (_, meta) = unsafe { read_meta(index, false) };
    meta.segments
        .iter()
        .map(|entry| u64::from(entry.docs))
        .sum::<u64>()
        + u64::from(meta.buffer.docs)
}

#[cfg(test)]
mod tests {
    use super::{bounded_merge_candidates, merge_candidates, tier};

    #[test]
    fn merge_budget_is_cumulative_and_emergency_work_is_minimal() {
        let docs = [1, 1, 2, 2];
        assert_eq!(bounded_merge_candidates(&docs, 2, 128, 2), Some(vec![0, 1]));
        // That merge spends the entire budget; its output must not cascade.
        assert_eq!(bounded_merge_candidates(&[2, 2, 2], 2, 128, 0), None);
        assert_eq!(bounded_merge_candidates(&[8, 8], 2, 128, 15), None);
        assert_eq!(
            bounded_merge_candidates(&[8, 8], 2, 128, 16),
            Some(vec![0, 1])
        );
        // Overflow overrides a full higher tier and the budget, picking only
        // the two smallest entries even when all entries exceed the budget.
        assert_eq!(
            bounded_merge_candidates(&[80, 80, 10, 20], 2, 3, 0),
            Some(vec![2, 3])
        );
        assert_eq!(
            bounded_merge_candidates(&[u32::MAX, u32::MAX], 2, 128, u64::from(u32::MAX)),
            None
        );
    }

    #[test]
    fn tiers_are_powers_of_the_factor() {
        assert_eq!(tier(0, 8), 0);
        assert_eq!(tier(7, 8), 0);
        assert_eq!(tier(8, 8), 1);
        assert_eq!(tier(63, 8), 1);
        assert_eq!(tier(64, 8), 2);
        assert_eq!(tier(16_384, 8), 4);
        assert_eq!(tier(131_072, 8), 5);
        assert_eq!(tier(u32::MAX, 2), 31);
    }

    #[test]
    fn a_full_tier_merges_before_anything_larger() {
        // Seven folds of one write buffer and two older, larger segments.
        let docs = [
            200_000, 16_384, 16_384, 16_384, 16_384, 16_384, 16_384, 16_384, 40_000,
        ];
        assert_eq!(merge_candidates(&docs, 8, 128), None);
        let docs = [
            200_000, 16_384, 16_384, 16_384, 16_384, 16_384, 16_384, 16_384, 40_000, 16_384,
        ];
        assert_eq!(
            merge_candidates(&docs, 8, 128),
            Some(vec![1, 2, 3, 4, 5, 6, 7, 9])
        );
        // The lowest due tier goes first even when a higher one is also due.
        let docs = [64, 64, 8, 8, 64];
        assert_eq!(merge_candidates(&docs, 2, 128), Some(vec![2, 3]));
    }

    #[test]
    fn an_overfull_directory_merges_its_smallest_entries() {
        // No tier is due, but the directory is over the limit: the smallest
        // entries merge into one so the count drops back to the limit.
        let docs = [500, 9, 70, 1, 3_000];
        assert_eq!(merge_candidates(&docs, 8, 3), Some(vec![3, 1, 2]));
        assert_eq!(merge_candidates(&docs, 8, 4), Some(vec![3, 1]));
        assert_eq!(merge_candidates(&docs, 8, 5), None);
        // A limit of one still merges at least two entries.
        assert_eq!(merge_candidates(&[5, 6], 8, 1), Some(vec![0, 1]));
        assert_eq!(merge_candidates(&[5], 8, 1), None);
        assert_eq!(merge_candidates(&[], 8, 1), None);
    }

    #[test]
    fn repeated_maintenance_keeps_the_directory_logarithmic() {
        // Simulate folds of one document each and count the directory after
        // every fold: it is the base-`factor` digit sum of the total, so
        // 1,000 documents never need more than (factor - 1) * tiers entries.
        for factor in [2u32, 3, 8] {
            let mut docs: Vec<u32> = Vec::new();
            let mut merges = 0usize;
            let mut rewritten = 0u64;
            for total in 1..=1_000u32 {
                docs.push(1);
                while let Some(positions) = merge_candidates(&docs, factor, 128) {
                    let merged: u32 = positions.iter().map(|p| docs[*p]).sum();
                    rewritten += u64::from(merged);
                    merges += 1;
                    let mut positions = positions;
                    positions.sort_unstable();
                    for position in positions.into_iter().rev() {
                        docs.remove(position);
                    }
                    docs.push(merged);
                }
                let mut digits = 0usize;
                let mut rest = total;
                while rest > 0 {
                    digits += (rest % factor) as usize;
                    rest /= factor;
                }
                assert_eq!(docs.len(), digits, "factor {factor}, total {total}");
                assert_eq!(docs.iter().sum::<u32>(), total);
            }
            assert!(merges > 0);
            // Every document is rewritten once per tier it climbs through.
            let tiers = tier(1_000, factor) as u64 + 1;
            assert!(rewritten <= 1_000 * tiers, "factor {factor}: {rewritten}");
        }
    }
}
