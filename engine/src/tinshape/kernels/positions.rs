// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A positions entry decoded ([`segment::tinshape::positions`]): its gaps
//! unpacked by the Elias-Fano lists' low-part bodies ([`super::unpack`]),
//! eight or sixteen at a time, then summed onto its first position. An
//! entry of few positions (most are) is read by the format's own decoder.

use super::{Level, UNPACK_ROOM, level, unpack_at};
use segment::tinshape::bits::packed_len;
use segment::tinshape::positions::Entry;
use segment::{Error, Result};

/// Entries with fewer gaps than this are read a gap at a time.
pub const SHORT: usize = 8;

/// `entry`'s positions into `out`, from `gaps`: the bytes from
/// [`Entry::gap_bytes`]'s start, at least to its end (more let more values
/// be read by vector loads).
#[inline]
pub fn positions_decode(entry: &Entry, gaps: &[u8], out: &mut Vec<u32>) -> Result<()> {
    positions_decode_at(level(), entry, gaps, out)
}

/// [`positions_decode`] at `level`; panics if the CPU lacks it.
pub fn positions_decode_at(
    level: Level,
    entry: &Entry,
    gaps: &[u8],
    out: &mut Vec<u32>,
) -> Result<()> {
    let n = entry.count as usize;
    if n - 1 < SHORT {
        return entry.decode(gaps, out);
    }
    let lead = entry.lead();
    let values = lead + n - 1;
    if packed_len(values, entry.width) > gaps.len() {
        return Err(Error::Truncated);
    }
    // Value `v` of the run unpacked into `out[1 + v]`: the sum then reads
    // `out[lead + j]` before writing `out[j]` (`lead + j >= j`).
    out.clear();
    out.resize(1 + values + UNPACK_ROOM, 0);
    unpack_at(level, gaps, entry.width, values, &mut out[1..]);
    out[0] = entry.first;
    for j in 1..n {
        out[j] = out[j - 1].wrapping_add(out[lead + j]).wrapping_add(1);
    }
    out.truncate(n);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use segment::payload::PayloadBuilder;
    use segment::tinshape::positions::{Positions, encode};

    /// Every level the CPU runs, each entry against the format's decoder.
    fn check(entries: &[Vec<u32>]) {
        let mut b = PayloadBuilder::default();
        entries.iter().for_each(|e| b.push(e).unwrap());
        let encoded = encode(&b.finish()).unwrap();
        let positions = Positions::parse(&encoded[..]).unwrap();
        let (mut want, mut got) = (Vec::new(), Vec::new());
        for level in Level::ALL.into_iter().filter(|l| l.supported()) {
            for index in 0..entries.len() as u32 {
                let block = positions.block_of(index).unwrap();
                let entry = positions.entry(&block, index).unwrap();
                let (from, to) = entry.gap_bytes();
                let exact = &encoded[from..to];
                entry.decode(exact, &mut want).unwrap();
                assert_eq!(want, entries[index as usize]);
                // The exact bytes, and with the stream's bytes after them.
                for gaps in [exact, &encoded[from..]] {
                    positions_decode_at(level, &entry, gaps, &mut got).unwrap();
                    assert_eq!(got, want, "{} entry {index}", level.name());
                }
            }
        }
    }

    #[test]
    fn every_gap_width() {
        for width in 0..=32u32 {
            let step = if width == 0 { 1 } else { 1u64 << (width - 1) };
            let entries: Vec<Vec<u32>> = (0..40u64)
                .map(|i| {
                    let n = (i % 37 + 1) as usize;
                    let mut p = i;
                    let mut e = vec![p as u32];
                    for k in 1..n {
                        let next = p + 1 + (step - 1).min(k as u64 * 7919 % step.max(1));
                        if next > u64::from(u32::MAX) {
                            break;
                        }
                        p = next;
                        e.push(p as u32);
                    }
                    e
                })
                .collect();
            check(&entries);
        }
    }

    proptest! {
        #[test]
        fn levels_agree(
            entries in proptest::collection::vec(
                (0u32..10_000, 1usize..90, 1u32..1 << 20).prop_map(|(first, n, step)| {
                    let mut p = first;
                    let mut e = vec![p];
                    for k in 1..n as u32 {
                        p += 1 + k.wrapping_mul(2_654_435_761) % step;
                        e.push(p);
                    }
                    e
                }),
                1..100,
            )
        ) {
            check(&entries);
        }
    }
}
