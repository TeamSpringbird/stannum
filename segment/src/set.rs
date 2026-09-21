// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Set operations over ordered cursors.
//!
//! Every operator is itself a [`Cursor`], so plans compose. Intersection
//! drives from its first input; callers should place the rarest input first so
//! the others are probed with `seek`, which skips groups and pages.

use crate::{Result, Tid};

pub trait Cursor {
    /// The posting the cursor is positioned on, or `None` when exhausted.
    fn current(&self) -> Option<Tid>;
    /// Moves to the next posting. A no-op when exhausted.
    fn advance(&mut self) -> Result<()>;
    /// Moves to the first posting at or after `target`.
    fn seek(&mut self, target: Tid) -> Result<()>;
}

impl<C: Cursor + ?Sized> Cursor for Box<C> {
    fn current(&self) -> Option<Tid> {
        (**self).current()
    }
    fn advance(&mut self) -> Result<()> {
        (**self).advance()
    }
    fn seek(&mut self, target: Tid) -> Result<()> {
        (**self).seek(target)
    }
}

/// An always-empty cursor, the identity for union.
#[derive(Clone, Copy, Debug, Default)]
pub struct Empty;

impl Cursor for Empty {
    fn current(&self) -> Option<Tid> {
        None
    }
    fn advance(&mut self) -> Result<()> {
        Ok(())
    }
    fn seek(&mut self, _: Tid) -> Result<()> {
        Ok(())
    }
}

/// A cursor over an in-memory sorted list, for tests and small sets.
#[derive(Clone, Debug)]
pub struct Slice<'a> {
    tids: &'a [Tid],
    index: usize,
}

impl<'a> Slice<'a> {
    /// `tids` must be strictly increasing.
    pub fn new(tids: &'a [Tid]) -> Self {
        debug_assert!(tids.windows(2).all(|pair| pair[0] < pair[1]));
        Self { tids, index: 0 }
    }
}

impl Cursor for Slice<'_> {
    fn current(&self) -> Option<Tid> {
        self.tids.get(self.index).copied()
    }
    fn advance(&mut self) -> Result<()> {
        if self.index < self.tids.len() {
            self.index += 1;
        }
        Ok(())
    }
    fn seek(&mut self, target: Tid) -> Result<()> {
        self.index += self.tids[self.index..].partition_point(|tid| *tid < target);
        Ok(())
    }
}

/// Postings present in every input.
pub struct Intersection<C> {
    cursors: Vec<C>,
    current: Option<Tid>,
}

impl<C: Cursor> Intersection<C> {
    /// An empty input list yields no postings: there is no universe to return.
    pub fn new(cursors: Vec<C>) -> Result<Self> {
        let mut this = Self {
            cursors,
            current: None,
        };
        this.align()?;
        Ok(this)
    }

    fn align(&mut self) -> Result<()> {
        let Some((lead, rest)) = self.cursors.split_first_mut() else {
            self.current = None;
            return Ok(());
        };
        'outer: loop {
            let Some(mut target) = lead.current() else {
                self.current = None;
                return Ok(());
            };
            for cursor in rest.iter_mut() {
                cursor.seek(target)?;
                match cursor.current() {
                    None => {
                        self.current = None;
                        return Ok(());
                    }
                    Some(found) if found > target => {
                        target = found;
                        lead.seek(target)?;
                        continue 'outer;
                    }
                    Some(_) => {}
                }
            }
            self.current = Some(target);
            return Ok(());
        }
    }
}

impl<C: Cursor> Cursor for Intersection<C> {
    fn current(&self) -> Option<Tid> {
        self.current
    }
    fn advance(&mut self) -> Result<()> {
        if self.current.is_none() {
            return Ok(());
        }
        self.cursors[0].advance()?;
        self.align()
    }
    fn seek(&mut self, target: Tid) -> Result<()> {
        if self.current.is_some_and(|current| current >= target) {
            return Ok(());
        }
        if let Some(lead) = self.cursors.first_mut() {
            lead.seek(target)?;
        }
        self.align()
    }
}

/// Postings present in any input.
pub struct Union<C> {
    cursors: Vec<C>,
    current: Option<Tid>,
}

