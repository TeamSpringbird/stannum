// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! The launcher and the per-database maintenance worker.
//!
//! The launcher is a static background worker, registered by `_PG_init`
//! when the library is preloaded and started once recovery has finished, so
//! a hot standby never runs one. Like PostgreSQL's logical replication
//! launcher it connects to no database, only to appear in
//! `pg_stat_activity` and take the postmaster's start and stop notices. It
//! sleeps on its latch; a backend that queues a job sets it, and so does a
//! worker that exits. When no worker is running and a job is queued, it
//! registers a dynamic worker for the database of the oldest queued job.
//!
//! A worker connects to its database, attaches to the worker slot, and takes
//! that database's jobs one at a time, each in its own transaction, until
//! the database has none left or it has finished
//! `stannum.maintenance_jobs_per_db` jobs while another database waits. It
//! then detaches and exits, and the launcher starts the next one. A job
//! takes the locks an insert does and runs [`super::ops::run_job`]; an
//! error ends the worker (a background worker has no outer error handler),
//! and its exit callback frees the slot and abandons the job, whose work the
//! index's directory still records.

use std::ffi::{CString, c_int};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pgrx::bgworkers::{BackgroundWorker, BackgroundWorkerBuilder, SignalWakeFlags};
use pgrx::{IntoDatum, pg_guard, pg_sys};

use super::QUEUE;
use super::queue::{Claimed, WorkerState};

const LIBRARY: &str = "stannum";
const LAUNCHER: &str = "stannum maintenance launcher";
const WORKER: &str = "stannum maintenance worker";

/// Registers the launcher. Called from `_PG_init` while preloading.
pub fn register_launcher() {
    BackgroundWorkerBuilder::new(LAUNCHER)
        .set_type(LAUNCHER)
        .set_library(LIBRARY)
        .set_function("stannum_maintenance_launcher")
        // A connection to no database (see the module); start time
        // RecoveryFinished.
        .enable_spi_access()
        // Restarted after an error, and after a crash's reinitialization
        // (a worker that never restarts is forgotten by then).
        .set_restart_time(Some(Duration::from_secs(5)))
        .load();
}

/// Installs `handler` for `signal`.
unsafe fn set_signal(signal: u32, handler: pg_sys::pqsigfunc) {
    #[cfg(feature = "pg17")]
    unsafe {
        pg_sys::pqsignal(signal as c_int, handler);
    }
    #[cfg(feature = "pg18")]
    unsafe {
        pg_sys::pqsignal_be(signal as c_int, handler);
    }
}

/// The worker's SIGTERM, as a backend's `die`: a termination request that
/// the next interrupt check delivers.
unsafe extern "C-unwind" fn terminate(_signal: c_int) {
    unsafe {
        pg_sys::InterruptPending = 1;
        pg_sys::ProcDiePending = 1;
        pg_sys::SetLatch(pg_sys::MyLatch);
    }
}

/// The worker the launcher registered and has not yet seen exit.
struct Started {
    handle: *mut pg_sys::BackgroundWorkerHandle,
    database: u32,
}

impl Started {
    fn stopped(&self) -> bool {
        let mut pid: pg_sys::pid_t = 0;
        let status = unsafe { pg_sys::GetBackgroundWorkerPid(self.handle, &mut pid) };
        status == pg_sys::BgwHandleStatus::BGWH_STOPPED
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        unsafe { pg_sys::pfree(self.handle.cast()) };
    }
}

