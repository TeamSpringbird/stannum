//! One immutable segment: dictionary, postings area, payload area and a
//! document table, assembled from documents and read back by term or TID.
//!
//! ```text
//! blob   := magic "LSG1", doc_count varint, total_length varint,
//!           dictionary_len varint, postings_len varint, payload_len varint,
//!           docs_len varint,
//!           dictionary, postings_area, payload_area, docs, lengths
//! docs   := a `postings` stream of every document TID in the segment
//! lengths:= u32le per document, in TID order (addressed by docs ordinal)
//! ```
//!
//! The postings and payload areas are concatenations of per-term streams;
//! each dictionary entry's extents locate them. Document frequency is the
//! postings count and `max_tf_bucket` is computed while building, so the
//! dictionary alone answers selectivity and score-bound questions.
//!
//! The builder holds the segment in memory. That matches the intended use,
//! folding a bounded write buffer, and an index build that partitions the heap
//! into bounded ranges. A sort-based external builder can share the same byte
//! layout later.

use std::collections::BTreeMap;

use crate::dictionary::{Dictionary, DictionaryBuilder, Extent, TermEntry};
use crate::forward::{ForwardRecord, ForwardTerm};
use crate::payload::{Payload, PayloadBuilder};
use crate::postings::{Postings, PostingsBuilder, PostingsCursor};
use crate::reader::Reader;
use crate::set::Cursor as _;
use crate::tf_bucket::TfBucket;
use crate::{Error, Result, Tid, varint};

const MAGIC: &[u8; 4] = b"LSG1";

struct Occurrence {
    tid: Tid,
    positions: Vec<u32>,
}

/// Accumulates documents in any TID order.
#[derive(Default)]
pub struct SegmentBuilder {
    lengths: BTreeMap<Tid, u32>,
    terms: BTreeMap<String, Vec<Occurrence>>,
}

impl SegmentBuilder {
    /// Adds one document. `tokens` are `(term, position)` in document order
    /// with strictly increasing positions; `doc_len` is the token count. A TID
    /// may be added once per segment; an empty document is still recorded.
    pub fn add_document<'t>(
        &mut self,
        tid: Tid,
        tokens: impl IntoIterator<Item = (&'t str, u32)>,
    ) -> Result<()> {
        Tid::new(tid.block, tid.offset)?;
        if self.lengths.contains_key(&tid) {
            return Err(Error::Unordered);
        }
        let mut by_term = BTreeMap::<&str, Vec<u32>>::new();
        let mut doc_len = 0u32;
        let mut last = None;
        for (term, position) in tokens {
            if term.is_empty() {
                return Err(Error::EmptyTerm);
            }
            if last.is_some_and(|last| last >= position) {
                return Err(Error::InvalidPositions);
            }
            last = Some(position);
            doc_len += 1;
            by_term.entry(term).or_default().push(position);
        }
        self.lengths.insert(tid, doc_len);
        for (term, positions) in by_term {
            self.terms
                .entry(term.to_owned())
                .or_default()
                .push(Occurrence { tid, positions });
        }
        Ok(())
    }

    /// Adds a document from a forward record, as a buffer fold does.
    pub fn add_record(&mut self, record: &ForwardRecord) -> Result<()> {
        self.add_document(record.tid, record.tokens())
    }

    pub fn document_count(&self) -> usize {
        self.lengths.len()
    }

