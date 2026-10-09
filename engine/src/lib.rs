// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Stannum's query engine over segments, without PostgreSQL.
//!
//! The extension (`postgres/`) captures an index view, checks visibility
//! and reports errors the PostgreSQL way; what lies between, from BM25
//! arithmetic to the ranked walk over ordinal streams, lives here, so it
//! can be run, measured and benchmarked over dumped segments outside a
//! server.
//!
//! A host installs its services once, before the first query: an interrupt
//! check ([`set_interrupt_check`]), a report of corrupt index data
//! ([`set_corruption_report`]) and a count of the pages it has read or hit
//! ([`set_blocks_probe`]). Without them, interrupts are not checked,
//! corruption panics and no pages are counted.

use std::sync::OnceLock;

pub mod bm25;
pub mod fold;
pub mod walk;

static INTERRUPT_CHECK: OnceLock<fn()> = OnceLock::new();
static CORRUPTION_REPORT: OnceLock<fn(String) -> !> = OnceLock::new();
static BLOCKS_PROBE: OnceLock<fn() -> i64> = OnceLock::new();

/// Installs `check`, which the walk calls every 64 steps of its loops. The
/// extension installs PostgreSQL's `CHECK_FOR_INTERRUPTS`, whose ERROR on a
/// cancel reaches Rust as a panic and unwinds out of the walk, its hold
/// spans released on the way. The first installation wins.
pub fn set_interrupt_check(check: fn()) {
    let _ = INTERRUPT_CHECK.set(check);
}

/// Installs `report`, which never returns: called with a message when index
/// data the engine reads is inconsistent. The extension raises an ERROR
/// telling the user to REINDEX. The first installation wins.
pub fn set_corruption_report(report: fn(String) -> !) {
    let _ = CORRUPTION_REPORT.set(report);
}

/// Installs `probe`, the host's count of pages read or hit so far, which
/// the walk splits into setup and walking (see [`walk::walk_blocks`]). The
/// first installation wins.
pub fn set_blocks_probe(probe: fn() -> i64) {
    let _ = BLOCKS_PROBE.set(probe);
}

/// Calls the installed interrupt check, if any.
#[inline]
pub fn check_interrupts() {
    if let Some(check) = INTERRUPT_CHECK.get() {
        check();
    }
}

/// Reports corrupt index data through the installed report, else panics.
#[cold]
pub fn corrupt(message: impl std::fmt::Display) -> ! {
    let message = message.to_string();
    match CORRUPTION_REPORT.get() {
        Some(report) => report(message),
        None => panic!("{message}; REINDEX required"),
    }
}

/// The installed count of pages read or hit; zero without one.
#[inline]
pub fn blocks_used() -> i64 {
    BLOCKS_PROBE.get().map_or(0, |probe| probe())
}

/// Codec results from a source this code cannot name; prefer
/// [`codec_in`] where the source is known.
pub fn segment_error<T>(result: segment::Result<T>) -> T {
    result.unwrap_or_else(|error| corrupt(format!("Stannum index data: {error}")))
}

/// Codec results from the source named `label`.
pub fn codec_in<T>(result: segment::Result<T>, label: &str) -> T {
    result.unwrap_or_else(|error| corrupt(format!("Stannum {label}: {error}")))
}
