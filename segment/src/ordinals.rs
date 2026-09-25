// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A term's documents as segment-local document ordinals.
//!
//! A segment's document table is in TID order, so a document's ordinal there is
//! a dense number in `0..doc_count`. Addressing a term's documents by ordinal
//! makes a frequent term a bitset, and a Boolean combination of terms a
//! word-wise fold: work proportional to the chunks the terms occupy rather
//! than to the number of matches.
//!
//! ```text
//! stream    := count varint, body
//! body      := count <= LIST_MAX: delta varint * count        first absolute, then gaps - 1
//!            | otherwise:         chunk_count varint, entry * chunk_count, chunk*
//! entry     := key u16le, cardinality - 1 u16le, at u32le
//!              key is ordinal >> 16, strictly ascending; the low 31 bits of
//!              at are a byte offset from the first chunk, the top bit marks
//!              a bitmap chunk
//! chunk     := array:  low u16le * cardinality, strictly ascending
//!            | bitmap: word u64le * 1024                         (bit low set)
//! ```
//!
//! Which form a chunk takes is the writer's choice and readers follow the
//! entry. A bitmap combines 64 documents per instruction where an array pays
//! per posting, so the writer switches at `ARRAY_MAX` postings, well below the
//! 4,096 at which the two are the same size: over the published Wikipedia
//! count queries 1,024 folds 2.6 times faster than 4,096 for 1.4 times the
//! bytes, and 256 only 1.2 times faster again for 1.6 times more.
//!
//! ```text
//! ```
//!
//! The chunked body is the array/bitmap hybrid of Roaring bitmaps without run
//! containers. A reader fetches the head, the directory and then only the
//! chunks a query visits, so a paged source never copies a whole dense stream.
//!
//! [`for_each_chunk`] evaluates a Boolean tree one 65,536-document chunk at a
//! time in fixed scratch buffers, visiting only chunks some term occupies.
//!
//! A term's stream also carries what scoring a member needs beside its
//! membership: a score bound per chunk a ranked walk prunes with, and the
//! member's term-frequency bucket as a nibble, so scoring never reads the
//! positions:
//!
//! ```text
//! stream    := count varint, bound, delta varint * count, nibbles     count <= LIST_MAX
//!            | count varint, chunk_count varint, bounds_len varint,
//!              entry * chunk_count, bound * chunk_count, (chunk, nibbles)*
//! nibbles   := u8 * ceil(members / 2): the members' buckets in order, the
//!              first in the low nibble
//! bound     := buckets varint, min_len varint per set bucket ascending,
//!              occupied varint, sub u8 per set bit of occupied ascending
//! ```
//!
//! A dead list is a stream without bounds or nibbles.
//!
//! A bound names the term-frequency buckets that occur among the members it
//! covers with the shortest document per bucket, as a postings block bound
//! does, and per occupied sub-block of `SUB` ordinals one past the largest
//! bucket there; `occupied` has a bit per sub-block, so a chunk of a few
//! members costs a few bytes. A list's one bound covers every chunk it
//! touches, its sub-blocks folded together.

use std::collections::BTreeSet;

use crate::tf_bucket::BUCKET_COUNT;
use crate::{Error, Result, varint};

/// Ordinals per chunk.
pub const CHUNK: u32 = 1 << 16;
/// Machine words per chunk bitmap.
pub const WORDS: usize = (CHUNK / 64) as usize;
/// Chunks with at least this many postings are written as bitmaps.
pub const ARRAY_MAX: usize = 1024;
/// Set on a directory entry's offset when its chunk is a bitmap.
const BITMAP: u32 = 1 << 31;
/// Streams of at most this many ordinals are a plain delta list.
pub const LIST_MAX: usize = 64;
const ENTRY: usize = 8;
/// Bytes that hold a stream's count, chunk count and bounds length.
const HEAD: usize = 16;

/// One chunk's membership.
pub type Words = [u64; WORDS];

/// Ordinals per sub-block of a chunk, the grain of a chunk bound's buckets.
pub const SUB: u32 = 1024;
/// Sub-blocks per chunk.
pub const SUBS: usize = (CHUNK / SUB) as usize;

/// The score bound over the members of one chunk, or of a whole list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkBound {
    /// Per term-frequency bucket, the shortest document among the members
    /// with that bucket; `u32::MAX` where the bucket does not occur.
    pub min_len: [u32; BUCKET_COUNT],
    /// Per sub-block, one past the largest bucket among its members; zero
    /// where there are none.
    pub subs: [u8; SUBS],
}

impl ChunkBound {
    fn empty() -> Self {
        Self {
            min_len: [u32::MAX; BUCKET_COUNT],
            subs: [0; SUBS],
        }
    }

    fn add(&mut self, low: u32, bucket: u8, len: u32) {
        let slot = &mut self.min_len[usize::from(bucket)];
        *slot = (*slot).min(len.min(u32::MAX - 1));
        let sub = &mut self.subs[(low / SUB) as usize];
        *sub = (*sub).max(bucket + 1);
    }

    fn put(&self, out: &mut Vec<u8>) {
        let buckets = self
            .min_len
            .iter()
            .enumerate()
            .filter(|(_, len)| **len != u32::MAX)
            .fold(0u64, |mask, (bucket, _)| mask | 1 << bucket);
        varint::put(out, buckets);
        for len in self.min_len.iter().filter(|len| **len != u32::MAX) {
            varint::put(out, u64::from(*len));
        }
        let occupied = self
            .subs
            .iter()
            .enumerate()
            .filter(|(_, sub)| **sub != 0)
            .fold(0u64, |mask, (i, _)| mask | 1 << i);
        varint::put(out, occupied);
        out.extend(self.subs.iter().filter(|sub| **sub != 0));
    }

    fn get(bytes: &[u8], at: &mut usize) -> Result<Self> {
        // A bound covers at least one member: some bucket and some sub-block.
        let buckets = varint::get(bytes, at)?;
        if buckets == 0 || buckets >> BUCKET_COUNT != 0 {
            return Err(Error::Corrupt("chunk bound buckets"));
        }
        let mut bound = Self::empty();
        for bucket in 0..BUCKET_COUNT {
            if buckets & (1 << bucket) != 0 {
                let len = varint::get_u32(bytes, at)?;
                if len == u32::MAX {
                    return Err(Error::Corrupt("chunk bound length"));
                }
                bound.min_len[bucket] = len;
            }
        }
        let occupied = varint::get(bytes, at)?;
        if occupied == 0 {
            return Err(Error::Corrupt("chunk bound sub-blocks"));
        }
        for i in 0..SUBS {
            if occupied & (1 << i) == 0 {
                continue;
            }
            let byte = *bytes.get(*at).ok_or(Error::Truncated)?;
            if byte == 0 || usize::from(byte) > BUCKET_COUNT {
                return Err(Error::Corrupt("chunk bound sub-block bucket"));
            }
            bound.subs[i] = byte;
            *at += 1;
        }
        Ok(bound)
    }
}

/// Encodes strictly ascending ordinals with a bound per chunk, from each
/// member's term-frequency bucket and document length.
pub fn encode_scored(ordinals: &[u32], scores: &[(u8, u32)]) -> Vec<u8> {
    assert_eq!(ordinals.len(), scores.len());
    encode_with(ordinals, Some(scores))
}

/// Encodes strictly ascending ordinals without bounds.
pub fn encode(ordinals: &[u32]) -> Vec<u8> {
    encode_with(ordinals, None)
}

