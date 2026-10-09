// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! The maintenance job queue as it lives in shared memory: a fixed array of
//! jobs keyed by (database, index), the one worker slot, and counters.
//!
//! Everything here is plain data and logic over it, with no PostgreSQL
//! calls, so the rules (deduplication, which job a worker takes next, when a
//! worker yields to another database) are tested without a server. The
//! caller holds the queue's LWLock around every method (see
//! [`super::shared`]).

/// Jobs the queue holds at once. A backend whose job does not fit runs the
/// work inline, as it would without workers.
pub const QUEUE_SLOTS: usize = 256;

/// What a job asks the worker to do, as bits; a queued job's kinds are the
/// union of every request for its index since it was queued.
pub mod kind {
    /// Merge the directory toward its target: due size tiers, then the
    /// smallest entries while it holds more than `target_segment_count`.
    pub const MERGE: u8 = 1 << 0;
    /// Rewrite segments whose dead fraction reached `dead_percent_threshold`.
    pub const REWRITE: u8 = 1 << 1;
    /// Free retired runs that no snapshot can still read.
    pub const RECLAIM: u8 = 1 << 2;
    /// Reserved: promote a sealed write segment. Today's format folds its
    /// write buffer in the writing session (see the design note), so no
    /// backend queues this yet.
    pub const PROMOTE: u8 = 1 << 3;
    /// Everything a fold leaves behind.
    pub const AFTER_FOLD: u8 = MERGE | REWRITE | RECLAIM;
    /// Kinds a job may carry.
    pub const ALL: u8 = MERGE | REWRITE | RECLAIM | PROMOTE;
}

const FREE: u8 = 0;
const QUEUED: u8 = 1;
const RUNNING: u8 = 2;

/// One job slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Job {
    pub database: u32,
    pub index: u32,
    pub kinds: u8,
    state: u8,
    /// Queue order: smaller is older.
    seq: u64,
}

/// What the worker slot is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkerState {
    /// No worker: the launcher may start one.
    #[default]
    Idle,
    /// The launcher registered a worker for `database` that has not yet
    /// attached to the slot.
    Starting,
    /// A worker is attached and taking jobs of `database`.
    Running,
}

/// The cluster's one maintenance worker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Worker {
    pub state: WorkerState,
    pub database: u32,
    pub pid: i32,
    /// Jobs finished since it attached, for `maintenance_jobs_per_db`.
    pub jobs_done: u32,
}

/// Monotonic counters, reported by `stannum.maintenance_status()`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Requests merged into the queue (deduplicated requests included).
    pub requested: u64,
    /// Jobs a worker finished.
    pub completed: u64,
    /// Jobs a worker held when it exited without finishing them.
    pub abandoned: u64,
    /// Requests a backend ran inline because no worker could take them.
    pub inline: u64,
    /// Workers started, and registrations PostgreSQL refused.
    pub launches: u64,
    pub launch_failures: u64,
}

/// The queue, the worker slot and the launcher, all under one LWLock.
#[derive(Clone, Copy, Debug)]
pub struct Queue {
    pub launcher_pid: i32,
    pub launcher_latch: usize,
    /// The last worker registration failed (no free `max_worker_processes`
    /// slot): backends run maintenance inline until one succeeds.
    pub launch_failed: bool,
    pub worker: Worker,
    /// The database the last worker served, so the launcher prefers another.
    pub last_database: u32,
    pub counters: Counters,
    next_seq: u64,
    jobs: [Job; QUEUE_SLOTS],
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            launcher_pid: 0,
            launcher_latch: 0,
            launch_failed: false,
            worker: Worker::default(),
            last_database: 0,
            counters: Counters::default(),
            next_seq: 1,
            jobs: [Job::default(); QUEUE_SLOTS],
        }
    }
}

/// A job handed to a worker: its slot, to finish it by, and what to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claimed {
    pub slot: usize,
    pub index: u32,
    pub kinds: u8,
}

/// Why [`Queue::claim`] gave the worker nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Idle {
    /// No job of the worker's database is queued.
    Drained,
    /// The worker finished its `maintenance_jobs_per_db` and another
    /// database has queued work.
    Yield,
}

