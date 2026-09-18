//! One read interface over immutable segments and the mutable index.
//!
//! The planner and scorer only need to resolve terms, expand term windows,
//! walk the document table and look up lengths. [`Index`] expresses exactly
//! that, so an immutable [`Reader`] and an in-memory [`MutableIndex`] that
//! grows by one record at a time are interchangeable at query time.

use std::cell::RefCell;
use std::collections::BTreeMap;

use rustc_hash::FxHashMap;

use crate::dictionary::{Extent, TermEntry};
use crate::forward::ForwardRecord;
use crate::payload::PayloadBuilder;
use crate::postings::{PostingsBuilder, PostingsCursor};
use crate::segment::{AreaFetch, Lengths, Reader, Term};
use crate::source::Source;
use crate::tf_bucket::TfBucket;
use crate::{Error, Result, Tid};

/// A lexicographic window of the term space.
#[derive(Clone, Copy, Debug)]
pub enum Window<'q> {
    Prefix(&'q str),
    /// Inclusive bounds; `None` is open.
    Range(Option<&'q str>, Option<&'q str>),
    All,
}

/// Terms found in a window, or a signal that more than `limit` matched.
pub enum Expanded<'a> {
    Terms(Vec<(String, Term<'a>)>),
    Overflow,
}

pub trait Index {
    fn document_count(&self) -> u32;
    fn total_length(&self) -> u64;
    fn term(&self, term: &str) -> Result<Option<Term<'_>>>;
    /// Terms in `window` accepted by `filter`, in order, up to `limit`.
    fn expand(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
        limit: usize,
    ) -> Result<Expanded<'_>>;
    /// Every document in the index, in TID order.
    fn documents(&self) -> Result<PostingsCursor<'_>>;
    fn lengths(&self) -> Lengths<'_>;
}

impl<S: Source> Index for Reader<S> {
    fn document_count(&self) -> u32 {
        Reader::document_count(self)
    }

    fn total_length(&self) -> u64 {
        Reader::total_length(self)
    }

    fn term(&self, term: &str) -> Result<Option<Term<'_>>> {
        Reader::term(self, term)
    }

    fn expand(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
        limit: usize,
    ) -> Result<Expanded<'_>> {
        let dictionary = self.dictionary()?;
        let items: Box<dyn Iterator<Item = Result<(String, TermEntry)>>> = match window {
            Window::Prefix(prefix) => Box::new(dictionary.prefix(prefix)),
            Window::Range(lower, upper) => Box::new(dictionary.range(lower, upper)),
            Window::All => Box::new(dictionary.iter()),
        };
        let mut found = Vec::new();
        for item in items {
            let (term, entry) = item?;
            if !filter(&term) {
                continue;
            }
            if found.len() >= limit {
                return Ok(Expanded::Overflow);
            }
            found.push((term, self.resolve(entry)?));
        }
        Ok(Expanded::Terms(found))
    }

    fn documents(&self) -> Result<PostingsCursor<'_>> {
        Reader::documents(self)
    }

    fn lengths(&self) -> Lengths<'_> {
        Reader::lengths(self)
    }
}

impl<I: Index + ?Sized> Index for &I {
    fn document_count(&self) -> u32 {
        (**self).document_count()
    }
    fn total_length(&self) -> u64 {
        (**self).total_length()
    }
    fn term(&self, term: &str) -> Result<Option<Term<'_>>> {
        (**self).term(term)
    }
    fn expand(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
        limit: usize,
    ) -> Result<Expanded<'_>> {
        (**self).expand(window, filter, limit)
    }
    fn documents(&self) -> Result<PostingsCursor<'_>> {
        (**self).documents()
    }
    fn lengths(&self) -> Lengths<'_> {
        (**self).lengths()
    }
}

