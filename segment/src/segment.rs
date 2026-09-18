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

use std::cell::{OnceCell, RefCell};

use crate::dictionary::{
    BlockFetch, Blocks, Dictionary, DictionaryBuilder, DictionaryIndex, Extent, TermEntry,
};
use crate::forward::{ForwardRecord, ForwardTerm};
use crate::payload::{Payload, PayloadBuilder};
use crate::postings::{Postings, PostingsBuilder, PostingsCursor};
use crate::set::Cursor as _;
use crate::source::Source;
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
    /// with strictly increasing positions. A TID may be added once per
    /// segment. A document with no tokens is not recorded at all: it can match
    /// nothing, and TIN excludes such documents from scoring statistics.
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
        if doc_len == 0 {
            return Ok(());
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

/// A term resolved against a segment. Postings and payload bytes are fetched
/// only when a cursor asks for them, so Boolean queries never read positions.
#[derive(Clone, Copy)]
pub struct Term<'a> {
    pub entry: TermEntry,
    areas: &'a dyn AreaFetch,
}

impl<'a> Term<'a> {
    /// A term over any area provider, such as the mutable index.
    pub const fn new(entry: TermEntry, areas: &'a dyn AreaFetch) -> Self {
        Self { entry, areas }
    }

    pub const fn df(&self) -> u32 {
        self.entry.df
    }

    pub fn postings(&self) -> Result<Postings<'a>> {
        Postings::parse(self.areas.postings_bytes(self.entry.postings)?)
    }

    pub fn cursor(&self) -> Result<PostingsCursor<'a>> {
        self.postings()?.cursor()
    }

    pub fn payload(&self) -> Result<Payload<'a>> {
        Payload::parse(self.areas.payload_bytes(self.entry.payload)?)
    }
}

/// Fetches extents of the postings and payload areas and document lengths.
pub trait AreaFetch {
    fn postings_bytes(&self, extent: Extent) -> Result<&[u8]>;
    fn payload_bytes(&self, extent: Extent) -> Result<&[u8]>;
    fn length(&self, ordinal: u32) -> Result<u32>;
}

#[derive(Clone, Copy, Debug)]
struct Header {
    doc_count: u32,
    total_length: u64,
    dictionary_at: u64,
    dictionary_len: usize,
    postings_at: u64,
    postings_len: usize,
    payload_at: u64,
    payload_len: usize,
    docs_at: u64,
    docs_len: usize,
    lengths_at: u64,
}

/// Reads a segment from any [`Source`], fetching only the extents a query
/// touches. Fetched bytes live in an append-only arena for the reader's
/// lifetime, so borrowed views stay valid however many fetches follow.
pub struct Reader<S: Source> {
    source: S,
    header: Header,
    arena: RefCell<Vec<Box<[u8]>>>,
    dictionary: OnceCell<DictionaryIndex<'static>>,
}

/// A segment held entirely in memory.
pub type Segment<'a> = Reader<&'a [u8]>;

impl<'a> Reader<&'a [u8]> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        Self::new(bytes)
    }
}

impl<S: Source> Reader<S> {
    pub fn new(source: S) -> Result<Self> {
        let total = source.len();
        let head = source.read(0, (total.min(64)) as usize)?;
        let mut reader = crate::reader::Reader::new(&head);
        if reader.take(MAGIC.len())? != MAGIC {
            return Err(Error::Corrupt("segment magic"));
        }
        let doc_count = reader.varint_u32()?;
        let total_length = reader.varint()?;
        let dictionary_len = reader.varint_u32()? as usize;
        let postings_len = reader.varint_u32()? as usize;
        let payload_len = reader.varint_u32()? as usize;
        let docs_len = reader.varint_u32()? as usize;
        let dictionary_at = reader.position() as u64;
        let postings_at = dictionary_at + dictionary_len as u64;
        let payload_at = postings_at + postings_len as u64;
        let docs_at = payload_at + payload_len as u64;
        let lengths_at = docs_at + docs_len as u64;
        if lengths_at + u64::from(doc_count) * 4 != total {
            return Err(Error::Corrupt("segment length"));
        }
        Ok(Self {
            source,
            header: Header {
                doc_count,
                total_length,
                dictionary_at,
                dictionary_len,
                postings_at,
                postings_len,
                payload_at,
                payload_len,
                docs_at,
                docs_len,
                lengths_at,
            },
            arena: RefCell::new(Vec::new()),
            dictionary: OnceCell::new(),
        })
    }

