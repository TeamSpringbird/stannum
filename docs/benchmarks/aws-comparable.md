# Comparable EC2 / PlanetScale experiment

Status: experiment completed on 2026-09-20 UTC; see the
[results and controlled OFFSET 0 reproduction](aws-comparable-results.md).
Raw evidence is exported and checksum-verified. AWS stack deletion completed;
the instance is terminated and its EBS volume is confirmed deleted. This is a read-only search experiment,
not a comparison of availability, durability, or replicated write throughput.

## Provisioning target

Create PlanetScale PS-160 ARM in AWS us-east-1, PostgreSQL 18, EBS gp3,
3,000 IOPS and 125 MiB/s. Single-node is sufficient if this size is available there.
Use 100 GiB allocated storage if selectable, to match the EC2 root/data volume;
otherwise record actual allocation and adjust or disclose the difference. Disable
storage autoscaling for the short experiment if possible. Confirm TIN is installed.

The candidate EC2 instance is r8g.large (Graviton4, 2 vCPUs, 16 GiB). The exact
Graviton generation of PS-160 is unconfirmed. Do not call these identical machines.
If PlanetScale confirms Graviton3, use r7g.large instead. Match settings from the
new server, not remembered defaults of the deleted database.

Sources: [AWS R8g specifications](https://aws.amazon.com/ec2/instance-types/r8g/),
[PlanetScale CPU architectures](https://planetscale.com/docs/postgres/cluster-configuration/cpu-architectures),
[PlanetScale storage settings](https://planetscale.com/docs/postgres/cluster-configuration).

## Owned resources and lifetime

`benchmarks/aws/stack.json` owns one VPC, subnet, route table, internet gateway,
security group with no inbound access, SSM role/profile, EC2 instance, launch template, and encrypted
gp3 root volume. There is no NAT gateway, load balancer, Elastic IP, snapshot, or
separate artifact bucket. SSM uses outbound HTTPS through an ephemeral public IPv4.
The host installs Docker and Python; PostgreSQL uses the existing benchmark image
recipe. This avoids inventing another database build harness.

All resources are removed by deleting this stack. The root volume explicitly has
DeleteOnTermination=true. An eight-hour systemd timer powers off the host, and
EC2 is configured to **terminate**, not stop, on OS shutdown. This is a secondary
backstop, not a guarantee if the guest fails to boot or run systemd. It also does
not delete the CloudFormation stack or its remaining networking/IAM resources.
Manually delete and verify the stack after exporting results. Termination destroys
unexported results. The timer starts at boot, including package installation/build time.

## Commands

Set the AWS profile explicitly after the owner selects the account; do not infer
that a production or management account is intended. No access keys belong in this
repository, cloud-init, or command transcripts. The principal needs CloudFormation,
EC2/VPC, IAM role/profile creation and PassRole, SSM parameter reads, and SSM session/
command access. Use the existing account access mechanism rather than creating keys.

```sh
export AWS_PROFILE=SELECTED_PROFILE
export AWS_REGION=us-east-1
export BENCH_STACK=stannum-tin-benchmark
aws sts get-caller-identity
aws cloudformation validate-template --template-body file://benchmarks/aws/stack.json
aws cloudformation deploy --stack-name "$BENCH_STACK" \
  --template-file benchmarks/aws/stack.json --capabilities CAPABILITY_IAM \
  --tags Purpose=stannum-tin-benchmark \
  --parameter-overrides InstanceType=r8g.large DiskGiB=100 LifetimeHours=8
aws cloudformation describe-stacks --stack-name "$BENCH_STACK" \
  --query 'Stacks[0].Outputs'
```

CloudFormation completion proves resource creation, not bootstrap success. Use SSM
to inspect `/var/log/cloud-init-output.log`, require
`/opt/stannum-benchmark/bootstrap-ready`, and check the expiry timer before loading data.
Use SSM port forwarding for PostgreSQL, bound only to localhost on the host. No
public PostgreSQL or SSH port is needed. Check current hourly instance, EBS and
public IPv4 prices in the selected account/region before launching; export them
alongside the run manifest. There is no reserved-instance commitment.

After **copying results off the instance and verifying local checksums**:

```sh
aws cloudformation delete-stack --stack-name "$BENCH_STACK"
aws cloudformation wait stack-delete-complete --stack-name "$BENCH_STACK"
```

Before deleting, save the stack resource inventory and attached volume IDs. After
deletion, confirm the instance is terminated and those volumes are gone. If deletion
fails, inspect stack events and resolve the remaining owned resources. PlanetScale
is a separate resource: the user deletes that database after results are exported.

## Measurement gates

1. Record AMI ID, CPU model, kernel, EC2 SKU, disk allocation/IOPS/throughput, PG full
   version, extension versions, immutable Stannum commit/image digest, and build flags.
   Build only before measurements. No concurrent Rust builds or other benchmark jobs.
2. Reuse the verified 100k Wikipedia corpus; compare checksums on both systems.
   Match column types, projection, text configuration, prepared/literal mode,
   predicates, LIMIT/OFFSET, and query parameters. Start from main, with experimental
   frontier disabled. Record index build settings and actual layouts for each engine.
3. Capture pg_settings on both servers. Match shared_buffers, work_mem, JIT and
   parallelism, plus planner settings where controllable. Record any unmatched
   managed settings. VACUUM ANALYZE both datasets and record heap/index sizes and
   visibility. A PostgreSQL minor-version mismatch must be resolved or disclosed.
4. Measure AND/OR counts against GIN and the native search index on each host.
   Validate exact membership for the selected queries before timing; exclude
   incompatible tokenization/query semantics from comparative summaries.
5. Measure unfiltered and filtered ranked top-k, common/rare terms, AND/OR,
   selective and adversarial filters. Validate sampled TIN/Stannum membership,
   score bits and ordering with an explicit tie policy. Keep Lead correctness
   verification separate. GIN ranking is not a BM25-equivalent baseline.
6. Use identical `EXPLAIN (ANALYZE, BUFFERS, SETTINGS, TIMING OFF, FORMAT JSON)`
   on both systems. Save every raw plan, planning time, execution time and client
   elapsed time separately. Both server_times.py and tin_experiments.py now use this instrumentation
   policy; preserve collector revisions because historical runs used different options.
7. Start with a warmed, single-client run: five warmups then at least 20 retained
   repetitions per query, alternating query order, across three independent rounds.
   Report medians and spread. Do not label warmed runs as cold-cache tests. Managed
   OS caches cannot necessarily be cleared in a comparable way.
8. Only then run concurrent throughput with a separate client host or a verified
   non-bottlenecking client. Client/network limits must not masquerade as database
   limits. Reuse the published-trace driver rather than building another load harness.

GIN on both hosts is a diagnostic control, not a universal hardware correction
factor. Report outcomes for these exact configurations, including unmatched factors.
A first session targets 2–4 hours after image/data setup, with eight-hour expiry;
stop early if correctness or configuration gates fail. Export raw artifacts after
each round, not only at the end.

Lifecycle references:
[AWS instance shutdown behavior](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/Using_ChangingInstanceInitiatedShutdownBehavior.html),
[CloudFormation instance properties](https://docs.aws.amazon.com/AWSCloudFormation/latest/TemplateReference/aws-resource-ec2-instance.html),
[Ubuntu AMI parameters](https://documentation.ubuntu.com/aws/aws-how-to/instances/build-cloudformation-templates/).

## First deployment record

Stack: `stannum-tin-benchmark`. Instance: `i-0fe89ebbba95cf4e3`.
Expiry verified as 2026-09-20 09:22:08 UTC (05:22:08 America/New_York).
AWS Pricing API quoted Linux shared-tenancy r8g.large at $0.11782/hour in
us-east-1, excluding EBS, public IPv4 and transfer. Local ignored artifacts under
`benchmarks/results/aws-comparable` retain the pricing response and resource inventory.

After bootstrap, the SSM command executes `build-image.sh` as a transient
`stannum-image-build` service. It builds the existing Docker recipe at main commit
`ab1e6db87e7c5bafbfc5c121ac66d879d48bbd3b` and records source/image provenance.
`start-postgres.sh` runs as `stannum-postgres-start`, waits for successful build
artifacts, then starts PostgreSQL and creates the Stannum extension. Initial memory
settings (2 GiB shared_buffers, 2 MiB work_mem) are provisional pending live TIN
settings. The container uses the host CPU/memory budget, a 3 GiB shared-memory
mount, and a Docker volume on the root gp3 disk. Localhost-only trust authentication
is for this disposable fixture; SSM access is privileged and no database port is public.

Inspect build logs using `journalctl -u stannum-image-build`; require
`/opt/stannum-benchmark/postgres-ready.txt` before declaring PostgreSQL ready.
Source build and PostgreSQL start scripts are one-shot scripts, not idempotent
reconfiguration tools. Match the remote minor version before beginning measurements.
