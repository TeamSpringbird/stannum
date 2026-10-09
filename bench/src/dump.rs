// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! An index as `script/dump-segments.py` writes it: a directory holding
//! `manifest.tsv`, a `gen<N>.segment` blob per directory entry, a
//! `gen<N>.dead` dead list per entry that has one, and optionally `ids.tsv`
//! mapping heap locations to the table's `id` column.
//!
//! `manifest.tsv` is tab-separated, one record per line:
//!
//! ```text
//! format        stannum-dump 1
//! spec          <the meta page's 8 tokenizer setting bytes, as hex>
//! k1            <the index's k1 reloption>          (absent for the default)
//! b             <the index's b reloption>           (absent for the default)
//! score_stop_words  <the reloption's text>          (absent when unset)
//! buffer_docs   <documents in the write buffer, which is not dumped>
//! segment       <generation> <documents> <blob file> <dead list file or ->
//! ```
//!
//! Segments are listed in directory order. `ids.tsv` holds `block offset id`
//! per heap tuple.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engine::bm25::Bm25Params;
use rustc_hash::FxHashMap;
use segment::Tid;
use tokenizer::CompiledTokenizerPipeline;

use crate::areas::AreaMap;

/// One dumped segment.
pub struct DumpedSegment {
    pub generation: u32,
    pub documents: u32,
    pub blob: Arc<Vec<u8>>,
    pub areas: Arc<AreaMap>,
    pub dead: Option<Arc<Vec<u8>>>,
}

/// A dumped index.
pub struct Dump {
    pub dir: PathBuf,
    pub spec: [u8; engine::spec::SPEC_BYTES],
    pub params: Bm25Params,
    pub stop_words: Option<String>,
    /// Documents in the write buffer, which the dump does not hold.
    pub buffer_docs: u64,
    pub segments: Vec<DumpedSegment>,
    /// Heap location to `id`, when `ids.tsv` was dumped.
    pub ids: Option<FxHashMap<Tid, String>>,
}

fn parse_hex(text: &str) -> Option<Vec<u8>> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

impl Dump {
    /// Reads the dump in `dir`.
    pub fn open(dir: &Path) -> Result<Self, String> {
        let manifest = std::fs::read_to_string(dir.join("manifest.tsv"))
            .map_err(|error| format!("{}: {error}", dir.join("manifest.tsv").display()))?;
        let mut spec = None;
        let mut params = Bm25Params::default_bm25();
        let mut stop_words = None;
        let mut buffer_docs = 0;
        let mut segments = Vec::new();
        for (number, line) in manifest.lines().enumerate() {
            let fields: Vec<&str> = line.split('\t').collect();
            let bad = || format!("manifest.tsv line {}: {line:?}", number + 1);
            // Reloptions are parsed as PostgreSQL parses a real option, a
            // double, and narrowed as the extension narrows it.
            let real = |text: &str| text.parse::<f64>().map(|v| v as f32).map_err(|_| bad());
            match fields.as_slice() {
                ["format", "stannum-dump 1"] => {}
                ["format", other] => return Err(format!("unsupported dump format {other}")),
                ["spec", hex] => {
                    let bytes = parse_hex(hex).ok_or_else(bad)?;
                    spec =
                        Some(<[u8; engine::spec::SPEC_BYTES]>::try_from(bytes).map_err(|_| bad())?);
                }
                ["k1", value] => params.k1 = real(value)?,
                ["b", value] => params.b = real(value)?,
                ["score_stop_words", value] => stop_words = Some((*value).to_owned()),
                ["buffer_docs", value] => buffer_docs = value.parse().map_err(|_| bad())?,
                ["segment", generation, documents, blob, dead] => {
                    let read = |name: &str| {
                        std::fs::read(dir.join(name))
                            .map_err(|error| format!("{}: {error}", dir.join(name).display()))
                    };
                    let blob = read(blob)?;
                    let areas = AreaMap::of(&blob).map_err(|error| format!("{line}: {error}"))?;
                    segments.push(DumpedSegment {
                        generation: generation.parse().map_err(|_| bad())?,
                        documents: documents.parse().map_err(|_| bad())?,
                        blob: Arc::new(blob),
                        areas: Arc::new(areas),
                        dead: match *dead {
                            "-" => None,
                            name => Some(Arc::new(read(name)?)),
                        },
                    });
                }
                [] | [""] => {}
                _ => return Err(bad()),
            }
        }
        let spec = spec.ok_or("manifest.tsv names no tokenizer spec")?;
        let ids = match std::fs::read_to_string(dir.join("ids.tsv")) {
            Ok(text) => {
                let mut ids = FxHashMap::default();
                for line in text.lines() {
                    let mut fields = line.splitn(3, '\t');
                    let (Some(block), Some(offset), Some(id)) =
                        (fields.next(), fields.next(), fields.next())
                    else {
                        return Err(format!("ids.tsv: {line:?}"));
                    };
                    let tid = Tid {
                        block: block.parse().map_err(|_| format!("ids.tsv: {line:?}"))?,
                        offset: offset.parse().map_err(|_| format!("ids.tsv: {line:?}"))?,
                    };
                    ids.insert(tid, id.to_owned());
                }
                Some(ids)
            }
            Err(_) => None,
        };
        Ok(Self {
            dir: dir.to_owned(),
            spec,
            params,
            stop_words,
            buffer_docs,
            segments,
            ids,
        })
    }

    /// The tokenizer the index analyzes text with.
    pub fn tokenizer(&self) -> Result<CompiledTokenizerPipeline, String> {
        engine::spec::decode_spec(&self.spec)
            .ok_or("the tokenizer settings are unreadable")?
            .compile()
            .map_err(|error| format!("tokenizer settings: {error}"))
    }

    pub fn documents(&self) -> u64 {
        self.segments.iter().map(|s| u64::from(s.documents)).sum()
    }

    pub fn bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.blob.len() as u64).sum()
    }
}
