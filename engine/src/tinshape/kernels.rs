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

/// Whether the AVX-512 level's VBMI bodies run (the Elias-Fano kernels and
/// [`set_bits`]): they need
/// VBMI, VBMI2 and BMI2 besides the level's features (Ice Lake, Sapphire
/// Rapids and Zen 4 on have them; Skylake-SP and Cascade Lake do not, and
/// run the AVX2 bodies at that level). Asked once.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(crate) fn vbmi() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static HAS: AtomicU8 = AtomicU8::new(0);

    #[cold]
    #[inline(never)]
    fn ask() -> bool {
        let has = std::arch::is_x86_feature_detected!("avx512vbmi")
            && std::arch::is_x86_feature_detected!("avx512vbmi2")
            && std::arch::is_x86_feature_detected!("bmi2");
        HAS.store(if has { 2 } else { 1 }, Ordering::Relaxed);
        has
    }

    match HAS.load(Ordering::Relaxed) {
        0 => ask(),
        h => h == 2,
    }
}

/// Whether the walks run their own instantiation of a disjunction's group
/// and candidate code built with POPCNT, BMI1, BMI2 and LZCNT on x86-64:
/// baseline x86-64 counts a word's bits by shifts and masks, and a
/// disjunction counts some for each candidate's terms. At a vector level
/// (`STANNUM_KERNELS=scalar` runs the baseline code too) on a CPU with all
/// four (every AVX2 CPU has them). Asked once.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(crate) fn bit_ops() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static HAS: AtomicU8 = AtomicU8::new(0);

    #[cold]
    #[inline(never)]
    fn ask() -> bool {
        let has = level() != Level::Scalar
            && std::arch::is_x86_feature_detected!("popcnt")
            && std::arch::is_x86_feature_detected!("bmi1")
            && std::arch::is_x86_feature_detected!("bmi2")
            && std::arch::is_x86_feature_detected!("lzcnt");
        HAS.store(if has { 2 } else { 1 }, Ordering::Relaxed);
        has
    }

    match HAS.load(Ordering::Relaxed) {
        0 => ask(),
        h => h == 2,
    }
}

/// Appends the position of each set bit of `words` (bit `i` of word `w`
/// at `64 * w + i`), in order.
#[inline]
pub fn set_bits(words: &[u64], out: &mut Vec<u32>) {
    set_bits_at(level(), words, out);
}

/// [`set_bits`] of a byte string read as little-endian words (its whole
/// words; a grid's, as stored).
#[inline]
pub fn set_bits_bytes(bytes: &[u8], out: &mut Vec<u32>) {
    set_bits_bytes_at(level(), bytes, out);
}

/// [`set_bits`] at `level`, which the CPU must support (checked only in
/// debug builds; [`set_bits`] passes the chosen one).
#[inline]
pub fn set_bits_at(level: Level, words: &[u64], out: &mut Vec<u32>) {
    debug_assert!(level.supported());
    match level {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: the level and its VBMI features are the CPU's; x86-64
        // is little-endian, so the words are their bytes.
        Level::Avx512 if vbmi() => unsafe { avx512::set_bits(bytes_of(words), out) },
        _ => scalar::set_bits(words, out),
    }
}

/// [`set_bits_bytes`] at `level` (see [`set_bits_at`]).
#[inline]
pub fn set_bits_bytes_at(level: Level, bytes: &[u8], out: &mut Vec<u32>) {
    debug_assert!(level.supported());
    let bytes = &bytes[..bytes.len() / 8 * 8];
    match level {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as in `set_bits_at`.
        Level::Avx512 if vbmi() => unsafe { avx512::set_bits(bytes, out) },
        _ => scalar::set_bits_bytes(bytes, out),
    }
}

// ---- A sieve's first pass: its first term's buckets as pass bits. ----
//
// A term's buckets in a group are a run of fields of one width (0, 1, 2 or
// 4 bits) in its members' order. [`pass_bits`] turns a run into a bit per
// member, whether its bucket passes (bit `b` of a 16-bit `pass`), by width:
// a mask per value for 1 bit, a three-mask select over each 2-bit field's
// bits and a compaction (PEXT where fast) for 2, a 16-entry table lookup
// (`tbl`, `pshufb`) for 4. [`deposit_pass`] lays a word's members' bits
// onto the word's candidates: PDEP where fast, else a byte at a time by a
// nibble table ([`expand`]; on the M4, 3-5% fewer cycles over a dense pair
// than reading each candidate's bit by its rank).

