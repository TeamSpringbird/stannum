// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! One immutable segment in TIN's shape: postings addressed by ctid.
//!
//! ```text
//! blob := "TNS1", documents varint, total_length varint,
//!         block_size varint, flags varint (bit 0: adaptive TF widths),
//!         dictionary_len, postings_len, positions_len, docset_len,
//!         lengths_len, liveness_len (varints),
//!         dictionary   the term map: [`crate::dictionary`], each entry's
//!                      `ordinals` extent locating the term's postings
//!                      record and its `payload` extent its positions
//!         postings     one [`super::postings`] record per term
//!         positions    one [`super::positions`] stream per term, entries
//!                      in posting order
//!         docset       the ctid grid and the document set
//!                      ([`super::docs`])
//!         lengths      the DL sidecar, by document rank
//!         liveness     the liveness bitmap, by document rank
//! ```
//!
//! Nothing maps a posting to a document but its slot: counts and Boolean
//! filters read only postings, a ranked query adds the TF tail and the DL
//! sidecar, and a phrase adds positions.

use super::docs::{self, DocSet, Geometry, Lengths, Liveness};
use super::postings::{self, Footer, Options, Postings, Stats};
use crate::dictionary::{
    Blocks, Dictionary, DictionaryBuilder, DictionaryIndex, Extent, TermEntry,
};
use crate::{Error, Result, Tid, varint};

/// The signature that opens a segment blob in this shape.
pub const MAGIC: &[u8; 4] = b"TNS1";

const FLAG_ADAPTIVE_TF: u64 = 1;

/// The areas of a blob, in order, for accounting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Area {
    Metadata,
    TermMap,
    Postings,
    Positions,
    DocSet,
    Lengths,
    Liveness,
}

/// Sizes by area and by kind of postings bytes, for a size report.
#[derive(Clone, Debug, Default)]
pub struct BuildStats {
    pub terms: usize,
    pub postings: u64,
    pub header: u64,
    pub dictionary: u64,
    pub record_headers: u64,
    /// Rare terms' inline lengths.
    pub inline_lengths: u64,
    pub footer: u64,
    pub payload: u64,
    pub tf: u64,
    pub positions: u64,
    pub docset: u64,
    pub lengths: u64,
    pub liveness: u64,
    pub blocks: u64,
    pub sparse_terms: usize,
    pub single_terms: usize,
    pub kinds: [u64; 3],
    pub kind_bytes: [u64; 3],
}

/// Builds a segment from its documents and, term by term in byte order,
/// their postings.
pub struct Builder {
    tids: Vec<Tid>,
    lengths: Vec<u32>,
    geometry: Geometry,
    options: Options,
    dictionary: DictionaryBuilder,
    postings: Vec<u8>,
    positions: Vec<u8>,
    pub stats: BuildStats,
    slots: Vec<u32>,
    term_lengths: Vec<u32>,
}

impl Builder {
    /// A builder for documents `tids` (strictly increasing) of `lengths`.
    pub fn new(tids: Vec<Tid>, lengths: Vec<u32>, options: Options) -> Result<Self> {
        if tids.len() != lengths.len() || lengths.contains(&0) {
            return Err(Error::Corrupt("document lengths"));
        }
        let geometry = Geometry::of(&tids)?;
        Ok(Self {
            tids,
            lengths,
            geometry,
            options,
            dictionary: DictionaryBuilder::default(),
            postings: Vec::new(),
            positions: Vec::new(),
            stats: BuildStats::default(),
            slots: Vec::new(),
            term_lengths: Vec::new(),
        })
    }

    pub fn geometry(&self) -> &Geometry {
        &self.geometry
    }

    /// Adds a term: its documents' ranks (ascending), each one's bucket, and
    /// its positions stream (a [`crate::payload`] stream in that order),
    /// stored as [`super::positions`] streams are.
    pub fn add_term(
        &mut self,
        term: &str,
        ranks: &[u32],
        buckets: &[u8],
        positions: &[u8],
    ) -> Result<Stats> {
        self.add_term_reusing(term, ranks, buckets, positions, &mut |_| None)
    }

