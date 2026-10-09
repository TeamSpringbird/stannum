// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A segment in this shape behind the query interface every source offers
//! ([`crate::index::Index`]), read a range at a time from a [`Source`].
//!
//! The planner, the Boolean cursors, the ordinal walk and the ordinal count
//! fold address a segment's documents by ordinal. A document's rank in this
//! shape (its position in ctid order) is exactly that ordinal, so a term
//! read here is handed out as an ordinal stream: its slots become ranks
//! through the document set, its buckets come from the TF tail and its
//! lengths from the DL sidecar, encoded once per reader the way the mutable
//! index encodes its terms. Its positions stream needs no translation: it
//! is the ordinal format's, byte for byte.
//!
//! The extension's count and ranked paths read the postings themselves
//! ([`super::segment::Segment`] through `engine::tinshape`); this reader
//! serves every other query shape, the plain index and bitmap scans and the
//! scorers' statistics.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use rustc_hash::FxHashMap;

use super::docs::{DocSet, Lengths as SidecarLengths};
use super::postings::Postings;
use super::segment::MAGIC;
use crate::dictionary::{BlockFetch, Blocks, Dictionary, DictionaryIndex, Extent, TermEntry};
use crate::docs::{DocCursor, DocTable, PageTable};
use crate::index::{Expanded, Index, Window};
use crate::segment::{AreaFetch, Lengths, Term};
use crate::source::Source;
use crate::{Error, Result, Tid, varint};

/// Where each area of a blob starts, and its end, as
/// [`super::segment::Area`] orders them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub documents: u32,
    pub total_length: u64,
    pub block_size: u32,
    pub adaptive_tf: bool,
    pub bounds: [u64; 8],
}

const FLAG_ADAPTIVE_TF: u64 = 1;

/// The longest header a blob can have: the magic and ten varints.
pub const HEADER_MAX: usize = 4 + 10 * 10;

impl Header {
    /// Parses the header at the start of `head` for a blob of `total`
    /// bytes.
    pub fn parse(head: &[u8], total: u64) -> Result<Self> {
        if head.get(..4) != Some(MAGIC.as_slice()) {
            return Err(Error::Corrupt("segment magic"));
        }
        let mut at = 4;
        let mut next = || varint::get(head, &mut at);
        let documents = u32::try_from(next()?).map_err(|_| Error::Corrupt("document count"))?;
        let total_length = next()?;
        let block_size = u32::try_from(next()?).map_err(|_| Error::Corrupt("block size"))?;
        let flags = next()?;
        let mut lens = [0u64; 6];
        for len in &mut lens {
            *len = next()?;
        }
        if block_size == 0 {
            return Err(Error::Corrupt("block size"));
        }
        let mut bounds = [0u64; 8];
        bounds[1] = at as u64;
        for (i, len) in lens.iter().enumerate() {
            bounds[i + 2] = bounds[i + 1].checked_add(*len).ok_or(Error::Truncated)?;
        }
        if bounds[7] != total {
            return Err(Error::Corrupt("segment length"));
        }
        Ok(Self {
            documents,
            total_length,
            block_size,
            adaptive_tf: flags & FLAG_ADAPTIVE_TF != 0,
            bounds,
        })
    }

    /// Where area `area` (in [`super::segment::Area`] order) starts.
    pub const fn at(&self, area: usize) -> u64 {
        self.bounds[area]
    }

    pub const fn len(&self, area: usize) -> usize {
        (self.bounds[area + 1] - self.bounds[area]) as usize
    }
}

const TERM_MAP: usize = 1;
const POSTINGS: usize = 2;
const POSITIONS: usize = 3;
const DOCSET: usize = 4;
const LENGTHS: usize = 5;

/// The per-document tables the ordinal interface reads, derived once from
/// the document set and the DL sidecar.
struct Tables {
    /// The ordinal format's offsets and page table (see [`crate::docs`]).
    offsets: Vec<u8>,
    pages: Vec<u8>,
    /// Lengths by rank, four bytes each, as [`Lengths::Bytes`] reads them.
    lengths: Vec<u8>,
}

/// Translated term streams. Append-only: a stream is never freed or moved
/// while the reader lives, so borrowed views survive later translations.
#[derive(Default)]
struct Encoded {
    streams: Vec<Box<[u8]>>,
    /// A term's ordinal-format entry by its record's offset in the postings
    /// area.
    terms: FxHashMap<u64, TermEntry>,
    /// A term's positions as an STN3 payload stream, by its extent's offset
    /// in the positions area.
    payloads: FxHashMap<u64, Extent>,
    bytes: usize,
}

impl Encoded {
    fn push(&mut self, bytes: Vec<u8>) -> Extent {
        let len = bytes.len();
        self.bytes += len;
        self.streams.push(bytes.into_boxed_slice());
        Extent {
            offset: ((self.streams.len() - 1) as u64) << 32,
            len: len as u32,
        }
    }