fn encode_with(ordinals: &[u32], scores: Option<&[(u8, u32)]>) -> Vec<u8> {
    debug_assert!(ordinals.windows(2).all(|pair| pair[0] < pair[1]));
    let mut out = Vec::new();
    varint::put(&mut out, ordinals.len() as u64);
    if ordinals.len() <= LIST_MAX {
        if let Some(scores) = scores {
            let mut bound = ChunkBound::empty();
            for (ordinal, (bucket, len)) in ordinals.iter().zip(scores) {
                bound.add(ordinal & 0xffff, *bucket, *len);
            }
            bound.put(&mut out);
        }
        let mut previous = None;
        for ordinal in ordinals {
            varint::put(
                &mut out,
                u64::from(previous.map_or(*ordinal, |p: u32| ordinal - p - 1)),
            );
            previous = Some(*ordinal);
        }
        if let Some(scores) = scores {
            put_nibbles(&mut out, scores.iter().map(|(bucket, _)| *bucket));
        }
        return out;
    }
    let mut directory = Vec::new();
    let mut bounds = Vec::new();
    let mut body = Vec::new();
    let mut chunks = 0u64;
    let mut scored = 0usize;
    for members in ordinals.chunk_by(|a, b| a >> 16 == b >> 16) {
        if let Some(scores) = scores {
            let mut bound = ChunkBound::empty();
            for (ordinal, (bucket, len)) in members.iter().zip(&scores[scored..]) {
                bound.add(ordinal & 0xffff, *bucket, *len);
            }
            bound.put(&mut bounds);
            scored += members.len();
        }
        directory.extend_from_slice(&((members[0] >> 16) as u16).to_le_bytes());
        directory.extend_from_slice(&((members.len() - 1) as u16).to_le_bytes());
        let bitmap = members.len() >= ARRAY_MAX;
        let at = body.len() as u32 | if bitmap { BITMAP } else { 0 };
        directory.extend_from_slice(&at.to_le_bytes());
        if !bitmap {
            for ordinal in members {
                body.extend_from_slice(&(*ordinal as u16).to_le_bytes());
            }
        } else {
            let mut words = [0u64; WORDS];
            for ordinal in members {
                let low = (*ordinal & 0xffff) as usize;
                words[low / 64] |= 1 << (low % 64);
            }
            for word in words {
                body.extend_from_slice(&word.to_le_bytes());
            }
        }
        if let Some(scores) = scores {
            let from = scored - members.len();
            put_nibbles(
                &mut body,
                scores[from..scored].iter().map(|(bucket, _)| *bucket),
            );
        }
        chunks += 1;
    }
    varint::put(&mut out, chunks);
    if scores.is_some() {
        varint::put(&mut out, bounds.len() as u64);
    }
    out.extend_from_slice(&directory);
    out.extend_from_slice(&bounds);
    out.extend_from_slice(&body);
    out
}

/// Packs buckets two to a byte, the first in the low nibble.
fn put_nibbles(out: &mut Vec<u8>, buckets: impl Iterator<Item = u8>) {
    let mut pending = None;
    for bucket in buckets {
        debug_assert!(bucket < 16);
        match pending.take() {
            None => pending = Some(bucket),
            Some(low) => out.push(low | bucket << 4),
        }
    }
    if let Some(low) = pending {
        out.push(low);
    }
}

/// The bucket of member `within` of `count` packed in `nibbles`.
fn nibble(nibbles: &[u8], within: usize) -> Option<u8> {
    let byte = *nibbles.get(within / 2)?;
    Some(if within.is_multiple_of(2) {
        byte & 0xf
    } else {
        byte >> 4
    })
}

/// Byte ranges of one encoded stream, fetched on demand.
pub trait Fetch<'a> {
    /// `len` bytes at `offset` from the start of the stream.
    fn fetch(&self, offset: u64, len: usize) -> Result<&'a [u8]>;
    /// An owned copy that the source need not keep: a chunk a walk visits.
    fn fetch_owned(&self, offset: u64, len: usize) -> Result<std::rc::Rc<[u8]>> {
        self.fetch(offset, len).map(std::rc::Rc::from)
    }
}

impl<'a> Fetch<'a> for &'a [u8] {
    fn fetch(&self, offset: u64, len: usize) -> Result<&'a [u8]> {
        usize::try_from(offset)
            .ok()
            .and_then(|at| self.get(at..at.checked_add(len)?))
            .ok_or(Error::Truncated)
    }
}

enum Body<'a> {
    List(Vec<u32>),
    Chunked {
        directory: &'a [u8],
        /// Where the first chunk starts in the stream.
        chunks_at: u64,
        /// Members before each chunk, computed when first selected from.
        before: std::cell::OnceCell<Vec<u32>>,
    },
}

/// An open stream. Chunk bodies are fetched and checked as they are visited.
pub struct Ordinals<'a> {
    source: Box<dyn Fetch<'a> + 'a>,
    len: u64,
    count: u32,
    body: Body<'a>,
    /// One per chunk, or one for a list; empty for a stream without bounds.
    bounds: Vec<ChunkBound>,
    /// Whether members carry buckets (and chunks bounds): a term's stream,
    /// as opposed to a dead list.
    scored: bool,
    /// A list's buckets, one per member.
    list_buckets: Vec<u8>,
}

fn entry_key(entry: &[u8]) -> u16 {
    u16::from_le_bytes([entry[0], entry[1]])
}

/// A directory entry's cardinality, body offset, body size (members only)
/// and whether the chunk is a bitmap.
fn entry_chunk(entry: &[u8]) -> (usize, u64, usize, bool) {
    let cardinality = usize::from(u16::from_le_bytes([entry[2], entry[3]])) + 1;
    let at = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]);
    let bitmap = at & BITMAP != 0;
    let size = if bitmap { WORDS * 8 } else { cardinality * 2 };
    (cardinality, u64::from(at & !BITMAP), size, bitmap)
}

/// Bytes of a chunk's nibbles for `cardinality` members, if `scored`.
fn nibbles_len(cardinality: usize, scored: bool) -> usize {
    if scored { cardinality.div_ceil(2) } else { 0 }
}

impl<'a> Ordinals<'a> {
    /// Opens the stream of `len` bytes behind `source`; `bounded` says the
    /// stream carries chunk bounds (`LSG5`).
    pub fn open(source: impl Fetch<'a> + 'a, len: u64, bounded: bool) -> Result<Self> {
        let head = source.fetch(0, len.min(HEAD as u64) as usize)?;
        let mut at = 0;
        let count = varint::get_u32(head, &mut at)?;
        let mut bounds = Vec::new();
        let mut list_buckets = Vec::new();
        let body = if count as usize <= LIST_MAX {
            let bytes = source.fetch(0, len as usize)?;
            if bounded {
                bounds.push(ChunkBound::get(bytes, &mut at)?);
            }
            let mut list = Vec::with_capacity(count as usize);
            let mut previous: Option<u32> = None;
            for _ in 0..count {
                let delta = varint::get_u32(bytes, &mut at)?;
                let ordinal = match previous {
                    None => Some(delta),
                    Some(p) => p.checked_add(delta).and_then(|o| o.checked_add(1)),
                }
                .ok_or(Error::Corrupt("ordinal overflow"))?;
                list.push(ordinal);
                previous = Some(ordinal);
            }
            if bounded {
                let nibbles = bytes
                    .get(at..at + (count as usize).div_ceil(2))
                    .ok_or(Error::Truncated)?;
                list_buckets = (0..count as usize)
                    .map(|i| nibble(nibbles, i).expect("within the nibbles"))
                    .collect();
                at += nibbles.len();
            }
            if at as u64 != len {
                return Err(Error::Corrupt("ordinal list length"));
            }
            Body::List(list)
        } else {
            let chunks = u64::from(varint::get_u32(head, &mut at)?);
            let bounds_len = if bounded {
                u64::from(varint::get_u32(head, &mut at)?)
            } else {
                0
            };
            let bounds_at = at as u64 + chunks * ENTRY as u64;
            let chunks_at = bounds_at + bounds_len;
            if chunks == 0 || chunks > u64::from(CHUNK) || chunks_at > len {
                return Err(Error::Corrupt("ordinal directory"));
            }
            if bounded {
                let bytes = source.fetch(bounds_at, bounds_len as usize)?;
                let mut at = 0;
                for _ in 0..chunks {
                    bounds.push(ChunkBound::get(bytes, &mut at)?);
                }
                if at != bytes.len() {
                    return Err(Error::Corrupt("chunk bounds length"));
                }
            }
            Body::Chunked {
                directory: source.fetch(at as u64, (chunks as usize) * ENTRY)?,
                chunks_at,
                before: std::cell::OnceCell::new(),
            }
        };
        Ok(Self {
            source: Box::new(source),
            len,
            count,
            body,
            bounds,
            scored: bounded,
            list_buckets,
        })
    }

