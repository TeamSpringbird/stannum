// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! The instruction set the query path's scalar code runs: the walks, the
//! count fold and the decoders under them (Elias-Fano, bit unpacking,
//! footers, TF tails, the DL sidecar, positions).
//!
//! A baseline x86-64 build (as PGDG packages are) compiles `count_ones`,
//! `trailing_zeros`, `leading_zeros` and variable shifts to SSE2-era
//! sequences. So on x86-64 each hot function marked [`hot`] is compiled
//! twice: a portable copy and one with x86-64-v3's integer extensions and
//! AVX2 (POPCNT, BMI1, BMI2, LZCNT, AVX2; not FMA, so no float result can
//! change). The function keeps its name and attributes and calls one of
//! the two copies by [`is_v3`]: one load and a predictable branch per call,
//! and none inside a copy's loops. A copy calls the functions it does not
//! inline through their own dispatch, so a query runs v3 from its entry
//! down to its leaves; a v3 copy inlines a hot leaf's v3 copy where it
//! would have inlined the leaf (their features match).
//!
//! - aarch64 and other targets: [`hot`] leaves the function as written.
//! - x86-64: v3 when the CPU has every feature ([`Isa::v3`]), chosen once
//!   per process. `STANNUM_KERNELS=scalar` (or `generic`) forces the
//!   portable copies, as it forces the portable word kernels; any other
//!   value runs v3 where supported.
//!
//! The copies differ only in instruction selection: float operations keep
//! their order (Rust never contracts them; FMA is not enabled) and the
//! walks' answers are compared bit for bit across levels by the tests.

/// `#[hot]`: compiles a function twice on x86-64 (see the module
/// documentation). Needs this module in scope as `isa`.
pub use isa_macro::hot;

/// The target features of the v3 copies.
pub const V3_FEATURES: &str = "popcnt,bmi1,bmi2,lzcnt,avx2";

/// An instruction set the hot copies are compiled for. Only levels the CPU
/// supports can be constructed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Isa(Kind);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    Generic,
    #[cfg(target_arch = "x86_64")]
    V3,
}

impl Isa {
    /// The portable copies (compiled for the build's target CPU).
    pub const GENERIC: Isa = Isa(Kind::Generic);

    /// The x86-64-v3 copies, if this CPU runs them.
    pub fn v3() -> Option<Isa> {
        #[cfg(target_arch = "x86_64")]
        {
            let has = std::arch::is_x86_feature_detected!("popcnt")
                && std::arch::is_x86_feature_detected!("bmi1")
                && std::arch::is_x86_feature_detected!("bmi2")
                && std::arch::is_x86_feature_detected!("lzcnt")
                && std::arch::is_x86_feature_detected!("avx2");
            if has {
                return Some(Isa(Kind::V3));
            }
        }
        None
    }

