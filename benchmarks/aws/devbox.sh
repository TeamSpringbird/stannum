#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# Run a command on a small, cheap x86 (Sapphire Rapids, AVX-512) AWS box and
# bring its results home: x86 tests, microbenchmarks, the offline replay
# under perf stat. One box per call; several calls can run at once.
#
#   devbox.sh --branch B [--branch B2 ...] --cmd 'CMD' | --cmd-file F
#             [--type c7i.2xlarge] [--dump] [--hours 2] [--env K=V ...]
#   devbox.sh --list | --terminate ID|all | --ledger
#   devbox.sh --cleanup-dump | --delete-stack
#
# Each --branch (a branch on the stannum remote, a local ref or a SHA) is
# uploaded as a git archive (no credentials on the box) and unpacked at
# /work/<name>; CMD runs in the first, with SRC1, SRC2, ... naming them,
# OUT=/work/out (everything there comes home) and DATA=/data. --dump syncs
# s3://<bucket>/replay-dumps/dump150m-v6/ to /data (dump150m/, trace.tsv,
# pg-ranked.tsv) on a gp3 root volume sized for it; tnsreplay reads every
# segment into memory (37.5 GB), so --dump defaults to r7i.2xlarge (64 GiB).
#
# The box is a one-time spot instance in the AZ with the lowest spot price
# (on-demand if no AZ has spot capacity), tagged stannum-devbox=<id>, with
# shutdown -h +<hours> and instance-initiated shutdown = terminate, so it
# dies on its own even if this script does. The script terminates it when
# the command ends (or on Ctrl-C), checks its volumes are gone, deletes the
# run's S3 objects and appends instance-hours and cost to the ledger.
# Results land in $DEVBOX_HOME/runs/<id>/ (cmd.log, box.log, machine.txt,
# exit-status and whatever CMD wrote to $OUT).
#
# Needs: aws CLI with AWS_PROFILE (default springbird-development), git,
# python3. The first call creates the stannum-devbox stack (an SSM-only
# instance role limited to two bucket prefixes, and a security group with no
# inbound rules; free while idle). See benchmarks/aws/README-devbox.md.

set -uo pipefail
export AWS_PROFILE=${AWS_PROFILE:-springbird-development} AWS_REGION=us-east-1 AWS_PAGER=
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(git -C "$HERE" rev-parse --show-toplevel)
HOME_DIR=${DEVBOX_HOME:-$HOME/stannum-lab/x86-devbox}
LEDGER=$HOME_DIR/ledger.txt
BUCKET=springbird-dev-stannum-corpus-cache-860510875764
DUMP_PREFIX=replay-dumps/dump150m-v6/
STACK=stannum-devbox
REMOTE=${DEVBOX_REMOTE:-stannum}
AMI_PARAM=/aws/service/canonical/ubuntu/server/noble/stable/current/amd64/hvm/ebs-gp3/ami-id
# us-east-1 Linux on-demand $/h (spot is billed at the market price; the
# ledger records the AZ's spot price at launch).
on_demand() { case $1 in c7i.xlarge) echo 0.1785 ;; c7i.2xlarge) echo 0.357 ;; r7i.2xlarge) echo 0.5292 ;; esac; }
TYPES="c7i.xlarge c7i.2xlarge r7i.2xlarge"

die() { echo "devbox: $*" >&2; exit 1; }
log() { echo "devbox $(date +%H:%M:%S) $*" >&2; }

branches=() envs=() cmd= cmd_file= type= dump=0 hours=2 action=run target=
while [ $# -gt 0 ]; do
  case $1 in
    --branch) branches+=("$2"); shift 2 ;;
    --cmd) cmd=$2; shift 2 ;;
    --cmd-file) cmd_file=$2; shift 2 ;;
    --type) type=$2; shift 2 ;;
    --dump) dump=1; shift ;;
    --hours) hours=$2; shift 2 ;;
    --env) envs+=("$2"); shift 2 ;;
    --list) action=list; shift ;;
    --terminate) action=terminate; target=$2; shift 2 ;;
    --ledger) action=ledger; shift ;;
    --cleanup-dump) action=cleanup-dump; shift ;;
    --delete-stack) action=delete-stack; shift ;;
    -h|--help) sed -n '5,36p' "$0"; exit 0 ;;
    *) die "unknown argument $1 (--help)" ;;
  esac
done