impl<I: Index + ?Sized> Index for Box<I> {
    fn document_count(&self) -> u32 {
        (**self).document_count()
    }
    fn total_length(&self) -> u64 {
        (**self).total_length()
    }
    fn term(&self, term: &str) -> Result<Option<Term<'_>>> {
        (**self).term(term)
    }
    fn expand(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
        limit: usize,
    ) -> Result<Expanded<'_>> {
        (**self).expand(window, filter, limit)
    }
    fn documents(&self) -> Result<PostingsCursor<'_>> {
        (**self).documents()
    }
    fn lengths(&self) -> Lengths<'_> {
        (**self).lengths()
    }
}

impl<I: Index + ?Sized> Index for std::rc::Rc<I> {
    fn document_count(&self) -> u32 {
        (**self).document_count()
    }
    fn total_length(&self) -> u64 {
        (**self).total_length()
    }
    fn term(&self, term: &str) -> Result<Option<Term<'_>>> {
        (**self).term(term)
    }
    fn expand(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
        limit: usize,
    ) -> Result<Expanded<'_>> {
        (**self).expand(window, filter, limit)
    }
    fn documents(&self) -> Result<PostingsCursor<'_>> {
        (**self).documents()
    }
    fn lengths(&self) -> Lengths<'_> {
        (**self).lengths()
    }
}

/// One document's occurrence of a term: the score inputs and where its
/// positions sit in the term's flat position store.
#[derive(Clone, Copy)]
struct Occurrence {
    bucket: u8,
    doc_len: u32,
    start: u32,
    len: u32,
}

/// A term's postings under construction. Positions of every occurrence share
/// one vector in arrival order, so adding a record costs one append per term
/// rather than one allocation per term; `occurrences` follows `tids` in TID
/// order and points back into it.
struct TermData {
    /// Strictly increasing.
    tids: Vec<Tid>,
    /// Aligned with `tids`.
    occurrences: Vec<Occurrence>,
    positions: Vec<u32>,
}

impl TermData {
    fn positions_of(&self, occurrence: &Occurrence) -> &[u32] {
        let start = occurrence.start as usize;
        &self.positions[start..start + occurrence.len as usize]
    }
}

/// Encoded streams handed to cursors. Append-only: a slot is never freed or
/// moved while the index lives, so borrowed views survive later appends.
#[derive(Default)]
struct Encoded {
    slots: Vec<Box<[u8]>>,
    /// Slots and entry per term, dropped from the map when the term changes.
    terms: FxHashMap<String, TermEntry>,
    documents: Option<usize>,
    lengths: Option<usize>,
}

impl Encoded {
    fn push(&mut self, bytes: Vec<u8>) -> Extent {
        self.slots.push(bytes.into_boxed_slice());
        let slot = self.slots.len() - 1;
        Extent {
            offset: slot as u64,
            len: self.slots[slot].len() as u32,
        }
    }

    /// The lifetime is the caller's: slots are boxed, never removed and never
    /// reallocated in place, so a slice stays valid for as long as the owning
    /// index lives, which is the only lifetime callers ask for.
    fn slot<'s>(&self, extent: Extent) -> Result<&'s [u8]> {
        let bytes = self
            .slots
            .get(extent.offset as usize)
            .ok_or(Error::Corrupt("mutable index slot"))?;
        if bytes.len() != extent.len as usize {
            return Err(Error::Corrupt("mutable index slot length"));
        }
        // SAFETY: see above; the box's heap allocation outlives every borrow.
        Ok(unsafe { &*std::ptr::from_ref::<[u8]>(bytes) })
    }
}

/// An in-memory inverted index that grows one record at a time. Adding a
/// record costs work proportional to that record; encoded streams are built
/// lazily per term on first use and rebuilt only after that term changes.
#[derive(Default)]
pub struct MutableIndex {
    documents: RefCell<BTreeMap<Tid, u32>>,
    total_length: RefCell<u64>,
    terms: RefCell<FxHashMap<String, TermData>>,
    /// Term names in order, built on the first expansion and dropped when a
    /// new term appears.
    sorted: RefCell<Option<Vec<String>>>,
    encoded: RefCell<Encoded>,
}

