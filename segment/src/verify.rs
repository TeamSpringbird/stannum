// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Whole-blob consistency checks that list every problem found instead of
//! stopping at the first one.
//!
//! The readers in this crate fail on the first malformed byte they touch,
//! which is right for a query but useless for an operator asking "what is
//! wrong with this index?". The checkers here decode a whole segment, dead
//! list or forward stream and collect [`Finding`]s: each names a location
//! inside the blob and says what disagrees with what. A valid blob yields no
//! findings. Nothing here can panic on malformed input; every decode goes
//! through the bounds-checked readers.
//!
//! What a segment check covers:
//!
//! * the header: magic, varints and the total length;
//! * the dictionary: block index in order, every block decoded exactly,
//!   each block's first term matching the index, terms increasing across
//!   blocks;
//! * every term: ordinals and payload extents inside their areas and not
//!   overlapping the previous term's, the ordinal stream well formed (see
//!   [`crate::ordinals::validate`]) with `df` members below the document
//!   count, the positions stream holding one entry per member whose count
//!   matches the member's bucket, `max_tf_bucket` equal to the largest
//!   bucket, and the stream's chunk bounds equal to what the buckets and
//!   document lengths imply;
//! * the document table: a well-formed page table and `doc_count` increasing
//!   locations, agreeing with each other;
//! * document lengths: nonzero, summing to `total_length`, equal to the
//!   number of positions the term payloads hold for that document, and in
//!   the class the class table records.
//!
//! Bytes of an area that no extent covers are not examined: no reader reaches
//! them.

use std::fmt;

use crate::docs::{PAGE_ENTRY, page_table};
use crate::forward::ForwardRecord;
use crate::ordinals::{self, Ordinals};
use crate::segment::{AreaFetch, Segment};
use crate::tf_bucket::TfBucket;
use crate::{Error, Tid};

/// How bad a finding is: an error means a reader can fail or return wrong
/// results; a warning means something is off but every reader copes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Error,
}

impl Severity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One problem: where it is and what disagrees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub location: String,
    pub message: String,
}

impl Finding {
    pub fn error(location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            location: location.into(),
            message: message.into(),
        }
    }

    pub fn warning(location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            location: location.into(),
            message: message.into(),
        }
    }

    /// The same finding with `prefix` in front of its location, so a caller
    /// checking many blobs can say which one it came from.
    pub fn within(mut self, prefix: &str) -> Self {
        self.location = if self.location.is_empty() {
            prefix.to_owned()
        } else {
            format!("{prefix}, {}", self.location)
        };
        self
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}: {}", self.severity, self.location, self.message)
    }
}

/// Findings a single check stops recording after, so a shredded blob does
/// not produce a report the size of the blob.
pub const MAX_FINDINGS: usize = 1_000;

/// Collects findings up to [`MAX_FINDINGS`], then one note that more exist.
#[derive(Debug, Default)]
pub struct Findings {
    pub items: Vec<Finding>,
    suppressed: usize,
}

impl Findings {
    pub fn push(&mut self, finding: Finding) {
        if self.items.len() < MAX_FINDINGS {
            self.items.push(finding);
        } else {
            self.suppressed += 1;
        }
    }

    pub fn error(&mut self, location: impl Into<String>, message: impl fmt::Display) {
        self.push(Finding::error(location, message.to_string()));
    }

