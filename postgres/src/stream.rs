//! An owning, restartable candidate stream for an unordered custom scan.
//!
//! Keep the captured view across FETCH and executor rescans. Reopening storage
//! would observe a different mutable buffer/directory, and retaining only a
//! borrowed cursor would leave references dangling after the first call.

use segment::{Tid, pages, set};
use tinql::runtime::{
    Query,
    plan::{Limits, page_plan, plan, prefers_pages},
};

use crate::storage::{View, codec_in};

enum Cursor {
    Rows(Box<dyn set::Cursor>),
    Pages(Box<dyn pages::Cursor>),
}

pub(crate) struct CandidateStream {
    // Drop order is part of the lifetime invariant: cursor before its owner.
    cursor: Option<Cursor>,
    view: Box<View>,
    query: Query,
    page: Option<pages::Page>,
    advance: bool,
    pub(crate) recheck: bool,
    pub(crate) page_masks: bool,
}

impl CandidateStream {
    pub(crate) fn new(view: View, query: Query) -> Self {
        let mut this = Self {
            cursor: None,
            view: Box::new(view),
            query,
            page: None,
            advance: false,
            recheck: false,
            page_masks: false,
        };
        this.rewind();
        this
    }

    /// Rebuild cursors against the SAME captured view, not current storage.
    pub(crate) fn rewind(&mut self) {
        self.cursor = None;
        self.page = None;
        self.advance = false;
        self.recheck = false;
        // SAFETY: view is a private, boxed owner that never moves or mutates
        // after construction. All cursors borrowing it are private, cannot
        // escape this struct, and drop before view (including during unwind).
        // Rewind drops the old cursor before constructing its replacement.
        // This is the same owning-reader pattern used by IndexScorer.
        let view = unsafe { std::mem::transmute::<&View, &'static View>(&self.view) };
        let limits = Limits::default();
        self.page_masks = view.sources.iter().any(|(source, _)| {
            prefers_pages(&self.query, source)
                .unwrap_or_else(|error| pgrx::error!("Stannum query plan: {error}"))
        });
        if self.page_masks {
            let mut inputs: Vec<Box<dyn pages::Cursor>> = Vec::new();
            for ((source, dead), label) in view.sources.iter().zip(&view.labels) {
                pgrx::check_for_interrupts!();
                let planned = page_plan(&self.query, source, &limits)
                    .unwrap_or_else(|error| pgrx::error!("Stannum query plan: {error}"));
                self.recheck |= !planned.exact;
                let mut cursor = planned.cursor;
                if let Some(dead) = dead {
                    let dead = codec_in(
                        segment::postings::Postings::parse(dead).and_then(|p| p.pages()),
                        label,
                    );
                    cursor = Box::new(codec_in(pages::Difference::new(cursor, dead), label));
                }
                inputs.push(cursor);
            }
            self.cursor = Some(Cursor::Pages(Box::new(pages::Union::new(inputs))));
        } else {
            let mut inputs: Vec<Box<dyn set::Cursor>> = Vec::new();
            for ((source, dead), label) in view.sources.iter().zip(&view.labels) {
                pgrx::check_for_interrupts!();
                let planned = plan(&self.query, source, &limits)
                    .unwrap_or_else(|error| pgrx::error!("Stannum query plan: {error}"));
                self.recheck |= !planned.exact;
                let mut cursor = planned.cursor;
                if let Some(dead) = dead {
                    let dead = codec_in(
                        segment::postings::Postings::parse(dead).and_then(|p| p.cursor()),
                        label,
                    );
                    cursor = Box::new(codec_in(set::Difference::new(cursor, dead), label));
                }
                inputs.push(cursor);
            }
            self.cursor = Some(Cursor::Rows(Box::new(set::Union::new(inputs))));
        }
    }

    /// The next distinct heap location. Do not advance beyond the consumed
    /// row/page until asked again, so LIMIT does not decode the next page.
    pub(crate) fn next(&mut self) -> Option<Tid> {
        match self.cursor.as_mut().expect("initialized stream") {
            Cursor::Rows(cursor) => {
                if self.advance {
                    codec_in(cursor.advance(), "search stream");
                }
                self.advance = true;
                cursor.current()
            }
            Cursor::Pages(cursor) => {
                if self.page.is_none() {
                    if self.advance {
                        codec_in(cursor.advance(), "search page stream");
                    }
                    self.page = cursor.current();
                    self.advance = true;
                }
                let page = self.page.as_mut()?;
                let tid = Tid {
                    block: page.block,
                    offset: page.offsets.pop_first().expect("nonempty page"),
                };
                if page.offsets.is_empty() {
                    self.page = None;
                }
                Some(tid)
            }
        }
    }
}
