// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A segment blob served as the extension serves a run: in pages of
//! [`PAGE_DATA`] bytes, copied for plain reads and held in place within hold
//! spans, so the segment reader and the walk take the same paths they take
//! over shared buffers. Every page a read copies or a held span or range
//! pins is a touch, as a `ReadBuffer` is in PostgreSQL, counted per query
//! by the area of the blob the read was for (see [`Area`]).

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use rustc_hash::FxHashSet;
use segment::source::{HELD_PIECES, HELD_SLOTS, HeldRange, HeldSpan, Source};
use segment::{Error, Result};

use crate::areas::{AREAS, Area, AreaMap};

/// Bytes of blob data per run page: an 8 KiB page less its header, its
/// special area and the chain pointer (`CHAIN_CAPACITY` in the extension's
/// `storage/layout.rs`).
pub const PAGE_DATA: usize = 8192 - 24 - 8 - 4;

/// Slots a source hands out within a hold span (`HELD_SLOT_LIMIT` in the
/// extension's storage).
const HELD_SLOT_LIMIT: usize = 64;

/// Page touches of one query, per area: every access (a copy of a page or a
/// new pin) and the distinct pages accessed.
#[derive(Clone, Debug, Default)]
pub struct Touches {
    pub accesses: [u64; AREAS],
    pub distinct: [u64; AREAS],
    /// Distinct pages over every area.
    pub pages: u64,
    seen: FxHashSet<(u32, u32, u8)>,
    seen_any: FxHashSet<(u32, u32)>,
}

impl Touches {
    fn note(&mut self, blob: u32, page: u32, area: Area) {
        let a = area as usize;
        self.accesses[a] += 1;
        if self.seen.insert((blob, page, area as u8)) {
            self.distinct[a] += 1;
        }
        if self.seen_any.insert((blob, page)) {
            self.pages += 1;
        }
    }

    /// Accesses over every area.
    pub fn total_accesses(&self) -> u64 {
        self.accesses.iter().sum()
    }
}

thread_local! {
    static TOUCHES: RefCell<Touches> = RefCell::new(Touches::default());
    static HOLD_SPANS: Cell<u64> = const { Cell::new(0) };
}

/// The touches since the last call, which resets them.
pub fn take_touches() -> Touches {
    TOUCHES.take()
}

/// Page accesses since the touches were last taken, over every area: the
/// replay's count of pages read or hit, as `pgBufferUsage` is the server's.
pub fn touches_so_far() -> u64 {
    TOUCHES.with_borrow(Touches::total_accesses)
}

/// Notes `pages` pages of `area` touched for a blob read outside a segment
/// reader (a dead list decoded for a cold query).
pub fn note_pages(blob: u32, pages: u32, area: Area) {
    TOUCHES.with_borrow_mut(|touches| {
        for page in 0..pages {
            touches.note(blob, page, area);
        }
    });
}

/// One blob as a paged, holdable source.
pub struct PagedSource {
    /// Distinguishes the blob's pages from another's in the counts.
    id: u32,
    bytes: Arc<Vec<u8>>,
    areas: Arc<AreaMap>,
    holding: Cell<u32>,
    generation: Cell<u64>,
    /// Per slot, the pages it holds.
    slots: RefCell<Vec<[Option<u32>; HELD_PIECES]>>,
}

impl PagedSource {
    pub fn new(id: u32, bytes: Arc<Vec<u8>>, areas: Arc<AreaMap>) -> Self {
        Self {
            id,
            bytes,
            areas,
            holding: Cell::new(0),
            generation: Cell::new(0),
            slots: RefCell::new(vec![[None; HELD_PIECES]; HELD_SLOTS]),
        }
    }