impl<C: Cursor> Union<C> {
    pub fn new(cursors: Vec<C>) -> Self {
        let mut this = Self {
            cursors,
            current: None,
        };
        this.align();
        this
    }

    fn align(&mut self) {
        self.current = self.cursors.iter().filter_map(Cursor::current).min();
    }
}

impl<C: Cursor> Cursor for Union<C> {
    fn current(&self) -> Option<Tid> {
        self.current
    }
    fn advance(&mut self) -> Result<()> {
        let Some(current) = self.current else {
            return Ok(());
        };
        for cursor in &mut self.cursors {
            if cursor.current() == Some(current) {
                cursor.advance()?;
            }
        }
        self.align();
        Ok(())
    }
    fn seek(&mut self, target: Tid) -> Result<()> {
        if self.current.is_some_and(|current| current >= target) {
            return Ok(());
        }
        for cursor in &mut self.cursors {
            cursor.seek(target)?;
        }
        self.align();
        Ok(())
    }
}

/// Union with cached input heads for wider query nodes.
pub struct CachedUnion<C> {
    cursors: Vec<C>,
    // Inputs can only move through this union, so unchanged heads stay valid.
    heads: Vec<Option<Tid>>,
    current: Option<Tid>,
}

impl<C: Cursor> CachedUnion<C> {
    pub fn new(cursors: Vec<C>) -> Self {
        let heads: Vec<_> = cursors.iter().map(Cursor::current).collect();
        let current = heads.iter().copied().flatten().min();
        Self {
            cursors,
            heads,
            current,
        }
    }
}

impl<C: Cursor> Cursor for CachedUnion<C> {
    fn current(&self) -> Option<Tid> {
        self.current
    }

    fn advance(&mut self) -> Result<()> {
        let Some(current) = self.current else {
            return Ok(());
        };
        let mut next: Option<Tid> = None;
        for (cursor, head) in self.cursors.iter_mut().zip(&mut self.heads) {
            if *head == Some(current) {
                cursor.advance()?;
                *head = cursor.current();
            }
            if let Some(tid) = *head {
                next = Some(next.map_or(tid, |value| value.min(tid)));
            }
        }
        self.current = next;
        Ok(())
    }

    fn seek(&mut self, target: Tid) -> Result<()> {
        if self.current.is_none_or(|current| current >= target) {
            return Ok(());
        }
        for (cursor, head) in self.cursors.iter_mut().zip(&mut self.heads) {
            if head.is_some_and(|tid| tid < target) {
                cursor.seek(target)?;
                *head = cursor.current();
            }
        }
        self.current = self.heads.iter().copied().flatten().min();
        Ok(())
    }
}

/// Select the cursor once, keeping the narrow union hot loop unchanged.
pub fn union<'a, C: Cursor + 'a>(cursors: Vec<C>) -> Box<dyn Cursor + 'a> {
    if cursors.len() > 2 {
        Box::new(CachedUnion::new(cursors))
    } else {
        Box::new(Union::new(cursors))
    }
}

/// Postings present in at least `min` of the inputs. With `min == 1` this is a
/// union; with `min == inputs.len()` an intersection without seek-driven skipping.
pub struct AtLeast<C> {
    cursors: Vec<C>,
    min: usize,
    current: Option<Tid>,
}

impl<C: Cursor> AtLeast<C> {
    /// `min` of zero is rejected as meaningless; use a universe cursor instead.
    pub fn new(cursors: Vec<C>, min: usize) -> Result<Self> {
        let mut this = Self {
            cursors,
            min: min.max(1),
            current: None,
        };
        this.align()?;
        Ok(this)
    }

    fn align(&mut self) -> Result<()> {
        loop {
            let Some(smallest) = self.cursors.iter().filter_map(Cursor::current).min() else {
                self.current = None;
                return Ok(());
            };
            let present = self
                .cursors
                .iter()
                .filter(|cursor| cursor.current() == Some(smallest))
                .count();
            if present >= self.min {
                self.current = Some(smallest);
                return Ok(());
            }
            for cursor in &mut self.cursors {
                if cursor.current() == Some(smallest) {
                    cursor.advance()?;
                }
            }
        }
    }
}

