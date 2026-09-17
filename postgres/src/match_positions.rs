/// One matched query part over token-position coordinates.
///
/// `start` / `end` are inclusive token positions in the indexed token stream.
/// Point matches use `start == end`.
#[derive(Debug, PartialEq)]
pub struct MatchPosition {
    pub part: String,
    pub start: i32,
    pub end: i32,
}

impl MatchPosition {
    pub(crate) fn point(part: impl Into<String>, pos: u32) -> Self {
        Self::span(part, pos, pos)
    }

    pub(crate) fn span(part: impl Into<String>, start: u32, end: u32) -> Self {
        Self {
            part: part.into(),
            start: start as i32,
            end: end as i32,
        }
    }
}
