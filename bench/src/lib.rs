// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Measuring the query engine outside PostgreSQL: a dumped index replayed
//! through the engine crate (`src/bin/replay.rs`), and benchmarks of the
//! walk's kernels (`benches/kernels.rs`). Nothing here is part of the
//! extension.

pub mod areas;
pub mod dump;
pub mod paged;
pub mod replay;
pub mod tinshape;
pub mod whatif;

/// A query of a trace: its name, style and TINQL text.
#[derive(Clone, Debug)]
pub struct TraceQuery {
    pub name: String,
    pub style: String,
    pub text: String,
}

/// Reads a trace: `name<TAB>style<TAB>query` per line.
pub fn read_trace(path: &std::path::Path) -> Result<Vec<TraceQuery>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.splitn(3, '\t');
            match (fields.next(), fields.next(), fields.next()) {
                (Some(name), Some(style), Some(text)) => Ok(TraceQuery {
                    name: name.to_owned(),
                    style: style.to_owned(),
                    text: text.to_owned(),
                }),
                _ => Err(format!("{}: {line:?}", path.display())),
            }
        })
        .collect()
}
