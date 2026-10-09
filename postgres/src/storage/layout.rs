// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Byte layouts for LDP2 index pages. No buffer, lock or WAL code lives here,
//! so every codec is testable as plain bytes.
//!
//! Every page is a standard PostgreSQL page with an 8-byte special area:
//!
//! ```text
//! special := magic u32 "LDP2", kind u8, version u8, flags u16
//! payload := bytes [PAGE_HEADER, pd_lower)
//! ```
//!
//! `flags` was reserved (always zero) before removal-horizon logging existed;
//! readers never validate it. Only the meta page uses it: bit 0
//! ([`FLAG_REMOVAL_HORIZONS`]) says the writer logs removal horizons for
//! standbys (see `storage::wal`). A writer without that support clears it.
//!
//! Page kinds:
//!
//! * `META` (block 0): tokenizer spec, write-buffer state, segment directory
//!   and runs awaiting reclamation.
//! * `BUFFER`: a chained page of the write buffer. Payload is a `next` link
//!   followed by raw stream bytes; the meta page's byte count says how much of
//!   the chain is live.
//! * `RUN`: a chained page of an immutable blob (a segment or a dead list).
//! * `FREE`: a page released by a merge or VACUUM, reusable through the FSM.

use std::mem::{offset_of, size_of};

use pgrx::pg_sys;

pub const MAGIC: u32 = 0x4c44_5032;
/// Version 5: segments in TIN's shape (`TNS1`), directory entries that
/// record their origin, sealed write segments and retired sources on the
/// meta page. Version 6: `TNS1` positions streams with single-position
/// masks, rare terms' lengths inline in their records and records without
/// a footer (phase B, round 2). An index of an
/// earlier version must be rebuilt.
pub const VERSION: u8 = 6;
pub const SPECIAL_SIZE: usize = 8;
pub const PAGE_SIZE: usize = pg_sys::BLCKSZ as usize;
pub const PAGE_HEADER: usize = size_of::<pg_sys::PageHeaderData>();
/// Payload bytes available on any page.
pub const CAPACITY: usize = PAGE_SIZE - PAGE_HEADER - SPECIAL_SIZE;
/// Payload bytes on a chained page after its `next` link.
pub const CHAIN_CAPACITY: usize = CAPACITY - 4;
pub const NONE: u32 = u32::MAX;

pub const KIND_META: u8 = 1;
pub const KIND_BUFFER: u8 = 2;
pub const KIND_RUN: u8 = 3;
pub const KIND_FREE: u8 = 4;

/// Meta-page flag: the last writer of this index emits removal-horizon WAL
/// records before freeing pages, so a hot standby with the same resource
/// manager can serve segmented reads from it.
pub const FLAG_REMOVAL_HORIZONS: u16 = 1;
const FLAGS: usize = PAGE_SIZE - SPECIAL_SIZE + 6;

/// Directory entries the meta page can hold before a merge is forced.
pub const MAX_SEGMENTS: usize = 96;
/// Runs the meta page can hold while they wait for readers to drain: what
/// fits beside a full directory.
pub const MAX_PENDING: usize = 48;
/// Sealed write segments waiting for promotion. A seal that would make a
/// third promotes the oldest first, as TIN does in manual mode.
pub const MAX_SEALED: usize = 2;
/// Retired sources `segment_info()` lists until their pages are reclaimed;
/// past this the oldest is forgotten (the list is for display only).
pub const MAX_RETIRED: usize = 4;

/// A segment or sealed write segment a promotion or merge retired, as
/// `segment_info()` shows it until its pages are reclaimed. It references
/// no page: its run waits on the pending list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Retired {
    /// The first page of its run or chain.
    pub first: u32,
    pub pages: u32,
    pub docs: u32,
    pub total_length: u64,
}

const RETIRED_BYTES: usize = 20;

const LOWER: usize = offset_of!(pg_sys::PageHeaderData, pd_lower);
const UPPER: usize = offset_of!(pg_sys::PageHeaderData, pd_upper);
const SPECIAL: usize = offset_of!(pg_sys::PageHeaderData, pd_special);

pub type Result<T> = std::result::Result<T, &'static str>;

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

pub fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(raw)
}

