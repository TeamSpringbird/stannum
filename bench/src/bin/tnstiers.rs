// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A census of impact tiers per (term, group) over dumped `TNS1` segments:
//! how many grouped entries and postings fall in each count range, and,
//! for the terms a trace's disjunctions name (weighted by how often), how
//! far the best posting outside a group's top fraction by impact falls
//! below the group's best, at BM25's default parameters and each segment's
//! average length.
//!
//! ```text
//! cargo run -p bench --release --bin tnstiers -- --dump DIR --trace FILE [--jobs N]
//! ```

use std::path::PathBuf;
use std::sync::Mutex;

use engine::bm25::{Bm25Params, TermScorer};
use rustc_hash::FxHashMap;
use segment::tf_bucket::TfBucket;
use segment::tinshape::postings::{Form, for_each_local};
use segment::tinshape::segment::Segment;
use tinql::runtime::eval::tokenize_doc;

/// Count ranges: an entry of `c` postings is in the first range whose
/// bound exceeds `c`.
const BINS: [u32; 10] = [16, 32, 64, 128, 256, 512, 1024, 2048, 4096, u32::MAX];
/// Top fractions tried for the high tier.
const FRACTIONS: [f64; 3] = [0.05, 0.10, 0.20];

#[derive(Default, Clone)]
struct Bin {
    entries: u64,
    postings: u64,
    /// Over the trace's terms, weighted by their uses: entries, postings,
    /// and per fraction the sum of lo/hi ratios and entries at ratio <=
    /// 0.8 and <= 0.9.
    w_entries: [f64; 2],
    w_postings: [f64; 2],
    ratio: [[f64; 3]; 2],
    under80: [[f64; 3]; 2],
    under90: [[f64; 3]; 2],
}

fn bin_of(count: u32) -> usize {
    BINS.iter().position(|b| count < *b).expect("a bin")
}

fn main() {
    let mut dump = None;
    let mut trace = None;
    let mut jobs = 4usize;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dump" => dump = it.next().map(PathBuf::from),
            "--trace" => trace = it.next().map(PathBuf::from),
            "--jobs" => jobs = it.next().and_then(|v| v.parse().ok()).unwrap_or(4),
            _ => {
                eprintln!("usage: tnstiers --dump DIR --trace FILE [--jobs N]");
                std::process::exit(2)
            }
        }
    }
    let dump = dump.expect("--dump");
    let manifest = std::fs::read_to_string(dump.join("manifest.tsv")).expect("manifest");
    let mut spec = None;
    let mut names = Vec::new();
    for line in manifest.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["spec", hex] => {
                let bytes: Vec<u8> = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
                    .collect();
                spec = Some(bytes);
            }
            ["segment", _, _, blob, _] => names.push((*blob).to_owned()),
            _ => {}
        }
    }
    let spec: [u8; engine::spec::SPEC_BYTES] = spec.expect("spec").try_into().expect("spec");
    let tokenizer = engine::spec::decode_spec(&spec)
        .expect("spec")
        .compile()
        .expect("tokenizer");
    // Each disjunction term's uses.
    let mut uses: FxHashMap<String, f64> = FxHashMap::default();
    for q in bench::read_trace(&trace.expect("--trace")).expect("trace") {
        if q.style != "disjunction" {
            continue;
        }
        for part in q.text.split(" OR ") {
            for t in tokenize_doc(part, &tokenizer).tokens() {
                *uses.entry(t.clone()).or_default() += 1.0;
            }
        }
    }
    eprintln!("{} distinct disjunction terms", uses.len());
    let queue = Mutex::new(names.into_iter());
    let total: Mutex<Vec<Bin>> = Mutex::new(vec![Bin::default(); BINS.len()]);
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let Some(name) = queue.lock().expect("queue").next() else {
                        return;
                    };
                    let blob = std::fs::read(dump.join(&name)).expect("segment");
                    let bins = census(&blob, &uses);
                    eprintln!("{name} done");
                    let mut t = total.lock().expect("total");
                    for (a, b) in t.iter_mut().zip(&bins) {
                        a.entries += b.entries;
                        a.postings += b.postings;
                        for m in 0..2 {
                            a.w_entries[m] += b.w_entries[m];
                            a.w_postings[m] += b.w_postings[m];
                            for k in 0..3 {
                                a.ratio[m][k] += b.ratio[m][k];
                                a.under80[m][k] += b.under80[m][k];
                                a.under90[m][k] += b.under90[m][k];
                            }
                        }
                    }
                }
            });
        }
    });
    let t = total.into_inner().expect("total");
    let all_p: u64 = t.iter().map(|b| b.postings).sum();
    println!(
        "bin\tentries\tpostings\tp%\tbits_MB\tmode\tw_entries\tw_postings\tr5\tr10\tr20\tu80_5\tu80_10\tu80_20\tu90_10"
    );
    let mut lo = 0;
    for (i, b) in t.iter().enumerate() {
        for (m, mode) in ["score", "full"].iter().enumerate() {
            let we = b.w_entries[m].max(1e-9);
            println!(
                "{lo}..{}\t{}\t{}\t{:.1}\t{:.0}\t{mode}\t{:.0}\t{:.0}\t{:.3}\t{:.3}\t{:.3}\t{:.2}\t{:.2}\t{:.2}\t{:.2}",
                BINS[i],
                b.entries,
                b.postings,
                100.0 * b.postings as f64 / all_p as f64,
                (b.postings as f64 / 8.0 + 3.0 * b.entries as f64) / 1e6,
                b.w_entries[m],
                b.w_postings[m],
                b.ratio[m][0] / we,
                b.ratio[m][1] / we,
                b.ratio[m][2] / we,
                b.under80[m][0] / we,
                b.under80[m][1] / we,
                b.under80[m][2] / we,
                b.under90[m][1] / we,
            );
        }
        lo = BINS[i];
    }
}

