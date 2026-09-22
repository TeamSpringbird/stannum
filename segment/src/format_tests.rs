// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Cross-format tests: a fixed document set, a fixture captured from the
//! writer of each released segment format, and checks that every reader
//! handles every format.

use crate::Tid;
use crate::forward::ForwardRecord;
use crate::ordinals;
use crate::payload::SKIP_INTERVAL;
use crate::postings::{BLOCK_POSTINGS, BlockBound};
use crate::segment::{Format, PAGE_ENTRY, Segment, SegmentBuilder};
use crate::verify::{Severity, verify_segment};

/// The blob `write_current_fixture` produced when `LSG2` was current.
const LSG2_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/lsg2.segment");

/// Documents covering every codec path: dense and sparse locations, terms
/// with one, several and many score-bound blocks, payloads with and without
/// skip slots, and enough distinct terms for several dictionary blocks.
pub(crate) fn fixture_documents() -> Vec<(Tid, Vec<(String, u32)>)> {
    let mut docs = Vec::new();
    // 300 documents on three dense pages.
    for i in 0..300u32 {
        let tid = Tid::new(i / 100, (i % 100 + 1) as u16).unwrap();
        docs.push((tid, fixture_tokens(i)));
    }
    // 150 documents spread far apart.
    for i in 300..450u32 {
        let tid = Tid::new(1000 + (i - 300) * 37, (i % 3 + 1) as u16).unwrap();
        docs.push((tid, fixture_tokens(i)));
    }
    docs
}

fn fixture_tokens(i: u32) -> Vec<(String, u32)> {
    let mut words: Vec<String> = Vec::new();
    // Every document, with term frequencies across several buckets.
    for _ in 0..(1 + (i * 7) % 13) {
        words.push("common".to_owned());
    }
    if i.is_multiple_of(50) {
        for _ in 0..100 {
            words.push("loud".to_owned());
        }
    }
    if i.is_multiple_of(2) {
        words.push("even".to_owned()); // df 225: two blocks
    }
    if i.is_multiple_of(3) {
        words.push("third".to_owned()); // df 150: two blocks
    }
    if i < 128 {
        words.push("block".to_owned()); // df 128: exactly one full block
    }
    if i < 129 {
        words.push("spill".to_owned()); // df 129: a full block and one more
    }
    if i.is_multiple_of(5) {
        words.push("fifth".to_owned()); // df 90: one block, several skip slots
    }
    words.push(format!("t{:04}", i % 200)); // df 2 or 3; 200 terms span dictionary blocks
    if i.is_multiple_of(4) {
        words.push(format!("u{i:04}")); // df 1
    }
    for k in 0..(i % 17) * 3 {
        words.push(format!("pad{}", k % 5));
    }
    words
        .into_iter()
        .enumerate()
        .map(|(position, word)| (word, position as u32 + 1))
        .collect()
}

pub(crate) fn build_as(documents: &[(Tid, Vec<(String, u32)>)], format: Format) -> Vec<u8> {
    let mut builder = SegmentBuilder::default();
    for (tid, tokens) in documents {
        builder
            .add_document(*tid, tokens.iter().map(|(t, p)| (t.as_str(), *p)))
            .unwrap();
    }
    builder.finish_as(format)
}

pub(crate) fn build_current(documents: &[(Tid, Vec<(String, u32)>)]) -> Vec<u8> {
    build_as(documents, Format::CURRENT)
}

/// What a reader learns from a segment regardless of its format: every
/// document, and per term the bounds a ranked scan prunes with.
pub(crate) type Contents = (Vec<ForwardRecord>, Vec<(String, Vec<BlockBound>)>);

pub(crate) fn contents(bytes: &[u8]) -> crate::Result<Contents> {
    let segment = Segment::parse(bytes)?;
    let records = segment.records(|_| false)?;
    let mut bounds = Vec::new();
    for item in segment.dictionary()?.iter() {
        let (term, entry) = item?;
        let resolved = segment.resolve(entry)?;
        let table = resolved.cursor()?.block_bounds()?;
        // `bound_at` from a fresh cursor agrees with the table at every
        // posting, whichever layout the bounds are stored in.
        let mut cursor = resolved.cursor()?;
        for (ordinal, tid) in resolved.postings()?.to_vec()?.into_iter().enumerate() {
            let expected = table.get(ordinal / BLOCK_POSTINGS as usize).copied();
            if cursor.bound_at(tid)? != expected {
                return Err(crate::Error::Corrupt("bound_at disagrees with the table"));
            }
        }
        bounds.push((term, table));
    }
    Ok((records, bounds))
}