live_boxes() {
  aws ec2 describe-instances \
    --filters Name=tag-key,Values=stannum-devbox Name=instance-state-name,Values=pending,running,stopping,stopped,shutting-down \
    --query 'Reservations[].Instances[].[InstanceId,Tags[?Key==`stannum-devbox`]|[0].Value,InstanceType,State.Name,LaunchTime]' --output text
}

case $action in
  list) live_boxes; exit 0 ;;
  ledger) cat "$LEDGER"; exit 0 ;;
  terminate)
    ids=$(live_boxes | awk -v t="$target" '$2==t || t=="all" || $1==t {print $1}')
    [ -n "$ids" ] || die "no live devbox matches $target"
    aws ec2 terminate-instances --instance-ids $ids --query 'TerminatingInstances[].[InstanceId,CurrentState.Name]' --output text
    exit 0 ;;
  cleanup-dump)
    aws s3 rm --only-show-errors --recursive "s3://$BUCKET/$DUMP_PREFIX" && echo "removed s3://$BUCKET/$DUMP_PREFIX"
    exit 0 ;;
  delete-stack)
    [ -z "$(live_boxes)" ] || die "devboxes are still alive (--list)"
    aws cloudformation delete-stack --stack-name $STACK &&
      aws cloudformation wait stack-delete-complete --stack-name $STACK && echo "stack $STACK deleted"
    exit 0 ;;
esac

[ ${#branches[@]} -gt 0 ] || die "--branch is required"
[ -n "$cmd$cmd_file" ] || die "--cmd or --cmd-file is required"
[ -n "$cmd_file" ] && { cmd=$(cat "$cmd_file") || die "cannot read $cmd_file"; }
[ -n "$type" ] || { [ $dump = 1 ] && type=r7i.2xlarge || type=c7i.2xlarge; }
[ -n "$(on_demand "$type")" ] || die "--type must be one of: $TYPES"
case $hours in 1|2|3) ;; *) die "--hours must be 1, 2 or 3" ;; esac

# --- the stack: role, instance profile, security group --------------------
stack_output() {
  aws cloudformation describe-stacks --stack-name $STACK \
    --query "Stacks[0].Outputs[?OutputKey=='$1'].OutputValue" --output text 2>/dev/null
}
if ! aws cloudformation describe-stacks --stack-name $STACK > /dev/null 2>&1; then
  vpc=$(aws ec2 describe-vpcs --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text)
  log "creating stack $STACK in $vpc"
  aws cloudformation create-stack --stack-name $STACK --capabilities CAPABILITY_IAM \
    --template-body "file://$HERE/devbox-stack.json" \
    --parameters ParameterKey=VpcId,ParameterValue="$vpc" ParameterKey=CorpusBucket,ParameterValue=$BUCKET \
    --tags Key=stannum-devbox,Value=stack > /dev/null || die "create-stack failed"
  aws cloudformation wait stack-create-complete --stack-name $STACK || die "stack $STACK failed"
  sleep 15 # let the new instance profile propagate to EC2
fi
aws cloudformation wait stack-create-complete --stack-name $STACK 2>/dev/null
profile_arn=$(stack_output InstanceProfileArn)
sg=$(stack_output SecurityGroupId)
[ -n "$profile_arn" ] && [ "$profile_arn" != None ] || die "stack $STACK has no instance profile"

# --- the run: id, sources, box script ---------------------------------------
id=$(date +%Y%m%d-%H%M%S)-$(printf '%04x' $((RANDOM)))
dir=$HOME_DIR/runs/$id
prefix=devbox-runs/$id
mkdir -p "$dir"
log "run $id -> $dir"

sources=
n=0
for b in "${branches[@]}"; do
  n=$((n + 1))
  git -C "$REPO" fetch -q "$REMOTE" "$b" 2>/dev/null
  sha=$(git -C "$REPO" rev-parse -q --verify "$REMOTE/$b^{commit}" 2>/dev/null ||
        git -C "$REPO" rev-parse -q --verify "$b^{commit}") || die "cannot resolve $b"
  name=s$n-$(echo "$b" | tr -c 'A-Za-z0-9._\n-' '-' | cut -c1-40)
  git -C "$REPO" archive --format=tar.gz "$sha" > "$dir/src-$name.tar.gz" || die "git archive $sha"
  aws s3 cp --only-show-errors "$dir/src-$name.tar.gz" "s3://$BUCKET/$prefix/src-$name.tar.gz" || die "upload $name"
  rm "$dir/src-$name.tar.gz"
  sources="$sources $name=$sha"
  log "source SRC$n $b = $sha"
done
printf '%s\n' "$cmd" > "$dir/cmd.sh"

