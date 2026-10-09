// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Benchmarks of the query engine's hot kernels, on synthetic blocks and,
//! with `STANNUM_BENCH_DUMP` naming a dump (see `script/dump-segments.py`),
//! on blocks of a real segment:
//!
//! ```text
//! cargo bench -p bench --bench kernels -- [FILTER] [--save FILE] [--baseline FILE]
//! script/bench-native [--save-baseline] [FILTER]
//! ```
//!
//! Each kernel runs in batches long enough to time (20 ms), and the median
//! of seven batches is reported per call. `--save` writes `name<TAB>ns`
//! lines; `--baseline` prints each kernel against such a file.

use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bench::dump::Dump;
use bench::paged::PagedSource;
use engine::bm25::{Bm25Params, TermScorer};
use engine::walk::kernels::{SUBS, Sieve, drop_dead};
use segment::bound::BlockBound;
use segment::dead::DeadDocs;
use segment::index::Index;
use segment::lanes::LaneSums;
use segment::ordinals::{self, Node, Ordinals, WORDS, Words};
use segment::segment::{Reader, Segment};
use segment::tf_bucket::{BUCKET_COUNT, TfBucket};

/// A small deterministic generator, so runs compare.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// `count` distinct ordinals below `limit`, ascending.
fn ordinals(rng: &mut Rng, limit: u32, count: usize) -> Vec<u32> {
    let mut set = std::collections::BTreeSet::new();
    while set.len() < count {
        set.insert(rng.below(u64::from(limit)) as u32);
    }
    set.into_iter().collect()
}

fn words(rng: &mut Rng, density: f64) -> Box<Words> {
    let mut out = Box::new([0u64; WORDS]);
    for word in out.iter_mut() {
        for bit in 0..64 {
            if (rng.below(1_000_000) as f64) < density * 1e6 {
                *word |= 1 << bit;
            }
        }
    }
    out
}

struct Harness {
    filter: Option<String>,
    save: Option<PathBuf>,
    baseline: Vec<(String, f64)>,
    results: Vec<(String, f64)>,
}

impl Harness {
    /// Times `f`, a call of the kernel, and reports nanoseconds per call.
    fn run(&mut self, name: &str, mut f: impl FnMut() -> u64) {
        if self
            .filter
            .as_ref()
            .is_some_and(|filter| !name.contains(filter.as_str()))
        {
            return;
        }
        let batch = |f: &mut dyn FnMut() -> u64, n: u64| {
            let started = Instant::now();
            for _ in 0..n {
                black_box(f());
            }
            started.elapsed()
        };
        let mut n = 1u64;
        while batch(&mut f, n) < Duration::from_millis(20) {
            n *= 2;
        }
        let mut samples: Vec<f64> = (0..7)
            .map(|_| batch(&mut f, n).as_nanos() as f64 / n as f64)
            .collect();
        samples.sort_by(f64::total_cmp);
        let ns = samples[3];
        let against = self
            .baseline
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, old)| format!("  {:>6.2}x baseline ({old:.1} ns)", ns / old))
            .unwrap_or_default();
        println!("{name:<40} {ns:>12.1} ns{against}");
        self.results.push((name.to_owned(), ns));
    }

    fn finish(&self) {
        if let Some(path) = &self.save {
            let text: String = self
                .results
                .iter()
                .map(|(name, ns)| format!("{name}\t{ns:.3}\n"))
                .collect();
            std::fs::write(path, text).expect("the results file");
        }
    }
}

