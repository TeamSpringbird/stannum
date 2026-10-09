// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Word kernels of the TIN-shaped path: a group's slot sets combined,
//! cleared and counted a block at a time.
//!
//! Each kernel has a portable body and a body per instruction set
//! ([`Level`]), chosen once per process and then by one load and branch per
//! call (never per word): every loop runs whole inside one function compiled
//! for its instruction set, so nothing depends on inlining across features.
//!
//! - aarch64: NEON is part of the baseline, so its bodies are the only ones a
//!   query runs.
//! - x86-64: a baseline build (as PGDG packages are) has neither a
//!   population-count instruction nor wide vectors, so the CPU is asked, as
//!   PostgreSQL's `pg_popcount` asks it: AVX-512 (F, BW, VPOPCNTDQ) with
//!   POPCNT, else AVX2 with POPCNT, else the portable bodies. A build for
//!   `x86-64-v3` or `-v4` runs the same choice (its portable bodies are
//!   compiled for those features anyway).
//!
//! On x86-64, `STANNUM_KERNELS` (`scalar`, `avx2` or `avx512`), read when
//! the choice is made, forces a level the CPU supports, for tests and A/B
//! runs; an unsupported or unknown name is ignored. The `*_at` functions run one
//! level on purpose, for tests that compare every level with the portable
//! one.
//!
//! Blocks are 64 bytes, a cache line on every x86-64 and most aarch64
//! parts; nothing here assumes the alignment of its inputs.

/// An instruction set the kernels have bodies for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Level {
    /// The portable bodies (vectorized as the compiler can for the target).
    Scalar,
    /// aarch64 Advanced SIMD.
    Neon,
    /// x86-64 AVX2 and POPCNT.
    Avx2,
    /// x86-64 AVX-512 F, BW and VPOPCNTDQ, and POPCNT.
    Avx512,
}

impl Level {
    pub const ALL: [Level; 4] = [Level::Scalar, Level::Neon, Level::Avx2, Level::Avx512];

    pub fn name(self) -> &'static str {
        match self {
            Level::Scalar => "scalar",
            Level::Neon => "neon",
            Level::Avx2 => "avx2",
            Level::Avx512 => "avx512",
        }
    }

    pub fn parse(name: &str) -> Option<Level> {
        Level::ALL
            .into_iter()
            .find(|l| l.name().eq_ignore_ascii_case(name.trim()))
    }

    /// Whether this CPU runs the level's bodies.
    pub fn supported(self) -> bool {
        match self {
            Level::Scalar => true,
            #[cfg(target_arch = "aarch64")]
            Level::Neon => true,
            #[cfg(target_arch = "x86_64")]
            Level::Avx2 => {
                std::arch::is_x86_feature_detected!("avx2")
                    && std::arch::is_x86_feature_detected!("popcnt")
            }
            #[cfg(target_arch = "x86_64")]
            Level::Avx512 => {
                std::arch::is_x86_feature_detected!("avx512f")
                    && std::arch::is_x86_feature_detected!("avx512bw")
                    && std::arch::is_x86_feature_detected!("avx512vpopcntdq")
                    && std::arch::is_x86_feature_detected!("popcnt")
            }
            #[allow(unreachable_patterns)]
            _ => false,
        }
    }

    /// The levels this CPU supports, the portable one first.
    pub fn supported_levels() -> Vec<Level> {
        Level::ALL.into_iter().filter(|l| l.supported()).collect()
    }

    /// The widest level this CPU supports.
    pub fn best() -> Level {
        [Level::Avx512, Level::Avx2, Level::Neon]
            .into_iter()
            .find(|l| l.supported())
            .unwrap_or(Level::Scalar)
    }
}

/// The level queries run: NEON on aarch64, a constant.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub fn level() -> Level {
    Level::Neon
}

/// The level queries run: chosen once, by the CPU and `STANNUM_KERNELS`.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub fn level() -> Level {
    use std::sync::atomic::{AtomicU8, Ordering};
    static CHOSEN: AtomicU8 = AtomicU8::new(0);

    #[cold]
    #[inline(never)]
    fn choose() -> Level {
        let level = chosen_level();
        CHOSEN.store(
            match level {
                Level::Avx512 => 3,
                Level::Avx2 => 2,
                _ => 1,
            },
            Ordering::Relaxed,
        );
        level
    }

    match CHOSEN.load(Ordering::Relaxed) {
        3 => Level::Avx512,
        2 => Level::Avx2,
        1 => Level::Scalar,
        _ => choose(),
    }
}

/// The level queries run: the portable bodies.
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
#[inline(always)]
pub fn level() -> Level {
    Level::Scalar
}

/// The level `STANNUM_KERNELS` names if the CPU supports it, else the best
/// (what [`level`] runs on x86-64).
pub fn chosen_level() -> Level {
    std::env::var("STANNUM_KERNELS")
        .ok()
        .and_then(|v| Level::parse(&v))
        .filter(|l| l.supported())
        .unwrap_or_else(Level::best)
}