    /// [`Self::add_term`] for a merge: `reuse` gives, for a group of
    /// [`Self::geometry`] (by index) an input holds unchanged, that input's
    /// container of the term, copied where [`postings::Reused`]'s conditions
    /// hold. The blob is byte for byte the one [`Self::add_term`] builds.
    pub fn add_term_reusing<'r>(
        &mut self,
        term: &str,
        ranks: &[u32],
        buckets: &[u8],
        positions: &[u8],
        reuse: &mut dyn FnMut(usize) -> Option<postings::Reused<'r>>,
    ) -> Result<Stats> {
        if ranks.is_empty() || ranks.len() != buckets.len() {
            return Err(Error::Corrupt("term postings"));
        }
        self.slots.clear();
        self.term_lengths.clear();
        for rank in ranks {
            let tid = *self
                .tids
                .get(*rank as usize)
                .ok_or(Error::Corrupt("posting rank"))?;
            self.slots.push(
                self.geometry
                    .slot_of(tid)
                    .expect("a document of the segment"),
            );
            self.term_lengths.push(self.lengths[*rank as usize]);
        }
        if self.slots.windows(2).any(|w| w[0] >= w[1]) {
            return Err(Error::Unordered);
        }
        let positions = super::positions::encode(positions, self.tids.len() as u32)?;
        let positions = positions.as_slice();
        let at = self.postings.len();
        let stats = postings::encode_reusing(
            &self.geometry,
            &self.slots,
            buckets,
            &self.term_lengths,
            &self.options,
            &mut self.postings,
            reuse,
        );
        let entry = TermEntry {
            df: ranks.len() as u32,
            max_tf_bucket: *buckets.iter().max().expect("a posting"),
            ordinals: Extent {
                offset: at as u64,
                len: (self.postings.len() - at) as u32,
            },
            payload: Extent {
                offset: self.positions.len() as u64,
                len: positions.len() as u32,
            },
        };
        self.positions.extend_from_slice(positions);
        self.dictionary.push(term, entry)?;
        let s = &mut self.stats;
        s.terms += 1;
        s.postings += ranks.len() as u64;
        s.record_headers += stats.header as u64;
        s.footer += stats.footer as u64;
        s.payload += stats.payload as u64;
        s.tf += stats.tf as u64;
        s.inline_lengths += stats.lengths as u64;
        s.blocks += stats.blocks as u64;
        s.positions += positions.len() as u64;
        if ranks.len() == 1 {
            s.single_terms += 1;
        } else if stats.sparse {
            s.sparse_terms += 1;
        }
        for k in 0..3 {
            s.kinds[k] += stats.kinds[k] as u64;
            s.kind_bytes[k] += stats.kind_bytes[k] as u64;
        }
        Ok(stats)
    }

    /// The blob, with `dead` (document ranks, ascending) cleared in the
    /// liveness bitmap, and where its bytes went.
    pub fn finish(mut self, dead: &[u32]) -> (Vec<u8>, BuildStats) {
        let dictionary = std::mem::take(&mut self.dictionary).finish();
        let docset = docs::encode_docset(&self.geometry, &self.tids);
        let lengths = docs::encode_lengths(&self.lengths);
        let liveness = docs::encode_liveness(self.tids.len() as u32, dead);
        let total_length: u64 = self.lengths.iter().map(|l| u64::from(*l)).sum();
        let mut out = Vec::with_capacity(
            64 + dictionary.len()
                + self.postings.len()
                + self.positions.len()
                + docset.len()
                + lengths.len()
                + liveness.len(),
        );
        out.extend_from_slice(MAGIC);
        let flags = if self.options.adaptive_tf {
            FLAG_ADAPTIVE_TF
        } else {
            0
        };
        for n in [
            self.tids.len() as u64,
            total_length,
            u64::from(self.options.block_size),
            flags,
            dictionary.len() as u64,
            self.postings.len() as u64,
            self.positions.len() as u64,
            docset.len() as u64,
            lengths.len() as u64,
            liveness.len() as u64,
        ] {
            varint::put(&mut out, n);
        }
        let s = &mut self.stats;
        s.header = out.len() as u64;
        s.dictionary = dictionary.len() as u64;
        s.docset = docset.len() as u64;
        s.lengths = lengths.len() as u64;
        s.liveness = liveness.len() as u64;
        for section in [
            &dictionary,
            &self.postings,
            &self.positions,
            &docset,
            &lengths,
            &liveness,
        ] {
            out.extend_from_slice(section);
        }
        (out, self.stats)
    }
}

