// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! One immutable segment: dictionary, ordinal streams, payload, document
//! table and lengths, assembled from documents and read back by term or by
//! tuple location.
//!
//! ```text
//! blob     := magic "STN3", doc_count varint, total_length varint,
//!             dictionary_len varint, ordinals_len varint, payload_len varint,
//!             pages_len varint,
//!             dictionary, ordinals_area, payload_area, offsets, lengths,
//!             classes, pages
//! offsets  := u16le per document, in heap order (see [`crate::docs`])
//! lengths  := u32le per document, in heap order
//! classes  := u8 per document, in heap order (see [`crate::length_class`])
//! pages    := (block u32le, first u32le)* per heap block holding a document
//! ```
//!
//! Documents are numbered in heap order; that ordinal addresses the offsets,
//! lengths and pages tables. The ordinals area concatenates one
//! [`crate::ordinals`] stream per term: the term's documents as ordinals with
//! a score bound per chunk, which is the only representation of a term's
//! document set. The payload area concatenates one [`crate::payload`] stream
//! per term, addressed by a document's rank within the term's stream. Each
//! dictionary entry's extents locate the two streams; document frequency and
//! the largest frequency bucket are in the entry, so the dictionary alone
//! answers selectivity and score-bound questions.
//!
//! Only this signature is read. Earlier formats (`LSG1` to `LSG5`, `STN1`)
//! stored the document set twice or the bucket beside the positions;
//! indexes in them are rebuilt with `REINDEX`.
//!
//! The builder holds the segment in memory. That matches the intended use,
//! folding a bounded write buffer, and an index build that partitions the heap
//! into bounded ranges.

use std::collections::BTreeMap;

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use crate::dictionary::{
    BlockFetch, Blocks, Dictionary, DictionaryBuilder, DictionaryIndex, Extent, TermEntry,
};
use crate::docs::{self, DocCursor, DocTable, PageCursor, PageTable, TidCursor};
use crate::forward::{ForwardRecord, ForwardTerm};
use crate::ordinals::Ordinals;
use crate::payload::{Payload, PayloadBuilder};
use crate::source::Source;
use crate::tf_bucket::TfBucket;
use crate::{Error, Result, Tid, varint};

/// The signature that opens a segment blob.
pub const MAGIC: &[u8; 4] = b"STN3";

struct Occurrence {
    tid: Tid,
    doc_len: u32,
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
                .push(Occurrence {
                    tid,
                    doc_len,
                    positions,
                });
        }
        Ok(())
    }

    /// Adds a document from a forward record, as a buffer fold does.
    pub fn add_record(&mut self, record: &ForwardRecord) -> Result<()> {
        Tid::new(record.tid.block, record.tid.offset)?;
        if self.lengths.contains_key(&record.tid) {
            return Err(Error::Unordered);
        }
        // Records emitted by our codecs are already grouped by term. Avoid
        // expanding them into tokens, sorting by position and regrouping them.
        // Keep the old normalization behavior for arbitrary public records:
        // unsorted/repeated terms and positions, or empty groups, are allowed
        // when their normalized token stream is valid.
        let canonical = record.terms.windows(2).all(|w| w[0].term < w[1].term)
            && record.terms.iter().all(|term| {
                !term.term.is_empty()
                    && !term.positions.is_empty()
                    && term.positions.windows(2).all(|w| w[0] < w[1])
            });
        let doc_len = record.terms.iter().try_fold(0u32, |n, term| {
            n.checked_add(u32::try_from(term.positions.len()).ok()?)
        });
        let Some(doc_len) = doc_len.filter(|_| canonical) else {
            return self.add_document(record.tid, record.tokens());
        };
        if doc_len == 0 {
            return Ok(());
        }
        let max_position = record
            .terms
            .iter()
            .filter_map(|term| term.positions.last())
            .copied()
            .max()
            .unwrap();
        // Cross-term duplicate positions must still fail before any mutation.
        // A bounded bitmap is cheap for normal token positions. Sparse public
        // records use the old path rather than allocating by a huge position.
        if u64::from(max_position) > u64::from(doc_len) * 8 {
            return self.add_document(record.tid, record.tokens());
        }
        let mut seen = vec![0u64; max_position as usize / 64 + 1];
        for term in &record.terms {
            for position in &term.positions {
                let word = &mut seen[*position as usize / 64];
                let bit = 1u64 << (position % 64);
                if *word & bit != 0 {
                    return Err(Error::InvalidPositions);
                }
                *word |= bit;
            }
        }
        // Historically the builder recomputes length from actual tokens, even
        // if a caller's record.doc_len disagrees. Preserve that scoring input.
        self.lengths.insert(record.tid, doc_len);
        for term in &record.terms {
            self.terms
                .entry(term.term.clone())
                .or_default()
                .push(Occurrence {
                    tid: record.tid,
                    doc_len,
                    positions: term.positions.clone(),
                });
        }
        Ok(())
    }

    pub fn document_count(&self) -> usize {
        self.lengths.len()
    }

    pub fn finish(self) -> Vec<u8> {
        let mut dictionary = DictionaryBuilder::default();
        let mut ordinals_area = Vec::new();
        let mut payload_area = Vec::new();
        let documents: Vec<Tid> = self.lengths.keys().copied().collect();
        let mut ordinals = Vec::new();
        let mut scores = Vec::new();
        for (term, mut occurrences) in self.terms {
            occurrences.sort_unstable_by_key(|occurrence| occurrence.tid);
            let mut payload = PayloadBuilder::default();
            let mut max_tf_bucket = 0;
            ordinals.clear();
            scores.clear();
            for occurrence in &occurrences {
                let bucket = TfBucket::from_count(occurrence.positions.len() as u32).value();
                max_tf_bucket = max_tf_bucket.max(bucket);
                scores.push((bucket, occurrence.doc_len));
                ordinals.push(
                    documents
                        .binary_search(&occurrence.tid)
                        .expect("every occurrence belongs to a recorded document")
                        as u32,
                );
                payload
                    .push(&occurrence.positions)
                    .expect("positions validated on insertion");
            }
            let ordinals_bytes = crate::ordinals::encode_scored(&ordinals, &scores);
            let payload_bytes = payload.finish();
            let entry = TermEntry {
                df: occurrences.len() as u32,
                max_tf_bucket,
                ordinals: Extent {
                    offset: ordinals_area.len() as u64,
                    len: ordinals_bytes.len() as u32,
                },
                payload: Extent {
                    offset: payload_area.len() as u64,
                    len: payload_bytes.len() as u32,
                },
            };
            ordinals_area.extend_from_slice(&ordinals_bytes);
            payload_area.extend_from_slice(&payload_bytes);
            dictionary
                .push(&term, entry)
                .expect("terms come from an ordered map");
        }
        let dictionary_bytes = dictionary.finish();
        let mut lengths = Vec::with_capacity(self.lengths.len() * 4);
        let mut classes = Vec::with_capacity(self.lengths.len());
        let mut total_length = 0u64;
        for doc_len in self.lengths.values() {
            lengths.extend_from_slice(&doc_len.to_le_bytes());
            classes.push(crate::length_class::class_of(*doc_len));
            total_length += u64::from(*doc_len);
        }
        let offsets = docs::offsets(documents.iter().copied());
        let pages = docs::page_table(documents.iter().copied());
        assemble(
            self.lengths.len() as u32,
            total_length,
            &dictionary_bytes,
            &ordinals_area,
            &payload_area,
            &offsets,
            &lengths,
            &classes,
            &pages,
        )
    }
}

