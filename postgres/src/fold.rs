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

use pgrx::pg_sys;
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

    pub fn is_visible(&self, block: u32) -> bool {
        self.visible
            .get(block as usize / 64)
            .is_some_and(|word| word >> (block % 64) & 1 == 1)
    }

    /// Reads the visibility map a page at a time: `visibilitymap_get_status`
    /// pins the map page covering a heap block, and the page's bits are then
    /// scanned a word at a time.
    ///
    /// As for an index-only scan, a bit read as set stays valid for this
    /// snapshot when it is cleared afterwards. A bit VACUUM set after the
    /// caller captured its index view is another matter: the view may still
    /// hold the tuples VACUUM removed. The caller must confirm afterwards that
    /// the view is still current (`storage::view_is_current`).
    ///
    /// # Safety
    /// `heap` is an open heap relation.
    pub unsafe fn read(heap: pg_sys::Relation) -> Self {
        // Two bits per heap block after the page header; the low bit of each
        // pair is all-visible.
        const HEADER: usize = 24;
        const LOW_BITS: u64 = 0x5555_5555_5555_5555;
        let blocks = unsafe {
            pg_sys::RelationGetNumberOfBlocksInFork(heap, pg_sys::ForkNumber::MAIN_FORKNUM)
        };
        let per_page = ((pg_sys::BLCKSZ as usize - HEADER) * 4) as u32;
        let mut visible = vec![0u64; (blocks as usize).div_ceil(64)];
        let mut vmbuf = pg_sys::InvalidBuffer as pg_sys::Buffer;
        let mut first = 0u32;
        while first < blocks {
            pgrx::check_for_interrupts!();
            let end = first.saturating_add(per_page).min(blocks);
            unsafe {
                pg_sys::visibilitymap_get_status(heap, first, &mut vmbuf);
                // The buffer stays invalid where the map has no page yet.
                if vmbuf != pg_sys::InvalidBuffer as pg_sys::Buffer
                    && pg_sys::BufferGetBlockNumber(vmbuf) == first / per_page
                {
                    let page = std::slice::from_raw_parts(
                        pg_sys::BufferGetPage(vmbuf).cast::<u8>().add(HEADER),
                        pg_sys::BLCKSZ as usize - HEADER,
                    );
                    let count = (end - first) as usize;
                    for (i, word) in page.chunks_exact(8).take(count.div_ceil(32)).enumerate() {
                        // 32 heap blocks per map word; pack their low bits.
                        let mut pairs = u64::from_le_bytes(word.try_into().unwrap()) & LOW_BITS;
                        let mut packed = 0u64;
                        if pairs == LOW_BITS {
                            packed = u64::from(u32::MAX);
                        } else {
                            let mut bit = 0;
                            while pairs != 0 {
                                packed |= (pairs & 1) << bit;
                                pairs >>= 2;
                                bit += 1;
                            }
                        }
                        let block = first as usize + i * 32;
                        let valid = (count - i * 32).min(32);
                        let packed = packed & (u64::MAX >> (64 - valid));
                        visible[block / 64] |= packed << (block % 64);
                        if block % 64 + valid > 64 {
                            visible[block / 64 + 1] |= packed >> (64 - block % 64);
                        }
                    }
                }
            }
            first = end;
        }
        if vmbuf != pg_sys::InvalidBuffer as pg_sys::Buffer {
            unsafe { pg_sys::ReleaseBuffer(vmbuf) };
        }
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

#[cfg(feature = "pg_test")]
thread_local! {
    /// Steps folds have spent clearing dead documents from their chunks:
    /// a word per step, a chunk's words at a time.
    static CLEAR_STEPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Steps this backend's folds have spent clearing dead documents.
#[cfg(feature = "pg_test")]
pub(crate) fn dead_clear_steps() -> u64 {
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
            #[cfg(feature = "pg_test")]
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
