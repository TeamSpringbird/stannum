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
use super::postings::Options;
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
    ctids(max)
        .prop_flat_map(|tids| {
            let n = tids.len();
            (
                Just(tids),
                prop::collection::vec((0u32..1000, 1usize..12), n),
            )
        })
        .prop_map(|(tids, seeds)| tids.into_iter().zip(seeds).collect::<BTreeMap<_, _>>())
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

/// Fewer than `max` ctids, ascending: random line pointers (pages with
/// holes, wide at the first blocks), or heap pages each holding rows
/// `1..=n` for its own `n`, some with a hole, as a heap only inserted into
/// is laid out (each page's width its row count). Blocks cluster into few
/// 256-page groups either way.
pub(crate) fn ctids(max: usize) -> impl Strategy<Value = Vec<Tid>> {
    let block = |raw: u32| if raw.is_multiple_of(3) { raw / 64 } else { raw };
    let random = prop::collection::btree_set(
        (0u32..1400, 1u16..=MAX_OFFSET).prop_map(move |(raw, offset)| {
            let block = block(raw);
            Tid::new(block, if block < 4 { offset } else { 1 + offset % 30 }).unwrap()
        }),
        1..max,
    )
    .prop_map(|set| set.into_iter().collect::<Vec<_>>());
    let pages = (max / 8).max(2);
    let heap = prop::collection::btree_map(0u32..1400, (1u16..=40, any::<u8>()), 1..pages)
        .prop_map(move |pages| {
            let mut tids: Vec<Tid> = Vec::new();
            for (raw, (rows, shape)) in pages {
                let block = block(raw);
                let rows = if shape % 13 == 0 {
                    MAX_OFFSET - u16::from(shape % 7)
                } else {
                    rows
                };
                let hole = (shape % 4 == 0).then_some(u16::from(shape) % rows + 1);
                tids.extend(
                    (1..=rows)
                        .filter(|o| Some(*o) != hole)
                        .map(|o| Tid::new(block, o).unwrap()),
                );
            }
            tids.sort_unstable();
            tids.dedup();
            tids.truncate(max - 1);
            if tids.is_empty() {
                tids.push(Tid::new(0, 1).unwrap());
            }
            tids
        });
    prop_oneof![random, heap]
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
    (
        prop::bool::ANY,
        1u32..6,
        prop::sample::select(vec![0u32, 2, 64]),
    )
        .prop_map(|(small, density, array_max)| Options {
            block_size: if small { 3 } else { 256 },
            grid_density: if small { density } else { 64 },
            array_max,
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
        input_arrays in prop::sample::select(vec![0u32, 64]),
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
        // Inputs written with arrays for other lists than the output's are
        // re-encoded where those differ, not copied.
        let written = Options {
            array_max: input_arrays,
            ..options
        };
        let blobs: Vec<Vec<u8>> = inputs.iter().map(|input| tns(input, written)).collect();
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
fn a_merge_renumbers_groups_whose_pages_change_width() {
    // Pages of 1 to 9 rows. In groups 0 and 1 input `a` holds the even
    // pages and the first rows of the odd ones, `b` the odd pages' last
    // rows, so each input's odd pages are narrower than the merge's (`a`)
    // or start later (`b`); some of `b`'s last rows are dead, so those
    // pages are narrower in the merge than in `b`. Group 2 is `a`'s alone,
    // its widths unchanged: its containers are kept.
    let doc = |block: u32, offset: u16| -> Doc {
        let n = block * 16 + u32::from(offset);
        let mut tokens = vec![("w".to_owned(), 0)];
        if n.is_multiple_of(3) {
            tokens.push(("c".to_owned(), 1));
        }
        if n.is_multiple_of(29) {
            tokens.push(("r".to_owned(), 2));
        }
        (Tid::new(block, offset).unwrap(), tokens)
    };
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for group in 0..3u32 {
        for page in 0..60u32 {
            let rows = 1 + (page % 9) as u16;
            for offset in 1..=rows {
                let d = doc(group * 256 + page, offset);
                if group == 2 || page % 2 == 0 || offset <= rows / 2 {
                    a.push(d);
                } else {
                    b.push(d);
                }
            }
        }
    }
    a.sort_by_key(|d| d.0);
    let options = Options {
        block_size: 16,
        grid_min_postings: 0,
        ..Options::default()
    };
    let blobs = [tns(&a, options), tns(&b, options)];
    // b's dead: every third page's last row.
    let dead: Vec<u32> = b
        .iter()
        .enumerate()
        .filter(|(i, d)| {
            let next = b.get(i + 1).map(|n: &Doc| n.0.block);
            next != Some(d.0.block) && d.0.block % 3 == 0
        })
        .map(|(i, _)| i as u32)
        .collect();
    assert!(!dead.is_empty());
    let none = DeadDocs::decode(&crate::ordinals::encode(&[]), a.len() as u32).unwrap();
    let some = DeadDocs::decode(&crate::ordinals::encode(&dead), b.len() as u32).unwrap();
    let merged = merge(
        &[
            Input {
                bytes: &blobs[0],
                dead: &none,
            },
            Input {
                bytes: &blobs[1],
                dead: &some,
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
            .filter(|(i, _)| !dead.contains(&(*i as u32)))
            .map(|(_, d)| d.clone()),
    );
    live.sort_by_key(|d| d.0);
    assert!(merged.reused >= 2, "{}", merged.reused);
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
