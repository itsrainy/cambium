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

Nine verbs; `cambi -h` fits one screen (27 lines, and no verb's `-h` runs
past 28; `--help` is the long form), and each verb's first help line says
what it's for.

### `affected`

The range is positional, like `git diff`: `cambi affected
origin/main..HEAD`. A single ref means `<ref>..HEAD`; no ref means the
default branch's merge base to `HEAD`. `--base`/`--head` remain aliases.
When `[git].default_branch` has no local branch here (a CI checkout
usually makes none), its remote-tracking branch `origin/<branch>` stands in,
and the narration says so. A revision the clone does not have is
`CAMBI-E003` naming it as typed, with the fetch that brings it in: a shallow
clone needs the base fetched into its remote-tracking ref, `git fetch origin
main:refs/remotes/origin/main --depth=N`, since `git fetch --depth=1 origin
main` can leave it only in `FETCH_HEAD`.

`--dry-run` prints the plan and exits: which index would load (revision,
lineage, age), whether the answer is a splice, cold rebuild or global, and
how many packages it would re-query. `status` shows it in short form. The
stages it runs are the run's own, up to but not including the query, so the
dirty set and every global the run reaches before querying are the plan's
too. The one verdict it cannot reach is the toolchain check (`CAMBI-W023`),
which reads rule classes off the rows the query returns: a `splice` or `cold
rebuild` plan can still end as a `//...` run, exit 2.

While it runs it narrates itself on stderr in the shared grammar below: an
`index` phase (where it looked for an index, and what it found: `found at
<sha>  (N commits behind, lineage <hex>, 2 d old)`, or `none within N
ancestors`, after which a `cold index` phase builds one, or `CAMBI-E006`
ends the run under `--no-full-build`), a `splice` phase for the replay up to the base
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
affected: <reason>` on stderr, and has no distribution to draw. For a
script, `--format labels` prints the labels alone, one per line (see the
output contract below).

Also takes: `--tests-only`, `--include-manual`, `--max-depth`, `--hist`,
`--no-full-build`, `--publish-base`, `--out`. `--explain` is kept one
release; the verb below replaces it.

`--out FILE` writes the head index the run built. A global run builds none
(the answer is `//...` and nothing is spliced to keep), and `affected` does
not pay for a cold build at head in its place, so on a global run `--out`
removes any older file at that path rather than leave it to be taken for this
run's head (a `cambi index publish FILE` after it would otherwise upload an
index for the wrong commit). The narration says `no index written to FILE`
and why, names the `cambi index update BASE..HEAD --out FILE` that builds one
cold, and the exit code is still 2. `--publish-base` is unaffected by a global
run: the base index it publishes describes the base, and is published under
the base revision before the head side is looked at; the head index is never
published from `affected`.

### `explain`

With a label: the shortest path back to the change, its length in hops and in
package boundaries crossed, and the direct reason at the end, in the `reason`
shape the JSON contract defines. With no label: why
the run went global or cold, which file/flag did it, and a `help:` line; or,
for a run that was neither, its counts (changed files, dirty packages,
direct and affected targets, the base index), with how many direct targets
`output.exclude_tags` dropped from `affected` when any were. A label just
narrows the same shape.

`explain` shares `affected`'s computation, range and flags, but isn't a flag
on it: output shape and audience differ. It narrates the same three phases on
stderr, for the same reason `affected` does.

### `profile`

The numbers for a proposal to adopt the tool, measured on the repo itself,
not on the benchmark's subjects: target count, cold build time and disk
size, incremental answer time on a typical commit, the share of the
repository's targets and tests a typical commit affects, and how often a
commit invalidates the whole graph. It needs no `cambium.toml`, and leaves
the checkout and the store alone, so it is the first thing to run on a new
repository.