b64() { base64 | tr -d '\n'; }
{
  echo "RUN=$id BUCKET=$BUCKET PREFIX=$prefix DUMP=$dump DUMP_PREFIX=$DUMP_PREFIX"
  echo "SOURCES='${sources# }'"
  echo "ENVS=$(printf '%s\n' ${envs[@]+"${envs[@]}"} | b64)"
  echo "CMD=$(printf '%s\n' "$cmd" | b64)"
  cat "$HERE/devbox-box.sh"
} > "$dir/box.sh"

# --- launch: the cheapest AZ's spot, else on-demand ------------------------
disk=30
[ $dump = 1 ] && disk=90
volume="{\"VolumeSize\":$disk,\"VolumeType\":\"gp3\",\"DeleteOnTermination\":true"
[ $dump = 1 ] && volume="$volume,\"Throughput\":500,\"Iops\":4000"
volume="$volume}"
mapping="[{\"DeviceName\":\"/dev/sda1\",\"Ebs\":$volume}]"
tags="ResourceType=instance,Tags=[{Key=Name,Value=stannum-devbox-$id},{Key=stannum-devbox,Value=$id}]"
vtags="ResourceType=volume,Tags=[{Key=stannum-devbox,Value=$id}]"
userdata=$(printf '#!/bin/bash\nshutdown -h +%d\n' $((hours * 60)) | b64)
vpc=$(aws ec2 describe-security-groups --group-ids "$sg" --query 'SecurityGroups[0].VpcId' --output text)
offered=$(aws ec2 describe-instance-type-offerings --location-type availability-zone \
  --filters Name=instance-type,Values=$type --query 'InstanceTypeOfferings[].Location' --output text)
# "az subnet spotprice" lines, cheapest first
zones=$(
  aws ec2 describe-spot-price-history --instance-types $type --product-descriptions Linux/UNIX \
    --start-time "$(date -u +%Y-%m-%dT%H:%M:%S)" --query 'SpotPriceHistory[].[AvailabilityZone,SpotPrice]' --output text |
  while read -r az price; do
    printf '%s\n' $offered | grep -qx "$az" || continue
    subnet=$(aws ec2 describe-subnets --filters Name=vpc-id,Values=$vpc Name=availability-zone,Values=$az \
      Name=default-for-az,Values=true --query 'Subnets[0].SubnetId' --output text < /dev/null)
    [ "$subnet" != None ] && echo "$az $subnet $price"
  done | sort -k3 -g)
[ -n "$zones" ] || die "no AZ offers $type"

launch() { # market az subnet -> instance id
  local market=()
  [ "$1" = spot ] && market=(--instance-market-options 'MarketType=spot,SpotOptions={SpotInstanceType=one-time,InstanceInterruptionBehavior=terminate}')
  aws ec2 run-instances --image-id "resolve:ssm:$AMI_PARAM" --instance-type $type \
    --subnet-id "$3" --security-group-ids "$sg" --iam-instance-profile Arn="$profile_arn" \
    --instance-initiated-shutdown-behavior terminate --block-device-mappings "$mapping" \
    --metadata-options HttpTokens=required --user-data "$userdata" \
    --tag-specifications "$tags" "$vtags" ${market[@]+"${market[@]}"} \
    --query 'Instances[0].InstanceId' --output text 2> "$dir/launch.err" < /dev/null
}
instance= market= az= price=
while read -r z subnet spot; do
  instance=$(launch spot "$z" "$subnet") && { market=spot az=$z price=$spot; break; }
  log "spot in $z failed: $(grep -o -E '\(([A-Za-z.]+)\)' "$dir/launch.err" | head -1)"
  instance=
done <<< "$zones"
if [ -z "$instance" ]; then
  read -r z subnet _ <<< "$zones"
  instance=$(launch on-demand "$z" "$subnet") || die "on-demand launch failed: $(cat "$dir/launch.err")"
  market=on-demand az=$z price=$(on_demand "$type")
fi
launched=$(date +%s)
log "launched $instance $type $market in $az at \$$price/h, self-terminates in ${hours} h"
cat > "$dir/meta.txt" <<EOF
id=$id instance=$instance type=$type market=$market az=$az price=$price disk=$disk dump=$dump hours=$hours
launched=$(date -u +%Y-%m-%dT%H:%M:%SZ) sources=$sources
EOF
echo "$(date '+%F %T %Z') launch $id $instance $type $market $az \$$price/h disk=${disk}GiB" >> "$LEDGER"