    /// `len` bytes at `at`, a stream index in the high half and an offset
    /// within the stream in the low half. The lifetime is the caller's:
    /// streams are boxed and never removed, so a slice stays valid for as
    /// long as the owning reader lives.
    fn range<'s>(&self, at: u64, len: usize) -> Result<&'s [u8]> {
        let bytes = self
            .streams
            .get((at >> 32) as usize)
            .ok_or(Error::Corrupt("translated stream"))?;
        let within = (at & 0xffff_ffff) as usize;
        let bytes = bytes
            .get(within..within.checked_add(len).ok_or(Error::Truncated)?)
            .ok_or(Error::Truncated)?;
        // SAFETY: see above; the box's heap allocation outlives every borrow.
        Ok(unsafe { &*std::ptr::from_ref::<[u8]>(bytes) })
    }
}

/// Fetched byte ranges by (offset, len).
type Arena = FxHashMap<(u64, usize), Box<[u8]>>;

/// A segment in this shape read through a [`Source`].
pub struct Reader<S: Source> {
    source: S,
    header: Header,
    arena: RefCell<Arena>,
    arena_bytes: Cell<usize>,
    dictionary: OnceCell<DictionaryIndex<'static>>,
    docs: OnceCell<Rc<DocSet>>,
    tables: OnceCell<Tables>,
    encoded: RefCell<Encoded>,
}

impl<S: Source> Reader<S> {
    pub fn new(source: S) -> Result<Self> {
        let total = source.len();
        let head = source.read(0, total.min(HEADER_MAX as u64) as usize)?;
        let header = Header::parse(&head, total)?;
        Ok(Self {
            source,
            header,
            arena: RefCell::default(),
            arena_bytes: Cell::new(0),
            dictionary: OnceCell::new(),
            docs: OnceCell::new(),
            tables: OnceCell::new(),
            encoded: RefCell::default(),
        })
    }

    pub const fn header(&self) -> &Header {
        &self.header
    }

    pub const fn source(&self) -> &S {
        &self.source
    }

    /// Bytes this reader holds: fetched ranges, derived tables and
    /// translated streams, for cache budgeting.
    pub fn cached_bytes(&self) -> usize {
        let tables = self
            .tables
            .get()
            .map_or(0, |t| t.offsets.len() + t.pages.len() + t.lengths.len());
        let docs = self.docs.get().map_or(0, |d| d.heap_bytes());
        self.arena_bytes.get() + tables + docs + self.encoded.borrow().bytes
    }

    /// A range kept for the reader's lifetime.
    fn load(&self, offset: u64, len: usize) -> Result<&[u8]> {
        if let Some(slice) = self.source.slice(offset, len) {
            return Ok(slice);
        }
        if let Some(bytes) = self.arena.borrow().get(&(offset, len)) {
            let pointer: *const [u8] = &**bytes;
            // SAFETY: the box stays in the arena for `self`'s life.
            return Ok(unsafe { &*pointer });
        }
        let bytes = self.source.read(offset, len)?.into_boxed_slice();
        if bytes.len() != len {
            return Err(Error::Truncated);
        }
        let pointer: *const [u8] = &*bytes;
        self.arena_bytes.set(self.arena_bytes.get() + len);
        self.arena.borrow_mut().insert((offset, len), bytes);
        // SAFETY: the box was just moved into the arena, which only ever
        // grows and is dropped with `self`; the heap allocation never moves.
        Ok(unsafe { &*pointer })
    }

    /// A range read for the moment, not kept.
    fn read(&self, offset: u64, len: usize) -> Result<Rc<[u8]>> {
        if let Some(slice) = self.source.slice(offset, len) {
            return Ok(Rc::from(slice));
        }
        let bytes = self.source.read_shared(offset, len)?;
        if bytes.len() != len {
            return Err(Error::Truncated);
        }
        Ok(bytes)
    }

    /// The document set: the ctid grid and every document's slot, decoded
    /// once.
    pub fn docs(&self) -> Result<Rc<DocSet>> {
        if let Some(docs) = self.docs.get() {
            return Ok(docs.clone());
        }
        let bytes = self.read(self.header.at(DOCSET), self.header.len(DOCSET))?;
        let docs = DocSet::decode(&bytes)?;
        if docs.geometry.documents != self.header.documents {
            return Err(Error::Corrupt("document set count"));
        }
        Ok(self.docs.get_or_init(|| Rc::new(docs)).clone())
    }