It's an incremental walk over the repo's own history in one process: a cold
build at the oldest of the last `--commits N` first-parent commits (30
default), one incremental step per commit to `REV` (HEAD default), a cold
rebuild wherever a commit went global, counted as its cost. `FROM..TO` walks
from `FROM` to `TO` along `TO`'s first-parent line instead (`FROM..` means
`FROM..HEAD`), and takes no `--commits`. The header says how many commits
the walk covers and that the oldest is indexed cold first; that commit is
the likeliest not to build, and a smaller `--commits` or a `FROM..` range
moves it. Steps run in a detached worktree, never the working tree, one
Bazel server. Per-commit time is the engine's own (`timings_ms.total`, as
`affected` reports it), index held in memory between steps; opening the
index file, which every real `affected` pays first, is measured once,
printed beside it. The numbers exclude what a CI job starts with: they come
from one warm Bazel server across steps and the index in memory; a cold CI
runner also pays the store fetch, a server start and external fetches.

A commit that fails (`bazel query` cannot load it, a fetch fails) does not
end the profile. Its step is recorded as failed with the error, the next
commit is indexed cold and counted as a rebuild with that reason, and three
failures in a row stop the walk. Either way the report covers what was
walked. Only a walk in which nothing built at all exits with the error.

While it runs it narrates itself on stderr in the shared grammar below: a
`worktree` phase, a `cold index` phase with the target count it found, and a
`commits` phase with one row per commit (`commit / time / affected / tests /
index`, the commit as `sha7 MM-DD HH:MM subject`). The `index` column is what
the commit did to the stored index: `= unchanged` when it changed nothing the
graph models, `~ N rewritten` with the rows the splice actually wrote,
`rebuilt` when the commit went global (its `affected` and `tests` read
`all`), or `x failed`. With `--format jsonl` each `row` event also carries
`step`, the raw step with typed numbers, beside the display strings.

On stdout, `--report kv` (the default) is an aligned key/value block and the
table. The block reads: `repository`; `measured` (the warm-server sentence
above); `targets`; `full index` (where the cold index was built, its targets
and tests, size, load time); `per commit` (mean, p50, p90 and max over the
incremental steps, and the mean over every step with its rebuilds);
`affected` and `tests` (the median incremental commit's count and its share
of the cold index's targets or tests, manual targets included, with mean and
p90); `global` (how many commits answered `//...` and their share, then one
line per reason with its `CAMBI-W` code and `help:` line); `rebuilds`
(globals and `limits.max_dirty_packages` re-queries counted and timed apart);
`index work`; and `failed` and `stopped` when a commit failed. `--report
prose` is the paragraphs that block replaced, kept one release, and then the
same table. `--format json` is the document behind both, carrying every
figure the block shows (the JSON contract, version 17, lists the fields).
`--out <file>` keeps the walk's final index (`REV`'s, or the last commit
that built), `cambi index publish`-able, so a profile doubles as the first
cold build; without it, the index goes to a deleted scratch file.

### `index`

