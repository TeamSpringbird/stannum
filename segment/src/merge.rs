// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

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
        let mut document_at = 0;
        if input.dead.iter().any(|tid| {
            crate::verify::ordered_rank(&report.documents, &mut document_at, *tid).is_none()
        }) {
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
// Complete validation proved source membership; the map contains every live
// document. A missing/mismatched owner therefore denotes this source's dead
// occurrence, including a TID reused live by another source.
fn skip_dead(
    postings: &mut crate::postings::PostingsCursor<'_>,
    payload: &mut crate::payload::PayloadCursor<'_>,
    live_lengths: &HashMap<Tid, (u32, usize, u32)>,
    source: usize,
    checkpoint: &mut impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<Option<(u32, u32)>, MergeError> {
    while let Some(tid) = postings.current() {
        if let Some(&(length, owner, ordinal)) = live_lengths.get(&tid)
            && owner == source
        {
            return Ok(Some((length, ordinal)));
        }
        checkpoint()?;
        payload.next_bucket()?;
        postings.advance()?;
    }
    Ok(None)
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
    let mut page_documents = Vec::new();
    let mut doc_builder = PostingsBuilder::default();
    let mut length_bytes = Vec::new();
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
    let mut ordinals_area = Vec::new();
    let mut ordinals = Vec::new();
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
            let mut postings = resolved.cursor()?;
            if resolved.df() == 1
                && postings.current().is_some_and(|tid| {
                    live_lengths
                        .get(&tid)
                        .is_none_or(|&(_, owner, _)| owner != i)
                })
            {
                // This fully validated source term has no surviving posting.
                // No payload cursor will be consumed for it.
                checkpoint()?;
                continue;
            }
            let mut payload = resolved.payload()?.cursor();
            let length = skip_dead(
                &mut postings,
                &mut payload,
                &live_lengths,
                i,
                &mut checkpoint,
            )?;
            if let Some(tid) = postings.current() {
                postings_heap.push(Reverse((tid, cursors.len())));
            }
            cursors.push((i, postings, payload, length));
        }
        let mut postings = PostingsBuilder::default();
        let mut payload = PayloadBuilder::default();
        let mut count = 0u32;
        let mut max_bucket = 0;
        ordinals.clear();
        while let Some(Reverse((tid, c))) = postings_heap.pop() {
            checkpoint()?;
            let (i, cursor, positions_cursor, length) = &mut cursors[c];
            positions.clear();
            let bucket = positions_cursor.next_into(&mut positions)?;
            let (len, ordinal) = length.ok_or(Error::Corrupt("posting missing document"))?;
            ordinals.push(ordinal);
            postings.push_scored(tid, bucket, len)?;
            payload.push(bucket, &positions)?;
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
            let posting_bytes = postings.finish_as(format.streams());
            let payload_bytes = payload.finish_as(format.streams());
            let ordinal_bytes = if format.has_ordinals() {
                crate::ordinals::encode(&ordinals)
            } else {
                Vec::new()
            };
            check(
                postings_area
                    .len()
                    .checked_add(payload_area.len())
                    .and_then(|n| n.checked_add(posting_bytes.len()))
                    .and_then(|n| n.checked_add(payload_bytes.len()))
                    .and_then(|n| n.checked_add(ordinals_area.len()))
                    .and_then(|n| n.checked_add(ordinal_bytes.len()))
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
                    ordinals: Extent {
                        offset: ordinals_area.len() as u64,
                        len: u32::try_from(ordinal_bytes.len())
                            .map_err(|_| MergeError::Limit("ordinal extent"))?,
                    },
                },
            )?;
            postings_area.extend_from_slice(&posting_bytes);
            payload_area.extend_from_slice(&payload_bytes);
            ordinals_area.extend_from_slice(&ordinal_bytes);
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
    let documents = doc_builder.finish();
    let mut header = Vec::new();
    header.extend_from_slice(format.magic());
    for n in [
        live_lengths.len() as u64,
        total_length,
        dictionary.len() as u64,
        postings_area.len() as u64,
        payload_area.len() as u64,
        documents.len() as u64,
    ] {
        varint::put(&mut header, n);
    }
    let pages = if format.has_ordinals() {
        crate::segment::page_table(page_documents.iter().copied())
    } else {
        Vec::new()
    };
    drop(page_documents);
    if format.has_ordinals() {
        varint::put(&mut header, ordinals_area.len() as u64);
        varint::put(&mut header, pages.len() as u64);
    }
    drop(live_lengths);
    let out = assemble(
        [
            header,
            dictionary,
            postings_area,
            payload_area,
            documents,
            length_bytes,
            ordinals_area,
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
mod tests {
    use super::*;
    use crate::direct_merge_poc::{FORMATS, fixture, reference};

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
    fn validated_merge_matches_reference_for_all_formats() {
        for interleaved in [false, true] {
            for deletion in [0, 1, 7] {
                let (blobs, dead) = fixture(4, 65, 40, 67, interleaved, deletion);
                for format in FORMATS {
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
                reference(&blobs, &dead, Format::CURRENT).unwrap()
            );
        }
    }

    /// Every term's ordinal stream against its postings and the document
    /// table, and the page table against the documents, read independently of
    /// the verifier.
    fn assert_ordinals_name_postings(bytes: &[u8]) {
        let segment = Segment::parse(bytes).unwrap();
        let documents = crate::set::collect(segment.documents().unwrap()).unwrap();
        for item in segment.dictionary().unwrap().iter() {
            let (term, entry) = item.unwrap();
            let resolved = segment.resolve(entry).unwrap();
            let named: Vec<Tid> = resolved
                .ordinals()
                .unwrap()
                .unwrap()
                .to_vec()
                .unwrap()
                .into_iter()
                .map(|ordinal| documents[ordinal as usize])
                .collect();
            assert_eq!(
                named,
                resolved.postings().unwrap().to_vec().unwrap(),
                "{term}"
            );
        }
        let mut pages = Vec::new();
        for (ordinal, tid) in documents.iter().enumerate() {
            if ordinal == 0 || documents[ordinal - 1].block != tid.block {
                pages.push((tid.block, ordinal as u32));
            }
        }
        let stored: Vec<(u32, u32)> = segment
            .page_table()
            .unwrap()
            .chunks_exact(crate::segment::PAGE_ENTRY)
            .map(|entry| {
                (
                    u32::from_le_bytes(entry[..4].try_into().unwrap()),
                    u32::from_le_bytes(entry[4..].try_into().unwrap()),
                )
            })
            .collect();
        assert_eq!(stored, pages);
    }

    #[test]
    fn lsg4_merges_renumber_ordinals_and_pages_across_deletions_and_tid_reuse() {
        // Eight inputs in every format, 4,800 documents on 48 pages. Dropping
        // every seventh document shifts each later ordinal; the shared terms
        // span list, array and bitmap containers.
        for interleaved in [false, true] {
            let (mut blobs, mut dead) = fixture(8, 600, 6, 5, interleaved, 7);
            assert!(dead.iter().all(|dead| !dead.is_empty()));
            // A further `LSG4` input reuses TIDs that are dead in the inputs
            // above, under a term of its own and the most common shared one.
            let mut reuse = crate::segment::SegmentBuilder::default();
            for (n, tid) in dead.iter().flatten().step_by(3).enumerate() {
                let rare = format!("rare{:02}", n % 40);
                reuse
                    .add_document(*tid, [("reused", 1), ("term0000", 2), (rare.as_str(), 3)])
                    .unwrap();
            }
            blobs.push(reuse.finish_as(Format::Lsg4));
            dead.push(BTreeSet::new());
            let output = merge(&inputs(&blobs, &dead), limits(), || Ok(())).unwrap();
            assert_eq!(&output[..4], b"LSG4");
            assert_eq!(output, reference(&blobs, &dead, Format::Lsg4).unwrap());
            let report = crate::verify::verify_segment(&output);
            assert!(report.is_clean(), "{:?}", report.findings);
            assert_ordinals_name_postings(&output);
            let segment = Segment::parse(&output).unwrap();
            let common = segment.term("term0000").unwrap().unwrap();
            assert!(common.df() as usize >= crate::ordinals::ARRAY_MAX);
            let reused = segment.term("reused").unwrap().unwrap();
            assert!(reused.df() as usize > crate::ordinals::LIST_MAX);
            let rare = segment.term("rare00").unwrap().unwrap();
            assert!((2..=crate::ordinals::LIST_MAX).contains(&(rare.df() as usize)));
            // Merging the merged segment again, with more deletions, renumbers
            // from `LSG4` ordinals rather than carrying them over.
            let again_dead = [report.documents.iter().copied().step_by(5).collect()];
            let again_blobs = [output];
            let again = merge(&inputs(&again_blobs, &again_dead), limits(), || Ok(())).unwrap();
            assert_eq!(
                again,
                reference(&again_blobs, &again_dead, Format::Lsg4).unwrap()
            );
            assert!(crate::verify::verify_segment(&again).is_clean());
            assert_ordinals_name_postings(&again);
        }
    }

    #[test]
    fn corrupt_lengths_are_rejected_even_for_dead_documents() {
        for format in FORMATS {
            let (sources, mut dead) = fixture(1, 1, 4, 2, true, 0);
            let mut blobs = vec![reference(&sources, &dead, format).unwrap()];
            // The length table holds one u32 for the only document, ahead of
            // the sections `LSG4` appends. Its header total and actual positions
            // still say four, so trusting it would corrupt BM25.
            let sections = Segment::parse(&blobs[0]).unwrap().sections();
            let n = blobs[0].len() - sections.ordinals - sections.pages;
            assert_eq!(blobs[0][n - 4..n], 4u32.to_le_bytes());
            blobs[0][n - 4..n].copy_from_slice(&5u32.to_le_bytes());
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
    }

    #[test]
    fn corrupt_ordinals_and_pages_are_rejected_even_though_a_merge_rebuilds_them() {
        let (sources, dead) = fixture(1, 3, 4, 2, true, 0);
        let blob = reference(&sources, &dead, Format::Lsg4).unwrap();
        let sections = Segment::parse(&blob).unwrap().sections();
        let appended = sections.ordinals + sections.pages;
        assert!(sections.ordinals != 0 && sections.pages != 0);
        for at in blob.len() - appended..blob.len() {
            let mut changed = blob.clone();
            changed[at] ^= 1;
            assert!(
                matches!(
                    merge(&inputs(&[changed], &dead), limits(), || Ok(())),
                    Err(MergeError::InvalidInput { index: 0, .. })
                ),
                "byte {at}"
            );
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
