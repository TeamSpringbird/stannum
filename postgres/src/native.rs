// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Boolean matching over segments in TIN's shape, for the index scans.
//!
//! A query the native paths handle ([`engine::tinshape::lower`]) is folded
//! over an immutable segment's ctid sets a 256-page group at a time, dead
//! documents (the published dead list, as the segment's liveness) cleared
//! in the fold. A bitmap scan adds each group's matches as they come; a
//! plain scan's candidate stream keeps them as the group's words, a bit per
//! slot (about a fifth of a byte per document matched in the worst case)
//! and walks them in ctid order; a stream walked to k rows on the spot (a
//! zero fill) folds groups as it reaches them, one and then twice as many
//! each time, so its rows read the containers of the groups they are in,
//! not every group's. Wildcards, regexes, ranges and fuzzy terms are
//! expanded against each segment's term map first. Queries that still do
//! not lower, and the write buffer and sealed segments, are planned over
//! ordinal streams.

use std::rc::Rc;

use engine::tinshape::{
    NoTouch, Node, TermSet, fold_groups, for_each_match, lower, match_groups, open_terms, tids_in,
};
use segment::Tid;
use segment::pages::{self, Offsets, Page};
use segment::set;
use segment::tinshape::docs::DocSet;
use tinql::runtime::Query;

use crate::storage::{View, codec_in, with_native};

/// `queries` (clauses that must all hold) lowered over one table of terms.
#[derive(Clone)]
pub(crate) struct Lowered {
    pub(crate) node: Node,
    pub(crate) names: Vec<String>,
}

impl Lowered {
    pub(crate) fn of(queries: &[Query]) -> Option<Self> {
        let mut names = Vec::new();
        let mut nodes = queries
            .iter()
            .map(|query| lower(query, &mut names))
            .collect::<Option<Vec<_>>>()?;
        let node = if nodes.len() == 1 {
            nodes.pop().expect("one node")
        } else {
            Node::And(nodes)
        };
        Some(Self { node, names })
    }
}

/// Queries lowered once for every source, or per source where an expansion
/// needs the source's term map.
pub(crate) struct Lowering<'q> {
    queries: &'q [Query],
    plain: Option<Lowered>,
}

impl<'q> Lowering<'q> {
    pub(crate) fn new(queries: &'q [Query]) -> Self {
        Self {
            queries,
            plain: Lowered::of(queries),
        }
    }

    /// The queries lowered for source `i` of `view`; `None` when they do
    /// not lower there or the source is not a segment in TIN's shape.
    pub(crate) fn get(&self, view: &View, i: usize) -> Option<std::borrow::Cow<'_, Lowered>> {
        if let Some(plain) = &self.plain {
            return Some(std::borrow::Cow::Borrowed(plain));
        }
        if i >= view.immutable_sources {
            return None;
        }
        let index = &*view.sources[i].0;
        let limits = tinql::runtime::plan::Limits::default();
        let expanded = self
            .queries
            .iter()
            .map(|query| {
                tinql::runtime::plan::expand_terms(query, index, &limits)
                    .ok()
                    .flatten()
            })
            .collect::<Option<Vec<_>>>()?;
        Lowered::of(&expanded).map(std::borrow::Cow::Owned)
    }
}

/// Calls `visit` with source `i`'s live matches of `lowered` in ctid
/// order; `false` when the source has no native reader (the caller then
/// plans it over ordinal streams).
pub(crate) fn visit_matches(
    view: &View,
    i: usize,
    lowered: &Lowered,
    visit: &mut dyn FnMut(Tid),
) -> bool {
    let found = with_native(view, i, &lowered.names, |segment| {
        let mut terms = open_terms(segment, &lowered.names, &mut NoTouch)?;
        let geometry = &segment.docs.geometry;
        for_each_match(
            segment,
            &lowered.node,
            &mut terms,
            &mut NoTouch,
            &mut |group, words| {
                tids_in(geometry, group, words, &mut *visit);
            },
        )
    });
    match found {
        Some(result) => {
            codec_in(result, &view.labels[i]);
            true
        }
        None => false,
    }
}

