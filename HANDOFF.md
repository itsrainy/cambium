# Handoff: cambium benchmark suite

Written 2026-09-08 for the next orchestrator. Read this, then
`docs/METHODOLOGY.md`. The owner is Rainy (they/them).

## Where things stand

Task 18 — the published run — has been **run once**, on the EC2 box, with
Rainy's go. It finished. It is **not published**: `docs/RESULTS.md` was
deliberately left uncommitted because two of the five subjects lost a
comparison tool to a harness failure, and the run's own gates did not notice.
The three defects it exposed are fixed and pushed (`7f2c666`); the run needs
doing again on top of them.

| commit | what |
|---|---|
| `1562325` | `tools/ec2/launch.sh` points at the AWS resource names that exist. The corpus->bench sweep renamed the key pair, PEM path, security group and instance-tag filter; the AWS resources kept the old names, because a key pair and a security group cannot be renamed in place. Defaults are `cambium-corpus` again, with a comment; the finder accepts tag `bench` or `corpus`. |
| `7f2c666` | The three fixes below. |

Everything before that is described in `git log` and in the previous
handoff's table (tasks 14-17, issue #3, PR-shaped change sets).

### What the run produced

5 subjects, 5 histories (1 274 commits), 200 change sets, ~7.5 h wall.
**All five rebuild oracles passed** — a cold index at the pin matched the
incrementally-updated one node, edge and rule hash for rule. That is the
suite's strongest correctness evidence and it is clean.

| subject | change sets | min recall | history: commits / p50 / globals / cold rebuilds |
|---|---|---|---|
| bazel | 39/40 | 1.000 | 300 / 2 ms / 4 / 0 |
| sourcegraph | 39/40 | 1.000 | 300 / 3 ms / 24 / 0 |
| apollo | 38/40 | 1.000 | 74 / 216 ms / 11 / 0 |
| envoy | 29/40 | 0.911 | 300 / 4 920 ms / 22 / 130 |
| xla | 40/40 | 1.000 | 300 / 9 ms / 31 / 0 |

Warm change-set timings, p50 ms (cambium against the foils):

| subject | cambium | bazel-diff | Tango (no cache, cold) |
|---|---|---|---|
| bazel | 23 | 1 730 | 19 750 |
| sourcegraph | 33 | *errored ×40* | 58 208 |
| apollo | 23 | 8 053 | *errored ×40* |
| envoy | 427 | 2 466 | 184 567 |
| xla | 39 | 11 335 | 211 831 |

Envoy is the honest outlier: 427 ms warm, an order of magnitude slower than
the rest and the only subject where bazel-diff is within reach. Deep `.bzl`
layering and 444 k edges. It is also the subject whose graph is closest to
the production repo this is all aimed at, so understanding that number is
worth more than any of the fast ones.

