// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! One immutable segment: dictionary, postings area, payload area and a
//! document table, assembled from documents and read back by term or TID.
//!
//! ```text
//! blob   := magic "LSG4", doc_count varint, total_length varint,
//!           dictionary_len varint, postings_len varint, payload_len varint,
//!           docs_len varint, ordinals_len varint, pages_len varint,
//!           dictionary, postings_area, payload_area, docs, lengths,
//!           ordinals_area, pages
//! docs   := a `postings` stream of every document TID in the segment
//! lengths:= u32le per document, in TID order (addressed by docs ordinal)
//! pages  := (block u32le, first u32le)* per heap block holding a document,
//!           ascending; `first` is the ordinal of the block's first document
//! ```
//!
//! The ordinals area concatenates an [`crate::ordinals`] stream per term: the
//! term's documents as ordinals into `docs`, which Boolean counts fold without
//! touching TIDs. `pages` maps between ordinals and heap blocks for the
//! visibility checks of such a count. Both follow `lengths` and are absent
//! before `LSG4`.
//!
//! The postings and payload areas are concatenations of per-term streams;
//! each dictionary entry's extents locate them. Document frequency is the
//! postings count and `max_tf_bucket` is computed while building, so the
//! dictionary alone answers selectivity and score-bound questions. Each
//! term's postings carry score bounds (see [`crate::postings`]).
//!
//! Every released signature is still read; see [`Format`]. `LSG2` added
//! block bounds to term postings and fixed-width payload skip offsets;
//! `LSG3` stores a single term bound for postings of one block, drops the
//! payload skip slot for entry 0 and gap-encodes dictionary extents. Ranked
//! scans over `LSG1` segments score every candidate instead of pruning.
//!
//! The builder holds the segment in memory. That matches the intended use,
//! folding a bounded write buffer, and an index build that partitions the heap
//! into bounded ranges. A sort-based external builder can share the same byte
//! layout later.

use std::collections::BTreeMap;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;

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

/// A segment format, named by the signature that opens its blob. Every
/// format listed is read; only [`Format::CURRENT`] is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Format {
    /// No score bounds; payload skips as varint deltas every 64 entries.
    Lsg1,
    /// Block bounds on every term; fixed-width payload skips every 32
    /// entries, counted explicitly and including entry 0.
    Lsg2,
    /// A single term bound for postings of one block; no payload skip slot
    /// for entry 0; dictionary entries pack `df` with the bucket and store
    /// extents as gaps from the previous entry's.
    Lsg3,
    /// `Lsg3` streams, plus an ordinal stream per term (see
    /// [`crate::ordinals`]) and a table of the heap pages the documents span.
    Lsg4,
}

impl Format {
    pub const CURRENT: Self = Self::Lsg4;

