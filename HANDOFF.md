# Handoff: cambium real-repository validation suite

Rewritten 2026-09-07 ~10:45 EDT by the orchestrating session. Read top to
bottom. `tools/corpus-progress.sh` reconstructs the live picture at any time.

## Where things stand

All code is committed and pushed on `main` (HEAD `1684956`). The tree has
uncommitted edits from two subagents still working (see "Running"). Commit
only what is green; each agent reports what it changed.

Delivered and pushed: `crates/compare`, `crates/corpus`, `cambi corpus
fetch|build|replay|pairs|compare|report|all`, `bench legacy` (bazel-diff-rust
on synth; `docs/bench-legacy.svg`), `tools/bazel-diff/fetch.sh` (v46.1.0),
`tools/tango/` (standalone `tango-cli` built inside uber/tango v0.0.46 plus
one small patch for `--keep_going` partial graphs), CI (`synthetic-threeway`,
`bazel-version-matrix` over 7.4.1/8.5.1/9.2.0, `corpus-nightly`,
`corpus-weekly`), `docs/CORPUS.md` with generated sections between markers,
`tools/corpus-progress.sh`.

### Engine bugs the suite found (all fixed, all in git log)

1. `generator_location` in the rule hash (bazel-diff and Tango drop it).
2. A modified BUILD file is a direct input of any rule that globs it.
3. Deleting `BUILD` while `BUILD.bazel` remains is not a package deletion.
4. Ignore globs silently dropped consumed files (CHANGELOG.md in a genrule).
5. `rbuildfiles`/dirty-package queries under `--keep_going` exit 3: tombstone
   or escalate, never fail. Cold build accepts exit 3 too (records partial).
6. Stale store lineage won over a compatible one (config-hash-aware fetch).
7. A missing base index was built with Bazel on the *head* checkout
   (now a detached worktree at base, opened lazily only when the store misses).
8. `label_flag`/`label_setting` edges: `build_setting_default` is typed
   STRING by Bazel; the first fix (v3) matched LABEL and never fired. v4.
9. `--noimplicit_deps` dropped real main-repo edges (a rule's private
   `attr.label(default=...)`); removed from the index query, oracle, verify,
   classifier. Cost 3-4% index size.
10. A `.bzl` loaded only from MODULE.bazel/WORKSPACE (repository rules) was
    treated as dead: rbuildfiles names `@repo//:BUILD` / `//external:WORKSPACE`,
    now a global invalidation.
11. Producers shared cambium's `--output_base` across workspaces, poisoning
    external repos (harness bug).

`RULE_HASH_VERSION` is 4; it is mixed into the config hash, so every index
older than commit `250bfb7` is a different lineage.

### Results as of this handoff (item 4 landed at b5a7211; quick profile at ace2c76)

One Bazel per repository (Rainy, 2026-09-07): each entry runs at its own
`.bazelversion`, or a `bazel_version` pin only when it has none (apollo).
Bazel 7/8/9 coverage is CI's synthetic `bazel-version-matrix` job, not the
corpus; the historical-pin entries `bazel-7`, `bazel-8`, `envoy-7` were removed
in ace2c76 (their last results stay on disk under `corpus-results/`).

