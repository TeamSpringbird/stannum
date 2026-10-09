// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Checking a segment blob in this shape completely: its header and areas,
//! the document set and DL sidecar, every term's slots, buckets and
//! positions, and that each document's positions across all terms add up
//! to its length.

use super::segment::Segment;
use crate::payload::Payload;
use crate::tf_bucket::TfBucket;
use crate::verify::{Findings, SegmentReport};
use crate::{Error, Tid};

fn describe(tid: Tid) -> String {
    format!("({},{})", tid.block, tid.offset)
}

/// Checks one segment blob in this shape.
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
    report.doc_count = Some(segment.documents);
    report.total_length = Some(segment.total_length);
    let documents = segment.docs.tids();
    if documents.windows(2).any(|w| w[0] >= w[1]) {
        findings.error("document set", "documents are not in ctid order");
    }
    let mut lengths = Vec::with_capacity(documents.len());
    let mut total = 0u64;
    for (rank, tid) in documents.iter().enumerate() {
        match segment.lengths.get(rank as u32) {
            Ok(0) => {
                findings.error(
                    format!("document {}", describe(*tid)),
                    "length is zero; the builder never records empty documents",
                );
                lengths.push(0);
            }
            Ok(length) => {
                total += u64::from(length);
                lengths.push(length);
            }
            Err(error) => {
                findings.error(format!("document {}", describe(*tid)), error);
                lengths.push(0);
            }
        }
    }
    if total != segment.total_length {
        findings.error(
            "header",
            format!(
                "total_length is {} but the document lengths sum to {total}",
                segment.total_length
            ),
        );
    }
    if segment.liveness.dead != 0 {
        findings.error(
            "liveness",
            format!(
                "{} documents are dead in the blob; dead documents are published apart",
                segment.liveness.dead
            ),
        );
    }

    let mut positions_of = vec![0u64; documents.len()];
    let mut complete = true;
    let mut previous: Option<String> = None;
    let mut scratch = Vec::new();
    for item in segment.dictionary().iter() {
        let (term, entry) = match item {
            Ok(item) => item,
            Err(error) => {
                findings.error("term map", error);
                complete = false;
                break;
            }
        };
        if previous.as_deref().is_some_and(|p| p >= term.as_str()) {
            findings.error(
                format!("term {term:?}"),
                format!(
                    "follows {:?} out of order",
                    previous.as_deref().unwrap_or("")
                ),
            );
        }
        let location = format!("term {term:?}");
        previous = Some(term);
        if entry.df == 0 {
            findings.error(&location, "term has no documents");
            continue;
        }
        let checked = (|| -> crate::Result<Vec<String>> {
            let mut problems = Vec::new();
            let found = segment.resolve(entry)?;
            let footer = found.postings.footer(
                segment.block_size,
                entry.max_tf_bucket,
                segment.adaptive_tf,
            )?;
            let slots = found.postings.slots(&segment.docs.geometry)?;
            if slots.len() != entry.df as usize {
                problems.push(format!(
                    "holds {} postings but df is {}",
                    slots.len(),
                    entry.df
                ));
                return Ok(problems);
            }
            if slots.windows(2).any(|w| w[0] >= w[1]) {
                problems.push("postings are not in ctid order".to_owned());
            }
            let (stream, _) = segment.positions(&entry)?;
            let stream = super::positions::Positions::parse(stream)?.payload();
            let payload = Payload::parse(&stream)?;
            if payload.count() != entry.df {
                problems.push(format!(
                    "positions hold {} entries but df is {}",
                    payload.count(),
                    entry.df
                ));
                return Ok(problems);
            }
            let mut cursor = payload.cursor();
            let mut largest = 0u8;
            for (i, slot) in slots.iter().enumerate() {
                let rank = segment
                    .docs
                    .rank(*slot)
                    .ok_or(Error::Corrupt("posting outside the document set"))?;
                let bucket = footer.bucket(found.postings.tf, i as u32)?;
                largest = largest.max(bucket);
                scratch.clear();
                cursor.next_into(&mut scratch)?;
                let expected = TfBucket::from_count(scratch.len() as u32).value();
                if bucket != expected {
                    problems.push(format!(
                        "posting {i} has bucket {bucket} but {} positions",
                        scratch.len()
                    ));
                }
                positions_of[rank as usize] += scratch.len() as u64;
            }
            if largest != entry.max_tf_bucket {
                problems.push(format!(
                    "largest bucket is {largest} but the term map says {}",
                    entry.max_tf_bucket
                ));
            }
            Ok(problems)
        })();
        match checked {
            Ok(problems) => {
                for problem in problems {
                    findings.error(&location, problem);
                }
            }
            Err(error) => {
                findings.error(&location, error);
                complete = false;
            }
        }
    }
    if complete {
        for (rank, tid) in documents.iter().enumerate() {
            if positions_of[rank] != u64::from(lengths[rank]) {
                findings.error(
                    format!("document {}", describe(*tid)),
                    format!(
                        "length is {} but its terms hold {} positions",
                        lengths[rank], positions_of[rank]
                    ),
                );
            }
        }
    }
    report.documents = documents;
    report.findings = findings.finish();
    report
}
