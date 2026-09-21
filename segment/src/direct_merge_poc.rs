// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Experimental, test-only direct merge. No production call sites or new format.
//! Run: cargo test -p segment --release direct_merge_poc -- --include-ignored --nocapture
use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap};
use std::time::Instant;

use crate::dictionary::{DictionaryBuilder, Extent, TermEntry};
use crate::payload::PayloadBuilder;
use crate::postings::PostingsBuilder;
use crate::segment::{Format, Segment, SegmentBuilder};
use crate::set::Cursor;
use crate::{Error, Result, Tid, varint};

// A document-length lookup remains O(live documents). Input/output blobs and
// per-term encoding buffers remain resident. This is not bounded-memory I/O.
fn direct(blobs: &[Vec<u8>], dead: &[BTreeSet<Tid>], format: Format) -> Result<Vec<u8>> {
    let segments = blobs
        .iter()
        .map(|b| Segment::parse(b))
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
    // Each live document's length and its ordinal in the output table.
    let mut live_lengths = HashMap::new();
    let mut live_documents = Vec::new();
    let mut doc_builder = PostingsBuilder::default();
    let mut length_bytes = Vec::new();
    let mut total_length = 0u64;
    while let Some(Reverse((tid, i))) = heap.pop() {
        if !dead[i].contains(&tid) {
            let len = segments[i].length_at(docs[i].ordinal())?;
            let ordinal = live_documents.len() as u32;
            if live_lengths.insert(tid, (len, ordinal)).is_some() {
                return Err(Error::Unordered);
            }
            live_documents.push(tid);
            doc_builder.push(tid)?;
            length_bytes.extend_from_slice(&len.to_le_bytes());
            total_length += u64::from(len);
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
    while let Some(Reverse((term, first))) = terms.pop() {
        let mut inputs = vec![first];
        while terms.peek().is_some_and(|Reverse((next, _))| next == &term) {
            inputs.push(terms.pop().unwrap().0.1);
        }
        let mut cursors = Vec::new();
        let mut postings_heap = BinaryHeap::new();
        for &i in &inputs {
            let resolved = segments[i].resolve(entries[i].take().unwrap())?;
            let postings = resolved.cursor()?;
            let payload = resolved.payload()?.cursor();
            if let Some(tid) = postings.current() {
                postings_heap.push(Reverse((tid, cursors.len())));
            }
            cursors.push((i, postings, payload));
        }
        let mut postings = PostingsBuilder::default();
        let mut payload = PayloadBuilder::default();
        let mut count = 0u32;
        let mut max_bucket = 0;
        ordinals.clear();
        while let Some(Reverse((tid, c))) = postings_heap.pop() {
            let (i, cursor, positions_cursor) = &mut cursors[c];
            if dead[*i].contains(&tid) {
                positions_cursor.next_bucket()?;
            } else {
                positions.clear();
                let bucket = positions_cursor.next_into(&mut positions)?;
                let (len, ordinal) = *live_lengths
                    .get(&tid)
                    .ok_or(Error::Corrupt("posting missing document"))?;
                ordinals.push(ordinal);
                postings.push_scored(tid, bucket, len)?;
                payload.push(bucket, &positions)?;
                count += 1;
                max_bucket = max_bucket.max(bucket);
            }
            cursor.advance()?;
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
            dictionary.push(
                &term,
                TermEntry {
                    df: count,
                    max_tf_bucket: max_bucket,
                    postings: Extent {
                        offset: postings_area.len() as u64,
                        len: posting_bytes.len() as u32,
                    },
                    payload: Extent {
                        offset: payload_area.len() as u64,
                        len: payload_bytes.len() as u32,
                    },
                    ordinals: Extent {
                        offset: ordinals_area.len() as u64,
                        len: ordinal_bytes.len() as u32,
                    },
                },
            )?;
            postings_area.extend_from_slice(&posting_bytes);
            payload_area.extend_from_slice(&payload_bytes);
            ordinals_area.extend_from_slice(&ordinal_bytes);
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
    let pages = if format.has_ordinals() {
        crate::segment::page_table(live_documents.iter().copied())
    } else {
        Vec::new()
    };
    if format.has_ordinals() {
        varint::put(&mut out, ordinals_area.len() as u64);
        varint::put(&mut out, pages.len() as u64);
    }
    // The two sections `LSG4` appends are empty before it.
    for bytes in [
        &dictionary,
        &postings_area,
        &payload_area,
        &documents,
        &length_bytes,
        &ordinals_area,
        &pages,
    ] {
        out.extend_from_slice(bytes);
    }
    Ok(out)
}

pub(crate) fn reference(
    blobs: &[Vec<u8>],
    dead: &[BTreeSet<Tid>],
    format: Format,
) -> Result<Vec<u8>> {
    let mut builder = SegmentBuilder::default();
    for (blob, dead) in blobs.iter().zip(dead) {
        for record in Segment::parse(blob)?.records(|tid| dead.contains(&tid))? {
            builder.add_record(&record)?;
        }
    }
    Ok(builder.finish_as(format))
}

/// Every format, so fixtures mix inputs and tests write each of them.
pub(crate) const FORMATS: [Format; 4] = [Format::Lsg1, Format::Lsg2, Format::Lsg3, Format::Lsg4];

pub(crate) fn fixture(
    parts: usize,
    docs: usize,
    tokens: usize,
    vocab: usize,
    interleaved: bool,
    deletion: usize,
) -> (Vec<Vec<u8>>, Vec<BTreeSet<Tid>>) {
    let mut blobs = Vec::new();
    let mut dead = Vec::new();
    for part in 0..parts {
        let mut builder = SegmentBuilder::default();
        let mut deleted = BTreeSet::new();
        for doc in 0..docs {
            let id = if interleaved {
                doc * parts + part
            } else {
                part * docs + doc
            };
            let tid = Tid::new((id / 100) as u32, (id % 100 + 1) as u16).unwrap();
            let terms = (0..tokens)
                .map(|p| format!("term{:04}", (p + id) % vocab))
                .collect::<Vec<_>>();
            builder
                .add_document(
                    tid,
                    terms
                        .iter()
                        .enumerate()
                        .map(|(p, t)| (t.as_str(), p as u32 + 1)),
                )
                .unwrap();
            if deletion != 0 && id % deletion == 0 {
                deleted.insert(tid);
            }
        }
        blobs.push(builder.finish_as(FORMATS[part % FORMATS.len()]));
        dead.push(deleted);
    }
    (blobs, dead)
}

fn assert_verified(bytes: &[u8], format: Format) {
    let report = crate::verify::verify_segment(bytes);
    if format == Format::Lsg1 {
        assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
        let finding = &report.findings[0];
        assert_eq!(finding.severity, crate::verify::Severity::Warning);
        assert_eq!(finding.location, "header");
        assert_eq!(
            finding.message,
            "LSG1 segment: ranked scans over it score every candidate; REINDEX to upgrade"
        );
    } else {
        assert!(report.is_clean(), "{:?}", report.findings);
    }
}

#[test]
fn direct_merge_poc_matches_all_formats_and_deletions() {
    for interleaved in [false, true] {
        for deletion in [0, 1, 7] {
            let (blobs, dead) = fixture(8, 65, 40, 67, interleaved, deletion);
            for format in FORMATS {
                let actual = direct(&blobs, &dead, format).unwrap();
                assert_eq!(actual, reference(&blobs, &dead, format).unwrap());
                assert_verified(&actual, format);
            }
        }
    }
    for parts in [0, 1, 8] {
        let (blobs, dead) = fixture(parts, 0, 0, 1, false, 0);
        assert_eq!(
            direct(&blobs, &dead, Format::CURRENT),
            reference(&blobs, &dead, Format::CURRENT)
        );
    }
}

#[test]
fn direct_merge_poc_duplicate_live_tid_fails_but_dead_reuse_survives() {
    let (mut blobs, mut dead) = fixture(1, 1, 40, 4, false, 0);
    blobs.push(blobs[0].clone());
    dead.push(BTreeSet::new());
    assert_eq!(
        direct(&blobs, &dead, Format::CURRENT),
        Err(Error::Unordered)
    );
    assert_eq!(
        reference(&blobs, &dead, Format::CURRENT),
        Err(Error::Unordered)
    );
    dead[0].insert(Tid::new(0, 1).unwrap());
    assert_eq!(
        direct(&blobs, &dead, Format::CURRENT),
        reference(&blobs, &dead, Format::CURRENT)
    );
}

#[test]
fn direct_merge_poc_sparse_tids_unicode_and_disjoint_vocabulary() {
    let mut blobs = Vec::new();
    for part in 0..4 {
        let mut builder = SegmentBuilder::default();
        for doc in 0..150 {
            let id = doc * 4 + part;
            let tid = Tid::new(
                if id == 599 {
                    crate::tid::MAX_BLOCK
                } else {
                    id * 1024
                },
                if id == 599 { crate::tid::MAX_OFFSET } else { 1 },
            )
            .unwrap();
            let unique = format!("独自{part}-{doc}");
            builder
                .add_document(tid, [("café", 1), (unique.as_str(), 257), ("café", 65536)])
                .unwrap();
        }
        blobs.push(builder.finish_as(FORMATS[part as usize]));
    }
    let dead = vec![BTreeSet::new(); blobs.len()];
    for format in FORMATS {
        let actual = direct(&blobs, &dead, format).unwrap();
        assert_eq!(actual, reference(&blobs, &dead, format).unwrap());
        assert_verified(&actual, format);
    }
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(32))]
    #[test]
    fn direct_merge_poc_generated_equivalence(parts in 1usize..10, docs in 1usize..80, tokens in 1usize..80, vocab in 1usize..100, deletion in 0usize..10, interleaved in proptest::bool::ANY) {
        let (blobs, dead) = fixture(parts, docs, tokens, vocab, interleaved, deletion);
        proptest::prop_assert_eq!(direct(&blobs, &dead, Format::CURRENT), reference(&blobs, &dead, Format::CURRENT));
    }
}

#[test]
#[ignore = "release-only timing probe; not a database performance claim"]
fn direct_merge_poc_microprobe() {
    for (docs, tokens, vocab, interleaved) in [
        (32, 40, 4, true),
        (512, 400, 4, true),
        (512, 400, 400, true),
        (512, 400, 400, false),
    ] {
        let (blobs, dead) = fixture(8, docs, tokens, vocab, interleaved, 7);
        let expected = reference(&blobs, &dead, Format::CURRENT).unwrap();
        let mut times = [Vec::new(), Vec::new()];
        for round in 0..10 {
            for method in if round % 2 == 0 { [0, 1] } else { [1, 0] } {
                let start = Instant::now();
                let actual = if method == 0 {
                    reference(&blobs, &dead, Format::CURRENT)
                } else {
                    direct(&blobs, &dead, Format::CURRENT)
                }
                .unwrap();
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(actual, expected);
                if round >= 2 {
                    times[method].push(ms);
                }
            }
        }
        for t in &mut times {
            t.sort_by(f64::total_cmp);
        }
        let median = |t: &[f64]| (t[3] + t[4]) / 2.0;
        println!(
            "8 x {docs} docs, {tokens} tokens/doc, vocabulary {vocab}, interleaved={interleaved}: reference {:.3} ms, direct {:.3} ms, bytes {}; samples {:?}",
            median(&times[0]),
            median(&times[1]),
            expected.len(),
            times
        );
    }
}