/// Validates a page's header and special area and returns its kind.
pub fn kind(page: &[u8]) -> Result<u8> {
    if page.len() != PAGE_SIZE {
        return Err("invalid Stannum page size");
    }
    let lower = usize::from(u16_at(page, LOWER));
    let upper = usize::from(u16_at(page, UPPER));
    let special = usize::from(u16_at(page, SPECIAL));
    if special != PAGE_SIZE - SPECIAL_SIZE {
        return Err("not a Stannum LDP2 page");
    }
    if lower < PAGE_HEADER || lower > upper || upper > special {
        return Err("invalid Stannum page bounds");
    }
    if u32_at(page, special) != MAGIC {
        return Err("not a Stannum LDP2 page");
    }
    if page[special + 5] != VERSION {
        return Err(
            "unsupported Stannum page version: the index was written in another storage format; REINDEX it",
        );
    }
    Ok(page[special + 4])
}

/// The flags of a validated page (zero on pages written before flags existed).
pub fn flags(page: &[u8]) -> u16 {
    u16_at(page, FLAGS)
}

/// Sets the flags of a page already written by [`write`].
pub fn set_flags(page: &mut [u8], flags: u16) {
    page[FLAGS..FLAGS + 2].copy_from_slice(&flags.to_le_bytes());
}

/// The live payload of a validated page.
pub fn payload(page: &[u8]) -> &[u8] {
    let lower = usize::from(u16_at(page, LOWER));
    &page[PAGE_HEADER..lower]
}

/// Writes a payload into a page initialized by `PageInit` with
/// `SPECIAL_SIZE`, stamping the special area.
pub fn write(page: &mut [u8], kind: u8, payload: &[u8]) -> Result<()> {
    if page.len() != PAGE_SIZE || payload.len() > CAPACITY {
        return Err("Stannum page payload too large");
    }
    let special = PAGE_SIZE - SPECIAL_SIZE;
    page[PAGE_HEADER..PAGE_HEADER + payload.len()].copy_from_slice(payload);
    let lower = (PAGE_HEADER + payload.len()) as u16;
    page[LOWER..LOWER + 2].copy_from_slice(&lower.to_le_bytes());
    page[UPPER..UPPER + 2].copy_from_slice(&(special as u16).to_le_bytes());
    page[SPECIAL..SPECIAL + 2].copy_from_slice(&(special as u16).to_le_bytes());
    page[special..special + 4].copy_from_slice(&MAGIC.to_le_bytes());
    page[special + 4] = kind;
    page[special + 5] = VERSION;
    page[special + 6] = 0;
    page[special + 7] = 0;
    Ok(())
}

/// A chained page's link and data.
pub fn chain(page: &[u8]) -> Result<(u32, &[u8])> {
    let payload = payload(page);
    if payload.len() < 4 {
        return Err("truncated Stannum chain page");
    }
    Ok((u32_at(payload, 0), &payload[4..]))
}

pub fn chain_payload(next: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&next.to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// A chain of pages holding one immutable blob.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Run {
    pub first: u32,
    pub blocks: u32,
    pub bytes: u32,
    /// The chain's last page, so joining two retired chains writes one
    /// page instead of walking a run that may be millions of pages, under
    /// the exclusive meta lock.
    pub last: u32,
}

impl Run {
    pub const EMPTY: Self = Self {
        first: NONE,
        blocks: 0,
        bytes: 0,
        last: NONE,
    };

    pub const fn is_empty(&self) -> bool {
        self.first == NONE
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegmentEntry {
    pub run: Run,
    /// The run's block numbers in order, so byte offsets map to pages.
    pub map: Run,
    pub dead: Run,
    /// Names this dead list, from the index's generation counter: a
    /// replacement can reuse the old list's pages and size, so the run
    /// alone does not tell a reader's cached copy from the current one.
    pub dead_stamp: u32,
    pub docs: u32,
    pub total_length: u64,
    pub generation: u32,
    pub origin: Origin,
}

/// How a segment came to be, as TIN's `segment_info()` reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Origin {
    /// Written by CREATE INDEX or REINDEX.
    #[default]
    Build = 1,
    /// Promoted from the write segment.
    Promotion = 2,
    /// Merged from other segments, or rewritten without its dead documents.
    Merge = 3,
}

impl Origin {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Promotion => "promotion",
            Self::Merge => "merge",
        }
    }

    fn decode(byte: u8) -> Result<Self> {
        Ok(match byte {
            1 => Self::Build,
            2 => Self::Promotion,
            3 => Self::Merge,
            _ => return Err("invalid Stannum segment origin"),
        })
    }
}