    pub fn warning(&mut self, location: impl Into<String>, message: impl fmt::Display) {
        self.push(Finding::warning(location, message.to_string()));
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn finish(mut self) -> Vec<Finding> {
        if self.suppressed > 0 {
            self.items.push(Finding::warning(
                "",
                format!("{} further findings not listed", self.suppressed),
            ));
        }
        self.items
    }
}

/// The outcome of checking a segment: the findings plus what the checker
/// learned about the blob, so a caller can compare it with its directory.
#[derive(Debug, Default)]
pub struct SegmentReport {
    pub findings: Vec<Finding>,
    /// From the header, when it parsed.
    pub doc_count: Option<u32>,
    pub total_length: Option<u64>,
    /// The document table, when it decoded; empty otherwise.
    pub documents: Vec<Tid>,
}

impl SegmentReport {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

fn describe(tid: Tid) -> String {
    format!("({},{})", tid.block, tid.offset)
}

/// Compares the stored page table with the one `documents` implies.
fn check_page_table(found: &[u8], documents: &[Tid], findings: &mut Findings) {
    let expected = page_table(documents.iter().copied());
    if found == expected {
        return;
    }
    if found.len() != expected.len() {
        findings.error(
            "page table",
            format!(
                "holds {} entries but the documents span {} pages",
                found.len() / PAGE_ENTRY,
                expected.len() / PAGE_ENTRY
            ),
        );
    }
    let entry = |bytes: &[u8]| {
        (
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        )
    };
    let entries = found
        .chunks_exact(PAGE_ENTRY)
        .zip(expected.chunks_exact(PAGE_ENTRY));
    for (index, (found, expected)) in entries.enumerate() {
        if found != expected {
            let (found, expected) = (entry(found), entry(expected));
            findings.error(
                "page table",
                format!(
                    "entry {index} is block {} from ordinal {} but the document table implies block {} from ordinal {}",
                    found.0, found.1, expected.0, expected.1
                ),
            );
            // Later entries of a shifted table all differ; one names the spot.
            break;
        }
    }
}

/// Checks one segment blob completely.
pub fn verify_segment(bytes: &[u8]) -> SegmentReport {
    let mut report = SegmentReport::default();
    let mut findings = Findings::default();
    let segment = match Segment::parse(bytes) {
        Ok(segment) => segment,
        Err(error) => {
            findings.error("header", error);
            report.findings = findings.finish();
            return report;
        }
    };
    report.doc_count = Some(segment.document_count());
    report.total_length = Some(segment.total_length());

    // The document table and lengths first: every term check refers to them.
    let documents = match segment.doc_table().and_then(|docs| docs.to_vec()) {
        Ok(documents) => documents,
        Err(error) => {
            findings.error("document table", error);
            Vec::new()
        }
    };
    let table_ok = !documents.is_empty() || segment.document_count() == 0;
    if table_ok && documents.len() != segment.document_count() as usize {
        findings.error(
            "document table",
            format!(
                "holds {} documents but the header says {}",
                documents.len(),
                segment.document_count()
            ),
        );
    }
    let lengths = segment.lengths();
    let mut length_of = Vec::with_capacity(documents.len());
    let mut total = 0u64;
    for (ordinal, tid) in documents.iter().enumerate() {
        match lengths.get(ordinal as u32) {
            Ok(length) => {
                if length == 0 {
                    findings.error(
                        format!("document {}", describe(*tid)),
                        "length is zero; the builder never records empty documents",
                    );
                }
                total += u64::from(length);
                length_of.push(length);
            }
            Err(error) => {
                findings.error(format!("document {}", describe(*tid)), error);
                length_of.push(0);
            }
        }
    }
    for (ordinal, tid) in documents.iter().enumerate() {
        match segment.length_class(ordinal as u32) {
            Ok(class) if class != crate::length_class::class_of(length_of[ordinal]) => {
                findings.error(
                    format!("document {}", describe(*tid)),
                    format!(
                        "length class is {class} but length {} is class {}",
                        length_of[ordinal],
                        crate::length_class::class_of(length_of[ordinal])
                    ),
                );
            }
            Ok(_) => {}
            Err(error) => findings.error(format!("document {}", describe(*tid)), error),
        }
    }
    if table_ok && total != segment.total_length() {
        findings.error(
            "header",
            format!(
                "total_length is {} but the document lengths sum to {total}",
                segment.total_length()
            ),
        );
    }
    if table_ok {
        match segment.page_table() {
            Ok(pages) => check_page_table(pages.bytes(), &documents, &mut findings),
            Err(error) => findings.error("page table", error),
        }
    }

    // Positions counted per document across every term, to compare with
    // the length table once the dictionary has been walked completely.
    let mut positions_of = vec![0u64; documents.len()];
    let mut complete = table_ok;

    let dictionary = match segment.dictionary() {
        Ok(dictionary) => Some(dictionary),
        Err(error) => {
            findings.error("dictionary", error);
            None
        }
    };
    let sections = segment.sections();
    let (ordinals_len, payload_len) = (sections.ordinals, sections.payload);
    let mut previous: Option<String> = None;
    let mut ordinals_end = 0u64;
    let mut payload_end = 0u64;
    // Reuse per-term scratch across the dictionary, especially for singleton
    // terms. Every consumer clears its state before use, including after a
    // malformed term skips the remaining checks in its iteration.
    let mut scores: Vec<(u8, u32)> = Vec::new();
    let blocks = dictionary.map_or(0, |d| d.index().blocks());
    for block in 0..blocks {
        let dictionary = dictionary.expect("blocks come from a parsed dictionary");
        let terms = match dictionary.block(block) {
            Ok(terms) => terms,
            Err(error) => {
                findings.error(format!("dictionary block {block}"), error);
                complete = false;
                continue;
            }
        };
        if let (Some((first, _)), Some(indexed)) =
            (terms.first(), dictionary.index().block_first(block))
            && first.as_bytes() != indexed
        {
            findings.error(
                format!("dictionary block {block}"),
                format!(
                    "first term {first:?} differs from the block index entry {:?}",
                    String::from_utf8_lossy(indexed)
                ),
            );
        }
        for (term, entry) in terms {
            if previous.as_deref().is_some_and(|p| p >= term.as_str()) {
                findings.error(
                    format!("term {term:?}"),
                    format!(
                        "follows {:?} out of order",
                        previous.as_deref().unwrap_or("")
                    ),
                );
            }
            previous = Some(term);
            let term = previous.as_ref().expect("just stored the current term");
            let location = || format!("term {term:?}");
            let mut resolvable = true;
            for (name, extent, area_len, end) in [
                ("ordinals", entry.ordinals, ordinals_len, &mut ordinals_end),
                ("payload", entry.payload, payload_len, &mut payload_end),
            ] {
                let extent_end = extent.offset.saturating_add(u64::from(extent.len));
                if extent_end > area_len as u64 {
                    findings.error(
                        location(),
                        format!(
                            "{name} extent {}+{} exceeds the {name} area of {area_len} bytes",
                            extent.offset, extent.len
                        ),
                    );
                    resolvable = false;
                } else if extent.offset < *end {
                    findings.error(
                        location(),
                        format!(
                            "{name} extent starts at {} inside the previous term's extent ending at {end}",
                            extent.offset
                        ),
                    );
                }
                *end = (*end).max(extent_end);
            }
            if !resolvable {
                complete = false;
                continue;
            }
            let resolved = match segment.resolve(entry) {
                Ok(resolved) => resolved,
                Err(error) => {
                    findings.error(location(), error);
                    complete = false;
                    continue;
                }
            };
            if entry.df == 0 {
                findings.error(location(), "term has no documents");
            }

            // Ordinals: well formed, df members, all in the document table.
            let stream_bytes =
                match segment.ordinals_bytes(entry.ordinals.offset, entry.ordinals.len as usize) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        findings.error(location(), format!("ordinals: {error}"));
                        complete = false;
                        continue;
                    }
                };
            let members =
                match ordinals::validate(stream_bytes, entry.df, segment.document_count(), true)
                    .and_then(|()| Ordinals::open(stream_bytes, stream_bytes.len() as u64, true))
                    .and_then(|stream| {
                        let mut cursor = stream.cursor()?;
                        let mut members = Vec::with_capacity(entry.df as usize);
                        while let Some(ordinal) = cursor.current() {
                            let bucket = cursor
                                .bucket()
                                .ok_or(Error::Corrupt("member without a bucket"))?;
                            members.push((ordinal, bucket));
                            cursor.advance()?;
                        }
                        Ok(members)
                    }) {
                    Ok(members) => members,
                    Err(error) => {
                        findings.error(location(), format!("ordinals: {error}"));
                        complete = false;
                        continue;
                    }
                };

            // Payload: one entry per member, buckets matching positions.
            let payload = match resolved.payload() {
                Ok(payload) => payload,
                Err(error) => {
                    findings.error(location(), format!("payload header: {error}"));
                    complete = false;
                    continue;
                }
            };
            if payload.count() != entry.df {
                findings.error(
                    location(),
                    format!(
                        "payload holds {} entries for {} documents",
                        payload.count(),
                        entry.df
                    ),
                );
            }
            let mut cursor = payload.cursor();
            scores.clear();
            scores.reserve(members.len());
            let mut max_bucket = 0u8;
            let mut payload_ok = true;
            for (index, (ordinal, bucket)) in members.iter().enumerate() {
                if index as u32 >= payload.count() {
                    break;
                }
                let bucket = *bucket;
                let position_count = match cursor.next_count() {
                    Ok(counted) => counted,
                    Err(error) => {
                        findings.error(
                            location(),
                            format!("payload entry {index} for ordinal {ordinal}: {error}"),
                        );
                        payload_ok = false;
                        break;
                    }
                };
                let expected = TfBucket::from_count(position_count as u32).value();
                if bucket != expected {
                    findings.error(
                        location(),
                        format!(
                            "member {index} (ordinal {ordinal}) has bucket {bucket} but its {position_count} positions imply {expected}"
                        ),
                    );
                }
                max_bucket = max_bucket.max(bucket);
                if let Some(counted) = positions_of.get_mut(*ordinal as usize) {
                    *counted += position_count as u64;
                    scores.push((bucket, length_of[*ordinal as usize]));
                }
            }
            if !payload_ok {
                complete = false;
                continue;
            }
            if !members.is_empty() && max_bucket != entry.max_tf_bucket {
                findings.error(
                    location(),
                    format!(
                        "dictionary max_tf_bucket is {} but the largest payload bucket is {max_bucket}",
                        entry.max_tf_bucket
                    ),
                );
            }

            // Bounds: what the payload buckets and lengths imply. The encoding
            // is canonical, so the stream the writer would produce is the one
            // that passes; comparing with it first spares a sound term, the
            // common case, a second decoding.
            if scores.len() != members.len() || payload.count() != entry.df {
                continue;
            }
            let member_ordinals: Vec<u32> = members.iter().map(|(ordinal, _)| *ordinal).collect();
            let canonical = ordinals::encode_scored(&member_ordinals, &scores);
            if canonical == stream_bytes {
                continue;
            }
            let same_bounds = Ordinals::open(stream_bytes, stream_bytes.len() as u64, true)
                .and_then(|found| {
                    Ordinals::open(&canonical[..], canonical.len() as u64, true)
                        .map(|wanted| found.bounds() == wanted.bounds())
                })
                .unwrap_or(false);
            if same_bounds {
                findings.warning(
                    location(),
                    "ordinals: the stream names its documents but is not encoded as the writer would",
                );
            } else {
                findings.error(
                    location(),
                    "ordinal chunk bounds disagree with the payload buckets and document lengths",
                );
            }
        }
    }

