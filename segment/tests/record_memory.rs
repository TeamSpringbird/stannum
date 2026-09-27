// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Memory that turning one document's tokens into index structures costs.
//!
//! An insert groups a document's tokens into a forward record and an index
//! build groups them into its segment. A document can be as large as a
//! `text` value, so the working memory must stay a small multiple of the
//! document: a Rust allocation failure aborts the backend, and PostgreSQL
//! then restarts every session. This is its own test binary because its
//! counting allocator sees every allocation of the process.

use std::alloc::{GlobalAlloc, Layout, System};
use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};

use segment::forward::ForwardRecord;
use segment::segment::SegmentBuilder;
use segment::tid::Tid;
use tokenizer::Tokenizer;
use tokenizer::presets::default_pipeline;

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(bytes: usize) {
    let now = CURRENT.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(now, Ordering::Relaxed);
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

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                CURRENT.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Peak bytes allocated while `work` runs, above what was allocated before.
fn peak_during<T>(work: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = work();
    (out, PEAK.load(Ordering::Relaxed) - base)
}

/// About `bytes` of prose-like text: words of 2 to 11 letters from a
/// 50,000-word vocabulary drawn with a skew toward the first words, some
/// capitalized, with punctuation.
fn document(bytes: usize) -> String {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let vocabulary: Vec<String> = (0..50_000)
        .map(|_| {
            let len = 2 + (next() % 10) as usize;
            (0..len)
                .map(|_| char::from(b'a' + (next() % 26) as u8))
                .collect()
        })
        .collect();
    let mut text = String::with_capacity(bytes + 64);
    while text.len() < bytes {
        let r = next();
        // The product of two uniform draws favors small indexes.
        let index = (((r & 0xffff) * ((r >> 16) & 0xffff)) as usize * vocabulary.len()) >> 32;
        let word = &vocabulary[index];
        if (r >> 40) & 15 == 0 {
            let mut chars = word.chars();
            text.push(chars.next().unwrap().to_ascii_uppercase());
            text.push_str(chars.as_str());
        } else {
            text.push_str(word);
        }
        text.push_str(match (r >> 44) & 31 {
            0 => ". ",
            1 => ", ",
            _ => " ",
        });
    }
    text
}

fn tokens(text: &str) -> impl Iterator<Item = (Cow<'_, str>, u32)> {
    default_pipeline()
        .tokenize(text)
        .map(|token| (token.text, token.pos))
}

/// A 16 MiB document costs less than three times its size to turn into an
/// encoded forward record (an insert) or into a segment builder's postings
/// (an index build). The limit covers the positions themselves, four bytes
/// a token, with room for vector growth; a heap string per token costs
/// several times the document.
#[test]
fn grouping_a_large_document_costs_a_small_multiple_of_its_size() {
    let text = document(16 << 20);
    let tid = Tid::new(1, 1).unwrap();
    // Also compiles the tokenizer outside the measurements.
    let token_count = tokens(&text).count();

    let ((doc_len, encoded), record_peak) = peak_during(|| {
        let record = ForwardRecord::from_token_stream(tid, tokens(&text)).unwrap();
        let mut bytes = Vec::new();
        record.encode(&mut bytes).unwrap();
        (record.doc_len, bytes.len())
    });
    let (built, build_peak) = peak_during(|| {
        let mut builder = SegmentBuilder::default();
        builder.add_token_stream(tid, tokens(&text)).unwrap();
        builder
    });
    assert_eq!(built.document_count(), 1);
    drop(built);

    let ratio = |peak: usize| peak as f64 / text.len() as f64;
    eprintln!(
        "document {} bytes, {token_count} tokens, record {encoded} bytes: \
         forward record peak {record_peak} bytes ({:.2}x), \
         segment builder peak {build_peak} bytes ({:.2}x)",
        text.len(),
        ratio(record_peak),
        ratio(build_peak),
    );
    assert_eq!(doc_len as usize, token_count);
    assert!(
        ratio(record_peak) < 3.0,
        "forward record: {:.2}x the document",
        ratio(record_peak)
    );
    assert!(
        ratio(build_peak) < 3.0,
        "segment builder: {:.2}x the document",
        ratio(build_peak)
    );
}