impl Queue {
    /// Whether a backend may hand work to a worker rather than run it inline.
    pub fn accepting(&self) -> bool {
        self.launcher_pid != 0 && !self.launch_failed
    }

    /// Queues `kinds` for the index, merging them into the index's queued
    /// job if it has one. A job a worker is running does not absorb the
    /// request: the work it asks for may postdate what the worker read.
    /// Returns false when the queue is full.
    pub fn enqueue(&mut self, database: u32, index: u32, kinds: u8) -> bool {
        let kinds = kinds & kind::ALL;
        if let Some(job) = self
            .jobs
            .iter_mut()
            .find(|job| job.state == QUEUED && job.database == database && job.index == index)
        {
            job.kinds |= kinds;
            self.counters.requested += 1;
            return true;
        }
        let Some(job) = self.jobs.iter_mut().find(|job| job.state == FREE) else {
            return false;
        };
        *job = Job {
            database,
            index,
            kinds,
            state: QUEUED,
            seq: self.next_seq,
        };
        self.next_seq += 1;
        self.counters.requested += 1;
        true
    }

    /// The oldest queued job of `database`, now running; or why there is
    /// none. With `jobs_per_db` above zero, a worker that has finished that
    /// many yields when another database has queued work; at zero it drains
    /// its database first.
    pub fn claim(&mut self, database: u32, jobs_per_db: u32) -> Result<Claimed, Idle> {
        if jobs_per_db > 0
            && self.worker.jobs_done >= jobs_per_db
            && self
                .jobs
                .iter()
                .any(|job| job.state == QUEUED && job.database != database)
        {
            return Err(Idle::Yield);
        }
        let (slot, job) = self
            .jobs
            .iter_mut()
            .enumerate()
            .filter(|(_, job)| job.state == QUEUED && job.database == database)
            .min_by_key(|(_, job)| job.seq)
            .ok_or(Idle::Drained)?;
        job.state = RUNNING;
        Ok(Claimed {
            slot,
            index: job.index,
            kinds: job.kinds,
        })
    }

    /// Frees a finished job's slot.
    pub fn finish(&mut self, claimed: Claimed) {
        let job = &mut self.jobs[claimed.slot];
        debug_assert_eq!(job.state, RUNNING);
        *job = Job::default();
        self.worker.jobs_done += 1;
        self.counters.completed += 1;
    }

    /// Frees the running jobs of `database`, which a worker held when it
    /// exited: the index's directory still records the work, and the next
    /// fold queues it again, so a job that keeps failing cannot keep a
    /// worker restarting.
    pub fn abandon_running(&mut self, database: u32) {
        for job in &mut self.jobs {
            if job.state == RUNNING && job.database == database {
                *job = Job::default();
                self.counters.abandoned += 1;
            }
        }
    }

    /// Drops every job of `database`, whose worker could not start (the
    /// database was dropped, or refuses connections).
    pub fn drop_database(&mut self, database: u32) {
        for job in &mut self.jobs {
            if job.database == database && job.state != FREE {
                *job = Job::default();
                self.counters.abandoned += 1;
            }
        }
    }

    /// The database the launcher should start a worker for, if any: that of
    /// the oldest queued job, preferring a database other than the last
    /// one served, so a database with a steady stream of jobs cannot keep
    /// the others waiting once its worker yields.
    pub fn next_database(&self) -> Option<u32> {
        let oldest = |other: bool| {
            self.jobs
                .iter()
                .filter(|job| job.state == QUEUED && (!other || job.database != self.last_database))
                .min_by_key(|job| job.seq)
                .map(|job| job.database)
        };
        oldest(true).or_else(|| oldest(false))
    }

