#!/bin/bash
# Bootstraps an Ubuntu 24.04 x86_64 benchmark box: everything a full
# `cambi bench` run needs, for every subject in subject_repos.toml.
#
# Runs as EC2 user-data (root) on first boot, then everything cambium-related
# is owned by `ubuntu`. Idempotent enough to re-run by hand:
#   sudo bash /var/lib/cloud/instance/scripts/part-001
# Progress lands in /var/log/cambium-bootstrap.log; the last line is
# `BOOTSTRAP DONE` when it is safe to log in and start a run.
set -euxo pipefail
exec > >(tee -a /var/log/cambium-bootstrap.log) 2>&1

export DEBIAN_FRONTEND=noninteractive
apt-get update
# build-essential, python3, git: Bazel and repository rules; openjdk: bazel-diff
# (the Java reference implementation is not used, but Tango and some subject
# repos want a JDK on PATH); docker: bazel_ci_rules generates RBE configs by
# running containers; the rest are what envoy's and apollo's repository rules
# shell out to.
apt-get install -y --no-install-recommends \
  build-essential git curl ca-certificates unzip zip python3 python3-pip \
  pkg-config libssl-dev clang lld cmake ninja-build autoconf automake libtool \
  patch patchelf openjdk-21-jdk-headless docker.io jq
systemctl enable --now docker
usermod -aG docker ubuntu

# Bazelisk, the one Bazel front door every subject uses.
curl -fsSL -o /usr/local/bin/bazelisk \
  https://github.com/bazelbuild/bazelisk/releases/latest/download/bazelisk-linux-amd64
chmod +x /usr/local/bin/bazelisk
ln -sf /usr/local/bin/bazelisk /usr/local/bin/bazel

# Go from upstream: uber/tango's go.mod asks for 1.25 and Ubuntu ships 1.22.
GO_VER=$(curl -fsSL 'https://go.dev/VERSION?m=text' | head -1)
curl -fsSL "https://go.dev/dl/${GO_VER}.linux-amd64.tar.gz" | tar -C /usr/local -xz
ln -sf /usr/local/go/bin/go /usr/local/bin/go
ln -sf /usr/local/go/bin/gofmt /usr/local/bin/gofmt

# Rust for cambium and bazel-diff-rust; rust-toolchain.toml picks the channel.
sudo -u ubuntu -H bash -c 'curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal'

# The repository is private, so the box cannot clone it: launch.sh copies the
# working tree over ssh after boot and runs the rest of this as `ubuntu`. This
# half only prepares the machine; the repository half is `finish-bootstrap.sh`
# in the home directory, written by launch.sh.
echo "MACHINE READY"
exit 0

sudo -u ubuntu -H bash -c '
  set -euxo pipefail
  source ~/.cargo/env
  export PATH=/usr/local/go/bin:$PATH
  cd ~/cambium
  cargo build --release
  mkdir -p ~/.cache/cambium/bin
  ln -sf /usr/local/bin/bazelisk ~/.cache/cambium/bin/bazelisk
  tools/bazel-diff/fetch.sh
  tools/tango/fetch.sh
'
echo "BOOTSTRAP DONE"