    pub fn finish(self) -> Vec<u8> {
        let mut dictionary = DictionaryBuilder::default();
        let mut postings_area = Vec::new();
        let mut payload_area = Vec::new();
        for (term, mut occurrences) in self.terms {
            occurrences.sort_unstable_by_key(|occurrence| occurrence.tid);
            let mut postings = PostingsBuilder::default();
            let mut payload = PayloadBuilder::default();
            let mut max_tf_bucket = 0;
            for occurrence in &occurrences {
                let bucket = TfBucket::from_count(occurrence.positions.len() as u32).value();
                max_tf_bucket = max_tf_bucket.max(bucket);
                postings
                    .push(occurrence.tid)
                    .expect("occurrences are unique per document and sorted");
                payload
                    .push(bucket, &occurrence.positions)
                    .expect("positions validated on insertion");
            }
            let postings_bytes = postings.finish();
            let payload_bytes = payload.finish();
            let entry = TermEntry {
                df: occurrences.len() as u32,
                max_tf_bucket,
                postings: Extent {
                    offset: postings_area.len() as u64,
                    len: postings_bytes.len() as u32,
                },
                payload: Extent {
                    offset: payload_area.len() as u64,
                    len: payload_bytes.len() as u32,
                },
            };
            postings_area.extend_from_slice(&postings_bytes);
            payload_area.extend_from_slice(&payload_bytes);
            dictionary
                .push(&term, entry)
                .expect("terms come from an ordered map");
        }
        let dictionary_bytes = dictionary.finish();
        let mut docs = PostingsBuilder::default();
        let mut lengths = Vec::with_capacity(self.lengths.len() * 4);
        let mut total_length = 0u64;
        for (tid, doc_len) in &self.lengths {
            docs.push(*tid).expect("map keys are ordered and unique");
            lengths.extend_from_slice(&doc_len.to_le_bytes());
            total_length += u64::from(*doc_len);
        }
        let docs_bytes = docs.finish();

        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        varint::put(&mut out, self.lengths.len() as u64);
        varint::put(&mut out, total_length);
        varint::put(&mut out, dictionary_bytes.len() as u64);
        varint::put(&mut out, postings_area.len() as u64);
        varint::put(&mut out, payload_area.len() as u64);
        varint::put(&mut out, docs_bytes.len() as u64);
        out.extend_from_slice(&dictionary_bytes);
        out.extend_from_slice(&postings_area);
        out.extend_from_slice(&payload_area);
        out.extend_from_slice(&docs_bytes);
        out.extend_from_slice(&lengths);
        out
    }
}

/// A term resolved against a segment.
#[derive(Clone, Copy, Debug)]
pub struct Term<'a> {
    pub entry: TermEntry,
    postings: &'a [u8],
    payload: &'a [u8],
}

impl<'a> Term<'a> {
    pub const fn df(&self) -> u32 {
        self.entry.df
    }

    pub fn postings(&self) -> Result<Postings<'a>> {
        Postings::parse(self.postings)
    }

    pub fn cursor(&self) -> Result<PostingsCursor<'a>> {
        self.postings()?.cursor()
    }

    pub fn payload(&self) -> Result<Payload<'a>> {
        Payload::parse(self.payload)
    }
}

#[derive(Clone, Debug)]
pub struct Segment<'a> {
    doc_count: u32,
    total_length: u64,
    dictionary: Dictionary<'a>,
    postings_area: &'a [u8],
    payload_area: &'a [u8],
    docs: &'a [u8],
    lengths: &'a [u8],
}