    /// Opens a stream held in memory, without bounds.
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        Self::open(bytes, bytes.len() as u64, false)
    }

    /// The bounds the stream carries: one per chunk, or one for a list.
    pub fn bounds(&self) -> &[ChunkBound] {
        &self.bounds
    }

    /// The bound over chunk `i`, or over the whole list, when stored.
    pub fn chunk_bound(&self, i: usize) -> Option<&ChunkBound> {
        match &self.body {
            Body::List(_) => self.bounds.first(),
            Body::Chunked { .. } => self.bounds.get(i),
        }
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    /// Adds the chunk keys the stream occupies.
    fn keys(&self, into: &mut BTreeSet<u16>) {
        match &self.body {
            Body::List(list) => into.extend(list.iter().map(|ordinal| (ordinal >> 16) as u16)),
            Body::Chunked { directory, .. } => {
                into.extend(directory.chunks_exact(ENTRY).map(entry_key));
            }
        }
    }

    /// Combines chunk `key` into `out`. `at` is the caller's position in the
    /// directory or list, which only moves forward. Returns whether the stream
    /// occupies the chunk; an absent chunk leaves `out` alone.
    fn combine(&self, key: u16, at: &mut usize, op: Op, out: &mut Words) -> Result<bool> {
        match &self.body {
            Body::List(list) => {
                while list.get(*at).is_some_and(|o| ((o >> 16) as u16) < key) {
                    *at += 1;
                }
                let end = *at
                    + list[*at..]
                        .iter()
                        .take_while(|o| (*o >> 16) as u16 == key)
                        .count();
                if end == *at {
                    return Ok(false);
                }
                // The position stays on the chunk: a tree may name a term twice.
                apply_lows(
                    list[*at..end].iter().map(|o| (o & 0xffff) as usize),
                    op,
                    out,
                );
                Ok(true)
            }
            Body::Chunked {
                directory,
                chunks_at,
                ..
            } => {
                let entry = |i: usize| &directory[i * ENTRY..(i + 1) * ENTRY];
                let chunks = directory.len() / ENTRY;
                while *at < chunks && entry_key(entry(*at)) < key {
                    *at += 1;
                }
                if *at == chunks || entry_key(entry(*at)) != key {
                    return Ok(false);
                }
                let (_, offset, size, bitmap) = entry_chunk(entry(*at));
                let start = chunks_at + offset;
                if start + size as u64 > self.len {
                    return Err(Error::Truncated);
                }
                let bytes = self.source.fetch_owned(start, size)?;
                if !bitmap {
                    let lows = bytes
                        .chunks_exact(2)
                        .map(|low| usize::from(u16::from_le_bytes([low[0], low[1]])));
                    apply_lows(lows, op, out);
                } else {
                    match op {
                        Op::Assign => kernels::assign_bytes(out, &bytes),
                        Op::Or => kernels::or_bytes(out, &bytes),
                        Op::And => kernels::and_bytes(out, &bytes),
                    }
                }
                Ok(true)
            }
        }
    }

    /// Bytes before the chunk bodies of a chunked stream: the count, the
    /// directory and the bounds. A list is all head.
    pub fn head_len(&self) -> usize {
        match &self.body {
            Body::List(_) => self.len as usize,
            Body::Chunked { chunks_at, .. } => *chunks_at as usize,
        }
    }

    /// Chunks stored as bitmaps.
    pub fn bitmap_chunks(&self) -> usize {
        match &self.body {
            Body::List(_) => 0,
            Body::Chunked { directory, .. } => directory
                .chunks_exact(ENTRY)
                .filter(|entry| entry_chunk(entry).3)
                .count(),
        }
    }

    /// The stream as a list, when it is short enough to be stored as one.
    pub fn list(&self) -> Option<&[u32]> {
        match &self.body {
            Body::List(list) => Some(list),
            Body::Chunked { .. } => None,
        }
    }

    /// A list's members' buckets, parallel to [`Ordinals::list`]; empty for
    /// a chunked stream or a stream without buckets.
    pub fn list_buckets(&self) -> &[u8] {
        &self.list_buckets
    }

    /// Whether members carry buckets.
    pub const fn is_scored(&self) -> bool {
        self.scored
    }

    /// Chunks of a chunked stream; zero for a list.
    pub fn chunk_count(&self) -> usize {
        match &self.body {
            Body::List(_) => 0,
            Body::Chunked { directory, .. } => directory.len() / ENTRY,
        }
    }

    /// The key of chunk `i` of a chunked stream.
    pub fn chunk_key(&self, i: usize) -> u16 {
        match &self.body {
            Body::List(_) => unreachable!("a list has no chunks"),
            Body::Chunked { directory, .. } => entry_key(&directory[i * ENTRY..(i + 1) * ENTRY]),
        }
    }

    /// Members in the chunks before chunk `i` of a chunked stream.
    fn before(&self, i: usize) -> u32 {
        let Body::Chunked {
            directory, before, ..
        } = &self.body
        else {
            unreachable!("a list has no chunks")
        };
        before.get_or_init(|| {
            let mut total = 0u32;
            directory
                .chunks_exact(ENTRY)
                .map(|entry| {
                    let start = total;
                    total = total.saturating_add(entry_chunk(entry).0 as u32);
                    start
                })
                .collect()
        })[i]
    }

    /// Chunk `i` of a chunked stream, with its body fetched.
    pub fn chunk(&self, i: usize) -> Result<Chunk> {
        let Body::Chunked {
            directory,
            chunks_at,
            ..
        } = &self.body
        else {
            return Err(Error::Corrupt("a list has no chunks"));
        };
        let entry = &directory[i * ENTRY..(i + 1) * ENTRY];
        let (cardinality, offset, size, bitmap) = entry_chunk(entry);
        let start = chunks_at + offset;
        let total = size + nibbles_len(cardinality, self.scored);
        if start + total as u64 > self.len {
            return Err(Error::Truncated);
        }
        Ok(Chunk {
            key: entry_key(entry),
            cardinality: cardinality as u32,
            before: self.before(i),
            bytes: self.source.fetch_owned(start, total)?,
            bitmap,
            body_len: size,
            buckets_at: self.scored.then_some(size),
        })
    }

    /// The ordinal at `index` of the stream: the document of the term's
    /// `index`-th posting, since the stream parallels the term's postings.
    pub fn select(&self, index: u32) -> Result<u32> {
        if index >= self.count {
            return Err(Error::Corrupt("posting beyond the ordinal stream"));
        }
        let (directory, chunks_at, before) = match &self.body {
            Body::List(list) => return Ok(list[index as usize]),
            Body::Chunked {
                directory,
                chunks_at,
                before,
            } => (directory, *chunks_at, before),
        };
        let before = before.get_or_init(|| {
            let mut total = 0u32;
            directory
                .chunks_exact(ENTRY)
                .map(|entry| {
                    let start = total;
                    total = total.saturating_add(entry_chunk(entry).0 as u32);
                    start
                })
                .collect()
        });
        let chunk = before.partition_point(|start| *start <= index) - 1;
        let entry = &directory[chunk * ENTRY..(chunk + 1) * ENTRY];
        let (cardinality, offset, size, bitmap) = entry_chunk(entry);
        let mut within = (index - before[chunk]) as usize;
        if within >= cardinality {
            return Err(Error::Corrupt("ordinal count differs from its chunks"));
        }
        let start = chunks_at + offset;
        if start + size as u64 > self.len {
            return Err(Error::Truncated);
        }
        let bytes = self.source.fetch_owned(start, size)?;
        let base = u32::from(entry_key(entry)) << 16;
        if !bitmap {
            let at = within * 2;
            return Ok(base | u32::from(u16::from_le_bytes([bytes[at], bytes[at + 1]])));
        }
        for (i, word) in bytes.chunks_exact(8).enumerate() {
            let mut word = u64::from_le_bytes(word.try_into().unwrap());
            let set = word.count_ones() as usize;
            if within < set {
                for _ in 0..within {
                    word &= word - 1;
                }
                return Ok(base | (i as u32 * 64 + word.trailing_zeros()));
            }
            within -= set;
        }
        Err(Error::Corrupt("ordinal bitmap cardinality"))
    }

    /// The rank of `ordinal` in the stream, if it is a member: the index
    /// of the term's entry for that document in its payload.
    pub fn rank(&self, ordinal: u32) -> Result<Option<u32>> {
        let (directory, ..) = match &self.body {
            Body::List(list) => return Ok(list.binary_search(&ordinal).ok().map(|i| i as u32)),
            Body::Chunked {
                directory,
                chunks_at,
                before,
            } => (directory, chunks_at, before),
        };
        let key = (ordinal >> 16) as u16;
        let chunks = directory.len() / ENTRY;
        let (mut lo, mut hi) = (0usize, chunks);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match entry_key(&directory[mid * ENTRY..(mid + 1) * ENTRY]).cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let chunk = self.chunk(mid)?;
                    return Ok(chunk
                        .rank((ordinal & 0xffff) as u16)
                        .map(|r| chunk.before + r));
                }
            }
        }
        Ok(None)
    }

    /// Whether the stream is dense enough that a scan over it should work
    /// a heap page at a time: any chunk stored as a bitmap.
    pub fn prefers_pages(&self) -> bool {
        match &self.body {
            Body::List(_) => false,
            Body::Chunked { directory, .. } => directory
                .chunks_exact(ENTRY)
                .any(|entry| entry_chunk(entry).3),
        }
    }

    /// A cursor over the stream's members in ascending order.
    pub fn cursor(self) -> Result<OrdinalCursor<'a>> {
        let mut cursor = OrdinalCursor {
            stream: self,
            chunk: None,
            index: 0,
            rank: 0,
            low: 0,
        };
        cursor.enter(0)?;
        Ok(cursor)
    }

    /// Every ordinal, for verification and tests.
    pub fn to_vec(&self) -> Result<Vec<u32>> {
        if let Body::List(list) = &self.body {
            return Ok(list.clone());
        }
        let mut keys = BTreeSet::new();
        self.keys(&mut keys);
        // The count is unverified: reserve no more than the chunks can hold.
        let mut out = Vec::with_capacity((self.count as usize).min(keys.len() * CHUNK as usize));
        let mut at = 0;
        let mut words = Box::new([0u64; WORDS]);
        for key in keys {
            self.combine(key, &mut at, Op::Assign, &mut words)?;
            members(&words, u32::from(key) << 16, &mut out);
        }
        Ok(out)
    }
}