/// Lays the sections out as a blob with its header.
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble(
    doc_count: u32,
    total_length: u64,
    dictionary: &[u8],
    ordinals: &[u8],
    payload: &[u8],
    offsets: &[u8],
    lengths: &[u8],
    classes: &[u8],
    pages: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        32 + dictionary.len()
            + ordinals.len()
            + payload.len()
            + offsets.len()
            + lengths.len()
            + classes.len()
            + pages.len(),
    );
    out.extend_from_slice(&header(
        doc_count,
        total_length,
        dictionary.len(),
        ordinals.len(),
        payload.len(),
        pages.len(),
    ));
    for section in [
        dictionary, ordinals, payload, offsets, lengths, classes, pages,
    ] {
        out.extend_from_slice(section);
    }
    out
}

/// The header of a blob whose sections have the given lengths.
pub(crate) fn header(
    doc_count: u32,
    total_length: u64,
    dictionary_len: usize,
    ordinals_len: usize,
    payload_len: usize,
    pages_len: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.extend_from_slice(MAGIC);
    for n in [
        u64::from(doc_count),
        total_length,
        dictionary_len as u64,
        ordinals_len as u64,
        payload_len as u64,
        pages_len as u64,
    ] {
        varint::put(&mut out, n);
    }
    out
}

/// A term resolved against a segment. Stream bytes are fetched only when a
/// cursor asks for them, so Boolean queries never read positions.
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

    /// The term's documents as ordinals, with a bound per chunk.
    pub fn ordinals(&self) -> Result<Ordinals<'a>> {
        let fetch = OrdinalsFetch {
            areas: self.areas,
            base: self.entry.ordinals.offset,
        };
        Ordinals::open(fetch, u64::from(self.entry.ordinals.len), true)
    }

    /// The term's documents as tuple locations, in heap order.
    pub fn cursor(&self) -> Result<TidCursor<'a>> {
        TidCursor::new(self.ordinals()?.cursor()?, self.areas.doc_table()?)
    }

    /// The term's documents a heap page at a time.
    pub fn pages(&self) -> Result<PageCursor<'a>> {
        PageCursor::new(self.ordinals()?.cursor()?, self.areas.doc_table()?)
    }

    /// Whether a scan over the term should work a heap page at a time.
    pub fn prefers_pages(&self) -> Result<bool> {
        Ok(self.ordinals()?.prefers_pages())
    }

    pub fn payload(&self) -> Result<Payload<'a>> {
        if self.areas.ranged_payloads() {
            let extent = self.entry.payload;
            return Payload::open(self.areas, extent.offset, extent.len as usize);
        }
        Payload::parse(self.areas.payload_bytes(self.entry.payload)?)
    }
}

/// One term's stream within the ordinals area, fetched a range at a time.
struct OrdinalsFetch<'a> {
    areas: &'a dyn AreaFetch,
    base: u64,
}

