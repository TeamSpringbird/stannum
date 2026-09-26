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

pub fn get(bytes: &[u8], at: &mut usize) -> Result<u64> {
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
