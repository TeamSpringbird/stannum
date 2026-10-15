// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Boolean counts over segment-local document ordinals.
//!
//! A segment stores each term's documents as ordinals into its
//! TID-ordered document table (see [`segment::ordinals`]). A Boolean count
//! folds those streams a 65,536-document chunk at a time and counts set bits,
//! so its work follows the chunks the terms occupy rather than the number of
//! matches. Dead documents are cleared from each chunk; matches on heap pages
//! that are not all-visible leave the population count and are handed to the
//! caller for the per-tuple check. A location is live in one source only
//! (the index checker reports anything else), so per-segment counts add up.

use segment::Result;
use segment::dead::DeadDocs;
use segment::index::Index;
use segment::ordinals::{self, Node, Words};
use tinql::runtime::Query;

/// Whether every node is a Boolean combination of plain terms.
pub fn supported(query: &Query) -> bool {
    match query {
        Query::Term(_) => true,
        Query::And(a, b) | Query::Or(a, b) => supported(a) && supported(b),
        Query::Conjunction(children)
        | Query::Disjunction { min: 1, children }
        | Query::AtLeast { min: 1, children } => children.iter().all(supported),
        Query::Boost { inner, .. } => supported(inner),
        _ => false,
    }
}

/// The query as a tree over indexes into `terms`, which collects each
/// distinct term once.
fn lower<'q>(query: &'q Query, terms: &mut Vec<&'q str>) -> Node {
    match query {
        Query::Term(term) => {
            let at = terms
                .iter()
                .position(|known| known == term)
                .unwrap_or_else(|| {
                    terms.push(term);
                    terms.len() - 1
                });
            Node::Term(at)
        }
        Query::Or(a, b) => Node::Or(vec![lower(a, terms), lower(b, terms)]),
        Query::And(a, b) => Node::And(vec![lower(a, terms), lower(b, terms)]),
        Query::Disjunction { children, .. } | Query::AtLeast { children, .. } => {
            Node::Or(children.iter().map(|child| lower(child, terms)).collect())
        }
        Query::Conjunction(children) => {
            Node::And(children.iter().map(|child| lower(child, terms)).collect())
        }
        Query::Boost { inner, .. } => lower(inner, terms),
        _ => unreachable!("fold::supported admits only Boolean terms"),
    }
}

/// The all-visible bit of every heap block, read once per count.
pub struct Visibility {
    /// Bit `block` set: the page is all-visible.
    visible: Vec<u64>,
    /// Every block is all-visible, so no match needs the heap.
    pub all: bool,
    /// The blocks that are not all-visible, ascending, when they are few: a
    /// vacuumed table keeps a handful, such as its last pages, and a count
    /// should pay for those rather than test every page it matches on.
    few: Option<Vec<u32>>,
}

/// More blocks than this are found by testing the pages a chunk covers.
const FEW_BLOCKS: u64 = 512;

impl Visibility {
    /// No page is trusted: every match is checked against the heap.
    pub fn none() -> Self {
        Self {
            visible: Vec::new(),
            all: false,
            few: None,
        }
    }

    /// Every page is all-visible: no match needs the heap, as for a table
    /// read outside a server, where there is no heap to check.
    pub fn all_visible() -> Self {
        Self {
            visible: Vec::new(),
            all: true,
            few: None,
        }
    }

    /// The all-visible bits of the 256 heap blocks from `first`, a multiple
    /// of 256, as four words (bit `i` of word `w`: block `first + 64w + i`);
    /// blocks past the map read as not all-visible.
    pub fn group_bits(&self, first: u32) -> [u64; 4] {
        if self.all {
            return [u64::MAX; 4];
        }
        let base = first as usize / 64;
        std::array::from_fn(|i| self.visible.get(base + i).copied().unwrap_or(0))
    }

    pub fn is_visible(&self, block: u32) -> bool {
        self.visible
            .get(block as usize / 64)
            .is_some_and(|word| word >> (block % 64) & 1 == 1)
    }

    /// The visibility of `blocks` heap blocks from their all-visible bits,
    /// bit `block` of `visible` set where the page is all-visible.
    pub fn from_bits(visible: Vec<u64>, blocks: u32) -> Self {
        let set: u64 = visible.iter().map(|w| u64::from(w.count_ones())).sum();
        let few = (u64::from(blocks) - set <= FEW_BLOCKS).then(|| {
            let mut few = Vec::new();
            for (i, word) in visible.iter().enumerate() {
                let mut clear = !word;
                while clear != 0 {
                    let block = i as u32 * 64 + clear.trailing_zeros();
                    if block < blocks {
                        few.push(block);
                    }
                    clear &= clear - 1;
                }
            }
            few
        });
        Self {
            all: set == u64::from(blocks),
            visible,
            few,
        }
    }
}

