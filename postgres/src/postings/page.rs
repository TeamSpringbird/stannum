//! Checked LDP1 bytes. PostgreSQL buffer/WAL ownership stays in the parent module.
use pgrx::pg_sys;
use std::mem::{offset_of, size_of};

pub(super) const MAGIC: u32 = 0x4c445031;
pub(super) const BUCKETS: u32 = 128;
pub(super) const NONE: u32 = u32::MAX;
const HEADER: usize = 16;
const ENTRY: usize = 16;
const PAGE_HEADER: usize = size_of::<pg_sys::PageHeaderData>();
const PAGE_SIZE: usize = pg_sys::BLCKSZ as usize;
pub(super) const CAPACITY: usize = (PAGE_SIZE - PAGE_HEADER - HEADER) / ENTRY;
// PostgreSQL's MaxHeapTuplesPerPage also bounds HOT line pointers.
const MAX_HEAP_OFFSET: usize = (PAGE_SIZE - PAGE_HEADER)
    / (offset_of!(pg_sys::HeapTupleHeaderData, t_bits)
        .next_multiple_of(pg_sys::MAXIMUM_ALIGNOF as usize)
        + size_of::<pg_sys::ItemIdData>());
const LOWER: usize = offset_of!(pg_sys::PageHeaderData, pd_lower);
const UPPER: usize = offset_of!(pg_sys::PageHeaderData, pd_upper);
const SPECIAL: usize = offset_of!(pg_sys::PageHeaderData, pd_special);
const SIZE_VERSION: usize = offset_of!(pg_sys::PageHeaderData, pd_pagesize_version);

type Result<T> = std::result::Result<T, &'static str>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Posting {
    pub hash: u64,
    pub block: u32,
    pub offset: u16,
}
impl Posting {
    pub fn validate(self) -> Result<Self> {
        if self.block == NONE || self.offset == 0 || self.offset as usize > MAX_HEAP_OFFSET {
            return Err("invalid Lead heap tuple location");
        }
        Ok(self)
    }

    pub fn tid(self) -> pg_sys::ItemPointerData {
        pg_sys::ItemPointerData {
            ip_blkid: pg_sys::BlockIdData {
                bi_hi: (self.block >> 16) as u16,
                bi_lo: self.block as u16,
            },
            ip_posid: self.offset,
        }
    }
}

/// A validated immutable view, borrowing its locked buffer or test-owned bytes.
pub(super) struct Page<'a> {
    bytes: &'a [u8],
    pub count: usize,
    pub next: u32,
    pub tail: u32,
}
impl<'a> Page<'a> {
    pub fn read(bytes: &'a [u8], block: u32, head: Option<u32>, limit: u32) -> Result<Self> {
        if bytes.len() != PAGE_SIZE {
            return Err("invalid Lead page size");
        }
        let data = &bytes[PAGE_HEADER..];
        let count = get(data, 12) as usize;
        if get(data, 0) != MAGIC || count > CAPACITY {
            return Err("invalid Lead posting page; REINDEX required");
        }
        if native_u16(bytes, LOWER) as usize != PAGE_HEADER + HEADER + count * ENTRY
            || native_u16(bytes, UPPER) as usize != PAGE_SIZE
            || native_u16(bytes, SPECIAL) as usize != PAGE_SIZE
            || native_u16(bytes, SIZE_VERSION) as usize
                != (PAGE_SIZE | pg_sys::PG_PAGE_LAYOUT_VERSION as usize)
        {
            return Err("invalid Lead posting page boundary");
        }
        let next = get(data, 4);
        let tail = get(data, 8);
        if block >= limit {
            return Err("invalid Lead posting block");
        }
        if block == 0 {
            if head.is_some() || count != 0 || next != NONE || tail != 0 {
                return Err("invalid Lead metadata page");
            }
        } else {
            let head = head.ok_or("missing Lead bucket identity")?;
            if !(1..=BUCKETS).contains(&head) || (block <= BUCKETS && block != head) {
                return Err("invalid Lead bucket page");
            }
            // LDP1 only appends overflow blocks; links always move forward.
            if next != NONE && (next <= BUCKETS || next <= block || next >= limit) {
                return Err("invalid Lead posting link");
            }
            if block == head {
                if tail != head && (tail <= BUCKETS || tail >= limit) {
                    return Err("invalid Lead posting tail");
                }
                if (next == NONE) != (tail == head) || (next != NONE && tail < next) {
                    return Err("inconsistent Lead posting tail");
                }
            } else if tail != NONE {
                return Err("invalid Lead overflow page tail");
            }
            for entry in data[HEADER..HEADER + count * ENTRY].chunks_exact(ENTRY) {
                let posting = decode(entry).validate()?;
                if bucket(posting.hash) != head || entry[14..16] != [0, 0] {
                    return Err("invalid Lead posting bucket or reserved bytes");
                }
            }
        }
        Ok(Self {
            bytes,
            count,
            next,
            tail,
        })
    }

