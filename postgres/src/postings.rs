//! Version 1 durable fingerprint postings. Collisions require heap rechecks.
//! All locks are acquired bucket head -> tail -> relation extension. Readers and
//! VACUUM hold the head lock for the chain's lifetime; chains never change buckets.
mod page;

use page::{BUCKETS, CAPACITY, NONE, Page, PageMut, Posting};
use pgrx::{FromDatum, pg_sys};
use std::collections::BTreeSet;
use tokenizer::{Tokenizer, presets::default_pipeline};

fn checked<T>(result: Result<T, &'static str>) -> T {
    result.unwrap_or_else(|message| pgrx::error!("{message}; REINDEX required"))
}

/// Owns a buffer pin and content lock; page borrows cannot outlive this guard.
struct Buffer(pg_sys::Buffer);
impl Buffer {
    /// The caller owns a live, appropriately locked index relation. `block` is
    /// an existing block or P_NEW; callers enforce head-before-overflow ordering.
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

    fn page(&self, head: Option<u32>, limit: u32) -> Page<'_> {
        // SAFETY: the guard pins this BLCKSZ allocation and holds its content
        // lock. This immutable view borrows the guard, never an invented lifetime.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                pg_sys::BufferGetPage(self.0).cast(),
                pg_sys::BLCKSZ as usize,
            )
        };
        let block = unsafe { pg_sys::BufferGetBlockNumber(self.0) };
        checked(Page::read(bytes, block, head, limit))
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: this guard owns exactly one locked pin, acquired in read().
        unsafe { pg_sys::UnlockReleaseBuffer(self.0) };
    }
}

/// Edit a generic-WAL private image, never the shared buffer page directly.
///
/// # Safety
/// `wal` is an active record for this buffer's relation; the buffer has an
/// exclusive content lock and a validated LDP1 page (unless initializing).
/// Register buffers in lock acquisition order. The callback's borrowed view
/// cannot escape, so repeated edits never create overlapping mutable references.
unsafe fn edit(
    wal: *mut pg_sys::GenericXLogState,
    buffer: &Buffer,
    initialize: bool,
    change: impl FnOnce(&mut PageMut<'_>),
) {
    unsafe {
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
            pg_sys::PageInit(raw, pg_sys::BLCKSZ as usize, 0);
        }
        let bytes = std::slice::from_raw_parts_mut(raw.cast(), pg_sys::BLCKSZ as usize);
        change(&mut PageMut::new(bytes));
    }
}

fn fingerprint(term: &str) -> u64 {
    term.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}
unsafe fn blocks(index: pg_sys::Relation) -> u32 {
    unsafe { pg_sys::RelationGetNumberOfBlocksInFork(index, pg_sys::ForkNumber::MAIN_FORKNUM) }
}

/// Zero-page legacy, temporary and unlogged indexes retain the reference path.
///
/// # Safety
/// `index` is a live index relation held open by the caller.
pub unsafe fn present(index: pg_sys::Relation) -> bool {
    unsafe {
        let n = blocks(index);
        if n == 0 {
            return false;
        }
        if n <= BUCKETS {
            pgrx::error!("incomplete Lead index; REINDEX required");
        }
        let meta = Buffer::read(index, 0, false);
        meta.page(None, n);
        true
    }
}

/// # Safety
/// The caller owns an empty relation locked for index construction.
pub unsafe fn build_empty(index: pg_sys::Relation) {
    unsafe {
        // The init-fork/unlogged lifecycle remains on the old, correct fallback.
        if (*(*index).rd_rel).relpersistence as u8 != b'p' {
            return;
        }
        if blocks(index) != 0 {
            pgrx::error!("Lead index build requires an empty relation");
        }
        for expected in 0..=BUCKETS {
            let buffer = Buffer::read(index, NONE, true); // P_NEW
            if pg_sys::BufferGetBlockNumber(buffer.0) != expected {
                pgrx::error!("unexpected Lead index allocation");
            }
            let wal = pg_sys::GenericXLogStart(index);
            edit(wal, &buffer, true, |page| page.initialize(expected));
            pg_sys::GenericXLogFinish(wal);
        }
    }
}

