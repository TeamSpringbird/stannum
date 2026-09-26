// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Removal-horizon WAL records for hot standbys.
//!
//! Generic WAL replays page images and deltas but carries no snapshot
//! information, so a standby cannot know that reusing a freed run page will
//! break a reader still holding the directory that referenced it. This module
//! is the standard PostgreSQL answer, the same one nbtree, GiST and hash use
//! for page deletion and reuse: a custom resource manager whose `RECLAIM`
//! record names the index and the latest transaction id whose snapshots could
//! still read the pages about to be freed. Replay resolves the conflict with
//! `ResolveRecoveryConflictWithSnapshot` *before* the generic records that mark
//! the pages free and reuse them, so conflicting standby queries are cancelled
//! or waited for exactly as for heap pruning and btree page reuse
//! (`max_standby_streaming_delay`, `hot_standby_feedback` apply unchanged).
//!
//! Registration needs `shared_preload_libraries = 'stannum'` on the primary
//! and on every standby that replays its WAL (PostgreSQL refuses custom
//! resource managers registered later, and fails recovery on records of an
//! unregistered manager). The manager id is `stannum.wal_rmgr_id` (default
//! 128, the experimental id) and must agree cluster-wide. Without preload the
//! primary works as before and writes no such records; a meta-page flag then
//! tells standbys to keep the heap fallback (see `storage::index_reads_allowed`).
//!
//! The record has no registered buffers: the page changes themselves stay in
//! generic WAL, so a crash-recovering primary (never in hot standby) applies it
//! as a no-op and `wal_consistency_checking` needs no mask function.

use std::ffi::{CString, c_char};
use std::sync::atomic::{AtomicU8, Ordering};

use pgrx::{GucContext, GucFlags, GucRegistry, GucSetting, pg_extern, pg_guard, pg_sys};

use super::layout::u32_at;

/// `stannum.wal_rmgr_id`: the custom resource manager id this cluster uses.
static RMGR_ID: GucSetting<i32> = GucSetting::<i32>::new(pg_sys::RM_MIN_CUSTOM_ID as i32);

/// The id registered in this postmaster, or zero when not preloaded.
/// Registration happens once in the postmaster and is inherited by fork.
static REGISTERED: AtomicU8 = AtomicU8::new(0);

/// `xl_info` of a reclaim record (the resource manager's four high bits).
pub const XLOG_STANNUM_RECLAIM: u8 = 0x00;

/// Method tables hold C string pointers; they are immutable and only read by
/// the server.
struct Rmgr(pg_sys::RmgrData);
unsafe impl Sync for Rmgr {}

static RMGR: Rmgr = Rmgr(pg_sys::RmgrData {
    rm_name: c"stannum".as_ptr(),
    rm_redo: Some(redo),
    rm_desc: Some(desc),
    rm_identify: Some(identify),
    rm_startup: None,
    rm_cleanup: None,
    rm_mask: None,
    rm_decode: None,
});

/// Registers the resource manager when loaded through
/// `shared_preload_libraries`; a no-op otherwise, so a primary without
/// preload keeps working and simply writes no removal horizons.
pub fn init() {
    if !unsafe { pg_sys::process_shared_preload_libraries_in_progress } {
        return;
    }
    GucRegistry::define_int_guc(
        c"stannum.wal_rmgr_id",
        c"Custom WAL resource manager id used for Stannum removal-horizon records",
        c"Must be identical on the primary and every standby, and unused by other extensions. Effective only with shared_preload_libraries.",
        &RMGR_ID,
        pg_sys::RM_MIN_CUSTOM_ID as i32,
        pg_sys::RM_MAX_CUSTOM_ID as i32,
        GucContext::Postmaster,
        GucFlags::default(),
    );
    let id = RMGR_ID.get() as u8;
    unsafe { pg_sys::RegisterCustomRmgr(id, &RMGR.0) };
    REGISTERED.store(id, Ordering::Release);
}

/// The registered resource manager id, if this server preloaded Stannum.
pub fn registered() -> Option<u8> {
    match REGISTERED.load(Ordering::Acquire) {
        0 => None,
        id => Some(id),
    }
}

/// Whether the startup process is replaying for hot-standby sessions. Only
/// then can a snapshot conflict exist; crash recovery has no readers.
fn in_hot_standby() -> bool {
    let state = unsafe { *std::ptr::addr_of!(pg_sys::standbyState) };
    state >= pg_sys::HotStandbyState::STANDBY_SNAPSHOT_PENDING
}

