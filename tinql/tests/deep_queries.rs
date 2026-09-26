// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Oversized and deeply nested queries must end in `Ok` or a clean `Err`.
//!
//! A PostgreSQL backend runs the whole query front end (parse, sub-tokenize,
//! lower, simplify, display, estimate, drop) on an 8 MiB stack, and Rust has
//! no stack-overflow handler inside the host: an overflow aborts the backend
//! and the postmaster restarts every session. Each case here therefore runs
//! the front end on a thread with an 8 MiB stack, in a child process (this
//! test binary re-executed with [`CHILD_ENV`] set), so an abort is reported
//! as a failing test instead of killing the whole test binary, and under a
//! time limit, so a super-linear pass fails instead of hanging.

use std::convert::Infallible;
use std::fs::File;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use segment::index::Window;
use tinql::runtime::estimate::{Statistics, estimate};
use tinql::runtime::lower::lower_with_profile;
use tinql::runtime::subtokenize::sub_tokenize;
use tinql::runtime::{SimplificationProfile, parse_tinql_to_query_default, simplify};
use tinql::{ImplicitOp, parse};
use tokenizer::presets::default_pipeline;

/// Set in the re-executed child; names the case it should run.
const CHILD_ENV: &str = "TINQL_DEEP_QUERY_CHILD";
/// Printed by the child once the front end returned normally.
const DONE_MARKER: &str = "deep-query-case-finished:";
/// The PostgreSQL backend stack the front end must fit in.
const BACKEND_STACK: usize = 8 << 20;
/// Wall-clock budget per case, generous for unoptimized builds.
const TIME_LIMIT: Duration = Duration::from_secs(120);

/// Uniform statistics: every term is in a tenth of the documents.
struct Uniform;

impl Statistics for Uniform {
    type Error = Infallible;
    fn documents(&self) -> Result<f64, Infallible> {
        Ok(1_000_000.0)
    }
    fn document_frequency(&self, _term: &str) -> Result<f64, Infallible> {
        Ok(100_000.0)
    }
    fn expansion_frequency(
        &self,
        _window: Window<'_>,
        _filter: &dyn Fn(&str) -> bool,
    ) -> Result<Option<f64>, Infallible> {
        Ok(Some(100_000.0))
    }
}

/// Runs every front-end pass over `query` and describes how it ended. Each
/// intermediate value is dropped inside the pass that owns it, so dropping a
/// deep tree is exercised too.
fn exercise(query: &str) -> String {
    let mut outcome = Vec::new();
    for implicit in [ImplicitOp::And, ImplicitOp::Or] {
        match parse(query, implicit) {
            Ok(expr) => {
                let shown = expr.to_string();
                outcome.push(format!("parsed ({} bytes shown)", shown.len()));
                let copy = expr.clone();
                assert!(copy == expr, "a clone compares equal");
                drop(copy);
                match sub_tokenize(expr, default_pipeline()) {
                    Ok(analyzed) => {
                        let _ = analyzed.to_string();
                        for profile in [
                            SimplificationProfile::Structural,
                            SimplificationProfile::StructuralPreserveTermMultiplicity,
                        ] {
                            match lower_with_profile(&analyzed, profile) {
                                Ok(lowered) => {
                                    let _ = lowered.to_string();
                                    let _ = lowered.estimate_tuples(1_000_000, &|_| 1_000);
                                    let _ = estimate(&lowered, &Uniform);
                                    let _ = lowered.terms().len();
                                    let unscored =
                                        simplify(lowered, SimplificationProfile::LogicalUnscored);
                                    let _ = unscored.to_string();
                                    outcome.push("lowered".into());
                                }
                                Err(error) => outcome.push(format!("lower error: {error}")),
                            }
                        }
                    }
                    Err(error) => outcome.push(format!("sub-tokenize error: {error}")),
                }
            }
            Err(error) => outcome.push(format!("parse error: {error}")),
        }
    }
    match parse_tinql_to_query_default(query) {
        Ok(_) => outcome.push("default pipeline ok".into()),
        Err(error) => outcome.push(format!("default pipeline error: {error}")),
    }
    let mut outcome = outcome.join("; ");
    outcome.truncate(2_000);
    outcome
}

