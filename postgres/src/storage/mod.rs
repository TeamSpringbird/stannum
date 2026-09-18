//! LDP2 storage: a write buffer of forward records folded into immutable
//! segments, all in ordinary index pages.
//!
//! Locking: the meta page (block 0) serializes every structural change.
//! Writers hold it exclusively for the whole insert, fold, VACUUM or merge.
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

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use layout::{
    BufferState, CHAIN_CAPACITY, KIND_BUFFER, KIND_FREE, KIND_META, KIND_RUN, MAX_PENDING,
    MAX_SEGMENTS, Meta, NONE, PAGE_SIZE, Pending, Run, SPECIAL_SIZE, SegmentEntry,
};
use pgrx::{FromDatum, GucContext, GucFlags, GucRegistry, GucSetting, pg_sys};
use segment::Tid;
use segment::forward::ForwardRecord;
use segment::postings::{Postings, PostingsBuilder};
use segment::segment::{Segment, SegmentBuilder};
use segment::set::{Cursor, Difference, Intersection};
use tinql::runtime::Query;
use tinql::runtime::plan::{Limits, plan};
use tokenizer::{CompiledTokenizerPipeline, Tokenizer};

/// The write buffer folds into a segment past this many bytes.
pub const BUFFER_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Backend-local cache of immutable segment bytes.
const CACHE_BYTES: usize = 64 * 1024 * 1024;
const BITMAP_BATCH: usize = 1024;

/// Documents the write buffer holds before folding into a segment.
static WRITE_BUFFER_DOCS: GucSetting<i32> = GucSetting::<i32>::new(16_384);
/// Documents an index build accumulates before writing a segment.
static BUILD_SEGMENT_DOCS: GucSetting<i32> = GucSetting::<i32>::new(32_768);
/// Segments allowed before a fold merges everything into one.
static MAX_SEGMENTS_GUC: GucSetting<i32> = GucSetting::<i32>::new(MAX_SEGMENTS as i32);

/// Registers the tunables. Low values exist so tests can drive folds,
/// merges and reclamation at small scale; the defaults are the intended ones.
pub fn init() {
    GucRegistry::define_int_guc(
        c"tin.write_buffer_docs",
        c"Documents buffered before folding into a Lead segment",
        c"Lower values fold sooner, producing more and smaller segments.",
        &WRITE_BUFFER_DOCS,
        1,
        1_000_000,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"tin.build_segment_docs",
        c"Documents an index build accumulates per Lead segment",
        c"Bounds build memory; lower values write more segments.",
        &BUILD_SEGMENT_DOCS,
        1,
        10_000_000,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"tin.max_segments",
        c"Lead segments allowed before a fold merges them",
        c"The on-disk directory holds at most 128 entries.",
        &MAX_SEGMENTS_GUC,
        1,
        MAX_SEGMENTS as i32,
        GucContext::Userset,
        GucFlags::default(),
    );
}

fn max_segments() -> usize {
    (MAX_SEGMENTS_GUC.get().max(1) as usize).min(MAX_SEGMENTS)
}

fn checked<T>(result: Result<T, &'static str>) -> T {
    result.unwrap_or_else(|message| pgrx::error!("{message}; REINDEX required"))
}

fn codec<T>(result: segment::Result<T>) -> T {
    result.unwrap_or_else(|error| pgrx::error!("Lead index data: {error}; REINDEX required"))
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
        checked(layout::kind(self.page()))
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
    unsafe { (*(*index).rd_rel).relpersistence as u8 == b'p' }
}

/// Zero-page (unlogged, temporary, or legacy) indexes keep the reference path.
///
/// # Safety
/// `index` is a live index relation held open by the caller.
pub unsafe fn present(index: pg_sys::Relation) -> bool {
    unsafe {
        if blocks(index) == 0 {
            return false;
        }
        let meta = Buffer::read(index, 0, false);
        if meta.kind() != KIND_META {
            pgrx::error!("Lead index has an unsupported format; REINDEX required");
        }
        true
    }
}

