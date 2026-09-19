# Offline analysis of the 100k TIN experiments

Analysis date: 2026-09-19. No database queries were issued for this analysis.
Sources are the completed
[small experiment](../../benchmarks/results/tin-expanded-small-100k/experiment.json)
and [large experiment](../../benchmarks/results/tin-expanded-large-100k/experiment.json).
Artifact IDs below refer to `<ID>.json` and `<ID>.sql` inside those directories;
IDs align between the two runs. These local result directories are ignored by
Git, so preserve their contents separately when sharing this note.

Both manifests have collector SHA256
`25d21e702872af3ffc1e865a82446c1072c92bde0b8ffd612fe3464360380284`.
Use these manifests and saved SQL as the executed protocol: the current collector
has since gained additional cases that were **not** run in this 100k experiment.

## Main conclusions for Stannum

1. **Make indexed-filter pushdown a measurable execution alternative.** It
   substantially reduces text candidates on these fixtures. TIN's automatic
   choice is not always the fastest observed choice, so copying its thresholds
   would be less useful than reproducing the alternatives and measuring our own.
2. **Keep prepared-query strategy selection separate from parameter binding.**
   Generic ranked scans work, but selective SQL filters can make their execution
   much slower than custom plans. Supporting parameters does not finish the
   optimization problem.
3. **Retain specialized counting and avoid unconditional parallel execution.**
   The serial count path is much faster than either materializing matches or
   forcing a parallel count for the tested queries.
4. **Measure candidate rows as well as page work.** The faster pushdown plan
   sometimes touches more index pages. Page touches alone would reward the wrong
   plan here.

These are priorities suggested by TIN experiments, not measured speedups already
achieved in Stannum.

## Forced filtering: useful alternatives, imperfect automatic choice

All numbers below are median server execution milliseconds over three
instrumented executions; matching configurations used `work_mem = 2MB`.

| Query and configuration | Small | Large | Artifacts |
| --- | ---: | ---: | --- |
| Synthetic `common OR rare`, ID first 1%, LIMIT 10: auto | 1.348 | 1.363 | 00263, 00272, 00278 |
| Same: forced generic conjunction | 22.692 | 22.278 | 00260, 00267, 00276 |
| Same: disable index probe | 46.422 | 46.006 | 00261, 00270, 00275 |
| Wikipedia `history OR war`, ID first 25%, LIMIT 10: auto | 31.573 | 29.160 | 00766, 00775, 00781 |
| Same: forced pushdown | 19.195 | 18.534 | 00765, 00772, 00780 |
| Wikipedia first 25%, LIMIT 100: auto | 29.724 | 29.288 | 00787, 00796, 00802 |
| Same: forced pushdown | 19.010 | 18.607 | 00786, 00793, 00801 |

The 25% Wikipedia case improves about 36–39% with pushdown. The provider tree
alone hides the distinction: both contain `Conjunction Scan`, `Text Search Scan`,
and `Index TID Probe`. In
[large 00765](../../benchmarks/results/tin-expanded-large-100k/00765.json),
`Execution Mode` is `TID Filter Pushdown`; it reads 25,000 secondary TIDs and
the text node emits 6,974 rows. In
[large 00766](../../benchmarks/results/tin-expanded-large-100k/00766.json),
the mode is `Generic`, and the text node emits 27,946 rows before the conjunction
reduces them to 6,974. Small-machine observations show the same row counts.

This supports a hypothesis that avoiding candidate processing outweighs extra
index work. It does not reveal which particular CPU operation dominates. Three
repetitions and reordered strategies provide evidence for this fixture, not a
universal 25% cutoff. At the tested filtered shapes, forcing `TopK` often leaves
the sort/conjunction strategy in place; a requested debug setting is not evidence
that a distinct alternative actually ran.

## Counting: serial specialization wins here

| Count query/configuration | Small median ms | Large median ms | Artifacts |
| --- | ---: | ---: | --- |
| Synthetic OR, auto | 0.343 | 0.323 | 00386–00388 |
| Synthetic OR, disable count pushdown | 7.063 | 6.914 | 00389–00391 |
| Synthetic OR, force parallel | 10.762 | 9.612 | 00398–00400 |
| Wikipedia `history OR war`, auto | 0.402 | 0.308 | 00805–00807 |
| Wikipedia OR, disable count pushdown | 3.772 | 3.192 | 00808–00810 |
| Wikipedia OR, force parallel | 11.016 | 9.728 | 00817–00819 |

Auto uses `Tin Count`. Disabling pushdown produces `Aggregate / Text Search
Scan`; forcing parallel produces `Aggregate / Gather / Partial Tin Count`.
For the Wikipedia query, the latter two are approximately 9–10x and 27–32x
slower, respectively. This is a strong argument for a cheap serial count path
and a minimum-work threshold before worker startup. It is not evidence that
parallel search is generally harmful. Streaming versus sorted visibility flags
leave the observed `Tin Count` provider unchanged here and show similar times;
these clean-data queries do not establish a visibility-strategy crossover.

## Prepared statements: valid generic plans can still be expensive

Each table/mode executed a fixed sequence of 24 calls cycling four query texts
and two filter bounds. Auto chose five custom and then nineteen generic plans
on both machines and both tables. Force-custom and force-generic recorded
0/24 and 24/0 generic/custom counts, respectively (manifest `prepared-counts`
events).

