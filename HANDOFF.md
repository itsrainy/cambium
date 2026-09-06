# Handoff: cambium real-repository validation and comparison suite

Written 2026-09-06 by the agent implementing the "corpus" spec (v1). Read this
top to bottom before touching anything; the spec the user gave is reproduced in
short at the end.

## Where things stand

Work is on `main` in `/Users/rainy/Documents/src/cambium`, **uncommitted**.
`git status` shows every changed/new file. Nothing has been committed or pushed;
the user has not asked for commits (commit only if they ask). The user said
"stop once you've updated the handoff", so the previous agent stopped here with
the tree formatted, clippy-clean, and every test green
(`cargo test --workspace` last ran green before `crates/corpus` was added, and
`-p cambium-synth -p cambium-engine -p cambium-corpus` green after; rerun the
full workspace first thing).

Files touched (from `git status`): `Cargo.toml`, `Cargo.lock`, `README.md`,
`docs/STORE.md`, `crates/{bazel,diff,index,engine,cli,bench,synth}` edits,
new `crates/corpus/`, placeholder `crates/compare/`, `corpus.toml`,
`tools/bazel-diff/fetch.sh`, `crates/engine/tests/tree_keys.rs`, this file.

Milestone status (numbers are the spec's §12):

| milestone | status |
|---|---|
| 3. Engine changes §9.1 (tree-hash store) and §9.2 (per-target depth, schema 3) | **done, tested** |
| 1. `crates/corpus` (manifest, fetch, build, pairs, replay, state) | **written, all tests green** (fetch/checkout against a real remote is untested; run `cambi corpus fetch bazel` once the CLI exists) |
| 2. `crates/compare` (bazel-diff runner, three-way, classifier) | **not started** (placeholder `crates/compare/src/lib.rs`, Cargo.toml written, already a workspace member) |
| 4. bazel-diff Rust as `bench legacy` on synth | **fetch script done and the binary is built**; bench stage not written |
| CLI `cambi corpus ...` | not started |
| 5. docs/CORPUS.md, CI nightly/weekly | not started |
| 6. tensorflow | manifest entry only (`skip = true`) |

Tasks are also tracked in the session's task list (TaskList tool), ids 1-9.

## Environment facts (verified)

- macOS x86_64, 8 cores, 16 GB RAM, ~430 GB free. Toolchain pinned by
  `rust-toolchain.toml` to 1.98.1 (installed).
- **No system Bazel, no Java, no Docker.** The user said Bazel may be installed.
  I installed bazelisk to `~/.cache/cambium/bin/bazelisk` (v1.29.0); it has
  already downloaded Bazel 9.2.0. Use it via `--bazel-binary` or by putting
  `~/.cache/cambium/bin` on PATH. Kotlin bazel-diff cannot run here (no JRE).
- **bazel-diff Rust binary is built**: `~/.cache/cambium/bazel-diff/bin/bazel-diff-rust`
  (`--version` prints `bazel-diff 39.0.0`; the crate version lags the tag).
  Built by `tools/bazel-diff/fetch.sh` from Tinder/bazel-diff at tag v46.1.0,
  commit `27ce213b8b090792e06057bff96e37bd284ece8e`. `VERSION` file records it.
  Upstream has no prebuilt macOS x86_64 Rust binary, which is why it builds
  from source (needs only cargo; protoc is vendored by its build script).
- Network works (GitHub API, raw.githubusercontent, git ls-remote).
- `sourcegraph/sourcegraph` is private now; the manifest uses
  `sourcegraph/sourcegraph-public-snapshot` (archived 2024-09-02).
- Corpus pins in `corpus.toml` are the upstream HEADs as of 2026-09-06.

## What was learned about bazel-diff (needed for milestones 2 and 4)

Source fetched to `/Users/rainy/.claude/jobs/6b58f265/tmp/bd_bazel.rs`,
`bd_hash.rs`, `bd_model.rs`, `perf_workload.py` (job tmp; may be gone — refetch
from `https://raw.githubusercontent.com/Tinder/bazel-diff/master/src/...`).

- CLI (Rust and Kotlin share flags):
  `bazel-diff generate-hashes -w <ws> -b <bazel> [--bazelStartupOptions ...]
  [--bazelCommandOptions ...] [--fineGrainedHashExternalRepos a,b] [-k]
  [--excludeExternalTargets] [--depEdgesFile deps.json] <out.json>` and
  `bazel-diff get-impacted-targets --startingHashes a.json --finalHashes b.json
  [-d deps.json] -o out.txt -w <ws> -b <bazel>`. Output is one label per line
  (JSON list of `{label,targetDistance,packageDistance}` with `-d`).
- How it drives Bazel: `bazel version` (parses `Build label:` or `bazel X`),
  `bazel mod graph` (to detect bzlmod; on >= 8.6 it also runs
  `mod dump_repo_mapping` / `mod show_repo`), then one
  `bazel query --output streamed_proto --proto:instantiation_stack
  --order_output=no [--keep_going] --query_file <f> [--output_file <f>]`
  with expression `'//external:all-targets' + '//...:all-targets'` (retries
  without `//external` if that package errors). `--output_file` is used when
  version >= 8.2.0.
- Its perf gate replays a **fake `bazel` shim** answering `version`, `mod`
  (exit 1 = not bzlmod), `info`, `query` (copies a fixture streamed_proto to
  `--output_file` or stdout). That is the recipe for `bench legacy` on synth:
  emit the synth workspace (`cambium_synth::emit_workspace` /
  `emit_git_history`), dump `cambium_synth::proto::to_streamed_proto(&graph)`
  at base and head, point bazel-diff at a shim that serves the right dump, so
  Bazel time is removed. bazel-diff hashes source-file *content* from the
  workspace, so the emitted files must be on disk at each revision.
- Hash files are JSON `{label: hash}`; `get-impacted-targets` diffs them.

## What changed in the engine (done)

- `crates/index/src/walk.rs`: `WalkOptions.max_depth: Option<u32>`,
  `WalkResult.depth: Vec<u32>` (NO_ID = unvisited), `depth_of(id)`. Tombstones
  count as hops. Tests in `crates/index/tests/index_core.rs`.
- `crates/engine/src/report.rs`: `SCHEMA_VERSION = 3`, `AffectedTarget
  {label, depth, test}`, `AffectedReport.affected: Vec<AffectedTarget>`,
  helpers `affected_labels()`, `affected_within(n)`, `Stats.rbuildfiles_called`.
- `AffectedRequest.max_depth`; `QueryOptions.max_depth`; CLI `--max-depth` on
  `affected` and `index rdeps`. `index stats` gained `in_degree.top`
  (`HubTarget {label, in_degree}`, 20 entries, `TOP_IN_DEGREE`) — this is the
  hub list `corpus pairs --kind stress` should feed from.
- `crates/engine/tests/json_contract.rs` updated; README contract table
  updated (version 3 row).
- Tree-hash store (§9.1): `cambium_index::format::index_key(lineage, tree)`,
  `commit_pointer_key(lineage, sha)` (`commits/<lineage>/<sha>`, content =
  tree hash), unlistable-store fallbacks `indexes/by-tree/<tree>.bin` and
  `commits/by-commit/<sha>`; legacy `indexes/by-commit/<sha>.bin` still read.
  `Engine::tree_of(rev)` (falls back to the rev string when git can't
  resolve — synthetic ids), `fetch_baseline` resolves the tree first, then the
  pointer. `Repo::tree_of`, `first_parent_log`, `parents`, `git(args)` added
  to `crates/diff`. Tests: `crates/engine/tests/tree_keys.rs` (identical
  trees, squash-merge, pointer for unresolvable sha). `docs/STORE.md` updated.
- Bazel plumbing: `Bazel::with_startup_flags`, `with_env`, `command(sub)`
  (used by `index verify` now); `[bazel] startup_flags` in `cambium.toml`,
  **excluded from the config hash** (`Config::hash` clears it) and skipped in
  serialisation when empty so existing hashes don't move.
- `cargo test --workspace` was fully green after the engine changes (before
  the corpus crate was added).

## `crates/corpus` (written; read the module docs)

- `manifest.rs`: `Manifest`/`RepoEntry` for `corpus.toml` (root of repo).
  Extra field `bazel_versions` for the §3.2 matrix (`USE_BAZEL_VERSION` via
  bazelisk env). Test loads the checked-in manifest.
- `checkout.rs`: `CorpusDir` (`$CAMBIUM_CORPUS_DIR`, default
  `~/.cache/cambium/corpus`): `<id>.git` mirror, `<id>` worktree,
  `store/<id>` dir store, `output/<id>[-ver]` output bases, shared
  `repository_cache`, `disk_cache`, `bazelrc`. `Checkout::config_for` builds
  the generated `cambium.toml` (defaults + manifest invalidators + startup
  flags `--output_base` and `--bazelrc` + dir store + max_ancestor_walk =
  replay_range); `write_config` writes it into the worktree and adds it to
  `.git/info/exclude`; `engine(entry, bazel_version)` opens an Engine.
- `pairs.rs`: adjacent / pr (merge commits → (merge-base(p1,p2), merge)) /
  stress (bzl, build_add_delete, hub, invalidator; `git log --first-parent
  -- <pathspecs>`, capped with a seeded sample). `PairSet` JSON.
- `replay.rs`: `Driver` trait (checkout + querier), `BazelDriver`, `replay()`
  producing `replay-<pin>.json` (`ReplayResult`: cold build with `index
  stats`, per-commit `CommitRecord`, `Summary` p50/p90/p99, zero-dirty
  fraction, largest dirty set, globals; rebuild + oracle). Publishes every
  index to the engine's store; resumes from the last published index.
- `oracle.rs`: `compare_by_label(replayed, rebuilt, cap) -> GraphDiff`.
- `state.rs`: `corpus-results/<id>/state.json`.
- Tests: `crates/corpus/tests/replay_synthetic.rs` (real git + SynthQuerier).

### A bug the replay oracle found (fixed)

The first run of `replays_a_history_and_the_oracle_holds` failed with one
rule-hash mismatch. Root cause was in the **synthetic emitter/mutator, not the
engine**: `Mutation::EditRuleAttr` could pick an `alias` (or `test_suite`),
bumping its in-memory `attr_salt` — which feeds `SynthGraph::target_hash` —
while the emitted BUILD file carries no salt for aliases, so `git diff` saw no
change and the engine (correctly) re-queried nothing. Fixed in
`crates/synth/src/mutate.rs` by making `EditRuleAttr` only pick rules that
have attributes. All of `cambium-synth`, `cambium-engine`, `cambium-corpus`
tests pass after the fix; `cargo fmt` and `cargo clippy -D warnings` are
clean on the whole workspace.

Worth remembering for the real corpus: a rule hash in a spliced index can
legitimately go stale relative to a rebuild when a *dependency in another
package* is deleted (the consumer's row is not re-queried; the engine marks the
consumer direct via `DeletedDep`, so this is over-approximation, not
unsoundness). With real Bazel the consumer's proto still names the dangling
label, so the hash does not move at all — but if `compare_by_label` reports
`rule_hash_mismatch` on a real repo, check for this before calling it a bug.

## Remaining plan (what to build next, in order)

1. (done) The oracle failure above is fixed.
2. `crates/compare`:
   - `bazeldiff.rs`: runner for the Rust binary (`CAMBIUM_BAZEL_DIFF` env or
     `tools/bazel-diff/fetch.sh` output), Kotlin via `java -jar` when present;
     `generate-hashes` at base and head (needs the worktree checked out at
     each; use a second `git worktree` for base so cambium's head checkout is
     undisturbed), then `get-impacted-targets`. Record impl + version. Pass
     `--fineGrainedHashExternalRepos` empty, no `--useCquery`, and
     `--bazelStartupOptions` = the corpus startup flags so the same output
     base is used. Time cold (two hashes) and warm (one hash + diff).
   - `oracle.rs`: sample k labels of cambium's direct set (seeded),
     `bazel query "rdeps(//..., <label>)" --output=label --noimplicit_deps`
     at head via `engine.bazel.command("query")`; union; cambium's walk from
     the same roots on the head index must be a superset.
   - `classify.rs`: `Facts` trait + classifier returning the §4.2 classes;
     unit tests with hand-built facts per class. Bazel-backed `Facts`
     implementation (query the rule proto at head with
     `--proto:flatten_selects=false` to see `selector_list` for
     `cambium_extra_select`; compare base/head canonical rule minus
     visibility/tags for `cambium_extra_visibility`; changed paths with no
     consumer in the head index for `bazeldiff_extra_content`; external
     labels for `bazeldiff_extra_external`).
   - `threeway.rs`: per pair: run `engine.affected` (materialize to get the
     head index), bazel-diff, oracle; symmetric difference → classify;
     precision `|bd ∩ cambium| / |cambium|`, recall vs oracle must be 1.0;
     pass = zero `cambium_missing` and zero `unclassified`. Write
     `corpus-results/<id>/compare-<pin>.json`; mark pair keys done in
     `State.compared`.
   - `report.rs`: regenerate the repo's section of `docs/CORPUS.md` from the
     replay/compare/bench JSON (§11), plus the top-level table.
3. `bench legacy` (spec §7) in `crates/bench`: new stage `legacy_bazel_diff`
   for `cambi bench legacy --preset ... --changes typical-pr`: emit synth
   workspace + 2 revisions, proto dumps per revision, shim script (see above),
   run bazel-diff twice + diff, run cambium `compute` in memory on the same
   `Commit`; compare label sets (agree up to `select`); report cold/warm
   bazel-diff vs cambium. Headline sweep over targets 1e4..1e7 with
   `bench sweep --stage legacy_bazel_diff`-style plotting (`harness::plot_svg`
   draws one line; extend to two).
4. CLI `cambi corpus fetch|build|replay|pairs|compare|report|all` in
   `crates/cli/src/cmd/corpus.rs` (add `Corpus(cmd::corpus::Args)` to
   `main.rs`, `pub mod corpus` in `cmd/mod.rs`, deps `cambium-corpus` and
   `cambium-compare` in `crates/cli/Cargo.toml`). Common flags: `--manifest`
   (default: discover `corpus.toml` upward from cwd), `--results-dir`
   (default `corpus-results`), `--corpus-dir`, `--bazel-version`, `--seed`.
   `build --at <sha>` defaults to the pin and writes
   `corpus-results/<id>/index-<sha>.bin` (+ publishes). `pairs --kind` reads
   hubs from that index's `stats().in_degree.top` → packages.
5. `docs/CORPUS.md` (tables per repo + machine requirements 16+ cores, 64 GB,
   500 GB disk), README additions (CLI list, corpus section), CI: nightly job
   for `bazel` + `envoy` (last 50 commits, adjacent + stress, oracle k=100),
   weekly `sourcegraph`, `apollo`, `xla`; per-PR synthetic three-way at
   `small` (`bench legacy`).
6. Actually run it here: `cambi corpus all bazel --bazel-binary
   ~/.cache/cambium/bin/bazelisk` (the `bazel` repo is small; expect the
   first `bazel query //...:*` to take many minutes with external fetches;
   the `bazel` repo's `.bazelversion` decides the version). Record numbers in
   `docs/CORPUS.md`. Then `bench legacy` on synth for the headline chart.
   The user explicitly said installing Bazel on this machine is fine.

## Conventions observed in this repo

- Doc comments explain *why*; tests are named as sentences. Keep that style.
- Every JSON document carries `schema` (`cambium_engine::report::SCHEMA_VERSION`).
- `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings`
  gate CI on the pinned toolchain; run both before finishing.
- Temp files go in `$CLAUDE_JOB_DIR/tmp` (`/Users/rainy/.claude/jobs/6b58f265/tmp`).

## The spec, in brief

Goals: soundness on real repos; like-for-like vs bazel-diff; updater
throughput on real histories; reproducible and CI-schedulable. Deliverables:
`crates/corpus`, `crates/compare`, `tools/bazel-diff/`, `cambi corpus ...`,
`bench legacy` on bazel-diff-rust, `docs/CORPUS.md`. Corpus: bazel, envoy,
sourcegraph, apollo, xla, tensorflow with pins; generated `cambium.toml`;
shared repository/disk cache and per-repo output base; version matrix (7.x,
8.x) for bazel and envoy. Three-way: cambium vs bazel-diff vs sampled
`rdeps` oracle; disagreement classes `cambium_extra_select`,
`cambium_extra_implicit`, `cambium_extra_visibility`,
`bazeldiff_extra_content`, `bazeldiff_extra_external`, `cambium_missing`
(fail), `unclassified` (fail); precision informational, recall vs oracle must
be 1.0. Pairs: adjacent, PR-shaped, stress (bzl, BUILD add/delete, hub,
invalidator; 20 per category). Replay: cold build at pin-range, forward
updates, per-commit metrics, rebuild at pin must equal replayed index. Bench
legacy: bazel-diff-rust on synth proto dumps; cold (two hashes) and warm (one
hash) vs cambium; headline chart targets 1e4..1e7 at typical-pr. CLI
subcommands resumable via `corpus-results/<id>/state.json`. CI: per-PR
synthetic three-way; nightly bazel+envoy; weekly sourcegraph/apollo/xla;
tensorflow monthly. Reporting: `docs/CORPUS.md` per-repo sections plus a
top-level table and the scaling chart.