    pub fn postings(&self) -> impl Iterator<Item = Posting> + '_ {
        self.bytes[PAGE_HEADER + HEADER..PAGE_HEADER + HEADER + self.count * ENTRY]
            .chunks_exact(ENTRY)
            .map(decode)
    }

    pub fn require_terminal(&self) -> Result<()> {
        if self.next != NONE {
            return Err("nonterminal Lead posting tail");
        }
        Ok(())
    }
}

pub(super) fn bucket(hash: u64) -> u32 {
    1 + (hash % u64::from(BUCKETS)) as u32
}
fn native_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes(bytes[at..at + 2].try_into().unwrap())
}
fn get(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn put(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn decode(bytes: &[u8]) -> Posting {
    Posting {
        hash: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
        block: get(bytes, 8),
        offset: u16::from_le_bytes(bytes[12..14].try_into().unwrap()),
    }
}
fn set_count(bytes: &mut [u8], count: usize) {
    put(&mut bytes[PAGE_HEADER..], 12, count as u32);
    bytes[LOWER..LOWER + 2]
        .copy_from_slice(&((PAGE_HEADER + HEADER + count * ENTRY) as u16).to_ne_bytes());
}

/// Mutable access exists only inside a scoped WAL edit (or owned test storage).
/// The caller supplies a previously validated page, or a fresh PageInit image.
pub(super) struct PageMut<'a>(&'a mut [u8]);
impl<'a> PageMut<'a> {
    pub fn new(bytes: &'a mut [u8]) -> Self {
        assert_eq!(bytes.len(), PAGE_SIZE);
        Self(bytes)
    }
    pub fn initialize(&mut self, tail: u32) {
        put(&mut self.0[PAGE_HEADER..], 0, MAGIC);
        self.set_next(NONE);
        self.set_tail(tail);
        set_count(self.0, 0);
    }
    pub fn set_next(&mut self, block: u32) {
        put(&mut self.0[PAGE_HEADER..], 4, block);
    }
    pub fn set_tail(&mut self, block: u32) {
        put(&mut self.0[PAGE_HEADER..], 8, block);
    }
    pub fn append(&mut self, posting: Posting) -> Result<()> {
        let posting = posting.validate()?;
        let count = get(&self.0[PAGE_HEADER..], 12) as usize;
        if count >= CAPACITY {
            return Err("full Lead posting page");
        }
        let at = PAGE_HEADER + HEADER + count * ENTRY;
        self.0[at..at + 8].copy_from_slice(&posting.hash.to_le_bytes());
        put(self.0, at + 8, posting.block);
        self.0[at + 12..at + 14].copy_from_slice(&posting.offset.to_le_bytes());
        self.0[at + 14..at + 16].fill(0);
        set_count(self.0, count + 1);
        Ok(())
    }
    pub fn retain(&mut self, postings: &[Posting]) {
        assert!(postings.len() <= CAPACITY);
        set_count(self.0, 0);
        for posting in postings {
            self.append(*posting).expect("previously validated posting");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(tail: u32) -> Vec<u8> {
        let mut bytes = vec![0; PAGE_SIZE];
        for at in [UPPER, SPECIAL] {
            bytes[at..at + 2].copy_from_slice(&(PAGE_SIZE as u16).to_ne_bytes());
        }
        bytes[SIZE_VERSION..SIZE_VERSION + 2].copy_from_slice(
            &((PAGE_SIZE | pg_sys::PG_PAGE_LAYOUT_VERSION as usize) as u16).to_ne_bytes(),
        );
        PageMut::new(&mut bytes).initialize(tail);
        bytes
    }
    fn posting() -> Posting {
        Posting {
            hash: 128,
            block: 7,
            offset: 1,
        }
    }

    #[test]
    fn roundtrip_full_page_and_compaction_preserve_ldp1_bytes() {
        let mut bytes = fresh(1);
        let mut edit = PageMut::new(&mut bytes);
        for _ in 0..CAPACITY {
            edit.append(posting()).unwrap();
        }
        assert!(edit.append(posting()).is_err());
        let page = Page::read(&bytes, 1, Some(1), 129).unwrap();
        assert_eq!(page.postings().count(), CAPACITY);
        assert!(page.postings().all(|p| p == posting()));
        assert_eq!(
            &bytes[PAGE_HEADER + HEADER..PAGE_HEADER + HEADER + 16],
            &[128, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 1, 0, 0, 0]
        );
        PageMut::new(&mut bytes).retain(&[posting()]);
        assert_eq!(Page::read(&bytes, 1, Some(1), 129).unwrap().count, 1);
        PageMut::new(&mut bytes).retain(&[]);
        assert_eq!(Page::read(&bytes, 1, Some(1), 129).unwrap().count, 0);
    }

    #[test]
    fn rejects_invalid_headers_before_mutation() {
        let valid = fresh(1);
        for (at, value) in [
            (LOWER, 0),
            (UPPER, (PAGE_HEADER + HEADER) as u16),
            (SPECIAL, 0),
            (SIZE_VERSION, 0),
        ] {
            let mut bad = valid.clone();
            bad[at..at + 2].copy_from_slice(&value.to_ne_bytes());
            assert!(Page::read(&bad, 1, Some(1), 129).is_err());
        }
        let mut bad = valid.clone();
        put(&mut bad[PAGE_HEADER..], 0, 0);
        assert!(Page::read(&bad, 1, Some(1), 129).is_err());
        let mut bad = valid.clone();
        put(&mut bad[PAGE_HEADER..], 12, CAPACITY as u32 + 1);
        assert!(Page::read(&bad, 1, Some(1), 129).is_err());
        assert!(Page::read(&valid[..100], 1, Some(1), 129).is_err());
    }

    #[test]
    fn rejects_invalid_postings_and_bucket_identity() {
        let mut valid = fresh(1);
        PageMut::new(&mut valid).append(posting()).unwrap();
        for offset in [0, MAX_HEAP_OFFSET as u16 + 1, u16::MAX] {
            let mut bad = valid.clone();
            let at = PAGE_HEADER + HEADER + 12;
            bad[at..at + 2].copy_from_slice(&offset.to_le_bytes());
            assert!(Page::read(&bad, 1, Some(1), 129).is_err());
        }
        for (relative, value) in [(8, NONE), (0, 129), (12, 0x10001)] {
            let mut bad = valid.clone();
            put(&mut bad, PAGE_HEADER + HEADER + relative, value);
            assert!(Page::read(&bad, 1, Some(1), 129).is_err());
        }
        assert!(Page::read(&valid, 1, Some(2), 129).is_err());
    }

    #[test]
    fn checks_page_roles_links_and_terminal_tail() {
        assert!(Page::read(&fresh(0), 0, None, 129).is_ok());
        assert!(Page::read(&fresh(1), 0, None, 129).is_err());
        let mut head = fresh(1);
        PageMut::new(&mut head).set_next(129);
        assert!(Page::read(&head, 1, Some(1), 131).is_err());
        PageMut::new(&mut head).set_tail(130);
        assert!(
            Page::read(&head, 1, Some(1), 131)
                .unwrap()
                .require_terminal()
                .is_err()
        );
        for next in [0, 1, 128, 131, 132] {
            PageMut::new(&mut head).set_next(next);
            assert!(Page::read(&head, 1, Some(1), 131).is_err());
        }
        let mut tail = fresh(NONE);
        assert!(
            Page::read(&tail, 130, Some(1), 131)
                .unwrap()
                .require_terminal()
                .is_ok()
        );
        PageMut::new(&mut tail).set_next(129);
        assert!(Page::read(&tail, 130, Some(1), 131).is_err());
        PageMut::new(&mut tail).set_next(131);
        assert!(
            Page::read(&tail, 130, Some(1), 132)
                .unwrap()
                .require_terminal()
                .is_err()
        );
    }
}
