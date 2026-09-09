# Handoff: cambium benchmark suite

Written 2026-09-08 for the next orchestrator. Read this, then
`docs/METHODOLOGY.md`.

This file is session state and nothing else: what is in flight, what to do
next, and what is known to be open. The durable material has moved out of it
--- how to run the box and what has bitten people is
[docs/OPERATIONS.md](docs/OPERATIONS.md), and the first run's numbers are a
Findings paragraph in [docs/RESULTS.md](docs/RESULTS.md).

## Where things stand

Task 18 --- the published run --- has been **run once**, on the EC2 box, with
Rainy's go. It finished. It is **not published**: `docs/RESULTS.md` was
deliberately left uncommitted because two of the five subjects lost a
comparison tool to a harness failure, and the run's own gates did not notice.
The defects it exposed are fixed and pushed; the run needs doing again on top
of them.

The three fixes in `7f2c666` --- the deleted half of the rdeps oracle reading
`dirty_packages`, a failed tool no longer passing its change set, and the
`tool_missing` class --- are described in that commit and in
`crates/bench/src/classify/mod.rs`. `1562325` points `tools/ec2/launch.sh` at
the AWS resource names that actually exist. Everything before that is in `git
log` and in the previous handoff.

## Before merging an engine change

Run `tools/bench/precheck.sh` and paste what it said. It replays the engine's
static stages — the externals rules, the dirty-package set, the cold-rebuild
decision — against every recorded change set of the real subjects, offline,
from the stored indexes, in minutes and with no Bazel. It exits 1 on
divergence. `docs/METHODOLOGY.md`, "Validating an engine change before a run",
says what it cannot see; a clean precheck does not replace the run, it is what
makes starting one worth the money.

## Next: the second published run

Nothing is blocked on Rainy. The box costs money, so say you are
starting it, but the go for task 18 was given and these fixes are what it was
waiting on.

The two tool failures the last run tripped over are diagnosed and fixed; both
were misread at first, so the real causes are worth knowing:

- **apollo's Tango failures were never about worktrees.** The
  `'…/tango-worktrees/base' is not a working tree` lines were the stale-worktree
  recovery announcing itself on a *clean* registry (`git worktree
  unlock`/`remove` on an unregistered path), four lines of noise above the real
  error on every change set: `compute base graph: cannot find external
  repository //external:bazel_tools~cc_configure_extension~local_config_cc`.
  apollo has no tracked `MODULE.bazel` (Bazel 7 writes one into a checkout on
  first run, which is why the head worktree has one and every fresh base
  worktree does not), so tango-cli runs it as a WORKSPACE repo — correct, that
  is where its deps are — but Bazel 7.4.1 has bzlmod on regardless, and the
  repositories `bazel_tools`' own extensions generate have no `//external:` rule
  to hash a file target against. `tools/tango/0001-keep-going-partial-graph.patch`
  now registers those as opaque external nodes, the same treatment the patch
  already gave a repository that failed to load; the recovery only runs (and
  only speaks) when git still has an entry for the path; `main_test.go` covers
  both, run by `fetch.sh`. Verified by hand on the box: tango-cli over
  `b352846f..a6871fc6` completes with a clean stderr.
- **bazel-diff's `^D` on sourcegraph is the Aspect CLI's preset picker.**
  sourcegraph's `.bazeliskrc` routes bazelisk to `aspect/5.8.19`; bazel-diff
  passes the query as `--query_file`, and `aspect query` with no positional
  expression opens an interactive "Select a preset query" menu instead of
  passing the flag through — it reads the nulled stdin, gets EOF, prints `^D`.
  `CI=1` does not skip it. `bazelisk_env` in `crates/bench/src/checkout.rs` now
  sets `USE_BAZEL_VERSION=<.bazelversion>` and `BAZELISK_BASE_URL` to the
  official releases whenever a subject's rc points at the Aspect CLI, so every
  tool and cambium alike run plain Bazel 7.2.0 there (env beats `.bazeliskrc`;
  verified on the box, and `generate-hashes` then hashes 64 850 targets).
  Expect sourcegraph's `bazel_version_used` to read `bazel 7.2.0` instead of
  `aspect 5.8.19`.

Then the recipe in [docs/OPERATIONS.md](docs/OPERATIONS.md). Both re-runs are
cheap: `changes` resumes from `state.json`, so a subject that already has
results only redoes what changed.

Expect, and do not be alarmed by:

- **envoy failing ~6 change sets on recall.** All six have
  `external_route == missing`: entirely the documented limitation where Bazel
  reaches a target by leaving `//...` and coming back
  (`//distribution/dockerhub:*` through `@envoy_repo//:project`, and
  `//source/common/quic:*` through `@quiche`). cambium stops at the first
  external node. `rdeps.rs` says outright this "is a real cambium miss and it
  fails the change set" — it is designed to fail, with the cause named.

## Open issues

- #1 live TUI for the benchmark matrix (`bench watch`).
- #2 CLI surface: intent-first verbs, diagnostics with codes, one output
  contract.
- **#3 externals input-path precision: do NOT close it.** The evidence the
  last handoff nominated does not show the effect. Envoy's history at the
  same pin, before (a result tree from the pre-rename build, since deleted)
  against after: globals 22 -> 22,
  cold rebuilds 140 -> 130, `largest_dirty_packages` 1115 -> 1115 on the same
  commit, and `mean_affected` identical to four decimal places
  (4101.2467). The projected "21 of 23 commits stop being global" did not
  happen. Caveat before you conclude anything: the before-file comes from a
  pre-rename build, so a strict comparison wants a re-run of the old commit
  rather than a read of the old file. Worth checking first whether the
  history path exercises the externals stage at all.
- **#4 was a real bug and is fixed (926df79), but it was never envoy's.**
  The `cambium_missing_bzl_closure=554` the first run reported, and the second
  run reported again *after* the fix, is not a load-closure gap at all. The
  index that run built has a `pkg_ext` row for every one of those packages
  (`bazel/external` → `@@rules_cc+//cc:cc_library.bzl`, `tools/*` →
  `@@rules_python+//python:defs.bzl`), so rule (a) sees them; no `.bzl` in
  those change sets declares a repository they load from. `rbuildfiles` was
  naming the *whole repository*: it walks Skyframe's file nodes, and under
  bzlmod every package's evaluation depends on the repository mapping, hence
  on the `.bzl` files that declare repositories. Bazel named 1211 packages on
  all five and the "missing" set was exactly the complement of cambium's dirty
  set every time; one of the packages named is
  `bazel/external/c-ares/include/config_linux`, whose whole `BUILD` file is
  `exports_files(["ares_config.h"])`. The oracle now recognises that signature
  (a named package whose `BUILD` file has no `load()`) and records the change
  set under `repository_evaluation` instead of failing it. The `rdeps` oracle,
  which checks the answer that matters, had recall 1.0 on all five.
- **The four `unclassified` extras on those same change sets were a
  classifier bug, now fixed.** `//bazel:libc++`, `//bazel:libc++_enabled`,
  `//bazel:libstdc++`, `//bazel:libstdc++_enabled`: `+` is Bazel query's union
  operator and its lexer ends a word there, so every rule-proto lookup for
  them was a syntax error and every predicate that compares the two revisions
  answered "no". They are `cambium_extra_bzl`, like the rest of the re-queried
  `//bazel` package. `cambium_bazel::query_word` quotes such patterns; the
  `rdeps` oracle's `set(...)` batches went through the same hole.
- #5 sourcegraph's `go.mod` / `pnpm-lock.yaml` extension inputs should be
  derived like envoy's `api/` mapping.