impl<'a> Segment<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        if reader.take(MAGIC.len())? != MAGIC {
            return Err(Error::Corrupt("segment magic"));
        }
        let doc_count = reader.varint_u32()?;
        let total_length = reader.varint()?;
        let dictionary_len = reader.varint_u32()? as usize;
        let postings_len = reader.varint_u32()? as usize;
        let payload_len = reader.varint_u32()? as usize;
        let docs_len = reader.varint_u32()? as usize;
        let dictionary = Dictionary::parse(reader.take(dictionary_len)?)?;
        let postings_area = reader.take(postings_len)?;
        let payload_area = reader.take(payload_len)?;
        let docs = reader.take(docs_len)?;
        let lengths = reader.take(doc_count as usize * 4)?;
        if reader.remaining() != 0 {
            return Err(Error::Corrupt("segment trailing bytes"));
        }
        if Postings::parse(docs)?.count() != doc_count {
            return Err(Error::Corrupt("segment document count"));
        }
        Ok(Self {
            doc_count,
            total_length,
            dictionary,
            postings_area,
            payload_area,
            docs,
            lengths,
        })
    }

    pub const fn document_count(&self) -> u32 {
        self.doc_count
    }

    /// Sum of document lengths, for average-length normalization.
    pub const fn total_length(&self) -> u64 {
        self.total_length
    }

    pub const fn dictionary(&self) -> &Dictionary<'a> {
        &self.dictionary
    }

    fn resolve(&self, entry: TermEntry) -> Result<Term<'a>> {
        let slice = |area: &'a [u8], extent: Extent| -> Result<&'a [u8]> {
            let start = usize::try_from(extent.offset).map_err(|_| Error::Truncated)?;
            let end = start
                .checked_add(extent.len as usize)
                .ok_or(Error::Truncated)?;
            area.get(start..end).ok_or(Error::Truncated)
        };
        Ok(Term {
            entry,
            postings: slice(self.postings_area, entry.postings)?,
            payload: slice(self.payload_area, entry.payload)?,
        })
    }

    pub fn term(&self, term: &str) -> Result<Option<Term<'a>>> {
        self.dictionary
            .get(term)?
            .map(|entry| self.resolve(entry))
            .transpose()
    }

    /// Resolves each dictionary item from an expansion iterator.
    pub fn resolve_all<'s>(
        &'s self,
        items: impl Iterator<Item = Result<(String, TermEntry)>> + 's,
    ) -> impl Iterator<Item = Result<(String, Term<'a>)>> + 's {
        items.map(move |item| {
            let (term, entry) = item?;
            Ok((term, self.resolve(entry)?))
        })
    }

    /// Cursor over every document in the segment, the universe for NOT.
    pub fn documents(&self) -> Result<PostingsCursor<'a>> {
        Postings::parse(self.docs)?.cursor()
    }

    /// Length of the document at `tid`, if it is in this segment.
    pub fn document_length(&self, tid: Tid) -> Result<Option<u32>> {
        let mut cursor = self.documents()?;
        cursor
            .rank(tid)?
            .map(|ordinal| self.length_at(ordinal))
            .transpose()
    }

    /// Rebuilds every document as a forward record, skipping those for which
    /// `skip` returns true. This is how folds and merges carry documents
    /// between segments without re-reading the heap.
    pub fn records(&self, mut skip: impl FnMut(Tid) -> bool) -> Result<Vec<ForwardRecord>> {
        let mut by_document: BTreeMap<Tid, Vec<ForwardTerm>> = BTreeMap::new();
        let mut documents = self.documents()?;
        while let Some(tid) = documents.current() {
            if !skip(tid) {
                by_document.insert(tid, Vec::new());
            }
            documents.advance()?;
        }
        for item in self.dictionary().iter() {
            let (term, entry) = item?;
            let resolved = self.resolve(entry)?;
            let mut postings = resolved.cursor()?;
            let mut payload = resolved.payload()?.cursor();
            while let Some(tid) = postings.current() {
                let mut positions = Vec::new();
                payload.next_into(&mut positions)?;
                if let Some(terms) = by_document.get_mut(&tid) {
                    terms.push(ForwardTerm {
                        term: term.clone(),
                        positions,
                    });
                }
                postings.advance()?;
            }
        }
        let mut lengths = self.documents()?;
        let mut out = Vec::with_capacity(by_document.len());
        for (tid, terms) in by_document {
            let ordinal = lengths
                .rank(tid)?
                .ok_or(Error::Corrupt("document missing from table"))?;
            out.push(ForwardRecord {
                tid,
                doc_len: self.length_at(ordinal)?,
                terms,
            });
        }
        Ok(out)
    }

    /// Length by document ordinal, as reported by [`Segment::documents`].
    pub fn length_at(&self, ordinal: u32) -> Result<u32> {
        self.lengths().get(ordinal)
    }

    /// A copyable handle on the length table.
    pub const fn lengths(&self) -> Lengths<'a> {
        Lengths(self.lengths)
    }
}

/// Document lengths addressed by document ordinal.
#[derive(Clone, Copy, Debug)]
pub struct Lengths<'a>(&'a [u8]);

