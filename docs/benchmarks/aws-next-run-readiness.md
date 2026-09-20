# Next AWS / TIN comparison readiness

Status as of 2026-09-20: the previous EC2 experiment is torn down. This is a
preflight for a fresh controlled comparison, not a claim of new TIN timings.
The [existing AWS protocol](aws-comparable.md) owns provisioning, measurement
and teardown; reuse it rather than introducing another harness.

## What blocks which comparison

Current main can be benchmarked locally or on EC2 now. Bounded merge-memory work
and the experimental spill stack are optimization tasks, not prerequisites for
an honest baseline. The ordered work and acceptance criteria live in the
[performance burndown](priorities.md).

For a fresh EC2 baseline, freeze the candidate/driver/data identities, run the
routine Lead gate, specify resource limits and record the provisioning/teardown
plan. Working development-account authentication was verified on 2026-09-20;
that is not proof that every provisioning permission or quota is available.
No new AWS resources have been created for the ten-minute local campaign.

There are two different TIN comparisons:

1. **Published TIN reference:** run Stannum and open-source baselines against the
   published corpus, trace and documented EC2/container configuration. Preserve
   unknown historical settings and revision differences. This does not require
   a live PlanetScale database or access to a TIN binary.
2. **Fresh managed TIN:** use a running PlanetScale endpoint and an EC2 candidate
   with matched inputs/query shapes and disclosed platform differences. Keep
   server-side execution time separate from client/network time. This is a
   separate experiment from reproducing the standalone EC2 blog environment.

Finish and audit the current local campaign, then establish quiet repeated runs
and a larger working-set rehearsal to inform the cloud resource plan. These
improve the comparison; they do not make every optimization a launch blocker.

## What is already available

- The exact prepared Wikipedia and Stack Exchange CSVs have verified hashes:
  [dataset receipts and loader](published-datasets.md).
- The pinned PlanetScale driver and repeated measurement orchestration exist:
  [published trace protocol](published-trace.md).
- Prior million-row pressure results and output-spill experiments are useful
  diagnostics, but are neither full-corpus results nor current TIN comparisons.
- CloudFormation owns the temporary EC2 resources, with a lifetime backstop and
  explicit teardown verification in the existing AWS protocol.

TIN is closed source and cannot be installed on our EC2 host. A live comparison
requires a PlanetScale database running TIN; provision it when its specific
measurement window and resource plan are ready. Until then, run Stannum/GIN
locally or on EC2 and label old TIN figures as historical reference data. Old
credentials and timing results are not a substitute. Matching vCPU/RAM/storage
budgets does not establish identical CPU generations or managed-service internals.
The maintainer confirmed the full Stack Exchange trace: 1,254 samples and
3,762 AND/OR/phrase forms. Keep fresh measurements separate from the historical
chart unless the remaining run settings and hardware are also matched.
