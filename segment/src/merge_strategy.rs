// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Explicit maintenance merge policy and equivalent, fully validated executors.
//!
//! Auto deliberately selects direct merging for admitted inputs. There is no
//! calibrated density crossover yet. Forced reconstruction is an experimental
//! comparison path, not the old unvalidated oversized-input fallback.
use crate::merge::{self, MergeError, MergeInput, MergeLimits};
use crate::segment::{Segment, SegmentBuilder};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Auto,
    ForceDirect,
    ForceReconstruct,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    Direct,
    Reconstruct,
    /// Existing per-source PostgreSQL fallback; outside the aggregate API.
    LegacyOversized,
}

#[derive(Clone, Copy, Debug)]
pub struct Facts {
    pub input_bytes: u64,
    pub documents: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub strategy: Strategy,
    pub reason: &'static str,
}

/// Policy cannot override format admission limits. Resource budgets and actual
/// input contents are checked by execution, not trusted from these estimates.
pub fn choose(facts: Facts, policy: Policy) -> Plan {
    if facts.input_bytes > u64::from(u32::MAX) || facts.documents > u64::from(u32::MAX) {
        return Plan {
            strategy: Strategy::LegacyOversized,
            reason: "aggregate exceeds current format admission; retain per-source fallback",
        };
    }
    match policy {
        Policy::Auto => Plan {
            strategy: Strategy::Direct,
            reason: "default direct merge; no calibrated adaptive policy",
        },
        Policy::ForceDirect => Plan {
            strategy: Strategy::Direct,
            reason: "experimental forced direct merge",
        },
        Policy::ForceReconstruct => Plan {
            strategy: Strategy::Reconstruct,
            reason: "experimental forced validated reconstruction",
        },
    }
}

