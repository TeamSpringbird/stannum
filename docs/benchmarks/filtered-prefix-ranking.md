# Filtered prefix expansion: targeted wins, substantial regressions

A larger ranked prefix avoids exhaustive completion for some moderately filtered
queries, but retrying from the beginning remains expensive when the larger
prefix also proves insufficient. Two implementations were tested. The narrower
one preserves four measured LIMIT 10 wins of 13–15%, yet deliberately adverse
filters regress 48–61%. These results do **not** support enabling this heuristic
by default. Retain the experiments as evidence for a resumable ranked traversal
or another strategy that avoids repeating work.

## What was tested

The initial candidate preserves the ordinary block-max prefix and PostgreSQL's
normal SQL filter execution. When a filtered prefix is exhausted, it retries
block-max ranking once with 16 times the original LIMIT plus OFFSET, capped at
4,096 retained candidates. Already consumed root TIDs cannot be emitted again.
If the expanded prefix still cannot satisfy the parent, the existing exhaustive
completion runs. The cap bounds retained prefix size and the number of attempts;
it does not bound postings visited or total CPU work.

The first 48-case comparison exposed a larger-bound regression. A separate
six-case comparison exposed sparse-filter regressions. The revised candidate
therefore retries only when the original bound is at most 32 and at least one
row has already passed PostgreSQL's quals, limiting the expanded prefix to 512.
The final campaign includes both earlier workloads and two adverse distributions
that deliberately pass early rows before becoming sparse. No further gates were
added to fit these results.

## Revised candidate: wins and remaining failure

Times are medians of two run medians. Positive change means slower.

| Query / SQL filter / bound | Baseline ms | Revised ms | Change |
| --- | ---: | ---: | ---: |
| `history OR war`, 10% IDs, LIMIT 10 | 5.181 | 4.482 | −13.5% |
| `history OR war`, 25% IDs, LIMIT 10 | 5.178 | 4.400 | −15.0% |
| `history`, 10% IDs, LIMIT 10 | 2.778 | 2.369 | −14.7% |
| `history`, 25% IDs, LIMIT 10 | 2.696 | 2.310 | −14.3% |
| OR, early two winners plus `id % 1000 = 0`, LIMIT 10 | 6.876 | 10.198 | +48.3% |
| `history`, early two winners plus `id % 1000 = 0`, LIMIT 10 | 4.377 | 7.050 | +61.1% |

All four improvements hold in both paired comparisons. For 10%-filtered OR,
exhaustive scorer calls fall from 27,946 to zero, while scored candidates across
both block-max walks increase from 570 to 3,318. The executor fetches the same
90 visible rows to produce ten qualifying rows. This avoids the full scoring
pass without moving heap filtering ahead of scoring.

The adverse predicates explicitly include the two highest-ranked IDs, followed
by sparse matches. They are `(id IN (75357,27144) OR id % 1000 = 0)` for OR and
`(id IN (36535,31304) OR id % 1000 = 0)` for `history`. Both pass the observed-
acceptance gate, then exhaust the expanded prefix. The candidate performs one
expansion **and** one completion, preserving all 27,946 or 22,063 exhaustive
scorer calls while adding the second block-max walk. Successful visible heap
fetches remain 5,486 and 6,634, respectively. Candidate/baseline ratios are
1.618/1.368 for OR and 1.809/1.456 for `history` across the two pairs.

Passing an early row therefore does not establish that later ranked rows will
pass. A small retained-prefix cap alone does not make the retry cheap.

## Controls and limits on attribution

The revised gate skips expansion for the sparse modulo predicates and original
bound 120. All-pass and unfiltered queries still finish from the initial prefix.
Runtime bounds with residual quals remain unbounded. Phrase queries in this
fixture do not use block-max pruning. These are controls, not expansion gains.

The final campaign also measures slower controls: unfiltered `history` LIMIT 10
is 0.569→0.625 ms (+10.0%), phrase with 25% IDs is 3.674→3.971 ms (+8.1%), and
`history` with 25% IDs, LIMIT 100 OFFSET 20 is 3.921→4.262 ms (+8.7%). Each is
slower in both pairs despite unchanged scorer and fetch counters. Most unchanged
paths move approximately 4–11% in this campaign. We have not isolated execution
overhead from environmental variation and do not claim that these controls are
performance-neutral. The much larger adverse-case regressions also have clear
additional work in the counters; they suffice to reject default promotion.

This is one mostly visible, read-only 100k fixture with warm repeated queries.
It does not establish behavior at a million rows, under concurrent performance
load, with cold storage, or for other score/filter correlations. These are local
Stannum comparisons, not TIN speed comparisons or reconstructed TIN policy.

## Initial experiments and why the gate changed

The initial candidate improves filtered OR LIMIT 10 by 20–24% and `history` by
15–18%, but loses at larger bounds. At 25%-filtered `history`, LIMIT 100 OFFSET
20 rises from 3.932 to 4.250 ms (+8.1%), with both pairs slower. Although it
eliminates 22,063 exhaustive scorer calls, the block-max walks now score 30,801
candidates in total versus 10,921 initially. A lower exhaustive-call count does
not by itself prove cheaper execution.

A separate six-case follow-up uses sparse modulo predicates. These leave zero
qualifying rows in the original top ten for OR and `history`. The extra prefix
also fails, adding a retry before the same exhaustive completion:

| Query / predicate / LIMIT 10 | Baseline ms | Initial ms | Change |
| --- | ---: | ---: | ---: |
| OR, `id % 100 = 0` | 5.648 | 7.685 | +36.1% |
| OR, `id % 1000 = 0` | 7.119 | 9.060 | +27.3% |
| `history`, `id % 100 = 0` | 2.960 | 4.541 | +53.4% |
| `history`, `id % 1000 = 0` | 5.342 | 6.348 | +18.8% |

