// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Boolean counts: over a segment's ctid sets with [`engine::tinshape`]'s
//! fold where the query lowers to it ([`count_native`]), else over
//! segment-local document ordinals with [`engine::fold`]; and the heap's
//! visibility map both read.

use pgrx::pg_sys;
use segment::Tid;
use tinql::runtime::Query;

#[cfg(feature = "pg_test")]
pub(crate) use engine::fold::dead_clear_steps;
pub(crate) use engine::fold::{Visibility, count_segment, supported};

/// Counts the live matches of `query` in the view's immutable source `i`
/// through its ctid sets, a 256-page group at a time: members on pages
/// `visibility` saw all-visible are counted, the others handed to
/// `pending` a heap page at a time (ascending) for the heap to settle, as
/// [`count_segment`] does over ordinals. `None` when the query does not
/// lower to the ctid fold or the source has no ctid-native reader.
pub(crate) fn count_native(
    view: &crate::storage::View,
    i: usize,
    query: &Query,
    visibility: &Visibility,
    mut pending: impl FnMut(u32, &[u16]),
) -> Option<segment::Result<u64>> {
    use engine::tinshape::{NoTouch, Node, count_terms_visible, open_terms};
    fn spans(node: &Node) -> bool {
        match node {
            Node::Span { .. } => true,
            Node::Term(_) => false,
            Node::Not(inner) => spans(inner),
            Node::And(children) | Node::Or(children) => children.iter().any(spans),
        }
    }
    let mut names = Vec::new();
    let node = engine::tinshape::lower(query, &mut names)?;
    crate::storage::with_native(view, i, &names, spans(&node), |segment| {
        let geometry = &segment.docs.geometry;
        let mut terms = open_terms(segment, &names, &mut NoTouch)?;
        let mut hidden: Vec<Tid> = Vec::new();
        let total = count_terms_visible(
            segment,
            &node,
            &mut terms,
            &mut NoTouch,
            &mut |group, mask| {
                let g = &geometry.groups[group as usize];
                let bits = visibility.group_bits(g.id * segment::tinshape::docs::GROUP_PAGES);
                if bits == [u64::MAX; 4] {
                    return true;
                }
                // Each all-visible page's slots, a run of `width` bits.
                let width = usize::from(g.width);
                for (w, word) in bits.iter().enumerate() {
                    let mut word = *word;
                    while word != 0 {
                        let page = w * 64 + word.trailing_zeros() as usize;
                        set_range(mask, page * width, (page + 1) * width);
                        word &= word - 1;
                    }
                }
                let index = group as usize;
                segment
                    .docs
                    .group_words(index)
                    .iter()
                    .zip(mask.iter())
                    .all(|(docs, visible)| docs & !visible == 0)
            },
            &mut |group, words| {
                for (w, word) in words.iter().enumerate() {
                    let mut word = *word;
                    while word != 0 {
                        let local = w as u32 * 64 + word.trailing_zeros();
                        hidden.push(geometry.tid_in(group as usize, local));
                        word &= word - 1;
                    }
                }
            },
        )?;
        let mut offsets = Vec::new();
        for page in hidden.chunk_by(|a, b| a.block == b.block) {
            offsets.clear();
            offsets.extend(page.iter().map(|tid| tid.offset));
            pending(page[0].block, &offsets);
        }
        Ok(total)
    })
}

/// Sets bits `[from, to)` of `words`.
fn set_range(words: &mut [u64], from: usize, to: usize) {
    let mut at = from;
    while at < to {
        let bit = at % 64;
        let take = (64 - bit).min(to - at);
        let run = if take == 64 {
            u64::MAX
        } else {
            ((1u64 << take) - 1) << bit
        };
        words[at / 64] |= run;
        at += take;
    }
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
pub unsafe fn read_visibility(heap: pg_sys::Relation) -> Visibility {
    // Two bits per heap block after the page header; the low bit of each
    // pair is all-visible.
    const HEADER: usize = 24;
    const LOW_BITS: u64 = 0x5555_5555_5555_5555;
    let blocks =
        unsafe { pg_sys::RelationGetNumberOfBlocksInFork(heap, pg_sys::ForkNumber::MAIN_FORKNUM) };
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
    Visibility::from_bits(visible, blocks)
}
