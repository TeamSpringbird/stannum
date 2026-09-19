// Copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

use crate::interval::Interval;
use crate::positions::TermPositions;

/// Leaf node state: cursor over a single term's position list.
/// Produces `[pos, pos]` intervals — one per occurrence.
pub(crate) struct TermState {
    term_index: usize,
    cursor: usize,
}

impl TermState {
    pub(crate) fn new(term_index: usize) -> Self {
        Self {
            term_index,
            cursor: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn next_interval(&mut self, positions: &impl TermPositions) -> Option<Interval> {
        let pos_list = positions.positions(self.term_index);
        if self.cursor < pos_list.len() {
            let pos = pos_list[self.cursor];
            self.cursor += 1;
            Some(Interval::point(pos))
        } else {
            None
        }
    }

    pub(crate) fn gaps(&self) -> Option<u64> {
        Some(0)
    }
}