    pub fn name(self) -> &'static str {
        match self.0 {
            Kind::Generic => "generic",
            #[cfg(target_arch = "x86_64")]
            Kind::V3 => "v3",
        }
    }

    /// The levels this CPU runs, the portable one first.
    pub fn supported() -> Vec<Isa> {
        std::iter::once(Isa::GENERIC).chain(Isa::v3()).collect()
    }

    /// The portable copies if `STANNUM_KERNELS` is `scalar` or `generic`,
    /// else the best level the CPU runs.
    pub fn chosen() -> Isa {
        let best = Isa::v3().unwrap_or(Isa::GENERIC);
        match std::env::var("STANNUM_KERNELS") {
            Ok(v) if matches!(v.trim().to_ascii_lowercase().as_str(), "scalar" | "generic") => {
                Isa::GENERIC
            }
            _ => best,
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod chosen {
    use super::{Isa, Kind};
    use std::sync::atomic::{AtomicU8, Ordering};

    /// 0 until chosen; then 1 (generic) or 2 (v3).
    static CHOSEN: AtomicU8 = AtomicU8::new(0);

    #[inline(always)]
    pub fn current() -> Isa {
        match CHOSEN.load(Ordering::Relaxed) {
            2 => Isa(Kind::V3),
            1 => Isa::GENERIC,
            _ => choose(),
        }
    }

    #[cold]
    #[inline(never)]
    fn choose() -> Isa {
        let isa = Isa::chosen();
        set(isa);
        isa
    }

    pub fn set(isa: Isa) {
        let code = match isa.0 {
            Kind::Generic => 1,
            Kind::V3 => 2,
        };
        CHOSEN.store(code, Ordering::Relaxed);
    }
}

/// The level the hot copies run: chosen once, by the CPU and
/// `STANNUM_KERNELS`.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub fn current() -> Isa {
    chosen::current()
}

/// The level the hot copies run: the only one there is.
#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
pub fn current() -> Isa {
    Isa::GENERIC
}

/// Whether the hot functions run their v3 copies: true only when the CPU
/// has every feature of [`V3_FEATURES`].
#[inline(always)]
pub fn is_v3() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        current().0 == Kind::V3
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Runs `f` with the hot functions at `isa`, then restores the level: for
/// tests and comparisons that run one query at each level. Calls are
/// serialized; anything else running meanwhile runs at `isa` too (every
/// level computes the same results).
#[doc(hidden)]
pub fn with<R>(isa: Isa, f: impl FnOnce() -> R) -> R {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    struct Restore(Isa);
    impl Drop for Restore {
        fn drop(&mut self) {
            set(self.0);
        }
    }
    let _restore = Restore(current());
    set(isa);
    f()
}

#[cfg(target_arch = "x86_64")]
fn set(isa: Isa) {
    chosen::set(isa);
}

#[cfg(not(target_arch = "x86_64"))]
fn set(_: Isa) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tinshape::isa;

    /// `STANNUM_EXPECT_KERNELS` naming `avx2`, `avx512` or `v3` makes a CPU
    /// without v3 a failure rather than a skip.
    #[test]
    fn reports_levels() {
        let have = Isa::supported();
        eprintln!(
            "hot copy levels here: {:?}; queries run {:?}",
            have.iter().map(|l| l.name()).collect::<Vec<_>>(),
            current().name()
        );
        if let Ok(want) = std::env::var("STANNUM_EXPECT_KERNELS") {
            let wants_v3 = want
                .split(',')
                .any(|w| matches!(w.trim(), "avx2" | "avx512" | "v3"));
            assert!(!wants_v3 || Isa::v3().is_some(), "v3 not supported here");
        }
        assert!(have.contains(&current()));
    }

    #[test]
    fn with_runs_at_a_level_and_restores() {
        for isa in Isa::supported() {
            assert_eq!(
                with(isa, || (current(), is_v3())),
                (isa, isa != Isa::GENERIC)
            );
        }
    }

    #[isa::hot]
    #[inline(never)]
    fn ops(x: u64) -> (u32, u32, u32, u64, u64, u32) {
        (
            x.count_ones(),
            x.trailing_zeros(),
            x.leading_zeros(),
            x & x.wrapping_sub(1),
            x >> (x & 63),
            (x as u32).leading_zeros(),
        )
    }

    struct Bits(u64);

    impl Bits {
        /// A method with a receiver, a pattern and an `impl Trait`.
        #[isa::hot]
        fn sum(&mut self, (a, b): (u32, u32), mut more: impl Iterator<Item = u32>) -> u64 {
            let first = more.next().unwrap_or(0);
            self.0 += u64::from(a.count_ones() + b.trailing_zeros() + first);
            self.0
        }
    }

    /// The bit operations the v3 copies compile to instructions agree with
    /// the portable ones; methods, patterns and `impl Trait` dispatch.
    #[test]
    fn bit_operations_agree() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..10_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let mut answers = Isa::supported()
                .into_iter()
                .map(|isa| with(isa, || ops(std::hint::black_box(x))));
            let first = answers.next().unwrap();
            assert!(answers.all(|a| a == first));
        }
        let sums: Vec<u64> = Isa::supported()
            .into_iter()
            .map(|isa| with(isa, || Bits(1).sum((7, 8), [3].into_iter())))
            .collect();
        assert!(sums.iter().all(|s| *s == 1 + 3 + 3 + 3));
    }
}