/// A segment in this shape, read from memory.
pub struct Segment<'a> {
    pub bytes: &'a [u8],
    pub documents: u32,
    pub total_length: u64,
    pub block_size: u32,
    pub adaptive_tf: bool,
    /// Where each area starts, in [`Area`] order, and the end.
    pub bounds: [usize; 8],
    index: DictionaryIndex<'a>,
    /// Shared, so a reader that decodes them once assembles a segment per
    /// query without copying them (see [`Self::assemble`]).
    pub docs: std::rc::Rc<DocSet>,
    pub lengths: Lengths<'a>,
    pub liveness: std::rc::Rc<Liveness>,
    /// Dictionary lookups answered so far, as a reader's memo keeps them.
    memo: std::cell::RefCell<rustc_hash::FxHashMap<String, Option<TermEntry>>>,
    /// Records parsed and footers decoded so far, by the record's offset,
    /// as a reader's caches keep them (STN3's walk keeps its terms' decoded
    /// bounds the same way); emptied past [`PARSED_BUDGET`] bytes.
    parsed: std::cell::RefCell<Parsed<'a>>,
    /// Footers decoded so far, by the record's offset: owned data, so a
    /// reader can keep them across the segments it assembles
    /// ([`Self::share_footers`]).
    footers: std::rc::Rc<std::cell::RefCell<FooterCache>>,
}

/// Decoded footers by their record's blob offset, emptied past
/// [`PARSED_BUDGET`] bytes; see [`Segment::share_footers`].
#[derive(Default)]
pub struct FooterCache {
    bytes: usize,
    footers: rustc_hash::FxHashMap<usize, std::rc::Rc<Footer>>,
}

impl FooterCache {
    /// Bytes the decoded footers hold, roughly.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Lookups a memo holds before it is emptied.
const MEMO_LIMIT: usize = 4096;

/// Bytes of parsed records and footers a segment keeps.
const PARSED_BUDGET: usize = 16 << 20;

#[derive(Default)]
struct Parsed<'a> {
    bytes: usize,
    records: rustc_hash::FxHashMap<usize, Postings<'a>>,
}

impl Parsed<'_> {
    /// Makes room for `bytes` more.
    fn room(&mut self, bytes: usize) {
        if self.bytes + bytes > PARSED_BUDGET {
            self.records.clear();
            self.bytes = 0;
        }
        self.bytes += bytes;
    }
}

/// A term found in a segment.
#[derive(Clone, Debug)]
pub struct Term<'a> {
    pub entry: TermEntry,
    /// Where its record starts in the blob.
    pub at: usize,
    pub postings: Postings<'a>,
}

