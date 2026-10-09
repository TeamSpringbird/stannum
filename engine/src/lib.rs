// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Stannum's query engine over segments, without PostgreSQL.
//!
//! The extension (`postgres/`) captures an index view, checks visibility
//! and reports errors the PostgreSQL way; what lies between, from BM25
//! arithmetic to the ranked walk over ordinal streams, lives here, so it
//! can be run, measured and benchmarked over dumped segments outside a
//! server.

pub mod bm25;