const RUN_BYTES: usize = 16;
const ENTRY_BYTES: usize = RUN_BYTES * 3 + 4 + 4 + 8 + 4 + 4;

/// A run waiting until every scan that could still read it has finished.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pending {
    pub run: Run,
    /// Reclaimable once this transaction id is older than every snapshot.
    pub xid: u32,
}

const PENDING_BYTES: usize = RUN_BYTES + 4;

/// The write buffer (the mutable write segment), or a sealed write segment:
/// a chain of buffer pages holding forward records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BufferState {
    /// Incremented on every change, so backends can cache the buffer's contents.
    pub version: u32,
    /// Incremented when the buffer is rewritten from its head (seal, VACUUM),
    /// so a cached prefix knows appends since it was built are still valid.
    /// A sealed segment's is a stamp from the index's generation counter,
    /// never repeated, under which backends cache its contents.
    pub epoch: u32,
    pub head: u32,
    /// Page holding the byte after the last written one.
    pub tail: u32,
    /// Bytes used on the tail page.
    pub tail_used: u32,
    pub bytes: u32,
    pub docs: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// Distinguishes this index's contents from a reused relation number.
    pub identity: u64,
    pub spec: [u8; crate::options::SPEC_BYTES],
    pub buffer: BufferState,
    pub next_generation: u32,
    pub segments: Vec<SegmentEntry>,
    pub pending: Vec<Pending>,
    /// Sealed write segments, oldest first: full write buffers that take no
    /// more documents and wait to be promoted into immutable segments.
    pub sealed: Vec<BufferState>,
    /// Retired sources, oldest first (see [`Retired`]).
    pub retired: Vec<Retired>,
}

const BUFFER_BYTES: usize = 28;
const META_HEADER: usize = 8 + crate::options::SPEC_BYTES + BUFFER_BYTES + 4 + 4 + 4 + 4 + 4;

fn put_buffer(out: &mut Vec<u8>, buffer: &BufferState) {
    for n in [
        buffer.version,
        buffer.epoch,
        buffer.head,
        buffer.tail,
        buffer.tail_used,
        buffer.bytes,
        buffer.docs,
    ] {
        out.extend_from_slice(&n.to_le_bytes());
    }
}

fn get_buffer(bytes: &[u8], at: usize) -> BufferState {
    BufferState {
        version: u32_at(bytes, at),
        epoch: u32_at(bytes, at + 4),
        head: u32_at(bytes, at + 8),
        tail: u32_at(bytes, at + 12),
        tail_used: u32_at(bytes, at + 16),
        bytes: u32_at(bytes, at + 20),
        docs: u32_at(bytes, at + 24),
    }
}

fn put_run(out: &mut Vec<u8>, run: Run) {
    out.extend_from_slice(&run.first.to_le_bytes());
    out.extend_from_slice(&run.blocks.to_le_bytes());
    out.extend_from_slice(&run.bytes.to_le_bytes());
    out.extend_from_slice(&run.last.to_le_bytes());
}

fn get_run(bytes: &[u8], at: usize) -> Run {
    Run {
        first: u32_at(bytes, at),
        blocks: u32_at(bytes, at + 4),
        bytes: u32_at(bytes, at + 8),
        last: u32_at(bytes, at + 12),
    }
}

