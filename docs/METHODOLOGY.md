# How cambium is benchmarked

This page defines the words the benchmark uses and what each measurement
can and cannot claim. The numbers themselves live in [RESULTS.md](RESULTS.md).
Everything here is driven by `cambi bench` (`crates/bench`); the real
subjects are described by `subject_repos.toml`, the synthetic ones by the
presets `cambi synth presets` lists.

## Concepts

**Subject.** A Bazel repository a benchmark points at. There are two kinds,
and every workload accepts either.

- *Real*: an open-source repository from `subject_repos.toml`, with a URL, a
  pinned commit, a history window (how many first-parent commits before the
  pin a workload may use), and the Bazel version it runs under: the subject's
  own `.bazelversion`, or a pin in the manifest only when the subject has none
  (apollo, a WORKSPACE-only repository, runs under 7.4.1 because Bazel 9
  cannot load it). Bazel 7, 8 and 9 coverage of cambium itself is CI's
  synthetic version matrix; a subject is never pinned twice to exercise
  another Bazel major. Today: `bazel` and `envoy` on demand,
  `sourcegraph`, `apollo` and `xla` on the weekly schedule, and `tensorflow`
  -- the one entry carrying `skip = true` -- monthly or by hand
  (`.github/workflows/bench-weekly.yml`).
- *Synthetic*: a generated graph from a preset, named `synth:<preset>`
  (`synth:medium` is 4 551 packages and 50 167 rule targets, 250 507 index
  nodes once source files are counted). It is emitted as a
  real Bazel workspace with a git history when a workload needs one, so the
  same commands run on it as on a real subject. Synthetic subjects are cheap,
  deterministic and scale to sizes no real subject reaches; they say nothing
  about real Starlark, which is what the real subjects are for.