impl<'a> Segment<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        if bytes.get(..4) != Some(MAGIC.as_slice()) {
            return Err(Error::Corrupt("segment magic"));
        }
        let mut at = 4;
        let mut next = || varint::get(bytes, &mut at);
        let documents = u32::try_from(next()?).map_err(|_| Error::Corrupt("document count"))?;
        let total_length = next()?;
        let block_size = u32::try_from(next()?).map_err(|_| Error::Corrupt("block size"))?;
        let flags = next()?;
        let mut lens = [0usize; 6];
        for len in &mut lens {
            *len = usize::try_from(next()?).map_err(|_| Error::Corrupt("area length"))?;
        }
        if block_size == 0 {
            return Err(Error::Corrupt("block size"));
        }
        let mut bounds = [0usize; 8];
        bounds[0] = 0;
        bounds[1] = at;
        for (i, len) in lens.iter().enumerate() {
            bounds[i + 2] = bounds[i + 1].checked_add(*len).ok_or(Error::Truncated)?;
        }
        if bounds[7] != bytes.len() {
            return Err(Error::Corrupt("segment length"));
        }
        let area = |a: usize| &bytes[bounds[a]..bounds[a + 1]];
        let dictionary = area(1);
        let prefix = DictionaryIndex::prefix_len(dictionary)?;
        let index = DictionaryIndex::parse(dictionary.get(..prefix).ok_or(Error::Truncated)?)?;
        let docs = std::rc::Rc::new(DocSet::decode(area(4))?);
        if docs.geometry.documents != documents {
            return Err(Error::Corrupt("document set count"));
        }
        let lengths = Lengths::parse(area(5), documents)?;
        let liveness = std::rc::Rc::new(Liveness::decode(area(6), &docs)?);
        Ok(Self {
            bytes,
            documents,
            total_length,
            block_size,
            adaptive_tf: flags & FLAG_ADAPTIVE_TF != 0,
            bounds,
            index,
            docs,
            lengths,
            liveness,
            memo: Default::default(),
            parsed: Default::default(),
            footers: Default::default(),
        })
    }

    /// A segment over `bytes` from parts a reader decoded once and keeps
    /// across queries (the term map's index, the document set and the
    /// liveness, see [`Self::parse`]), so that only the header and the DL
    /// sidecar's are read per query: the extension's ctid-native paths
    /// assemble one per query over a blob loaded as far as its queries need.
    pub fn assemble(
        bytes: &'a [u8],
        index: DictionaryIndex<'a>,
        docs: std::rc::Rc<DocSet>,
        liveness: std::rc::Rc<Liveness>,
    ) -> Result<Self> {
        let header = super::index::Header::parse(bytes, bytes.len() as u64)?;
        let mut bounds = [0usize; 8];
        for (bound, at) in bounds.iter_mut().zip(header.bounds) {
            *bound = usize::try_from(at).map_err(|_| Error::Truncated)?;
        }
        if docs.geometry.documents != header.documents {
            return Err(Error::Corrupt("document set count"));
        }
        let lengths = Lengths::parse(&bytes[bounds[5]..bounds[6]], header.documents)?;
        Ok(Self {
            bytes,
            documents: header.documents,
            total_length: header.total_length,
            block_size: header.block_size,
            adaptive_tf: header.adaptive_tf,
            bounds,
            index,
            docs,
            lengths,
            liveness,
            memo: Default::default(),
            parsed: Default::default(),
            footers: Default::default(),
        })
    }

    pub fn area(&self, area: Area) -> &'a [u8] {
        let a = area as usize;
        &self.bytes[self.bounds[a]..self.bounds[a + 1]]
    }

    /// Where `area` starts in the blob.
    pub fn area_at(&self, area: Area) -> usize {
        self.bounds[area as usize]
    }

    pub fn dictionary(&self) -> Dictionary<'_> {
        let bytes = self.area(Area::TermMap);
        Dictionary::new(&self.index, Blocks::Slice(&bytes[self.index.header_len..]))
    }

    /// Resolves a dictionary entry.
    pub fn resolve(&self, entry: TermEntry) -> Result<Term<'a>> {
        let area = self.area(Area::Postings);
        let from = usize::try_from(entry.ordinals.offset).map_err(|_| Error::Truncated)?;
        let record = area
            .get(from..from + entry.ordinals.len as usize)
            .ok_or(Error::Truncated)?;
        Ok(Term {
            entry,
            at: self.area_at(Area::Postings) + from,
            postings: Postings::parse(record, entry.df, &self.docs.geometry)?,
        })
    }

    /// Records `term`'s term-map entry (or its absence) in the memo, found
    /// elsewhere (the extension's paged reader), so lookups through
    /// [`Self::term_memo`] need not read the term map's blocks.
    pub fn remember(&self, term: &str, entry: Option<TermEntry>) {
        let mut memo = self.memo.borrow_mut();
        if memo.len() >= MEMO_LIMIT {
            memo.clear();
        }
        memo.insert(term.to_owned(), entry);
    }

    /// [`Self::resolve`] through the memo of records parsed earlier.
    pub fn resolve_memo(&self, entry: TermEntry) -> Result<Term<'a>> {
        let from = usize::try_from(entry.ordinals.offset).map_err(|_| Error::Truncated)?;
        let at = self.area_at(Area::Postings) + from;
        if let Some(postings) = self.parsed.borrow().records.get(&at) {
            return Ok(Term {
                entry,
                at,
                postings: postings.clone(),
            });
        }
        let term = self.resolve(entry)?;
        let bytes = match &term.postings.form {
            postings::Form::Grouped(entries) => {
                entries.len() * std::mem::size_of::<postings::GroupEntry>()
            }
            _ => 0,
        } + 64;
        let mut parsed = self.parsed.borrow_mut();
        parsed.room(bytes);
        parsed.records.insert(at, term.postings.clone());
        Ok(term)
    }

    /// The footer of the record at blob offset `at` (`postings`, of a term
    /// whose largest bucket is `max_bucket`) decoded, through the memo of
    /// footers decoded earlier.
    pub fn footer_memo(
        &self,
        at: usize,
        postings: &Postings<'a>,
        max_bucket: u8,
    ) -> Result<std::rc::Rc<Footer>> {
        if let Some(footer) = self.footers.borrow().footers.get(&at) {
            return Ok(footer.clone());
        }
        let footer =
            std::rc::Rc::new(postings.footer(self.block_size, max_bucket, self.adaptive_tf)?);
        let bytes = footer.blocks() * 24 + footer.frontier.len() * 8 + 64;
        let mut cache = self.footers.borrow_mut();
        if cache.bytes + bytes > PARSED_BUDGET {
            cache.footers.clear();
            cache.bytes = 0;
        }
        cache.bytes += bytes;
        cache.footers.insert(at, footer.clone());
        Ok(footer)
    }

    /// Has this segment keep the footers it decodes in `cache`, and find
    /// those decoded by earlier segments over the same blob there: a
    /// reader that assembles a segment per query keeps one per blob.
    pub fn share_footers(&mut self, cache: std::rc::Rc<std::cell::RefCell<FooterCache>>) {
        self.footers = cache;
    }

    /// [`Self::term`] through the memo of earlier lookups.
    pub fn term_memo(&self, term: &str) -> Result<Option<Term<'a>>> {
        if let Some(entry) = self.memo.borrow().get(term) {
            return entry.map(|entry| self.resolve_memo(entry)).transpose();
        }
        let entry = self.dictionary().get(term)?;
        let mut memo = self.memo.borrow_mut();
        if memo.len() >= MEMO_LIMIT {
            memo.clear();
        }
        memo.insert(term.to_owned(), entry);
        drop(memo);
        entry.map(|entry| self.resolve_memo(entry)).transpose()
    }

    pub fn term(&self, term: &str) -> Result<Option<Term<'a>>> {
        self.dictionary()
            .get(term)?
            .map(|entry| self.resolve(entry))
            .transpose()
    }

    /// A term's positions stream (a [`super::positions`] stream), and where
    /// it starts in the blob.
    pub fn positions(&self, entry: &TermEntry) -> Result<(&'a [u8], usize)> {
        let area = self.area(Area::Positions);
        let from = usize::try_from(entry.payload.offset).map_err(|_| Error::Truncated)?;
        let bytes = area
            .get(from..from + entry.payload.len as usize)
            .ok_or(Error::Truncated)?;
        Ok((bytes, self.area_at(Area::Positions) + from))
    }

    /// Where the length of the document of `rank` sits in the blob: its
    /// block's header and its packed bits.
    pub fn length_at(&self, rank: u32) -> (usize, usize) {
        let (header, bits) = self.lengths.at(rank);
        let area = self.area_at(Area::Lengths);
        (area + header, area + bits)
    }
}

