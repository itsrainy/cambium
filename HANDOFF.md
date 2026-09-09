# Handoff: cambium benchmark suite

Written 2026-09-09 for the next orchestrator; updated the same day after the fallback work. Read this, then
`docs/METHODOLOGY.md`.

This file is session state and nothing else: what is in flight, what to do
next, and what is known to be open. The durable material lives elsewhere:
how to run the box and what has bitten people is
[docs/OPERATIONS.md](docs/OPERATIONS.md); the published numbers and the
Findings paragraph are in [docs/RESULTS.md](docs/RESULTS.md).

## Where things stand

Task 18, the published run, is **done and published** (`docs/RESULTS.md`).
The box is **stopped**. Since then, on 2026-09-09, the engine was reworked so
that a global invalidation or a whole-repository re-query is the exception,
all validated offline (no Bazel here) with `bench replay --rederive`:

- **Attribution is per occurrence, not per file** (`crates/index/src/attribution.rs`):
  a main-repo label in a workspace `.bzl` is an input of the repository the
  enclosing call names, or of what the enclosing def / rule / extension can
  instantiate; loader propagation is per imported symbol; helpers inherit
  their call sites. `local_path_override` / `local_repository` mappings are
  *exhaustive*, so a path outside them cannot be a `.bzl` inside (issue #6).
- **A changed `.bzl` the closure does not know is witnessed** against every
  `load()` in the head tree instead of ignored (issue #7); the report carries
  `bzl_witness*` stats.
- **`MODULE.bazel.lock` alone, `WORKSPACE`, `override_repo` / `inject_repo`**
  are diffed structurally (`crates/index/src/lockfile.rs`, `workspace.rs`).
- **Starlark build settings in a bazelrc** seed the setting's node instead of
  going global, unless a `.bzl` names it, it is `--config`-scoped, or it lives
  in an external repository (`crates/engine/src/flags.rs`).
- **Defaults**: root `BUILD` and `third_party/**/*.bzl` are no longer
  invalidators (`REPO.bazel` is); test-selection flags are allowlisted;
  `[baseline]` and `[limits]` are out of the config hash; indexes are
  zstd-compressed at rest (raw ones still read).
- **Subject invalidators** for apollo, sourcegraph and xla are `[]`: every
  file they caught is reached by label or not read by Bazel. **Unconfirmed on
  the box** — the recall oracle is the gate for this, and for everything
  above that changes what a run answers.
- `cambi bench replay --rederive` (with `--repo`) recomputes the externals
  tables from the text at each change set's base, so index-time derivation
  changes are checkable offline. `REDERIVE=1 tools/bench/precheck.sh`.

Offline replay, change sets against the pin indexes, before → after:
envoy 9 → 3 cold rebuilds (the 3 are `api/bazel/api_build_system.bzl`
edits, loaded by every package: exact); apollo 12 globals → 1 global
(`.bazelrc`, unreadable offline) + 6 cold (3 are a configure `.bzl` whose
generated repository every package loads; 3 a WORKSPACE diff moving 39
repositories); sourcegraph 2 → 0 global; bazel unchanged. Envoy history (340
commits): 35 → 20 cold, 16 → 13 global.

## What to do next

1. Start the box, `launch.sh sync`, run `tools/bench/run-published.sh` for
   all five subjects (about eight hours). Compare each subject's summary line
   with `docs/RESULTS.md` before letting the run continue. The recall oracle
   must stay at 100% on the four subjects that had it; envoy's known 6
   `external_route` failures are by design.
2. Regenerate `docs/RESULTS.md`, rewrite the Findings paragraph, close #6, #7
   and #8 with the box numbers on the issue threads.
3. If a subject's recall drops, the invalidator trims in `subject_repos.toml`
   are the first suspect, then `crates/index/src/attribution.rs` (the
   `PURE_BUILTINS` list decides when an unresolved call keeps the wide
   answer).

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
- #5 sourcegraph's `go.mod` / `pnpm-lock.yaml`: superseded by the
  invalidator trim (go.mod is not read by Bazel; pnpm-lock is derived from
  WORKSPACE). Close with the box numbers.
- #6, #7: fixed offline (above); close with the box numbers.
- #8 rule-label harvesting over-claims subtrees: fixed on `main` (a label
  feeds a repository only through the exact file it names; a rule target
  reached by the reverse walk moves the repository's targets, found at walk
  time). Close with the box numbers.
- Remaining fallbacks by design: `WORKSPACE` statements the diff cannot
  classify, toolchain modules and `register_toolchains`, `--config`-scoped
  and external-repository Starlark settings, `--registry` / `--repo_env` /
  compilation flags, `.bazelversion`, `try-import`.

## Known noise

- envoy's history p50 moved between passes with the same dirty-package
  counts (118 ms on one pass, 424 ms on the published one); the difference is
  entirely `query_ms`, the one Bazel query per commit, so it is the box's
  Bazel server, not the engine. Report it as such if asked.
- envoy fails 6 change sets on recall by design: `external_route`, the
  documented round trip through `@envoy_repo` and `@quiche`.
