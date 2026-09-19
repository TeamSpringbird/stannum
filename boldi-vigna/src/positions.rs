// Copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

/// Provides sorted token positions for leaf terms in a span query.
///
/// Each term in a [`SpanQuery::Term(i)`](crate::SpanQuery::Term) references
/// positions by index `i`. Implementors supply the sorted position list for
/// each index, borrowing existing data without copying.
pub trait TermPositions {
    /// Returns the sorted ascending positions for the term at `term_index`.
    ///
    /// # Panics
    ///
    /// May panic if `term_index` is out of range.
    fn positions(&self, term_index: usize) -> &[u32];
}

// --- Blanket implementations ---

impl TermPositions for Vec<Vec<u32>> {
    fn positions(&self, term_index: usize) -> &[u32] {
        &self[term_index]
    }
}

impl TermPositions for Vec<&[u32]> {
    fn positions(&self, term_index: usize) -> &[u32] {
        self[term_index]
    }
}

impl<const N: usize> TermPositions for [Vec<u32>; N] {
    fn positions(&self, term_index: usize) -> &[u32] {
        &self[term_index]
    }
}

impl<const N: usize> TermPositions for [&[u32]; N] {
    fn positions(&self, term_index: usize) -> &[u32] {
        self[term_index]
    }
}
