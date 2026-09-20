# AWS published-reference baseline — 2026-09-20

Authorized account/profile: springbird-development, us-east-1. This is separate
from the historical ARM managed-TIN experiment and the local article fixtures.

Target: i7i.8xlarge (32 vCPUs, 256 GiB), with one of its two 3,750 GB local NVMe
instance-store devices holding Docker, corpora and results. No inbound ports;
SSM management only. An encrypted 80 GiB gp3 root is deleted on termination.
A private encrypted temporary S3 bucket exports evidence, with seven-day object
expiry. Export/download/checksum evidence before emptying the bucket and deleting
the CloudFormation stack. An eight-hour guest shutdown timer terminates the VM;
it is a backstop, not a substitute for verified stack/bucket teardown.

Pricing preflight: AWS Pricing API returned $3.0202/hour Linux On-Demand for
us-east-1 ($24.1616 for eight hours), excluding root storage, IPv4, S3 and transfer.
Save the exact price response alongside the run. No Spot interruption risk.

First campaign: full verified Wikipedia, 5,032,104 rows, disjunction COUNT(*),
three 600-second repetitions per engine, alternating Stannum/GIN order. Use the
pinned upstream driver and corpus hashes. Query containers: 8 CPUs, 32 GiB,
24 GiB shared buffers, 24 GiB maintenance_work_mem, 8 parallel workers; separate
64 GiB construction cap. Use two clients, ten-second warmup, 90 selected
same-engine membership checks on 1,000 rows, and retain all 302 timed OR forms.
Rebuild fresh containers for each run and keep builds outside timing. Counts do
not establish BM25 equivalence; the pinned Lead oracle remains separate.

The client count and warmup are explicit local protocol choices; they are not
confirmed historical PlanetScale parameters. Storage filesystem/striping, CPU
allocation and service/kernel versions must be retained, not assumed identical.
The runtime base is pinned to ParadeDB 0.25.2's amd64 PG18 image; record the actual
PostgreSQL minor version before timing. Upgrade that shared runtime to the pinned
PGDG PostgreSQL 18.6-1.pgdg13+2 package before compilation; the original image
contains 18.4. Builder and both measured engines inherit this same runtime.
Never emulate the prior ARM image.

The 150-million-row Stack Exchange corpus is a subsequent capacity experiment,
not part of this initial baseline. Published TIN timings remain historical;
no fresh TIN binary or managed endpoint is involved. Do not infer a TIN speedup
or apply a GIN-based hardware multiplier from this baseline.

## Reusable dataset cache

The September 20 campaign keeps compressed parts and the extracted CSV on its
NVMe filesystem and reuses them across all rounds. A separate private, AES256
encrypted S3 bucket preserves the Wikipedia inputs beyond EC2/stack teardown:

`s3://springbird-dev-stannum-corpus-cache-860510875764/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/wikipedia/`

It contains the pinned upstream manifests, query file, source attribution,
verification receipt, compressed parts, and uncompressed CSV. Objects expire
after 30 days; incomplete multipart uploads expire after one day. This bucket
is deliberately outside the temporary stack and holds public benchmark inputs,
not database credentials. Upload status and file hashes are recorded locally in
`benchmarks/results/aws-corpus-cache.json`; do not assume an incomplete upload
is a usable cache.

Restore from an authorized AWS identity before starting measured traffic:

```sh
aws --profile springbird-development --region us-east-1 s3 cp \
  s3://springbird-dev-stannum-corpus-cache-860510875764/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/wikipedia/ \
  /path/to/datasets/wikipedia/ --recursive
python3 benchmarks/published_dataset.py --corpus wikipedia \
  --output /path/to/datasets/wikipedia
```

The acquisition command verifies the restored CSV against the pinned upstream
SHA256 before reusing it. If cache objects have expired, use the normal upstream
acquisition path. The current EC2 role only has access to its temporary artifact
bucket; a future host needs a read-only grant to the cache prefix before using
this restore command (and uses its instance role rather than a local profile).
Fresh database loads and index construction remain part of each run's setup;
caching inputs does not replace those steps or alter timed queries.

## Active campaign amendment: concurrency sweep

After the first Stannum measurement, sampled CPU usage averaged 1.97 cores under
an eight-CPU quota. On user instruction, the remaining two paired repetitions
were superseded by a Stannum-only concurrency sweep. Finish the original
Stannum/GIN pair at two clients unchanged, then measure 4, 8, 16, and if needed
32 clients. Preserve fresh construction, 10-second warmup, 600-second timing,
all 302 OR forms, memory limits, source/image, and correctness checks.

At eight or more clients, less than 10% throughput gain over the preceding level
is a candidate plateau. Repeat the highest-throughput observed client count;
confirmation within 10% is reported as a stable candidate, not a universal
capacity bound. If throughput keeps growing through 32 clients, report that
saturation was not established. CPU utilization, throttling, memory and I/O
counters, and latency percentiles accompany each result. Do not combine these
runs into baseline-repetition averages or silently replace first-run chart data.

The replacement lifecycle controller uses the existing instance and expiry.
It leaves the in-flight first pair unchanged and exports evidence before resource
cleanup. No new run starts with fewer than 55 minutes remaining before expiry.
The sweep's exact script and decision receipt are retained in the evidence
archive, with summaries checkpointed to S3 after each run. Duplicate input.csv
files are excluded from that archive; input hashes remain in each manifest and
the exact source corpus is retained separately in the reusable cache.