impl<'a> crate::ordinals::Fetch<'a> for OrdinalsFetch<'a> {
    fn fetch(&self, offset: u64, len: usize) -> Result<&'a [u8]> {
        let at = self.base.checked_add(offset).ok_or(Error::Truncated)?;
        self.areas.ordinals_bytes(at, len)
    }
    fn fetch_owned(&self, offset: u64, len: usize) -> Result<Rc<[u8]>> {
        let at = self.base.checked_add(offset).ok_or(Error::Truncated)?;
        self.areas.ordinals_range_owned(at, len)
    }
}

/// Document lengths per window of the length table a paged source hands out.
/// One lookup needs four bytes, and the table of a large segment is far
/// bigger than a backend's read cache, so a wide window evicted everything
/// else to serve one value. The window is a fraction of a page and is not
/// cached privately: the table is already in shared buffers, which every
/// backend shares.
/// Documents per length window handed to a walk: a candidate's length is a
/// window fetch when the window last read does not hold it, and candidates
/// are sparse enough that a window served about one of them, so a 2,048
/// document window was an 8 KiB copy per candidate.
const LENGTH_WINDOW: u32 = 64;

/// Fetches the streams of the ordinals and payload areas, the document
/// table and document lengths.
pub trait AreaFetch {
    /// Bytes of the ordinals area.
    fn ordinals_bytes(&self, offset: u64, len: usize) -> Result<&[u8]>;
    /// Ranges a cursor reads and moves on from, shared through the bounded
    /// [`crate::cache`] rather than kept with the reader, so a query that
    /// sweeps a frequent term's streams holds one span of each at a time.
    fn ordinals_range_owned(&self, offset: u64, len: usize) -> Result<Rc<[u8]>> {
        self.ordinals_bytes(offset, len).map(Rc::from)
    }
    /// A term's whole payload stream.
    fn payload_bytes(&self, extent: Extent) -> Result<&[u8]>;
    /// Whether payload streams are read a range at a time through
    /// [`AreaFetch::payload_range`] rather than as whole extents.
    fn ranged_payloads(&self) -> bool {
        false
    }
    /// Bytes of the payload area.
    fn payload_range(&self, _offset: u64, _len: usize) -> Result<&[u8]> {
        Err(Error::Corrupt("source has no ranged payloads"))
    }
    fn payload_range_owned(&self, offset: u64, len: usize) -> Result<Rc<[u8]>> {
        self.payload_range(offset, len).map(Rc::from)
    }
    /// The document table.
    fn doc_table(&self) -> Result<DocTable<'_>>;
    fn length(&self, ordinal: u32) -> Result<u32>;
    /// The document's length class (see [`crate::length_class`]).
    fn length_class(&self, ordinal: u32) -> Result<u8>;
    /// The window of the length table holding `ordinal` as an owned copy,
    /// with the window's first ordinal; `None` for a source whose table is
    /// held whole.
    fn length_window_owned(&self, _ordinal: u32) -> Result<Option<(Rc<[u8]>, u32)>> {
        Ok(None)
    }
}

#[derive(Clone, Copy, Debug)]
struct Header {
    doc_count: u32,
    total_length: u64,
    dictionary_at: u64,
    dictionary_len: usize,
    ordinals_at: u64,
    ordinals_len: usize,
    payload_at: u64,
    payload_len: usize,
    offsets_at: u64,
    lengths_at: u64,
    classes_at: u64,
    pages_at: u64,
    pages_len: usize,
}

/// Reads a segment from any [`Source`], fetching only the extents a query
/// touches. Fetched bytes live in an arena for the reader's lifetime, keyed
/// by extent so a repeated fetch returns the same bytes; borrowed views stay
/// valid however many fetches follow.
pub struct Reader<S: Source> {
    source: S,
    header: Header,
    /// Identity in the [`crate::cache`].
    id: u64,
    arena: RefCell<Arena>,
    arena_bytes: Cell<usize>,
    dictionary: OnceCell<DictionaryIndex<'static>>,
    /// The page table, checked once: every cursor over the segment starts
    /// from it, and a large segment's table has hundreds of thousands of
    /// entries.
    pages: OnceCell<PageTable<'static>>,
    /// The length chunk read last, by offset: scoring reads lengths in
    /// document order, so consecutive reads hit the same chunk.
    last_chunk: Cell<Option<(u64, *const [u8])>>,
    /// The class window read last, as (first ordinal, bytes): a walk asks
    /// for classes in ordinal order, and a window fetch was a buffer read
    /// and an 8 KiB copy per candidate.
    last_classes: RefCell<Option<(u32, Rc<[u8]>)>>,
}

/// Byte lengths of a segment's sections, in blob order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sections {
    pub header: usize,
    pub dictionary: usize,
    pub ordinals: usize,
    pub payload: usize,
    pub offsets: usize,
    pub lengths: usize,
    pub classes: usize,
    pub pages: usize,
}

/// Fetched byte ranges by (offset, len). A ranged cursor fetches thousands
/// of windows per query, so the hash is the cheap one.
type Arena = rustc_hash::FxHashMap<(u64, usize), Box<[u8]>>;

/// Granularity at which document lengths are fetched from a paged source.
const LENGTH_CHUNK: u64 = 4096;

