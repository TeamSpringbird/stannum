// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! The ctid-addressed segment against the ordinal one built from the same
//! documents: everything the query interface hands out must be the same,
//! since the planner, the cursors, the ordinal walk and the count fold see
//! nothing else. Merges against a build of the merged documents.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::index::Reader as TnsReader;
use super::merge::{Input, merge};
use super::postings::{Form, GroupFrontiers, Options, group_frontier, rounded_length};
use super::verify::verify_segment;
use crate::dead::DeadDocs;
use crate::index::{Expanded, Index, Window};
use crate::segment::{Segment, SegmentBuilder};
use crate::set::{Cursor, collect};
use crate::source::Source;
use crate::tid::MAX_OFFSET;
use crate::{Result, Tid};

/// A source that only copies, as the extension's page-backed one does.
struct Paged(Vec<u8>);

impl Source for Paged {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let at = offset as usize;
        self.0
            .get(at..at + len)
            .map(<[u8]>::to_vec)
            .ok_or(crate::Error::Truncated)
    }
}

type Doc = (Tid, Vec<(String, u32)>);

/// Documents with clustered ctids over several 256-page groups and terms
/// of every frequency: `w` in most, `k<n>` in few, `t<i>` in between.
fn documents(max: usize) -> impl Strategy<Value = Vec<Doc>> {
    prop::collection::btree_map(
        (0u32..1400, 1u16..=MAX_OFFSET).prop_map(|(raw, offset)| {
            let block = if raw % 3 == 0 { raw / 64 } else { raw };
            Tid::new(block, if block < 4 { offset } else { 1 + offset % 30 }).unwrap()
        }),
        (0u32..1000, 1usize..12),
        1..max,
    )
    .prop_map(|docs: BTreeMap<Tid, (u32, usize)>| {
        docs.into_iter()
            .map(|(tid, (seed, len))| {
                let tokens = (0..len)
                    .map(|p| {
                        let name = match (seed + p as u32 * 7) % 9 {
                            0..=2 => "w".to_owned(),
                            3 => format!("k{}", seed % 40),
                            n => format!("t{}", (seed + n) % 6),
                        };
                        (name, p as u32)
                    })
                    .collect();
                (tid, tokens)
            })
            .collect()
    })
}

fn builder(docs: &[Doc]) -> SegmentBuilder {
    let mut builder = SegmentBuilder::default();
    for (tid, tokens) in docs {
        builder
            .add_document(*tid, tokens.iter().map(|(t, p)| (t.as_str(), *p)))
            .unwrap();
    }
    builder
}

fn tns(docs: &[Doc], options: Options) -> Vec<u8> {
    builder(docs).finish_tns(options).unwrap().unwrap().blob
}

fn options() -> impl Strategy<Value = Options> {
    (prop::bool::ANY, 1u32..6).prop_map(|(small, density)| Options {
        block_size: if small { 3 } else { 256 },
        grid_density: if small { density } else { 64 },
        ..Options::default()
    })
}

