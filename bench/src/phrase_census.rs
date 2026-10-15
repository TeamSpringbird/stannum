// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A phrase's ranked walk timed per candidate step
//! ([`engine::tinshape::PhraseStep`]), to size what checking positions
//! before the common terms' buckets would save (`tnsreplay
//! --phrase-census`).
//!
//! Each candidate the walk stages is checked apart for the phrase first
//! (its reads untouched, its time left out), then timed in three parts:
//! staging up to its common terms' buckets (`pre`), from there to the end
//! of its staging (`common`), and its finishing (`finish`: length and
//! score, its own positions check apart as `check`). Under the reorder a
//! candidate that does not hold the phrase would stop after `pre` and a
//! check; one that does would pay the check before `common`.

use std::time::Instant;

use engine::tinshape::{Part, PhraseStep, Touch};
use rustc_hash::FxHashMap;

/// A candidate's times so far, in nanoseconds.
#[derive(Clone, Copy, Default)]
struct Cand {
    holds: bool,
    common: bool,
    /// Its own positions check's verdict, when it had one.
    checked: Option<bool>,
    finished: bool,
    at: u64,
    pre: u64,
    post: u64,
    finish: u64,
    check: u64,
}

/// Per class of candidate (whether it holds the phrase, whether it read
/// its common terms' buckets): how many, and their times.
#[derive(Clone, Copy, Default, Debug)]
pub struct Class {
    pub count: u64,
    pub finished: u64,
    pub checked: u64,
    pub pre: u64,
    pub post: u64,
    pub finish: u64,
    pub check: u64,
    /// Those whose own check failed after their common terms' buckets
    /// were read, and what those buckets and their finishing took.
    pub failed: u64,
    pub failed_ns: u64,
}

/// The census's tallies over every walk.
#[derive(Default, Debug)]
pub struct Tally {
    /// `[holds][common]`.
    pub classes: [[Class; 2]; 2],
    pub oracle_ns: u64,
    pub sieve_ns: u64,
    pub sieve_in: u64,
    /// The census passes' walks, and the plain ones, timed whole.
    pub walk_ns: u64,
    pub plain_ns: u64,
    pub queries: u64,
    /// A clock read's cost, measured once.
    pub clock_ns: f64,
    /// Clock reads the census took.
    pub reads: u64,
}

/// The census's [`Touch`]: no pages, every phrase step timed.
pub struct Census<'t> {
    pub tally: &'t mut Tally,
    epoch: Instant,
    cur: Option<(u32, u32, Cand)>,
    staged: FxHashMap<(u32, u32), Cand>,
    oracle_at: u64,
    sieve_at: u64,
}

impl<'t> Census<'t> {
    pub fn new(tally: &'t mut Tally) -> Self {
        Self {
            tally,
            epoch: Instant::now(),
            cur: None,
            staged: FxHashMap::default(),
            oracle_at: 0,
            sieve_at: 0,
        }
    }

    fn close(&mut self, c: Cand) {
        let class = &mut self.tally.classes[usize::from(c.holds)][usize::from(c.common)];
        class.count += 1;
        class.pre += c.pre;
        class.post += c.post;
        class.finish += c.finish;
        class.check += c.check;
        class.finished += u64::from(c.finished);
        class.checked += u64::from(c.checked.is_some());
        if c.checked == Some(false) && c.common {
            class.failed += 1;
            class.failed_ns += c.post + c.finish;
        }
    }

    /// The candidate at `(g, local)`: the one at hand, or a staged one.
    fn take(&mut self, g: u32, local: u32) -> Option<Cand> {
        match self.cur.take() {
            Some((cg, cl, c)) if (cg, cl) == (g, local) => Some(c),
            other => {
                self.cur = other;
                self.staged.remove(&(g, local))
            }
        }
    }

    /// Ends a walk: staged candidates never finished were cut.
    pub fn end(&mut self) {
        if let Some((_, _, c)) = self.cur.take() {
            self.close(c);
        }
        let left: Vec<Cand> = self.staged.drain().map(|(_, c)| c).collect();
        for c in left {
            self.close(c);
        }
    }
}