cleaned=0
cleanup() {
  [ $cleaned = 1 ] && return; cleaned=1
  log "terminating $instance"
  aws ec2 terminate-instances --instance-ids "$instance" > /dev/null
  aws ec2 wait instance-terminated --instance-ids "$instance"
  local ended secs vols
  ended=$(date +%s); secs=$((ended - launched))
  vols=$(aws ec2 describe-volumes --filters Name=tag:stannum-devbox,Values="$id" --query 'Volumes[].[VolumeId,State]' --output text)
  for v in $(echo "$vols" | awk '$2=="available"{print $1}'); do aws ec2 delete-volume --volume-id "$v"; done
  sleep 5
  vols=$(aws ec2 describe-volumes --filters Name=tag:stannum-devbox,Values="$id" --query 'Volumes[].VolumeId' --output text)
  aws s3 rm --only-show-errors --recursive "s3://$BUCKET/$prefix/"
  python3 - "$secs" "$price" "$disk" "$dump" <<'EOF' > "$dir/cost.txt"
import sys
secs, price, disk, dump = int(sys.argv[1]), float(sys.argv[2]), int(sys.argv[3]), sys.argv[4] == "1"
h = max(secs, 60) / 3600
ebs = (disk * 0.08 + (375 * 0.04 + 1000 * 0.005 if dump else 0)) / 730 * h
print(f"{h:.3f} h x ${price:.4f} = ${h * price:.3f} + EBS ${ebs:.3f} = ${h * price + ebs:.3f}")
EOF
  echo "$(date '+%F %T %Z') end    $id $instance $(cat "$dir/cost.txt") volumes-left=[${vols}]" >> "$LEDGER"
  log "terminated; $(cat "$dir/cost.txt"); volumes left: ${vols:-none}"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# --- run through SSM ---------------------------------------------------------
for i in $(seq 1 60); do
  ping=$(aws ssm describe-instance-information --filters Key=InstanceIds,Values="$instance" \
    --query 'InstanceInformationList[0].PingStatus' --output text 2>/dev/null)
  [ "$ping" = Online ] && break
  sleep 10
done
[ "$ping" = Online ] || die "SSM agent never came online"
script=$(b64 < "$dir/box.sh")
params=$(python3 -c 'import json,sys;print(json.dumps({"commands":["echo %s | base64 -d > /root/devbox.sh && bash /root/devbox.sh" % sys.argv[1]],"executionTimeout":[str(int(sys.argv[2])*3600)]}))' "$script" "$hours")
command=$(aws ssm send-command --instance-ids "$instance" --document-name AWS-RunShellScript \
  --comment "stannum-devbox $id" --parameters "$params" --query Command.CommandId --output text) || die "send-command failed"
log "ssm command $command"

seen=0 status=
while :; do
  sleep 20
  if aws s3 cp --only-show-errors "s3://$BUCKET/$prefix/progress.txt" "$dir/progress.txt" 2>/dev/null; then
    total=$(wc -l < "$dir/progress.txt")
    [ "$total" -gt "$seen" ] && { tail -n +$((seen + 1)) "$dir/progress.txt" | sed 's/^/  box /' >&2; seen=$total; }
  fi
  aws s3 ls "s3://$BUCKET/$prefix/DONE" > /dev/null 2>&1 && break
  status=$(aws ssm get-command-invocation --command-id "$command" --instance-id "$instance" \
    --query Status --output text 2>/dev/null)
  case $status in Success|Failed|TimedOut|Cancelled) sleep 20; aws s3 ls "s3://$BUCKET/$prefix/DONE" > /dev/null 2>&1 || break ;; esac
  state=$(aws ec2 describe-instances --instance-ids "$instance" --query 'Reservations[0].Instances[0].State.Name' --output text)
  [ "$state" = running ] || { log "instance is $state"; break; }
done

if aws s3 cp --only-show-errors "s3://$BUCKET/$prefix/results.tar.gz" "$dir/results.tar.gz" 2>/dev/null; then
  tar -xzf "$dir/results.tar.gz" -C "$dir" && rm "$dir/results.tar.gz"
  log "results in $dir/out (cmd exit $(cat "$dir/out/exit-status" 2>/dev/null))"
  tail -n 15 "$dir/out/cmd.log" >&2
else
  aws ssm get-command-invocation --command-id "$command" --instance-id "$instance" \
    --query '[Status,StandardOutputContent,StandardErrorContent]' --output text > "$dir/ssm-output.txt" 2>&1
  log "no results; SSM said $status, see $dir/ssm-output.txt"
fi
