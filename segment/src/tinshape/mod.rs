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
//! * [`positions`]: a term's positions, STN3's stream with finer skips for
//!   frequent terms.
//! * [`segment`]: the blob that holds them with the term map and positions.
//! * [`ef`], [`bits`]: the Elias-Fano and bit-packing codecs underneath.
//!
//! This is phase A of the move: the format and its codecs, measured offline
//! against the ordinal format (`STN3`) by the bench crate's `tinshape`
//! binary. Nothing in the extension reads it yet.

pub mod bits;
pub mod docs;
pub mod ef;
pub mod positions;
pub mod postings;
pub mod segment;

/// The varint codec the format's headers use, for readers outside the
/// crate.
pub mod varint {
    pub use crate::varint::{get, get_u32, put};
}