    pub const fn magic(self) -> &'static [u8; 4] {
        match self {
            Self::Lsg1 => b"LSG1",
            Self::Lsg2 => b"LSG2",
            Self::Lsg3 => b"LSG3",
            Self::Lsg4 => b"LSG4",
        }
    }

    pub fn from_magic(magic: &[u8]) -> Option<Self> {
        [Self::Lsg1, Self::Lsg2, Self::Lsg3, Self::Lsg4]
            .into_iter()
            .find(|format| format.magic() == magic)
    }

    /// True when term postings carry score bounds a ranked scan can prune with.
    pub const fn has_bounds(self) -> bool {
        !matches!(self, Self::Lsg1)
    }

    /// True when terms carry ordinal streams and the segment a page table.
    pub const fn has_ordinals(self) -> bool {
        matches!(self, Self::Lsg4)
    }

    /// The layout of postings, payload and dictionary streams: `Lsg4` writes
    /// them as `Lsg3` did.
    pub const fn streams(self) -> Self {
        match self {
            Self::Lsg4 => Self::Lsg3,
            other => other,
        }
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(std::str::from_utf8(self.magic()).expect("ASCII"))
    }
}

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
        self.finish_as(Format::CURRENT)
    }

    /// Encodes in the layout of an earlier format, for compatibility tests.
    pub(crate) fn finish_as(self, format: Format) -> Vec<u8> {
        self.finish_mixed(format, format, format)
    }

    /// A builder over `documents`, for tests.
    #[cfg(test)]
    pub(crate) fn from_documents(documents: &[(Tid, Vec<(String, u32)>)]) -> Self {
        let mut builder = Self::default();
        for (tid, tokens) in documents {
            builder
                .add_document(*tid, tokens.iter().map(|(t, p)| (t.as_str(), *p)))
                .unwrap();
        }
        builder
    }

    /// Encodes with the signature of one format and the stream layouts of
    /// others, so the verifier's layout checks can be exercised.
    pub(crate) fn finish_mixed(self, magic: Format, postings: Format, payload: Format) -> Vec<u8> {
        let (postings_format, payload_format) = (postings.streams(), payload.streams());
        let mut dictionary = DictionaryBuilder::with_format(magic);
        let mut postings_area = Vec::new();
        let mut payload_area = Vec::new();
        let mut ordinals_area = Vec::new();
        let documents: Vec<Tid> = self.lengths.keys().copied().collect();
        for (term, mut occurrences) in self.terms {
            occurrences.sort_unstable_by_key(|occurrence| occurrence.tid);
            let mut postings = PostingsBuilder::default();
            let mut payload = PayloadBuilder::default();
            let mut max_tf_bucket = 0;
            for occurrence in &occurrences {
                let bucket = TfBucket::from_count(occurrence.positions.len() as u32).value();
                max_tf_bucket = max_tf_bucket.max(bucket);
                postings
                    .push_scored(occurrence.tid, bucket, occurrence.doc_len)
                    .expect("occurrences are unique per document and sorted");
                payload
                    .push(bucket, &occurrence.positions)
                    .expect("positions validated on insertion");
            }
            let postings_bytes = postings.finish_as(postings_format);
            let payload_bytes = payload.finish_as(payload_format);
            let ordinals_bytes = if magic.has_ordinals() {
                let ordinals: Vec<u32> = occurrences
                    .iter()
                    .map(|occurrence| {
                        documents
                            .binary_search(&occurrence.tid)
                            .expect("every occurrence belongs to a recorded document")
                            as u32
                    })
                    .collect();
                crate::ordinals::encode(&ordinals)
            } else {
                Vec::new()
            };
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
                ordinals: Extent {
                    offset: ordinals_area.len() as u64,
                    len: ordinals_bytes.len() as u32,
                },
            };
            postings_area.extend_from_slice(&postings_bytes);
            payload_area.extend_from_slice(&payload_bytes);
            ordinals_area.extend_from_slice(&ordinals_bytes);
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
        let pages = page_table(documents.iter().copied());

        let mut out = Vec::new();
        out.extend_from_slice(magic.magic());
        varint::put(&mut out, self.lengths.len() as u64);
        varint::put(&mut out, total_length);
        varint::put(&mut out, dictionary_bytes.len() as u64);
        varint::put(&mut out, postings_area.len() as u64);
        varint::put(&mut out, payload_area.len() as u64);
        varint::put(&mut out, docs_bytes.len() as u64);
        if magic.has_ordinals() {
            varint::put(&mut out, ordinals_area.len() as u64);
            varint::put(&mut out, pages.len() as u64);
        }
        out.extend_from_slice(&dictionary_bytes);
        out.extend_from_slice(&postings_area);
        out.extend_from_slice(&payload_area);
        out.extend_from_slice(&docs_bytes);
        out.extend_from_slice(&lengths);
        if magic.has_ordinals() {
            out.extend_from_slice(&ordinals_area);
            out.extend_from_slice(&pages);
        }
        out
    }
}

/// Bytes per entry of the page table.
pub const PAGE_ENTRY: usize = 8;

/// The page table over documents in TID order.
pub fn page_table(documents: impl Iterator<Item = Tid>) -> Vec<u8> {
    let mut pages = Vec::new();
    let mut previous = None;
    for (ordinal, tid) in documents.enumerate() {
        if previous != Some(tid.block) {
            pages.extend_from_slice(&tid.block.to_le_bytes());
            pages.extend_from_slice(&(ordinal as u32).to_le_bytes());
            previous = Some(tid.block);
        }
    }
    pages
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

    /// The term's documents as ordinals, where the source stores them.
    pub fn ordinals(&self) -> Result<Option<crate::ordinals::Ordinals<'a>>> {
        if !self.areas.format().has_ordinals() || self.entry.ordinals.len == 0 {
            return Ok(None);
        }
        let fetch = OrdinalsFetch {
            areas: self.areas,
            base: self.entry.ordinals.offset,
        };
        crate::ordinals::Ordinals::open(fetch, u64::from(self.entry.ordinals.len)).map(Some)
    }

    pub fn payload(&self) -> Result<Payload<'a>> {
        let format = self.areas.format();
        if self.areas.ranged_payloads() && format != Format::Lsg1 {
            let extent = self.entry.payload;
            return Payload::open(self.areas, extent.offset, extent.len as usize, format);
        }
        let bytes = self.areas.payload_bytes(self.entry.payload)?;
        Payload::parse_format(bytes, format)
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
}