/// Everything the query interface hands out, read through `index`.
fn observe(index: &dyn Index) -> String {
    let mut out = format!(
        "documents {} total {}\n",
        index.document_count(),
        index.total_length()
    );
    let tids = collect(index.documents().unwrap()).unwrap();
    out += &format!("tids {tids:?}\n");
    let table = index.doc_table().unwrap();
    for (ordinal, tid) in tids.iter().enumerate() {
        assert_eq!(table.tid_at(ordinal as u32).unwrap(), *tid);
        assert_eq!(table.ordinal_of(*tid).unwrap(), Some(ordinal as u32));
        let length = index.lengths().get(ordinal as u32).unwrap();
        let class = index.length_class(ordinal as u32).unwrap();
        out += &format!("{ordinal}: {length} {class}\n");
    }
    out += &format!(
        "pages {:?}\n",
        index.page_table().unwrap().entries().collect::<Vec<_>>()
    );
    let Expanded::Terms(terms) = index.expand(Window::All, &|_| true, usize::MAX).unwrap() else {
        panic!("no overflow without a limit");
    };
    for (name, term) in terms {
        let found = index.term(&name).unwrap().expect("an expanded term");
        assert_eq!(found.entry.df, term.entry.df);
        let mut cursor = term.cursor().unwrap();
        let mut postings = Vec::new();
        while let Some(tid) = cursor.current() {
            postings.push((tid, cursor.ordinal(), cursor.bucket()));
            cursor.advance().unwrap();
        }
        let ordinals = term.ordinals().unwrap().to_vec().unwrap();
        let payload = term.payload().unwrap();
        let positions: Vec<_> = (0..term.df())
            .map(|i| payload.get(i).unwrap().positions)
            .collect();
        out += &format!(
            "{name} df {} max {} {postings:?} {ordinals:?} {positions:?}\n",
            term.entry.df, term.entry.max_tf_bucket
        );
    }
    for window in [Window::Prefix("t"), Window::Range(Some("k1"), Some("t3"))] {
        if let Expanded::Terms(terms) = index.expand(window, &|t| t.len() < 3, 1000).unwrap() {
            out += &format!(
                "{:?}\n",
                terms.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>()
            );
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn the_ctid_reader_hands_out_what_the_ordinal_reader_does(
        docs in documents(400),
        options in options(),
    ) {
        let ordinal = builder(&docs).finish();
        let blob = tns(&docs, options);
        let report = verify_segment(&blob);
        prop_assert!(report.is_clean(), "{:?}", report.findings);
        let want = observe(&Segment::parse(&ordinal).unwrap());
        let paged = TnsReader::new(Paged(blob.clone())).unwrap();
        prop_assert_eq!(&observe(&paged), &want);
        let whole = TnsReader::new(blob.as_slice()).unwrap();
        prop_assert_eq!(&observe(&whole), &want);
    }

    #[test]
    fn a_merge_is_a_build_of_the_live_documents(
        docs in documents(500),
        parts in 1usize..5,
        dead in prop::collection::btree_set(0usize..500, 0..120),
        options in options(),
        ranges in any::<bool>(),
    ) {
        // The k-th document goes to input k % parts: inputs interleave by
        // ctid, as rows updated into pages other segments cover do. Or each
        // input takes a run of ctids, as segments of appended rows do, and
        // most groups have one input, whose containers the merge keeps.
        let mut inputs: Vec<Vec<Doc>> = vec![Vec::new(); parts];
        let run = docs.len().div_ceil(parts).max(1);
        for (i, doc) in docs.iter().enumerate() {
            let input = if ranges { i / run } else { i % parts };
            inputs[input].push(doc.clone());
        }
        inputs.retain(|input| !input.is_empty());
        let blobs: Vec<Vec<u8>> = inputs.iter().map(|input| tns(input, options)).collect();
        let mut live: Vec<Doc> = Vec::new();
        let mut deads = Vec::new();
        let mut global = 0usize;
        for input in &inputs {
            let mut ranks = Vec::new();
            for (rank, doc) in input.iter().enumerate() {
                if dead.contains(&global) {
                    ranks.push(rank as u32);
                } else {
                    live.push(doc.clone());
                }
                global += 1;
            }
            let list = crate::ordinals::encode(&ranks);
            deads.push(DeadDocs::decode(&list, input.len() as u32).unwrap());
        }
        let merged = merge(
            &blobs
                .iter()
                .zip(&deads)
                .map(|(bytes, dead)| Input { bytes, dead })
                .collect::<Vec<_>>(),
            options,
            || Ok(()),
        )
        .unwrap();
        live.sort_by_key(|d| d.0);
        match merged {
            None => prop_assert!(live.is_empty()),
            Some(merged) => {
                prop_assert!(verify_segment(&merged.blob).is_clean());
                prop_assert_eq!(merged.documents as usize, live.len());
                prop_assert_eq!(&merged.blob, &tns(&live, options));
            }
        }
    }
}

/// Per (term, heap group number), per bucket, the shortest length of the
/// term's live postings there, from a segment's postings, buckets and
/// lengths (not its frontiers).
fn group_minima(blob: &[u8], dead: &DeadDocs) -> BTreeMap<(String, u32), [u32; 16]> {
    let segment = super::segment::Segment::parse(blob).unwrap();
    let geometry = &segment.docs.geometry;
    let mut out = BTreeMap::new();
    for item in segment.dictionary().iter() {
        let (term, entry) = item.unwrap();
        let found = segment.resolve(entry).unwrap();
        let footer = found
            .postings
            .footer(segment.block_size, entry.max_tf_bucket, segment.adaptive_tf)
            .unwrap();
        for (i, slot) in found.postings.slots(geometry).unwrap().iter().enumerate() {
            let rank = segment.docs.rank(*slot).unwrap();
            if dead.contains(rank) {
                continue;
            }
            let group = geometry.groups[geometry.group_of_slot(*slot)].id;
            let bucket = footer.bucket(found.postings.tf, i as u32).unwrap();
            let length = segment.lengths.get(rank).unwrap();
            let least: &mut [u32; 16] = out.entry((term.clone(), group)).or_insert([u32::MAX; 16]);
            least[usize::from(bucket)] = least[usize::from(bucket)].min(length);
        }
    }
    out
}

/// Per (term, heap group number), the frontier a segment's group
/// directories hold, for its grouped terms.
fn stored_frontiers(blob: &[u8]) -> BTreeMap<(String, u32), Vec<(u8, u32)>> {
    let segment = super::segment::Segment::parse(blob).unwrap();
    let geometry = &segment.docs.geometry;
    let mut out = BTreeMap::new();
    for item in segment.dictionary().iter() {
        let (term, entry) = item.unwrap();
        let found = segment.resolve(entry).unwrap();
        if let Form::Grouped(entries) = &found.postings.form {
            for e in entries.iter() {
                let front = found
                    .postings
                    .group_frontier(e)
                    .expect("frontiers")
                    .collect();
                out.insert((term.clone(), geometry.groups[e.index as usize].id), front);
            }
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn a_merge_keeps_the_least_length_per_bucket_of_its_inputs(
        docs in documents(500),
        parts in 2usize..5,
        dead in prop::collection::btree_set(0usize..500, 0..80),
        any_dead in any::<bool>(),
        rounded in any::<bool>(),
        small in any::<bool>(),
    ) {
        let dead = if any_dead { dead } else { Default::default() };
        // Inputs interleaved by ctid share groups: a group's frontier in
        // the merge comes from several inputs' postings, less the dead.
        let options = Options {
            block_size: if small { 3 } else { 256 },
            grid_min_postings: 0,
            sparse: false,
            group_frontiers: if rounded {
                GroupFrontiers::Rounded
            } else {
                GroupFrontiers::Exact
            },
            ..Options::default()
        };
        let mut inputs: Vec<Vec<Doc>> = vec![Vec::new(); parts];
        for (i, doc) in docs.iter().enumerate() {
            inputs[i % parts].push(doc.clone());
        }
        inputs.retain(|input| !input.is_empty());
        let blobs: Vec<Vec<u8>> = inputs.iter().map(|input| tns(input, options)).collect();
        let mut deads = Vec::new();
        let mut global = 0usize;
        for input in &inputs {
            let ranks: Vec<u32> = (0..input.len())
                .filter(|r| dead.contains(&(global + r)))
                .map(|r| r as u32)
                .collect();
            global += input.len();
            deads.push(DeadDocs::decode(&crate::ordinals::encode(&ranks), input.len() as u32).unwrap());
        }
        // The least length per bucket over the inputs' live postings.
        let mut want: BTreeMap<(String, u32), [u32; 16]> = BTreeMap::new();
        for (blob, dead) in blobs.iter().zip(&deads) {
            for (key, least) in group_minima(blob, dead) {
                let into = want.entry(key).or_insert([u32::MAX; 16]);
                for b in 0..16 {
                    into[b] = into[b].min(least[b]);
                }
            }
        }
        let merged = merge(
            &blobs
                .iter()
                .zip(&deads)
                .map(|(bytes, dead)| Input { bytes, dead })
                .collect::<Vec<_>>(),
            options,
            || Ok(()),
        )
        .unwrap();
        let Some(merged) = merged else {
            prop_assert!(want.is_empty());
            return Ok(());
        };
        // A term of one posting has no directory.
        let stored = stored_frontiers(&merged.blob);
        let grouped: std::collections::BTreeSet<&String> = stored.keys().map(|k| &k.0).collect();
        want.retain(|key, _| grouped.contains(&key.0));
        prop_assert_eq!(stored.len(), want.len());
        for (key, least) in &want {
            let pairs = least
                .iter()
                .enumerate()
                .filter(|(_, l)| **l != u32::MAX)
                .map(|(b, l)| (b as u8, *l));
            let expected: Vec<(u8, u32)> = group_frontier(pairs, options.group_frontiers)
                .into_iter()
                .map(|(b, v)| (b, if rounded { rounded_length(v) } else { v }))
                .collect();
            prop_assert_eq!(stored.get(key), Some(&expected), "{:?}", key);
        }
        // With nothing dead, that is the least per bucket over the inputs'
        // stored frontiers.
        if dead.is_empty() {
            let mut union: BTreeMap<(String, u32), Vec<(u8, u32)>> = BTreeMap::new();
            for (blob, none) in blobs.iter().zip(&deads) {
                let fronts = stored_frontiers(blob);
                // An input's term of one posting holds no directory: its
                // posting stands for its frontier.
                for (key, least) in group_minima(blob, none) {
                    if !grouped.contains(&key.0) {
                        continue;
                    }
                    let into = union.entry(key.clone()).or_default();
                    match fronts.get(&key) {
                        Some(front) => into.extend(front),
                        None => into.extend(
                            least
                                .iter()
                                .enumerate()
                                .filter(|(_, l)| **l != u32::MAX)
                                .map(|(b, l)| (b as u8, *l)),
                        ),
                    }
                }
            }
            for (key, pairs) in union {
                // Stored lengths are rounded already: rounding again keeps them.
                let expected: Vec<(u8, u32)> =
                    group_frontier(pairs.into_iter(), options.group_frontiers)
                        .into_iter()
                        .map(|(b, v)| (b, if rounded { rounded_length(v) } else { v }))
                        .collect();
                prop_assert_eq!(stored.get(&key), Some(&expected), "{:?}", key);
            }
        }
    }
}

#[test]
fn a_merge_keeps_the_containers_of_groups_one_input_holds() {
    // Two inputs over two 256-page groups each, every page full to the same
    // offset, so each group's geometry survives the merge; a dense word and
    // a common one make grids and lists. One dead document leaves its group
    // to be re-encoded.
    let docs = |groups: std::ops::Range<u32>| -> Vec<Doc> {
        let mut out = Vec::new();
        for group in groups {
            for page in 0..40 {
                for offset in 1..=6u16 {
                    let n = group * 1000 + page * 6 + u32::from(offset);
                    let mut tokens = vec![("w".to_owned(), 0)];
                    if n % 3 == 0 {
                        tokens.push(("c".to_owned(), 1));
                    }
                    if n % 97 == 0 {
                        tokens.push(("r".to_owned(), 2));
                    }
                    out.push((Tid::new(group * 256 + page, offset).unwrap(), tokens));
                }
            }
        }
        out
    };
    let options = Options {
        block_size: 16,
        grid_min_postings: 0,
        ..Options::default()
    };
    let (a, b) = (docs(0..2), docs(2..4));
    let blobs = [tns(&a, options), tns(&b, options)];
    let none = DeadDocs::decode(&crate::ordinals::encode(&[]), a.len() as u32).unwrap();
    let one = DeadDocs::decode(&crate::ordinals::encode(&[7]), b.len() as u32).unwrap();
    let merged = merge(
        &[
            Input {
                bytes: &blobs[0],
                dead: &none,
            },
            Input {
                bytes: &blobs[1],
                dead: &one,
            },
        ],
        options,
        || Ok(()),
    )
    .unwrap()
    .unwrap();
    let mut live = a.clone();
    live.extend(
        b.iter()
            .enumerate()
            .filter(|(i, _)| *i != 7)
            .map(|(_, d)| d.clone()),
    );
    assert!(merged.reused >= 6, "{}", merged.reused);
    assert!(verify_segment(&merged.blob).is_clean());
    assert_eq!(merged.blob, tns(&live, options));
}

#[test]
fn a_corrupted_length_is_found() {
    let docs: Vec<Doc> = (1..=5)
        .map(|o| {
            (
                Tid::new(0, o).unwrap(),
                vec![("w".to_owned(), 0), ("x".to_owned(), 1)],
            )
        })
        .collect();
    let blob = tns(&docs, Options::default());
    assert!(verify_segment(&blob).is_clean());
    let segment = super::segment::Segment::parse(&blob).unwrap();
    let mut broken = blob.clone();
    broken[segment.length_at(2).0] = 3;
    assert!(!verify_segment(&broken).is_clean());
}

#[test]
fn a_corrupted_frontier_is_found() {
    // A word in every document of two groups: grouped, two directory
    // entries, each with its frontier.
    let docs: Vec<Doc> = (0..2u32)
        .flat_map(|g| {
            (1..=5u16).map(move |o| {
                (
                    Tid::new(g * 256, o).unwrap(),
                    (0..u32::from(o)).map(|p| ("w".to_owned(), p)).collect(),
                )
            })
        })
        .collect();
    let options = Options {
        sparse: false,
        ..Options::default()
    };
    let blob = tns(&docs, options);
    assert!(verify_segment(&blob).is_clean());
    let segment = super::segment::Segment::parse(&blob).unwrap();
    let term = segment.term("w").unwrap().unwrap();
    let Form::Grouped(entries) = &term.postings.form else {
        panic!("a grouped term");
    };
    // The first frontier's first length (after its head byte): the group's
    // shortest document, 1 token, made 2.
    let at = term.at + term.postings.payload_at + entries[0].frontier as usize + 1;
    assert_eq!(blob[at], 1);
    let mut broken = blob.clone();
    broken[at] = 2;
    let report = verify_segment(&broken);
    assert!(!report.is_clean());
    assert!(format!("{:?}", report.findings).contains("frontier"));
}
