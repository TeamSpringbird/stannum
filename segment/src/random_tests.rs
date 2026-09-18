//! Property tests against `BTreeSet`/`BTreeMap` oracles.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use crate::dictionary::{Dictionary, DictionaryBuilder, Extent, TermEntry};
use crate::forward::ForwardRecord;
use crate::payload::{Payload, PayloadBuilder};
use crate::postings::{Postings, PostingsBuilder};
use crate::set::{Cursor, Difference, Intersection, Union, collect};
use crate::tid::MAX_OFFSET;
use crate::{Result, Tid};

/// Tuple locations with clustered density: a few blocks hold many tuples, most
/// hold few, and the block range spans several 256-page groups.
fn tid_set(max_len: usize) -> impl Strategy<Value = BTreeSet<Tid>> {
    prop::collection::btree_set(
        (0u32..2048, 1u16..=MAX_OFFSET).prop_map(|(raw_block, offset)| {
            // Fold the block space so some blocks are hot.
            let block = if raw_block % 3 == 0 {
                raw_block / 64
            } else {
                raw_block
            };
            Tid::new(block, if block < 4 { offset } else { 1 + offset % 20 }).unwrap()
        }),
        0..max_len,
    )
}

/// Default case count, overridable with `PROPTEST_CASES` for heavier runs.
fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn encode(set: &BTreeSet<Tid>) -> Vec<u8> {
    let mut builder = PostingsBuilder::default();
    for tid in set {
        builder.push(*tid).unwrap();
    }
    builder.finish()
}

