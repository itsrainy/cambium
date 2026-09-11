# The `cambi` command line

The surface `cambi` is moving to, and the rules every verb follows. This is
the design for issue #2; it replaces the proposal in that issue's body where
the two differ. The JSON field tables and the contract history stay in the
README; this document is about the verbs, the diagnostics, and what a
caller may rely on.

## What the tool is for

`cambi` answers one question: given two revisions of a Bazel repository,
which targets could a build or test see differently. Everything on the
surface exists to ask that question, to keep the indexes that make the answer
fast, or to explain the answer when it surprises someone. Nothing else
belongs here. In particular the benchmark harness, the synthetic workspace
generator, and the corpus tooling are not part of the product and leave the
binary (see "The benchmark tools" below).

The surface is organised around what someone is trying to do, not around
the crates. Three people use it: a developer at a terminal who wants to know
what to run, a CI job that wants a stable document and a meaningful exit
code, and someone (or some agent) debugging an answer who wants to ask "why"
and get something short and structured.

## The verbs

```
cambi                       # no arguments: `cambi status`
cambi init                  # write cambium.toml, asking only what it cannot detect
cambi status                # one screen: where am I, what index would a run use
cambi doctor                # can a run work here; what is wrong and how to fix it
cambi affected [BASE..HEAD] # the product
cambi explain [LABEL]       # why is this target affected; or, why did the run go global
cambi index {build, update, publish, list, show, gc}     # the stored indexes
cambi inspect {deps, rdeps, path, owner, consumers, targets, stats, externals, verify}
cambi config {show, explain, hash}
```

Eight verbs. Top-level `--help` fits on one screen, and every verb's first
help line says what someone would use it for.

### `affected`

The range is positional and reads like `git diff`: `cambi affected
origin/main..HEAD`. A single ref means `<ref>..HEAD`. No ref means the merge
base of the configured default branch to `HEAD`. `--base` and `--head` stay
as aliases so existing scripts keep working.

`--dry-run` prints the plan and exits: which index the run would load
(revision, lineage, age), whether the answer would be a splice, a cold
rebuild or global, and how many packages it would re-query. This is the one
line a developer wants before a two-minute wait, and it is what `status`
shows in short form.

Everything else `affected` takes today stays: `--tests-only`,
`--include-manual`, `--max-depth`, `--no-full-build`, `--publish-base`,
`--write-head-index`. `--explain` on `affected` is kept for a release and
then goes: the verb below is its replacement.

### `explain`

With a label, the shortest path from that target back to the change and the
direct reason at the end of it, the same `reason` shapes the JSON contract
already defines. With no label, the run's own story: why it went global or
cold, which file or flag did it, and the `help:` line that names the fix.
The two scopes share one document shape; a label narrows it.

`explain` runs the same computation `affected` does, so it takes the same
range and the same flags. It is not a flag on `affected` because it has its
own output shape and its own audience.

### `index`

The stored artifact is an index and the chain of them is a lineage; those
are the only two nouns. `build [REV]` is the cold build that mints a lineage.
`update A..B` brings an index forward. `publish` uploads one. `list` shows
what the store holds, `show` one index's header and counts, `gc` applies the
retention policy. These are today's `baseline` verbs under the noun they
were always about; `baseline` survives for one release as an alias with a
deprecation diagnostic.

### `inspect`

Graph questions against one index file: reverse and forward dependencies,
the shortest path between two targets, which rule consumes a file, which
package owns a path, a package's targets, byte accounting, the externals
tables, and `verify` against Bazel. These are plumbing, and their help says
so. They are how a surprising `explain` answer is checked without reading
the index by hand.

### `status`

Read-only and fast: no Bazel, no network beyond a listing of the store, and
if the store is unreachable it says so on one line and shows the rest. In
order of how often someone needs each:

- **Workspace**: root, Bazel version from the pin, WORKSPACE or bzlmod mode,
  which `cambium.toml` was read and its hash.
- **Position**: the checked-out revision, whether an index exists for it,
  and if not the nearest ancestor that has one and how many commits back.
  This is the line that answers "warm or cold?".
- **Index**: for the index a run would load — size, packages, targets,
  external repositories, when it was built and by which `cambi`.
- **Lineage**: the lineage that index belongs to and its head; whether a
  stale or incompatible lineage sits alongside it (a config-hash mismatch,
  an older engine id). That was a real bug once and `status` names it
  before `doctor` has to.
- **Store**: backend and location, reachable or not, how many indexes.
- **Warnings**: anything that would send the next run global or cold —
  an unclassifiable WORKSPACE statement, a `.bazelrc` flag off the
  allowlist, an unreadable file the fixpoint fell back on.

`status` reports; `doctor` judges. Keeping them apart is what keeps `status`
quiet.

### `doctor`

What a new user runs first and what a bug report asks for. Bazel found and
its version supported; workspace mode detected and consistent (a WORKSPACE
tree carrying an inert `MODULE.bazel` is named, not guessed at); the config
parses and every value's origin is known; the store answers a fetch and a
push of a probe object; the index format is current; the packages `bazel
query` could not load last time, if any. Every finding is a diagnostic with
a code and a `help:` line. Exit 0 if a run can work here, 1 otherwise.

### `init`

Writes `cambium.toml` and nothing else. It does not build an index, does not
touch the store beyond the same probe `doctor` makes, and does not hold or
ask for credentials: the store backends are a directory, an HTTP base, or a
command template that drives `aws s3` or `gsutil` under whatever credentials
the environment already has, and that is all `cambi` will ever know about
authentication.

