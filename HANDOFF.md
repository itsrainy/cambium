# Handoff: cambium benchmark suite

Written 2026-09-09 for the next orchestrator. Read this, then
`docs/METHODOLOGY.md`.

This file is session state and nothing else: what is in flight, what to do
next, and what is known to be open. The durable material lives elsewhere:
how to run the box and what has bitten people is
[docs/OPERATIONS.md](docs/OPERATIONS.md); the published numbers and the
Findings paragraph are in [docs/RESULTS.md](docs/RESULTS.md).

## Where things stand

Task 18, the published run, is **done and published**. `docs/RESULTS.md` is
committed with the regenerated tables and a hand-written Findings paragraph
that says which pass each subject's numbers come from. Issues #3 and #4 are
closed with evidence on the issue threads. The box is **stopped**.

The day's work, in the order it landed on `main` (all in `git log`):

- issue #4's fix (`pkg_ext`, FORMAT_VERSION 7), issue #3's refinement
  (`target_only` inputs), and the two harness tool failures diagnosed;
  FORMAT_VERSION settled at 8 after the merge;
- the change-set step resuming a previous engine's answers (fixed: the
  results file records the engine id, like the history file);
- the apollo classifier: `tool_extra_source_directory`, for Tango hashing a
  package directory that Bazel's graph has no edge to;
- the envoy load oracle (`rbuildfiles` names the whole repository under
  bzlmod; `repository_evaluation`) and `+` in `//bazel:libc++` breaking
  Bazel query syntax;
- the offline replay: `cambi bench replay`, `tools/bench/precheck.sh`, and
  the driver in the repo at `tools/bench/run-published.sh` with `SUBJECTS`,
  `STEPS`, `RESTART_HISTORY`, `RESTART_CHANGES`;
- the splice dropping every repository path mapping (`from_view` never read
  `repo_path_maps` back), the rebuild oracle comparing externals side tables,
  and `CONTENT_REVISION` in the config hash so the artifact store stops
  serving indexes the buggy splice wrote;
- a `--deleted_packages` package no longer making a spliced index partial;
- process: `cambi --version` names its commit, the driver refuses a binary
  that is not the tree or a build that did not finish, `launch.sh sync`
  blocks on the box's build and cleans stale files.

## Before merging an engine change

Run `tools/bench/precheck.sh` and paste what it said. It replays the engine's
static stages against every recorded change set of the real subjects,
offline, from the stored indexes, in minutes and with no Bazel, and exits 1
on divergence. `docs/METHODOLOGY.md`, "Validating an engine change before a
run", lists what it cannot see. Two of today's bugs were only visible with
`--index` pointed at the base index a run actually used, not the pin: the
replay reads a healthy index, a run reads whatever the store kept.

Then, on the box: `launch.sh sync` must exit 0 before anything runs, and the
driver's first log line must name the same sha as `git rev-parse --short
HEAD` there. Compare the first subject's summary line against the previous
run before letting the rest proceed.

## Provenance of the published numbers

envoy's tables are from the third pass (2026-09-09, engine id
`f8/1de01276093d1698`); the other four subjects' are from the second pass,
under their own engine ids. The offline replay shows the engine changes
between the passes did not alter their answers, and none of them has the
path mapping the splice defect dropped, so they were not re-run. If a clean
single-lineage run is ever wanted, `tools/bench/run-published.sh` with no
overrides does all five in about eight hours and every subject starts over.

## Open issues

- #1 live TUI for the benchmark matrix (`bench watch`).
- #2 CLI surface: intent-first verbs, diagnostics with codes, one output
  contract.
- #5 sourcegraph's `go.mod` / `pnpm-lock.yaml` extension inputs should be
  derived like envoy's `api/` mapping.
- **#6 (new)** a `local_path_override` mapping is exhaustive: envoy's 11
  remaining change-set cold rebuilds are all at pre-bzlmod bases where the
  WORKSPACE-era `bazel/repositories.bzl` has no attributable mapping and the
  every-repository fallback fires. `cambi bench replay envoy --all --repo
  <mirror>` reproduces them offline.
- **#7 (new)** a changed `.bzl` the load closure does not know is silently
  ignored, and the load oracle no longer covers that on
  `repository_evaluation` change sets.

## Known noise

- envoy's history p50 moved between passes with the same dirty-package
  counts (118 ms on one pass, 424 ms on the published one); the difference is
  entirely `query_ms`, the one Bazel query per commit, so it is the box's
  Bazel server, not the engine. Report it as such if asked.
- envoy fails 6 change sets on recall by design: `external_route`, the
  documented round trip through `@envoy_repo` and `@quiche`.
