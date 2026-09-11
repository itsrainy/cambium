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
#   SMOKE=1 tools/bench/run-published.sh
#
# SMOKE=1 is the fifteen-minute pass to run before the eight-hour one: every
# subject's fetch and build, a three-commit history walk with the rebuild
# oracle, and one change set with every tool and both oracles, into
# bench-results-smoke/ so the real run neither resumes nor overwrites it.
# Two of the confirmation runs of 2026-09-10 lost hours to a refusal
# (a partial pin index) that this would have shown in the first minutes.
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
# Two binaries: the product that is measured, and the harness that measures it.
# `CAMBI` is exported because the harness reads it when no --cambi is given.
CAMBI=${CAMBI:-./target/release/cambi}
export CAMBI
CAMBIUM_BENCH=${CAMBIUM_BENCH:-./target/release/cambium-bench}

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
echo "=== harness $($CAMBIUM_BENCH --version) ==="

SUBJECTS=${SUBJECTS:-"bazel sourcegraph apollo envoy xla"}
STEPS=${STEPS:-"fetch build history changes report"}
RESTART_CHANGES=${RESTART_CHANGES:-0}
RESTART_HISTORY=${RESTART_HISTORY:-0}
SMOKE=${SMOKE:-0}
results=()
if [ "$SMOKE" = 1 ]; then
  results=(--results-dir "$PWD/bench-results-smoke")
  echo "=== SMOKE: results in bench-results-smoke/, 3 history commits, 1 change set per subject ==="
fi

step() { echo "=== $(date -u +%H:%M:%S) $* ==="; }
wants() { case " $STEPS " in *" $1 "*) return 0;; *) return 1;; esac; }

for S in $SUBJECTS; do
  if wants fetch; then
    step "$S fetch"
    $CAMBIUM_BENCH --log info subjects fetch "$S" "${results[@]}"   || { echo "FAILED $S fetch"; continue; }
  fi
  if wants build; then
    step "$S build"
    $CAMBIUM_BENCH --log info subjects build "$S" "${results[@]}"   || { echo "FAILED $S build"; continue; }
  fi
  if wants history; then
    step "$S history"
    history_args=(--oracle rebuild "${results[@]}")
    [ "$RESTART_HISTORY" = 1 ] && history_args+=(--restart)
    [ "$SMOKE" = 1 ] && history_args+=(--commits 3 --restart)
    $CAMBIUM_BENCH --log info history "$S" "${history_args[@]}" || echo "FAILED $S history"
  fi
  if wants changes; then
    step "$S changes"
    changes_args=(--tools cambium,bazel-diff,tango --oracle rdeps,load "${results[@]}")
    [ "$RESTART_CHANGES" = 1 ] && changes_args+=(--restart)
    [ "$SMOKE" = 1 ] && changes_args+=(--change-sets 1 --restart)
    $CAMBIUM_BENCH --log info changes "$S" "${changes_args[@]}" || echo "FAILED $S changes"
  fi
  if wants report; then
    step "$S report"
    $CAMBIUM_BENCH report "$S" "${results[@]}" || echo "FAILED $S report"
  fi
done

step "final report"
$CAMBIUM_BENCH report "${results[@]}"
if [ "$SMOKE" = 1 ]; then echo "SMOKE RUN DONE"; else echo "PUBLISHED RUN DONE"; fi