/// Fetches extents of the postings and payload areas and document lengths.
pub trait AreaFetch {
    fn postings_bytes(&self, extent: Extent) -> Result<&[u8]>;
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
    /// Bytes of the ordinals area; sources without one have no streams.
    fn ordinals_bytes(&self, _offset: u64, _len: usize) -> Result<&[u8]> {
        Err(Error::Corrupt("segment has no ordinal streams"))
    }
    fn length(&self, ordinal: u32) -> Result<u32>;
    /// The format the streams were written in.
    fn format(&self) -> Format {
        Format::CURRENT
    }
}

#[derive(Clone, Copy, Debug)]
struct Header {
    format: Format,
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
    ordinals_at: u64,
    ordinals_len: usize,
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
    arena: RefCell<Arena>,
    arena_bytes: Cell<usize>,
    dictionary: OnceCell<DictionaryIndex<'static>>,
    /// The length chunk read last, by offset: scoring reads lengths in
    /// document order, so consecutive reads hit the same chunk.
    last_chunk: Cell<Option<(u64, *const [u8])>>,
}

/// Byte lengths of a segment's sections, in blob order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sections {
    pub header: usize,
    pub dictionary: usize,
    pub postings: usize,
    pub payload: usize,
    pub docs: usize,
    pub lengths: usize,
    pub ordinals: usize,
    pub pages: usize,
}

/// Fetched extents by (offset, len).
type Arena = HashMap<(u64, usize), Box<[u8]>>;

