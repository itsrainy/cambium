#!/usr/bin/env bash
# Fetches and builds bazel-diff at a pinned commit, so every comparison in the
# benchmark's real subjects and its synthetic ones run against the same code.
#
#   tools/bazel-diff/fetch.sh [--kotlin] [--dir DIR]
#
# Produces:
#   $DIR/src/                        the checkout at $BAZEL_DIFF_COMMIT
#   $DIR/bin/bazel-diff-rust         the Rust binary, built with cargo
#   $DIR/bin/bazel-diff_deploy.jar   the Kotlin jar (release asset), with --kotlin
#   $DIR/VERSION                     "<commit> <tag>", recorded in results
#
# DIR defaults to $CAMBIUM_BAZEL_DIFF_DIR, then ~/.cache/cambium/bazel-diff.
# The Rust build needs only cargo (protoc is vendored by the crate's build
# script); the Kotlin jar is downloaded rather than built, because building it
# needs Bazel and a JDK and running it needs a JRE. Kotlin is optional and off
# by default: the spec keeps Rust only after milestone 4.
set -euo pipefail

BAZEL_DIFF_TAG="v46.1.0"
BAZEL_DIFF_COMMIT="27ce213b8b090792e06057bff96e37bd284ece8e"
BAZEL_DIFF_REPO="https://github.com/Tinder/bazel-diff.git"

kotlin=0
dir="${CAMBIUM_BAZEL_DIFF_DIR:-$HOME/.cache/cambium/bazel-diff}"
while [ $# -gt 0 ]; do
  case "$1" in
    --kotlin) kotlin=1 ;;
    --dir) shift; dir="$1" ;;
    --dir=*) dir="${1#--dir=}" ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

mkdir -p "$dir/bin"
src="$dir/src"

if [ ! -d "$src/.git" ]; then
  git clone --quiet "$BAZEL_DIFF_REPO" "$src"
fi
git -C "$src" fetch --quiet origin "$BAZEL_DIFF_COMMIT" 2>/dev/null || git -C "$src" fetch --quiet origin
git -C "$src" checkout --quiet --detach "$BAZEL_DIFF_COMMIT"

have="$(cat "$dir/VERSION" 2>/dev/null || true)"
want="$BAZEL_DIFF_COMMIT $BAZEL_DIFF_TAG"
if [ "$have" != "$want" ] || [ ! -x "$dir/bin/bazel-diff-rust" ]; then
  echo "building bazel-diff (rust) at $BAZEL_DIFF_TAG" >&2
  # The crate asks for rust >= 1.85; our pinned toolchain qualifies, and using
  # it keeps the build reproducible across machines.
  ( cd "$src" && RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-$(sed -n 's/^channel = "\(.*\)"/\1/p' "$(dirname "$0")/../../rust-toolchain.toml")}" \
      cargo build --release --locked --bin bazel-diff )
  cp "$src/target/release/bazel-diff" "$dir/bin/bazel-diff-rust"
  echo "$want" > "$dir/VERSION"
fi

if [ "$kotlin" = 1 ] && [ ! -f "$dir/bin/bazel-diff_deploy.jar" ]; then
  echo "downloading bazel-diff_deploy.jar $BAZEL_DIFF_TAG" >&2
  curl -sSL -o "$dir/bin/bazel-diff_deploy.jar" \
    "https://github.com/Tinder/bazel-diff/releases/download/$BAZEL_DIFF_TAG/bazel-diff_deploy.jar"
fi

echo "$dir/bin/bazel-diff-rust"