impl Meta {
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.segments.len() > MAX_SEGMENTS
            || self.pending.len() > MAX_PENDING
            || self.sealed.len() > MAX_SEALED
            || self.retired.len() > MAX_RETIRED
        {
            return Err("Stannum meta page overflow");
        }
        let mut out = Vec::with_capacity(
            META_HEADER
                + self.segments.len() * ENTRY_BYTES
                + self.pending.len() * PENDING_BYTES
                + self.sealed.len() * BUFFER_BYTES,
        );
        out.extend_from_slice(&self.identity.to_le_bytes());
        out.extend_from_slice(&self.spec);
        put_buffer(&mut out, &self.buffer);
        out.extend_from_slice(&self.next_generation.to_le_bytes());
        out.extend_from_slice(&(self.segments.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.pending.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.sealed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.retired.len() as u32).to_le_bytes());
        for entry in &self.segments {
            put_run(&mut out, entry.run);
            put_run(&mut out, entry.map);
            put_run(&mut out, entry.dead);
            out.extend_from_slice(&entry.dead_stamp.to_le_bytes());
            out.extend_from_slice(&entry.docs.to_le_bytes());
            out.extend_from_slice(&entry.total_length.to_le_bytes());
            out.extend_from_slice(&entry.generation.to_le_bytes());
            out.extend_from_slice(&[entry.origin as u8, 0, 0, 0]);
        }
        for pending in &self.pending {
            put_run(&mut out, pending.run);
            out.extend_from_slice(&pending.xid.to_le_bytes());
        }
        for sealed in &self.sealed {
            put_buffer(&mut out, sealed);
        }
        for retired in &self.retired {
            out.extend_from_slice(&retired.first.to_le_bytes());
            out.extend_from_slice(&retired.pages.to_le_bytes());
            out.extend_from_slice(&retired.docs.to_le_bytes());
            out.extend_from_slice(&retired.total_length.to_le_bytes());
        }
        if out.len() > CAPACITY {
            return Err("Stannum meta page overflow");
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < META_HEADER {
            return Err("truncated Stannum meta page");
        }
        let identity = u64_at(bytes, 0);
        let mut spec = [0u8; crate::options::SPEC_BYTES];
        spec.copy_from_slice(&bytes[8..8 + crate::options::SPEC_BYTES]);
        let mut at = 8 + crate::options::SPEC_BYTES;
        let buffer = get_buffer(bytes, at);
        at += BUFFER_BYTES;
        let next_generation = u32_at(bytes, at);
        let segment_count = u32_at(bytes, at + 4) as usize;
        let pending_count = u32_at(bytes, at + 8) as usize;
        let sealed_count = u32_at(bytes, at + 12) as usize;
        let retired_count = u32_at(bytes, at + 16) as usize;
        at += 20;
        if segment_count > MAX_SEGMENTS
            || pending_count > MAX_PENDING
            || sealed_count > MAX_SEALED
            || retired_count > MAX_RETIRED
            || bytes.len()
                != at
                    + segment_count * ENTRY_BYTES
                    + pending_count * PENDING_BYTES
                    + sealed_count * BUFFER_BYTES
                    + retired_count * RETIRED_BYTES
        {
            return Err("invalid Stannum meta page");
        }
        let mut segments = Vec::with_capacity(segment_count);
        for _ in 0..segment_count {
            let entry = SegmentEntry {
                run: get_run(bytes, at),
                map: get_run(bytes, at + RUN_BYTES),
                dead: get_run(bytes, at + 2 * RUN_BYTES),
                dead_stamp: u32_at(bytes, at + 3 * RUN_BYTES),
                docs: u32_at(bytes, at + 3 * RUN_BYTES + 4),
                total_length: u64_at(bytes, at + 3 * RUN_BYTES + 8),
                generation: u32_at(bytes, at + 3 * RUN_BYTES + 16),
                origin: Origin::decode(bytes[at + 3 * RUN_BYTES + 20])?,
            };
            if entry.run.is_empty() || entry.run.blocks == 0 {
                return Err("invalid Stannum segment entry");
            }
            segments.push(entry);
            at += ENTRY_BYTES;
        }
        let mut pending = Vec::with_capacity(pending_count);
        for _ in 0..pending_count {
            pending.push(Pending {
                run: get_run(bytes, at),
                xid: u32_at(bytes, at + RUN_BYTES),
            });
            at += PENDING_BYTES;
        }
        let mut sealed = Vec::with_capacity(sealed_count);
        for _ in 0..sealed_count {
            sealed.push(get_buffer(bytes, at));
            at += BUFFER_BYTES;
        }
        let mut retired = Vec::with_capacity(retired_count);
        for _ in 0..retired_count {
            retired.push(Retired {
                first: u32_at(bytes, at),
                pages: u32_at(bytes, at + 4),
                docs: u32_at(bytes, at + 8),
                total_length: u64_at(bytes, at + 12),
            });
            at += RETIRED_BYTES;
        }
        Ok(Self {
            identity,
            spec,
            buffer,
            next_generation,
            segments,
            pending,
            sealed,
            retired,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_meta() -> Meta {
        Meta {
            identity: 0x1234_5678_9abc_def0,
            spec: [0, 1, 1, 2, 0, 1, 1, 1],
            buffer: BufferState {
                version: 5,
                epoch: 2,
                head: 1,
                tail: 7,
                tail_used: 300,
                bytes: 40_000,
                docs: 12,
            },
            next_generation: 3,
            segments: vec![
                SegmentEntry {
                    run: Run {
                        first: 2,
                        blocks: 5,
                        bytes: 40_123,
                        last: 6,
                    },
                    map: Run {
                        first: 12,
                        blocks: 1,
                        bytes: 20,
                        last: 12,
                    },
                    dead: Run::EMPTY,
                    dead_stamp: 0,
                    docs: 100,
                    total_length: 12_345,
                    generation: 1,
                    origin: Origin::Build,
                },
                SegmentEntry {
                    run: Run {
                        first: 9,
                        blocks: 1,
                        bytes: 12,
                        last: 9,
                    },
                    map: Run::EMPTY,
                    dead: Run {
                        first: 10,
                        blocks: 1,
                        bytes: 5,
                        last: 10,
                    },
                    dead_stamp: 7,
                    docs: 1,
                    total_length: 2,
                    generation: 2,
                    origin: Origin::Merge,
                },
            ],
            pending: vec![Pending {
                run: Run {
                    first: 11,
                    blocks: 2,
                    bytes: 9_000,
                    last: 12,
                },
                xid: 77,
            }],
            sealed: vec![BufferState {
                version: 9,
                epoch: 31,
                head: 40,
                tail: 44,
                tail_used: 100,
                bytes: 5 * 8000,
                docs: 1200,
            }],
            retired: vec![Retired {
                first: 50,
                pages: 3,
                docs: 7,
                total_length: 90,
            }],
        }
    }

    #[test]
    fn meta_round_trips_and_rejects_malformed_input() {
        let meta = sample_meta();
        let bytes = meta.encode().unwrap();
        assert_eq!(Meta::decode(&bytes).unwrap(), meta);
        assert!(Meta::decode(&bytes[..bytes.len() - 1]).is_err());
        assert!(Meta::decode(&[]).is_err());
        let mut too_many = meta.clone();
        too_many.segments = vec![meta.segments[0]; MAX_SEGMENTS + 1];
        assert!(too_many.encode().is_err());
        let mut full = meta.clone();
        full.segments = vec![meta.segments[0]; MAX_SEGMENTS];
        full.pending = vec![meta.pending[0]; MAX_PENDING];
        full.sealed = vec![meta.sealed[0]; MAX_SEALED];
        full.retired = vec![meta.retired[0]; MAX_RETIRED];
        let bytes = full.encode().unwrap();
        assert!(bytes.len() <= CAPACITY);
        assert_eq!(Meta::decode(&bytes).unwrap(), full);
        let mut overfull = full.clone();
        overfull.sealed.push(meta.sealed[0]);
        assert!(overfull.encode().is_err());
    }

    #[test]
    fn pages_stamp_and_validate_special_area() {
        let mut page = vec![0u8; PAGE_SIZE];
        let payload = chain_payload(42, b"hello");
        write(&mut page, KIND_BUFFER, &payload).unwrap();
        assert_eq!(kind(&page).unwrap(), KIND_BUFFER);
        assert_eq!(chain(&page).unwrap(), (42, &b"hello"[..]));
        let mut bad = page.clone();
        bad[PAGE_SIZE - SPECIAL_SIZE] ^= 1;
        assert!(kind(&bad).is_err());
        let mut legacy = vec![0u8; PAGE_SIZE];
        legacy[SPECIAL..SPECIAL + 2].copy_from_slice(&(PAGE_SIZE as u16).to_le_bytes());
        assert!(kind(&legacy).is_err());
        assert!(write(&mut page, KIND_RUN, &vec![0; CAPACITY + 1]).is_err());
        assert!(write(&mut page, KIND_RUN, &vec![7; CAPACITY]).is_ok());
        assert_eq!(super::payload(&page).len(), CAPACITY);
    }

    #[test]
    fn flags_live_in_the_special_area_and_are_not_validated() {
        let mut page = vec![0u8; PAGE_SIZE];
        write(&mut page, KIND_META, b"meta").unwrap();
        assert_eq!(flags(&page), 0);
        set_flags(&mut page, FLAG_REMOVAL_HORIZONS);
        assert_eq!(flags(&page), FLAG_REMOVAL_HORIZONS);
        assert_eq!(kind(&page).unwrap(), KIND_META);
        assert_eq!(super::payload(&page), b"meta");
        // A page written by a build without flags reads as zero, and a
        // rewrite clears whatever was there.
        write(&mut page, KIND_META, b"meta").unwrap();
        assert_eq!(flags(&page), 0);
    }
}
