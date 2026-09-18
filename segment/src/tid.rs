//! Heap tuple locations.

use crate::{Error, Result};

/// `MaxHeapTuplesPerPage` for 8 KiB pages: `(8192 - 24) / (4 + 24)`.
pub const MAX_OFFSET: u16 = 291;
/// `InvalidBlockNumber` is reserved by PostgreSQL.
pub const MAX_BLOCK: u32 = u32::MAX - 1;

/// A heap tuple location. Ordering is heap order: block, then offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tid {
    pub block: u32,
    pub offset: u16,
}

impl Tid {
    pub const fn new(block: u32, offset: u16) -> Result<Self> {
        if block > MAX_BLOCK || offset == 0 || offset > MAX_OFFSET {
            return Err(Error::InvalidTid);
        }
        Ok(Self { block, offset })
    }

    /// The 256-block group this location belongs to.
    pub const fn group(self) -> u32 {
        self.block >> 8
    }

    /// Position of this block within its group.
    pub const fn page_bit(self) -> u16 {
        (self.block & 0xff) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_bounds_and_orders_by_heap_position() {
        assert!(Tid::new(0, 1).is_ok());
        assert!(Tid::new(MAX_BLOCK, MAX_OFFSET).is_ok());
        assert_eq!(Tid::new(0, 0), Err(Error::InvalidTid));
        assert_eq!(Tid::new(0, MAX_OFFSET + 1), Err(Error::InvalidTid));
        assert_eq!(Tid::new(u32::MAX, 1), Err(Error::InvalidTid));
        assert!(Tid::new(1, 1).unwrap() > Tid::new(0, 291).unwrap());
        assert_eq!(Tid::new(513, 7).unwrap().group(), 2);
        assert_eq!(Tid::new(513, 7).unwrap().page_bit(), 1);
    }
}