fn census(blob: &[u8], uses: &FxHashMap<String, f64>) -> Vec<Bin> {
    let segment = Segment::parse(blob).expect("segment");
    let geometry = &segment.docs.geometry;
    let groups = &geometry.groups;
    let avgdl = segment.total_length as f32 / segment.documents as f32;
    let scorer = TermScorer::new(1.0, 1.0, Bm25Params::default_bm25(), avgdl).expect("scorer");
    let mut bins = vec![Bin::default(); BINS.len()];
    let mut impacts = Vec::new();
    let mut locals = Vec::new();
    for item in segment.dictionary().iter() {
        let (term, entry) = item.expect("dictionary");
        let Ok(found) = segment.resolve(entry) else {
            continue;
        };
        let Form::Grouped(dir) = &found.postings.form else {
            continue;
        };
        let entries = dir.entries(groups).expect("entries");
        let used = uses.get(term.as_str()).copied();
        // Scored under `score` only below a tenth of the documents.
        let score_mode = u64::from(entry.df) * 10 < u64::from(segment.documents);
        for e in &entries {
            let b = &mut bins[bin_of(e.count)];
            b.entries += 1;
            b.postings += u64::from(e.count);
            let Some(w) = used else { continue };
            let p = &found.postings;
            let bytes = p.container(e).all().expect("container");
            locals.clear();
            for_each_local(e, bytes, geometry.starts(e.index as usize), |l| {
                locals.push(l)
            })
            .expect("members");
            impacts.clear();
            for (i, &l) in locals.iter().enumerate() {
                let bucket = p.bucket_in(e, e.first + i as u32).expect("bucket");
                let rank = segment.docs.rank_in(e.index as usize, l).expect("rank");
                let length = segment.lengths.get(rank).expect("length");
                impacts.push(scorer.bound_through(TfBucket::new(bucket).expect("b"), length));
            }
            impacts.sort_unstable_by(|a, b| b.total_cmp(a));
            let hb = impacts[0];
            for m in 0..2 {
                if m == 0 && !score_mode {
                    continue;
                }
                b.w_entries[m] += w;
                b.w_postings[m] += w * f64::from(e.count);
                for (k, f) in FRACTIONS.iter().enumerate() {
                    let h = (f64::from(e.count) * f).ceil() as usize;
                    let r = if h >= impacts.len() {
                        1.0
                    } else {
                        f64::from(impacts[h] / hb)
                    };
                    b.ratio[m][k] += w * r;
                    b.under80[m][k] += w * f64::from(u8::from(r <= 0.8));
                    b.under90[m][k] += w * f64::from(u8::from(r <= 0.9));
                }
            }
        }
    }
    bins
}