#[cfg(test)]
mod tests {
    use super::super::docs::Geometry;
    use super::*;
    use crate::payload::PayloadBuilder;
    use crate::tf_bucket::TfBucket;

    /// Two inputs, one holding groups 0 and 1, the other groups 1 to 3 (so
    /// group 1 is shared), merged afresh and by copying the containers of
    /// groups one input holds alone: the blobs are the same bytes.
    #[test]
    fn merges_by_copying_containers() {
        let tids = |blocks: std::ops::Range<u32>, step: u32| -> Vec<Tid> {
            blocks
                .step_by(step as usize)
                .flat_map(|b| {
                    (1..=(b % 23 + 3) as u16).map(move |o| Tid {
                        block: b,
                        offset: o,
                    })
                })
                .collect()
        };
        let a = tids(0..300, 1);
        let b: Vec<Tid> = tids(301..400, 7)
            .into_iter()
            .chain(tids(512..1000, 1))
            .collect();
        let mut merged: Vec<Tid> = a.iter().chain(&b).copied().collect();
        merged.sort_unstable();
        fn length(t: &Tid) -> u32 {
            (t.block * 7 + u32::from(t.offset)) % 50 + 1
        }
        // Term `t` holds a document with a density that falls with `t`, and
        // twice as often past block 300, so some terms cross the floor.
        fn holds(t: u32, tid: &Tid) -> bool {
            let h = (tid.block.wrapping_mul(2_654_435_761) ^ (u32::from(tid.offset) * 40_503))
                .wrapping_add(t * 97);
            let rate = if tid.block > 300 { 2 } else { 1 };
            h % (2 + t * t) < rate
        }
        const TERMS: u32 = 12;
        fn build<'r>(
            docs: &[Tid],
            reuse: &mut dyn FnMut(u32, usize) -> Option<postings::Reused<'r>>,
        ) -> Vec<u8> {
            let options = Options {
                grid_min_postings: 400,
                block_size: 64,
                ..Options::default()
            };
            let lengths = docs.iter().map(length).collect();
            let mut builder = Builder::new(docs.to_vec(), lengths, options).unwrap();
            for t in 0..TERMS {
                let ranks: Vec<u32> = (0..docs.len() as u32)
                    .filter(|r| holds(t, &docs[*r as usize]))
                    .collect();
                if ranks.is_empty() {
                    continue;
                }
                let buckets: Vec<u8> = ranks.iter().map(|r| (r % 3) as u8).collect();
                let mut payload = PayloadBuilder::default();
                for r in &ranks {
                    payload.push(&[r % 5]).unwrap();
                }
                let name = format!("t{t:02}");
                builder
                    .add_term_reusing(&name, &ranks, &buckets, &payload.finish(), &mut |g| {
                        reuse(t, g)
                    })
                    .unwrap();
            }
            builder.finish(&[]).0
        }
        let blob_a = build(&a, &mut |_, _| None);
        let blob_b = build(&b, &mut |_, _| None);
        let fresh = build(&merged, &mut |_, _| None);
        let inputs = [
            (Segment::parse(&blob_a).unwrap(), &a),
            (Segment::parse(&blob_b).unwrap(), &b),
        ];
        let geometry = Geometry::of(&merged).unwrap();
        let mut copied = 0;
        let reusing = build(&merged, &mut |t, g| {
            let group = geometry.groups[g];
            let docs: Vec<Tid> = merged
                .iter()
                .filter(|tid| tid.block / 256 == group.id)
                .copied()
                .collect();
            for (segment, input) in &inputs {
                let geometry_in = &segment.docs.geometry;
                let Some(i) = geometry_in.group_index(group.id * 256) else {
                    continue;
                };
                let held = geometry_in.groups[i];
                let input_docs: Vec<Tid> = input
                    .iter()
                    .filter(|tid| tid.block / 256 == group.id)
                    .copied()
                    .collect();
                if (held.width, held.first, held.pages) != (group.width, group.first, group.pages)
                    || input_docs != docs
                {
                    return None;
                }
                let term = segment.term(&format!("t{t:02}")).unwrap()?;
                let postings::Form::Grouped(entries) = &term.postings.form else {
                    return None;
                };
                let entry = entries.iter().find(|e| e.index as usize == i)?;
                copied += 1;
                return Some(postings::Reused {
                    kind: entry.kind,
                    bytes: term.postings.container(entry),
                    input_df: term.entry.df,
                });
            }
            None
        });
        assert!(copied > 10, "only {copied} containers copied");
        assert!(
            fresh == reusing,
            "a copying merge differs from a fresh build"
        );
    }

    #[test]
    fn builds_and_reads_back() {
        // Three heap groups; documents with offsets 1 and 291.
        let tids = vec![
            Tid {
                block: 0,
                offset: 1,
            },
            Tid {
                block: 0,
                offset: 2,
            },
            Tid {
                block: 3,
                offset: 291,
            },
            Tid {
                block: 256,
                offset: 1,
            },
            Tid {
                block: 9000,
                offset: 5,
            },
        ];
        let lengths = vec![3, 1, 70_000, 2, 4];
        type Docs = Vec<(u32, Vec<u32>)>;
        let terms: Vec<(&str, Docs)> = vec![
            ("alpha", vec![(0, vec![0, 2]), (2, vec![5]), (4, vec![1])]),
            ("beta", vec![(3, vec![0])]),
            ("gamma", (0..5).map(|r| (r, vec![0])).collect()),
        ];
        for options in [
            Options::default(),
            Options {
                block_size: 2,
                adaptive_tf: false,
                grid_density: 2,
                ..Options::default()
            },
        ] {
            let mut builder = Builder::new(tids.clone(), lengths.clone(), options).unwrap();
            for (term, docs) in &terms {
                let ranks: Vec<u32> = docs.iter().map(|(r, _)| *r).collect();
                let buckets: Vec<u8> = docs
                    .iter()
                    .map(|(_, p)| TfBucket::from_count(p.len() as u32).value())
                    .collect();
                let mut payload = PayloadBuilder::default();
                for (_, positions) in docs {
                    payload.push(positions).unwrap();
                }
                builder
                    .add_term(term, &ranks, &buckets, &payload.finish())
                    .unwrap();
            }
            let (blob, _) = builder.finish(&[1]);
            let segment = Segment::parse(&blob).unwrap();
            assert_eq!(segment.documents, 5);
            assert_eq!(segment.liveness.dead, 1);
            assert_eq!(segment.docs.tids(), tids);
            for (rank, length) in lengths.iter().enumerate() {
                assert_eq!(segment.lengths.get(rank as u32).unwrap(), *length);
            }
            for (term, docs) in &terms {
                let found = segment.term(term).unwrap().unwrap();
                let slots = found.postings.slots(&segment.docs.geometry).unwrap();
                let got: Vec<Tid> = slots
                    .iter()
                    .map(|s| segment.docs.geometry.tid_of(*s))
                    .collect();
                let want: Vec<Tid> = docs.iter().map(|(r, _)| tids[*r as usize]).collect();
                assert_eq!(got, want);
                let footer = found
                    .postings
                    .footer(
                        segment.block_size,
                        found.entry.max_tf_bucket,
                        segment.adaptive_tf,
                    )
                    .unwrap();
                for (i, (_, positions)) in docs.iter().enumerate() {
                    assert_eq!(
                        footer.bucket(found.postings.tf, i as u32).unwrap(),
                        TfBucket::from_count(positions.len() as u32).value()
                    );
                }
                let (bytes, _) = segment.positions(&found.entry).unwrap();
                let stream = super::super::positions::Positions::parse(bytes).unwrap();
                let mut at = stream.data_at;
                let mut out = Vec::new();
                for (i, (_, positions)) in docs.iter().enumerate() {
                    at = stream.read_entry(i as u32, at, &mut out).unwrap();
                    assert_eq!(&out, positions);
                }
            }
            assert!(segment.term("delta").unwrap().is_none());
        }
    }
}