    if complete {
        for (ordinal, tid) in documents.iter().enumerate() {
            if positions_of[ordinal] != u64::from(length_of[ordinal]) {
                findings.error(
                    format!("document {}", describe(*tid)),
                    format!(
                        "length is {} but its terms hold {} positions",
                        length_of[ordinal], positions_of[ordinal]
                    ),
                );
            }
        }
    }

    report.documents = documents;
    report.findings = findings.finish();
    report
}

/// Checks a dead list: an ordinal stream without bounds whose members must
/// all be below `doc_count`, the segment's document count.
pub fn verify_dead_list(bytes: &[u8], doc_count: u32) -> Vec<Finding> {
    let mut findings = Findings::default();
    match Ordinals::open(bytes, bytes.len() as u64, false)
        .and_then(|stream| ordinals::validate(bytes, stream.count(), doc_count, false))
    {
        Ok(()) => {}
        Err(error) => findings.error("dead list", error),
    }
    findings.finish()
}

/// The outcome of checking a forward stream (a write buffer's contents).
#[derive(Debug, Default)]
pub struct ForwardReport {
    pub findings: Vec<Finding>,
    /// Records decoded before the first malformed one.
    pub records: u32,
    /// Their locations, in stream order.
    pub tids: Vec<Tid>,
}