fn synthetic(h: &mut Harness) {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    // Chunk forms: a bitmap at 30% and an array of 500 members.
    let dense = ordinals(&mut rng, ordinals::CHUNK, 20_000);
    let sparse = ordinals(&mut rng, ordinals::CHUNK, 500);
    let buckets = |n: usize, rng: &mut Rng| -> Vec<(u8, u32)> {
        (0..n)
            .map(|_| (rng.below(6) as u8, 20 + rng.below(400) as u32))
            .collect()
    };
    let dense_bytes = ordinals::encode_scored(&dense, &buckets(dense.len(), &mut rng));
    let sparse_bytes = ordinals::encode_scored(&sparse, &buckets(sparse.len(), &mut rng));
    let dense_stream =
        Ordinals::open(&dense_bytes[..], dense_bytes.len() as u64, true).expect("a stream");
    let sparse_stream =
        Ordinals::open(&sparse_bytes[..], sparse_bytes.len() as u64, true).expect("a stream");
    let mut out: Box<Words> = Box::new([0; WORDS]);
    let mut members = Vec::new();

    h.run("chunk.decode.bitmap", || {
        let chunk = dense_stream.chunk(0).expect("a chunk");
        chunk.words(&mut out);
        out[7]
    });
    h.run("chunk.decode.array", || {
        let chunk = sparse_stream.chunk(0).expect("a chunk");
        members.clear();
        chunk.members(&mut members);
        members.len() as u64
    });
    let bitmap = dense_stream.chunk(0).expect("a chunk");
    let array = sparse_stream.chunk(0).expect("a chunk");
    h.run("chunk.and_into.bitmap", || {
        out.fill(!0);
        bitmap.and_into(&mut out);
        out[3]
    });
    let mut array_members = Vec::new();
    array.members(&mut array_members);
    // As the walk scatters an array chunk's members for its bit tests.
    h.run("chunk.scatter.array", || {
        out.fill(0);
        for &low in &array_members {
            let low = usize::from(low);
            out[low / 64] |= 1 << (low % 64);
        }
        out[3]
    });
    let lows: Vec<u16> = (0..64).map(|_| rng.below(65_536) as u16).collect();
    h.run("chunk.rank.bitmap.64", || {
        lows.iter()
            .map(|low| u64::from(bitmap.rank(*low).unwrap_or(0)))
            .sum()
    });
    h.run("chunk.bucket.bitmap.64", || {
        (0..64u32)
            .map(|i| u64::from(bitmap.bucket(i * 300).unwrap_or(0)))
            .sum()
    });
    let a = words(&mut rng, 0.3);
    h.run("words.count", || u64::from(ordinals::count(&a)));

    // The count's fold over 16 chunks of two and three streams.
    let multi: Vec<Vec<u8>> = (0..3)
        .map(|_| ordinals::encode(&ordinals(&mut rng, 16 * ordinals::CHUNK, 200_000)))
        .collect();
    let streams: Vec<Option<Ordinals<'_>>> = multi
        .iter()
        .map(|bytes| Some(Ordinals::parse(bytes).expect("a stream")))
        .collect();
    let and2 = Node::And(vec![Node::Term(0), Node::Term(1)]);
    let or3 = Node::Or(vec![Node::Term(0), Node::Term(1), Node::Term(2)]);
    h.run("fold.and2.16chunks", || {
        let mut total = 0u64;
        ordinals::for_each_chunk(&and2, &streams, |_, _, n| {
            total += u64::from(n);
            Ok(())
        })
        .expect("a fold");
        total
    });
    h.run("fold.or3.16chunks", || {
        let mut total = 0u64;
        ordinals::for_each_chunk(&or3, &streams, |_, _, n| {
            total += u64::from(n);
            Ok(())
        })
        .expect("a fold");
        total
    });

    // The word sieve and its bit-sliced lanes, over a chunk's words.
    let term_words: Vec<Box<Words>> = (0..8)
        .map(|i| words(&mut rng, 0.02 * (i + 1) as f64))
        .collect();
    h.run("lanes.8terms.per_word", || {
        let mut kept = 0u64;
        for i in 0..64 {
            let mut lanes = LaneSums::new(5, LaneSums::MAX_TARGET);
            for (t, w) in term_words.iter().enumerate() {
                lanes.add(w[i], 4 + t as u32 * 3);
            }
            kept ^= lanes.reached();
        }
        kept
    });
    let subs: Vec<[f32; SUBS]> = (0..8).map(|t| [0.5 + t as f32 * 0.4; SUBS]).collect();
    let required = [false; 8];
    let mut sieve = Sieve::default();
    sieve.plan(4.0, &subs, 0, &required);
    let mut scratch = [0u64; 8];
    h.run("sieve.keep.8terms.per_word", || {
        let mut kept = 0u64;
        for i in 0..64 {
            for (slot, w) in scratch.iter_mut().zip(&term_words) {
                *slot = w[i];
            }
            let union = scratch.iter().fold(0, |a, b| a | b);
            kept ^= sieve.keep(union, &scratch);
        }
        kept
    });
    h.run("sieve.plan.8terms", || {
        sieve.plan(4.0, &subs, 3, &required);
        1
    });

    // Bounds and scores.
    let scorer = TermScorer::from_statistics(1_000_000, 20_000, 1.0, Bm25Params::default(), 70.0)
        .expect("a scorer");
    let mut min_len = [u32::MAX; BUCKET_COUNT];
    for (bucket, slot) in min_len.iter_mut().enumerate().take(8) {
        *slot = 10 + bucket as u32 * 7;
    }
    let block = BlockBound { min_len };
    h.run("bound.bounds_by_bucket", || {
        scorer.bounds_by_bucket(black_box(&block), black_box(30))[5].to_bits() as u64
    });
    h.run("bound.bound_through", || {
        scorer
            .bound_through(
                black_box(TfBucket::new(5).expect("a bucket")),
                black_box(40),
            )
            .to_bits() as u64
    });
    h.run("score.bucket.16", || {
        (0..16u32)
            .map(|i| {
                let bucket = TfBucket::new((black_box(i) % 8) as u8).expect("a bucket");
                scorer.score_bucket(bucket, 20 + i).to_bits() as u64
            })
            .sum()
    });

    // Dead documents.
    let dead_list = ordinals::encode(&ordinals(&mut rng, ordinals::CHUNK, 6_500));
    let dead = DeadDocs::decode(&dead_list, ordinals::CHUNK).expect("a dead list");
    h.run("dead.clear.chunk", || {
        out.fill(!0);
        dead.clear(0, &mut out);
        out[9]
    });
    let candidates: Vec<u16> = ordinals(&mut rng, ordinals::CHUNK, 500)
        .iter()
        .map(|o| *o as u16)
        .collect();
    let mut lows = Vec::new();
    h.run("dead.drop.sparse500", || {
        lows.clear();
        lows.extend_from_slice(&candidates);
        drop_dead(&dead, 0, true, &mut out, &mut lows);
        lows.len() as u64
    });
    h.run("dead.contains.64", || {
        (0..64u32)
            .filter(|i| dead.contains(i * 997 % ordinals::CHUNK))
            .count() as u64
    });

    // A two-word phrase's positions check.
    let query = tinql::runtime::parse_tinql_to_query_default("\"quick brown\"").expect("a phrase");
    let tinql::runtime::Query::Span { span_query, .. } = &query else {
        panic!("a phrase is a span");
    };
    let mut solver = boldi_vigna::SpanSolver::new(span_query).expect("a solver");
    let positions: Vec<Vec<u32>> = vec![
        (0..40).map(|i| i * 13).collect(),
        (0..40).map(|i| i * 17 + 5).collect(),
    ];
    h.run("phrase.solve.2x40", || {
        solver.intervals(&positions).count() as u64
    });
    let plan = boldi_vigna::PhrasePlan::new(span_query, |slot| slot as u64 + 1).expect("a plan");
    h.run("phrase.pairs.2x40", || {
        plan.steps()
            .iter()
            .filter_map(|step| step.pair)
            .filter(|pair| plan.pair_keeps(*pair, &positions))
            .count() as u64
    });
}

