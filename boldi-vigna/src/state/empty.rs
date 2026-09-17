use crate::interval::Interval;
use crate::positions::TermPositions;

/// Empty interval source: always exhausted.
pub(crate) struct EmptyState;

impl EmptyState {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn reset(&mut self) {}

    pub(crate) fn next_interval(&mut self, _positions: &impl TermPositions) -> Option<Interval> {
        None
    }

    pub(crate) fn gaps(&self) -> Option<u64> {
        Some(0)
    }
}