/// Whether PDEP and PEXT run fast: BMI2, on any CPU but AMD's and Hygon's
/// before Zen 3 (family 0x19), whose microcode takes a cycle or more per set
/// bit of the mask. `STANNUM_PDEP=0` turns them off (A/B runs). Asked once.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub fn fast_pdep() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static HAS: AtomicU8 = AtomicU8::new(0);

    #[cold]
    #[inline(never)]
    fn ask() -> bool {
        let has = std::arch::is_x86_feature_detected!("bmi2")
            && std::env::var("STANNUM_PDEP").map_or(true, |v| v.trim() != "0")
            && !slow_pdep_vendor();
        HAS.store(if has { 2 } else { 1 }, Ordering::Relaxed);
        has
    }

    match HAS.load(Ordering::Relaxed) {
        0 => ask(),
        h => h == 2,
    }
}

/// Whether the CPU is AMD's or Hygon's, of a family before 0x19 (Zen 3).
#[cfg(target_arch = "x86_64")]
fn slow_pdep_vendor() -> bool {
    use core::arch::x86_64::__cpuid;
    // CPUID is on every x86-64; leaf 0 gives the vendor, leaf 1 the family.
    let v = __cpuid(0);
    let vendor = (v.ebx, v.edx, v.ecx);
    let amd = vendor == (0x6874_7541, 0x6974_6e65, 0x444d_4163); // AuthenticAMD
    let hygon = vendor == (0x6f67_7948, 0x6e65_476e, 0x656e_6975); // HygonGenuine
    if !(amd || hygon) {
        return false;
    }
    let eax = __cpuid(1).eax;
    let base = (eax >> 8) & 0xf;
    let family = if base == 0xf {
        base + ((eax >> 20) & 0xff)
    } else {
        base
    };
    family < 0x19
}

/// Appends to `out` the `n` bits of `bytes` from bit `bit` on, a word per 64
/// (bit `i` of the run at bit `i % 64` of word `i / 64`), the last word's
/// bits past the run clear; bits past the bytes read as clear.
pub fn gather_bits(bytes: &[u8], bit: usize, n: usize, out: &mut Vec<u64>) {
    let words = n.div_ceil(64);
    out.reserve(words);
    for i in 0..words {
        let b = bit + 64 * i;
        let (at, s) = (b / 8, (b % 8) as u32);
        let mut nine = [0u8; 9];
        let chunk = match bytes.get(at..at + 9) {
            Some(c) => c,
            None => {
                let rest = bytes.get(at..).unwrap_or(&[]);
                let k = rest.len().min(9);
                nine[..k].copy_from_slice(&rest[..k]);
                &nine
            }
        };
        let (lo, hi) = (le(&chunk[..8]), u64::from(chunk[8]));
        out.push((lo >> s) | ((hi << 1) << (63 - s)));
    }
    if !n.is_multiple_of(64) {
        *out.last_mut().expect("a word") &= (1u64 << (n % 64)) - 1;
    }
}

/// `out[k]` = the pass bits of fields `64 k ..` of the run in `fields`
/// (fields of `width` bits, packed from bit 0 of the first word): bit `j`
/// set when bit `v` of `pass` is, `v` field `64 k + j`'s value. `fields`
/// holds `width` words per word of `out` (none at width 0).
#[inline]
pub fn pass_bits(fields: &[u64], width: u32, pass: u16, out: &mut [u64]) {
    pass_bits_at(level(), fields, width, pass, out);
}

/// [`pass_bits`] at `level`, which the CPU must support.
pub fn pass_bits_at(level: Level, fields: &[u64], width: u32, pass: u16, out: &mut [u64]) {
    assert!(matches!(width, 0 | 1 | 2 | 4), "a TF width");
    assert!(
        fields.len() >= out.len() * width as usize,
        "the fields of every word"
    );
    debug_assert!(level.supported());
    match level {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: the level is the CPU's (AVX-512's CPUs have AVX2).
        Level::Avx2 | Level::Avx512 => unsafe { avx2::pass_bits(fields, width, pass, out) },
        #[cfg(target_arch = "aarch64")]
        Level::Neon => neon::pass_bits(fields, width, pass, out),
        _ => scalar::pass_bits(fields, width, pass, out),
    }
}

