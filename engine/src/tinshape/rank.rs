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
use segment::payload::SKIP_INTERVAL;
use segment::tf_bucket::{BUCKET_COUNT, TfBucket};
use segment::tinshape::bits;
use segment::tinshape::blob::{Bytes, LazyBlob};
use segment::tinshape::docs::{Geometry, Group};
use segment::tinshape::ef::{Ef, EfCursor};
use segment::tinshape::positions::{Block, Positions};
use segment::tinshape::postings::{
    Directory, Form, GroupEntry, GroupFrontier, InOrder, KIND_ARRAY, KIND_EF, KIND_GRID,
    LazyFooter, SUB_NONE, SubCodes, array_bytes, fold_group_frontier, higher, lower,
    rounded_length,
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

/// A led walk's group of all grids holding at least this many candidates
/// is cut by its scoring terms' sub-range codes first ([`Walk::sub_cut`]):
/// the cut reads a byte per term, frontier pair and word holding one. At
/// 32.6M rows (M4, whole trace) 32 cut `full_score` conjunctions' cycles
/// 23% and phrases' 10%, 96 15% and 9%, 256 4% and 7%: a dense
/// conjunction's group holds about 100 candidates.
const SUB_MIN: u64 = 32;

/// A group with at least this many candidates has them sieved by their
/// buckets ([`Walk::sieve`]) this many at a time ([`SIEVE_CHUNK`]) before
/// any is bounded alone.
const SIEVE_MIN: usize = 32;
const SIEVE_CHUNK: usize = 256;

/// A disjunction's group is sieved ([`Walk::or_sieve`]) when its mask
/// holds at least this many members.
const OR_SIEVE_MIN: u64 = 128;

/// A disjunction's candidate holding at least this many scoring terms
/// reads its length class before its buckets ([`Walk::or_stage`]); one
/// holding fewer reads it after them, only if they leave it in reach. A
/// class read first saves a bucket read for each term it rules out, and
/// costs a cache line of the class plane (64 bytes, 256 documents) that a
/// sparse candidate shares with no other: at 150M rows a score
/// disjunction's candidates (dense terms elided) hold few terms, its class
/// reads first touched 11.2K distinct lines a query, two in three new;
/// `full_score`'s hold many, and read first its candidates read 62% fewer
/// buckets. At four, score disjunctions touch 42% fewer class lines for
/// 0.35% more instructions, `full_score` ones 0.2% more.
const CLASS_FIRST_TERMS: usize = 4;

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
/// with the parsed record, read in place, its bytes the frontiers), and the
/// segment's groups, which turn an entry's group index into the group's
/// last slot.
struct GroupBounds<'s> {
    dir: std::rc::Rc<Directory>,
    rounded: bool,
    groups: &'s [Group],
}

