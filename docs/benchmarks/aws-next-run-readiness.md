# Next AWS / TIN comparison readiness

Status as of 2026-09-20: the previous EC2 experiment is torn down. This is a
preflight for a fresh controlled comparison, not a claim of new TIN timings.
The [existing AWS protocol](aws-comparable.md) owns provisioning, measurement
and teardown; reuse it rather than introducing another harness.

## Ordered remaining work

1. Compose bounded dictionary, postings and payload readers into maintenance
   input validation. Preserve document ownership, lengths, dead-TID checks,
   score-bound validation and cancellation. The existing whole-input verifier
   must remain until equivalent streaming checks are demonstrated.
2. Integrate incremental output encoding and account for every retained merge
   allocation, including document maps, dictionary output and PostgreSQL source
   caches. The experimental output-spill setting bounds output buffers only.
   Prove cancellation and failures cannot publish partial output; retain the
   separate crash/recovery campaign before promoting the spill stack.
3. Rehearse index construction and the pinned trace locally on all 5,032,104
   verified Wikipedia rows. Record build peaks, relation sizes, query-phase
   memory, reads, correctness and repeated latency/throughput. Choose memory
   caps from the measured working set; an OOM is a retained result, not a
   successful benchmark. Do not overlap builds with timed trials.
4. Freeze the candidate commit/image, driver revision, query-file hash and corpus
   hashes. Run the routine approximately 15-minute Lead compatibility gate;
   retain boundary/lifecycle diagnostic findings separately. Same-engine
   exhaustive ranking checks complement, rather than replace, Lead checks.
5. Run a fresh managed TIN endpoint and EC2 candidate with the same corpus,
   query text, projections, parameters, client counts and disclosed hardware/
   settings differences. Validate results before timing. Use server-side EXPLAIN
   times for single-query comparisons and a non-bottlenecking client for
   throughput. Preserve raw plans and client timings separately.

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
requires a PlanetScale database running TIN; provision it only when the local
gates pass and the measurement window is ready. Until then, run Stannum/GIN
locally or on EC2 and label old TIN figures as historical reference data. Old
credentials and timing results are not a substitute. Matching vCPU/RAM/storage
budgets does not establish identical CPU generations or managed-service internals.
The historical article's query-trace identity is also unresolved; keep a fresh
same-workload comparison separate from any comparison with its published chart.
