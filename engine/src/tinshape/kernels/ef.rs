// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Elias-Fano lists decoded a block at a time (see [`segment::tinshape::ef`]
//! for the format: value `i` is `(p_i - i) << l | low_i`, where `p_i` is the
//! position of the highs' `i`-th set bit and `low_i` its packed low part).
//!
//! The portable body is [`Ef::for_each`]: per member, the next set bit
//! found (count trailing zeros, clear lowest), its low part read from a bit
//! stream, the two combined: some 24 instructions and a branch whose outcome
//! follows the data. The vector bodies split that in two passes, neither
//! with a branch per member:
//!
//! 1. The high parts, a byte of the highs at a time: a table holds, per
//!    byte value, the zeros before each of its set bits; a byte's members
//!    are that row plus the byte's start less its rank, eight lanes stored
//!    at once (lanes past the byte's members are overwritten by the next
//!    byte's, or lie past the end). A word's byte ranks come from its byte
//!    counts by one multiplication.
//! 2. The low parts, eight values at a time: their `8 * l` bits are `l`
//!    whole bytes, so where each lane's value starts (byte and shift)
//!    depends on `l` alone; a byte shuffle gathers each lane's four bytes,
//!    a shift per lane and a mask cut it out, and it is ORed below the high
//!    part. Widths above 25 (a value may then span five bytes) and the last
//!    few values are read one at a time.
//!
//! With AVX-512 VBMI2 (see [`super::vbmi`]) the first pass takes a whole
//! word at once (`vpcompressb` of the positions 0..63 under it) and the
//! second sixteen values at once (`vpermb` over 64 bytes, read by a load
//! masked at the list's end, so only widths above 25 go one at a time).
//!
//! Both write into the caller's vector, past whose end the first pass
//! stores up to [`SLACK`] values (its capacity reserved for them).
//! [`ef_into_row`] decodes through a buffer on the stack, a few hundred
//! values at a time, and sets the values as bits as a disjunction's row
//! takes them: a word's bits gathered in a register and the word stored
//! after each, without a branch or a load.
//!
//! Lists of fewer than [`SHORT`] values take the portable body: the passes'
//! setup and their last word's unused bytes cost more than they save there.

#[cfg(target_arch = "x86_64")]
use super::vbmi;
use super::{Level, level};
use segment::Result;

/// Runs Elias-Fano body `$name` at `$level`, which the CPU must support
/// (see [`super::vbmi`] for the AVX-512 level).
macro_rules! ef_run {
    ($level:expr, $name:ident($($arg:expr),*)) => {
        match $level {
            #[cfg(target_arch = "x86_64")]
            // SAFETY: the level and its extra features are the CPU's.
            Level::Avx512 if vbmi() => unsafe { avx512::$name($($arg),*) },
            #[cfg(target_arch = "x86_64")]
            // SAFETY: AVX-512 implies AVX2 and the level is the CPU's.
            Level::Avx512 | Level::Avx2 => unsafe { avx2::$name($($arg),*) },
            #[cfg(target_arch = "aarch64")]
            Level::Neon => neon::$name($($arg),*),
            _ => scalar::$name($($arg),*),
        }
    };
}

use super::array::{array_decode, array_into_row};
use segment::tinshape::ef::Ef;
use segment::tinshape::postings::{
    GroupEntry, KIND_ARRAY, KIND_EF, array_bytes, for_each_local, group_slots,
};

/// Appends the list's values, in order.
#[inline]
pub fn ef_decode(ef: Ef<'_>, out: &mut Vec<u32>) {
    if !short_decode(&ef, out) {
        ef_run!(level(), ef_decode(ef, out));
    }
}

/// [`ef_decode`] at `level`; panics if the CPU lacks it.
pub fn ef_decode_at(level: Level, ef: Ef<'_>, out: &mut Vec<u32>) {
    assert!(
        level.supported(),
        "kernel level {} unsupported here",
        level.name()
    );
    if !short_decode(&ef, out) {
        ef_run!(level, ef_decode(ef, out));
    }
}

/// Sets bit `v` of `row` for each value `v` of the list, the words they
/// fall in being zero (as a cleared row's are): each such word is written,
/// not read. Panics if a value is past the row.
#[inline]
pub fn ef_into_row(ef: Ef<'_>, row: &mut [u64]) {
    if !short_row(&ef, row) {
        ef_run!(level(), ef_into_row(ef, row));
    }
}

/// [`ef_into_row`] at `level`; panics if the CPU lacks it.
pub fn ef_into_row_at(level: Level, ef: Ef<'_>, row: &mut [u64]) {
    assert!(
        level.supported(),
        "kernel level {} unsupported here",
        level.name()
    );
    if !short_row(&ef, row) {
        ef_run!(level, ef_into_row(ef, row));
    }
}

/// Appends a group container's members (local slots, in order), as
/// [`for_each_local`] visits them: an Elias-Fano container's by
/// [`ef_decode`], an array's by [`array_decode`]. `starts` is the group's
/// page starts ([`segment::tinshape::docs::Geometry::starts`]).
#[inline]
pub fn container_locals(
    entry: &GroupEntry,
    bytes: &[u8],
    starts: &[u32],
    out: &mut Vec<u32>,
) -> Result<()> {
    if entry.kind == KIND_ARRAY {
        array_decode(array_bytes(bytes, entry.count, group_slots(starts))?, out);
        return Ok(());
    }
    if entry.kind == KIND_EF {
        ef_decode(
            Ef::parse(bytes, entry.count as usize, group_slots(starts))?,
            out,
        );
        return Ok(());
    }
    out.reserve(entry.count as usize);
    for_each_local(entry, bytes, starts, |l| out.push(l))
}