    /// Bytes `[offset, offset + len)` of the source, borrowed for as long as
    /// this reader lives.
    fn load(&self, offset: u64, len: usize) -> Result<&[u8]> {
        if let Some(slice) = self.source.slice(offset, len) {
            return Ok(slice);
        }
        let bytes = self.source.read(offset, len)?.into_boxed_slice();
        let pointer: *const [u8] = &*bytes;
        self.arena.borrow_mut().push(bytes);
        // SAFETY: the box was just moved into the arena, which only ever
        // grows and is dropped with `self`; the heap allocation never moves.
        Ok(unsafe { &*pointer })
    }

    pub const fn document_count(&self) -> u32 {
        self.header.doc_count
    }

    /// Sum of document lengths, for average-length normalization.
    pub const fn total_length(&self) -> u64 {
        self.header.total_length
    }

    fn dictionary_index(&self) -> Result<&DictionaryIndex<'_>> {
        if let Some(index) = self.dictionary.get() {
            return Ok(index);
        }
        let probe = self.load(
            self.header.dictionary_at,
            self.header.dictionary_len.min(32),
        )?;
        let prefix = DictionaryIndex::prefix_len(probe)?;
        if prefix > self.header.dictionary_len {
            return Err(Error::Corrupt("dictionary index length"));
        }
        let bytes = self.load(self.header.dictionary_at, prefix)?;
        let index = DictionaryIndex::parse(bytes)?;
        // SAFETY: `bytes` lives in the arena for the reader's lifetime; the
        // index is only ever handed out shortened to a borrow of `self`.
        let index: DictionaryIndex<'static> = unsafe { std::mem::transmute(index) };
        Ok(self.dictionary.get_or_init(|| index))
    }

    /// The dictionary, with blocks fetched on demand.
    pub fn dictionary(&self) -> Result<Dictionary<'_>> {
        let index = self.dictionary_index()?;
        let blocks = if let Some(all) = self
            .source
            .slice(self.header.dictionary_at, self.header.dictionary_len)
        {
            Blocks::Slice(&all[index.header_len..])
        } else {
            Blocks::Lazy {
                len: self.header.dictionary_len - index.header_len,
                fetch: self,
            }
        };
        Ok(Dictionary::new(index, blocks))
    }

    /// Resolves a dictionary entry obtained earlier from this segment.
    pub fn resolve(&self, entry: TermEntry) -> Result<Term<'_>> {
        let within = |extent: Extent, area_len: usize| {
            extent
                .offset
                .checked_add(u64::from(extent.len))
                .is_some_and(|end| end <= area_len as u64)
        };
        if !within(entry.postings, self.header.postings_len)
            || !within(entry.payload, self.header.payload_len)
        {
            return Err(Error::Truncated);
        }
        Ok(Term { entry, areas: self })
    }

    pub fn term(&self, term: &str) -> Result<Option<Term<'_>>> {
        self.dictionary()?
            .get(term)?
            .map(|entry| self.resolve(entry))
            .transpose()
    }

    /// Resolves each dictionary item from an expansion iterator.
    pub fn resolve_all<'s>(
        &'s self,
        items: impl Iterator<Item = Result<(String, TermEntry)>> + 's,
    ) -> impl Iterator<Item = Result<(String, Term<'s>)>> + 's {
        items.map(move |item| {
            let (term, entry) = item?;
            Ok((term, self.resolve(entry)?))
        })
    }

    /// Cursor over every document in the segment, the universe for NOT.
    pub fn documents(&self) -> Result<PostingsCursor<'_>> {
        Postings::parse(self.load(self.header.docs_at, self.header.docs_len)?)?.cursor()
    }

    /// Length of the document at `tid`, if it is in this segment.
    pub fn document_length(&self, tid: Tid) -> Result<Option<u32>> {
        let mut cursor = self.documents()?;
        cursor
            .rank(tid)?
            .map(|ordinal| self.length_at(ordinal))
            .transpose()
    }

    /// Length by document ordinal, as reported by [`Reader::documents`].
    pub fn length_at(&self, ordinal: u32) -> Result<u32> {
        self.lengths().get(ordinal)
    }

    /// A copyable handle on the length table.
    pub fn lengths(&self) -> Lengths<'_> {
        let len = self.header.doc_count as usize * 4;
        match self.source.slice(self.header.lengths_at, len) {
            Some(bytes) => Lengths::Bytes(bytes),
            None => Lengths::Lazy {
                fetch: self,
                count: self.header.doc_count,
            },
        }
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
        for item in self.dictionary()?.iter() {
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
}

