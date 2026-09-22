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
//! * every term: postings and payload extents inside their areas and not
//!   overlapping the previous term's, postings decoding to `df` increasing
//!   locations that are all in the document table, the payload holding one
//!   entry per posting with a valid bucket that matches its position count,
//!   `max_tf_bucket` equal to the largest bucket, and (from `LSG2` on) score
//!   bounds equal to what the postings and document lengths imply, in the
//!   layout the segment's format writes;
//! * every term from `LSG4` on: the ordinals extent inside its area and not
//!   overlapping the previous term's, the stream well formed (see
//!   [`crate::ordinals::validate`]) and naming, in order, exactly the
//!   documents of the term's postings;
//! * the document table: decodes to `doc_count` increasing locations;
//! * document lengths: nonzero, summing to `total_length`, and equal to the
//!   number of positions the term payloads hold for that document;
//! * the page table from `LSG4` on: equal to the one the document table
//!   implies.
//!
//! Bytes of an area that no extent covers are not examined: no reader reaches
//! them.

use std::fmt;

use crate::dictionary::TermEntry;
use crate::forward::ForwardRecord;
use crate::ordinals::{self, Ordinals};
use crate::postings::{BLOCK_POSTINGS, BlockBound, Postings};
use crate::segment::{AreaFetch, Format, PAGE_ENTRY, Segment, page_table};
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
pub(crate) fn ordered_rank(documents: &[Tid], at: &mut usize, target: Tid) -> Option<usize> {
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

/// Compares a term's ordinal stream with its postings: well formed for that
/// many documents, and naming them in order. `expected` holds each posting's
/// ordinal in `documents`, where it has one; a term with postings outside the
/// table is only checked for form, as that finding already covers it.
fn check_ordinals(
    bytes: &[u8],
    tids: &[Tid],
    expected: &[Option<usize>],
    documents: &[Tid],
    doc_count: u32,
    // The postings' buckets and lengths, for a stream that carries chunk
    // bounds; `None` for a stream without them.
    scores: Option<&[(u8, u32)]>,
) -> Result<(), Finding> {
    let expected: Option<Vec<u32>> = expected
        .iter()
        .map(|ordinal| ordinal.and_then(|ordinal| u32::try_from(ordinal).ok()))
        .collect();
    // The encoding is canonical, so the stream the writer would produce for
    // these postings is the only one that passes every check below. Comparing
    // with it first spares a sound term, the common case, a second decoding.
    let canonical = expected.as_deref().map(|expected| match scores {
        Some(scores) if scores.len() == expected.len() => ordinals::encode_scored(expected, scores),
        Some(_) => Vec::new(),
        None => ordinals::encode(expected),
    });
    if canonical
        .as_deref()
        .is_some_and(|canonical| canonical == bytes)
    {
        return Ok(());
    }
    let bounded = scores.is_some();
    let malformed = |error: Error| Finding::error("", format!("ordinals: {error}"));
    ordinals::validate(bytes, tids.len() as u32, doc_count, bounded).map_err(malformed)?;
    let found = Ordinals::open(bytes, bytes.len() as u64, bounded)
        .and_then(|stream| stream.to_vec())
        .map_err(malformed)?;
    if let (Some(scores), Some(expected)) = (scores, expected.as_deref())
        && scores.len() == expected.len()
        && let Some(canonical) = canonical.as_deref()
        && let Ok(stream) = Ordinals::open(bytes, bytes.len() as u64, true)
        && let Ok(wanted) = Ordinals::open(canonical, canonical.len() as u64, true)
        && stream.bounds() != wanted.bounds()
    {
        return Err(Finding::error(
            "",
            "ordinal chunk bounds disagree with the postings and lengths",
        ));
    }
    let Some(expected) = expected else {
        return Ok(());
    };
    let differing = found.iter().zip(&expected).filter(|(a, b)| a != b).count();
    let Some(index) = found.iter().zip(&expected).position(|(a, b)| a != b) else {
        return Err(Finding::warning(
            "",
            "ordinals: the stream names its postings but is not encoded as the writer would",
        ));
    };
    let named = documents
        .get(found[index] as usize)
        .map_or_else(|| "no document".to_owned(), |tid| describe(*tid));
    Err(Finding::error(
        "",
        format!(
            "ordinal {index} is {} and names {named} but posting {index} is {}; {differing} of {} ordinals differ from their postings",
            found[index],
            describe(tids[index]),
            tids.len()
        ),
    ))
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

    if format.has_ordinals() && table_ok {
        match segment.page_table() {
            Ok(pages) => check_page_table(pages, &documents, &mut findings),
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
    let (postings_len, payload_len) = segment.area_lengths();
    let ordinals_len = segment.sections().ordinals;
    let mut previous: Option<String> = None;
    let mut postings_end = 0u64;
    let mut payload_end = 0u64;
    let mut ordinals_end = 0u64;
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
            // An unusable ordinals extent does not stop the checks of the
            // term's postings and payload, which no ordinal stream feeds.
            let mut ordinals_extent = format.has_ordinals().then_some(entry.ordinals);
            if let Some(extent) = ordinals_extent {
                let extent_end = extent.offset.saturating_add(u64::from(extent.len));
                if extent_end > ordinals_len as u64 {
                    findings.error(
                        location(),
                        format!(
                            "ordinals extent {}+{} exceeds the ordinals area of {ordinals_len} bytes",
                            extent.offset, extent.len
                        ),
                    );
                    ordinals_extent = None;
                } else if extent.offset < ordinals_end {
                    findings.error(
                        location(),
                        format!(
                            "ordinals extent starts at {} inside the previous term's extent ending at {ordinals_end}",
                            extent.offset
                        ),
                    );
                }
                ordinals_end = ordinals_end.max(extent_end);
            }
            postings_end = postings_end.max(postings_extent_end);
            payload_end = payload_end.max(payload_extent_end);
            if !resolvable {
                complete = false;
                continue;
            }
            let entry = TermEntry {
                ordinals: ordinals_extent.unwrap_or_default(),
                ..entry
            };
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

            // Ordinals: the same documents as the postings, by table ordinal.
            if let Some(extent) = ordinals_extent
                && let Err(finding) = segment
                    .ordinals_bytes(extent.offset, extent.len as usize)
                    .map_err(|error| Finding::error("", format!("ordinals: {error}")))
                    .and_then(|bytes| {
                        check_ordinals(
                            bytes,
                            &tids,
                            &ordinals,
                            &documents,
                            segment.document_count(),
                            format.has_chunk_bounds().then_some(&scores[..]),
                        )
                    })
            {
                findings.push(finding.within(&location()));
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
                Format::Lsg3 | Format::Lsg4 | Format::Lsg5 => {
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
    use crate::dictionary::Extent;
    use crate::postings::PostingsBuilder;
    use crate::segment::SegmentBuilder;

    fn tid(block: u32, offset: u16) -> Tid {
        Tid::new(block, offset).unwrap()
    }

    /// A segment with more than one dictionary block, a term with several
    /// score-bound blocks, and both sparse and grouped postings.
    fn sample() -> Vec<u8> {
        sample_as(Format::CURRENT)
    }

    fn sample_as(format: Format) -> Vec<u8> {
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
        builder.finish_as(format)
    }

    /// Where the length table, the ordinals area and the page table start.
    fn trailing_sections(bytes: &[u8]) -> (usize, usize, usize) {
        let sections = Segment::parse(bytes).unwrap().sections();
        let pages_at = bytes.len() - sections.pages;
        let ordinals_at = pages_at - sections.ordinals;
        (ordinals_at - sections.lengths, ordinals_at, pages_at)
    }

    /// `bytes` with every dictionary entry passed through `edit`.
    fn with_entries(bytes: &[u8], mut edit: impl FnMut(&str, &mut TermEntry)) -> Vec<u8> {
        let segment = Segment::parse(bytes).unwrap();
        let sections = segment.sections();
        let mut dictionary = crate::dictionary::DictionaryBuilder::with_format(segment.format());
        for item in segment.dictionary().unwrap().iter() {
            let (term, mut entry) = item.unwrap();
            edit(&term, &mut entry);
            dictionary.push(&term, entry).unwrap();
        }
        let dictionary = dictionary.finish();
        let mut reader = crate::reader::Reader::new(bytes);
        let mut out = reader.take(4).unwrap().to_vec();
        let fields = if segment.format().has_ordinals() {
            8
        } else {
            6
        };
        for field in 0..fields {
            let value = reader.varint().unwrap();
            crate::varint::put(
                &mut out,
                if field == 2 {
                    dictionary.len() as u64
                } else {
                    value
                },
            );
        }
        assert_eq!(reader.position(), sections.header);
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
        let at = trailing_sections(bytes).1 + entry.ordinals.offset as usize;
        at..at + entry.ordinals.len as usize
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
        let report = verify_segment(&sample_as(Format::Lsg1));
        assert!(report.legacy);
        assert_eq!(report.findings.len(), 1, "{}", messages(&report.findings));
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.severity == Severity::Warning && f.message.contains("LSG1"))
        );
        // Older headers lack the lengths `LSG4` added, so relabelling a blob
        // breaks the total length rather than reinterpreting its sections.
        for older in [Format::Lsg1, Format::Lsg2, Format::Lsg3] {
            let mut relabelled = bytes.clone();
            relabelled[..4].copy_from_slice(older.magic());
            let report = verify_segment(&relabelled);
            assert_eq!(report.findings.len(), 1, "{older}");
            assert_eq!(report.findings[0].location, "header", "{older}");
        }
    }

    #[test]
    fn swapped_lengths_and_total_length_are_caught() {
        let bytes = sample();
        // The last two documents have lengths 2 and 2; the first two
        // documents (0,1) and (0,2) have lengths 3 and 4.
        let (lengths_at, ordinals_at, _) = trailing_sections(&bytes);
        assert_eq!(ordinals_at - lengths_at, 450 * 4);
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
        let sections = segment.sections();
        let payload_at = sections.header + sections.dictionary + sections.postings;
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

    fn only_for<'a>(report: &'a SegmentReport, location: &str) -> Vec<&'a str> {
        assert!(
            report.findings.iter().all(|f| f.location == location),
            "{}",
            messages(&report.findings)
        );
        report.findings.iter().map(|f| f.message.as_str()).collect()
    }

    #[test]
    fn a_malformed_ordinal_stream_is_reported_for_its_term() {
        let bytes = sample();
        assert_eq!(with_entries(&bytes, |_, _| ()), bytes);
        // `common` holds every document: one array chunk, whose last two
        // members swapped are no longer ascending.
        let stream = ordinal_stream(&bytes, "common");
        let mut swapped = bytes.clone();
        swapped.swap(stream.end - 4, stream.end - 2);
        swapped.swap(stream.end - 3, stream.end - 1);
        let report = verify_segment(&swapped);
        let found = only_for(&report, "term \"common\"");
        assert_eq!(
            found,
            ["ordinals: corrupt segment data: ordinal array order"]
        );
        // A list stream counting more ordinals than it holds, or fewer.
        // One member: the count, its chunk bound and one delta.
        let stream = ordinal_stream(&bytes, "term0003");
        assert!((5..=10).contains(&stream.len()), "{}", stream.len());
        for (count, problem) in [
            (2, "ordinals: truncated segment data"),
            (0, "ordinals: corrupt segment data: ordinal list length"),
        ] {
            let mut miscounted = bytes.clone();
            miscounted[stream.start] = count;
            let report = verify_segment(&miscounted);
            assert_eq!(only_for(&report, "term \"term0003\""), [problem]);
        }
        // A stream cut short by its extent.
        let shortened = with_entries(&bytes, |term, entry| {
            if term == "even" {
                entry.ordinals.len -= 1;
            }
        });
        let report = verify_segment(&shortened);
        let found = only_for(&report, "term \"even\"");
        assert_eq!(found, ["ordinals: truncated segment data"]);
        // A term of an `LSG4` segment always has a stream.
        let missing = with_entries(&bytes, |term, entry| {
            if term == "term0149" {
                entry.ordinals.len = 0;
            }
        });
        let report = verify_segment(&missing);
        let found = only_for(&report, "term \"term0149\"");
        assert_eq!(found, ["ordinals: truncated segment data"]);
    }

    #[test]
    fn a_valid_ordinal_stream_naming_the_wrong_document_is_reported() {
        let bytes = sample();
        // The 150 even documents are ordinals 0, 2, .. 298; the last becomes
        // 299, which is a document, but an odd one.
        let stream = ordinal_stream(&bytes, "even");
        assert_eq!(bytes[stream.end - 2..stream.end], 298u16.to_le_bytes());
        let mut renamed = bytes.clone();
        renamed[stream.end - 2..stream.end].copy_from_slice(&299u16.to_le_bytes());
        let report = verify_segment(&renamed);
        let found = only_for(&report, "term \"even\"");
        assert_eq!(
            found,
            [
                "ordinal 149 is 299 and names (42,6) but posting 149 is (42,5); 1 of 150 ordinals differ from their postings"
            ]
        );
        // Two terms of one document each, exchanging streams: each stream is
        // well formed and inside the area, and each names the other's document.
        let segment = Segment::parse(&bytes).unwrap();
        let first = segment.term("term0003").unwrap().unwrap().entry.ordinals;
        let second = segment.term("term0004").unwrap().unwrap().entry.ordinals;
        let exchanged = with_entries(&bytes, |term, entry| match term {
            "term0003" => entry.ordinals = second,
            "term0004" => entry.ordinals = first,
            _ => (),
        });
        let report = verify_segment(&exchanged);
        let found: Vec<String> = report.findings.iter().map(ToString::to_string).collect();
        assert_eq!(
            found,
            [
                "error: term \"term0003\": ordinal 0 is 304 and names (1012,1) but posting 0 is (1009,1); 1 of 1 ordinals differ from their postings",
                "error: term \"term0004\": ordinals extent starts at 2217 inside the previous term's extent ending at 2231",
                "error: term \"term0004\": ordinal 0 is 303 and names (1009,1) but posting 0 is (1012,1); 1 of 1 ordinals differ from their postings",
            ]
        );
    }

    #[test]
    fn a_wrong_page_table_entry_is_reported() {
        let bytes = sample();
        let (_, _, pages_at) = trailing_sections(&bytes);
        // Seven documents a page: the second page starts at ordinal 7.
        let entry = pages_at + PAGE_ENTRY;
        assert_eq!(bytes[entry..entry + 4], 1u32.to_le_bytes());
        assert_eq!(bytes[entry + 4..entry + 8], 7u32.to_le_bytes());
        let mut shifted = bytes.clone();
        shifted[entry + 4] = 8;
        let report = verify_segment(&shifted);
        assert_eq!(
            only_for(&report, "page table"),
            [
                "entry 1 is block 1 from ordinal 8 but the document table implies block 1 from ordinal 7"
            ]
        );
        let mut renumbered = bytes.clone();
        renumbered[entry] = 2;
        let report = verify_segment(&renumbered);
        assert_eq!(
            only_for(&report, "page table"),
            [
                "entry 1 is block 2 from ordinal 7 but the document table implies block 1 from ordinal 7"
            ]
        );
        // A table missing its last page: the header still adds up, since the
        // ordinals area is taken to be one entry longer.
        let mut reader = crate::reader::Reader::new(&bytes);
        reader.take(4).unwrap();
        let fields: Vec<u64> = (0..8).map(|_| reader.varint().unwrap()).collect();
        let mut short = bytes[..4].to_vec();
        for (field, value) in fields.iter().enumerate() {
            crate::varint::put(
                &mut short,
                if field == 7 {
                    value - PAGE_ENTRY as u64
                } else {
                    *value
                },
            );
        }
        short.extend_from_slice(&bytes[reader.position()..bytes.len() - PAGE_ENTRY]);
        let report = verify_segment(&short);
        assert_eq!(
            only_for(&report, "page table"),
            [format!(
                "holds {} entries but the documents span {} pages",
                fields[7] as usize / PAGE_ENTRY - 1,
                fields[7] as usize / PAGE_ENTRY
            )]
        );
    }

    #[test]
    fn an_ordinals_extent_outside_its_area_is_reported() {
        let bytes = sample();
        let area = Segment::parse(&bytes).unwrap().sections().ordinals;
        for (offset, len) in [(area as u64, 1), (area as u64 - 1, 4), (1 << 40, 3)] {
            let moved = with_entries(&bytes, |term, entry| {
                if term == "term0149" {
                    entry.ordinals = Extent { offset, len };
                }
            });
            // The last term, so no later extent is measured from this one.
            // Its postings and payload are still checked, and found sound.
            let report = verify_segment(&moved);
            assert_eq!(
                only_for(&report, "term \"term0149\""),
                [format!(
                    "ordinals extent {offset}+{len} exceeds the ordinals area of {area} bytes"
                )]
            );
            // Readers refuse the term rather than read outside the area.
            let segment = Segment::parse(&moved).unwrap();
            assert_eq!(segment.term("term0149").err(), Some(Error::Truncated));
        }
    }

    #[test]
    fn older_formats_have_no_ordinals_or_pages_to_check() {
        for format in [Format::Lsg2, Format::Lsg3] {
            let bytes = sample_as(format);
            let report = verify_segment(&bytes);
            assert!(
                report.is_clean(),
                "{format}: {}",
                messages(&report.findings)
            );
            let segment = Segment::parse(&bytes).unwrap();
            assert_eq!(segment.sections().ordinals + segment.sections().pages, 0);
            assert!(
                segment
                    .term("common")
                    .unwrap()
                    .unwrap()
                    .ordinals()
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn every_single_byte_flip_is_survived_and_detected_unless_still_decodable() {
        let bytes = sample();
        let original = Segment::parse(&bytes).unwrap().records(|_| false).unwrap();
        let mut detected = 0usize;
        let mut silent = 0usize;
        let (_, ordinals_at, pages_at) = trailing_sections(&bytes);
        assert!(ordinals_at < pages_at && pages_at < bytes.len());
        for at in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0x55;
            let report = verify_segment(&flipped);
            if report.is_clean() {
                // Every byte of the ordinals area and the page table is
                // implied by the postings and the document table.
                assert!(at < ordinals_at, "flip at {at} is silent");
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