/// Execute an admitted strategy with complete input validation, including dead
/// postings. Duplicate live tuples are errors. Limits bound admission and output,
/// not peak allocations; the builder and a single records/finish operation are
/// infallible allocations and are not interruptible, like individual codec calls
/// in direct merging. No partial output is returned on cancellation or failure.
pub fn execute(
    strategy: Strategy,
    inputs: &[MergeInput<'_>],
    limits: MergeLimits,
    mut checkpoint: impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<Vec<u8>, MergeError> {
    match strategy {
        Strategy::Direct => merge::merge(inputs, limits, checkpoint),
        Strategy::LegacyOversized => Err(MergeError::Limit("legacy fallback requires caller")),
        Strategy::Reconstruct => {
            merge::validate_inputs(inputs, limits, &mut checkpoint)?;
            let mut builder = SegmentBuilder::default();
            for input in inputs {
                checkpoint()?;
                let segment = Segment::parse(input.bytes)?;
                for record in segment.records(|tid| input.dead.contains(&tid))? {
                    checkpoint()?;
                    builder.add_record(&record)?;
                }
            }
            checkpoint()?;
            let output = builder.finish();
            if output.len() > limits.max_output_bytes.min(u32::MAX as usize) {
                return Err(MergeError::Limit("output bytes"));
            }
            checkpoint()?;
            Ok(output)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::direct_merge_poc::fixture;
    use std::collections::BTreeSet;

    fn limits() -> MergeLimits {
        MergeLimits {
            max_inputs: 32,
            max_input_bytes: 1 << 24,
            max_documents: 10000,
            max_output_bytes: 1 << 24,
        }
    }

    #[test]
    fn policy_is_explicit_and_never_overrides_format_admission() {
        let facts = Facts {
            input_bytes: 100,
            documents: 10,
        };
        assert_eq!(choose(facts, Policy::Auto).strategy, Strategy::Direct);
        assert_eq!(
            choose(facts, Policy::ForceDirect).strategy,
            Strategy::Direct
        );
        assert_eq!(
            choose(facts, Policy::ForceReconstruct).strategy,
            Strategy::Reconstruct
        );
        for policy in [Policy::Auto, Policy::ForceDirect, Policy::ForceReconstruct] {
            for oversized in [
                Facts {
                    input_bytes: 1 << 32,
                    ..facts
                },
                Facts {
                    documents: 1 << 32,
                    ..facts
                },
            ] {
                assert_eq!(
                    choose(oversized, policy).strategy,
                    Strategy::LegacyOversized
                );
            }
        }
    }

    #[test]
    fn forced_strategies_have_identical_complete_output() {
        for deletion in [0, 1, 2, 7] {
            let (blobs, dead) = fixture(3, 65, 40, 67, true, deletion);
            let inputs = blobs
                .iter()
                .zip(&dead)
                .map(|(bytes, dead)| MergeInput { bytes, dead })
                .collect::<Vec<_>>();
            let direct = execute(Strategy::Direct, &inputs, limits(), || Ok(())).unwrap();
            assert_eq!(
                direct,
                execute(Strategy::Reconstruct, &inputs, limits(), || Ok(())).unwrap()
            );
            for strategy in [Strategy::Direct, Strategy::Reconstruct] {
                assert!(matches!(
                    execute(
                        strategy,
                        &inputs,
                        MergeLimits {
                            max_output_bytes: direct.len() - 1,
                            ..limits()
                        },
                        || Ok(())
                    ),
                    Err(MergeError::Limit("output bytes"))
                ));
                assert!(matches!(
                    execute(strategy, &inputs, limits(), || Err(MergeError::Cancelled)),
                    Err(MergeError::Cancelled)
                ));
            }
        }
    }

    #[test]
    fn mixed_legacy_inputs_and_dead_tuple_reuse_match() {
        use crate::segment::Format;
        let tid = crate::Tid::new(0, 1).unwrap();
        let mut blobs = Vec::new();
        for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] {
            let mut builder = SegmentBuilder::default();
            builder
                .add_document(tid, [("needle", 1), ("common", 2)])
                .unwrap();
            blobs.push(builder.finish_as(format));
        }
        let dead = [
            BTreeSet::from([tid]),
            BTreeSet::from([tid]),
            BTreeSet::new(),
        ];
        let inputs = blobs
            .iter()
            .zip(&dead)
            .map(|(bytes, dead)| MergeInput { bytes, dead })
            .collect::<Vec<_>>();
        let direct = execute(Strategy::Direct, &inputs, limits(), || Ok(())).unwrap();
        assert_eq!(
            direct,
            execute(Strategy::Reconstruct, &inputs, limits(), || Ok(())).unwrap()
        );
        for strategy in [Strategy::Direct, Strategy::Reconstruct] {
            for limited in [
                MergeLimits {
                    max_inputs: 2,
                    ..limits()
                },
                MergeLimits {
                    max_input_bytes: 1,
                    ..limits()
                },
                MergeLimits {
                    max_documents: 2,
                    ..limits()
                },
            ] {
                assert!(matches!(
                    execute(strategy, &inputs, limited, || Ok(())),
                    Err(MergeError::Limit(_))
                ));
            }
            // Cancellation during traversal, after complete input validation.
            let mut calls = 0;
            assert!(matches!(
                execute(strategy, &inputs, limits(), || {
                    calls += 1;
                    if calls == 9 {
                        Err(MergeError::Cancelled)
                    } else {
                        Ok(())
                    }
                }),
                Err(MergeError::Cancelled)
            ));
        }
    }

    #[test]
    fn both_strategies_reject_duplicates_invalid_dead_sets_and_dead_corruption() {
        let (mut blobs, _) = fixture(1, 4, 8, 3, false, 0);
        let empty = BTreeSet::new();
        let absent = BTreeSet::from([crate::Tid::new(99999, 1).unwrap()]);
        for strategy in [Strategy::Direct, Strategy::Reconstruct] {
            let input = MergeInput {
                bytes: &blobs[0],
                dead: &empty,
            };
            assert!(execute(strategy, &[input, input], limits(), || Ok(())).is_err());
            assert!(
                execute(
                    strategy,
                    &[MergeInput {
                        dead: &absent,
                        ..input
                    }],
                    limits(),
                    || Ok(())
                )
                .is_err()
            );
        }
        let dead = crate::verify::verify_segment(&blobs[0])
            .documents
            .into_iter()
            .collect();
        // Corrupt a dead document length while retaining a parseable header.
        // Skipping dead-document validation would silently accept this input.
        let n = blobs[0].len();
        blobs[0][n - 4..].copy_from_slice(&999u32.to_le_bytes());
        for strategy in [Strategy::Direct, Strategy::Reconstruct] {
            assert!(
                execute(
                    strategy,
                    &[MergeInput {
                        bytes: &blobs[0],
                        dead: &dead
                    }],
                    limits(),
                    || Ok(())
                )
                .is_err()
            );
        }
    }
}