/// Clears from `words` (a group's candidates, a bit per slot) each that is
/// a member (`members`, a bitmap as stored) whose pass bit is clear, and
/// each that is not unless `absent` is all ones; gives how many it cleared.
/// The pass bit of the member of rank `r` (members counted from the
/// group's first slot) is bit `r - lo` of `pass`, which holds a word past
/// the last such bit read; every member of a word holding a candidate
/// member ranks `lo` or more.
#[inline]
pub fn deposit_pass(words: &mut [u64], members: &[u8], pass: &[u64], lo: u32, absent: u64) -> u64 {
    deposit_pass_at(level(), words, members, pass, lo, absent)
}

/// [`deposit_pass`] at `level`, which the CPU must support.
pub fn deposit_pass_at(
    level: Level,
    words: &mut [u64],
    members: &[u8],
    pass: &[u64],
    lo: u32,
    absent: u64,
) -> u64 {
    debug_assert!(level.supported());
    match level {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: BMI2 is the CPU's (`fast_pdep`), POPCNT the level's.
        Level::Avx2 | Level::Avx512 if fast_pdep() => unsafe {
            bmi2::deposit_pass(words, members, pass, lo, absent)
        },
        _ => scalar::deposit_pass(words, members, pass, lo, absent),
    }
}

/// PDEP without BMI2: bits of `p` laid onto the set bits of `m` in order (the
/// lowest bit of `p` onto the lowest of `m`), a byte of `m` at a time, each
/// from where the bytes before it end (a prefix sum of their counts) and a
/// table per nibble: no branch on the bits. Also the count of `m`.
#[inline(always)]
fn expand(p: u64, m: u64) -> (u64, u32) {
    /// `EXPAND4[mask << 4 | bits]`: the low bits of `bits` laid onto `mask`.
    const EXPAND4: [u8; 256] = {
        let mut t = [0u8; 256];
        let mut i = 0;
        while i < 256 {
            let (mask, bits) = (i >> 4, i & 15);
            let (mut out, mut k, mut b) = (0u8, 0, 0);
            while b < 4 {
                if mask >> b & 1 == 1 {
                    out |= ((bits >> k & 1) as u8) << b;
                    k += 1;
                }
                b += 1;
            }
            t[i] = out;
            i += 1;
        }
        t
    };
    const ONES: u64 = 0x0101_0101_0101_0101;
    let mut c = m - ((m >> 1) & 0x5555_5555_5555_5555);
    c = (c & 0x3333_3333_3333_3333) + ((c >> 2) & 0x3333_3333_3333_3333);
    c = (c + (c >> 4)) & 0x0f0f_0f0f_0f0f_0f0f;
    // Byte `i`: the members of the bytes before it.
    let before = c.wrapping_mul(ONES) << 8;
    let mut out = 0u64;
    for i in 0..8 {
        let mb = (m >> (8 * i)) as usize & 0xff;
        let pb = p >> ((before >> (8 * i)) & 0xff);
        let (lo, hi) = (mb & 15, mb >> 4);
        let low = EXPAND4[(lo << 4) | (pb as usize & 15)];
        // The low nibble's count, from a 16-entry table packed in a word.
        let pb = pb >> ((0x4332_3221_3221_2110u64 >> (4 * lo)) & 15);
        let high = EXPAND4[(hi << 4) | (pb as usize & 15)];
        out |= u64::from(low | (high << 4)) << (8 * i);
    }
    (out, (c.wrapping_mul(ONES) >> 56) as u32)
}

/// Word `i` of a bitmap as stored (bytes past it clear).
#[inline(always)]
fn member_word(bytes: &[u8], i: usize) -> u64 {
    match bytes.get(i * 8..i * 8 + 8) {
        Some(c) => le(c),
        None => {
            let mut eight = [0u8; 8];
            let rest = bytes.get(i * 8..).unwrap_or(&[]);
            eight[..rest.len()].copy_from_slice(rest);
            u64::from_le_bytes(eight)
        }
    }
}

/// Bits `off ..` of `pass` (a word past them held).
#[inline(always)]
fn pass_window(pass: &[u64], off: u32) -> u64 {
    let (i, s) = ((off / 64) as usize, off % 64);
    (pass[i] >> s) | ((pass[i + 1] << 1) << (63 - s))
}