/// Source `i`'s live matches of `lowered`, every group folded now, or with
/// `lazy` a few groups at a time as the walk reaches them; `None` when the
/// source has no native reader.
///
/// A lazy fold assembles the segment again when the walk gets to the end
/// of the last fold, which it cannot inside another walk over the segment
/// (see [`with_native`]). Only a caller that walks the matches before it
/// returns may ask for one: it reads them inside a walk only if it was
/// itself called inside it, and then the segment was busy (and read the
/// ordinal way) when the matches were asked for too. A scan's stream kept
/// between executor calls folds every group at once.
pub(crate) fn matches<'v>(
    view: &'v View,
    i: usize,
    lowered: &Lowered,
    lazy: bool,
) -> Option<Matches<'v>> {
    let found = with_native(view, i, &lowered.names, |segment| {
        let mut terms = open_terms(segment, &lowered.names, &mut NoTouch)?;
        let pending = match_groups(segment, &lowered.node, &mut terms, &mut NoTouch);
        let mut matches = Matches {
            view,
            source: i,
            lowered: lowered.clone(),
            pending,
            next: 0,
            take: if lazy { 1 } else { usize::MAX },
            docs: segment.docs.clone(),
            groups: Vec::new(),
            group: 0,
            bit: 0,
        };
        let Matches {
            lowered,
            pending,
            next,
            take,
            groups,
            ..
        } = &mut matches;
        *next += fold_some(
            segment,
            lowered,
            &mut terms,
            &pending[*next..],
            take,
            groups,
        )?;
        Ok(matches)
    })?;
    let mut matches = codec_in(found, &view.labels[i]);
    matches.settle();
    Some(matches)
}

thread_local! {
    static GROUPS_FOLDED: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

/// Groups the candidate streams' segments folded since the last reset
/// (EXPLAIN's Stream Groups Folded).
pub(crate) fn groups_folded() -> i64 {
    GROUPS_FOLDED.get()
}

pub(crate) fn reset_groups_folded() {
    GROUPS_FOLDED.set(0);
}

/// Folds `pending` groups into `groups` (cleared first) until `*take` of
/// them hold matches, then doubles `*take`: the number of groups folded.
fn fold_some<'a>(
    segment: &segment::tinshape::segment::Segment<'a>,
    lowered: &Lowered,
    terms: &mut [Option<TermSet<'a>>],
    pending: &[u32],
    take: &mut usize,
    groups: &mut Vec<(usize, Box<[u64]>)>,
) -> segment::Result<usize> {
    groups.clear();
    let wanted = *take;
    *take = take.saturating_mul(2);
    let folded = fold_groups(
        segment,
        &lowered.node,
        terms,
        &mut NoTouch,
        pending,
        &mut |group, words| {
            groups.push((group, Box::<[u64]>::from(words)));
            groups.len() < wanted
        },
    )?;
    GROUPS_FOLDED.set(GROUPS_FOLDED.get() + folded as i64);
    Ok(folded)
}

/// A segment's matches as group words, walked in ctid order: a
/// [`set::Cursor`] over documents and a [`pages::Cursor`] over pages.
///
/// Lazy matches ([`matches`]) fold groups when the walk reaches them: first
/// the first group holding any match, then twice as many groups holding
/// matches as the fold before, the segment assembled again for each fold.
/// A walk that stops after k rows (a zero fill of elided terms) reads the
/// containers of the groups it walked; a walk to the end folds every group
/// in about log2(groups) folds.
pub(crate) struct Matches<'v> {
    view: &'v View,
    source: usize,
    lowered: Lowered,
    /// The groups that may hold matches, ascending; `pending[next..]` are
    /// not folded yet.
    pending: Vec<u32>,
    next: usize,
    /// How many groups holding matches the next fold takes.
    take: usize,
    docs: Rc<DocSet>,
    /// The groups of the last fold that hold matches, and their words.
    groups: Vec<(usize, Box<[u64]>)>,
    /// The group and bit of the current match.
    group: usize,
    bit: usize,
}