/// A cursor over a stream's members: the ordinals in ascending order with
/// each member's rank, which addresses the term's payload.
pub struct OrdinalCursor<'a> {
    stream: Ordinals<'a>,
    /// The chunk being walked, for a chunked stream; `None` once exhausted.
    chunk: Option<Chunk>,
    /// Position in the list, or the chunk's index in the directory.
    index: usize,
    /// Rank of the current member.
    rank: u32,
    /// Low bits of the current member of a chunk; for an array chunk, the
    /// index into it.
    low: u32,
}

impl<'a> OrdinalCursor<'a> {
    pub fn stream(&self) -> &Ordinals<'a> {
        &self.stream
    }

    pub fn count(&self) -> u32 {
        self.stream.count()
    }

    /// Positions on the first member of chunk `i`, or exhausts the cursor
    /// past the last chunk; a list positions on entry `i`.
    fn enter(&mut self, i: usize) -> Result<()> {
        match &self.stream.body {
            Body::List(_) => {
                self.index = i;
                self.rank = i as u32;
            }
            Body::Chunked { .. } => {
                if i >= self.stream.chunk_count() {
                    self.chunk = None;
                    self.index = i;
                    self.rank = self.stream.count();
                    return Ok(());
                }
                let chunk = self.stream.chunk(i)?;
                self.index = i;
                self.rank = chunk.before;
                self.low = if chunk.bitmap {
                    Self::first_set(chunk.body(), 0).ok_or(Error::Corrupt("empty ordinal chunk"))?
                } else {
                    0
                };
                self.chunk = Some(chunk);
            }
        }
        Ok(())
    }

    /// The first set bit at or after `from` in a bitmap chunk.
    fn first_set(bytes: &[u8], from: u32) -> Option<u32> {
        let mut word = (from / 64) as usize;
        let mut mask = !0u64 << (from % 64);
        while word < WORDS {
            let at = &bytes[word * 8..word * 8 + 8];
            let value = u64::from_le_bytes(at.try_into().unwrap()) & mask;
            if value != 0 {
                return Some(word as u32 * 64 + value.trailing_zeros());
            }
            word += 1;
            mask = !0;
        }
        None
    }

    fn array_low(chunk: &Chunk, at: usize) -> u32 {
        let bytes = &chunk.body()[at * 2..at * 2 + 2];
        u32::from(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    /// The current member.
    pub fn current(&self) -> Option<u32> {
        match &self.stream.body {
            Body::List(list) => list.get(self.index).copied(),
            Body::Chunked { .. } => {
                let chunk = self.chunk.as_ref()?;
                let low = if chunk.bitmap {
                    self.low
                } else {
                    Self::array_low(chunk, self.low as usize)
                };
                Some(chunk.base() | low)
            }
        }
    }

    /// The rank of the current member; the count once exhausted.
    pub fn rank(&self) -> u32 {
        self.rank
    }

    /// The current member's term-frequency bucket, when the stream stores
    /// buckets.
    pub fn bucket(&self) -> Option<u8> {
        match &self.stream.body {
            Body::List(_) => self.stream.list_buckets.get(self.index).copied(),
            Body::Chunked { .. } => {
                let chunk = self.chunk.as_ref()?;
                chunk.bucket(self.rank - chunk.before)
            }
        }
    }

    pub fn advance(&mut self) -> Result<()> {
        match &self.stream.body {
            Body::List(list) => {
                if self.index < list.len() {
                    self.index += 1;
                    self.rank += 1;
                }
                Ok(())
            }
            Body::Chunked { .. } => {
                let Some(chunk) = &self.chunk else {
                    return Ok(());
                };
                let next = if chunk.bitmap {
                    Self::first_set(chunk.body(), self.low + 1)
                } else {
                    (self.low as usize + 1 < chunk.body_len / 2).then_some(self.low + 1)
                };
                match next {
                    Some(low) => {
                        self.low = low;
                        self.rank += 1;
                        Ok(())
                    }
                    None => self.enter(self.index + 1),
                }
            }
        }
    }

    /// Back to the first member, without reparsing the stream.
    pub fn rewind(&mut self) -> Result<()> {
        self.enter(0)
    }

    /// Moves to the first member at or after `target`.
    pub fn seek(&mut self, target: u32) -> Result<()> {
        if self.current().is_none_or(|current| current >= target) {
            return Ok(());
        }
        match &self.stream.body {
            Body::List(list) => {
                let at = self.index + list[self.index..].partition_point(|o| *o < target);
                self.enter(at)
            }
            Body::Chunked { directory, .. } => {
                let key = (target >> 16) as u16;
                let chunks = directory.len() / ENTRY;
                let current_key = self.chunk.as_ref().map(|chunk| chunk.key);
                if current_key != Some(key) {
                    // The target's chunk, or the first one after it.
                    let mut i = self.index;
                    while i < chunks && entry_key(&directory[i * ENTRY..(i + 1) * ENTRY]) < key {
                        i += 1;
                    }
                    self.enter(i)?;
                    if self.chunk.as_ref().is_none_or(|chunk| chunk.key != key) {
                        return Ok(());
                    }
                }
                let chunk = self
                    .chunk
                    .as_ref()
                    .expect("positioned on the target's chunk");
                let low = target & 0xffff;
                let next = if chunk.bitmap {
                    // Count only the members between the current one and the
                    // target: a forward seek within a chunk costs the bits it
                    // passes, not the chunk.
                    let same = current_key == Some(key);
                    let from = if same { self.low + 1 } else { 0 };
                    let base = if same {
                        self.rank - chunk.before + 1
                    } else {
                        0
                    };
                    Self::first_set(chunk.body(), low.max(from)).map(|found| {
                        (
                            found,
                            base + Self::popcount_between(chunk.body(), from, found),
                        )
                    })
                } else {
                    let count = chunk.body_len / 2;
                    let (mut lo, mut hi) = (self.low as usize, count);
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        if Self::array_low(chunk, mid) < low {
                            lo = mid + 1;
                        } else {
                            hi = mid;
                        }
                    }
                    (lo < count).then_some((lo as u32, lo as u32))
                };
                match next {
                    Some((low, within)) => {
                        self.low = low;
                        self.rank = chunk.before + within;
                        Ok(())
                    }
                    None => self.enter(self.index + 1),
                }
            }
        }
    }

    /// Set bits in `from..to` of a bitmap chunk.
    fn popcount_between(bytes: &[u8], from: u32, to: u32) -> u32 {
        if from >= to {
            return 0;
        }
        let word = |i: usize| {
            let at = &bytes[i * 8..i * 8 + 8];
            u64::from_le_bytes(at.try_into().unwrap())
        };
        let (first, last) = ((from / 64) as usize, (to / 64) as usize);
        let low_mask = !0u64 << (from % 64);
        let high_mask = (1u64 << (to % 64)) - 1;
        if first == last {
            return (word(first) & low_mask & high_mask).count_ones();
        }
        let mut count = (word(first) & low_mask).count_ones();
        for i in first + 1..last {
            count += word(i).count_ones();
        }
        count + (word(last) & high_mask).count_ones()
    }
}

