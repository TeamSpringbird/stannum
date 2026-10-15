// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! The maintenance operations, as the scheduler and the SQL functions see
//! them. This is the seam between background maintenance and a storage
//! format: the queue, the workers, `stannum.promote()` and `stannum.merge()`
//! know only these types and functions, and each function calls the
//! format's implementation (today `crate::storage`, the LDP2/STN3 format).
//!
//! A format plugs in by providing, for an open index of its kind:
//!
//! - `promote`: make the sealed write segments immutable now, merging
//!   nothing, and report it as a [`FoldReport`]; the write buffer (TIN's
//!   mutable write segment) is left alone until it is full and sealed;
//! - `merge`: merge toward a [`MergeRequest`]'s target and report a
//!   [`MergeReport`];
//! - `pass`: do what a queued job's kinds ask ([`PassRequest`]) and report a
//!   [`PassReport`].
//!
//! The contract each implementation keeps:
//!
//! - It is called inside a transaction, with the heap and the index locked
//!   in `RowExclusiveLock` (the lock an insert holds), holding no buffer lock.
//! - Work that takes long runs without the index's metadata lock, checking
//!   for interrupts, so a cancel or a termination stops it at a checkpoint.
//! - Nothing the published metadata references changes before publication;
//!   pages written for an unpublished result are written under the index's
//!   maintenance lock, which VACUUM's orphan reclamation also takes, and an
//!   error or a crash before publication leaves only unreferenced pages.
//! - Publication rechecks its inputs and discards stale output, so jobs may
//!   run concurrently with inserts, VACUUM and each other.

use pgrx::pg_sys;

/// What [`promote`] did, in TIN's `promote()` terms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FoldReport {
    /// Sealed write segments consumed (TIN's `consumed_controls`).
    pub consumed: u32,
    /// Documents moved into immutable segments.
    pub docs: u64,
    /// Distinct terms of each promotion's documents, summed over promotions.
    pub terms: u64,
    /// Immutable segments published (TIN's `linked_segments`).
    pub segments: u32,
}

/// The parameters of [`merge`]; `None` takes the index's setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeRequest {
    /// Segments to merge down to: the index's `target_segment_count`, else
    /// `stannum.max_segments`.
    pub target: Option<usize>,
    /// Merge only when the directory holds more than `target` times this
    /// many segments (one by default), unless `force`.
    pub high_water: Option<usize>,
    /// Most segments one merge takes (the directory's bound by default).
    pub max_fan_in: Option<usize>,
    pub force: bool,
}

/// What [`merge`] did, in TIN's `merge()` terms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeReport {
    /// Directory entries when it started.
    pub considered: u32,
    /// Inputs whose every document was dead: they left without a successor.
    pub retired_only: u32,
    /// Inputs merged into a successor.
    pub merged: u32,
    /// Merged segments published.
    pub linked: u32,
    pub output_docs: u64,
    /// Postings of the published segments: one per term and document.
    pub output_postings: u64,
    /// Dead documents the merges dropped.
    pub replayed_kills: u64,
    /// Why nothing was merged, when nothing was.
    pub no_op_reason: Option<&'static str>,
}

/// What a maintenance worker's job asks for (see [`super::queue::kind`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassRequest {
    pub promote: bool,
    pub merge: bool,
    pub rewrite: bool,
    pub reclaim: bool,
}

/// What a pass published.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassReport {
    pub merges: u32,
    pub rewrites: u32,
}

/// Promotes `index`'s sealed write segments now, each into one immutable
/// segment or, with `extent_cap_bytes`, into several of about that many
/// bytes of input. See the module.
///
/// # Safety
/// As the module's contract says.
pub unsafe fn promote(index: pg_sys::Relation, extent_cap_bytes: Option<u64>) -> FoldReport {
    unsafe {
        crate::storage::promote_sealed(
            index,
            crate::storage::layout::MAX_SEALED,
            extent_cap_bytes,
            0,
        )
    }
}

/// Merges `index` toward `request`'s target. See the module.
///
/// # Safety
/// As the module's contract says.
pub unsafe fn merge(index: pg_sys::Relation, request: MergeRequest) -> MergeReport {
    unsafe { crate::storage::merge_toward(index, request) }
}

/// What a queued job's kinds (see [`super::queue::kind`]) ask of the format.
pub fn pass_request(kinds: u8) -> PassRequest {
    use super::queue::kind;
    PassRequest {
        promote: kinds & kind::PROMOTE != 0,
        merge: kinds & kind::MERGE != 0,
        rewrite: kinds & kind::REWRITE != 0,
        reclaim: kinds & kind::RECLAIM != 0,
    }
}

/// Why [`run_job`] did nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skipped {
    /// The index or its table is gone.
    Dropped,
    /// DDL holds a lock that conflicts with an insert's.
    Busy,
    /// Not an index this format maintains, or not ready for writes.
    NotApplicable,
}

/// A maintenance worker's job on the index with OID `index_oid`, in the
/// caller's transaction: takes the locks an insert takes, without waiting
/// behind DDL (the job is dropped instead: the directory still records the
/// work, and the next fold queues it again), and runs the format's pass.
///
/// # Safety
/// Called in a transaction of the index's database, holding no buffer lock.
pub unsafe fn run_job(index_oid: pg_sys::Oid, kinds: u8) -> Result<PassReport, Skipped> {
    unsafe {
        let mode = pg_sys::RowExclusiveLock as pg_sys::LOCKMODE;
        let heap_oid = pg_sys::IndexGetRelation(index_oid, true);
        if heap_oid == pg_sys::InvalidOid {
            return Err(Skipped::Dropped);
        }
        // Heap before index, the order an insert takes them in.
        if !pg_sys::ConditionalLockRelationOid(heap_oid, mode) {
            return Err(Skipped::Busy);
        }
        if !pg_sys::ConditionalLockRelationOid(index_oid, mode) {
            return Err(Skipped::Busy);
        }
        // Locked: the index cannot be dropped now, but may have been before.
        let index = pg_sys::try_relation_open(index_oid, pg_sys::NoLock as pg_sys::LOCKMODE);
        if index.is_null() {
            return Err(Skipped::Dropped);
        }
        let applies = (*(*index).rd_rel).relkind == pg_sys::RELKIND_INDEX as std::ffi::c_char
            && pg_sys::IndexGetRelation(index_oid, true) == heap_oid
            && crate::udfs::is_stannum_index(index)
            && !(*index).rd_index.is_null()
            && (*(*index).rd_index).indisready
            && crate::storage::present(index);
        let result = if applies {
            Ok(crate::storage::maintenance_pass(index, pass_request(kinds)))
        } else {
            Err(Skipped::NotApplicable)
        };
        pg_sys::relation_close(index, pg_sys::NoLock as pg_sys::LOCKMODE);
        result
    }
}
