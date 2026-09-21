// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Boolean counts over segment-local document ordinals.
//!
//! An `LSG4` segment stores each term's documents as ordinals into its
//! TID-ordered document table (see [`segment::ordinals`]). A Boolean count
//! folds those streams a 65,536-document chunk at a time and counts set bits,
//! so its work follows the chunks the terms occupy rather than the number of
//! matches. Dead documents are cleared from each chunk; matches on heap pages
//! that are not all-visible leave the population count and are handed to the
//! caller for the per-tuple check. A location is live in one source only
//! (the index checker reports anything else), so per-segment counts add up.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use pgrx::pg_sys;
use segment::index::Index;
use segment::ordinals::{self, Node, Words};
use segment::segment::PAGE_ENTRY;
use segment::set::Cursor as _;
use segment::{Result, Tid};
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

    fn is_visible(&self, block: u32) -> bool {
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
    /// no dead list was published since the view (`storage::view_is_current`).
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

/// Dead documents of a segment as ascending ordinals, per dead set.
type DeadOrdinals = (Rc<BTreeSet<Tid>>, Rc<Vec<u32>>);

thread_local! {
    /// By index identity and generation. The dead set is held so the pointer
    /// comparison cannot match a later allocation.
    static DEAD: RefCell<HashMap<(u64, u32), DeadOrdinals>> = RefCell::new(HashMap::new());
}

fn dead_ordinals(
    key: (u64, u32),
    source: &dyn Index,
    dead_set: &Rc<BTreeSet<Tid>>,
) -> Result<Rc<Vec<u32>>> {
    if dead_set.is_empty() {
        return Ok(Rc::default());
    }
    if let Some(found) = DEAD.with_borrow(|dead| {
        dead.get(&key)
            .filter(|(set, _)| Rc::ptr_eq(set, dead_set))
            .map(|(_, ordinals)| ordinals.clone())
    }) {
        return Ok(found);
    }
    let mut documents = source.documents()?;
    let mut ordinals = Vec::with_capacity(dead_set.len());
    for tid in dead_set.iter() {
        documents.seek(*tid)?;
        // A dead location the segment never held is harmless.
        if documents.current() == Some(*tid) {
            ordinals.push(documents.ordinal());
        }
    }
    let ordinals = Rc::new(ordinals);
    DEAD.with_borrow_mut(|dead| {
        if dead.len() > 4096 {
            dead.clear();
        }
        dead.insert(key, (dead_set.clone(), ordinals.clone()));
    });
    Ok(ordinals)
}

/// A segment's page table: heap blocks ascending with each block's first ordinal.
struct Pages<'a>(&'a [u8]);

