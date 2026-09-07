# Handoff: cambium benchmark suite

Written 2026-09-07 (late) for the next orchestrator. Read this, then
`docs/METHODOLOGY.md`, then the task list. The owner is Rainy (they/them).

## Where things stand

The engine work is done and on `main`:

| commit | what |
|---|---|
| `b5a7211` | item 4: static `.bzl` load closure replaces rbuildfiles |
| `5bcb978` | item 2: `.bazelrc` edits parsed and diffed against an allowlist |
| `f4cf76d` | item 6: `limits.max_dirty_packages` (500) replaces `max_changed_files`; over it is an exact cold rebuild, not a global |
| `7036fa0` | items 1+3: MODULE.bazel diff and repository-rule inputs map to repositories; their consumers become direct |
| `11ae8e0` | soundness fix: a `.bzl` a package owns is also an ordinary changed source (filegroups listing it were missed; globals had hidden it) |
| `726d913` | harness: tool subprocesses get stdin closed and `CI=1` (Aspect CLI prompt); Tango driver recovers a locked stale worktree |
| `5596e64` | compare runs the other tools on global pairs too; all-pairs timing rows; Globals table; full pair cap 40 |
| `1cf10cf` | `docs/METHODOLOGY.md`: the signed-off vocabulary (below) |

Evidence so far (old vocabulary; files under `corpus-results/` locally and
`corpus-results-box/` copied from the box):

- envoy full history on the box: **93 -> 22 globals per 300**, rebuild oracle
  ok with an EMPTY invalidator list, but **140 engine cold rebuilds** (p50
  4.4 s): GitHub issue #3, being fixed (task 17).
- bazel full history: 16 -> 4 globals per 300; three-way on 18 incremental
  pairs: 0 disagreements, cambium 24 ms warm vs bazel-diff 1.7 s warm vs Tango
  17.9 s.
- sourcegraph full history: 25 -> 24 globals (its globals are `go.mod`,
  `pnpm-lock.yaml` invalidators and a legacy WORKSPACE; see issue #3's
  "same family" section).
- apollo (74-commit history, the loadable range): 11 globals, all from its
  own invalidator list; it is WORKSPACE-only so items 1+3 leave it global.

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

## Running right now (agents in `.claude/worktrees/`, all branched from main at 1cf10cf+)

| task | agent scope | merge notes |
|---|---|---|
| 14 rename to `cambi bench` | crates/corpus + crates/compare + crates/bench -> one crate; CLI tree; `subject_repos.toml`; `bench-results/`; RESULTS.md; CI; word sweep. Commits per step. | Merge FIRST. |
| 17 issue #3 | crates/index + crates/engine only (path mapping for repository-rule inputs; FORMAT_VERSION bump) | Independent of 14; merge second. |
| 15 full rdeps oracle | crates/compare/src/oracle.rs, its call site in threeway.rs, smoke_real_bazel.rs, docs | Written against the OLD layout. After 14 lands, re-apply onto crates/bench (the agent's report lists every hunk outside oracle.rs). |
| 16 PR-shaped change sets | crates/corpus/src/pairs.rs (+ a crates/diff accessor) | Same: re-apply onto the renamed tree after 14. |
| 18 published run | not started; box is STOPPED; needs Rainy's go | see below |

When an agent reports: `git merge --no-edit <branch>`, resolve, run
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, `cargo test --workspace`, the e2e if engine/index changed, then
push and `git worktree remove --force` + `git branch -d`. Watch for
`.claude/` or `ubuntu@...` directories sneaking into a commit (`.gitignore`
now covers them).

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
- Published run recipe (after 14-17 land and Rainy says go): `launch.sh
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

- #1 live TUI for the benchmark matrix (now `bench watch`).
- #2 CLI surface: intent-first verbs, diagnostics with codes, one output
  contract.
- #3 externals input-path precision (task 17).
- Not filed yet: sourcegraph's `go.mod` / `pnpm-lock.yaml` extension inputs
  should be derived like envoy's (see #3 "same family").