fn messages(report: &crate::verify::SegmentReport) -> String {
    report
        .findings
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Writes `tests/fixtures/<magic>.segment` from the current writer. Run by
/// hand before changing the writer, so the old format stays covered.
#[test]
#[ignore = "writes the current format's fixture; run by hand when the format changes"]
fn write_current_fixture() {
    let bytes = build_current(&fixture_documents());
    let magic = std::str::from_utf8(&bytes[..4])
        .unwrap()
        .to_ascii_lowercase();
    let path = format!(
        "{}/tests/fixtures/{magic}.segment",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::write(&path, &bytes).unwrap();
    eprintln!("wrote {} bytes to {path}", bytes.len());
}

#[test]
fn fixture_documents_cover_every_block_count_of_interest() {
    let segment = Segment::parse(LSG2_FIXTURE).unwrap();
    let df = |term: &str| segment.term(term).unwrap().unwrap().df();
    assert_eq!(df("u0000"), 1);
    assert_eq!(df("t0000"), 3);
    assert_eq!(df("fifth"), 90);
    assert_eq!(df("block"), BLOCK_POSTINGS);
    assert_eq!(df("spill"), BLOCK_POSTINGS + 1);
    assert_eq!(df("third"), 150);
    assert_eq!(df("even"), 225);
    assert_eq!(df("common"), 450);
    assert!(segment.dictionary().unwrap().index().blocks() >= 4);
    // Dense one-block and two-block terms are grouped; the far-apart
    // documents make `common` sparse, as every rare term is.
    let form = |term: &str| {
        segment
            .term(term)
            .unwrap()
            .unwrap()
            .postings()
            .unwrap()
            .is_grouped()
    };
    assert!(form("block") && form("spill") && !form("common") && !form("u0400"));
}

#[test]
fn lsg2_fixture_reads_like_the_current_format() {
    assert_eq!(&LSG2_FIXTURE[..4], Format::Lsg2.magic());
    let documents = fixture_documents();
    // The compatibility writer reproduces the captured bytes exactly, so
    // the proptests over it exercise the released layout.
    assert_eq!(build_as(&documents, Format::Lsg2), LSG2_FIXTURE);
    let report = verify_segment(LSG2_FIXTURE);
    assert!(report.is_clean(), "{}", messages(&report));
    assert_eq!(report.format, Some(Format::Lsg2));
    assert!(!report.legacy);

    // Same documents, and the same bounds for every term: a ranked scan
    // prunes an LSG2 segment and its rewrite in a later format identically.
    let (old_records, old_bounds) = contents(LSG2_FIXTURE).unwrap();
    for format in [Format::Lsg3, Format::CURRENT] {
        let rewrite = build_as(&documents, format);
        assert_eq!(&rewrite[..4], format.magic());
        let report = verify_segment(&rewrite);
        assert!(report.is_clean(), "{}", messages(&report));
        assert_eq!(report.format, Some(format));
        let (new_records, new_bounds) = contents(&rewrite).unwrap();
        assert_eq!(old_records, new_records);
        assert_eq!(old_bounds, new_bounds);
        assert!(new_bounds.iter().all(|(_, bounds)| !bounds.is_empty()));
    }
    // The LSG3 stream layouts are smaller; LSG4 spends some of that on its
    // ordinals area and page table, and nothing else.
    let lsg3 = build_as(&documents, Format::Lsg3);
    let current = build_current(&documents);
    assert_eq!(&current[..4], Format::CURRENT.magic());
    assert!(lsg3.len() < LSG2_FIXTURE.len());
    let (old, new) = (
        Segment::parse(&lsg3).unwrap().sections(),
        Segment::parse(&current).unwrap().sections(),
    );
    assert_eq!(
        (new.postings, new.payload, new.docs, new.lengths),
        (old.postings, old.payload, old.docs, old.lengths)
    );
    assert_eq!(
        current.len() - lsg3.len(),
        new.ordinals + new.pages + (new.header - old.header) + (new.dictionary - old.dictionary)
    );
}

#[test]
fn lsg4_appends_an_ordinal_stream_per_term_and_a_page_table() {
    let documents = fixture_documents();
    let current = build_current(&documents);
    assert_eq!(Format::CURRENT, Format::Lsg5);
    assert_eq!(&current[..4], b"LSG5");
    let segment = Segment::parse(&current).unwrap();
    let sections = segment.sections();
    assert!(sections.ordinals != 0 && sections.pages != 0);
    assert_eq!(
        sections.header
            + sections.dictionary
            + sections.postings
            + sections.payload
            + sections.docs
            + sections.lengths
            + sections.ordinals
            + sections.pages,
        current.len()
    );

    // Every term's stream names its postings' documents by table ordinal, in
    // order, and the streams tile the ordinals area in dictionary order.
    let table = crate::set::collect(segment.documents().unwrap()).unwrap();
    let mut ordinals_end = 0;
    for item in segment.dictionary().unwrap().iter() {
        let (term, entry) = item.unwrap();
        assert_eq!(entry.ordinals.offset, ordinals_end, "{term}");
        ordinals_end += u64::from(entry.ordinals.len);
        let resolved = segment.resolve(entry).unwrap();
        let expected: Vec<u32> = resolved
            .postings()
            .unwrap()
            .to_vec()
            .unwrap()
            .iter()
            .map(|tid| table.binary_search(tid).unwrap() as u32)
            .collect();
        let stream = resolved.ordinals().unwrap().unwrap();
        assert_eq!(stream.count(), entry.df, "{term}");
        assert_eq!(stream.to_vec().unwrap(), expected, "{term}");
    }
    assert_eq!(ordinals_end, sections.ordinals as u64);

    // A rare term costs a few bytes: its count, one ordinal and its bound,
    // most of which is the byte per sub-block.
    let rare = segment.term("u0448").unwrap().unwrap();
    assert_eq!(rare.df(), 1);
    assert!(
        rare.entry.ordinals.len <= 3 + 5,
        "{}",
        rare.entry.ordinals.len
    );
    assert_eq!(rare.ordinals().unwrap().unwrap().to_vec().unwrap(), [448]);
    // A term in every document is one array chunk: two bytes a document, and
    // one bound.
    let common = segment.term("common").unwrap().unwrap();
    assert!(common.entry.ordinals.len as usize <= 450 * 2 + 16 + 90 + ordinals::SUBS + 10);

    // The page table: a (block, first ordinal) entry per heap block, ascending.
    let pages: Vec<(u32, u32)> = segment
        .page_table()
        .unwrap()
        .chunks_exact(PAGE_ENTRY)
        .map(|entry| {
            (
                u32::from_le_bytes(entry[..4].try_into().unwrap()),
                u32::from_le_bytes(entry[4..].try_into().unwrap()),
            )
        })
        .collect();
    assert_eq!(pages.len() * PAGE_ENTRY, sections.pages);
    assert_eq!(pages.len(), 3 + 150);
    assert_eq!(pages[..4], [(0, 0), (1, 100), (2, 200), (1000, 300)]);
    assert_eq!(pages[152], (1000 + 149 * 37, 449));

    // The same documents written as LSG3 carry neither section, and their
    // terms report no ordinals rather than failing.
    let older = build_as(&documents, Format::Lsg3);
    assert_eq!(&older[..4], b"LSG3");
    let segment = Segment::parse(&older).unwrap();
    assert_eq!(
        (segment.sections().ordinals, segment.sections().pages),
        (0, 0)
    );
    assert_eq!(segment.page_table().unwrap(), b"");
    for item in segment.dictionary().unwrap().iter() {
        let (term, entry) = item.unwrap();
        assert_eq!(entry.ordinals, crate::dictionary::Extent::default());
        assert!(
            segment
                .resolve(entry)
                .unwrap()
                .ordinals()
                .unwrap()
                .is_none(),
            "{term}"
        );
    }
    assert!(verify_segment(&older).is_clean());
}

#[test]
fn lsg1_segments_read_without_bounds() {
    let documents = fixture_documents();
    let old = build_as(&documents, Format::Lsg1);
    assert_eq!(&old[..4], Format::Lsg1.magic());
    let report = verify_segment(&old);
    assert!(report.legacy);
    assert_eq!(report.format, Some(Format::Lsg1));
    assert!(
        report
            .findings
            .iter()
            .all(|f| f.severity == Severity::Warning && f.message.contains("LSG1")),
        "{}",
        messages(&report)
    );
    let (old_records, old_bounds) = contents(&old).unwrap();
    let (new_records, _) = contents(&build_current(&documents)).unwrap();
    assert_eq!(old_records, new_records);
    assert!(old_bounds.iter().all(|(_, bounds)| bounds.is_empty()));
    let segment = Segment::parse(&old).unwrap();
    assert!(
        !segment
            .term("common")
            .unwrap()
            .unwrap()
            .cursor()
            .unwrap()
            .has_bounds()
    );
}

#[test]
fn stream_layouts_follow_the_format() {
    let lsg3 = build_as(&fixture_documents(), Format::Lsg3);
    let current = build_current(&fixture_documents());
    for (bytes, format) in [
        (LSG2_FIXTURE, Format::Lsg2),
        (&lsg3[..], Format::Lsg3),
        (&current[..], Format::Lsg5),
    ] {
        let segment = Segment::parse(bytes).unwrap();
        assert_eq!(segment.format(), format);
        let mut term_bounds = 0;
        let mut tables = 0;
        let mut multi_block = 0;
        for item in segment.dictionary().unwrap().iter() {
            let (term, entry) = item.unwrap();
            let resolved = segment.resolve(entry).unwrap();
            let postings = resolved.postings().unwrap();
            assert!(postings.has_bounds(), "{format} {term}");
            let one_block = postings.count() <= BLOCK_POSTINGS;
            assert_eq!(
                postings.has_term_bound(),
                format.streams() == Format::Lsg3 && one_block,
                "{format} {term}"
            );
            term_bounds += usize::from(postings.has_term_bound());
            tables += usize::from(!postings.has_term_bound());
            multi_block += usize::from(!one_block);
            let payload = resolved.payload().unwrap();
            let slots = (entry.df as usize).div_ceil(SKIP_INTERVAL as usize);
            let expected = if format.streams() == Format::Lsg3 {
                slots - 1
            } else {
                slots
            };
            assert_eq!(payload.skip_table_len(), expected * 4, "{format} {term}");
        }
        assert!(multi_block >= 4, "{multi_block}");
        match format.streams() {
            Format::Lsg3 => assert!(term_bounds > 300 && tables == multi_block),
            _ => assert!(term_bounds == 0 && tables > 300),
        }
    }
}

#[test]
fn verifier_warns_about_stream_layouts_of_another_format() {
    let documents = fixture_documents();
    // LSG2 tables inside an LSG3 blob: readable, but not what LSG3 writes.
    let mixed = SegmentBuilder::from_documents(&documents).finish_mixed(
        Format::Lsg3,
        Format::Lsg2,
        Format::Lsg3,
    );
    let report = verify_segment(&mixed);
    assert!(
        report
            .findings
            .iter()
            .all(|f| f.severity == Severity::Warning && f.message.contains("term bound")),
        "{}",
        messages(&report)
    );
    // One warning per one-block term, none for the multi-block ones.
    assert!(report.findings.len() > 300, "{}", report.findings.len());
    assert!(
        report
            .findings
            .iter()
            .all(|f| !f.location.contains("common"))
    );
    let (records, bounds) = contents(&mixed).unwrap();
    assert_eq!(
        (records, bounds),
        contents(&build_current(&documents)).unwrap()
    );
    // Term bounds inside an LSG2 blob, likewise.
    let mixed = SegmentBuilder::from_documents(&documents).finish_mixed(
        Format::Lsg2,
        Format::Lsg3,
        Format::Lsg2,
    );
    let report = verify_segment(&mixed);
    assert!(
        report
            .findings
            .iter()
            .all(|f| f.severity == Severity::Warning && f.message.contains("LSG3 term bound")),
        "{}",
        messages(&report)
    );
    assert!(report.findings.len() > 300);
}