The stored artifact is an index; the chain of them is a lineage. `build
[REV]` is the cold build minting a lineage; `update FROM..TO` brings one
forward (bare `FROM` means `FROM..HEAD`). Both need `--out <file>`; only a
run also `--publish`ing may omit it (indexes run gigabytes). `publish`
uploads one, `list` shows the store's holdings, `show` prints an index's
header/counts, `gc` removes the lineages this `cambi` cannot read (another
`cambium.toml`, an older index format; `--keep-incompatible` keeps them) and
applies retention (`[baseline].retention_days`, by age; `--keep <key>`
spares one key; a directory store only, see
[STORE.md](STORE.md#garbage-collection)). These were the `baseline` verbs,
renamed; `baseline` survives one release as a deprecation alias.

### `inspect`

Graph questions against one index file: reverse/forward dependencies,
shortest path between two targets, which rule consumes a file, which
package owns a path, a package's targets, byte accounting, externals
tables, and `verify` against Bazel. Plumbing, for checking a surprising
`explain` answer without reading the index by hand. Each takes the index
file first. A package is written as a label writes it, `//pkg` (or `pkg`,
and `//` for the root package), and `@repo//pkg` in an external
repository: `cambi inspect targets FILE //shell` lists what `//shell`
holds.

`inspect verify FILE` samples rule targets (`--sample`, `--seed`) and
checks that the index's reverse closure of each holds every target `bazel
query rdeps(//..., target)` reports, run against the checkout as it is now.
Both sides are compared as main-repository rule targets, because that is
what `affected` reports: the source and generated files between rules (a
stardoc `.md`, a `.binaryproto`) and targets in external repositories are
counted and left out. A sample with a miss prints `UNSOUND` and every
missing target (`--limit n` lists n and counts the rest), and the command
exits 1. Misses Bazel connects to the sample only through an external
repository (`@bazel_tools//tools/test:collect_coverage` depending back on
the root module's `//shell/runfiles`, say) are listed apart as round trips,
naming the external targets on the path. An index carries those paths as
edges of the external label, asked of Bazel when it is built, so a round
trip listed here is an index without them: one written before they
existed, or one whose question Bazel did not answer. A run answers `//...` under
`CAMBI-W023` when the row of a toolchain-class rule (`toolchain_type`,
`toolchain`, `platform`, `constraint_setting`, `constraint_value`) moves,
and a sample of one of those classes that depends in the index on nothing
but other toolchain-class rules cannot be reached by any change that does
not move such a row. Its misses (the rdeps toolchain resolution adds) are
covered by that answer, so it prints `ESCALATED` rather than `UNSOUND` and
does not fail the check. A toolchain-class sample that depends on anything
else (a `toolchain` whose `target_settings` names a `config_setting`, say)
can be reached by a change that fires nothing, and its misses fail like any
other. Precision is the share of the targets the index reported that Bazel
also reports.

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
  unreadable file a stage had to answer without.

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
only the store, via a flag per backend (`--store-dir PATH`, `--store-s3
BUCKET/PREFIX`, `--store-gcs BUCKET/PREFIX`, `--store-http URL`,
`--store-none`). `--store-dir` writes the path as given; a relative one is
read from the workspace root. `--yes` takes the rest's defaults; an
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
`baseline.compress_indexes`, `baseline.publish_partial`, excluded from the
config hash so machine-local overrides can't falsely claim checkouts
interchangeable; any other key is `CAMBI-E014`, naming key and file. One
exception, and it cuts the same way: the startup flags that decide which
files are rc files at all (`--bazelrc=`, `--noworkspace_rc`) are hashed,
because under `--noworkspace_rc` the root `.bazelrc` is a source file the
run may skip. A machine that sets them gets its own lineage rather than
splicing onto one built without them.
`--cache-dir`, `--bazel-binary`, `CAMBIUM_CACHE_DIR` and `--config` take
precedence over both files.

`cambi config explain` prints every key with its effective value and where
it came from, defaults included, which is the full list of sections and
defaults.

### The store

`[baseline].store` says where indexes are published and fetched. One of:

```toml
[baseline]
store = { kind = "dir", path = ".cambium-store" }
# or
store = { kind = "http", url = "https://indexes.example.com/cambium", headers = ["Authorization: Bearer <token>"] }
# or, for S3, GCS or any CLI (the full templates are in docs/STORE.md)
[baseline.store]
kind = "command"
fetch_cmd = ["aws", "s3", "cp", "--only-show-errors", "s3://my-bucket/cambium/{name}", "{path}"]
push_cmd = ["aws", "s3", "cp", "--only-show-errors", "{path}", "s3://my-bucket/cambium/{name}"]
list_cmd = ["aws", "s3", "ls", "--recursive", "s3://my-bucket/cambium/"]
not_found_exit = 1
```

- `dir`: a relative `path` is read from the workspace root, not the working
  directory, so `-C` reaches the same store. A directory that does not
  exist yet is an empty store (said once as `CAMBI-W007`), and the first
  `--publish` creates it. A symlink to nothing is not: it reads as
  `CAMBI-E011`, exit 5.
- `http`: `GET` and `PUT` on `url` + `/` + key. `headers` are
  `Name: value` strings, used as written (no environment expansion). HTTP
  cannot list, so it only finds indexes a publisher wrote under the
  lineage-less keys: a publisher that can list writes those only with
  `baseline.lineage_less_copies = true`.
- `command`: `{name}` and `{path}` substituted in each argument;
  `not_found_exit` is the fetch exit status meaning "no such object". A
  failing fetch is reported from what the command prints on stderr, so keep
  errors there: `aws s3 cp --quiet` prints nothing when it fails, and an
  outage then reads as a miss with nothing said; `--only-show-errors`
  hides the progress and keeps the errors.

Other `[baseline]` keys: `max_ancestor_walk` (20), `retention_days` (30,
what `cambi index gc` keeps), `cache_dir`, `compress_indexes` (true),
`publish_partial`, `lineage_less_copies` (false). None is in the config
hash, so a CI job can point at another store with `--config <file>` and
still splice onto the same lineage (the `cambium.local.toml` read is then
the one beside that file). [docs/STORE.md](STORE.md) has the key
layout, what each backend reads as a miss and as an outage, and what `gc`
keeps.

Every verb takes the same global options, declared once on `cambi`: `-C,
--workspace <dir>` (default cwd), `--config <cambium.toml>`, `--bazel-binary
<binary>` (overrides config, `$BAZEL_REAL`), `--cache-dir <dir>` (overrides
`baseline.cache_dir`, `$CAMBIUM_CACHE_DIR`), `--log
<error|warn|info|debug|trace>` (also `$CAMBIUM_LOG`, explicit wins),
`--format`, `--quiet`, `--color`.

## The output contract

- `--format human` (the default, on a terminal or not), `json` (one
  document), `jsonl` (one event per line, so a watcher can follow), `labels`
  (one label per line and nothing else, `affected` only), `query` (a
  `set(...)` expression for `bazel query`, `affected` only), `null` (exit
  code only). `--output` stays an alias one release. No field selection:
  `--format json`/`jq` are the whole story.
- `labels` is the format for CI: `bazel test --target_pattern_file` reads it
  as it is. It prints the same labels `human` lists under its count line (the
  tests alone under `--tests-only`), with no header, blank line or chart; an
  empty answer is an empty stdout, and a global one is the single line
  `//...` with exit 2, which the same file runs as everything. `query`'s
  `set(...)` is a query expression, not a target pattern: `bazel build` and
  `bazel test` reject it.

  ```sh
  rc=0; cambi affected origin/main --tests-only --format labels > targets.txt || rc=$?
  case $rc in 0|2) [ ! -s targets.txt ] || bazel test --target_pattern_file=targets.txt ;; *) exit $rc ;; esac
  ```

  The `|| rc=$?` keeps a `set -e` shell (a GitHub Actions `run:` step is
  one) from stopping at exit 2 before the `case` reads it. An empty file is
  skipped because `bazel test` with no targets fails.
- A reader that closes stdout early (`cambi ... | head`) ends the run
  quietly with exit 0: the answer was computed and the reader took what it
  wanted, and a non-zero exit would fail a `set -o pipefail` pipeline whose
  consumer succeeded.
- Results to stdout; progress and diagnostics to stderr, and nothing at all
  on stderr on success under `--quiet`. A non-TTY stderr is not silent: it
  loses the live line, not the run. The narration below, `--log` lines
  and the diagnostics are coloured by one rule: only when stderr is a TTY,
  never under `NO_COLOR`, `CLICOLOR=0` or `TERM=dumb`, always under
  `--color always` or `CLICOLOR_FORCE`.
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
- Every `--format json` document carries `schema`, the contract version,
  17 today. It is shared by every verb's document, so one bump moves all;
  the README's JSON contract has the field tables and says what each
  version added (`affected[].depth` and `depth_histogram` in 3 and 10,
  their package-boundary twins `package_depth` and
  `package_depth_histogram` in 11, and so on).
- Exit codes are few and never change meaning:

  | code | meaning |
  |---:|---|
  | 0 | success, including "nothing affected" |
  | 1 | error, or a usage error `cambi` diagnoses (`CAMBI-E013`) |
  | 2 | global invalidation; everything is affected |
  | 3 | Bazel failed, or no Bazel workspace was found |
  | 4 | no index available and `--no-full-build` was given |
  | 5 | the store is there and could not be read |

  A command line the argument parser rejects before `cambi` runs (an
  unknown flag or verb, a missing value) exits 1 with the parser's message
  and its `--help` hint on stderr and nothing on stdout: it is an error,
  and 2 is only ever the global answer. `--help` and `--version` exit 0.

  0-4 keep their numbers. 5 is new: a store that is there and cannot be
  read differs from one with no index; CI retries the first, builds for the
  second. A directory store whose directory does not exist is the second
  (it has never been published to); one whose path is a file, or sits
  under one, or is refused, or is a symlink to nothing, is the first. With the `aws s3 cp` and `gsutil
  cp` templates an outage exits 1 like a miss, so it reads as the second;
  [docs/STORE.md](STORE.md#misses-and-outages) has the per-backend table
  and a fetch wrapper that tells them apart.

## Diagnostics

Every warning and error has a stable code, a one-line message, an optional
location, and a `help:` line naming what to change. Human form follows `rustc`:

```
warning[CAMBI-W004]: .bazelrc changed and the run went global: .bazelrc: common --repo_env changed, not on the loading-irrelevant allowlist
  --> .bazelrc
  = help: add the flag to [bazelrc].allowlist_extra if it cannot change what `bazel query` loads; a flag under a --config the query does not run with is already ignored, and [bazelrc].configs names the ones it does run with
```

The location is a file, and a line when the diagnostic knows one; no
source excerpt is printed.

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
`human`. `affected --write-index` is also still accepted for `--out`,
without a diagnostic. `bench`/`synth` were removed outright, never aliased: never part of
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
cambium-bench pull-results           # copy a run's results off the machine that ran it
cambium-bench labels   <subject>     # the label sets a step kept
cambium-bench watch                  # follow a run
cambium-bench report                 # regenerate docs/RESULTS.md
```

The verbs `history`, `changes`, `stages`, `sweep`, `regress` and `synth`
were removed: one workload (the walk) replaced the two, and
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
(small/medium/large, large being the full-size run at `--tier full`), exact
command lines shown before anything runs. On confirm it spawns those lines
(ordinary `cambium-bench walk ...`) as child processes, switching to the
watch screen with stderr tail in the footer; `q` asks before killing a
child. It reimplements no workload. `cambium-bench run` is the same plan
from flags (`--subject`, `--steps`, `--smoke`, `--resume`, `--fresh`,
`--require-sha`), what the full-size run script calls (docs/OPERATIONS.md), defined once in
`crates/bench/src/plan.rs`. In a pipe, no verb is the usual usage error.

Recorded results are never continued or discarded unasked. Over a results
directory already holding results, `run` refuses unless given `--resume` or
`--fresh` (moves it aside to `<dir>-<timestamp>`), the launcher's review
asking via `r`/`f`. `walk` alone refuses a result recorded for the pin
unless given `--resume` or `--restart`; a `--resume` that can't continue
(another build's result, range) fails, not restarts.

`cambium-bench pull-results` copies a subject's results off the machine that ran them for
offline replay: the index at the pin, recorded results, archived base/head
indexes, and the diff of every recorded change set. `cambium-bench replay`
reruns the engine's static stages over them, no Bazel, no checkout,
milliseconds a change set, against recorded answers. Diffs are written
where the run happened, by `cambium-bench replay --write-paths` from the subject's git
mirror, so a replay elsewhere gets every change set, repo-free.
`tools/bench/precheck.sh` is that sequence; `docs/OPERATIONS.md` says when.
