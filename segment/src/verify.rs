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
//! * every term: postings and payload extents inside their areas and not
//!   overlapping the previous term's, postings decoding to `df` increasing
//!   locations that are all in the document table, the payload holding one
//!   entry per posting with a valid bucket that matches its position count,
//!   `max_tf_bucket` equal to the largest bucket, and (from `LSG2` on) score
//!   bounds equal to what the postings and document lengths imply, in the
//!   layout the segment's format writes;
//! * the document table: decodes to `doc_count` increasing locations;
//! * document lengths: nonzero, summing to `total_length`, and equal to the
//!   number of positions the term payloads hold for that document.

use std::fmt;

use crate::forward::ForwardRecord;
use crate::postings::{BLOCK_POSTINGS, BlockBound, Postings};
use crate::segment::{Format, Segment};
use crate::set::{Cursor, collect};
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
    /// From the signature, when it parsed.
    pub format: Option<Format>,
    /// True for an `LSG1` blob.
    pub legacy: bool,
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

/// Locate increasing postings in an increasing document table. Galloping over
/// gaps keeps sparse terms logarithmic in their gap size, while adjacent matches
/// need one comparison instead of searching the whole table for every posting.
fn ordered_rank(documents: &[Tid], at: &mut usize, target: Tid) -> Option<usize> {
    let remaining = &documents[*at..];
    if remaining.first().is_some_and(|tid| *tid < target) {
        let mut end = 1usize;
        while end < remaining.len() && remaining[end] < target {
            end = end.saturating_mul(2);
        }
        let end = end.saturating_add(1).min(remaining.len());
        *at += remaining[..end].partition_point(|tid| *tid < target);
    }
    if documents.get(*at) == Some(&target) {
        let ordinal = *at;
        *at += 1;
        Some(ordinal)
    } else {
        None
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
    let format = segment.format();
    report.format = Some(format);
    report.legacy = segment.is_legacy();
    if report.legacy {
        findings.warning(
            "header",
            "LSG1 segment: ranked scans over it score every candidate; REINDEX to upgrade",
        );
    }

    // The document table and lengths first: every term check refers to them.
    let documents = match segment.documents().and_then(collect) {
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
    if table_ok && total != segment.total_length() {
        findings.error(
            "header",
            format!(
                "total_length is {} but the document lengths sum to {total}",
                segment.total_length()
            ),
        );
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
    let (postings_len, payload_len) = segment.area_lengths();
    let mut previous: Option<String> = None;
    let mut postings_end = 0u64;
    let mut payload_end = 0u64;
    // Reuse per-term scratch across the dictionary, especially for singleton
    // terms. Every consumer clears its state before use, including after a
    // malformed term skips the remaining checks in its iteration.
    let mut tids = Vec::new();
    let mut ordinals = Vec::new();
    let mut scores: Vec<(u8, u32)> = Vec::new();
    let mut expected = Vec::new();
    let mut bounds = Vec::new();
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
            let postings_extent_end = entry
                .postings
                .offset
                .saturating_add(u64::from(entry.postings.len));
            let payload_extent_end = entry
                .payload
                .offset
                .saturating_add(u64::from(entry.payload.len));
            let mut resolvable = true;
            if postings_extent_end > postings_len as u64 {
                findings.error(
                    location(),
                    format!(
                        "postings extent {}+{} exceeds the postings area of {postings_len} bytes",
                        entry.postings.offset, entry.postings.len
                    ),
                );
                resolvable = false;
            } else if entry.postings.offset < postings_end {
                findings.error(
                    location(),
                    format!(
                        "postings extent starts at {} inside the previous term's extent ending at {postings_end}",
                        entry.postings.offset
                    ),
                );
            }
            if payload_extent_end > payload_len as u64 {
                findings.error(
                    location(),
                    format!(
                        "payload extent {}+{} exceeds the payload area of {payload_len} bytes",
                        entry.payload.offset, entry.payload.len
                    ),
                );
                resolvable = false;
            } else if entry.payload.offset < payload_end {
                findings.error(
                    location(),
                    format!(
                        "payload extent starts at {} inside the previous term's extent ending at {payload_end}",
                        entry.payload.offset
                    ),
                );
            }
            postings_end = postings_end.max(postings_extent_end);
            payload_end = payload_end.max(payload_extent_end);
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

            // Postings: count, order, membership in the document table.
            let postings = match resolved.postings() {
                Ok(postings) => postings,
                Err(error) => {
                    findings.error(location(), format!("postings header: {error}"));
                    complete = false;
                    continue;
                }
            };
            if postings.count() != entry.df {
                findings.error(
                    location(),
                    format!(
                        "dictionary df is {} but the postings hold {}",
                        entry.df,
                        postings.count()
                    ),
                );
            }
            tids.clear();
            let mut posting_cursor = match (|| {
                let mut cursor = postings.cursor()?;
                while let Some(tid) = cursor.current() {
                    tids.push(tid);
                    cursor.advance()?;
                }
                if tids.len() != postings.count() as usize {
                    return Err(Error::Corrupt("posting count mismatch"));
                }
                Ok(cursor)
            })() {
                Ok(cursor) => cursor,
                Err(error) => {
                    findings.error(location(), format!("postings: {error}"));
                    complete = false;
                    continue;
                }
            };
            if tids.is_empty() {
                findings.error(location(), "term has no postings");
            }
            ordinals.clear();
            ordinals.reserve(tids.len());
            let mut unknown = 0usize;
            let mut document_at = 0;
            for tid in &tids {
                match if tids.len() == 1 {
                    documents.binary_search(tid).ok()
                } else {
                    ordered_rank(&documents, &mut document_at, *tid)
                } {
                    Some(ordinal) => ordinals.push(Some(ordinal)),
                    None => {
                        if unknown == 0 {
                            findings.error(
                                location(),
                                format!("posting {} is not in the document table", describe(*tid)),
                            );
                        }
                        unknown += 1;
                        ordinals.push(None);
                    }
                }
            }
            if unknown > 1 {
                findings.error(
                    location(),
                    format!("{unknown} postings are not in the document table"),
                );
            }

            // Payload: one entry per posting, buckets matching positions.
            let payload = match resolved.payload() {
                Ok(payload) => payload,
                Err(error) => {
                    findings.error(location(), format!("payload header: {error}"));
                    complete = false;
                    continue;
                }
            };
            if payload.count() != postings.count() {
                findings.error(
                    location(),
                    format!(
                        "payload holds {} entries for {} postings",
                        payload.count(),
                        postings.count()
                    ),
                );
            }
            let mut cursor = payload.cursor();
            scores.clear();
            scores.reserve(tids.len());
            let mut max_bucket = 0u8;
            let mut payload_ok = true;
            for (index, tid) in tids.iter().enumerate() {
                if index as u32 >= payload.count() {
                    break;
                }
                let (bucket, position_count) = match cursor.next_count() {
                    Ok(counted) => counted,
                    Err(error) => {
                        findings.error(
                            location(),
                            format!("payload entry {index} for {}: {error}", describe(*tid)),
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
                            "payload entry {index} for {} has bucket {bucket} but {} positions imply {expected}",
                            describe(*tid),
                            position_count
                        ),
                    );
                }
                max_bucket = max_bucket.max(bucket);
                if let Some(ordinal) = ordinals[index] {
                    positions_of[ordinal] += position_count as u64;
                    scores.push((bucket, length_of[ordinal]));
                }
            }
            if !payload_ok {
                complete = false;
                continue;
            }
            if !tids.is_empty() && max_bucket != entry.max_tf_bucket {
                findings.error(
                    location(),
                    format!(
                        "dictionary max_tf_bucket is {} but the largest payload bucket is {max_bucket}",
                        entry.max_tf_bucket
                    ),
                );
            }

            // Score bounds: what the postings and lengths imply, in the
            // layout the segment's format writes.
            match posting_cursor.block_bounds_into(&mut bounds) {
                Ok(()) => (),
                Err(error) => {
                    findings.error(location(), format!("block bounds: {error}"));
                    continue;
                }
            };
            if !format.has_bounds() {
                continue;
            }
            if bounds.is_empty() {
                if !tids.is_empty() {
                    findings.error(location(), "postings carry no block bounds");
                }
                continue;
            }
            let one_block = postings.count() <= BLOCK_POSTINGS;
            match format {
                Format::Lsg1 => {}
                Format::Lsg2 => {
                    if postings.has_term_bound() {
                        findings.warning(
                            location(),
                            "postings carry an LSG3 term bound where LSG2 writes a block table",
                        );
                    }
                }
                Format::Lsg3 => {
                    if one_block && !postings.has_term_bound() {
                        findings.warning(
                            location(),
                            "postings of one block carry a block table where LSG3 writes a term bound",
                        );
                    }
                }
            }
            if unknown > 0 || scores.len() != tids.len() {
                // Lengths are unknown for postings outside the table; the
                // finding above already covers this term.
                continue;
            }
            expected.clear();
            expected.extend(
                tids.chunks(BLOCK_POSTINGS as usize)
                    .zip(scores.chunks(BLOCK_POSTINGS as usize))
                    .map(|(block, scores)| BlockBound::over(scores, block[block.len() - 1])),
            );
            if bounds.len() != expected.len() {
                findings.error(
                    location(),
                    format!(
                        "{} block bounds for {} blocks of postings",
                        bounds.len(),
                        expected.len()
                    ),
                );
                continue;
            }
            for (index, (found, wanted)) in bounds.iter().zip(&expected).enumerate() {
                if found != wanted {
                    findings.error(
                        location(),
                        format!(
                            "block bound {index} (last {}) disagrees with its postings (last {})",
                            describe(found.last),
                            describe(wanted.last)
                        ),
                    );
                }
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

/// Checks a dead list: a postings stream whose locations must all be in
/// `documents`, the segment's document table in order.
pub fn verify_dead_list(bytes: &[u8], documents: &[Tid]) -> Vec<Finding> {
    let mut findings = Findings::default();
    match Postings::parse(bytes).and_then(|postings| postings.to_vec()) {
        Ok(dead) => {
            let mut missing = 0usize;
            for tid in &dead {
                if documents.binary_search(tid).is_err() {
                    if missing == 0 {
                        findings.error(
                            "dead list",
                            format!("{} is not in the document table", describe(*tid)),
                        );
                    }
                    missing += 1;
                }
            }
            if missing > 1 {
                findings.error(
                    "dead list",
                    format!("{missing} entries are not in the document table"),
                );
            }
            if dead.len() > documents.len() {
                findings.error(
                    "dead list",
                    format!(
                        "holds {} entries for a segment of {} documents",
                        dead.len(),
                        documents.len()
                    ),
                );
            }
        }
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
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::*;
    use crate::postings::PostingsBuilder;
    use crate::segment::SegmentBuilder;

    fn tid(block: u32, offset: u16) -> Tid {
        Tid::new(block, offset).unwrap()
    }

    /// A segment with more than one dictionary block, a term with several
    /// score-bound blocks, and both sparse and grouped postings.
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

    fn messages(findings: &[Finding]) -> String {
        findings
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_valid_segment_is_clean() {
        let bytes = sample();
        let report = verify_segment(&bytes);
        assert!(report.is_clean(), "{}", messages(&report.findings));
        assert_eq!(report.doc_count, Some(450));
        assert_eq!(report.documents.len(), 450);
        assert!(!report.legacy);
        assert!(verify_segment(&SegmentBuilder::default().finish()).is_clean());
    }

    #[test]
    fn header_corruption_is_reported_at_the_header() {
        let bytes = sample();
        let mut magic = bytes.clone();
        magic[0] = b'X';
        let report = verify_segment(&magic);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].location, "header");
        assert!(report.findings[0].message.contains("magic"));
        assert_eq!(report.doc_count, None);
        // Every truncation of the blob breaks the total length.
        for cut in [0, 3, 4, 10, bytes.len() / 2, bytes.len() - 1] {
            let report = verify_segment(&bytes[..cut]);
            assert!(!report.is_clean(), "cut at {cut}");
            assert_eq!(report.findings[0].location, "header", "cut at {cut}");
        }
        // An LSG1 signature is a warning, not an error.
        let mut legacy = bytes.clone();
        legacy[..4].copy_from_slice(b"LSG1");
        let report = verify_segment(&legacy);
        assert!(report.legacy);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.severity == Severity::Warning && f.message.contains("LSG1"))
        );
    }

    #[test]
    fn swapped_lengths_and_total_length_are_caught() {
        let bytes = sample();
        // The last two documents have lengths 2 and 2; the first two
        // documents (0,1) and (0,2) have lengths 3 and 4.
        let lengths_at = bytes.len() - 450 * 4;
        let mut swapped = bytes.clone();
        swapped.copy_within(lengths_at..lengths_at + 4, lengths_at + 4);
        swapped.copy_within(
            bytes.len() - 450 * 4 + 4..bytes.len() - 450 * 4 + 8,
            lengths_at,
        );
        let first = u32::from_le_bytes(bytes[lengths_at..lengths_at + 4].try_into().unwrap());
        let second = u32::from_le_bytes(bytes[lengths_at + 4..lengths_at + 8].try_into().unwrap());
        assert_ne!(first, second);
        let mut swapped = bytes.clone();
        swapped[lengths_at..lengths_at + 4].copy_from_slice(&second.to_le_bytes());
        swapped[lengths_at + 4..lengths_at + 8].copy_from_slice(&first.to_le_bytes());
        let report = verify_segment(&swapped);
        let locations: Vec<&str> = report
            .findings
            .iter()
            .map(|f| f.location.as_str())
            .collect();
        assert!(
            locations.contains(&"document (0,1)"),
            "{}",
            messages(&report.findings)
        );
        assert!(
            locations.contains(&"document (0,2)"),
            "{}",
            messages(&report.findings)
        );
        // Bounds for terms in those documents change too.
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.message.contains("block bound")),
            "{}",
            messages(&report.findings)
        );
        // Changing one length breaks the header total and the document.
        let mut bumped = bytes.clone();
        bumped[lengths_at] ^= 0x01;
        let report = verify_segment(&bumped);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.location == "header" && f.message.contains("total_length")),
            "{}",
            messages(&report.findings)
        );
    }

    #[test]
    fn a_reordered_dictionary_index_is_caught() {
        let bytes = sample();
        // Zero the first term byte of the second block index entry, so the
        // index no longer increases.
        let segment = Segment::parse(&bytes).unwrap();
        let dictionary = segment.dictionary().unwrap();
        assert!(dictionary.index().blocks() >= 3);
        let second = dictionary.index().block_first(1).unwrap().to_vec();
        let at = bytes
            .windows(second.len())
            .position(|window| window == second.as_slice())
            .unwrap();
        let mut tampered = bytes.clone();
        tampered[at] = 0;
        let report = verify_segment(&tampered);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.location == "dictionary" && f.message.contains("index order")),
            "{}",
            messages(&report.findings)
        );
    }

    #[test]
    fn a_wrong_bucket_or_bound_is_reported_for_its_term() {
        let bytes = sample();
        let segment = Segment::parse(&bytes).unwrap();
        let entry = segment.term("even").unwrap().unwrap().entry;
        // The payload area starts after the postings area; find the first
        // entry's bucket byte by re-parsing the extent.
        let (postings_len, _) = segment.area_lengths();
        let dictionary_at = {
            let mut reader = crate::reader::Reader::new(&bytes);
            reader.take(4).unwrap();
            for _ in 0..6 {
                reader.varint().unwrap();
            }
            reader.position()
        };
        let dictionary_len = {
            let mut reader = crate::reader::Reader::new(&bytes);
            reader.take(4).unwrap();
            reader.varint().unwrap();
            reader.varint().unwrap();
            reader.varint().unwrap() as usize
        };
        let payload_at = dictionary_at + dictionary_len + postings_len;
        let extent = &bytes[payload_at + entry.payload.offset as usize
            ..payload_at + entry.payload.offset as usize + entry.payload.len as usize];
        let payload = crate::payload::Payload::parse(extent).unwrap();
        let first = payload.get(0).unwrap();
        assert_eq!(first.tf_bucket, 0);
        // The bucket byte of entry 0 is the first data byte.
        let data_at = extent.len() - payload.data_len();
        let mut tampered = bytes.clone();
        tampered[payload_at + entry.payload.offset as usize + data_at] = 3;
        let report = verify_segment(&tampered);
        let for_even: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|f| f.location == "term \"even\"")
            .collect();
        assert!(
            for_even
                .iter()
                .any(|f| f.message.contains("bucket 3") && f.message.contains("imply 0")),
            "{}",
            messages(&report.findings)
        );
        assert!(
            for_even.iter().any(|f| f.message.contains("block bound 0")),
            "{}",
            messages(&report.findings)
        );
        assert!(
            report
                .findings
                .iter()
                .all(|f| f.location == "term \"even\""),
            "{}",
            messages(&report.findings)
        );
    }

    #[test]
    fn every_single_byte_flip_is_survived_and_detected_unless_still_decodable() {
        let bytes = sample();
        let original = Segment::parse(&bytes).unwrap().records(|_| false).unwrap();
        let mut detected = 0usize;
        let mut silent = 0usize;
        for at in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0x55;
            let report = verify_segment(&flipped);
            if report.is_clean() {
                // Undetectable flips must leave a readable blob whose
                // only difference is in token positions or term bytes,
                // which nothing cross-checks.
                let records = Segment::parse(&flipped)
                    .unwrap()
                    .records(|_| false)
                    .unwrap_or_else(|error| {
                        panic!("flip at {at} is silent but unreadable: {error}")
                    });
                assert_eq!(records.len(), original.len(), "flip at {at}");
                for (a, b) in records.iter().zip(&original) {
                    assert_eq!(a.tid, b.tid, "flip at {at}");
                    assert_eq!(a.doc_len, b.doc_len, "flip at {at}");
                    let terms = |r: &ForwardRecord| {
                        r.terms
                            .iter()
                            .map(|t| t.positions.len())
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(terms(a), terms(b), "flip at {at}");
                }
                silent += 1;
            } else {
                detected += 1;
            }
        }
        assert!(
            detected > silent * 4,
            "{detected} detected, {silent} silent"
        );
    }

    #[test]
    fn dead_lists_must_be_subsets_of_the_document_table() {
        let bytes = sample();
        let documents = verify_segment(&bytes).documents;
        let mut builder = PostingsBuilder::default();
        builder.push(documents[3]).unwrap();
        builder.push(documents[10]).unwrap();
        let dead = builder.finish();
        assert!(verify_dead_list(&dead, &documents).is_empty());
        let mut builder = PostingsBuilder::default();
        builder.push(tid(0, 291)).unwrap();
        builder.push(documents[10]).unwrap();
        builder.push(tid(5_000_000, 1)).unwrap();
        let findings = verify_dead_list(&builder.finish(), &documents);
        assert_eq!(findings.len(), 2, "{}", messages(&findings));
        assert!(findings[0].message.contains("(0,291)"));
        assert!(findings[1].message.contains("2 entries"));
        assert_eq!(
            verify_dead_list(&dead[..dead.len() - 1], &documents).len(),
            1
        );
        assert_eq!(verify_dead_list(b"", &documents).len(), 1);
    }

    #[test]
    fn forward_streams_report_framing_and_duplicates() {
        let mut stream = Vec::new();
        for i in 0..5u32 {
            ForwardRecord::from_tokens(tid(i, 1), [("x", 1), ("y", 2)])
                .unwrap()
                .encode(&mut stream)
                .unwrap();
        }
        let report = verify_forward_stream(&stream);
        assert!(report.findings.is_empty(), "{}", messages(&report.findings));
        assert_eq!(report.records, 5);
        assert_eq!(report.tids.len(), 5);
        let report = verify_forward_stream(&stream[..stream.len() - 1]);
        assert_eq!(report.records, 4);
        assert_eq!(report.findings.len(), 1);
        assert!(report.findings[0].location.starts_with("record 4 at byte"));
        assert!(report.findings[0].message.contains("past the end"));
        let mut twice = stream.clone();
        twice.extend_from_slice(&stream[..ForwardRecord::encoded_len(&stream).unwrap()]);
        let report = verify_forward_stream(&twice);
        assert_eq!(report.records, 6);
        assert!(report.findings[0].message.contains("recorded twice"));
        let mut wrong_len = stream.clone();
        wrong_len[3] ^= 0x01; // doc_len of the first record
        let report = verify_forward_stream(&wrong_len);
        assert!(
            report.findings[0].message.contains("positions"),
            "{}",
            messages(&report.findings)
        );
        assert!(verify_forward_stream(b"").findings.is_empty());
    }

    #[test]
    fn findings_are_capped() {
        let mut findings = Findings::default();
        for i in 0..MAX_FINDINGS + 5 {
            findings.error("x", i);
        }
        let all = findings.finish();
        assert_eq!(all.len(), MAX_FINDINGS + 1);
        assert!(all.last().unwrap().message.contains("5 further"));
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
        fn ordered_membership_matches_independent_search(
            docs in prop::collection::btree_set(0u32..100_000, 0..500),
            postings in prop::collection::btree_set(0u32..100_000, 0..500),
        ) {
            let documents = docs.into_iter().map(|block| tid(block, 1)).collect::<Vec<_>>();
            let mut at = 0;
            for block in postings {
                let posting = tid(block, 1);
                prop_assert_eq!(ordered_rank(&documents, &mut at, posting), documents.binary_search(&posting).ok());
            }
        }

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
            let _ = verify_dead_list(&bytes, &[]);
            let _ = verify_forward_stream(&bytes);
        }
    }
}
