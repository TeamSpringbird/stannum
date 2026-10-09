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
//! and walks them in ctid order. Wildcards, regexes, ranges and fuzzy terms
//! are expanded against each segment's term map first. Queries that still
//! do not lower, and the write buffer and sealed segments, are planned over
//! ordinal streams.

use std::rc::Rc;

use engine::tinshape::{NoTouch, Node, for_each_match, lower, open_terms, tids_in};
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

/// Source `i`'s live matches of `lowered`, held a group at a time; `None`
/// when the source has no native reader.
pub(crate) fn matches(view: &View, i: usize, lowered: &Lowered) -> Option<Matches> {
    let found = with_native(view, i, &lowered.names, |segment| {
        let mut terms = open_terms(segment, &lowered.names, &mut NoTouch)?;
        let mut groups = Vec::new();
        for_each_match(
            segment,
            &lowered.node,
            &mut terms,
            &mut NoTouch,
            &mut |group, words| {
                groups.push((group, Box::<[u64]>::from(words)));
            },
        )?;
        Ok(Matches {
            docs: segment.docs.clone(),
            groups,
            group: 0,
            bit: 0,
        })
    })?;
    let mut matches = codec_in(found, &view.labels[i]);
    matches.settle();
    Some(matches)
}

/// A segment's matches as group words, walked in ctid order: a
/// [`set::Cursor`] over documents and a [`pages::Cursor`] over pages.
pub(crate) struct Matches {
    docs: Rc<DocSet>,
    groups: Vec<(usize, Box<[u64]>)>,
    /// The group and bit of the current match.
    group: usize,
    bit: usize,
}

impl Matches {
    /// Moves to the first set bit at or after the position.
    fn settle(&mut self) {
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
    }

    fn tid(&self, group: usize, bit: usize) -> Tid {
        self.docs.geometry.tid_in(group, bit as u32)
    }

    /// The bit of `tid` in group `group`'s words, or where it would be.
    fn bit_of(&self, group: usize, tid: Tid) -> usize {
        let g = &self.docs.geometry.groups[group];
        let first = g.id * 256 + u32::from(g.first);
        if tid.block < first {
            return 0;
        }
        let page = (tid.block - first) as usize;
        let width = usize::from(g.width);
        page * width + usize::from(tid.offset.saturating_sub(1)).min(width)
    }
}

impl set::Cursor for Matches {
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
            let (group, words) = &self.groups[self.group];
            let last = self.tid(*group, words.len() * 64 - 1);
            if target > last {
                self.group += 1;
                self.bit = 0;
            } else {
                self.bit = self.bit.max(self.bit_of(*group, target));
            }
            self.settle();
        }
        Ok(())
    }
}

impl pages::Cursor for Matches {
    fn current(&self) -> Option<Page> {
        let (group, words) = self.groups.get(self.group)?;
        let width = usize::from(self.docs.geometry.groups[*group].width);
        let block = self.tid(*group, self.bit).block;
        let page_start = self.bit / width * width;
        let mut offsets = Offsets::default();
        for bit in self.bit..page_start + width {
            if words[bit / 64] >> (bit % 64) & 1 == 1 {
                offsets.insert((bit - page_start + 1) as u16);
            }
        }
        Some(Page { block, offsets })
    }

    fn advance(&mut self) -> segment::Result<()> {
        if let Some((group, _)) = self.groups.get(self.group) {
            let width = usize::from(self.docs.geometry.groups[*group].width);
            self.bit = (self.bit / width + 1) * width;
            self.settle();
        }
        Ok(())
    }
}