/// Runs kernel `$name` at `$level`, which the CPU must support.
macro_rules! run {
    ($level:expr, $name:ident($($arg:expr),*)) => {
        match $level {
            #[cfg(target_arch = "x86_64")]
            // SAFETY: a level is only run when the CPU supports it.
            Level::Avx512 => unsafe { avx512::$name($($arg),*) },
            #[cfg(target_arch = "x86_64")]
            // SAFETY: as above.
            Level::Avx2 => unsafe { avx2::$name($($arg),*) },
            #[cfg(target_arch = "aarch64")]
            Level::Neon => neon::$name($($arg),*),
            _ => scalar::$name($($arg),*),
        }
    };
}

/// Each kernel at the chosen level, and at a given one (`$at`), checked to
/// be supported.
macro_rules! kernels {
    ($($(#[$doc:meta])* fn $name:ident / $at:ident ($($arg:ident: $ty:ty),*) $(-> $ret:ty)?;)*) => {$(
        $(#[$doc])*
        #[inline]
        pub fn $name($($arg: $ty),*) $(-> $ret)? {
            run!(level(), $name($($arg),*))
        }

        #[doc = concat!("[`", stringify!($name), "`] at `level`; panics if the CPU lacks it.")]
        pub fn $at(level: Level, $($arg: $ty),*) $(-> $ret)? {
            assert!(level.supported(), "kernel level {} unsupported here", level.name());
            run!(level, $name($($arg),*))
        }
    )*};
}

kernels! {
    /// Set bits in `words`.
    fn popcount / popcount_at(words: &[u64]) -> u64;
    /// Set bits in a byte string (a grid's words, as stored).
    fn popcount_bytes / popcount_bytes_at(bytes: &[u8]) -> u32;
    /// `out = bytes` read as little-endian words (as many as both hold).
    fn load / load_at(out: &mut [u64], bytes: &[u8]);
    /// `out &= bytes`.
    fn and_bytes / and_bytes_at(out: &mut [u64], bytes: &[u8]);
    /// `out |= bytes`.
    fn or_bytes / or_bytes_at(out: &mut [u64], bytes: &[u8]);
    /// `out &= !bytes`.
    fn andnot_bytes / andnot_bytes_at(out: &mut [u64], bytes: &[u8]);
    /// `out &= other`.
    fn and_words / and_words_at(out: &mut [u64], other: &[u64]);
    /// `out |= other`.
    fn or_words / or_words_at(out: &mut [u64], other: &[u64]);
    /// `out &= !other`.
    fn andnot_words / andnot_words_at(out: &mut [u64], other: &[u64]);
    /// Whether any bit of `words` is set.
    fn any_set / any_set_at(words: &[u64]) -> bool;
    /// Set bits of `a & b`, with `b` as little-endian bytes.
    fn and_count_bytes / and_count_bytes_at(a: &[u64], bytes: &[u8]) -> u64;
    /// Set bits of the AND of two little-endian byte bitmaps.
    fn and_count_two / and_count_two_at(x: &[u8], y: &[u8]) -> u64;
    /// Counts the bits of `out & mask`, then clears them from `out`; with
    /// whether any bit of `out` is left.
    fn and_count_clear / and_count_clear_at(out: &mut [u64], mask: &[u64]) -> (u64, bool);
    /// Appends, for each whole little-endian word of `bytes`, the set bits of
    /// the words before it.
    fn prefix_counts / prefix_counts_at(bytes: &[u8], out: &mut Vec<u32>);
}

#[inline(always)]
fn le(c: &[u8]) -> u64 {
    u64::from_le_bytes(c.try_into().expect("eight bytes"))
}

/// The portable bodies: the reference every other level is tested against.
mod scalar {
    use super::le;

    #[inline(always)]
    pub fn popcount(words: &[u64]) -> u64 {
        words.iter().map(|w| u64::from(w.count_ones())).sum()
    }

    #[inline(always)]
    pub fn popcount_bytes(bytes: &[u8]) -> u32 {
        let mut chunks = bytes.chunks_exact(8);
        let total: u32 = (&mut chunks).map(|c| le(c).count_ones()).sum();
        total
            + chunks
                .remainder()
                .iter()
                .map(|b| b.count_ones())
                .sum::<u32>()
    }

    #[inline(always)]
    pub fn load(out: &mut [u64], bytes: &[u8]) {
        for (w, c) in out.iter_mut().zip(bytes.chunks_exact(8)) {
            *w = le(c);
        }
    }

    #[inline(always)]
    pub fn and_bytes(out: &mut [u64], bytes: &[u8]) {
        for (w, c) in out.iter_mut().zip(bytes.chunks_exact(8)) {
            *w &= le(c);
        }
    }

    #[inline(always)]
    pub fn or_bytes(out: &mut [u64], bytes: &[u8]) {
        for (w, c) in out.iter_mut().zip(bytes.chunks_exact(8)) {
            *w |= le(c);
        }
    }

    #[inline(always)]
    pub fn andnot_bytes(out: &mut [u64], bytes: &[u8]) {
        for (w, c) in out.iter_mut().zip(bytes.chunks_exact(8)) {
            *w &= !le(c);
        }
    }

    #[inline(always)]
    pub fn and_words(out: &mut [u64], other: &[u64]) {
        for (o, x) in out.iter_mut().zip(other) {
            *o &= x;
        }
    }

    #[inline(always)]
    pub fn or_words(out: &mut [u64], other: &[u64]) {
        for (o, x) in out.iter_mut().zip(other) {
            *o |= x;
        }
    }

    #[inline(always)]
    pub fn andnot_words(out: &mut [u64], other: &[u64]) {
        for (o, x) in out.iter_mut().zip(other) {
            *o &= !x;
        }
    }

    #[inline(always)]
    pub fn any_set(words: &[u64]) -> bool {
        words.iter().any(|w| *w != 0)
    }

    #[inline(always)]
    pub fn and_count_bytes(a: &[u64], bytes: &[u8]) -> u64 {
        a.iter()
            .zip(bytes.chunks_exact(8))
            .map(|(w, c)| u64::from((w & le(c)).count_ones()))
            .sum()
    }

    #[inline(always)]
    pub fn and_count_two(x: &[u8], y: &[u8]) -> u64 {
        x.iter()
            .zip(y)
            .map(|(a, b)| u64::from((a & b).count_ones()))
            .sum()
    }

    #[inline(always)]
    pub fn and_count_clear(out: &mut [u64], mask: &[u64]) -> (u64, bool) {
        let mut total = 0u64;
        let mut any = false;
        for (o, m) in out.iter_mut().zip(mask) {
            total += u64::from((*o & m).count_ones());
            *o &= !m;
            any |= *o != 0;
        }
        (total, any)
    }

    #[inline(always)]
    pub fn prefix_counts(bytes: &[u8], out: &mut Vec<u32>) {
        out.reserve(bytes.len() / 8);
        let mut n = 0u32;
        for c in bytes.chunks_exact(8) {
            out.push(n);
            n += le(c).count_ones();
        }
    }
}

/// The NEON bodies, where explicit vectors beat what the compiler makes of
/// the portable ones; the rest are those.
#[cfg(target_arch = "aarch64")]
mod neon {
    pub use super::scalar::{
        and_bytes, and_count_clear, and_words, andnot_bytes, andnot_words, any_set, load, or_bytes,
        or_words, prefix_counts,
    };

    #[inline]
    pub fn popcount(words: &[u64]) -> u64 {
        // SAFETY: NEON is baseline on aarch64; loads stay within `words`.
        unsafe {
            use core::arch::aarch64::*;
            let mut acc = vdupq_n_u64(0);
            let chunks = words.chunks_exact(4);
            let rest = chunks.remainder();
            for c in chunks {
                let a = vcntq_u8(vreinterpretq_u8_u64(vld1q_u64(c.as_ptr())));
                let b = vcntq_u8(vreinterpretq_u8_u64(vld1q_u64(c.as_ptr().add(2))));
                acc = vaddq_u64(acc, vpaddlq_u32(vpaddlq_u16(vpaddlq_u8(vaddq_u8(a, b)))));
            }
            vaddvq_u64(acc) + rest.iter().map(|w| u64::from(w.count_ones())).sum::<u64>()
        }
    }

    #[inline]
    pub fn popcount_bytes(bytes: &[u8]) -> u32 {
        // SAFETY: NEON is baseline on aarch64; every load is within `bytes`.
        // Each 16-bit lane gains at most 32 per step, so 1,024 steps (32 KiB)
        // leave it below 65,536 (2,048 would reach it and wrap; a group's grid
        // is at most 9.3 KiB).
        unsafe {
            use core::arch::aarch64::*;
            let n = bytes.len();
            let p = bytes.as_ptr();
            let mut i = 0;
            let mut total = 0u32;
            while i + 32 <= n {
                let mut acc = vdupq_n_u16(0);
                let end = (i + 32 * 1024).min(n - n % 32);
                while i < end {
                    let a = vcntq_u8(vld1q_u8(p.add(i)));
                    let b = vcntq_u8(vld1q_u8(p.add(i + 16)));
                    acc = vpadalq_u8(acc, vaddq_u8(a, b));
                    i += 32;
                }
                total += vaddlvq_u16(acc);
            }
            while i + 8 <= n {
                total += super::le(&bytes[i..i + 8]).count_ones();
                i += 8;
            }
            while i < n {
                total += bytes[i].count_ones();
                i += 1;
            }
            total
        }
    }

    #[inline]
    pub fn and_count_bytes(a: &[u64], bytes: &[u8]) -> u64 {
        // SAFETY: NEON is baseline on aarch64; every load is within its
        // slice, the byte loads unaligned as `vld1q_u8` allows.
        unsafe {
            use core::arch::aarch64::*;
            let n = a.len().min(bytes.len() / 8);
            let mut acc = vdupq_n_u64(0);
            let mut i = 0;
            while i + 4 <= n {
                let x0 = vandq_u8(
                    vreinterpretq_u8_u64(vld1q_u64(a.as_ptr().add(i))),
                    vld1q_u8(bytes.as_ptr().add(i * 8)),
                );
                let x1 = vandq_u8(
                    vreinterpretq_u8_u64(vld1q_u64(a.as_ptr().add(i + 2))),
                    vld1q_u8(bytes.as_ptr().add(i * 8 + 16)),
                );
                let s = vaddq_u8(vcntq_u8(x0), vcntq_u8(x1));
                acc = vaddq_u64(acc, vpaddlq_u32(vpaddlq_u16(vpaddlq_u8(s))));
                i += 4;
            }
            let mut total = vaddvq_u64(acc);
            while i < n {
                let b = super::le(&bytes[i * 8..i * 8 + 8]);
                total += u64::from((a[i] & b).count_ones());
                i += 1;
            }
            total
        }
    }

    #[inline]
    pub fn and_count_two(x: &[u8], y: &[u8]) -> u64 {
        // SAFETY: as above.
        unsafe {
            use core::arch::aarch64::*;
            let n = x.len().min(y.len());
            let mut acc = vdupq_n_u64(0);
            let mut i = 0;
            while i + 32 <= n {
                let a = vandq_u8(vld1q_u8(x.as_ptr().add(i)), vld1q_u8(y.as_ptr().add(i)));
                let b = vandq_u8(
                    vld1q_u8(x.as_ptr().add(i + 16)),
                    vld1q_u8(y.as_ptr().add(i + 16)),
                );
                let s = vaddq_u8(vcntq_u8(a), vcntq_u8(b));
                acc = vaddq_u64(acc, vpaddlq_u32(vpaddlq_u16(vpaddlq_u8(s))));
                i += 32;
            }
            let mut total = vaddvq_u64(acc);
            while i < n {
                total += u64::from((x[i] & y[i]).count_ones());
                i += 1;
            }
            total
        }
    }
}

/// What a word operation does with its destination `o` and source `s`.
#[cfg(target_arch = "x86_64")]
mod op {
    pub const COPY: u8 = 0;
    pub const AND: u8 = 1;
    pub const OR: u8 = 2;
    pub const ANDNOT: u8 = 3;
}

/// The words of `w` as their little-endian bytes (x86-64 is little-endian).
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn bytes_of(w: &[u64]) -> &[u8] {
    // SAFETY: any initialized `u64` is eight initialized bytes, and `u8`
    // has no alignment to keep.
    unsafe { core::slice::from_raw_parts(w.as_ptr().cast(), w.len() * 8) }
}

/// AVX2 bodies: 32-byte vectors, two to a 64-byte block; population counts
/// by nibble lookup (`vpshufb`) summed by `vpsadbw` (Mula, Kurz and Lemire),
/// tails by POPCNT.
#[cfg(target_arch = "x86_64")]
mod avx2 {
    use super::{bytes_of, le, op, scalar};
    use core::arch::x86_64::*;

    /// Per-byte set-bit counts of `v`.
    #[target_feature(enable = "avx2,popcnt")]
    #[inline]
    fn counts(v: __m256i) -> __m256i {
        let lut = _mm256_setr_epi8(
            0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4, 0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2,
            3, 3, 4,
        );
        let low = _mm256_set1_epi8(0x0f);
        let lo = _mm256_and_si256(v, low);
        let hi = _mm256_and_si256(_mm256_srli_epi16::<4>(v), low);
        _mm256_add_epi8(_mm256_shuffle_epi8(lut, lo), _mm256_shuffle_epi8(lut, hi))
    }

    #[target_feature(enable = "avx2,popcnt")]
    #[inline]
    fn sum(v: __m256i) -> u64 {
        (_mm256_extract_epi64::<0>(v) as u64)
            .wrapping_add(_mm256_extract_epi64::<1>(v) as u64)
            .wrapping_add(_mm256_extract_epi64::<2>(v) as u64)
            .wrapping_add(_mm256_extract_epi64::<3>(v) as u64)
    }

    /// Set bits of the first `n` bytes of `a` (ANDed with `b`'s if `AND`).
    #[target_feature(enable = "avx2,popcnt")]
    #[inline]
    fn count<const AND: bool>(a: &[u8], b: &[u8], n: usize) -> u64 {
        debug_assert!(n <= a.len() && (!AND || n <= b.len()));
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let zero = _mm256_setzero_si256();
        let mut acc = zero;
        let mut i = 0;
        // SAFETY: every load is of 32 bytes at most `n` bytes in.
        unsafe {
            let ld = |p: *const u8, i: usize| _mm256_loadu_si256(p.add(i).cast());
            while i + 64 <= n {
                let (mut x0, mut x1) = (ld(pa, i), ld(pa, i + 32));
                if AND {
                    x0 = _mm256_and_si256(x0, ld(pb, i));
                    x1 = _mm256_and_si256(x1, ld(pb, i + 32));
                }
                let c = _mm256_add_epi8(counts(x0), counts(x1));
                acc = _mm256_add_epi64(acc, _mm256_sad_epu8(c, zero));
                i += 64;
            }
            if i + 32 <= n {
                let mut x = ld(pa, i);
                if AND {
                    x = _mm256_and_si256(x, ld(pb, i));
                }
                acc = _mm256_add_epi64(acc, _mm256_sad_epu8(counts(x), zero));
                i += 32;
            }
        }
        let mut total = sum(acc);
        while i + 8 <= n {
            let mut x = le(&a[i..i + 8]);
            if AND {
                x &= le(&b[i..i + 8]);
            }
            total += u64::from(x.count_ones());
            i += 8;
        }
        while i < n {
            let x = if AND { a[i] & b[i] } else { a[i] };
            total += u64::from(x.count_ones());
            i += 1;
        }
        total
    }

    /// `out[..n] op= s`, `s` read as little-endian words.
    #[target_feature(enable = "avx2,popcnt")]
    #[inline]
    fn binop<const OP: u8>(out: &mut [u64], s: &[u8]) {
        let n = out.len().min(s.len() / 8);
        let (po, ps) = (out.as_mut_ptr(), s.as_ptr());
        let mut i = 0;
        // SAFETY: every load and store is of four words at most `n` in.
        unsafe {
            while i + 4 <= n {
                let x = _mm256_loadu_si256(ps.add(i * 8).cast());
                let o = po.add(i).cast::<__m256i>();
                let v = match OP {
                    op::COPY => x,
                    op::AND => _mm256_and_si256(_mm256_loadu_si256(o), x),
                    op::OR => _mm256_or_si256(_mm256_loadu_si256(o), x),
                    _ => _mm256_andnot_si256(x, _mm256_loadu_si256(o)),
                };
                _mm256_storeu_si256(o, v);
                i += 4;
            }
        }
        for (o, c) in out[i..n].iter_mut().zip(s[i * 8..].chunks_exact(8)) {
            let x = le(c);
            *o = match OP {
                op::COPY => x,
                op::AND => *o & x,
                op::OR => *o | x,
                _ => *o & !x,
            };
        }
    }

    #[target_feature(enable = "avx2,popcnt")]
    pub fn popcount(words: &[u64]) -> u64 {
        let b = bytes_of(words);
        count::<false>(b, b, b.len())
    }

    #[target_feature(enable = "avx2,popcnt")]
    pub fn popcount_bytes(bytes: &[u8]) -> u32 {
        count::<false>(bytes, bytes, bytes.len()) as u32
    }

    #[target_feature(enable = "avx2,popcnt")]
    pub fn load(out: &mut [u64], bytes: &[u8]) {
        binop::<{ op::COPY }>(out, bytes)
    }

    #[target_feature(enable = "avx2,popcnt")]
    pub fn and_bytes(out: &mut [u64], bytes: &[u8]) {
        binop::<{ op::AND }>(out, bytes)
    }

    #[target_feature(enable = "avx2,popcnt")]
    pub fn or_bytes(out: &mut [u64], bytes: &[u8]) {
        binop::<{ op::OR }>(out, bytes)
    }

    #[target_feature(enable = "avx2,popcnt")]
    pub fn andnot_bytes(out: &mut [u64], bytes: &[u8]) {
        binop::<{ op::ANDNOT }>(out, bytes)
    }

    #[target_feature(enable = "avx2,popcnt")]
    pub fn and_words(out: &mut [u64], other: &[u64]) {
        binop::<{ op::AND }>(out, bytes_of(other))
    }

    #[target_feature(enable = "avx2,popcnt")]
    pub fn or_words(out: &mut [u64], other: &[u64]) {
        binop::<{ op::OR }>(out, bytes_of(other))
    }

    #[target_feature(enable = "avx2,popcnt")]
    pub fn andnot_words(out: &mut [u64], other: &[u64]) {
        binop::<{ op::ANDNOT }>(out, bytes_of(other))
    }

    #[target_feature(enable = "avx2,popcnt")]
    pub fn any_set(words: &[u64]) -> bool {
        let n = words.len();
        let p = words.as_ptr();
        let mut i = 0;
        // SAFETY: every load is of four words at most `n` in.
        unsafe {
            while i + 8 <= n {
                let x = _mm256_or_si256(
                    _mm256_loadu_si256(p.add(i).cast()),
                    _mm256_loadu_si256(p.add(i + 4).cast()),
                );
                if _mm256_testz_si256(x, x) == 0 {
                    return true;
                }
                i += 8;
            }
        }
        words[i..].iter().any(|w| *w != 0)
    }

    #[target_feature(enable = "avx2,popcnt")]
    pub fn and_count_bytes(a: &[u64], bytes: &[u8]) -> u64 {
        let n = a.len().min(bytes.len() / 8) * 8;
        count::<true>(bytes_of(a), bytes, n)
    }

    #[target_feature(enable = "avx2,popcnt")]
    pub fn and_count_two(x: &[u8], y: &[u8]) -> u64 {
        count::<true>(x, y, x.len().min(y.len()))
    }

    #[target_feature(enable = "avx2,popcnt")]
    pub fn and_count_clear(out: &mut [u64], mask: &[u64]) -> (u64, bool) {
        let n = out.len().min(mask.len());
        let (po, pm) = (out.as_mut_ptr(), mask.as_ptr());
        let zero = _mm256_setzero_si256();
        let mut acc = zero;
        let mut left = zero;
        let mut i = 0;
        // SAFETY: every load and store is of four words at most `n` in.
        unsafe {
            while i + 4 <= n {
                let o = po.add(i).cast::<__m256i>();
                let (x, m) = (_mm256_loadu_si256(o), _mm256_loadu_si256(pm.add(i).cast()));
                acc = _mm256_add_epi64(acc, _mm256_sad_epu8(counts(_mm256_and_si256(x, m)), zero));
                let r = _mm256_andnot_si256(m, x);
                _mm256_storeu_si256(o, r);
                left = _mm256_or_si256(left, r);
                i += 4;
            }
        }
        let (rest, any) = scalar::and_count_clear(&mut out[i..n], &mask[i..n]);
        (sum(acc) + rest, any || _mm256_testz_si256(left, left) == 0)
    }

    #[target_feature(enable = "avx2,popcnt")]
    pub fn prefix_counts(bytes: &[u8], out: &mut Vec<u32>) {
        scalar::prefix_counts(bytes, out)
    }
}

/// AVX-512 bodies: one 64-byte vector to a block, VPOPCNTQ counts, and
/// tails by masked loads and stores (which touch no byte past the mask).
#[cfg(target_arch = "x86_64")]
mod avx512 {
    use super::{bytes_of, op, scalar};
    use core::arch::x86_64::*;

    /// The mask of the first `k` (below 64) of 64 bytes.
    #[inline(always)]
    fn first(k: usize) -> __mmask64 {
        debug_assert!(k < 64);
        (1u64 << k) - 1
    }

    /// Set bits of the first `n` bytes of `a` (ANDed with `b`'s if `AND`).
    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    #[inline]
    fn count<const AND: bool>(a: &[u8], b: &[u8], n: usize) -> u64 {
        debug_assert!(n <= a.len() && (!AND || n <= b.len()));
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc = _mm512_setzero_si512();
        let mut i = 0;
        // SAFETY: every full load is of 64 bytes at most `n` in; the last
        // is masked to the bytes below `n`.
        unsafe {
            while i + 64 <= n {
                let mut x = _mm512_loadu_si512(pa.add(i).cast());
                if AND {
                    x = _mm512_and_si512(x, _mm512_loadu_si512(pb.add(i).cast()));
                }
                acc = _mm512_add_epi64(acc, _mm512_popcnt_epi64(x));
                i += 64;
            }
            if i < n {
                let m = first(n - i);
                let mut x = _mm512_maskz_loadu_epi8(m, pa.add(i).cast());
                if AND {
                    x = _mm512_and_si512(x, _mm512_maskz_loadu_epi8(m, pb.add(i).cast()));
                }
                acc = _mm512_add_epi64(acc, _mm512_popcnt_epi64(x));
            }
        }
        _mm512_reduce_add_epi64(acc) as u64
    }

    /// `out[..n] op= s`, `s` read as little-endian words.
    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    #[inline]
    fn binop<const OP: u8>(out: &mut [u64], s: &[u8]) {
        let n = out.len().min(s.len() / 8);
        let (po, ps) = (out.as_mut_ptr(), s.as_ptr());
        let apply = |o: __m512i, x: __m512i| match OP {
            op::COPY => x,
            op::AND => _mm512_and_si512(o, x),
            op::OR => _mm512_or_si512(o, x),
            _ => _mm512_andnot_si512(x, o),
        };
        let mut i = 0;
        // SAFETY: every full load and store is of eight words at most `n`
        // in; the last are masked to the words below `n`.
        unsafe {
            while i + 8 <= n {
                let o = po.add(i);
                let x = _mm512_loadu_si512(ps.add(i * 8).cast());
                let v = if OP == op::COPY {
                    x
                } else {
                    apply(_mm512_loadu_si512(o.cast()), x)
                };
                _mm512_storeu_si512(o.cast(), v);
                i += 8;
            }
            if i < n {
                let m = ((1u32 << (n - i)) - 1) as __mmask8;
                let o = po.add(i);
                let x = _mm512_maskz_loadu_epi64(m, ps.add(i * 8).cast());
                let v = if OP == op::COPY {
                    x
                } else {
                    apply(_mm512_maskz_loadu_epi64(m, o.cast()), x)
                };
                _mm512_mask_storeu_epi64(o.cast(), m, v);
            }
        }
    }

    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    pub fn popcount(words: &[u64]) -> u64 {
        let b = bytes_of(words);
        count::<false>(b, b, b.len())
    }

    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    pub fn popcount_bytes(bytes: &[u8]) -> u32 {
        count::<false>(bytes, bytes, bytes.len()) as u32
    }

    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    pub fn load(out: &mut [u64], bytes: &[u8]) {
        binop::<{ op::COPY }>(out, bytes)
    }

    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    pub fn and_bytes(out: &mut [u64], bytes: &[u8]) {
        binop::<{ op::AND }>(out, bytes)
    }

    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    pub fn or_bytes(out: &mut [u64], bytes: &[u8]) {
        binop::<{ op::OR }>(out, bytes)
    }

    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    pub fn andnot_bytes(out: &mut [u64], bytes: &[u8]) {
        binop::<{ op::ANDNOT }>(out, bytes)
    }

    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    pub fn and_words(out: &mut [u64], other: &[u64]) {
        binop::<{ op::AND }>(out, bytes_of(other))
    }

    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    pub fn or_words(out: &mut [u64], other: &[u64]) {
        binop::<{ op::OR }>(out, bytes_of(other))
    }

    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    pub fn andnot_words(out: &mut [u64], other: &[u64]) {
        binop::<{ op::ANDNOT }>(out, bytes_of(other))
    }

    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    pub fn any_set(words: &[u64]) -> bool {
        let n = words.len();
        let p = words.as_ptr();
        let mut i = 0;
        // SAFETY: every full load is of eight words at most `n` in; the
        // last is masked to the words below `n`.
        unsafe {
            while i + 8 <= n {
                let x = _mm512_loadu_si512(p.add(i).cast());
                if _mm512_test_epi64_mask(x, x) != 0 {
                    return true;
                }
                i += 8;
            }
            if i < n {
                let m = ((1u32 << (n - i)) - 1) as __mmask8;
                let x = _mm512_maskz_loadu_epi64(m, p.add(i).cast());
                return _mm512_test_epi64_mask(x, x) != 0;
            }
        }
        false
    }

    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    pub fn and_count_bytes(a: &[u64], bytes: &[u8]) -> u64 {
        let n = a.len().min(bytes.len() / 8) * 8;
        count::<true>(bytes_of(a), bytes, n)
    }

    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    pub fn and_count_two(x: &[u8], y: &[u8]) -> u64 {
        count::<true>(x, y, x.len().min(y.len()))
    }

    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    pub fn and_count_clear(out: &mut [u64], mask: &[u64]) -> (u64, bool) {
        let n = out.len().min(mask.len());
        let (po, pm) = (out.as_mut_ptr(), mask.as_ptr());
        let mut acc = _mm512_setzero_si512();
        let mut left = _mm512_setzero_si512();
        let mut i = 0;
        // SAFETY: every full load and store is of eight words at most `n`
        // in; the last are masked to the words below `n`.
        unsafe {
            while i < n {
                let m = if i + 8 <= n {
                    0xff
                } else {
                    ((1u32 << (n - i)) - 1) as __mmask8
                };
                let o = po.add(i);
                let x = _mm512_maskz_loadu_epi64(m, o.cast());
                let k = _mm512_maskz_loadu_epi64(m, pm.add(i).cast());
                acc = _mm512_add_epi64(acc, _mm512_popcnt_epi64(_mm512_and_si512(x, k)));
                let r = _mm512_andnot_si512(k, x);
                _mm512_mask_storeu_epi64(o.cast(), m, r);
                left = _mm512_or_si512(left, r);
                i += 8;
            }
        }
        (
            _mm512_reduce_add_epi64(acc) as u64,
            _mm512_test_epi64_mask(left, left) != 0,
        )
    }

    #[target_feature(enable = "avx512f,avx512bw,avx512vpopcntdq,popcnt")]
    pub fn prefix_counts(bytes: &[u8], out: &mut Vec<u32>) {
        scalar::prefix_counts(bytes, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Bytes from `seed` with about `density` of 256 bits set (0 and 256:
    /// none and all).
    fn bytes(seed: u64, density: u32, n: usize) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                let mut b = 0u8;
                for bit in 0..8 {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    if ((s >> 24) as u32 & 0xff) < density {
                        b |= 1 << bit;
                    }
                }
                b
            })
            .collect()
    }

    fn words(seed: u64, density: u32, n: usize) -> Vec<u64> {
        bytes(seed, density, n * 8)
            .chunks_exact(8)
            .map(le)
            .collect()
    }

    /// The levels to compare with the portable one. `STANNUM_EXPECT_KERNELS`
    /// (a comma-separated list) names levels that must be among them, so a
    /// run on a machine expected to have AVX-512 fails rather than skips.
    fn levels() -> Vec<Level> {
        let have = Level::supported_levels();
        if let Ok(want) = std::env::var("STANNUM_EXPECT_KERNELS") {
            for name in want.split(',').filter(|s| !s.trim().is_empty()) {
                let level = Level::parse(name).unwrap_or_else(|| panic!("unknown level {name}"));
                assert!(have.contains(&level), "{} not supported here", level.name());
            }
        }
        have
    }

    fn density() -> impl Strategy<Value = u32> {
        prop_oneof![Just(0u32), Just(256), 1u32..32, 32u32..224, 224u32..256]
    }

    /// Lengths around the block sizes, and up to a grid's largest (9.3 KiB)
    /// and beyond.
    fn length() -> impl Strategy<Value = usize> {
        prop_oneof![0usize..70, 120usize..136, 0usize..1300, 9000usize..10_000]
    }

    #[test]
    fn reports_levels() {
        let have = levels();
        eprintln!(
            "kernel levels here: {:?}; queries run {:?}",
            have.iter().map(|l| l.name()).collect::<Vec<_>>(),
            level().name()
        );
        assert!(have.contains(&Level::Scalar));
        assert!(have.contains(&level()));
        #[cfg(target_arch = "x86_64")]
        assert_eq!(level(), chosen_level());
    }

    #[test]
    fn counts_without_overflow() {
        // All ones past where 8- and 16-bit lane sums would wrap.
        let ones = vec![!0u8; 70_001];
        let words = vec![!0u64; 70_001 / 8];
        for level in levels() {
            assert_eq!(popcount_bytes_at(level, &ones), 70_001 * 8, "{level:?}");
            assert_eq!(popcount_at(level, &words), 8750 * 64, "{level:?}");
            assert_eq!(
                and_count_two_at(level, &ones, &ones),
                70_001 * 8,
                "{level:?}"
            );
            assert_eq!(
                and_count_bytes_at(level, &words, &ones),
                8750 * 64,
                "{level:?}"
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn counts_agree(seed in any::<u64>(), d in density(), n in length(), at in 0usize..64) {
            let buf = bytes(seed, d, n + at);
            let b = &buf[at..];
            let wbuf = words(seed ^ 1, d, n / 8 + 8);
            let w = &wbuf[at % 8..at % 8 + n / 8];
            let want_bytes = scalar::popcount_bytes(b);
            let want_words = scalar::popcount(w);
            for level in levels() {
                prop_assert_eq!(popcount_bytes_at(level, b), want_bytes, "{:?}", level);
                prop_assert_eq!(popcount_at(level, w), want_words, "{:?}", level);
                prop_assert_eq!(any_set_at(level, w), scalar::any_set(w), "{:?}", level);
            }
        }

        #[test]
        fn and_counts_agree(
            seed in any::<u64>(),
            d in density(),
            e in density(),
            n in length(),
            m in length(),
            at in 0usize..64,
            bt in 0usize..64,
        ) {
            let xb = bytes(seed, d, n + at);
            let yb = bytes(seed ^ 7, e, m + bt);
            let (x, y) = (&xb[at..], &yb[bt..]);
            let wbuf = words(seed ^ 3, e, n / 8 + 8);
            let w = &wbuf[bt % 8..bt % 8 + n / 8];
            let two = scalar::and_count_two(x, y);
            let mixed = scalar::and_count_bytes(w, y);
            for level in levels() {
                prop_assert_eq!(and_count_two_at(level, x, y), two, "{:?}", level);
                prop_assert_eq!(and_count_bytes_at(level, w, y), mixed, "{:?}", level);
            }
        }

        #[test]
        fn word_ops_agree(
            seed in any::<u64>(),
            d in density(),
            e in density(),
            n in length(),
            m in length(),
            at in 0usize..64,
            ot in 0usize..8,
        ) {
            let n = n / 8;
            let src_buf = bytes(seed, d, m + at);
            let src = &src_buf[at..];
            let other_buf = words(seed ^ 5, d, m / 8 + 8);
            let other = &other_buf[ot..ot + m / 8];
            let start = words(seed ^ 9, e, n + 8);
            type Op = fn(Level, &mut [u64], &[u8]);
            type WordOp = fn(Level, &mut [u64], &[u64]);
            type ByteRef = fn(&mut [u64], &[u8]);
            type WordRef = fn(&mut [u64], &[u64]);
            let byte_ops: [(&str, Op, ByteRef); 4] = [
                ("load", load_at, scalar::load),
                ("and_bytes", and_bytes_at, scalar::and_bytes),
                ("or_bytes", or_bytes_at, scalar::or_bytes),
                ("andnot_bytes", andnot_bytes_at, scalar::andnot_bytes),
            ];
            let word_ops: [(&str, WordOp, WordRef); 3] = [
                ("and_words", and_words_at, scalar::and_words),
                ("or_words", or_words_at, scalar::or_words),
                ("andnot_words", andnot_words_at, scalar::andnot_words),
            ];
            for level in levels() {
                for (name, at_level, reference) in byte_ops {
                    let mut want = start.clone();
                    reference(&mut want[ot..ot + n], src);
                    let mut have = start.clone();
                    at_level(level, &mut have[ot..ot + n], src);
                    prop_assert_eq!(&have, &want, "{} {:?}", name, level);
                }
                for (name, at_level, reference) in word_ops {
                    let mut want = start.clone();
                    reference(&mut want[ot..ot + n], other);
                    let mut have = start.clone();
                    at_level(level, &mut have[ot..ot + n], other);
                    prop_assert_eq!(&have, &want, "{} {:?}", name, level);
                }
                let mut want = start.clone();
                let r = scalar::and_count_clear(&mut want[ot..ot + n], other);
                let mut have = start.clone();
                let h = and_count_clear_at(level, &mut have[ot..ot + n], other);
                prop_assert_eq!(h, r, "and_count_clear {:?}", level);
                prop_assert_eq!(&have, &want, "and_count_clear {:?}", level);
            }
        }

        #[test]
        fn prefix_counts_agree(seed in any::<u64>(), d in density(), n in length(), at in 0usize..64) {
            let buf = bytes(seed, d, n + at);
            let b = &buf[at..];
            let mut want = vec![7];
            scalar::prefix_counts(b, &mut want);
            for level in levels() {
                let mut have = vec![7];
                prefix_counts_at(level, b, &mut have);
                prop_assert_eq!(&have, &want, "{:?}", level);
            }
        }
    }
}