/// Checks records packed back to back, as the write buffer holds them.
pub fn verify_forward_stream(bytes: &[u8]) -> ForwardReport {
    let mut report = ForwardReport::default();
    let mut findings = Findings::default();
    let mut at = 0usize;
    while at < bytes.len() {
        match ForwardRecord::decode(&bytes[at..]) {
            Ok((record, consumed)) => {
                let counted: u32 = record.terms.iter().map(|t| t.positions.len() as u32).sum();
                if counted != record.doc_len {
                    findings.error(
                        format!("record {} at byte {at}", report.records),
                        format!(
                            "document {} has length {} but its terms hold {counted} positions",
                            describe(record.tid),
                            record.doc_len
                        ),
                    );
                }
                report.tids.push(record.tid);
                report.records += 1;
                at += consumed;
            }
            Err(error) => {
                let what = match error {
                    Error::Truncated => "record runs past the end of the buffer".to_owned(),
                    other => other.to_string(),
                };
                findings.error(format!("record {} at byte {at}", report.records), what);
                break;
            }
        }
    }
    let mut sorted = report.tids.clone();
    sorted.sort_unstable();
    for pair in sorted.windows(2) {
        if pair[0] == pair[1] {
            findings.error(
                "write buffer",
                format!("document {} is recorded twice", describe(pair[0])),
            );
        }
    }
    report.findings = findings.finish();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictionary::TermEntry;
    use crate::segment::SegmentBuilder;
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    fn tid(block: u32, offset: u16) -> Tid {
        Tid::new(block, offset).unwrap()
    }

    /// A segment with more than one dictionary block, a term with a chunked
    /// stream, and both short and long terms.
    fn sample() -> Vec<u8> {
        let mut builder = SegmentBuilder::default();
        for i in 0..300u32 {
            let mut tokens = vec![("common", 1u32)];
            tokens.push((if i % 2 == 0 { "even" } else { "odd" }, 2));
            for k in 0..(i % 5) {
                tokens.push(("common", 3 + k));
            }
            tokens.push((["a", "b", "c"][(i % 3) as usize], 20));
            builder
                .add_document(tid(i / 7, (i % 7 + 1) as u16), tokens.clone())
                .unwrap();
        }
        // Enough distinct terms for several dictionary blocks.
        for i in 0..150u32 {
            builder
                .add_document(
                    tid(1000 + i * 3, 1),
                    [(format!("term{i:04}").leak() as &str, 1u32), ("common", 2)],
                )
                .unwrap();
        }
        builder.finish()
    }

    /// Where the ordinals area, the offsets, the lengths and the pages start.
    fn section_starts(bytes: &[u8]) -> (usize, usize, usize, usize) {
        let sections = Segment::parse(bytes).unwrap().sections();
        let ordinals_at = sections.header + sections.dictionary;
        let offsets_at = ordinals_at + sections.ordinals + sections.payload;
        let lengths_at = offsets_at + sections.offsets;
        (
            ordinals_at,
            offsets_at,
            lengths_at,
            lengths_at + sections.lengths + sections.classes,
        )
    }

    /// `bytes` with every dictionary entry passed through `edit`.
    fn with_entries(bytes: &[u8], mut edit: impl FnMut(&str, &mut TermEntry)) -> Vec<u8> {
        let segment = Segment::parse(bytes).unwrap();
        let sections = segment.sections();
        let mut dictionary = crate::dictionary::DictionaryBuilder::default();
        for item in segment.dictionary().unwrap().iter() {
            let (term, mut entry) = item.unwrap();
            edit(&term, &mut entry);
            dictionary.push(&term, entry).unwrap();
        }
        let dictionary = dictionary.finish();
        let mut out = crate::segment::header(
            segment.document_count(),
            segment.total_length(),
            dictionary.len(),
            sections.ordinals,
            sections.payload,
            sections.pages,
        );
        out.extend_from_slice(&dictionary);
        out.extend_from_slice(&bytes[sections.header + sections.dictionary..]);
        out
    }

    /// The bytes of `term`'s ordinal stream within `bytes`.
    fn ordinal_stream(bytes: &[u8], term: &str) -> std::ops::Range<usize> {
        let entry = Segment::parse(bytes)
            .unwrap()
            .term(term)
            .unwrap()
            .unwrap()
            .entry;
        let at = section_starts(bytes).0 + entry.ordinals.offset as usize;
        at..at + entry.ordinals.len as usize
    }

    fn messages(findings: &[Finding]) -> String {
        findings
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn only_for<'a>(report: &'a SegmentReport, location: &str) -> Vec<&'a str> {
        report
            .findings
            .iter()
            .filter(|f| f.location == location)
            .map(|f| f.message.as_str())
            .collect()
    }

    #[test]
    fn a_valid_segment_is_clean() {
        let report = verify_segment(&sample());
        assert!(report.is_clean(), "{}", messages(&report.findings));
        assert_eq!(report.doc_count, Some(450));
        assert_eq!(report.documents.len(), 450);
    }

    #[test]
    fn header_corruption_is_reported_at_the_header() {
        let bytes = sample();
        for (edit, expected) in [
            (0usize, "segment magic"),
            (bytes.len() - 1, "segment length"),
        ] {
            let mut bad = if edit == 0 {
                bytes.clone()
            } else {
                bytes[..edit].to_vec()
            };
            if edit == 0 {
                bad[0] = b'X';
            }
            let report = verify_segment(&bad);
            assert_eq!(report.findings.len(), 1);
            assert_eq!(report.findings[0].location, "header");
            assert!(
                report.findings[0].message.contains(expected),
                "{}",
                report.findings[0]
            );
        }
    }

    #[test]
    fn swapped_lengths_and_total_length_are_caught() {
        let mut bytes = sample();
        let (_, _, lengths_at, _) = section_starts(&bytes);
        // Swap the lengths of documents 0 and 1: the total is unchanged but
        // every term over them now disagrees with its bounds, and the
        // position counts disagree with the lengths.
        let (a, b) = (lengths_at, lengths_at + 4);
        let first: [u8; 4] = bytes[a..a + 4].try_into().unwrap();
        let second: [u8; 4] = bytes[b..b + 4].try_into().unwrap();
        assert_ne!(first, second);
        bytes[a..a + 4].copy_from_slice(&second);
        bytes[b..b + 4].copy_from_slice(&first);
        let report = verify_segment(&bytes);
        assert!(!report.is_clean());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.location.starts_with("document (0,1)")),
            "{}",
            messages(&report.findings)
        );
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.message.contains("chunk bounds disagree")),
            "{}",
            messages(&report.findings)
        );
        // A wrong total is reported at the header.
        let mut bytes = sample();
        bytes[lengths_at] ^= 1;
        let report = verify_segment(&bytes);
        assert!(
            only_for(&report, "header")
                .iter()
                .any(|m| m.contains("total_length")),
            "{}",
            messages(&report.findings)
        );
    }

    #[test]
    fn a_wrong_bucket_is_reported_for_its_term() {
        let bytes = with_entries(&sample(), |term, entry| {
            if term == "even" {
                entry.max_tf_bucket = 9;
            }
        });
        let report = verify_segment(&bytes);
        assert_eq!(
            only_for(&report, "term \"even\""),
            ["dictionary max_tf_bucket is 9 but the largest payload bucket is 0"]
        );
        assert_eq!(report.findings.len(), 1, "{}", messages(&report.findings));
    }

    #[test]
    fn a_malformed_ordinal_stream_is_reported_for_its_term() {
        let bytes = sample();
        let range = ordinal_stream(&bytes, "common");
        let mut bad = bytes.clone();
        // Change the last member's bucket nibble: it no longer matches the
        // member's positions, and the chunk bound no longer matches it.
        bad[range.end - 1] ^= 0x0f;
        let report = verify_segment(&bad);
        assert!(
            only_for(&report, "term \"common\"")
                .iter()
                .any(|m| m.contains("positions imply") || m.contains("bounds disagree")),
            "{}",
            messages(&report.findings)
        );
        // Break a member of the array chunk: order or count breaks.
        let mut bad = bytes.clone();
        bad[range.start + 8] = 0xff;
        let report = verify_segment(&bad);
        assert!(
            only_for(&report, "term \"common\"")
                .iter()
                .any(|m| m.starts_with("ordinals:")
                    || m.contains("bounds disagree")
                    || m.contains("positions imply")),
            "{}",
            messages(&report.findings)
        );
        // A truncated extent.
        let bad = with_entries(&bytes, |term, entry| {
            if term == "common" {
                entry.ordinals.len -= 1;
            }
        });
        let report = verify_segment(&bad);
        assert!(
            !only_for(&report, "term \"common\"").is_empty(),
            "{}",
            messages(&report.findings)
        );
    }

    #[test]
    fn a_wrong_page_table_entry_is_reported() {
        let mut bytes = sample();
        let (_, _, _, pages_at) = section_starts(&bytes);
        // The second entry's block.
        bytes[pages_at + PAGE_ENTRY] ^= 0x40;
        let report = verify_segment(&bytes);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.location == "document table" || f.location == "page table"),
            "{}",
            messages(&report.findings)
        );
    }

    #[test]
    fn an_extent_outside_its_area_is_reported() {
        let bytes = with_entries(&sample(), |term, entry| {
            if term == "odd" {
                entry.ordinals.offset += 1 << 20;
            }
        });
        let report = verify_segment(&bytes);
        assert!(
            only_for(&report, "term \"odd\"")
                .iter()
                .any(|m| m.contains("exceeds the ordinals area")),
            "{}",
            messages(&report.findings)
        );
        let bytes = with_entries(&sample(), |term, entry| {
            if term == "odd" {
                entry.payload.offset = 0;
            }
        });
        let report = verify_segment(&bytes);
        assert!(
            only_for(&report, "term \"odd\"")
                .iter()
                .any(|m| m.contains("payload extent starts at 0 inside")),
            "{}",
            messages(&report.findings)
        );
    }

    #[test]
    fn every_single_byte_flip_is_survived_and_detected_unless_still_decodable() {
        let mut builder = SegmentBuilder::default();
        for i in 0..40u32 {
            builder
                .add_document(
                    tid(i / 3, (i % 3 + 1) as u16),
                    [("x", 1u32), (["y", "z"][(i % 2) as usize], 2)],
                )
                .unwrap();
        }
        let bytes = builder.finish();
        assert!(verify_segment(&bytes).is_clean());
        let (ordinals_at, offsets_at, lengths_at, pages_at) = section_starts(&bytes);
        let sections = Segment::parse(&bytes).unwrap().sections();
        let payload_at = ordinals_at + sections.ordinals;
        let classes_at = lengths_at + sections.lengths;
        let mut undetected = [0usize; 8];
        for at in 0..bytes.len() {
            for bit in 0..8 {
                let mut bad = bytes.clone();
                bad[at] ^= 1 << bit;
                if verify_segment(&bad).is_clean() {
                    let section = match at {
                        _ if at >= pages_at => 7,
                        _ if at >= classes_at => 6,
                        _ if at >= lengths_at => 5,
                        _ if at >= offsets_at => 4,
                        _ if at >= payload_at => 3,
                        _ if at >= ordinals_at => 2,
                        _ if at >= sections.header => 1,
                        _ => 0,
                    };
                    undetected[section] += 1;
                }
            }
        }
        // Values nothing cross-references survive a flip that keeps them
        // plausible: a term's name (still in order), a page table entry's
        // block number, a tuple offset that keeps its block's offsets
        // ascending, and a position value. Everything structural, every
        // count, every length and every bound is caught.
        let [
            header,
            dictionary,
            ordinals,
            payload,
            offsets,
            lengths,
            classes,
            pages,
        ] = undetected;
        assert_eq!(
            (header, ordinals, lengths, classes),
            (0, 0, 0, 0),
            "{undetected:?}"
        );
        assert!(dictionary <= 8, "{undetected:?}");
        assert!(pages * 2 < sections.pages * 8, "{undetected:?}");
        assert!(payload * 2 < sections.payload * 8, "{undetected:?}");
        assert!(offsets < sections.offsets * 8, "{undetected:?}");
    }

    #[test]
    fn dead_lists_must_be_within_the_document_count() {
        let list = ordinals::encode(&[0, 3, 7]);
        assert!(verify_dead_list(&list, 8).is_empty());
        let findings = verify_dead_list(&list, 7);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].location, "dead list");
        assert!(!verify_dead_list(&[0xff, 0xff], 8).is_empty());
        assert!(verify_dead_list(&ordinals::encode(&[]), 0).is_empty());
    }

    #[test]
    fn forward_streams_report_framing_and_duplicates() {
        let mut stream = Vec::new();
        let record = |block: u32, text: &str| {
            ForwardRecord::from_tokens(
                tid(block, 1),
                text.split_whitespace()
                    .enumerate()
                    .map(|(i, w)| (w, i as u32 + 1)),
            )
            .unwrap()
        };
        record(1, "a b").encode(&mut stream).unwrap();
        record(2, "c").encode(&mut stream).unwrap();
        let report = verify_forward_stream(&stream);
        assert!(report.findings.is_empty(), "{}", messages(&report.findings));
        assert_eq!(report.records, 2);
        record(1, "d").encode(&mut stream).unwrap();
        let report = verify_forward_stream(&stream);
        assert_eq!(report.records, 3);
        assert!(
            report.findings.iter().any(|f| f.message.contains("twice")),
            "{}",
            messages(&report.findings)
        );
        stream.push(0x80);
        let report = verify_forward_stream(&stream);
        assert!(report.findings.len() >= 2);
    }

    #[test]
    fn findings_are_capped() {
        let mut findings = Findings::default();
        for i in 0..(MAX_FINDINGS + 10) {
            findings.error("x", i);
        }
        let all = findings.finish();
        assert_eq!(all.len(), MAX_FINDINGS + 1);
        assert!(all.last().unwrap().message.contains("further"));
    }

    #[test]
    fn finding_locations_nest() {
        assert_eq!(
            Finding::error("term", "bad").within("segment 3").location,
            "segment 3, term"
        );
        assert_eq!(
            Finding::error("", "bad").within("segment 3").location,
            "segment 3"
        );
    }

    /// Documents with a few terms each; positions in document order.
    fn documents() -> impl Strategy<Value = BTreeMap<Tid, Vec<(String, u32)>>> {
        prop::collection::btree_map(
            (0u32..64, 1u16..=40).prop_map(|(b, o)| Tid::new(b, o).unwrap()),
            prop::collection::vec((0u8..12, 1u32..4), 1..30).prop_map(|tokens| {
                let mut position = 0u32;
                tokens
                    .into_iter()
                    .map(|(term, gap)| {
                        position += gap;
                        (format!("t{term}"), position)
                    })
                    .collect::<Vec<_>>()
            }),
            0..200,
        )
    }

    fn build(documents: &BTreeMap<Tid, Vec<(String, u32)>>) -> Vec<u8> {
        let mut builder = SegmentBuilder::default();
        for (tid, tokens) in documents {
            builder
                .add_document(*tid, tokens.iter().map(|(t, p)| (t.as_str(), *p)))
                .unwrap();
        }
        builder.finish()
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

        #[test]
        fn valid_segments_never_yield_findings(documents in documents()) {
            let bytes = build(&documents);
            let report = verify_segment(&bytes);
            prop_assert!(report.is_clean(), "{}", messages(&report.findings));
            prop_assert_eq!(report.documents.len(), documents.len());
            let mut stream = Vec::new();
            for (tid, tokens) in &documents {
                ForwardRecord::from_tokens(*tid, tokens.iter().map(|(t, p)| (t.as_str(), *p)))
                    .unwrap()
                    .encode(&mut stream)
                    .unwrap();
            }
            let forward = verify_forward_stream(&stream);
            prop_assert!(forward.findings.is_empty(), "{}", messages(&forward.findings));
            prop_assert_eq!(forward.records as usize, documents.len());
        }

        #[test]
        fn mutated_segments_never_panic(
            documents in documents(),
            edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
            cut in any::<prop::sample::Index>(),
        ) {
            let mut bytes = build(&documents);
            for (at, value) in &edits {
                let at = at.index(bytes.len());
                bytes[at] = *value;
            }
            let _ = verify_segment(&bytes);
            let _ = verify_segment(&bytes[..cut.index(bytes.len())]);
            let _ = verify_dead_list(&bytes, 10);
            let _ = verify_forward_stream(&bytes);
        }
    }
}
