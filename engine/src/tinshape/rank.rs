// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Ranked top k over a segment in TIN's shape, a 256-page group at a time.
//!
//! A query some terms of which every match holds (a conjunction, a phrase)
//! walks the groups its rarest such term holds. In each, the terms' members
//! are intersected whole (word by word where all are grids, else from the
//! fewest, a grid by bit and a list by a merge), and
//! the survivors are bounded, cheapest first, before anything is read for
//! them: by the footer blocks they fall in (a window per run of slots no
//! block boundary crosses), then by their length alone against the
//! window's largest buckets, and only then scored exactly from the TF tail.
//! A phrase reads positions only for a candidate that would enter the top
//! k: before the top k fill, every match does, so its positions are
//! checked first, in ctid order, and only a match reads its length; after,
//! a group's candidates are scored best bound first and one that scores
//! into the top k is checked at once, so a confirmed one raises the
//! threshold over the rest.
//!
//! Any other query is block-max MaxScore over its scoring terms, a group at
//! a time: see [`Walk::run_or`].

use boldi_vigna::{PhrasePlan, SpanQuery, SpanSolver};
use segment::Tid;
use segment::tf_bucket::{BUCKET_COUNT, TfBucket};
use segment::tinshape::bits;
use segment::tinshape::blob::{Bytes, LazyBlob};
use segment::tinshape::docs::{Geometry, Group};
use segment::tinshape::ef::{Ef, EfCursor};
use segment::tinshape::positions::Positions;
use segment::tinshape::postings::{
    Directory, Form, GroupEntry, GroupFrontier, KIND_ARRAY, KIND_EF, KIND_GRID, LazyFooter,
    array_bytes, higher,
};
use segment::tinshape::segment::Segment;
use segment::{Error, Result};

use super::{
    Node, Part, RankedAnswer, Src, TermSet, Touch, kernels, matches, open_terms, required_terms,
};
use crate::bm25::TermScorer;
use crate::walk::{Ranked, TopRows, Visibility};

/// A required term's Elias-Fano container in a group is probed for the
/// candidates left rather than decoded when it holds more than this many
/// times as many members.
const SEEK_RATIO: usize = 4;

/// Cache lines of the lead's next container asked for while a group is
/// walked ([`Walk::run`]): a rare lead's container is a line or a few.
const LEAD_PREFETCH_LINES: usize = 4;

/// [`Walk::buckets`] of a term the candidate does not hold.
const NO_BUCKET: u8 = u8::MAX;

/// [`Walk::buckets`] of a common term (see [`Sc::make_common`]) the
/// candidate holds, its bucket not read yet.
const LATER: u8 = u8::MAX - 1;

/// A scoring term of a led walk in at least one document in this many is
/// common ([`Sc::make_common`]), when the walk scores a rarer term too.
const COMMON_RATIO: u64 = 10;

/// Candidates staged before they are finished best first: a group's
/// candidates are staged and finished this many at a time, so a threshold
/// that is still low early in a walk rises between chunks rather than
/// letting a whole group's candidates be bounded and sorted against it.
const STAGE_CHUNK: usize = 256;

/// A group with at least this many candidates has them sieved by their
/// buckets ([`Walk::sieve`]) this many at a time ([`SIEVE_CHUNK`]) before
/// any is bounded alone.
const SIEVE_MIN: usize = 32;
const SIEVE_CHUNK: usize = 256;

/// A disjunction's group is sieved ([`Walk::or_sieve`]) when its mask
/// holds at least this many members.
const OR_SIEVE_MIN: u64 = 128;

/// A scoring term's score metadata as a walk bounds by it: a sparse
/// record's footer blocks, decoded a block at a time as the walk reaches
/// them ([`LazyFooter`]); a grouped record's directory entries, one per
/// group the term holds, each bounded from its frontier when the walk
/// visits the group (as TIN reads one footer entry per term and group). A
/// "block" of either is numbered from zero: a footer block, or a position
/// in the directory.
enum Bounds<'s, 'a> {
    /// Boxed: a lazy footer is several vectors, a group's bounds a few
    /// words.
    Blocks(Box<LazyFooter<'a>>),
    Groups(GroupBounds<'s>),
}

/// A grouped record's entries as a walk's blocks: the directory (shared
/// with the parsed record, its entries decoded as the walk reaches them, its
/// bytes the frontiers), and the segment's groups, which turn an entry's
/// group index into the group's last slot.
struct GroupBounds<'s> {
    dir: std::rc::Rc<Directory>,
    rounded: bool,
    groups: &'s [Group],
}

impl GroupBounds<'_> {
    /// Entry `b` (decoded if it was not yet).
    #[inline]
    fn entry(&self, b: usize) -> &GroupEntry {
        self.dir
            .get(b, self.groups)
            .unwrap_or_else(|e| corrupt_footer(e))
    }

    /// Entry `b`'s group's first slot.
    #[inline]
    fn start(&self, b: usize) -> u32 {
        self.groups[self.entry(b).index as usize].slot_base
    }

    /// Entry `b`'s group's last slot.
    #[inline]
    fn end(&self, b: usize) -> u32 {
        let group = &self.groups[self.entry(b).index as usize];
        group.slot_base + group.slots() - 1
    }

    /// The frontier of entry `b`.
    #[inline]
    fn frontier(&self, b: usize) -> GroupFrontier<'_> {
        GroupFrontier::new(
            self.dir.bytes(),
            self.entry(b).frontier as usize,
            self.rounded,
        )
    }

    /// The first entry from `from` on whose group ends at `slot` or later:
    /// a step or two forward as a rule (the walk's next group), else a
    /// gallop and a binary search; the entry count when there is none.
    #[inline]
    fn first_reaching(&self, from: usize, slot: u32) -> usize {
        let n = self.dir.len();
        if from >= n || self.end(from) >= slot {
            return from;
        }
        let (mut lo, mut step) = (from, 1);
        let mut hi = loop {
            let at = lo + step;
            if at >= n {
                break n;
            }
            if self.end(at) >= slot {
                break at;
            }
            lo = at;
            step *= 2;
        };
        // `lo` ends before `slot`; `hi` ends at or after it, or is `n`.
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if self.end(mid) >= slot {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        hi
    }
}

impl<'a> Bounds<'_, 'a> {
    /// Blocks the record has.
    #[inline]
    fn blocks(&self) -> usize {
        match self {
            Self::Blocks(f) => f.blocks(),
            Self::Groups(g) => g.dir.len(),
        }
    }

    /// Blocks decoded so far (every one of a grouped record's).
    #[inline]
    fn decoded(&self) -> usize {
        match self {
            Self::Blocks(f) => f.decoded(),
            Self::Groups(g) => g.dir.len(),
        }
    }

    /// A single posting's bucket, which the dictionary keeps.
    #[inline]
    fn single(&self) -> Option<u8> {
        match self {
            Self::Blocks(f) => f.single(),
            Self::Groups(_) => None,
        }
    }

    /// Block `b`'s last slot (decoded): a footer block's last posting's, a
    /// group entry's group's last.
    #[inline]
    fn last(&self, b: usize) -> u32 {
        match self {
            Self::Blocks(f) => f.last(b),
            Self::Groups(g) => g.end(b),
        }
    }

    /// Whether block `b`'s postings all lie past `slot`: a group entry
    /// whose group starts after it (a footer block may start anywhere).
    #[inline]
    fn starts_after(&self, b: usize, slot: u32) -> bool {
        match self {
            Self::Blocks(_) => false,
            Self::Groups(g) => g.start(b) > slot,
        }
    }

    /// The first block from `from` on whose last slot is `slot` or later,
    /// decoded (a footer's decoded as far as it); the block count when
    /// there is none. Looks among the blocks decoded first.
    #[inline]
    fn first_reaching(&mut self, from: usize, slot: u32) -> Result<usize> {
        match self {
            Self::Blocks(f) => {
                let last = f.lasts();
                let mut b = from;
                while b < last.len() && last[b] < slot {
                    b += 1;
                }
                if b < last.len() {
                    Ok(b)
                } else {
                    f.seek(b, slot)
                }
            }
            Self::Groups(g) => Ok(g.first_reaching(from, slot)),
        }
    }

    /// Block `b`'s largest bucket (decoded).
    #[inline]
    fn max_bucket(&self, b: usize) -> u8 {
        match self {
            Self::Blocks(f) => f.max_bucket(b),
            Self::Groups(g) => g.entry(b).max_bucket,
        }
    }

    /// Where block `b`'s buckets start in the TF tail, in bytes (for page
    /// accounting).
    #[inline]
    fn tf_byte(&self, b: usize) -> usize {
        match self {
            Self::Blocks(f) => f.tf_at(b) as usize,
            Self::Groups(g) => g.entry(b).tf_bit as usize / 8,
        }
    }

    /// The bucket of posting `index`, which block `b` holds, from the
    /// record's TF tail.
    #[inline]
    fn bucket_in(&mut self, b: usize, tf: Bytes<'_>, index: u32) -> Result<u8> {
        match self {
            Self::Blocks(f) => f.bucket(tf, index),
            Self::Groups(g) => {
                let e = g.entry(b);
                debug_assert!(e.first <= index && index < e.first + e.count);
                if e.width == 0 {
                    return Ok(0);
                }
                let bit = e.bucket_bit(index);
                let bytes = tf.window(bit / 8, 8)?;
                Ok(bits::get_at(bytes, bit % 8, u32::from(e.width))? as u8)
            }
        }
    }

    /// [`Self::floor`] of block `b` at every bucket, from one read of its
    /// frontier.
    #[inline]
    fn floors(&self, b: usize) -> [u32; BUCKET_COUNT] {
        match self {
            Self::Blocks(f) => f.floors(b),
            Self::Groups(g) => {
                let mut out = [u32::MAX; BUCKET_COUNT];
                for (bucket, length) in g.frontier(b) {
                    let at = usize::from(bucket) % BUCKET_COUNT;
                    out[at] = out[at].min(length);
                }
                let mut least = u32::MAX;
                for floor in out.iter_mut().rev() {
                    least = least.min(*floor);
                    *floor = if least == u32::MAX { 0 } else { least };
                }
                out
            }
        }
    }

    /// The best of `bound` over block `b`'s frontier pairs (0 at least).
    #[inline]
    fn fold_bound(&self, b: usize, mut bound: impl FnMut(u8, u32) -> f32) -> f32 {
        match self {
            Self::Blocks(f) => {
                if f.keeps_pairs() {
                    f.frontier(b)
                        .iter()
                        .map(|(bucket, length)| bound(*bucket, *length))
                        .fold(0.0_f32, higher)
                } else {
                    // A skimmed footer's, or one decoded for its bounds.
                    let mut v = 0.0_f32;
                    f.each_pair(b, |bucket, length| v = higher(v, bound(bucket, length)));
                    v
                }
            }
            Self::Groups(g) => g
                .frontier(b)
                .map(|(bucket, length)| bound(bucket, length))
                .fold(0.0_f32, higher),
        }
    }

    /// Footer bytes read so far (a sparse record's).
    #[inline]
    fn read(&self) -> usize {
        match self {
            Self::Blocks(f) => f.read(),
            Self::Groups(_) => 0,
        }
    }

    /// See [`LazyFooter::borrowed`]: nothing for a grouped record, whose
    /// directory is copied when parsed.
    #[inline]
    fn borrowed(&self) -> Option<(usize, usize)> {
        match self {
            Self::Blocks(f) => f.borrowed(),
            Self::Groups(_) => None,
        }
    }

    /// See [`LazyFooter::release_passed`].
    fn release_passed(&mut self) {
        if let Self::Blocks(f) = self {
            f.release_passed();
        }
    }
}

/// One scoring term of a walk.
pub(super) struct Sc<'s, 'a> {
    pub(super) term: usize,
    pub(super) scorer: TermScorer,
    /// The term's blocks: its footer, decoded as far as the walk has
    /// reached, or its group entries.
    footer: Bounds<'s, 'a>,
    /// A footer's blocks' bounds, per block decoded; NaN until asked.
    bounds: Vec<f32>,
    /// A grouped record's entry bounded last, and its bound: a walk asks
    /// for a group's bound a few times (the group, its sub-ranges, each
    /// candidate's window) and moves on, so one is kept.
    gb: usize,
    gv: f32,
    /// Group entries bounded, for the memo counts.
    computed: u64,
    /// The block a range bound starts from, moved forward only.
    wb: usize,
    /// The block holding the last candidate asked about, moved forward only.
    cb: usize,
    /// [`TermScorer::length_bound_parts`], for [`Walk::length_bound`]'s
    /// cheap first test.
    num: [f32; BUCKET_COUNT],
    den: [f32; BUCKET_COUNT],
    factor: f32,
    /// [`Self::floor`] per bucket, with the bucket's score at that length,
    /// and the block it was taken in (a rare term's block spans many
    /// groups' candidates): an entry of another block is stale, so moving
    /// to a new block clears nothing.
    bb: [(u32, f32); BUCKET_COUNT],
    bb_block: [u32; BUCKET_COUNT],
    /// The floors of block `floors_block` at every bucket, read at once.
    floors: [u32; BUCKET_COUNT],
    floors_block: u32,
    /// A common term's bound over a group (see [`Sc::make_common`]).
    common: Option<f32>,
}

/// A footer block past what a walk could reach was asked for, or a block
/// it reached is corrupt.
#[cold]
fn corrupt_footer(error: Error) -> ! {
    crate::corrupt(format!("Stannum postings footer: {error}"))
}

impl<'s, 'a> Sc<'s, 'a> {
    /// The scoring term `term` of `set`, whose blocks are its directory's
    /// entries (a grouped record) or its footer's blocks (decoded per
    /// query, a block at a time as the walk reaches it, and read as far).
    pub(super) fn new(
        term: usize,
        scorer: TermScorer,
        set: &TermSet<'a>,
        segment: &'s Segment<'a>,
    ) -> Result<Self> {
        let footer = match &set.postings.form {
            Form::Grouped(dir) => Bounds::Groups(GroupBounds {
                dir: dir.clone(),
                rounded: set.postings.rounded,
                groups: &segment.docs.geometry.groups,
            }),
            _ => Bounds::Blocks(Box::new(set.postings.lazy_footer(
                segment.block_size,
                set.max_bucket,
                segment.adaptive_tf,
            )?)),
        };
        let (num, den, factor) = scorer.length_bound_parts();
        Ok(Self {
            term,
            bounds: Vec::new(),
            gb: usize::MAX,
            gv: 0.0,
            computed: 0,
            footer,
            scorer,
            wb: 0,
            cb: 0,
            num,
            den,
            factor,
            bb: [(0, 0.0); BUCKET_COUNT],
            bb_block: [u32::MAX; BUCKET_COUNT],
            floors: [0; BUCKET_COUNT],
            floors_block: u32::MAX,
            common: None,
        })
    }

    /// Whether the term's blocks are its group entries.
    #[inline]
    fn grouped(&self) -> bool {
        matches!(self.footer, Bounds::Groups(_))
    }

    /// Makes the term common, for a led walk that scores a rarer term too.
    /// A sparse record's group is bounded by its record (what its largest
    /// bucket `max_bucket` scores at the least length, which no block's
    /// frontier exceeds), not its blocks there, and its footer is skimmed
    /// ([`LazyFooter::skim`]): most of its blocks are decoded only for
    /// their TF offsets. A grouped record's group is bounded by its entry,
    /// one frontier per group. Either way a candidate's window still bounds
    /// it by its block, but its bucket is read last, only for a candidate
    /// the rarer terms' buckets leave in reach.
    fn make_common(&mut self, max_bucket: u8) {
        let bucket = TfBucket::new(max_bucket).expect("a valid bucket");
        self.common = Some(self.scorer.bound_through(bucket, 0));
        if let Bounds::Blocks(f) = &mut self.footer {
            f.skim();
        }
    }

    /// A common term's bound over a group before its blocks are asked:
    /// its record's, for a sparse record; `None` for a grouped one, whose
    /// entry bounds the group as cheaply.
    #[inline]
    fn common_bound(&self) -> Option<f32> {
        if self.grouped() { None } else { self.common }
    }

    /// Block `b`'s bound, which must be decoded: the best score of its
    /// frontier.
    #[inline]
    fn bound(&mut self, b: usize) -> f32 {
        if self.grouped() {
            if self.gb == b {
                return self.gv;
            }
        } else if let Some(v) = self.bounds.get(b)
            && !v.is_nan()
        {
            return *v;
        }
        self.bound_of(b)
    }

    /// [`Self::bound`], not known yet.
    #[inline(never)]
    fn bound_of(&mut self, b: usize) -> f32 {
        let scorer = &self.scorer;
        let v = self.footer.fold_bound(b, |bucket, length| {
            scorer.bound_through(TfBucket::new(bucket).expect("a valid bucket"), length)
        });
        if self.grouped() {
            self.gb = b;
            self.gv = v;
            self.computed += 1;
        } else {
            if b >= self.bounds.len() {
                self.bounds
                    .resize(self.footer.decoded().max(b + 1), f32::NAN);
            }
            self.bounds[b] = v;
        }
        v
    }

    /// Decodes every block of a footer and its bound now, in one pass: for
    /// a walk that bounds every block anyway (a disjunction bounds each
    /// group its terms hold), so decoding a block at a time as it is
    /// reached saves nothing, and reading the footer's pages between a
    /// group's leaves them pinned among the group's (see
    /// [`Walk::group_done`]). Each block is bounded as it is decoded, and
    /// its frontier is not kept as pairs ([`LazyFooter::decode_bounds`]):
    /// most blocks' are never read again. A grouped record's entries are
    /// decoded in one pass ([`Directory::decode_all`]) and bounded as its
    /// groups are visited.
    fn decode_all(&mut self) {
        let f = match &mut self.footer {
            Bounds::Blocks(f) => f,
            Bounds::Groups(g) => {
                g.dir
                    .decode_all(g.groups)
                    .unwrap_or_else(|e| corrupt_footer(e));
                return;
            }
        };
        let scorer = &self.scorer;
        self.bounds.clear();
        f.decode_bounds(
            |bucket, length| {
                scorer.bound_through(TfBucket::new(bucket).expect("a valid bucket"), length)
            },
            &mut self.bounds,
        )
        .unwrap_or_else(|e| corrupt_footer(e));
    }

    /// The first block from `from` on whose last slot is `slot` or later,
    /// decoded; the block count when there is none.
    #[inline]
    fn first_reaching(&mut self, from: usize, slot: u32) -> usize {
        let b = self
            .footer
            .first_reaching(from, slot)
            .unwrap_or_else(|e| corrupt_footer(e));
        if !self.grouped() && self.bounds.len() < self.footer.decoded() {
            if self.bounds.capacity() == 0 {
                self.bounds.reserve_exact(self.footer.blocks());
            }
            self.bounds.resize(self.footer.decoded(), f32::NAN);
        }
        b
    }

