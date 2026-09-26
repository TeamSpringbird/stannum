// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Streaming set operations over exact, nonempty heap-page bitmaps.
//!
//! Only the current page of each input is retained. Row cursors participate
//! through [`Rows`]; a term's ordinal stream produces pages directly through
//! [`crate::docs::PageCursor`]. Padding bits are never part of a set.

use crate::{Result, Tid, set, tid::MAX_OFFSET};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Offsets([u64; 5]);

impl Offsets {
    pub fn insert(&mut self, offset: u16) {
        assert!((1..=MAX_OFFSET).contains(&offset));
        let bit = usize::from(offset - 1);
        self.0[bit / 64] |= 1 << (bit % 64);
    }

    pub fn from_bitmap(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 37 || bytes[36] & 0xf8 != 0 {
            return Err(crate::Error::Corrupt("tuple bitmap offset"));
        }
        let mut words = [0; 5];
        for (word, chunk) in words.iter_mut().zip(bytes.chunks(8)) {
            let mut raw = [0; 8];
            raw[..chunk.len()].copy_from_slice(chunk);
            *word = u64::from_le_bytes(raw);
        }
        Ok(Self(words))
    }

    /// Removes and returns the lowest offset, without expanding the mask.
    pub fn pop_first(&mut self) -> Option<u16> {
        for (i, word) in self.0.iter_mut().enumerate() {
            if *word != 0 {
                let bit = word.trailing_zeros() as usize;
                *word &= *word - 1;
                return Some((i * 64 + bit + 1) as u16);
            }
        }
        None
    }

    pub fn count(self) -> u32 {
        self.0.iter().map(|word| word.count_ones()).sum()
    }

    pub fn is_empty(self) -> bool {
        self.0 == [0; 5]
    }

    pub fn union(&mut self, other: Self) {
        for (a, b) in self.0.iter_mut().zip(other.0) {
            *a |= b;
        }
    }

    pub fn intersect(&mut self, other: Self) {
        for (a, b) in self.0.iter_mut().zip(other.0) {
            *a &= b;
        }
    }

    pub fn subtract(&mut self, other: Self) {
        for (a, b) in self.0.iter_mut().zip(other.0) {
            *a &= !b;
        }
    }

