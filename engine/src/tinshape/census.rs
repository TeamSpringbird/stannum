// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A census of window bounds (research round 4, direction 3), for offline
//! measurement only: the extension never enables the `census` feature, and
//! a build with it counts nothing unless `STANNUM_WIN_CENSUS` is set.
//!
//! A window is a run of 32, 64 or 128 groups by group index. A term's
//! bound over a window is the best of its group bounds there (a sparse
//! record's: of its footer blocks overlapping the window's slots; a common
//! term's: its record's). When a walk visits the first group of a window
//! it has not visited before, the census sums its scoring terms' window
//! bounds in the scorer's order and asks the walk's cut at the threshold
//! then: if the window is cut, every group the walk visits in it would be
//! skipped unread, and its group bound folds with it. The threshold only
//! rises, so every such group must be one the walk prunes itself; one it
//! does not is counted as a violation (there must be none).
//!
//! Folds are a grouped term's frontier folds (`Sc::bound_of`), counted for
//! the group being visited from its first bound to the next group's.

use std::cell::Cell;
use std::sync::OnceLock;

/// Window sizes as shifts of a group index: 32, 64 and 128 groups.
pub const SHIFTS: [u32; SIZES] = [5, 6, 7];
pub const SIZES: usize = 3;

/// What the census counted, summed over walks.
#[derive(Clone, Copy, Debug, Default)]
pub struct Tally {
    pub walks: u64,
    /// Groups the walks visited, the groups they pruned (by the group
    /// bound, or a disjunction's empty mask), and the grouped folds in
    /// each.
    pub groups: u64,
    pub pruned: u64,
    pub folds: u64,
    pub pruned_folds: u64,
    /// Per window size: window checks (windows entered), windows cut, and
    /// the groups and folds visited in cut windows.
    pub checks: [u64; SIZES],
    pub cut_windows: [u64; SIZES],
    pub cut_groups: [u64; SIZES],
    pub cut_folds: [u64; SIZES],
    /// Groups in a cut window the walk did not prune (must be none).
    pub violations: [u64; SIZES],
    /// A ceiling: the folds in windows the walk's final threshold cuts
    /// (what no check can know when it enters the window).
    pub final_folds: [u64; SIZES],
    /// The walks' grouped terms' directory entries, and their windows
    /// holding a group of theirs, per size: what window bounds would add.
    pub entries: u64,
    pub windows: [u64; SIZES],
}

impl Tally {
    pub fn add(&mut self, o: &Tally) {
        self.walks += o.walks;
        self.groups += o.groups;
        self.pruned += o.pruned;
        self.folds += o.folds;
        self.pruned_folds += o.pruned_folds;
        self.entries += o.entries;
        for k in 0..SIZES {
            self.checks[k] += o.checks[k];
            self.cut_windows[k] += o.cut_windows[k];
            self.cut_groups[k] += o.cut_groups[k];
            self.cut_folds[k] += o.cut_folds[k];
            self.violations[k] += o.violations[k];
            self.final_folds[k] += o.final_folds[k];
            self.windows[k] += o.windows[k];
        }
    }
}

thread_local! {
    static TALLY: Cell<Tally> = Cell::new(Tally::default());
}

/// This thread's tally since the last call, and a fresh one.
pub fn take() -> Tally {
    TALLY.with(Cell::take)
}

pub(super) fn add(t: &Tally) {
    TALLY.with(|cell| {
        let mut all = cell.get();
        all.add(t);
        cell.set(all);
    });
}

/// Whether `STANNUM_WIN_CENSUS` is set (read once).
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("STANNUM_WIN_CENSUS").is_some())
}

/// One walk's census.
pub(super) struct WalkCensus {
    /// Per scoring term, per window size, its bound per window (empty
    /// until the walk's first group).
    pub bounds: Vec<[Vec<f32>; SIZES]>,
    /// Per size, the window at hand and whether it was cut.
    pub current: [u32; SIZES],
    pub cut: [bool; SIZES],
    /// The group at hand's start: the scoring terms' folds and the walk's
    /// pruned groups then.
    pub open: Option<(u64, u64)>,
    /// Per size, the windows visited: (window, its first group visited,
    /// the folds in it).
    pub visited: [Vec<(u32, u32, u64)>; SIZES],
    pub tally: Tally,
}

impl WalkCensus {
    pub fn start() -> Option<Box<Self>> {
        enabled().then(|| {
            Box::new(Self {
                bounds: Vec::new(),
                current: [u32::MAX; SIZES],
                cut: [false; SIZES],
                open: None,
                visited: Default::default(),
                tally: Tally::default(),
            })
        })
    }

    /// Closes the group at hand: `folds` and `pruned` are the walk's
    /// counts now.
    pub fn close(&mut self, folds: u64, pruned: u64) {
        let Some((f0, p0)) = self.open.take() else {
            return;
        };
        let folds = folds - f0;
        let pruned = pruned > p0;
        let t = &mut self.tally;
        t.groups += 1;
        t.folds += folds;
        if pruned {
            t.pruned += 1;
            t.pruned_folds += folds;
        }
        for k in 0..SIZES {
            if let Some(v) = self.visited[k].last_mut() {
                v.2 += folds;
            }
            if self.cut[k] {
                t.cut_groups[k] += 1;
                t.cut_folds[k] += folds;
                if !pruned {
                    t.violations[k] += 1;
                }
            }
        }
    }
}