/// Granularity at which document lengths are fetched from a paged source.
const LENGTH_CHUNK: u64 = 4096;

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
        let magic = reader.take(4)?;
        let format = Format::from_magic(magic).ok_or(Error::Corrupt("segment magic"))?;
        let doc_count = reader.varint_u32()?;
        let total_length = reader.varint()?;
        let dictionary_len = reader.varint_u32()? as usize;
        let postings_len = reader.varint_u32()? as usize;
        let payload_len = reader.varint_u32()? as usize;
        let docs_len = reader.varint_u32()? as usize;
        let (ordinals_len, pages_len) = if format.has_ordinals() {
            (reader.varint_u32()? as usize, reader.varint_u32()? as usize)
        } else {
            (0, 0)
        };
        let dictionary_at = reader.position() as u64;
        let postings_at = dictionary_at + dictionary_len as u64;
        let payload_at = postings_at + postings_len as u64;
        let docs_at = payload_at + payload_len as u64;
        let lengths_at = docs_at + docs_len as u64;
        let ordinals_at = lengths_at + u64::from(doc_count) * 4;
        let pages_at = ordinals_at + ordinals_len as u64;
        if pages_at + pages_len as u64 != total || !pages_len.is_multiple_of(PAGE_ENTRY) {
            return Err(Error::Corrupt("segment length"));
        }
        Ok(Self {
            source,
            header: Header {
                format,
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
                ordinals_at,
                ordinals_len,
                pages_at,
                pages_len,
            },
            arena: RefCell::new(HashMap::new()),
            arena_bytes: Cell::new(0),
            dictionary: OnceCell::new(),
            last_chunk: Cell::new(None),
        })
    }

    /// Bytes held in the arena, for cache budgeting.
    pub fn cached_bytes(&self) -> usize {
        self.arena_bytes.get()
    }

    /// Bytes `[offset, offset + len)` of the source, borrowed for as long as
    /// this reader lives.
    fn load(&self, offset: u64, len: usize) -> Result<&[u8]> {
        if let Some(slice) = self.source.slice(offset, len) {
            return Ok(slice);
        }
        if let Some(bytes) = self.arena.borrow().get(&(offset, len)) {
            let pointer: *const [u8] = &**bytes;
            // SAFETY: as below; the box stays in the arena for `self`'s life.
            return Ok(unsafe { &*pointer });
        }
        let bytes = self.source.read(offset, len)?.into_boxed_slice();
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

    /// The blob's format, from its signature.
    pub const fn format(&self) -> Format {
        self.header.format
    }

    /// True for an `LSG1` blob, whose term postings carry no block bounds.
    pub const fn is_legacy(&self) -> bool {
        matches!(self.header.format, Format::Lsg1)
    }

    /// Byte lengths of the postings and payload areas.
    pub const fn area_lengths(&self) -> (usize, usize) {
        (self.header.postings_len, self.header.payload_len)
    }

    /// Byte lengths of every section of the blob, for size accounting.
    pub const fn sections(&self) -> Sections {
        Sections {
            header: self.header.dictionary_at as usize,
            dictionary: self.header.dictionary_len,
            postings: self.header.postings_len,
            payload: self.header.payload_len,
            docs: self.header.docs_len,
            lengths: self.header.doc_count as usize * 4,
            ordinals: self.header.ordinals_len,
            pages: self.header.pages_len,
        }
    }

    /// The page table, empty before `LSG4`: see the module documentation.
    pub fn page_table(&self) -> Result<&[u8]> {
        self.load(self.header.pages_at, self.header.pages_len)
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
        Ok(Dictionary::with_format(index, blocks, self.header.format))
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
            || !within(entry.ordinals, self.header.ordinals_len)
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
            // A paged source hands out the table once, cached with the reader:
            // a ranked walk looks a length up per candidate, and one fetch per
            // lookup cost more than the scoring.
            None => match self.load(self.header.lengths_at, len) {
                Ok(bytes) => Lengths::Bytes(bytes),
                Err(_) => Lengths::Lazy {
                    fetch: self,
                    count: self.header.doc_count,
                },
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

    fn ordinals_bytes(&self, offset: u64, len: usize) -> Result<&[u8]> {
        if offset
            .checked_add(len as u64)
            .is_none_or(|end| end > self.header.ordinals_len as u64)
        {
            return Err(Error::Truncated);
        }
        self.load(self.header.ordinals_at + offset, len)
    }

    fn format(&self) -> Format {
        self.header.format
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
        // Term postings carry block bounds over the term's documents.
        let mut cursor = beer.cursor().unwrap();
        assert!(cursor.has_bounds());
        assert_eq!(
            cursor.block_bounds().unwrap(),
            [crate::postings::BlockBound::over(
                &[
                    (TfBucket::from_count(1).value(), 2),
                    (TfBucket::from_count(2).value(), 3)
                ],
                tid(2, 1),
            )]
        );
        assert!(!segment.documents().unwrap().has_bounds());
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
    fn grouped_records_match_token_rebuild_in_every_format() {
        for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3, Format::Lsg4] {
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
            let expected = tokens_builder.finish_as(format);
            let actual = grouped.finish_as(format);
            // Exact bytes cover positions, document lengths, per-term and
            // block score bounds, and dictionary/postings ordering together.
            assert_eq!(actual, expected, "{format}");
            let report = crate::verify::verify_segment(&actual);
            assert!(
                report
                    .findings
                    .iter()
                    .all(|finding| finding.severity != crate::verify::Severity::Error),
                "{format}: {:?}",
                report.findings
            );
        }
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
        assert!(Segment::parse(b"LSG4").is_err());
        assert!(Segment::parse(b"LSG3").is_err());
        assert!(Segment::parse(b"LSG1").is_err());
        assert_eq!(&bytes[..4], Format::CURRENT.magic());
        // An unknown signature is rejected outright.
        let mut future = bytes.clone();
        future[..4].copy_from_slice(b"LSG5");
        assert_eq!(
            Segment::parse(&future).err(),
            Some(Error::Corrupt("segment magic"))
        );
        // Earlier signatures are still readable. Their headers lack the two
        // lengths `LSG4` added, so an `LSG4` blob under an older signature is
        // not a segment.
        for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] {
            let old = SegmentBuilder::default().finish_as(format);
            assert_eq!(old.len() + 2, bytes.len());
            let segment = Segment::parse(&old).unwrap();
            assert_eq!(segment.document_count(), 0);
            assert_eq!(segment.format(), format);
            assert_eq!(segment.is_legacy(), format == Format::Lsg1);
            assert_eq!(segment.page_table().unwrap(), b"");
            let mut relabelled = bytes.clone();
            relabelled[..4].copy_from_slice(format.magic());
            assert_eq!(
                Segment::parse(&relabelled).err(),
                Some(Error::Corrupt("segment length"))
            );
        }
        for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3, Format::Lsg4] {
            let mut builder = SegmentBuilder::default();
            builder.add_document(tid(1, 1), tokens("a b c")).unwrap();
            let mut bytes = builder.finish_as(format);
            let sections = Segment::parse(&bytes).unwrap().sections();
            // The length table ends the blob until `LSG4` appends its sections.
            assert_eq!(
                (sections.ordinals != 0, sections.pages),
                (
                    format.has_ordinals(),
                    usize::from(format.has_ordinals()) * PAGE_ENTRY
                ),
                "{format}"
            );
            let last = bytes.len() - sections.ordinals - sections.pages - 1;
            bytes[last] ^= 0x80; // Corrupt the length table.
            let segment = Segment::parse(&bytes).unwrap();
            assert_eq!(
                segment.document_length(tid(1, 1)).unwrap(),
                Some(3 | 0x8000_0000)
            );
            bytes.push(0);
            assert!(Segment::parse(&bytes).is_err());
            // A page table is whole entries.
            if format.has_ordinals() {
                bytes.pop();
                let pages_len_at = sections.header - 1;
                assert_eq!(bytes[pages_len_at] as usize, PAGE_ENTRY);
                bytes[pages_len_at] -= 1;
                bytes.pop();
                assert_eq!(
                    Segment::parse(&bytes).err(),
                    Some(Error::Corrupt("segment length"))
                );
            }
        }
    }
}