    /// A grouped term's bound over the group of its directory entry `b`,
    /// which the walk's seek found: [`Self::range_bound`] over the group's
    /// slots, without seeking the entry again.
    #[inline]
    fn entry_bound(&mut self, b: usize) -> f32 {
        debug_assert!(self.grouped());
        higher(0.0, self.bound(b))
    }

    /// The best bound of the blocks overlapping slots `from..=to`; ranges
    /// must be asked in increasing order of `from`. Zero where a grouped
    /// record's next entry lies past the range: the term has nothing there.
    #[inline]
    fn range_bound(&mut self, from: u32, to: u32) -> f32 {
        // The blocks from the first ending at `from` or later through the
        // first ending at `to` or later (or the last), in one pass,
        // decoding as far as the last of them when it is not yet.
        self.wb = self.first_reaching(self.wb, from);
        let blocks = self.footer.blocks();
        let mut best = 0.0_f32;
        let mut b = self.wb;
        if b < blocks && self.footer.starts_after(b, to) {
            return 0.0;
        }
        while b < blocks {
            if b >= self.footer.decoded() {
                self.first_reaching(b, to);
            }
            best = higher(best, self.bound(b));
            if self.footer.last(b) >= to {
                break;
            }
            b += 1;
        }
        best
    }

    /// The block holding `slot`, if the term has a posting at or after it;
    /// slots must be asked in increasing order.
    #[inline]
    fn block_at(&mut self, slot: u32) -> Option<usize> {
        // As a rule a block decoded already: the candidate's, or one after.
        self.cb = self.first_reaching(self.cb, slot);
        (self.cb < self.footer.blocks()).then_some(self.cb)
    }

    /// The block holding posting `index`: a footer's by the block size, a
    /// grouped record's the entry `entry` the term's members were loaded
    /// from for the group at hand.
    #[inline]
    fn block_of(&self, index: u32, entry: usize) -> usize {
        match &self.footer {
            Bounds::Blocks(f) => (index / f.block_size()) as usize,
            Bounds::Groups(g) => {
                debug_assert!(
                    entry < g.dir.len()
                        && g.entry(entry).first <= index
                        && index < g.entry(entry).first + g.entry(entry).count
                );
                entry
            }
        }
    }

    /// The bucket of posting `index`, which block `b` holds.
    #[inline]
    fn bucket_in(&mut self, b: usize, tf: Bytes<'_>, index: u32) -> Result<u8> {
        self.footer.bucket_in(b, tf, index)
    }

    /// A block's floor at a bucket (see [`Bounds::floors`]) and what bucket `bucket` scores at most there (the
    /// term's own bound, before the other terms raise the length), kept per
    /// bucket for the block it was last asked about in.
    #[inline]
    fn floor_kept(&mut self, b: usize, bucket: u8) -> (u32, f32) {
        let at = usize::from(bucket) % BUCKET_COUNT;
        // Blocks are numbered below `u32::MAX` (a record has fewer postings).
        let tag = b as u32;
        if self.bb_block[at] == tag {
            return self.bb[at];
        }
        self.bb_block[at] = tag;
        let floor = self.floor_at(b, bucket);
        let v = (
            floor,
            self.scorer
                .bound_through(TfBucket::new(bucket).expect("a valid bucket"), floor),
        );
        self.bb[at] = v;
        v
    }

    /// A block's floor at a bucket, its floors read at once and kept for the
    /// block (a footer decoded for its bounds keeps its frontiers as their
    /// entries' bytes; a group entry's frontier is read from the
    /// directory).
    #[inline]
    fn floor_at(&mut self, b: usize, bucket: u8) -> u32 {
        if let Bounds::Blocks(f) = &self.footer
            && f.keeps_pairs()
        {
            return f.floor(b, bucket);
        }
        // Blocks are numbered below `u32::MAX` (a record has fewer postings).
        let tag = b as u32;
        if self.floors_block != tag {
            self.floors_block = tag;
            self.floors = self.footer.floors(b);
        }
        self.floors[usize::from(bucket) % BUCKET_COUNT]
    }

    /// Block `b`'s largest bucket: its frontier's last pair.
    #[inline]
    fn max_bucket(&self, b: usize) -> u8 {
        self.footer.max_bucket(b)
    }

    /// A grouped record's directory entry `b`; `None` for a sparse record.
    #[inline]
    fn group_entry(&self, b: usize) -> Option<GroupEntry> {
        match &self.footer {
            Bounds::Groups(g) => (b < g.dir.len()).then(|| *g.entry(b)),
            Bounds::Blocks(_) => None,
        }
    }
}

/// How a term's members in the group at hand are held.
#[derive(Default)]
enum Kind<'a> {
    /// Decoded into [`Mem::list`].
    #[default]
    List,
    /// A grid container, read in place.
    Grid(&'a [u8]),
    /// An array container's bytes, read in place by seeking (an array much
    /// longer than the candidates it filters; a shorter one is a list).
    Array(&'a [u8]),
    /// An Elias-Fano list read by seeking: a group container (`offset` 0)
    /// or a sparse term's whole list (`offset` the group's first slot).
    Cursor { cursor: EfCursor<'a>, offset: u32 },
}

/// A term's members in the group at hand, and a forward cursor over their
/// posting indexes.
#[derive(Default)]
struct Mem<'a> {
    /// The group loaded, plus one; zero for none.
    loaded: u32,
    kind: Kind<'a>,
    /// The members' local slots, ascending, for [`Kind::List`].
    list: Vec<u32>,
    /// Postings in earlier groups (zero for a sparse term's cursor, whose
    /// rank is the index).
    first: u32,
    /// Members in the group: from the directory, or estimated for a sparse
    /// term.
    count: u32,
    /// Grid: members in words `..w` are `run`. List: the next member is at
    /// `pos`.
    w: usize,
    run: u32,
    pos: usize,
    /// The term's group directory position, moved forward only; and the
    /// entry the members at hand were loaded from (a grouped term's block
    /// for the group at hand), `usize::MAX` for none.
    hint: usize,
    entry: usize,
    /// Containers loaded from the directory, for [`MemoCounts`].
    ///
    /// [`MemoCounts`]: segment::tinshape::segment::MemoCounts
    used: u32,
}

impl Mem<'_> {
    /// The posting index of `local` if the term holds it; slots must be
    /// asked in increasing order (asking one again is allowed).
    #[inline]
    fn find(&mut self, local: u32) -> Option<u32> {
        match &mut self.kind {
            Kind::Grid(bytes) => {
                if bytes[local as usize / 8] >> (local % 8) & 1 == 0 {
                    return None;
                }
                let w = local as usize / 64;
                if w > self.w {
                    self.run += kernels::popcount_bytes(&bytes[self.w * 8..w * 8]);
                    self.w = w;
                }
                let word = bits::word(bytes, w);
                Some(self.first + self.run + (word & ((1u64 << (local % 64)) - 1)).count_ones())
            }
            Kind::List => {
                let list = &self.list;
                let mut pos = self.pos;
                while pos < list.len() && list[pos] < local {
                    pos += 1;
                }
                self.pos = pos;
                (pos < list.len() && list[pos] == local).then(|| self.first + pos as u32)
            }
            Kind::Array(bytes) => {
                let pos = kernels::array_seek(bytes, self.pos, local);
                self.pos = pos;
                let held = pos < bytes.len() / 2
                    && u16::from_le_bytes([bytes[2 * pos], bytes[2 * pos + 1]]) as u32 == local;
                held.then(|| self.first + pos as u32)
            }
            Kind::Cursor { cursor, offset } => {
                let target = *offset + local;
                cursor.seek(target);
                (cursor.current() == Some(target)).then(|| self.first + cursor.rank() as u32)
            }
        }
    }
}

/// [`Walk::cut`] against the bar `bar`.
#[inline]
fn cut_by(bar: Option<(f32, Tid)>, bound: f32, from: impl FnOnce() -> Tid) -> bool {
    match bar {
        None => false,
        Some((score, tid)) => match bound.total_cmp(&score) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Equal => from() >= tid,
            std::cmp::Ordering::Greater => false,
        },
    }
}

/// A scoring term a group's candidates are sieved by ([`Walk::sieve`]): a
/// grouped term whose members in the group are a bitmap or a list.
#[derive(Clone, Copy)]
struct SieveTerm {
    /// The scoring term (an index into the walk's), and its directory
    /// entry for the group.
    i: usize,
    entry: usize,
    group: GroupEntry,
    /// Whether its members are the walk's row of it (a disjunction's),
    /// rather than its [`Mem`].
    row: bool,
    /// Where its members' counts per word start in [`Walk::sieve_prefix`]
    /// (a bitmap's), else `usize::MAX` (a list's).
    prefix: usize,
    /// Per bucket, what a candidate of that bucket scores at most by the
    /// term ([`Sc::floor_kept`]); NaN until a candidate has the bucket. And
    /// the floor that bound is taken at (set with it).
    table: [f32; BUCKET_COUNT],
    floors: [u32; BUCKET_COUNT],
}

/// What the bar does to a sieved candidate whose bound is known: cuts it,
/// keeps it, or ties it (cut from the bar's ctid on, see [`Walk::cut`]).
#[derive(Clone, Copy)]
enum Verdict {
    Drop,
    Keep,
    Tie,
}

/// Where a sieved term's members lie, read without moving the walk's
/// cursor over them: a bitmap of the group's slots, with its members
/// before each word (`prefix`, one more than its words); or a list of local
/// slots, the next to look at `pos`.
#[derive(Clone, Copy)]
enum SieveMembers<'m> {
    Bits { bytes: &'m [u8], prefix: &'m [u32] },
    List { list: &'m [u32], pos: usize },
}

/// Appends to `out` the members of the bitmap `bytes` before each of its
/// words, and in all.
fn sieve_prefix(bytes: &[u8], out: &mut Vec<u32>) {
    let mut run = 0;
    for w in 0..bytes.len().div_ceil(8) {
        out.push(run);
        run += bits::word(bytes, w).count_ones();
    }
    out.push(run);
}

/// Into `ranks`, per local slot of `cands` (ascending, none before `list`'s
/// member `pos`), its posting index in the term (`first` and on, by its
/// place in `list`), or `u32::MAX` where the term lacks it.
fn sieve_ranks(list: &[u32], mut pos: usize, first: u32, cands: &[u32], ranks: &mut Vec<u32>) {
    ranks.clear();
    for &local in cands {
        while pos < list.len() && list[pos] < local {
            pos += 1;
        }
        ranks.push(if pos < list.len() && list[pos] == local {
            first + pos as u32
        } else {
            u32::MAX
        });
    }
}

/// Into `out`, per posting index of `ranks` (ascending where not
/// `u32::MAX`), its bucket in the TF tail `tf` at group entry `e` (else
/// [`NO_BUCKET`]); returns the bucket values seen as a mask, and the bytes
/// of `tf` read (for page accounting).
fn sieve_buckets(
    e: &GroupEntry,
    tf: Bytes<'_>,
    ranks: &[u32],
    out: &mut Vec<u8>,
) -> Result<(u32, std::ops::Range<usize>)> {
    out.clear();
    let held = |r: &&u32| **r != u32::MAX;
    let (Some(&lo), Some(&hi)) = (ranks.iter().find(held), ranks.iter().rfind(held)) else {
        out.resize(ranks.len(), NO_BUCKET);
        return Ok((0, 0..0));
    };
    let width = u32::from(e.width);
    if width == 0 {
        out.extend(
            ranks
                .iter()
                .map(|r| if *r == u32::MAX { NO_BUCKET } else { 0 }),
        );
        return Ok((1, 0..0));
    }
    let from = e.bucket_bit(lo) / 8;
    let to = (e.bucket_bit(hi) + width as usize).div_ceil(8);
    // Eight bytes past the last bucket where the tail has them: whole loads.
    let bytes = tf.get(from, (to + 7).min(tf.len()))?;
    let mut seen = 0u32;
    out.resize(ranks.len(), NO_BUCKET);
    for (bucket, &r) in out.iter_mut().zip(ranks) {
        if r == u32::MAX {
            continue;
        }
        let bit = e.bucket_bit(r) - from * 8;
        let value = match bytes.get(bit / 8..bit / 8 + 8) {
            Some(chunk) => {
                let word = u64::from_le_bytes(chunk.try_into().expect("eight bytes"));
                (word >> (bit % 8) & ((1u64 << width) - 1)) as u32
            }
            None => bits::get_at(bytes, bit, width)?,
        };
        // A bucket past the table's is left to the candidate's own check.
        *bucket = value.min(BUCKET_COUNT as u32) as u8;
        seen |= 1 << *bucket;
    }
    Ok((seen, from..to))
}

/// [`sieve_ranks`] and [`sieve_buckets`] in one pass over `cands`, for the
/// members of a bitmap (`bytes`, counted per word in `prefix`) whose
/// postings start at `e.first`: the TF slice read is the one the
/// candidates' words span.
fn sieve_bits_buckets(
    bytes: &[u8],
    prefix: &[u32],
    e: &GroupEntry,
    tf: Bytes<'_>,
    cands: &[u32],
    out: &mut Vec<u8>,
) -> Result<(u32, std::ops::Range<usize>)> {
    out.clear();
    let (Some(&lo), Some(&hi)) = (cands.first(), cands.last()) else {
        return Ok((0, 0..0));
    };
    // The postings of the candidates' words, by their place in the group.
    let (lo, hi) = (prefix[lo as usize / 64], prefix[hi as usize / 64 + 1]);
    let width = u32::from(e.width);
    let held = |local: u32| bits::word(bytes, local as usize / 64) >> (local % 64) & 1 == 1;
    if hi == lo || width == 0 {
        // None held, or every bucket zero.
        out.extend(cands.iter().map(|&local| {
            if width == 0 && held(local) {
                0
            } else {
                NO_BUCKET
            }
        }));
        return Ok((u32::from(hi > lo), 0..0));
    }
    let from = e.bucket_bit(e.first + lo) / 8;
    let to = (e.bucket_bit(e.first + hi - 1) + width as usize).div_ceil(8);
    // Eight bytes past the last bucket where the tail has them: whole loads.
    let tail = tf.get(from, (to + 7).min(tf.len()))?;
    let mask = (1u64 << width) - 1;
    let mut seen = 0u32;
    out.reserve(cands.len());
    for &local in cands {
        let at = local as usize / 64;
        let word = bits::word(bytes, at);
        let below = (word & ((1u64 << (local % 64)) - 1)).count_ones();
        // A candidate the term lacks reads a bucket it ignores, of a member
        // in the slice (one after the last is not).
        let place = (prefix[at] + below).min(hi - 1);
        let bit = e.bucket_bit(e.first + place) - from * 8;
        let value = match tail.get(bit / 8..bit / 8 + 8) {
            Some(chunk) => {
                let chunk = u64::from_le_bytes(chunk.try_into().expect("eight bytes"));
                (chunk >> (bit % 8) & mask) as u32
            }
            None => bits::get_at(tail, bit, width)?,
        };
        // A bucket past the table's is left to the candidate's own check.
        let bucket = value.min(BUCKET_COUNT as u32);
        let keep = (word >> (local % 64) & 1) as u32;
        seen |= keep << bucket;
        // `NO_BUCKET` where not held, without a branch.
        out.push((bucket | keep.wrapping_sub(1)) as u8);
    }
    Ok((seen, from..to))
}

/// The group holding `slot`, which is group `from`'s first slot or later:
/// searched forward from `from`, doubling the step, so a walk's next
/// group (as a rule `from` or one soon after) costs a probe or two rather
/// than a binary search of every group.
#[inline]
fn group_from(geometry: &Geometry, from: usize, slot: u32) -> usize {
    let groups = &geometry.groups;
    debug_assert!(groups[from].slot_base <= slot);
    // `lo` holds `slot` or comes before the group that does; `hi` is past it.
    let (mut lo, mut step) = (from, 1);
    let mut hi = loop {
        let at = lo + step;
        if at >= groups.len() {
            break groups.len();
        }
        if groups[at].slot_base > slot {
            break at;
        }
        lo = at;
        step *= 2;
    };
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if groups[mid].slot_base <= slot {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// The first group at or after `from` holding a member of `set`.
fn next_group(
    set: &mut TermSet<'_>,
    geometry: &Geometry,
    from: u32,
    hint: &mut usize,
    touch: &mut impl Touch,
) -> Option<u32> {
    if let Form::Sparse(list) = &set.postings.form {
        let group = geometry.groups.get(from as usize)?;
        let cursor = set.sparse.get_or_insert_with(|| {
            touch.touch(
                Part::Payload,
                set.at + set.postings.payload_at,
                set.postings.payload.len(),
            );
            list.cursor()
        });
        cursor.seek(group.slot_base);
        return cursor
            .current()
            .map(|slot| group_from(geometry, from as usize, slot) as u32);
    }
    set.seek_index(from, hint)
}

/// Loads `set`'s members in group `g`: decoded into a list, or an
/// Elias-Fano list of more than `probe_over` members left to be sought
/// (for a sparse term, which must hold the group, its share of the term's
/// postings by slots stands for its count there). False, with nothing
/// loaded, when a grouped term does not hold the group: the directory is
/// looked up once.
fn load<'a>(
    set: &mut TermSet<'a>,
    geometry: &Geometry,
    g: u32,
    mem: &mut Mem<'a>,
    probe_over: usize,
    touch: &mut impl Touch,
) -> Result<bool> {
    mem.loaded = g + 1;
    mem.kind = Kind::List;
    mem.list.clear();
    mem.w = 0;
    mem.run = 0;
    mem.pos = 0;
    mem.entry = usize::MAX;
    let group = geometry.groups[g as usize];
    if let Form::Sparse(list) = &set.postings.form {
        let cursor = set.sparse.get_or_insert_with(|| {
            touch.touch(
                Part::Payload,
                set.at + set.postings.payload_at,
                set.postings.payload.len(),
            );
            list.cursor()
        });
        cursor.seek(group.slot_base);
        let share = u64::from(set.df) * u64::from(group.slots()) / u64::from(geometry.slots).max(1);
        if share.max(1) > probe_over as u64 {
            mem.count = share.max(1) as u32;
            mem.first = 0;
            mem.kind = Kind::Cursor {
                cursor: cursor.clone(),
                offset: group.slot_base,
            };
            return Ok(true);
        }
        mem.first = cursor.rank() as u32;
        let end = group.slot_base + group.slots();
        let list = &mut mem.list;
        cursor.drain_below(end, |slot| list.push(slot - group.slot_base));
        mem.count = mem.list.len() as u32;
        return Ok(true);
    }
    let Some(entry) = set.find(g, &mut mem.hint) else {
        mem.count = 0;
        return Ok(false);
    };
    mem.count = entry.count;
    mem.entry = mem.hint;
    match entry.src {
        Src::Container {
            entry: e,
            bytes,
            at,
        } => {
            touch.touch(Part::Payload, at, bytes.len());
            let bytes = bytes.all()?;
            mem.used += 1;
            mem.first = e.first;
            if e.kind == KIND_GRID {
                mem.kind = Kind::Grid(bytes);
            } else if e.kind == KIND_ARRAY && e.count as usize > probe_over {
                mem.kind = Kind::Array(array_bytes(bytes, e.count, group.slots())?);
            } else if e.kind == KIND_EF && e.count as usize > probe_over {
                let ef = Ef::parse(bytes, e.count as usize, group.slots())?;
                mem.kind = Kind::Cursor {
                    cursor: ef.cursor(),
                    offset: 0,
                };
            } else {
                kernels::container_locals(
                    &e,
                    bytes,
                    group.slots(),
                    geometry.starts(g as usize),
                    &mut mem.list,
                )?;
            }
        }
        Src::Locals { from, to } => {
            mem.first = from;
            mem.list
                .extend_from_slice(&set.locals[from as usize..to as usize]);
        }
    }
    Ok(true)
}

/// A term's positions read by posting index, forward from the last entry
/// read where that is nearer than the entry its tables locate.
pub(super) struct PosCursor<'a> {
    positions: Positions<'a>,
    at: usize,
    /// Entry `next` starts at byte `next_at`.
    next: u32,
    next_at: usize,
}

