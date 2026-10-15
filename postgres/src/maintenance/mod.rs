// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Background maintenance: a launcher registered when the library is
//! preloaded, one dynamic worker at a time per database with queued work,
//! and a job queue in shared memory that inserting backends hand the
//! maintenance a fold leaves behind to. `docs/architecture/maintenance-workers.md`
//! describes the model; [`ops`] is the interface through which jobs and the
//! SQL functions reach the storage format.
//!
//! Without `shared_preload_libraries = 'stannum'` there is no queue and no
//! worker, and every backend runs maintenance inline, as it always has.

pub(crate) mod ops;
pub(crate) mod queue;
mod sql;
mod worker;

use std::sync::atomic::{AtomicBool, Ordering};

use pgrx::{
    GucContext, GucFlags, GucRegistry, GucSetting, PgLwLock, pg_guard, pg_shmem_init, pg_sys,
};

use queue::Queue;

/// `stannum.index_maintenance_mode`: who runs the maintenance a fold leaves
/// behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, pgrx::PostgresGucEnum)]
pub enum Mode {
    /// A maintenance worker, when one can take it; otherwise the writing
    /// session, as in foreground mode.
    #[name = c"background"]
    Background,
    /// The writing session, after its fold has published.
    #[name = c"foreground"]
    Foreground,
    /// Nobody: only VACUUM, `stannum.merge()` and the merge that keeps a full
    /// directory within its on-disk bound.
    #[name = c"manual"]
    Manual,
}

static MODE: GucSetting<Mode> = GucSetting::<Mode>::new(Mode::Background);
static JOBS_PER_DB: GucSetting<i32> = GucSetting::<i32>::new(0);

/// pg_test builds only: `<action>:<race point>`, where action is `crash`
/// (flush WAL, then PANIC) or `terminate` (as `pg_terminate_backend`), applied
/// by a maintenance worker at the first such race point of each job.
#[cfg(feature = "pg_test")]
static WORKER_RACE: GucSetting<Option<std::ffi::CString>> =
    GucSetting::<Option<std::ffi::CString>>::new(None);

/// The queue, the worker slot and the launcher's latch.
static QUEUE: PgLwLock<Queue> = unsafe { PgLwLock::new(c"stannum_maintenance") };

// SAFETY: plain integers and arrays of them; the latch addresses point into
// PostgreSQL's shared memory, mapped at the same address in every process.
unsafe impl pgrx::PGRXSharedMemory for Queue {}

/// Whether this postmaster preloaded the library, so [`QUEUE`] exists. Set in
/// the postmaster and inherited by every process it forks.
static PRELOADED: AtomicBool = AtomicBool::new(false);

// pg_shmem_init! tests features of every PostgreSQL version pgrx supports.
#[allow(unexpected_cfgs)]
pub fn init() {
    GucRegistry::define_enum_guc(
        c"stannum.index_maintenance_mode",
        c"Who runs the merges and reclamation a Stannum write-buffer fold leaves behind",
        c"background hands them to a maintenance worker when the library is preloaded and a worker can take them, and otherwise runs them in the writing session; foreground runs them in the writing session; manual leaves them to VACUUM and stannum.merge().",
        &MODE,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"stannum.maintenance_jobs_per_db",
        c"Jobs the Stannum maintenance worker finishes in one database before yielding to another with queued work",
        c"Zero drains the current database first.",
        &JOBS_PER_DB,
        0,
        i32::MAX,
        GucContext::Sighup,
        GucFlags::default(),
    );
    #[cfg(feature = "pg_test")]
    GucRegistry::define_string_guc(
        c"stannum.debug_maintenance_race",
        c"Test builds only: crash:<point> or terminate:<point> for maintenance workers",
        c"",
        &WORKER_RACE,
        GucContext::Sighup,
        GucFlags::default(),
    );
    if unsafe { pg_sys::process_shared_preload_libraries_in_progress } {
        pg_shmem_init!(QUEUE);
        PRELOADED.store(true, Ordering::Release);
        worker::register_launcher();
    }
}

fn preloaded() -> bool {
    PRELOADED.load(Ordering::Acquire)
}

/// Whether `stannum.index_maintenance_mode` is manual: sealed write segments
/// wait for `promote()` (or a third seal), merges for VACUUM and `merge()`.
pub fn manual() -> bool {
    MODE.get() == Mode::Manual
}

/// What the writing session does with the maintenance its fold leaves behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Queue it for a worker after publishing (and run it inline if the
    /// queue refuses); spend no inline merge budget under the meta lock.
    Defer,
    /// Run it inline: the fold's budgeted merges, then its deferred merge.
    Inline,
    /// Run none of it.
    Skip,
}

/// The plan for a fold of `index` in this session. Called once per fold.
///
/// # Safety
/// `index` is an open relation.
pub unsafe fn plan(index: pg_sys::Relation) -> Plan {
    match MODE.get() {
        Mode::Manual => Plan::Skip,
        Mode::Foreground => Plan::Inline,
        Mode::Background => {
            if !preloaded() || !unsafe { queueable(index) } {
                return Plan::Inline;
            }
            let mut queue = QUEUE.exclusive();
            if queue.accepting() {
                Plan::Defer
            } else {
                queue.counters.inline += 1;
                Plan::Inline
            }
        }
    }
}

/// Whether a worker could open `index` at all: not another session's
/// temporary relation, and not on a standby (which takes no writes anyway).
unsafe fn queueable(index: pg_sys::Relation) -> bool {
    unsafe {
        (*(*index).rd_rel).relpersistence != pg_sys::RELPERSISTENCE_TEMP as std::ffi::c_char
            && !pg_sys::RecoveryInProgress()
    }
}

/// Queues `kinds` (see [`queue::kind`]) for `index` and wakes the launcher.
/// Returns false, counting an inline fallback, when no worker can take it:
/// the library is not preloaded, the launcher is not running, the last
/// worker registration failed, or the queue is full. Never waits for a
/// worker; the queue's lock is held for a scan of its slots.
///
/// # Safety
/// `index` is an open relation; the caller holds no buffer lock.
pub unsafe fn request(index: pg_sys::Relation, kinds: u8) -> bool {
    if !preloaded() || !unsafe { queueable(index) } {
        return false;
    }
    let database = unsafe { pg_sys::MyDatabaseId }.to_u32();
    let oid = unsafe { (*index).rd_id }.to_u32();
    let latch = {
        let mut queue = QUEUE.exclusive();
        if !queue.accepting() || !queue.enqueue(database, oid, kinds) {
            queue.counters.inline += 1;
            return false;
        }
        queue.launcher_latch
    };
    if latch != 0 {
        unsafe { pg_sys::SetLatch(latch as *mut pg_sys::Latch) };
    }
    true
}

/// `stannum.maintenance_jobs_per_db` as the worker applies it.
fn jobs_per_db() -> u32 {
    JOBS_PER_DB.get().max(0) as u32
}
