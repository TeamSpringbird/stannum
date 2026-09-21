# Full Wikipedia AWS crossover rerun

The previous six-query probe timed out in its correctness SQL before producing
timings/profiles. The replacement captures a fixed 1,000-row physical TID sample
once, independently tokenizes its text, and checks membership using the real
full-corpus Stannum bitmap index with a TID filter. Plain heap/TID scan plans are
rejected. The result set is bounded and the materialized millions-of-text-IDs
CTE is gone. Oracle work_mem is 256MB only inside its transaction to avoid lossy
bitmaps; timed queries retain 16MB. The sample/plan evidence is retained. This is
sampled membership verification plus full-count agreement between strategies,
not an exhaustive independent full-corpus oracle.

The corrected oracle passed all 302 queries in four local visibility phases.
The old six-query profiling runner uses the same helper now. Local campaign
scripts are snapshotted before execution, preventing live source edits from
changing later phases. Docker retains release optimization plus debug line
information for profiling.

The authorized rerun uses springbird-development/us-east-1, one i7i.8xlarge,
local NVMe, PG18.6, eight CPU/32GiB query limits, 24GiB shared buffers and a
64GiB build cap. Its eight-hour shutdown timer starts at boot and terminates the
instance. The private cached 5,032,104-row corpus is restored using a read-only
S3 prefix grant and verified against the pinned upstream checksum.

`benchmarks/aws/crossover.sh` builds from the pinned checkout, loads the full
corpus, builds one index, then runs `crossover_campaign.py`. All 302 published OR
COUNT queries run with nine alternating repetitions per strategy through four
phases: vacuumed, vacuumed repeat, mutated, revacuumed. Each phase includes full
count agreement, sampled membership checks, raw plans, resource sampling and
S3 checkpoints. Four queries (302,88,139,146) get separate perf profiles in both
modes after the first vacuumed phase and after mutations.

The table retains the original id/body layout. Mutations use deterministic
hashes of id: about 5% id=id updates, 10% whitespace body updates, and 1/101
deletes. These are not the exact local ordinal-based mutation schedule, nor its
fillfactor/payload-column layout; the distinction is deliberate and recorded.
Autovacuum is disabled to retain controlled phase boundaries. Actual row counts,
visibility maps and segment state are captured. The frozen local density rule
is evaluated without retuning; no new automatic selector is enabled. This is
an execution diagnostic, not a new 600-second concurrent capacity benchmark or
a fresh TIN measurement.

Fresh stack: stannum-count-crossover-20260921-r4. The detached controller saves
and verifies the exported archive checksum/size locally before deleting the
owned artifact bucket/stack and checking instance/EBS removal. Phase checkpoints
retain partial evidence if the hard deadline interrupts work. No new phase
starts with less than 75 minutes remaining. Failed measurements/profiles remain
failures in the receipt rather than being silently omitted. The independent
corpus cache survives temporary-resource teardown.