#[derive(Clone, Copy)]
enum Op {
    Assign,
    Or,
    And,
}

fn apply_lows(lows: impl Iterator<Item = usize>, op: Op, out: &mut Words) {
    match op {
        Op::Or => lows.for_each(|low| out[low / 64] |= 1 << (low % 64)),
        Op::Assign => {
            out.fill(0);
            lows.for_each(|low| out[low / 64] |= 1 << (low % 64));
        }
        Op::And => {
            let mut kept = [0u64; WORDS];
            lows.for_each(|low| kept[low / 64] |= out[low / 64] & (1 << (low % 64)));
            *out = kept;
        }
    }
}

/// One chunk of a chunked stream: its members within `key << 16 ..`.
pub struct Chunk {
    pub key: u16,
    pub cardinality: u32,
    /// Members of the stream before this chunk: the rank of its first member.
    pub before: u32,
    /// The members, then their bucket nibbles when the stream stores them.
    bytes: std::rc::Rc<[u8]>,
    bitmap: bool,
    /// Bytes of the members: the words of a bitmap or the array.
    body_len: usize,
    /// Where the members' bucket nibbles start in `bytes`, when stored.
    buckets_at: Option<usize>,
}

impl Chunk {
    /// The members' bytes: `WORDS` words for a bitmap, two bytes per member
    /// for an array.
    fn body(&self) -> &[u8] {
        &self.bytes[..self.body_len]
    }

    /// The bucket of the member at rank `within` inside the chunk, when the
    /// stream stores buckets.
    pub fn bucket(&self, within: u32) -> Option<u8> {
        let at = self.buckets_at?;
        nibble(&self.bytes[at..], within as usize)
    }

    /// The first ordinal the chunk can hold.
    pub fn base(&self) -> u32 {
        u32::from(self.key) << 16
    }

    /// Word `i` of a bitmap chunk's members, read in place: the bytes are
    /// the cache's, so a walk tests bits through the chunk rather than
    /// copying its 8 KiB into a word buffer per load. The bytes need not be
    /// word aligned.
    ///
    /// # Panics
    ///
    /// On an array chunk, which has no words.
    #[inline]
    pub fn word(&self, i: usize) -> u64 {
        assert!(self.bitmap, "an array chunk has no words");
        let at = &self.bytes[i * 8..i * 8 + 8];
        u64::from_le_bytes(at.try_into().expect("eight bytes"))
    }

    /// Set bits in words `from..to` of a bitmap chunk.
    pub fn count_words(&self, from: usize, to: usize) -> u32 {
        assert!(self.bitmap, "an array chunk has no words");
        self.bytes[from * 8..to * 8]
            .chunks_exact(8)
            .map(|w| u64::from_le_bytes(w.try_into().expect("eight bytes")).count_ones())
            .sum()
    }

    /// Ors a bitmap chunk's members into `out`.
    pub fn or_into(&self, out: &mut Words) {
        assert!(self.bitmap, "an array chunk has no words");
        kernels::or_bytes(out, self.body());
    }

    /// Keeps of `out` the members of a bitmap chunk.
    pub fn and_into(&self, out: &mut Words) {
        assert!(self.bitmap, "an array chunk has no words");
        kernels::and_bytes(out, self.body());
    }

    /// The rank within the chunk of the member with low bits `low`, if any.
    pub fn rank(&self, low: u16) -> Option<u32> {
        if self.bitmap {
            let word = usize::from(low / 64);
            let bit = low % 64;
            let value = self.word(word);
            if value & (1 << bit) == 0 {
                return None;
            }
            let before = self.count_words(0, word);
            Some(before + (value & ((1u64 << bit) - 1)).count_ones())
        } else {
            let lows = self.body().chunks_exact(2);
            let count = lows.len();
            let (mut lo, mut hi) = (0usize, count);
            while lo < hi {
                let mid = (lo + hi) / 2;
                let at = &self.body()[mid * 2..mid * 2 + 2];
                let value = u16::from_le_bytes([at[0], at[1]]);
                match value.cmp(&low) {
                    std::cmp::Ordering::Less => lo = mid + 1,
                    std::cmp::Ordering::Greater => hi = mid,
                    std::cmp::Ordering::Equal => return Some(mid as u32),
                }
            }
            None
        }
    }

    /// Whether the chunk is a bitmap rather than an array of members.
    pub fn is_bitmap(&self) -> bool {
        self.bitmap
    }

    /// A bitmap chunk's bytes, shared with the cache: `WORDS` little-endian
    /// words, then the bucket nibbles when stored. A walk that tests bits
    /// per word holds these rather than going through the chunk each time.
    pub fn bitmap_bytes(&self) -> Option<std::rc::Rc<[u8]>> {
        self.bitmap.then(|| self.bytes.clone())
    }

    /// Appends the low bits of an array chunk's members, ascending; nothing
    /// for a bitmap.
    pub fn members(&self, out: &mut Vec<u16>) {
        if !self.bitmap {
            out.extend(
                self.body()
                    .chunks_exact(2)
                    .map(|low| u16::from_le_bytes([low[0], low[1]])),
            );
        }
    }

    /// Sets `out` to the chunk's members.
    pub fn words(&self, out: &mut Words) {
        if self.bitmap {
            kernels::assign_bytes(out, self.body());
        } else {
            out.fill(0);
            for low in self.body().chunks_exact(2) {
                let low = usize::from(u16::from_le_bytes([low[0], low[1]]));
                out[low / 64] |= 1 << (low % 64);
            }
        }
    }
}

/// The number of members of a chunk.
pub fn count(words: &Words) -> u32 {
    kernels::count(words)
}

/// The word loops of a fold. An x86-64 build targets a baseline without a
/// population-count instruction or wide vectors, so each loop is also compiled
/// for the features a fold profits from and chosen by what the CPU reports;
/// the bodies are the same safe code throughout. Other architectures' baselines
/// already include what these loops use.
mod kernels {
    use super::Words;