#[pg_guard]
#[unsafe(no_mangle)]
pub extern "C-unwind" fn stannum_maintenance_launcher(_arg: pg_sys::Datum) {
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);
    // No database: shared catalogs only. The postmaster's SIGUSR1 notices
    // (bgw_notify_pid) then set the latch through the procsignal handler.
    unsafe { pg_sys::BackgroundWorkerInitializeConnection(std::ptr::null(), std::ptr::null(), 0) };
    {
        let mut queue = QUEUE.exclusive();
        queue.launcher_pid = unsafe { pg_sys::MyProcPid };
        queue.launcher_latch = unsafe { &raw mut (*pg_sys::MyProc).procLatch } as usize;
        // A launcher restarted after an error finds the slot as its
        // predecessor left it; a worker it had started has exited or will
        // detach on its own.
        queue.launch_failed = false;
    }
    let mut started: Option<Started> = None;
    loop {
        if BackgroundWorker::sigterm_received() {
            break;
        }
        if BackgroundWorker::sighup_received() {
            unsafe { pg_sys::ProcessConfigFile(pg_sys::GucContext::PGC_SIGHUP) };
        }
        reap(&mut started);
        let launch = {
            let mut queue = QUEUE.exclusive();
            match (queue.worker.state, queue.next_database()) {
                (WorkerState::Idle, Some(database)) => {
                    queue.worker.state = WorkerState::Starting;
                    queue.worker.database = database;
                    queue.worker.pid = 0;
                    queue.worker.jobs_done = 0;
                    Some(database)
                }
                _ => None,
            }
        };
        if let Some(database) = launch {
            let handle = launch_worker(database);
            let mut queue = QUEUE.exclusive();
            match handle {
                Some(handle) => {
                    queue.launch_failed = false;
                    queue.counters.launches += 1;
                    started = Some(Started { handle, database });
                }
                None => {
                    // No free worker slot: backends run maintenance inline
                    // until a registration succeeds.
                    queue.worker.state = WorkerState::Idle;
                    queue.launch_failed = true;
                    queue.counters.launch_failures += 1;
                }
            }
        }
        // Sleep until a backend queues a job or a worker starts or exits.
        // While something is pending the launcher also looks again after a
        // second: a refused registration is retried, and a worker that
        // exited before attaching is noticed even if its notice was missed.
        let pending = {
            let queue = QUEUE.share();
            queue.launch_failed || queue.worker.state != WorkerState::Idle
        };
        let timeout = pending.then_some(Duration::from_secs(1));
        if !BackgroundWorker::wait_latch(timeout) {
            break;
        }
    }
    let mut queue = QUEUE.exclusive();
    queue.launcher_pid = 0;
    queue.launcher_latch = 0;
}

/// Forgets a worker that has exited. One that exited without detaching
/// (it failed before attaching, for instance because its database was
/// dropped) takes its database's queued jobs with it, so the launcher does
/// not start a worker for that database again and again.
fn reap(started: &mut Option<Started>) {
    let Some(worker) = started else {
        return;
    };
    if !worker.stopped() {
        return;
    }
    let database = worker.database;
    *started = None;
    let mut queue = QUEUE.exclusive();
    if queue.worker.database != database {
        return;
    }
    match queue.worker.state {
        WorkerState::Starting => {
            queue.drop_database(database);
            queue.worker.state = WorkerState::Idle;
        }
        WorkerState::Running => {
            queue.abandon_running(database);
            queue.worker.state = WorkerState::Idle;
        }
        WorkerState::Idle => {}
    }
}

/// Registers a worker for `database`; `None` when PostgreSQL has no free
/// background worker slot.
fn launch_worker(database: u32) -> Option<*mut pg_sys::BackgroundWorkerHandle> {
    let builder = BackgroundWorkerBuilder::new(&format!("{WORKER} for database {database}"))
        .set_type(WORKER)
        .set_library(LIBRARY)
        .set_function("stannum_maintenance_worker")
        .enable_spi_access()
        .set_argument(pg_sys::Oid::from(database).into_datum())
        .set_notify_pid(unsafe { pg_sys::MyProcPid });
    let mut worker: pg_sys::BackgroundWorker = (&builder).into();
    let mut handle: *mut pg_sys::BackgroundWorkerHandle = std::ptr::null_mut();
    let registered = unsafe { pg_sys::RegisterDynamicBackgroundWorker(&mut worker, &mut handle) };
    registered.then_some(handle)
}

/// Whether this worker has left the worker slot, so its exit callback has
/// nothing to free.
static DETACHED: AtomicBool = AtomicBool::new(false);

