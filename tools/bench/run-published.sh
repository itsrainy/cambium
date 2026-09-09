#!/bin/bash
# The published run (task 18). One subject at a time, cheapest first, so
# bench-results/ accumulates even if a later subject is abandoned. Every step
# is resumable: rerunning skips what state.json records as finished.
#
# This is the driver that produced the published numbers, kept in the
# repository rather than only in ~/run-published.sh on the box, so that what
# was run is reviewable and a re-run is a `tools/ec2/launch.sh sync` away.
# Its default behaviour is exactly the box's; the overrides below only narrow
# it, which is what makes re-running one subject or one step cheap:
#
#   SUBJECTS="envoy" tools/bench/run-published.sh
#   SUBJECTS="apollo" STEPS="changes report" tools/bench/run-published.sh
#   SUBJECTS="sourcegraph" STEPS=changes RESTART_CHANGES=1 tools/bench/run-published.sh
#
# RESTART_CHANGES=1 passes `--restart` to `bench changes`, which compares every
# change set again instead of resuming the ones state.json says are finished.
# That is what a fix to a *tool driver* needs, since a resumed change set keeps
# the old tool's answer; a fix to cambium itself changes the engine id and is
# started over anyway. RESTART_HISTORY=1 does the same for `bench history`,
# for a fix that changes what the walk publishes without changing the engine
# id (the splice fix of 2026-09-08 was one: same format, same config hash,
# different side tables in every index the walk wrote).
set -uo pipefail
source ~/.cargo/env
export PATH=/usr/local/go/bin:$PATH
# The mirrors, output bases and store predate the corpus->bench rename. The
# output bases have absolute paths baked in, so the directory is pointed at,
# not moved.
export CAMBIUM_SUBJECTS_DIR=${CAMBIUM_SUBJECTS_DIR:-$HOME/.cache/cambium/corpus}
cd "${CAMBIUM_REPO:-$HOME/cambium}"
CAMBI=${CAMBI:-./target/release/cambi}

# Preflight: the binary must be built from this tree, and the last sync's
# build must have finished. Twice a run went on for hours with a binary that
# was not what the log said it was (a resumed result file, then a stale build).
BIN_SHA=$($CAMBI --version | sed -n 's/.*(\([0-9a-f]*\)).*/\1/p')
TREE_SHA=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)
if [ -z "$BIN_SHA" ] || [ "$BIN_SHA" != "$TREE_SHA" ]; then
  echo "REFUSING: binary is built from '${BIN_SHA:-?}' but the tree is at '$TREE_SHA'; rebuild first" >&2
  exit 2
fi
if [ -f ~/finish-bootstrap.log ] && ! grep -q "BOOTSTRAP DONE" ~/finish-bootstrap.log; then
  echo "REFUSING: ~/finish-bootstrap.log does not end in BOOTSTRAP DONE; the last sync's build did not finish" >&2
  exit 2
fi
echo "=== $(date -u +%H:%M:%S) $($CAMBI --version) tree $TREE_SHA ==="

SUBJECTS=${SUBJECTS:-"bazel sourcegraph apollo envoy xla"}
STEPS=${STEPS:-"fetch build history changes report"}
RESTART_CHANGES=${RESTART_CHANGES:-0}
RESTART_HISTORY=${RESTART_HISTORY:-0}

step() { echo "=== $(date -u +%H:%M:%S) $* ==="; }
wants() { case " $STEPS " in *" $1 "*) return 0;; *) return 1;; esac; }

for S in $SUBJECTS; do
  if wants fetch; then
    step "$S fetch"
    $CAMBI --log info bench subjects fetch "$S"   || { echo "FAILED $S fetch"; continue; }
  fi
  if wants build; then
    step "$S build"
    $CAMBI --log info bench subjects build "$S"   || { echo "FAILED $S build"; continue; }
  fi
  if wants history; then
    step "$S history"
    history_args=(--oracle rebuild)
    [ "$RESTART_HISTORY" = 1 ] && history_args+=(--restart)
    $CAMBI --log info bench history "$S" "${history_args[@]}" || echo "FAILED $S history"
  fi
  if wants changes; then
    step "$S changes"
    changes_args=(--tools cambium,bazel-diff,tango --oracle rdeps,load)
    [ "$RESTART_CHANGES" = 1 ] && changes_args+=(--restart)
    $CAMBI --log info bench changes "$S" "${changes_args[@]}" || echo "FAILED $S changes"
  fi
  if wants report; then
    step "$S report"
    $CAMBI bench report "$S" || echo "FAILED $S report"
  fi
done

step "final report"
$CAMBI bench report
echo "PUBLISHED RUN DONE"
