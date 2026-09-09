#!/bin/bash
# Launches (or reports) the benchmark box. One instance, tagged
# cambium=bench, in us-east-2; re-running when it exists prints its address.
#
#   tools/ec2/launch.sh              c7i.4xlarge, 500 GB gp3
#   SIZE=c7i.8xlarge DISK=1000 tools/ec2/launch.sh
#   tools/ec2/launch.sh sync           copy this working tree over and build on the box
#   tools/ec2/launch.sh stop|start|terminate|ssh
set -euo pipefail
R=${AWS_REGION:-us-east-2}
SIZE=${SIZE:-c7i.4xlarge}
DISK=${DISK:-500}
# The AWS resources predate the corpus->bench rename and keep their old names:
# a key pair and a security group cannot be renamed in place. Only the words
# inside the repository were swept. The instance carries both `cambium` tags.
KEY=${KEY:-cambium-corpus}
PEM=${PEM:-$HOME/.ssh/cambium-corpus.pem}
SG_NAME=${SG_NAME:-cambium-corpus}
HERE=$(cd "$(dirname "$0")" && pwd)

find_instance() {
  aws ec2 describe-instances --region "$R" \
    --filters Name=tag:cambium,Values=bench,corpus "Name=instance-state-name,Values=pending,running,stopping,stopped" \
    --query 'Reservations[0].Instances[0].[InstanceId,State.Name,PublicDnsName]' --output text 2>/dev/null | grep -v None || true
}

# The instance the subcommand needs, or a clear failure instead of an `aws`
# call with an empty --instance-ids.
need_instance() {
  read -r ID STATE HOST <<<"$(find_instance)"
  [ -n "${ID:-}" ] || { echo "no cambium benchmark instance in $R; run $0 launch" >&2; exit 1; }
}

case "${1:-launch}" in
  ssh)
    need_instance
    exec ssh -i "$PEM" -o StrictHostKeyChecking=accept-new "ubuntu@$HOST" "${@:2}" ;;
  stop|start|terminate)
    need_instance
    aws ec2 "$1-instances" --region "$R" --instance-ids "$ID" --query "*[0].[InstanceId,CurrentState.Name]" --output text ;;
  launch)
    EXISTING=$(find_instance)
    if [ -n "$EXISTING" ]; then echo "already exists: $EXISTING"; exit 0; fi
    SG=$(aws ec2 describe-security-groups --region "$R" --filters "Name=group-name,Values=$SG_NAME" --query 'SecurityGroups[0].GroupId' --output text)
    AMI=$(aws ec2 describe-images --region "$R" --owners 099720109477 \
      --filters "Name=name,Values=ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*" Name=state,Values=available \
      --query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text)
    ID=$(aws ec2 run-instances --region "$R" --image-id "$AMI" --instance-type "$SIZE" \
      --key-name "$KEY" --security-group-ids "$SG" \
      --block-device-mappings "[{\"DeviceName\":\"/dev/sda1\",\"Ebs\":{\"VolumeSize\":$DISK,\"VolumeType\":\"gp3\",\"Throughput\":500,\"Iops\":6000,\"DeleteOnTermination\":true}}]" \
      --user-data "file://$HERE/bootstrap.sh" \
      --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=cambium-bench},{Key=cambium,Value=bench}]" \
      --query 'Instances[0].InstanceId' --output text)
    echo "launched $ID ($SIZE, ${DISK} GB, $AMI)"
    aws ec2 wait instance-running --region "$R" --instance-ids "$ID"
    HOST=$(aws ec2 describe-instances --region "$R" --instance-ids "$ID" --query 'Reservations[0].Instances[0].PublicDnsName' --output text)
    echo "running: ssh -i $PEM ubuntu@$HOST"
    echo "machine bootstrap: tools/ec2/launch.sh ssh tail -f /var/log/cambium-bootstrap.log (ends with MACHINE READY)"
    echo "then: tools/ec2/launch.sh sync   # copies this tree and builds cambi, bazel-diff-rust, tango" ;;
  sync)
    # The repository is private; the box gets the working tree, not a clone.
    need_instance
    ROOT=$(cd "$HERE/../.." && pwd)
    # No `2>/dev/null` on either side: under pipefail a tar or ssh failure
    # has to be visible, or a half-copied tree builds on the box.
    # COPYFILE_DISABLE keeps macOS AppleDouble `._*` files out of the tar. The
    # `git clean -fd` after extraction removes files the working tree no longer
    # has: a module that became a directory once left its old `.rs` beside it,
    # the build failed, and a run went on with the previous binary. Ignored
    # paths (target, bench-results, the caches) are untouched by it.
    COPYFILE_DISABLE=1 tar -C "$ROOT" -czf - --exclude=./target --exclude='./bench-results*' \
      --exclude='./corpus-results*' --exclude=./.claude --exclude=./.git/worktrees . \
      | ssh -i "$PEM" -o StrictHostKeyChecking=accept-new "ubuntu@$HOST" 'mkdir -p ~/cambium && tar -xzf - -C ~/cambium && git -C ~/cambium clean -fdq'
    ssh -i "$PEM" "ubuntu@$HOST" 'cat > ~/finish-bootstrap.sh <<"EOS"
set -euxo pipefail
trap '"'"'echo "BOOTSTRAP FAILED"'"'"' ERR
source ~/.cargo/env
export PATH=/usr/local/go/bin:$PATH
cd ~/cambium
cargo build --release
mkdir -p ~/.cache/cambium/bin
ln -sf /usr/local/bin/bazelisk ~/.cache/cambium/bin/bazelisk
tools/bazel-diff/fetch.sh
tools/tango/fetch.sh
echo "BOOTSTRAP DONE"
EOS
rm -f ~/finish-bootstrap.log; nohup bash ~/finish-bootstrap.sh > ~/finish-bootstrap.log 2>&1 &'
    # Block until the build is done, and fail loudly: a sync that returns
    # before the build has finished is how a run started on a stale binary.
    echo "synced; building on the box (tools/ec2/launch.sh ssh tail -f finish-bootstrap.log)"
    # The previous sync's log is removed before the new build starts, so a
    # DONE line here is this build's; without that the wait once returned
    # on the old line and the driver's preflight had to refuse the binary.
    while ! ssh -i "$PEM" "ubuntu@$HOST" 'grep -qE "BOOTSTRAP (DONE|FAILED)" ~/finish-bootstrap.log' 2>/dev/null; do sleep 20; done
    if ssh -i "$PEM" "ubuntu@$HOST" 'grep -q "BOOTSTRAP FAILED" ~/finish-bootstrap.log'; then
      echo "BUILD FAILED on the box:" >&2
      ssh -i "$PEM" "ubuntu@$HOST" 'grep -E "^error|BOOTSTRAP FAILED" -A4 ~/finish-bootstrap.log | tail -30' >&2
      exit 1
    fi
    ssh -i "$PEM" "ubuntu@$HOST" 'cd ~/cambium && echo "box: $(./target/release/cambi --version) tree $(git rev-parse --short HEAD)"' ;;
  *) echo "usage: $0 [launch|sync|ssh|stop|start|terminate]"; exit 1 ;;
esac