impl<C: Cursor> Cursor for AtLeast<C> {
    fn current(&self) -> Option<Tid> {
        self.current
    }
    fn advance(&mut self) -> Result<()> {
        let Some(current) = self.current else {
            return Ok(());
        };
        for cursor in &mut self.cursors {
            if cursor.current() == Some(current) {
                cursor.advance()?;
            }
        }
        self.align()
    }
    fn seek(&mut self, target: Tid) -> Result<()> {
        if self.current.is_some_and(|current| current >= target) {
            return Ok(());
        }
        for cursor in &mut self.cursors {
            cursor.seek(target)?;
        }
        self.align()
    }
}

/// Postings of `keep` that are absent from `remove`. Only safe when `keep` is
/// an exact set; subtracting from a superset is never sound.
pub struct Difference<A, B> {
    keep: A,
    remove: B,
    current: Option<Tid>,
}

impl<A: Cursor, B: Cursor> Difference<A, B> {
    pub fn new(keep: A, remove: B) -> Result<Self> {
        let mut this = Self {
            keep,
            remove,
            current: None,
        };
        this.align()?;
        Ok(this)
    }

    fn align(&mut self) -> Result<()> {
        loop {
            let Some(candidate) = self.keep.current() else {
                self.current = None;
                return Ok(());
            };
            self.remove.seek(candidate)?;
            if self.remove.current() == Some(candidate) {
                self.keep.advance()?;
                continue;
            }
            self.current = Some(candidate);
            return Ok(());
        }
    }
}

impl<A: Cursor, B: Cursor> Cursor for Difference<A, B> {
    fn current(&self) -> Option<Tid> {
        self.current
    }
    fn advance(&mut self) -> Result<()> {
        if self.current.is_none() {
            return Ok(());
        }
        self.keep.advance()?;
        self.align()
    }
    fn seek(&mut self, target: Tid) -> Result<()> {
        if self.current.is_some_and(|current| current >= target) {
            return Ok(());
        }
        self.keep.seek(target)?;
        self.align()
    }
}

/// Drains a cursor, counting its postings.
pub fn count(mut cursor: impl Cursor) -> Result<u64> {
    let mut total = 0;
    while cursor.current().is_some() {
        total += 1;
        cursor.advance()?;
    }
    Ok(total)
}

