# Handoff: cambium benchmark suite

Written 2026-09-07 (night) for the next orchestrator. Read this, then
`docs/METHODOLOGY.md`. The owner is Rainy (they/them).

## Where things stand

Everything the previous handoff had in flight is on `main` and pushed. Tasks
14-17 are done; only task 18 (the published run) remains, and it needs Rainy's
go because it starts the box.

| commit | what |
|---|---|
| `aab1997` | issue #3: per-repository path maps (`local_path_override`, `local_repository`, literal `ctx.symlink`) decide whether an input edit is the loaded `.bzl`; envoy's `api/**` edits no longer dirty ~1200 packages. Index FORMAT_VERSION 6, `stats.repos_label_only`, SCHEMA_VERSION 6. |
| `54edacf` | task 14: `crates/corpus` + `crates/compare` -> `crates/bench`; CLI is `cambi bench {subjects,history,changes,stages,sweep,regress,report}` and `cambi synth`; manifest `subject_repos.toml`; results under `bench-results/<subject>/<workload>/`; `docs/RESULTS.md` generated, `docs/METHODOLOGY.md` hand-written; old words swept. |
| `4445ab9` | task 16: PR-shaped change sets. `pr_reference` matches `Merge pull request #N`, `PR #N`, trailing `(#N)` and trailing `(https://github.com/o/r/pull/N)`; a commit is in exactly one kind; cap fills PR-shaped first, then stress <= cap/4, then adjacent. |
| `faa95c8` | task 15: full rdeps oracle. `kind(rule, rdeps(//..., set(<every changed file's label>)))` at head, batched (500 labels / 96 KiB) with bisection of failing batches; deleted files resolved from the base index's consumers; `--oracle-sample N` is an escape hatch, default 0 = full; `ChangesOptions::rdeps_oracle` toggles it (the CLI's `--oracle rdeps`). |
| `d57c76e` | smoke-test fixup after merging 15 and 16 together |

Gates run on the merged tree: fmt, clippy `-D warnings`, `cargo test
--workspace`, the engine e2e (4/4) and the bench real-Bazel smoke
(`CAMBIUM_E2E=1 CAMBIUM_BAZEL=$HOME/.cache/cambium/bin/bazelisk cargo test -p
cambium-bench --test smoke_real_bazel -- --ignored`, 2/2).

Earlier engine commits (`b5a7211`, `5bcb978`, `f4cf76d`, `7036fa0`,
`11ae8e0`, `726d913`, `5596e64`) are described in `git log`; the evidence
they produced is in the old vocabulary under `corpus-results/` and
`corpus-results-box/`, which this build can no longer read (RESULT_SCHEMA 6,
renamed fields and paths). Those directories are history only.

PR-shape counts over the last 300 first-parent commits (read-only on the
mirrors): bazel 101 PR-shaped / 199 adjacent (the 199 are Piper exports with
no PR), envoy 287 / 13, sourcegraph 300 / 0.

## The direction (signed off by Rainy)

`docs/METHODOLOGY.md` is the spec. Summary: "corpus" is retired. The
benchmark is `cambi bench` over **subjects** (real from `subject_repos.toml`,
synthetic `synth:<preset>`), **tools** (cambium, bazel-diff, Tango),
**oracles** (reverse-dependency = full `rdeps(//..., changed files)`,
rebuild, load), **workloads** (history; change sets of kinds PR-shaped
[primary], adjacent, stress), **runs** keyed by subject/workload/engine
identity under `bench-results/`. `docs/RESULTS.md` is generated for a reader
deciding whether to adopt cambium; `docs/METHODOLOGY.md` is hand-written.

Rules Rainy set, in their words where it matters:

- The benchmark is for timing and accuracy numbers against bazel-diff and
  Tango, "not for us to test our changes; we should have quicker methods".
  Validate with unit tests, the synthetic soundness sweep, the real-Bazel e2e
  (`CAMBIUM_E2E=1 CAMBIUM_BAZEL=$HOME/.cache/cambium/bin/bazelisk cargo test
  -p cambium-engine --test e2e_bazel`), and targeted runs. The
  "Choosing what to run" table in METHODOLOGY.md says which workload a change
  needs.
- Timing rows must include the change sets where cambium goes global or cold
  rebuilds, "so we don't artificially inflate our advantage".
- One Bazel per subject (its `.bazelversion`); 7/8/9 coverage is CI's
  synthetic matrix, never extra subject checkouts.