It detects what it can — `.bazelversion`, WORKSPACE versus bzlmod, an inert
`MODULE.bazel`, `.bazelrc` `try-import`s, the default branch — shows what it
found, and asks only for what it cannot infer: the store. Every question has
a flag (`--store-dir`, `--store-s3 bucket/prefix`, `--store-http`), and
`--yes` takes the defaults for the rest, so a script can run it. On an
existing file it edits the keys it was asked about and leaves the rest.

The last thing it prints is the next command to run.

### `config`

`show` prints the effective configuration; `hash` prints the hash every
index carries; `explain` prints every effective value and where it came
from — the file, the flag, the environment variable, or the default. The
local override (below) shows up here as its own origin.

## Configuration

`cambium.toml` at the workspace root is the contract and is meant to be
checked in. It says what the graph is: Bazel flags, invalidators, ignore
globs, the `.bazelrc` allowlist, the store, the limits. It is hashed into
every index it produces, and an index whose hash differs cannot be spliced
onto.

Beside it, gitignored, `cambium.local.toml` holds what is about this machine
and not about the graph: `cache_dir`, `bazel.binary`, `bazel.startup_flags`.
Only keys already excluded from the config hash may appear there; anything
else is a diagnostic. `--cache-dir`, `--bazel`, `CAMBIUM_CACHE_DIR` and
`--config` keep their precedence over both files.

## The output contract

- `--format human` (the default on a TTY), `json` (one document), `jsonl`
  (one event per line for long runs, so a watcher can follow), `query` (the
  Bazel `set(...)` expression, `affected` only), `null` (exit code only).
  `--output` stays as an alias for a release. There is no field selection;
  `--format json` and `jq` are the whole story.
- Results go to stdout. Progress and diagnostics go to stderr, and stderr is
  empty on success under `--quiet` or when stdout is not a TTY. Colour only
  on a TTY and never under `NO_COLOR`.
- Every document carries `schema`. `affected` and the `inspect` documents
  keep the existing `SCHEMA_VERSION`; `status`, `explain` and `doctor` are
  new documents and start at that version too, because the version is shared
  and one bump moves all of them.
- Exit codes are few and never change meaning:

  | code | meaning |
  |---:|---|
  | 0 | success — including "nothing affected" |
  | 1 | error, or usage |
  | 2 | global invalidation; everything is affected |
  | 3 | Bazel failed |
  | 4 | no index available and `--no-full-build` was given |
  | 5 | store unreachable |

  0 through 4 are the codes callers already branch on and keep their
  numbers. 5 is new: a store that cannot be reached is not the same failure
  as a store that has no index, and a CI job should be able to retry the
  first and build for the second.

## Diagnostics

Every warning and error has a stable code, a one-line message, an optional
location, and a `help:` line that names the flag, file or key to change.
Human form follows `rustc`:

```
warning[CAMBI-W004]: .bazelrc changed and the run went global
  --> .bazelrc:14
   |
14 | build --define=FOO=1
   |       ^^^^^^^^ --define is not on the loading-irrelevant allowlist
   = help: add it to [bazelrc].allowlist_extra if it cannot affect analysis
```

Under `--format json` the same diagnostic is one object,
`{"code", "level", "message", "location", "help"}`, in a `diagnostics` array
on the document; under `jsonl` it is its own event. Codes are `CAMBI-E` for
errors and `CAMBI-W` for warnings, numbered once and never reused, and every
one is listed with its meaning and its usual fix in `docs/DIAGNOSTICS.md`.
The rule for what earns a code: anything a person or an agent might need to
act on. A log line is not a diagnostic.

## Migration

Every existing spelling keeps working for one release and prints a
deprecation diagnostic naming the new one (`CAMBI-W001: cambi baseline build
is now cambi index build`). `--output` becomes `--format`, `baseline`
becomes `index`, the old `index` queries become `inspect`, `--explain`
becomes `explain`. The `bench` and `synth` verbs are removed rather than
deprecated — not aliased, not warned about: they were never part of the
contract, their only caller is the repository's own driver, and they live in
`cambium-bench` now. `cambi bench ...` is an ordinary unknown-subcommand error
from clap.

## The benchmark tools

The harness (`bench`, `replay`, `labels`, `report`, the subject manifest,
the synthetic generator) is its own binary, `cambium-bench`
(`crates/bench-cli`), built from the `bench` and `synth` crates, and it drives
`cambi` by shelling out to it: `cambi index build` for a cold index, `cambi
index update` to bring one forward, `cambi index publish` to upload one, and
`cambi affected --format json` for the answer, read back with the engine's own
report types. That is the honest arrangement: the harness measures the binary
users run, with the same flags, the same store and the same exit codes, and a
change to the product surface shows up in the benchmark as it would for a
user. `replay` and the stage micro-benchmarks (`stages`, `sweep`, `regress`)
are the exception and keep linking the engine directly, because they are
white-box by nature: one replays the engine's static stages against a stored
index with no product run to measure, and the others time a single in-memory
stage. The live matrix view of issue #1 is that binary's front door, not a `cambi`
verb. It follows the output rules here because they are good rules, not
because it is part of the contract.

`cambium-bench watch` is that view: one row per subject — step, progress,
the commit or change set in hand and the tool on it, elapsed, an ETA from the
p50 of what is already done, globals so far, the worst recall so far, the pass
count — with the last error per subject and the diagnostics the last `cambi`
document carried underneath. `enter` expands a row into its change sets,
`q` quits. It is a pure reader: the workloads write
`bench-results/<subject>/progress.json` atomically on every step and the
watcher polls it (`--interval`, default a second), so the same screen follows
a run in progress, a finished run, and a `bench-results/` copied off the
benchmark box. `--once` prints the same table as text and exits, and
`--once --format json` answers with the snapshot document (`{schema,
results_dir, at, subjects}`) for a tool that wants the matrix without
learning the result files.
