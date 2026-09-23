// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A bounded cache of the byte ranges cursors read and replace: postings
//! windows, ordinal chunks, payload spans and length windows.
//!
//! The reader's arena keeps every range it fetches for as long as the reader
//! lives, which suits headers and tables a query touches once but not the
//! ranges a cursor sweeps: a phrase of common words fetched hundreds of
//! megabytes through it. Those ranges come through here instead. Entries are
//! shared with the cursors that hold them and evicted least recently used
//! once the budget is exceeded, so the hot chunks of frequent terms stay
//! resident across queries while a sweep of a long stream displaces only
//! itself. One cache per thread: a PostgreSQL backend is one thread.

use rustc_hash::FxHashMap;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// A range of one reader's source.
type Key = (u64, u64, u32);

const NONE: usize = usize::MAX;

struct Entry {
    key: Key,
    bytes: Rc<[u8]>,
    /// More recently used neighbour.
    prev: usize,
    /// Less recently used neighbour.
    next: usize,
}

struct Cache {
    map: FxHashMap<Key, usize>,
    entries: Vec<Entry>,
    free: Vec<usize>,
    /// Most recently used.
    head: usize,
    /// Least recently used.
    tail: usize,
    bytes: usize,
    budget: usize,
}

/// The default budget: enough for the hot streams of a benchmark workload
/// across a segment directory.
pub const DEFAULT_BUDGET: usize = 64 * 1024 * 1024;

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache {
        map: FxHashMap::default(),
        entries: Vec::new(),
        free: Vec::new(),
        head: NONE,
        tail: NONE,
        bytes: 0,
        budget: DEFAULT_BUDGET,
    });
    static NEXT_READER: Cell<u64> = const { Cell::new(1) };
}

/// Areas of a segment, in blob order, for read accounting.
pub const AREAS: usize = 8;
/// Names of [`AREAS`], in order.
pub const AREA_NAMES: [&str; AREAS] = [
    "header",
    "dictionary",
    "postings",
    "payload",
    "documents",
    "lengths",
    "ordinals",
    "pages",
];

thread_local! {
    static AREA_BYTES: Cell<[u64; AREAS]> = const { Cell::new([0; AREAS]) };
    static AREA_DISK: Cell<[u64; AREAS]> = const { Cell::new([0; AREAS]) };
    /// The host's count of pages read from storage, where it offers one.
    static DISK_PROBE: Cell<Option<fn() -> u64>> = const { Cell::new(None) };
}

/// Installs the host's counter of pages read from storage, so fetches can be
/// attributed to the area that caused them.
pub fn set_disk_probe(probe: fn() -> u64) {
    DISK_PROBE.set(Some(probe));
}

/// The host's count of pages read from storage; zero without a probe.
pub fn disk_pages() -> u64 {
    DISK_PROBE.get().map_or(0, |probe| probe())
}

/// Records pages `area` caused to be read from storage.
pub fn note_disk(area: usize, pages: u64) {
    if pages == 0 {
        return;
    }
    AREA_DISK.with(|counts| {
        let mut all = counts.get();
        all[area.min(AREAS - 1)] += pages;
        counts.set(all);
    });
}

/// Pages read from storage per area since the last reset.
pub fn area_disk() -> [u64; AREAS] {
    AREA_DISK.with(Cell::get)
}

/// Records bytes fetched from a segment's `area`, counted only where the
/// caches missed and the source was actually read.
pub fn note_read(area: usize, bytes: usize) {
    AREA_BYTES.with(|counts| {
        let mut all = counts.get();
        all[area.min(AREAS - 1)] += bytes as u64;
        counts.set(all);
    });
}

/// Bytes fetched per area since the last reset.
pub fn area_bytes() -> [u64; AREAS] {
    AREA_BYTES.with(Cell::get)
}

pub fn reset_areas() {
    AREA_BYTES.with(|counts| counts.set([0; AREAS]));
    AREA_DISK.with(|counts| counts.set([0; AREAS]));
}

/// A fresh identity for a reader, so its ranges never collide with another
/// reader's over the same offsets.
pub fn reader_id() -> u64 {
    NEXT_READER.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    })
}

/// Sets the byte budget, evicting down to it.
pub fn set_budget(bytes: usize) {
    CACHE.with_borrow_mut(|cache| {
        cache.budget = bytes;
        cache.evict();
    });
}

