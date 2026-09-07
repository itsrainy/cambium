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

### Results as of this handoff (pre item-4 rerun)

| entry | Bazel | replay oracle | three-way |
|---|---|---|---|
| bazel | 9.2.0 | green, 300 commits | 40/40 pass at v3; v4 rerun in progress |
| bazel-8 | 8.5.1 | green | 40/40 at v3; v4 rerun queued |
| bazel-7 | 7.4.1 | green | 40/40 at v3; v4 rerun queued |
| sourcegraph | aspect 5.8.19 | green | 36/36 pass at v4, cambium_missing 0, recall 1.0 |
| envoy | 8.7.0 | green (93/300 global before invalidator work) | v4 rerun in progress |
| envoy-7 | 7.7.1 | green | v4 rerun queued |
| apollo | see agent | cold build needed partial-universe support | agent running |

Timings (v3 runs, warm store): cambium 0.2-0.5 s per pair; bazel-diff warm
4-15 s, cold 7-64 s; Tango 33-530 s. Synthetic 100k targets: cambium 67 ms vs
bazel-diff 37 s warm, exact agreement. See `docs/CORPUS.md` (regenerate with
`cambi corpus report <id>` for each id; it rewrites the top table, version
matrix and bench section every time).

## Running right now (pids as of 10:45 EDT; logs in $CLAUDE_JOB_DIR/tmp = /Users/rainy/.claude/jobs/6b58f265/tmp)

- **Lane A4** `rerun-lane.sh bazel bazel-8 bazel-7` (pid 58226): build --force,
  replay --restart, pairs, compare --restart, per id. Log `laneA4.log`.
  Binary `tmp/cambi-run` built from `1684956`.
- **Envoy/sourcegraph agent driver** `run.sh` (pid 43240) then `finish_sg.sh`
  (pid 28876): sourcegraph compare (done, 36/36) → envoy replay --restart
  (running, pid 20850, `envoy_replay.log`) → envoy compare → envoy-7 replay →
  envoy-7 compare. Logs `driver.log`, `driver2.log`, `envoy_replay.log`,
  `envoy_compare.log`, `envoy7_replay.log`, `envoy7_compare.log`. Prints
  `ALL DONE` / `SUITE COMPLETE`. Uses `./target/release/cambi` built at 08:08
  (v4, before the lazy-worktree fix: its cambium walls are ~7-10 s too high;
  labels are right).
- **Apollo agent** (subagent a6f0c50f6432ac8b2): partial-universe cold build,
  replay, compare on ~/.cache/cambium/corpus/apollo. Logs `apollo_*.log`.
- **Item-4 agent** (subagent abaab62f5fd9b4357): static `.bzl` load closure,
  no corpus runs. Will report design + tests; do NOT run corpus until it lands.

Monitors in this session: `byf6n0lsx` (laneA4). Others were stopped.

## The agreed plan (from the owner, in order; each ships with before/after replay globals and a clean three-way in the same commit)

5. (done, `1684956`) bazel entries had no real invalidators.
4. Static `.bzl` load closure replaces `rbuildfiles` on the `.bzl` path
   (repo mapping via `bazel mod dump_repo_mapping ""`; external loads stop at
   the external label; MODULE/WORKSPACE closure → global; keep `rbuildfiles`
   as a corpus oracle asserting the static closure is a superset).
1+3. Repo-granular external invalidation: MODULE.bazel.lock diff and
   repository-rule inputs map to repos; dirty (a) main-repo consumers of
   `@repo//...` edges and (b) every package whose `.bzl` closure contains a
   file from that repo. A module that registers toolchains stays global
   (toolchain resolution has no edge to follow). Needs item 4.
2. `.bazelrc` diff as an allowlist of (command, flag) pairs known not to
   affect loading (`--remote_*`, `--jobs`, `--test_*`, `--disk_cache`,
   configs no query uses); unknown flags and `import`/`try-import` go global.
6. Replace `max_changed_files` with "cold build when dirty packages exceed
   N", N from replay per-package query timings.
Target: envoy replay globals 93 → under 20.

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
- HEAD pins only load under their own Bazel major; the matrix uses
  historical pins (`bazel-7`, `bazel-8`, `envoy-7`; envoy never pinned 9.x).
- This Mac cannot fetch some externals (GraalVM macos-x64, Docker-based RBE
  configs, some mirror 404s): producers pay for those on every run, cambium
  mostly does not; absolute producer timings are inflated here.
- Replay-then-compare, never compare alone after a hash bump: compare
  cold-builds every pair's base otherwise.
