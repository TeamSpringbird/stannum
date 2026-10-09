// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Boolean counts over segment-local document ordinals: the fold itself is
//! [`engine::fold`]; this reads the heap's visibility map for it.

use pgrx::pg_sys;

#[cfg(feature = "pg_test")]
pub(crate) use engine::fold::dead_clear_steps;
pub(crate) use engine::fold::{Visibility, count_segment, supported};

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
