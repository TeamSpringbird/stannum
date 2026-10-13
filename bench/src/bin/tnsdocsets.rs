// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! What each dumped segment's document set costs a backend: its encoded
//! bytes, the heap the decoded set holds (by part) and the time to decode
//! it. A backend's reader cache keeps one per segment.
//!
//! ```text
//! tnsdocsets DUMP_DIR [--repeat N]
//! ```

use std::io::{Read, Seek, SeekFrom};
use std::time::Instant;

use segment::tinshape::docs::DocSet;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = std::path::PathBuf::from(args.next().expect("DUMP_DIR"));
    let mut repeat = 3usize;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--repeat" => repeat = args.next().expect("N").parse().expect("N"),
            _ => panic!("usage: tnsdocsets DUMP_DIR [--repeat N]"),
        }
    }
    let manifest = std::fs::read_to_string(dir.join("manifest.tsv")).expect("manifest.tsv");
    let (mut encoded, mut heap, mut geometry, mut micros) = (0usize, 0usize, 0usize, 0f64);
    println!("segment\tdocs\tgroups\tencoded\theap\tgeometry\tdecode_ms");
    for line in manifest.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let ["segment", generation, docs, blob, _] = f.as_slice() else {
            continue;
        };
        let mut file = std::fs::File::open(dir.join(blob)).expect("segment");
        let mut head = vec![0u8; 256];
        file.read_exact(&mut head).expect("header");
        assert_eq!(&head[..4], b"TNS1", "{blob}: not a TNS1 segment");
        let mut at = 4;
        // LEB128, as the segment's header writes it.
        let mut next = || {
            let (mut value, mut shift) = (0u64, 0u32);
            loop {
                let byte = head[at];
                at += 1;
                value |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    return value;
                }
                shift += 7;
            }
        };
        let (_documents, _total, _block, _flags) = (next(), next(), next(), next());
        let mut lens = [0u64; 6];
        for len in &mut lens {
            *len = next();
        }
        // Areas follow the header: area a spans bounds[a]..bounds[a + 1],
        // bounds[1] = header end; the document set is area 4.
        let start = at as u64 + lens[..3].iter().sum::<u64>();
        let len = lens[3] as usize;
        let mut bytes = vec![0u8; len];
        file.seek(SeekFrom::Start(start)).expect("seek");
        file.read_exact(&mut bytes).expect("document set");
        let mut best = f64::MAX;
        let mut set = None;
        for _ in 0..repeat.max(1) {
            let t = Instant::now();
            let decoded = DocSet::decode(&bytes).expect("decode");
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
            set = Some(decoded);
        }
        let set = set.expect("decoded");
        let h = set.heap_bytes();
        let g = set.geometry.heap_bytes();
        println!(
            "gen{generation}\t{docs}\t{}\t{len}\t{h}\t{g}\t{best:.2}",
            set.geometry.groups.len()
        );
        encoded += len;
        heap += h;
        geometry += g;
        micros += best;
    }
    println!(
        "total\t-\t-\t{encoded}\t{heap}\t{geometry}\t{micros:.2}\t(MiB: encoded {:.1}, heap {:.1}, geometry {:.1})",
        encoded as f64 / 1048576.0,
        heap as f64 / 1048576.0,
        geometry as f64 / 1048576.0
    );
}