The no-inflation rule Rainy set holds up. Including the change sets where
cambium answers `//...` moves the median by about a millisecond (apollo
23 -> 24, xla 39 -> 35), because a global answer is *cheap* — cambium returns
`//...` without querying anything. What a global costs is precision, and
that is priced separately in `global_stats` (apollo's globals: mean 12 975
cambium labels against bazel-diff's 5 615). The expensive fallback is the
cold rebuild, and it **fired zero times in the change-set workload on every
subject**, so no published change-set timing includes one. If you want the
splice-limit overflow priced, it needs change sets chosen to exceed
`limits.max_dirty_packages`, or the history numbers cited alongside — envoy's
130 cold rebuilds in 300 commits are what drag its history p50 to 4 920 ms.

### The three fixes in `7f2c666`

All three were the harness's fault, not cambium's.

1. **The deleted half of the rdeps oracle now sees `dirty_packages`.** Its
   escape hatch has always been "the rule, or its package" — a rule deleted
   along with its source exists at neither end, and failing a change set for
   being right about a target that is gone would be wrong. But it read
   packages off cambium's *affected labels*, and a package deleted in its
   entirety has none: every rule is tombstoned, cambium correctly reports not
   one of them, and the package name survives only in `dirty_packages`. The
   hatch was unsatisfiable for exactly the case it existed for. Confirmed by
   re-running sourcegraph's `50dbc74f..5c5ed6ca` directly: cambium puts
   `internal/search/smartsearch` in `dirty_packages` and emits no label for
   it. This was **every** deleted-half failure on sourcegraph and apollo.
2. **A tool that could not run no longer passes its change set.** It used to
   record `pass: true, labels: 0`. Aggregates skip a tool that reported
   nothing, so the summary tables simply omit it and the subject renders as a
   comparison that agreed. That is how this run published sourcegraph without
   bazel-diff and apollo without Tango and looked clean doing it.
3. **`tool_missing`, a new class** — the taxonomy's first statement about a
   foil rather than about cambium. It fires only when no benign predicate
   explains a cambium-side extra *and* the rdeps oracle independently
   confirms the label, and it is reported rather than gated: cambium's run
   should not fail on another tool's defect. It is ranked **last** on
   cambium's side on purpose — the oracle is an unconfigured query, so it
   contains the `select` branches and macro-loaded packages
   `cambium_extra_select` and `cambium_extra_bzl` already name, and asking it
   first would relabel most of those as tool misses while saying nothing
   about mechanism. A test pins that ordering; do not reorder it without
   reading `the_oracle_is_asked_last_so_benign_classes_keep_their_labels`.
   The rdeps oracle now runs **before** the tools, because its answer is one
   of the classifier's facts.

## Before merging an engine change

Run `tools/bench/precheck.sh` and paste what it said. It replays the engine's
static stages — the externals rules, the dirty-package set, the cold-rebuild
decision — against every recorded change set of the real subjects, offline,
from the stored indexes, in minutes and with no Bazel. It exits 1 on
divergence. `docs/METHODOLOGY.md`, "Validating an engine change before a run",
says what it cannot see; a clean precheck does not replace the run, it is what
makes starting one worth the money.

## Next: the second published run

Nothing is blocked on Rainy. The box is theirs to pay for, so say you are
starting it, but the go for task 18 was given and these fixes are what it was
waiting on.

The two tool failures the last run tripped over are diagnosed and fixed;
both were misread at first, so the real causes are worth knowing:

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

Then the recipe under "The box". Both re-runs are cheap: `changes` resumes
from `state.json`, so a subject that already has results only redoes what
changed.

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
  same pin, before (`corpus-results-box/`) against after: globals 22 -> 22,
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

## The box

- `i-03c826487f1ab30ac`, c7i.4xlarge, us-east-2, 500 GB gp3, **stopped**.
  Volume bills ~$45/month while stopped. Key `~/.ssh/cambium-corpus.pem`,
  security group `cambium-corpus`. The AWS names are all `cambium-corpus` and
  will stay that way; only the words in this repository were swept. The
  instance carries both `cambium=bench` and the old tag value in the finder's
  filter.
- The SG allows SSH from Rainy's current IP only. When it changes, `aws ec2
  revoke-security-group-ingress` the old CIDR and
  `authorize-security-group-ingress` the new one.
- `tools/ec2/launch.sh {launch,sync,ssh,start,stop,terminate}`. The repo is
  private: `sync` tars the working tree over ssh and rebuilds cambi,
  bazel-diff-rust and tango there (`~/finish-bootstrap.log` ends with
  `BOOTSTRAP DONE`).
- **The mirrors on the box are still under `~/.cache/cambium/corpus/`** (61 GB,
  all five subjects fetched and built). Do not move them: the Bazel
  `--output_base` directories under there have absolute paths baked in, and
  moving the tree costs a full re-fetch and re-analysis. Set
  `CAMBIUM_SUBJECTS_DIR=$HOME/.cache/cambium/corpus` instead — the driver
  does.
- The driver from the last run is at `~/run-published.sh` on the box: one
  subject at a time, cheapest first (bazel, sourcegraph, apollo, envoy, xla),
  `fetch` / `build` / `history --oracle rebuild` / `changes --tools
  cambium,bazel-diff,tango --oracle rdeps,load` / `report`, then a final
  `bench report`. A failing subject does not stop the rest. Run it under
  `nohup` with a log and watch it with a Monitor; grep the log for
  `^=== `, `FAILED`, `PUBLISHED RUN DONE`, `panic`, `recall`, `unclassified`,
  `cambium_missing`, `No space left`.
- Timings from the last run, for planning: bazel ~23 min, sourcegraph ~57 min,
  apollo ~32 min, envoy ~2 h 34 m (Tango is ~3 min per change set there),
  xla ~3 h. Fetches and cold builds are ~free now that the caches exist.
- When it is done: tar `bench-results/` back, regenerate and commit
  `docs/RESULTS.md`, and `launch.sh stop`.

## Gotchas

- The job tmp dir (`$CLAUDE_JOB_DIR/tmp`) was wiped mid-session once; keep
  driver scripts and logs under the repo (ignored) or on the box. Note the
  engine's own index cache sits in `/tmp/cambium-cache/` on the box, which is
  the same kind of place.
- Tango, bazel-diff and cambium must never share an `--output_base`.
- macOS x86_64 cannot fetch some subjects' externals (toolchains_llvm,
  GraalVM, Docker-generated RBE configs): local tool timings are inflated and
  not publishable; bazelisk for the suite is `~/.cache/cambium/bin/bazelisk`.
- A killed change-set run leaves tango worktrees registered on the subject
  mirror; tango-cli recovers them itself on the next run (and says so on
  stderr when it does), `git -C <mirror>.git worktree prune` is the manual
  fix. An `is not a working tree` line is *not* that failure — see the apollo
  item above.
- Results written by another cambium build (format version or config hash)
  are started over, not resumed; that identity is in every state file. You
  will see `WARN skipping an index this build cannot read` for the same
  reason, which is benign.
- apollo: WORKSPACE-only, pinned to Bazel 7.4.1, history capped at 74
  commits because a deeper revision does not load. Its rebuild oracle is
  `partial` with 1 package in error — `@local_config_cuda`, no CUDA on the
  box. Expected.
- xla loads in ~25 s and indexes 455 packages against 449 BUILD files on
  disk, so that is a *complete* load, not a truncated one. The manifest's
  "~1 h loading, large fetch" note is about analysis with externals, which a
  loading-phase `bazel query` never pays. Its 4 `PARTIAL` packages are the
  Windows `tools/toolchains/win_1803/py3{8,9}` toolchains failing on Linux.
- `corpus-results/` and `corpus-results-box/` are the old vocabulary and this
  build cannot read them. They are history, and the only thing they are still
  good for is the issue #3 before-comparison above.
- zsh does not word-split `$VAR`; use explicit arrays in ad-hoc shell.