- Commit and push as you go. Agents work in isolated worktrees and commit to
  branches; the orchestrator merges, gates, pushes.
- Issue #3 lands before the published run. PR-shaped change sets are primary.

## Nothing is running

No agents, no worktrees with unmerged work. Two stale worktrees under
`.claude/worktrees/` (`agent-a1477d118ede418ed`, `agent-a32a183a90cb3eb84`)
hold nothing unmerged; `git worktree remove --force` them when convenient
(the auto-mode classifier blocked that command in this session).

When an agent reports: `git merge --no-edit <branch>`, resolve, run
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, `cargo test --workspace`, the e2e if engine/index changed, the
bench smoke if crates/bench changed, then push and `git worktree remove
--force` + `git branch -d`. Watch for `.claude/` or `ubuntu@...` directories
sneaking into a commit.

## Next: task 18, the published run (needs Rainy's go)

Blocked on Rainy: starting the box costs money and the security group must
allow their current IP. Once they say go, follow the recipe under "The box".
`tools/ec2/launch.sh` already uses the renamed tree. The mirrors on this Mac
still live under
`~/.cache/cambium/corpus/`; the renamed code looks in
`~/.cache/cambium/subjects/` (or `$CAMBIUM_SUBJECTS_DIR`), so on the box
either move the directory or set the variable rather than re-fetching.

## The box

- `i-03c826487f1ab30ac`, c7i.4xlarge, us-east-2, 500 GB gp3, **stopped**.
  Volume bills ~$45/month while stopped. Key `~/.ssh/cambium-corpus.pem`,
  security group `cambium-corpus` allows SSH from Rainy's current IP only
  (revoke the old CIDR and authorize the new one with `aws ec2
  revoke-security-group-ingress` / `authorize-security-group-ingress` when
  their IP changes).
- `tools/ec2/launch.sh {launch,sync,ssh,start,stop,terminate}`. The repo is
  private: `sync` tars the working tree over ssh and rebuilds cambi,
  bazel-diff-rust and tango there (`~/finish-bootstrap.log` ends with
  `BOOTSTRAP DONE`). Mirrors, caches and indexes live under
  `~/.cache/cambium/` on the box and survive stop/start.
- Published run recipe (when Rainy says go): `launch.sh
  start`, `launch.sh sync`, wait for BOOTSTRAP DONE, then per subject
  `bench subjects fetch`, `bench subjects build`, `bench history --oracle
  rebuild`, `bench changes --tools cambium,bazel-diff,tango --oracle
  rdeps,load`, then `bench report`; tar `bench-results/` back; commit
  RESULTS.md; `launch.sh stop`. Run under `nohup` with a log and watch it with
  a Monitor; the box takes ~1 min per subject for a 300-commit history and
  the change sets are the long part (Tango up to minutes per change set on
  envoy).

## Gotchas

- The job tmp dir (`$CLAUDE_JOB_DIR/tmp`) was wiped mid-session once; keep
  driver scripts and logs under the repo (ignored) or on the box.
- Tango, bazel-diff and cambium must never share an `--output_base`.
- macOS x86_64 cannot fetch some subjects' externals (toolchains_llvm,
  GraalVM, Docker-generated RBE configs): local tool timings are inflated and
  not publishable; bazelisk for the suite is `~/.cache/cambium/bin/bazelisk`.
- A killed change-set run leaves tango worktrees registered on the subject
  mirror; the driver now recovers, but `git -C <mirror>.git worktree prune`
  is the manual fix.
- Results written by another cambium build (format version or config hash)
  are started over, not resumed; that identity is in every state file.
- apollo: WORKSPACE-only, pinned to Bazel 7.4.1, history capped at 74
  commits because a deeper revision does not load.
- zsh does not word-split `$VAR`; use explicit arrays in ad-hoc shell.

## Open issues

- #1 live TUI for the benchmark matrix (`bench watch`).
- #2 CLI surface: intent-first verbs, diagnostics with codes, one output
  contract.
- #3 externals input-path precision: landed (`aab1997`); close it after the
  published run confirms envoy's cold-rebuild count dropped.
- #4 a BUILD file's direct `load()` of an external `.bzl` is invisible to
  rule (a) (`LoadGraph::package_closure` drops the external half). Found
  while landing #3; write the direct-load e2e first.
- #5 sourcegraph's `go.mod` / `pnpm-lock.yaml` extension inputs should be
  derived like envoy's `api/` mapping.
