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
PostgreSQL minor version before timing. Never emulate the prior ARM image.

The 150-million-row Stack Exchange corpus is a subsequent capacity experiment,
not part of this initial baseline. Published TIN timings remain historical;
no fresh TIN binary or managed endpoint is involved. Do not infer a TIN speedup
or apply a GIN-based hardware multiplier from this baseline.
