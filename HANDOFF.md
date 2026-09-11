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
was confirmed with `cambium-bench replay --rederive` before the next pass, envoy's
with `--index` pointed at the base index the run itself loaded. Everything
is pushed; keep it that way, commit and push after every green test run.

Provenance: bazel, sourcegraph, apollo and envoy are from the pass under
`bfa4624`; xla from a pass under `8d7524a` alone, on its own wiped store.
The replay of the other four under `8d7524a` with `--rederive` changes no
change-set answer (apollo's one moved set is a cold rebuild either way) and
moves history commits only by pin-versus-base drift. If a clean single-binary
run is ever wanted, `tools/bench/run-published.sh` on a wiped store does all
five in about eight hours.

## The surface split, landed (issue #2, step 5)

The harness left `cambi`. `bench`, `replay`, `labels`, `report`, `subjects`
and `synth` are `cambium-bench` now (`crates/bench-cli`), and `cambi --help`
lists exactly the eight verbs `docs/CLI.md` settles on. Two things follow for
anyone driving a run:

- **Build both binaries.** `cargo build --release -p cambium-cli -p
  cambium-bench-cli`. `tools/bench/run-published.sh` and `precheck.sh` do it
  and export `CAMBI` for the harness; a hand-run `cambium-bench` needs
  `--cambi <path>`, `$CAMBI`, or a `cambi` on `PATH`, and says so at once
  rather than an hour in.
- **The numbers now include the process.** The harness measures cambium by
  running `cambi` — `index build`, `affected --format json`, `index publish`
  — so a wall time carries process start and JSON serialisation. Each result
  also records the engine's own `timings_ms.total` (`cold_engine_ms`,
  `warm_engine_ms`, `engine_total_ms`) and the `cambi --version` string of
  the binary that produced it. RESULTS.md still quotes the wall times only:
  showing both columns needs a new aggregate in `ChangesSummary` and a fresh
  run, so the published numbers were left alone. Expect the next run's warm
  numbers to sit slightly above the published ones for that reason, and say
  so when publishing them.

## Workflow work, landed 2026-09-10 (evening)

All five proposals are in: `SMOKE=1`, `cambi inspect externals`,
`CAMBIUM_REDERIVE_DUMP`, base-index archive (`bases/`) and label sets
(`bench labels`), the head-index archive with `--head-check`, and the e2e
tests for the permanently-unloadable package, the inert `MODULE.bazel` in
WORKSPACE mode (both `.bazelrc` forms) and the repository rule declared in
WORKSPACE itself. Two pre-existing `e2e_bazel` failures on this laptop
(`a_bzl_edit_reaches_every_package_that_loads_it`,
`an_external_repository_edit_reaches_only_its_consumers`) predate all of it
and reproduce on unmodified HEAD; unverified whether it is the local bazelisk
or a regression — the nightly workflow's `e2e` job is the arbiter (`gh workflow run nightly` runs it on demand).

## Workflow work, as proposed

The feedback loop was the lesson of the day: unit tests and the pin replay
run in minutes, but three of the five regressions were only visible on the
box, hours in. What has landed:

- `SMOKE=1 tools/bench/run-published.sh`: every subject's fetch and build,
  three history commits and one change set with every tool and oracle, into
  `bench-results-smoke/`, in about fifteen minutes. **Run it after every
  sync, before the real driver.**
- `cambium-bench replay --rederive` is trustworthy on WORKSPACE-mode subjects now
  (`fee13a7`: an unrecorded canonical name was read as a main-repository
  alias, and sourcegraph's `deps.bzl` replayed as six cold rebuilds the run
  never made). `CAMBIUM_REDERIVE_DUMP=<dir>` keeps each re-derived index for
  `cambi inspect externals`, which dumps an index's repository tables and is
  how the envoy mode bug was found (diff the pin's tables against the run's
  base index's).
- The corpus attribution test (`cargo test -p cambium-index corpus_attribution
  -- --ignored --nocapture`, with `CAMBIUM_CORPUS_DIR`, `_PATHS`, `_ALIAS`,
  `_INDEX`) answers "what does the derivation say about this file in this
  tree" in twenty seconds, with a stored index's mapping when given one.

Two more landed the same day, from subagents, and are on `main`:

1. **Each change set's base index is archived** in
   `<results>/<subject>/bases/<base>.bin` (`7b5c535`, `3a188ca`); `bench
   replay` uses it for that change set instead of the pin, says so per change
   set and in the `--all` summary, and the pin-drift guards go silent for it;
   `precheck.sh` pulls the directory. This is the change that would have
   caught the envoy mode bug offline. The first run to produce them is the
   next one.
2. **Every tool's full label set and the per-label classification** are kept
   per change set under `<results>/<subject>/changes/labels/`, with `cambi
   bench labels <subject> --change-set base..head [--tool] [--class]
   [--side] [--diff]` to read them back (`1564850`).

Still proposals: recording the head universe rows so the rebuild oracle's
index comparison can run offline, and synthetic CI subjects for the shapes
that bit (a constant partial package; a tree carrying both WORKSPACE and an
inert `MODULE.bazel`; a repository rule declared directly in WORKSPACE).

## What to do next

1. Next engine change: `tools/bench/precheck.sh` with `REDERIVE=1`, then
   `SMOKE=1` on the box, then the full driver. Compare each subject's summary
   line against `docs/RESULTS.md` before letting the run continue.
2. The 814k-target answer on the 900k-target repository: `0e92cc8` did not
   move it (issue #9, measured cold: bit-identical, plus three new
   universe-wide `Prefix` rows). `a723a4c` is the second attempt — a file the
   fixpoint learns nothing about no longer takes the universe as its fallback
   unless something in it is unreadable, dead scopes contribute nothing, and
   a format-string label on the fallback makes no prefix. The issue comment
   lists the four re-measurements asked for; if the tools-directory
   workspace `.bzl` is still at universe, the new `debug` line names the
   def and the callee that made it so.
3. The offline rebuild oracle (`cambium-bench replay --head-check`, `dd7dc28`) has
   no archive to check yet: `heads/<pin>.bin` appears on the next history
   run. The three box-only shapes now have real-Bazel e2e tests
   (`77fbb32`), run by the nightly workflow's `e2e` and `bazel-version-matrix` jobs.

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