/// Runs `build`'s query through [`exercise`] in a child process on a
/// backend-sized stack. `name` must be the calling test's name, which the
/// child uses to select it.
fn assert_front_end_survives(name: &str, build: fn() -> String) {
    if std::env::var(CHILD_ENV).as_deref() == Ok(name) {
        let query = build();
        let outcome = std::thread::Builder::new()
            .stack_size(BACKEND_STACK)
            .spawn(move || exercise(&query))
            .expect("spawn the front-end thread")
            .join()
            .expect("the front end panicked");
        println!("{DONE_MARKER} {outcome}");
        return;
    }

    let log = |stream: &str| -> PathBuf {
        std::env::temp_dir().join(format!(
            "tinql-deep-query-{}-{name}.{stream}",
            std::process::id()
        ))
    };
    let (stdout_path, stderr_path) = (log("stdout"), log("stderr"));
    let mut child = Command::new(std::env::current_exe().expect("test binary path"))
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, name)
        .stdin(Stdio::null())
        .stdout(File::create(&stdout_path).expect("stdout log"))
        .stderr(File::create(&stderr_path).expect("stderr log"))
        .spawn()
        .expect("re-execute the test binary");

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll the child") {
            break Some(status);
        }
        if started.elapsed() > TIME_LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let elapsed = started.elapsed();
    let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    let _ = std::fs::remove_file(&stdout_path);
    let _ = std::fs::remove_file(&stderr_path);
    let tail = |text: &str| -> String {
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(8)..].join("\n")
    };

    let Some(status) = status else {
        panic!("{name}: the front end did not finish within {TIME_LIMIT:?}");
    };
    // libtest prints "test <name> ... " without a newline before the child's
    // own output, so the marker can sit mid-line.
    let finished = stdout
        .find(DONE_MARKER)
        .map(|at| stdout[at..].lines().next().unwrap_or_default());
    assert!(
        status.success() && finished.is_some(),
        "{name}: the front end did not return normally ({status}) after {elapsed:?}\n\
         --- child stderr ---\n{}\n--- child stdout ---\n{}",
        tail(&stderr),
        tail(&stdout),
    );
    eprintln!("{name}: {} in {elapsed:?}", finished.unwrap());
}

const WORDS: usize = 300_000;
const NESTING: usize = 100_000;

#[test]
fn plain_words_300_000() {
    assert_front_end_survives("plain_words_300_000", || "a ".repeat(WORDS));
}

#[test]
fn or_chain_300_000() {
    assert_front_end_survives("or_chain_300_000", || vec!["a"; WORDS].join(" OR "));
}

#[test]
fn and_not_chain_300_000() {
    assert_front_end_survives("and_not_chain_300_000", || {
        vec!["a"; WORDS].join(" AND NOT ")
    });
}

#[test]
fn then_chain_300_000() {
    assert_front_end_survives("then_chain_300_000", || vec!["a"; WORDS].join(" THEN/1 "));
}

#[test]
fn or_chain_in_alternatives_300_000() {
    assert_front_end_survives("or_chain_in_alternatives_300_000", || {
        format!("[{}]", vec!["a"; WORDS].join(" OR "))
    });
}

#[test]
fn words_inside_a_span_300_000() {
    assert_front_end_survives("words_inside_a_span_300_000", || {
        format!("({}) WITHIN 5", "a ".repeat(WORDS))
    });
}

#[test]
fn phrase_with_pinned_gaps_100_000() {
    assert_front_end_survives("phrase_with_pinned_gaps_100_000", || {
        format!("\"{}a\"", "a _ ".repeat(NESTING))
    });
}

#[test]
fn nested_parentheses_100_000() {
    assert_front_end_survives("nested_parentheses_100_000", || {
        format!("{}a{}", "(".repeat(NESTING), ")".repeat(NESTING))
    });
}

#[test]
fn nested_groups_with_terms_100_000() {
    assert_front_end_survives("nested_groups_with_terms_100_000", || {
        format!("{}a{}", "(a ".repeat(NESTING), ")".repeat(NESTING))
    });
}

#[test]
fn nested_alternatives_100_000() {
    assert_front_end_survives("nested_alternatives_100_000", || {
        format!("{}a{}", "[".repeat(NESTING), "]".repeat(NESTING))
    });
}

#[test]
fn unclosed_parentheses_100_000() {
    assert_front_end_survives("unclosed_parentheses_100_000", || {
        format!("{}a", "(".repeat(NESTING))
    });
}

#[test]
fn matches_with_nested_groups_100_000() {
    assert_front_end_survives("matches_with_nested_groups_100_000", || {
        format!("MATCHES {}a{}", "(".repeat(NESTING), ")".repeat(NESTING))
    });
}

#[test]
fn matches_with_unclosed_groups_64() {
    assert_front_end_survives("matches_with_unclosed_groups_64", || {
        format!("MATCHES {}a", "(".repeat(64))
    });
}

#[test]
fn matches_with_unclosed_classes_100_000() {
    assert_front_end_survives("matches_with_unclosed_classes_100_000", || {
        format!("MATCHES {}", "[".repeat(NESTING))
    });
}

#[test]
fn matches_inside_nested_parentheses_100_000() {
    assert_front_end_survives("matches_inside_nested_parentheses_100_000", || {
        format!("{}MATCHES a.*{}", "(".repeat(NESTING), ")".repeat(NESTING))
    });
}

#[test]
fn phrase_alternatives_with_nested_parentheses_100_000() {
    assert_front_end_survives(
        "phrase_alternatives_with_nested_parentheses_100_000",
        || format!("\"x [{}a{}]\"", "(".repeat(NESTING), ")".repeat(NESTING)),
    );
}

#[test]
fn at_least_one_of_100_000_terms() {
    assert_front_end_survives("at_least_one_of_100_000_terms", || {
        let terms: Vec<String> = (0..NESTING).map(|i| format!("t{i}")).collect();
        format!("AT LEAST 1 OF [{}]", terms.join(" "))
    });
}

#[test]
fn at_least_half_of_100_000_distinct_terms() {
    assert_front_end_survives("at_least_half_of_100_000_distinct_terms", || {
        let terms: Vec<String> = (0..NESTING).map(|i| format!("t{i}")).collect();
        format!("AT LEAST 50% OF [{}]", terms.join(" "))
    });
}