**Tool.** Anything that answers "which targets could be affected by this
change": cambium, [bazel-diff](https://github.com/Tinder/bazel-diff) (the Rust
port, `bazel-diff-rust`), and [Tango](https://github.com/uber/tango). In a
comparison cambium is one of three tools, not the thing being served. How each
is invoked is in [Tools](#tools) below.

**Oracle.** An independent check named by what it proves. No oracle proves
precision; see [What is not measured](#what-is-not-measured).

| oracle | question it answers | how |
|---|---|---|
| reverse-dependency | did a tool miss a target? | `bazel query "kind(rule, rdeps(//..., set(<every changed file's label>)))"` on the head revision, over every changed path that belongs to a package at head, batched by command-line length and unioned. cambium's answer must be a superset. A file *deleted* at head has no label to name, so its consumers are read from the base index's file rows instead and must appear in cambium's answer by label, by the package of an affected target, or in `dirty_packages`. The last spelling is the only one a package deleted in its entirety has: every rule in it is tombstoned, so cambium rightly reports none of them. Runs on every incremental change set; `--oracle-sample N` caps the labels queried and is an escape hatch for enormous subjects, not a mode a published number may use. |
| rebuild | is the incremental updater exact? | after a history run, a cold index built at the pin is diffed node by node, edge by edge and rule hash by rule hash against the index the history produced. Any difference fails. |
| load | is the static `.bzl` load closure complete? | on every change set touching a `.bzl`, Bazel's `rbuildfiles` must be a subset of the packages cambium dirtied. |

**Workload.** What tools are run on. There are two.

- *History.* The subject's last N first-parent commits, applied one at a time
  to an index built at the start of the window. This exercises cambium's
  incremental updater and is cambium-only: the other tools keep no state
  between runs, so there is nothing to replay. It yields the per-commit cost,
  the global-invalidation rate, the cold-rebuild rate, and the rebuild oracle.
- *Change sets.* `(base, head)` pairs shaped like the question a CI job asks.
  Every tool runs on each change set, and the reverse-dependency oracle
  scores recall. Kinds, in order of importance:
  - **PR-shaped** — one pull request's worth of change. For a merge commit,
    `merge-base(parents)..commit`. For a squash- or rebase-merged history,
    where every first-parent commit *is* a pull request, `parent..commit` for
    commits whose message carries a PR reference (`(#1234)`, `PR #1234`,
    `Merge pull request`, or a trailing
    `(https://github.com/<owner>/<repo>/pull/1234)`, the form bazel's own
    import tooling writes). This is the primary kind and the one the headline
    numbers are drawn from.
  - **adjacent** — `parent..commit` for first-parent commits with no PR
    reference. Same shape as a squash-merged PR; kept separate so the
    PR-shaped numbers are not diluted by fix-ups.
  - **stress** — the subject's own commits, chosen for what they touch rather
    than for their shape: a `.bzl` file, a `BUILD` file added or deleted, a
    path under a high in-degree "hub" package (`crates/index`'s
    top-in-degree list), or a path the manifest names as an invalidator. Not
    synthetic edits -- every one is a real commit in the window, filed under
    the category that selected it, so a subject whose history never deletes a
    `BUILD` file simply has no change set in that category. They exist to
    reach paths a quiet stretch of history would not.

**Run.** One benchmark run is the tuple *(subject, workload, tools, oracles,
window size, machine)* and the identity of the cambium build that ran it
(index format version and configuration hash). Results are stored under
`bench-results/<subject>/<workload>/` with that identity inside, so a run
from another build never resumes into this one and synthetic and real results
sit in one tree. Window size is a
parameter of the workload (`--commits 50`, `--change-sets 40`), not a mode: a
short local run and the published run differ only in size.

## Measurements

**Correctness.**

- *Recall* against the reverse-dependency oracle, per change set. Anything
  under 1.0 is a failure of the run.
- *Agreement* with the other tools, with every disagreement filed under one
  class of the [disagreement taxonomy](#disagreement-taxonomy). Three classes
  fail the run: cambium reporting fewer targets than a tool or the oracle
  (`cambium_missing`), the load oracle finding a package the closure missed
  (`cambium_missing_bzl_closure`), and a disagreement no rule explains
  (`unclassified`). Every other class is an over-approximation or a known
  tool difference and is reported, not gated. A change set also fails when a
  tool could not run at all: a comparison that did not happen must not read as
  a comparison that agreed, because the summary tables omit a tool that
  reported nothing and the absence is otherwise invisible.

**Cost.** Wall time per tool per change set, in two populations. *All change
sets* is the published number: it includes the change sets on which cambium
fell back to `//...`. *Incremental* is the change sets cambium answered with
a walk, and is the population precision and the class counts are computed
over. Cold and warm are defined per tool:

| tool | cold | warm |
|---|---|---|
| cambium | the base index is not in the store and is built with one `bazel query //...` | the base index is in the store; only the changed packages are queried |
| bazel-diff | first run, no hash cache | second run on the same pair, its hash cache present |
| Tango | every run; Tango has no cache | — |

**Fallbacks.** Where cambium gave up precision.

- *Global rate.* The share of history commits and change sets on which
  cambium answered `//...`, with the reason for each (a `.bazelrc` flag not
  on the allowlist, a lockfile edit, a legacy `WORKSPACE`, an invalidator
  path). On a global change set the tools still run and the report prices the
  fallback: cambium's answer size (every rule target in `//...`) against the
  precise set each tool found.
- *Cold-rebuild rate.* Change sets where the dirty package set exceeded
  `limits.max_dirty_packages` and cambium re-indexed the whole repository
  instead of splicing. The answer is exact and enters every statistic; the
  cost is a cold number, and the count says how often that happened.

## Tools

How each tool is invoked, so the comparison can be checked for fairness.

- **cambium**: `cambi affected --base <base> --head <head>` with the
  subject's generated `cambium.toml` (defaults plus a per-subject output base
  and store). The only subject-specific configuration is the manifest's
  invalidator list -- paths whose change invalidates everything, which is a
  soundness escape hatch rather than tuning. `bazel` and `envoy` have none;
  `sourcegraph`, `apollo`, `xla` and `tensorflow` each name a handful, and
  every one of them is in `subject_repos.toml` with the reason beside it.
- **bazel-diff-rust** v46.1.0: `generate-hashes` at base and head in two
  worktrees, then `get-impacted-targets`, with `-k` (keep going) so a package
  that fails to load does not abort the run. External repositories are
  hashed as bazel-diff does by default.
- **Tango** v0.0.46: `tango-cli` built inside the upstream module with a
  small driver (`tools/tango`) that runs its library on a `(base, head)` pair
  and prints the impacted set; patched only to pass `--keep_going` to its
  Bazel queries, for the same reason as above.

All three see the same checkout, the same Bazel version (the subject's), the
same repository cache, and stdin closed with `CI=1` set. Labels are compared
after canonicalising external repository names, and file targets are dropped
from every tool's answer so that only rule targets are compared.

## What is not measured

- **Precision has no oracle.** Bazel has no query that says "and nothing else
  could change". bazel-diff and Tango are foils, and the taxonomy is our
  explanation of each difference. "Agreement" is therefore a weaker claim than
  "recall".
- **External repository contents.** cambium's graph stops at the main
  repository's edge. A dependency changing *by declaration* (a version bump in
  `MODULE.bazel`, a main-repo file a repository rule reads) is seen; a change
  in an external repository's contents at the same declared version is not.
  The hash-diffing tools fetch and hash externals, so on such a change they
  are right and cambium is wrong, and the run would file it under
  `cambium_missing`. None has been observed in the subjects' histories.
- **Configured builds.** All three tools work on the unconfigured target
  graph. `select()` branches are kept, which is the source of cambium's
  largest over-approximation class.

## Disagreement taxonomy

Every disagreement between cambium and a tool, or between cambium and an
oracle, is filed under exactly one class (`crates/bench/src/classify/mod.rs`).
The three that fail a run are marked.

| class | meaning |
|---|---|
| `cambium_extra_select` | cambium keeps `select()` branches a configured query resolves away |
| `cambium_extra_external_repo` | reached through an external repository whose declaration moved; cambium cannot ask whether its contents did |
| `cambium_extra_implicit` | reached only through an implicit (`$`-prefixed) attribute edge the other tool does not walk |
| `cambium_extra_visibility` | rules differ only in `visibility` or `tags`, which cambium's hash ignores |
| `cambium_extra_bzl` | the rule is unchanged but a `.bzl` its package loads changed, so the loading package is direct |
| `cambium_extra_deleted` | a dependency of the target was deleted; not an approximation |
| `cambium_extra_build_glob` | a changed `BUILD` file is a source of a `glob()`bing filegroup |
| `bazeldiff_extra_content` | bazel-diff hashes a path cambium's graph does not name |
| `bazeldiff_extra_external` | canonicalisation of external labels only |
| `bazeldiff_extra_external_content` | external load nondeterminism under `--keep_going` in bazel-diff |
| `bazeldiff_extra_workspace_path` | a rule that bakes the absolute workspace root into its attributes; a harness artifact |
| `bazeldiff_extra_line_shift` | only the rule's line number moved; bazel-diff hashes `generator_location`, cambium and Tango do not |
| `tool_extra_source_directory` | the target consumes a *directory* as a source file (`src_dir = ["."]`, which Bazel renders as `//pkg:.`) and a changed path lies under it. Tango hashes a source directory's contents on disk; Bazel's graph has no edge from a file to the directory above it, which Bazel documents as untracked, so neither the reverse-dependency oracle nor cambium reaches the target. Filed only when the oracle ran and did not reach the label; if it did, the label is `cambium_missing`. Reported, not gated — a limitation of Bazel's source model, seen on every apollo change set that touches a package with an `install_src_files` rule |
| `tool_missing` | the tool missed a target the reverse-dependency oracle confirms is affected: the tool's miss, not cambium's extra. Reported, not gated — cambium's run does not fail on another tool's defect |
| `cambium_missing` | **fails** — a tool or the oracle reports a target cambium does not |
| `cambium_missing_bzl_closure` | **fails** — `rbuildfiles` names a package the static closure did not dirty |
| `unclassified` | **fails** — no rule explains it |

## Machines

Local runs (a laptop) are for development evidence and their tool timings are
not comparable: some subjects' external repositories cannot be fetched on
macOS x86_64, and a failed fetch is retried by bazel-diff and Tango on every
change set. Published numbers come from one machine, named in RESULTS.md,
with Docker available (some subjects generate remote-execution configurations
by running containers) and every external fetchable.

## The commands

```
cambi bench subjects            list, fetch, or cold-build a subject
cambi bench history  <subject>  the history workload; rebuild oracle
cambi bench changes  <subject>  the change-set workload across tools and oracles
cambi bench stages   <subject>  engine stage micro-benchmarks (synthetic subjects)
cambi bench sweep    <subject>  vary one parameter and report how a stage scales
cambi bench regress  <a> <b>    compare two result files and fail on a regression
cambi bench report              regenerate RESULTS.md from bench-results/
```

`<subject>` is a name from `subject_repos.toml` or `synth:<preset>`. Every
workload takes `--tools`, `--oracle`, `--kind` and a size (`--commits`,
`--change-sets`) so a run can be exactly as large as the question needs.
CI's per-pull-request job is `cambi bench changes synth:small --tools
cambium,bazel-diff`, the same command a person runs. Tango is left out of it
only because it costs 30-500 s a change set; the weekly job on the real
subjects runs all three.

## Running it

```console
$ tools/bazel-diff/fetch.sh          # builds bazel-diff-rust at its pin, once
$ tools/tango/fetch.sh               # builds tango-cli at its pin, once
$ cambi bench subjects fetch bazel   # mirror and check out the pin
$ cambi bench subjects build bazel   # one cold `bazel query //...`
$ cambi bench changes bazel --quick
```

Both tool scripts cache their binaries under
`~/.cache/cambium/{bazel-diff,tango}` (override with `$CAMBIUM_BAZEL_DIFF_DIR`
/ `$CAMBIUM_TANGO_DIR`). A run uses whichever of the two are installed and says
so when one is missing, rather than refusing to compare the other.

Mirrors, worktrees, per-subject stores, emitted synthetic workspaces and a
*shared* Bazel `repository_cache` and `disk_cache` live under
`$CAMBIUM_SUBJECTS_DIR`, default `~/.cache/cambium/subjects`. Sharing the
caches is what stops the subjects redownloading each other's externals.

`--quick` is 50 commits, 50 change sets and Tango on the first 10 — a bounded,
deterministic slice that finishes on a laptop, and a subset of what a full run
selects at the same pin and seed. It sets three flags and nothing else: the
results land in the same tree, and the file records the sizes that produced
them.

The real subjects are large. Budget, per concurrent subject: 16+ cores (Bazel's
own parallelism dominates), 64+ GB of RAM (`envoy` and `xla` load a large query
result into one server process), and 500+ GB of disk. `bazel` is the smallest
and cleanest entry specifically so that a from-scratch contributor can exercise
the whole pipeline on a laptop.

## Engine stages

Underneath the workloads sit the in-memory stage benchmarks, which need no
Bazel and so can be measured at sizes no real subject reaches:
`build_index`, `load_index`, `classify`, `parse_proto`, `rule_hash`, `splice`,
`reverse_rebuild`, `walk` / `walk_hub`, `e2e_memory`, `e2e_materialize`, and
`legacy_bazel_diff`.

```console
$ cambi bench stages synth:medium
$ cambi bench sweep  synth:medium --stage walk --param targets --values 1e4,1e6
$ cambi bench regress a.json b.json --threshold 15
```

Results carry a machine fingerprint, and `bench regress` refuses to compare
across machines rather than reporting noise.

`legacy_bazel_diff` is the like-for-like stage: it emits the graph as two real
workspaces, writes the `streamed_proto` dump `bazel query` would produce for
each, and puts a `bazel` shim in front of both tools that replays those bytes.
That takes the Bazel server out of the measurement, so what is left is each
tool's own work — the same replay technique bazel-diff's own perf gate uses.
The stage fails outright if bazel-diff reports a label cambium does not; the
other direction is the `select()` over-approximation above, and is reported.
Its sweep draws the headline chart (`bench-legacy.svg`).

## Validating an engine change before a run

A full run is seven hours on a rented machine, which is far too slow to be the
first thing that tells you an engine change moved an answer. Most of what such
a change touches needs no Bazel: between the diff and the `bazel query` the
engine decides which external repositories moved and by which rule, which
packages that dirties, and whether the dirty set exceeds
`limits.max_dirty_packages` and the run therefore cold-rebuilds. A published
run leaves the index at the pin behind, so all of that can be replayed against
the real subjects offline, in milliseconds a change set.

```console
$ tools/bench/precheck.sh                       # every cached subject
$ SUBJECTS="envoy apollo" tools/bench/precheck.sh
```

`precheck.sh` pulls each subject's stored index and recorded results off the
benchmark box into `bench-precheck/` (git-ignored, and skipped once cached),
builds `cambi` in release, and runs the check. Underneath it is one command:

```console
$ cambi bench replay envoy --change-set b352846f..a6871fc6   # one change set
$ cambi bench replay envoy --paths 'M\tbazel/repo.bzl'       # a hypothetical
$ cambi bench replay envoy --all                             # exit 1 on divergence
```

It prints the repositories dirtied and by which rule (input path, target-only,
module diff, load) and whether each drags its loaders with it; the packages
dirtied and by which route, separating rule (a) reached through a `.bzl` row
from the same rule reached through `pkg_ext` — the direct `load()` issue #4 is
about; the dirty-package count against the limit and whether that means a cold
rebuild; and a diff against what the run recorded.

The loop is honest about its four blind spots, and says so in its own output:

- **The answer needs Bazel.** `direct` and `affected` come from rule hashes
  after the head packages are re-queried. The replay reports an
  `affected (lower bound)` — the reverse walk from what the base index already
  knows changed — and never gates on it.
- **A cold rebuild's answer needs Bazel.** The replay says the fallback fires,
  which is the decision under test; not what `//...` would then return.
- **The stored index is at the pin**, and a change set's base is an ancestor of
  it. A divergence from a recorded answer is therefore a lead, not a verdict.
  The tight loop is `--save-baseline` before the change and `--baseline` after
  (`SAVE_BASELINE=1` / `BASELINE=1` for `precheck.sh`): both sides replay on
  the same index, so every difference is the engine's.
- **Some change sets it declines outright**, marking them indeterminate and
  leaving them out of the gate: one that edits `MODULE.bazel` (not in the
  index; `--repo <git dir>` settles it), one that edits a `.bazelrc` (whether
  that goes global depends on which flags moved, which needs the file's text at
  both revisions), and one that adds or removes a `BUILD` file (the package set
  at the base is not the package set at the pin, and the engine asks the head
  worktree whether a deleted `BUILD` really deleted its package). With `--repo`
  it also declines the two shapes of pin drift that are large enough to change
  the externals stage's whole input: a repository the stage dirtied that the
  base's `MODULE.bazel` does not declare, and a modified `.bzl` the stored load
  closure does not contain. envoy's three hundred history commits straddle a
  Bzlmod migration and produce both; without these, twenty-two of its forty
  change sets diverge and the run is right about eighteen of them.

A clean precheck is not a substitute for the run. It is what makes the run
worth starting.

## Choosing what to run

A change to cambium reruns only the workload it can affect:

| change | run |
|---|---|
| invalidation (what goes global, what gets dirtied) | `cambi bench history <subject> --oracle rebuild` on the subjects that exercise it |
| the walk, hashing, or the affected set | `cambi bench changes <subject> --kind pr --tools cambium --oracle rdeps` |
| a tool driver | `cambi bench changes <subject> --tools <tool> --change-sets 5` |
| an engine stage's cost | `cambi bench stages synth:medium` |
| any of the static stages, before any of the above | `tools/bench/precheck.sh` — offline, minutes, no Bazel |
| a timing claim against other tools | the full run on the benchmark machine, and only then |

Unit tests, the synthetic soundness sweep and the real-Bazel end-to-end tests
come before any of these. The benchmark is for numbers against other tools,
not for finding out whether a change works.