/// Documents per window of the length-class table. A window is read
/// uncached from shared buffers and held until a lookup falls outside it;
/// candidates are sparse, so a wide window was mostly copied for one value.
pub const CLASS_WINDOW: u32 = 1024;

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
        if reader.take(4)? != MAGIC {
            return Err(Error::Corrupt("segment magic"));
        }
        let doc_count = reader.varint_u32()?;
        let total_length = reader.varint()?;
        let dictionary_len = reader.varint_u32()? as usize;
        let ordinals_len = reader.varint_u32()? as usize;
        let payload_len = reader.varint_u32()? as usize;
        let pages_len = reader.varint_u32()? as usize;
        let dictionary_at = reader.position() as u64;
        let ordinals_at = dictionary_at + dictionary_len as u64;
        let payload_at = ordinals_at + ordinals_len as u64;
        let offsets_at = payload_at + payload_len as u64;
        let lengths_at = offsets_at + u64::from(doc_count) * 2;
        let classes_at = lengths_at + u64::from(doc_count) * 4;
        let pages_at = classes_at + u64::from(doc_count);
        if pages_at + pages_len as u64 != total || !pages_len.is_multiple_of(docs::PAGE_ENTRY) {
            return Err(Error::Corrupt("segment length"));
        }
        Ok(Self {
            source,
            header: Header {
                doc_count,
                total_length,
                dictionary_at,
                dictionary_len,
                ordinals_at,
                ordinals_len,
                payload_at,
                payload_len,
                offsets_at,
                lengths_at,
                classes_at,
                pages_at,
                pages_len,
            },
            id: crate::cache::reader_id(),
            arena: RefCell::new(Arena::default()),
            arena_bytes: Cell::new(0),
            dictionary: OnceCell::new(),
            pages: OnceCell::new(),
            last_chunk: Cell::new(None),
            last_classes: RefCell::new(None),
        })
    }

    /// Bytes held in the arena, for cache budgeting.
    pub fn cached_bytes(&self) -> usize {
        self.arena_bytes.get()
    }

    /// Which area of the blob `offset` lies in, for read accounting.
    fn area_of(&self, offset: u64) -> usize {
        let header = &self.header;
        match offset {
            _ if offset >= header.pages_at => 7,
            _ if offset >= header.classes_at => 6,
            _ if offset >= header.lengths_at => 5,
            _ if offset >= header.offsets_at => 4,
            _ if offset >= header.payload_at => 3,
            _ if offset >= header.ordinals_at => 2,
            _ if offset >= header.dictionary_at => 1,
            _ => 0,
        }
    }

    /// A range read straight from the source, kept by the caller alone: for
    /// a table that shared buffers already cache, where a private copy would
    /// only evict what nothing else holds.
    fn read_uncached(&self, offset: u64, len: usize) -> Result<Rc<[u8]>> {
        if let Some(slice) = self.source.slice(offset, len) {
            return Ok(Rc::from(slice));
        }
        let area = self.area_of(offset);
        crate::cache::note_read(area, len);
        let before = crate::cache::disk_pages();
        let bytes = self.source.read(offset, len)?;
        crate::cache::note_disk(area, crate::cache::disk_pages() - before);
        if bytes.len() != len {
            return Err(Error::Truncated);
        }
        Ok(Rc::from(bytes))
    }

    /// A range a cursor sweeps, through the bounded cache rather than the
    /// arena.
    fn read_owned(&self, offset: u64, len: usize) -> Result<Rc<[u8]>> {
        if let Some(slice) = self.source.slice(offset, len) {
            return Ok(Rc::from(slice));
        }
        if let Some(bytes) = crate::cache::get(self.id, offset, len) {
            return Ok(bytes);
        }
        let area = self.area_of(offset);
        let before = crate::cache::disk_pages();
        let bytes = self.source.read(offset, len)?;
        crate::cache::note_disk(area, crate::cache::disk_pages() - before);
        if bytes.len() != len {
            return Err(Error::Truncated);
        }
        crate::cache::note_read(area, len);
        Ok(crate::cache::insert(self.id, offset, bytes))
    }

    fn load(&self, offset: u64, len: usize) -> Result<&[u8]> {
        if let Some(slice) = self.source.slice(offset, len) {
            return Ok(slice);
        }
        if let Some(bytes) = self.arena.borrow().get(&(offset, len)) {
            let pointer: *const [u8] = &**bytes;
            // SAFETY: as below; the box stays in the arena for `self`'s life.
            return Ok(unsafe { &*pointer });
        }
        let area = self.area_of(offset);
        crate::cache::note_read(area, len);
        let before = crate::cache::disk_pages();
        let bytes = self.source.read(offset, len)?.into_boxed_slice();
        crate::cache::note_disk(area, crate::cache::disk_pages() - before);
        let pointer: *const [u8] = &*bytes;
        self.arena_bytes.set(self.arena_bytes.get() + bytes.len());
        self.arena.borrow_mut().insert((offset, len), bytes);
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

    /// Byte lengths of every section of the blob, for size accounting.
    pub const fn sections(&self) -> Sections {
        Sections {
            header: self.header.dictionary_at as usize,
            dictionary: self.header.dictionary_len,
            ordinals: self.header.ordinals_len,
            payload: self.header.payload_len,
            offsets: self.header.doc_count as usize * 2,
            lengths: self.header.doc_count as usize * 4,
            classes: self.header.doc_count as usize,
            pages: self.header.pages_len,
        }
    }

    /// The page table: the heap blocks the documents span.
    pub fn page_table(&self) -> Result<PageTable<'_>> {
        if let Some(pages) = self.pages.get() {
            return Ok(*pages);
        }
        let pages = PageTable::parse(
            self.load(self.header.pages_at, self.header.pages_len)?,
            self.header.doc_count,
        )?;
        // SAFETY: the bytes live in the arena for the reader's lifetime; the
        // table is only ever handed out shortened to a borrow of `self`.
        let pages: PageTable<'static> = unsafe { std::mem::transmute(pages) };
        Ok(*self.pages.get_or_init(|| pages))
    }

    /// The document table, with offsets fetched a heap block at a time.
    pub fn doc_table(&self) -> Result<DocTable<'_>> {
        DocTable::new(
            self.page_table()?,
            OffsetsFetch { reader: self },
            u64::from(self.header.doc_count) * 2,
        )
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
        if !within(entry.ordinals, self.header.ordinals_len)
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
    pub fn documents(&self) -> Result<DocCursor<'_>> {
        self.doc_table()?.into_cursor()
    }

    /// The location of document `ordinal`.
    pub fn tid_at(&self, ordinal: u32) -> Result<Tid> {
        self.doc_table()?.tid_at(ordinal)
    }

    /// The ordinal of the document at `tid`, if it is in this segment.
    pub fn ordinal_of(&self, tid: Tid) -> Result<Option<u32>> {
        self.doc_table()?.ordinal_of(tid)
    }

    /// Length of the document at `tid`, if it is in this segment.
    pub fn document_length(&self, tid: Tid) -> Result<Option<u32>> {
        self.ordinal_of(tid)?
            .map(|ordinal| self.length_at(ordinal))
            .transpose()
    }

    /// Length by document ordinal.
    pub fn length_at(&self, ordinal: u32) -> Result<u32> {
        self.lengths().get(ordinal)
    }

    /// Length class by document ordinal. A paged source reads the table in
    /// windows of [`CLASS_WINDOW`] documents and keeps the window last read:
    /// the table is a byte per document and a walk consults it per
    /// candidate, in ordinal order.
    pub fn length_class(&self, ordinal: u32) -> Result<u8> {
        if ordinal >= self.header.doc_count {
            return Err(Error::Corrupt("document ordinal out of range"));
        }
        let at = self.header.classes_at + u64::from(ordinal);
        if let Some(bytes) = self.source.slice(at, 1) {
            return Ok(bytes[0]);
        }
        let mut held = self.last_classes.borrow_mut();
        let hit = held.as_ref().is_some_and(|(first, bytes)| {
            ordinal >= *first && ((ordinal - first) as usize) < bytes.len()
        });
        if !hit {
            let first = ordinal - ordinal % CLASS_WINDOW;
            let len = (self.header.doc_count - first).min(CLASS_WINDOW) as usize;
            let window = self.read_uncached(self.header.classes_at + u64::from(first), len)?;
            *held = Some((first, window));
        }
        let (first, bytes) = held.as_ref().expect("window loaded");
        Ok(bytes[(ordinal - first) as usize])
    }

    /// A copyable handle on the length table.
    pub fn lengths(&self) -> Lengths<'_> {
        let len = self.header.doc_count as usize * 4;
        match self.source.slice(self.header.lengths_at, len) {
            Some(bytes) => Lengths::Bytes(bytes),
            // A paged source hands the table out in windows, cached with the
            // reader: a walk looks lengths up per candidate, one fetch per
            // lookup cost more than the scoring, and the whole table of a
            // large segment is megabytes a query rarely needs.
            None => Lengths::Lazy {
                fetch: self,
                count: self.header.doc_count,
                window: std::cell::RefCell::new(None),
            },
        }
    }

    /// Rebuilds every document as a forward record, skipping those for which
    /// `skip` returns true. This is how folds and merges carry documents
    /// between segments without re-reading the heap.
    pub fn records(&self, mut skip: impl FnMut(Tid) -> bool) -> Result<Vec<ForwardRecord>> {
        let docs = self.doc_table()?;
        let documents = docs.to_vec()?;
        let mut by_document: BTreeMap<Tid, Vec<ForwardTerm>> = BTreeMap::new();
        for tid in &documents {
            if !skip(*tid) {
                by_document.insert(*tid, Vec::new());
            }
        }
        for item in self.dictionary()?.iter() {
            let (term, entry) = item?;
            let resolved = self.resolve(entry)?;
            let mut ordinals = resolved.ordinals()?.cursor()?;
            let mut payload = resolved.payload()?.cursor();
            while let Some(ordinal) = ordinals.current() {
                let mut positions = Vec::new();
                payload.next_into(&mut positions)?;
                let tid = *documents
                    .get(ordinal as usize)
                    .ok_or(Error::Corrupt("ordinal beyond the document table"))?;
                if let Some(terms) = by_document.get_mut(&tid) {
                    terms.push(ForwardTerm {
                        term: term.clone(),
                        positions,
                    });
                }
                ordinals.advance()?;
            }
        }
        let mut out = Vec::with_capacity(by_document.len());
        for (tid, terms) in by_document {
            let ordinal = documents
                .binary_search(&tid)
                .map_err(|_| Error::Corrupt("document missing from table"))?;
            out.push(ForwardRecord {
                tid,
                doc_len: self.length_at(ordinal as u32)?,
                terms,
            });
        }
        Ok(out)
    }
}

