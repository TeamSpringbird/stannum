# TIN plan choices across two machine sizes

On September 19, 2026, the same catalog collector ran 85 cases at each of
1,000, 10,000 and 50,000 rows on two user-provided PlanetScale databases.
Both reported PostgreSQL 18.6 on ARM64 and TIN 1.0.2. All 510 observations
completed, and both databases were verified to have no remaining probe schemas.
TIN was enabled on the new database and remains installed.

[Machine comparison data](tin-machine-comparison.json) records the server
settings, relation sizes, and matched plan signatures. The complete EXPLAIN
JSON, SQL and collector snapshots remain locally under
`benchmarks/results/tin-catalog-small-paired` and
`benchmarks/results/tin-catalog-large`. Neither artifact set contains connection
credentials. See [the catalog methodology](tin-plan-catalog.md) for fixtures
and query definitions.

| Setting | Small database | Larger database |
| --- | --- | --- |
| shared_buffers | 67 MiB | 2 GiB |
| work_mem | 2 MiB | 26 MiB |
| effective_cache_size | 203 MiB | 8 GiB |
| random_page_cost | 1.1 | 1.1 |
| max_parallel_workers_per_gather | 2 | 2 |

These are observed PostgreSQL settings, not independently verified hardware
allocations or SKU names. The larger database was requested after recommending
PS-160 ARM, but SQL does not establish which SKU was provisioned.

## Findings

All **255 matched cases had identical visible plan signatures**: node/provider
sequence, Top K, Scoring, Elided Terms and Join Type. Costs, execution times,
page counters and opaque internal algorithms are outside that equivalence.

At 50,000 rows, an indexed ID filter selecting 10% of documents used a ranked
text scan for LIMIT 10, but a Conjunction Scan with Index TID Probe and Sort
for LIMIT 100. At 10,000 rows, both limits used the conjunction strategy.
Both machines made the same choices. This is evidence that the observed
boundary depends on corpus size and LIMIT, not just filter selectivity.

With the explicitly matched work_mem settings, the 50,000-row tenant-filter
query at LIMIT 1000 wrote 115 temporary blocks at 64 KiB on each machine, and
zero at 2 MiB and 8 MiB. The plan shape remained the same. Temporary block
counts use the maximum inclusive node counter, not the sum across nested nodes.
Dense-term elision and generic prepared ranked scans also had matching visible
signatures.

## Interpretation and next experiment

The biggest complete relation was only 8,126,464 bytes, with a 1,826,816-byte
TIN index, identically sized on both databases. It fits within even the smaller
server's shared buffers. This experiment therefore does **not** establish
hardware independence, capacity, a performance ratio, or memory-pressure
behavior. Each case ran once in a fixed order; execution times are observations,
not repeated throughput measurements. Most cases use each server's defaults;
only the work_mem probes explicitly match that setting.

For Stannum, first measure selective filtered ranking across corpus sizes and
LIMIT values, comparing rank-then-filter against TID intersection and scoring.
Choose strategies from Stannum's measured costs and preserve Lead correctness;
do not transplant a selectivity cutoff inferred from TIN.

A further hardware experiment needs a reproducible corpus exceeding the small
server's cache, repeated runs and bounded concurrency, with fixture loading and
cleanup designed before starting another paid session. This small-fixture
catalog is complete. A subsequent [expanded experiment](tin-expanded-experiments.md)
uses the same temporary database; consult its status before deleting the database.