#[cfg(feature = "test-hooks")]
thread_local! {
    /// Steps folds have spent clearing dead documents from their chunks:
    /// a word per step, a chunk's words at a time.
    static CLEAR_STEPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Steps this backend's folds have spent clearing dead documents.
#[cfg(feature = "test-hooks")]
pub fn dead_clear_steps() -> u64 {
    CLEAR_STEPS.get()
}

/// A chunk with at most this many matches looks each one's page up; a fuller
/// chunk walks the page table across it instead.
const SPARSE_CHUNK: u32 = 256;

/// Counts the live documents of one immutable segment matching `query`.
///
/// Returns the number of matches on all-visible pages. Matches elsewhere are
/// passed to `check` a heap page at a time, as the block and the matching
/// offsets, and are not included. `None` when the segment predates ordinal
/// streams, in which case nothing was counted or checked. `dead` is the
/// segment's decoded dead list, as the view holds it.
pub fn count_segment(
    source: &dyn Index,
    dead: &DeadDocs,
    query: &Query,
    visibility: &Visibility,
    mut check: impl FnMut(u32, &[u16]),
) -> Result<Option<u64>> {
    let docs = source.doc_table()?;
    let pages = *docs.pages();
    let documents = source.document_count();
    let mut terms = Vec::new();
    let node = lower(query, &mut terms);
    let mut streams = Vec::with_capacity(terms.len());
    for term in terms {
        streams.push(match source.term(term)? {
            Some(found) => Some(found.ordinals()?),
            None => None,
        });
    }
    // The segment's page-table entries among a short list of blocks that are
    // not all-visible; ascending, like the ordinals they cover.
    let listed: Option<Vec<usize>> = visibility.few.as_ref().map(|few| {
        if pages.is_empty() {
            return Vec::new();
        }
        let from = few.partition_point(|block| *block < pages.block(0));
        few[from..]
            .iter()
            .take_while(|block| **block <= pages.block(pages.len() - 1))
            .filter_map(|block| pages.find(*block).ok())
            .collect()
    });
    let mut offsets = Vec::new();
    let mut sure = 0u64;
    ordinals::for_each_chunk(&node, &streams, |chunk, words, members| {
        let low = u32::from(chunk) << 16;
        let high = low.saturating_add(ordinals::CHUNK).min(documents);
        let live: Box<Words>;
        let has_dead = dead.chunk(low).is_some();
        let words = if has_dead {
            let mut cleared = Box::new(*words);
            dead.clear(low, &mut cleared);
            #[cfg(feature = "test-hooks")]
            CLEAR_STEPS.set(CLEAR_STEPS.get() + ordinals::WORDS as u64);
            live = cleared;
            &live
        } else {
            words
        };
        // Only a chunk with dead documents was changed since it was counted.
        let matched = if has_dead {
            ordinals::count(words)
        } else {
            members
        };
        if matched == 0 {
            return Ok(());
        }
        sure += u64::from(matched);
        if visibility.all {
            return Ok(());
        }
        // The heap pages to check: those holding a match and not all-visible.
        let mut unchecked: Vec<usize> = Vec::new();
        if let Some(listed) = &listed {
            let from = listed.partition_point(|entry| pages.end(*entry) <= low);
            unchecked.extend(
                listed[from..]
                    .iter()
                    .take_while(|entry| pages.first(**entry) < high),
            );
        } else if matched <= SPARSE_CHUNK {
            let mut members = Vec::with_capacity(matched as usize);
            ordinals::members(words, low, &mut members);
            for ordinal in members {
                let entry = pages
                    .entry_of(ordinal)
                    .ok_or(segment::Error::Corrupt("ordinal beyond the document table"))?;
                if unchecked.last() != Some(&entry) && !visibility.is_visible(pages.block(entry)) {
                    unchecked.push(entry);
                }
            }
        } else {
            let mut entry = pages.entry_of(low).unwrap_or(pages.len());
            while entry < pages.len() && pages.first(entry) < high {
                if !visibility.is_visible(pages.block(entry)) {
                    unchecked.push(entry);
                }
                entry += 1;
            }
        }
        for entry in unchecked {
            let block = pages.block(entry);
            let first = pages.first(entry).max(low);
            let end = pages.end(entry).min(high);
            let any = (first..end).any(|o| {
                let bit = (o - low) as usize;
                words[bit / 64] >> (bit % 64) & 1 == 1
            });
            if !any {
                continue;
            }
            // The k-th document of the block has ordinal `first of block + k`.
            let mut resolver = docs.resolver();
            offsets.clear();
            for ordinal in first..end {
                let bit = (ordinal - low) as usize;
                if words[bit / 64] >> (bit % 64) & 1 == 1 {
                    offsets.push(resolver.tid_at(ordinal)?.offset);
                }
            }
            sure -= offsets.len() as u64;
            check(block, &offsets);
        }
        Ok(())
    })?;
    Ok(Some(sure))
}