impl<'a> PosCursor<'a> {
    pub(super) fn new(stream: (segment::tinshape::blob::Bytes<'a>, usize)) -> Result<Self> {
        let positions = Positions::parse(stream.0)?;
        Ok(Self {
            next: 0,
            next_at: positions.data_at,
            positions,
            at: stream.1,
        })
    }

    /// Reads entry `index` into `out`.
    pub(super) fn read(
        &mut self,
        index: u32,
        out: &mut Vec<u32>,
        touch: &mut impl Touch,
    ) -> Result<()> {
        let (entry, entry_at, reads) = self.positions.locate(index)?;
        if index < self.next || entry > self.next {
            for (at, len) in reads {
                if len > 0 {
                    touch.touch(Part::Positions, self.at + at, len);
                }
            }
            self.next = entry;
            self.next_at = entry_at;
        }
        if let Some(m) = self.positions.mask_at(index) {
            touch.touch(Part::Positions, self.at + m, 4);
        }
        let from = self.next_at;
        let mut p = self
            .positions
            .skip_entries(self.next, self.next_at, index)?;
        p = self.positions.read_entry(index, p, out)?;
        self.next = index + 1;
        self.next_at = p;
        touch.touch(Part::Positions, self.at + from, p - from);
        Ok(())
    }
}

/// A top-level span's positions check.
struct SpanCheck<'a> {
    /// Per span slot, its term, and the first slot of the same term (a
    /// phrase repeating a word reads its positions once).
    slots: Vec<usize>,
    first: Vec<usize>,
    cursors: Vec<PosCursor<'a>>,
    solver: SpanSolver,
    plan: Option<PhrasePlan>,
    positions: Vec<Vec<u32>>,
    read: Vec<bool>,
}

impl SpanCheck<'_> {
    /// Reads slot `slot`'s positions unless read: its term's first slot's,
    /// copied, when an earlier slot holds the same term.
    fn read_slot(&mut self, slot: usize, index: &[u32], touch: &mut impl Touch) -> Result<()> {
        if std::mem::replace(&mut self.read[slot], true) {
            return Ok(());
        }
        let first = self.first[slot];
        if first == slot {
            return self.cursors[slot].read(index[slot], &mut self.positions[slot], touch);
        }
        self.read_slot(first, index, touch)?;
        let (head, tail) = self.positions.split_at_mut(slot);
        tail[0].clone_from(&head[first]);
        Ok(())
    }

    /// Whether the candidate whose posting index in each slot's term is
    /// `index[slot]` holds the span.
    fn holds(&mut self, index: &[u32], touch: &mut impl Touch) -> Result<bool> {
        self.read.fill(false);
        if let Some(plan) = self.plan.take() {
            let mut kept = true;
            for step in plan.steps() {
                if let Err(error) = self.read_slot(step.slot, index, touch) {
                    self.plan = Some(plan);
                    return Err(error);
                }
                if let Some(pair) = step.pair
                    && !plan.pair_keeps(pair, &self.positions)
                {
                    kept = false;
                    break;
                }
            }
            self.plan = Some(plan);
            if !kept {
                return Ok(false);
            }
        }
        for slot in 0..index.len() {
            self.read_slot(slot, index, touch)?;
        }
        Ok(self.solver.intervals(&self.positions).next().is_some())
    }
}

/// How a candidate holding the walk's terms is confirmed as a match.
enum Verify<'a> {
    /// Holding them is matching (a flat AND or a term).
    Flat,
    /// A top-level span: positions.
    Span(Box<SpanCheck<'a>>),
    /// Anything else: the node evaluated at the candidate, in slot order.
    Node,
}

/// A ranked walk over one segment: led by required terms (`run`) or a
/// disjunction of the scoring terms (`run_or`).
struct Walk<'s, 'a, T: Touch> {
    segment: &'s Segment<'a>,
    geometry: &'s Geometry,
    node: &'s Node,
    /// The rows kept so far, shared with the walks over other sources, and
    /// the check a row passes as it is kept (heap visibility, the scan's
    /// other restrictions).
    top: &'s mut TopRows,
    /// [`TopRows::bar`], kept as the walk pushes: the walk is the only
    /// writer of `top` while it runs, and the bar is read per word and
    /// per candidate.
    bar: Option<(f32, Tid)>,
    visibility: &'s mut dyn Visibility,
    touch: &'s mut T,
    terms: Vec<Option<TermSet<'a>>>,
    /// Per term (by index into `terms`), its members in the group at hand.
    mems: Vec<Mem<'a>>,
    sc: Vec<Sc<'s, 'a>>,
    /// Required terms, rarest first.
    req: Vec<usize>,
    /// With common terms, the scoring terms (indexes into `sc`), rarest
    /// first.
    sc_order: Vec<usize>,
    /// Whether a scoring term is common ([`Sc::make_common`]).
    common_terms: bool,
    /// Per scoring term: whether it is required; whether it holds the group
    /// at hand, and the candidate at hand, and that candidate's block; its
    /// bound over the sub-range at hand.
    sc_required: Vec<bool>,
    present: Vec<bool>,
    /// A disjunction's present terms in the group at hand, and those the
    /// candidate at hand holds, both in the scorer's order.
    present_list: Vec<usize>,
    held_list: Vec<usize>,
    blocks: Vec<usize>,
    sub_bounds: Vec<f32>,
    plan: Plan,
    /// A disjunction's group at hand: per scoring term room for a row of
    /// the group's words holding its members (unless read in place, see
    /// [`RowAt`]), and its row and index cursor.
    rows: Vec<u64>,
    row_state: Vec<Row>,
    /// Per scoring term, whether its row holds the group at hand.
    row_loaded: Vec<bool>,
    /// A disjunction's group at hand: per present term with a positive
    /// bound there, the term, its row, that bound and its bound over the
    /// sub-range at hand; and its word at hand.
    or_terms: Vec<usize>,
    /// Those terms' rows, bounds over the group and over the sub-range at
    /// hand, and words at the word at hand (as the kernels take them).
    or_rows: Vec<RowAt>,
    or_bounds: Vec<f32>,
    or_sub: Vec<f32>,
    or_held: Vec<u64>,
    /// A disjunction's candidate at hand: per scoring term it holds, the
    /// bound of the block or, once its bucket is read, of the bucket (see
    /// [`Walk::or_stage`]).
    held_bound: Vec<f32>,
    /// Per scoring term, the candidate at hand's bucket ([`NO_BUCKET`]
    /// when it does not hold the term).
    buckets: Vec<u8>,
    /// Per scoring term, the candidate at hand's posting index in it (read
    /// with its bucket); and per term, its index into `sc` (`usize::MAX`
    /// for a term that does not score).
    held_index: Vec<u32>,
    term_sc: Vec<usize>,
    /// The window: the last slot of every scoring term's footer block at
    /// the slot it was set at, and its bound; per scoring term, its block's
    /// largest bucket ([`NO_BUCKET`] when it holds nothing from the slot
    /// on).
    window_end: Option<u32>,
    window_bound: f32,
    window_buckets: Vec<u8>,
    /// Per scoring term, its bound over the window.
    window_terms: Vec<f32>,
    /// The window's length bound parts: its terms' numerators summed, and
    /// the least of their denominators and length factors.
    window_parts: (f64, f64, f64),
    /// A required term whose record carries its documents' lengths, the
    /// led walk reads them from.
    inline: Option<usize>,
    /// A phrase's scoring terms it uses more than once, by index into `sc`,
    /// each with the least bucket of a count reaching their number of uses.
    repeats: Vec<(usize, u8)>,
    answer: RankedAnswer,
    verify: Verify<'a>,
    /// A span's candidate at hand: its posting index in each span slot's
    /// term.
    span_index: Vec<u32>,
    /// The group's staged candidates: (bound, local slot, length or
    /// `u32::MAX`), and per one its buckets (one per scoring term) and, for
    /// a span, its posting indexes; and the order they are finished in.
    staged: Vec<(f32, u32, u32)>,
    staged_buckets: Vec<u8>,
    staged_span: Vec<u32>,
    staged_order: Vec<u32>,
    cands: Vec<u32>,
    /// The group at hand's sieve ([`Walk::sieve`]): the terms it reads,
    /// per scoring term what bounds a candidate before that term's bucket
    /// is read, and its scratch.
    sieve_terms: Vec<SieveTerm>,
    sieve_ub: Vec<f32>,
    sieve_vals: Vec<f32>,
    sieve_ranks: Vec<u32>,
    sieve_buckets: Vec<u8>,
    sieve_held: Vec<u8>,
    sieve_cands: Vec<u32>,
    /// The sieve's terms read per chunk: fewer once one dropped too few.
    sieve_depth: usize,
    /// Whether the group's candidate words were sieved by the first term
    /// already ([`Walk::sieve_words`]): its pass over a chunk then keeps
    /// nearly all, which says nothing of what it drops.
    sieve_pre: bool,
    sieve_prefix: Vec<u32>,
    /// [`Walk::sieve_words`]' scratch: the first term's buckets gathered
    /// from the TF tail, and their pass bits.
    sieve_fields: Vec<u64>,
    sieve_pass: Vec<u64>,
    sieve_locals: Vec<u32>,
    sieve_left: Vec<u32>,
    words: Vec<u64>,
    /// A bitmap over a group's slots, all clear between uses: a list's
    /// members marked to filter candidates by ([`bits::intersect_marked`]).
    marks: Vec<u64>,
    /// The node check's own cursors (the walk moves the terms' sparse
    /// cursors past each group), and its scratch.
    node_terms: Vec<Option<TermSet<'a>>>,
    positions: Vec<Vec<u32>>,
    /// The blob the segment is read from in place, and the mark of the
    /// pages pinned before the walk opened its terms (see
    /// [`Walk::group_done`]); the ranges of the blob the terms' records keep
    /// slices of.
    frame: Option<(&'a LazyBlob, usize)>,
    held: Vec<(usize, usize)>,
}

impl<'a, T: Touch> Walk<'_, 'a, T> {
    /// Records the footer bytes the walk read, and adds what it used of
    /// its terms' footers and directories to this thread's
    /// [`segment::tinshape::segment::MemoCounts`].
    fn count_metadata(&mut self) {
        for s in &self.sc {
            let read = s.footer.read();
            if read > 0 {
                let set = self.terms[s.term].as_ref().expect("scoring");
                self.touch
                    .touch(Part::Footer, set.at + set.postings.footer_at, read);
            }
        }
        let used: u64 = self
            .sc
            .iter()
            .map(|s| s.computed + s.bounds.iter().filter(|b| !b.is_nan()).count() as u64)
            .sum();
        let blocks_reached: u64 = self.sc.iter().map(|s| s.footer.decoded() as u64).sum();
        let blocks_whole: u64 = self.sc.iter().map(|s| s.footer.blocks() as u64).sum();
        let bytes_whole: u64 = self
            .sc
            .iter()
            .filter_map(|s| self.terms[s.term].as_ref())
            .map(|set| (set.postings.footer.len() + set.postings.directory_bytes().len()) as u64)
            .sum();
        let (mut reached, mut loaded) = (0u64, 0u64);
        for (set, mem) in self.terms.iter().zip(&self.mems) {
            if let Some(set) = set
                && matches!(set.postings.form, Form::Grouped(_))
            {
                reached += (mem.hint + 1).min(set.group_count()) as u64;
                loaded += u64::from(mem.used);
            }
        }
        segment::tinshape::segment::count_memo(|c| {
            c.blocks_used += used;
            c.blocks_reached += blocks_reached;
            c.footer_blocks_whole += blocks_whole;
            c.footer_bytes_whole += bytes_whole;
            c.entries_reached += reached;
            c.entries_used += loaded;
        });
    }

    #[inline]
    fn threshold(&self) -> Option<f32> {
        self.bar.map(|bar| bar.0)
    }

    /// Whether no match of ctid `from()` or later scoring at most `bound`
    /// can be kept: it scores below the bar, or ties it from a later ctid
    /// (rows rank by score, then ctid).
    ///
    /// Bounds are exact: each term's is an `f32` score of the scorer's
    /// own at a bucket and length no worse than the match's, and they are
    /// summed in `f32` in the scorer's order, the order [`Walk::process`]
    /// sums the scores in. Correctly rounded addition is monotonic in both
    /// operands, so the summed bound is at least the summed score, bit for
    /// bit; a term a match lacks adds zero to its score.
    #[inline]
    fn cut(&self, bound: f32, from: impl FnOnce() -> Tid) -> bool {
        cut_by(self.bar, bound, from)
    }

    /// Whether `(total, tid)` would be kept.
    #[inline]
    fn admits(&self, total: f32, tid: Tid) -> bool {
        self.bar
            .is_none_or(|(score, at)| Ranked(total, tid) < Ranked(score, at))
    }

    /// Keeps `(total, tid)` if it ranks above the bar and passes the check.
    fn push(&mut self, total: f32, tid: Tid) {
        let row = Ranked(total, tid);
        if self.admits(total, tid) && self.visibility.visible(tid) {
            self.top.push(row);
            self.bar = self.top.bar();
        }
    }

    /// With nothing scoring, whether no match from `tid` on can be kept:
    /// each scores zero, and the bar ranks before it (rows of other
    /// sources may sit anywhere in ctid order).
    fn unscored_done(&self, tid: Tid) -> bool {
        !self.admits(0.0, tid)
    }

    /// Walks the groups the rarest required term holds. A group whose
    /// scoring terms' bounds cannot reach the threshold is skipped unread;
    /// in any other the required terms' members are intersected whole (word
    /// by word where every one is a grid, else term by term from the fewest)
    /// and the candidates left are bounded, scored and admitted in order.
    fn run(&mut self) -> Result<()> {
        let lead = self.req[0];
        let n = self.sc.len();
        let mut from = 0u32;
        loop {
            // The lead's next group, from its directory (or its list): its
            // members are read only once the group's bound passes.
            let set = self.terms[lead].as_mut().expect("the lead");
            let Some(g) = next_group(
                set,
                self.geometry,
                from,
                &mut self.mems[lead].hint,
                self.touch,
            ) else {
                break;
            };
            from = g + 1;
            let group = self.geometry.groups[g as usize];
            let (base, end) = (group.slot_base, group.slot_base + group.slots() - 1);
            if n == 0 && self.unscored_done(self.geometry.tid_in(g as usize, 0)) {
                // Nothing scores: every later match ties at zero and ranks
                // after the bar.
                break;
            }
            self.answer.windows += 1;
            if self.group_below(g, base, end) {
                self.answer.windows_pruned += 1;
                continue;
            }
            if self.mems[lead].loaded != g + 1 {
                let set = self.terms[lead].as_mut().expect("the lead");
                load(
                    set,
                    self.geometry,
                    g,
                    &mut self.mems[lead],
                    usize::MAX,
                    self.touch,
                )?;
            }
            // The lead's next container is read first in its group, which
            // waited on memory for it: asked for now, it arrives while this
            // group is walked.
            let mem = &self.mems[lead];
            if mem.entry != usize::MAX {
                let set = self.terms[lead].as_ref().expect("the lead");
                set.prefetch_next(mem.hint, LEAD_PREFETCH_LINES);
            }
            let more = if self.all_grids(g) {
                self.dense_group(g)?
            } else {
                self.group_and(g)?
            };
            if !more {
                break;
            }
            self.group_done();
        }
        Ok(())
    }

    /// Ends a group's reads: the pages pinned since the walk's mark are
    /// released, all but those the terms' records borrow and the few read
    /// last the blob keeps (see [`LazyBlob::release_since`]), once more
    /// than those are held. A group's reads leave nothing behind that
    /// borrows its pages: its candidates were scored, its phrase
    /// candidates checked, and the terms' members (a grid or an
    /// Elias-Fano list read in place) are forgotten here; positions, TF
    /// tails and the DL sidecar are read a value at a time, and a
    /// disjunction's rows (a grid read in place) are loaded afresh in the
    /// next group they are read in. What the walk keeps across
    /// groups holds no slice (directories, decoded; positions streams,
    /// read when asked), lies in [`Walk::held`] (a sparse term's list,
    /// inline lengths) or is held for this release: a scoring term's
    /// footer, decoded lazily, reads on from the page it read last
    /// ([`LazyFooter::borrowed`]), one page per term at most.
    #[inline]
    fn group_done(&mut self) {
        let Some((blob, mark)) = self.frame else {
            return;
        };
        if !blob.over_keep(mark) {
            return;
        }
        debug_assert!(self.staged.is_empty());
        for mem in &mut self.mems {
            mem.loaded = 0;
            mem.kind = Kind::List;
        }
        let held = self.held.len();
        self.held
            .extend(self.sc.iter().filter_map(|s| s.footer.borrowed()));
        // SAFETY: nothing read in place since the mark is used after but
        // what lies in `held` (see above): the members' slices were
        // dropped just now, and the footers' windows are held.
        unsafe { blob.release_since(mark, &self.held) };
        self.held.truncate(held);
    }

    /// Whether no match in slots `base..=end` of group `g` can be kept by
    /// the scoring terms' bounds there (see [`Walk::cut`]). Bounds are not
    /// negative, so the sum stops once it passes the threshold. With
    /// nothing scoring every match scores zero.
    #[inline]
    fn group_below(&mut self, g: u32, base: u32, end: u32) -> bool {
        let Some(theta) = self.threshold() else {
            return false;
        };
        if !self.common_terms {
            let mut bound = 0.0_f32;
            for s in &mut self.sc {
                bound += s.range_bound(base, end);
                if bound > theta {
                    return false;
                }
            }
            let geometry = self.geometry;
            return self.cut(bound, || geometry.tid_in(g as usize, 0));
        }
        // With common terms, rarest first (theirs last, by their records):
        // a sum past the threshold says no, in any order; one that is not
        // is summed again in the scorer's order.
        let mut partial = 0.0_f32;
        for k in 0..self.sc_order.len() {
            let i = self.sc_order[k];
            let s = &mut self.sc[i];
            let v = match s.common_bound() {
                Some(bound) => bound,
                None => s.range_bound(base, end),
            };
            self.sub_bounds[i] = v;
            partial += v;
            if partial > theta {
                return false;
            }
        }
        let mut bound = 0.0_f32;
        for v in &self.sub_bounds {
            bound += *v;
        }
        let geometry = self.geometry;
        self.cut(bound, || geometry.tid_in(g as usize, 0))
    }