/// The offsets table of a reader, fetched a heap block at a time.
struct OffsetsFetch<'a, S: Source> {
    reader: &'a Reader<S>,
}

impl<'a, S: Source> crate::ordinals::Fetch<'a> for OffsetsFetch<'a, S> {
    fn fetch(&self, offset: u64, len: usize) -> Result<&'a [u8]> {
        let header = &self.reader.header;
        match offset.checked_add(len as u64) {
            Some(end) if end <= u64::from(header.doc_count) * 2 => {
                self.reader.load(header.offsets_at + offset, len)
            }
            _ => Err(Error::Truncated),
        }
    }
    fn fetch_owned(&self, offset: u64, len: usize) -> Result<Rc<[u8]>> {
        let header = &self.reader.header;
        match offset.checked_add(len as u64) {
            Some(end) if end <= u64::from(header.doc_count) * 2 => {
                self.reader.read_owned(header.offsets_at + offset, len)
            }
            _ => Err(Error::Truncated),
        }
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
    fn ordinals_bytes(&self, offset: u64, len: usize) -> Result<&[u8]> {
        if offset
            .checked_add(len as u64)
            .is_none_or(|end| end > self.header.ordinals_len as u64)
        {
            return Err(Error::Truncated);
        }
        self.load(self.header.ordinals_at + offset, len)
    }

    fn ordinals_range_owned(&self, offset: u64, len: usize) -> Result<Rc<[u8]>> {
        if offset
            .checked_add(len as u64)
            .is_none_or(|end| end > self.header.ordinals_len as u64)
        {
            return Err(Error::Truncated);
        }
        self.read_owned(self.header.ordinals_at + offset, len)
    }

    fn payload_bytes(&self, extent: Extent) -> Result<&[u8]> {
        self.load(self.header.payload_at + extent.offset, extent.len as usize)
    }

    fn ranged_payloads(&self) -> bool {
        true
    }

    fn payload_range(&self, offset: u64, len: usize) -> Result<&[u8]> {
        match offset.checked_add(len as u64) {
            Some(end) if end <= self.header.payload_len as u64 => {
                self.load(self.header.payload_at + offset, len)
            }
            _ => Err(Error::Truncated),
        }
    }

    fn payload_range_owned(&self, offset: u64, len: usize) -> Result<Rc<[u8]>> {
        match offset.checked_add(len as u64) {
            Some(end) if end <= self.header.payload_len as u64 => {
                self.read_owned(self.header.payload_at + offset, len)
            }
            _ => Err(Error::Truncated),
        }
    }

    fn doc_table(&self) -> Result<DocTable<'_>> {
        Reader::doc_table(self)
    }

    fn length_class(&self, ordinal: u32) -> Result<u8> {
        Reader::length_class(self, ordinal)
    }

    fn length_window_owned(&self, ordinal: u32) -> Result<Option<(Rc<[u8]>, u32)>> {
        if ordinal >= self.header.doc_count {
            return Err(Error::Corrupt("document ordinal out of range"));
        }
        let first = ordinal - ordinal % LENGTH_WINDOW;
        let len = ((self.header.doc_count - first).min(LENGTH_WINDOW) as usize) * 4;
        let bytes = self.read_uncached(self.header.lengths_at + u64::from(first) * 4, len)?;
        Ok(Some((bytes, first)))
    }

    fn length(&self, ordinal: u32) -> Result<u32> {
        if ordinal >= self.header.doc_count {
            return Err(Error::Corrupt("document ordinal out of range"));
        }
        let at = self.header.lengths_at + u64::from(ordinal) * 4;
        if let Some(bytes) = self.source.slice(at, 4) {
            return Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
        }
        // Paged sources: fetch the chunk around the entry once, then index it.
        let within = (at - self.header.lengths_at) % LENGTH_CHUNK;
        let chunk_at = at - within;
        let chunk: &[u8] = match self.last_chunk.get() {
            // SAFETY: the pointer came from `load`, whose arena keeps the
            // allocation alive and in place for as long as `self` lives.
            Some((last_at, pointer)) if last_at == chunk_at => unsafe { &*pointer },
            _ => {
                let end = self.header.lengths_at + u64::from(self.header.doc_count) * 4;
                let chunk_len = (end - chunk_at).min(LENGTH_CHUNK) as usize;
                let chunk = self.load(chunk_at, chunk_len)?;
                self.last_chunk
                    .set(Some((chunk_at, std::ptr::from_ref(chunk))));
                chunk
            }
        };
        let i = within as usize;
        Ok(u32::from_le_bytes([
            chunk[i],
            chunk[i + 1],
            chunk[i + 2],
            chunk[i + 3],
        ]))
    }
}

