// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! The Rust heap of a pg_test build, counted.
//!
//! Every backend holds its segment caches on the Rust heap, outside
//! PostgreSQL's memory contexts, so `pg_backend_memory_contexts` does not
//! see them. Tests that bound what a query keeps per backend read these
//! counters instead of the process's resident size, which moves with
//! shared buffers and the allocator's own caching. Allocations are
//! deterministic for a given query and index, so the counts are too.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(bytes: usize) {
    let now = CURRENT.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

fn shrank(bytes: usize) {
    CURRENT.fetch_sub(bytes, Ordering::Relaxed);
}

// SAFETY: every call forwards to `System` unchanged; the counters only
// observe the sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        shrank(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                shrank(layout.size() - new_size);
            }
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Rust heap bytes allocated and not yet freed.
pub fn current() -> usize {
    CURRENT.load(Ordering::Relaxed)
}

/// Runs `f` and returns its result with the most Rust heap bytes it held at
/// once beyond what was allocated when it started, and what it left
/// allocated (negative when it freed more than it allocated).
pub fn measure<T>(f: impl FnOnce() -> T) -> (T, usize, isize) {
    let start = current();
    PEAK.store(start, Ordering::Relaxed);
    let result = f();
    let peak = PEAK.load(Ordering::Relaxed) - start;
    (result, peak, current() as isize - start as isize)
}