    /// Whether every required term holds group `g` as a grid.
    fn all_grids(&mut self, g: u32) -> bool {
        for r in 0..self.req.len() {
            let t = self.req[r];
            let set = self.terms[t].as_ref().expect("required");
            if matches!(set.postings.form, Form::Sparse(_)) {
                return false;
            }
            let mem = &mut self.mems[t];
            match set.find(g, &mut mem.hint) {
                Some(entry) if matches!(entry.src, Src::Container { entry, .. } if entry.kind == KIND_GRID) =>
                    {}
                _ => return false,
            }
        }
        true
    }

    /// Intersects the required terms' members in group `g`, which the lead
    /// holds and has loaded: the lead's members, then each other required
    /// term's, rarest first, only while candidates are left (in most groups
    /// a rare lead's few members leave none after one or two terms). A grid
    /// filters them by bit, an Elias-Fano list much longer than they are by
    /// probing its high bits in place, any other list by a merge. Then walks
    /// the candidates left.
    fn group_and(&mut self, g: u32) -> Result<bool> {
        let mut cands = std::mem::take(&mut self.cands);
        cands.clear();
        let lead = &self.mems[self.req[0]];
        match &lead.kind {
            Kind::Grid(bytes) => kernels::set_bits_bytes(bytes, &mut cands),
            Kind::List => cands.extend_from_slice(&lead.list),
            Kind::Cursor { .. } | Kind::Array(_) => unreachable!("a lead is loaded whole"),
        }
        for r in 1..self.req.len() {
            if cands.is_empty() {
                break;
            }
            let t = self.req[r];
            if self.mems[t].loaded != g + 1 {
                let set = self.terms[t].as_mut().expect("required");
                let mem = &mut self.mems[t];
                let sparse = matches!(set.postings.form, Form::Sparse(_));
                if sparse && next_group(set, self.geometry, g, &mut mem.hint, self.touch) != Some(g)
                {
                    cands.clear();
                    break;
                }
                let probe_over = if sparse {
                    usize::MAX
                } else {
                    SEEK_RATIO * cands.len()
                };
                if !load(set, self.geometry, g, mem, probe_over, self.touch)? {
                    cands.clear();
                    break;
                }
            }
            let mem = &self.mems[t];
            match &mem.kind {
                Kind::Grid(bytes) => {
                    bits::retain(&mut cands, |l| bytes[l as usize / 8] >> (l % 8) & 1 == 1);
                }
                Kind::Array(bytes) => kernels::array_retain(bytes, &mut cands),
                Kind::Cursor { cursor, offset: 0 } => cursor.list().retain_members(&mut cands),
                Kind::Cursor { .. } => {
                    let mem = &mut self.mems[t];
                    cands.retain(|l| mem.find(*l).is_some());
                }
                Kind::List => {
                    let words = self.geometry.groups[g as usize].words();
                    if self.marks.len() < words {
                        self.marks.resize(words, 0);
                    }
                    bits::intersect_marked(&mut cands, &mem.list, &mut self.marks);
                }
            }
        }
        let more = self.walk_candidates(g, &cands)?;
        self.cands = cands;
        Ok(more)
    }

    /// Bounds, scores and admits group `g`'s candidates `cands` (local
    /// slots, ascending); false once nothing later can enter the top k. A span's candidates, with
    /// terms to score, are bounded by their buckets first and scored best
    /// first (see [`Walk::finish_staged`]): its top k fill only with
    /// confirmed matches, which its best-bounded candidates find soonest,
    /// and each one scored may mean a positions check. Any other walk scores
    /// in ctid order: on TIN's corpus staging its candidates scored fewer
    /// of them but read no fewer DL pages (the sidecar holds about a
    /// group's lengths per page), and took longer.
    fn walk_candidates(&mut self, g: u32, cands: &[u32]) -> Result<bool> {
        let staging = !self.sc.is_empty() && matches!(self.verify, Verify::Span(_));
        if cands.len() >= SIEVE_MIN && self.sieve_led(g)? {
            return self.walk_sieved(g, cands, staging);
        }
        self.walk_listed(g, cands, staging)
    }

    /// [`Walk::walk_candidates`] with no sieve: each candidate bounded alone.
    fn walk_listed(&mut self, g: u32, cands: &[u32], staging: bool) -> Result<bool> {
        let base = self.geometry.groups[g as usize].slot_base;
        let mut more = true;
        let mut at = 0;
        while at < cands.len() {
            let local = cands[at];
            at += 1;
            if self.window(g, base, local) {
                // Skip the rest of the window unread.
                match self.window_end {
                    Some(e) if e < u32::MAX => {
                        at += cands[at..].partition_point(|l| base + *l <= e);
                        continue;
                    }
                    _ => break,
                }
            }
            if staging {
                self.stage_into(g, local)?;
                if self.staged.len() >= STAGE_CHUNK {
                    self.finish_staged(g)?;
                }
            } else if !self.process(g, local)? {
                more = false;
                break;
            }
        }
        if staging {
            self.finish_staged(g)?;
        }
        Ok(more)
    }