| Wikipedia query, `id <= 1000`, LIMIT 10 | Small custom / generic ms | Large custom / generic ms | Custom artifacts | Generic artifacts |
| --- | ---: | ---: | --- | --- |
| `history` | 1.518 / 7.527 | 1.349 / 7.511 | 00632, 00636, 00640, 00644, 00648, 00652 | 00656, 00660, 00664, 00668, 00672, 00676 |
| `"united states"` | 3.379 / 10.204 | 3.041 / 9.612 | 00634, 00638, 00642, 00646, 00650, 00654 | 00658, 00662, 00666, 00670, 00674, 00678 |

These medians use six observations each. Custom plans use the conjunction/TID
probe and sort; generic plans retain `$1` and use a ranked text scan with
projection/filtering. Conversely, synthetic `common` with the same 1% ID bound
is faster under generic planning: large-machine medians 0.402 versus 1.243 ms
(generic 00237/00241/00245/00249/00253/00257, custom
00213/00217/00221/00225/00229/00233). There is no blanket winner.

**Not tested in these files:** reversed first-five histories or dynamic LIMIT.
Those cases exist in the newer collector, but the executed observation names
and events contain only the earlier fixed sequence. Do not attribute a
history-sensitivity result to this dataset.

## Mutation evidence is narrower than a maintenance benchmark

The maintenance probe is ranked `history OR war` LIMIT 10 using `full_score`,
not a timed count query. Artifacts 00823–00825 are before mutation;
00827–00829 follow deleting IDs divisible by ten; 00831–00833 follow appending
a marker to 5,000 rows; 00835–00837 follow VACUUM ANALYZE.

| Phase | Small median ms | Large median ms |
| --- | ---: | ---: |
| Before | 108.647 | 21.300 |
| Deleted | 72.990 | 20.905 |
| Updated | 133.427 | 31.714 |
| Vacuumed | 53.855 | 30.521 |

Large-machine results show an update-associated slowdown that the immediate
vacuum endpoint does not erase. Small-machine timings are highly variable,
including 40.750–109.744 ms before mutations. All these plans use parallel
ranking with `Gather Merge`; background activity, cache state, worker scheduling,
and physical layout are unresolved confounders. Do not fit a maintenance cost
model from these twelve observations per machine.

The marker count is zero before/deleted and exactly 5,000 after updates and
vacuum on both machines. Total index bytes grow from 176,275,456 to 356,368,384
on small and from 179,077,120 to 355,033,088 on large after updates. Those values
cover **all table indexes**, not just TIN. No reindex endpoint, fsck output, or
post-mutation segment snapshots exist in these completed 100k runs. They cannot establish
that maintenance fully converged or explain the size growth internally.

## Counter and hardware interpretation

In the 25% Wikipedia comparison, faster pushdown has more `Page Touches`:
large 112 versus 91, small 195 versus 137 (00765 versus 00766). Both examples
report zero shared reads. A page-count-only ranking would choose incorrectly.

Enabling reuse tracking for the Wikipedia count gives, in artifact 00820:

| Machine | Total touches | Unique | Repeated | Shared reads |
| --- | ---: | ---: | ---: | ---: |
| Small | 92 | 55 | 37 | 0 |
| Large | 55 | 51 | 4 | 0 |

The totals explicitly include repeated access. Differences in footer touches
are consistent with different layouts, but cannot identify the layout mechanism.
Build `maintenance_work_mem` was 16 MiB on small versus 690 MiB on large; default
index reloptions were NULL. Separate
[small](../../benchmarks/results/tin-expanded-small-100k/segment-snapshot.json)
and [large](../../benchmarks/results/tin-expanded-large-100k/segment-snapshot.json)
snapshots captured before mutation show four immutable build segments each,
with identical per-segment document counts (24,537; 25,342; 24,667; 25,454),
posting counts, and summed document lengths. Their total segment pages differ:
19,144 small versus 19,485 large. Thus segment-count differences do not explain
these runs, while internal page layout remains a confounder. The
Wikipedia table totals also differ: 392,282,112 versus 395,083,776 bytes at build.
Treat this as a default-configuration comparison, not an isolated CPU/RAM test.

Small shared buffers were 67 MiB and large 2 GiB. The approximately 374–377 MiB
table-plus-index working set exceeds small shared buffers, but that does not
prove it exceeds operating-system cache or causes physical storage I/O. The
selected probes above are already warm at the PostgreSQL buffer layer.

The concurrency phases are client-observed, network-inclusive tests using
persistent connections. At eight clients, small achieved 101.44/101.47 QPS,
large 164.40/161.70 QPS; no errors were recorded. Treat these as the measured
remote four-query workload, not either server's maximum search capacity.

## Errors and correctness scope

Each experiment records 828 attempted observations: 826 successful plans and
two errors. Both failures are the invalid syntax `common NOT rare`, artifacts
00162 and 00163, SQLSTATE `XX000`; the valid documented syntax is
`common AND NOT rare`. They are fixture errors, not failed valid search cases.
The current collector has already changed that string, so its source alone
would conceal what the old run attempted.

Each experiment contains 84 `same_score_sequence_as_auto` checks, all true.
They compare the ordered numeric scores and returned-row count at repetition
zero across the forced-strategy combinations. They do not prove complete row
membership, exact tie identity, or compatibility against Lead; the score
sequences themselves are not saved when they match. Count-strategy probes
collect EXPLAIN plans, not independently verified count values. There are no
recorded concurrency errors. Both manifests report successful cleanup and zero
remaining owned schemas.