/// Kernels over blocks of a real segment: the dump's largest.
fn real(h: &mut Harness, dir: &std::path::Path) {
    let dump = Dump::open(dir).expect("the dump");
    let (n, dumped) = dump
        .segments
        .iter()
        .enumerate()
        .max_by_key(|(_, s)| s.documents)
        .expect("a segment");
    let segment = Segment::parse(&dumped.blob).expect("a segment");
    let documents = segment.document_count();
    // Terms by document frequency: the densest, one in twenty documents,
    // one in a thousand.
    let mut picks: Vec<(String, u32)> = Vec::new();
    let targets = [u32::MAX, documents / 20, documents / 1000];
    let mut best: Vec<Option<(String, u32)>> = vec![None; targets.len()];
    for item in segment.dictionary().expect("a dictionary").iter() {
        let (term, entry) = item.expect("an entry");
        for (slot, target) in best.iter_mut().zip(targets) {
            let distance = |df: u32| df.abs_diff(target.min(documents));
            if slot
                .as_ref()
                .is_none_or(|(_, df)| distance(entry.df) < distance(*df))
            {
                *slot = Some((term.clone(), entry.df));
            }
        }
    }
    picks.extend(best.into_iter().flatten());
    eprintln!("real segment: {documents} documents; terms {picks:?}");
    let mut rng = Rng(7);
    let mut out: Box<Words> = Box::new([0; WORDS]);
    let mut members = Vec::new();
    for (label, (term, _)) in ["dense", "df.5pct", "df.0.1pct"].iter().zip(&picks) {
        let found = Index::term(&segment, term)
            .expect("a lookup")
            .expect("a term");
        let stream = found.ordinals().expect("a stream");
        if stream.list().is_some() {
            continue;
        }
        let chunks = stream.chunk_count();
        h.run(&format!("real.{label}.chunks.decode"), || {
            let mut sum = 0u64;
            for c in 0..chunks {
                let chunk = stream.chunk(c).expect("a chunk");
                if chunk.is_bitmap() {
                    chunk.words(&mut out);
                    sum ^= out[1];
                } else {
                    members.clear();
                    chunk.members(&mut members);
                    sum += members.len() as u64;
                }
            }
            sum
        });
        h.run(&format!("real.{label}.bounds.parse"), || {
            let reparsed = found.ordinals().expect("a stream");
            reparsed.bounds().expect("bounds").len() as u64
        });
    }
    // Two common terms' conjunction and disjunction counted by the fold.
    if picks.len() >= 2 {
        let a = Index::term(&segment, &picks[0].0)
            .expect("a lookup")
            .expect("a term");
        let b = Index::term(&segment, &picks[1].0)
            .expect("a lookup")
            .expect("a term");
        let streams = vec![
            Some(a.ordinals().expect("a stream")),
            Some(b.ordinals().expect("a stream")),
        ];
        for (name, node) in [
            (
                "real.fold.and2",
                Node::And(vec![Node::Term(0), Node::Term(1)]),
            ),
            (
                "real.fold.or2",
                Node::Or(vec![Node::Term(0), Node::Term(1)]),
            ),
        ] {
            h.run(name, || {
                let mut total = 0u64;
                ordinals::for_each_chunk(&node, &streams, |_, _, n| {
                    total += u64::from(n);
                    Ok(())
                })
                .expect("a fold");
                total
            });
        }
    }
    // Length and class lookups as a walk makes them: through a paged reader
    // holding the table's pages, at scattered and at ascending ordinals.
    let paged = Reader::new(PagedSource::new(
        n as u32,
        dumped.blob.clone(),
        dumped.areas.clone(),
    ))
    .expect("a reader");
    paged.hold(true);
    let lengths = paged.lengths();
    let random: Vec<u32> = (0..256)
        .map(|_| rng.below(u64::from(documents)) as u32)
        .collect();
    let mut ascending = random.clone();
    ascending.sort_unstable();
    h.run("real.lengths.random.256", || {
        random
            .iter()
            .map(|o| u64::from(lengths.get(*o).expect("a length")))
            .sum()
    });
    h.run("real.lengths.ascending.256", || {
        ascending
            .iter()
            .map(|o| u64::from(lengths.get(*o).expect("a length")))
            .sum()
    });
    h.run("real.classes.random.256", || {
        random
            .iter()
            .map(|o| u64::from(paged.length_class(*o).expect("a class")))
            .sum()
    });
    h.run("real.classes.ascending.256", || {
        ascending
            .iter()
            .map(|o| u64::from(paged.length_class(*o).expect("a class")))
            .sum()
    });
    paged.hold(false);
    // Positions of the densest term's members, read as a phrase check does.
    if let Some((term, df)) = picks.first() {
        let found = Index::term(&segment, term)
            .expect("a lookup")
            .expect("a term");
        let payload = found.payload().expect("a payload");
        let ranks: Vec<u32> = {
            let mut ranks: Vec<u32> = (0..64).map(|_| rng.below(u64::from(*df)) as u32).collect();
            ranks.sort_unstable();
            ranks
        };
        let mut positions = Vec::new();
        h.run("real.positions.seek_read.64", || {
            let mut cursor = payload.cursor();
            let mut total = 0u64;
            for rank in &ranks {
                cursor.seek(*rank).expect("a seek");
                positions.clear();
                cursor.next_into(&mut positions).expect("positions");
                total += positions.len() as u64;
            }
            total
        });
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut h = Harness {
        filter: None,
        save: None,
        baseline: Vec::new(),
        results: Vec::new(),
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            // cargo bench passes it to every benchmark binary.
            "--bench" => {}
            "--save" => h.save = args.next().map(PathBuf::from),
            "--baseline" => {
                let path = args.next().expect("--baseline FILE");
                if let Ok(text) = std::fs::read_to_string(&path) {
                    h.baseline = text
                        .lines()
                        .filter_map(|line| {
                            let (name, ns) = line.split_once('\t')?;
                            Some((name.to_owned(), ns.parse().ok()?))
                        })
                        .collect();
                } else {
                    eprintln!("no baseline at {path} yet");
                }
            }
            filter => h.filter = Some(filter.to_owned()),
        }
    }
    synthetic(&mut h);
    if let Some(dir) = std::env::var_os("STANNUM_BENCH_DUMP") {
        real(&mut h, std::path::Path::new(&dir));
    }
    h.finish();
}