/// Sets a group container's members in `row`, which is cleared where they
/// fall (see [`ef_into_row`]).
#[inline]
pub fn container_row(
    entry: &GroupEntry,
    bytes: &[u8],
    starts: &[u32],
    row: &mut [u64],
) -> Result<()> {
    if entry.kind == KIND_ARRAY {
        array_into_row(array_bytes(bytes, entry.count, group_slots(starts))?, row);
        return Ok(());
    }
    if entry.kind == KIND_EF {
        ef_into_row(
            Ef::parse(bytes, entry.count as usize, group_slots(starts))?,
            row,
        );
        return Ok(());
    }
    let mut put = RowPut::new();
    for_each_local(entry, bytes, starts, |l| put.put(row, l))
}

/// Lists shorter than this are decoded by [`short`] at every level.
pub const SHORT: usize = 16;

/// Values the first pass may store past the last member it finds: a word's
/// last byte stores eight lanes from where the word's members end, and the
/// word is begun while fewer than `n` members are found.
const SLACK: usize = 64 + 8;

/// Highs words a chunk of [`ef_into_row`] decodes: at most 512 values.
const CHUNK_WORDS: usize = 8;

/// Per byte value, the zeros before each of its set bits (lanes past its
/// last set bit are 0), as 16-bit lanes (widened to 32 by one add).
static ZEROS: [[u16; 8]; 256] = zeros();

const fn zeros() -> [[u16; 8]; 256] {
    let mut table = [[0u16; 8]; 256];
    let mut b = 0;
    while b < 256 {
        let (mut bit, mut j) = (0, 0);
        while bit < 8 {
            if b >> bit & 1 == 1 {
                table[b][j] = (bit - j) as u16;
                j += 1;
            }
            bit += 1;
        }
        b += 1;
    }
    table
}

/// The widest low part the vector bodies unpack: a value at a bit offset
/// below 8 then fits in four bytes.
const WIDEST: u32 = 25;

/// Where eight `l`-bit values starting on a byte lie: lanes 0-3 in the 16
/// bytes from that byte, lanes 4-7 in the 16 from byte `half` (`4l / 8`).
/// Per half, the four bytes each lane takes (a shuffle) and the shift that
/// brings its value down.
#[derive(Clone, Copy)]
struct Lows {
    take: [[u8; 16]; 2],
    shift: [[u32; 4]; 2],
    half: usize,
}

static LOWS: [Lows; WIDEST as usize + 1] = lows();

const fn lows() -> [Lows; WIDEST as usize + 1] {
    let mut table = [Lows {
        take: [[0; 16]; 2],
        shift: [[0; 4]; 2],
        half: 0,
    }; WIDEST as usize + 1];
    let mut l = 1;
    while l <= WIDEST as usize {
        let half = 4 * l / 8;
        table[l].half = half;
        let mut h = 0;
        while h < 2 {
            let mut j = 0;
            while j < 4 {
                let bit = (4 * h + j) * l - 8 * h * half;
                let byte = bit / 8;
                let mut m = 0;
                while m < 4 {
                    table[l].take[h][4 * j + m] = (byte + m) as u8;
                    m += 1;
                }
                table[l].shift[h][j] = (bit % 8) as u32;
                j += 1;
            }
            h += 1;
        }
        l += 1;
    }
    table
}

/// Where sixteen `l`-bit values starting on a byte lie, for one `vpermb` of
/// the 64 bytes from there: the four bytes from the one each lane's value
/// starts in, and the shift that brings it down (16 values of at most
/// [`WIDEST`] bits end within 54 bytes).
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
struct Lows16 {
    take: [u8; 64],
    shift: [u32; 16],
}

#[cfg(target_arch = "x86_64")]
static LOWS16: [Lows16; WIDEST as usize + 1] = lows16();

#[cfg(target_arch = "x86_64")]
const fn lows16() -> [Lows16; WIDEST as usize + 1] {
    let mut table = [Lows16 {
        take: [0; 64],
        shift: [0; 16],
    }; WIDEST as usize + 1];
    let mut l = 1;
    while l <= WIDEST as usize {
        let mut j = 0;
        while j < 16 {
            let bit = j * l;
            let mut m = 0;
            while m < 4 {
                table[l].take[4 * j + m] = (bit / 8 + m) as u8;
                m += 1;
            }
            table[l].shift[j] = (bit % 8) as u32;
            j += 1;
        }
        l += 1;
    }
    table
}

/// The set bits of each byte of `word`, as its bytes.
#[inline(always)]
fn byte_counts(word: u64) -> u64 {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: NEON is baseline on aarch64.
    unsafe {
        use core::arch::aarch64::*;
        vget_lane_u64::<0>(vreinterpret_u64_u8(vcnt_u8(vcreate_u8(word))))
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let c = word - ((word >> 1) & 0x5555_5555_5555_5555);
        let c = (c & 0x3333_3333_3333_3333) + ((c >> 2) & 0x3333_3333_3333_3333);
        (c + (c >> 4)) & 0x0f0f_0f0f_0f0f_0f0f
    }
}

/// `ef`'s lows, and the bytes after them its highs: a vector load may read
/// past the lows' end into them (the lanes it fills there are not used).
#[inline(always)]
fn lows_padded<'a>(ef: &Ef<'a>) -> &'a [u8] {
    let (lows, highs) = (ef.lows(), ef.highs());
    assert!(
        core::ptr::eq(lows.as_ptr_range().end, highs.as_ptr()),
        "Elias-Fano highs follow the lows"
    );
    // SAFETY: both are parts of one borrowed byte string, the highs
    // directly after the lows (checked above), so the span of both is too.
    unsafe { core::slice::from_raw_parts(lows.as_ptr(), lows.len() + highs.len()) }
}

