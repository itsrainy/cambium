# How cambium is benchmarked

This page defines the words the benchmark uses and what each measurement
can and cannot claim. The numbers themselves live in [RESULTS.md](RESULTS.md).
Everything here is driven by `cambium-bench` (`crates/bench-cli`, over
`crates/bench`), which measures `cambi` by running it; real subjects are
described by `subject_repos.toml`, synthetic ones by the presets
`cambium-bench synth presets` lists.

## Concepts

**Subject.** A Bazel repository a benchmark points at. There are two kinds,
and every workload accepts either.

- *Real*: an open-source repository from `subject_repos.toml`, with a URL,
  a pinned commit, a history window (first-parent commits before the pin a
  workload may use), and a Bazel version: the subject's own
  `.bazelversion`, or a manifest pin only when it has none (apollo, a
  WORKSPACE-only repository, runs under 7.4.1 since Bazel 9 can't load
  it). A subject is never pinned twice for another Bazel major; Bazel
  7/8/9 coverage of cambium itself is CI's synthetic version matrix.
  Today: `bazel` and `envoy` on demand, `sourcegraph`, `apollo` and `xla`
  weekly, `tensorflow` (`skip = true`) monthly or by hand
  (`.github/workflows/bench-weekly.yml`).
- *Synthetic*: a generated graph from a preset, `synth:<preset>`
  (`synth:medium` is 4 551 packages, 50 167 rule targets, 250 507 index
  nodes counting source files), emitted as a real Bazel workspace with git
  history when a workload needs one, so the same commands run as on a
  real subject: cheap, deterministic, scales past any real subject, and
  says nothing about real Starlark, which real subjects are for.