/// Drains a cursor into a vector.
pub fn collect(mut cursor: impl Cursor) -> Result<Vec<Tid>> {
    let mut out = Vec::new();
    while let Some(tid) = cursor.current() {
        out.push(tid);
        cursor.advance()?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tids(blocks: &[u32]) -> Vec<Tid> {
        blocks.iter().map(|b| Tid::new(*b, 1).unwrap()).collect()
    }

    #[test]
    fn composes_intersection_union_and_difference() {
        let a = tids(&[1, 3, 5, 7, 9]);
        let b = tids(&[3, 4, 5, 9, 10]);
        let c = tids(&[5, 9, 11]);
        let and = Intersection::new(vec![Slice::new(&a), Slice::new(&b), Slice::new(&c)]).unwrap();
        assert_eq!(collect(and).unwrap(), tids(&[5, 9]));
        let or = Union::new(vec![Slice::new(&a), Slice::new(&c)]);
        assert_eq!(collect(or).unwrap(), tids(&[1, 3, 5, 7, 9, 11]));
        let diff = Difference::new(Slice::new(&a), Slice::new(&b)).unwrap();
        assert_eq!(collect(diff).unwrap(), tids(&[1, 7]));
        let nested: Vec<Box<dyn Cursor>> = vec![
            Box::new(Union::new(vec![Slice::new(&a), Slice::new(&c)])),
            Box::new(Difference::new(Slice::new(&b), Slice::new(&a)).unwrap()),
        ];
        assert_eq!(
            collect(Intersection::new(nested).unwrap()).unwrap(),
            Vec::<Tid>::new()
        );
        assert_eq!(
            count(Intersection::<Slice>::new(Vec::new()).unwrap()).unwrap(),
            0
        );
        assert_eq!(count(Union::new(vec![Empty, Empty])).unwrap(), 0);
    }

    #[test]
    fn union_reads_each_head_only_when_its_input_moves() {
        use std::cell::Cell;
        struct Observed<'a> {
            inner: Slice<'a>,
            reads: &'a Cell<usize>,
        }
        impl Cursor for Observed<'_> {
            fn current(&self) -> Option<Tid> {
                self.reads.set(self.reads.get() + 1);
                self.inner.current()
            }
            fn advance(&mut self) -> Result<()> {
                self.inner.advance()
            }
            fn seek(&mut self, target: Tid) -> Result<()> {
                self.inner.seek(target)
            }
        }
        let inputs = [tids(&[1, 3, 5]), tids(&[2, 3, 6]), tids(&[])];
        let reads = Cell::new(0);
        let union = CachedUnion::new(
            inputs
                .iter()
                .map(|input| Observed {
                    inner: Slice::new(input),
                    reads: &reads,
                })
                .collect(),
        );
        assert_eq!(collect(union).unwrap(), tids(&[1, 2, 3, 5, 6]));
        assert!(
            reads.get() <= inputs.len() + inputs.iter().map(Vec::len).sum::<usize>(),
            "head reads: {}",
            reads.get()
        );
    }

    proptest::proptest! {
        #[test]
        fn union_cached_heads_preserve_interleaved_seek_and_advance(
            lists in proptest::collection::vec(proptest::collection::vec(0u32..1000, 0..40), 0..20),
            operations in proptest::collection::vec(proptest::option::of(0u32..1100), 0..60),
        ) {
            let lists: Vec<Vec<Tid>> = lists.into_iter().map(|mut input| { input.sort_unstable(); input.dedup(); tids(&input) }).collect();
            let mut expected: Vec<Tid> = lists.iter().flatten().copied().collect();
            expected.sort_unstable(); expected.dedup();
            let mut reference = Slice::new(&expected);
            let mut actual = union(lists.iter().map(|list| Slice::new(list)).collect());
            for op in operations {
                proptest::prop_assert_eq!(actual.current(), reference.current());
                match op {
                    Some(block) => { let target = Tid::new(block, 1).unwrap(); actual.seek(target).unwrap(); reference.seek(target).unwrap(); }
                    None => { actual.advance().unwrap(); reference.advance().unwrap(); }
                }
            }
            proptest::prop_assert_eq!(collect(actual).unwrap(), collect(reference).unwrap());
        }
    }

    #[test]
    fn at_least_counts_members_across_inputs() {
        let a = tids(&[1, 2, 3, 9]);
        let b = tids(&[2, 3, 4, 9]);
        let c = tids(&[3, 4, 5]);
        let two = AtLeast::new(vec![Slice::new(&a), Slice::new(&b), Slice::new(&c)], 2).unwrap();
        assert_eq!(collect(two).unwrap(), tids(&[2, 3, 4, 9]));
        let three = AtLeast::new(vec![Slice::new(&a), Slice::new(&b), Slice::new(&c)], 3).unwrap();
        assert_eq!(collect(three).unwrap(), tids(&[3]));
        let mut one = AtLeast::new(vec![Slice::new(&a), Slice::new(&c)], 1).unwrap();
        one.seek(Tid::new(4, 1).unwrap()).unwrap();
        assert_eq!(collect(one).unwrap(), tids(&[4, 5, 9]));
        assert_eq!(
            count(AtLeast::new(vec![Slice::new(&a)], 2).unwrap()).unwrap(),
            0
        );
    }

    #[test]
    fn seek_on_composed_cursors_lands_on_first_member_at_or_after() {
        let a = tids(&[1, 3, 5, 7, 9]);
        let b = tids(&[2, 3, 6, 7]);
        let mut or = Union::new(vec![Slice::new(&a), Slice::new(&b)]);
        or.seek(Tid::new(4, 1).unwrap()).unwrap();
        assert_eq!(or.current(), Some(Tid::new(5, 1).unwrap()));
        let mut and = Intersection::new(vec![Slice::new(&a), Slice::new(&b)]).unwrap();
        and.seek(Tid::new(4, 1).unwrap()).unwrap();
        assert_eq!(and.current(), Some(Tid::new(7, 1).unwrap()));
        and.seek(Tid::new(8, 1).unwrap()).unwrap();
        assert_eq!(and.current(), None);
    }
}
