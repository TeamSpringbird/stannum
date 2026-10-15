// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Unsigned LEB128 integers.

use crate::{Error, Result};

pub fn put(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

#[inline]
pub fn get(bytes: &[u8], at: &mut usize) -> Result<u64> {
    // Directories and footers are runs of varints, most of one or two
    // bytes, decoded per query: those take no loop and one bounds check.
    if let Some(pair) = bytes.get(*at..*at + 2) {
        let (b0, b1) = (pair[0], pair[1]);
        if b0 & 0x80 == 0 {
            *at += 1;
            return Ok(u64::from(b0));
        }
        if b1 & 0x80 == 0 {
            *at += 2;
            return Ok(u64::from(b0 & 0x7f) | (u64::from(b1) << 7));
        }
    }
    get_slow(bytes, at)
}

#[inline(never)]
fn get_slow(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*at).ok_or(Error::Truncated)?;
        *at += 1;
        if shift == 63 && byte > 1 {
            return Err(Error::Corrupt("varint exceeds 64 bits"));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift > 63 {
            return Err(Error::Corrupt("varint exceeds 64 bits"));
        }
    }
}

#[inline]
pub fn get_u32(bytes: &[u8], at: &mut usize) -> Result<u32> {
    u32::try_from(get(bytes, at)?).map_err(|_| Error::Corrupt("value exceeds 32 bits"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_boundaries() {
        for value in [0, 1, 127, 128, 16_383, 16_384, u32::MAX as u64, u64::MAX] {
            let mut out = Vec::new();
            put(&mut out, value);
            let mut at = 0;
            assert_eq!(get(&out, &mut at).unwrap(), value);
            assert_eq!(at, out.len());
        }
    }

    #[test]
    fn reads_runs_of_short_and_long_values_to_the_last_byte() {
        let values: Vec<u64> = (0..2000u64)
            .map(|i| match i % 5 {
                0 => i % 128,
                1 => 128 + i * 7,
                2 => 16_384 + i * 1_000,
                3 => u64::from(u32::MAX) - i,
                _ => i % 3,
            })
            .collect();
        let mut out = Vec::new();
        for &v in &values {
            put(&mut out, v);
        }
        let mut at = 0;
        for &v in &values {
            assert_eq!(get(&out, &mut at).unwrap(), v);
        }
        assert_eq!(at, out.len());
        assert_eq!(get(&out, &mut at), Err(Error::Truncated));
        // A two-byte value whose second byte is past the end.
        assert_eq!(get(&[0x81], &mut 0), Err(Error::Truncated));
        assert_eq!(get(&[0x05, 0x81], &mut 1), Err(Error::Truncated));
    }

    #[test]
    fn rejects_truncation_and_overflow() {
        assert_eq!(get(&[0x80], &mut 0), Err(Error::Truncated));
        assert_eq!(get(&[], &mut 0), Err(Error::Truncated));
        let too_long = [0xff; 11];
        assert!(matches!(get(&too_long, &mut 0), Err(Error::Corrupt(_))));
        assert!(matches!(
            get_u32(&[0x80, 0x80, 0x80, 0x80, 0x10], &mut 0),
            Err(Error::Corrupt(_))
        ));
    }
}