    fn total(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn page_bytes(&self, page: u32) -> (*const u8, usize) {
        let start = page as usize * PAGE_DATA;
        let len = PAGE_DATA.min(self.bytes.len() - start);
        // SAFETY: `start` lies within the blob, which `self` keeps alive.
        (unsafe { self.bytes.as_ptr().add(start) }, len)
    }

    /// Notes a touch of `page` for the read starting at blob offset `at`.
    fn touch(&self, page: u32, at: u64) {
        let area = self.areas.area_of(at);
        TOUCHES.with_borrow_mut(|touches| touches.note(self.id, page, area));
    }
}

fn check(total: u64, offset: u64, len: usize) -> Result<u64> {
    let end = offset.checked_add(len as u64).ok_or(Error::Truncated)?;
    if end > total {
        return Err(Error::Truncated);
    }
    Ok(end)
}

impl Source for PagedSource {
    fn len(&self) -> u64 {
        self.total()
    }

    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let end = check(self.total(), offset, len)?;
        let mut at = offset;
        while at < end {
            let page = (at / PAGE_DATA as u64) as u32;
            self.touch(page, at);
            at = (u64::from(page) + 1) * PAGE_DATA as u64;
        }
        Ok(self.bytes[offset as usize..end as usize].to_vec())
    }

    fn hold(&self, open: bool) {
        if open {
            if self.holding.get() == 0 {
                let generation = HOLD_SPANS.get() + 1;
                HOLD_SPANS.set(generation);
                self.generation.set(generation);
            }
            self.holding.set(self.holding.get() + 1);
        } else {
            let depth = self.holding.get().saturating_sub(1);
            self.holding.set(depth);
            if depth == 0 {
                let mut slots = self.slots.borrow_mut();
                slots
                    .iter_mut()
                    .for_each(|slot| *slot = [None; HELD_PIECES]);
                slots.truncate(HELD_SLOTS);
            }
        }
    }

    fn hold_generation(&self) -> u64 {
        self.generation.get()
    }

    fn held_span(&self, slot: usize, offset: u64) -> Option<Result<HeldSpan>> {
        if self.holding.get() == 0 {
            return None;
        }
        if offset >= self.total() {
            return Some(Err(Error::Truncated));
        }
        let mut slots = self.slots.borrow_mut();
        let pages = slots.get_mut(slot)?;
        let page = (offset / PAGE_DATA as u64) as u32;
        let pinned = !pages.contains(&Some(page));
        if pinned {
            *pages = [None; HELD_PIECES];
            pages[0] = Some(page);
            self.touch(page, offset);
        }
        let (data, len) = self.page_bytes(page);
        Some(Ok(HeldSpan {
            start: u64::from(page) * PAGE_DATA as u64,
            data,
            len,
            pinned,
        }))
    }

    fn held_slot(&self) -> Option<usize> {
        if self.holding.get() == 0 {
            return None;
        }
        let mut slots = self.slots.borrow_mut();
        if slots.len() >= HELD_SLOT_LIMIT {
            return None;
        }
        slots.push([None; HELD_PIECES]);
        Some(slots.len() - 1)
    }

    fn held_range(&self, slot: usize, offset: u64, len: usize) -> Option<Result<HeldRange>> {
        if self.holding.get() == 0 || len == 0 {
            return None;
        }
        let end = match check(self.total(), offset, len) {
            Ok(end) => end,
            Err(error) => return Some(Err(error)),
        };
        let capacity = PAGE_DATA as u64;
        let (first, last) = ((offset / capacity) as u32, ((end - 1) / capacity) as u32);
        if (last - first) as usize >= HELD_PIECES {
            return None;
        }
        let mut slots = self.slots.borrow_mut();
        let pages = slots.get_mut(slot)?;
        // Pages the slot holds already stay held, as in the extension.
        for page in pages.iter_mut() {
            if page.is_some_and(|held| held < first || held > last) {
                *page = None;
            }
        }
        let mut range = HeldRange::default();
        for page in first..=last {
            let (data, page_len) = self.page_bytes(page);
            if !pages.contains(&Some(page)) {
                *pages
                    .iter_mut()
                    .find(|held| held.is_none())
                    .expect("a free place in the slot") = Some(page);
                range.pinned_bytes += page_len;
                self.touch(page, offset.max(u64::from(page) * capacity));
            }
            let within = if page == first {
                (offset % capacity) as usize
            } else {
                0
            };
            // SAFETY: within the page's bytes.
            range.push(unsafe { data.add(within) }, page_len - within);
        }
        Some(Ok(range))
    }
}
