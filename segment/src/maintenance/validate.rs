// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

use super::{DictionaryCursor, PayloadCursor, PostingsCursor, Window};
use crate::{
    Error, Result, Tid,
    dictionary::Extent,
    ordinals::{CHUNK, LIST_MAX, WORDS},
    postings::BLOCK_POSTINGS,
    segment::Format,
    source::Source,
    tf_bucket::BUCKET_COUNT,
};

/// Independently bounded dictionary, posting, payload and ordinals areas
/// (offset zero in each source). The ordinals area is empty before `LSG4`.
/// Source implementations must not cache every read indefinitely.
pub struct TermAreas<'a, S: Source + ?Sized> {
    pub dictionary: &'a S,
    pub postings: &'a S,
    pub payload: &'a S,
    pub ordinals: &'a S,
    pub format: Format,
}

/// Validate every term, including postings a later merge will discard.
///
/// `document` must reject unknown TIDs, return their true lengths, and accumulate
/// the supplied position count for subsequent per-document length verification.
/// It must not filter dead tuples. Its state is provisional until this function
/// and the caller's whole-document checks succeed; discard it after any error.
///
/// From `LSG4` on each term's ordinals extent is bounded like the others and its
/// stream checked for form: `df` ordinals, strictly ascending, in canonical
/// containers that fill the extent exactly. `document` reports no ordinal, so
/// whether a stream names the documents of its postings, and stays below the
/// document count, is not checked here; nor is the page table, which is outside
/// the term areas. The whole-blob verifier checks all three.
///
/// This composes the bounded readers, but is not a complete segment verifier:
/// header/document table integrity, total lengths, dead-set membership and
/// cross-input live-TID ownership remain the caller's responsibility. Production
/// merges must retain existing verification until those checks are integrated.
/// No output is published. Memory is eight read windows, two bounded term
/// buffers, and fixed codec state, excluding sources and callback allocations.
pub fn validate_terms<S: Source + ?Sized>(
    areas: TermAreas<'_, S>,
    window_bytes: usize,
    max_term_bytes: usize,
    mut checkpoint: impl FnMut() -> Result<()>,
    mut document: impl FnMut(Tid, u32) -> Result<u32>,
) -> Result<()> {
    checkpoint()?;
    let mut dictionary = DictionaryCursor::new(
        areas.dictionary,
        0,
        areas.dictionary.len(),
        areas.format,
        window_bytes,
        max_term_bytes,
    )?;
    let mut postings_end = 0;
    let mut payload_end = 0;
    let mut ordinals_end = 0;
    // Dictionary and term traversal execute sequentially. RefCell permits both
    // callbacks to borrow the same cancellation function without retaining data.
    let checkpoint = std::cell::RefCell::new(&mut checkpoint);
    while dictionary
        .next_with(
            || (checkpoint.borrow_mut())(),
            |_, entry| {
                check_extent(entry.postings, areas.postings.len(), &mut postings_end)?;
                check_extent(entry.payload, areas.payload.len(), &mut payload_end)?;
                if areas.format.has_ordinals() {
                    check_extent(entry.ordinals, areas.ordinals.len(), &mut ordinals_end)?;
                }
                let mut postings = PostingsCursor::new(
                    areas.postings,
                    entry.postings.offset,
                    u64::from(entry.postings.len),
                    areas.format,
                    window_bytes,
                )?;
                let mut payload = PayloadCursor::new_with_checkpoint(
                    areas.payload,
                    entry.payload.offset,
                    u64::from(entry.payload.len),
                    areas.format,
                    window_bytes,
                    || (checkpoint.borrow_mut())(),
                )?;
                if entry.df == 0 || postings.count() != entry.df || payload.count() != entry.df {
                    return Err(Error::Corrupt("term document frequency"));
                }
                let mut minima = [u32::MAX; BUCKET_COUNT];
                let mut max_bucket = 0;
                let mut ordinal = 0u32;
                while let Some(posting) = postings.next_with(|| (checkpoint.borrow_mut())())? {
                    let positions = payload
                        .next_with(|_| (checkpoint.borrow_mut())())?
                        .ok_or(Error::Corrupt("missing posting payload"))?;
                    let length = document(posting.tid, positions.positions)?;
                    if positions.positions > length || length == u32::MAX {
                        return Err(Error::Corrupt("term frequency exceeds document length"));
                    }
                    max_bucket = max_bucket.max(positions.tf_bucket);
                    let minimum = &mut minima[positions.tf_bucket as usize];
                    *minimum = (*minimum).min(length);
                    ordinal += 1;
                    let boundary = ordinal.is_multiple_of(BLOCK_POSTINGS) || ordinal == entry.df;
                    if areas.format.has_bounds() && boundary {
                        let found = posting
                            .completed_bound
                            .ok_or(Error::Corrupt("missing score bound"))?;
                        if found.min_len != minima || found.last != posting.tid {
                            return Err(Error::Corrupt("score bound disagrees with documents"));
                        }
                        minima = [u32::MAX; BUCKET_COUNT];
                    } else if posting.completed_bound.is_some() {
                        return Err(Error::Corrupt("unexpected score bound"));
                    }
                }
                if payload
                    .next_with(|_| (checkpoint.borrow_mut())())?
                    .is_some()
                    || max_bucket != entry.max_tf_bucket
                {
                    return Err(Error::Corrupt("term payload summary"));
                }
                if areas.format.has_ordinals() {
                    check_ordinals(
                        areas.ordinals,
                        entry.ordinals,
                        entry.df,
                        window_bytes,
                        || (checkpoint.borrow_mut())(),
                    )?;
                }
                Ok(())
            },
        )?
        .is_some()
    {}
    Ok(())
}