    macro_rules! kernel {
        ($(#[$doc:meta])* $name:ident, $body:ident, ($($arg:ident: $ty:ty),*) $(-> $ret:ty)?) => {
            $(#[$doc])*
            pub fn $name($($arg: $ty),*) $(-> $ret)? {
                #[cfg(target_arch = "x86_64")]
                {
                    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
                    unsafe fn widest($($arg: $ty),*) $(-> $ret)? {
                        $body($($arg),*)
                    }
                    #[target_feature(enable = "avx2,popcnt")]
                    unsafe fn wide($($arg: $ty),*) $(-> $ret)? {
                        $body($($arg),*)
                    }
                    if std::arch::is_x86_feature_detected!("avx512f")
                        && std::arch::is_x86_feature_detected!("avx512bw")
                        && std::arch::is_x86_feature_detected!("avx512vpopcntdq")
                    {
                        // SAFETY: the CPU reports every feature `widest` enables.
                        return unsafe { widest($($arg),*) };
                    }
                    if std::arch::is_x86_feature_detected!("avx2")
                        && std::arch::is_x86_feature_detected!("popcnt")
                    {
                        // SAFETY: the CPU reports every feature `wide` enables.
                        return unsafe { wide($($arg),*) };
                    }
                }
                $body($($arg),*)
            }
        };
    }

    fn word(bytes: &[u8]) -> u64 {
        u64::from_le_bytes(bytes.try_into().unwrap())
    }

    #[inline(always)]
    fn assign_bytes_body(out: &mut Words, bytes: &[u8]) {
        out.iter_mut()
            .zip(bytes.chunks_exact(8))
            .for_each(|(o, w)| *o = word(w));
    }

    #[inline(always)]
    fn or_bytes_body(out: &mut Words, bytes: &[u8]) {
        out.iter_mut()
            .zip(bytes.chunks_exact(8))
            .for_each(|(o, w)| *o |= word(w));
    }

    #[inline(always)]
    fn and_bytes_body(out: &mut Words, bytes: &[u8]) {
        out.iter_mut()
            .zip(bytes.chunks_exact(8))
            .for_each(|(o, w)| *o &= word(w));
    }

    #[inline(always)]
    fn or_words_body(out: &mut Words, other: &Words) {
        out.iter_mut().zip(other).for_each(|(o, w)| *o |= w);
    }

    #[inline(always)]
    fn and_words_body(out: &mut Words, other: &Words) {
        out.iter_mut().zip(other).for_each(|(o, w)| *o &= w);
    }

    #[inline(always)]
    fn count_body(words: &Words) -> u32 {
        words.iter().map(|word| word.count_ones()).sum()
    }

    kernel!(
        /// Overwrites `out` with a bitmap chunk's little-endian words.
        assign_bytes, assign_bytes_body, (out: &mut Words, bytes: &[u8])
    );
    kernel!(or_bytes, or_bytes_body, (out: &mut Words, bytes: &[u8]));
    kernel!(and_bytes, and_bytes_body, (out: &mut Words, bytes: &[u8]));
    kernel!(or_words, or_words_body, (out: &mut Words, other: &Words));
    kernel!(and_words, and_words_body, (out: &mut Words, other: &Words));
    kernel!(count, count_body, (words: &Words) -> u32);
}

/// Appends the ordinals set in `words`, offset by `base`.
pub fn members(words: &Words, base: u32, out: &mut Vec<u32>) {
    for (i, word) in words.iter().enumerate() {
        let mut word = *word;
        while word != 0 {
            out.push(base + i as u32 * 64 + word.trailing_zeros());
            word &= word - 1;
        }
    }
}

/// A Boolean combination of streams, by index into the stream list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Term(usize),
    Or(Vec<Node>),
    And(Vec<Node>),
}

struct Evaluator<'s, 'a> {
    streams: &'s [Option<Ordinals<'a>>],
    positions: Vec<usize>,
    /// Scratch chunks by tree depth, absent while lent out.
    scratch: Vec<Option<Box<Words>>>,
}

impl Evaluator<'_, '_> {
    fn term(&mut self, i: usize, key: u16, op: Op, out: &mut Words) -> Result<bool> {
        match &self.streams[i] {
            Some(stream) => stream.combine(key, &mut self.positions[i], op, out),
            None => Ok(false),
        }
    }