/// Document lengths addressed by document ordinal.
#[derive(Clone)]
pub enum Lengths<'a> {
    Bytes(&'a [u8]),
    Lazy {
        fetch: &'a dyn AreaFetch,
        count: u32,
        /// The window last read, as (first ordinal, bytes).
        window: std::cell::RefCell<Option<(u32, Rc<[u8]>)>>,
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
            Self::Lazy {
                fetch,
                count,
                window,
            } => {
                if ordinal >= *count {
                    return Err(Error::Corrupt("document ordinal out of range"));
                }
                let mut held = window.borrow_mut();
                let hit = held.as_ref().is_some_and(|(first, bytes)| {
                    ordinal >= *first && ((ordinal - first) as usize) * 4 + 4 <= bytes.len()
                });
                if !hit {
                    match fetch.length_window_owned(ordinal)? {
                        Some((bytes, first)) => *held = Some((first, bytes)),
                        None => return fetch.length(ordinal),
                    }
                }
                let (first, bytes) = held.as_ref().expect("window loaded");
                let at = (ordinal - first) as usize * 4;
                let bytes = bytes.get(at..at + 4).ok_or(Error::Truncated)?;
                Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
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
        assert_eq!(segment.tid_at(1).unwrap(), tid(2, 1));
        assert_eq!(segment.ordinal_of(tid(0, 5)).unwrap(), Some(0));

        let beer = segment.term("beer").unwrap().unwrap();
        assert_eq!(beer.df(), 2);
        assert_eq!(beer.entry.max_tf_bucket, TfBucket::from_count(2).value());
        assert_eq!(
            collect(beer.cursor().unwrap()).unwrap(),
            [tid(0, 5), tid(2, 1)]
        );
        // The term's stream carries a bound over its documents.
        let ordinals = beer.ordinals().unwrap();
        assert_eq!(ordinals.to_vec().unwrap(), [0, 1]);
        let bound = ordinals.chunk_bound(0).unwrap();
        assert_eq!(bound.min_len[TfBucket::from_count(1).value() as usize], 2);
        assert_eq!(bound.min_len[TfBucket::from_count(2).value() as usize], 3);
        let payload = beer.payload().unwrap();
        let rank = ordinals.rank(1).unwrap().unwrap();
        assert_eq!(payload.get(rank).unwrap().positions, [1, 2]);
        assert_eq!(payload.get(0).unwrap().positions, [2]);
        let mut cursor = beer.cursor().unwrap();
        cursor.seek(tid(1, 1)).unwrap();
        assert_eq!(cursor.current(), Some(tid(2, 1)));
        assert_eq!(cursor.rank(), 1);
        assert!(!beer.prefers_pages().unwrap());
        let pages: Vec<(u32, Vec<u16>)> = {
            let mut out = Vec::new();
            let mut pages = beer.pages().unwrap();
            while let Some(page) = crate::pages::Cursor::current(&pages) {
                out.push((page.block, page.offsets.iter().collect()));
                crate::pages::Cursor::advance(&mut pages).unwrap();
            }
            out
        };
        assert_eq!(pages, [(0, vec![5]), (2, vec![1])]);

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
    fn grouped_records_match_token_rebuild() {
        let mut grouped = SegmentBuilder::default();
        let mut tokens_builder = SegmentBuilder::default();
        for n in (0..140).rev() {
            let mut record = ForwardRecord::from_tokens(
                tid(n / 50, (n % 50 + 1) as u16),
                (0..(n % 30 + 1)).map(|p| (["alpha", "beer", "wine"][(p % 3) as usize], p * 2)),
            )
            .unwrap();
            // The caller's header is not trusted for scoring lengths.
            record.doc_len = 999;
            grouped.add_record(&record).unwrap();
            tokens_builder
                .add_document(record.tid, record.tokens())
                .unwrap();
        }
        let expected = tokens_builder.finish();
        let actual = grouped.finish();
        // Exact bytes cover positions, document lengths, chunk bounds and
        // dictionary ordering together.
        assert_eq!(actual, expected);
        let report = crate::verify::verify_segment(&actual);
        assert!(
            report
                .findings
                .iter()
                .all(|finding| finding.severity != crate::verify::Severity::Error),
            "{:?}",
            report.findings
        );
    }

    #[test]
    fn public_record_normalization_and_errors_match_token_path() {
        let term = |name: &str, positions: &[u32]| ForwardTerm {
            term: name.to_owned(),
            positions: positions.to_vec(),
        };
        let cases = [
            vec![],
            vec![term("", &[])],
            vec![term("a", &[])],
            vec![term("a", &[0, 2]), term("b", &[1, 3])],
            vec![term("a", &[0, 2]), term("b", &[2, 3])],
            vec![term("a", &[63, 64]), term("b", &[64, 65])],
            // Nine tokens keep max=65 within the bitmap's dense threshold,
            // covering separate words and a cross-term duplicate at bit 0.
            vec![term("a", &[1, 2, 3, 4, 63, 64]), term("b", &[5, 6, 65])],
            vec![term("a", &[1, 2, 3, 4, 63, 64]), term("b", &[5, 6, 64])],
            vec![term("a", &[64, 63])],
            vec![term("a", &[1, 1])],
            vec![term("a", &[u32::MAX]), term("b", &[0])],
            vec![term("b", &[1]), term("a", &[2])],
            vec![term("a", &[1]), term("a", &[2])],
            vec![term("", &[1]), term("a", &[2])],
        ];
        for terms in cases {
            for record_tid in [
                tid(0, 1),
                tid(0, 2),
                Tid {
                    block: 0,
                    offset: 0,
                },
            ] {
                let record = ForwardRecord {
                    tid: record_tid,
                    doc_len: 777,
                    terms: terms.clone(),
                };
                let mut grouped = SegmentBuilder::default();
                let mut baseline = SegmentBuilder::default();
                grouped.add_document(tid(0, 1), [("seed", 1)]).unwrap();
                baseline.add_document(tid(0, 1), [("seed", 1)]).unwrap();
                let expected = baseline.add_document(record.tid, record.tokens());
                assert_eq!(grouped.add_record(&record), expected, "{record:?}");
                assert_eq!(grouped.finish(), baseline.finish(), "{record:?}");
            }
        }
    }

    proptest::proptest! {
        #[test]
        fn grouped_record_ingestion_matches_normalization(
            items in proptest::collection::vec((0usize..8, 0u32..300), 0..200)
        ) {
            // Include duplicate cross-term positions and noncanonical order;
            // the token path independently determines acceptance and output.
            let mut terms = BTreeMap::<String, Vec<u32>>::new();
            for (term, position) in items {
                terms.entry(format!("term{term}")).or_default().push(position);
            }
            for positions in terms.values_mut() {
                positions.sort_unstable();
            }
            let record = ForwardRecord {
                tid: tid(0, 1), doc_len: 0,
                terms: terms.into_iter().map(|(term, positions)| ForwardTerm {term, positions}).collect()
            };
            let mut grouped = SegmentBuilder::default();
            let mut baseline = SegmentBuilder::default();
            proptest::prop_assert_eq!(grouped.add_record(&record), baseline.add_document(record.tid, record.tokens()));
            proptest::prop_assert_eq!(grouped.finish(), baseline.finish());
        }
    }

    #[test]
    #[ignore = "manual release-mode ingestion microprobe; not a database benchmark"]
    fn grouped_record_ingestion_microprobe() {
        use std::hint::black_box;
        use std::time::Instant;
        for repeats in [20, 200] {
            let records: Vec<_> = (0..512)
                .map(|n| {
                    ForwardRecord::from_tokens(
                        tid(n / 100, (n % 100 + 1) as u16),
                        (0..repeats * 2).map(|p| (["common", "filler"][(p % 2) as usize], p + 1)),
                    )
                    .unwrap()
                })
                .collect();
            let mut timings = [Vec::new(), Vec::new()];
            for round in 0..12 {
                for mode in [round % 2, (round + 1) % 2] {
                    let mut builder = SegmentBuilder::default();
                    let start = Instant::now();
                    for record in black_box(&records) {
                        if mode == 0 {
                            builder.add_document(record.tid, record.tokens()).unwrap();
                        } else {
                            builder.add_record(record).unwrap();
                        }
                    }
                    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                    black_box(builder);
                    if round >= 2 {
                        timings[mode].push(elapsed);
                    }
                }
            }
            for times in &mut timings {
                times.sort_by(f64::total_cmp);
            }
            let median = |times: &[f64]| (times[4] + times[5]) / 2.0;
            eprintln!(
                "512 docs, {} tokens/doc: token path {:.3} ms, grouped {:.3} ms; sorted samples per path {:?}",
                repeats * 2,
                median(&timings[0]),
                median(&timings[1]),
                timings
            );
        }
    }

    #[test]
    fn empty_segment_and_corruption() {
        let bytes = SegmentBuilder::default().finish();
        let segment = Segment::parse(&bytes).unwrap();
        assert_eq!(segment.document_count(), 0);
        assert!(segment.term("x").unwrap().is_none());
        assert_eq!(segment.documents().unwrap().current(), None);
        assert!(Segment::parse(&bytes[..bytes.len() - 1]).is_err());
        assert_eq!(&bytes[..4], MAGIC);
        // Earlier and unknown signatures are rejected outright.
        for magic in [b"LSG1", b"LSG5", b"STN2", b"STN4"] {
            let mut other = bytes.clone();
            other[..4].copy_from_slice(magic);
            assert_eq!(
                Segment::parse(&other).err(),
                Some(Error::Corrupt("segment magic"))
            );
        }
        let mut builder = SegmentBuilder::default();
        builder.add_document(tid(1, 1), tokens("a b c")).unwrap();
        let mut bytes = builder.finish();
        let sections = Segment::parse(&bytes).unwrap().sections();
        assert_eq!(sections.pages, docs::PAGE_ENTRY);
        assert_eq!(sections.offsets, 2);
        assert_eq!(sections.classes, 1);
        assert_eq!(
            Segment::parse(&bytes).unwrap().length_class(0).unwrap(),
            crate::length_class::class_of(3)
        );
        let last = bytes.len() - sections.pages - sections.classes - 1;
        bytes[last] ^= 0x80; // Corrupt the length table.
        let segment = Segment::parse(&bytes).unwrap();
        assert_eq!(
            segment.document_length(tid(1, 1)).unwrap(),
            Some(3 | 0x8000_0000)
        );
        bytes.push(0);
        assert!(Segment::parse(&bytes).is_err());
        // A page table is whole entries.
        bytes.pop();
        let pages_len_at = sections.header - 1;
        assert_eq!(bytes[pages_len_at] as usize, docs::PAGE_ENTRY);
        bytes[pages_len_at] -= 1;
        bytes.pop();
        assert_eq!(
            Segment::parse(&bytes).err(),
            Some(Error::Corrupt("segment length"))
        );
    }
}