    fn tables(&self) -> Result<&Tables> {
        if let Some(tables) = self.tables.get() {
            return Ok(tables);
        }
        let docs = self.docs()?;
        let tids = docs.tids();
        let sidecar = self.read(self.header.at(LENGTHS), self.header.len(LENGTHS))?;
        let sidecar = SidecarLengths::parse(&*sidecar, self.header.documents)?;
        let mut lengths = Vec::with_capacity(tids.len() * 4);
        for rank in 0..self.header.documents {
            lengths.extend_from_slice(&sidecar.get(rank)?.to_le_bytes());
        }
        let tables = Tables {
            offsets: crate::docs::offsets(tids.iter().copied()),
            pages: crate::docs::page_table(tids.iter().copied()),
            lengths,
        };
        Ok(self.tables.get_or_init(|| tables))
    }

    fn dictionary_index(&self) -> Result<&DictionaryIndex<'_>> {
        if let Some(index) = self.dictionary.get() {
            return Ok(index);
        }
        let at = self.header.at(TERM_MAP);
        let len = self.header.len(TERM_MAP);
        let probe = self.load(at, len.min(32))?;
        let prefix = DictionaryIndex::prefix_len(probe)?;
        if prefix > len {
            return Err(Error::Corrupt("dictionary index length"));
        }
        let bytes = self.load(at, prefix)?;
        let index = DictionaryIndex::parse(bytes)?;
        // SAFETY: `bytes` lives in the arena for the reader's lifetime; the
        // index is only ever handed out shortened to a borrow of `self`.
        let index: DictionaryIndex<'static> = unsafe { std::mem::transmute(index) };
        Ok(self.dictionary.get_or_init(|| index))
    }

    /// The term map, with blocks fetched on demand.
    pub fn dictionary(&self) -> Result<Dictionary<'_>> {
        let index = self.dictionary_index()?;
        let at = self.header.at(TERM_MAP);
        let len = self.header.len(TERM_MAP);
        let blocks = match self.source.slice(at, len) {
            Some(all) => Blocks::Slice(&all[index.header_len..]),
            None => Blocks::Lazy {
                len: len - index.header_len,
                fetch: self,
            },
        };
        Ok(Dictionary::new(index, blocks))
    }

    /// A term's postings record, read for the moment.
    pub fn record(&self, entry: &TermEntry) -> Result<Rc<[u8]>> {
        let end = entry
            .ordinals
            .offset
            .checked_add(u64::from(entry.ordinals.len))
            .ok_or(Error::Truncated)?;
        if end > self.header.len(POSTINGS) as u64 {
            return Err(Error::Truncated);
        }
        self.read(
            self.header.at(POSTINGS) + entry.ordinals.offset,
            entry.ordinals.len as usize,
        )
    }

    /// The term's documents by rank with their buckets, in rank order.
    pub fn postings(&self, entry: &TermEntry) -> Result<(Vec<u32>, Vec<u8>)> {
        let docs = self.docs()?;
        let record = self.record(entry)?;
        let postings = Postings::parse(&record, entry.df, &docs.geometry)?;
        let footer = postings.footer(
            self.header.block_size,
            entry.max_tf_bucket,
            self.header.adaptive_tf,
        )?;
        let mut ranks = Vec::with_capacity(entry.df as usize);
        let mut failed = false;
        postings.for_each_slot(&docs.geometry, |slot| match docs.rank(slot) {
            Some(rank) => ranks.push(rank),
            None => failed = true,
        })?;
        if failed || ranks.len() != entry.df as usize {
            return Err(Error::Corrupt("posting outside the document set"));
        }
        let buckets = (0..entry.df)
            .map(|i| footer.bucket(postings.tf, i))
            .collect::<Result<Vec<u8>>>()?;
        Ok((ranks, buckets))
    }

    /// The ordinal-format entry of a term-map entry, translating its
    /// postings on first use.
    fn translate(&self, entry: TermEntry) -> Result<TermEntry> {
        if let Some(found) = self.encoded.borrow().terms.get(&entry.ordinals.offset) {
            return Ok(*found);
        }
        let end = entry
            .payload
            .offset
            .checked_add(u64::from(entry.payload.len))
            .ok_or(Error::Truncated)?;
        if end > self.header.len(POSITIONS) as u64 {
            return Err(Error::Truncated);
        }
        let (ranks, buckets) = self.postings(&entry)?;
        let lengths = &self.tables()?.lengths;
        let scores = ranks
            .iter()
            .zip(&buckets)
            .map(|(rank, bucket)| {
                let at = *rank as usize * 4;
                let length = u32::from_le_bytes(lengths[at..at + 4].try_into().expect("four"));
                (*bucket, length)
            })
            .collect::<Vec<_>>();
        let stream = crate::ordinals::encode_scored(&ranks, &scores);
        let mut encoded = self.encoded.borrow_mut();
        let translated = TermEntry {
            ordinals: encoded.push(stream),
            ..entry
        };
        encoded.terms.insert(entry.ordinals.offset, translated);
        Ok(translated)
    }

    /// Resolves a term-map entry obtained from this segment. Its postings
    /// are translated when a cursor first reads them
    /// ([`AreaFetch::ordinals_extent`]).
    pub fn resolve(&self, entry: TermEntry) -> Result<Term<'_>> {
        Ok(Term::new(entry, self))
    }

    pub fn term_entry(&self, term: &str) -> Result<Option<TermEntry>> {
        self.dictionary()?.get(term)
    }

    fn lengths_bytes(&self) -> Result<&[u8]> {
        Ok(&self.tables()?.lengths)
    }
}