unsafe fn append(index: pg_sys::Relation, posting: Posting) {
    unsafe {
        let head_block = page::bucket(posting.hash);
        let head = Buffer::read(index, head_block, true);
        let limit = blocks(index);
        let tail_block = head.page(Some(head_block), limit).tail;
        let tail_buffer = (tail_block != head_block).then(|| Buffer::read(index, tail_block, true));
        let tail = tail_buffer.as_ref().unwrap_or(&head);
        let tail_page = tail.page(Some(head_block), limit);
        checked(tail_page.require_terminal());
        if tail_page.count < CAPACITY {
            let wal = pg_sys::GenericXLogStart(index);
            edit(wal, tail, false, |page| checked(page.append(posting)));
            pg_sys::GenericXLogFinish(wal);
        } else {
            pg_sys::LockRelationForExtension(index, pg_sys::ExclusiveLock as i32);
            let new = Buffer::read(index, NONE, true);
            pg_sys::UnlockRelationForExtension(index, pg_sys::ExclusiveLock as i32);
            let block = pg_sys::BufferGetBlockNumber(new.0);
            let wal = pg_sys::GenericXLogStart(index);
            edit(wal, &head, false, |page| {
                page.set_tail(block);
                if tail_block == head_block {
                    page.set_next(block);
                }
            });
            if tail_block != head_block {
                edit(wal, tail, false, |page| page.set_next(block));
            }
            edit(wal, &new, true, |page| {
                page.initialize(NONE);
                checked(page.append(posting));
            });
            pg_sys::GenericXLogFinish(wal);
        }
    }
}

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
        // Predicate semantics currently use the default tokenizer, regardless of
        // scoring reloptions. Pruning must follow the predicate, not scoring.
        let terms: BTreeSet<_> = default_pipeline()
            .tokenize(&text)
            .map(|t| fingerprint(&t.text))
            .collect();
        let block = (u32::from((*tid).ip_blkid.bi_hi) << 16) | u32::from((*tid).ip_blkid.bi_lo);
        let offset = (*tid).ip_posid;
        for hash in terms {
            pgrx::check_for_interrupts!();
            append(
                index,
                checked(
                    Posting {
                        hash,
                        block,
                        offset,
                    }
                    .validate(),
                ),
            );
        }
    }
}

/// # Safety
/// `index` is a live LDP1 relation; `bitmap` is a valid, writable PostgreSQL TID
/// bitmap. The caller performs heap visibility and exact predicate rechecks.
pub unsafe fn lookup(index: pg_sys::Relation, term: &str, bitmap: *mut pg_sys::TIDBitmap) -> i64 {
    unsafe {
        let hash = fingerprint(term);
        let head_block = page::bucket(hash);
        let head = Buffer::read(index, head_block, false);
        let limit = blocks(index);
        let mut block = head_block;
        let mut count = 0;
        for _ in 0..limit {
            pgrx::check_for_interrupts!();
            let other = (block != head_block).then(|| Buffer::read(index, block, false));
            let page = other
                .as_ref()
                .unwrap_or(&head)
                .page(Some(head_block), limit);
            for posting in page.postings() {
                if posting.hash == hash {
                    let mut tuple = posting.tid();
                    pg_sys::tbm_add_tuples(bitmap, &mut tuple, 1, true);
                    count += 1;
                }
            }
            block = page.next;
            if block == NONE {
                return count;
            }
        }
        pgrx::error!("cycle in Lead posting chain");
    }
}

/// # Safety
/// `index` is a live LDP1 relation locked for VACUUM. The callback and its state
/// satisfy PostgreSQL's index bulk-delete contract for the duration of the call.
pub unsafe fn vacuum(
    index: pg_sys::Relation,
    callback: pg_sys::IndexBulkDeleteCallback,
    state: *mut std::ffi::c_void,
) -> (u64, u64) {
    unsafe {
        let callback = callback.expect("VACUUM callback");
        let (mut live, mut removed) = (0, 0);
        for head_block in 1..=BUCKETS {
            let head = Buffer::read(index, head_block, true);
            let limit = blocks(index);
            let mut block = head_block;
            let mut steps = 0;
            loop {
                pgrx::check_for_interrupts!();
                steps += 1;
                if steps > limit {
                    pgrx::error!("cycle in Lead posting chain");
                }
                let other = (block != head_block).then(|| Buffer::read(index, block, true));
                let buffer = other.as_ref().unwrap_or(&head);
                let page = buffer.page(Some(head_block), limit);
                let count = page.count;
                let following = page.next;
                let mut retained = Vec::with_capacity(count);
                for posting in page.postings() {
                    if callback(&mut posting.tid(), state) {
                        removed += 1;
                    } else {
                        retained.push(posting);
                        live += 1;
                    }
                }
                if retained.len() != count {
                    let wal = pg_sys::GenericXLogStart(index);
                    edit(wal, buffer, false, |page| page.retain(&retained));
                    pg_sys::GenericXLogFinish(wal);
                }
                block = following;
                if block == NONE {
                    break;
                }
            }
        }
        (live, removed)
    }
}
