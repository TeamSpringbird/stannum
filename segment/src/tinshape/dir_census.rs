// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A lab census of the group directory bytes a query reads: per directory
//! parsed, the 64-byte lines of its copy that its parse checked and that
//! walks read afterwards, and the role the walk gave its term (what copying
//! a directory a line or a chunk at a time, as it is first read, would
//! leave uncopied).
//!
//! Built with the `dir-census` feature only (the bench's); without it every
//! hook is an empty inline function and [`take`] gives nothing.

/// A term's role in the walk that reads its directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    /// No walk named it (read by something else, or not read).
    Unset,
    /// The rarest required term: the walk steps its groups in order.
    Lead,
    /// Another required term: the walk asks it for the lead's groups.
    Probed,
    /// A scoring term not required (a disjunction's).
    Or,
}

/// What a query read of one parsed directory.
#[derive(Clone, Debug)]
pub struct Read {
    pub role: Role,
    /// The directory's bytes (what the parse copies).
    pub len: usize,
    /// Its 64-byte lines the parse read, and those read after it, a bit a
    /// line.
    pub parse: Vec<u64>,
    pub walk: Vec<u64>,
    /// Bytes of its header, presence and bases; of its rows; of its
    /// frontiers.
    pub sections: [usize; 3],
    /// Its 32-row blocks (see `postings::DIRECTORY_BLOCK`), and those whose
    /// rows or frontiers the walk read, a bit a block.
    pub blocks: usize,
    pub blocks_read: Vec<u64>,
    /// Where it lies in the segment's blob, when read from one.
    pub offset: Option<usize>,
}

#[cfg(feature = "dir-census")]
mod on {
    use super::{Read, Role};
    use std::cell::{Cell, RefCell};

    thread_local! {
        static ON: Cell<bool> = const { Cell::new(false) };
        static PARSING: Cell<usize> = const { Cell::new(0) };
        static DIRS: RefCell<rustc_hash::FxHashMap<usize, Read>> =
            RefCell::new(rustc_hash::FxHashMap::default());
        static DONE: RefCell<Vec<Read>> = const { RefCell::new(Vec::new()) };
    }

    pub fn start() {
        ON.set(true);
        DIRS.with_borrow_mut(|d| d.clear());
        DONE.with_borrow_mut(|d| d.clear());
    }

    pub fn take() -> Vec<Read> {
        ON.set(false);
        let mut out = DONE.take();
        out.extend(DIRS.with_borrow_mut(|d| d.drain().map(|(_, r)| r).collect::<Vec<_>>()));
        out
    }

    fn set(bits: &mut [u64], from: usize, to: usize) {
        for line in from / 64..to.div_ceil(64) {
            if let Some(w) = bits.get_mut(line / 64) {
                *w |= 1 << (line % 64);
            }
        }
    }

    pub fn parse_begin(
        id: usize,
        len: usize,
        sections: [usize; 3],
        blocks: usize,
        offset: Option<usize>,
    ) {
        if !ON.get() {
            return;
        }
        let words = len.div_ceil(64).div_ceil(64);
        let read = Read {
            role: Role::Unset,
            len,
            parse: vec![0; words],
            walk: vec![0; words],
            sections,
            blocks,
            blocks_read: vec![0; blocks.div_ceil(64)],
            offset,
        };
        DIRS.with_borrow_mut(|d| {
            // A directory dropped and its memory reused: the old one's
            // reads are kept apart.
            if let Some(old) = d.insert(id, read) {
                DONE.with_borrow_mut(|done| done.push(old));
            }
        });
        PARSING.set(id);
    }

    pub fn parse_end() {
        PARSING.set(0);
    }

    pub fn touch(id: usize, at: usize, n: usize) {
        if !ON.get() {
            return;
        }
        let parsing = PARSING.get() == id;
        DIRS.with_borrow_mut(|d| {
            if let Some(r) = d.get_mut(&id) {
                let to = (at + n).min(r.len);
                let bits = if parsing { &mut r.parse } else { &mut r.walk };
                set(bits, at.min(to), to);
            }
        });
    }

    pub fn block(id: usize, block: usize) {
        if !ON.get() {
            return;
        }
        DIRS.with_borrow_mut(|d| {
            if let Some(w) = d
                .get_mut(&id)
                .and_then(|r| r.blocks_read.get_mut(block / 64))
            {
                *w |= 1 << (block % 64);
            }
        });
    }

    pub fn role(id: usize, role: Role) {
        if !ON.get() {
            return;
        }
        DIRS.with_borrow_mut(|d| {
            if let Some(r) = d.get_mut(&id) {
                // A term both led and probed (two walks) counts as led.
                if r.role == Role::Unset || role == Role::Lead {
                    r.role = role;
                }
            }
        });
    }
}

#[cfg(feature = "dir-census")]
pub use on::{start, take};

#[cfg(feature = "dir-census")]
pub(crate) use on::{block, parse_begin, parse_end, role, touch};

#[cfg(not(feature = "dir-census"))]
mod off {
    use super::{Read, Role};

    #[inline(always)]
    pub fn start() {}

    #[inline(always)]
    pub fn take() -> Vec<Read> {
        Vec::new()
    }

    #[inline(always)]
    pub fn parse_begin(
        _id: usize,
        _len: usize,
        _sections: [usize; 3],
        _blocks: usize,
        _offset: Option<usize>,
    ) {
    }

    #[inline(always)]
    pub fn block(_id: usize, _block: usize) {}

    #[inline(always)]
    pub fn parse_end() {}

    #[inline(always)]
    pub fn touch(_id: usize, _at: usize, _n: usize) {}

    #[inline(always)]
    pub fn role(_id: usize, _role: Role) {}
}

#[cfg(not(feature = "dir-census"))]
pub use off::{start, take};

#[cfg(not(feature = "dir-census"))]
pub(crate) use off::{block, parse_begin, parse_end, role, touch};

/// Whether the census is built in (the `dir-census` feature).
pub const BUILT: bool = cfg!(feature = "dir-census");
