# x86 devboxes

`benchmarks/aws/devbox.sh` runs one command on a small, cheap x86 AWS box
(Sapphire Rapids: AVX2 and AVX-512 run for real, perf has hardware
counters) and copies the results home. It is for lanes that need x86 test
runs, microbenchmarks or the offline replay without a full campaign host.
Each call launches its own box, so several can run at once.

```sh
export AWS_PROFILE=springbird-development
# tests at every forced kernel level
benchmarks/aws/devbox.sh --branch tinshape/x86-ipc \
  --cmd 'for l in scalar avx2 avx512; do STANNUM_KERNELS=$l STANNUM_EXPECT_KERNELS=$l cargo test --release -p engine -p segment || exit 1; done'
# the 150M replay under perf stat (--dump: 64 GiB r7i.2xlarge, dump in /data)
benchmarks/aws/devbox.sh --branch my-branch --dump --cmd 'cargo build --release -p bench --bin tnsreplay &&
  perf stat -e instructions:u,cycles:u,branch-misses:u -o $OUT/perf.txt -- target/release/tnsreplay \
  --dump $DATA/dump150m --trace $DATA/trace.tsv --expect $DATA/pg-ranked.tsv --style conjunction --limit 400 --repeat 3 > $OUT/replay.txt'
# A/B: two trees on one box, SRC1 and SRC2, CMD in a file
benchmarks/aws/devbox.sh --branch base --branch candidate --dump --cmd-file benchmarks/aws/devbox-replay-ab.sh
# build options of one tree: plain, PGO, x86-64-v3/v4 + PGO, PGO + BOLT (an hour: on-demand)
benchmarks/aws/devbox.sh --branch my-branch --dump --on-demand --cmd-file benchmarks/aws/devbox-bolt-variants.sh
benchmarks/aws/devbox.sh --list                # live devboxes
benchmarks/aws/devbox.sh --terminate ID|all    # kill one, or every devbox
benchmarks/aws/devbox.sh --ledger              # instance-hours and cost so far
```

Options: `--type c7i.2xlarge` (default; 8 vCPU, 16 GiB), `c7i.xlarge`
(4 vCPU) or `r7i.2xlarge` (8 vCPU, 64 GiB; the default with `--dump`,
because `tnsreplay` reads all 37.5 GB of segments into memory). `--hours N`
(1 to 3, default 2) is a hard limit: the box runs `shutdown -h +N*60` with
instance-initiated shutdown = terminate. `--env K=V` sets variables for CMD.
`--on-demand` skips spot: on 2026-10-12 two r7i.2xlarge spot boxes in a
row were reclaimed 15 and 55 minutes in, so take on-demand for sessions
that cannot be lost (a command file can also upload `$OUT` as it goes, as
`devbox-bolt-variants.sh` does, so a reclaim keeps what was done).
`--spot-only` refuses the on-demand fallback instead (the call fails when
no AZ has spot capacity), for a session with a small budget.

## What a call does

1. Creates the `stannum-devbox` stack on first use (`devbox-stack.json`):
   an instance role with `AmazonSSMManagedInstanceCore` and S3 access
   limited to `replay-dumps/` (read) and `devbox-runs/` (read/write) of the
   corpus bucket, and a security group with no inbound rules in the default
   VPC. It holds no instances and costs nothing while idle.
2. Uploads each `--branch` as a `git archive` (no credentials on the box) to
   `s3://<corpus bucket>/devbox-runs/<id>/`. Branches resolve on the
   `stannum` remote first (fetched), then locally.
3. Launches a one-time spot instance (Ubuntu 24.04 amd64) in the AZ with
   the lowest spot price, trying the next AZs and then on-demand if spot has
   no capacity. Tags: `stannum-devbox=<id>` on the instance and its volume.
4. Through SSM (no SSH): installs build tools, perf (`linux-tools`) and the
   toolchain in `rust-toolchain.toml`, unpacks the sources at
   `/work/s1-<branch>`, ..., syncs the dump to `/data` if `--dump` (gp3,
   90 GiB, 500 MB/s), then runs CMD in the first tree with `SRC1`, `SRC2`,
   `OUT=/work/out`, `DATA=/data`. Phases print as they happen.
5. Copies `$OUT` home to `~/stannum-lab/x86-devbox/runs/<id>/out/`
   (`cmd.log`, `box.log`, `machine.txt` with lscpu and a perf probe,
   `exit-status`, and whatever CMD wrote), terminates the box (also on
   Ctrl-C or error), deletes leftover volumes, removes the run's S3 objects
   and appends instance-hours and cost to `~/stannum-lab/x86-devbox/ledger.txt`
   (`DEVBOX_HOME` moves both).

Cost: spot in us-east-1 was about $0.18/h for c7i.2xlarge, $0.07/h for
c7i.xlarge and $0.16 to $0.22/h for r7i.2xlarge in October 2026 (on-demand
$0.357, $0.179, $0.529). Measured: engine + segment tests at four kernel
settings took 3 minutes of box time ($0.01); the dump sync takes 75 s and a
two-tree replay A/B (`devbox-replay-ab.sh`: builds, tnsef, 10 replays) 23
minutes ($0.07 with the volume). `devbox-bolt-variants.sh` (five builds,
three PGO trainings, a BOLT training, 15 replays) took 62 minutes ($0.59
on-demand with the volume).

On these VM sizes perf sees the core events (instructions, cycles,
branches, branch-misses, L1 cache events); `machine.txt` records which
work. Count user space only (`:u`) so the dump's load does not swamp the
kernel side, and run a load-only baseline (`--limit 1 --repeat 1`) to
subtract the setup.

## The replay dump in S3

`s3://springbird-dev-stannum-corpus-cache-860510875764/replay-dumps/dump150m-v6/`
holds the 150M TNS1 replay dump (`dump150m/`: 14 segments, ids.tsv,
manifest.tsv), `trace.tsv` and `pg-ranked.tsv`, 40.4 GB. It was uploaded
once from `~/stannum-lab/tinshape/phase-c-perf/` and **stays there until
deleted, at about $0.93 a month**. Delete it with
`benchmarks/aws/devbox.sh --cleanup-dump`. A dump of a newer format needs a
new prefix (and the `DUMP_PREFIX` in devbox.sh).

## Removing everything

```sh
benchmarks/aws/devbox.sh --terminate all     # if any box is alive
benchmarks/aws/devbox.sh --cleanup-dump      # the dump in S3
benchmarks/aws/devbox.sh --delete-stack      # the role, profile and security group
# equivalently: aws cloudformation delete-stack --stack-name stannum-devbox
```