impl<S: Source> BlockFetch for Reader<S> {
    fn fetch_block(&self, offset: usize, len: usize) -> Result<&[u8]> {
        let index = self.dictionary_index()?;
        let at = self.header.at(TERM_MAP) + index.header_len as u64 + offset as u64;
        self.load(at, len)
    }
}

impl<S: Source> AreaFetch for Reader<S> {
    fn ordinals_bytes(&self, offset: u64, len: usize) -> Result<&[u8]> {
        self.encoded.borrow().range(offset, len)
    }

    fn ordinals_extent(&self, entry: &TermEntry) -> Result<Extent> {
        Ok(self.translate(*entry)?.ordinals)
    }

    fn payload_bytes(&self, extent: Extent) -> Result<&[u8]> {
        let end = extent
            .offset
            .checked_add(u64::from(extent.len))
            .ok_or(Error::Truncated)?;
        if end > self.header.len(POSITIONS) as u64 {
            return Err(Error::Truncated);
        }
        // The ordinal interface reads STN3's payload streams: a term's
        // positions are translated once, like its postings.
        if let Some(found) = self.encoded.borrow().payloads.get(&extent.offset) {
            return self
                .encoded
                .borrow()
                .range(found.offset, found.len as usize);
        }
        let stream = self.read(
            self.header.at(POSITIONS) + extent.offset,
            extent.len as usize,
        )?;
        let payload = super::positions::Positions::parse(&*stream)?.payload()?;
        let mut encoded = self.encoded.borrow_mut();
        let found = encoded.push(payload);
        encoded.payloads.insert(extent.offset, found);
        encoded.range(found.offset, found.len as usize)
    }

    fn doc_table(&self) -> Result<DocTable<'_>> {
        let tables = self.tables()?;
        DocTable::parse(&tables.pages, &tables.offsets, self.header.documents)
    }

    fn length(&self, ordinal: u32) -> Result<u32> {
        let at = ordinal as usize * 4;
        let bytes = self
            .lengths_bytes()?
            .get(at..at + 4)
            .ok_or(Error::Corrupt("document ordinal out of range"))?;
        Ok(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
    }

    fn length_class(&self, ordinal: u32) -> Result<u8> {
        AreaFetch::length(self, ordinal).map(crate::length_class::class_of)
    }
}

impl<S: Source> Index for Reader<S> {
    fn document_count(&self) -> u32 {
        self.header.documents
    }

    fn total_length(&self) -> u64 {
        self.header.total_length
    }

    fn term(&self, term: &str) -> Result<Option<Term<'_>>> {
        self.term_entry(term)?
            .map(|entry| self.resolve(entry))
            .transpose()
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
        for (scanned, item) in items.enumerate() {
            if (scanned + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                crate::check_interrupts("expand:scan");
            }
            let (term, entry) = item?;
            if !filter(&term) {
                continue;
            }
            if found.len() >= limit {
                return Ok(Expanded::Overflow);
            }
            found.push((term, entry));
        }
        let mut terms = Vec::with_capacity(found.len());
        for (term, entry) in found {
            terms.push((term, self.resolve(entry)?));
        }
        Ok(Expanded::Terms(terms))
    }

    fn documents(&self) -> Result<DocCursor<'_>> {
        AreaFetch::doc_table(self)?.into_cursor()
    }

    fn doc_table(&self) -> Result<DocTable<'_>> {
        AreaFetch::doc_table(self)
    }

    fn page_table(&self) -> Result<PageTable<'_>> {
        PageTable::parse(&self.tables()?.pages, self.header.documents)
    }

    fn lengths(&self) -> Lengths<'_> {
        match self.lengths_bytes() {
            Ok(bytes) => Lengths::Bytes(bytes),
            // An empty table fails every lookup as out of range, which a
            // reader of a corrupt sidecar reports when it asks.
            Err(_) => Lengths::Bytes(&[]),
        }
    }

    fn length_class(&self, ordinal: u32) -> Result<u8> {
        AreaFetch::length_class(self, ordinal)
    }
}

/// Every document's ctid, in rank order.
pub fn tids<S: Source>(reader: &Reader<S>) -> Result<Vec<Tid>> {
    Ok(reader.docs()?.tids())
}
