// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Weighted sums over the 64 lanes of a word, bit-sliced: each lane is a
//! member of a bitmap word, and a weight is added to every lane a mask
//! selects in a few word operations rather than one lane at a time.

/// Bits per lane counter: counts reach at most [`LaneSums::MAX_TARGET`].
/// Six rather than eight measured a few percent faster over long
/// disjunctions, for a coarser unit that let through a few more members.
const SLICES: usize = 6;

/// Per lane of a word, a running sum of small integer weights, and which
/// lanes have reached a target.
///
/// Each counter starts at `2^SLICES - target` plus the starting sum, so a
/// lane reaches the target exactly when its counter carries out of the top
/// slice; the carry is kept, so a lane that has reached the target stays
/// reached however far past it its counter wraps. The sums saturate at the
/// target and never lose a lane that reached it.
#[derive(Clone, Copy, Debug)]
pub struct LaneSums {
    slices: [u64; SLICES],
    reached: u64,
}

impl LaneSums {
    /// The largest target a counter holds.
    pub const MAX_TARGET: u32 = (1 << SLICES) - 1;

    /// Every lane at `start`, counting towards `target`, at most
    /// [`Self::MAX_TARGET`]. A start at or past the target has every lane
    /// reached.
    pub fn new(start: u32, target: u32) -> Self {
        assert!(
            (1..=Self::MAX_TARGET).contains(&target),
            "lane target {target} out of range"
        );
        if start >= target {
            return Self {
                slices: [0; SLICES],
                reached: !0,
            };
        }
        let init = (1u32 << SLICES) - target + start;
        let mut slices = [0; SLICES];
        for (j, slice) in slices.iter_mut().enumerate() {
            if init >> j & 1 != 0 {
                *slice = !0;
            }
        }
        Self { slices, reached: 0 }
    }

    /// Adds `weight`, at most [`Self::MAX_TARGET`], to the lanes of `mask`:
    /// a ripple-carry add over every slice, without branches. Stopping once
    /// the carry died, or skipping an empty mask, was slower: the branches
    /// mispredict on real words.
    #[inline]
    pub fn add(&mut self, mask: u64, weight: u32) {
        debug_assert!(weight <= Self::MAX_TARGET);
        let mut carry = 0u64;
        for (j, slice) in self.slices.iter_mut().enumerate() {
            let bits = mask & 0u64.wrapping_sub(u64::from(weight >> j & 1));
            let sum = *slice ^ bits;
            let next = (*slice & bits) | (carry & sum);
            *slice = sum ^ carry;
            carry = next;
        }
        self.reached |= carry;
    }

    /// The lanes whose sum has reached the target.
    #[inline]
    pub fn reached(&self) -> u64 {
        self.reached
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Random weights and masks against a per-lane sum, over targets from 1
    /// to the largest, with sums far past the target so counters wrap.
    #[test]
    fn matches_a_scalar_sum_per_lane() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for round in 0..20_000 {
            let target = 1 + rng.below(u64::from(LaneSums::MAX_TARGET)) as u32;
            let start = match round % 4 {
                0 => 0,
                1 => rng.below(u64::from(target)) as u32,
                _ => rng.below(u64::from(target) * 2) as u32,
            };
            // Small weights mostly, some at the cap, and enough adds to wrap.
            let adds = rng.below(40) as usize;
            let mut lanes = LaneSums::new(start, target);
            let mut sums = [u64::from(start); 64];
            for _ in 0..adds {
                let weight = match rng.below(4) {
                    0 => LaneSums::MAX_TARGET,
                    1 => rng.below(u64::from(LaneSums::MAX_TARGET) + 1) as u32,
                    _ => rng.below(u64::from(target).min(16) + 1) as u32,
                };
                let mask = match rng.below(3) {
                    0 => rng.next() & rng.next(),
                    1 => rng.next(),
                    _ => rng.next() | rng.next(),
                };
                lanes.add(mask, weight);
                for (lane, sum) in sums.iter_mut().enumerate() {
                    if mask >> lane & 1 != 0 {
                        *sum += u64::from(weight);
                    }
                }
                let expect = sums
                    .iter()
                    .enumerate()
                    .filter(|(_, sum)| **sum >= u64::from(target))
                    .fold(0u64, |acc, (lane, _)| acc | 1 << lane);
                assert_eq!(
                    lanes.reached(),
                    expect,
                    "round {round} target {target} start {start} sums {sums:?}"
                );
            }
        }
    }

    #[test]
    fn saturates_rather_than_losing_a_lane() {
        let mut lanes = LaneSums::new(0, LaneSums::MAX_TARGET);
        for _ in 0..1_000 {
            lanes.add(0b1010, LaneSums::MAX_TARGET);
            lanes.add(0b0110, 1);
        }
        assert_eq!(lanes.reached(), 0b1110);
        let full = LaneSums::new(5, 5);
        assert_eq!(full.reached(), !0);
        let mut one = LaneSums::new(0, 1);
        one.add(1 << 63, 0);
        assert_eq!(one.reached(), 0);
        one.add(1 << 63, 1);
        assert_eq!(one.reached(), 1 << 63);
    }
}
