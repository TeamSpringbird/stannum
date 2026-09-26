// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Bounds-checked sequential reads over a byte slice.

use crate::{Error, Result, varint};

#[derive(Clone, Copy, Debug)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    pub const fn at(bytes: &'a [u8], at: usize) -> Self {
        Self { bytes, at }
    }

    pub const fn position(&self) -> usize {
        self.at
    }

    pub const fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }

    pub fn varint(&mut self) -> Result<u64> {
        varint::get(self.bytes, &mut self.at)
    }

    pub fn varint_u32(&mut self) -> Result<u32> {
        varint::get_u32(self.bytes, &mut self.at)
    }

    pub fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(len).ok_or(Error::Truncated)?;
        let slice = self.bytes.get(self.at..end).ok_or(Error::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    pub fn skip(&mut self, len: usize) -> Result<()> {
        self.take(len).map(|_| ())
    }

    /// Moves to an absolute position that must not exceed the end of input.
    pub fn seek(&mut self, at: usize) -> Result<()> {
        if at > self.bytes.len() {
            return Err(Error::Truncated);
        }
        self.at = at;
        Ok(())
    }
}