impl MutableIndex {
    /// Adds a document. A token-less record is not recorded, matching the
    /// segment builder. A TID already present is rejected.
    pub fn add_record(&self, record: ForwardRecord) -> Result<()> {
        let mut bytes = Vec::new();
        record.encode(&mut bytes)?;
        self.add_encoded(&bytes).map(drop)
    }

    /// Adds the encoded record at the start of `bytes` straight from its
    /// bytes, returning the bytes consumed. Only a new term allocates its
    /// name; positions are copied once.
    pub fn add_encoded(&self, bytes: &[u8]) -> Result<usize> {
        let header = ForwardRecord::peek(bytes)?;
        if header.doc_len == 0 {
            return ForwardRecord::encoded_len(bytes);
        }
        let mut documents = self.documents.borrow_mut();
        if documents.contains_key(&header.tid) {
            return Err(Error::Unordered);
        }
        documents.insert(header.tid, header.doc_len);
        *self.total_length.borrow_mut() += u64::from(header.doc_len);
        let mut terms = self.terms.borrow_mut();
        let mut sorted = self.sorted.borrow_mut();
        let mut encoded = self.encoded.borrow_mut();
        encoded.documents = None;
        encoded.lengths = None;
        let tid = header.tid;
        let doc_len = header.doc_len;
        let (_, consumed) = ForwardRecord::decode_with(bytes, |term, positions| {
            crate::payload::validate_positions(positions)?;
            let bucket = TfBucket::from_count(positions.len() as u32).value();
            if !encoded.terms.is_empty() {
                encoded.terms.remove(term);
            }
            let data = match terms.get_mut(term) {
                Some(data) => data,
                None => {
                    *sorted = None;
                    terms.entry(term.to_owned()).or_insert_with(|| TermData {
                        tids: Vec::new(),
                        occurrences: Vec::new(),
                        positions: Vec::new(),
                    })
                }
            };
            // Records arrive in TID order almost always; insertion elsewhere
            // is the rare out-of-order case.
            let at = match data.tids.last() {
                Some(last) if *last < tid => data.tids.len(),
                _ => data.tids.partition_point(|existing| *existing < tid),
            };
            let start = u32::try_from(data.positions.len())
                .ok()
                .filter(|start| start.checked_add(positions.len() as u32).is_some())
                .ok_or(Error::Corrupt("mutable index positions"))?;
            data.positions.extend_from_slice(positions);
            data.tids.insert(at, tid);
            data.occurrences.insert(
                at,
                Occurrence {
                    bucket,
                    doc_len,
                    start,
                    len: positions.len() as u32,
                },
            );
            Ok(())
        })?;
        Ok(consumed)
    }

    pub fn is_empty(&self) -> bool {
        self.documents.borrow().is_empty()
    }

    fn encode_term(&self, term: &str, data: &TermData) -> TermEntry {
        let mut postings = PostingsBuilder::default();
        let mut payload = PayloadBuilder::default();
        let mut max_tf_bucket = 0;
        for (tid, occurrence) in data.tids.iter().zip(&data.occurrences) {
            postings
                .push_scored(*tid, occurrence.bucket, occurrence.doc_len)
                .expect("tids kept sorted and unique");
            payload
                .push(occurrence.bucket, data.positions_of(occurrence))
                .expect("positions validated on insertion");
            max_tf_bucket = max_tf_bucket.max(occurrence.bucket);
        }
        let mut encoded = self.encoded.borrow_mut();
        let entry = TermEntry {
            df: data.tids.len() as u32,
            max_tf_bucket,
            postings: encoded.push(postings.finish()),
            payload: encoded.push(payload.finish()),
        };
        encoded.terms.insert(term.to_owned(), entry);
        entry
    }

