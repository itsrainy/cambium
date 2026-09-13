#!/bin/bash
# Launches (or reports) the benchmark box. One instance, tagged
# cambium=bench, in us-east-2; re-running when it exists prints its address.
#
#   tools/ec2/launch.sh              c7i.4xlarge, 500 GB gp3
#   SIZE=c7i.8xlarge DISK=1000 tools/ec2/launch.sh
#   tools/ec2/launch.sh sync           copy this working tree over and build on the box
#   tools/ec2/launch.sh stop|start|terminate|ssh
#
# Provisioning, and nothing else: what a run needs off the box once it
# exists is `cambium-bench pull-results`.
set -euo pipefail

# ---- the box; every value overridable ----
R=${AWS_REGION:-us-east-2}
SIZE=${SIZE:-c7i.4xlarge}
DISK=${DISK:-500}
# The AWS resources predate the corpus->bench rename and keep their old names:
# a key pair and a security group cannot be renamed in place. The instance
# carries both `cambium` tags.
KEY=${KEY:-cambium-corpus}
PEM=${PEM:-$HOME/.ssh/cambium-corpus.pem}
SG_NAME=${SG_NAME:-cambium-corpus}
TAG_FILTER=${TAG_FILTER:-Name=tag:cambium,Values=bench,corpus}
INSTANCE_NAME=${INSTANCE_NAME:-cambium-bench}
# Canonical's Ubuntu 24.04 images.
AMI_OWNER=${AMI_OWNER:-099720109477}
AMI_NAME=${AMI_NAME:-ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*}
BOX_USER=${BOX_USER:-ubuntu}
# Where the tree lands on the box, relative to the user's home.
BOX_REPO=${BOX_REPO:-cambium}
BOOTSTRAP_LOG=${BOOTSTRAP_LOG:-finish-bootstrap.log}
# How often `sync` asks whether the build is done.
POLL_SECS=${POLL_SECS:-20}
HERE=$(cd "$(dirname "$0")" && pwd)

find_instance() {
  aws ec2 describe-instances --region "$R" \
    --filters "$TAG_FILTER" "Name=instance-state-name,Values=pending,running,stopping,stopped" \
    --query 'Reservations[0].Instances[0].[InstanceId,State.Name,PublicDnsName]' --output text 2>/dev/null | grep -v None || true
}

# The instance the subcommand needs, or a clear failure instead of an `aws`
# call with an empty --instance-ids.
need_instance() {
  read -r ID STATE HOST <<<"$(find_instance)"
  [ -n "${ID:-}" ] || { echo "no cambium benchmark instance in $R; run $0 launch" >&2; exit 1; }
}

box_ssh() {  # command...
  ssh -i "$PEM" -o StrictHostKeyChecking=accept-new "$BOX_USER@$HOST" "$@"
}

case "${1:-launch}" in
  ssh)
    need_instance
    exec ssh -i "$PEM" -o StrictHostKeyChecking=accept-new "$BOX_USER@$HOST" "${@:2}" ;;
  stop|start|terminate)
    need_instance
    aws ec2 "$1-instances" --region "$R" --instance-ids "$ID" --query "*[0].[InstanceId,CurrentState.Name]" --output text ;;
  launch)
    EXISTING=$(find_instance)
    if [ -n "$EXISTING" ]; then echo "already exists: $EXISTING"; exit 0; fi
    SG=$(aws ec2 describe-security-groups --region "$R" --filters "Name=group-name,Values=$SG_NAME" --query 'SecurityGroups[0].GroupId' --output text)
    AMI=$(aws ec2 describe-images --region "$R" --owners "$AMI_OWNER" \
      --filters "Name=name,Values=$AMI_NAME" Name=state,Values=available \
      --query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text)
    ID=$(aws ec2 run-instances --region "$R" --image-id "$AMI" --instance-type "$SIZE" \
      --key-name "$KEY" --security-group-ids "$SG" \
      --block-device-mappings "[{\"DeviceName\":\"/dev/sda1\",\"Ebs\":{\"VolumeSize\":$DISK,\"VolumeType\":\"gp3\",\"Throughput\":500,\"Iops\":6000,\"DeleteOnTermination\":true}}]" \
      --user-data "file://$HERE/bootstrap.sh" \
      --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$INSTANCE_NAME},{Key=cambium,Value=bench}]" \
      --query 'Instances[0].InstanceId' --output text)
    echo "launched $ID ($SIZE, ${DISK} GB, $AMI)"
    aws ec2 wait instance-running --region "$R" --instance-ids "$ID"
    HOST=$(aws ec2 describe-instances --region "$R" --instance-ids "$ID" --query 'Reservations[0].Instances[0].PublicDnsName' --output text)
    echo "running: ssh -i $PEM $BOX_USER@$HOST"
    echo "machine bootstrap: tools/ec2/launch.sh ssh tail -f /var/log/cambium-bootstrap.log (ends with MACHINE READY)"
    echo "then: tools/ec2/launch.sh sync   # copies this tree and builds cambi and the foils" ;;
  sync)
    # The repository is private; the box gets the working tree, not a clone.
    need_instance
    ROOT=$(cd "$HERE/../.." && pwd)
    # No `2>/dev/null` on either side: under pipefail a tar or ssh failure
    # has to be visible, or a half-copied tree builds on the box.
    # COPYFILE_DISABLE keeps macOS AppleDouble `._*` files out of the tar. The
    # `git clean -fd` after extraction removes files the working tree no longer
    # has, so a module that became a directory does not leave its old `.rs`
    # beside it and build the previous binary. Ignored paths (target,
    # bench-results, the caches) are untouched by it.
    COPYFILE_DISABLE=1 tar -C "$ROOT" -czf - --exclude=./target --exclude='./bench-results*' \
      --exclude='./corpus-results*' --exclude=./.claude --exclude=./.git/worktrees . \
      | box_ssh "mkdir -p ~/$BOX_REPO && tar -xzf - -C ~/$BOX_REPO && git -C ~/$BOX_REPO clean -fdq"
    # The build script travels with the tree. The previous sync's log is
    # removed before the new build starts, so a DONE line is this build's.
    box_ssh "rm -f ~/$BOOTSTRAP_LOG; CAMBIUM_REPO=~/$BOX_REPO nohup bash ~/$BOX_REPO/tools/ec2/finish-bootstrap.sh > ~/$BOOTSTRAP_LOG 2>&1 &"
    # Block until the build is done, and fail loudly: a sync that returns
    # before the build has finished is how a run started on a stale binary.
    echo "synced; building on the box (tools/ec2/launch.sh ssh tail -f $BOOTSTRAP_LOG)"
    # Anchored greps: `set -x` echoes the trap line into the log too.
    while ! box_ssh "grep -qE '^BOOTSTRAP (DONE|FAILED)' ~/$BOOTSTRAP_LOG" 2>/dev/null; do sleep "$POLL_SECS"; done
    if box_ssh "grep -q '^BOOTSTRAP FAILED' ~/$BOOTSTRAP_LOG"; then
      echo "BUILD FAILED on the box:" >&2
      box_ssh "grep -E '^error|BOOTSTRAP FAILED' -A4 ~/$BOOTSTRAP_LOG | tail -30" >&2
      exit 1
    fi
    box_ssh "cd ~/$BOX_REPO && echo \"box: \$(./target/release/cambi --version) tree \$(git rev-parse --short HEAD)\"" ;;
  *) echo "usage: $0 [launch|sync|ssh|stop|start|terminate]"; exit 1 ;;
esac
