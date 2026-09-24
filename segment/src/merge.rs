// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Validated direct posting merges, independent of PostgreSQL publication.
//!
//! Inputs are borrowed complete blobs with per-input dead sets. All inputs,
//! including dead documents, are verified before metadata is reused. No index
//! page is written.
//! Foreground PostgreSQL merges retain the metadata lock; VACUUM merges owned
//! snapshots unlocked and revalidates before publication. Page allocation, WAL
//! and publication remain the caller’s responsibility.
use crate::dictionary::{DictionaryBuilder, Extent, TermEntry};
use crate::docs::TidCursor;
use crate::payload::PayloadBuilder;
use crate::segment::Segment;
use crate::set::Cursor;
use crate::{Error, Result, Tid};
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
/// between input validations, documents, terms and members; a single validation
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
    merge_inner(inputs, limits, checkpoint)
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
        if let Some(finding) = report.findings.first() {
            return Err(MergeError::InvalidInput {
                index,
                detail: finding.to_string(),
            });
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

// Dead documents have already been fully validated. Advance past them outside
// the cross-input heap; only live candidates need ordering against other
// sources. Complete validation proved source membership; the map contains
// every live document. A missing/mismatched owner therefore denotes this
// source's dead occurrence, including a TID reused live by another source.
fn skip_dead(
    documents: &mut TidCursor<'_>,
    payload: &mut crate::payload::PayloadCursor<'_>,
    live_lengths: &HashMap<Tid, (u32, usize, u32)>,
    source: usize,
    checkpoint: &mut impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<Option<(u32, u32)>, MergeError> {
    while let Some(tid) = documents.current() {
        if let Some(&(length, owner, ordinal)) = live_lengths.get(&tid)
            && owner == source
        {
            return Ok(Some((length, ordinal)));
        }
        checkpoint()?;
        payload.skip_entry()?;
        documents.advance()?;
    }
    Ok(None)
}

fn merge_inner(
    inputs: &[MergeInput<'_>],
    limits: MergeLimits,
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
    let mut page_documents = Vec::new();
    let mut length_bytes = Vec::new();
    let mut class_bytes = Vec::new();
    let mut total_length = 0u64;
    while let Some(Reverse((tid, i))) = heap.pop() {
        checkpoint()?;
        if !inputs[i].dead.contains(&tid) {
            let len = segments[i].length_at(docs[i].ordinal())?;
            let ordinal = u32::try_from(live_lengths.len())
                .map_err(|_| MergeError::Limit("document count"))?;
            page_documents.push(tid);
            if live_lengths.insert(tid, (len, i, ordinal)).is_some() {
                return Err(Error::Unordered.into());
            }
            length_bytes.extend_from_slice(&len.to_le_bytes());
            class_bytes.push(crate::length_class::class_of(len));
            total_length = total_length
                .checked_add(u64::from(len))
                .ok_or(MergeError::Limit("total document length"))?;
        }
        docs[i].advance()?;
        if let Some(tid) = docs[i].current() {
            heap.push(Reverse((tid, i)));
        }
    }
    drop(docs);
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
    let mut dictionary = DictionaryBuilder::default();
    let mut ordinals_area = Vec::new();
    let mut payload_area = Vec::new();
    let mut ordinals = Vec::new();
    let mut scores: Vec<(u8, u32)> = Vec::new();
    let mut positions = Vec::new();
    let mut term_inputs = Vec::new();
    let mut cursors = Vec::new();
    let mut postings_heap = BinaryHeap::new();
    while let Some(Reverse((term, first))) = terms.pop() {
        checkpoint()?;
        term_inputs.clear();
        term_inputs.push(first);
        while terms.peek().is_some_and(|Reverse((next, _))| next == &term) {
            // The heap was just peeked and no intervening operation can empty it.
            term_inputs.push(terms.pop().expect("peeked term exists").0.1);
        }
        cursors.clear();
        postings_heap.clear();
        for &i in &term_inputs {
            let resolved =
                segments[i].resolve(entries[i].take().expect("each queued term owns an entry"))?;
            let mut documents = resolved.cursor()?;
            if resolved.df() == 1
                && documents.current().is_some_and(|tid| {
                    live_lengths
                        .get(&tid)
                        .is_none_or(|&(_, owner, _)| owner != i)
                })
            {
                // This fully validated source term has no surviving document.
                // No payload cursor will be consumed for it.
                checkpoint()?;
                continue;
            }
            let mut payload = resolved.payload()?.cursor();
            let length = skip_dead(
                &mut documents,
                &mut payload,
                &live_lengths,
                i,
                &mut checkpoint,
            )?;
            if let Some(tid) = documents.current() {
                postings_heap.push(Reverse((tid, cursors.len())));
            }
            cursors.push((i, documents, payload, length));
        }
        let mut payload = PayloadBuilder::default();
        let mut count = 0u32;
        let mut max_bucket = 0;
        ordinals.clear();
        scores.clear();
        while let Some(Reverse((_, c))) = postings_heap.pop() {
            checkpoint()?;
            let (i, cursor, positions_cursor, length) = &mut cursors[c];
            positions.clear();
            positions_cursor.next_into(&mut positions)?;
            let bucket = cursor
                .bucket()
                .ok_or(Error::Corrupt("term member without a bucket"))?;
            let (len, ordinal) = length.ok_or(Error::Corrupt("posting missing document"))?;
            ordinals.push(ordinal);
            scores.push((bucket, len));
            payload.push(&positions)?;
            count = count
                .checked_add(1)
                .ok_or(MergeError::Limit("postings count"))?;
            max_bucket = max_bucket.max(bucket);
            cursor.advance()?;
            *length = skip_dead(cursor, positions_cursor, &live_lengths, *i, &mut checkpoint)?;
            if let Some(tid) = cursor.current() {
                postings_heap.push(Reverse((tid, c)));
            }
        }
        if count != 0 {
            let ordinal_bytes = crate::ordinals::encode_scored(&ordinals, &scores);
            let payload_bytes = payload.finish();
            check(
                ordinals_area
                    .len()
                    .checked_add(payload_area.len())
                    .and_then(|n| n.checked_add(ordinal_bytes.len()))
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
                    ordinals: Extent {
                        offset: ordinals_area.len() as u64,
                        len: u32::try_from(ordinal_bytes.len())
                            .map_err(|_| MergeError::Limit("ordinal extent"))?,
                    },
                    payload: Extent {
                        offset: payload_area.len() as u64,
                        len: u32::try_from(payload_bytes.len())
                            .map_err(|_| MergeError::Limit("payload extent"))?,
                    },
                },
            )?;
            ordinals_area.extend_from_slice(&ordinal_bytes);
            payload_area.extend_from_slice(&payload_bytes);
        }
        for &i in &term_inputs {
            if let Some(item) = dictionaries[i].next() {
                let (term, entry) = item?;
                entries[i] = Some(entry);
                terms.push(Reverse((term, i)));
            }
        }
    }
    let dictionary = dictionary.finish();
    let offsets = crate::docs::offsets(page_documents.iter().copied());
    let pages = crate::docs::page_table(page_documents.iter().copied());
    let header = crate::segment::header(
        live_lengths.len() as u32,
        total_length,
        dictionary.len(),
        ordinals_area.len(),
        payload_area.len(),
        pages.len(),
    );
    drop(page_documents);
    drop(live_lengths);
    let out = assemble(
        [
            header,
            dictionary,
            ordinals_area,
            payload_area,
            offsets,
            length_bytes,
            class_bytes,
            pages,
        ],
        limits.max_output_bytes,
    )?;
    checkpoint()?;
    check(out.len(), limits.max_output_bytes, "output bytes")?;
    Ok(out)
}

/// Preserve the byte layout while reusing the largest allocation. Reserving a
/// separate output would retain a second complete copy of the encoded areas.
fn assemble(mut parts: [Vec<u8>; 8], limit: usize) -> std::result::Result<Vec<u8>, MergeError> {
    let mut offsets = [0; 8];
    let mut total = 0usize;
    for (offset, part) in offsets.iter_mut().zip(&parts) {
        *offset = total;
        total = total
            .checked_add(part.len())
            .ok_or(MergeError::Limit("output bytes"))?;
    }
    check(total, limit, "output bytes")?;
    let largest = (0..parts.len())
        .max_by_key(|&i| parts[i].capacity())
        .unwrap();
    let mut out = std::mem::take(&mut parts[largest]);
    let old_len = out.len();
    out.try_reserve_exact(total - old_len)
        .map_err(|_| MergeError::Allocation)?;
    out.resize(total, 0);
    out.copy_within(0..old_len, offsets[largest]);
    for (part, offset) in parts.into_iter().zip(offsets) {
        out[offset..offset + part.len()].copy_from_slice(&part);
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::segment::SegmentBuilder;

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

    /// `inputs` segments of `docs` documents over `terms` terms with up to
    /// `repeat` occurrences each; `interleaved` spreads each input's
    /// documents across the heap, and every `deletion`th document (when
    /// nonzero) is dead.
    pub(crate) fn fixture(
        inputs: usize,
        docs: u32,
        terms: u32,
        repeat: u32,
        interleaved: bool,
        deletion: u32,
    ) -> (Vec<Vec<u8>>, Vec<BTreeSet<Tid>>) {
        let mut blobs = Vec::new();
        let mut dead = Vec::new();
        for input in 0..inputs {
            let mut builder = SegmentBuilder::default();
            let mut gone = BTreeSet::new();
            for d in 0..docs {
                let n = if interleaved {
                    d * inputs as u32 + input as u32
                } else {
                    input as u32 * docs + d
                };
                let tid = Tid::new(n / 5, (n % 5 + 1) as u16).unwrap();
                let mut position = 0u32;
                let mut tokens = Vec::new();
                for t in 0..terms {
                    if (n + t) % 3 == 0 {
                        continue;
                    }
                    for _ in 0..(1 + (n + t) % repeat.max(1)) {
                        position += 1;
                        tokens.push((format!("t{t:03}"), position));
                    }
                }
                if tokens.is_empty() {
                    tokens.push(("t000".to_owned(), 1));
                }
                builder
                    .add_document(tid, tokens.iter().map(|(t, p)| (t.as_str(), *p)))
                    .unwrap();
                if deletion != 0 && d % deletion == 0 {
                    gone.insert(tid);
                }
            }
            blobs.push(builder.finish());
            dead.push(gone);
        }
        (blobs, dead)
    }

    /// The merge by way of forward records, which the segment builder
    /// assembles independently of the streaming merge.
    fn reference(blobs: &[Vec<u8>], dead: &[BTreeSet<Tid>]) -> Result<Vec<u8>> {
        let mut builder = SegmentBuilder::default();
        for (bytes, dead) in blobs.iter().zip(dead) {
            for record in Segment::parse(bytes)?.records(|tid| dead.contains(&tid))? {
                builder.add_record(&record)?;
            }
        }
        Ok(builder.finish())
    }

    #[test]
    fn assembly_preserves_order_and_reuses_every_possible_area() {
        for largest in 0..8 {
            for empty_mask in 0..256 {
                let mut parts: [Vec<u8>; 8] = std::array::from_fn(|i| {
                    if empty_mask & (1 << i) != 0 {
                        Vec::new()
                    } else {
                        vec![i as u8 + 1; i * 3 + 1]
                    }
                });
                let expected = parts.concat();
                parts[largest].reserve_exact(256);
                let allocation = parts[largest].as_ptr();
                let actual = assemble(parts, expected.len()).unwrap();
                assert_eq!(actual, expected);
                assert_eq!(actual.as_ptr(), allocation);
            }
        }
        // Eight parts of 1..=8 bytes: the limit is the exact total.
        let parts = std::array::from_fn(|i| vec![i as u8; i + 1]);
        assert!(matches!(
            assemble(parts, 35),
            Err(MergeError::Limit("output bytes"))
        ));
        let parts = std::array::from_fn(|i| vec![i as u8; i + 1]);
        assert_eq!(assemble(parts.clone(), 36).unwrap(), parts.concat());
    }

    #[test]
    fn validated_merge_matches_reference() {
        for interleaved in [false, true] {
            for deletion in [0, 1, 7] {
                let (blobs, dead) = fixture(4, 65, 40, 67, interleaved, deletion);
                let merged = merge(&inputs(&blobs, &dead), limits(), || Ok(())).unwrap();
                assert_eq!(merged, reference(&blobs, &dead).unwrap());
                assert!(crate::verify::verify_segment(&merged).is_clean());
            }
        }
        let empty = merge(&[], limits(), || Ok(())).unwrap();
        assert_eq!(empty, reference(&[], &[]).unwrap());
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
                reference(&blobs, &dead).unwrap()
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
            reference(&blobs, &dead).unwrap()
        );
    }

    #[test]
    fn live_source_ownership_distinguishes_reused_tids_in_shared_terms() {
        let (mut blobs, _) = fixture(1, 3, 4, 2, true, 0);
        blobs.push(blobs[0].clone());
        let all_dead = crate::set::collect(Segment::parse(&blobs[0]).unwrap().documents().unwrap())
            .unwrap()
            .into_iter()
            .collect::<BTreeSet<_>>();
        for dead in [
            [all_dead.clone(), BTreeSet::new()],
            [BTreeSet::new(), all_dead.clone()],
        ] {
            assert_eq!(
                merge(&inputs(&blobs, &dead), limits(), || Ok(())).unwrap(),
                reference(&blobs, &dead).unwrap()
            );
        }
    }
}