unsafe fn read_meta(index: pg_sys::Relation, exclusive: bool) -> (Buffer, Meta) {
    unsafe {
        let buffer = Buffer::read(index, 0, exclusive);
        if buffer.kind() != KIND_META {
            pgrx::error!("Lead index has an unsupported format; REINDEX required");
        }
        let meta = checked(Meta::decode(layout::payload(buffer.page())));
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
                    pgrx::error!("Lead index tokenizer settings are unreadable; REINDEX required")
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

/// Reads a whole run into memory.
unsafe fn read_run(index: pg_sys::Relation, run: Run) -> Vec<u8> {
    unsafe {
        let mut out = Vec::with_capacity(run.bytes as usize);
        let mut block = run.first;
        for _ in 0..run.blocks {
            pgrx::check_for_interrupts!();
            if block == NONE {
                pgrx::error!("Lead run ends early; REINDEX required");
            }
            let buffer = Buffer::read(index, block, false);
            if buffer.kind() != KIND_RUN {
                pgrx::error!("Lead run page has the wrong kind; REINDEX required");
            }
            let (next, data) = checked(layout::chain(buffer.page()));
            let take = (run.bytes as usize - out.len()).min(data.len());
            out.extend_from_slice(&data[..take]);
            block = next;
        }
        if out.len() != run.bytes as usize {
            pgrx::error!("Lead run is shorter than its directory entry; REINDEX required");
        }
        out
    }
}

/// Writes a blob as a new chain of run pages, last page first so each page
/// can carry its successor's block number.
unsafe fn write_run(index: pg_sys::Relation, data: &[u8]) -> Run {
    unsafe {
        let chunks: Vec<&[u8]> = if data.is_empty() {
            vec![&[][..]]
        } else {
            data.chunks(CHAIN_CAPACITY).collect()
        };
        let mut next = NONE;
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
        }
        Run {
            first: next,
            blocks: chunks.len() as u32,
            bytes: data.len() as u32,
        }
    }
}

/// Queues a run for reclamation once no scan can still hold it.
fn release(meta: &mut Meta, run: Run) {
    if run.is_empty() {
        return;
    }
    if meta.pending.len() >= MAX_PENDING {
        // Leak rather than block; VACUUM will drain the list over time.
        pgrx::warning!(
            "Lead index pending-free list is full; {} pages leaked until REINDEX",
            run.blocks
        );
        return;
    }
    meta.pending.push(Pending {
        run,
        xid: unsafe { pg_sys::ReadNextTransactionId() }.into_inner(),
    });
}

// --- Segment cache ------------------------------------------------------------

struct Cache {
    total: usize,
    entries: HashMap<(u64, u32, u32), Rc<Vec<u8>>>,
}

thread_local! {
    static SEGMENTS: RefCell<Cache> = RefCell::new(Cache { total: 0, entries: HashMap::new() });
}

fn cached(key: (u64, u32, u32), load: impl FnOnce() -> Vec<u8>) -> Rc<Vec<u8>> {
    if let Some(bytes) = SEGMENTS.with_borrow(|cache| cache.entries.get(&key).cloned()) {
        return bytes;
    }
    let bytes = Rc::new(load());
    SEGMENTS.with_borrow_mut(|cache| {
        if cache.total + bytes.len() > CACHE_BYTES {
            cache.entries.clear();
            cache.total = 0;
        }
        cache.total += bytes.len();
        cache.entries.insert(key, bytes.clone());
    });
    bytes
}

unsafe fn cached_segment(
    index: pg_sys::Relation,
    identity: u64,
    entry: &SegmentEntry,
) -> Rc<Vec<u8>> {
    cached((identity, entry.run.first, entry.generation), || unsafe {
        read_run(index, entry.run)
    })
}

/// The write buffer as an in-memory segment, rebuilt only when it changes.
fn cached_buffer_segment(identity: u64, version: u32, stream: &[u8]) -> Rc<Vec<u8>> {
    cached((identity, NONE, version), || {
        let mut builder = SegmentBuilder::default();
        for record in segment::forward::records(stream) {
            codec(builder.add_record(&codec(record)));
        }
        builder.finish()
    })
}

unsafe fn dead_set(index: pg_sys::Relation, entry: &SegmentEntry) -> BTreeSet<Tid> {
    if entry.dead.is_empty() {
        return BTreeSet::new();
    }
    let bytes = unsafe { read_run(index, entry.dead) };
    codec(Postings::parse(&bytes).and_then(|p| p.to_vec()))
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
                pgrx::error!("Lead write buffer ends early; REINDEX required");
            }
            let buffer = Buffer::read(index, block, false);
            if buffer.kind() != KIND_BUFFER {
                pgrx::error!("Lead write buffer page has the wrong kind; REINDEX required");
            }
            let (next, data) = checked(layout::chain(buffer.page()));
            let take = (state.bytes as usize - out.len()).min(data.len());
            if take < data.len() && out.len() + take < state.bytes as usize {
                pgrx::error!("Lead write buffer page is short; REINDEX required");
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
            if tail.kind() != KIND_BUFFER {
                pgrx::error!("Lead write buffer page has the wrong kind; REINDEX required");
            }
            let (next, existing) = checked(layout::chain(tail.page()));
            let used = state.tail_used as usize;
            if used > existing.len() {
                pgrx::error!("Lead write buffer page is short; REINDEX required");
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

/// Rewrites the write buffer from its head with new contents.
unsafe fn replace_buffer(index: pg_sys::Relation, state: &mut BufferState, data: &[u8], docs: u32) {
    state.tail = state.head;
    state.tail_used = 0;
    state.bytes = 0;
    state.docs = docs;
    state.version = state.version.wrapping_add(1);
    unsafe { append_to_buffer(index, state, data) };
}

// --- Segments -----------------------------------------------------------------

/// Publishes a built segment: writes its run and appends a directory entry,
/// merging everything first if the directory is full.
unsafe fn add_segment(
    index: pg_sys::Relation,
    meta: &mut Meta,
    blob: &[u8],
    docs: u32,
    total_length: u64,
) {
    unsafe {
        if meta.segments.len() >= max_segments() {
            merge_all(index, meta);
        }
        let run = write_run(index, blob);
        let generation = meta.next_generation;
        meta.next_generation = meta.next_generation.wrapping_add(1);
        meta.segments.push(SegmentEntry {
            run,
            dead: Run::EMPTY,
            docs,
            total_length,
            generation,
        });
    }
}

fn finish_builder(builder: SegmentBuilder) -> (Vec<u8>, u32, u64) {
    let docs = builder.document_count() as u32;
    let blob = builder.finish();
    let total_length = codec(Segment::parse(&blob)).total_length();
    (blob, docs, total_length)
}

/// Rewrites every segment into one, dropping dead documents.
unsafe fn merge_all(index: pg_sys::Relation, meta: &mut Meta) {
    unsafe {
        let mut builder = SegmentBuilder::default();
        let old = std::mem::take(&mut meta.segments);
        for entry in &old {
            pgrx::check_for_interrupts!();
            let bytes = cached_segment(index, meta.identity, entry);
            let segment = codec(Segment::parse(&bytes));
            let dead = dead_set(index, entry);
            for record in codec(segment.records(|tid| dead.contains(&tid))) {
                codec(builder.add_record(&record));
            }
        }
        let (blob, docs, total_length) = finish_builder(builder);
        let run = write_run(index, &blob);
        let generation = meta.next_generation;
        meta.next_generation = meta.next_generation.wrapping_add(1);
        meta.segments.push(SegmentEntry {
            run,
            dead: Run::EMPTY,
            docs,
            total_length,
            generation,
        });
        for entry in old {
            release(meta, entry.run);
            release(meta, entry.dead);
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
            codec(builder.add_record(&codec(record)));
        }
        let (blob, docs, total_length) = finish_builder(builder);
        add_segment(index, meta, &blob, docs, total_length);
        meta.buffer.tail = meta.buffer.head;
        meta.buffer.tail_used = 0;
        meta.buffer.bytes = 0;
        meta.buffer.docs = 0;
        meta.buffer.version = meta.buffer.version.wrapping_add(1);
    }
}

// --- Build --------------------------------------------------------------------

/// # Safety
/// The caller owns an empty relation locked for index construction.
pub unsafe fn build_empty(index: pg_sys::Relation) {
    unsafe {
        if !is_permanent(index) {
            // The init-fork/unlogged lifecycle remains on the reference fallback.
            return;
        }
        if blocks(index) != 0 {
            pgrx::error!("Lead index build requires an empty relation");
        }
        let meta_buffer = Buffer::allocate(index);
        let head_buffer = Buffer::allocate(index);
        if meta_buffer.block() != 0 || head_buffer.block() != 1 {
            pgrx::error!("unexpected Lead index allocation");
        }
        write_page(
            index,
            &head_buffer,
            true,
            KIND_BUFFER,
            &layout::chain_payload(NONE, &[]),
        );
        drop(head_buffer);
        let spec = crate::options::tokenizer_spec(index);
        let relnumber = u64::from((*index).rd_locator.relNumber.to_u32());
        let xid = u64::from(pg_sys::ReadNextTransactionId().into_inner());
        let meta = Meta {
            identity: (relnumber << 32) | xid,
            spec: crate::options::encode_spec(&spec),
            buffer: BufferState {
                version: 0,
                head: 1,
                tail: 1,
                tail_used: 0,
                bytes: 0,
                docs: 0,
            },
            next_generation: 1,
            segments: Vec::new(),
            pending: Vec::new(),
        };
        write_page(
            index,
            &meta_buffer,
            true,
            KIND_META,
            &checked(meta.encode()),
        );
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
            add_segment(index, &mut meta, &blob, docs, total_length);
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
            && (meta.buffer.bytes as usize + bytes.len() > BUFFER_MAX_BYTES
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

/// One searchable unit: segment bytes and an optional dead list.
type Source = (Rc<Vec<u8>>, Option<Vec<u8>>);

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
        let (meta_buffer, meta) = read_meta(index, false);
        // The buffer is read under the shared meta lock so a fold cannot
        // rewrite it underneath us.
        let stream = read_buffer_stream(index, &meta.buffer);
        drop(meta_buffer);

        let limits = Limits::default();
        let mut added = 0i64;
        let mut pending: Vec<pg_sys::ItemPointerData> = Vec::with_capacity(BITMAP_BATCH);
        let flush = |pending: &mut Vec<pg_sys::ItemPointerData>, recheck: bool| {
            if !pending.is_empty() {
                pg_sys::tbm_add_tuples(bitmap, pending.as_mut_ptr(), pending.len() as i32, recheck);
                pending.clear();
            }
        };

        let mut sources: Vec<Source> = Vec::with_capacity(meta.segments.len() + 1);
        for entry in &meta.segments {
            pgrx::check_for_interrupts!();
            let bytes = cached_segment(index, meta.identity, entry);
            let dead_bytes = (!entry.dead.is_empty()).then(|| read_run(index, entry.dead));
            sources.push((bytes, dead_bytes));
        }
        if meta.buffer.docs > 0 {
            sources.push((
                cached_buffer_segment(meta.identity, meta.buffer.version, &stream),
                None,
            ));
        }
        for (bytes, dead_bytes) in &sources {
            pgrx::check_for_interrupts!();
            let segment = codec(Segment::parse(bytes));
            let mut exact = true;
            let mut cursors: Vec<Box<dyn Cursor>> = Vec::with_capacity(queries.len());
            for query in queries {
                let plan = plan(query, &segment, &limits)
                    .unwrap_or_else(|error| pgrx::error!("Lead query plan: {error}"));
                exact &= plan.exact;
                cursors.push(plan.cursor);
            }
            let mut cursor: Box<dyn Cursor> = if cursors.len() == 1 {
                cursors.pop().expect("one cursor")
            } else {
                Box::new(codec(Intersection::new(cursors)))
            };
            if let Some(dead_bytes) = dead_bytes {
                let dead = codec(Postings::parse(dead_bytes).and_then(|p| p.cursor()));
                cursor = Box::new(codec(Difference::new(cursor, dead)));
            }
            while let Some(tid) = cursor.current() {
                pending.push(pointer_of(tid));
                added += 1;
                if pending.len() == BITMAP_BATCH {
                    pgrx::check_for_interrupts!();
                    flush(&mut pending, !exact);
                }
                codec(cursor.advance());
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
            let bytes = cached_segment(index, meta.identity, &entry);
            let segment = codec(Segment::parse(&bytes));
            let mut dead = dead_set(index, &entry);
            let before = dead.len();
            let mut documents = codec(segment.documents());
            while let Some(tid) = documents.current() {
                if dead.contains(&tid) {
                    // Already dead: nothing to report.
                } else if is_dead(tid) {
                    dead.insert(tid);
                    removed += 1;
                } else {
                    live += 1;
                }
                codec(documents.advance());
            }
            if dead.len() != before {
                let run = write_run(index, &encode_dead(&dead));
                let old = std::mem::replace(&mut meta.segments[i].dead, run);
                release(&mut meta, old);
            }
        }
        if meta.buffer.docs > 0 {
            let stream = read_buffer_stream(index, &meta.buffer);
            let mut kept = Vec::with_capacity(stream.len());
            let mut kept_docs = 0u32;
            let mut dropped = false;
            for record in segment::forward::records(&stream) {
                let record = codec(record);
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
        let (meta_buffer, mut meta) = read_meta(index, true);
        for i in 0..meta.segments.len() {
            pgrx::check_for_interrupts!();
            let entry = meta.segments[i];
            if entry.dead.is_empty() {
                continue;
            }
            let dead_bytes = read_run(index, entry.dead);
            let dead_count = codec(Postings::parse(&dead_bytes)).count();
            if u64::from(dead_count) * 2 < u64::from(entry.docs) {
                continue;
            }
            let dead: BTreeSet<Tid> = codec(Postings::parse(&dead_bytes).and_then(|p| p.to_vec()))
                .into_iter()
                .collect();
            let bytes = cached_segment(index, meta.identity, &entry);
            let segment = codec(Segment::parse(&bytes));
            let mut builder = SegmentBuilder::default();
            for record in codec(segment.records(|tid| dead.contains(&tid))) {
                codec(builder.add_record(&record));
            }
            let (blob, docs, total_length) = finish_builder(builder);
            let run = write_run(index, &blob);
            let generation = meta.next_generation;
            meta.next_generation = meta.next_generation.wrapping_add(1);
            meta.segments[i] = SegmentEntry {
                run,
                dead: Run::EMPTY,
                docs,
                total_length,
                generation,
            };
            release(&mut meta, entry.run);
            release(&mut meta, entry.dead);
        }
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
                let (next, _) = checked(layout::chain(buffer.page()));
                write_page(index, &buffer, false, KIND_FREE, &pending.xid.to_le_bytes());
                let freed = buffer.block();
                drop(buffer);
                pg_sys::RecordFreeIndexPage(index, freed);
                block = next;
            }
        }
        meta.pending = still_pending;
        write_meta(index, &meta_buffer, &meta);
        drop(meta_buffer);
        pg_sys::IndexFreeSpaceMapVacuum(index);
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
