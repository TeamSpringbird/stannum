// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Little-endian bit packing: value `i` of width `w` occupies bits
//! `i * w .. (i + 1) * w` of the byte string, least significant bit first.

use crate::{Error, Result};

/// Appends fixed-width values to a byte string.
#[derive(Default)]
pub struct BitWriter {
    pub bytes: Vec<u8>,
    acc: u64,
    filled: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends the low `width` bits of `value`; `width` at most 32.
    pub fn put(&mut self, value: u32, width: u32) {
        debug_assert!(width <= 32);
        if width == 0 {
            return;
        }
        debug_assert!(width == 32 || value >> width == 0);
        self.acc |= u64::from(value) << self.filled;
        self.filled += width;
        while self.filled >= 8 {
            self.bytes.push(self.acc as u8);
            self.acc >>= 8;
            self.filled -= 8;
        }
    }

    /// Sets bit `at` (relative to where this writer started), growing the
    /// string as needed; only for writers that use [`Self::set`] alone.
    pub fn set(&mut self, at: usize) {
        let byte = at / 8;
        if self.bytes.len() <= byte {
            self.bytes.resize(byte + 1, 0);
        }
        self.bytes[byte] |= 1 << (at % 8);
    }

    /// The bytes, the last one padded with zero bits.
    pub fn finish(mut self) -> Vec<u8> {
        if self.filled > 0 {
            self.bytes.push(self.acc as u8);
        }
        self.bytes
    }
}

/// Bytes needed for `count` values of `width` bits.
pub const fn packed_len(count: usize, width: u32) -> usize {
    (count * width as usize).div_ceil(8)
}

/// Value `index` of width `width` (at most 32) from a packed string.
#[inline]
pub fn get(bytes: &[u8], index: usize, width: u32) -> Result<u32> {
    if width == 0 {
        return Ok(0);
    }
    get_at(bytes, index * width as usize, width)
}

/// The `width`-bit value at bit `bit` of `bytes`.
#[inline]
pub fn get_at(bytes: &[u8], bit: usize, width: u32) -> Result<u32> {
    if width == 0 {
        return Ok(0);
    }
    let first = bit / 8;
    // One unaligned load where eight bytes remain: a value of at most 32
    // bits at a bit offset below 8 fits in them.
    if let Some(chunk) = bytes.get(first..first + 8) {
        let value = u64::from_le_bytes(chunk.try_into().expect("eight bytes")) >> (bit % 8);
        return Ok((value & ((1u64 << width) - 1)) as u32);
    }
    let last = (bit + width as usize - 1) / 8;
    if last >= bytes.len() {
        return Err(Error::Truncated);
    }
    // Fewer than eight bytes remain: assembled a byte at a time (a slice
    // copy here is a call to memcpy).
    let mut word = 0u64;
    for (i, b) in bytes[first..=last].iter().enumerate() {
        word |= u64::from(*b) << (8 * i);
    }
    let value = word >> (bit % 8);
    Ok((value & ((1u64 << width) - 1)) as u32)
}

/// The little-endian word `i` of a byte string, zero past its end.
#[inline]
pub fn word(bytes: &[u8], i: usize) -> u64 {
    let at = i * 8;
    if let Some(chunk) = bytes.get(at..at + 8) {
        u64::from_le_bytes(chunk.try_into().expect("eight bytes"))
    } else if at < bytes.len() {
        let mut word = 0u64;
        for (i, b) in bytes[at..].iter().enumerate() {
            word |= u64::from(*b) << (8 * i);
        }
        word
    } else {
        0
    }
}

/// Bits needed to hold values below `bound` (at least one).
pub const fn width_below(bound: u32) -> u32 {
    if bound <= 2 {
        1
    } else {
        32 - (bound - 1).leading_zeros()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn widths() {
        assert_eq!(width_below(1), 1);
        assert_eq!(width_below(2), 1);
        assert_eq!(width_below(3), 2);
        assert_eq!(width_below(256), 8);
        assert_eq!(width_below(257), 9);
        assert_eq!(width_below(291), 9);
    }

    proptest! {
        #[test]
        fn round_trips(width in 0u32..=32, values in prop::collection::vec(any::<u32>(), 0..200)) {
            let mask = if width == 32 { u32::MAX } else { (1u32 << width) - 1 };
            let values: Vec<u32> = values.into_iter().map(|v| v & mask).collect();
            let mut writer = BitWriter::new();
            for v in &values {
                writer.put(*v, width);
            }
            let bytes = writer.finish();
            prop_assert_eq!(bytes.len(), packed_len(values.len(), width));
            for (i, v) in values.iter().enumerate() {
                prop_assert_eq!(get(&bytes, i, width).unwrap(), *v);
            }
        }
    }
}
