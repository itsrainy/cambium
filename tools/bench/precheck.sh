#!/bin/bash
# Check an engine change against the real corpus, offline, in minutes.
#
#   tools/bench/precheck.sh                      # every cached subject
#   SUBJECTS="envoy apollo" tools/bench/precheck.sh
#   SAVE_BASELINE=1 tools/bench/precheck.sh      # record today's answers
#   BASELINE=1 tools/bench/precheck.sh           # gate against that record
#   REDERIVE=1 tools/bench/precheck.sh           # also re-derive the externals
#                                                # tables at each base, and run
#                                                # the head check at each pin
#
# REDERIVE=1 is for a change to how the *index* derives its externals side
# tables (`cambium_index::build_repo_inputs` and what feeds it). Without it a
# replay reads the tables the run's cambium already wrote, so such a change is
# invisible. Compare a re-derived run only against another re-derived run.
# It also runs `cambium-bench replay --head-check` per subject, the one check
# here that compares against Bazel's truth rather than against what an
# earlier engine recorded.
#
# Two verbs underneath: `cambium-bench pull-results` copies each subject's
# stored index, archived bases and heads, recorded results and (unless
# PATHS_FROM_BOX=0) the diff of every change set off the box into
# `bench-precheck/` (git-ignored; a subject already there is not pulled
# again), and `cambium-bench replay --all` exits 1 on divergence. Read-only on
# the box: scp, and one `cambium-bench replay --write-paths` over the mirror.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
CACHE=${CACHE:-$ROOT/bench-precheck}
SUBJECTS=${SUBJECTS:-"bazel sourcegraph apollo envoy xla"}
# `--repo <dir>` per subject, when a local clone exists: it settles the change
# sets a replay otherwise marks indeterminate (a MODULE.bazel it cannot read).
LOCAL_MIRRORS=${LOCAL_MIRRORS:-${CAMBIUM_SUBJECTS_DIR:-$HOME/.cache/cambium/subjects}}
REDERIVE=${REDERIVE:-0}
BASELINE=${BASELINE:-0}
SAVE_BASELINE=${SAVE_BASELINE:-0}
# How to reach the box; the defaults are `cambium-bench pull-results`'s own.
PEM=${PEM:-$HOME/.ssh/cambium-corpus.pem}
export AWS_REGION=${AWS_REGION:-us-east-2}
BOX_HOST=${BOX_HOST:-}
BOX_REPO=${BOX_REPO:-cambium}
BOX_SUBJECTS=${BOX_SUBJECTS:-.cache/cambium/corpus}
PATHS_FROM_BOX=${PATHS_FROM_BOX:-1}

echo "=== building cambi and cambium-bench (release) ==="
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p cambium-cli -p cambium-bench-cli || exit 1
CAMBI=$ROOT/target/release/cambi
CAMBIUM_BENCH=${CAMBIUM_BENCH:-$ROOT/target/release/cambium-bench}
export CAMBI

pull_args=(--cache "$CACHE" --key "$PEM" --box-repo "$BOX_REPO" --box-subjects "$BOX_SUBJECTS")
[ -n "$BOX_HOST" ] && pull_args+=(--host "$BOX_HOST")
[ "$PATHS_FROM_BOX" = 1 ] || pull_args+=(--no-paths)

fail=0
for s in $SUBJECTS; do
  echo "=== $s ==="
  "$CAMBIUM_BENCH" pull-results "$s" "${pull_args[@]}" || { fail=1; continue; }
  args=("$s" --results-dir "$CACHE" --all)
  mirror=$LOCAL_MIRRORS/$s.git
  if [ -d "$mirror" ]; then
    args+=(--repo "$mirror")
    [ "$REDERIVE" = 1 ] && args+=(--rederive)
  elif [ "$REDERIVE" = 1 ]; then
    echo "  REDERIVE=1 but no mirror at $mirror; replaying the stored tables" >&2
  fi
  [ "$BASELINE" = 1 ] && args+=(--baseline "$CACHE/$s/baseline.json")
  [ "$SAVE_BASELINE" = 1 ] && args+=(--save-baseline "$CACHE/$s/baseline.json")
  "$CAMBIUM_BENCH" replay "${args[@]}" || fail=1

  # The rebuild oracle's externals half, offline. It needs the mirror for the
  # head tree's text, and it is silent about a subject whose run archived no
  # head index.
  if [ "$REDERIVE" = 1 ] && [ -d "$mirror" ]; then
    "$CAMBIUM_BENCH" replay "$s" --results-dir "$CACHE" --repo "$mirror" --head-check || fail=1
  fi
done

if [ "$fail" = 0 ]; then echo "=== PRECHECK CLEAN ==="; else echo "=== PRECHECK FOUND DIVERGENCE ==="; fi
exit "$fail"
