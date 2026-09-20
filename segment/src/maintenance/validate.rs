// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

use super::{DictionaryCursor, PayloadCursor, PostingsCursor};
use crate::{
    Error, Result, Tid, dictionary::Extent, postings::BLOCK_POSTINGS, segment::Format,
    source::Source, tf_bucket::BUCKET_COUNT,
};

/// Independently bounded dictionary, posting and payload areas (offset zero in
/// each source). Source implementations must not cache every read indefinitely.
pub struct TermAreas<'a, S: Source + ?Sized> {
    pub dictionary: &'a S,
    pub postings: &'a S,
    pub payload: &'a S,
    pub format: Format,
}

/// Validate every term, including postings a later merge will discard.
///
/// `document` must reject unknown TIDs, return their true lengths, and accumulate
/// the supplied position count for subsequent per-document length verification.
/// It must not filter dead tuples. Its state is provisional until this function
/// and the caller's whole-document checks succeed; discard it after any error.
///
/// This composes the bounded readers, but is not a complete segment verifier:
/// header/document table integrity, total lengths, dead-set membership and
/// cross-input live-TID ownership remain the caller's responsibility. Production
/// merges must retain existing verification until those checks are integrated.
/// No output is published. Memory is six read windows, two bounded term buffers,
/// and fixed codec state, excluding sources and callback allocations.
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
    // Dictionary and term traversal execute sequentially. RefCell permits both
    // callbacks to borrow the same cancellation function without retaining data.
    let checkpoint = std::cell::RefCell::new(&mut checkpoint);
    while dictionary
        .next_with(
            || (checkpoint.borrow_mut())(),
            |_, entry| {
                check_extent(entry.postings, areas.postings.len(), &mut postings_end)?;
                check_extent(entry.payload, areas.payload.len(), &mut payload_end)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        dictionary::{DictionaryBuilder, TermEntry},
        payload::PayloadBuilder,
        postings::PostingsBuilder,
        tf_bucket::TfBucket,
    };

    fn fixture(format: Format, count: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
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
        let postings = postings.finish_as(format);
        let payload = payload.finish_as(format);
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
                },
            )
            .unwrap();
        (dictionary.finish(), postings, payload)
    }
    fn areas(f: &(Vec<u8>, Vec<u8>, Vec<u8>), format: Format) -> TermAreas<'_, [u8]> {
        TermAreas {
            dictionary: &f.0,
            postings: &f.1,
            payload: &f.2,
            format,
        }
    }

    #[test]
    fn validates_common_terms_all_formats_and_score_block_boundaries() {
        for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] {
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
        let mut d = DictionaryBuilder::default();
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
            let mut d = DictionaryBuilder::default();
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
                    },
                )
                .unwrap();
            }
            let combined = (
                d.finish(),
                [f.1.as_slice(), f.1.as_slice()].concat(),
                [f.2.as_slice(), f.2.as_slice()].concat(),
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
        let f = fixture(Format::Lsg3, 100_000);
        let tracked = [f.0, f.1, f.2].map(|bytes| Tracked {
            bytes,
            max: Cell::new(0),
        });
        validate_terms(
            TermAreas {
                dictionary: &tracked[0],
                postings: &tracked[1],
                payload: &tracked[2],
                format: Format::Lsg3,
            },
            113,
            64,
            || Ok(()),
            |_, _| Ok(20),
        )
        .unwrap();
        assert!(tracked.iter().all(|s| s.max.get() <= 113));
        let mut unscored = PostingsBuilder::default();
        unscored.push(Tid::new(0, 1).unwrap()).unwrap();
        let mut f = fixture(Format::Lsg3, 1);
        f.1 = unscored.finish_as(Format::Lsg3);
        let mut d = DictionaryBuilder::default();
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
            },
        )
        .unwrap();
        f.0 = d.finish();
        assert!(validate_terms(areas(&f, Format::Lsg3), 3, 64, || Ok(()), |_, _| Ok(20)).is_err());
    }

    #[test]
    fn every_checkpoint_can_cancel_without_success() {
        let f = fixture(Format::Lsg3, 129);
        let mut calls = 0;
        validate_terms(
            areas(&f, Format::Lsg3),
            7,
            64,
            || {
                calls += 1;
                Ok(())
            },
            |_, _| Ok(20),
        )
        .unwrap();
        for stop in 0..calls {
            let mut n = 0;
            let result = validate_terms(
                areas(&f, Format::Lsg3),
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