| entry | Bazel | replay oracle (old binary, pre item 4) | three-way |
|---|---|---|---|
| bazel | 9.2.0 | green; 16/300 global, 2 in the last 50 | 40/40 at v3 and v4 |
| sourcegraph | aspect 5.8.19 | green; 25/300 global, 1 in the last 50 | 36/36 at v4, cambium_missing 0 |
| envoy | 8.7.0 | 74/159 global (93/300 at v3); last-50 window replaying now | 11/40 at v4 (cambium_missing 914, mostly api/** and externals) |
| apollo | 7.4.1 (pinned; WORKSPACE-only) | green over 74 commits; 11 global, 8 in the last 50 | 39/40, the 1 is the new bazeldiff_extra_line_shift class |

Removed entries, last numbers: bazel-8 40/40, 17/150 global; bazel-7 40/40,
20/150 global; envoy-7 9/40 pass.

Timings (v3 runs, warm store): cambium 0.2-0.5 s per pair; bazel-diff warm
4-15 s, cold 7-64 s; Tango 33-530 s. Synthetic 100k targets: cambium 67 ms vs
bazel-diff 37 s warm, exact agreement. See `docs/CORPUS.md` (regenerate with
`cambi corpus report <id>`; `--profile quick` writes
`corpus-results/<id>/quick/CORPUS-quick.md` instead).

### Profiles

`--profile quick` (env `CAMBIUM_CORPUS_PROFILE=quick`): 50 commits, 50
deterministic pairs, Tango on the first 10; results under
`corpus-results/<id>/quick/`. Local before/after evidence for the invalidation
plan uses quick; the published numbers come from a full run on EC2
(c7i.4xlarge for bazel/envoy/sourcegraph/apollo; 8xlarge + 500 GB for xla/tf).

## Running right now (logs in $CLAUDE_JOB_DIR/tmp = /Users/rainy/.claude/jobs/6b58f265/tmp)

- **after.sh** (log `after.log`): release build at 7036fa0+, then
  `corpus all <id> --profile quick` for bazel, sourcegraph, apollo, envoy
  (FORMAT_VERSION 5 => every repo cold-builds once), then the true envoy
  before-window with `tmp/old-target/release/cambi` (15a46b1) into
  `corpus-results-before-true/envoy`. Prints `=== AFTER DONE`.
- Envoy's corpus.toml invalidators are now empty (derived by items 1+3); the
  replay oracle is the check. A mismatch on a commit touching api/**,
  changelogs/**, .github/config.yml or VERSION.txt means put that path back.
- Then: quick before/after table into docs/CORPUS.md Findings, commit.

## The agreed plan (from the owner, in order; each ships with before/after replay globals and a clean three-way in the same commit)

5. (done, `1684956`) bazel entries had no real invalidators.
4. (done, `b5a7211`) Static `.bzl` load closure replaces `rbuildfiles` on the `.bzl` path
   (repo mapping via `bazel mod dump_repo_mapping ""`; external loads stop at
   the external label; MODULE/WORKSPACE closure → global; keep `rbuildfiles`
   as a corpus oracle asserting the static closure is a superset).
1+3. (done, merged 7036fa0) Repo-granular external invalidation: MODULE.bazel.lock diff and
   repository-rule inputs map to repos; dirty (a) main-repo consumers of
   `@repo//...` edges and (b) every package whose `.bzl` closure contains a
   file from that repo. A module that registers toolchains stays global
   (toolchain resolution has no edge to follow). Needs item 4.
2. (done, merged 5bcb978) `.bazelrc` diff as an allowlist of (command, flag) pairs known not to
   affect loading (`--remote_*`, `--jobs`, `--test_*`, `--disk_cache`,
   configs no query uses); unknown flags and `import`/`try-import` go global.
6. (done) Replace `max_changed_files` with "cold build when dirty packages
   exceed N", N from replay per-package query timings. N = 500: the replay
   crossover (cold build / marginal ms per dirty package) is ~450 on bazel,
   ~620 on apollo, ~210 on sourcegraph quick and past the whole repository on
   envoy. A cold rebuild re-queries `//...` at head and splices it onto the
   base, so it stays exact — it is reported as `stats.cold_rebuild`, not as a
   global — and the corpus replay counts the two apart
   (`summary.engine_cold_rebuilds`).
Target: envoy replay globals 93 → under 20 per 300.

Validation policy (Rainy, 2026-09-07): the corpus suite is for timing and
accuracy numbers against bazel-diff and Tango, not for testing our changes.
Each item validates with unit tests, the synth soundness sweep, the real-Bazel
e2e, and at most a handful of targeted `cambi affected` runs on specific
corpus commits of the class being changed. Quick-profile replays are run
once per milestone, full replays only on EC2.

Then: regenerate all report sections, write the hand-written `## Findings`
section of `docs/CORPUS.md` (the eleven bugs above, the externals
limitation, the laptop caveat on producer timings), delete this file's
"Running" section or the file, commit, push.

## Gotchas learned

- zsh does not word-split `$VAR`; use `${=VAR}` or explicit variables in
  ad-hoc shell.
- `$TMPDIR/cambium-cache` holds fetched indexes; after a FORMAT_VERSION bump
  old files must be deleted (check the `CAMBIDX1` header version).
- Killing a compare mid-pair leaves registered tango worktrees under
  `corpus-results/<id>/scratch/tango-worktrees/` and `base-worktree` in the
  corpus checkout: `git worktree remove --force` them and re-checkout the pin.
- Never share one `--output_base` across workspace directories.
- HEAD pins only load under their own Bazel major, which is why the corpus
  runs one Bazel per repository and never a version matrix.
- This Mac cannot fetch some externals (GraalVM macos-x64, Docker-based RBE
  configs, some mirror 404s): producers pay for those on every run, cambium
  mostly does not; absolute producer timings are inflated here.
- Replay-then-compare, never compare alone after a hash bump: compare
  cold-builds every pair's base otherwise.