impl Pages<'_> {
    fn len(&self) -> usize {
        self.0.len() / PAGE_ENTRY
    }

    fn block(&self, i: usize) -> u32 {
        let at = i * PAGE_ENTRY;
        u32::from_le_bytes(self.0[at..at + 4].try_into().unwrap())
    }

    /// The first ordinal of entry `i`; past the end, `documents`.
    fn first(&self, i: usize, documents: u32) -> u32 {
        if i >= self.len() {
            return documents;
        }
        let at = i * PAGE_ENTRY + 4;
        u32::from_le_bytes(self.0[at..at + 4].try_into().unwrap())
    }

    /// The entry of `block`, if the segment has documents on it.
    fn entry_for(&self, block: u32) -> Option<usize> {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let middle = (low + high) / 2;
            if self.block(middle) < block {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        (low < self.len() && self.block(low) == block).then_some(low)
    }

    /// The entry holding `ordinal`.
    fn entry_of(&self, ordinal: u32, documents: u32) -> usize {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let middle = (low + high) / 2;
            if self.first(middle, documents) <= ordinal {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low.saturating_sub(1)
    }
}

/// A chunk with at most this many matches looks each one's page up; a fuller
/// chunk walks the page table across it instead.
const SPARSE_CHUNK: u32 = 256;

/// Counts the live documents of one immutable segment matching `query`.
///
/// Returns the number of matches on all-visible pages. Matches elsewhere are
/// passed to `check` a heap page at a time, as the block and the matching
/// offsets, and are not included. `None` when the segment predates ordinal
/// streams, in which case nothing was counted or checked.
pub fn count_segment(
    key: (u64, u32),
    source: &dyn Index,
    dead_set: &Rc<BTreeSet<Tid>>,
    query: &Query,
    visibility: &Visibility,
    mut check: impl FnMut(u32, &[u16]),
) -> Result<Option<u64>> {
    let Some(pages) = source.page_table()? else {
        return Ok(None);
    };
    let pages = Pages(pages);
    let documents = source.document_count();
    let mut terms = Vec::new();
    let node = lower(query, &mut terms);
    let mut streams = Vec::with_capacity(terms.len());
    for term in terms {
        streams.push(match source.term(term)? {
            Some(found) => match found.ordinals()? {
                Some(stream) => Some(stream),
                None => return Err(segment::Error::Corrupt("term without an ordinal stream")),
            },
            None => None,
        });
    }
    let dead = dead_ordinals(key, source, dead_set)?;
    // The segment's page-table entries among a short list of blocks that are
    // not all-visible; ascending, like the ordinals they cover.
    let listed: Option<Vec<usize>> = visibility.few.as_ref().map(|few| {
        if pages.len() == 0 {
            return Vec::new();
        }
        let from = few.partition_point(|block| *block < pages.block(0));
        few[from..]
            .iter()
            .take_while(|block| **block <= pages.block(pages.len() - 1))
            .filter_map(|block| pages.entry_for(*block))
            .collect()
    });
    let mut tids = None;
    let mut offsets = Vec::new();
    let mut sure = 0u64;
    ordinals::for_each_chunk(&node, &streams, |chunk, words, members| {
        let low = u32::from(chunk) << 16;
        let high = low.saturating_add(ordinals::CHUNK).min(documents);
        let from = dead.partition_point(|ordinal| *ordinal < low);
        let to = from + dead[from..].partition_point(|ordinal| *ordinal < high);
        let live: Box<Words>;
        let words = if from == to {
            words
        } else {
            let mut cleared = Box::new(*words);
            for ordinal in &dead[from..to] {
                let bit = (ordinal - low) as usize;
                cleared[bit / 64] &= !(1 << (bit % 64));
            }
            live = cleared;
            &live
        };
        // Only a chunk with dead documents was changed since it was counted.
        let matched = if from == to {
            members
        } else {
            ordinals::count(words)
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
            let from = listed.partition_point(|entry| pages.first(entry + 1, documents) <= low);
            unchecked.extend(
                listed[from..]
                    .iter()
                    .take_while(|entry| pages.first(**entry, documents) < high),
            );
        } else if matched <= SPARSE_CHUNK {
            let mut members = Vec::with_capacity(matched as usize);
            ordinals::members(words, low, &mut members);
            for ordinal in members {
                let entry = pages.entry_of(ordinal, documents);
                if unchecked.last() != Some(&entry) && !visibility.is_visible(pages.block(entry)) {
                    unchecked.push(entry);
                }
            }
        } else {
            let mut entry = pages.entry_of(low, documents);
            while entry < pages.len() && pages.first(entry, documents) < high {
                if !visibility.is_visible(pages.block(entry)) {
                    unchecked.push(entry);
                }
                entry += 1;
            }
        }
        for entry in unchecked {
            let block = pages.block(entry);
            let first = pages.first(entry, documents).max(low);
            let end = pages.first(entry + 1, documents).min(high);
            let any = (first..end).any(|o| {
                let bit = (o - low) as usize;
                words[bit / 64] >> (bit % 64) & 1 == 1
            });
            if !any {
                continue;
            }
            // The k-th document of the block has ordinal `first of block + k`.
            let cursor = match &mut tids {
                Some(cursor) => cursor,
                None => tids.insert(source.documents()?),
            };
            cursor.seek(Tid { block, offset: 1 })?;
            offsets.clear();
            while let Some(tid) = cursor.current().filter(|tid| tid.block == block) {
                let ordinal = cursor.ordinal();
                if ordinal >= end {
                    break;
                }
                if ordinal >= first {
                    let bit = (ordinal - low) as usize;
                    if words[bit / 64] >> (bit % 64) & 1 == 1 {
                        offsets.push(tid.offset);
                    }
                }
                cursor.advance()?;
            }
            sure -= offsets.len() as u64;
            check(block, &offsets);
        }
        Ok(())
    })?;
    Ok(Some(sure))
}
