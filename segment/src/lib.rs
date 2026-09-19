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
//!   and the extents of that term's postings and payload streams. Supports exact
//!   lookup, prefix iteration and lexicographic ranges.
//! * [`postings`]: a term's tuple locations in heap order, encoded either as a
//!   sparse delta list or as 256-page groups with page bitmaps and per-page
//!   offset lists or tuple bitmaps. Cursors support `seek` with group and page
//!   skipping and report each posting's ordinal.
//! * [`payload`]: per-posting term-frequency bucket and token positions, in the
//!   same order as the postings, with a skip table addressed by ordinal so
//!   Boolean queries never decode it.
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

pub mod dictionary;
pub mod forward;
pub mod index;
pub mod merge;
pub mod pages;
pub mod payload;
pub mod postings;
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

#[cfg(test)]
mod format_tests;

#[cfg(test)]
mod direct_merge_poc;