impl<S: Source> BlockFetch for Reader<S> {
    fn fetch_block(&self, offset: usize, len: usize) -> Result<&[u8]> {
        let index = self.dictionary_index()?;
        let at = self.header.dictionary_at + index.header_len as u64 + offset as u64;
        self.load(at, len)
    }
}

impl<S: Source> AreaFetch for Reader<S> {
    fn postings_bytes(&self, extent: Extent) -> Result<&[u8]> {
        self.load(self.header.postings_at + extent.offset, extent.len as usize)
    }

    fn payload_bytes(&self, extent: Extent) -> Result<&[u8]> {
        self.load(self.header.payload_at + extent.offset, extent.len as usize)
    }

    fn length(&self, ordinal: u32) -> Result<u32> {
        if ordinal >= self.header.doc_count {
            return Err(Error::Corrupt("document ordinal out of range"));
        }
        let at = self.header.lengths_at + u64::from(ordinal) * 4;
        if let Some(bytes) = self.source.slice(at, 4) {
            return Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
        }
        let bytes = self.source.read(at, 4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

/// Document lengths addressed by document ordinal.
#[derive(Clone, Copy)]
pub enum Lengths<'a> {
    Bytes(&'a [u8]),
    Lazy {
        fetch: &'a dyn AreaFetch,
        count: u32,
    },
}

impl Lengths<'_> {
    pub fn get(&self, ordinal: u32) -> Result<u32> {
        match self {
            Self::Bytes(bytes) => {
                let at = ordinal as usize * 4;
                let bytes = bytes
                    .get(at..at + 4)
                    .ok_or(Error::Corrupt("document ordinal out of range"))?;
                Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            }
            Self::Lazy { fetch, count } => {
                if ordinal >= *count {
                    return Err(Error::Corrupt("document ordinal out of range"));
                }
                fetch.length(ordinal)
            }
        }
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
        // The empty document is not recorded.
        assert_eq!(segment.document_count(), 2);
        assert_eq!(segment.total_length(), 5);
        assert_eq!(
            collect(segment.documents().unwrap()).unwrap(),
            [tid(0, 5), tid(2, 1)]
        );
        assert_eq!(segment.document_length(tid(2, 1)).unwrap(), Some(3));
        assert_eq!(segment.document_length(tid(1, 3)).unwrap(), None);
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
        let terms: Vec<String> = segment
            .dictionary()
            .unwrap()
            .iter()
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(terms, ["beer", "craft", "wine"]);
        let expanded: Vec<(String, u32)> = segment
            .resolve_all(segment.dictionary().unwrap().prefix("c"))
            .map(|r| r.map(|(t, term)| (t, term.df())).unwrap())
            .collect();
        assert_eq!(expanded, [("craft".to_owned(), 1)]);

        // NOT beer, against the segment's own universe.
        let not_beer =
            crate::set::Difference::new(segment.documents().unwrap(), beer.cursor().unwrap())
                .unwrap();
        assert_eq!(collect(not_beer).unwrap(), Vec::<Tid>::new());
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
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].tid, tid(2, 1));
        assert_eq!(records[0].doc_len, 3);
        assert_eq!(records[0].tokens(), [("beer", 1), ("beer", 2), ("wine", 3)]);
        // Rebuilding from the records yields an equivalent segment.
        let mut rebuilt = SegmentBuilder::default();
        for record in &records {
            rebuilt.add_record(record).unwrap();
        }
        let rebuilt = rebuilt.finish();
        let again = Segment::parse(&rebuilt).unwrap();
        assert_eq!(again.document_count(), 1);
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
