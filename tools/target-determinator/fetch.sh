#!/usr/bin/env bash
# Fetch a pinned bazel-contrib/target-determinator and build the binary.
#
#   tools/target-determinator/fetch.sh              # clone (if needed) + build
#   CAMBIUM_TARGET_DETERMINATOR_DIR=... fetch.sh    # override the install dir
#
# Layout:
#   $DIR/src        pinned bazel-contrib/target-determinator checkout
#   $DIR/bin/target-determinator
#   $DIR/VERSION    the pinned tag + commit
#
# Bazel is needed for the build even though the build itself is `go build`:
# the Go bindings for Bazel's `analysis_v2` and `build` protos are generated,
# not checked in, so `go build` alone fails with `undefined:
# analysis.ConfiguredTarget`. Upstream's own `copy_proto_output` rules
# generate them into the source tree, which is what the loop below runs.
set -euo pipefail

TD_REPO="${TD_REPO:-https://github.com/bazel-contrib/target-determinator}"
TD_TAG="${TD_TAG:-v0.34.0}"
TD_COMMIT="${TD_COMMIT:-d4b6125546979713431e63b5c3e65810fa989446}"

DIR="${CAMBIUM_TARGET_DETERMINATOR_DIR:-$HOME/.cache/cambium/target-determinator}"
SRC="$DIR/src"
GO="${GO:-go}"
# The repository pins its own Bazel in .bazelversion, so bazelisk is preferred.
BAZEL="${BAZEL:-$(command -v bazelisk || command -v bazel)}"

mkdir -p "$DIR/bin"

if [ ! -d "$SRC/.git" ]; then
  rm -rf "$SRC"
  git clone --quiet "$TD_REPO" "$SRC"
fi

git -C "$SRC" fetch --quiet --tags origin || true
git -C "$SRC" checkout --quiet --detach "$TD_COMMIT"
# Hard reset and clean: the generated protos below land *in* the source tree,
# so a second run would otherwise build against whatever the first left.
git -C "$SRC" reset --quiet --hard "$TD_COMMIT"
git -C "$SRC" clean -qfdx

have="$(git -C "$SRC" rev-parse HEAD)"
if [ "$have" != "$TD_COMMIT" ]; then
  echo "fetch.sh: expected $TD_COMMIT, got $have" >&2
  exit 1
fi
printf 'tag=%s\ncommit=%s\nrepo=%s\n' "$TD_TAG" "$TD_COMMIT" "$TD_REPO" > "$DIR/VERSION"

cd "$SRC"
for t in //third_party/protobuf/bazel/build:copy_build \
         //third_party/protobuf/bazel/analysis:copy_analysis \
         //third_party/protobuf/bazel/stardoc_output:copy_stardoc_output; do
  echo "fetch.sh: generating protos with $t"
  "$BAZEL" run --noshow_progress --ui_event_filters=,+error,+fail "$t"
done
# The proto build is the only thing Bazel is here for, and its server would
# otherwise sit on a few GB of the benchmark box for the rest of the run.
"$BAZEL" shutdown >/dev/null 2>&1 || true

"$GO" build -o "$DIR/bin/target-determinator" ./target-determinator
echo "fetch.sh: built $DIR/bin/target-determinator ($TD_TAG / ${TD_COMMIT:0:12})"