/// Each 2-bit field of `x`, at its low bit, whether bit `v` of `pass` is set
/// (`v` its value); its high bit clear.
#[inline(always)]
fn pass2(x: u64, pass: u16) -> u64 {
    const EVEN: u64 = 0x5555_5555_5555_5555;
    let v = |b: u32| 0u64.wrapping_sub(u64::from((pass >> b) & 1));
    let (lo, hi) = (x & EVEN, (x >> 1) & EVEN);
    let below = (lo & v(1)) | (!lo & v(0));
    let above = (lo & v(3)) | (!lo & v(2));
    ((hi & above) | (!hi & below)) & EVEN
}

/// The even bits of `x` (its odd ones clear), packed into its low 32.
#[inline(always)]
fn compress_even(mut x: u64) -> u64 {
    x = (x | (x >> 1)) & 0x3333_3333_3333_3333;
    x = (x | (x >> 2)) & 0x0f0f_0f0f_0f0f_0f0f;
    x = (x | (x >> 4)) & 0x00ff_00ff_00ff_00ff;
    x = (x | (x >> 8)) & 0x0000_ffff_0000_ffff;
    (x | (x >> 16)) & 0xffff_ffff
}

/// The table [`pass_bits`] looks a 4-bit field up in: `set` where `pass`
/// passes the value, else 0.
#[inline(always)]
fn pass_table(pass: u16, set: u8) -> [u8; 16] {
    std::array::from_fn(|v| if (pass >> v) & 1 != 0 { set } else { 0 })
}

/// BMI2 bodies (PDEP, PEXT), run where they are fast ([`fast_pdep`]).
#[cfg(target_arch = "x86_64")]
mod bmi2 {
    use super::{member_word, pass_window, pass2};
    use core::arch::x86_64::*;

    #[target_feature(enable = "bmi2,popcnt")]
    pub fn deposit_pass(
        words: &mut [u64],
        members: &[u8],
        pass: &[u64],
        lo: u32,
        absent: u64,
    ) -> u64 {
        let (mut run, mut dropped) = (0u32, 0u64);
        for (w, word) in words.iter_mut().enumerate() {
            let m = member_word(members, w);
            let c = *word;
            if c != 0 {
                let p = if c & m != 0 {
                    pass_window(pass, run - lo)
                } else {
                    0
                };
                let out = (c & !m & absent) | (_pdep_u64(p, m) & c);
                dropped += u64::from((c & !out).count_ones());
                *word = out;
            }
            run += m.count_ones();
        }
        dropped
    }

    /// [`super::pass_bits`] of 2-bit fields.
    #[target_feature(enable = "bmi2")]
    pub fn pass_bits2(fields: &[u64], pass: u16, out: &mut [u64]) {
        const EVEN: u64 = 0x5555_5555_5555_5555;
        for (o, f) in out.iter_mut().zip(fields.chunks_exact(2)) {
            *o = _pext_u64(pass2(f[0], pass), EVEN) | (_pext_u64(pass2(f[1], pass), EVEN) << 32);
        }
    }
}

