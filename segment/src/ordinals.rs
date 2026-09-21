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

use std::collections::BTreeSet;

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
/// Bytes that hold a stream's count and chunk count.
const HEAD: usize = 12;

/// One chunk's membership.
pub type Words = [u64; WORDS];

/// Encodes strictly ascending ordinals.
pub fn encode(ordinals: &[u32]) -> Vec<u8> {
    debug_assert!(ordinals.windows(2).all(|pair| pair[0] < pair[1]));
    let mut out = Vec::new();
    varint::put(&mut out, ordinals.len() as u64);
    if ordinals.len() <= LIST_MAX {
        let mut previous = None;
        for ordinal in ordinals {
            varint::put(
                &mut out,
                u64::from(previous.map_or(*ordinal, |p: u32| ordinal - p - 1)),
            );
            previous = Some(*ordinal);
        }
        return out;
    }
    let mut directory = Vec::new();
    let mut body = Vec::new();
    let mut chunks = 0u64;
    for members in ordinals.chunk_by(|a, b| a >> 16 == b >> 16) {
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
        chunks += 1;
    }
    varint::put(&mut out, chunks);
    out.extend_from_slice(&directory);
    out.extend_from_slice(&body);
    out
}

/// Byte ranges of one encoded stream, fetched on demand.
pub trait Fetch<'a> {
    /// `len` bytes at `offset` from the start of the stream.
    fn fetch(&self, offset: u64, len: usize) -> Result<&'a [u8]>;
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
    },
}

/// An open stream. Chunk bodies are fetched and checked as they are visited.
pub struct Ordinals<'a> {
    source: Box<dyn Fetch<'a> + 'a>,
    len: u64,
    count: u32,
    body: Body<'a>,
}

fn entry_key(entry: &[u8]) -> u16 {
    u16::from_le_bytes([entry[0], entry[1]])
}

/// A directory entry's cardinality, body offset, body size and whether the
/// chunk is a bitmap.
fn entry_chunk(entry: &[u8]) -> (usize, u64, usize, bool) {
    let cardinality = usize::from(u16::from_le_bytes([entry[2], entry[3]])) + 1;
    let at = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]);
    let bitmap = at & BITMAP != 0;
    let size = if bitmap { WORDS * 8 } else { cardinality * 2 };
    (cardinality, u64::from(at & !BITMAP), size, bitmap)
}

impl<'a> Ordinals<'a> {
    /// Opens the stream of `len` bytes behind `source`.
    pub fn open(source: impl Fetch<'a> + 'a, len: u64) -> Result<Self> {
        let head = source.fetch(0, len.min(HEAD as u64) as usize)?;
        let mut at = 0;
        let count = varint::get_u32(head, &mut at)?;
        let body = if count as usize <= LIST_MAX {
            let bytes = source.fetch(0, len as usize)?;
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
            if at as u64 != len {
                return Err(Error::Corrupt("ordinal list length"));
            }
            Body::List(list)
        } else {
            let chunks = u64::from(varint::get_u32(head, &mut at)?);
            let chunks_at = at as u64 + chunks * ENTRY as u64;
            if chunks == 0 || chunks > u64::from(CHUNK) || chunks_at > len {
                return Err(Error::Corrupt("ordinal directory"));
            }
            Body::Chunked {
                directory: source.fetch(at as u64, (chunks as usize) * ENTRY)?,
                chunks_at,
            }
        };
        Ok(Self {
            source: Box::new(source),
            len,
            count,
            body,
        })
    }

    /// Opens a stream held in memory.
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        Self::open(bytes, bytes.len() as u64)
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
                let bytes = self.source.fetch(start, size)?;
                if !bitmap {
                    let lows = bytes
                        .chunks_exact(2)
                        .map(|low| usize::from(u16::from_le_bytes([low[0], low[1]])));
                    apply_lows(lows, op, out);
                } else {
                    let words = bytes
                        .chunks_exact(8)
                        .map(|word| u64::from_le_bytes(word.try_into().unwrap()));
                    match op {
                        Op::Assign => out.iter_mut().zip(words).for_each(|(o, w)| *o = w),
                        Op::Or => out.iter_mut().zip(words).for_each(|(o, w)| *o |= w),
                        Op::And => out.iter_mut().zip(words).for_each(|(o, w)| *o &= w),
                    }
                }
                Ok(true)
            }
        }
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
                            out.iter_mut().zip(other.iter()).for_each(|(o, w)| *o &= w);
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
                    out.iter_mut().zip(other.iter()).for_each(|(o, w)| *o |= w);
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
/// the chunk key and its membership; an ordinal is `key << 16 | bit index`.
pub fn for_each_chunk(
    node: &Node,
    streams: &[Option<Ordinals<'_>>],
    mut visit: impl FnMut(u16, &Words) -> Result<()>,
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
        if evaluator.assign(node, key, 0, &mut out)? && out.iter().any(|word| *word != 0) {
            visit(key, &out)?;
        }
    }
    Ok(())
}

/// Checks a stream against its term: `count` ordinals, strictly ascending,
/// below `documents`, in canonical containers.
pub fn validate(bytes: &[u8], count: u32, documents: u32) -> Result<()> {
    let stream = Ordinals::parse(bytes)?;
    if stream.count() != count {
        return Err(Error::Corrupt("ordinal count differs from the term"));
    }
    if let Body::Chunked {
        directory,
        chunks_at,
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
            let chunk = bytes.fetch(chunks_at + at, size)?;
            if !bitmap {
                let mut last = None;
                for low in chunk.chunks_exact(2) {
                    let low = u16::from_le_bytes([low[0], low[1]]);
                    if last.is_some_and(|last| last >= low) {
                        return Err(Error::Corrupt("ordinal array order"));
                    }
                    last = Some(low);
                }
            } else {
                let set: usize = chunk
                    .chunks_exact(8)
                    .map(|w| u64::from_le_bytes(w.try_into().unwrap()).count_ones() as usize)
                    .sum();
                if set != cardinality {
                    return Err(Error::Corrupt("ordinal bitmap cardinality"));
                }
            }
            expected += size as u64;
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
        for_each_chunk(node, &streams, |key, words| {
            assert!(last.is_none_or(|last| last < key));
            last = Some(key);
            members(words, u32::from(key) << 16, &mut out);
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
            validate(&bytes, list.len() as u32, documents).unwrap();
            let stream = Ordinals::parse(&bytes).unwrap();
            assert_eq!(stream.count() as usize, list.len());
            assert_eq!(stream.to_vec().unwrap(), list);
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
        assert!(validate(&bytes[..bytes.len() - 9], list.len() as u32, documents).is_err());
        assert!(validate(&bytes, list.len() as u32 - 1, documents).is_err());
        assert!(validate(&bytes, list.len() as u32, CHUNK).is_err());
        // Sparse enough for array chunks, where order is checked.
        let sparse = sample(documents, 200, 3);
        assert!(sparse.len() > LIST_MAX && sparse.len() / 2 < ARRAY_MAX);
        let mut swapped = encode(&sparse);
        let last = swapped.len() - 1;
        // Exchange the final two array entries.
        swapped.swap(last - 1, last - 3);
        swapped.swap(last, last - 2);
        assert!(validate(&swapped, sparse.len() as u32, documents).is_err());
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
}