    /// [`Walk::walk_candidates`] with the candidates sieved
    /// ([`Walk::sieve`]) a chunk at a time: only those left are bounded and
    /// scored alone, in the same order.
    fn walk_sieved(&mut self, g: u32, cands: &[u32], staging: bool) -> Result<bool> {
        let base = self.geometry.groups[g as usize].slot_base;
        let mut left = std::mem::take(&mut self.sieve_cands);
        let mut more = true;
        'chunks: for chunk in cands.chunks(SIEVE_CHUNK) {
            left.clear();
            left.extend_from_slice(chunk);
            self.sieve(g, &mut left)?;
            for &local in &left {
                // A window cut skips its candidates (each is cut by it
                // again, unread); with no term left, the rest of the group.
                if self.window(g, base, local) {
                    match self.window_end {
                        Some(e) if e < u32::MAX => continue,
                        _ => break 'chunks,
                    }
                }
                if staging {
                    self.stage_into(g, local)?;
                    if self.staged.len() >= STAGE_CHUNK {
                        self.finish_staged(g)?;
                    }
                } else if !self.process(g, local)? {
                    more = false;
                    break 'chunks;
                }
            }
        }
        self.sieve_cands = left;
        if staging {
            self.finish_staged(g)?;
        }
        Ok(more)
    }

    /// Sets up the sieve of a led walk's group `g` ([`Walk::sieve`]): its
    /// grouped scoring terms whose members there are a grid or a list are
    /// read; any other bounds every candidate by its bound over the group
    /// (a grouped term's entry's, or a sparse one's record's). False when
    /// there is no bar yet or no term to read.
    fn sieve_led(&mut self, g: u32) -> Result<bool> {
        if self.bar.is_none() {
            return Ok(false);
        }
        self.sieve_terms.clear();
        self.sieve_ub.clear();
        for i in 0..self.sc.len() {
            let t = self.sc[i].term;
            self.ensure(t, g)?;
            let mem = &self.mems[t];
            let s = &mut self.sc[i];
            let Some(group) = s.group_entry(mem.entry) else {
                let ub = if s.grouped() {
                    // A grouped term without the group: no candidate holds it.
                    0.0
                } else {
                    let set = self.terms[t].as_ref().expect("scoring");
                    let most = TfBucket::new(set.max_bucket).expect("a valid bucket");
                    s.scorer.bound_through(most, 0)
                };
                self.sieve_ub.push(ub);
                continue;
            };
            self.sieve_ub.push(s.bound(mem.entry));
            if matches!(mem.kind, Kind::Grid(_) | Kind::List) {
                self.sieve_terms.push(SieveTerm {
                    i,
                    entry: mem.entry,
                    group,
                    row: false,
                    prefix: usize::MAX,
                    table: [f32::NAN; BUCKET_COUNT],
                    floors: [0; BUCKET_COUNT],
                });
            }
        }
        Ok(self.ready_sieve())
    }

    /// Orders the sieve's terms by their bounds over the group, highest
    /// first (a candidate's bucket of a term whose bound is high is likelier
    /// to take the most off its bound), and says whether the sieve is likely
    /// to drop many: whether the first term's bucket 0, the others at their
    /// bounds over the group, leaves a candidate below the bar. With many
    /// terms (a long disjunction), or words rarer than a `full_score`'s
    /// dense ones, it does not, and a candidate's own check, which reads its
    /// terms' buckets best first and bounds them at one length, is left to
    /// bound it.
    fn ready_sieve(&mut self) -> bool {
        let ub = &self.sieve_ub;
        self.sieve_terms
            .sort_by(|a, b| ub[b.i].total_cmp(&ub[a.i]).then(a.i.cmp(&b.i)));
        self.sieve_prefix.clear();
        self.sieve_depth = self.sieve_terms.len();
        self.sieve_pre = false;
        let Some(first) = self.sieve_terms.first_mut() else {
            return false;
        };
        let Some((score, _)) = self.bar else {
            return false;
        };
        let rest: f32 = ub
            .iter()
            .enumerate()
            .map(|(i, u)| if i == first.i { 0.0 } else { *u })
            .sum();
        if rest >= score {
            return false;
        }
        // Most postings count one (bucket 0): unless those can drop, few
        // can. An estimate (summed in another order); the sieve sums
        // exactly.
        let (floor, v) = self.sc[first.i].floor_kept(first.entry, 0);
        first.table[0] = v;
        first.floors[0] = floor;
        rest + v < score
    }

    /// Counts the members per word of sieve term `k`'s bitmap, once per
    /// group, when it is first read.
    fn sieve_prefix_of(&mut self, k: usize) {
        let st = &mut self.sieve_terms[k];
        if st.prefix != usize::MAX {
            return;
        }
        let bytes = if st.row {
            self.row_state[st.i].at.bytes()
        } else {
            match &self.mems[self.sc[st.i].term].kind {
                Kind::Grid(bytes) => bytes,
                _ => return,
            }
        };
        st.prefix = self.sieve_prefix.len();
        sieve_prefix(bytes, &mut self.sieve_prefix);
    }

    /// Per bucket of sieve term `i`, the first (and [`NO_BUCKET`] last), what
    /// the bar does to a candidate whose bound by that term is `value` of its
    /// bucket, every other term at its bound over the group: the bound
    /// summed in the scorer's order, as [`Walk::sieve`] sums it.
    fn first_verdicts(&self, i: usize, value: impl Fn(u8) -> f32) -> [Verdict; BUCKET_COUNT + 2] {
        let mut verdict = [Verdict::Keep; BUCKET_COUNT + 2];
        for (at, v) in verdict.iter_mut().enumerate() {
            let bucket = match at {
                b if b <= BUCKET_COUNT => b as u8,
                _ => NO_BUCKET,
            };
            let mut bound = 0.0_f32;
            for (j, &u) in self.sieve_ub.iter().enumerate() {
                bound += if j == i { value(bucket) } else { u };
            }
            *v = match self.bar.map(|(score, _)| bound.total_cmp(&score)) {
                Some(std::cmp::Ordering::Less) => Verdict::Drop,
                Some(std::cmp::Ordering::Equal) => Verdict::Tie,
                _ => Verdict::Keep,
            };
        }
        verdict
    }

    /// Clears from `words`, group `g`'s candidates (a bit per slot), those
    /// the sieve's first term drops by its bucket alone (the first pass of
    /// [`Walk::sieve`], a tie kept), when that term's members are a bitmap:
    /// a word at a time, each candidate's bucket read where the bitmap ranks
    /// it, before any candidate is listed. Most of a dense pair's
    /// candidates fall there (at 150M rows, "is to" kept one in seven), and
    /// listing them, then sieving each, cost several times the read.
    fn sieve_words(&mut self, words: &mut [u64]) -> Result<()> {
        let Some(&st) = self.sieve_terms.first() else {
            return Ok(());
        };
        let bytes = if st.row {
            self.row_state[st.i].at.bytes()
        } else {
            match &self.mems[self.sc[st.i].term].kind {
                Kind::Grid(bytes) => *bytes,
                _ => return Ok(()),
            }
        };
        let e = st.group;
        let width = u32::from(e.width);
        // Every bucket the term's members there can have, bounded once.
        let most = if width == 0 {
            0
        } else {
            ((1u32 << width) - 1).min(BUCKET_COUNT as u32 - 1)
        };
        let (mut table, mut floors) = (st.table, st.floors);
        for bucket in 0..=most as u8 {
            let b = usize::from(bucket);
            if table[b].is_nan() {
                (floors[b], table[b]) = self.sc[st.i].floor_kept(st.entry, bucket);
            }
        }
        self.sieve_terms[0].table = table;
        self.sieve_terms[0].floors = floors;
        let ub = self.sieve_ub[st.i];
        let verdict = self.first_verdicts(st.i, |bucket| match bucket {
            NO_BUCKET => 0.0,
            b if usize::from(b) < BUCKET_COUNT => table[usize::from(b)],
            _ => ub,
        });
        // The buckets (one past the table's as `BUCKET_COUNT`) whose
        // candidates stay, as bits; and whether one lacking the term does.
        let mut pass = 0u32;
        for (b, v) in verdict[..=BUCKET_COUNT].iter().enumerate() {
            pass |= u32::from(!matches!(v, Verdict::Drop)) << b;
        }
        let absent = if matches!(verdict[BUCKET_COUNT + 1], Verdict::Drop) {
            0
        } else {
            !0
        };
        let t = self.sc[st.i].term;
        let set = self.terms[t].as_ref().expect("scoring");
        // The group's buckets, eight bytes past the last where the tail has
        // them: whole loads.
        let (from, to) = if width == 0 || e.count == 0 {
            (0, 0)
        } else {
            (
                e.bucket_bit(e.first) / 8,
                (e.bucket_bit(e.first + e.count - 1) + width as usize).div_ceil(8),
            )
        };
        let tail = set
            .postings
            .tf
            .get(from, (to + 7).min(set.postings.tf.len()))?;
        let start = if width == 0 {
            0
        } else {
            e.tf_bit as usize - from * 8
        };
        // The members' ranks the candidates need: from the first member of
        // the first word holding a candidate member (`lo`) to the last
        // candidate member (`hi`, past it).
        let (mut run, mut lo, mut hi) = (0u32, u32::MAX, 0u32);
        for (w, &c) in words.iter().enumerate() {
            let m = bits::word(bytes, w);
            let held = c & m;
            if held != 0 {
                lo = lo.min(run);
                hi = run + (m & (u64::MAX >> held.leading_zeros())).count_ones();
            }
            run += m.count_ones();
        }
        // Their buckets as pass bits, a bit per rank from `lo` (a word to
        // spare), laid onto the candidate words: a 4-bit field's by a table
        // lookup, a word's members' bits deposited at once.
        self.sieve_pass.clear();
        if lo < hi {
            let n = (hi - lo) as usize;
            let out_words = n.div_ceil(64);
            self.sieve_fields.clear();
            if width > 0 {
                let end = start + hi as usize * width as usize;
                if end.div_ceil(8) > tail.len() {
                    return Err(Error::Corrupt("a group's buckets past the TF tail"));
                }
                let at = start + lo as usize * width as usize;
                kernels::gather_bits(tail, at, n * width as usize, &mut self.sieve_fields);
                self.sieve_fields.resize(out_words * width as usize, 0);
            }
            self.sieve_pass.resize(out_words + 1, 0);
            // Every bucket a field holds is at most 15, below the table's.
            const _: () = assert!(BUCKET_COUNT >= 16);
            kernels::pass_bits(
                &self.sieve_fields,
                width,
                pass as u16,
                &mut self.sieve_pass[..out_words],
            );
        }
        let dropped = kernels::deposit_pass(words, bytes, &self.sieve_pass, lo, absent);
        if width > 0 && lo < hi {
            let (a, b) = (
                start + lo as usize * width as usize,
                start + hi as usize * width as usize,
            );
            self.touch.touch(
                Part::TfTail,
                set.at + set.postings.tf_at + from + a / 8,
                b.div_ceil(8) - a / 8,
            );
        }
        // Those dropped were examined (bounded) as those left will be.
        self.answer.candidates += dropped;
        self.sieve_pre = true;
        Ok(())
    }

    /// Sieves `cands`, local slots of group `g` (ascending, none before the
    /// members' cursors), by their buckets: each candidate is bounded by
    /// the terms it holds, a term set up ([`Walk::sieve_led`],
    /// [`Walk::or_sieve`]) by what its bucket scores at most at the
    /// shortest document of the group holding that bucket or more (the
    /// term's own bound at its bucket, [`Sc::floor_kept`]), any other by
    /// its bound over the group; a term at a time, the candidates the bar
    /// cuts dropped after each. Those left are kept, in order.
    ///
    /// Exact: a candidate's own check ([`Walk::read_buckets`],
    /// [`Walk::or_stage`]) bounds it by no more than these, summed in the
    /// same order, so one the sieve drops it would cut too (the bar only
    /// rises). Reading a group's buckets for many candidates at once, from
    /// one slice of the TF tail, costs a fraction of reading them one
    /// candidate at a time: with a dense term, whose frontier is its
    /// group's, most of a group's candidates fall at their buckets.
    fn sieve(&mut self, g: u32, cands: &mut Vec<u32>) -> Result<()> {
        if self.sieve_depth == 0 {
            return Ok(());
        }
        let (n, m) = (self.sc.len(), cands.len());
        // Per term, its bound of each candidate (`m` a term): its bound
        // over the group until its bucket is read.
        let mut vals = std::mem::take(&mut self.sieve_vals);
        vals.clear();
        for &ub in &self.sieve_ub {
            vals.resize(vals.len() + m, ub);
        }
        // The candidates left, by their place in `cands`, and their slots.
        let mut left = std::mem::take(&mut self.sieve_left);
        left.clear();
        left.extend(0..cands.len() as u32);
        let mut locals = std::mem::take(&mut self.sieve_locals);
        locals.clear();
        locals.extend_from_slice(cands);
        // Per term, each candidate's bucket once read (`m` a term).
        let mut held = std::mem::take(&mut self.sieve_held);
        held.clear();
        held.resize(n * m, NO_BUCKET);
        let geometry = self.geometry;
        // The terms read for every candidate left.
        let mut passes = 0;
        for k in 0..self.sieve_depth {
            if left.is_empty() {
                break;
            }
            self.sieve_prefix_of(k);
            let st = self.sieve_terms[k];
            let t = self.sc[st.i].term;
            let set = self.terms[t].as_ref().expect("scoring");
            let (members, first) = if st.row {
                let r = &self.row_state[st.i];
                let prefix = &self.sieve_prefix[st.prefix..];
                let bytes = r.at.bytes();
                (SieveMembers::Bits { bytes, prefix }, r.first)
            } else {
                let mem = &self.mems[t];
                let members = match &mem.kind {
                    Kind::Grid(bytes) => SieveMembers::Bits {
                        bytes,
                        prefix: &self.sieve_prefix[st.prefix..],
                    },
                    Kind::List => SieveMembers::List {
                        list: &mem.list,
                        pos: mem.pos,
                    },
                    Kind::Cursor { .. } | Kind::Array(_) => {
                        unreachable!("a sieved term is not sought")
                    }
                };
                (members, mem.first)
            };
            let (seen, read) = match members {
                SieveMembers::Bits { bytes, prefix } => {
                    debug_assert_eq!(first, st.group.first);
                    sieve_bits_buckets(
                        bytes,
                        prefix,
                        &st.group,
                        set.postings.tf,
                        &locals,
                        &mut self.sieve_buckets,
                    )?
                }
                SieveMembers::List { list, pos } => {
                    sieve_ranks(list, pos, first, &locals, &mut self.sieve_ranks);
                    sieve_buckets(
                        &st.group,
                        set.postings.tf,
                        &self.sieve_ranks,
                        &mut self.sieve_buckets,
                    )?
                }
            };
            if !read.is_empty() {
                self.touch.touch(
                    Part::TfTail,
                    set.at + set.postings.tf_at + read.start,
                    read.len(),
                );
            }
            // The buckets seen, bounded once per group.
            let (mut table, mut floors) = (st.table, st.floors);
            let mut fresh = seen & ((1 << BUCKET_COUNT) - 1);
            while fresh != 0 {
                let bucket = fresh.trailing_zeros() as u8;
                fresh &= fresh - 1;
                let b = usize::from(bucket);
                if table[b].is_nan() {
                    (floors[b], table[b]) = self.sc[st.i].floor_kept(st.entry, bucket);
                }
            }
            self.sieve_terms[k].table = table;
            self.sieve_terms[k].floors = floors;
            let ub = self.sieve_ub[st.i];
            let value = |bucket: u8| match bucket {
                NO_BUCKET => 0.0,
                b if usize::from(b) < BUCKET_COUNT => table[usize::from(b)],
                _ => ub,
            };
            let bar = self.bar;
            if k == 0 {
                // The first term's bucket is all that varies yet: whether
                // the bar cuts a candidate is its bucket's, but for a tie.
                let verdict = self.first_verdicts(st.i, value);
                let column = &mut vals[st.i * m..(st.i + 1) * m];
                let buckets = &mut held[st.i * m..(st.i + 1) * m];
                let mut kept = 0;
                for j in 0..left.len() {
                    let bucket = self.sieve_buckets[j];
                    let local = locals[j];
                    let keep = match verdict[usize::from(bucket.min(BUCKET_COUNT as u8 + 1))] {
                        Verdict::Keep => true,
                        Verdict::Drop => false,
                        // Rows rank by score, then ctid (see `Walk::cut`).
                        Verdict::Tie => {
                            bar.is_some_and(|(_, tid)| geometry.tid_in(g as usize, local) < tid)
                        }
                    };
                    let c = left[j];
                    column[c as usize] = value(bucket);
                    buckets[c as usize] = bucket;
                    left[kept] = c;
                    locals[kept] = local;
                    kept += usize::from(keep);
                }
                // A term that drops too few is not worth its reads: the
                // group's later chunks stop before it.
                if kept * 8 > left.len() * 7 && !self.sieve_pre {
                    self.sieve_depth = k;
                }
                left.truncate(kept);
                locals.truncate(kept);
                passes += 1;
                if self.sieve_depth == k {
                    break;
                }
                continue;
            }
            let column = &mut vals[st.i * m..(st.i + 1) * m];
            let buckets = &mut held[st.i * m..(st.i + 1) * m];
            for (&c, &bucket) in left.iter().zip(&self.sieve_buckets) {
                column[c as usize] = value(bucket);
                buckets[c as usize] = bucket;
            }
            // Those the bar cuts dropped, the rest kept in order.
            let mut kept = 0;
            for j in 0..left.len() {
                let c = left[j] as usize;
                let mut bound = 0.0_f32;
                for i in 0..n {
                    bound += vals[i * m + c];
                }
                let local = locals[j];
                left[kept] = c as u32;
                locals[kept] = local;
                kept += usize::from(!cut_by(bar, bound, || geometry.tid_in(g as usize, local)));
            }
            if kept * 8 > left.len() * 7 {
                self.sieve_depth = k;
            }
            left.truncate(kept);
            locals.truncate(kept);
            passes += 1;
            if self.sieve_depth == k {
                break;
            }
        }
        // Every scoring term's bucket read: those left bounded as their own
        // checks bound them, at the one length their buckets imply.
        if passes == n && self.sieve_terms.len() == n && !left.is_empty() {
            let kept = self.sieve_shared(g, m, &held, &mut left, &mut locals);
            left.truncate(kept);
            locals.truncate(kept);
        }
        // Those dropped were examined (bounded) as those left will be.
        self.answer.candidates += (m - locals.len()) as u64;
        cands.clear();
        cands.extend_from_slice(&locals);
        self.sieve_vals = vals;
        self.sieve_held = held;
        self.sieve_left = left;
        self.sieve_locals = locals;
        Ok(())
    }

    /// Keeps, in order at the front of `left` (the sieve's candidates left,
    /// by their place among `m`) and `locals` (their slots), those the bar
    /// leaves in reach at the shared length: the least length a document
    /// holding each term's bucket can have (the largest of their floors,
    /// [`Sc::floor_kept`]), every term bounded there ([`Walk::reach`]: TIN's
    /// shared length). Returns how many. `held` holds each term's bucket of
    /// each candidate (`m` a term).
    ///
    /// Exact: a document scores at most that (it is at least that long),
    /// so one the bar cuts at it could not enter the top k. A candidate's own
    /// check cut the same ones, after reading its buckets again: at 150M rows
    /// "is to" left 3.4M candidates to it for 0.4M scored.
    fn sieve_shared(
        &mut self,
        g: u32,
        m: usize,
        held: &[u8],
        left: &mut [u32],
        locals: &mut [u32],
    ) -> usize {
        let geometry = self.geometry;
        let bar = self.bar;
        let mut kept = 0;
        for j in 0..left.len() {
            let c = left[j] as usize;
            let (mut floor, mut low, mut whole) = (0u32, u32::MAX, true);
            for st in &self.sieve_terms {
                let b = held[st.i * m + c];
                if b == NO_BUCKET {
                    continue;
                }
                match st.floors.get(usize::from(b)) {
                    Some(&f) => (floor, low) = (floor.max(f), low.min(f)),
                    None => whole = false,
                }
            }
            // Floors that agree bound it as the sieve did; a bucket past
            // the table's is left to its own check.
            let keep = !whole
                || low >= floor
                || !cut_by(
                    bar,
                    {
                        let mut reach = 0.0_f32;
                        for (i, s) in self.sc.iter().enumerate() {
                            let b = held[i * m + c];
                            if let Some(bucket) = TfBucket::new(b).filter(|_| b != NO_BUCKET) {
                                reach += s.scorer.bound_through(bucket, floor);
                            }
                        }
                        reach
                    },
                    || geometry.tid_in(g as usize, locals[j]),
                );
            left[kept] = left[j];
            locals[kept] = locals[j];
            kept += usize::from(keep);
        }
        kept
    }

    /// Sieves group `g`, every required term of which is a grid there, by
    /// the AND of their words, and walks its candidates.
    fn dense_group(&mut self, g: u32) -> Result<bool> {
        let group = self.geometry.groups[g as usize];
        for r in 0..self.req.len() {
            let t = self.req[r];
            if self.mems[t].loaded != g + 1 {
                let set = self.terms[t].as_mut().expect("required");
                load(
                    set,
                    self.geometry,
                    g,
                    &mut self.mems[t],
                    usize::MAX,
                    self.touch,
                )?;
            }
        }
        let grid = |m: &Mem<'a>| match m.kind {
            Kind::Grid(bytes) => bytes,
            _ => unreachable!("a grid"),
        };
        let words = &mut self.words;
        words.clear();
        words.resize(group.words(), 0);
        kernels::load(words, grid(&self.mems[self.req[0]]));
        for &t in &self.req[1..] {
            kernels::and_bytes(words, grid(&self.mems[t]));
        }
        if let Some(dead) = self.segment.liveness.groups[g as usize].as_deref() {
            kernels::andnot_words(words, dead);
        }
        // A group the sieve takes is sieved by its first term's buckets
        // before its candidates are listed.
        let sieved = kernels::popcount(&self.words) >= SIEVE_MIN as u64 && self.sieve_led(g)?;
        if sieved {
            let mut words = std::mem::take(&mut self.words);
            let done = self.sieve_words(&mut words);
            self.words = words;
            done?;
        }
        let mut cands = std::mem::take(&mut self.cands);
        cands.clear();
        kernels::set_bits(&self.words, &mut cands);
        let staging = !self.sc.is_empty() && matches!(self.verify, Verify::Span(_));
        let more = if sieved {
            self.walk_sieved(g, &cands, staging)
        } else {
            self.walk_listed(g, &cands, staging)
        };
        self.cands = cands;
        more
    }

    /// Moves the window to slot `local` of group `g` (whose first slot is
    /// `base`) and says whether no match there can be kept by its bound:
    /// slots up to [`Self::window_end`] share every scoring term's footer
    /// block, so its bound and its largest buckets.
    #[inline]
    fn window(&mut self, g: u32, base: u32, local: u32) -> bool {
        let slot = base + local;
        if self.window_end.is_none_or(|e| slot > e) {
            let group = &self.geometry.groups[g as usize];
            let group_end = base + group.slots() - 1;
            let mut e = u32::MAX;
            let mut bound = 0.0_f32;
            let (mut nsum, mut dmin, mut fmin) = (0.0_f64, f64::INFINITY, f64::INFINITY);
            for ((s, mb), wt) in self
                .sc
                .iter_mut()
                .zip(&mut self.window_buckets)
                .zip(&mut self.window_terms)
            {
                let Some(b) = s.block_at(slot) else {
                    *mb = NO_BUCKET;
                    *wt = 0.0;
                    continue;
                };
                // A grouped term whose next entry lies past this group
                // holds nothing in it: the window ends with the group.
                if s.footer.starts_after(b, group_end) {
                    *mb = NO_BUCKET;
                    *wt = 0.0;
                    e = e.min(group_end);
                    continue;
                }
                *wt = s.bound(b);
                bound += *wt;
                e = e.min(s.footer.last(b));
                *mb = s.max_bucket(b);
                let m = usize::from(*mb);
                nsum += f64::from(s.num[m]);
                dmin = dmin.min(f64::from(s.den[m]));
                fmin = fmin.min(f64::from(s.factor));
            }
            self.window_end = Some(e);
            self.window_bound = bound;
            self.window_parts = (nsum, dmin, fmin);
        }
        let geometry = self.geometry;
        self.cut(self.window_bound, || geometry.tid_in(g as usize, local))
    }

    /// Whether no document of `length` and ctid `tid` in the window can be
    /// kept: first by the terms' numerators over the least of their
    /// denominators, one division, pruning only a bound below the
    /// threshold by far more than `f32` rounding can explain; then exactly,
    /// by [`Self::length_bound`].
    #[inline]
    fn length_cut(&self, length: u32, tid: impl FnOnce() -> Tid) -> bool {
        let Some(theta) = self.threshold() else {
            return false;
        };
        let (nsum, dmin, fmin) = self.window_parts;
        if nsum / (dmin + fmin * f64::from(length)) * (1.0 + 1e-5) < f64::from(theta) {
            return true;
        }
        self.cut(self.length_bound(length), tid)
    }

    /// What a document of `length` in the window scores at most: per
    /// scoring term, the best score of a bucket up to its block's largest
    /// at that length, summed in the scorer's order.
    #[inline]
    fn length_bound(&self, length: u32) -> f32 {
        let mut bound = 0.0_f32;
        for (s, mb) in self.sc.iter().zip(&self.window_buckets) {
            if *mb != NO_BUCKET {
                bound += s
                    .scorer
                    .bound_through(TfBucket::new(*mb).expect("a valid bucket"), length);
            }
        }
        bound
    }

    /// Term `t`'s members in group `g`, loaded on first use (none when it
    /// holds no member there).
    fn ensure(&mut self, t: usize, g: u32) -> Result<()> {
        if self.mems[t].loaded == g + 1 {
            return Ok(());
        }
        let set = self.terms[t].as_mut().expect("a term of the walk");
        let mem = &mut self.mems[t];
        let held = !matches!(set.postings.form, Form::Sparse(_))
            || next_group(set, self.geometry, g, &mut mem.hint, self.touch) == Some(g);
        if !(held && load(set, self.geometry, g, mem, 64, self.touch)?) {
            mem.loaded = g + 1;
            mem.kind = Kind::List;
            mem.list.clear();
            mem.pos = 0;
        }
        Ok(())
    }

    /// Bounds, scores and admits the candidate at slot `local` of group
    /// `g`, which every required term holds; false once nothing later can
    /// enter the top k.
    fn process(&mut self, g: u32, local: u32) -> Result<bool> {
        if self.sc.is_empty() && self.unscored_done(self.geometry.tid_in(g as usize, local)) {
            return Ok(false);
        }
        if let Some((_, length)) = self.stage(g, local)? {
            self.finish(g, local, length)?;
        }
        Ok(true)
    }

    /// Stages group `g`'s candidate at slot `local`: bounds it by its
    /// buckets and keeps it, best bound first, for [`Walk::finish_staged`].
    fn stage_into(&mut self, g: u32, local: u32) -> Result<()> {
        let Some((reach, length)) = self.stage(g, local)? else {
            return Ok(());
        };
        self.staged.push((reach, local, length));
        self.staged_buckets.extend_from_slice(&self.buckets);
        if matches!(self.verify, Verify::Span(_)) {
            self.staged_span.extend_from_slice(&self.span_index);
        }
        Ok(())
    }

    /// Reads the lengths of the group's staged candidates and scores them,
    /// best bound first, each against the threshold as it then stands: the
    /// best of a group raise it over the rest, which its first candidates
    /// in ctid order did not (on TIN's corpus, `w3 AND w17` read the DL
    /// sidecar for six times the candidates the final threshold needs).
    ///
    /// While the top k fill, a span's candidates are finished in ctid order
    /// instead: each is checked whatever its bound, and in that order each
    /// term's positions are read forward from the entry read last, where
    /// best first went back to the start of the entry's block (a phrase of
    /// common words that never fills its top k checks every candidate).
    fn finish_staged(&mut self, g: u32) -> Result<()> {
        let staged = std::mem::take(&mut self.staged);
        let mut order = std::mem::take(&mut self.staged_order);
        let mut result = Ok(());
        let mut filled = 0;
        if matches!(self.verify, Verify::Span(_)) {
            while filled < staged.len() && self.bar.is_none() && result.is_ok() {
                result = self.finish_one(g, &staged, filled);
                filled += 1;
            }
        }
        order.clear();
        order.extend(filled as u32..staged.len() as u32);
        order.sort_unstable_by(|a, b| {
            let (x, y) = (staged[*a as usize], staged[*b as usize]);
            y.0.total_cmp(&x.0).then(x.1.cmp(&y.1))
        });
        for &i in &order {
            if result.is_err() {
                break;
            }
            let (reach, local, _) = staged[i as usize];
            let geometry = self.geometry;
            // In this order (bound down, then ctid up) and with a bar that
            // only rises, every candidate after one cut is cut too.
            if self.cut(reach, || geometry.tid_in(g as usize, local)) {
                break;
            }
            result = self.finish_one(g, &staged, i as usize);
        }
        let mut staged = staged;
        staged.clear();
        self.staged = staged;
        self.staged_order = order;
        self.staged_buckets.clear();
        self.staged_span.clear();
        result
    }

    /// Finishes staged candidate `i` of `staged` (group `g`'s), its buckets
    /// and a span's posting indexes restored from where they were staged.
    fn finish_one(&mut self, g: u32, staged: &[(f32, u32, u32)], i: usize) -> Result<()> {
        let n = self.sc.len();
        let (_, local, length) = staged[i];
        self.buckets
            .copy_from_slice(&self.staged_buckets[i * n..(i + 1) * n]);
        if matches!(self.verify, Verify::Span(_)) {
            let width = self.span_index.len();
            self.span_index
                .copy_from_slice(&self.staged_span[i * width..(i + 1) * width]);
        }
        self.finish(g, local, length)
    }

    /// Bounds the candidate at slot `local` of group `g` by what can be
    /// read before its length: `None` when it cannot be kept, else its
    /// bound and its length when that is known (else `u32::MAX`), with its
    /// buckets in [`Walk::buckets`] and, for a span, its posting indexes in
    /// [`Walk::span_index`].
    fn stage(&mut self, g: u32, local: u32) -> Result<Option<(f32, u32)>> {
        let n = self.sc.len();
        if let Some(dead) = self.segment.liveness.groups[g as usize].as_deref()
            && dead[local as usize / 64] >> (local % 64) & 1 == 1
        {
            return Ok(None);
        }
        self.answer.candidates += 1;
        // Its ctid only breaks a tie with the threshold's: most candidates
        // never need it.
        let geometry = self.geometry;
        let tid = move || geometry.tid_in(g as usize, local);
        let staged = match self.inline {
            // The length alone, against the window's largest buckets, from
            // a rare required term's record that carries lengths.
            Some(t) => {
                let index = self.mems[t]
                    .find(local)
                    .expect("a candidate holds every required term");
                let set = self.terms[t].as_ref().expect("required");
                let inline = set.postings.lengths.as_ref().expect("inline lengths");
                self.touch
                    .touch(Part::Payload, set.at + inline.at_of(index), 4);
                let length = inline.get(index)?;
                if n > 0 && self.length_cut(length, tid) {
                    return Ok(None);
                }
                let reach = self.read_buckets(g, local, tid)?;
                if self.cut(reach, tid) || self.too_few() {
                    return Ok(None);
                }
                // The length known, its buckets bound it at that length.
                (
                    self.reach((0..n).filter(|i| self.buckets[*i] != NO_BUCKET), length),
                    length,
                )
            }
            // The candidate's buckets first, each against the shortest
            // document of its block holding that bucket or more (the
            // footer's frontier), and the DL sidecar only for a candidate
            // they leave in reach: at 150 million rows two in three scored
            // candidates fell short on their buckets alone.
            None => {
                let reach = self.read_buckets(g, local, tid)?;
                if self.cut(reach, tid) || self.too_few() {
                    return Ok(None);
                }
                (reach, u32::MAX)
            }
        };
        self.span_indexes(local);
        Ok(Some(staged))
    }

    /// Scores and admits the candidate at slot `local` of group `g`, staged
    /// with its buckets in [`Walk::buckets`] (and a span's posting indexes
    /// in [`Walk::span_index`]); `length` is its length, or `u32::MAX` to
    /// read it from the DL sidecar.
    fn finish(&mut self, g: u32, local: u32, length: u32) -> Result<()> {
        let n = self.sc.len();
        // A span checks positions before scoring while the top k fill:
        // every match enters them, and only a match reads its length (at
        // 150M rows four in ten of a phrase's lengths were read for
        // candidates its positions then ruled out).
        let span_first = matches!(self.verify, Verify::Span(_)) && self.bar.is_none();
        if span_first {
            let Verify::Span(check) = &mut self.verify else {
                unreachable!()
            };
            self.answer.position_checks += 1;
            if !check.holds(&self.span_index, self.touch)? {
                return Ok(());
            }
        }
        // A walk with nothing to score (every term elided) scores nothing,
        // and needs no length.
        let length = if length == u32::MAX && n > 0 {
            self.dl_length(g, local)?
        } else {
            length
        };
        self.answer.scored += u64::from(n > 0);
        let mut total = 0.0_f32;
        for i in 0..n {
            let bucket = self.buckets[i];
            if bucket == NO_BUCKET {
                continue;
            }
            total += self.sc[i]
                .scorer
                .score_bucket(TfBucket::new(bucket).ok_or(Error::InvalidTfBucket)?, length);
        }
        // Its ctid only once it would be kept (or ties the bar): the
        // geometry's tables it is read from are cold for most candidates.
        let geometry = self.geometry;
        if self.cut(total, || geometry.tid_in(g as usize, local)) {
            return Ok(());
        }
        let tid = geometry.tid_in(g as usize, local);
        match &self.verify {
            Verify::Flat => self.push(total, tid),
            Verify::Span(_) if span_first => self.push(total, tid),
            // Once they fill, only a candidate that would enter them is
            // checked, as it is scored (best bound first in its group, so a
            // confirmed one raises the bar over the rest).
            Verify::Span(_) => {
                let Verify::Span(check) = &mut self.verify else {
                    unreachable!()
                };
                self.answer.position_checks += 1;
                if check.holds(&self.span_index, self.touch)? {
                    self.push(total, tid);
                }
            }
            Verify::Node => {
                let slot = self.geometry.groups[g as usize].slot_base + local;
                if matches(
                    self.segment,
                    self.node,
                    &mut self.node_terms,
                    slot,
                    &mut self.positions,
                    self.touch,
                )? {
                    self.push(total, tid);
                }
            }
        }
        Ok(())
    }

    /// Reads into [`Walk::buckets`] the bucket of each scoring term the
    /// candidate at slot `local` of group `g` holds ([`NO_BUCKET`] for the
    /// others), and returns what the candidate can score at most by them:
    /// see [`Walk::reach`], taken only when each term's own bound (kept per
    /// block and bucket) leaves the candidate, of ctid `tid`, in reach.
    fn read_buckets(&mut self, g: u32, local: u32, tid: impl Fn() -> Tid + Copy) -> Result<f32> {
        if self.common_terms {
            return self.read_buckets_common(g, local, tid);
        }
        let (mut floor, mut low, mut own) = (0u32, u32::MAX, 0.0_f32);
        for i in 0..self.sc.len() {
            let t = self.sc[i].term;
            if !self.sc_required[i] {
                self.ensure(t, g)?;
            }
            let Some(index) = self.mems[t].find(local) else {
                self.buckets[i] = NO_BUCKET;
                continue;
            };
            let s = &mut self.sc[i];
            let set = self.terms[t].as_ref().expect("scoring");
            let block = s.block_of(index, self.mems[t].entry);
            let bucket = s.bucket_in(block, set.postings.tf, index)?;
            if s.footer.single().is_none() {
                self.touch.touch(
                    Part::TfTail,
                    set.at + set.postings.tf_at + s.footer.tf_byte(block),
                    1,
                );
            }
            let (f, bound) = self.sc[i].floor_kept(block, bucket);
            (floor, low) = (floor.max(f), low.min(f));
            own += bound;
            self.buckets[i] = bucket;
            self.held_index[i] = index;
        }
        // Each term's own bound is at least the shared one: it prunes what
        // it can without a division per term, and is the shared one when
        // the terms' lengths agree.
        if low >= floor || self.cut(own, tid) {
            return Ok(own);
        }
        Ok(self.reach(
            (0..self.sc.len()).filter(|i| self.buckets[*i] != NO_BUCKET),
            floor,
        ))
    }

    /// [`Walk::read_buckets`] in a walk with common terms
    /// ([`Sc::make_common`]): their buckets last, only for a candidate the
    /// others' buckets leave in reach.
    #[inline(never)]
    fn read_buckets_common(
        &mut self,
        g: u32,
        local: u32,
        tid: impl Fn() -> Tid + Copy,
    ) -> Result<f32> {
        let (mut floor, mut low, mut own) = (0u32, u32::MAX, 0.0_f32);
        // Whether the candidate holds a common term, and whether a bucket
        // was read before its (else the common terms' window bounds are the
        // window's, which the candidate passed).
        let mut common = false;
        let mut read = false;
        for i in 0..self.sc.len() {
            let t = self.sc[i].term;
            if !self.sc_required[i] {
                self.ensure(t, g)?;
            }
            let Some(index) = self.mems[t].find(local) else {
                self.buckets[i] = NO_BUCKET;
                continue;
            };
            self.held_index[i] = index;
            if self.sc[i].common.is_some() {
                // Its bucket only for a candidate the others leave in reach:
                // until then, its block's bound.
                common = true;
                self.buckets[i] = LATER;
                own += self.window_terms[i];
                continue;
            }
            let block = self.sc[i].block_of(index, self.mems[t].entry);
            let bucket = self.read_bucket(i, block, index)?;
            let (f, bound) = self.sc[i].floor_kept(block, bucket);
            (floor, low) = (floor.max(f), low.min(f));
            self.held_bound[i] = bound;
            own += bound;
            read = true;
        }
        if !common {
            // Each term's own bound is at least the shared one: it prunes
            // what it can without a division per term, and is the shared one
            // when the terms' lengths agree.
            if low >= floor || self.cut(own, tid) {
                return Ok(own);
            }
        } else {
            // The common terms at their blocks' bounds, the others at their
            // buckets'.
            if read && self.cut(own, tid) {
                return Ok(own);
            }
            // Then their buckets, each bounded at its block's frontier as
            // the others' are.
            own = 0.0;
            for i in 0..self.sc.len() {
                match self.buckets[i] {
                    NO_BUCKET => {}
                    LATER => {
                        let index = self.held_index[i];
                        let t = self.sc[i].term;
                        let block = self.sc[i].block_of(index, self.mems[t].entry);
                        let bucket = self.read_bucket(i, block, index)?;
                        let (f, bound) = self.sc[i].floor_kept(block, bucket);
                        (floor, low) = (floor.max(f), low.min(f));
                        own += bound;
                    }
                    _ => own += self.held_bound[i],
                }
            }
            if low >= floor || self.cut(own, tid) {
                return Ok(own);
            }
        }
        Ok(self.reach(
            (0..self.sc.len()).filter(|i| self.buckets[*i] != NO_BUCKET),
            floor,
        ))
    }

    /// Reads into [`Walk::buckets`] scoring term `i`'s bucket for its
    /// posting `index`, which its block `block` holds.
    #[inline]
    fn read_bucket(&mut self, i: usize, block: usize, index: u32) -> Result<u8> {
        let s = &mut self.sc[i];
        let set = self.terms[s.term].as_ref().expect("scoring");
        let bucket = s.bucket_in(block, set.postings.tf, index)?;
        if s.footer.single().is_none() {
            self.touch.touch(
                Part::TfTail,
                set.at + set.postings.tf_at + s.footer.tf_byte(block),
                1,
            );
        }
        self.buckets[i] = bucket;
        Ok(bucket)
    }

    /// What a candidate holding the scoring terms `held` (in the scorer's
    /// order), with the buckets in [`Walk::buckets`], scores at most when it
    /// is at least `floor` long. A document has one length, and every term
    /// it holds bounds that from below: by the shortest length its block
    /// holds at the candidate's bucket or above. The longest of those
    /// bounds every term's score (on TIN's corpus, where a word's count
    /// grows with the length, it left a seventh of the candidates in reach
    /// that each term's own bound did).
    #[inline]
    fn reach(&self, held: impl Iterator<Item = usize>, floor: u32) -> f32 {
        let mut reach = 0.0_f32;
        for i in held {
            let bucket = TfBucket::new(self.buckets[i]).expect("a valid bucket");
            reach += self.sc[i].scorer.bound_through(bucket, floor);
        }
        reach
    }

    /// Whether the candidate at hand, its buckets read, holds a word its
    /// phrase repeats fewer times than the phrase does (its bucket's counts
    /// all fall short), so cannot match.
    #[inline]
    fn too_few(&self) -> bool {
        self.repeats
            .iter()
            .any(|&(i, least)| self.buckets[i] < least)
    }

    /// The length of the document at slot `local` of group `g`, from the DL
    /// sidecar.
    #[inline]
    fn dl_length(&mut self, g: u32, local: u32) -> Result<u32> {
        let rank = self
            .segment
            .docs
            .rank_in(g as usize, local)
            .ok_or(Error::Corrupt("a posting without a document"))?;
        let (header, bits) = self.segment.length_at(rank);
        self.touch.touch(Part::DlSidecar, header, 8);
        self.touch.touch(Part::DlSidecar, bits, 4);
        self.segment.lengths.get(rank)
    }

    /// The candidate's posting index in each span slot's term.
    fn span_indexes(&mut self, local: u32) {
        let Verify::Span(check) = &self.verify else {
            return;
        };
        self.span_index.clear();
        for &t in &check.slots {
            // A scoring term's index was found with its bucket.
            let s = self.term_sc[t];
            let index = if s != usize::MAX && self.buckets[s] != NO_BUCKET {
                self.held_index[s]
            } else {
                self.mems[t]
                    .find(local)
                    .expect("a candidate holds every span term")
            };
            self.span_index.push(index);
        }
    }
}