/// A reclaim record: pages of `locator` are about to be freed; every
/// snapshot with `xmin <= horizon` may still hold a directory referencing them.
#[derive(Clone, Copy, Debug)]
pub struct Reclaim {
    pub locator: pg_sys::RelFileLocator,
    pub horizon: u32,
    pub is_catalog: bool,
}

impl PartialEq for Reclaim {
    fn eq(&self, other: &Self) -> bool {
        self.encode() == other.encode()
    }
}

impl Reclaim {
    const BYTES: usize = 20;

    pub fn encode(&self) -> [u8; Self::BYTES] {
        let mut out = [0u8; Self::BYTES];
        out[0..4].copy_from_slice(&self.locator.spcOid.to_u32().to_le_bytes());
        out[4..8].copy_from_slice(&self.locator.dbOid.to_u32().to_le_bytes());
        out[8..12].copy_from_slice(&self.locator.relNumber.to_u32().to_le_bytes());
        out[12..16].copy_from_slice(&self.horizon.to_le_bytes());
        out[16..20].copy_from_slice(&u32::from(self.is_catalog).to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::BYTES {
            return None;
        }
        Some(Self {
            locator: pg_sys::RelFileLocator {
                spcOid: pg_sys::Oid::from(u32_at(bytes, 0)),
                dbOid: pg_sys::Oid::from(u32_at(bytes, 4)),
                relNumber: pg_sys::Oid::from(u32_at(bytes, 8)),
            },
            horizon: u32_at(bytes, 12),
            is_catalog: u32_at(bytes, 16) != 0,
        })
    }
}

/// Logs that pages of `index` are about to be freed, for standby snapshots up
/// to `horizon`. Must precede the generic records that free and reuse them.
/// A no-op without a registered resource manager or for relations that are
/// not WAL-logged.
///
/// # Safety
/// `index` is a live, permanent index relation held exclusively by a
/// primary-side writer.
pub unsafe fn log_reclaim(index: pg_sys::Relation, horizon: u32) {
    let Some(rmid) = registered() else {
        return;
    };
    let record = Reclaim {
        locator: unsafe { (*index).rd_locator },
        horizon,
        is_catalog: false,
    }
    .encode();
    unsafe {
        pg_sys::XLogBeginInsert();
        pg_sys::XLogRegisterData(record.as_ptr() as _, record.len() as u32);
        pg_sys::XLogInsert(rmid, XLOG_STANNUM_RECLAIM);
    }
}

unsafe fn main_data<'a>(record: *mut pg_sys::XLogReaderState) -> (u8, &'a [u8]) {
    unsafe {
        let decoded = (*record).record;
        let info = (*decoded).header.xl_info & pg_sys::XLR_RMGR_INFO_MASK as u8;
        let data = if (*decoded).main_data.is_null() {
            &[][..]
        } else {
            std::slice::from_raw_parts(
                (*decoded).main_data.cast::<u8>(),
                (*decoded).main_data_len as usize,
            )
        };
        (info, data)
    }
}

fn decode_reclaim(data: &[u8]) -> Reclaim {
    Reclaim::decode(data).unwrap_or_else(|| {
        pgrx::error!(
            "stannum WAL record: malformed RECLAIM payload of {} bytes",
            data.len()
        )
    })
}