All four lose in both pairs. The revised gate addresses these exact conditions,
but the later adverse cases show why they are insufficient for general use.
Do not compare timings from different campaigns as a paired estimate.

## Measurement and correctness

The existing `benchmarks/server_times.py` harness ran baseline/candidate/
candidate/baseline rounds, with seven retained interleaved repetitions after
discarding two per round. Each campaign reused one baseline-built fixture across
clean server restarts with separate frozen images. Reported times use normal
EXPLAIN ANALYZE node instrumentation and are server execution times, not client
throughput. No scan method was forcibly disabled.

- PostgreSQL 18.6 ARM64 in Docker; four CPU quota, 4 GiB RAM, 1 GiB shared
  buffers, 512 MiB maintenance memory, 16 MiB work memory, JIT off.
- 100,000 Wikipedia documents, IDs 1–100,000; CSV SHA256
  `d01f490c802313190924a1edd1722d9b1c543793a8ccd0950108d813cb8b7da1`.
- VACUUM ANALYZE before each campaign. All-visible pages remain 10,467 of
  10,496 before and after every measured round in all three campaigns.
- Force generic prepared plans; text queries are parameters while literal and
  runtime bounds remain distinct. ID-range percentages describe the corpus,
  not the fraction of text matches.
- Per-round validation checks eligible membership, unique IDs, exact float32
  score bits and descending score sequence against baseline MATERIALIZED
  exhaustive references. Tied IDs may differ where SQL leaves them unspecified.
- All **440 checks pass**: 192 initial, 24 sparse follow-up, 224 revised. This
  differential check supplements the separate Lead semantic gate.

The revised source also passes all 126 native PostgreSQL 18 tests and Clippy.
A 45.3-second concurrent fuzzer run passes 264 comparisons over 32,140 rows,
with 1,608 writer operations, 261 vacuums, 257 cursor fetches and two observed
expanded plans. Coverage instrumentation runs EXPLAIN ANALYZE probes; the
existing differential oracle is unchanged. This supplies bounded correctness
coverage, not concurrent throughput evidence. The broader initial candidate's
separate 364-comparison fuzz run is not counted as revised-source validation.

## Next architectural experiment

Preserve the ranked traversal's unfinished frontier so an insufficient prefix
can resume without rescoring the same documents from the beginning. A small
prototype should first prove monotonic score order, tie handling, consumed-root
identity, cursor continuation, rescan behavior and snapshot correctness, then
measure the same successful and adverse cases. This is a proposed experiment,
not an implemented or proven speedup.

An eligibility-aware ranking path is another option if it can cheaply acquire
eligible root TIDs or evaluate filters during pruning. The previous
[early-filter experiment](early-filter-ranking.md) shows why fetching every text
candidate from the heap is not enough. Neither experiment yet establishes the
cost of integrating secondary-index eligibility, HOT identity, lossy rechecks
and visibility. Prefer a small measured proof before a wider architecture change.

## Reproduction and retained evidence

These experiments describe the implementation on
[PR #41](https://github.com/TeamSpringbird/stannum/pull/41), not a default strategy
available on main. Build the precise source commits below with
`python3 benchmarks/tin.py build-image`; image labels and frozen source manifests
identify the implementations that actually ran.

| Version | Source commit | Source SHA256 |
| --- | --- | --- |
| Baseline | `07fedc69f049c34ce302c660c5cb5edf19e5d96d` | `e6d2d2f2d8ac20677911528827bc0ade9e4aa21ccd70ed59318678efdb7bdcde` |
| Initial | `394af890efc93f490e8463ce0210629d903e9007` | `11747c357d41b13743a52f14740bccbab4ce3f183531791c67d21b5c406e9eed` |
| Revised | `9b05b5a16ccb246de13a6146941b5bdfaf7d3f45` | `33d72cb462c6a268f0a7aa5a5b39cf5e4128197c90b89101e4bde1d7f13341bb` |

Image IDs and relevant settings are in the compact results. Checked-in artifacts:
[56 final cases](filtered-prefix-cases.json),
[final results](filtered-prefix-results.json),
[representative final plans](filtered-prefix-plan-examples.json),
[initial results](filtered-prefix-initial-results.json),
[initial sparse results](filtered-prefix-initial-sparse-results.json), and
[representative initial plans](filtered-prefix-initial-plan-examples.json).
Case specifications describe SQL generation; the generated setup/query input is
retained in raw artifacts for the existing server-times harness.

Ignored local raw directories are `benchmarks/results/filtered-prefix-100k/`,
`filtered-prefix-sparse-100k/`, and `filtered-prefix-gated-100k/`. They retain
source manifests, image labels, generated SQL, all 3,960 instrumented plans,
reference scores, checks, visibility snapshots and temporary orchestration.
Each campaign's container and volume were removed after completion. No new
permanent benchmark runner was added.

A local archive preserves all three campaigns, three build provenances, both
fuzz results and orchestration: `benchmarks/results/filtered-prefix-evidence.tar.gz`
in the prototype worktree. It is ignored and is not distributed by the findings
PR. Size: 2,183,502 bytes; SHA256:
`20dca108327a64633e404fa07373ad7b57929964f2c198b1f55101328b716f8c`.
Checked-in summaries and representative plans remain available without it.

The retry prototype is not being promoted to main. A separate correctness fix
for rescanning a completed ranked scan can be reviewed independently; it does
not ship the speculative retry or claim these prototype speedups for main.