/// Every row is visible.
struct Everything;

impl Visibility for Everything {
    fn visible(&mut self, _: Tid) -> bool {
        true
    }
}

/// [`super::top_k`]: the walk into fresh rows, then the zero fill.
pub(super) fn top_k(
    segment: &Segment<'_>,
    node: &Node,
    names: &[String],
    scorers: &[(String, TermScorer)],
    k: usize,
    touch: &mut impl Touch,
) -> Result<RankedAnswer> {
    let mut top = TopRows::new(k, false);
    let mut answer = RankedAnswer::default();
    let mut terms = walk_into(
        segment,
        node,
        names,
        scorers,
        &mut top,
        &mut Everything,
        touch,
        &mut answer,
    )?;
    let mut rows = top.into_rows();
    // Zero fill: fewer than k rows, the rest are matches of non-scoring
    // terms in ctid order (a led walk saw every match already).
    if rows.len() < k
        && required_terms(node).is_empty()
        && let Some(terms) = terms.as_mut()
    {
        let seen: std::collections::HashSet<Tid> = rows.iter().map(|r| r.1).collect();
        let fill = super::first_matches(segment, node, terms, k - rows.len(), &seen, touch)?;
        rows.extend(fill.into_iter().map(|t| (0.0, t)));
        rows.sort_by(crate::walk::rank);
        rows.truncate(k);
    }
    answer.rows = rows;
    Ok(answer)
}

/// [`super::top_k_into`]: walks `segment` into `top`, keeping only rows
/// `visibility` passes, and adds what it did to `answer`. Returns the
/// opened terms, for a zero fill; `None` when nothing can match.
#[expect(
    clippy::too_many_arguments,
    reason = "the walk's inputs and its two outputs"
)]
pub(super) fn walk_into<'s, 'a>(
    segment: &'s Segment<'a>,
    node: &Node,
    names: &[String],
    scorers: &[(String, TermScorer)],
    top: &mut TopRows,
    visibility: &mut dyn Visibility,
    touch: &mut impl Touch,
    answer: &mut RankedAnswer,
) -> Result<Option<Vec<Option<TermSet<'a>>>>> {
    let k = top.k();
    let required = required_terms(node);
    // A segment lacking a required term matches nothing: its terms'
    // records (their group directories) are not parsed.
    for t in &required {
        if !segment.holds_memo(&names[*t])? {
            return Ok(None);
        }
    }
    // Pages pinned from here on may be released a group at a time when
    // nothing the walk keeps borrows them (see `Walk::group_done`).
    let blob = segment.bytes.blob();
    let opened = blob.map(LazyBlob::pin_mark);
    let terms = open_terms(segment, names, touch)?;
    if k == 0 || required.iter().any(|t| terms[*t].is_none()) {
        return Ok(None);
    }
    // Sized once: an `Sc` is large, and a growing vector copies them.
    let mut sc: Vec<Sc<'s, 'a>> = Vec::with_capacity(scorers.len());
    for (name, scorer) in scorers {
        let Some(t) = names.iter().position(|n| n == name) else {
            continue;
        };
        let Some(set) = &terms[t] else { continue };
        sc.push(Sc::new(t, scorer.clone(), set, segment)?);
    }
    // Leaves only: a candidate holding every required term matches a flat
    // AND, and one holding a scoring term a flat OR.
    let flat = match node {
        Node::And(c) | Node::Or(c) => c.iter().all(|c| matches!(c, Node::Term(_))),
        Node::Term(_) => true,
        _ => false,
    };
    let verify = if flat {
        Verify::Flat
    } else if let Node::Span { slots, query } = node {
        span_check(slots, query, &terms)?
    } else {
        Verify::Node
    };
    let node_terms = if matches!(verify, Verify::Node) {
        open_terms(segment, names, touch)?
    } else {
        Vec::new()
    };
    let mut req: Vec<usize> = required.clone();
    req.sort_by_key(|t| terms[*t].as_ref().expect("checked").df);
    let n = sc.len();
    let bar = top.bar();
    let mut walk = Walk {
        segment,
        geometry: &segment.docs.geometry,
        node,
        top,
        bar,
        visibility,
        mems: (0..terms.len()).map(|_| Mem::default()).collect(),
        terms,
        sc_required: Vec::with_capacity(n),
        present: vec![false; n],
        present_list: Vec::with_capacity(n),
        held_list: Vec::with_capacity(n),
        blocks: vec![0; n],
        sub_bounds: Vec::new(),
        plan: Plan::default(),
        rows: Vec::new(),
        row_state: vec![Row::default(); n],
        row_loaded: Vec::new(),
        or_terms: Vec::new(),
        or_rows: Vec::new(),
        or_bounds: Vec::new(),
        or_sub: Vec::new(),
        or_held: Vec::new(),
        held_bound: vec![0.0; n],
        buckets: vec![NO_BUCKET; n],
        held_index: vec![0; n],
        term_sc: Vec::new(),
        window_end: None,
        window_bound: 0.0,
        window_parts: (0.0, f64::INFINITY, f64::INFINITY),
        window_buckets: vec![NO_BUCKET; n],
        window_terms: vec![0.0; n],
        inline: None,
        repeats: Vec::new(),
        req,
        sc_order: Vec::new(),
        common_terms: false,
        answer: RankedAnswer::default(),
        verify,
        span_index: Vec::new(),
        staged: Vec::new(),
        staged_buckets: Vec::new(),
        staged_span: Vec::new(),
        staged_order: Vec::new(),
        cands: Vec::new(),
        sieve_terms: Vec::new(),
        sieve_ub: Vec::new(),
        sieve_vals: Vec::new(),
        sieve_ranks: Vec::new(),
        sieve_buckets: Vec::new(),
        sieve_held: Vec::new(),
        sieve_cands: Vec::new(),
        sieve_depth: 0,
        sieve_pre: false,
        sieve_prefix: Vec::new(),
        sieve_fields: Vec::new(),
        sieve_pass: Vec::new(),
        sieve_locals: Vec::new(),
        sieve_left: Vec::new(),
        words: Vec::new(),
        marks: Vec::new(),
        positions: Vec::new(),
        node_terms,
        touch,
        sc,
        frame: None,
        held: Vec::new(),
    };
    walk.term_sc = vec![usize::MAX; walk.terms.len()];
    for i in 0..n {
        let r = required.contains(&walk.sc[i].term);
        walk.sc_required.push(r);
        walk.term_sc[walk.sc[i].term] = i;
    }
    if let Verify::Span(check) = &walk.verify
        && let Some(plan) = &check.plan
    {
        // A phrase's leaves sit at distinct positions: a word that is
        // several of them needs a count of at least that many.
        for i in 0..n {
            let uses = plan
                .leaves()
                .iter()
                .filter(|slot| check.slots[**slot] == walk.sc[i].term)
                .count() as u32;
            if uses > 1 {
                walk.repeats.push((i, TfBucket::from_count(uses).value()));
            }
        }
    }
    if !required.is_empty() {
        let common = |s: &Sc, set: &TermSet| {
            u64::from(set.df) * COMMON_RATIO >= u64::from(segment.documents)
                && s.footer.single().is_none()
        };
        let rarer = walk.sc.iter().any(|s| {
            let set = walk.terms[s.term].as_ref().expect("scoring");
            !common(s, set)
        });
        if rarer {
            for s in &mut walk.sc {
                let set = walk.terms[s.term].as_ref().expect("scoring");
                if common(s, set) {
                    s.make_common(set.max_bucket);
                    walk.common_terms = true;
                }
            }
        }
    }
    if walk.common_terms {
        walk.sc_order = (0..walk.sc.len()).collect();
        walk.sc_order
            .sort_by_key(|i| walk.terms[walk.sc[*i].term].as_ref().expect("scoring").df);
        walk.sub_bounds = vec![0.0; walk.sc.len()];
    }
    walk.inline = walk.req.iter().copied().find(|t| {
        walk.terms[*t]
            .as_ref()
            .is_some_and(|s| s.postings.lengths.is_some())
    });
    // A node check's cursors keep a group's grid between seeks: its walk
    // holds what it pins. Otherwise the pages read to open the terms
    // (records, directories, decoded) are released with the
    // groups', but for the ranges the records keep slices of.
    if let (Some(blob), Some(opened)) = (blob, opened)
        && !matches!(walk.verify, Verify::Node)
    {
        for set in walk.terms.iter().flatten() {
            for (from, to) in set.postings.borrowed().into_iter().flatten() {
                walk.held.push((set.at + from, set.at + to));
            }
        }
        // The pages those ranges lie on, read to open the terms, are held
        // to the walk's end before the mark the groups release to.
        walk.frame = Some((blob, blob.hold_since(opened, &walk.held)));
        // A footer's pages hold only its bytes: each is released once the
        // footer has read past it, not left pinned to the group's end.
        for s in &mut walk.sc {
            s.footer.release_passed();
        }
    }
    if required.is_empty() {
        walk.run_or()?;
    } else {
        walk.run()?;
    }
    walk.count_metadata();
    let done = walk.answer;
    answer.scored += done.scored;
    answer.windows += done.windows;
    answer.windows_pruned += done.windows_pruned;
    answer.candidates += done.candidates;
    answer.position_checks += done.position_checks;
    Ok(Some(walk.terms))
}

