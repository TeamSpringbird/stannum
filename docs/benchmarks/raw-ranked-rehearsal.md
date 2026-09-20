# Raw Stack Exchange ranked rehearsal

Both engines completed ranked campaigns on 100 and 10,000 unmodified prepared
Stack Exchange documents. All 3,762 forms passed exact sampled membership checks
and full-corpus exhaustive same-engine top-ten score-multiset checks. Both also
traversed all 3,762 forms during measured traffic. Equal-score ties are unordered.
This is a local integration/scale-up result, not a closed-source TIN measurement.

The 100-row run checked membership on all rows. The 10,000-row run checked it on
1,000 rows while retaining full-corpus counts and exhaustive ranked checks for
every form. The separate 1,000-row Lead campaign remains the independent semantic
check; this larger run does not compare 10,000 rows against Lead.

## Conditions and observations

Native ARM64 Docker / PostgreSQL 18, four allocated CPUs, two query clients,
2 GiB container memory, 128 MiB shared buffers, 64 MiB maintenance_work_mem,
16 MiB work_mem and JIT disabled. The same immutable image was used for both
sizes. Each engine started from a fresh container/volume; builds and validation
were outside measured traffic. There were no updates. The 100-row window used
three seconds warmup and 15 seconds measurement; 10,000 rows used five and 60.

| Rows | Engine | QPS | p95 ms | Measured forms |
|---:|---|---:|---:|---:|
| 100 | Stannum | 2,062.1 | 1.539 | 3,762 |
| 100 | GIN | 4,718.9 | 1.240 | 3,762 |
| 10,000 | Stannum | 4,018.4 | 1.463 | 3,762 |
| 10,000 | GIN | 121.9 | 87.273 | 3,762 |

These are single diagnostic windows, not repeated capacity estimates. Do not
interpret the change between sizes as a scaling curve: window duration differs,
planner/strategy choices can change, and no controlled explanation was tested.
GIN uses ts_rank_cd and Stannum uses BM25. At 10,000 rows there were 1,184 forms
with differing membership on the sample and 1,326 with differing full counts.
No equivalent-result speedup ratio is justified by this table.

At 10,000 rows, Stannum p95 was 0.764 ms for AND, 1.958 ms for OR and 0.716 ms
for phrases. GIN's OR p95 was 117.991 ms; its other families were approximately
1.24 ms. This identifies ranked OR as a useful workload to keep profiling, not
proof of the cause of the difference between engines.

Stannum's index was 2.12 MiB and total relation storage 4.95 MiB. Sampled
query-phase container memory peaked at 111,915,008 bytes (106.7 MiB), with a
274,792,448-byte lifetime high-water mark. GIN's sampled query peak was
159,326,208 bytes. Neither run recorded OOM events. This is far below the 2 GiB
cap and does not demonstrate memory-pressure behavior. VM I/O counters do not
prove physical SSD traffic; memory samples can miss peaks.

## Next experiments

1. Run 100,000 raw rows with the full trace and an explicit validation policy.
   Retain every full ranked check if practical; if sampling becomes necessary,
   record the selected forms and do not call unchecked forms verified.
2. Repeat baseline/candidate trials before claiming an optimization. Profile
   ranked OR using server plans and CPU data on a corpus large enough to matter.
3. Add concurrent updates and post-update correctness separately from read-only
   ranking. Then increase corpus size until the working set pressures memory.
4. Use the established EC2 protocol for an AWS rehearsal once these local gates
   pass. A fresh TIN comparison requires a running PlanetScale instance with the
   same corpus/trace and disclosed settings/hardware differences.

## Evidence and reproduction

[Compact results](raw-ranked-rehearsal-results.json) retain immutable image/source,
input/trace identities, configuration, correctness results, family distributions
and resource coverage. Raw evidence is archived locally at
`benchmarks/results/raw-ranked-rehearsal-evidence.tar.gz`; its SHA-256 is in the
receipt. The archive includes full SQL, plans, observations, driver output,
resource samples and protocol snapshots. Both engines' owned containers and
volumes were verified absent after completion.

Reproduce with the published corpus and verified driver using `benchmarks/tin.py
run --published-corpus stackexchange --rows 10000 --validation-rows 1000
--engines stannum postgres --workload topk --style mixed --warmup 5 --seconds 60
--clients 2 --memory 2g --shared-buffers 128MB --maintenance-work-mem 64MB`, plus
explicit dataset, driver, matching image and new output paths. The harness owns
the native-test lock. Use a freshly built matching image or its recorded source
manifest; do not bypass provenance checks to reuse an unrelated image.
