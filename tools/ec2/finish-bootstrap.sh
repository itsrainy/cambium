#!/bin/bash
# The repository half of the box's bootstrap, run as `ubuntu` after
# `tools/ec2/launch.sh sync` has copied the working tree over: build both
# binaries in release and fetch the two foil tools. `launch.sh` starts it
# under nohup and waits for the last line, which is `BOOTSTRAP DONE` or
# `BOOTSTRAP FAILED`; the driver refuses to run while the log says neither.
#
#   bash ~/cambium/tools/ec2/finish-bootstrap.sh > ~/finish-bootstrap.log 2>&1
set -euxo pipefail
trap 'echo "BOOTSTRAP FAILED"' ERR

CAMBIUM_REPO=${CAMBIUM_REPO:-$HOME/cambium}
CARGO_ENV=${CARGO_ENV:-$HOME/.cargo/env}
GO_BIN=${GO_BIN:-/usr/local/go/bin}
# The bazelisk the machine bootstrap installed, linked to where the harness
# looks for a Bazel that is on no PATH.
BAZELISK=${BAZELISK:-/usr/local/bin/bazelisk}

source "$CARGO_ENV"
export PATH=$GO_BIN:$PATH
cd "$CAMBIUM_REPO"
cargo build --release
mkdir -p ~/.cache/cambium/bin
ln -sf "$BAZELISK" ~/.cache/cambium/bin/bazelisk
tools/bazel-diff/fetch.sh
tools/tango/fetch.sh
echo "BOOTSTRAP DONE"