/// Bytes held.
pub fn bytes() -> usize {
    CACHE.with_borrow(|cache| cache.bytes)
}

/// Drops every entry.
pub fn clear() {
    CACHE.with_borrow_mut(|cache| {
        cache.map.clear();
        cache.entries.clear();
        cache.free.clear();
        cache.head = NONE;
        cache.tail = NONE;
        cache.bytes = 0;
    });
}

/// The cached range, marking it most recently used.
pub fn get(reader: u64, offset: u64, len: usize) -> Option<Rc<[u8]>> {
    let key = (reader, offset, u32::try_from(len).ok()?);
    CACHE.with_borrow_mut(|cache| {
        let index = *cache.map.get(&key)?;
        cache.unlink(index);
        cache.link_front(index);
        Some(cache.entries[index].bytes.clone())
    })
}

/// Caches a range just read, sharing it with the caller. A range larger
/// than the budget is returned without being kept.
pub fn insert(reader: u64, offset: u64, bytes: Vec<u8>) -> Rc<[u8]> {
    let bytes: Rc<[u8]> = Rc::from(bytes);
    let Ok(len) = u32::try_from(bytes.len()) else {
        return bytes;
    };
    let key = (reader, offset, len);
    CACHE.with_borrow_mut(|cache| {
        if bytes.len() > cache.budget {
            return bytes.clone();
        }
        if let Some(&index) = cache.map.get(&key) {
            cache.unlink(index);
            cache.link_front(index);
            return cache.entries[index].bytes.clone();
        }
        let entry = Entry {
            key,
            bytes: bytes.clone(),
            prev: NONE,
            next: NONE,
        };
        let index = match cache.free.pop() {
            Some(index) => {
                cache.entries[index] = entry;
                index
            }
            None => {
                cache.entries.push(entry);
                cache.entries.len() - 1
            }
        };
        cache.map.insert(key, index);
        cache.bytes += bytes.len();
        cache.link_front(index);
        cache.evict();
        bytes
    })
}

impl Cache {
    fn link_front(&mut self, index: usize) {
        self.entries[index].prev = NONE;
        self.entries[index].next = self.head;
        if self.head != NONE {
            self.entries[self.head].prev = index;
        }
        self.head = index;
        if self.tail == NONE {
            self.tail = index;
        }
    }

    fn unlink(&mut self, index: usize) {
        let (prev, next) = (self.entries[index].prev, self.entries[index].next);
        if prev == NONE {
            self.head = next;
        } else {
            self.entries[prev].next = next;
        }
        if next == NONE {
            self.tail = prev;
        } else {
            self.entries[next].prev = prev;
        }
    }

    /// Evicts least recently used entries until within budget.
    fn evict(&mut self) {
        while self.bytes > self.budget && self.tail != NONE {
            let index = self.tail;
            self.unlink(index);
            let entry = &mut self.entries[index];
            self.bytes -= entry.bytes.len();
            self.map.remove(&entry.key);
            entry.bytes = Rc::from(Vec::new());
            self.free.push(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used_within_budget() {
        clear();
        set_budget(30);
        let a = insert(1, 0, vec![1; 10]);
        let _b = insert(1, 10, vec![2; 10]);
        let _c = insert(1, 20, vec![3; 10]);
        assert_eq!(bytes(), 30);
        assert!(get(1, 0, 10).is_some(), "a is touched, so b is the oldest");
        let _d = insert(1, 30, vec![4; 10]);
        assert_eq!(bytes(), 30);
        assert!(get(1, 10, 10).is_none(), "b evicted");
        assert!(get(1, 0, 10).is_some());
        assert!(get(1, 20, 10).is_some());
        assert!(get(1, 30, 10).is_some());
        assert_eq!(&*a, &[1; 10]);
        // Another reader's identical offsets are distinct entries.
        assert!(get(2, 0, 10).is_none());
        // A range beyond the budget is handed back but not kept.
        let big = insert(1, 100, vec![9; 40]);
        assert_eq!(big.len(), 40);
        assert!(get(1, 100, 40).is_none());
        assert!(bytes() <= 30);
        set_budget(DEFAULT_BUDGET);
        clear();
    }
}