impl Lengths<'_> {
    pub fn get(&self, ordinal: u32) -> Result<u32> {
        let at = ordinal as usize * 4;
        let bytes = self
            .0
            .get(at..at + 4)
            .ok_or(Error::Corrupt("document ordinal out of range"))?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::set::{Cursor, collect};

    fn tid(block: u32, offset: u16) -> Tid {
        Tid::new(block, offset).unwrap()
    }

    fn tokens(text: &str) -> Vec<(&str, u32)> {
        text.split_whitespace()
            .enumerate()
            .map(|(i, word)| (word, i as u32 + 1))
            .collect()
    }

    #[test]
    fn builds_and_reads_terms_documents_and_lengths() {
        let mut builder = SegmentBuilder::default();
        builder
            .add_document(tid(2, 1), tokens("beer beer wine"))
            .unwrap();
        builder
            .add_document(tid(0, 5), tokens("craft beer"))
            .unwrap();
        builder.add_document(tid(1, 3), tokens("")).unwrap();
        assert_eq!(
            builder.add_document(tid(2, 1), tokens("dup")),
            Err(Error::Unordered)
        );
        let bytes = builder.finish();
        let segment = Segment::parse(&bytes).unwrap();
        assert_eq!(segment.document_count(), 3);
        assert_eq!(segment.total_length(), 5);
        assert_eq!(
            collect(segment.documents().unwrap()).unwrap(),
            [tid(0, 5), tid(1, 3), tid(2, 1)]
        );
        assert_eq!(segment.document_length(tid(2, 1)).unwrap(), Some(3));
        assert_eq!(segment.document_length(tid(1, 3)).unwrap(), Some(0));
        assert_eq!(segment.document_length(tid(1, 4)).unwrap(), None);

        let beer = segment.term("beer").unwrap().unwrap();
        assert_eq!(beer.df(), 2);
        assert_eq!(beer.entry.max_tf_bucket, TfBucket::from_count(2).value());
        assert_eq!(
            collect(beer.cursor().unwrap()).unwrap(),
            [tid(0, 5), tid(2, 1)]
        );
        let payload = beer.payload().unwrap();
        let mut cursor = beer.cursor().unwrap();
        let ordinal = cursor.rank(tid(2, 1)).unwrap().unwrap();
        assert_eq!(payload.get(ordinal).unwrap().positions, [1, 2]);
        assert_eq!(payload.get(0).unwrap().positions, [2]);

        assert!(segment.term("ale").unwrap().is_none());
        let terms: Vec<String> = segment.dictionary().iter().map(|r| r.unwrap().0).collect();
        assert_eq!(terms, ["beer", "craft", "wine"]);
        let expanded: Vec<(String, u32)> = segment
            .resolve_all(segment.dictionary().prefix("c"))
            .map(|r| r.map(|(t, term)| (t, term.df())).unwrap())
            .collect();
        assert_eq!(expanded, [("craft".to_owned(), 1)]);

        // NOT beer, against the segment's own universe.
        let not_beer =
            crate::set::Difference::new(segment.documents().unwrap(), beer.cursor().unwrap())
                .unwrap();
        assert_eq!(collect(not_beer).unwrap(), [tid(1, 3)]);
    }

    #[test]
    fn records_reconstruct_documents_and_skip_dead_ones() {
        let mut builder = SegmentBuilder::default();
        builder
            .add_document(tid(2, 1), tokens("beer beer wine"))
            .unwrap();
        builder
            .add_document(tid(0, 5), tokens("craft beer"))
            .unwrap();
        builder.add_document(tid(1, 3), tokens("")).unwrap();
        let bytes = builder.finish();
        let segment = Segment::parse(&bytes).unwrap();
        let records = segment.records(|t| t == tid(0, 5)).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].tid, tid(1, 3));
        assert_eq!(records[0].doc_len, 0);
        assert_eq!(records[1].tid, tid(2, 1));
        assert_eq!(records[1].doc_len, 3);
        assert_eq!(records[1].tokens(), [("beer", 1), ("beer", 2), ("wine", 3)]);
        // Rebuilding from the records yields an equivalent segment.
        let mut rebuilt = SegmentBuilder::default();
        for record in &records {
            rebuilt.add_record(record).unwrap();
        }
        let rebuilt = rebuilt.finish();
        let again = Segment::parse(&rebuilt).unwrap();
        assert_eq!(again.document_count(), 2);
        assert_eq!(again.term("craft").unwrap().map(|t| t.df()), None);
        assert_eq!(again.term("beer").unwrap().map(|t| t.df()), Some(1));
    }

    #[test]
    fn empty_segment_and_corruption() {
        let bytes = SegmentBuilder::default().finish();
        let segment = Segment::parse(&bytes).unwrap();
        assert_eq!(segment.document_count(), 0);
        assert!(segment.term("x").unwrap().is_none());
        assert_eq!(segment.documents().unwrap().current(), None);
        assert!(Segment::parse(&bytes[..bytes.len() - 1]).is_err());
        assert!(Segment::parse(b"LSG2").is_err());
        let mut builder = SegmentBuilder::default();
        builder.add_document(tid(1, 1), tokens("a b c")).unwrap();
        let mut bytes = builder.finish();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x80; // Corrupt the length table.
        let segment = Segment::parse(&bytes).unwrap();
        assert_eq!(
            segment.document_length(tid(1, 1)).unwrap(),
            Some(3 | 0x8000_0000)
        );
        bytes.push(0);
        assert!(Segment::parse(&bytes).is_err());
    }
}