#[pg_guard]
#[unsafe(no_mangle)]
pub extern "C-unwind" fn stannum_maintenance_worker(arg: pg_sys::Datum) {
    let database = arg.value() as u32;
    // Terminate as a backend does, at the next interrupt check: merge
    // checkpoints deliver it, as they do pg_terminate_backend to an insert.
    // Signals stay blocked until attach_signal_handlers unblocks them.
    unsafe { set_signal(pg_sys::SIGTERM, Some(terminate)) };
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP);
    unsafe {
        pg_sys::BackgroundWorkerInitializeConnectionByOid(
            pg_sys::Oid::from(database),
            pg_sys::InvalidOid,
            0,
        )
    };
    {
        let mut queue = QUEUE.exclusive();
        if queue.worker.state != WorkerState::Starting || queue.worker.database != database {
            // Not the worker the launcher is waiting for (a restart found a
            // stale registration): leave the slot alone.
            DETACHED.store(true, Ordering::Relaxed);
            return;
        }
        queue.worker.state = WorkerState::Running;
        queue.worker.pid = unsafe { pg_sys::MyProcPid };
        queue.worker.jobs_done = 0;
    }
    unsafe { pg_sys::before_shmem_exit(Some(detach_on_exit), pg_sys::Datum::from(0)) };
    loop {
        pgrx::check_for_interrupts!();
        if unsafe { pg_sys::ConfigReloadPending } != 0 {
            unsafe {
                pg_sys::ConfigReloadPending = 0;
                pg_sys::ProcessConfigFile(pg_sys::GucContext::PGC_SIGHUP);
            }
        }
        let claimed = {
            let mut queue = QUEUE.exclusive();
            match queue.claim(database, super::jobs_per_db()) {
                Ok(claimed) => claimed,
                Err(_) => {
                    // Detach in the same critical section that found nothing
                    // to do: a job queued after it wakes the launcher, which
                    // then sees no worker and starts one.
                    queue.worker.state = WorkerState::Idle;
                    queue.worker.pid = 0;
                    queue.last_database = database;
                    DETACHED.store(true, Ordering::Relaxed);
                    let latch = queue.launcher_latch;
                    drop(queue);
                    wake(latch);
                    break;
                }
            }
        };
        run(claimed);
        QUEUE.exclusive().finish(claimed);
    }
}

/// Runs one job in its own transaction.
fn run(claimed: Claimed) {
    unsafe {
        pg_sys::SetCurrentStatementStartTimestamp();
        pg_sys::StartTransactionCommand();
        let activity = CString::new(format!(
            "maintenance of index {} (kinds {:#x})",
            claimed.index, claimed.kinds
        ))
        .expect("no NUL");
        pg_sys::pgstat_report_activity(pg_sys::BackendState::STATE_RUNNING, activity.as_ptr());
    }
    #[cfg(feature = "pg_test")]
    super::sql::arm_worker_race();
    let outcome = unsafe { super::ops::run_job(pg_sys::Oid::from(claimed.index), claimed.kinds) };
    pgrx::debug1!(
        "Stannum maintenance of index {}: {outcome:?}",
        claimed.index
    );
    #[cfg(feature = "pg_test")]
    crate::storage::testing::set_race_hook(None);
    unsafe {
        pg_sys::CommitTransactionCommand();
        pg_sys::pgstat_report_activity(pg_sys::BackendState::STATE_IDLE, std::ptr::null());
    }
}

fn wake(latch: usize) {
    if latch != 0 {
        unsafe { pg_sys::SetLatch(latch as *mut pg_sys::Latch) };
    }
}

/// Frees the worker slot and abandons the running job when the worker exits
/// without detaching: an error, a termination, or postmaster shutdown.
unsafe extern "C-unwind" fn detach_on_exit(_code: c_int, _arg: pg_sys::Datum) {
    if DETACHED.load(Ordering::Relaxed) {
        return;
    }
    // An error raised while the queue's lock was held would leave it held
    // until ProcKill, after this callback.
    unsafe { pg_sys::LWLockReleaseAll() };
    let latch = {
        let mut queue = QUEUE.exclusive();
        if queue.worker.pid == unsafe { pg_sys::MyProcPid } {
            let database = queue.worker.database;
            queue.abandon_running(database);
            queue.worker.state = WorkerState::Idle;
            queue.worker.pid = 0;
            queue.last_database = database;
        }
        queue.launcher_latch
    };
    wake(latch);
}
