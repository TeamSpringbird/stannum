// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Segments in TIN's shape: a term's postings are a set of heap ctids, not
//! ordinals into a document table. See `docs/architecture/tin-shape.md`.
//!
//! * [`docs`]: the ctid grid (heap pages in groups of 256, a slot per line
//!   pointer up to the group's largest offset), the document set with its
//!   rank directory, the DL sidecar and the liveness bitmap.
//! * [`postings`]: a term's slot set (one Elias-Fano list, or per group a
//!   grid bitmap, an Elias-Fano list or per-page containers), its footer of
//!   per-block impact frontiers and its TF tail.
//! * [`segment`]: the blob that holds them with the term map and positions.
//! * [`ef`], [`bits`]: the Elias-Fano and bit-packing codecs underneath.
//! * [`index`]: a segment read through a [`crate::source::Source`] behind
//!   the query interface every source offers, for the query shapes the
//!   ctid-native paths do not take.
//! * [`merge`]: merging segments without their dead documents.
//! * [`verify`]: checking a blob for corruption.
//!
//! The extension writes every immutable segment in this shape.

pub mod bits;
pub mod docs;
pub mod ef;
pub mod index;
pub mod merge;
pub mod postings;
pub mod segment;
pub mod verify;

/// The varint codec the format's headers use, for readers outside the
/// crate.
pub mod varint {
    pub use crate::varint::{get, get_u32, put};
}

#[cfg(test)]
mod tests;