impl Touch for Census<'_> {
    #[inline]
    fn touch(&mut self, _: Part, _: usize, _: usize) {}

    #[inline]
    fn census(&self) -> bool {
        true
    }

    fn census_phrase(&mut self, g: u32, local: u32, step: PhraseStep) {
        let now = self.epoch.elapsed().as_nanos() as u64;
        self.tally.reads += 1;
        match step {
            PhraseStep::Oracle => self.oracle_at = now,
            PhraseStep::Stage { holds } => {
                self.tally.oracle_ns += now - self.oracle_at;
                if let Some((_, _, c)) = self.cur.take() {
                    self.close(c);
                }
                let c = Cand {
                    holds,
                    at: now,
                    ..Cand::default()
                };
                self.cur = Some((g, local, c));
            }
            PhraseStep::Common => {
                if let Some((_, _, c)) = &mut self.cur {
                    c.pre += now - c.at;
                    c.at = now;
                    c.common = true;
                }
            }
            PhraseStep::Staged { kept } => {
                let Some(mut c) = self.take(g, local) else {
                    return;
                };
                if c.common {
                    c.post += now - c.at;
                } else {
                    c.pre += now - c.at;
                }
                if kept {
                    self.staged.insert((g, local), c);
                } else {
                    self.close(c);
                }
            }
            PhraseStep::Finish => {
                if let Some(mut c) = self.take(g, local) {
                    c.at = now;
                    c.finished = true;
                    self.cur = Some((g, local, c));
                }
            }
            PhraseStep::Check => {
                if let Some((_, _, c)) = &mut self.cur {
                    c.finish += now - c.at;
                    c.at = now;
                }
            }
            PhraseStep::Checked { holds } => {
                if let Some((_, _, c)) = &mut self.cur {
                    c.check += now - c.at;
                    c.at = now;
                    c.checked = Some(holds);
                }
            }
            PhraseStep::Finished => {
                if let Some(mut c) = self.take(g, local) {
                    c.finish += now - c.at;
                    self.close(c);
                }
            }
            PhraseStep::Sieve { n } => {
                self.sieve_at = now;
                self.tally.sieve_in += u64::from(n);
            }
            PhraseStep::Sieved => self.tally.sieve_ns += now - self.sieve_at,
        }
    }
}

/// What a clock read costs, in nanoseconds.
pub fn clock_ns() -> f64 {
    let epoch = Instant::now();
    let n = 1_000_000u64;
    let mut sum = 0u64;
    for _ in 0..n {
        sum = sum.wrapping_add(epoch.elapsed().as_nanos() as u64);
    }
    std::hint::black_box(sum);
    epoch.elapsed().as_nanos() as f64 / n as f64
}

impl Tally {
    /// The census's report.
    pub fn report(&self) -> String {
        use std::fmt::Write as _;
        let ms = |ns: f64| ns / 1e6;
        let mut out = String::new();
        // The walk's own time: the census's less its apart checks and the
        // clock reads it added.
        let own = self.walk_ns as f64 - self.oracle_ns as f64 - self.reads as f64 * self.clock_ns;
        let _ = writeln!(
            out,
            "\nphrase census: {} queries; walks {:.1} ms plain, {:.1} ms with the census, \
             {:.1} ms of it the apart checks, {} clock reads at {:.1} ns: own {:.1} ms",
            self.queries,
            ms(self.plain_ns as f64),
            ms(self.walk_ns as f64),
            ms(self.oracle_ns as f64),
            self.reads,
            self.clock_ns,
            ms(own)
        );
        let _ = writeln!(
            out,
            "sieve {:.1} ms over {} candidates",
            ms(self.sieve_ns as f64),
            self.sieve_in
        );
        let _ = writeln!(
            out,
            "{:<14} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10} {:>9} {:>9} {:>8} {:>9}",
            "class",
            "cands",
            "finished",
            "checked",
            "pre ms",
            "common ms",
            "finish ms",
            "check ms",
            "ns/cand",
            "failed",
            "fail ms"
        );
        // A part's time less the clock read it holds.
        let fix = |ns: u64, parts: u64| ns as f64 - parts as f64 * self.clock_ns;
        for holds in [false, true] {
            for common in [false, true] {
                let c = &self.classes[usize::from(holds)][usize::from(common)];
                let all = fix(c.pre, c.count)
                    + fix(c.post, if common { c.count } else { 0 })
                    + fix(c.finish, c.finished);
                let _ = writeln!(
                    out,
                    "{:<14} {:>9} {:>9} {:>9} {:>9.1} {:>9.1} {:>10.1} {:>9.1} {:>9.0} {:>8} {:>9.1}",
                    format!(
                        "{}/{}",
                        if holds { "holds" } else { "not" },
                        if common { "common" } else { "rare" }
                    ),
                    c.count,
                    c.finished,
                    c.checked,
                    ms(fix(c.pre, c.count)),
                    ms(fix(c.post, if common { c.count } else { 0 })),
                    ms(fix(c.finish, c.finished)),
                    ms(fix(c.check, c.checked)),
                    all / c.count.max(1) as f64,
                    c.failed,
                    ms(c.failed_ns as f64)
                );
            }
        }
        let not = &self.classes[0];
        let wasted_common = fix(not[1].post, not[1].count) + fix(not[1].finish, not[1].finished);
        let failed = (0..2).map(|h| self.classes[h][1].failed_ns).sum::<u64>() as f64;
        let wasted_rare = fix(not[0].pre, not[0].count) + fix(not[0].finish, not[0].finished);
        let _ = writeln!(
            out,
            "not holding the phrase, from their common terms' buckets on: {:.1} ms ({:.1}% of \
             own); of it by those whose own check failed: {:.1} ms ({:.1}%); not holding, \
             with no common term, staged and finished: {:.1} ms ({:.1}%)",
            ms(wasted_common),
            100.0 * wasted_common / own,
            ms(failed),
            100.0 * failed / own,
            ms(wasted_rare),
            100.0 * wasted_rare / own
        );
        out
    }
}