/// Redo: resolve the snapshot conflict before the page changes that follow.
#[pg_guard]
unsafe extern "C-unwind" fn redo(record: *mut pg_sys::XLogReaderState) {
    let (info, data) = unsafe { main_data(record) };
    match info {
        XLOG_STANNUM_RECLAIM => {
            let reclaim = decode_reclaim(data);
            if in_hot_standby() {
                unsafe {
                    pg_sys::ResolveRecoveryConflictWithSnapshot(
                        pg_sys::TransactionId::from(reclaim.horizon),
                        reclaim.is_catalog,
                        reclaim.locator,
                    )
                };
            }
        }
        other => pgrx::error!("stannum WAL redo: unknown record type {other:#x}"),
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn desc(buf: pg_sys::StringInfo, record: *mut pg_sys::XLogReaderState) {
    let (info, data) = unsafe { main_data(record) };
    let text = match info {
        XLOG_STANNUM_RECLAIM => {
            let reclaim = decode_reclaim(data);
            format!(
                "rel {}/{}/{}; snapshotConflictHorizon {}, isCatalogRel {}",
                reclaim.locator.spcOid.to_u32(),
                reclaim.locator.dbOid.to_u32(),
                reclaim.locator.relNumber.to_u32(),
                reclaim.horizon,
                if reclaim.is_catalog { 'T' } else { 'F' },
            )
        }
        other => format!("unknown record type {other:#x}"),
    };
    let text = CString::new(text).expect("no interior NUL");
    unsafe { pg_sys::appendStringInfoString(buf, text.as_ptr()) };
}

#[pg_guard]
unsafe extern "C-unwind" fn identify(info: u8) -> *const c_char {
    match info & pg_sys::XLR_RMGR_INFO_MASK as u8 {
        XLOG_STANNUM_RECLAIM => c"RECLAIM".as_ptr(),
        _ => std::ptr::null(),
    }
}

/// The custom WAL resource manager id this server registered, or NULL when
/// Stannum was not loaded through `shared_preload_libraries`.
#[pg_extern(stable, parallel_safe)]
fn wal_rmgr_id() -> Option<i32> {
    registered().map(i32::from)
}

/// Whether the last writer of `index` logged removal horizons, which is what
/// a hot standby with the resource manager needs before serving segmented
/// reads from it.
#[pg_extern(volatile, parallel_safe)]
fn logs_removal_horizons(index: pgrx::PgRelation) -> bool {
    crate::udfs::require_stannum_index(&index, "logs_removal_horizons");
    unsafe { super::present(index.as_ptr()) && super::removal_horizons_logged(index.as_ptr()) }
}

/// Whether this session would serve segmented reads from `index` right now:
/// always on a primary; on a hot standby only with the resource manager
/// registered here and removal horizons logged by the primary.
#[pg_extern(volatile, parallel_safe)]
fn index_reads_allowed(index: pgrx::PgRelation) -> bool {
    crate::udfs::require_stannum_index(&index, "index_reads_allowed");
    unsafe { super::index_reads_allowed(index.as_ptr()) }
}

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use super::*;
    use pgrx::prelude::*;

    #[pg_test]
    fn reclaim_record_round_trips_and_rejects_bad_lengths() {
        let record = Reclaim {
            locator: pg_sys::RelFileLocator {
                spcOid: pg_sys::Oid::from(1663),
                dbOid: pg_sys::Oid::from(16384),
                relNumber: pg_sys::Oid::from(16410),
            },
            horizon: 0xdead_beef,
            is_catalog: false,
        };
        let bytes = record.encode();
        assert_eq!(Reclaim::decode(&bytes), Some(record));
        assert_eq!(Reclaim::decode(&bytes[..19]), None);
        assert_eq!(Reclaim::decode(&[]), None);
        unsafe {
            assert_eq!(identify(XLOG_STANNUM_RECLAIM), c"RECLAIM".as_ptr());
            assert!(identify(0x10).is_null());
        }
    }

    #[pg_test]
    fn without_preload_no_resource_manager_and_no_horizons() {
        assert_eq!(
            Spi::get_one::<String>("SHOW shared_preload_libraries").unwrap(),
            Some(String::new())
        );
        assert_eq!(registered(), None);
        assert_eq!(
            Spi::get_one::<i32>("SELECT stannum.wal_rmgr_id()").unwrap(),
            None
        );
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT bool_or(rm_name = 'stannum') FROM pg_get_wal_resource_managers()"
            )
            .unwrap(),
            Some(false)
        );
        Spi::run(
            "CREATE TABLE wal_probe (id int, body text);
             SET stannum.write_buffer_docs = 4; SET stannum.max_segments = 3;
             SET stannum.merge_tier_factor = 2;
             INSERT INTO wal_probe SELECT n, 'needle common' FROM generate_series(1, 200) n;
             CREATE INDEX wal_probe_idx ON wal_probe USING stannum (body);
             INSERT INTO wal_probe SELECT n, 'needle common' FROM generate_series(201, 400) n;
             DELETE FROM wal_probe WHERE id % 2 = 0;",
        )
        .unwrap();
        // Primaries never depend on the flag; without preload it stays clear.
        assert_eq!(
            Spi::get_one::<bool>("SELECT stannum.logs_removal_horizons('wal_probe_idx')").unwrap(),
            Some(false)
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT stannum.index_reads_allowed('wal_probe_idx')").unwrap(),
            Some(true)
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM wal_probe WHERE body ==> 'needle'").unwrap(),
            Some(200)
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM stannum.verify_index('wal_probe_idx', true)")
                .unwrap(),
            Some(0)
        );
    }
}