**Tool.** Anything that answers "which targets could be affected by this
change": cambium, [bazel-diff](https://github.com/Tinder/bazel-diff) (the
Rust port, `bazel-diff-rust`), [Tango](https://github.com/uber/tango) and
[target-determinator](https://github.com/bazel-contrib/target-determinator).
Cambium is one of four in a comparison, not the thing being served;
invocation detail is in [Tools](#tools).

**Oracle.** An independent check named by what it proves. No oracle proves
precision; see [What is not measured](#what-is-not-measured).

| oracle | question it answers | how |
|---|---|---|
| reverse-dependency | did a tool miss a target? | `bazel query "kind(rule, rdeps(//..., set(<every changed file's label>)))"` on the head revision, over every changed path in a head package, batched by command-line length and unioned. cambium's answer must be a superset. A file *deleted* at head has no label, so its consumers come from the base index's file rows instead and must appear by label, by package, or in `dirty_packages` (the only spelling when a whole package is deleted, since every rule in it is tombstoned). Runs on every incremental change set; `--oracle-sample N` caps labels queried, an escape hatch for enormous subjects, not a mode a published number may use. |
| rebuild | is the incremental updater exact? | after a history run, a cold index built at the pin is diffed node by node, edge by edge and rule hash by rule hash against the index the history produced. Any difference fails. |
| load | is the static `.bzl` load closure complete? | on every change set touching a `.bzl`, Bazel's `rbuildfiles` must be a subset of the packages cambium dirtied. |

**Workload.** What tools are run on. There are two.

- *History.* The subject's last N first-parent commits, applied one at a
  time to an index built at the start of the window. Cambium-only: other
  tools keep no state between runs to replay. Yields per-commit cost, the
  global-invalidation rate, the cold-rebuild rate, and the rebuild oracle.
- *Change sets.* `(base, head)` pairs shaped like the question a CI job
  asks. Every tool runs on each, and the reverse-dependency oracle scores
  recall. Kinds, in order of importance:
  - **PR-shaped**: one pull request's worth of change. For a merge commit,
    `merge-base(parents)..commit`. For a squash- or rebase-merged history,
    where every first-parent commit *is* a pull request, `parent..commit`
    for commits whose message carries a PR reference (`(#1234)`, `PR
    #1234`, `Merge pull request`, or a trailing
    `(https://github.com/<owner>/<repo>/pull/1234)`, the form bazel's own
    import tooling writes). The primary kind; headline numbers come from it.
  - **adjacent**: `parent..commit` for first-parent commits with no PR
    reference, same shape as a squash-merged PR, kept separate so it
    doesn't dilute PR-shaped numbers with fix-ups.
  - **stress**: real commits chosen for what they touch: a `.bzl` file, a
    `BUILD` file added or deleted, a path under a high in-degree "hub"
    package (`crates/index`'s top-in-degree list), or a manifest
    invalidator path. Filed under the category that selected it, so a
    history that never deletes a `BUILD` file has no change set in that
    category; they reach paths a quiet stretch of history would not.

**Run.** One benchmark run is the tuple *(subject, workload, tools,
oracles, window size, machine)* and the identity of the cambium build that
ran it (index format version and configuration hash). Results are stored
under `bench-results/<subject>/<workload>/` with that identity inside, so
a run from another build never resumes into this one, and synthetic and
real results sit in one tree. Window size is a workload parameter
(`--commits 50`, `--change-sets 40`), not a mode: a short local run and
the published run differ only in size.

## Measurements

**Correctness.**

- *Recall* against the reverse-dependency oracle, per change set. Anything
  under 1.0 fails the run.
- *Agreement* with the other tools, with every disagreement filed under one
  class of the [disagreement taxonomy](#disagreement-taxonomy). Three
  classes fail the run: cambium reporting fewer targets than a tool or the
  oracle (`cambium_missing`), the load oracle finding a package the closure
  missed (`cambium_missing_bzl_closure`), and a disagreement no rule
  explains (`unclassified`). Every other class is an over-approximation or
  a known tool difference, reported but not gated. A change set also fails
  if a tool could not run at all, so the absence never reads as agreement.

**Cost.** Wall time per tool per change set, in two populations: *all
change sets* is the published number, including cambium's `//...`
fallbacks; *incremental* is the sets cambium answered with a walk, the
population precision and class counts are computed over. Cold and warm are
defined per tool:

| tool | cold | warm |
|---|---|---|
| cambium | the base index is not in the store and is built with one `bazel query //...` | the base index is in the store; only the changed packages are queried |
| bazel-diff | first run, no hash cache | second run on the same pair, its hash cache present |
| Tango | every run; Tango has no cache | (none) |
| target-determinator | every run; its result cache is turned off | (none) |

Every cambium time is a *whole-process* wall time, the `cambi` child
process from start to JSON serialisation. The engine's own
`timings_ms.total` travels beside it in every result file
(`cold_engine_ms`, `warm_engine_ms`, a history commit's `engine_total_ms`),
making overhead visible rather than hidden in engine cost. The timings
table prints both the walls and the engine's p50/p90 as *engine cold* and
*engine warm*; results predating `cambi`-as-child-process carry no engine
time and read `n/a`.

**Fallbacks.** Where cambium gave up precision.

- *Global rate.* The share of history commits and change sets cambium
  answered `//...`, with the reason for each (a `.bazelrc` flag not on the
  allowlist, a lockfile edit, a legacy `WORKSPACE`, an invalidator path).
  The tools still run on a global change set, and the report prices the
  fallback: cambium's answer size (every rule target in `//...`) against
  the precise set each tool found.
- *Cold-rebuild rate.* Change sets where the dirty package set exceeded
  `limits.max_dirty_packages` and cambium re-indexed the whole repository
  instead of splicing: an exact answer, entered into every statistic, at
  cold cost, with the count showing how often it happened.

## Tools

How each tool is invoked, for fairness:

- **cambium**: `cambi affected <base>..<head>` with the subject's generated
  `cambium.toml` (defaults plus a per-subject output base and store). The
  only subject-specific configuration is the manifest's invalidator list
  (paths whose change invalidates everything), a soundness escape hatch,
  not tuning. `bazel` and `envoy` have none; `sourcegraph`, `apollo`, `xla`
  and `tensorflow` each name a handful, every one in `subject_repos.toml`
  with its reason.
- **bazel-diff-rust** v46.1.0: `generate-hashes` at base and head in two
  worktrees, then `get-impacted-targets`, with `-k` (keep going) so a
  failed-to-load package doesn't abort the run. Externals are hashed as
  bazel-diff does by default.
- **Tango** v0.0.46: `tango-cli` built inside the upstream module with a
  small driver (`tools/tango`) running its library on a `(base, head)` pair
  and printing the impacted set, patched only to pass `--keep_going` to its
  Bazel queries, for the same reason.
- **target-determinator** v0.34.0: upstream's own binary (`tools/target-determinator`),
  run from a head worktree of its own with the base revision as its one
  positional argument. It is the only foil that works from the *configured*
  graph (one `bazel cquery` per revision, hashing configured targets) rather
  than from the loading phase, so a difference that is only a configuration
  difference is a real disagreement between shapes and not a harness
  artefact. Two of its defaults are overridden: a failed base query is a
  failure (`-before-query-error-behavior fatal`) rather than an answer of
  "everything is affected", and its cross-run result cache is off
  (`-nocache_results`) so a change set's cost does not depend on which change
  sets ran before it. It reports no distance, so it has no depth column.

All four see the same checkout, the subject's Bazel version, repository
cache, and stdin closed with `CI=1` set. Labels are compared after
canonicalising external repository names, with file targets dropped so
only rule targets are compared.

## What is not measured

- **Precision has no oracle.** Bazel has no query for "and nothing else
  could change"; bazel-diff, Tango and target-determinator are foils, and
  the taxonomy is our
  explanation of each difference, so "agreement" is weaker than "recall".
- **External repository contents.** cambium's graph stops at the main
  repository's edge. A dependency changing *by declaration* (a version
  bump in `MODULE.bazel`, a main-repo file a repository rule reads) is
  seen; a change in an external repository's contents at the same
  declared version is not. The hash-diffing tools fetch and hash
  externals, so on such a change they are right and cambium is wrong,
  filed under `cambium_missing`. None observed in the subjects' histories.
- **Configured builds.** All three tools work on the unconfigured target
  graph, keeping `select()` branches: cambium's largest over-approximation
  class.
- **The edge of the invariant.** Toolchain and platform declarations
  outside the invalidator globs, `.bazelignore`, autoloaded native rules
  and symlinked directories aren't modelled; the README's "Where the
  invariant stops" section is the authoritative list, with each case's
  status.

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
| `tool_extra_source_directory` | the target consumes a *directory* as a source file (`src_dir = ["."]`, rendered by Bazel as `//pkg:.`), and a changed path lies under it. Tango hashes the directory's contents on disk; Bazel has no edge from a file to its parent directory (documented untracked), so neither oracle nor cambium reaches the target. Filed only when the oracle missed the label too (otherwise `cambium_missing`). Reported, not gated: seen on every apollo change set touching an `install_src_files` package |
| `tool_missing` | the tool missed a target the reverse-dependency oracle confirms is affected: the tool's miss, not cambium's extra. Reported, not gated |
| `cambium_missing` | **fails**: a tool or the oracle reports a target cambium does not |
| `cambium_missing_bzl_closure` | **fails**: `rbuildfiles` names a package the static closure did not dirty |
| `unclassified` | **fails**: no rule explains it |

## Machines

Local runs (a laptop) are development evidence only; timings aren't
comparable, since some subjects' externals can't be fetched on macOS
x86_64 and a failed fetch is retried by bazel-diff and Tango every change
set. Published numbers come from one machine, named in RESULTS.md, with
Docker available (some subjects generate remote-execution configurations
by running containers) and every external fetchable.

## The commands

```
cambium-bench subjects            list, fetch, or cold-build a subject
cambium-bench history  <subject>  the history workload; rebuild oracle
cambium-bench changes  <subject>  the change-set workload across tools and oracles
cambium-bench stages   <subject>  engine stage micro-benchmarks (synthetic subjects)
cambium-bench sweep    <subject>  vary one parameter and report how a stage scales
cambium-bench regress  <a> <b>    compare two result files and fail on a regression
cambium-bench replay   <subject>  replay the static stages offline, against a stored index
cambium-bench labels   <subject>  the label sets a change-set comparison kept
cambium-bench watch               follow a run: one row per subject
cambium-bench report              regenerate RESULTS.md from bench-results/
```

`cambium-bench` with no verb, on a terminal, is the launcher: it composes
a run from the manifest's subjects, tools, workloads and a size, shows the
exact command lines it will run, then switches to the watch screen,
reimplementing no workload (the lines are the ones
`tools/bench/run-published.sh` runs).

`<subject>` is a name from `subject_repos.toml` or `synth:<preset>`. Every
workload takes `--tools`, `--oracle`, `--kind` and a size (`--commits`,
`--change-sets`). The nightly job, `cambium-bench changes synth:small
--tools cambium,bazel-diff`, is the same command a person runs; Tango is
left out only for its 30-500 s per change set, and the weekly job on real
subjects runs all three. target-determinator is a fourth foil and is never
in the default list: it is named explicitly
(`--tools cambium,bazel-diff,tango,target-determinator`) by a run that wants
it.

## Running it

```console
$ tools/bazel-diff/fetch.sh          # builds bazel-diff-rust at its pin, once
$ tools/tango/fetch.sh               # builds tango-cli at its pin, once
$ tools/target-determinator/fetch.sh # builds target-determinator at its pin, once
$ cambium-bench subjects fetch bazel   # mirror and check out the pin
$ cambium-bench subjects build bazel   # one cold `bazel query //...`
$ cambium-bench changes bazel --quick
```

The tool scripts cache their binaries under
`~/.cache/cambium/{bazel-diff,tango,target-determinator}` (override with
`$CAMBIUM_BAZEL_DIFF_DIR` / `$CAMBIUM_TANGO_DIR` /
`$CAMBIUM_TARGET_DETERMINATOR_DIR`); a run uses whichever is
installed and says so when one is missing, rather than refusing to
compare the other. Mirrors, worktrees, per-subject stores, emitted
synthetic workspaces and a *shared* Bazel `repository_cache` and
`disk_cache` live under `$CAMBIUM_SUBJECTS_DIR`, default
`~/.cache/cambium/subjects`, so subjects don't redownload each other's
externals.

`--quick` is 50 commits, 50 change sets and Tango on the first 10: a
bounded, deterministic slice that finishes on a laptop and is a subset of
what a full run selects at the same pin and seed. It sets three flags and
nothing else, so results land in the same tree, with sizes recorded.

Budget per concurrent real subject: 16+ cores (Bazel's own parallelism
dominates), 64+ GB of RAM (`envoy` and `xla` load a large query result
into one server process), and 500+ GB of disk. `bazel` is the smallest
entry, so a from-scratch contributor can exercise the pipeline on a
laptop.

## Engine stages

Underneath the workloads sit in-memory stage benchmarks, needing no Bazel
and measurable at sizes no real subject reaches: `build_index`,
`load_index`, `classify`, `parse_proto`, `rule_hash`, `splice`,
`reverse_rebuild`, `walk` / `walk_hub`, `e2e_memory`, `e2e_materialize`,
and `legacy_bazel_diff`.

```console
$ cambium-bench stages synth:medium
$ cambium-bench sweep  synth:medium --stage walk --param targets --values 1e4,1e6
$ cambium-bench regress a.json b.json --threshold 15
```

Results carry a machine fingerprint, and `cambium-bench regress` refuses
to compare across machines rather than report noise.

`legacy_bazel_diff` is the like-for-like stage: it emits the graph as two
real workspaces, writes the `streamed_proto` dump `bazel query` would
produce for each, and shims `bazel` in front of both tools to replay
those bytes, taking the Bazel server out of the measurement and leaving
each tool's own work (bazel-diff's own perf gate uses the same
technique). The stage fails outright if bazel-diff reports a label
cambium does not; the other direction is the `select()`
over-approximation above, and is reported. Its sweep draws the headline
chart (`bench-legacy.svg`).

## Validating an engine change before a run

A full run takes seven hours on a rented machine: too slow to be the
first signal an engine change moved an answer. Most of what such a change
touches needs no Bazel: the engine decides which external repositories
moved and by which rule, which packages that dirties, and whether the
dirty set exceeds `limits.max_dirty_packages` and cold-rebuilds. A
published run leaves the index at the pin behind, so all of this replays
offline, in milliseconds a change set.

Replay is the only part of the harness that doesn't run `cambi`: it links
the engine in process against a stored index, with no Bazel, checkout, or
process start, as do the stage micro-benchmarks (`stages`, `sweep`,
`regress`).

```console
$ tools/bench/precheck.sh                       # every cached subject
$ SUBJECTS="envoy apollo" tools/bench/precheck.sh
```

`precheck.sh` builds both binaries and runs two verbs per subject:
`cambium-bench pull-results` pulls the stored index, the base indexes,
recorded results and each change set's diff (from `cambium-bench replay
--write-paths`) into `bench-precheck/` (git-ignored, skipped once
cached); then the check:

```console
$ cambium-bench replay envoy --change-set b352846f..a6871fc6   # one change set
$ cambium-bench replay envoy --paths 'M\tbazel/repo.bzl'       # a hypothetical
$ cambium-bench replay envoy --all                             # exit 1 on divergence
```

Replay beats the pin where it can: a run never splices onto the index at
the pin, but takes the change set's own base from the store (an inert
`MODULE.bazel` read in WORKSPACE mode was invisible against the pin but
reproduced against that base). The driver archives each base at
`bench-results/<subject>/bases/<base>.bin`; `replay` prefers an archived
base, naming which it used and skipping the drift guards below for those
sets (`--index` overrides this). History commits still replay against
the pin, since nothing archives their bases.

Symmetrically: the rebuild oracle's fresh index at the pin is the only
artefact stating what Bazel's graph and externals tables *are* there, and
the run discards it. Archived at
`bench-results/<subject>/heads/<pin>.bin`, `cambium-bench replay <subject>
--head-check --repo <git dir>` re-derives externals tables from the head
tree's own text and runs the oracle's `compare_by_label`. Node, edge and
rule-hash halves are empty by construction, so the externals section is
the whole signal: the one an engine change to those tables moves.
`--rederive` only checks today's derivation against an earlier engine's
tables; `--head-check` checks it against Bazel's truth, in seconds on a
laptop, and `precheck.sh` runs it per subject with a head archive when
`REDERIVE=1`.

It prints: repositories dirtied and by which rule; packages dirtied and
by which route (a `.bzl`-row hit versus `pkg_ext`, issue #4); the
dirty-package count against the limit; and a diff against the run's
record.

Four blind spots, reported in its own output:

- **The answer needs Bazel.** `direct` and `affected` come from rule
  hashes after the head packages are re-queried; replay reports only
  `affected (lower bound)` and never gates on it.
- **A cold rebuild's answer needs Bazel.** Replay reports only that the
  fallback fires, not what `//...` would return.
- **The stored index is at the pin**, so a divergence from a recorded
  answer is a lead, not a verdict. The tight loop is `--save-baseline`
  before the change and `--baseline` after (`SAVE_BASELINE=1` /
  `BASELINE=1` for `precheck.sh`): both replay on the same index, so
  every difference is the engine's.
- **Some change sets are declined outright**: one editing `MODULE.bazel`
  (`--repo <git dir>` settles it), one editing a `.bazelrc` (needs the
  file's text at both revisions), one adding or removing a `BUILD` file
  (the engine asks the head worktree whether a deletion really removed
  the package). `--repo` also declines two shapes of pin drift large
  enough to change the externals stage's input: an undeclared dirtied
  repository, and a modified `.bzl` outside the stored load closure.
  envoy's three hundred commits straddle a Bzlmod migration and produce
  both; without these, twenty-two of its forty change sets diverge, and
  the run is right about eighteen.
- **It replays against a healthy index; a run does not.** A run's base
  keeps whatever the previous run published, including an index a
  since-fixed cambium derived wrongly. `FORMAT_VERSION` and
  `RULE_HASH_VERSION` don't move when only the derivation changes, so a
  fix to *what a correct index contains* must bump
  `cambium_index::CONTENT_REVISION` or the precheck stays clean while the
  run keeps splicing onto the broken base, as when envoy cold-rebuilt
  ~1100 packages after a path-mapping fix while the same sets replayed
  at 6 and 18.

### `--rederive`: an index-time change the stored tables cannot show

A fifth blind spot: the externals stage looks up which main-repo paths
feed external repositories in side tables (`bzl_repos`, per-repository
path maps, `target_only`, the repository set)
`cambium_index::build_repo_inputs` derived at index time from
`MODULE.bazel`, `WORKSPACE` and the workspace `.bzl` closure. A change to
that derivation is invisible to a plain replay, which reruns the stage
over the old stored tables.

```console
$ cambium-bench replay envoy --all --repo ~/.cache/cambium/corpus/envoy.git --rederive
$ REDERIVE=1 tools/bench/precheck.sh
```

`--rederive` replays each change set with those tables rebuilt from the
change set's *own base* (`git show <base>:MODULE.bazel` and its workspace
closure). It needs `--repo`; every outcome carries `rederived` in the
JSON and a `REDERIVED` marker, and costs a whole index rebuild per
distinct base (about 2.5 s on envoy's 7 MB index, 0.6 s on bazel's, so
forty change sets go from seconds to a minute or two), holding only a
few bases at once since `--history` walks three hundred of them.

It settles whether an index-time derivation change moves the externals
answer on the real corpus, and whether the current derivation still
matches what the run stored. It cannot see the package closures or set,
which are the pin's: only `bazel query` builds those, so `--rederive`
rebuilds only the text-derived tables and keeps every query-derived one.
A `.bzl` in the base's workspace closure the pin's index never had stays
invisible (the load-closure drift guard marks those indeterminate), and
the answer, as ever, needs Bazel.

Compare a re-derived run only against another: `--baseline` across the
two names which side diverged, since the difference is the derivation,
not the engine. A clean precheck is not a substitute for the run; it is
what makes the run worth starting.

## Choosing what to run

A change to cambium reruns only the workload it can affect:

| change | run |
|---|---|
| invalidation (what goes global, what gets dirtied) | `cambium-bench history <subject> --oracle rebuild` on the subjects that exercise it |
| the walk, hashing, or the affected set | `cambium-bench changes <subject> --kind pr --tools cambium --oracle rdeps` |
| a tool driver | `cambium-bench changes <subject> --tools <tool> --change-sets 5` |
| an engine stage's cost | `cambium-bench stages synth:medium` |
| any of the static stages, before any of the above | `tools/bench/precheck.sh` (offline, minutes, no Bazel) |
| a timing claim against other tools | the full run on the benchmark machine, and only then |

Unit tests, the synthetic soundness sweep and the real-Bazel end-to-end
tests come before any of these. The benchmark is for numbers against other
tools, not for finding out whether a change works.