fn span_check<'a>(
    slots: &[usize],
    query: &SpanQuery,
    terms: &[Option<TermSet<'a>>],
) -> Result<Verify<'a>> {
    let solver = SpanSolver::new(query).map_err(|_| Error::Corrupt("span query"))?;
    let set = |slot: usize| terms[slots[slot]].as_ref().expect("a span's terms exist");
    let plan = PhrasePlan::new(query, |slot| u64::from(set(slot).df));
    let cursors = (0..slots.len())
        .map(|slot| PosCursor::new(set(slot).positions))
        .collect::<Result<Vec<_>>>()?;
    let first = (0..slots.len())
        .map(|slot| {
            slots
                .iter()
                .position(|t| *t == slots[slot])
                .expect("the slot itself")
        })
        .collect();
    Ok(Verify::Span(Box::new(SpanCheck {
        slots: slots.to_vec(),
        first,
        cursors,
        solver,
        plan,
        positions: vec![Vec::new(); slots.len()],
        read: vec![false; slots.len()],
    })))
}

/// Words of a disjunction's sub-range: a group's members are bounded by
/// their terms' blocks over this many words (1,024 slots) at a time.
const SUB_WORDS: usize = 16;

/// Whether a disjunction's words and members are weighed in lanes, by the
/// weighing kernels (`kernels::word_sums`, `kernels::member_sums`), rather
/// than one at a time inline (see `Walk::or_group`). The sums are the same
/// bit for bit. On x86-64 the kernels ran 6-8% fewer instructions of a
/// disjunction; on aarch64 the inline loop ran 2-4% fewer cycles (Apple M4,
/// where the one-at-a-time sums retire about three instructions a cycle).
const WEIGH_IN_LANES: bool = cfg!(target_arch = "x86_64");

/// Members a word must hold for its members to be weighed in lanes rather
/// than one at a time (see `Walk::or_group`): the narrowest kernel's lanes.
const MEMBER_LANES: usize = 4;

use super::kernels::or_zero;

/// A disjunction's plan for a group at one threshold (MaxScore with the
/// group's bounds).
#[derive(Default)]
struct Plan {
    /// Scoring terms (indexes into the walk's) every candidate must hold:
    /// without any one, the others' bounds cannot reach the threshold.
    required: Vec<usize>,
    /// The terms a candidate must hold one of, when `by_essential`.
    essential: Vec<usize>,
    by_essential: bool,
    order: Vec<(f32, usize)>,
}

/// Sets a group's local slots, given in ascending order, in a row of words
/// cleared before: the word at hand's bits are kept in a register and
/// stored whole at each member, so a member never reads back the word the
/// one before it stored.
struct RowBits<'r> {
    row: &'r mut [u64],
    w: usize,
    bits: u64,
}

impl<'r> RowBits<'r> {
    fn new(row: &'r mut [u64]) -> Self {
        Self { row, w: 0, bits: 0 }
    }

    #[inline]
    fn set(&mut self, local: u32) {
        let w = local as usize / 64;
        debug_assert!(w >= self.w, "local slots in ascending order");
        let kept = if w == self.w { self.bits } else { 0 };
        self.bits = kept | 1 << (local % 64);
        self.w = w;
        self.row[w] = self.bits.to_le();
    }
}

/// A disjunction term's members in the group at hand, as a row of words
/// (see [`RowAt`]), and a forward cursor over their posting indexes.
#[derive(Clone, Copy, Default)]
struct Row {
    /// Postings in earlier groups; the term's directory entry for the
    /// group at hand (`usize::MAX` for a sparse term).
    first: u32,
    entry: usize,
    /// Members in words `..w` are `run`.
    w: usize,
    run: u32,
    at: RowAt,
}

/// Where a disjunction term's row lies in the group at hand: the group's
/// words, little-endian. A grid on one page is read in place there; other
/// members (decoded, or a grid across pages copied a page's part at a
/// time) lie in the term's part of [`Walk::rows`]. A word is the same load
/// from either, so the walk never asks which ([`kernels::Row`]: a pointer,
/// sound as the walk uses it: a group's rows are loaded after `rows` is
/// sized for the group, and read only within that group, before
/// [`Walk::group_done`] releases its pages).
///
/// Copying every grid into its row cost 31 MB of copies per disjunction at
/// 150M rows (106 MB scoring every term), libc `memcpy` on x86-64.
///
/// A conjunction's grids stay slices (`Kind::Grid`): copying a grid across
/// pages into the term's own words rather than stitching it copied as many
/// bytes, and checking for it per grid cost 1.3% more instructions.
type RowAt = kernels::Row;

/// A grid container's row (`row`, of its group's words, holding it only if
/// need be): in place where the grid lies on one page, else copied into
/// `row` a page's part at a time rather than stitched into the span's
/// buffers first.
#[inline]
fn grid_row(grid: Bytes<'_>, row: &mut [u64]) -> Result<RowAt> {
    let len = grid.len();
    if len != size_of_val(row) {
        return Err(Error::Corrupt("a grid of its group's words"));
    }
    if grid.page_end(0) == len {
        return Ok(RowAt::new(grid.all()?));
    }
    // SAFETY: a `u64` slice's bytes, any value valid.
    let out = unsafe { std::slice::from_raw_parts_mut(row.as_mut_ptr().cast::<u8>(), len) };
    let mut at = 0;
    while at < len {
        let end = grid.page_end(at);
        out[at..end].copy_from_slice(grid.get(at, end)?);
        at = end;
    }
    Ok(RowAt::of(row))
}