impl Matches<'_> {
    /// Moves to the first set bit at or after the position, folding the
    /// next groups when the folded ones are walked.
    fn settle(&mut self) {
        loop {
            while let Some((_, words)) = self.groups.get(self.group) {
                let mut w = self.bit / 64;
                if w < words.len() {
                    let mut word = words[w] & (u64::MAX << (self.bit % 64));
                    loop {
                        if word != 0 {
                            self.bit = w * 64 + word.trailing_zeros() as usize;
                            return;
                        }
                        w += 1;
                        if w >= words.len() {
                            break;
                        }
                        word = words[w];
                    }
                }
                self.group += 1;
                self.bit = 0;
            }
            if self.next >= self.pending.len() {
                return;
            }
            self.fold();
        }
    }

    /// Folds the next groups (see [`fold_some`]) over the segment
    /// assembled again.
    fn fold(&mut self) {
        pgrx::check_for_interrupts!();
        let Matches {
            view,
            source,
            lowered,
            pending,
            next,
            take,
            groups,
            group,
            bit,
            ..
        } = self;
        let found = with_native(view, *source, &lowered.names, |segment| {
            let mut terms = open_terms(segment, &lowered.names, &mut NoTouch)?;
            fold_some(
                segment,
                lowered,
                &mut terms,
                &pending[*next..],
                take,
                groups,
            )
        });
        // The segment is busy only inside another walk over it, and lazy
        // matches are walked where they were asked for (see `matches`).
        let found = found.unwrap_or_else(|| {
            pgrx::error!("Stannum: {} is busy in another walk", view.labels[*source])
        });
        *next += codec_in(found, &view.labels[*source]);
        *group = 0;
        *bit = 0;
    }

    /// Passes over the groups not folded yet whose documents all come
    /// before `target`.
    fn skip_before(&mut self, target: Tid) {
        while let Some(&group) = self.pending.get(self.next) {
            let slots = self.docs.geometry.groups[group as usize].slots();
            if slots > 0 && self.tid(group as usize, slots as usize - 1) >= target {
                break;
            }
            self.next += 1;
        }
    }

    fn tid(&self, group: usize, bit: usize) -> Tid {
        self.docs.geometry.tid_in(group, bit as u32)
    }

    /// The bit of `tid` in group `group`'s words, or where it would be.
    fn bit_of(&self, group: usize, tid: Tid) -> usize {
        let geometry = &self.docs.geometry;
        let g = &geometry.groups[group];
        let first = g.id * 256 + u32::from(g.first);
        if tid.block < first {
            return 0;
        }
        let page = (tid.block - first) as usize;
        let starts = geometry.starts(group);
        if page >= usize::from(g.pages) {
            return g.slots() as usize;
        }
        let width = (starts[page + 1] - starts[page]) as usize;
        starts[page] as usize + usize::from(tid.offset.saturating_sub(1)).min(width)
    }

    /// The bits of the page holding bit `bit` of group `group`.
    fn page_bits(&self, group: usize, bit: usize) -> std::ops::Range<usize> {
        let geometry = &self.docs.geometry;
        let page = geometry.page_of(group, bit as u32);
        let starts = geometry.starts(group);
        starts[page] as usize..starts[page + 1] as usize
    }
}

impl set::Cursor for Matches<'_> {
    fn current(&self) -> Option<Tid> {
        let (group, _) = self.groups.get(self.group)?;
        Some(self.tid(*group, self.bit))
    }

    fn advance(&mut self) -> segment::Result<()> {
        if self.group < self.groups.len() {
            self.bit += 1;
            self.settle();
        }
        Ok(())
    }

    fn seek(&mut self, target: Tid) -> segment::Result<()> {
        while let Some(current) = set::Cursor::current(self) {
            if current >= target {
                break;
            }
            let (group, _) = &self.groups[self.group];
            let slots = self.docs.geometry.groups[*group].slots() as usize;
            let last = self.tid(*group, slots - 1);
            if target > last {
                self.group += 1;
                self.bit = 0;
                if self.group == self.groups.len() {
                    // Groups wholly before the target are not folded.
                    self.skip_before(target);
                }
            } else {
                self.bit = self.bit.max(self.bit_of(*group, target));
            }
            self.settle();
        }
        Ok(())
    }
}

impl pages::Cursor for Matches<'_> {
    fn current(&self) -> Option<Page> {
        let (group, words) = self.groups.get(self.group)?;
        let block = self.tid(*group, self.bit).block;
        let page = self.page_bits(*group, self.bit);
        let mut offsets = Offsets::default();
        for bit in self.bit..page.end {
            if words[bit / 64] >> (bit % 64) & 1 == 1 {
                offsets.insert((bit - page.start + 1) as u16);
            }
        }
        Some(Page { block, offsets })
    }

    fn advance(&mut self) -> segment::Result<()> {
        if let Some((group, _)) = self.groups.get(self.group) {
            self.bit = self.page_bits(*group, self.bit).end;
            self.settle();
        }
        Ok(())
    }
}
