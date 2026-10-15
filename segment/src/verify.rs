// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Whole-blob consistency checks that list every problem found instead of
//! stopping at the first one.
//!
//! The readers in this crate fail on the first malformed byte they touch,
//! which is right for a query but useless for an operator asking "what is
//! wrong with this index?". The checkers here decode a whole dead list or
//! forward stream and collect [`Finding`]s: each names a location
//! inside the blob and says what disagrees with what. A valid blob yields no
//! findings. Nothing here can panic on malformed input; every decode goes
//! through the bounds-checked readers.
//!
//! A segment blob is checked by [`crate::tinshape::verify`]; this module
//! keeps the findings, the dead-list check and the forward-stream check.

use std::fmt;

use crate::forward::ForwardRecord;
use crate::ordinals::{self, Ordinals};
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
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    fn tid(block: u32, offset: u16) -> Tid {
        Tid::new(block, offset).unwrap()
    }

    fn messages(findings: &[Finding]) -> String {
        findings
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
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

    fn forward(documents: &BTreeMap<Tid, Vec<(String, u32)>>) -> Vec<u8> {
        let mut stream = Vec::new();
        for (tid, tokens) in documents {
            ForwardRecord::from_tokens(*tid, tokens.iter().map(|(t, p)| (t.as_str(), *p)))
                .unwrap()
                .encode(&mut stream)
                .unwrap();
        }
        stream
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

        #[test]
        fn valid_forward_streams_never_yield_findings(documents in documents()) {
            let report = verify_forward_stream(&forward(&documents));
            prop_assert!(report.findings.is_empty(), "{:?}", report.findings);
            prop_assert_eq!(report.records as usize, documents.len());
        }

        #[test]
        fn mutated_streams_never_panic(
            documents in documents(),
            edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
        ) {
            let mut bytes = forward(&documents);
            if bytes.is_empty() {
                return Ok(());
            }
            for (at, value) in &edits {
                let at = at.index(bytes.len());
                bytes[at] = *value;
            }
            let _ = verify_dead_list(&bytes, 10);
            let _ = verify_forward_stream(&bytes);
        }
    }
}