    /// Writes `node`'s members of chunk `key` to `out`; false when there are
    /// none, in which case `out` is unspecified.
    fn assign(&mut self, node: &Node, key: u16, depth: usize, out: &mut Words) -> Result<bool> {
        match node {
            Node::Term(i) => self.term(*i, key, Op::Assign, out),
            Node::Or(children) => {
                let mut any = false;
                for child in children {
                    any |= if any {
                        self.or_into(child, key, depth, out)?
                    } else {
                        self.assign(child, key, depth, out)?
                    };
                }
                Ok(any)
            }
            Node::And(children) => {
                let Some((first, rest)) = children.split_first() else {
                    return Ok(false);
                };
                if !self.assign(first, key, depth, out)? {
                    return Ok(false);
                }
                for child in rest {
                    let present = if let Node::Term(i) = child {
                        self.term(*i, key, Op::And, out)?
                    } else {
                        let mut other = self.lend(depth);
                        let present = self.assign(child, key, depth + 1, &mut other)?;
                        if present {
                            kernels::and_words(out, &other);
                        }
                        self.scratch[depth] = Some(other);
                        present
                    };
                    if !present {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }

    fn or_into(&mut self, node: &Node, key: u16, depth: usize, out: &mut Words) -> Result<bool> {
        match node {
            Node::Term(i) => self.term(*i, key, Op::Or, out),
            Node::Or(children) => {
                let mut any = false;
                for child in children {
                    any |= self.or_into(child, key, depth, out)?;
                }
                Ok(any)
            }
            Node::And(_) => {
                let mut other = self.lend(depth);
                let present = self.assign(node, key, depth + 1, &mut other)?;
                if present {
                    kernels::or_words(out, &other);
                }
                self.scratch[depth] = Some(other);
                Ok(present)
            }
        }
    }

    fn lend(&mut self, depth: usize) -> Box<Words> {
        if self.scratch.len() <= depth {
            self.scratch.resize_with(depth + 1, || None);
        }
        self.scratch[depth]
            .take()
            .unwrap_or_else(|| Box::new([0; WORDS]))
    }
}

/// Evaluates `node` over `streams` one chunk at a time, in ascending chunk
/// order, visiting only chunks where the tree has members. `visit` receives
/// the chunk key, its membership and the member count; an ordinal is
/// `key << 16 | bit index`.
pub fn for_each_chunk(
    node: &Node,
    streams: &[Option<Ordinals<'_>>],
    mut visit: impl FnMut(u16, &Words, u32) -> Result<()>,
) -> Result<()> {
    let mut keys = BTreeSet::new();
    for stream in streams.iter().flatten() {
        stream.keys(&mut keys);
    }
    let mut evaluator = Evaluator {
        streams,
        positions: vec![0; streams.len()],
        scratch: Vec::new(),
    };
    let mut out = Box::new([0u64; WORDS]);
    for key in keys {
        if evaluator.assign(node, key, 0, &mut out)? {
            let members = count(&out);
            if members != 0 {
                visit(key, &out, members)?;
            }
        }
    }
    Ok(())
}

/// Checks a stream against its term: `count` ordinals, strictly ascending,
/// below `documents`, in canonical containers, with bounds when `bounded`.
pub fn validate(bytes: &[u8], count: u32, documents: u32, bounded: bool) -> Result<()> {
    let stream = Ordinals::open(bytes, bytes.len() as u64, bounded)?;
    if stream.count() != count {
        return Err(Error::Corrupt("ordinal count differs from the term"));
    }
    if let Body::Chunked {
        directory,
        chunks_at,
        ..
    } = &stream.body
    {
        let mut expected = 0u64;
        let mut previous = None;
        for entry in directory.chunks_exact(ENTRY) {
            let key = entry_key(entry);
            let (cardinality, at, size, bitmap) = entry_chunk(entry);
            if previous.is_some_and(|p| p >= key) || at != expected {
                return Err(Error::Corrupt("ordinal directory order"));
            }
            previous = Some(key);
            let total = size + nibbles_len(cardinality, bounded);
            let chunk = bytes.fetch(chunks_at + at, total)?;
            let body = &chunk[..size];
            if !bitmap {
                let mut last = None;
                for low in body.chunks_exact(2) {
                    let low = u16::from_le_bytes([low[0], low[1]]);
                    if last.is_some_and(|last| last >= low) {
                        return Err(Error::Corrupt("ordinal array order"));
                    }
                    last = Some(low);
                }
            } else {
                let set: usize = body
                    .chunks_exact(8)
                    .map(|w| u64::from_le_bytes(w.try_into().unwrap()).count_ones() as usize)
                    .sum();
                if set != cardinality {
                    return Err(Error::Corrupt("ordinal bitmap cardinality"));
                }
            }
            expected += total as u64;
        }
        if chunks_at + expected != bytes.len() as u64 {
            return Err(Error::Corrupt("ordinal stream length"));
        }
    }
    let all = stream.to_vec()?;
    if all.len() != count as usize || all.last().is_some_and(|last| *last >= documents) {
        return Err(Error::Corrupt("ordinal beyond the segment"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn cursor_matches(ordinals: &[u32], scored: bool) {
        let scores: Vec<(u8, u32)> = ordinals.iter().map(|o| ((o % 7) as u8, o + 10)).collect();
        let bytes = if scored {
            encode_scored(ordinals, &scores)
        } else {
            encode(ordinals)
        };
        let stream = Ordinals::open(&bytes[..], bytes.len() as u64, scored).unwrap();
        for (rank, ordinal) in ordinals.iter().enumerate() {
            assert_eq!(stream.rank(*ordinal).unwrap(), Some(rank as u32));
        }
        assert_eq!(
            stream
                .rank(ordinals.last().map_or(0, |last| last + 1))
                .unwrap(),
            None
        );
        let mut cursor = Ordinals::open(&bytes[..], bytes.len() as u64, scored)
            .unwrap()
            .cursor()
            .unwrap();
        for (rank, ordinal) in ordinals.iter().enumerate() {
            assert_eq!(cursor.current(), Some(*ordinal));
            assert_eq!(cursor.rank(), rank as u32);
            assert_eq!(
                cursor.bucket(),
                scored.then_some(scores[rank].0),
                "bucket of {ordinal}"
            );
            cursor.advance().unwrap();
        }
        assert_eq!(stream.is_scored(), scored);
        if scored && stream.list().is_some() {
            assert_eq!(
                stream.list_buckets(),
                scores.iter().map(|s| s.0).collect::<Vec<_>>()
            );
        }
        assert_eq!(cursor.current(), None);
        assert_eq!(cursor.rank(), ordinals.len() as u32);
        cursor.advance().unwrap();
        assert_eq!(cursor.current(), None);
        // Seeking to every member, every gap and past the end from a fresh
        // cursor and from the previous position.
        let targets: Vec<u32> = ordinals
            .iter()
            .flat_map(|o| [o.saturating_sub(1), *o, o + 1])
            .collect();
        let mut walking = Ordinals::open(&bytes[..], bytes.len() as u64, scored)
            .unwrap()
            .cursor()
            .unwrap();
        for target in targets {
            let expected = ordinals.partition_point(|o| *o < target);
            let mut fresh = Ordinals::open(&bytes[..], bytes.len() as u64, scored)
                .unwrap()
                .cursor()
                .unwrap();
            fresh.seek(target).unwrap();
            assert_eq!(
                fresh.current(),
                ordinals.get(expected).copied(),
                "seek {target}"
            );
            assert_eq!(fresh.rank(), expected as u32, "rank after seek {target}");
            if walking.current().is_some_and(|c| c < target) || walking.current().is_none() {
                walking.seek(target).unwrap();
                assert_eq!(
                    walking.current(),
                    ordinals.get(expected).copied(),
                    "walk seek {target}"
                );
                assert_eq!(walking.rank(), expected as u32);
                if scored && expected < ordinals.len() {
                    assert_eq!(walking.bucket(), Some(scores[expected].0));
                    assert_eq!(fresh.bucket(), Some(scores[expected].0));
                }
            }
        }
    }

    #[test]
    fn cursor_walks_lists_arrays_and_bitmaps() {
        cursor_matches(&[], false);
        cursor_matches(&[5], true);
        cursor_matches(&(0..40).map(|i| i * 3).collect::<Vec<_>>(), true);
        // Two chunks: a sparse array and a bitmap, with a gap chunk between.
        let mut dense: Vec<u32> = (0..200).map(|i| i * 300).collect();
        dense.extend(3 * CHUNK..3 * CHUNK + 5000);
        dense.extend((3 * CHUNK + 6000..3 * CHUNK + 6100).step_by(7));
        cursor_matches(&dense, true);
        cursor_matches(&dense, false);
        cursor_matches(&sample(200_000, 3, 9), true);
    }

    fn sample(documents: u32, step: u32, seed: u32) -> Vec<u32> {
        let mut state = seed | 1;
        (0..documents)
            .filter(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state.is_multiple_of(step)
            })
            .collect()
    }

    fn reference(node: &Node, lists: &[Vec<u32>]) -> BTreeSet<u32> {
        match node {
            Node::Term(i) => lists[*i].iter().copied().collect(),
            Node::Or(children) => children
                .iter()
                .flat_map(|child| reference(child, lists))
                .collect(),
            Node::And(children) => {
                let mut sets = children.iter().map(|child| reference(child, lists));
                let first = sets.next().unwrap_or_default();
                sets.fold(first, |all, next| &all & &next)
            }
        }
    }

    fn evaluate(node: &Node, lists: &[Vec<u32>]) -> Vec<u32> {
        let encoded: Vec<Vec<u8>> = lists.iter().map(|list| encode(list)).collect();
        let streams: Vec<Option<Ordinals<'_>>> = encoded
            .iter()
            .zip(lists)
            .map(|(bytes, list)| (!list.is_empty()).then(|| Ordinals::parse(bytes).unwrap()))
            .collect();
        let mut out = Vec::new();
        let mut last = None;
        for_each_chunk(node, &streams, |key, words, members_in_chunk| {
            assert!(last.is_none_or(|last| last < key));
            last = Some(key);
            let before = out.len();
            members(words, u32::from(key) << 16, &mut out);
            assert_eq!(out.len() - before, members_in_chunk as usize);
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn streams_round_trip_in_every_container() {
        let documents = 3 * CHUNK + 1234;
        for list in [
            vec![],
            vec![0],
            vec![documents - 1],
            sample(documents, 5000, 3),
            sample(documents, 97, 7),
            sample(documents, 9, 13),
            sample(documents, 2, 11),
            (0..documents).collect(),
        ] {
            let bytes = encode(&list);
            validate(&bytes, list.len() as u32, documents, false).unwrap();
            let stream = Ordinals::parse(&bytes).unwrap();
            assert_eq!(stream.count() as usize, list.len());
            assert_eq!(stream.to_vec().unwrap(), list);
            // A stride keeps the dense lists quick while crossing every chunk.
            for index in (0..list.len()).step_by(list.len() / 997 + 1) {
                assert_eq!(stream.select(index as u32).unwrap(), list[index]);
            }
            if let Some(last) = list.len().checked_sub(1) {
                assert_eq!(stream.select(last as u32).unwrap(), list[last]);
            }
            assert!(stream.select(list.len() as u32).is_err());
        }
        // A rare term costs a few bytes.
        assert!(encode(&[documents - 1]).len() <= 5);
    }

    #[test]
    fn boolean_trees_match_set_operations() {
        let documents = 3 * CHUNK + 1234;
        let lists = vec![
            sample(documents, 97, 7),
            sample(documents, 2, 11),
            sample(documents, 9, 13),
            vec![5, CHUNK + 1, 2 * CHUNK + 7],
            vec![],
        ];
        let term = Node::Term;
        for node in [
            term(0),
            term(4),
            Node::Or(vec![term(0), term(1), term(2), term(3), term(4)]),
            Node::And(vec![term(1), term(2)]),
            Node::And(vec![term(1), term(3)]),
            Node::And(vec![term(3), term(1)]),
            Node::And(vec![term(1), term(4)]),
            Node::And(vec![term(1), Node::Or(vec![term(0), term(3)])]),
            Node::Or(vec![term(3), Node::And(vec![term(1), term(2)])]),
            Node::And(vec![
                Node::Or(vec![term(0), term(3)]),
                Node::Or(vec![term(2), Node::And(vec![term(1), term(0)])]),
            ]),
        ] {
            let expected: Vec<u32> = reference(&node, &lists).into_iter().collect();
            assert_eq!(evaluate(&node, &lists), expected, "{node:?}");
        }
    }

    #[test]
    fn malformed_streams_are_errors() {
        let documents = 2 * CHUNK;
        let list = sample(documents, 2, 3);
        let bytes = encode(&list);
        assert!(
            validate(
                &bytes[..bytes.len() - 9],
                list.len() as u32,
                documents,
                false
            )
            .is_err()
        );
        assert!(validate(&bytes, list.len() as u32 - 1, documents, false).is_err());
        assert!(validate(&bytes, list.len() as u32, CHUNK, false).is_err());
        // Sparse enough for array chunks, where order is checked.
        let sparse = sample(documents, 200, 3);
        assert!(sparse.len() > LIST_MAX && sparse.len() / 2 < ARRAY_MAX);
        let mut swapped = encode(&sparse);
        let last = swapped.len() - 1;
        // Exchange the final two array entries.
        swapped.swap(last - 1, last - 3);
        swapped.swap(last, last - 2);
        assert!(validate(&swapped, sparse.len() as u32, documents, false).is_err());
        assert!(Ordinals::parse(&[]).is_err());
        assert!(Ordinals::parse(&[200, 1, 0]).is_err());
        for cut in 0..bytes.len().min(64) {
            // Truncation anywhere is an error or a shorter stream, never a panic.
            if let Ok(stream) = Ordinals::parse(&bytes[..cut]) {
                let _ = stream.to_vec();
            }
        }
    }

    fn tree(terms: usize) -> impl Strategy<Value = Node> {
        let leaf = (0..terms).prop_map(Node::Term);
        leaf.prop_recursive(3, 12, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 1..4).prop_map(Node::Or),
                prop::collection::vec(inner, 1..4).prop_map(Node::And),
            ]
        })
    }

    proptest! {
        #[test]
        fn random_trees_match_set_operations(
            node in tree(4),
            densities in prop::collection::vec(
                prop_oneof![Just(2u32), Just(7), Just(300), Just(40_000)], 4),
            seed in 1u32..1000,
        ) {
            let documents = 2 * CHUNK + 99;
            let lists: Vec<Vec<u32>> = densities
                .iter()
                .enumerate()
                .map(|(i, step)| sample(documents, *step, seed + i as u32))
                .collect();
            let expected: Vec<u32> = reference(&node, &lists).into_iter().collect();
            prop_assert_eq!(evaluate(&node, &lists), expected);
        }
    }
    #[test]
    fn bitmap_chunk_words_are_read_in_place() {
        // A bitmap chunk after an array chunk, so the bitmap's bytes start
        // at an odd offset in the body and are read unaligned.
        let mut ordinals: Vec<u32> = (0..7).map(|i| i * 11).collect();
        ordinals.extend((CHUNK..2 * CHUNK).filter(|o| o % 3 == 0 || o % 1000 == 1));
        let scores: Vec<(u8, u32)> = ordinals.iter().map(|o| ((o % 5) as u8, 10 + o)).collect();
        let bytes = encode_scored(&ordinals, &scores);
        let stream = Ordinals::open(&bytes[..], bytes.len() as u64, true).unwrap();
        assert_eq!(stream.chunk_count(), 2);
        let array = stream.chunk(0).unwrap();
        assert!(!array.is_bitmap());
        let mut lows = Vec::new();
        array.members(&mut lows);
        assert_eq!(lows, (0..7u16).map(|i| i * 11).collect::<Vec<_>>());
        let chunk = stream.chunk(1).unwrap();
        assert!(chunk.is_bitmap());
        let mut copied = Box::new([0u64; WORDS]);
        chunk.words(&mut copied);
        for (i, word) in copied.iter().enumerate() {
            assert_eq!(chunk.word(i), *word, "word {i}");
        }
        let expected: u32 = copied[3..700].iter().map(|w| w.count_ones()).sum();
        assert_eq!(chunk.count_words(3, 700), expected);
        assert_eq!(chunk.count_words(5, 5), 0);
        assert_eq!(chunk.count_words(0, WORDS), chunk.cardinality);
        let mut out = Box::new([0xffu64; WORDS]);
        chunk.and_into(&mut out);
        assert_eq!(&out[..], &copied.map(|w| w & 0xff)[..]);
        chunk.or_into(&mut out);
        assert_eq!(&out[..], &copied[..]);
        // Ranks through the words agree with the stream's.
        for ordinal in &ordinals[7..] {
            let low = (ordinal & 0xffff) as u16;
            assert_eq!(
                chunk.rank(low).map(|r| r + chunk.before),
                stream.rank(*ordinal).unwrap()
            );
        }
        assert_eq!(
            chunk.rank(1),
            None,
            "65537 is neither a multiple of 3 nor 1 mod 1000"
        );
    }

    #[test]
    fn bounded_streams_carry_a_bound_per_chunk_and_reject_corrupt_ones() {
        for (name, ordinals) in [
            ("list", (0..40u32).map(|i| i * 3000).collect::<Vec<_>>()),
            ("chunked", sample(300_000, 3, 7)),
        ] {
            let scores: Vec<(u8, u32)> = ordinals
                .iter()
                .map(|o| ((o % 5) as u8, 10 + o % 300))
                .collect();
            let bytes = encode_scored(&ordinals, &scores);
            let stream = Ordinals::open(&bytes[..], bytes.len() as u64, true).unwrap();
            assert_eq!(stream.to_vec().unwrap(), ordinals, "{name}");
            validate(&bytes, ordinals.len() as u32, 300_000, true).unwrap();
            let expected_chunks = if stream.list().is_some() {
                1
            } else {
                stream.chunk_count()
            };
            assert_eq!(stream.bounds().len(), expected_chunks, "{name}");
            // Every member's bucket and length are covered by its chunk's bound.
            for (o, (bucket, len)) in ordinals.iter().zip(&scores) {
                let bound = match stream.list() {
                    Some(_) => stream.chunk_bound(0).unwrap(),
                    None => {
                        let i = (0..stream.chunk_count())
                            .find(|i| stream.chunk_key(*i) == (o >> 16) as u16)
                            .unwrap();
                        stream.chunk_bound(i).unwrap()
                    }
                };
                assert!(bound.min_len[usize::from(*bucket)] <= *len, "{name} {o}");
                assert!(
                    bound.subs[((o & 0xffff) / SUB) as usize] > *bucket,
                    "{name} {o}"
                );
            }
            // The same members without bounds are a different, shorter stream.
            assert!(encode(&ordinals).len() < bytes.len());
            assert!(
                Ordinals::open(&bytes[..], bytes.len() as u64, false).is_err()
                    || validate(&bytes, ordinals.len() as u32, 300_000, false).is_err(),
                "{name}"
            );
            // A sub-block byte past the bucket range is rejected.
            let mut corrupt = bytes.clone();
            let at = if stream.list().is_some() {
                2
            } else {
                3 + stream.chunk_count() * ENTRY
            };
            corrupt[at + 1] = 0xff;
            assert!(
                Ordinals::open(&corrupt[..], corrupt.len() as u64, true).is_err()
                    || validate(&corrupt, ordinals.len() as u32, 300_000, true).is_err(),
                "{name}"
            );
        }
    }
}