impl<'a, T: Touch> Walk<'_, 'a, T> {
    /// Block-max MaxScore over the scoring terms, a group at a time: a
    /// group whose terms' bounds cannot reach the threshold is skipped; else
    /// it is planned at its bounds (required terms ANDed, essential terms
    /// ORed into its mask), and each member of the mask is weighed by the
    /// bounds over its sub-range of the terms it holds; each one left is
    /// bounded by its terms' blocks, then by its buckets and length, then
    /// scored.
    fn run_or(&mut self) -> Result<()> {
        let n = self.sc.len();
        if n == 0 {
            return Ok(());
        }
        // Every group a term holds is bounded by its blocks there, so every
        // block is: decoded whole first, its pages released as it reads
        // past them, rather than a block at a time between a group's reads.
        for s in &mut self.sc {
            s.decode_all();
        }
        let mut next = vec![0u32; n];
        let mut from = 0u32;
        let mut fresh = vec![false; n];
        loop {
            let mut g = u32::MAX;
            for i in 0..n {
                if !fresh[i] || next[i] < from {
                    let t = self.sc[i].term;
                    let set = self.terms[t].as_mut().expect("scoring");
                    next[i] =
                        next_group(set, self.geometry, from, &mut self.mems[t].hint, self.touch)
                            .unwrap_or(u32::MAX);
                    fresh[i] = true;
                }
                g = g.min(next[i]);
            }
            if g == u32::MAX {
                break;
            }
            self.present_list.clear();
            for (i, (present, next)) in self.present.iter_mut().zip(&next).enumerate() {
                *present = *next == g;
                if *present {
                    self.present_list.push(i);
                }
            }
            self.or_group(g)?;
            self.group_done();
            from = g + 1;
        }
        Ok(())
    }

    /// Scoring term `i`'s members in group `g`, which it holds, as its row
    /// of `words` words ([`Row::at`]).
    #[inline(never)]
    fn or_load(&mut self, i: usize, g: u32, words: usize) -> Result<()> {
        let t = self.sc[i].term;
        let geometry = self.geometry;
        let group = geometry.groups[g as usize];
        let row = &mut self.rows[i * words..(i + 1) * words];
        let set = self.terms[t].as_mut().expect("scoring");
        let r = &mut self.row_state[i];
        r.w = 0;
        r.run = 0;
        r.entry = usize::MAX;
        r.at = RowAt::of(row);
        if let Form::Sparse(list) = &set.postings.form {
            row.fill(0);
            let cursor = set.sparse.get_or_insert_with(|| {
                self.touch.touch(
                    Part::Payload,
                    set.at + set.postings.payload_at,
                    set.postings.payload.len(),
                );
                list.cursor()
            });
            cursor.seek(group.slot_base);
            r.first = cursor.rank() as u32;
            let end = group.slot_base + group.slots();
            let mut out = RowBits::new(row);
            cursor.drain_below(end, |slot| out.set(slot - group.slot_base));
            return Ok(());
        }
        // The entry `run_or`'s seek found for the group, in the hint.
        let hint = self.mems[t].hint;
        let entry = set.group_at(hint);
        if entry.index != g {
            return Err(Error::Corrupt("a group the term holds"));
        }
        r.entry = hint;
        match entry.src {
            Src::Container {
                entry: e,
                bytes,
                at,
            } => {
                self.touch.touch(Part::Payload, at, bytes.len());
                self.mems[t].used += 1;
                r.first = e.first;
                if e.kind == KIND_GRID {
                    r.at = grid_row(bytes, row)?;
                } else {
                    row.fill(0);
                    kernels::container_row(&e, bytes.all()?, geometry.starts(g as usize), row)?;
                }
            }
            Src::Locals { from, to } => {
                r.first = from;
                row.fill(0);
                let mut out = RowBits::new(row);
                for l in &set.locals[from as usize..to as usize] {
                    out.set(*l);
                }
            }
        }
        Ok(())
    }

    #[inline(never)]
    fn or_group(&mut self, g: u32) -> Result<()> {
        let n = self.sc.len();
        let group = self.geometry.groups[g as usize];
        let base = group.slot_base;
        let end = base + group.slots() - 1;
        self.answer.windows += 1;
        // The present terms' bounds over the group.
        let mut sb = std::mem::take(&mut self.sub_bounds);
        sb.clear();
        sb.resize(n, 0.0);
        let mut total = 0.0_f32;
        for (i, bound) in sb.iter_mut().enumerate() {
            if self.present[i] {
                // A grouped term's entry for the group is the one its seek
                // left in its hint (see `run_or`).
                let s = &mut self.sc[i];
                *bound = if s.grouped() {
                    s.entry_bound(self.mems[s.term].hint)
                } else {
                    s.range_bound(base, end)
                };
                total += *bound;
            }
        }
        let geometry = self.geometry;
        let gi = g as usize;
        if self.cut(total, || geometry.tid_in(gi, 0)) {
            self.answer.windows_pruned += 1;
            self.sub_bounds = sb;
            return Ok(());
        }
        let words = group.words();
        // Rows are overwritten where read: only present terms' are.
        if self.rows.len() < n * words {
            self.rows.resize(n * words, 0);
        }
        let dead = self.segment.liveness.groups[g as usize].as_deref();
        let mut plan = std::mem::take(&mut self.plan);
        // The group's plan at its own bounds and the threshold now: a
        // document holding none of its essential terms (or lacking one of
        // its required ones) cannot reach the threshold anywhere in the
        // group, which only rises. Those terms' rows are read first and
        // combined into the group's mask; a group whose mask is empty reads
        // nothing else, and a sub-range whose mask is empty is skipped
        // before its bounds are taken.
        let real: f64 = sb.iter().map(|b| f64::from(*b)).sum();
        self.plan_group(&mut plan, self.threshold(), &sb, real);
        let mut loaded = std::mem::take(&mut self.row_loaded);
        loaded.clear();
        loaded.resize(n, false);
        let mut mask = std::mem::take(&mut self.words);
        mask.clear();
        mask.resize(words, 0);
        let filters = plan.required.len()
            + if plan.by_essential {
                plan.essential.len()
            } else {
                0
            };
        if filters == 0 {
            mask.fill(!0);
        }
        // The essential terms ORed into the mask, cleared above, then the
        // required terms ANDed in; without essential terms the first
        // required one is the mask to start from.
        let mut required = plan.required.iter();
        if plan.by_essential && !plan.essential.is_empty() {
            for e in 0..plan.essential.len() {
                let i = plan.essential[e];
                self.or_load(i, g, words)?;
                loaded[i] = true;
                kernels::or_bytes(&mut mask, self.row_state[i].at.bytes());
            }
        } else if let Some(&first) = required.next() {
            self.or_load(first, g, words)?;
            loaded[first] = true;
            kernels::load(&mut mask, self.row_state[first].at.bytes());
        }
        for &i in required {
            self.or_load(i, g, words)?;
            loaded[i] = true;
            kernels::and_bytes(&mut mask, self.row_state[i].at.bytes());
        }
        if let Some(dead) = dead {
            kernels::andnot_words(&mut mask, dead);
        }
        if !kernels::any_set(&mask) {
            self.answer.windows_pruned += 1;
            self.words = mask;
            self.row_loaded = loaded;
            self.sub_bounds = sb;
            self.plan = plan;
            return Ok(());
        }
        for (i, done) in loaded.iter().enumerate() {
            if self.present[i] && !done {
                self.or_load(i, g, words)?;
            }
        }
        self.row_loaded = loaded;
        // Each member of the mask weighed alone: the bounds of the terms it
        // holds, summed, must reach the threshold. A word's members are
        // weighed first all at once, by the group's bounds of the terms
        // holding any of them; each member of a word they leave in reach then
        // by its sub-range's (1,024 slots), taken once per sub-range: tighter,
        // but a range bound per sparse term. A grouped term's is its group
        // entry's, the group's: one frontier per (term, group), its entry
        // starting at the group's first slot. (A word check at the sub-range's
        // bounds too cut 3% of the words it saw, for 2% of a `full_score`
        // disjunction's instructions.) A member's blocks bound it no higher,
        // so one this leaves below the threshold would fail its candidate
        // check. At 150 million rows a group's mask holds ~85 members in a
        // third of its words; weighing them alone, without a plan per
        // sub-range, cost less than sieving every word of it.
        let mut terms = std::mem::take(&mut self.or_terms);
        let mut rows = std::mem::take(&mut self.or_rows);
        let mut bounds = std::mem::take(&mut self.or_bounds);
        let mut sub = std::mem::take(&mut self.or_sub);
        let mut held = std::mem::take(&mut self.or_held);
        terms.clear();
        rows.clear();
        bounds.clear();
        for p in 0..self.present_list.len() {
            let i = self.present_list[p];
            if sb[i] > 0.0 {
                terms.push(i);
                rows.push(self.row_state[i].at);
                bounds.push(sb[i]);
            }
        }
        // A grouped term's sub-range bounds are its group bound, set once; a
        // sparse term's are taken per sub-range below. Without a sparse
        // term, every sub-range's sum is the group's.
        sub.clear();
        sub.extend_from_slice(&bounds);
        let sparse = terms.iter().any(|&i| !self.sc[i].grouped());
        let mut group_total = 0.0_f32;
        for &bound in &bounds {
            group_total += bound;
        }
        held.clear();
        held.resize(terms.len(), 0);
        // The sums the kernels weigh a sub-range's words and a word's members
        // by, lanes at a time (`kernels::word_sums`, `kernels::member_sums`).
        let mut sums = [0.0_f32; SUB_WORDS];
        let mut member_sums = [0.0_f32; 64];
        let mut bits = [0u8; 64];
        // The members left after the weighing below are sieved by their
        // buckets a chunk at a time before each is bounded alone, in a
        // group whose every weighed term is required (the disjunction is a
        // conjunction there) and whose mask holds many (a `full_score`
        // disjunction of dense words; a `score` one's holds some 85, which
        // their own checks bound for less).
        let sieving = plan.required.len() == terms.len()
            && kernels::popcount(&mask) >= OR_SIEVE_MIN
            && self.or_sieve(&terms, &sb);
        if sieving {
            self.sieve_words(&mut mask)?;
        }
        // A word's members are bounded from its first slot's ctid (see
        // `Walk::cut`), each member from its own.
        let first = |w: usize| geometry.tid_in(gi, (w * 64) as u32);
        let mut w0 = 0;
        while w0 < words {
            let w1 = (w0 + SUB_WORDS).min(words);
            // The live words, walked by their bits rather than tested.
            let mut live = 0u32;
            for (j, word) in mask[w0..w1].iter().enumerate() {
                live |= u32::from(*word != 0) << j;
            }
            // The sub-range's live words weighed at once, at the group's
            // bounds (in lanes; one at a time below otherwise).
            let weighed = live != 0 && self.threshold().is_some();
            if weighed && WEIGH_IN_LANES {
                kernels::word_sums(
                    &mask[w0..w1],
                    live,
                    w0,
                    &rows,
                    &bounds,
                    &mut sums[..w1 - w0],
                );
            }
            let mut bounded = false;
            while live != 0 {
                let w = w0 + live.trailing_zeros() as usize;
                live &= live - 1;
                let mut word = mask[w];
                if weighed {
                    let most = if WEIGH_IN_LANES {
                        sums[w - w0]
                    } else {
                        let mut most = 0.0_f32;
                        for ((h, row), &bound) in held.iter_mut().zip(&rows).zip(&bounds) {
                            *h = row.word(w);
                            most += or_zero(bound, *h & word != 0);
                        }
                        most
                    };
                    if self.cut(most, || first(w)) {
                        continue;
                    }
                    if !bounded {
                        bounded = true;
                        let total = if sparse {
                            let from = base + (w0 * 64) as u32;
                            let to = (base + (w1 * 64) as u32 - 1).min(end);
                            let mut total = 0.0_f32;
                            for (&i, bound) in terms.iter().zip(sub.iter_mut()) {
                                if !self.sc[i].grouped() {
                                    *bound = self.sc[i].range_bound(from, to);
                                }
                                total += *bound;
                            }
                            total
                        } else {
                            group_total
                        };
                        // The sub-range's words left start at `w`.
                        if self.cut(total, || first(w)) {
                            break;
                        }
                    }
                    // The word's members, each at the sub-range's bounds of
                    // the terms holding it: lanes at a time where the word
                    // holds enough of them to fill a vector, else one at a
                    // time (most live words hold one to three). The terms'
                    // words were read by the word's own check one at a time.
                    if WEIGH_IN_LANES {
                        for (h, row) in held.iter_mut().zip(&rows) {
                            *h = row.word(w);
                        }
                    }
                    let mut keep = 0u64;
                    if WEIGH_IN_LANES && word.count_ones() as usize >= MEMBER_LANES {
                        let mut n = 0;
                        let mut left = word;
                        while left != 0 {
                            bits[n] = left.trailing_zeros() as u8;
                            n += 1;
                            left &= left - 1;
                        }
                        let member_sums = &mut member_sums[..n];
                        member_sums.fill(0.0);
                        kernels::member_sums(&held, &bits[..n], &sub, member_sums);
                        for (&bit, &sum) in bits[..n].iter().zip(member_sums.iter()) {
                            let bit = u32::from(bit);
                            let tid = || geometry.tid_in(gi, (w * 64) as u32 + bit);
                            keep |= u64::from(!self.cut(sum, tid)) << bit;
                        }
                    } else {
                        let mut left = word;
                        while left != 0 {
                            let bit = left.trailing_zeros();
                            left &= left - 1;
                            let mut sum = 0.0_f32;
                            for (h, &bound) in held.iter().zip(&sub) {
                                sum += or_zero(bound, *h >> bit & 1 == 1);
                            }
                            let tid = || geometry.tid_in(gi, (w * 64) as u32 + bit);
                            keep |= u64::from(!self.cut(sum, tid)) << bit;
                        }
                    }
                    word = keep;
                }
                if sieving {
                    self.or_keep(g, base, w, word)?;
                    continue;
                }
                while word != 0 {
                    let bit = word.trailing_zeros();
                    word &= word - 1;
                    self.or_candidate(g, base, w, bit)?;
                }
            }
            w0 = w1;
        }
        if sieving {
            self.or_sieved(g, base)?;
        }
        if !self.staged.is_empty() {
            self.finish_staged(g)?;
        }
        self.or_terms = terms;
        self.or_rows = rows;
        self.or_bounds = bounds;
        self.or_sub = sub;
        self.or_held = held;
        self.words = mask;
        self.sub_bounds = sb;
        self.plan = plan;
        Ok(())
    }

    /// Plans a group whose present terms' bounds are `sb` (summing to
    /// `total`) at threshold `theta`.
    #[inline(never)]
    fn plan_group(&self, plan: &mut Plan, theta: Option<f32>, sb: &[f32], total: f64) {
        plan.required.clear();
        plan.essential.clear();
        plan.order.clear();
        plan.by_essential = true;
        let present = (0..sb.len()).filter(|i| self.present[*i]);
        let t = theta.map_or(0.0, f64::from);
        // Without a positive threshold every member of every term may enter.
        if !(t > 0.0 && t.is_finite()) || sb.iter().any(|b| b.is_nan() || *b < 0.0) {
            plan.essential.extend(present);
            return;
        }
        let slack = 1.0 + f64::from(f32::EPSILON) * 256.0;
        let mut fixed = 0.0_f64;
        for i in present {
            if (total - f64::from(sb[i])) * slack < t {
                plan.required.push(i);
                fixed += f64::from(sb[i]);
            } else if sb[i] > 0.0 {
                plan.order.push((sb[i], i));
            }
        }
        plan.order.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let mut tail = fixed;
        let mut inessential = 0;
        for &(bound, _) in &plan.order {
            let next = tail + f64::from(bound);
            if next * slack >= t {
                break;
            }
            tail = next;
            inessential += 1;
        }
        plan.by_essential = plan.required.is_empty() || inessential > 0;
        plan.essential
            .extend(plan.order[inessential..].iter().map(|&(_, i)| i));
    }

    /// The posting index of member `local` of scoring term `i`'s row;
    /// members must be asked in increasing order.
    #[inline]
    fn row_index(&mut self, i: usize, local: u32) -> u32 {
        let r = &mut self.row_state[i];
        let w = local as usize / 64;
        if w > r.w {
            r.run += kernels::popcount_bytes(&r.at.bytes()[r.w * 8..w * 8]);
            r.w = w;
        }
        r.first + r.run + (r.at.word(w) & ((1u64 << (local % 64)) - 1)).count_ones()
    }

    /// Bounds the candidate at bit `bit` of word `w` of group `g` and, if
    /// it is left in reach, stages it: the group's staged candidates are
    /// scored best bound first ([`Walk::finish_staged`]), at the group's end
    /// or every [`STAGE_CHUNK`], so the best of a group raise the threshold
    /// over the rest before their lengths are read (TIN reads the DL
    /// sidecar of a disjunction for about as many documents with 15 terms
    /// as with 2; scored in ctid order, each later, better document of a
    /// group read its length). A node's own check ([`Verify::Node`]) reads
    /// cursors that only move forward, so its candidates are scored in
    /// ctid order as they come.
    ///
    /// Kept out of line, as the group and plan steps are: inlined into the
    /// word loop it measured 8% slower over the disjunction trace.
    #[inline(never)]
    fn or_candidate(&mut self, g: u32, base: u32, w: usize, bit: u32) -> Result<()> {
        let local = (w * 64) as u32 + bit;
        if let Some((reach, length)) = self.or_stage(g, base, w, bit)? {
            if !matches!(self.verify, Verify::Flat) {
                return self.finish(g, local, length);
            }
            self.staged.push((reach, local, length));
            self.staged_buckets.extend_from_slice(&self.buckets);
            if self.staged.len() >= STAGE_CHUNK {
                self.finish_staged(g)?;
            }
        }
        Ok(())
    }

    /// Sets up the sieve of a disjunction's group ([`Walk::sieve`]) whose
    /// weighed terms are `terms` and present terms' bounds `sb`: its grouped
    /// terms' rows are read, any other bounds every candidate by its bound
    /// over the group. False when there is no bar yet or no term to read.
    fn or_sieve(&mut self, terms: &[usize], sb: &[f32]) -> bool {
        if self.bar.is_none() {
            return false;
        }
        self.sieve_terms.clear();
        // A term absent from the group adds nothing, any other at most its
        // bound over the group.
        self.sieve_ub.clear();
        self.sieve_ub.extend_from_slice(sb);
        for &i in terms {
            let entry = self.row_state[i].entry;
            if let Some(group) = self.sc[i].group_entry(entry) {
                self.sieve_terms.push(SieveTerm {
                    i,
                    entry,
                    group,
                    row: true,
                    prefix: usize::MAX,
                    table: [f32::NAN; BUCKET_COUNT],
                    floors: [0; BUCKET_COUNT],
                });
            }
        }
        self.ready_sieve()
    }

    /// Keeps the members of `word`, word `w` of group `g` (first slot
    /// `base`), for the sieve, and sieves them once a chunk is kept.
    #[inline(never)]
    fn or_keep(&mut self, g: u32, base: u32, w: usize, mut word: u64) -> Result<()> {
        while word != 0 {
            self.sieve_cands
                .push((w * 64) as u32 + word.trailing_zeros());
            word &= word - 1;
        }
        if self.sieve_cands.len() >= SIEVE_CHUNK {
            self.or_sieved(g, base)?;
        }
        Ok(())
    }

    /// Sieves the disjunction candidates kept ([`Walk::or_keep`]) of group
    /// `g` (first slot `base`) and bounds each one left alone
    /// ([`Walk::or_candidate`]).
    fn or_sieved(&mut self, g: u32, base: u32) -> Result<()> {
        let mut left = std::mem::take(&mut self.sieve_cands);
        let mut result = Ok(());
        if left.len() >= SIEVE_MIN {
            result = self.sieve(g, &mut left);
        }
        for &local in &left {
            if result.is_err() {
                break;
            }
            result = self.or_candidate(g, base, local as usize / 64, local % 64);
        }
        left.clear();
        self.sieve_cands = left;
        result
    }

    /// Bounds the candidate at bit `bit` of word `w` of group `g` by what
    /// can be read before its length from the DL sidecar (see
    /// [`Walk::stage`]): its terms' blocks, then its buckets, and a length
    /// a term it holds carries inline. Its buckets are left in
    /// [`Walk::buckets`] ([`NO_BUCKET`] for a term it lacks).
    fn or_stage(&mut self, g: u32, base: u32, w: usize, bit: u32) -> Result<Option<(f32, u32)>> {
        let local = (w * 64) as u32 + bit;
        let slot = base + local;
        self.answer.candidates += 1;
        // Its ctid only breaks a tie with the threshold's: most candidates
        // never need it.
        let geometry = self.geometry;
        let tid = move || geometry.tid_in(g as usize, local);
        let mut bound = 0.0_f32;
        self.held_list.clear();
        for p in 0..self.present_list.len() {
            let i = self.present_list[p];
            if self.row_state[i].at.word(w) >> bit & 1 == 1 {
                self.held_list.push(i);
                // A member's block is its posting index's: the footer is
                // decoded whole (`Sc::decode_all`), and its blocks hold
                // `block_size` postings each.
                let index = self.row_index(i, local);
                self.held_index[i] = index;
                let entry = self.row_state[i].entry;
                let s = &mut self.sc[i];
                let b = s.block_of(index, entry);
                debug_assert_eq!(Some(b), s.block_at(slot));
                self.blocks[i] = b;
                let v = s.bound(b);
                self.held_bound[i] = v;
                bound += v;
            }
        }
        if self.cut(bound, tid) {
            return Ok(None);
        }
        // The candidate's buckets, before its length is read (see
        // `Walk::stage`), a term at a time while the candidate is left in
        // reach, the term bounding it highest first (as TIN reads a term's
        // TF only for a candidate that still needs it). A bucket read bounds
        // the document's length from below: by the shortest document its
        // block holds at that bucket or above. A document has one length, so
        // the longest of those floors bounds every term it holds (TIN bounds
        // a document at one shared length): a term read by its bucket there,
        // any other by its block's largest bucket there (and its block's
        // bound). A rare word's bucket in a long document leaves the common
        // words' blocks, bounded by their short documents, far below the
        // threshold, before their TF is read. The bounds are summed in the
        // scorer's order, each no lower than its term's score, so the sum
        // bounds the candidate.
        self.buckets.fill(NO_BUCKET);
        let mut floor = 0u32;
        let mut reach = bound;
        loop {
            let mut top = usize::MAX;
            let mut best = f32::NEG_INFINITY;
            for &h in &self.held_list {
                if self.buckets[h] == NO_BUCKET && self.held_bound[h] > best {
                    (best, top) = (self.held_bound[h], h);
                }
            }
            if top == usize::MAX {
                break;
            }
            let f = self.or_bucket_of(top)?;
            if f > floor {
                floor = f;
                reach = self.or_rebound(floor);
            } else {
                let s = &self.sc[top];
                let bucket = TfBucket::new(self.buckets[top]).expect("a bucket read");
                self.held_bound[top] = s.scorer.bound_through(bucket, floor);
                reach = 0.0;
                for &h in &self.held_list {
                    reach += self.held_bound[h];
                }
            }
            if self.cut(reach, tid) {
                return Ok(None);
            }
        }
        let carrier = self.held_list.iter().copied().find(|i| {
            self.terms[self.sc[*i].term]
                .as_ref()
                .is_some_and(|s| s.postings.lengths.is_some())
        });
        Ok(Some(match carrier {
            Some(i) => {
                let index = self.held_index[i];
                let set = self.terms[self.sc[i].term].as_ref().expect("scoring");
                let inline = set.postings.lengths.as_ref().expect("inline lengths");
                self.touch
                    .touch(Part::Payload, set.at + inline.at_of(index), 4);
                let length = inline.get(index)?;
                // The length known, its buckets bound it at that length.
                (self.reach(self.held_list.iter().copied(), length), length)
            }
            None => (reach, u32::MAX),
        }))
    }

    /// Bounds each scoring term the candidate at hand holds at length
    /// `floor`, which its length is no shorter than, into
    /// [`Walk::held_bound`]: a term whose bucket is read by that bucket, any
    /// other by its block's largest bucket (and no higher than it was).
    /// Returns their sum, in the scorer's order.
    #[inline]
    fn or_rebound(&mut self, floor: u32) -> f32 {
        let mut sum = 0.0_f32;
        for &h in &self.held_list {
            let s = &self.sc[h];
            let read = self.buckets[h];
            let v = if read == NO_BUCKET {
                let most = TfBucket::new(s.max_bucket(self.blocks[h])).expect("a valid bucket");
                s.scorer.bound_through(most, floor).min(self.held_bound[h])
            } else {
                s.scorer
                    .bound_through(TfBucket::new(read).expect("a valid bucket"), floor)
            };
            self.held_bound[h] = v;
            sum += v;
        }
        sum
    }

    /// Reads into [`Walk::buckets`] the bucket of held scoring term `i`
    /// for the candidate at hand (its posting index in
    /// [`Walk::held_index`], its block in [`Walk::blocks`]); returns the
    /// shortest length the block holds at that bucket or above.
    #[inline(always)]
    fn or_bucket_of(&mut self, i: usize) -> Result<u32> {
        let index = self.held_index[i];
        let block = self.blocks[i];
        let s = &mut self.sc[i];
        let set = self.terms[s.term].as_ref().expect("scoring");
        let bucket = s.bucket_in(block, set.postings.tf, index)?;
        if s.footer.single().is_none() {
            self.touch.touch(
                Part::TfTail,
                set.at + set.postings.tf_at + s.footer.tf_byte(block),
                1,
            );
        }
        let floor = s.floor_at(block, bucket);
        self.buckets[i] = bucket;
        Ok(floor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_entrys_bound_covers_its_postings() {
        use crate::bm25::Bm25Params;
        use segment::payload::PayloadBuilder;
        use segment::tinshape::postings::{GroupFrontiers, Options};
        use segment::tinshape::segment::Builder;
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..24 {
            // 40 groups of 64 pages of 1 to 4 documents, a term in one
            // document in three (or in every one of a group, or none).
            let mut docs: Vec<Tid> = Vec::new();
            for g in 0..40u32 {
                for p in 0..64u32 {
                    let n = 1 + (next() % 4) as u16;
                    for o in 1..=n {
                        docs.push(Tid {
                            block: g * 256 + p,
                            offset: o,
                        });
                    }
                }
            }
            let lengths: Vec<u32> = (0..docs.len())
                .map(|_| 1 + (next() % 3000) as u32)
                .collect();
            let mut members: Vec<(usize, u32)> = Vec::new();
            for (r, doc) in docs.iter().enumerate() {
                let g = doc.block / 256;
                let held = match g % 5 {
                    0 => true,
                    1 => false,
                    _ => next() % 3 == 0,
                };
                if held {
                    members.push((r, 1 + (next() % 40) as u32));
                }
            }
            let rounded = round % 2 == 0;
            let options = Options {
                sparse: false,
                grid_min_postings: 0,
                inline_lengths_min_documents: u32::MAX,
                group_frontiers: if rounded {
                    GroupFrontiers::Rounded
                } else {
                    GroupFrontiers::Exact
                },
                ..Options::default()
            };
            let mut builder = Builder::new(docs.clone(), lengths.clone(), options).unwrap();
            let ranks: Vec<u32> = members.iter().map(|(r, _)| *r as u32).collect();
            let buckets: Vec<u8> = members
                .iter()
                .map(|(_, tf)| TfBucket::from_count(*tf).value())
                .collect();
            let mut payload = PayloadBuilder::default();
            for (_, tf) in &members {
                payload.push(&(0..*tf).collect::<Vec<u32>>()).unwrap();
            }
            builder
                .add_term("t", &ranks, &buckets, &payload.finish())
                .unwrap();
            let blob = builder.finish(&[]).0;
            let segment = Segment::parse(&blob).unwrap();
            let (k1, b, boost, average) = [
                (1.2f32, 0.75f32, 1.0f32, 300.0f32),
                (0.0, 0.0, 0.5, 13.0),
                (3.0, 1.0, 7.25, 4999.0),
                (1e-6, 0.25, 1.0, 0.5),
            ][round % 4];
            let scorer = TermScorer::from_statistics(
                docs.len() as u64,
                members.len() as u64,
                boost,
                Bm25Params { k1, b },
                average,
            )
            .unwrap();
            let set = TermSet::open(&segment, "t", &mut crate::tinshape::NoTouch)
                .unwrap()
                .unwrap();
            assert!(
                matches!(set.postings.form, Form::Grouped(_)),
                "a grouped record"
            );
            let mut sc = Sc::new(0, scorer.clone(), &set, &segment).unwrap();
            let geometry = &segment.docs.geometry;
            let mut held = 0;
            for g in 0..geometry.groups.len() {
                let group = geometry.groups[g];
                let (base, end) = (group.slot_base, group.slot_base + group.slots() - 1);
                let bound = sc.range_bound(base, end);
                let mut any = false;
                let mut shortest = [u32::MAX; 16];
                for (r, tf) in &members {
                    let slot = geometry.slot_of(docs[*r]).unwrap();
                    if slot < base || slot > end {
                        continue;
                    }
                    any = true;
                    let score = scorer.score_bucket(TfBucket::from_count(*tf), lengths[*r]);
                    assert!(
                        bound.total_cmp(&score).is_ge(),
                        "round {round} group {g}: bound {bound} below score {score} (tf {tf}, length {})",
                        lengths[*r]
                    );
                    let bucket = usize::from(TfBucket::from_count(*tf).value());
                    shortest[bucket] = shortest[bucket].min(lengths[*r]);
                }
                if any {
                    held += 1;
                    // The window: the entry holding the group, ending with it.
                    let b = sc.block_at(base).unwrap();
                    assert_eq!(sc.footer.last(b), end);
                    assert!(!sc.footer.starts_after(b, end));
                    assert_eq!(sc.bound(b).to_bits(), bound.to_bits());
                    let mut least = u32::MAX;
                    for bucket in (0..16).rev() {
                        least = least.min(shortest[bucket]);
                        let (floor, v) = sc.floor_kept(b, bucket as u8);
                        if least != u32::MAX {
                            assert!(
                                floor <= least,
                                "group {g} bucket {bucket}: floor {floor} over {least}"
                            );
                            assert!(
                                v.total_cmp(
                                    &scorer
                                        .bound_through(TfBucket::new(bucket as u8).unwrap(), least)
                                )
                                .is_ge()
                            );
                        }
                    }
                } else {
                    assert_eq!(
                        bound.to_bits(),
                        0.0f32.to_bits(),
                        "round {round} group {g} the term lacks"
                    );
                }
            }
            assert_eq!(held, sc.footer.blocks());
        }
    }

    #[test]
    fn group_from_finds_the_group_holding_a_slot_from_any_earlier_group() {
        // Groups of uneven widths, some heap groups skipped.
        let mut tids = Vec::new();
        for group in [0u32, 1, 2, 5, 6, 9, 10, 11, 12, 30, 31, 40] {
            let width = 1 + (group % 4) as u16;
            for page in (0..256).step_by(1 + group as usize % 7) {
                for offset in 1..=width {
                    tids.push(Tid {
                        block: group * 256 + page,
                        offset,
                    });
                }
            }
        }
        let geometry = Geometry::of(&tids).unwrap();
        let last = geometry.groups[geometry.groups.len() - 1];
        let slots = last.slot_base + last.slots();
        for slot in (0..slots).step_by(37).chain([0, slots - 1]) {
            let holding = geometry.group_of_slot(slot);
            for from in 0..=holding {
                assert_eq!(
                    group_from(&geometry, from, slot),
                    holding,
                    "slot {slot} from {from}"
                );
            }
        }
    }
}