mod array;
pub use array::{array_decode, array_decode_at, array_into_row, array_retain, array_seek};
mod ef;
pub use ef::{
    container_locals, container_row, ef_decode, ef_decode_at, ef_into_row, ef_into_row_at,
};
mod rows;
pub(crate) use rows::RowPut;
pub use rows::{
    intersect_marked, set_ascending, set_ascending_at, set_ascending_u16, set_ascending_u16_at,
};
mod weigh;
pub use weigh::{Row, member_sums, member_sums_at, or_zero, word_sums, word_sums_at};
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

    /// A word's set bits, lowest first: count trailing zeros, clear lowest.
    #[inline(always)]
    fn word_bits(at: u32, mut word: u64, out: &mut Vec<u32>) {
        while word != 0 {
            out.push(at + word.trailing_zeros());
            word &= word - 1;
        }
    }

    #[inline]
    pub fn set_bits(words: &[u64], out: &mut Vec<u32>) {
        for (w, word) in words.iter().enumerate() {
            word_bits((w * 64) as u32, *word, out);
        }
    }

    #[inline]
    pub fn set_bits_bytes(bytes: &[u8], out: &mut Vec<u32>) {
        for (w, c) in bytes.chunks_exact(8).enumerate() {
            word_bits((w * 64) as u32, le(c), out);
        }
    }

    pub fn pass_bits(fields: &[u64], width: u32, pass: u16, out: &mut [u64]) {
        use super::{compress_even, pass2};
        let v = |b: u32| 0u64.wrapping_sub(u64::from((pass >> b) & 1));
        match width {
            0 => out.fill(v(0)),
            1 => {
                for (o, &x) in out.iter_mut().zip(fields) {
                    *o = (x & v(1)) | (!x & v(0));
                }
            }
            2 => {
                for (o, f) in out.iter_mut().zip(fields.chunks_exact(2)) {
                    *o =
                        compress_even(pass2(f[0], pass)) | (compress_even(pass2(f[1], pass)) << 32);
                }
            }
            _ => {
                for (o, f) in out.iter_mut().zip(fields.chunks_exact(4)) {
                    let mut w = 0u64;
                    for (j, &x) in f.iter().enumerate() {
                        for k in 0..16 {
                            let value = (x >> (4 * k)) & 15;
                            w |= u64::from((pass >> value) & 1) << (16 * j + k);
                        }
                    }
                    *o = w;
                }
            }
        }
    }

    /// A word's members' pass bits deposited at once ([`super::expand`]): no
    /// branch on the candidates.
    pub fn deposit_pass(
        words: &mut [u64],
        members: &[u8],
        pass: &[u64],
        lo: u32,
        absent: u64,
    ) -> u64 {
        use super::{expand, member_word, pass_window};
        let (mut run, mut dropped) = (0u32, 0u64);
        for (w, word) in words.iter_mut().enumerate() {
            let m = member_word(members, w);
            let c = *word;
            if c != 0 {
                let p = if c & m != 0 {
                    pass_window(pass, run - lo)
                } else {
                    0
                };
                let (e, n) = expand(p, m);
                let out = (c & !m & absent) | (e & c);
                dropped += u64::from((c & !out).count_ones());
                *word = out;
                run += n;
            } else {
                run += m.count_ones();
            }
        }
        dropped
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

    /// 4-bit fields by `tbl`, 64 a word: each byte's two fields looked up,
    /// interleaved back in order, weighted by their bit in the byte and
    /// summed pairwise into the word.
    pub fn pass_bits(fields: &[u64], width: u32, pass: u16, out: &mut [u64]) {
        if width != 4 {
            return super::scalar::pass_bits(fields, width, pass, out);
        }
        const WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        let table = super::pass_table(pass, 0xff);
        // SAFETY: NEON is baseline on aarch64; each step loads the four
        // words of its chunk (32 bytes).
        unsafe {
            use core::arch::aarch64::*;
            let lut = vld1q_u8(table.as_ptr());
            let weights = vld1q_u8(WEIGHTS.as_ptr());
            let low = vdupq_n_u8(15);
            for (o, f) in out.iter_mut().zip(fields.chunks_exact(4)) {
                let p = f.as_ptr().cast::<u8>();
                let (a, b) = (vld1q_u8(p), vld1q_u8(p.add(16)));
                let (al, ah) = (
                    vqtbl1q_u8(lut, vandq_u8(a, low)),
                    vqtbl1q_u8(lut, vshrq_n_u8::<4>(a)),
                );
                let (bl, bh) = (
                    vqtbl1q_u8(lut, vandq_u8(b, low)),
                    vqtbl1q_u8(lut, vshrq_n_u8::<4>(b)),
                );
                let z0 = vandq_u8(vzip1q_u8(al, ah), weights);
                let z1 = vandq_u8(vzip2q_u8(al, ah), weights);
                let z2 = vandq_u8(vzip1q_u8(bl, bh), weights);
                let z3 = vandq_u8(vzip2q_u8(bl, bh), weights);
                let s = vpaddq_u8(vpaddq_u8(z0, z1), vpaddq_u8(z2, z3));
                let s = vpaddq_u8(s, s);
                *o = vgetq_lane_u64::<0>(vreinterpretq_u64_u8(s));
            }
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

    /// 4-bit fields by `pshufb`, 32 a 16-byte step: each byte's two fields
    /// looked up, interleaved back in order, their high bits gathered by
    /// `pmovmskb`. 2-bit fields compacted by PEXT where it is fast.
    #[target_feature(enable = "avx2,popcnt")]
    pub fn pass_bits(fields: &[u64], width: u32, pass: u16, out: &mut [u64]) {
        if width == 2 && super::fast_pdep() {
            // SAFETY: BMI2 is the CPU's (`fast_pdep`).
            return unsafe { super::bmi2::pass_bits2(fields, pass, out) };
        }
        if width != 4 {
            return scalar::pass_bits(fields, width, pass, out);
        }
        let table = super::pass_table(pass, 0x80);
        // SAFETY: each step loads two of its chunk's four words (16 bytes).
        unsafe {
            let lut = _mm_loadu_si128(table.as_ptr().cast());
            let low = _mm_set1_epi8(0x0f);
            for (o, f) in out.iter_mut().zip(fields.chunks_exact(4)) {
                let mut w = 0u64;
                for h in 0..2 {
                    let v = _mm_loadu_si128(f.as_ptr().add(2 * h).cast());
                    let lo = _mm_shuffle_epi8(lut, _mm_and_si128(v, low));
                    let hi = _mm_shuffle_epi8(lut, _mm_and_si128(_mm_srli_epi16::<4>(v), low));
                    let a = _mm_movemask_epi8(_mm_unpacklo_epi8(lo, hi)) as u32 as u64;
                    let b = _mm_movemask_epi8(_mm_unpackhi_epi8(lo, hi)) as u32 as u64;
                    w |= (a | (b << 16)) << (32 * h);
                }
                *o = w;
            }
        }
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

    /// [`super::set_bits`] of whole little-endian words (Langdale; Lemire):
    /// a word's set bits' positions by one `vpcompressb` of the bytes
    /// 0..63 under it, widened sixteen at a time onto the word's start,
    /// sixteen stored, and more only for a word with more members.
    #[target_feature(enable = "avx512f,avx512bw,avx512vbmi,avx512vbmi2,avx512vpopcntdq,popcnt")]
    pub fn set_bits(bytes: &[u8], out: &mut Vec<u32>) {
        let total = count::<false>(bytes, bytes, bytes.len()) as usize;
        // Lanes past a word's members are stored, then overwritten by the
        // next word's or left past the length.
        out.reserve(total + 64);
        let start = out.len();
        // SAFETY: each word stores from its first member's place at most
        // 64 lanes, within the `total + 64` reserved; the loads are of whole
        // words of `bytes`; the `total` from `start` are then written.
        unsafe {
            let iota = _mm512_loadu_si512(IOTA.as_ptr().cast());
            let mut d = out.as_mut_ptr().add(start);
            for (w, c) in bytes.chunks_exact(8).enumerate() {
                let word = u64::from_le_bytes(c.try_into().expect("eight bytes"));
                let n = word.count_ones() as usize;
                let pos = _mm512_maskz_compress_epi8(word, iota);
                let base = _mm512_set1_epi32((w * 64) as i32);
                let widen = |x: __m128i| _mm512_add_epi32(_mm512_cvtepu8_epi32(x), base);
                _mm512_storeu_si512(d.cast(), widen(_mm512_castsi512_si128(pos)));
                if n > 16 {
                    _mm512_storeu_si512(
                        d.add(16).cast(),
                        widen(_mm512_extracti32x4_epi32::<1>(pos)),
                    );
                    _mm512_storeu_si512(
                        d.add(32).cast(),
                        widen(_mm512_extracti32x4_epi32::<2>(pos)),
                    );
                    _mm512_storeu_si512(
                        d.add(48).cast(),
                        widen(_mm512_extracti32x4_epi32::<3>(pos)),
                    );
                }
                d = d.add(n);
            }
            out.set_len(start + total);
        }
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
        fn set_bits_agree(seed in any::<u64>(), d in density(), n in length(), at in 0usize..64) {
            let buf = bytes(seed, d, n + at);
            let b = &buf[at..];
            let wbuf = words(seed ^ 11, d, n / 8 + 8);
            let w = &wbuf[at % 8..at % 8 + n / 8];
            let mut want = vec![7];
            for (i, word) in w.iter().enumerate() {
                for bit in 0..64 {
                    if word >> bit & 1 == 1 {
                        want.push((i * 64 + bit) as u32);
                    }
                }
            }
            let mut want_bytes = vec![7];
            scalar::set_bits_bytes(b, &mut want_bytes);
            let mut check = vec![7];
            for (i, c) in b.chunks_exact(8).enumerate() {
                for bit in 0..64 {
                    if le(c) >> bit & 1 == 1 {
                        check.push((i * 64 + bit) as u32);
                    }
                }
            }
            prop_assert_eq!(&want_bytes, &check);
            for level in levels() {
                let mut have = vec![7];
                set_bits_at(level, w, &mut have);
                prop_assert_eq!(&have, &want, "{:?}", level);
                let mut have = vec![7];
                set_bits_bytes_at(level, b, &mut have);
                prop_assert_eq!(&have, &want_bytes, "bytes {:?}", level);
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

        #[test]
        fn gathered_bits_are_the_run(seed in any::<u64>(), n_bytes in 0usize..80, bit in 0usize..700, n in 0usize..400) {
            let b = bytes(seed, 128, n_bytes);
            let mut have = vec![7];
            gather_bits(&b, bit, n, &mut have);
            prop_assert_eq!(have.len(), 1 + n.div_ceil(64));
            for i in 0..n.div_ceil(64) * 64 {
                let at = bit + i;
                let want = i < n && b.get(at / 8).is_some_and(|x| x >> (at % 8) & 1 == 1);
                prop_assert_eq!(have[1 + i / 64] >> (i % 64) & 1 == 1, want, "bit {}", i);
            }
        }

        #[test]
        fn pass_bits_agree(seed in any::<u64>(), w in 0usize..4, pass in any::<u16>(), n in 0usize..9) {
            let width = [0u32, 1, 2, 4][w];
            let fields = words(seed, 128, n * width as usize);
            let mut want = vec![0u64; n];
            for (j, o) in want.iter_mut().enumerate() {
                for k in 0..64 {
                    let at = (j * 64 + k) * width as usize;
                    let value = if width == 0 { 0 } else { (fields[at / 64] >> (at % 64)) & ((1 << width) - 1) };
                    *o |= u64::from((pass >> value) & 1) << k;
                }
            }
            for level in levels() {
                let mut have = vec![!0u64; n];
                pass_bits_at(level, &fields, width, pass, &mut have);
                prop_assert_eq!(&have, &want, "{:?} width {}", level, width);
            }
        }

        #[test]
        fn deposited_pass_bits_are_each_candidates(
            seed in any::<u64>(),
            dm in density(),
            dc in density(),
            n in 0usize..40,
            lo_word in 0usize..40,
            absent in any::<bool>(),
        ) {
            let members = bytes(seed, dm, n * 8 + (seed % 7) as usize);
            let mut cands = words(seed ^ 0x9e37, dc, n);
            let lo_word = lo_word.min(n);
            // No candidate member before word `lo_word`: `lo` its rank.
            let m = |w: usize| member_word(&members, w);
            for (w, c) in cands.iter_mut().enumerate().take(lo_word) {
                *c &= !m(w);
            }
            let lo: u32 = (0..lo_word).map(|w| m(w).count_ones()).sum();
            let total: u32 = (0..n).map(|w| m(w).count_ones()).sum();
            let pass: Vec<u64> = words(seed ^ 0x51ed, 128, ((total - lo) as usize).div_ceil(64) + 1);
            let absent = if absent { !0 } else { 0 };
            // `STANNUM_EXPECT_PDEP=1`: the PDEP body must be among those run.
            #[cfg(target_arch = "x86_64")]
            if std::env::var("STANNUM_EXPECT_PDEP").is_ok_and(|v| v == "1") {
                prop_assert!(fast_pdep(), "PDEP expected fast here");
            }
            // Per candidate, as the sieve read it: its rank's bit.
            let mut want = cands.clone();
            let mut dropped = 0u64;
            let mut run = 0u32;
            for (w, c) in want.iter_mut().enumerate() {
                let mw = m(w);
                let mut out = 0u64;
                for bit in 0..64 {
                    if *c >> bit & 1 == 0 {
                        continue;
                    }
                    let keep = if mw >> bit & 1 == 1 {
                        let r = run + (mw & ((1u64 << bit) - 1)).count_ones() - lo;
                        pass[r as usize / 64] >> (r % 64) & 1 == 1
                    } else {
                        absent != 0
                    };
                    if keep {
                        out |= 1 << bit;
                    } else {
                        dropped += 1;
                    }
                }
                *c = out;
                run += mw.count_ones();
            }
            for level in levels() {
                let mut have = cands.clone();
                let d = deposit_pass_at(level, &mut have, &members, &pass, lo, absent);
                prop_assert_eq!(&have, &want, "{:?}", level);
                prop_assert_eq!(d, dropped, "{:?}", level);
                let mut have = cands.clone();
                let d = scalar::deposit_pass(&mut have, &members, &pass, lo, absent);
                prop_assert_eq!(&have, &want, "portable");
                prop_assert_eq!(d, dropped);
            }
        }
    }
}
