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

// ---------------------------------------------------------------------------
// The sub-range census (lane subrange, 2026-10-16), for offline measurement
// only, like the window census above: counts nothing unless
// `STANNUM_SUB_CENSUS` is set.
//
// A led walk's group, once its required terms are intersected, is cut into
// sub-ranges of 64, 256 or 1,024 local slots. Each scoring term holding the
// group as a grid gets a bound per sub-range from what a stored code would
// say of its postings there; any other term keeps its bound over the group.
// The bounds are summed in the scorer's order and asked of the walk's cut at
// the threshold when the group's candidates are listed. A cut sub-range's
// candidates could be dropped unread: no bucket, class or length read.
//
// Encodings (a term's bound per sub-range, each also capped by its group
// bound):
// - `st`: the sub-range's own frontier (per bucket, the shortest
//   `round_length`-rounded length of a posting at that bucket or above;
//   R10's codes): exact up to the rounding.
// - `bl`: its largest bucket at its shortest rounded length (one bucket and
//   one length code).
// - `mb`: its largest bucket alone, bounded by the group's frontier at that
//   bucket and below (4 bits).
// - `pr`: R10's stored form: per pair of the group's frontier, the shortest
//   rounded length of the sub-range's postings whose bucket lies above the
//   pair before's, bounded at the pair's bucket.
//
// With `STANNUM_SUB_SKIP=SIZE:ENC` (e.g. `256:st`) the walk drops the cut
// sub-ranges' candidates before its sieve, and re-checks each candidate's
// sub-range as the threshold rises: the answers must not change, and the
// replay's own counters say what the skip saved.

/// Sub-range sizes as shifts of a local slot: 64, 256 and 1,024 slots.
pub const SUB_SHIFTS: [u32; SUBS] = [6, 8, 10];
pub const SUBS: usize = 3;
pub const ENCS: usize = 4;
pub const ENC_NAMES: [&str; ENCS] = ["st", "bl", "mb", "pr"];

/// What the sub-range census counted, summed over walks.
#[derive(Clone, Copy, Debug, Default)]
pub struct SubTally {
    pub walks: u64,
    /// Groups whose candidates were listed with scoring terms, and their
    /// candidates.
    pub groups: u64,
    pub cands: u64,
    /// Scoring terms' (term, group) pairs there: held as a grid (bounded
    /// per sub-range) or not (bounded by the group).
    pub grid_terms: u64,
    pub other_terms: u64,
    /// Per size: sub-ranges holding a candidate; per size and encoding,
    /// those cut, their candidates, and groups whose every candidate is cut.
    pub subs: [u64; SUBS],
    pub cut_subs: [[u64; ENCS]; SUBS],
    pub cut_cands: [[u64; ENCS]; SUBS],
    pub cut_groups: [[u64; ENCS]; SUBS],
    /// With a skip: candidates dropped when listed, and later as the
    /// threshold rose.
    pub skipped: u64,
    pub skipped_late: u64,
}

impl SubTally {
    pub fn add(&mut self, o: &SubTally) {
        self.walks += o.walks;
        self.groups += o.groups;
        self.cands += o.cands;
        self.grid_terms += o.grid_terms;
        self.other_terms += o.other_terms;
        self.skipped += o.skipped;
        self.skipped_late += o.skipped_late;
        for k in 0..SUBS {
            self.subs[k] += o.subs[k];
            for e in 0..ENCS {
                self.cut_subs[k][e] += o.cut_subs[k][e];
                self.cut_cands[k][e] += o.cut_cands[k][e];
                self.cut_groups[k][e] += o.cut_groups[k][e];
            }
        }
    }
}

thread_local! {
    static SUB_TALLY: Cell<SubTally> = Cell::new(SubTally::default());
}

/// This thread's sub-range tally since the last call, and a fresh one.
pub fn take_sub() -> SubTally {
    SUB_TALLY.with(Cell::take)
}

pub(super) fn add_sub(t: &SubTally) {
    SUB_TALLY.with(|cell| {
        let mut all = cell.get();
        all.add(t);
        cell.set(all);
    });
}

/// Whether `STANNUM_SUB_CENSUS` is set (read once).
pub fn sub_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("STANNUM_SUB_CENSUS").is_some())
}

/// `STANNUM_SUB_SKIP=SIZE:ENC` as (size index, encoding index), read once.
pub fn sub_skip() -> Option<(usize, usize)> {
    static SKIP: OnceLock<Option<(usize, usize)>> = OnceLock::new();
    *SKIP.get_or_init(|| {
        let v = std::env::var("STANNUM_SUB_SKIP").ok()?;
        let (size, enc) = v.split_once(':')?;
        let size: u32 = size.parse().ok()?;
        let k = SUB_SHIFTS.iter().position(|s| 1u32 << s == size)?;
        let e = ENC_NAMES.iter().position(|n| *n == enc)?;
        Some((k, e))
    })
}