/// Word `w` of the highs, the last `highs_len` bytes of `padded` (see
/// [`lows_padded`]): their last part-word by a load ending at their end,
/// the bytes before it shifted out, where `padded` has eight bytes.
#[inline(always)]
fn highs_word(padded: &[u8], highs_len: usize, w: usize) -> u64 {
    let at = padded.len() - highs_len + 8 * w;
    if let Some(eight) = padded.get(at..at + 8) {
        return u64::from_le_bytes(eight.try_into().expect("eight bytes"));
    }
    match padded.len().checked_sub(8) {
        Some(last) => {
            u64::from_le_bytes(padded[last..].try_into().expect("eight bytes")) >> (8 * (at - last))
        }
        None => part_word(&padded[at..]),
    }
}

/// A part-word of fewer than eight bytes, assembled a byte at a time.
#[cold]
#[inline(never)]
fn part_word(bytes: &[u8]) -> u64 {
    segment::tinshape::bits::word(bytes, 0)
}

/// Values `i` with `i % 8 == 0` below the result have their eight low parts
/// (from byte `i / 8 * l`) and the vector loads' reach (`half + 16` bytes
/// from there) within `len` bytes.
#[inline(always)]
fn vector_reach(len: usize, l: u32) -> usize {
    let shape = &LOWS[l as usize];
    len.checked_sub(shape.half + 16)
        .map_or(0, |room| (room / l as usize + 1) * 8)
}

/// `*d = *d << l | low_i`.
///
/// # Safety
/// `d` is valid for one value.
#[inline(always)]
unsafe fn combine_one(lows: &[u8], l: u32, i: usize, d: *mut u32) {
    use segment::tinshape::bits;
    // SAFETY: the caller's.
    unsafe { *d = *d << l | bits::get(lows, i, l).unwrap_or(0) };
}

/// Sets ascending values in a row whose words they fall in are zero: the
/// word at hand's bits kept in a register, the word stored after each value
/// (a value in a new word starts from none), so that no store waits on a
/// load of the word the last one stored.
struct RowPut {
    w: usize,
    bits: u64,
}

impl RowPut {
    #[inline(always)]
    fn new() -> Self {
        Self {
            w: usize::MAX,
            bits: 0,
        }
    }

    #[inline(always)]
    fn put(&mut self, row: &mut [u64], v: u32) {
        let w = v as usize / 64;
        let kept = if w == self.w { self.bits } else { 0 };
        self.bits = kept | 1 << (v % 64);
        self.w = w;
        row[w] = self.bits;
    }

    /// [`Self::put`] of a value within the row.
    ///
    /// # Safety
    /// `v / 64 < row.len()`.
    #[inline(always)]
    unsafe fn put_unchecked(&mut self, row: &mut [u64], v: u32) {
        let w = v as usize / 64;
        let kept = if w == self.w { self.bits } else { 0 };
        self.bits = kept | 1 << (v % 64);
        self.w = w;
        // SAFETY: the caller's.
        unsafe { *row.get_unchecked_mut(w) = self.bits };
    }
}

/// Visits the values of a list shorter than [`SHORT`] whose highs fit a
/// word, as a valid one's do (fewer than `3n` bits); false, visiting
/// nothing, for any other.
///
/// Short lists are a few bytes, so every read is one eight-byte load
/// clamped to end at the list's end (its bytes past the end shifted out):
/// the highs word and each low part (the highs follow the lows). A list
/// shorter than eight bytes is read into one word first.
#[inline(always)]
fn short(ef: &Ef<'_>, mut visit: impl FnMut(u32)) -> bool {
    let (n, l, highs) = (ef.len(), ef.low_width(), ef.highs());
    if n >= SHORT || highs.len() > 8 {
        return false;
    }
    let bytes = lows_padded(ef);
    let whole = if bytes.len() < 8 {
        let mut whole = 0u64;
        for (k, b) in bytes.iter().enumerate() {
            whole |= u64::from(*b) << (8 * k);
        }
        whole
    } else {
        0
    };
    // The eight bytes from `byte`, those past the end zero.
    let load = |byte: usize| match bytes.len().checked_sub(8) {
        Some(last) => {
            let at = byte.min(last);
            u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
                >> (8 * (byte - at))
        }
        None => whole >> (8 * byte),
    };
    let mut word = load(ef.lows().len());
    let mask = if l == 0 { 0 } else { (1u64 << l) - 1 };
    let mut bit = 0usize;
    for i in 0..n as u32 {
        if word == 0 {
            break;
        }
        let low = (load(bit / 8) >> (bit % 8) & mask) as u32;
        visit((word.trailing_zeros() - i) << l | low);
        word &= word - 1;
        bit += l as usize;
    }
    true
}

/// [`ef_decode`] of a short list (see [`short`]); false for another.
#[inline(always)]
fn short_decode(ef: &Ef<'_>, out: &mut Vec<u32>) -> bool {
    if ef.len() >= SHORT {
        return false;
    }
    out.reserve(ef.len());
    let (start, mut k) = (out.len(), 0);
    let dst = out.as_mut_ptr();
    let done = short(ef, |v| {
        // SAFETY: `short` visits at most `n` values, the room reserved.
        unsafe { dst.add(start + k).write(v) };
        k += 1;
    });
    // SAFETY: the `k` values from `start` are written.
    unsafe { out.set_len(start + k) };
    done
}

