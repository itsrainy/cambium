#!/bin/bash
# The published run, on the benchmark box: build both binaries, refuse a
# stale one, and hand the plan to `cambium-bench run`. The plan itself —
# which verbs, which flags, cheapest subject first, a failed subject not
# stopping the rest — lives in the binary (crates/bench/src/plan.rs) and is
# the same plan the launcher screen composes; this script only carries the
# box's environment and turns its knobs into flags.
#
#   tools/bench/run-published.sh
#   SUBJECTS="envoy" tools/bench/run-published.sh
#   SUBJECTS="apollo" STEPS="changes report" tools/bench/run-published.sh
#   SUBJECTS="sourcegraph" STEPS=changes RESTART_CHANGES=1 tools/bench/run-published.sh
#   SMOKE=1 tools/bench/run-published.sh
#
# SMOKE=1 is the fifteen-minute pass to run before the eight-hour one: every
# subject's fetch and build, a three-commit history walk with the rebuild
# oracle, and one change set with every tool and both oracles, into
# bench-results-smoke/ so the real run neither resumes nor overwrites it.
#
# RESTART_CHANGES=1 compares every change set again instead of resuming the
# ones state.json says are finished — what a fix to a *tool driver* needs,
# since a resumed change set keeps the old tool's answer. RESTART_HISTORY=1
# does the same for the history walk, for a fix that changes what the walk
# publishes without changing the engine id.
set -uo pipefail

# ---- the box's environment; every value overridable ----
CAMBIUM_REPO=${CAMBIUM_REPO:-$HOME/cambium}
# The mirrors, output bases and store predate the corpus->bench rename. The
# output bases have absolute paths baked in, so the directory is pointed at,
# not moved.
CAMBIUM_SUBJECTS_DIR=${CAMBIUM_SUBJECTS_DIR:-$HOME/.cache/cambium/corpus}
CARGO_ENV=${CARGO_ENV:-$HOME/.cargo/env}
GO_BIN=${GO_BIN:-/usr/local/go/bin}
# Written by tools/ec2/finish-bootstrap.sh; its last line says whether the
# last sync's build finished.
BOOTSTRAP_LOG=${BOOTSTRAP_LOG:-$HOME/finish-bootstrap.log}
# Two binaries: the product that is measured, and the harness that measures
# it. `CAMBI` is exported because the harness reads it when no --cambi is given.
CAMBI=${CAMBI:-./target/release/cambi}
CAMBIUM_BENCH=${CAMBIUM_BENCH:-./target/release/cambium-bench}
# NO_BUILD=1 skips the cargo build, for a re-run on a box whose tree has not
# moved; the sha preflight below still refuses a binary the tree outgrew.
NO_BUILD=${NO_BUILD:-0}

# ---- the run's knobs, each a flag of `cambium-bench run` ----
SUBJECTS=${SUBJECTS:-"bazel sourcegraph apollo envoy xla"}
STEPS=${STEPS:-"fetch build history changes report"}
RESTART_CHANGES=${RESTART_CHANGES:-0}
RESTART_HISTORY=${RESTART_HISTORY:-0}
SMOKE=${SMOKE:-0}

[ -f "$CARGO_ENV" ] && source "$CARGO_ENV"
export PATH=$GO_BIN:$PATH
export CAMBIUM_SUBJECTS_DIR CAMBI
cd "$CAMBIUM_REPO" || exit 2

if [ -f "$BOOTSTRAP_LOG" ] && ! grep -q "BOOTSTRAP DONE" "$BOOTSTRAP_LOG"; then
  echo "REFUSING: $BOOTSTRAP_LOG does not end in BOOTSTRAP DONE; the last sync's build did not finish" >&2
  exit 2
fi
if [ "$NO_BUILD" != 1 ]; then
  cargo build --release -p cambium-cli -p cambium-bench-cli || exit 2
fi

args=(run --require-sha "$(git rev-parse --short HEAD 2>/dev/null || echo unknown)")
for s in $SUBJECTS; do args+=(--subject "$s"); done
args+=(--steps "$(echo "$STEPS" | tr ' ' ',')")
[ "$SMOKE" = 1 ] && args+=(--smoke)
[ "$RESTART_CHANGES" = 1 ] && args+=(--restart-changes)
[ "$RESTART_HISTORY" = 1 ] && args+=(--restart-history)
exec "$CAMBIUM_BENCH" "${args[@]}"