/// `STANNUM_SUB_DF`: the least document frequency, as a fraction of the
/// segment's documents, of a term whose grids carry codes (0 by default).
pub fn sub_df() -> f64 {
    static DF: OnceLock<f64> = OnceLock::new();
    *DF.get_or_init(|| {
        std::env::var("STANNUM_SUB_DF")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0)
    })
}

/// One walk's sub-range census and its scratch.
#[derive(Default)]
pub(super) struct SubWalk {
    pub tally: SubTally,
    /// Per scoring term and 64-slot chunk of the group at hand, the
    /// shortest rounded length per bucket (`u32::MAX` for none).
    pub stairs: Vec<[u32; 16]>,
    /// Per scoring term: its bound over the group when it is not bounded
    /// per sub-range; its group bound; and per bucket the best of its group
    /// frontier's bounds at that bucket and below.
    pub fixed: Vec<Option<f32>>,
    pub gbound: Vec<f32>,
    pub mb: Vec<[f32; 16]>,
    /// Per scoring term, its group frontier's buckets (ascending) and how
    /// many.
    pub pairs: Vec<([u8; 16], usize)>,
    /// The skip's group (plus one), and per sub-range its bound and
    /// whether it was cut.
    pub skip_g: u32,
    pub skip_sums: Vec<f32>,
    pub skip_cut: Vec<bool>,
}

impl SubWalk {
    pub fn start() -> Option<Box<Self>> {
        sub_enabled().then(|| Box::new(Self::default()))
    }
}

/// A skip's cuts recorded in one pass of a query (its answer pass) and
/// replayed in the next (its timed passes), so a timed pass pays only a
/// lookup per group, near what reading stored codes would cost: per
/// (segment, group), the sub-ranges' shift, bounds and cuts.
pub type Recorded = rustc_hash::FxHashMap<(usize, u32), (u32, Vec<f32>, Vec<bool>)>;

/// Whether the census records cuts (0), replays them (1) or neither (2).
pub const RECORD: u8 = 0;
pub const REPLAY: u8 = 1;
pub const OFF: u8 = 2;

thread_local! {
    pub(super) static MODE: Cell<u8> = const { Cell::new(RECORD) };
    pub(super) static RECORDED: std::cell::RefCell<Recorded> = std::cell::RefCell::new(Recorded::default());
}

/// Starts recording a query's cuts (clearing the last query's).
pub fn record() {
    MODE.with(|m| m.set(RECORD));
    RECORDED.with(|r| r.borrow_mut().clear());
}

/// Replays the cuts recorded since [`record`].
pub fn replay() {
    MODE.with(|m| m.set(REPLAY));
}

pub(super) fn mode() -> u8 {
    MODE.with(Cell::get)
}

/// Lab counters for lane lightprobe (research6 L3): what the disjunction
/// loads per group by the terms' roles. Offline only.
pub static LP: [std::sync::atomic::AtomicU64; 64] = [const { std::sync::atomic::AtomicU64::new(0) }; 64];

pub fn lp_add(i: usize, v: u64) {
    LP[i].fetch_add(v, std::sync::atomic::Ordering::Relaxed);
}

thread_local! {
    pub static LP_KIND: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub fn lp_kind(i: usize, k: u8) {
    LP_KIND.with(|v| {
        let mut v = v.borrow_mut();
        if v.len() <= i {
            v.resize(i + 1, 0);
        }
        v[i] = k;
    });
}

pub fn lp_dump() {
    let g = |i: usize| LP[i].load(std::sync::atomic::Ordering::Relaxed);
    let groups = g(0).max(1) as f64;
    println!("lpcensus groups {} counted {} with_S {}", g(0), g(1), g(38));
    for (c, name) in ["M(mask)", "H(heavy)", "S(rest)"].iter().enumerate() {
        let b = 8 + c * 8;
        println!(
            "lpcensus {name}: terms/group {:.2} postings/group {:.1} grid {} ef {} array {} paged {} locals {} sparse {}",
            g(b) as f64 / groups,
            g(b + 1) as f64 / groups,
            g(b + 2),
            g(b + 3),
            g(b + 4),
            g(b + 5),
            g(b + 6),
            g(b + 7)
        );
    }
    println!(
        "lpcensus per group: after count {:.1}, exact pass {:.2}, slack pass {:.2}, candidates {:.2}, probes(slack x S) {:.2}",
        g(32) as f64 / groups,
        g(33) as f64 / groups,
        g(34) as f64 / groups,
        g(35) as f64 / groups,
        g(37) as f64 / groups
    );
    println!(
        "lpcensus groups: exact>0 {:.1}%, slack>0 {:.1}%; S terms skippable lazily {:.1}% (of S terms), S postings skippable {:.1}%",
        100.0 * g(39) as f64 / groups,
        100.0 * g(40) as f64 / groups,
        100.0 * g(42) as f64 / g(24).max(1) as f64,
        100.0 * g(41) as f64 / g(43).max(1) as f64
    );
}