    fn entry(&self, term: &str) -> Option<TermEntry> {
        if let Some(entry) = self.encoded.borrow().terms.get(term) {
            return Some(*entry);
        }
        let terms = self.terms.borrow();
        let data = terms.get(term)?;
        Some(self.encode_term(term, data))
    }

    fn documents_extent(&self) -> Extent {
        if let Some(slot) = self.encoded.borrow().documents {
            let len = self.encoded.borrow().slots[slot].len() as u32;
            return Extent {
                offset: slot as u64,
                len,
            };
        }
        let mut postings = PostingsBuilder::default();
        for tid in self.documents.borrow().keys() {
            postings.push(*tid).expect("map keys are ordered");
        }
        let mut encoded = self.encoded.borrow_mut();
        let extent = encoded.push(postings.finish());
        encoded.documents = Some(extent.offset as usize);
        extent
    }

    fn lengths_extent(&self) -> Extent {
        if let Some(slot) = self.encoded.borrow().lengths {
            let len = self.encoded.borrow().slots[slot].len() as u32;
            return Extent {
                offset: slot as u64,
                len,
            };
        }
        let mut bytes = Vec::with_capacity(self.documents.borrow().len() * 4);
        for len in self.documents.borrow().values() {
            bytes.extend_from_slice(&len.to_le_bytes());
        }
        let mut encoded = self.encoded.borrow_mut();
        let extent = encoded.push(bytes);
        encoded.lengths = Some(extent.offset as usize);
        extent
    }

    fn term_view(&self, entry: TermEntry) -> Term<'_> {
        Term::new(entry, self)
    }
}

impl AreaFetch for MutableIndex {
    fn postings_bytes(&self, extent: Extent) -> Result<&[u8]> {
        self.encoded.borrow().slot(extent)
    }

    fn payload_bytes(&self, extent: Extent) -> Result<&[u8]> {
        self.encoded.borrow().slot(extent)
    }

    fn length(&self, ordinal: u32) -> Result<u32> {
        let extent = self.lengths_extent();
        let bytes = self.encoded.borrow().slot(extent)?;
        let at = ordinal as usize * 4;
        let bytes = bytes
            .get(at..at + 4)
            .ok_or(Error::Corrupt("document ordinal out of range"))?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

impl Index for MutableIndex {
    fn document_count(&self) -> u32 {
        self.documents.borrow().len() as u32
    }

    fn total_length(&self) -> u64 {
        *self.total_length.borrow()
    }

    fn term(&self, term: &str) -> Result<Option<Term<'_>>> {
        Ok(self.entry(term).map(|entry| self.term_view(entry)))
    }

    fn expand(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
        limit: usize,
    ) -> Result<Expanded<'_>> {
        let names: Vec<String> = {
            let mut sorted = self.sorted.borrow_mut();
            let sorted = sorted.get_or_insert_with(|| {
                let mut names: Vec<String> = self.terms.borrow().keys().cloned().collect();
                names.sort_unstable();
                names
            });
            let keys: &[String] = match window {
                Window::Prefix(prefix) => {
                    let start = sorted.partition_point(|k| k.as_str() < prefix);
                    let end = start + sorted[start..].partition_point(|k| k.starts_with(prefix));
                    &sorted[start..end]
                }
                Window::Range(lower, upper) => {
                    let start =
                        lower.map_or(0, |lower| sorted.partition_point(|k| k.as_str() < lower));
                    let end = upper.map_or(sorted.len(), |upper| {
                        sorted.partition_point(|k| k.as_str() <= upper)
                    });
                    &sorted[start..end.max(start)]
                }
                Window::All => sorted,
            };
            keys.iter().filter(|k| filter(k)).cloned().collect()
        };
        if names.len() > limit {
            return Ok(Expanded::Overflow);
        }
        let mut found = Vec::with_capacity(names.len());
        for name in names {
            let entry = self.entry(&name).expect("term exists");
            found.push((name, self.term_view(entry)));
        }
        Ok(Expanded::Terms(found))
    }