/// [`ef_into_row`] of a short list (see [`short`]); false for another.
#[inline(always)]
fn short_row(ef: &Ef<'_>, row: &mut [u64]) -> bool {
    if ef.len() >= SHORT {
        return false;
    }
    let mut put = RowPut::new();
    short(ef, |v| put.put(row, v))
}

/// The portable bodies: the format's own decoder.
mod scalar {
    use segment::tinshape::ef::Ef;

    #[inline(never)]
    pub fn ef_decode(ef: Ef<'_>, out: &mut Vec<u32>) {
        out.reserve(ef.len());
        ef.for_each(|v| out.push(v));
    }

    #[inline(never)]
    pub fn ef_into_row(ef: Ef<'_>, row: &mut [u64]) {
        let mut put = super::RowPut::new();
        ef.for_each(|v| put.put(row, v));
    }
}

/// The two passes and the drivers around them, for a module with
/// `highs_word(d, s, word)`, which stores a highs word's members' high
/// parts from `d` (`s`: the word's first bit less the rank of its first
/// member) and returns how many there are, and `Unpack`, `Unpack::LANES`
/// low parts at a time; `$tf` is its `#[target_feature]` attribute, if any.
macro_rules! passes {
    ($(#[$tf:meta])*) => {
        /// The high parts of `ef`'s members whose set bits are in words
        /// `w0..w1` of its highs, from member `idx`, stored from `dst`
        /// (member `origin`'s place); stops after the word in which its
        /// last member is found. The members found, counted from 0.
        ///
        /// # Safety
        /// `dst` has room for members `origin..min(n, found) + SLACK`, and
        /// `idx >= origin`.
        $(#[$tf])*
        #[inline]
        unsafe fn highs(
            ef: &Ef<'_>,
            w0: usize,
            w1: usize,
            mut idx: usize,
            dst: *mut u32,
            origin: usize,
        ) -> usize {
            let (n, padded, highs_len) = (ef.len(), super::lows_padded(ef), ef.highs().len());
            for w in w0..w1 {
                if idx >= n {
                    break;
                }
                // The last word may be part of one: its missing bytes are
                // zero, so store lanes past its members, within the slack.
                let word = super::highs_word(padded, highs_len, w);
                let s = ((w * 64) as u32).wrapping_sub(idx as u32);
                // SAFETY: the word's members start at `idx < n`, and it
                // stores at most 64 + 8 lanes from there.
                idx += unsafe { highs_word(dst.add(idx - origin), s, word) };
            }
            idx
        }

        /// Members `from..to` (`from` a multiple of 8): `dst[i - from] =
        /// dst[i - from] << l | low_i`, `Unpack::LANES` at a time where the
        /// lows' bytes allow (the last lanes may pass `to`).
        ///
        /// # Safety
        /// `dst` is valid for `to - from` values rounded up to a multiple
        /// of `Unpack::LANES`, all initialized.
        $(#[$tf])*
        #[inline]
        unsafe fn combine(lows: &[u8], l: u32, from: usize, to: usize, dst: *mut u32) {
            debug_assert!(from % 8 == 0);
            if l == 0 {
                return;
            }
            let mut i = from;
            // SAFETY (all below): `i - from` is below `to - from` rounded
            // up, the caller's room; vector loads stay within `lows` below
            // `Unpack::reach`.
            if l <= super::WIDEST {
                let stop = to.min(Unpack::reach(lows.len(), l));
                let unpack = Unpack::new(l);
                let p = lows.as_ptr();
                while i < stop {
                    let at = i / 8 * l as usize;
                    unsafe { unpack.lanes(p.add(at), lows.len() - at, dst.add(i - from)) };
                    i += Unpack::LANES;
                }
            }
            while i < to {
                unsafe { super::combine_one(lows, l, i, dst.add(i - from)) };
                i += 1;
            }
        }

        $(#[$tf])*
        #[inline(never)]
        pub fn ef_decode(ef: Ef<'_>, out: &mut Vec<u32>) {
            let n = ef.len();
            let highs_bytes = ef.highs();
            let lows = super::lows_padded(&ef);
            let start = out.len();
            out.reserve(n + super::SLACK);
            // SAFETY: the room is reserved for `n + SLACK` values from
            // `start`; `highs` writes within it, `combine` reads and writes
            // the `found` it wrote, and those are then the vector's.
            unsafe {
                let dst = out.as_mut_ptr().add(start);
                let words = highs_bytes.len().div_ceil(8);
                let found = highs(&ef, 0, words, 0, dst, 0).min(n);
                // The last lanes `combine` may take: initialized.
                dst.add(found).write_bytes(0, Unpack::LANES);
                combine(lows, ef.low_width(), 0, found, dst);
                out.set_len(start + found);
            }
        }

        $(#[$tf])*
        #[inline(never)]
        pub fn ef_into_row(ef: Ef<'_>, row: &mut [u64]) {
            let n = ef.len();
            let highs_bytes = ef.highs();
            let lows = super::lows_padded(&ef);
            let l = ef.low_width();
            // A chunk's members from the multiple of 8 at or before its
            // first (those before it zeros, decoded and not set).
            let mut buf = [core::mem::MaybeUninit::<u32>::uninit();
                8 + super::CHUNK_WORDS * 64 + super::SLACK];
            let dst = buf.as_mut_ptr().cast::<u32>();
            let total = highs_bytes.len().div_ceil(8);
            let mut put = super::RowPut::new();
            // Whether a chunk's values fit the row, by its last: the value
            // with the largest high part, and low bits at most `mask`.
            let mask = if l == 0 { 0 } else { (1u64 << l) - 1 };
            let (mut idx, mut w) = (0, 0);
            while w < total && idx < n {
                let w1 = (w + super::CHUNK_WORDS).min(total);
                // SAFETY: a chunk finds at most 64 members a word, so its
                // members from `origin` and their slack fit in `buf`, the
                // ones `combine` reads initialized; the reads below stay
                // within those found.
                unsafe {
                    let from = idx;
                    let origin = from & !7;
                    dst.write_bytes(0, from - origin);
                    idx = highs(&ef, w, w1, idx, dst, origin);
                    let to = idx.min(n);
                    w = w1;
                    if to == from {
                        // A chunk of empty buckets.
                        continue;
                    }
                    dst.add(to - origin).write_bytes(0, Unpack::LANES);
                    combine(lows, l, origin, to, dst);
                    let last = u64::from(*dst.add(to - origin - 1)) | mask;
                    if ((last / 64) as usize) < row.len() {
                        let values = core::slice::from_raw_parts(dst.add(from - origin), to - from);
                        let mut fours = values.chunks_exact(4);
                        for four in &mut fours {
                            put.put_unchecked(row, four[0]);
                            put.put_unchecked(row, four[1]);
                            put.put_unchecked(row, four[2]);
                            put.put_unchecked(row, four[3]);
                        }
                        for &v in fours.remainder() {
                            put.put_unchecked(row, v);
                        }
                    } else {
                        for k in from - origin..to - origin {
                            put.put(row, *dst.add(k));
                        }
                    }
                }
            }
        }
    };
}

/// NEON bodies (baseline on aarch64): a byte's eight high parts as two
/// widening adds of its table row to its base (one lane of the word's bases,
/// computed together); eight low parts by `tbl`, `ushl` and `and`.
#[cfg(target_arch = "aarch64")]
mod neon {
    use core::arch::aarch64::*;
    use segment::tinshape::ef::Ef;

    /// # Safety
    /// `d` is valid for 64 + 8 values.
    #[inline(always)]
    unsafe fn highs_word(d: *mut u32, s: u32, word: u64) -> usize {
        // Byte k's rank in the word: the set bits of bytes 0..k.
        let inclusive = super::byte_counts(word).wrapping_mul(0x0101_0101_0101_0101);
        let before = inclusive << 8;
        // SAFETY: table rows are 16 bytes; each store is at `d + before_k`,
        // below `d + 64`, and eight lanes long.
        unsafe {
            // Byte k's base: s + 8k - before_k (8k >= before_k).
            let offsets = vsubl_u8(vcreate_u8(0x3830_2820_1810_0800), vcreate_u8(before));
            let sv = vdupq_n_u32(s);
            let low = vaddw_u16(sv, vget_low_u16(offsets));
            let high = vaddw_high_u16(sv, offsets);
            macro_rules! byte {
                ($k:literal, $bases:ident, $lane:literal) => {{
                    let z = vld1q_u16(super::ZEROS[(word >> (8 * $k)) as u8 as usize].as_ptr());
                    let at = d.add(((before >> (8 * $k)) & 0xff) as usize);
                    let base = vdupq_laneq_u32::<$lane>($bases);
                    vst1q_u32(at, vaddw_u16(base, vget_low_u16(z)));
                    vst1q_u32(at.add(4), vaddw_high_u16(base, z));
                }};
            }
            byte!(0, low, 0);
            byte!(1, low, 1);
            byte!(2, low, 2);
            byte!(3, low, 3);
            byte!(4, high, 0);
            byte!(5, high, 1);
            byte!(6, high, 2);
            byte!(7, high, 3);
        }
        (inclusive >> 56) as usize
    }

    /// Eight low parts at a time at one width.
    struct Unpack {
        take: [uint8x16_t; 2],
        down: [int32x4_t; 2],
        mask: uint32x4_t,
        up: int32x4_t,
        half: usize,
    }

    impl Unpack {
        const LANES: usize = 8;

        /// See [`super::vector_reach`].
        #[inline(always)]
        fn reach(len: usize, l: u32) -> usize {
            super::vector_reach(len, l)
        }

        #[inline(always)]
        fn new(l: u32) -> Self {
            let shape = &super::LOWS[l as usize];
            // SAFETY: NEON is baseline; each load is of a table row.
            unsafe {
                let down =
                    |h: usize| vnegq_s32(vreinterpretq_s32_u32(vld1q_u32(shape.shift[h].as_ptr())));
                Self {
                    take: [
                        vld1q_u8(shape.take[0].as_ptr()),
                        vld1q_u8(shape.take[1].as_ptr()),
                    ],
                    down: [down(0), down(1)],
                    mask: vdupq_n_u32((1u32 << l) - 1),
                    up: vdupq_n_s32(l as i32),
                    half: shape.half,
                }
            }
        }

        /// `d[j] = d[j] << l | low_j` for the eight values from byte `p`
        /// (of which `_avail` bytes are the list's).
        ///
        /// # Safety
        /// `p..p + half + 16` is readable; `d` is valid for eight values.
        #[inline(always)]
        unsafe fn lanes(&self, p: *const u8, _avail: usize, d: *mut u32) {
            // SAFETY: the caller's.
            unsafe {
                let a = vqtbl1q_u8(vld1q_u8(p), self.take[0]);
                let b = vqtbl1q_u8(vld1q_u8(p.add(self.half)), self.take[1]);
                let a = vandq_u32(vshlq_u32(vreinterpretq_u32_u8(a), self.down[0]), self.mask);
                let b = vandq_u32(vshlq_u32(vreinterpretq_u32_u8(b), self.down[1]), self.mask);
                vst1q_u32(d, vorrq_u32(vshlq_u32(vld1q_u32(d), self.up), a));
                vst1q_u32(
                    d.add(4),
                    vorrq_u32(vshlq_u32(vld1q_u32(d.add(4)), self.up), b),
                );
            }
        }
    }

    passes!();
}

/// AVX2 bodies: a byte's eight high parts by one widening load of its
/// table row and an add; eight low parts by `vpshufb` over the two halves'
/// 16 bytes, `vpsrlvd` and a mask.
#[cfg(target_arch = "x86_64")]
mod avx2 {
    use core::arch::x86_64::*;
    use segment::tinshape::ef::Ef;

    /// # Safety
    /// `d` is valid for 64 + 8 values.
    #[target_feature(enable = "avx2,popcnt")]
    #[inline]
    unsafe fn highs_word(d: *mut u32, s: u32, word: u64) -> usize {
        // Byte k's rank in the word: the set bits of bytes 0..k.
        let inclusive = super::byte_counts(word).wrapping_mul(0x0101_0101_0101_0101);
        let before = inclusive << 8;
        // SAFETY: table rows are 16 bytes; each store is at `d + before_k`,
        // below `d + 64`, and eight lanes long.
        unsafe {
            // Byte k's base, s + 8k - before_k, for the eight together; each
            // byte's taken by a lane permute.
            let bases = _mm256_sub_epi32(
                _mm256_add_epi32(
                    _mm256_set1_epi32(s as i32),
                    _mm256_setr_epi32(0, 8, 16, 24, 32, 40, 48, 56),
                ),
                _mm256_cvtepu8_epi32(_mm_cvtsi64_si128(before as i64)),
            );
            for k in 0..8 {
                let e = ((before >> (8 * k)) & 0xff) as usize;
                let row = super::ZEROS[(word >> (8 * k)) as u8 as usize].as_ptr();
                let base = _mm256_permutevar8x32_epi32(bases, _mm256_set1_epi32(k));
                let v = _mm256_add_epi32(_mm256_cvtepu16_epi32(_mm_loadu_si128(row.cast())), base);
                _mm256_storeu_si256(d.add(e).cast(), v);
            }
        }
        (inclusive >> 56) as usize
    }

    /// Eight low parts at a time at one width.
    struct Unpack {
        take: __m256i,
        shift: __m256i,
        mask: __m256i,
        up: __m128i,
        half: usize,
    }

    impl Unpack {
        const LANES: usize = 8;

        /// See [`super::vector_reach`].
        #[inline(always)]
        fn reach(len: usize, l: u32) -> usize {
            super::vector_reach(len, l)
        }

        #[target_feature(enable = "avx2,popcnt")]
        #[inline]
        fn new(l: u32) -> Self {
            let shape = &super::LOWS[l as usize];
            // SAFETY: each load is of a 32-byte table row.
            unsafe {
                Self {
                    take: _mm256_loadu_si256(shape.take.as_ptr().cast()),
                    shift: _mm256_loadu_si256(shape.shift.as_ptr().cast()),
                    mask: _mm256_set1_epi32(((1u32 << l) - 1) as i32),
                    up: _mm_cvtsi32_si128(l as i32),
                    half: shape.half,
                }
            }
        }

        /// `d[j] = d[j] << l | low_j` for the eight values from byte `p`
        /// (of which `_avail` bytes are the list's).
        ///
        /// # Safety
        /// `p..p + half + 16` is readable; `d` is valid for eight values.
        #[target_feature(enable = "avx2,popcnt")]
        #[inline]
        unsafe fn lanes(&self, p: *const u8, _avail: usize, d: *mut u32) {
            // SAFETY: the caller's.
            unsafe {
                let bytes = _mm256_inserti128_si256::<1>(
                    _mm256_castsi128_si256(_mm_loadu_si128(p.cast())),
                    _mm_loadu_si128(p.add(self.half).cast()),
                );
                let low = _mm256_and_si256(
                    _mm256_srlv_epi32(_mm256_shuffle_epi8(bytes, self.take), self.shift),
                    self.mask,
                );
                let high = _mm256_sll_epi32(_mm256_loadu_si256(d.cast()), self.up);
                _mm256_storeu_si256(d.cast(), _mm256_or_si256(high, low));
            }
        }
    }

    passes!(#[target_feature(enable = "avx2,popcnt")]);
}

/// AVX-512 bodies (F, BW, VBMI and VBMI2), after Langdale's and Lemire's
/// bitset decoding: a highs word's members' high parts by one `vpcompressb`
/// of the bytes 0..63 under the word as a mask (its set bits' positions,
/// packed), less each lane's index (the member's rank in the word), widened
/// sixteen at a time onto the word's base; sixteen low parts by one `vpermb`
/// of 64 bytes, `vpsrlvd` and a mask, the bytes read by a masked load that
/// stops at the list's end, so no value is read one at a time below
/// [`WIDEST`].
#[cfg(target_arch = "x86_64")]
mod avx512 {
    use core::arch::x86_64::*;
    use segment::tinshape::ef::Ef;

    /// Bytes 0 to 63.
    static IOTA: [u8; 64] = {
        let mut t = [0u8; 64];
        let mut i = 0;
        while i < 64 {
            t[i] = i as u8;
            i += 1;
        }
        t
    };

    /// # Safety
    /// `d` is valid for 64 values.
    #[target_feature(enable = "avx512f,avx512bw,avx512vbmi,avx512vbmi2,bmi2,popcnt")]
    #[inline]
    unsafe fn highs_word(d: *mut u32, s: u32, word: u64) -> usize {
        // SAFETY: four stores of sixteen lanes from `d`.
        unsafe {
            let iota = _mm512_loadu_si512(IOTA.as_ptr().cast());
            // Member j's position less j (lanes past the word's members
            // hold garbage, overwritten by the next word's or past the end).
            let ranked = _mm512_sub_epi8(_mm512_maskz_compress_epi8(word, iota), iota);
            let base = _mm512_set1_epi32(s as i32);
            let put = |at: usize, sixteen: __m128i| {
                let v = _mm512_add_epi32(_mm512_cvtepu8_epi32(sixteen), base);
                _mm512_storeu_si512(d.add(at).cast(), v);
            };
            put(0, _mm512_castsi512_si128(ranked));
            put(16, _mm512_extracti32x4_epi32::<1>(ranked));
            put(32, _mm512_extracti32x4_epi32::<2>(ranked));
            put(48, _mm512_extracti32x4_epi32::<3>(ranked));
        }
        word.count_ones() as usize
    }

    /// Sixteen low parts at a time at one width.
    struct Unpack {
        take: __m512i,
        shift: __m512i,
        mask: __m512i,
        up: __m128i,
    }

    impl Unpack {
        const LANES: usize = 16;

        /// Every value: the loads are masked at the list's end.
        #[inline(always)]
        fn reach(_len: usize, _l: u32) -> usize {
            usize::MAX
        }

        #[target_feature(enable = "avx512f,avx512bw,avx512vbmi,avx512vbmi2,bmi2,popcnt")]
        #[inline]
        fn new(l: u32) -> Self {
            let shape = &super::LOWS16[l as usize];
            // SAFETY: each load is of a 64-byte table row.
            unsafe {
                Self {
                    take: _mm512_loadu_si512(shape.take.as_ptr().cast()),
                    shift: _mm512_loadu_si512(shape.shift.as_ptr().cast()),
                    mask: _mm512_set1_epi32(((1u32 << l) - 1) as i32),
                    up: _mm_cvtsi32_si128(l as i32),
                }
            }
        }

        /// `d[j] = d[j] << l | low_j` for the sixteen values from byte `p`,
        /// of which `avail` bytes are readable (bytes past them read as
        /// zeros, the lanes past the list's values then being garbage).
        ///
        /// # Safety
        /// `p..p + avail` is readable; `d` is valid for sixteen values.
        #[target_feature(enable = "avx512f,avx512bw,avx512vbmi,avx512vbmi2,bmi2,popcnt")]
        #[inline]
        unsafe fn lanes(&self, p: *const u8, avail: usize, d: *mut u32) {
            // SAFETY: the masked load reads no byte past `avail` (nor
            // faults there); the rest is the caller's.
            unsafe {
                let readable = _bzhi_u64(u64::MAX, avail.min(64) as u32);
                let bytes = _mm512_maskz_loadu_epi8(readable, p.cast());
                let low = _mm512_and_si512(
                    _mm512_srlv_epi32(_mm512_permutexvar_epi8(self.take, bytes), self.shift),
                    self.mask,
                );
                let high = _mm512_sll_epi32(_mm512_loadu_si512(d.cast()), self.up);
                _mm512_storeu_si512(d.cast(), _mm512_or_si512(high, low));
            }
        }
    }

    passes!(#[target_feature(enable = "avx512f,avx512bw,avx512vbmi,avx512vbmi2,bmi2,popcnt")]);
}

#[cfg(test)]
mod tests {
    use super::super::Level;
    #[cfg(target_arch = "x86_64")]
    use super::super::vbmi;
    #[cfg(target_arch = "aarch64")]
    use super::neon;
    use super::*;
    #[cfg(target_arch = "x86_64")]
    use super::{avx2, avx512};
    use proptest::prelude::*;
    use segment::tinshape::ef;

    /// The levels to compare with the portable one (see the kernels' own
    /// tests for `STANNUM_EXPECT_KERNELS`). `STANNUM_EXPECT_VBMI=1` makes
    /// the run fail where the AVX-512 level would not run its own bodies.
    fn levels() -> Vec<Level> {
        let have = Level::supported_levels();
        if std::env::var("STANNUM_EXPECT_VBMI").is_ok_and(|v| v == "1") {
            #[cfg(target_arch = "x86_64")]
            assert!(
                have.contains(&Level::Avx512) && vbmi(),
                "AVX-512 VBMI bodies not supported here"
            );
            #[cfg(not(target_arch = "x86_64"))]
            panic!("AVX-512 VBMI bodies not supported here");
        }
        if let Ok(want) = std::env::var("STANNUM_EXPECT_KERNELS") {
            for name in want.split(',').filter(|s| !s.trim().is_empty()) {
                let level = Level::parse(name).unwrap_or_else(|| panic!("unknown level {name}"));
                assert!(have.contains(&level), "{} not supported here", level.name());
            }
        }
        have
    }

    /// Encodes `values` below `universe` at offset `at` of a buffer with
    /// `after` bytes past the list (a container's bytes may be followed by
    /// others, or not).
    fn encoded(values: &[u32], universe: u32, at: usize, after: usize) -> Vec<u8> {
        let mut out = vec![0xa5; at];
        ef::encode(values, universe, &mut out);
        out.extend(std::iter::repeat_n(0x5a, after));
        out
    }

    fn check(values: &[u32], universe: u32, at: usize, after: usize) {
        let bytes = encoded(values, universe, at, after);
        let list = Ef::parse(&bytes[at..], values.len(), universe).unwrap();
        let words = (universe as usize).div_ceil(64);
        for level in levels() {
            let mut out = vec![7u32];
            ef_decode_at(level, list, &mut out);
            assert_eq!(&out[1..], values, "{level:?} universe {universe}");
            assert_eq!(out[0], 7);
            let mut want = vec![0u64; words + 1];
            for v in values {
                want[*v as usize / 64] |= 1 << (v % 64);
            }
            let mut have = vec![0u64; words + 1];
            ef_into_row_at(level, list, &mut have);
            assert_eq!(have, want, "{level:?} universe {universe}");
        }
    }

    #[test]
    fn tables() {
        for (b, row) in ZEROS.iter().enumerate() {
            let mut j = 0;
            for bit in 0..8 {
                if b >> bit & 1 == 1 {
                    assert_eq!(row[j] as usize, bit - j);
                    j += 1;
                }
            }
        }
        for (l, shape) in LOWS.iter().enumerate().skip(1) {
            for lane in 0..8 {
                let (h, j) = (lane / 4, lane % 4);
                let bit = lane * l - 8 * h * shape.half;
                assert_eq!(shape.shift[h][j] as usize, bit % 8);
                assert_eq!(shape.take[h][4 * j] as usize, bit / 8);
                assert!(shape.take[h][4 * j + 3] < 16);
                assert!(bit % 8 + l <= 32);
            }
        }
        #[cfg(target_arch = "x86_64")]
        for (l, shape) in LOWS16.iter().enumerate().skip(1) {
            for lane in 0..16 {
                let bit = lane * l;
                assert_eq!(shape.shift[lane] as usize, bit % 8);
                assert_eq!(shape.take[4 * lane] as usize, bit / 8);
                assert!(shape.take[4 * lane + 3] < 64);
            }
        }
    }

    #[test]
    fn every_width_and_length() {
        for universe in [
            1u32,
            2,
            7,
            64,
            1000,
            15_000,
            74_496,
            1 << 20,
            1 << 27,
            u32::MAX,
        ] {
            for n in [
                0usize, 1, 2, 7, 8, 9, 15, 16, 17, 31, 64, 65, 200, 1000, 3000,
            ] {
                if n as u64 > u64::from(universe) {
                    continue;
                }
                // Evenly spread, and bunched at both ends.
                let step = (u64::from(universe) / n.max(1) as u64).max(1);
                let even: Vec<u32> = (0..n as u64).map(|i| (i * step) as u32).collect();
                check(&even, universe, 0, 0);
                let ends: Vec<u32> = (0..n as u32)
                    .map(|i| {
                        if i < n as u32 / 2 {
                            i
                        } else {
                            universe - (n as u32 - i)
                        }
                    })
                    .collect();
                check(&ends, universe, 3, 0);
            }
        }
    }

    /// Lists from dense to sparse (low widths 0 to 31): gaps of up to
    /// `2^shift`, and the universe just past the last value or further.
    #[test]
    fn empty_chunks() {
        // 256 values at each end of a universe of 512 * 31 (l = 4): the
        // highs between them are some 960 zeros, more than a chunk of
        // `ef_into_row` (512 bits) holds, after a chunk ending on a
        // multiple of 8 members.
        let universe = 512 * 31;
        let values: Vec<u32> = (0..256).chain(universe - 256..universe).collect();
        check(&values, universe, 0, 0);
        check(&values, universe, 5, 3);
    }

    fn list() -> impl Strategy<Value = (Vec<u32>, u32)> {
        (0u32..31, prop_oneof![0usize..40, 0usize..1500]).prop_flat_map(|(shift, n)| {
            (
                prop::collection::vec(1u64..=(2u64 << shift), n),
                0u64..(1u64 << shift).max(2),
            )
                .prop_map(|(gaps, extra)| {
                    let mut values = Vec::with_capacity(gaps.len());
                    let mut v = 0u64;
                    for (i, gap) in gaps.into_iter().enumerate() {
                        v = if i == 0 { gap - 1 } else { v + gap };
                        if v >= u64::from(u32::MAX) {
                            break;
                        }
                        values.push(v as u32);
                    }
                    let universe = values.last().map_or(1, |&last| {
                        (u64::from(last) + 1 + extra).min(u64::from(u32::MAX))
                    });
                    (values, universe as u32)
                })
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn decodes_agree((values, universe) in list(), at in 0usize..16, after in 0usize..20) {
            let bytes = encoded(&values, universe, at, after);
            let list = Ef::parse(&bytes[at..], values.len(), universe).unwrap();
            let mut want = Vec::new();
            super::scalar::ef_decode(list, &mut want);
            prop_assert_eq!(&want, &values);
            let words = (universe as usize).div_ceil(64).min(1 << 20);
            let mut bits_want = vec![0u64; words];
            let rows = (universe as usize) <= words * 64;
            if rows {
                for v in &values {
                    bits_want[*v as usize / 64] |= 1 << (v % 64);
                }
            }
            for level in levels() {
                let mut have = Vec::new();
                ef_decode_at(level, list, &mut have);
                prop_assert_eq!(&have, &want, "{:?}", level);
                // The level's own body, which short lists skip.
                let mut have = vec![9];
                ef_run!(level, ef_decode(list, &mut have));
                prop_assert_eq!(&have[1..], &want[..], "body {:?}", level);
                if rows {
                    let mut bits_have = vec![0u64; words];
                    ef_into_row_at(level, list, &mut bits_have);
                    prop_assert_eq!(&bits_have, &bits_want, "{:?}", level);
                    let mut bits_have = vec![0u64; words];
                    ef_run!(level, ef_into_row(list, &mut bits_have));
                    prop_assert_eq!(&bits_have, &bits_want, "body {:?}", level);
                }
            }
        }
    }
}