    /// The jobs in queue order, for `stannum.maintenance_jobs()`.
    pub fn jobs(&self) -> Vec<(Job, bool)> {
        let mut jobs: Vec<Job> = self
            .jobs
            .iter()
            .copied()
            .filter(|job| job.state != FREE)
            .collect();
        jobs.sort_by_key(|job| job.seq);
        jobs.into_iter()
            .map(|job| (job, job.state == RUNNING))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_for_a_queued_index_joins_its_job() {
        let mut queue = Queue::default();
        assert!(queue.enqueue(1, 10, kind::MERGE));
        assert!(queue.enqueue(1, 10, kind::RECLAIM));
        assert!(queue.enqueue(1, 11, kind::MERGE));
        let jobs = queue.jobs();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].0.kinds, kind::MERGE | kind::RECLAIM);
        assert_eq!(queue.counters.requested, 3);
    }

    #[test]
    fn a_running_job_does_not_absorb_a_new_request() {
        let mut queue = Queue::default();
        queue.enqueue(1, 10, kind::MERGE);
        let running = queue.claim(1, 0).unwrap();
        assert!(queue.enqueue(1, 10, kind::MERGE));
        assert_eq!(queue.jobs().len(), 2);
        queue.finish(running);
        assert_eq!(queue.claim(1, 0).map(|c| c.index), Ok(10));
    }

    #[test]
    fn a_full_queue_refuses() {
        let mut queue = Queue::default();
        for index in 0..QUEUE_SLOTS as u32 {
            assert!(queue.enqueue(1, index, kind::MERGE));
        }
        assert!(!queue.enqueue(1, u32::MAX, kind::MERGE));
        // A request for a queued index still fits.
        assert!(queue.enqueue(1, 0, kind::RECLAIM));
    }

    #[test]
    fn a_worker_takes_its_databases_jobs_oldest_first() {
        let mut queue = Queue::default();
        queue.enqueue(1, 10, kind::MERGE);
        queue.enqueue(2, 20, kind::MERGE);
        queue.enqueue(1, 11, kind::MERGE);
        assert_eq!(queue.claim(1, 0).map(|c| c.index), Ok(10));
        assert_eq!(queue.claim(1, 0).map(|c| c.index), Ok(11));
        assert_eq!(queue.claim(1, 0), Err(Idle::Drained));
    }

    #[test]
    fn zero_jobs_per_db_drains_the_current_database_first() {
        let mut queue = Queue::default();
        queue.enqueue(2, 20, kind::MERGE);
        for index in 0..5 {
            queue.enqueue(1, index, kind::MERGE);
        }
        for _ in 0..5 {
            let job = queue.claim(1, 0).unwrap();
            queue.finish(job);
        }
        assert_eq!(queue.claim(1, 0), Err(Idle::Drained));
    }

    #[test]
    fn a_worker_yields_after_jobs_per_db_when_another_database_waits() {
        let mut queue = Queue::default();
        for index in 0..5 {
            queue.enqueue(1, index, kind::MERGE);
        }
        for _ in 0..2 {
            let job = queue.claim(1, 2).unwrap();
            queue.finish(job);
        }
        // Nobody else waits: carry on.
        let job = queue.claim(1, 2).unwrap();
        queue.finish(job);
        queue.enqueue(2, 20, kind::MERGE);
        assert_eq!(queue.claim(1, 2), Err(Idle::Yield));
        queue.last_database = 1;
        assert_eq!(queue.next_database(), Some(2));
    }

    #[test]
    fn the_launcher_prefers_a_database_other_than_the_last() {
        let mut queue = Queue::default();
        queue.enqueue(1, 10, kind::MERGE);
        queue.enqueue(2, 20, kind::MERGE);
        queue.last_database = 1;
        assert_eq!(queue.next_database(), Some(2));
        queue.last_database = 2;
        assert_eq!(queue.next_database(), Some(1));
        queue.drop_database(1);
        assert_eq!(queue.next_database(), Some(2));
    }

    #[test]
    fn an_exited_workers_jobs_are_abandoned_not_retried() {
        let mut queue = Queue::default();
        queue.enqueue(1, 10, kind::MERGE);
        queue.enqueue(1, 11, kind::MERGE);
        queue.claim(1, 0).unwrap();
        queue.abandon_running(1);
        assert_eq!(queue.counters.abandoned, 1);
        assert_eq!(queue.claim(1, 0).map(|c| c.index), Ok(11));
    }

    #[test]
    fn a_launch_failure_stops_accepting_work() {
        let mut queue = Queue::default();
        assert!(!queue.accepting());
        queue.launcher_pid = 42;
        assert!(queue.accepting());
        queue.launch_failed = true;
        assert!(!queue.accepting());
    }
}