    fn documents(&self) -> Result<PostingsCursor<'_>> {
        let extent = self.documents_extent();
        let bytes = self.encoded.borrow().slot(extent)?;
        crate::postings::Postings::parse(bytes)?.cursor()
    }

    fn lengths(&self) -> Lengths<'_> {
        // A document cursor retains its encoded TID order. Keep the matching
        // length order too: appending a record in reused heap space may insert
        // before those TIDs. A lazy lookup into the current map would then
        // score retained documents using another document's length.
        let extent = self.lengths_extent();
        Lengths::Bytes(
            self.encoded
                .borrow()
                .slot(extent)
                .expect("length extent was just encoded"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::SegmentBuilder;
    use crate::set::collect;

    fn record(id: u32, text: &str) -> ForwardRecord {
        let tokens: Vec<(&str, u32)> = text
            .split_whitespace()
            .enumerate()
            .map(|(i, w)| (w, i as u32 + 1))
            .collect();
        ForwardRecord::from_tokens(Tid::new(id / 50, (id % 50 + 1) as u16).unwrap(), tokens)
            .unwrap()
    }

    fn same<'a>(a: &'a dyn Index, b: &'a dyn Index, terms: &[&str]) {
        assert_eq!(a.document_count(), b.document_count());
        assert_eq!(a.total_length(), b.total_length());
        assert_eq!(
            collect(a.documents().unwrap()).unwrap(),
            collect(b.documents().unwrap()).unwrap()
        );
        for ordinal in 0..a.document_count() {
            assert_eq!(
                a.lengths().get(ordinal).unwrap(),
                b.lengths().get(ordinal).unwrap()
            );
        }
        for term in terms {
            let (x, y) = (a.term(term).unwrap(), b.term(term).unwrap());
            assert_eq!(x.is_some(), y.is_some(), "{term}");
            let (Some(x), Some(y)) = (x, y) else { continue };
            assert_eq!(x.entry.df, y.entry.df);
            assert_eq!(x.entry.max_tf_bucket, y.entry.max_tf_bucket);
            let tids = collect(x.cursor().unwrap()).unwrap();
            assert_eq!(tids, collect(y.cursor().unwrap()).unwrap());
            // Both carry the same block bounds, computed from the same documents.
            let bounds = x.cursor().unwrap().block_bounds().unwrap();
            assert!(!bounds.is_empty(), "{term}");
            assert_eq!(bounds, y.cursor().unwrap().block_bounds().unwrap());
            let (px, py) = (x.payload().unwrap(), y.payload().unwrap());
            for ordinal in 0..tids.len() as u32 {
                assert_eq!(px.get(ordinal).unwrap(), py.get(ordinal).unwrap());
            }
        }
        for window in [
            Window::Prefix("b"),
            Window::Range(Some("a"), Some("c")),
            Window::All,
        ] {
            let names = |e: Expanded<'_>| match e {
                Expanded::Terms(t) => t.into_iter().map(|(n, _)| n).collect::<Vec<_>>(),
                Expanded::Overflow => vec!["<overflow>".into()],
            };
            let x = names(a.expand(window, &|_| true, 1000).unwrap());
            let y = names(b.expand(window, &|_| true, 1000).unwrap());
            assert_eq!(x, y);
        }
    }

    #[test]
    fn retained_document_ordinals_keep_their_lengths_after_buffer_growth() {
        let mutable = MutableIndex::default();
        let original = record(50, "needle filler filler filler");
        let tid = original.tid;
        mutable.add_record(original).unwrap();
        let mut documents = mutable.documents().unwrap();
        let lengths = mutable.lengths();
        // Reused heap space inserts before the retained document cursor.
        mutable.add_record(record(1, "needle")).unwrap();
        let ordinal = documents.rank(tid).unwrap().unwrap();
        assert_eq!(lengths.get(ordinal).unwrap(), 4);
        assert_eq!(mutable.lengths().get(0).unwrap(), 1);
        assert_eq!(mutable.lengths().get(1).unwrap(), 4);
    }

    #[test]
    fn out_of_order_records_keep_each_occurrence_with_its_own_positions() {
        // Positions are stored flat per term in arrival order while postings
        // stay in TID order; an occurrence inserted before earlier ones must
        // still resolve to its own positions and term frequency.
        let mutable = MutableIndex::default();
        mutable
            .add_record(record(300, "beer beer beer ale"))
            .unwrap();
        mutable.add_record(record(200, "ale beer")).unwrap();
        mutable.add_record(record(100, "beer")).unwrap();
        let beer = mutable.term("beer").unwrap().unwrap();
        assert_eq!(beer.df(), 3);
        let tids = collect(beer.cursor().unwrap()).unwrap();
        assert_eq!(
            tids,
            [
                record(100, "").tid,
                record(200, "").tid,
                record(300, "").tid
            ]
        );
        let payload = beer.payload().unwrap();
        assert_eq!(payload.get(0).unwrap().positions, [1]);
        assert_eq!(payload.get(1).unwrap().positions, [2]);
        assert_eq!(payload.get(2).unwrap().positions, [1, 2, 3]);
        let ale = mutable.term("ale").unwrap().unwrap();
        let payload = ale.payload().unwrap();
        assert_eq!(payload.get(0).unwrap().positions, [1]);
        assert_eq!(payload.get(1).unwrap().positions, [4]);
        // The same records through the segment builder agree in every detail.
        let mut builder = SegmentBuilder::default();
        for (id, text) in [
            (300, "beer beer beer ale"),
            (200, "ale beer"),
            (100, "beer"),
        ] {
            builder.add_record(&record(id, text)).unwrap();
        }
        let bytes = builder.finish();
        same(&mutable, &Reader::parse(&bytes).unwrap(), &["beer", "ale"]);
    }

    #[test]
    fn mutable_index_matches_a_segment_built_from_the_same_records() {
        let texts = [
            (7, "beer beer wine"),
            (3, "craft beer"),
            (9, ""),
            (120, "wine cellar beer"),
            (1, "aardvark bee"),
            (55, "craft craft craft cider"),
        ];
        let mutable = MutableIndex::default();
        let mut builder = SegmentBuilder::default();
        let mut expected_docs = 0;
        for (i, (id, text)) in texts.iter().enumerate() {
            let record = record(*id, text);
            builder.add_record(&record).unwrap();
            if i == 3 {
                // Use the index before it is complete; later appends must not
                // invalidate what was handed out.
                let beer = mutable.term("beer").unwrap().unwrap();
                let early = collect(beer.cursor().unwrap()).unwrap();
                mutable.add_record(record.clone()).unwrap();
                assert_eq!(early.len(), 2, "earlier view stays readable");
            } else {
                mutable.add_record(record.clone()).unwrap();
            }
            if !text.is_empty() {
                expected_docs += 1;
            }
        }
        assert_eq!(mutable.document_count(), expected_docs);
        assert_eq!(
            mutable.add_record(record(7, "duplicate")),
            Err(Error::Unordered)
        );
        let bytes = builder.finish();
        let segment = Reader::parse(&bytes).unwrap();
        same(
            &mutable,
            &segment,
            &[
                "beer", "wine", "craft", "cider", "bee", "absent", "aardvark",
            ],
        );
        let overflow = mutable.expand(Window::All, &|_| true, 2).unwrap();
        assert!(matches!(overflow, Expanded::Overflow));
        let overflow = segment.expand(Window::All, &|_| true, 2).unwrap();
        assert!(matches!(overflow, Expanded::Overflow));
    }
}