impl GroupBounds<'_> {
    /// Entry `b` (as a rule the entry the directory read last).
    #[inline]
    fn entry(&self, b: usize) -> GroupEntry {
        self.dir
            .get(b, self.groups)
            .unwrap_or_else(|e| corrupt_footer(e))
    }

    /// The entry of group `g`, if the record holds it, read now (the walk
    /// bounds the group by it next).
    #[inline]
    fn find(&self, g: u32) -> Option<usize> {
        let b = self.dir.find(g)?;
        self.dir
            .entry_of(b, g, self.groups)
            .unwrap_or_else(|e| corrupt_footer(e));
        Some(b)
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

    /// The first entry from `from` on whose group ends at `slot` or later
    /// (the first group so ending, sought); the entry count when there is
    /// none.
    #[inline]
    fn first_reaching(&self, from: usize, slot: u32) -> usize {
        let g = self
            .groups
            .partition_point(|group| group.slot_base + group.slots() <= slot);
        self.dir.seek(g as u32, from).0
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
            // Its first pairs without a branch on how many it holds.
            Self::Groups(g) => fold_group_frontier(
                g.dir.frontier_bytes(),
                g.entry(b).frontier as usize,
                g.rounded,
                bound,
            ),
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
    /// Per length class (a segment with classes), what each bucket scores
    /// at most at the class's shortest length: [`Walk::class_reach`] sums
    /// them where a candidate's class bounds it.
    class_scores: [[f32; BUCKET_COUNT]; 4],
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
            class_scores: [[0.0; BUCKET_COUNT]; 4],
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
        let bucket = |b: u8| TfBucket::new(b).expect("a valid bucket");
        // The scorer's rise tested once, not per pair (on x86-64 it held a
        // register and a branch in each of a frontier's unrolled pairs).
        let v = if scorer.rises() {
            self.footer
                .fold_bound(b, |b, length| scorer.score_bucket(bucket(b), length))
        } else {
            self.footer
                .fold_bound(b, |b, length| scorer.bound_through(bucket(b), length))
        };
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
    /// read in order as its groups are visited ([`Directory::step`]) and
    /// bounded then.
    fn decode_all(&mut self) {
        let f = match &mut self.footer {
            Bounds::Blocks(f) => f,
            Bounds::Groups(_) => return,
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

    /// The best bound of the blocks overlapping slots `from..=to` of group
    /// `g`; ranges must be asked in increasing order of `from`. A grouped
    /// record's is its entry's for the group, zero where it does not hold
    /// the group.
    #[inline]
    fn range_bound(&mut self, g: u32, from: u32, to: u32) -> f32 {
        if let Bounds::Groups(groups) = &self.footer {
            return match groups.find(g) {
                Some(b) => higher(0.0, self.bound(b)),
                None => 0.0,
            };
        }
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

    /// The block holding `slot` (of group `g`), if the term has a posting
    /// at or after it; slots must be asked in increasing order. A grouped
    /// record's is its entry for the group, if it holds the group.
    #[inline]
    fn block_at(&mut self, g: u32, slot: u32) -> Option<usize> {
        if let Bounds::Groups(groups) = &self.footer {
            return groups.find(g);
        }
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

    /// The census's window bounds ([`super::census`]): per window size,
    /// the term's bound over each window of the segment's groups. The
    /// folds counted and the group bound kept are left as they were.
    #[cfg(feature = "census")]
    fn window_bounds(&mut self, groups: &[Group]) -> [Vec<f32>; super::census::SIZES] {
        use super::census::{SHIFTS, SIZES};
        let n = groups.len();
        let mut out: [Vec<f32>; SIZES] = std::array::from_fn(|k| vec![0.0; (n >> SHIFTS[k]) + 1]);
        let put = |out: &mut [Vec<f32>; SIZES], from: usize, to: usize, v: f32| {
            for (o, shift) in out.iter_mut().zip(SHIFTS) {
                for w in &mut o[from >> shift..=to >> shift] {
                    *w = higher(*w, v);
                }
            }
        };
        if let Some(c) = self.common_bound() {
            for o in &mut out {
                o.fill(c);
            }
            return out;
        }
        let (computed, gb, gv) = (self.computed, self.gb, self.gv);
        let group_of = |slot: u32| {
            groups
                .partition_point(|g| g.slot_base + g.slots() <= slot)
                .min(n - 1)
        };
        if let Bounds::Groups(g) = &self.footer {
            let indexes: Vec<usize> = (0..g.dir.len())
                .map(|b| g.entry(b).index as usize)
                .collect();
            for (b, index) in indexes.into_iter().enumerate() {
                let v = higher(0.0, self.bound_of(b));
                put(&mut out, index, index, v);
            }
        } else {
            let blocks = self.first_reaching(0, u32::MAX);
            let mut start = 0u32;
            for b in 0..blocks {
                let last = self.footer.last(b);
                let v = higher(0.0, self.bound(b));
                put(&mut out, group_of(start), group_of(last), v);
                start = last.saturating_add(1);
            }
        }
        (self.computed, self.gb, self.gv) = (computed, gb, gv);
        out
    }

    /// A grouped record's directory entry `b`; `None` for a sparse record.
    #[inline]
    fn group_entry(&self, b: usize) -> Option<GroupEntry> {
        match &self.footer {
            Bounds::Groups(g) => (b < g.dir.len()).then(|| g.entry(b)),
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
    /// A led walk's run of the group's buckets ([`TfRun`]), taken at the
    /// first bucket it reads there ([`Walk::read_bucket`]).
    tf: TfRun,
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
                    // A candidate a few words on (most of a dense group's)
                    // counts them inline, a far one by the kernel.
                    self.run += if w - self.w <= 4 {
                        bytes[self.w * 8..w * 8]
                            .chunks_exact(8)
                            .map(|c| u64::from_le_bytes(c.try_into().expect("a word")).count_ones())
                            .sum::<u32>()
                    } else {
                        kernels::popcount_bytes(&bytes[self.w * 8..w * 8])
                    };
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

/// `x`'s place in [`f32::total_cmp`]'s order: `a.total_cmp(&b)` is
/// `order_key(a).cmp(&order_key(b))` (the standard library's own mapping).
#[inline(always)]
fn order_key(x: f32) -> i32 {
    let bits = x.to_bits() as i32;
    bits ^ (((bits >> 31) as u32) >> 1) as i32
}

/// The bar as [`Walk::keeps`] compares bounds with it: its score's
/// [`order_key`] and its ctid.
#[derive(Clone, Copy)]
struct BarKey {
    key: i32,
    tid: Tid,
}

impl BarKey {
    fn of(bar: Option<(f32, Tid)>) -> Option<Self> {
        bar.map(|(score, tid)| Self {
            key: order_key(score),
            tid,
        })
    }

    /// Whether a match of ctid `from()` scoring at most `bound` may still
    /// be kept: `!cut_by(bar, bound, from)`, without a branch on the
    /// comparison but for a tie, which is rare.
    #[inline(always)]
    fn keeps(self, bound: f32, from: impl FnOnce() -> Tid) -> bool {
        let k = order_key(bound);
        if k == self.key {
            return from() < self.tid;
        }
        k > self.key
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
pub(super) fn group_from(geometry: &Geometry, from: usize, slot: u32) -> usize {
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
    mem.tf = TfRun::default();
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

/// A term's positions read by posting index: each entry located in its
/// block, which is kept for the next read (a rare word's candidates often
/// share one).
pub(super) struct PosCursor<'a> {
    positions: Positions<'a>,
    at: usize,
    block: Option<Block>,
}

/// Bytes past an entry's gaps read with them for the vector bodies' loads.
const GAP_SLACK: usize = 32;

impl<'a> PosCursor<'a> {
    pub(super) fn new(stream: (segment::tinshape::blob::Bytes<'a>, usize)) -> Result<Self> {
        Ok(Self {
            positions: Positions::parse(stream.0)?,
            at: stream.1,
            block: None,
        })
    }

    /// Reads entry `index` into `out`.
    pub(super) fn read(
        &mut self,
        index: u32,
        out: &mut Vec<u32>,
        touch: &mut impl Touch,
    ) -> Result<()> {
        let block = match self.block {
            Some(b) if b.index == index / SKIP_INTERVAL => b,
            _ => {
                let b = self.positions.block_of(index)?;
                if let Some(s) = b.skip_at {
                    touch.touch(Part::Positions, self.at + s, 4);
                }
                let (head, len) = b.head_bytes();
                touch.touch(Part::Positions, self.at + head, len);
                self.block = Some(b);
                b
            }
        };
        let entry = self.positions.entry(&block, index)?;
        let (first, len) = entry.first_bytes();
        touch.touch(Part::Positions, self.at + first, len);
        if entry.count == 1 {
            out.clear();
            out.push(entry.first);
            return Ok(());
        }
        let (from, to) = entry.gap_bytes();
        touch.touch(Part::Positions, self.at + from, to - from);
        let gaps = if entry.count as usize - 1 < kernels::POSITIONS_SHORT {
            self.positions.bytes.get(from, to)?
        } else {
            self.positions.bytes.window(from, to - from + GAP_SLACK)?
        };
        kernels::positions_decode(&entry, gaps, out)
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
/// A scoring term's sub-range codes ([`SubCodes`]), for a led walk's
/// sub-range cut ([`Walk::sub_cut`]), and per frontier bucket and code the
/// term's bound at that bucket and the code's length, filled as asked (NaN
/// until then; allocated at the first cut).
struct SubTerm<'a> {
    codes: SubCodes<'a>,
    table: Vec<f32>,
}

impl SubTerm<'_> {
    /// What a posting of bucket at most `bucket`, at least as long as `code`
    /// says, scores at most by `scorer`; zero for [`SUB_NONE`].
    #[inline]
    fn bound(&mut self, scorer: &TermScorer, bucket: u8, code: u8) -> f32 {
        let at = usize::from(bucket) % BUCKET_COUNT * 256 + usize::from(code);
        let v = self.table[at];
        if !v.is_nan() {
            return v;
        }
        let v = if code == SUB_NONE {
            0.0
        } else {
            scorer.bound_through(
                TfBucket::new(bucket).expect("a valid bucket"),
                rounded_length(u32::from(code)),
            )
        };
        self.table[at] = v;
        v
    }
}

/// The buckets of the group frontier at `at` of a directory's bytes (its
/// head, then a varint per further pair whose low four bits are the bucket
/// gap less one) into `out`, and how many; 0 for a malformed one (the cut
/// then bounds the term by its group). Its own decoder: iterating a
/// [`GroupFrontier`] here as well cost every walk ~1% more instructions
/// (aarch64 and x86-64: the hot frontier folds compiled differently).
#[inline(never)]
fn frontier_buckets(bytes: &[u8], at: usize, out: &mut [u8; 16]) -> usize {
    let varint = |p: &mut usize| -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *bytes.get(*p)?;
            *p += 1;
            value |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    };
    let Some(&head) = bytes.get(at) else {
        return 0;
    };
    let n = usize::from(head >> 4) + 1;
    let mut bucket = head & 15;
    out[0] = bucket;
    let mut p = at + 1;
    if varint(&mut p).is_none() {
        return 0;
    }
    for slot in out.iter_mut().take(n).skip(1) {
        let Some(d) = varint(&mut p) else {
            return 0;
        };
        bucket = bucket.wrapping_add(1 + (d & 15) as u8) & 15;
        *slot = bucket;
    }
    n
}

/// A led walk's sub-range cut ([`Walk::sub_cut`]): per scoring term (by
/// index into `sc`) its codes, if its record has them, and the group at
/// hand's sub-ranges' bounds.
struct SubCut<'a> {
    terms: Vec<Option<SubTerm<'a>>>,
    sums: Vec<f32>,
    /// Per scoring term in the group at hand: its bound over the group, and
    /// whether its codes bound it per sub-range.
    bounds: Vec<(f32, bool)>,
}

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
    /// A disjunction's word at hand (`usize::MAX` for none yet in the
    /// group): per present term (as `present_list` orders them), its word
    /// of members there and the posting index of the first.
    cand_w: usize,
    cand_words: Vec<u64>,
    cand_base: Vec<u32>,
    blocks: Vec<usize>,
    sub_bounds: Vec<f32>,
    /// A led walk's sub-range cut ([`Walk::sub_cut`]), when a scoring term
    /// has codes: boxed, so walks without them carry a pointer.
    subs: Option<Box<SubCut<'a>>>,
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
    /// A disjunction group's term count (see [`count_plan`]): its weighed
    /// terms by bound, the counted ones, and their rows.
    or_order: Vec<usize>,
    or_heavy: Vec<usize>,
    or_counted: Vec<RowAt>,
    /// A disjunction group's terms whose rows are read only once a member
    /// is left in reach (see `Walk::or_group`), and a row holding every
    /// member, which stands in for theirs until then.
    or_lazy: Vec<usize>,
    or_ones: Vec<u64>,
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
    /// The length the candidate at hand's buckets bound it at (the longest
    /// of their blocks' floors), set by [`Walk::read_buckets`].
    cand_floor: u32,
    /// The window: the last slot of every scoring term's footer block at
    /// the slot it was set at, and its bound; per scoring term, its block's
    /// largest bucket ([`NO_BUCKET`] when it holds nothing from the slot
    /// on).
    window_end: Option<u32>,
    /// The groups the walk visits (all when empty).
    groups: Groups,
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
    /// The window bounds census, when built and enabled.
    #[cfg(feature = "census")]
    census: Option<Box<super::census::WalkCensus>>,
    /// The sub-range census, when built and enabled.
    #[cfg(feature = "census")]
    sub: Option<Box<super::census::SubWalk>>,
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
    /// members marked to filter candidates by ([`kernels::intersect_marked`]).
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

#[cfg(feature = "census")]
impl<T: Touch> Walk<'_, '_, T> {
    /// The census at group `g`, which the walk visits next, before it is
    /// bounded: the group before it is closed, and each window `g` enters
    /// is checked against the threshold now.
    fn census_group(&mut self, g: u32) {
        use super::census::{SHIFTS, SIZES};
        let Some(mut c) = self.census.take() else {
            return;
        };
        let folds: u64 = self.sc.iter().map(|s| s.computed).sum();
        c.close(folds, self.answer.windows_pruned);
        if c.bounds.is_empty() {
            let groups = &self.geometry.groups;
            c.bounds = self
                .sc
                .iter_mut()
                .map(|s| s.window_bounds(groups))
                .collect();
            for (s, b) in self.sc.iter().zip(&c.bounds) {
                if s.grouped() {
                    c.tally.entries += s.footer.blocks() as u64;
                    for (windows, b) in c.tally.windows.iter_mut().zip(b) {
                        *windows += b.iter().filter(|v| **v > 0.0).count() as u64;
                    }
                }
            }
        }
        let geometry = self.geometry;
        for k in 0..SIZES {
            let w = g >> SHIFTS[k];
            if c.current[k] == w {
                continue;
            }
            c.current[k] = w;
            c.tally.checks[k] += 1;
            let mut sum = 0.0_f32;
            for b in &c.bounds {
                sum += b[k][w as usize];
            }
            c.cut[k] = self.cut(sum, || geometry.tid_in(g as usize, 0));
            c.tally.cut_windows[k] += u64::from(c.cut[k]);
            c.visited[k].push((w, g, 0));
        }
        c.open = Some((folds, self.answer.windows_pruned));
        self.census = Some(c);
    }

    /// Closes the walk's census and adds it to this thread's tally.
    fn census_finish(&mut self) {
        let Some(mut c) = self.census.take() else {
            return;
        };
        let folds: u64 = self.sc.iter().map(|s| s.computed).sum();
        c.close(folds, self.answer.windows_pruned);
        let geometry = self.geometry;
        for k in 0..super::census::SIZES {
            for &(w, g, folds) in &c.visited[k] {
                let mut sum = 0.0_f32;
                for b in &c.bounds {
                    sum += b[k][w as usize];
                }
                if self.cut(sum, || geometry.tid_in(g as usize, 0)) {
                    c.tally.final_folds[k] += folds;
                }
            }
        }
        c.tally.walks += 1;
        super::census::add(&c.tally);
    }
}

#[cfg(feature = "census")]
impl<'a, T: Touch> Walk<'_, 'a, T> {
    /// The sub-range census ([`super::census::SubWalk`]) of group `g`,
    /// whose candidates are `cands` (local slots, ascending), at the
    /// threshold now. With a skip asked, its size's shift (its cuts in the
    /// census's `skip_cut`).
    #[allow(clippy::needless_range_loop, reason = "lab code: parallel arrays")]
    fn sub_census(&mut self, g: u32, cands: &[u32]) -> Result<Option<u32>> {
        use super::census::{ENCS, RECORDED, REPLAY, SUB_SHIFTS, SUBS};
        use segment::tinshape::postings::{round_length, rounded_length};
        let n = self.sc.len();
        if n == 0 {
            return Ok(None);
        }
        let Some(mut c) = self.sub.take() else {
            return Ok(None);
        };
        let key = (std::ptr::from_ref(self.segment) as usize, g);
        if super::census::mode() == REPLAY {
            let shift = RECORDED.with(|r| {
                let r = r.borrow();
                let (shift, sums, cut) = r.get(&key)?;
                c.skip_sums.clone_from(sums);
                c.skip_cut.clone_from(cut);
                Some(*shift)
            });
            c.skip_g = if shift.is_some() { g + 1 } else { 0 };
            self.sub = Some(c);
            return Ok(shift);
        }
        let group = self.geometry.groups[g as usize];
        let slots = group.slots();
        let chunks = slots.div_ceil(64) as usize;
        c.stairs.clear();
        c.stairs.resize(n * chunks, [u32::MAX; BUCKET_COUNT]);
        c.fixed.clear();
        c.fixed.resize(n, None);
        c.gbound.clear();
        c.gbound.resize(n, 0.0);
        c.mb.clear();
        c.mb.resize(n, [0.0; BUCKET_COUNT]);
        c.pairs.clear();
        c.pairs.resize(n, ([0; 16], 0));
        let least_df = super::census::sub_df() * f64::from(self.segment.documents);
        for i in 0..n {
            let t = self.sc[i].term;
            let dense = f64::from(self.terms[t].as_ref().expect("scoring").df) >= least_df;
            let grid = match self.mems[t].kind {
                Kind::Grid(bytes)
                    if dense
                        && self.mems[t].loaded == g + 1
                        && self.sc[i].grouped()
                        && self.mems[t].entry != usize::MAX =>
                {
                    Some(bytes)
                }
                _ => None,
            };
            let Some(bytes) = grid else {
                let base = group.slot_base;
                let v = self.sc[i].range_bound(g, base, base + slots - 1);
                c.fixed[i] = Some(v);
                c.tally.other_terms += 1;
                continue;
            };
            c.tally.grid_terms += 1;
            let (first, entry) = (self.mems[t].first, self.mems[t].entry);
            let tf = self.terms[t].as_ref().expect("scoring").postings.tf;
            let mut rank = 0u32;
            for (w, chunk) in bytes.chunks(8).enumerate() {
                let mut word = [0u8; 8];
                word[..chunk.len()].copy_from_slice(chunk);
                let mut word = u64::from_le_bytes(word);
                while word != 0 {
                    let local = w as u32 * 64 + word.trailing_zeros();
                    word &= word - 1;
                    let bucket = self.sc[i].bucket_in(entry, tf, first + rank)?;
                    rank += 1;
                    let Some(doc) = self.segment.docs.rank_in(g as usize, local) else {
                        continue;
                    };
                    let length = self.segment.lengths.get(doc)?;
                    // R10's u8 code: at most 254.
                    let code = rounded_length(round_length(length).min(254));
                    let at = &mut c.stairs[i * chunks + local as usize / 64][usize::from(bucket)];
                    *at = (*at).min(code);
                }
            }
            c.gbound[i] = higher(0.0, self.sc[i].bound(entry));
            if let Bounds::Groups(gb) = &self.sc[i].footer {
                let (buckets, count) = &mut c.pairs[i];
                for (bucket, _) in gb.frontier(entry) {
                    if *count < 16 {
                        buckets[*count] = bucket;
                        *count += 1;
                    }
                }
            }
            let mut best = 0.0_f32;
            for b in 0..BUCKET_COUNT {
                best = higher(best, self.sc[i].floor_kept(entry, b as u8).1);
                c.mb[i][b] = best;
            }
        }
        c.tally.groups += 1;
        c.tally.cands += cands.len() as u64;
        let skip = super::census::sub_skip();
        let mut out = None;
        let geometry = self.geometry;
        for k in 0..SUBS {
            let shift = SUB_SHIFTS[k];
            let per = 1usize << (shift - 6);
            let subs = chunks.div_ceil(per);
            let mut counts = vec![0u32; subs];
            for l in cands {
                counts[(*l >> shift) as usize] += 1;
            }
            let mut sums = vec![[0.0_f32; ENCS]; subs];
            for (j, sum) in sums.iter_mut().enumerate() {
                if counts[j] == 0 {
                    continue;
                }
                for i in 0..n {
                    if let Some(v) = c.fixed[i] {
                        for e in sum.iter_mut() {
                            *e += v;
                        }
                        continue;
                    }
                    let mut stair = [u32::MAX; BUCKET_COUNT];
                    for ch in
                        &c.stairs[i * chunks + j * per..i * chunks + ((j + 1) * per).min(chunks)]
                    {
                        for b in 0..BUCKET_COUNT {
                            stair[b] = stair[b].min(ch[b]);
                        }
                    }
                    // R10's codes: per frontier pair, the shortest of its
                    // buckets above the pair before's.
                    let pr_codes = {
                        let (buckets, count) = c.pairs[i];
                        let mut codes = [u32::MAX; 16];
                        let mut from = 0usize;
                        for k in 0..count {
                            let to = usize::from(buckets[k]);
                            for b in from..=to.min(BUCKET_COUNT - 1) {
                                codes[k] = codes[k].min(stair[b]);
                            }
                            from = to + 1;
                        }
                        (codes, buckets, count)
                    };
                    // Shortest at the bucket or above.
                    let mut least = u32::MAX;
                    let mut top = None;
                    for b in (0..BUCKET_COUNT).rev() {
                        if stair[b] != u32::MAX && top.is_none() {
                            top = Some(b);
                        }
                        least = least.min(stair[b]);
                        stair[b] = least;
                    }
                    let Some(top) = top else {
                        continue;
                    };
                    let scorer = &self.sc[i].scorer;
                    let bucket = |b: usize| TfBucket::new(b as u8).expect("a valid bucket");
                    let mut st = 0.0_f32;
                    for b in 0..=top {
                        st = higher(st, scorer.bound_through(bucket(b), stair[b]));
                    }
                    let bl = scorer.bound_through(bucket(top), stair[0]);
                    let mut pr = 0.0_f32;
                    let (codes, buckets, count) = pr_codes;
                    for k in 0..count {
                        if codes[k] != u32::MAX {
                            pr = higher(
                                pr,
                                scorer.bound_through(bucket(usize::from(buckets[k])), codes[k]),
                            );
                        }
                    }
                    let gb = c.gbound[i];
                    sum[0] += lower(st, gb);
                    sum[1] += lower(bl, gb);
                    sum[2] += lower(c.mb[i][top], gb);
                    sum[3] += lower(pr, gb);
                }
            }
            c.tally.subs[k] += counts.iter().filter(|n| **n > 0).count() as u64;
            let mut cut = vec![[false; ENCS]; subs];
            for j in 0..subs {
                if counts[j] == 0 {
                    continue;
                }
                let from = (j << shift) as u32;
                for e in 0..ENCS {
                    if self.cut(sums[j][e], || geometry.tid_in(g as usize, from)) {
                        cut[j][e] = true;
                        c.tally.cut_subs[k][e] += 1;
                        c.tally.cut_cands[k][e] += u64::from(counts[j]);
                    }
                }
            }
            for e in 0..ENCS {
                if (0..subs).all(|j| counts[j] == 0 || cut[j][e]) {
                    c.tally.cut_groups[k][e] += 1;
                }
            }
            if let Some((sk, se)) = skip
                && sk == k
            {
                c.skip_g = g + 1;
                c.skip_sums = sums.iter().map(|s| s[se]).collect();
                c.skip_cut = cut.iter().map(|s| s[se]).collect();
                RECORDED.with(|r| {
                    r.borrow_mut()
                        .insert(key, (shift, c.skip_sums.clone(), c.skip_cut.clone()))
                });
                out = Some(shift);
            }
        }
        self.sub = Some(c);
        Ok(out)
    }

    /// With a sub-range skip, whether the candidate at slot `local` of
    /// group `g` lies in a sub-range the threshold now cuts (counted once
    /// per candidate).
    #[inline]
    fn sub_late(&mut self, g: u32, local: u32) -> bool {
        let Some(c) = self.sub.as_ref() else {
            return false;
        };
        if c.skip_g != g + 1 {
            return false;
        }
        let Some((k, _)) = super::census::sub_skip() else {
            return false;
        };
        let shift = super::census::SUB_SHIFTS[k];
        let j = (local >> shift) as usize;
        let sum = c.skip_sums[j];
        let geometry = self.geometry;
        let from = (j << shift) as u32;
        if !self.cut(sum, || geometry.tid_in(g as usize, from)) {
            return false;
        }
        if let Some(c) = self.sub.as_mut() {
            c.tally.skipped_late += 1;
        }
        true
    }
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
        let (mut reached, mut loaded, mut read) = (0u64, 0u64, 0u64);
        for (set, mem) in self.terms.iter().zip(&self.mems) {
            if let Some(set) = set
                && let Some(dir) = &set.dir
            {
                reached += (mem.hint + 1).min(set.group_count()) as u64;
                loaded += u64::from(mem.used);
                read += dir.take_read();
            }
        }
        segment::tinshape::segment::count_memo(|c| {
            c.blocks_used += used;
            c.blocks_reached += blocks_reached;
            c.footer_blocks_whole += blocks_whole;
            c.footer_bytes_whole += bytes_whole;
            c.entries_reached += reached;
            c.entries_used += loaded;
            c.directory_entries += read;
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
            match self.groups.admit(g) {
                Admit::Yes => {}
                Admit::From(start) => {
                    from = start;
                    continue;
                }
                Admit::Done => break,
            }
            from = g + 1;
            let group = self.geometry.groups[g as usize];
            let (base, end) = (group.slot_base, group.slot_base + group.slots() - 1);
            if n == 0 && self.unscored_done(self.geometry.tid_in(g as usize, 0)) {
                // Nothing scores: every later match ties at zero and ranks
                // after the bar.
                break;
            }
            self.answer.windows += 1;
            #[cfg(feature = "census")]
            self.census_group(g);
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
            mem.tf = TfRun::default();
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
                bound += s.range_bound(g, base, end);
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
                None => s.range_bound(g, base, end),
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
                    kernels::intersect_marked(&mut cands, &mem.list, &mut self.marks);
                }
            }
        }
        #[cfg(feature = "census")]
        if self.sub.is_some()
            && !cands.is_empty()
            && let Some(shift) = self.sub_census(g, &cands)?
            && let Some(c) = self.sub.as_mut()
        {
            let before = cands.len();
            let cut = &c.skip_cut;
            cands.retain(|l| !cut[(*l >> shift) as usize]);
            c.tally.skipped += (before - cands.len()) as u64;
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
            #[cfg(feature = "census")]
            if self.sub_late(g, local) {
                continue;
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
                #[cfg(feature = "census")]
                if self.sub_late(g, local) {
                    continue;
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
        if self.subs.is_some() && self.bar.is_some() && kernels::popcount(&self.words) >= SUB_MIN {
            self.sub_cut(g)?;
        }
        #[cfg(feature = "census")]
        if self.sub.is_some() {
            // Replaying, only the recorded cuts are looked up.
            let skip = if super::census::mode() == super::census::REPLAY {
                self.sub_census(g, &[])?
            } else {
                let mut listed = std::mem::take(&mut self.cands);
                listed.clear();
                kernels::set_bits(&self.words, &mut listed);
                let skip = if listed.is_empty() {
                    None
                } else {
                    self.sub_census(g, &listed)?
                };
                self.cands = listed;
                skip
            };
            if let Some(shift) = skip
                && let Some(c) = self.sub.as_mut()
            {
                let per = 1usize << (shift - 6);
                let mut dropped = 0u64;
                for (j, cut) in c.skip_cut.iter().enumerate() {
                    if *cut {
                        let end = ((j + 1) * per).min(self.words.len());
                        for w in &mut self.words[j * per..end] {
                            dropped += u64::from(w.count_ones());
                            *w = 0;
                        }
                    }
                }
                c.tally.skipped += dropped;
            }
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

    /// Clears from [`Walk::words`], group `g`'s candidates (a bit per slot,
    /// every required term a grid there, loaded), the words no match in
    /// which can be kept: each word is a sub-range of [`SUB_SLOTS`] slots,
    /// bounded by the scoring terms' bounds there summed in the scorer's
    /// order, a term's by its sub-range codes where its grid has them (the
    /// best of its frontier pairs' buckets at their codes' lengths, at
    /// most its group bound), any other's by its bound over the group.
    /// Exact as [`Walk::cut`] is: each term's bound covers its postings in
    /// the sub-range. A cut word's candidates read no bucket and no length.
    ///
    /// [`SUB_SLOTS`]: segment::tinshape::postings::SUB_SLOTS
    #[inline(never)]
    fn sub_cut(&mut self, g: u32) -> Result<()> {
        let group = self.geometry.groups[g as usize];
        let (base, end) = (group.slot_base, group.slot_base + group.slots() - 1);
        let words = self.words.len();
        debug_assert_eq!(
            words,
            segment::tinshape::postings::sub_ranges(group.slots())
        );
        let SubCut {
            terms,
            sums,
            bounds,
        } = self.subs.as_deref_mut().expect("codes");
        // Each term's bound over the group, and whether codes bound it: the
        // others' bounds alone at the bar or above leave no word to cut
        // (a word's bound is at least theirs), and no code is read.
        bounds.clear();
        let mut fixed = 0.0_f32;
        let mut any = false;
        for (i, term) in terms.iter().enumerate() {
            let t = self.sc[i].term;
            let entry = self.mems[t].entry;
            let held = self.mems[t].loaded == g + 1 && entry != usize::MAX;
            let gb = if held && self.sc[i].grouped() {
                higher(0.0, self.sc[i].bound(entry))
            } else {
                self.sc[i].range_bound(g, base, end)
            };
            let coded = held && term.is_some() && matches!(self.mems[t].kind, Kind::Grid(_));
            if !coded {
                fixed += gb;
            }
            any |= coded;
            bounds.push((gb, coded));
        }
        if !any || self.bar.is_none_or(|(theta, _)| fixed >= theta) {
            return Ok(());
        }
        sums.clear();
        sums.resize(words, 0.0);
        let mut coded = false;
        for (i, term) in terms.iter_mut().enumerate() {
            let t = self.sc[i].term;
            let entry = self.mems[t].entry;
            let (gb, codable) = bounds[i];
            if codable
                && let Some(st) = term.as_mut()
                && let Bounds::Groups(groups) = &self.sc[i].footer
            {
                let mut buckets = [0u8; 16];
                let pairs = frontier_buckets(
                    groups.dir.bytes(),
                    groups.entry(entry).frontier as usize,
                    &mut buckets,
                );
                if pairs > 0
                    && let Some((codes, at)) = st.codes.of(entry, words * pairs)?
                {
                    if st.table.is_empty() {
                        st.table = vec![f32::NAN; BUCKET_COUNT * 256];
                    }
                    let set = self.terms[t].as_ref().expect("scoring");
                    self.touch
                        .touch(Part::Footer, set.at + st.codes.at + at, codes.len());
                    coded = true;
                    let scorer = &self.sc[i].scorer;
                    for (j, sum) in sums.iter_mut().enumerate() {
                        if self.words[j] == 0 {
                            continue;
                        }
                        let row = &codes[j * pairs..(j + 1) * pairs];
                        let mut v = 0.0_f32;
                        for k in 0..pairs {
                            v = higher(v, st.bound(scorer, buckets[k], row[k]));
                        }
                        *sum += lower(v, gb);
                    }
                    continue;
                }
            }
            for sum in sums.iter_mut() {
                *sum += gb;
            }
        }
        if coded {
            let geometry = self.geometry;
            for (j, sum) in sums.iter().enumerate() {
                if self.words[j] != 0
                    && cut_by(self.bar, *sum, || {
                        geometry.tid_in(g as usize, (j * 64) as u32)
                    })
                {
                    self.words[j] = 0;
                }
            }
        }
        Ok(())
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
                let Some(b) = s.block_at(g, slot) else {
                    *mb = NO_BUCKET;
                    *wt = 0.0;
                    if s.grouped() {
                        // It does not hold the group: the window ends with
                        // the group.
                        e = e.min(group_end);
                    }
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
            mem.tf = TfRun::default();
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
                let mut reach = self.read_buckets(g, local, tid)?;
                if self.cut(reach, tid) || self.too_few() {
                    return Ok(None);
                }
                // Then at its length class's shortest length, where that
                // is longer than its buckets' floor: at 150M rows most
                // candidates the buckets leave in reach fall short at
                // their exact length, and the class is a byte beside the
                // ones its group's other candidates read.
                if n > 0 {
                    let Some(r) = self.class_bound(g, local, self.cand_floor, reach, tid)? else {
                        return Ok(None);
                    };
                    reach = r;
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
            let block = self.sc[i].block_of(index, self.mems[t].entry);
            let bucket = self.read_bucket(i, block, index)?;
            let (f, bound) = self.sc[i].floor_kept(block, bucket);
            (floor, low) = (floor.max(f), low.min(f));
            own += bound;
            self.held_index[i] = index;
        }
        self.cand_floor = floor;
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
            self.cand_floor = floor;
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
            self.cand_floor = floor;
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
    /// posting `index`, which its block `block` holds (a grouped term's
    /// entry for the group at hand): from the group's run of buckets
    /// ([`TfRun`]), taken at its first read in the group, where it has one.
    #[inline]
    fn read_bucket(&mut self, i: usize, block: usize, index: u32) -> Result<u8> {
        let t = self.sc[i].term;
        if self.mems[t].tf.state == TfRun::UNSET {
            self.mems[t].tf = self.tf_run(i, block)?;
        }
        let run = self.mems[t].tf;
        let s = &mut self.sc[i];
        let set = self.terms[t].as_ref().expect("scoring");
        let bucket = if run.state == TfRun::READY {
            self.touch
                .touch(Part::TfTail, set.at + set.postings.tf_at + run.tail, 1);
            run.bucket(index)?
        } else {
            let bucket = s.bucket_in(block, set.postings.tf, index)?;
            if s.footer.single().is_none() {
                self.touch.touch(
                    Part::TfTail,
                    set.at + set.postings.tf_at + s.footer.tf_byte(block),
                    1,
                );
            }
            bucket
        };
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

    /// Bounds the candidate at slot `local` of group `g`, its buckets read
    /// and bounding it at `reach` at length `floor`, by its length class:
    /// at the class's shortest length, where that is longer than `floor`.
    /// `None` when that rules it out, else its bound. The class is read
    /// only for a candidate the longest class's floor would rule out (one
    /// it leaves in reach reads its length whatever its class).
    #[inline]
    fn class_bound(
        &mut self,
        g: u32,
        local: u32,
        floor: u32,
        reach: f32,
        tid: impl FnOnce() -> Tid + Copy,
    ) -> Result<Option<f32>> {
        let floors = self.segment.lengths.floors();
        let top = floors[3];
        if top <= floor || self.threshold().is_none() {
            return Ok(Some(reach));
        }
        let n = self.sc.len();
        let at_top = self.class_reach((0..n).filter(|i| self.buckets[*i] != NO_BUCKET), 3);
        if !self.cut(at_top, tid) {
            return Ok(Some(reach));
        }
        let c = self.class_of(g, local)?;
        let class = floors[c];
        if class <= floor {
            return Ok(Some(reach));
        }
        let bound = if class == top {
            at_top
        } else {
            self.class_reach((0..n).filter(|i| self.buckets[*i] != NO_BUCKET), c)
        };
        if self.cut(bound, tid) {
            self.answer.class_cuts += 1;
            return Ok(None);
        }
        Ok(Some(bound))
    }

    /// The shortest length of the length class of the document at slot
    /// `local` of group `g` (0 when the segment has no classes): its length
    /// is at least that.
    #[inline]
    fn class_floor(&mut self, g: u32, local: u32) -> Result<u32> {
        let lengths = &self.segment.lengths;
        if !lengths.has_classes() {
            return Ok(0);
        }
        let rank = self
            .segment
            .docs
            .rank_in(g as usize, local)
            .ok_or(Error::Corrupt("a posting without a document"))?;
        self.touch
            .touch(Part::DlClasses, self.segment.class_at(rank), 1);
        lengths.floor(rank)
    }

    /// The length class of the document at slot `local` of group `g`, in a
    /// segment with classes: its length is at least the class's floor.
    #[inline]
    fn class_of(&mut self, g: u32, local: u32) -> Result<usize> {
        let rank = self
            .segment
            .docs
            .rank_in(g as usize, local)
            .ok_or(Error::Corrupt("a posting without a document"))?;
        self.touch
            .touch(Part::DlClasses, self.segment.class_at(rank), 1);
        Ok(usize::from(self.segment.lengths.class(rank)?) % 4)
    }

    /// [`Walk::reach`] at length class `c`'s floor, from the terms' tables
    /// ([`Sc::class_scores`]): the same scores, summed in the same order.
    #[inline]
    fn class_reach(&self, held: impl Iterator<Item = usize>, c: usize) -> f32 {
        let mut reach = 0.0_f32;
        for i in held {
            reach += self.sc[i].class_scores[c][usize::from(self.buckets[i]) % BUCKET_COUNT];
        }
        reach
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

/// The groups a walk visits: ascending, disjoint ranges `[start, end)` of
/// group indexes, every group when there are none.
struct Groups {
    ranges: Vec<(u32, u32)>,
    at: usize,
}

/// Whether a walk visits a group.
enum Admit {
    Yes,
    /// Not this group: the next one visited is no earlier than this.
    From(u32),
    /// Nor any later one.
    Done,
}

impl Groups {
    fn new(ranges: &[(u32, u32)]) -> Self {
        debug_assert!(ranges.windows(2).all(|w| w[0].1 <= w[1].0));
        Self {
            ranges: ranges.to_vec(),
            at: 0,
        }
    }

    /// Whether group `g` is visited; asked in ascending order.
    #[inline]
    fn admit(&mut self, g: u32) -> Admit {
        if self.ranges.is_empty() {
            return Admit::Yes;
        }
        while let Some(&(_, end)) = self.ranges.get(self.at) {
            if g < end {
                break;
            }
            self.at += 1;
        }
        match self.ranges.get(self.at) {
            None => Admit::Done,
            Some(&(start, _)) if g < start => Admit::From(start),
            Some(_) => Admit::Yes,
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
        &[],
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
    groups: &[(u32, u32)],
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
        cand_w: usize::MAX,
        cand_words: Vec::with_capacity(n),
        cand_base: Vec::with_capacity(n),
        blocks: vec![0; n],
        sub_bounds: Vec::new(),
        subs: None,
        plan: Plan::default(),
        rows: Vec::new(),
        row_state: vec![Row::default(); n],
        row_loaded: Vec::new(),
        or_terms: Vec::new(),
        or_rows: Vec::new(),
        or_bounds: Vec::new(),
        or_sub: Vec::new(),
        or_held: Vec::new(),
        or_order: Vec::new(),
        or_heavy: Vec::new(),
        or_counted: Vec::new(),
        or_lazy: Vec::new(),
        or_ones: Vec::new(),
        held_bound: vec![0.0; n],
        buckets: vec![NO_BUCKET; n],
        held_index: vec![0; n],
        term_sc: Vec::new(),
        cand_floor: 0,
        window_end: None,
        groups: Groups::new(groups),
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
        #[cfg(feature = "census")]
        census: super::census::WalkCensus::start(),
        #[cfg(feature = "census")]
        sub: super::census::SubWalk::start(),
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
    // A led walk bounds a group's sub-ranges by its terms' codes.
    if !walk.req.is_empty()
        && walk.sc.iter().any(|s| {
            walk.terms[s.term]
                .as_ref()
                .is_some_and(|set| set.postings.subs.is_some())
        })
    {
        let terms = walk
            .sc
            .iter()
            .map(|s| {
                let set = walk.terms[s.term].as_ref().expect("scoring");
                set.postings.subs.as_deref().map(|codes| SubTerm {
                    codes: *codes,
                    table: Vec::new(),
                })
            })
            .collect();
        walk.subs = Some(Box::new(SubCut {
            terms,
            sums: Vec::new(),
            bounds: Vec::new(),
        }));
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
    if segment.lengths.has_classes() {
        let floors = segment.lengths.floors();
        for s in &mut walk.sc {
            for (scores, floor) in s.class_scores.iter_mut().zip(floors) {
                for (bucket, v) in scores.iter_mut().enumerate() {
                    let bucket = TfBucket::new(bucket as u8).expect("a valid bucket");
                    *v = s.scorer.bound_through(bucket, floor);
                }
            }
        }
    }
    if required.is_empty() {
        walk.run_or()?;
    } else {
        walk.run()?;
    }
    #[cfg(feature = "census")]
    walk.census_finish();
    #[cfg(feature = "census")]
    if let Some(mut c) = walk.sub.take() {
        c.tally.walks += 1;
        super::census::add_sub(&c.tally);
    }
    walk.count_metadata();
    let done = walk.answer;
    answer.scored += done.scored;
    answer.windows += done.windows;
    answer.windows_pruned += done.windows_pruned;
    answer.candidates += done.candidates;
    answer.position_checks += done.position_checks;
    answer.class_cuts += done.class_cuts;
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

/// The fewest members of a disjunction group's mask its members' terms are
/// counted for before they are weighed (see [`count_plan`]): below it the
/// count cost more than the weighing it spared (M4, whole trace: at 64,
/// 13% fewer `score` disjunction instructions, 19% fewer `full_score`;
/// counted in every group, the cheapest forms' rose 1-2%). Tests count in
/// every group.
const COUNT_MIN: u64 = if cfg!(test) { 1 } else { 64 };
/// The most terms a member is counted to: a filter asking for more asks for
/// this many (weaker, still exact).
const COUNT_MAX: usize = 4;
/// The most weighed terms a group's count is planned for (its sums are
/// exact well within [`Walk::plan_group`]'s slack up to many more).
const COUNT_TERMS: usize = 64;

/// Plans the term count of a disjunction's group whose weighed terms'
/// group bounds are `bounds`, at bar `theta`: the terms counted, in
/// `heavy`, and how many of them a member must hold, or `None` where no
/// count can drop a member.
///
/// Leaving the `L` lightest terms out of the count and their bounds as
/// slack, a member holding `k` of the others scores at most the `k`
/// largest of their bounds plus every light bound. The fewest `k` where
/// that reaches the bar, `m`, is how many heavy terms a member must hold;
/// one holding fewer is cut by the weighing anyway, at bounds no higher
/// (a member's sub-range bounds are at most the group's), so the count
/// keeps a superset of what the weighing keeps. Sums are taken in `f64`
/// with [`Walk::plan_group`]'s slack, and only members strictly below the
/// bar are dropped (a tie may rank above it by its ctid).
///
/// Of the `L`, the count kept is the one asking each counted term to carry
/// most, `min(m, COUNT_MAX) / (n - L)`, the fewest terms on a tie: in a
/// `full_score` disjunction the common words' small bounds inflate a plain
/// count (`L = 0`), which nearly every member then passes.
fn count_plan(
    bounds: &[f32],
    theta: f32,
    order: &mut Vec<usize>,
    heavy: &mut Vec<usize>,
) -> Option<usize> {
    let n = bounds.len();
    let t = f64::from(theta);
    let usable = (2..=COUNT_TERMS).contains(&n)
        && t > 0.0
        && t.is_finite()
        && bounds.iter().all(|b| *b >= 0.0 && b.is_finite());
    if !usable {
        return None;
    }
    let slack = 1.0 + f64::from(f32::EPSILON) * 256.0;
    order.clear();
    order.extend(0..n);
    order.sort_unstable_by(|&a, &b| bounds[b].total_cmp(&bounds[a]));
    // top[k]: the k largest bounds, summed.
    let mut top = [0.0_f64; COUNT_TERMS + 1];
    for k in 0..n {
        top[k + 1] = top[k] + f64::from(bounds[order[k]]);
    }
    let reaches = |sum: f64| sum * slack >= t;
    let mut best: Option<(usize, usize)> = None; // (m, heavy terms)
    // light: the L lightest bounds, summed. m (the fewest heavy terms that
    // reach the bar) only falls as L rises: each L starts from the last.
    let (mut light, mut m) = (0.0_f64, n);
    for l in 0..=n - 2 {
        if l > 0 {
            light += f64::from(bounds[order[n - l]]);
        }
        let h = n - l;
        m = m.min(h);
        if !reaches(top[m] + light) {
            // Not even every term reaches it: the group's own check cuts it.
            continue;
        }
        while m > 0 && reaches(top[m - 1] + light) {
            m -= 1;
        }
        // m == 0: the light terms alone reach the bar, and m == 1: one
        // heavy term does, which the mask (the essential terms' union, see
        // `Walk::plan_group`) holds already; no larger L asks more.
        if m < 2 {
            break;
        }
        let asked = m.min(COUNT_MAX);
        // asked / h larger, or equal with fewer terms counted.
        if best.is_none_or(|(bm, bh)| asked * bh >= bm * h) {
            best = Some((asked, h));
        }
    }
    let (m, h) = best?;
    heavy.clear();
    heavy.extend_from_slice(&order[..h]);
    Some(m)
}

/// Keeps the members of `mask` that at least `M` of `rows` hold: four
/// words at a time (a vector or two), a block of four without members
/// skipped.
#[inline(never)]
fn at_least<const M: usize>(mask: &mut [u64], rows: &[RowAt]) {
    let mut chunks = mask.chunks_exact_mut(4);
    let mut w = 0;
    for chunk in &mut chunks {
        let live: [u64; 4] = [chunk[0], chunk[1], chunk[2], chunk[3]];
        if live.iter().fold(0, |a, b| a | b) != 0 {
            // seen[k]: members held by more than k of the rows so far.
            let mut seen = [[0u64; 4]; M];
            for row in rows {
                let mut h = [0u64; 4];
                for j in 0..4 {
                    h[j] = row.word(w + j) & live[j];
                }
                let mut k = M - 1;
                while k > 0 {
                    for j in 0..4 {
                        seen[k][j] |= h[j] & seen[k - 1][j];
                    }
                    k -= 1;
                }
                for j in 0..4 {
                    seen[0][j] |= h[j];
                }
            }
            chunk.copy_from_slice(&seen[M - 1]);
        }
        w += 4;
    }
    for (j, word) in chunks.into_remainder().iter_mut().enumerate() {
        let live = *word;
        let mut seen = [0u64; M];
        for row in rows {
            let h = row.word(w + j) & live;
            let mut k = M - 1;
            while k > 0 {
                seen[k] |= h & seen[k - 1];
                k -= 1;
            }
            seen[0] |= h;
        }
        *word = seen[M - 1];
    }
}

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
    /// A grouped term's buckets in the group at hand, once a candidate's
    /// is read.
    tf: TfRun,
}

impl Row {
    /// Moves the cursor to word `w` (no earlier than its own): `run`
    /// counts the members in the words before it. The few words between
    /// two candidates are counted here; a longer stretch by the kernel,
    /// whose call and reduction cost more than a word or two.
    #[inline(always)]
    fn advance(&mut self, w: usize) {
        if w <= self.w {
            return;
        }
        self.run += if w - self.w <= 4 {
            (self.w..w)
                .map(|x| self.at.word(x).count_ones())
                .sum::<u32>()
        } else {
            kernels::popcount_bytes(&self.at.bytes()[self.w * 8..w * 8])
        };
        self.w = w;
    }
}

/// A grouped term's buckets in the group at hand (its directory entry's
/// part of the TF tail), on the page they lie on: each candidate's bucket
/// is read there, without looking up its entry and the page again. Taken
/// at the group's first bucket read, and only where the part lies on one
/// page (else, or for a sparse term, buckets are read as before). Valid
/// until [`Walk::group_done`] releases the group's pages, as [`RowAt`] is.
#[derive(Clone, Copy)]
struct TfRun {
    state: u8,
    width: u8,
    /// The entry's first posting index, and where its bucket starts in
    /// `bytes`, in bits.
    first: u32,
    bit: usize,
    bytes: *const u8,
    len: usize,
    /// Where the entry's buckets start in the TF tail, in bytes (for page
    /// accounting).
    tail: usize,
}

impl TfRun {
    const UNSET: u8 = 0;
    const READY: u8 = 1;
    const NONE: u8 = 2;

    /// The bucket of posting `index`, which the run holds.
    #[inline(always)]
    fn bucket(&self, index: u32) -> Result<u8> {
        debug_assert_eq!(self.state, Self::READY);
        let at = self.bit + (index - self.first) as usize * usize::from(self.width);
        // SAFETY: on a page the open span pinned (or a buffer it owns),
        // read before the group's pages are released (see the type).
        let bytes = unsafe { std::slice::from_raw_parts(self.bytes, self.len) };
        Ok(bits::get_at(bytes, at, u32::from(self.width))? as u8)
    }
}

impl Default for TfRun {
    fn default() -> Self {
        Self {
            state: Self::UNSET,
            width: 0,
            first: 0,
            bit: 0,
            bytes: std::ptr::NonNull::dangling().as_ptr(),
            len: 0,
            tail: 0,
        }
    }
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
        // A term is asked for its next group only once the walk has passed
        // its group at hand (`from` one past it), so a grouped term's next
        // is the entry after it: read in order, not sought.
        let grouped: Vec<bool> = self.sc.iter().map(Sc::grouped).collect();
        let mut order = vec![InOrder::default(); n];
        loop {
            let mut g = u32::MAX;
            for i in 0..n {
                if !fresh[i] || next[i] < from {
                    let t = self.sc[i].term;
                    let set = self.terms[t].as_mut().expect("scoring");
                    let hint = &mut self.mems[t].hint;
                    next[i] = if grouped[i] {
                        set.step_index(&mut order[i], hint)
                    } else {
                        next_group(set, self.geometry, from, hint, self.touch)
                    }
                    .unwrap_or(u32::MAX);
                    debug_assert!(next[i] >= from);
                    fresh[i] = true;
                }
                g = g.min(next[i]);
            }
            if g == u32::MAX {
                break;
            }
            match self.groups.admit(g) {
                Admit::Yes => {}
                // The groups before the next range are passed unread: each
                // term behind it sought there (a grouped term's walk in
                // order moved to its first group from there).
                Admit::From(start) => {
                    from = start;
                    for i in 0..n {
                        if next[i] < start {
                            fresh[i] = false;
                            if grouped[i] {
                                let t = self.sc[i].term;
                                let set = self.terms[t].as_ref().expect("scoring");
                                set.step_from(&mut order[i], start, self.mems[t].hint);
                            }
                        }
                    }
                    continue;
                }
                Admit::Done => break,
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
        r.tf = TfRun::default();
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
            let mut out = kernels::RowPut::new();
            cursor.drain_below(end, |slot| out.put(row, slot - group.slot_base));
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
                kernels::set_ascending(&set.locals[from as usize..to as usize], row);
            }
        }
        Ok(())
    }

    /// Whether scoring term `i`'s row in the group at hand may be read only
    /// once a member is left in reach: any but a grid, which is read in
    /// place.
    #[inline]
    fn or_lazy_row(&self, i: usize) -> bool {
        let t = self.sc[i].term;
        let set = self.terms[t].as_ref().expect("scoring");
        if matches!(set.postings.form, Form::Sparse(_)) {
            return true;
        }
        !matches!(set.group_at(self.mems[t].hint).src,
            Src::Container { entry, .. } if entry.kind == KIND_GRID)
    }

    /// Reads the rows of group `g` (of `words` words) left unread, `lazy`,
    /// in place of the full row each weighed one among `terms` stood in
    /// with in `rows`.
    #[inline(never)]
    fn or_load_lazy(
        &mut self,
        lazy: &mut Vec<usize>,
        terms: &[usize],
        rows: &mut [RowAt],
        g: u32,
        words: usize,
    ) -> Result<()> {
        for &i in lazy.iter() {
            self.or_load(i, g, words)?;
            self.row_loaded[i] = true;
            if let Some(t) = terms.iter().position(|&x| x == i) {
                rows[t] = self.row_state[i].at;
            }
        }
        lazy.clear();
        Ok(())
    }

    /// [`Walk::or_group_in`] for group `g`: on x86-64 built with POPCNT
    /// and BMI where the CPU has them ([`kernels::bit_ops`]), its
    /// candidates too.
    #[inline(never)]
    fn or_group(&mut self, g: u32) -> Result<()> {
        #[cfg(target_arch = "x86_64")]
        if kernels::bit_ops() {
            // SAFETY: the CPU has the features (`bit_ops`).
            return unsafe { self.or_group_bits(g) };
        }
        self.or_group_in::<false>(g)
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "popcnt,bmi1,bmi2,lzcnt")]
    unsafe fn or_group_bits(&mut self, g: u32) -> Result<()> {
        self.or_group_in::<true>(g)
    }

    /// [`Walk::or_candidate`], built with POPCNT and BMI when `BITS` (only
    /// where the CPU has them, see [`Walk::or_group`]). Called through a
    /// pointer the optimizer cannot see through: called directly from the
    /// group's code built with the same features, LLVM inlines it into the
    /// word loop despite `#[inline(never)]`, which the candidate step is
    /// kept out of (see [`Walk::or_candidate`]).
    ///
    /// Calling it is safe only where the CPU has the features when `BITS`.
    #[inline(always)]
    fn or_candidate_fn<const BITS: bool>()
    -> unsafe fn(&mut Self, u32, u32, usize, u32) -> Result<()> {
        #[cfg(target_arch = "x86_64")]
        if BITS {
            return std::hint::black_box(
                Self::or_candidate_bits as unsafe fn(&mut Self, u32, u32, usize, u32) -> Result<()>,
            );
        }
        Self::or_candidate
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "popcnt,bmi1,bmi2,lzcnt")]
    #[inline(never)]
    unsafe fn or_candidate_bits(&mut self, g: u32, base: u32, w: usize, bit: u32) -> Result<()> {
        self.or_candidate_in(g, base, w, bit)
    }

    #[inline(always)]
    fn or_group_in<const BITS: bool>(&mut self, g: u32) -> Result<()> {
        let n = self.sc.len();
        let group = self.geometry.groups[g as usize];
        let base = group.slot_base;
        let end = base + group.slots() - 1;
        self.answer.windows += 1;
        #[cfg(feature = "census")]
        self.census_group(g);
        // The present terms' bounds over the group.
        self.cand_w = usize::MAX;
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
                    s.range_bound(g, base, end)
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
        if self.or_ones.len() < words {
            self.or_ones.resize(words, !0);
        }
        let ones = RowAt::of(&self.or_ones[..words]);
        for p in 0..self.present_list.len() {
            let i = self.present_list[p];
            if sb[i] > 0.0 {
                terms.push(i);
                rows.push(ones);
                bounds.push(sb[i]);
            }
        }
        // A member must hold at least `m` of the heavier weighed terms (see
        // `count_plan`): counted for every member of the mask at once, four
        // words at a time, bit-sliced, before any member is weighed alone.
        // It keeps every member the weighing below would keep, so it only
        // spares the weighing those it would cut (research6).
        let mut order = std::mem::take(&mut self.or_order);
        let mut heavy = std::mem::take(&mut self.or_heavy);
        let count = match self.threshold() {
            Some(theta) if terms.len() >= 2 && kernels::popcount(&mask) >= COUNT_MIN => {
                count_plan(&bounds, theta, &mut order, &mut heavy)
            }
            _ => None,
        };
        let mut counted_terms = 0u64;
        if count.is_some() {
            for &t in &heavy {
                counted_terms |= 1 << t;
            }
        }
        // The other terms' rows: read now if counted (or if every weighed
        // term is required, where the sieve may read them), else, if not a
        // grid read in place, only once a member is left in reach by the
        // weighing below, which bounds every member by their bounds until
        // then, as if it held them all. At 150 million rows those terms
        // carry 3 (`score`) to 12 (`full_score`) times the postings of the
        // terms that make the mask, and most groups' masks leave no member
        // in reach.
        let lazy_ok = self.threshold().is_some() && plan.required.len() != terms.len();
        let mut lazy = std::mem::take(&mut self.or_lazy);
        lazy.clear();
        let mut t = 0;
        for p in 0..self.present_list.len() {
            let i = self.present_list[p];
            let weighed = sb[i] > 0.0;
            if !loaded[i] {
                let counted = weighed && t < 64 && counted_terms >> t & 1 == 1;
                if lazy_ok && !counted && self.or_lazy_row(i) {
                    lazy.push(i);
                } else {
                    self.or_load(i, g, words)?;
                    loaded[i] = true;
                }
            }
            if weighed {
                if loaded[i] {
                    rows[t] = self.row_state[i].at;
                }
                t += 1;
            }
        }
        self.row_loaded = loaded;
        if let Some(m) = count {
            let mut counted = std::mem::take(&mut self.or_counted);
            counted.clear();
            counted.extend(heavy.iter().map(|&t| rows[t]));
            match m {
                2 => at_least::<2>(&mut mask, &counted),
                3 => at_least::<3>(&mut mask, &counted),
                _ => at_least::<COUNT_MAX>(&mut mask, &counted),
            }
            self.or_counted = counted;
        }
        self.or_order = order;
        self.or_heavy = heavy;
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
        let candidate = Self::or_candidate_fn::<BITS>();
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
                // The words the bar cuts, all at once: the loop below walks
                // only the others, with no branch on each word's verdict
                // (the bar it is taken at only rises after).
                let bar = BarKey::of(self.bar).expect("weighed under a bar");
                let mut kept = 0u32;
                for (j, &sum) in sums[..w1 - w0].iter().enumerate() {
                    kept |= u32::from(bar.keeps(sum, || first(w0 + j))) << j;
                }
                live &= kept;
            }
            let mut bounded = false;
            while live != 0 {
                let w = w0 + live.trailing_zeros() as usize;
                live &= live - 1;
                let mut word = mask[w];
                if weighed {
                    if !WEIGH_IN_LANES {
                        let mut most = 0.0_f32;
                        for ((h, row), &bound) in held.iter_mut().zip(&rows).zip(&bounds) {
                            *h = row.word(w);
                            most += or_zero(bound, *h & word != 0);
                        }
                        if self.cut(most, || first(w)) {
                            continue;
                        }
                    }
                    if !bounded {
                        bounded = true;
                        let total = if sparse {
                            let from = base + (w0 * 64) as u32;
                            let to = (base + (w1 * 64) as u32 - 1).min(end);
                            let mut total = 0.0_f32;
                            for (&i, bound) in terms.iter().zip(sub.iter_mut()) {
                                if !self.sc[i].grouped() {
                                    *bound = self.sc[i].range_bound(g, from, to);
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
                    // Weighed again, by every term's own row, once the rows
                    // left unread are read (see the plan above).
                    let keep = loop {
                        // Each member's verdict without a branch on it (see
                        // `BarKey::keeps`), at the bar as it stands now.
                        let bar = BarKey::of(self.bar).expect("weighed under a bar");
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
                                keep |= u64::from(bar.keeps(sum, tid)) << bit;
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
                                keep |= u64::from(bar.keeps(sum, tid)) << bit;
                            }
                        }
                        if keep == 0 || lazy.is_empty() {
                            break keep;
                        }
                        self.or_load_lazy(&mut lazy, &terms, &mut rows, g, words)?;
                        for (h, row) in held.iter_mut().zip(&rows) {
                            *h = row.word(w);
                        }
                    };
                    word = keep;
                }
                if sieving {
                    self.or_keep(g, base, w, word)?;
                    continue;
                }
                debug_assert!(word == 0 || lazy.is_empty());
                while word != 0 {
                    let bit = word.trailing_zeros();
                    word &= word - 1;
                    // SAFETY: with `BITS`, the CPU has the features
                    // (`or_group`).
                    unsafe { candidate(self, g, base, w, bit) }?;
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
        self.or_lazy = lazy;
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
        r.advance(w);
        r.first + r.run + (r.at.word(w) & ((1u64 << (local % 64)) - 1)).count_ones()
    }

    /// Makes `w` the disjunction's word at hand, a word of the group at
    /// hand no earlier than the last: each present term's word of members
    /// there and the posting index of its first (see [`Walk::cand_w`]),
    /// once for the word's candidates.
    #[inline(always)]
    fn or_word(&mut self, w: usize) {
        self.cand_w = w;
        self.cand_words.clear();
        self.cand_base.clear();
        for &i in &self.present_list {
            let r = &mut self.row_state[i];
            r.advance(w);
            self.cand_words.push(r.at.word(w));
            self.cand_base.push(r.first + r.run);
        }
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
        self.or_candidate_in(g, base, w, bit)
    }

    #[inline(always)]
    fn or_candidate_in(&mut self, g: u32, base: u32, w: usize, bit: u32) -> Result<()> {
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
    #[inline(always)]
    fn or_stage(&mut self, g: u32, base: u32, w: usize, bit: u32) -> Result<Option<(f32, u32)>> {
        let local = (w * 64) as u32 + bit;
        let slot = base + local;
        self.answer.candidates += 1;
        // Its ctid only breaks a tie with the threshold's: most candidates
        // never need it.
        let geometry = self.geometry;
        let tid = move || geometry.tid_in(g as usize, local);
        let mut bound = 0.0_f32;
        if self.cand_w != w {
            self.or_word(w);
        }
        let below = (1u64 << bit) - 1;
        self.held_list.clear();
        for p in 0..self.present_list.len() {
            let i = self.present_list[p];
            let word = self.cand_words[p];
            if word >> bit & 1 == 1 {
                self.held_list.push(i);
                // A member's block is its posting index's: the footer is
                // decoded whole (`Sc::decode_all`), and its blocks hold
                // `block_size` postings each.
                let index = self.cand_base[p] + (word & below).count_ones();
                debug_assert_eq!(index, self.row_index(i, local));
                self.held_index[i] = index;
                let entry = self.row_state[i].entry;
                let s = &mut self.sc[i];
                let b = s.block_of(index, entry);
                debug_assert_eq!(Some(b), s.block_at(g, slot));
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
        // Its length class first, for a candidate holding several terms: its
        // shortest length bounds every term it holds before any bucket is
        // read. One holding fewer reads its class only after its buckets,
        // if they leave it in reach (see `CLASS_FIRST_TERMS`).
        let class_first = self.held_list.len() >= CLASS_FIRST_TERMS;
        if class_first && self.threshold().is_some() {
            let class = self.class_floor(g, local)?;
            if class > 0 {
                floor = class;
                reach = self.or_rebound(class);
                if self.cut(reach, tid) {
                    self.answer.class_cuts += 1;
                    return Ok(None);
                }
            }
        }
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
            None => {
                // Its class after its buckets, if not read before them: at
                // its class's shortest length where that is longer than its
                // buckets' floor.
                if !class_first && self.threshold().is_some() && self.segment.lengths.has_classes()
                {
                    let c = self.class_of(g, local)?;
                    if self.segment.lengths.floors()[c] > floor {
                        reach = self.class_reach(self.held_list.iter().copied(), c);
                        if self.cut(reach, tid) {
                            self.answer.class_cuts += 1;
                            return Ok(None);
                        }
                    }
                }
                (reach, u32::MAX)
            }
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
                // No higher than it was: one `minss` on x86-64, not
                // `f32::min`'s NaN ordering (1.1% of a `full_score`
                // disjunction's cycles there); never NaN, the same.
                lower(self.held_bound[h], s.scorer.bound_through(most, floor))
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
        if self.row_state[i].tf.state == TfRun::UNSET {
            self.row_state[i].tf = self.tf_run(i, block)?;
        }
        let run = self.row_state[i].tf;
        let s = &mut self.sc[i];
        let set = self.terms[s.term].as_ref().expect("scoring");
        let bucket = if run.state == TfRun::READY {
            run.bucket(index)?
        } else {
            s.bucket_in(block, set.postings.tf, index)?
        };
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

    /// Scoring term `i`'s [`TfRun`] in the group at hand, whose directory
    /// entry is `block`: [`TfRun::NONE`] for a sparse term, or one whose
    /// buckets there cross a page.
    #[inline(never)]
    fn tf_run(&self, i: usize, block: usize) -> Result<TfRun> {
        let none = TfRun {
            state: TfRun::NONE,
            ..TfRun::default()
        };
        let s = &self.sc[i];
        let Some(e) = s.group_entry(block) else {
            return Ok(none);
        };
        if e.width == 0 || e.count == 0 {
            return Ok(TfRun {
                state: TfRun::READY,
                first: e.first,
                tail: e.tf_bit as usize / 8,
                ..TfRun::default()
            });
        }
        let tf = self.terms[s.term].as_ref().expect("scoring").postings.tf;
        let from = e.tf_bit as usize / 8;
        let to = (e.bucket_bit(e.first + e.count - 1) + usize::from(e.width)).div_ceil(8);
        let page = tf.page_end(from);
        if page < to {
            return Ok(none);
        }
        // Eight bytes past the last where the page has them: whole loads.
        let bytes = tf.get(from, (to + 7).min(page))?;
        Ok(TfRun {
            state: TfRun::READY,
            width: e.width,
            first: e.first,
            bit: e.tf_bit as usize - from * 8,
            bytes: bytes.as_ptr(),
            len: bytes.len(),
            tail: from,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A member [`count_plan`]'s count drops (fewer than `m` of the heavy
    /// terms) scores below the bar whichever terms it holds, summed in any
    /// order; and the plain count (no light terms) is what it asks for
    /// where every bound is equal.
    #[test]
    fn count_plan_drops_only_members_below_the_bar() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let (mut order, mut heavy) = (Vec::new(), Vec::new());
        let mut planned = 0;
        for _ in 0..4000 {
            let n = 2 + (next() % 9) as usize;
            let bounds: Vec<f32> = (0..n)
                .map(|_| match next() % 4 {
                    0 => (next() % 1000) as f32 / 997.0,
                    1 => 0.25,
                    _ => (next() % 100_000) as f32 / 9973.0,
                })
                .collect();
            let total: f32 = bounds.iter().sum();
            let theta = total * ((next() % 1000) as f32 / 1000.0);
            let Some(m) = count_plan(&bounds, theta, &mut order, &mut heavy) else {
                continue;
            };
            planned += 1;
            assert!((2..=COUNT_MAX).contains(&m) && heavy.len() >= m);
            for set in 0u32..1 << n {
                let held = heavy.iter().filter(|&&t| set >> t & 1 == 1).count();
                if held >= m {
                    continue;
                }
                let (mut up, mut down) = (0.0_f32, 0.0_f32);
                for t in 0..n {
                    if set >> t & 1 == 1 {
                        up += bounds[t];
                    }
                    if set >> (n - 1 - t) & 1 == 1 {
                        down += bounds[n - 1 - t];
                    }
                }
                assert!(
                    up < theta && down < theta,
                    "{bounds:?} {theta} m {m} heavy {heavy:?} set {set:b}"
                );
            }
        }
        assert!(planned > 500, "{planned}");
        let m = count_plan(&[1.0; 6], 3.5, &mut order, &mut heavy);
        assert_eq!((m, heavy.len()), (Some(4), 6));
        assert_eq!(count_plan(&[1.0; 6], 0.5, &mut order, &mut heavy), None);
        assert_eq!(count_plan(&[1.0], 0.5, &mut order, &mut heavy), None);
    }

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
                let bound = sc.range_bound(g as u32, base, end);
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
                    let b = sc.block_at(g as u32, base).unwrap();
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
    fn a_bar_key_keeps_what_the_bar_does_not_cut() {
        let mut values = vec![
            0.0f32,
            -0.0,
            f32::MIN_POSITIVE,
            -f32::MIN_POSITIVE,
            1.0,
            1.0 + f32::EPSILON,
            -1.0,
            f32::MAX,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            -f32::NAN,
            f32::from_bits(1),
            12.375,
        ];
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        for _ in 0..200 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            values.push(f32::from_bits(state as u32));
            values.push((state % 4000) as f32 / 64.0);
        }
        let tids = [
            Tid {
                block: 3,
                offset: 2,
            },
            Tid {
                block: 3,
                offset: 3,
            },
            Tid {
                block: 4,
                offset: 1,
            },
        ];
        for &score in &values {
            for &at in &tids {
                let bar = Some((score, at));
                let key = BarKey::of(bar).unwrap();
                for &bound in &values {
                    for &from in &tids {
                        assert_eq!(
                            key.keeps(bound, || from),
                            !cut_by(bar, bound, || from),
                            "bound {bound:?} ({:#x}) bar {score:?} ({:#x}) {from:?} {at:?}",
                            bound.to_bits(),
                            score.to_bits()
                        );
                    }
                }
            }
        }
        assert!(BarKey::of(None).is_none());
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
