// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Byte-level codecs for the immutable parts of a search index segment.
//!
//! This crate has no PostgreSQL dependency. It defines how a segment's
//! components are laid out as bytes and how they are read back with bounded
//! work, so the formats can be tested exhaustively outside a server. Page
//! allocation, WAL, locking and lifecycle belong to the caller.
//!
//! Components:
//!
//! * [`dictionary`]: sorted, prefix-compressed terms with per-term statistics
//!   and the extents of that term's ordinal and payload streams. Supports exact
//!   lookup, prefix iteration and lexicographic ranges.
//! * [`ordinals`]: a term's documents as ordinals into the segment's document
//!   table, as a short list or as 65,536-document chunks of arrays or bitmaps,
//!   with a score bound per chunk. Cursors support `seek` and report each
//!   member's rank.
//! * [`docs`]: the document table, from ordinal to tuple location and back,
//!   and the cursors that read a term's documents in heap order or a heap
//!   page at a time.
//! * [`payload`]: per-document term-frequency bucket and token positions, in
//!   the same order as the term's ordinals, with a skip table addressed by
//!   rank so Boolean queries never decode it.
//! * [`forward`]: one document's tokens as a single record for a mutable write
//!   buffer, so an insert is one append rather than one per term.
//! * [`set`]: intersection, union and difference over any cursors.
//! * [`segment`]: assembles the components above into one immutable segment
//!   with a document table of lengths, and reads them back.
//! * [`tf_bucket`]: the production-compatible term-frequency quantization.
//! * [`verify`]: whole-blob consistency checks that list every problem found
//!   instead of stopping at the first, for an index checker.
//!
//! Every decoder returns [`Error`] on malformed input instead of panicking.
//! Formats are versioned by the caller (page kind and version live in the
//! surrounding page header); this crate's constants document the byte layout.

mod error;
mod reader;
mod varint;

pub mod bound;
pub mod cache;
pub mod dictionary;
pub mod docs;
pub mod forward;
pub mod index;
pub mod length_class;
pub mod merge;
pub mod merge_strategy;
pub mod ordinals;
pub mod pages;
pub mod payload;
pub mod segment;
pub mod set;
pub mod source;
pub mod tf_bucket;
pub mod tid;
pub mod verify;

pub use error::{Error, Result};
pub use tid::Tid;

#[cfg(test)]
mod random_tests;
