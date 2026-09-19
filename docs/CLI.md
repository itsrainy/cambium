# The `cambi` command line

The reference for `cambi`'s command line: verbs, rules each follows, and
what a caller may rely on. JSON field tables and contract history are in
the README; this is the surface a caller types.

## What the tool is for

`cambi` answers one question: given two revisions of a Bazel repo, which
targets could a build or test see differently. Everything here asks that,
keeps the indexes fast, or explains a surprising answer. The benchmark
harness and synthetic workspace generator are not in this binary (see "The
benchmark tools" below).

The surface is organised around intent, not crates, for three users: a
developer at a terminal, a CI job wanting a stable document and exit code,
and someone (or an agent) asking "why" wanting something short and
structured.

## The verbs

```
cambi                       # no arguments: `cambi status`
cambi init                  # write cambium.toml, asking only what it cannot detect
cambi status                # one screen: where am I, what index would a run use
cambi doctor                # can a run work here; what is wrong and how to fix it
cambi affected [BASE..HEAD] # the product
cambi explain [LABEL]       # why is this target affected; or, why did the run go global
cambi profile [REV]         # what cambium costs and saves here, measured over the last N commits
cambi index {build, update, publish, list, show, gc}     # the stored indexes
cambi inspect {target, package, deps, rdeps, path, owner, consumers, targets,
                stats, externals, verify}
cambi config {show, explain, hash}
```

Nine verbs; `--help` fits one screen, each verb's first help line says what
it's for.

### `affected`

The range is positional, like `git diff`: `cambi affected
origin/main..HEAD`. A single ref means `<ref>..HEAD`; no ref means the
default branch's merge base to `HEAD`. `--base`/`--head` remain aliases.

`--dry-run` prints the plan and exits: which index would load (revision,
lineage, age), whether the answer is a splice, cold rebuild or global, and
how many packages it would re-query. `status` shows it in short form.

While it runs it narrates itself on stderr in the shared grammar below: an
`index` phase (where it looked for an index, and what it found: `found at
<sha>  (N commits behind, lineage <hex>, 2 d old)`, or `none within N
ancestors, building cold`), a `splice` phase for the replay up to the base
with a row in `profile`'s five columns, or `skipped (index already at base)`
/ `skipped (cold index is at base)` when there was nothing to replay, and a
`query` phase with the changed files, the packages they dirtied and what the
walk reached. On a terminal the ancestor search counts itself off on the live
line; a pipe gets one milestone line instead. The replay is a single row
because it is a single pass: the dirty set over `ancestor..base` is the union
of the range.

On stdout: a count line (`1,284 affected targets  (96 tests)  max depth 7
(3 package hops)`), the labels, then two distributions: targets by depth and
targets by package hops. `--hist columns` (the default) draws them as
vertical column charts, `--hist bars` as horizontal bars with the count after
each, `--hist none` not at all. Depth is reverse-dependency hops from the
change, 0 being a target whose own inputs moved. Package hops are the same
walk counted in package boundaries crossed: a hop inside one package is free,
so a target nine hops down a chain in one package is one package hop away.
The two charts have different axes, which is why they are not one chart.
`--tests-only` draws the tests' own distances, so the charts always match the
list above them. A global run prints `//...`, says `everything is
affected: <reason>` on stderr, and has no distribution to draw.

Also takes: `--tests-only`, `--include-manual`, `--max-depth`, `--hist`,
`--no-full-build`, `--publish-base`, `--out`. `--explain` is kept one
release; the verb below replaces it.

### `explain`

With a label: the shortest path back to the change, its length in hops and in
package boundaries crossed, and the direct reason at the end, in the `reason`
shape the JSON contract defines. With no label: why
the run went global or cold, which file/flag did it, and a `help:` line. A
label just narrows the same shape.

`explain` shares `affected`'s computation, range and flags, but isn't a flag
on it: output shape and audience differ. It narrates the same three phases on
stderr, for the same reason `affected` does.

### `profile`

The numbers for a proposal to adopt the tool, measured on the repo itself,
not the benchmark corpus: target count, cold build time and disk size,
incremental answer time on a typical commit, how often a commit invalidates
the whole graph.

It's an incremental walk over the repo's own history in one process: a cold
build at the last `--commits N` first-parent commits (30 default), one
incremental step per commit to `REV` (HEAD default), a cold rebuild wherever
a commit went
global, counted as its cost. Steps run in a detached worktree, never the
working tree, one Bazel server. Per-commit time is the engine's own
(`timings_ms.total`, as `affected` reports it), index held in memory
between steps; opening the index file, which every real `affected` pays
first, is measured once, printed beside it.

While it runs it narrates itself on stderr in the shared grammar below: a
`worktree` phase, a `cold index` phase with the target count it found, and a
`commits` phase with one row per commit (`commit / time / affected / tests /
index`, the commit as `sha7 MM-DD HH:MM subject`). The `index` column is what
the commit did to the stored index: `= unchanged` when it changed nothing the
graph models, `~ N rewritten` with the rows the splice actually wrote, or
`rebuilt` when the commit went global.

On stdout, `--report kv` (the default) is an aligned key/value block and the
table; `--report prose` is the paragraphs that block replaced, kept one
release, and then the same table. `--format json` is the same document as
ever, with two fields added per commit: `date` and `index` (`{"kind":
"identical"}`, `{"kind": "spliced", "rows_rewritten": N}` or `{"kind":
"rebuilt", "reason": "..."}`). `--out <file>` keeps the walk's final index
(`REV`'s, `cambi index publish`-able), so a profile doubles as the first cold
build; without it, the index goes to a deleted scratch file.

### `index`

The stored artifact is an index; the chain of them is a lineage. `build
[REV]` is the cold build minting a lineage; `update FROM..TO` brings one
forward (bare `FROM` means `FROM..HEAD`). Both need `--out <file>`; only a
run also `--publish`ing may omit it (indexes run gigabytes). `publish`
uploads one, `list` shows the store's holdings, `show` prints an index's
header/counts, `gc` applies retention. These were the `baseline` verbs,
renamed; `baseline` survives one release as a deprecation alias.

### `inspect`

Graph questions against one index file: reverse/forward dependencies,
shortest path between two targets, which rule consumes a file, which
package owns a path, a package's targets, byte accounting, externals
tables, and `verify` against Bazel. Plumbing, for checking a surprising
`explain` answer without reading the index by hand.

`inspect stats` reports the Skyframe loading layer alongside the target
graph: its node and edge counts, a line per node kind (`FILE_STATE`, `FILE`,
`DIRECTORY_LISTING_STATE`, `DIRECTORY_LISTING`, `GLOB`, `PACKAGE_LOOKUP`,
`PACKAGE`, `BZL_LOAD`, `IGNORED`), and its share of the bytes. An index
built without a dump says `none` there rather than zeros, because that is
an index no incremental run can be answered from, and whoever is reading
the line is the one who has to rebuild it.

### `status`

Read-only and fast: no Bazel, no network beyond a store listing; an
unreachable store gets one line, the rest still shows. In order of
frequency:

- **Workspace**: root, Bazel version from the pin, WORKSPACE/bzlmod mode,
  which `cambium.toml` was read and its hash.
- **Position**: checked-out revision, whether an index exists, else nearest
  ancestor with one and commits back ("warm or cold?").
- **Index**: the index a run would load: size, packages, targets, external
  repositories, when built, by which `cambi`.
- **Lineage**: the index's lineage and head; whether a stale/incompatible one
  sits alongside it (config-hash mismatch, older engine id), named here
  before `doctor` has to.
- **Store**: backend, location, reachable or not, index count.
- **Warnings**: anything sending the next run global or cold: an
  unclassifiable WORKSPACE statement, an off-allowlist `.bazelrc` flag, an
  unreadable file the fixpoint fell back on.

`status` reports; `doctor` judges, keeping `status` quiet.

### `doctor`

What a new user runs first, and a bug report asks for: Bazel found and
version supported; workspace mode consistent (an inert `MODULE.bazel`
beside WORKSPACE named, not guessed); config parses, every value's origin
known; store answers a probe fetch/push; index format current; packages
`bazel query` couldn't load last time. Each finding is a diagnostic with a
code and `help:` line. Exit 0 if a run can work, 1 otherwise.

### `init`

Writes `cambium.toml` and nothing else: no index build, no store touch
beyond the same probe `doctor` makes, no credential handling. Store backends
are a directory, an HTTP base, or a command template driving `aws
s3`/`gsutil` under whatever credentials the environment already has.

It detects what it can (`.bazelversion`, WORKSPACE vs bzlmod, an inert
`MODULE.bazel`, `.bazelrc` `try-import`s, default branch), shows it, asks
only the store, via a flag per backend (`--store-dir`, `--store-s3
bucket/prefix`, `--store-http`). `--yes` takes the rest's defaults; an
existing file's asked-about keys get edited, then the next command prints.

### `config`

`show` prints the effective configuration; `hash` prints the hash every
index carries; `explain` prints every value and its origin (file, flag, env
var, default); the local override (below) is its own origin.

## Configuration

`cambium.toml` at the workspace root is the contract, checked in: Bazel
flags, invalidators, ignore globs, the `.bazelrc` allowlist and the configs
the index query runs with, store, limits.
Hashed into every index produced; a hash mismatch blocks a splice.

Beside it, gitignored, `cambium.local.toml` holds what's local, not the
graph: only `bazel.binary`, `bazel.startup_flags`, `baseline.cache_dir`,
`baseline.compress_indexes`, `baseline.publish_partial`, each excluded from
the config hash so machine-local overrides can't falsely claim checkouts
interchangeable; any other key is `CAMBI-E014`, naming key and file.
`--cache-dir`, `--bazel-binary`, `CAMBIUM_CACHE_DIR` and `--config` take
precedence over both files.

Every verb takes the same global options, declared once on `cambi`: `-C,
--workspace <dir>` (default cwd), `--config <cambium.toml>`, `--bazel-binary
<binary>` (overrides config, `$BAZEL_REAL`), `--cache-dir <dir>` (overrides
`baseline.cache_dir`, `$CAMBIUM_CACHE_DIR`), `--log
<error|warn|info|debug|trace>` (also `$CAMBIUM_LOG`, explicit wins),
`--format`, `--quiet`, `--color`.

## The output contract

- `--format human` (default on a TTY), `json` (one document), `jsonl` (one
  event per line, so a watcher can follow), `query` (the Bazel `set(...)`
  expression, `affected` only), `null` (exit code only). `--output` stays an
  alias one release. No field selection: `--format json`/`jq` are the whole
  story.
- Results to stdout; progress and diagnostics to stderr, and nothing at all
  on stderr on success under `--quiet`. A non-TTY stderr is not silent: it
  loses the live line, not the run. Colour only when stderr is a TTY, never
  under `NO_COLOR`, `CLICOLOR=0` or `TERM=dumb`, always under `--color
  always` or `CLICOLOR_FORCE`.
- A long run narrates itself on stderr in one grammar, the same for every
  verb that has one (`profile` is the first on it): a
  header line `cambium <verb>  <repo>  <range>  <note>`, a phase line per
  stage (`  <name> <dots> <result>  <time>`, or `skipped (<reason>)`), and
  rows inside a phase in fixed columns that never reach past 96 of them. On a
  terminal there is also one live bottom line, `<spinner> <what it is doing>
  <m:ss>`, erased before anything else is printed so it never interleaves
  with a row, and a phase that has printed no rows has its result written
  back over its own start line. The line redraws itself about ten times a
  second whether or not the run has anything new to say, so a minute inside
  one Bazel query still turns and still counts; the elapsed beside it is how
  long the step on the line has been running, not the whole process. A pipe,
  a CI log (`CI` set) and `TERM=dumb`
  get the same bytes without that line, with both of each phase's lines
  printed; `--format jsonl` gets one JSON object per event on stderr
  instead; `--quiet` gets nothing.
- `affected`'s JSON gained two things with contract version 10 and version 3
  before it, both additive: `affected[].depth`, the hops from the change at
  which a target was first reached, and `depth_histogram`, those depths
  counted, one bin per depth from 0 to the deepest reached. Version 11 adds
  the same pair in package boundaries crossed, `affected[].package_depth`
  and `package_depth_histogram`, a target's never exceeding its `depth`.
  Nothing else on stdout changes under `--format json`.
- Every document carries `schema`. `affected`/`inspect` keep the existing
  `SCHEMA_VERSION`; `status`, `explain`, `doctor` start there too, since
  it's shared and one bump moves all.
- Exit codes are few and never change meaning:

  | code | meaning |
  |---:|---|
  | 0 | success, including "nothing affected" |
  | 1 | error, or usage |
  | 2 | global invalidation; everything is affected |
  | 3 | Bazel failed |
  | 4 | no index available and `--no-full-build` was given |
  | 5 | store unreachable |

  0-4 keep their numbers. 5 is new: an unreachable store differs from one
  with no index; CI retries the first, builds for the second.

## Diagnostics

Every warning and error has a stable code, a one-line message, an optional
location, and a `help:` line naming what to change. Human form follows `rustc`:

```
warning[CAMBI-W004]: .bazelrc changed and the run went global
  --> .bazelrc:14
   |
14 | common --repo_env=CC=clang
   |        ^^^^^^^^^^ --repo_env is not on the loading-irrelevant allowlist
   = help: add it to [bazelrc].allowlist_extra if it cannot change what `bazel query` loads
```

Under `--format json` it's one object,
`{"code", "level", "message", "location", "help"}`, in the `diagnostics`
array; under `jsonl`, its own event. Codes: `CAMBI-E` errors, `CAMBI-W`
warnings, numbered once, never reused, each listed with meaning and fix in
`docs/DIAGNOSTICS.md`. A code covers anything actionable; a log line is not
one.

## Migration

Every earlier spelling keeps working one release, printing a deprecation
diagnostic naming the new one (`CAMBI-W001: cambi baseline build is now cambi
index build`). `--output` became `--format`, `baseline` became `index`, old
`index` queries became `inspect`, `--explain` became `explain`, `text` became
`human`. `bench`/`synth` were removed outright, never aliased: never part of
the contract, their only caller was the repo's own driver; both live in
`cambium-bench` now. `cambi bench ...` is an ordinary unknown-subcommand
error from clap.

## The benchmark tools

The harness (`subjects`, `walk`, `run`, offline `replay` and the
`pull-results` feeding it, `labels`, `watch`, `report`, the subject
manifest, and the synthetic generator that `crates/synth` still provides as
a test fixture) is its own binary, `cambium-bench` (`crates/bench-cli`),
built from the `bench`/`synth` crates. It drives `cambi` by shelling out:
`cambi index build`/`update`/`publish`, and `cambi affected --format json`,
read via the engine's own report types. `replay` links the engine directly
instead, being white-box: it replays the static stages against a stored
index with no product run to measure. Its verbs:

```
cambium-bench                 # no verb, on a terminal: the launcher
cambium-bench subjects {list, fetch, build}
cambium-bench walk     <subject>     # the walk: one workload, one tier
cambium-bench run                    # the whole plan from flags
cambium-bench replay   <subject>     # the static stages, offline
cambium-bench pull-results           # copy a run's results off the box
cambium-bench labels   <subject>     # the label sets a step kept
cambium-bench watch                  # follow a run
cambium-bench report                 # regenerate docs/RESULTS.md
```

The verbs `history`, `changes`, `stages`, `sweep`, `regress` and `synth`
were removed with issue #14: one workload (the walk) replaced the two, and
the in-memory stage micro-benchmarks were retired. `synth:<preset>` is
still a walk target, and the launcher still offers the presets, but there
is no `synth` verb. What the walk measures and may claim is
[METHODOLOGY.md](METHODOLOGY.md). The live matrix is that binary's front
door, not a `cambi` verb, following the output rules here voluntarily.

`cambium-bench watch` is that view: one row per subject (step, progress,
commit/change set and tool in hand, elapsed, ETA from the p50 of what's
done, globals so far, worst recall, pass count), each subject's last error
and diagnostics underneath. `enter` expands a row into its change sets, `q`
quits. A finished/idle row whose state file another engine build wrote
reads `stale`, not `done`. A pure reader: workloads write
`bench-results/<subject>/progress.json` atomically each step, the watcher
polling it (`--interval`, default a second), following a run in progress,
finished, or copied elsewhere. `--once` prints the table as text and exits;
`--once --format json` returns the snapshot document (`{schema, results_dir,
at, subjects}`).

`cambium-bench` with no verb, on a terminal, is the launcher composing a
run: the manifest's subjects, the synthetic presets, the peers, and a size
(small/medium/large, large being the box run at `--tier full`), exact
command lines shown before anything runs. On confirm it spawns those lines
(ordinary `cambium-bench walk ...`) as child processes, switching to the
watch screen with stderr tail in the footer; `q` asks before killing a
child. It reimplements no workload. `cambium-bench run` is the same plan
from flags (`--subject`, `--steps`, `--smoke`, `--resume`, `--fresh`,
`--require-sha`), what `tools/bench/run-published.sh` calls, defined once in
`crates/bench/src/plan.rs`. In a pipe, no verb is the usual usage error.

Recorded results are never continued or discarded unasked. Over a results
directory already holding results, `run` refuses unless given `--resume` or
`--fresh` (moves it aside to `<dir>-<timestamp>`), the launcher's review
asking via `r`/`f`. `walk` alone refuses a result recorded for the pin
unless given `--resume` or `--restart`; a `--resume` that can't continue
(another build's result, range) fails, not restarts.

`cambium-bench pull-results` copies a subject's results off the box for
offline replay: the index at the pin, recorded results, archived base/head
indexes, and the diff of every recorded change set. `cambium-bench replay`
reruns the engine's static stages over them, no Bazel, no checkout,
milliseconds a change set, against recorded answers. Diffs are written on
the box by `cambium-bench replay --write-paths` from the subject's git
mirror, so a replay elsewhere gets every change set, repo-free.
`tools/bench/precheck.sh` is that sequence; `docs/OPERATIONS.md` says when.
