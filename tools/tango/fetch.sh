#!/usr/bin/env bash
# Fetch a pinned Uber Tango, graft cmd/tango-cli into it, and build the binary.
#
#   tools/tango/fetch.sh            # clone (if needed) + build
#   CAMBIUM_TANGO_DIR=... fetch.sh  # override the install dir
#
# Layout:
#   $DIR/src        pinned uber/tango checkout
#   $DIR/bin/tango-cli
#   $DIR/VERSION    the pinned tag + commit
set -euo pipefail

TANGO_REPO="${TANGO_REPO:-https://github.com/uber/tango}"
TANGO_TAG="${TANGO_TAG:-v0.0.46}"
TANGO_COMMIT="${TANGO_COMMIT:-005b0dec444d956cda42ebb62cce227362783948}"

DIR="${CAMBIUM_TANGO_DIR:-$HOME/.cache/cambium/tango}"
SRC="$DIR/src"
GO="${GO:-/usr/local/bin/go}"
command -v "$GO" >/dev/null 2>&1 || GO=go

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

mkdir -p "$DIR/bin"

if [ ! -d "$SRC/.git" ]; then
  rm -rf "$SRC"
  git clone --quiet "$TANGO_REPO" "$SRC"
fi

git -C "$SRC" fetch --quiet --tags origin || true
git -C "$SRC" checkout --quiet --detach "$TANGO_COMMIT"
git -C "$SRC" clean -qfd -e cmd

have="$(git -C "$SRC" rev-parse HEAD)"
if [ "$have" != "$TANGO_COMMIT" ]; then
  echo "fetch.sh: expected $TANGO_COMMIT, got $have" >&2
  exit 1
fi
printf 'tag=%s\ncommit=%s\nrepo=%s\n' "$TANGO_TAG" "$TANGO_COMMIT" "$TANGO_REPO" > "$DIR/VERSION"

# Apply any local patches to upstream (there are none today; keep the hook so a
# future one is recorded as a .patch rather than a hand edit).
for p in "$HERE"/*.patch; do
  [ -e "$p" ] || continue
  echo "fetch.sh: applying $(basename "$p")"
  git -C "$SRC" apply "$p"
done

# Graft the driver in as a package of the module so it can import internal/...
rm -rf "$SRC/cmd/tango-cli"
mkdir -p "$SRC/cmd/tango-cli"
cp "$HERE"/cmd/tango-cli/*.go "$SRC/cmd/tango-cli/"

cd "$SRC"
"$GO" build -o "$DIR/bin/tango-cli" ./cmd/tango-cli
echo "fetch.sh: built $DIR/bin/tango-cli ($TANGO_TAG / ${TANGO_COMMIT:0:12})"