    pub fn iter(self) -> impl Iterator<Item = u16> {
        self.0.into_iter().enumerate().flat_map(|(i, mut word)| {
            std::iter::from_fn(move || {
                if word == 0 {
                    return None;
                }
                let bit = word.trailing_zeros() as usize;
                word &= word - 1;
                Some((i * 64 + bit + 1) as u16)
            })
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    pub block: u32,
    pub offsets: Offsets,
}

pub trait Cursor {
    fn current(&self) -> Option<Page>;
    fn advance(&mut self) -> Result<()>;
    fn seek(&mut self, block: u32) -> Result<()> {
        while self.current().is_some_and(|page| page.block < block) {
            self.advance()?;
        }
        Ok(())
    }
}

impl<C: Cursor + ?Sized> Cursor for Box<C> {
    fn current(&self) -> Option<Page> {
        (**self).current()
    }
    fn advance(&mut self) -> Result<()> {
        (**self).advance()
    }
    fn seek(&mut self, block: u32) -> Result<()> {
        (**self).seek(block)
    }
}

/// Adapts a scalar cursor without buffering the whole result.
pub struct Rows<C> {
    rows: C,
    current: Option<Page>,
}

impl<C: set::Cursor> Rows<C> {
    pub fn new(rows: C) -> Result<Self> {
        let mut this = Self {
            rows,
            current: None,
        };
        this.advance()?;
        Ok(this)
    }
}

impl<C: set::Cursor> Cursor for Rows<C> {
    fn current(&self) -> Option<Page> {
        self.current
    }
    fn advance(&mut self) -> Result<()> {
        self.current = None;
        if let Some(first) = self.rows.current() {
            let mut offsets = Offsets::default();
            while let Some(tid) = self.rows.current().filter(|tid| tid.block == first.block) {
                offsets.insert(tid.offset);
                self.rows.advance()?;
            }
            self.current = Some(Page {
                block: first.block,
                offsets,
            });
        }
        Ok(())
    }
    fn seek(&mut self, block: u32) -> Result<()> {
        if self.current.is_some_and(|page| page.block < block) {
            self.rows.seek(Tid { block, offset: 1 })?;
            self.advance()?;
        }
        Ok(())
    }
}

pub struct Union<C> {
    inputs: Vec<C>,
    current: Option<Page>,
}

impl<C: Cursor> Union<C> {
    pub fn new(inputs: Vec<C>) -> Self {
        let mut this = Self {
            inputs,
            current: None,
        };
        this.select();
        this
    }
    fn select(&mut self) {
        self.current = self
            .inputs
            .iter()
            .filter_map(Cursor::current)
            .map(|p| p.block)
            .min()
            .map(|block| {
                let mut offsets = Offsets::default();
                for page in self
                    .inputs
                    .iter()
                    .filter_map(Cursor::current)
                    .filter(|p| p.block == block)
                {
                    offsets.union(page.offsets);
                }
                Page { block, offsets }
            });
    }
}
impl<C: Cursor> Cursor for Union<C> {
    fn current(&self) -> Option<Page> {
        self.current
    }
    fn advance(&mut self) -> Result<()> {
        if let Some(page) = self.current {
            for input in &mut self.inputs {
                if input.current().is_some_and(|p| p.block == page.block) {
                    input.advance()?;
                }
            }
            self.select();
        }
        Ok(())
    }
    fn seek(&mut self, block: u32) -> Result<()> {
        for input in &mut self.inputs {
            input.seek(block)?;
        }
        self.select();
        Ok(())
    }
}

pub struct Intersection<C> {
    inputs: Vec<C>,
    current: Option<Page>,
}
impl<C: Cursor> Intersection<C> {
    pub fn new(inputs: Vec<C>) -> Result<Self> {
        let mut this = Self {
            inputs,
            current: None,
        };
        this.align()?;
        Ok(this)
    }
    fn align(&mut self) -> Result<()> {
        self.current = None;
        'next: loop {
            let Some(mut page) = self.inputs.first().and_then(Cursor::current) else {
                return Ok(());
            };
            for input in &mut self.inputs[1..] {
                input.seek(page.block)?;
                let Some(other) = input.current() else {
                    return Ok(());
                };
                if other.block > page.block {
                    self.inputs[0].seek(other.block)?;
                    continue 'next;
                }
                page.offsets.intersect(other.offsets);
            }
            if !page.offsets.is_empty() {
                self.current = Some(page);
                return Ok(());
            }
            self.inputs[0].advance()?;
        }
    }
}
impl<C: Cursor> Cursor for Intersection<C> {
    fn current(&self) -> Option<Page> {
        self.current
    }
    fn advance(&mut self) -> Result<()> {
        if self.current.is_some() {
            self.inputs[0].advance()?;
            self.align()?;
        }
        Ok(())
    }
    fn seek(&mut self, block: u32) -> Result<()> {
        if self.current.is_some_and(|p| p.block < block) {
            self.inputs[0].seek(block)?;
            self.align()?;
        }
        Ok(())
    }
}

pub struct Difference<L, R> {
    left: L,
    right: R,
    current: Option<Page>,
}
impl<L: Cursor, R: Cursor> Difference<L, R> {
    pub fn new(left: L, right: R) -> Result<Self> {
        let mut this = Self {
            left,
            right,
            current: None,
        };
        this.align()?;
        Ok(this)
    }
    fn align(&mut self) -> Result<()> {
        self.current = None;
        while let Some(mut page) = self.left.current() {
            self.right.seek(page.block)?;
            if let Some(other) = self.right.current().filter(|p| p.block == page.block) {
                page.offsets.subtract(other.offsets);
            }
            if !page.offsets.is_empty() {
                self.current = Some(page);
                break;
            }
            self.left.advance()?;
        }
        Ok(())
    }
}
impl<L: Cursor, R: Cursor> Cursor for Difference<L, R> {
    fn current(&self) -> Option<Page> {
        self.current
    }
    fn advance(&mut self) -> Result<()> {
        if self.current.is_some() {
            self.left.advance()?;
            self.align()?;
        }
        Ok(())
    }
    fn seek(&mut self, block: u32) -> Result<()> {
        if self.current.is_some_and(|p| p.block < block) {
            self.left.seek(block)?;
            self.align()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::{Segment, SegmentBuilder};
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    /// A segment whose documents are the union of the sets, each set a term.
    fn segment_of(sets: &[&BTreeSet<Tid>]) -> Vec<u8> {
        let mut builder = SegmentBuilder::default();
        let all: BTreeSet<Tid> = sets.iter().flat_map(|set| set.iter().copied()).collect();
        for tid in all {
            let tokens: Vec<(String, u32)> = sets
                .iter()
                .enumerate()
                .filter(|(_, set)| set.contains(&tid))
                .map(|(i, _)| (format!("t{i}"), i as u32 + 1))
                .collect();
            builder
                .add_document(tid, tokens.iter().map(|(t, p)| (t.as_str(), *p)))
                .unwrap();
        }
        builder.finish()
    }
    fn pages<'a>(segment: &'a Segment<'a>, term: &str) -> Box<dyn Cursor + 'a> {
        match segment.term(term).unwrap() {
            Some(term) => Box::new(term.pages().unwrap()),
            None => Box::new(Rows::new(set::Empty).unwrap()),
        }
    }
    fn collect(mut pages: impl Cursor) -> Vec<Tid> {
        let mut out = Vec::new();
        while let Some(page) = pages.current() {
            assert_eq!(page.offsets.count() as usize, page.offsets.iter().count());
            let mut offsets = page.offsets;
            while let Some(offset) = offsets.pop_first() {
                out.push(Tid {
                    block: page.block,
                    offset,
                });
            }
            pages.advance().unwrap();
        }
        pages.advance().unwrap();
        assert!(pages.current().is_none());
        out
    }
    fn locations() -> impl Strategy<Value = BTreeSet<Tid>> {
        prop::collection::btree_set(
            (prop_oneof![0u32..8, 0u32..800], 1u16..=MAX_OFFSET)
                .prop_map(|(block, offset)| Tid { block, offset }),
            0..1500,
        )
    }
    proptest! {
        #[test]
        fn page_algebra_matches_independent_sets(a in locations(), b in locations(), target in 0u32..1000) {
            let bytes = segment_of(&[&a, &b]);
            let segment = Segment::parse(&bytes).unwrap();
            let mut cursor = pages(&segment, "t0");
            cursor.seek(target).unwrap();
            prop_assert_eq!(collect(cursor), a.iter().filter(|t| t.block >= target).copied().collect::<Vec<_>>());
            let mut union = Union::new(vec![pages(&segment, "t0"), pages(&segment, "t1")]);
            union.seek(target).unwrap();
            prop_assert_eq!(collect(union), a.union(&b).filter(|t| t.block >= target).copied().collect::<Vec<_>>());
            let mut intersection = Intersection::new(vec![pages(&segment, "t0"), pages(&segment, "t1")]).unwrap();
            intersection.seek(target).unwrap();
            prop_assert_eq!(collect(intersection), a.intersection(&b).filter(|t| t.block >= target).copied().collect::<Vec<_>>());
            let mut difference = Difference::new(pages(&segment, "t0"), pages(&segment, "t1")).unwrap();
            difference.seek(target).unwrap();
            prop_assert_eq!(collect(difference), a.difference(&b).filter(|t| t.block >= target).copied().collect::<Vec<_>>());
            // Rows over a TID cursor agree with the term's own pages.
            if let Some(term) = segment.term("t0").unwrap() {
                let rows = Rows::new(term.cursor().unwrap()).unwrap();
                prop_assert_eq!(collect(rows), a.iter().copied().collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn dense_pages_seek_across_chunks_and_preserve_boundary_offsets() {
        // Full blocks either side of a 65,536-document chunk boundary.
        let tids: BTreeSet<_> = (0..226)
            .chain([300, 500])
            .flat_map(|block| (1..=MAX_OFFSET).map(move |offset| Tid { block, offset }))
            .collect();
        assert!(tids.len() > crate::ordinals::CHUNK as usize);
        let bytes = segment_of(&[&tids]);
        let segment = Segment::parse(&bytes).unwrap();
        let term = segment.term("t0").unwrap().unwrap();
        assert!(term.prefers_pages().unwrap());
        assert_eq!(
            collect(term.pages().unwrap()),
            tids.iter().copied().collect::<Vec<_>>()
        );
        for target in [
            0,
            1,
            2,
            224,
            225,
            226,
            227,
            300,
            301,
            499,
            500,
            501,
            u32::MAX,
        ] {
            let mut cursor = term.pages().unwrap();
            cursor.seek(target).unwrap();
            assert_eq!(
                collect(cursor),
                tids.iter()
                    .filter(|t| t.block >= target)
                    .copied()
                    .collect::<Vec<_>>()
            );
        }
        let mut cursor = term.pages().unwrap();
        for target in [0, 1, 224, 225, 225, 300, 500, 501] {
            cursor.seek(target).unwrap();
            assert_eq!(
                cursor.current().map(|p| p.block),
                tids.iter().find(|t| t.block >= target).map(|t| t.block)
            );
        }
    }

    #[test]
    fn rejects_bitmap_padding_and_preserves_the_highest_valid_offset() {
        let mut raw = [0; 37];
        raw[36] = 4;
        assert_eq!(
            Offsets::from_bitmap(&raw)
                .unwrap()
                .iter()
                .collect::<Vec<_>>(),
            vec![291]
        );
        raw[36] = 8;
        assert!(Offsets::from_bitmap(&raw).is_err());
    }
}
