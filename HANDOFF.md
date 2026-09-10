# Handoff: cambium benchmark suite

Written 2026-09-10 for the next orchestrator. Read this, then
`docs/METHODOLOGY.md`.

This file is session state and nothing else: what is in flight, what to do
next, and what is known to be open. The durable material lives elsewhere:
how to run the box and what has bitten people is
[docs/OPERATIONS.md](docs/OPERATIONS.md); the published numbers and the
Findings paragraph are in [docs/RESULTS.md](docs/RESULTS.md).

## Where things stand

The confirmation run for the fallback work is **done and published**
(`docs/RESULTS.md`, commit `85cf417`): all five subjects on a wiped store,
recall 100% everywhere but envoy's three documented `external_route` sets,
rebuild and load oracles holding, no `unclassified` or `cambium_missing`
label anywhere. Issues #5, #6, #7 and #8 are closed with the numbers. The box
is **stopped**; its mirrors and store are intact under
`~/.cache/cambium/corpus` (the store was wiped before the run and now holds
this run's lineage).

It took four passes on 2026-09-10, and every earlier pass failed on something
the pin-based replay could not see. The Findings paragraph lists what was
found and fixed (commits `72bc80b` .. `8d7524a`); each has a unit test and
was confirmed with `bench replay --rederive` before the next pass, envoy's
with `--index` pointed at the base index the run itself loaded. The local
tree is 12 commits ahead of `origin/main`; nothing has been pushed.

Provenance: bazel, sourcegraph, apollo and envoy are from the pass under
`bfa4624`; xla from a pass under `8d7524a` alone, on its own wiped store.
The replay of the other four under `8d7524a` with `--rederive` changes no
change-set answer (apollo's one moved set is a cold rebuild either way) and
moves history commits only by pin-versus-base drift. If a clean single-binary
run is ever wanted, `tools/bench/run-published.sh` on a wiped store does all
five in about eight hours.

## Workflow work, in flight

The feedback loop was the lesson of the day: unit tests and the pin replay
run in minutes, but three of the five regressions were only visible on the
box, hours in. What has landed:

- `SMOKE=1 tools/bench/run-published.sh`: every subject's fetch and build,
  three history commits and one change set with every tool and oracle, into
  `bench-results-smoke/`, in about fifteen minutes. **Run it after every
  sync, before the real driver.**
- `bench replay --rederive` is trustworthy on WORKSPACE-mode subjects now
  (`fee13a7`: an unrecorded canonical name was read as a main-repository
  alias, and sourcegraph's `deps.bzl` replayed as six cold rebuilds the run
  never made). `CAMBIUM_REDERIVE_DUMP=<dir>` keeps each re-derived index for
  `cambi index externals`, which dumps an index's repository tables and is
  how the envoy mode bug was found (diff the pin's tables against the run's
  base index's).
- The corpus attribution test (`cargo test -p cambium-index corpus_attribution
  -- --ignored --nocapture`, with `CAMBIUM_CORPUS_DIR`, `_PATHS`, `_ALIAS`,
  `_INDEX`) answers "what does the derivation say about this file in this
  tree" in twenty seconds, with a stored index's mapping when given one.

Two more were being built by subagents in worktrees when this was written;
check `git worktree list` and `git log --all --oneline | head` for their
commits, and cherry-pick them onto `main` if they are not there:

1. **Archive each change set's base index** into
   `<results>/<subject>/bases/<base>.bin`, have `bench replay` use it for that
   change set instead of the pin, and have `precheck.sh` pull the directory.
   This is the change that would have caught the envoy mode bug offline.
2. **Persist every tool's full label set** and the per-label classification
   per change set, with `cambi bench labels` to read them back. Yesterday's
   990-label and 165-label disagreements could only be examined from 20
   examples.

Still proposals: recording the head universe rows so the rebuild oracle's
index comparison can run offline, and synthetic CI subjects for the shapes
that bit (a constant partial package; a tree carrying both WORKSPACE and an
inert `MODULE.bazel`; a repository rule declared directly in WORKSPACE).

## What to do next

1. Land the two subagent commits, run `cargo test --release` once, push.
2. Next engine change: `tools/bench/precheck.sh` with `REDERIVE=1`, then
   `SMOKE=1` on the box, then the full driver. Compare each subject's summary
   line against `docs/RESULTS.md` before letting the run continue.
3. The precision question from the 50-pair measurement against the internal
   tool (a whole-graph answer where it returned 288k of 900k targets): the
   next lever is symbol-level attribution of a changed `.bzl` to packages
   and targets, the same idea as #6 applied on the package side. Ask that
   agent for `--explain` on the pair and the direct-reason histogram first.

## Open issues

- #1 live TUI for the benchmark matrix (`bench watch`).
- #2 CLI surface: intent-first verbs, diagnostics with codes, one output
  contract.
- Remaining fallbacks by design: `WORKSPACE` statements the diff cannot
  classify, toolchain modules and `register_toolchains`, `--config`-scoped
  and external-repository Starlark settings, `--registry` / `--repo_env` /
  compilation flags, `.bazelversion`, `try-import`; and the `max_dirty_packages`
  overflow, which keeps the lineage and re-queries `//...` (envoy: 7 change
  sets, 37 history commits; apollo: 3 and 4).

## Known noise

- envoy fails 3 change sets on recall by design: `external_route`, the
  documented round trip through `@quiche`. Set 37 lists 25 of its 36 missing
  labels as probed; the other 11 are the same quic targets past the
  25-probe diagnosis cap (`MAX_ROUTE_PROBES`).
- History replays of envoy and sourcegraph differ from the recorded run by
  a few packages on a handful of commits; that is the pin-versus-base drift
  the replay names, not an engine change. Change-set replays are exact.
- The box's `bazel --version` probe returned nothing for this run (fixed in
  `85cf417`); the report reads the pin's `.bazelversion` instead.
