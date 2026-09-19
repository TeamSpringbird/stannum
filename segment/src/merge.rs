//! Validated direct posting merges, independent of PostgreSQL publication.
//!
//! Inputs are borrowed complete blobs with per-input dead sets. All inputs,
//! including dead documents, are verified before metadata is reused. The output
//! uses the current format; old formats remain readable. No index page is written.
//! Foreground PostgreSQL merges retain the metadata lock; VACUUM merges owned
//! snapshots unlocked and revalidates before publication. Page allocation, WAL
//! and publication remain the caller’s responsibility.
use crate::dictionary::{DictionaryBuilder, Extent, TermEntry};
use crate::payload::PayloadBuilder;
use crate::postings::PostingsBuilder;
use crate::segment::{Format, Segment};
use crate::set::Cursor;
use crate::{Error, Result, Tid, varint};
use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap};

/// Pairing a blob and its dead set prevents mismatched parallel input arrays.
#[derive(Clone, Copy)]
pub struct MergeInput<'a> {
    pub bytes: &'a [u8],
    pub dead: &'a BTreeSet<Tid>,
}

/// Admission/output limits, not a peak-memory or elapsed-time guarantee.
/// Verification and existing codecs allocate temporary data. Cancellation runs
/// between input validations, documents, terms and postings; a single validation
/// or codec call is not interruptible. Callers must bound individual input sizes.
#[derive(Clone, Copy, Debug)]
pub struct MergeLimits {
    pub max_inputs: usize,
    pub max_input_bytes: usize,
    pub max_documents: usize,
    pub max_output_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error(transparent)]
    Codec(#[from] Error),
    #[error("invalid merge input {index}: {detail}")]
    InvalidInput { index: usize, detail: String },
    #[error("merge limit exceeded: {0}")]
    Limit(&'static str),
    #[error("merge cancelled")]
    Cancelled,
    #[error("cannot allocate merge output")]
    Allocation,
}

/// Validate and merge immutable segments, filtering each source's dead tuples.
/// Duplicate live CTIDs are errors; dead CTID reuse in another input is allowed.
/// On error/cancellation no output is returned, and inputs are never modified.
/// Resource exhaustion in existing infallible codec allocations is not converted
/// to `MergeError`; callers must enforce their own memory budget as well.
pub fn merge(
    inputs: &[MergeInput<'_>],
    limits: MergeLimits,
    checkpoint: impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<Vec<u8>, MergeError> {
    merge_as(inputs, limits, Format::CURRENT, checkpoint)
}

fn check(actual: usize, limit: usize, name: &'static str) -> std::result::Result<(), MergeError> {
    if actual > limit {
        Err(MergeError::Limit(name))
    } else {
        Ok(())
    }
}

/// Apply the same admission and complete input validation used by direct merges.
/// Alternative executors must additionally reject duplicate live TIDs, enforce
/// output limits and provide checkpoints while constructing their output.
pub fn validate_inputs(
    inputs: &[MergeInput<'_>],
    limits: MergeLimits,
    mut checkpoint: impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<(), MergeError> {
    checkpoint()?;
    check(inputs.len(), limits.max_inputs, "input count")?;
    let mut input_bytes = 0usize;
    let mut input_docs = 0usize;
    // Admission precedes the whole-segment verifier and merge allocations.
    for input in inputs {
        input_bytes = input_bytes
            .checked_add(input.bytes.len())
            .ok_or(MergeError::Limit("input bytes"))?;
        check(
            input_bytes,
            limits.max_input_bytes.min(u32::MAX as usize),
            "input bytes",
        )?;
        let parsed = Segment::parse(input.bytes)?;
        check(
            input.dead.len(),
            parsed.document_count() as usize,
            "dead tuple count",
        )?;
        input_docs = input_docs
            .checked_add(parsed.document_count() as usize)
            .ok_or(MergeError::Limit("documents"))?;
        check(
            input_docs,
            limits.max_documents.min(u32::MAX as usize),
            "documents",
        )?;
    }
    for (index, input) in inputs.iter().enumerate() {
        checkpoint()?;
        let report = crate::verify::verify_segment(input.bytes);
        for finding in &report.findings {
            let legacy_notice = finding.severity == crate::verify::Severity::Warning
                && finding.location == "header"
                && finding.message
                    == "LSG1 segment: ranked scans over it score every candidate; REINDEX to upgrade";
            if !legacy_notice {
                return Err(MergeError::InvalidInput {
                    index,
                    detail: finding.to_string(),
                });
            }
        }
        if input
            .dead
            .iter()
            .any(|tid| report.documents.binary_search(tid).is_err())
        {
            return Err(MergeError::InvalidInput {
                index,
                detail: "dead tuple absent from input document table".into(),
            });
        }
        checkpoint()?;
    }
    Ok(())
}

// Dead postings have already been fully validated. Advance them outside the
// cross-input heap; only live candidates need ordering against other sources.
fn skip_dead(
    postings: &mut crate::postings::PostingsCursor<'_>,
    payload: &mut crate::payload::PayloadCursor<'_>,
    dead: &BTreeSet<Tid>,
    checkpoint: &mut impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<(), MergeError> {
    while postings.current().is_some_and(|tid| dead.contains(&tid)) {
        checkpoint()?;
        payload.next_bucket()?;
        postings.advance()?;
    }
    Ok(())
}

fn merge_as(
    inputs: &[MergeInput<'_>],
    limits: MergeLimits,
    format: Format,
    mut checkpoint: impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<Vec<u8>, MergeError> {
    validate_inputs(inputs, limits, &mut checkpoint)?;
    let segments = inputs
        .iter()
        .map(|input| Segment::parse(input.bytes))
        .collect::<Result<Vec<_>>>()?;
    let mut docs = segments
        .iter()
        .map(Segment::documents)
        .collect::<Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::new();
    for (i, cursor) in docs.iter().enumerate() {
        if let Some(tid) = cursor.current() {
            heap.push(Reverse((tid, i)));
        }
    }
    let mut live_lengths = HashMap::new();
    let mut doc_builder = PostingsBuilder::default();
    let mut length_bytes = Vec::new();
    let mut total_length = 0u64;
    while let Some(Reverse((tid, i))) = heap.pop() {
        checkpoint()?;
        if !inputs[i].dead.contains(&tid) {
            let len = segments[i].length_at(docs[i].ordinal())?;
            if live_lengths.insert(tid, len).is_some() {
                return Err(Error::Unordered.into());
            }
            doc_builder.push(tid)?;
            length_bytes.extend_from_slice(&len.to_le_bytes());
            total_length = total_length
                .checked_add(u64::from(len))
                .ok_or(MergeError::Limit("total document length"))?;
        }
        docs[i].advance()?;
        if let Some(tid) = docs[i].current() {
            heap.push(Reverse((tid, i)));
        }
    }
    let sources = inputs;
    let mut dictionaries = segments
        .iter()
        .map(|s| Ok(s.dictionary()?.iter()))
        .collect::<Result<Vec<_>>>()?;
    let mut entries = vec![None; segments.len()];
    let mut terms = BinaryHeap::new();
    for (i, iter) in dictionaries.iter_mut().enumerate() {
        if let Some(item) = iter.next() {
            let (term, entry) = item?;
            entries[i] = Some(entry);
            terms.push(Reverse((term, i)));
        }
    }
    let mut dictionary = DictionaryBuilder::with_format(format);
    let mut postings_area = Vec::new();
    let mut payload_area = Vec::new();
    let mut positions = Vec::new();
    while let Some(Reverse((term, first))) = terms.pop() {
        checkpoint()?;
        let mut inputs = vec![first];
        while terms.peek().is_some_and(|Reverse((next, _))| next == &term) {
            // The heap was just peeked and no intervening operation can empty it.
            inputs.push(terms.pop().expect("peeked term exists").0.1);
        }
        let mut cursors = Vec::new();
        let mut postings_heap = BinaryHeap::new();
        for &i in &inputs {
            let resolved =
                segments[i].resolve(entries[i].take().expect("each queued term owns an entry"))?;
            let mut postings = resolved.cursor()?;
            let mut payload = resolved.payload()?.cursor();
            skip_dead(
                &mut postings,
                &mut payload,
                sources[i].dead,
                &mut checkpoint,
            )?;
            if let Some(tid) = postings.current() {
                postings_heap.push(Reverse((tid, cursors.len())));
            }
            cursors.push((i, postings, payload));
        }
        let mut postings = PostingsBuilder::default();
        let mut payload = PayloadBuilder::default();
        let mut count = 0u32;
        let mut max_bucket = 0;
        while let Some(Reverse((tid, c))) = postings_heap.pop() {
            checkpoint()?;
            let (i, cursor, positions_cursor) = &mut cursors[c];
            positions.clear();
            let bucket = positions_cursor.next_into(&mut positions)?;
            let len = *live_lengths
                .get(&tid)
                .ok_or(Error::Corrupt("posting missing document"))?;
            postings.push_scored(tid, bucket, len)?;
            payload.push(bucket, &positions)?;
            count = count
                .checked_add(1)
                .ok_or(MergeError::Limit("postings count"))?;
            max_bucket = max_bucket.max(bucket);
            cursor.advance()?;
            skip_dead(cursor, positions_cursor, sources[*i].dead, &mut checkpoint)?;
            if let Some(tid) = cursor.current() {
                postings_heap.push(Reverse((tid, c)));
            }
        }
        if count != 0 {
            let posting_bytes = postings.finish_as(format);
            let payload_bytes = payload.finish_as(format);
            check(
                postings_area
                    .len()
                    .checked_add(payload_area.len())
                    .and_then(|n| n.checked_add(posting_bytes.len()))
                    .and_then(|n| n.checked_add(payload_bytes.len()))
                    .ok_or(MergeError::Limit("output bytes"))?,
                limits.max_output_bytes,
                "output bytes",
            )?;
            dictionary.push(
                &term,
                TermEntry {
                    df: count,
                    max_tf_bucket: max_bucket,
                    postings: Extent {
                        offset: postings_area.len() as u64,
                        len: u32::try_from(posting_bytes.len())
                            .map_err(|_| MergeError::Limit("posting extent"))?,
                    },
                    payload: Extent {
                        offset: payload_area.len() as u64,
                        len: u32::try_from(payload_bytes.len())
                            .map_err(|_| MergeError::Limit("payload extent"))?,
                    },
                },
            )?;
            postings_area.extend_from_slice(&posting_bytes);
            payload_area.extend_from_slice(&payload_bytes);
        }
        for i in inputs {
            if let Some(item) = dictionaries[i].next() {
                let (term, entry) = item?;
                entries[i] = Some(entry);
                terms.push(Reverse((term, i)));
            }
        }
    }
    let dictionary = dictionary.finish();
    let documents = doc_builder.finish();
    let mut out = Vec::new();
    out.extend_from_slice(format.magic());
    for n in [
        live_lengths.len() as u64,
        total_length,
        dictionary.len() as u64,
        postings_area.len() as u64,
        payload_area.len() as u64,
        documents.len() as u64,
    ] {
        varint::put(&mut out, n);
    }
    for bytes in [
        &dictionary,
        &postings_area,
        &payload_area,
        &documents,
        &length_bytes,
    ] {
        let next = out
            .len()
            .checked_add(bytes.len())
            .ok_or(MergeError::Limit("output bytes"))?;
        check(next, limits.max_output_bytes, "output bytes")?;
        out.try_reserve(bytes.len())
            .map_err(|_| MergeError::Allocation)?;
        out.extend_from_slice(bytes);
    }
    checkpoint()?;
    check(out.len(), limits.max_output_bytes, "output bytes")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::direct_merge_poc::{fixture, reference};

    fn limits() -> MergeLimits {
        MergeLimits {
            max_inputs: 128,
            max_input_bytes: 64 << 20,
            max_documents: 100_000,
            max_output_bytes: 64 << 20,
        }
    }
    fn inputs<'a>(blobs: &'a [Vec<u8>], dead: &'a [BTreeSet<Tid>]) -> Vec<MergeInput<'a>> {
        blobs
            .iter()
            .zip(dead)
            .map(|(bytes, dead)| MergeInput { bytes, dead })
            .collect()
    }

    #[test]
    fn validated_merge_matches_reference_for_all_formats() {
        for interleaved in [false, true] {
            for deletion in [0, 1, 7] {
                let (blobs, dead) = fixture(3, 65, 40, 67, interleaved, deletion);
                for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] {
                    assert_eq!(
                        merge_as(&inputs(&blobs, &dead), limits(), format, || Ok(())).unwrap(),
                        reference(&blobs, &dead, format).unwrap()
                    );
                }
            }
        }
        let empty = merge(&[], limits(), || Ok(())).unwrap();
        assert_eq!(empty, reference(&[], &[], Format::CURRENT).unwrap());
    }

    #[test]
    fn limits_reject_without_returning_partial_output() {
        let (blobs, dead) = fixture(2, 3, 4, 2, true, 0);
        let input = inputs(&blobs, &dead);
        let exact = merge(&input, limits(), || Ok(())).unwrap();
        for (limited, expected) in [
            (
                MergeLimits {
                    max_inputs: 1,
                    ..limits()
                },
                "input count",
            ),
            (
                MergeLimits {
                    max_input_bytes: blobs.iter().map(Vec::len).sum::<usize>() - 1,
                    ..limits()
                },
                "input bytes",
            ),
            (
                MergeLimits {
                    max_documents: 5,
                    ..limits()
                },
                "documents",
            ),
            (
                MergeLimits {
                    max_output_bytes: exact.len() - 1,
                    ..limits()
                },
                "output bytes",
            ),
        ] {
            assert!(
                matches!(merge(&input,limited,|| Ok(())),Err(MergeError::Limit(n)) if n == expected)
            );
        }
        assert_eq!(
            merge(
                &input,
                MergeLimits {
                    max_inputs: 2,
                    max_input_bytes: blobs.iter().map(Vec::len).sum(),
                    max_documents: 6,
                    max_output_bytes: exact.len()
                },
                || Ok(())
            )
            .unwrap(),
            exact
        );
    }

    #[test]
    fn cancellation_at_every_checkpoint_is_atomic() {
        for deletion in [0, 1, 2] {
            let (blobs, dead) = fixture(2, 3, 4, 2, true, deletion);
            let original = blobs.clone();
            let input = inputs(&blobs, &dead);
            let mut calls = 0;
            merge(&input, limits(), || {
                calls += 1;
                Ok(())
            })
            .unwrap();
            for stop in 1..=calls {
                let mut at = 0;
                assert!(matches!(
                    merge(&input, limits(), || {
                        at += 1;
                        if at == stop {
                            Err(MergeError::Cancelled)
                        } else {
                            Ok(())
                        }
                    }),
                    Err(MergeError::Cancelled)
                ));
            }
            assert_eq!(blobs, original);
            assert_eq!(
                merge(&input, limits(), || Ok(())).unwrap(),
                reference(&blobs, &dead, Format::CURRENT).unwrap()
            );
        }
    }

    #[test]
    fn rejects_unknown_dead_tids_and_duplicate_live_tids() {
        let (mut blobs, mut dead) = fixture(1, 1, 4, 2, true, 0);
        dead[0].insert(Tid::new(42, 1).unwrap());
        assert!(matches!(
            merge(&inputs(&blobs, &dead), limits(), || Ok(())),
            Err(MergeError::InvalidInput { index: 0, .. })
        ));
        dead[0].clear();
        blobs.push(blobs[0].clone());
        dead.push(BTreeSet::new());
        assert!(matches!(
            merge(&inputs(&blobs, &dead), limits(), || Ok(())),
            Err(MergeError::Codec(Error::Unordered))
        ));
        dead[0].insert(Tid::new(0, 1).unwrap());
        assert_eq!(
            merge(&inputs(&blobs, &dead), limits(), || Ok(())).unwrap(),
            reference(&blobs, &dead, Format::CURRENT).unwrap()
        );
    }

    #[test]
    fn corrupt_lengths_are_rejected_even_for_dead_documents() {
        let (mut blobs, mut dead) = fixture(1, 1, 4, 2, true, 0);
        // Length table is the final u32 for the only document. Its header total
        // and actual positions still say four, so trusting it would corrupt BM25.
        let n = blobs[0].len();
        blobs[0][n - 4..].copy_from_slice(&5u32.to_le_bytes());
        for deleted in [false, true] {
            if deleted {
                dead[0].insert(Tid::new(0, 1).unwrap());
            }
            assert!(matches!(
                merge(&inputs(&blobs, &dead), limits(), || Ok(())),
                Err(MergeError::InvalidInput { index: 0, .. })
            ));
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(32))]
        #[test]
        fn validated_merge_generated_equivalence(parts in 1usize..6,docs in 0usize..40,tokens in 1usize..40,vocab in 1usize..50,deletion in 0usize..8,interleaved in proptest::bool::ANY) {
            let (blobs,dead)=fixture(parts,docs,tokens,vocab,interleaved,deletion);
            let output=merge(&inputs(&blobs,&dead),limits(),|| Ok(())).unwrap();
            proptest::prop_assert_eq!(output,reference(&blobs,&dead,Format::CURRENT).unwrap());
        }
        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(proptest::num::u8::ANY,0..1024)) {
            let dead=BTreeSet::new();
            let _=merge(&[MergeInput { bytes:&bytes,dead:&dead }],limits(),|| Ok(()));
        }
    }

    #[test]
    fn mutated_valid_inputs_never_emit_unverified_output() {
        let (blobs, dead) = fixture(1, 4, 8, 3, true, 0);
        for i in 0..blobs[0].len() {
            let mut changed = blobs[0].clone();
            changed[i] ^= 1;
            if let Ok(output) = merge(
                &[MergeInput {
                    bytes: &changed,
                    dead: &dead[0],
                }],
                limits(),
                || Ok(()),
            ) {
                assert!(
                    crate::verify::verify_segment(&output).is_clean(),
                    "byte {i}"
                );
            }
        }
    }

    #[test]
    #[ignore = "release timing includes full validation; not database throughput"]
    fn hardened_merge_microprobe() {
        use std::time::Instant;
        for (docs, tokens, vocab) in [(32, 40, 4), (512, 400, 4), (512, 400, 400)] {
            let (blobs, dead) = fixture(8, docs, tokens, vocab, true, 7);
            let input = inputs(&blobs, &dead);
            let expected = reference(&blobs, &dead, Format::CURRENT).unwrap();
            let mut times = [Vec::new(), Vec::new()];
            for round in 0..10 {
                for method in if round % 2 == 0 { [0, 1] } else { [1, 0] } {
                    let start = Instant::now();
                    let output = if method == 0 {
                        reference(&blobs, &dead, Format::CURRENT).unwrap()
                    } else {
                        merge(&input, limits(), || Ok(())).unwrap()
                    };
                    let elapsed = start.elapsed().as_secs_f64() * 1000.;
                    assert_eq!(output, expected);
                    if round >= 2 {
                        times[method].push(elapsed);
                    }
                }
            }
            for samples in &mut times {
                samples.sort_by(f64::total_cmp);
            }
            println!(
                "8x{docs}, tokens={tokens}, vocab={vocab}: reference {:.3} ms, validated merge {:.3} ms; samples {:?}",
                (times[0][3] + times[0][4]) / 2.,
                (times[1][3] + times[1][4]) / 2.,
                times
            );
        }
    }
}
