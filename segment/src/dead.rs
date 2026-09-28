// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A segment's dead documents as a bitmap over its ordinals.
//!
//! A dead list is stored as an ordinal stream. Readers decode it once into
//! this form, which costs at most a bit per document of the segment plus a
//! pointer per 65,536-document chunk, however many of the documents are
//! dead: chunks without a dead document hold nothing. The walks and counts
//! over ordinal chunks clear a chunk's dead documents word by word, and a
//! membership test is one bit.

use crate::Result;
use crate::error::Error;
use crate::ordinals::{CHUNK, Ordinals, WORDS, Words};

/// Dead documents of one segment, by ordinal.
#[derive(Default)]
pub struct DeadDocs {
    /// Per chunk of [`CHUNK`] ordinals, its dead documents; `None` for a
    /// chunk with none. Trailing chunks without any are not listed.
    chunks: Vec<Option<Box<Words>>>,
    count: u32,
}

impl DeadDocs {
    /// Decodes the dead list `list` of a segment of `documents` documents.
    /// An ordinal at or past `documents` is corruption.
    pub fn decode(list: &[u8], documents: u32) -> Result<Self> {
        let stream = Ordinals::parse(list)?;
        let mut chunks: Vec<Option<Box<Words>>> = Vec::new();
        let mut count = 0u32;
        stream.each_chunk(|key, words| {
            let base = u32::from(key) << 16;
            let members = crate::ordinals::count(words);
            if members == 0 {
                return Ok(());
            }
            let last = WORDS - 1 - words.iter().rev().take_while(|w| **w == 0).count();
            let high = base + last as u32 * 64 + (63 - words[last].leading_zeros());
            if high >= documents {
                return Err(Error::Corrupt("dead ordinal beyond the document table"));
            }
            let i = usize::from(key);
            if chunks.len() <= i {
                chunks.resize_with(i + 1, || None);
            }
            chunks[i] = Some(Box::new(*words));
            count = count.saturating_add(members);
            Ok(())
        })?;
        chunks.shrink_to_fit();
        Ok(Self { chunks, count })
    }

    /// The number of dead documents.
    pub fn len(&self) -> u32 {
        self.count
    }

    /// Whether no document is dead.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Whether the document at `ordinal` is dead.
    pub fn contains(&self, ordinal: u32) -> bool {
        self.chunk(ordinal & !(CHUNK - 1)).is_some_and(|words| {
            let bit = (ordinal & (CHUNK - 1)) as usize;
            words[bit / 64] >> (bit % 64) & 1 == 1
        })
    }

    /// The dead documents of the chunk starting at ordinal `base`, or
    /// `None` when it has none.
    pub fn chunk(&self, base: u32) -> Option<&Words> {
        self.chunks.get((base >> 16) as usize)?.as_deref()
    }

    /// Clears the dead documents of the chunk at `base` from `words`;
    /// returns whether the chunk has any.
    pub fn clear(&self, base: u32, words: &mut Words) -> bool {
        let Some(dead) = self.chunk(base) else {
            return false;
        };
        for (word, dead) in words.iter_mut().zip(dead.iter()) {
            *word &= !dead;
        }
        true
    }

    /// Heap bytes held.
    pub fn heap_bytes(&self) -> usize {
        self.chunks.capacity() * std::mem::size_of::<Option<Box<Words>>>()
            + self.chunks.iter().flatten().count() * std::mem::size_of::<Words>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ordinals::encode;
    use proptest::prelude::*;

    #[test]
    fn empty_and_absent_chunks_hold_nothing() {
        let dead = DeadDocs::decode(&encode(&[]), 10).unwrap();
        assert!(dead.is_empty());
        assert_eq!(dead.heap_bytes(), 0);
        assert!(!dead.contains(0));
        // One dead document in the third chunk: a bitmap for that chunk only.
        let dead = DeadDocs::decode(&encode(&[2 * CHUNK + 5]), 3 * CHUNK).unwrap();
        assert_eq!(dead.len(), 1);
        assert!(dead.contains(2 * CHUNK + 5));
        assert!(!dead.contains(5) && !dead.contains(CHUNK + 5));
        assert!(dead.chunk(0).is_none() && dead.chunk(CHUNK).is_none());
        assert!(dead.heap_bytes() < std::mem::size_of::<Words>() + 64);
    }

    #[test]
    fn an_ordinal_past_the_documents_is_corrupt() {
        assert!(DeadDocs::decode(&encode(&[9]), 9).is_err());
        assert!(DeadDocs::decode(&encode(&[8]), 9).is_ok());
    }

    proptest! {
        #[test]
        fn matches_the_ordinals_it_was_decoded_from(
            set in proptest::collection::btree_set(0u32..3 * CHUNK, 0..3000),
            probes in proptest::collection::vec(0u32..3 * CHUNK, 0..200),
        ) {
            let ordinals: Vec<u32> = set.iter().copied().collect();
            let dead = DeadDocs::decode(&encode(&ordinals), 3 * CHUNK).unwrap();
            prop_assert_eq!(dead.len() as usize, ordinals.len());
            for probe in probes.iter().chain(&ordinals) {
                prop_assert_eq!(dead.contains(*probe), set.contains(probe));
            }
            for chunk in 0..3u32 {
                let base = chunk * CHUNK;
                let mut words = Box::new([u64::MAX; WORDS]);
                dead.clear(base, &mut words);
                for low in (0..CHUNK).step_by(97) {
                    let live = words[(low / 64) as usize] >> (low % 64) & 1 == 1;
                    prop_assert_eq!(live, !set.contains(&(base + low)));
                }
            }
            prop_assert!(dead.heap_bytes() <= 3 * (std::mem::size_of::<Words>() + 8));
        }
    }
}