fn check_extent(extent: Extent, area_len: u64, previous_end: &mut u64) -> Result<()> {
    let end = extent
        .offset
        .checked_add(u64::from(extent.len))
        .ok_or(Error::Truncated)?;
    if extent.offset < *previous_end || end > area_len {
        return Err(Error::Corrupt("term extent overlap or bounds"));
    }
    *previous_end = end;
    Ok(())
}

/// Bytes per chunk directory entry of an ordinal stream.
const DIRECTORY_ENTRY: u64 = 8;

/// The form of one ordinal stream (see [`crate::ordinals`]), read forward
/// through one window over its head and directory and one over its chunks.
fn check_ordinals<S: Source + ?Sized>(
    source: &S,
    extent: Extent,
    df: u32,
    window_bytes: usize,
    mut checkpoint: impl FnMut() -> Result<()>,
) -> Result<()> {
    let end = extent.offset + u64::from(extent.len);
    let mut head = Window::new(source, extent.offset, end, window_bytes)?;
    if head.u32()? != df {
        return Err(Error::Corrupt("ordinal count differs from the term"));
    }
    if df as usize <= LIST_MAX {
        let mut last: Option<u32> = None;
        for _ in 0..df {
            let delta = head.u32()?;
            last = Some(
                match last {
                    None => Some(delta),
                    Some(last) => last.checked_add(delta).and_then(|o| o.checked_add(1)),
                }
                .ok_or(Error::Corrupt("ordinal overflow"))?,
            );
        }
        if head.at != end {
            return Err(Error::Corrupt("ordinal list length"));
        }
        return Ok(());
    }
    let chunks = u64::from(head.u32()?);
    let chunks_at = head.at + chunks * DIRECTORY_ENTRY;
    if chunks == 0 || chunks > u64::from(CHUNK) || chunks_at > end {
        return Err(Error::Corrupt("ordinal directory"));
    }
    let mut body = Window::new(source, chunks_at, end, window_bytes)?;
    let mut previous_key = None;
    let mut members = 0u64;
    for _ in 0..chunks {
        checkpoint()?;
        let key = head.u16le()?;
        let cardinality = usize::from(head.u16le()?) + 1;
        // The top bit of the offset marks a bitmap chunk.
        let at = head.fixed()?;
        let bitmap = at & (1 << 31) != 0;
        if previous_key.is_some_and(|previous| previous >= key)
            || at & !(1 << 31) != body.at - chunks_at
        {
            return Err(Error::Corrupt("ordinal directory order"));
        }
        previous_key = Some(key);
        if !bitmap {
            let mut last = None;
            for _ in 0..cardinality {
                let low = body.u16le()?;
                if last.is_some_and(|last| last >= low) {
                    return Err(Error::Corrupt("ordinal array order"));
                }
                last = Some(low);
            }
        } else {
            let mut set = 0usize;
            for _ in 0..WORDS * 8 {
                set += body.byte()?.count_ones() as usize;
            }
            if set != cardinality {
                return Err(Error::Corrupt("ordinal bitmap cardinality"));
            }
        }
        members += cardinality as u64;
    }
    if body.at != end {
        return Err(Error::Corrupt("ordinal stream length"));
    }
    if members != u64::from(df) {
        return Err(Error::Corrupt("ordinal count differs from the term"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        dictionary::{DictionaryBuilder, TermEntry},
        payload::PayloadBuilder,
        postings::PostingsBuilder,
        tf_bucket::TfBucket,
    };

    /// Dictionary, postings, payload and ordinals areas of one term.
    type Areas = (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>);

    fn fixture(format: Format, count: u32) -> Areas {
        let mut postings = PostingsBuilder::default();
        let mut payload = PayloadBuilder::default();
        for i in 0..count {
            let frequency = i % 7 + 1;
            let bucket = TfBucket::from_count(frequency).value();
            postings
                .push_scored(Tid::new(i / 200, (i % 200 + 1) as u16).unwrap(), bucket, 20)
                .unwrap();
            payload
                .push(bucket, &(0..frequency).collect::<Vec<_>>())
                .unwrap();
        }
        let postings = postings.finish_as(format.streams());
        let payload = payload.finish_as(format.streams());
        // The fixture's documents are consecutive, so posting `i` is ordinal `i`.
        let ordinals = if format.has_ordinals() {
            crate::ordinals::encode(&(0..count).collect::<Vec<_>>())
        } else {
            Vec::new()
        };
        let mut dictionary = DictionaryBuilder::with_format(format);
        dictionary
            .push(
                "common",
                TermEntry {
                    df: count,
                    max_tf_bucket: TfBucket::from_count(count.min(7)).value(),
                    postings: Extent {
                        offset: 0,
                        len: postings.len() as u32,
                    },
                    payload: Extent {
                        offset: 0,
                        len: payload.len() as u32,
                    },
                    ordinals: Extent {
                        offset: 0,
                        len: ordinals.len() as u32,
                    },
                },
            )
            .unwrap();
        (dictionary.finish(), postings, payload, ordinals)
    }
    fn areas(f: &Areas, format: Format) -> TermAreas<'_, [u8]> {
        TermAreas {
            dictionary: &f.0,
            postings: &f.1,
            payload: &f.2,
            ordinals: &f.3,
            format,
        }
    }

    #[test]
    fn validates_common_terms_all_formats_and_score_block_boundaries() {
        for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3, Format::Lsg4] {
            for count in [1, 127, 128, 129, 10_000] {
                let f = fixture(format, count);
                let mut seen = 0;
                validate_terms(
                    areas(&f, format),
                    7,
                    64,
                    || Ok(()),
                    |_, frequency| {
                        assert_eq!(frequency, seen % 7 + 1);
                        seen += 1;
                        Ok(20)
                    },
                )
                .unwrap();
                assert_eq!(seen, count);
            }
        }
    }

    #[test]
    fn rejects_unknown_documents_wrong_lengths_and_inconsistent_bounds() {
        let f = fixture(Format::Lsg3, 129);
        for length in [0, 19, 21, u32::MAX] {
            assert!(
                validate_terms(areas(&f, Format::Lsg3), 3, 64, || Ok(()), |_, _| Ok(length))
                    .is_err()
            );
        }
        assert!(
            validate_terms(
                areas(&f, Format::Lsg3),
                3,
                64,
                || Ok(()),
                |_, _| Err(Error::InvalidTid)
            )
            .is_err()
        );
        // No dead-set shortcut exists: even a document the caller plans to drop
        // must have a valid payload, length and score bound.
        let mut corrupt = f.clone();
        corrupt.2.push(0);
        let mut d = DictionaryBuilder::with_format(Format::Lsg3);
        d.push(
            "common",
            TermEntry {
                df: 129,
                max_tf_bucket: 3,
                postings: Extent {
                    offset: 0,
                    len: corrupt.1.len() as u32,
                },
                payload: Extent {
                    offset: 0,
                    len: corrupt.2.len() as u32,
                },
                ordinals: Default::default(),
            },
        )
        .unwrap();
        corrupt.0 = d.finish();
        assert!(
            validate_terms(
                areas(&corrupt, Format::Lsg3),
                3,
                64,
                || Ok(()),
                |_, _| Ok(20)
            )
            .is_err()
        );
    }

    #[test]
    fn checks_multiple_terms_summaries_and_extent_overlap() {
        let f = fixture(Format::Lsg3, 129);
        for (df, bucket, second_offset, good) in [
            (129, 3, f.1.len() as u64, true),
            (128, 3, f.1.len() as u64, false),
            (129, 2, f.1.len() as u64, false),
            (129, 3, 0, false),
        ] {
            let mut d = DictionaryBuilder::with_format(Format::Lsg3);
            for (term, offset, payload_offset) in
                [("a", 0, 0), ("b", second_offset, f.2.len() as u64)]
            {
                d.push(
                    term,
                    TermEntry {
                        df,
                        max_tf_bucket: bucket,
                        postings: Extent {
                            offset,
                            len: f.1.len() as u32,
                        },
                        payload: Extent {
                            offset: payload_offset,
                            len: f.2.len() as u32,
                        },
                        ordinals: Default::default(),
                    },
                )
                .unwrap();
            }
            let combined = (
                d.finish(),
                [f.1.as_slice(), f.1.as_slice()].concat(),
                [f.2.as_slice(), f.2.as_slice()].concat(),
                Vec::new(),
            );
            let mut visits = 0;
            let result = validate_terms(
                areas(&combined, Format::Lsg3),
                3,
                64,
                || Ok(()),
                |_, _| {
                    visits += 1;
                    Ok(20)
                },
            );
            assert_eq!(result.is_ok(), good);
            if good {
                assert_eq!(visits, 258);
            }
        }
    }

    #[test]
    fn all_area_reads_stay_bounded_and_missing_bounds_fail() {
        use std::cell::Cell;
        struct Tracked {
            bytes: Vec<u8>,
            max: Cell<usize>,
        }
        impl Source for Tracked {
            fn len(&self) -> u64 {
                self.bytes.len() as u64
            }
            fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
                self.max.set(self.max.get().max(len));
                self.bytes
                    .get(offset as usize..offset as usize + len)
                    .map(|s| s.to_vec())
                    .ok_or(Error::Truncated)
            }
        }
        for format in [Format::Lsg3, Format::Lsg4] {
            // From `LSG4` on, two bitmap chunks of ordinals, each far larger
            // than the window.
            let f = fixture(format, 100_000);
            assert_eq!(f.3.len() > 16 * 1024, format.has_ordinals());
            let tracked = [f.0, f.1, f.2, f.3].map(|bytes| Tracked {
                bytes,
                max: Cell::new(0),
            });
            validate_terms(
                TermAreas {
                    dictionary: &tracked[0],
                    postings: &tracked[1],
                    payload: &tracked[2],
                    ordinals: &tracked[3],
                    format,
                },
                113,
                64,
                || Ok(()),
                |_, _| Ok(20),
            )
            .unwrap();
            assert!(tracked.iter().all(|s| s.max.get() <= 113));
            assert_eq!(tracked[3].max.get() != 0, format.has_ordinals());
        }
        let mut unscored = PostingsBuilder::default();
        unscored.push(Tid::new(0, 1).unwrap()).unwrap();
        let mut f = fixture(Format::Lsg3, 1);
        f.1 = unscored.finish_as(Format::Lsg3);
        let mut d = DictionaryBuilder::with_format(Format::Lsg3);
        d.push(
            "common",
            TermEntry {
                df: 1,
                max_tf_bucket: 0,
                postings: Extent {
                    offset: 0,
                    len: f.1.len() as u32,
                },
                payload: Extent {
                    offset: 0,
                    len: f.2.len() as u32,
                },
                ordinals: Default::default(),
            },
        )
        .unwrap();
        f.0 = d.finish();
        assert!(validate_terms(areas(&f, Format::Lsg3), 3, 64, || Ok(()), |_, _| Ok(20)).is_err());
    }

    #[test]
    fn lsg4_ordinal_extents_are_bounded_and_streams_checked_for_form() {
        let validate =
            |f: &Areas| validate_terms(areas(f, Format::Lsg4), 5, 64, || Ok(()), |_, _| Ok(20));
        // A list, an array chunk, and a bitmap chunk followed by an array.
        for count in [1, 64, 65, 129, 4095, 4096, 70_000] {
            let f = fixture(Format::Lsg4, count);
            validate(&f).unwrap();
            // The streaming check accepts exactly the streams the in-memory
            // one does, whichever byte is damaged.
            let step = (f.3.len() / 600).max(1);
            for at in (0..f.3.len()).step_by(step).chain(f.3.len() - 2..f.3.len()) {
                let mut flipped = f.clone();
                flipped.3[at] ^= 0x55;
                assert_eq!(
                    validate(&flipped).is_ok(),
                    crate::ordinals::validate(&flipped.3, count, u32::MAX).is_ok(),
                    "{count} documents, byte {at}"
                );
            }
            // Extents must end inside the area and hold the stream exactly.
            let entry = |ordinals: Extent| {
                let mut d = DictionaryBuilder::with_format(Format::Lsg4);
                d.push(
                    "common",
                    TermEntry {
                        df: count,
                        max_tf_bucket: TfBucket::from_count(count.min(7)).value(),
                        postings: Extent {
                            offset: 0,
                            len: f.1.len() as u32,
                        },
                        payload: Extent {
                            offset: 0,
                            len: f.2.len() as u32,
                        },
                        ordinals,
                    },
                )
                .unwrap();
                d.finish()
            };
            let len = f.3.len() as u32;
            for (offset, len, padding, good) in [
                (0, len, 0, true),
                (0, len, 1, true),
                (1, len, 1, false),
                (0, len + 1, 1, false),
                (0, len + 1, 0, false),
                (0, len - 1, 0, false),
                (0, 0, 0, false),
                (u64::MAX, 1, 0, false),
            ] {
                let mut moved = f.clone();
                moved.0 = entry(Extent { offset, len });
                moved.3.resize(f.3.len() + padding, 0);
                assert_eq!(validate(&moved).is_ok(), good, "{count}: {offset}+{len}");
            }
        }
        // A second term may not start inside the first one's stream.
        let f = fixture(Format::Lsg4, 129);
        for (second, good) in [
            (f.3.len() as u64, true),
            (f.3.len() as u64 - 1, false),
            (0, false),
        ] {
            let mut d = DictionaryBuilder::with_format(Format::Lsg4);
            for (term, postings, payload, ordinals) in [
                ("a", 0, 0, 0),
                ("b", f.1.len() as u64, f.2.len() as u64, second),
            ] {
                d.push(
                    term,
                    TermEntry {
                        df: 129,
                        max_tf_bucket: 3,
                        postings: Extent {
                            offset: postings,
                            len: f.1.len() as u32,
                        },
                        payload: Extent {
                            offset: payload,
                            len: f.2.len() as u32,
                        },
                        ordinals: Extent {
                            offset: ordinals,
                            len: f.3.len() as u32,
                        },
                    },
                )
                .unwrap();
            }
            let combined = (d.finish(), f.1.repeat(2), f.2.repeat(2), f.3.repeat(2));
            assert_eq!(
                validate(&combined).is_ok(),
                good,
                "second stream at {second}"
            );
        }
        // Before `LSG4` the ordinals area is never read.
        let mut old = fixture(Format::Lsg3, 129);
        old.3 = vec![0xff; 16];
        validate_terms(areas(&old, Format::Lsg3), 5, 64, || Ok(()), |_, _| Ok(20)).unwrap();
    }

    #[test]
    fn every_checkpoint_can_cancel_without_success() {
        let mut previous = 0;
        for format in [Format::Lsg3, Format::Lsg4] {
            let f = fixture(format, 129);
            let mut calls = 0;
            validate_terms(
                areas(&f, format),
                7,
                64,
                || {
                    calls += 1;
                    Ok(())
                },
                |_, _| Ok(20),
            )
            .unwrap();
            // An ordinal stream adds a checkpoint per chunk.
            assert!(calls > previous);
            previous = calls;
            for stop in 0..calls {
                let mut n = 0;
                let result = validate_terms(
                    areas(&f, format),
                    7,
                    64,
                    || {
                        n += 1;
                        if n > stop {
                            Err(Error::Corrupt("cancelled"))
                        } else {
                            Ok(())
                        }
                    },
                    |_, _| Ok(20),
                );
                assert!(result.is_err());
            }
        }
    }
}