fn successor(set: &BTreeSet<Tid>, target: Tid) -> Option<Tid> {
    set.range(target..).next().copied()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(256), ..ProptestConfig::default() })]

    #[test]
    fn postings_round_trip_and_seek_match_oracle(
        set in tid_set(1500),
        targets in prop::collection::vec((0u32..2100, 1u16..=MAX_OFFSET), 0..40),
    ) {
        let bytes = encode(&set);
        let postings = Postings::parse(&bytes).unwrap();
        prop_assert_eq!(postings.count() as usize, set.len());
        prop_assert_eq!(postings.to_vec().unwrap(), set.iter().copied().collect::<Vec<_>>());
        let ordered: Vec<Tid> = set.iter().copied().collect();

        // Fresh cursor per target.
        for (block, offset) in &targets {
            let target = Tid::new(*block, *offset).unwrap();
            let mut cursor = postings.cursor().unwrap();
            cursor.seek(target).unwrap();
            let expected = successor(&set, target);
            prop_assert_eq!(cursor.current(), expected);
            if let Some(found) = expected {
                prop_assert_eq!(cursor.ordinal() as usize, ordered.binary_search(&found).unwrap());
            }
        }

        // One cursor, monotone seeks, with ordinals checked throughout.
        let mut sorted_targets: Vec<Tid> = targets.iter().map(|(b, o)| Tid::new(*b, *o).unwrap()).collect();
        sorted_targets.sort_unstable();
        let mut cursor = postings.cursor().unwrap();
        // After an advance the cursor cannot move backwards, so the oracle
        // answers for the larger of the target and the advanced-to position.
        let mut floor: Option<Tid> = None;
        for target in sorted_targets {
            cursor.seek(target).unwrap();
            let effective = floor.map_or(target, |floor| floor.max(target));
            let expected = successor(&set, effective);
            prop_assert_eq!(cursor.current(), expected);
            if let Some(found) = expected {
                prop_assert_eq!(cursor.ordinal() as usize, ordered.binary_search(&found).unwrap());
                // Advance once and check again so advance-after-seek is covered.
                cursor.advance().unwrap();
                let next = ordered.binary_search(&found).unwrap() + 1;
                prop_assert_eq!(cursor.current(), ordered.get(next).copied());
                floor = ordered.get(next).copied().or(Some(Tid { block: u32::MAX, offset: 1 }));
                if next < ordered.len() {
                    prop_assert_eq!(cursor.ordinal() as usize, next);
                }
            }
        }

        // Rank of every member and of some non-members.
        let mut cursor = postings.cursor().unwrap();
        for (index, tid) in ordered.iter().enumerate() {
            prop_assert_eq!(cursor.rank(*tid).unwrap(), Some(index as u32));
        }
        let mut cursor = postings.cursor().unwrap();
        for (block, offset) in targets.iter().take(10) {
            let probe = Tid::new(*block, *offset).unwrap();
            let expected = set.contains(&probe).then(|| ordered.binary_search(&probe).unwrap() as u32);
            let mut fresh = postings.cursor().unwrap();
            prop_assert_eq!(fresh.rank(probe).unwrap(), expected);
            if cursor.current().is_some_and(|current| probe >= current) {
                prop_assert_eq!(cursor.rank(probe).unwrap(), expected);
            }
        }
    }

    #[test]
    fn set_operations_match_oracle(
        a in tid_set(400),
        b in tid_set(400),
        c in tid_set(400),
    ) {
        let (ba, bb, bc) = (encode(&a), encode(&b), encode(&c));
        let (pa, pb, pc) = (
            Postings::parse(&ba).unwrap(),
            Postings::parse(&bb).unwrap(),
            Postings::parse(&bc).unwrap(),
        );
        let and = Intersection::new(vec![pa.cursor().unwrap(), pb.cursor().unwrap(), pc.cursor().unwrap()]).unwrap();
        let expected: Vec<Tid> = a.iter().filter(|t| b.contains(t) && c.contains(t)).copied().collect();
        prop_assert_eq!(collect(and).unwrap(), expected);

        let or = Union::new(vec![pa.cursor().unwrap(), pb.cursor().unwrap(), pc.cursor().unwrap()]);
        let expected: Vec<Tid> = a.union(&b).copied().collect::<BTreeSet<_>>().union(&c).copied().collect();
        prop_assert_eq!(collect(or).unwrap(), expected);

        let two = crate::set::AtLeast::new(vec![pa.cursor().unwrap(), pb.cursor().unwrap(), pc.cursor().unwrap()], 2).unwrap();
        let expected: Vec<Tid> = a.iter().chain(&b).chain(&c).copied().collect::<BTreeSet<_>>().into_iter()
            .filter(|t| [&a, &b, &c].iter().filter(|s| s.contains(t)).count() >= 2).collect();
        prop_assert_eq!(collect(two).unwrap(), expected);

        let diff = Difference::new(pa.cursor().unwrap(), pb.cursor().unwrap()).unwrap();
        let expected: Vec<Tid> = a.difference(&b).copied().collect();
        prop_assert_eq!(collect(diff).unwrap(), expected);

        // (a OR b) AND NOT c, composed through boxed cursors, with a mid-stream seek.
        let or: Box<dyn Cursor> = Box::new(Union::new(vec![pa.cursor().unwrap(), pb.cursor().unwrap()]));
        let mut composed = Difference::new(or, pc.cursor().unwrap()).unwrap();
        let expected: Vec<Tid> = a.union(&b).filter(|t| !c.contains(t)).copied().collect();
        if let Some(middle) = expected.get(expected.len() / 2) {
            composed.seek(*middle).unwrap();
            prop_assert_eq!(collect(composed).unwrap(), expected[expected.len() / 2..].to_vec());
        } else {
            prop_assert_eq!(collect(composed).unwrap(), expected);
        }
    }

    #[test]
    fn payload_round_trips_by_ordinal(
        entries in prop::collection::vec(
            (0u8..=15, prop::collection::btree_set(0u32..5000, 1..12)),
            0..300,
        ),
        probes in prop::collection::vec(0usize..300, 0..30),
    ) {
        let mut builder = PayloadBuilder::default();
        let entries: Vec<(u8, Vec<u32>)> = entries
            .into_iter()
            .map(|(bucket, positions)| (bucket, positions.into_iter().collect()))
            .collect();
        for (bucket, positions) in &entries {
            builder.push(*bucket, positions).unwrap();
        }
        let bytes = builder.finish();
        let payload = Payload::parse(&bytes).unwrap();
        prop_assert_eq!(payload.count() as usize, entries.len());
        let mut cursor = payload.cursor();
        let mut scratch = Vec::new();
        for (bucket, positions) in &entries {
            let decoded = cursor.next_into(&mut scratch).unwrap();
            prop_assert_eq!(decoded, *bucket);
            prop_assert_eq!(&scratch, positions);
            scratch.clear();
        }
        prop_assert!(cursor.next_entry().is_err());
        for probe in probes {
            if probe < entries.len() {
                let entry = payload.get(probe as u32).unwrap();
                prop_assert_eq!((entry.tf_bucket, entry.positions), entries[probe].clone());
                cursor.seek(probe as u32).unwrap();
                let entry = cursor.next_entry().unwrap();
                prop_assert_eq!((entry.tf_bucket, entry.positions), entries[probe].clone());
            } else {
                prop_assert!(payload.get(probe as u32).is_err());
            }
        }
    }

    #[test]
    fn dictionary_matches_btreemap(
        terms in prop::collection::btree_map(
            "[a-c\u{e9}\u{65e5}]{1,6}",
            (0u32..1000, 0u8..=15, 0u64..1_000_000, 0u32..10_000),
            0..200,
        ),
        probes in prop::collection::vec("[a-d\u{e9}\u{65e5}]{0,6}", 0..30),
    ) {
        let mut builder = DictionaryBuilder::default();
        let expected: BTreeMap<String, TermEntry> = terms
            .into_iter()
            .map(|(term, (df, bucket, offset, len))| {
                (
                    term,
                    TermEntry {
                        df,
                        max_tf_bucket: bucket,
                        postings: Extent { offset, len },
                        payload: Extent { offset: offset * 2, len: len / 2 },
                    },
                )
            })
            .collect();
        for (term, entry) in &expected {
            builder.push(term, *entry).unwrap();
        }
        let bytes = builder.finish();
        let dictionary = Dictionary::parse(&bytes).unwrap();
        prop_assert_eq!(dictionary.len(), expected.len());
        let all: Vec<(String, TermEntry)> = dictionary.iter().collect::<Result<_>>().unwrap();
        prop_assert_eq!(all, expected.iter().map(|(t, e)| (t.clone(), *e)).collect::<Vec<_>>());
        for probe in probes {
            prop_assert_eq!(dictionary.get(&probe).unwrap(), expected.get(&probe).copied());
            let from: Vec<String> = dictionary.iter_from(&probe).map(|r| r.unwrap().0).collect();
            let oracle: Vec<String> = expected.range(probe.clone()..).map(|(t, _)| t.clone()).collect();
            prop_assert_eq!(from, oracle);
            let prefixed: Vec<String> = dictionary.prefix(&probe).map(|r| r.unwrap().0).collect();
            let oracle: Vec<String> = expected.keys().filter(|t| t.starts_with(&probe)).cloned().collect();
            prop_assert_eq!(prefixed, oracle);
            let upper = format!("{probe}b");
            let ranged: Vec<String> = dictionary
                .range(Some(&probe), Some(&upper))
                .map(|r| r.unwrap().0)
                .collect();
            let oracle: Vec<String> = expected
                .range(probe.clone()..=upper.clone())
                .map(|(t, _)| t.clone())
                .collect();
            prop_assert_eq!(ranged, oracle);
        }
    }

    #[test]
    fn forward_records_round_trip(
        docs in prop::collection::vec(
            (
                (0u32..100, 1u16..=MAX_OFFSET),
                prop::collection::vec("[a-c]{1,4}", 0..40),
            ),
            0..20,
        ),
    ) {
        let mut bytes = Vec::new();
        let mut expected = Vec::new();
        for ((block, offset), words) in docs {
            let tokens: Vec<(&str, u32)> = words
                .iter()
                .enumerate()
                .map(|(i, word)| (word.as_str(), 1 + i as u32 * 2))
                .collect();
            let record = ForwardRecord::from_tokens(Tid::new(block, offset).unwrap(), tokens.iter().copied()).unwrap();
            prop_assert_eq!(record.tokens(), tokens);
            record.encode(&mut bytes).unwrap();
            expected.push(record);
        }
        let decoded: Vec<ForwardRecord> = crate::forward::records(&bytes).collect::<Result<_>>().unwrap();
        prop_assert_eq!(decoded, expected);
    }

    #[test]
    fn segment_assembly_matches_per_document_oracle(
        docs in prop::collection::btree_map(
            (0u32..300, 1u16..=MAX_OFFSET),
            prop::collection::vec("[a-e]{1,3}", 0..30),
            0..40,
        ),
        probes in prop::collection::vec("[a-f]{1,3}", 0..10),
    ) {
        use crate::segment::{Segment, SegmentBuilder};
        use crate::tf_bucket::TfBucket;
        let mut builder = SegmentBuilder::default();
        let mut oracle: BTreeMap<String, BTreeMap<Tid, Vec<u32>>> = BTreeMap::new();
        let mut lengths: BTreeMap<Tid, u32> = BTreeMap::new();
        for ((block, offset), words) in &docs {
            let tid = Tid::new(*block, *offset).unwrap();
            let tokens: Vec<(&str, u32)> = words.iter().enumerate().map(|(i, w)| (w.as_str(), i as u32 + 1)).collect();
            builder.add_document(tid, tokens.iter().copied()).unwrap();
            lengths.insert(tid, words.len() as u32);
            for (word, position) in tokens {
                oracle.entry(word.to_owned()).or_default().entry(tid).or_default().push(position);
            }
        }
        let bytes = builder.finish();
        let segment = Segment::parse(&bytes).unwrap();
        prop_assert_eq!(segment.document_count() as usize, lengths.len());
        prop_assert_eq!(segment.total_length(), lengths.values().map(|l| u64::from(*l)).sum::<u64>());
        prop_assert_eq!(collect(segment.documents().unwrap()).unwrap(), lengths.keys().copied().collect::<Vec<_>>());
        for (tid, len) in &lengths {
            prop_assert_eq!(segment.document_length(*tid).unwrap(), Some(*len));
        }
        let listed: Vec<String> = segment.dictionary().iter().map(|r| r.unwrap().0).collect();
        prop_assert_eq!(&listed, &oracle.keys().cloned().collect::<Vec<_>>());
        for word in oracle.keys().chain(probes.iter()) {
            let resolved = segment.term(word).unwrap();
            match oracle.get(word) {
                None => prop_assert!(resolved.is_none()),
                Some(expected) => {
                    let term = resolved.unwrap();
                    prop_assert_eq!(term.df() as usize, expected.len());
                    prop_assert_eq!(collect(term.cursor().unwrap()).unwrap(), expected.keys().copied().collect::<Vec<_>>());
                    let payload = term.payload().unwrap();
                    let mut max_bucket = 0;
                    let mut cursor = term.cursor().unwrap();
                    for (tid, positions) in expected {
                        let ordinal = cursor.rank(*tid).unwrap().unwrap();
                        let entry = payload.get(ordinal).unwrap();
                        prop_assert_eq!(&entry.positions, positions);
                        let bucket = TfBucket::from_count(positions.len() as u32).value();
                        prop_assert_eq!(entry.tf_bucket, bucket);
                        max_bucket = max_bucket.max(bucket);
                    }
                    prop_assert_eq!(term.entry.max_tf_bucket, max_bucket);
                }
            }
        }
    }

    #[test]
    fn decoders_never_panic_on_arbitrary_bytes(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        let _ = Postings::parse(&bytes).and_then(|p| p.to_vec());
        let _ = Payload::parse(&bytes).and_then(|p| p.get(0));
        let _ = Dictionary::parse(&bytes).map(|d| d.iter().count());
        let _ = Dictionary::parse(&bytes).and_then(|d| d.get("a"));
        let _ = ForwardRecord::decode(&bytes);
        let _ = crate::segment::Segment::parse(&bytes).and_then(|s| s.term("a").map(|_| ()));
    }
}
