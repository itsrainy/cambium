# cambium

Incremental changed-target computation for large Bazel monorepos.

Given a repository at two commits, cambium reports the build and test targets
whose outputs could differ, proportional to the change and the answer, not the
repository.

The project is cambium; the binary it installs is `cambi`.

```console
$ cambi affected origin/main..HEAD --tests-only --format labels
//app:main_test
//lib/core:core_test
```

It is built never to miss an affected target; [How it works](#how-it-works)
says what that rests on, and where it does not yet hold.

## Install

Prebuilt binaries:

```console
$ curl -fsSL https://raw.githubusercontent.com/itsrainy/cambium/main/scripts/install.sh | sh
$ curl -fsSL https://raw.githubusercontent.com/itsrainy/cambium/main/scripts/install.sh | sh -s -- --version v0.1.0 --prefix /usr/local
```

No release exists yet, so today these commands fail; build from a clone
(below) instead. When `curl` fails, `curl ... | sh` still exits 0, because
`sh` runs the empty script it was given: in a bash script or CI step, run
`set -o pipefail` first so a failed download fails the step, as the
templates in [docs/ci/](docs/ci/) do. Older POSIX shells (dash before
0.5.12) reject `pipefail`; there, download the script to a file with
`curl -fsSL -o install.sh ...` and run `sh install.sh` only if that
succeeded.

The script picks the build for the machine, checks the download against the
release's `SHA256SUMS`, and installs `cambi` to `~/.local/bin` (or
`<prefix>/bin`); it exits non-zero when there is no tarball to download.
The latest release is the default; `--version` pins one, which is what a
CI job should do. The tarballs and `SHA256SUMS` are on the release page for
a download by hand.

From source, with Rust 1.98 or newer (the workspace `rust-version`) and a C
compiler (`blake3`, `ring` and `zstd` build C code):

```console
$ cargo install --git https://github.com/itsrainy/cambium cambium-cli --locked
$ cargo install --path crates/cli --locked    # from a clone
$ cambi --version
cambi 0.1.0 (0a4a001)
```

`cargo install --git` works only once the repository is public; until then
it cannot fetch it, and the from-a-clone line is the route that works.

The crate is `cambium-cli`, the binary `cambi`. The crate named `cambi` on
crates.io is an unrelated project. `--version` names the commit the binary
was built from, or `unknown` for a source tree with neither git metadata nor
cargo's packaged-crate metadata. The benchmark harness is a second binary,
`cambium-bench`, from `cambium-bench-cli`; nothing in it is part of the
contract below.

No `protoc` needed: the vendored `build.proto` is compiled by `protox`, a
pure-Rust protobuf compiler.

Platforms: Linux on x86_64 and aarch64 (glibc), macOS on x86_64 and arm64.
Windows is not supported.

At run time, on `PATH`:

- `bazelisk` or `bazel`, needed to run and not to build. cambi prefers
  `bazelisk` when both are there, and `bazel.binary` in `cambium.toml`
  names another;
- `git`, which cambi runs for the diff and the revisions;
- for a `command` store only, whatever its templates run (typically `aws` or
  `gsutil`, see [docs/STORE.md](docs/STORE.md#s3-and-gcs)).

## Try it

`cambi` has nine verbs: `init`, `status`, `doctor`, `affected`, `explain`,
`profile`, `index`, `inspect`, `config`. [docs/CLI.md](docs/CLI.md) is the reference for
all of them, their flags and their output rules; this is the path through.

```console
$ cambi init --store-dir .cambium-store
```

Writes `cambium.toml` and nothing else (no index, no Bazel, no credentials),
detecting `.bazelversion`, WORKSPACE versus bzlmod, the `.bazelrc`
`try-import`s and the default branch, and asking only for the store. A
relative store path is read from the workspace root, so `.cambium-store/`
goes in `.gitignore` beside the `cambium.local.toml` line `init` asks for.
A tree with only a `WORKSPACE` file and no `.bazelversion` is warned about
(`CAMBI-W016`): bazelisk then runs the latest Bazel, which reads no
`WORKSPACE`, so pin one that does. Pinning is the fix that sticks: Bazel 7
and later write an untracked `MODULE.bazel` stub into a WORKSPACE repository
the first time they run there, cambi's own queries included. The stub is
comment-only and is not read as a marker, so the tree stays a WORKSPACE one.

```console
$ cambi doctor
```

Grades what a run needs: Bazel found, its version supported and able to
read the workspace's mode (Bazel 9 on a `WORKSPACE`-only tree fails under
`CAMBI-E015`), the workspace mode unambiguous, the config parsed, the store
answering both a fetch and a push of a probe object. Exit 0 if a run can
work here, 1 otherwise. Every finding is a diagnostic with a code and a
`help:` line.

```console
$ cambi profile --commits 30 --out ./cambium-index.bin
$ cambi index publish ./cambium-index.bin
```

What cambium costs and saves on this repository, from its own history: one
cold index at the oldest of the last 30 first-parent commits, then one
incremental step per commit up to `HEAD`, in a scratch worktree (the checkout
and the store are left alone). The report gives the graph's size, the cold
index's time and size, what a typical commit costs and affects as a share of
the repository's targets and tests at `HEAD`, and how often a commit goes
global and under which `CAMBI-W` code. Its times come from one warm Bazel server with
the index in memory; a cold CI runner also pays the store fetch, a server
start and external fetches. A commit that does not build is reported as
failed and the walk goes on; if the oldest one is the problem, a smaller
`--commits` or a `FROM..` range moves the cold index to a newer commit.

`--out` keeps the index the walk ends with, and `index publish` uploads it as
the first index of a lineage, so the profile doubles as the first cold build:
one build, not two. Everything after is incremental. The file is written in
this checkout, the path `init` suggests, so it is this repository's index and
no other's (a fixed path outside it, shared by every repository on the
machine, is how one repository's index gets published into another's store);
delete it once published, or give it a `.gitignore` line beside the other two.
To skip the profile, `cambi index build --publish` is the same cold `bazel
query //...:*` on its own. `cambi status` says where the checkout is and which
index a run would load.

```console
$ cambi affected origin/main..HEAD --tests-only
```

On a terminal that is a count line, the labels, and two charts of how far
from the change they sit. For CI, `--format labels` prints the labels alone,
one per line, for `bazel test --target_pattern_file`; exit 2 means the change
reached everything, and stdout is then `//...`:

```sh
rc=0; cambi affected origin/main --tests-only --no-full-build --format labels > targets.txt || rc=$?
case $rc in
  0) [ ! -s targets.txt ] || bazel test --target_pattern_file=targets.txt ;;
  2) bazel test --target_pattern_file=targets.txt ;;  # global: the file holds //...
  4|5) bazel test //... ;;                             # no index in reach, or no store
  *) exit "$rc" ;;
esac
```

An empty file is skipped, because `bazel test` with no targets fails. Exit
4 is no index within reach under `--no-full-build`, and 5 a store that is
there and could not be read. A mistyped flag exits 1, with the parser's
message on stderr and nothing on stdout, so the `*)` arm fails the job.

The range answers one question: what did `HEAD` change since it forked from
the base. `origin/main..HEAD`, `origin/main...HEAD` and a single ref
(`origin/main`, meaning `origin/main..HEAD`) all compare from the merge base
of the two, which is the base itself whenever `HEAD` descends from it. On a
branch behind `origin/main` it is not, and the run says so (`origin/main is
not an ancestor of HEAD; comparing from their merge base <sha>`) rather than
reading every commit `main` gained since as a change the branch reverted.
Two commits with no merge base in the clone (a shallow clone cut above it)
are `CAMBI-E013`, whose help says to deepen; in a CI checkout that means
`HEAD`'s parent must be in the clone (`fetch-depth: 2`). A reversed range
(`HEAD..HEAD~3`, the base descending from `HEAD~3`) compares nothing, exit 0,
and the narration says so and names the range the other way round. No
range means the merge base of `[git].default_branch` with `HEAD` (or with
`origin/<branch>` in a CI checkout that has no local branch).
`--dry-run` prints the plan (which index would load, how the diff classifies,
whether the run would splice, cold-rebuild or go global) and exits before any
Bazel query. One verdict is pending that query: the toolchain check reads rule
classes off the rows the query returns, so a plan that says `splice` or `cold
rebuild` can still end as `//...` under `CAMBI-W023`. Every other global the
run reaches, the plan reaches too.

```console
$ cambi explain //app:main_test origin/main..HEAD
```

Answers "why is this test running?": the shortest path from the target back to
the change, and the direct reason at its end. With no label it tells the run's
own story: what made it global or cold, with the `help:` line naming the fix.

`cambi inspect` asks graph questions against one index file: `target`,
`package`, `deps`, `rdeps`, `path`, `owner`, `consumers`, `targets`, `stats`,
`externals`, `verify`. It is plumbing, and how a surprising `explain` answer
gets checked.

### In CI

Two jobs: one after each merge to the default branch keeps an index at its
tip (`cambi index update HEAD --publish`), and one per pull request runs the
script above on `HEAD^1..HEAD`, the merge commit GitHub checks out.
[docs/CI.md](docs/CI.md) is the whole setup: how the index moves between
runners, two GitHub Actions workflows to copy, clone depth, what a lineage
break looks like and how it heals, which store to use, and what a cold
runner pays that `profile`'s numbers leave out.

[docs/STORE.md](docs/STORE.md) is the store: key layout, compression at rest,
what makes an index incompatible, why concurrent publishers are safe, and the
exact S3 and GCS command templates.

## How it did

`cambium-bench` walked the first-parent history of five open-source Bazel
repositories, bazel, envoy, sourcegraph, apollo and xla, 300 commits each
and 74 for apollo, carrying one index forward a commit at a time the way a
CI job would. Every step's answer was checked against Bazel's own `rdeps`
query; at the last commit the carried index was compared row by row with
one built from scratch (the rebuild oracle); and two peers, bazel-diff and
Tango, ran on a stratified sample of 40 steps. envoy and xla are from box
run G (2026-09-27, `9655ce3`), the other three from box run A
(2026-09-19/20).

On every step the check ran on (288, 282, 297, 73 and 296 of them),
cambium's answer held every target Bazel named: 0 missing labels, worst
recall 1.0000 on all five. The rebuild oracle held on bazel, envoy and xla.
At the median step the answer was exact on bazel, sourcegraph and xla
(tightness p50 1.000, Bazel's count over cambium's) and a little wider on
envoy (0.986) and apollo (0.969). A step cost 96 ms to 3.1 s at the median
across the five (p90 0.8 to 17.4 s), against 7.0 to 24.8 s for bazel-diff
and 19.1 to 217.8 s for Tango, which recompute the whole graph every time.
All of it is from one 16 vCPU Linux machine with Bazel's server warm across
steps and no index store, so a CI runner pays more
([docs/CI.md](docs/CI.md#what-a-cold-runner-pays)); envoy's and xla's
index builds (121 s and 174 s) were on cold output bases.

The check has limits, and the tables say where. The `rdeps` answer is a
lower bound wherever Bazel cannot resolve a target and drops what depends
on it (112 of envoy's 282 steps, 295 of xla's 296). Three xla steps, at
and just after two LLVM upgrades whose patches do not apply, are
oracle-blind: the changed packages cannot load, so Bazel cannot say what
they reach, and they count as neither sound nor missing. The ⚠ beside sourcegraph and apollo in
[docs/RESULTS.md](docs/RESULTS.md) is run A's rebuild oracle, which ran on
the walk's own Bazel server; on envoy and xla the same kind of failure
traced to that server's leftovers, and it passed once the reference build
had a fresh server, but sourcegraph's and apollo's rows have not been read.
xla's index is partial on 296 of its 300 steps by two Windows toolchain
packages that cannot load on Linux. [Known gaps](#known-gaps) names routes
the check cannot see; docs/RESULTS.md has every number and
[docs/METHODOLOGY.md](docs/METHODOLOGY.md) what each one can claim.

## How it works

It never under-approximates: missing an affected target is a bug, and the test
suite is built around proving it does not happen. Over-approximation is
acceptable and measured. The [known gaps](#known-gaps) below are where
that does not yet hold.

That guarantee rests on one invariant:

> The outgoing edges and rule attributes of a target are a pure function of its
> `BUILD` file, the `.bzl` files that file transitively loads, the set of paths
> under the package that are not in a sub-package, and the repository-global
> inputs (`MODULE.bazel`, lockfiles, `.bazelrc`, toolchains).

So an index built at one commit stays valid at another for every package whose
inputs did not change. A run loads a base index, turns
`git diff --name-status` into dirty packages and directly-changed files,
re-queries only the dirty packages (`bazel query 'set(//a:* //b:* ...)'`),
splices those rows in, derives the direct set, and walks the reverse edges.
Every step is proportional to the change or the answer.
[docs/ENGINE_COSTS.md](docs/ENGINE_COSTS.md) has the time and the bytes.

Which packages a changed path moved is not cambium's own reading of that
invariant. The index carries Bazel's loading-phase graph alongside the target
graph, taken from `bazel dump --skyframe=deps` on the server that just loaded
the repository: the file states, directory listings, `glob` nodes, package
lookups, packages and `BZL_LOAD` nodes, with the edges between them
(`crates/index/src/sky`). A changed path becomes the Skyframe keys it moved,
the reverse walk follows those edges, and every `PACKAGE` or `PACKAGE_LOOKUP`
node it reaches is a dirty package. The invariant above says what makes that
sound; the layer is where the edges come from.

### Where the invariant stops

The invariant covers Bazel's *loading phase*, where Starlark is hermetic: a
`.bzl` cannot read a file, the environment or the clock, so its edges are a
function of the package's inputs however convoluted its macros. cambium never
reads a rule implementation; it re-queries the dirty package with real Bazel
and hashes what comes back, so an adversarial `.bzl` costs nothing.
Counterexamples at the edges of that phase are modelled or escalated to a cold
rebuild.

**Modelled** (cambium answers exactly, at incremental cost):

- A new `BUILD` file carving a sub-package out of its parent, or a deleted one
  returning its files: the parent depends on the sub-directory's
  `PACKAGE_LOOKUP` because Bazel had to decide whose files those were, so the
  reverse walk reaches it and both packages are re-queried. A package that did
  not exist when the index was built is the one thing the layer cannot name
  (Bazel never looked at the directory), so its own `BUILD` file names it.
- A file appearing or vanishing where a `glob()` can see it: the directory
  listing it moved is an edge into the package's glob node, so the package is
  re-queried. A `glob` that does not reach the directory has no such edge, and
  the change costs nothing.
- A changed `.bzl`, including one loaded only by other `.bzl` files: its
  `BZL_LOAD` node hangs off the file and the loaders' `BZL_LOAD` nodes hang off
  it, so the packages behind it are re-queried and their rules are direct
  whatever their hashes say (a rule implementation can move without any rule
  proto moving).
- `.bazelignore`: every package Bazel consulted the ignore list to load
  depends on the node that reads the file, so an edit re-queries all of them.
- A `bazel_dep` version bump, an override, or a lockfile moving on its own:
  the `MODULE.bazel` and `MODULE.bazel.lock` diffs name the repositories that
  moved, and every main-repo rule naming a label in them is reached through
  the graph.
- A repository rule reading the main repository (`local_path_override`,
  `local_repository`, a `repository_ctx.symlink` of a literal path): the index
  records which main-repo paths feed which repository.
- A dependency path that leaves through an external target and comes back
  in. `bazelbuild/rules_shell` is the root module, and every test it declares
  depends on `@bazel_tools//tools/test:collect_coverage`, which depends on
  `//shell/runfiles:runfiles`. Each external label the index holds carries
  an edge to every target of the index it reaches through targets outside
  it, Bazel's answer at cold build (`deps` over every such label, then
  `allpaths` only when that closure reaches back in), and again at head in
  a run that moves an external repository or a main-repo `.bzl` the
  external packages on the route load. An edit to
  `shell/runfiles/runfiles.bash` reaches all 27 rules `rdeps` names (24
  tests), where the index without those edges reached 5 (2 tests), counted
  with `--include-manual`.
- `select()`: every branch's inputs are edges, so the branch a configuration
  picks cannot matter.
- A source file whose *contents* changed: consumers are direct and nothing is
  re-queried, since loading-phase edges depend on file names and `glob()` is
  the only filesystem access there is.

**Escalated** (`cold_rebuild_reason` is reported and `//...` queried once;
still exact, only not incremental):

- `.bazelrc` edits on a line the index query reads (`common`, `always`,
  `query`, under no `--config` or one in effect) to a flag outside the
  allowlist of flags known not to change what `bazel query` loads
  (`crates/engine/src/bazelrc.rs`), including `--deleted_packages`,
  `--repo_env`, `--enable_bzlmod` and nearly every `--incompatible_*` flag.
  A `build`-only line, or one under a `--config` the query does not run
  with, is not a change; `--registry` is answered by the lockfile diff.
- A `toolchain()`, `platform()`, `toolchain_type()`, `constraint_setting()`
  or `constraint_value()` rule added, changed or removed, in any package
  (`CAMBI-W023`). The splice reports every rule id it added, changed or
  tombstoned, and one of those five classes among them is a whole-repository
  answer, because toolchain resolution happens in analysis and has no
  query-graph edge for the reverse walk to follow. The first three declare
  what resolution chooses between; the two constraint classes are what a
  platform and a toolchain match through, and a setting's default or a
  value's parent moving changes that matching with no platform or toolchain
  proto moving. This is the rule the default `tools/toolchains/**` and
  `platforms/**` invalidator globs used to stand in for; they are gone, and a
  rule of another class under those directories is an ordinary rule. The
  check reads the classes off the rows the query returned, so it is the one
  verdict a dry run cannot reach (see `--dry-run` below).
- `register_toolchains` / `register_execution_platforms` in `MODULE.bazel`, in
  a `*.MODULE.bazel` segment the root `include()`s, or in `WORKSPACE` (the
  same `CAMBI-W023`), and any `MODULE.bazel` or `WORKSPACE` statement the
  readers cannot classify by shape.
- An external repository this commit moved that a registration pattern at
  head offers rules from (the same `CAMBI-W023`): a `local_path_override`
  directory, a `local_repository`, a repository built from a main-repo file,
  a version bump. A declaration in an external repository is not a row of
  `//...`, so the rule-class check above cannot see it move, and the routes
  that dirty a repository reach only its label consumers and loaders.
  A repository dirtied only through a path that a target the repository rule
  *writes* names is spared: nothing the rule wrote, its `BUILD` files
  included, can have moved.
- `.bazelversion`, `REPO.bazel`, `WORKSPACE.bzlmod.lock`, and any other
  `[invalidators]` path or glob (`CAMBI-W013`; no glob by default). The
  other default paths are read by a model of their own first and are global
  only when it cannot answer: `MODULE.bazel`, its lock and the `WORKSPACE`
  files by the external-repository stage (the module, lock and statement
  diffs), `.bazelrc` by the rc screen (the first item here). With
  `[externals] enabled = false` the `MODULE.bazel` and `WORKSPACE` entries
  are the bare rule again. `cambi config explain` says which is which.
- A module Bazel autoloads native rules from (`rules_python`, `rules_java`,
  `rules_shell`, `protobuf`, `rules_android`) moving in `MODULE.bazel`, its
  lock or `WORKSPACE`, under a Bazel that autoloads (`CAMBI-W024`): Bazel 8 at
  its default, a Bazel 7.4 whose `.bazelrc` sets
  `--incompatible_autoload_externally` to something, or a Bazel whose version
  nothing states. A bare `py_binary` is then that module's rule through a load
  no BUILD file writes; Bazel records it as a dependency on
  `PRECOMPUTED:autoload_symbols`, a singleton every package shares, which the
  loading layer drops because keeping it would put every package one reverse
  hop from every other. So nothing in the graph names the consumers, and the
  sound answer is everything. Elsewhere the same bump reaches the module's
  consumers through the graph (or stays global for the toolchains it
  registers, `CAMBI-W014`): on Bazel 7 at its default, on an 8 whose
  `.bazelrc` sets the flag empty, and on Bazel 9, which disabled autoloads and
  made the flag a no-op, so every one of those rules arrives through a `load()`
  the package writes. `rules_cc` is on no Bazel's autoload list (8 keeps
  `cc_library` in its own builtins, 9 moved it to `@rules_cc` behind an
  explicit load), so its bump is a toolchain move and nothing more.

**Not modelled** (each with its status):

- **A toolchain's implementation moving behind an unchanged declaration.**
  The five escalating classes are native rules with no implementation of
  their own, so what each one *declares* is in its proto and the rule hash
  sees it move: that is what makes the rule above a complete witness for the
  declarations themselves, constraint definitions included. What the hash
  cannot see is the implementation the `toolchain` attribute points at: a
  `cc_toolchain_config` whose `.bzl` changed reaches the `toolchain()` that
  names it through ordinary edges, and the walk stops there, because nothing
  depends on a `toolchain()` by label. That is toolchain resolution itself,
  an analysis-phase edge declined with the rest (docs/ROADMAP.md). A
  repository that wants a `.bzl` edit under its toolchain directories to stay
  global can list them in `[invalidators].globs`, which is what the old
  defaults did.
- **Symlinked directories inside a package.** `glob()` walks into them, and a
  change to the link target is a diff elsewhere. Unmodelled, alongside the
  source-directory limitation in
  [docs/METHODOLOGY.md](docs/METHODOLOGY.md#disagreement-taxonomy).
- **Analysis-phase edges in general**: aspects' private attributes,
  transitions, exec-platform choice. cambium is exact at the unconfigured
  target graph, which over-approximates the configured one except for
  aspect-private attributes, which `bazel query` cannot see. That gap is the
  same on a cold build and an incremental one.

### Known gaps

Routes on which an answer can miss a target today, each tracked in a
maintainer document:

- **A round trip Bazel could not load.** The route back in from an
  external target is Bazel's answer to `--keep_going` queries over the
  closure of every external label the index holds (see "Where the
  invariant stops" above). A label whose closure fails to load (a
  repository that will not fetch on this machine, a package with errors)
  is recorded with the paths Bazel could load, and when the question cannot
  be asked at all every label keeps the edges it had, none at a cold build.
  Nothing reports either beyond the `debug` log. The same query records
  what the route was derived through (the main-repo `.bzl` files the
  external packages on it load, which can define the rule class that gives
  an external target its edge back in), and a run that changes one of them
  asks again, so a route that moves with the main repository is followed.
  `cambi inspect verify` lists a miss of this kind apart, as a round trip.
  docs/ROADMAP.md (Phase 5) tracks the class.
- **Repository inputs Bazel's markers do not record.** A patch, a
  `build_file` or a lock file is an input of the repository built from it,
  not a target anything depends on, so the benchmark's `rdeps` check roots
  a changed input through Bazel's own repository marker files. Those do not
  record a module extension's file inputs (xla's `pip.parse` reads a
  requirements lock no marker names) or a change to a repository's own
  definition (xla's `LLVM_COMMIT` bump in a `.bzl`), so a step whose only
  route is one of those scores recall 1.0 without having tested it: what
  cambium answers there is unmeasured. docs/METHODOLOGY.md ("The
  repository-input roots") has the details, and docs/ROADMAP.md the
  under-reports reading such rows has found.

## The JSON contract

cambium's JSON is parsed by things that are not cambium, so field names,
shapes and exit codes are a versioned contract. Every document carries the
version it was written against:

```json
{ "schema": 21, "base": "...", "head": "...", ... }
```

Bump `SCHEMA_VERSION` (`crates/engine/src/report.rs`) whenever a consumer could
notice: a renamed, removed or retyped field, an added field, or an exit code
changing meaning. `crates/engine/tests/json_contract.rs` pins the
affected report's shape and fails with instructions if it moves without a bump;
`crates/cli/src/main.rs` pins the exit codes.

### Exit codes

| code | meaning |
|---:|---|
| 0 | success |
| 1 | error, or a usage error: one `cambi` diagnoses (`CAMBI-E013`), or a command line the argument parser rejects (an unknown flag or verb, a missing value), which prints the parser's message on stderr and nothing on stdout |
| 2 | global invalidation: everything is affected; stdout is `//...`. Also a run whose index is short of packages Bazel could not load that `baseline.publish_partial` does not name (`CAMBI-W006`, or `CAMBI-W028` for packages the run carried): the answer cannot be trusted to include what the change reaches through them; `--allow-partial` answers from the index instead |
| 3 | Bazel failed, or no Bazel workspace was found (`CAMBI-E004`), or the Bazel binary could not be started (`CAMBI-E015`) |
| 4 | no index available and `--no-full-build` was given |
| 5 | the store is there and could not be read (as against holding no index, which is 4): a CI job can retry this one. A directory store whose directory does not exist holds no index; the `aws s3 cp`/`gsutil cp` templates cannot tell an outage from a miss ([docs/STORE.md](docs/STORE.md#misses-and-outages)) |

### Diagnostics

Every warning and error `cambi` reports carries a stable code (`CAMBI-W004`,
`CAMBI-E009`), a message, sometimes a location, and a `help:` line naming the
flag, file or key to change. The human form follows `rustc` and goes to stderr; under
`--format json` the objects fill the document's `diagnostics` array, under
`--format jsonl` each is its own event. Numbers are assigned once and never
reused.
[docs/DIAGNOSTICS.md](docs/DIAGNOSTICS.md) has every code, when it fires and
what to do about it.

### `cambi affected --format json`

| field | type | meaning |
|---|---|---|
| `schema` | int | contract version |
| `base`, `head` | string | resolved revisions |
| `global` | bool | everything is affected; `affected` and `tests` are empty and the answer is `//...` |
| `global_reason` | string, optional | why, when `global` |
| `cold_rebuild_reason` | string, optional | why the run re-indexed the whole repository, when `stats.cold_rebuild` |
| `dirty_packages` | \[string] | packages re-queried, without the leading `//` |
| `bazelrc_flags` | \[string], optional | Starlark build settings a `.bazelrc` edit changed and the index resolved, as the rc file writes them (`@envoy//bazel:crypto`) |
| `direct` | \[string] | labels whose own definition or inputs changed |
| `affected` | \[object] | `direct` plus its reverse closure, rule targets only, less the targets a tag in `output.exclude_tags` marks (`manual`, by default) unless `--include-manual` was passed, sorted by `label`: a `manual` target can be in `direct` and not here, so `stats.direct_targets` can exceed `stats.affected_targets`; each entry is `{label, depth, package_depth, test}`, where `depth` is the number of reverse-dependency hops from the change (0 for the direct set; a hop through a deleted dependency counts) and `package_depth` the same walk counted in package boundaries crossed, never more than `depth` |
| `tests` | \[string] | the subset of `affected` that is a test |
| `depth_histogram` | \[int], optional | how many affected targets sit at each depth, `depth_histogram[d]` for depth `d` from 0 to the deepest reached; a depth nothing reached is a zero rather than a gap. Omitted when empty, which is what a global run and an empty answer both are |
| `package_depth_histogram` | \[int], optional | the same count by `package_depth`, omitted on the same terms |
| `timings_ms` | object | `load`, `dirty`, `query`, `splice`, `direct`, `walk`, `total` |
| `stats` | object | `base_nodes`, `head_nodes`, `head_edges`, `dirty_packages`, `queried_targets`, `direct_targets`, `affected_targets`, `changed_files`, `ignored_files`, `rbuildfiles_called`, `bzl_closure_hits`, `bazelrc_safe_diffs`, `cold_rebuild`, `repos_dirty_by_input`, `repos_dirty_by_module`, `repos_dirty_by_bzl`, `repos_dirty_by_target`, `repos_label_only`, `external_direct_targets`, `sky_seeds`, `sky_unknown_seeds`, `sky_visited`, `sky_new_packages`, `bazelrc_flag_nodes`, `spliced_rows`, `repos_settled`, `repos_dirty_by_consumer`, `repos_dirty_by_loader` |
| `diagnostics` | \[object], optional | the actionable warnings this run raised, each `{code, level, message, location?, help?}`; omitted when empty. See [docs/DIAGNOSTICS.md](docs/DIAGNOSTICS.md) |
| `explanations` | \[object], optional | with the deprecated `--explain` (and within `cambi explain`): `target`, `path` (target → direct member), `direct`, `reason` |

`reason` is tagged by `kind`: `new`, `rule_hash`, `bzl_closure` (with
`package`), `file` (with `path`), `deleted_dep` (with `label`), `external_repo` (with
`label`), `external_repo_target` (with `repo` and `via`), `starlark_flag` (with
`label`).

Optional fields are omitted rather than null, so absence is meaningful. A
document with no `schema` key predates the field and is version 1.

`--tests-only` applies to `--format json`: `affected` keeps only entries with
`test: true`, while `direct` and every other field are unchanged, so a label
touched directly that is not itself a test can still appear there. The shape
does not change, so no `SCHEMA_VERSION` bump: a consumer already reading
`affected` sees a subset.

`cambi inspect ... --format json` and `cambium-bench` results carry `schema`
on the same terms.

### `cambi status --format json`

One document, one field per probe, nothing computed that the human form does
not also show:

```json
{
  "schema": 21,
  "workspace": { "root": "/src/repo", "mode": "bzlmod", "markers": ["MODULE.bazel"],
                 "inert_module_bazel": false, "inert_workspace": false,
                 "bazelversion": "7.4.1", "bazelversion_major": 7,
                 "bazelrc_try_imports": ["%workspace%/user.bazelrc"],
                 "bazelrc_imports": [], "diagnostics": [] },
  "config":    { "path": "/src/repo/cambium.toml", "exists": true,
                 "hash": "f6311c0b...", "parse_error": null, "diagnostics": [] },
  "position":  { "head": "abc1234...", "head_tree": "9f8e...", "dirty": false,
                 "has_index": false, "nearest": "def5678...", "distance": 3,
                 "max_ancestor_walk": 20, "plan": "splice",
                 "index_path": "...", "diagnostics": [] },
  "index":     { "path": "...", "size_bytes": 81234567, "framed_bytes": 412345678,
                 "packages": 4120, "targets": 38211,
                 "nodes": 120394, "edges": 488120, "external_repos": 212,
                 "built_at_unix": 1789000000, "format_version": 13,
                 "commit": "def5678...", "lineage": "9f...", "config_hash": "f6...",
                 "bazel_version": "bazel 7.4.1", "partial": false, "diagnostics": [] },
  "lineage":   { "current": "9f...", "others": [], "conflicting": [], "diagnostics": [] },
  "store":     { "kind": "dir", "location": "...", "configured": true,
                 "listable": true, "reachable": true, "indexes": 41,
                 "lineages": ["9f..."], "error": null, "diagnostics": [] },
  "diagnostics": []
}
```

`plan` is `warm` (HEAD has an index), `splice` (an ancestor does) or `cold`.
A field whose probe could not answer is `null` rather than absent, except the
probes `status` never runs (`bazel`, `round_trip`), absent here.

### `cambi doctor --format json`

One graded check per probe. `level` is `ok`, `warn` or `fail`; anything not
`ok` carries the diagnostic that graded it, and the exit code is 1 if any check
failed, 0 otherwise, so a `warn` is a run that works:

```json
{
  "schema": 21,
  "checks": [
    { "name": "workspace", "level": "ok", "detail": "/src/repo (bzlmod)" },
    { "name": "bazel", "level": "fail", "detail": "bazel",
      "diagnostic": { "code": "CAMBI-E015", "level": "error",
                      "message": "the Bazel binary `bazel` is not on PATH",
                      "help": "install Bazel or bazelisk, or ..." } }
  ]
}
```

`doctor` runs the two things `status` will not: `bazel --version`, and a publish
and fetch-back of a probe object under the store's `.probe/` prefix. That
round trip matters before `--publish`, because a store that answers a read and
refuses a write looks healthy until a run tries to use it.
The probe object is left behind: no backend here can delete a key.

### `cambi explain --format json`

One document for both scopes: the run's fields always, the label's only when a
label was asked about.

```json
{
  "schema": 21,
  "base": "e5ac543...", "head": "ca7fa29...",
  "global": false,
  "diagnostics": [],
  "summary": { "changed_files": 3, "dirty_packages": 2, "direct_targets": 4,
               "affected_targets": 9, "base_nodes": 120394, "cold_rebuild": false },
  "label": "//foo:bar",
  "affected": true,
  "path": ["//foo:bar", "//lib:core", "//lib:base"],
  "reason": { "kind": "file", "path": "lib/base.cc" }
}
```

| field | type | meaning |
|---|---|---|
| `schema` | int | contract version |
| `base`, `head` | string | resolved revisions |
| `global` | bool | the run was globally invalidated, so every target is affected |
| `global_reason` | string, optional | why, when `global` |
| `cold_rebuild_reason` | string, optional | why the run re-indexed the whole repository, when `summary.cold_rebuild` |
| `diagnostics` | \[object] | the warnings the run raised, the same shape `affected` carries; the cause of a global or cold run is one of these, and the human form prints it with its `help:` line |
| `label` | string, optional | the label asked about, when one was |
| `affected` | bool, optional | whether it is affected, with a label |
| `path` | \[string], optional | shortest path from the label back to the direct-set member, inclusive |
| `reason` | object, optional | why that member was direct: the same `kind`-tagged shapes `affected`'s `explanations` use |
| `why_not` | string, optional | with a label that is not affected: the one line the human form prints under it |
| `summary` | object | `changed_files`, `dirty_packages`, `direct_targets`, `affected_targets`, `base_nodes`, `cold_rebuild` |

A label in neither index is a usage error (`CAMBI-E013`), not an answer. A
label that exists and is not affected is an answer, and exits 0.

### `cambi affected --dry-run --format json`

```json
{
  "schema": 21,
  "plan": { "base": "e5ac543...", "head": "ca7fa29...",
            "index": { "revision": "e5ac543...", "lineage": "f901cd...", "distance": 3,
                       "age_seconds": 7200, "packages": 4120, "targets": 38211 },
            "changed_files": 12, "dirty_packages": 4,
            "mode": "splice" }
}
```

`mode` is `splice`, `cold_rebuild`, `global` or `cold_build`. `diagnostics`
is present when the plan raised any. `index` is absent
when nothing in the store is usable here (a cold build), `dirty_packages` when
there is no index to classify the diff against: "unknown until queried", never
guessed. `global_reason` is present when the plan can already
tell the answer is `//...`, which is also exit code 2.

### Contract versions

| version | change |
|---:|---|
| 1 | the first contract |
| 2 | `inspect stats` gained `bytes.label_sorted`, `bytes.label_total_share` and `bytes.label_packing` |
| 3 | `affected` became `[{label, depth, test}]`; `stats.rbuildfiles_called`; `inspect stats` gained `in_degree.top` (the hub list) |
| 4 | `stats.bzl_closure_hits`; `stats.rbuildfiles_called` is now always `false`, a changed `.bzl` being answered from the index instead of from Bazel. `inspect stats` gained `counts.bzl_files`, `counts.bzl_workspace_files` and `bytes.bzl_closure` |
| 5 | `stats.bazelrc_safe_diffs` (changed bazelrc files the screen read and cleared, so the file itself invalidated nothing: every triple it changed that the index query reads was allowlisted, resolved as a Starlark build setting, or routed to the externals stage as `--registry`. A file is counted before those routed causes are answered, so a cleared rc file can appear in a run that is global for one of them); `stats.cold_rebuild` and the optional `cold_rebuild_reason` (the dirty set exceeded `limits.max_dirty_packages`, so the run re-indexed at head, still exact, not global; `limits.max_changed_files`, which used to make that case global, is gone); `stats.repos_dirty_by_input`, `stats.repos_dirty_by_module`, `stats.repos_dirty_by_bzl` and `stats.external_direct_targets`; `--explain` gained the direct reason `{"kind": "external_repo", "label": ...}`, the target naming something in an external repository whose contents moved (a `MODULE.bazel` edit, or a main-repo file the repository is built from) |
| 6 | `stats.repos_label_only`: of the repositories an input-path edit dirtied, how many the repository's derived path mapping showed the edit could not be a `.bzl` loaded from; those contribute their label consumers to the direct set, and their loading packages are not re-queried |
| 7 | `stats.bzl_witnessed_dead`, `stats.bzl_witnessed_loaded`, `stats.bzl_witness_packages` and `stats.bzl_witness_unavailable`: what the engine did about a changed `.bzl` the base index's load closure had no node for. `stats.bazelrc_flag_nodes` and the `bazelrc_flags` list: a `.bazelrc` edit that changes a Starlark build setting is no longer automatically global, and `--explain` gained the direct reason `{"kind": "starlark_flag", "label": ...}` |
| 8 | `stats.repos_dirty_by_target`: external repositories whose *generated* targets name a main-repo rule target the reverse walk reached (the `@envoy_repo` shape, where the BUILD file the repository rule writes lists `@envoy//changelogs` in a `genrule`'s `tools`). Such a repository is found after the walk rather than from the diff, so the walk runs again from its label consumers, and `--explain` gained the direct reason `{"kind": "external_repo_target", "repo": ..., "via": ...}`. Only the repository's *targets* move this way: a repository rule runs during loading and cannot read a rule target's output, so no `.bzl` it exports can have changed and no package's load closure is re-queried |
| 9 | `cambi status --format json` and `cambi doctor --format json`, new documents (fields above): a new document needs no bump, because the version says what the *existing* documents mean and a reader that has never seen `status` cannot be broken by its arrival; the same holds for `explain`. Also at 9: `diagnostics` on the affected report, the actionable warnings a run raised, each `{code, level, message, location?, help?}` with a stable `CAMBI-W` code, omitted when empty and purely additive, so a run with nothing to say prints what version 8 printed; `stats.spliced_rows`, the target rows the splice wrote into the base index, zero when the commit changed nothing the graph models; `cambi explain --format json` and `cambi affected --dry-run --format json`, two more new documents on the same terms; exit code 5 (the store could not be reached); and the global `--format`, with `--output` kept as a deprecated alias |
| 10 | `depth_histogram`: how many affected targets sit at each reverse-dependency depth, index 0 first. Purely additive; the per-target `depth` it summarises has been in the document since 3 |
| 11 | `affected[].package_depth` and `package_depth_histogram`: the same walk counted in package boundaries crossed, an edge inside one package being free. A target's `package_depth` never exceeds its `depth` |
| 12 | `stats.sky_seeds`, `stats.sky_unknown_seeds`, `stats.sky_visited` and `stats.sky_new_packages`, describing the reverse walk over the Skyframe loading layer that now decides which packages a change moved; the four `stats.bzl_witness*` counters go with the witness they described. `stats.bzl_closure_hits` keeps its name and its meaning (dirty packages a changed Starlark file reached). `inspect stats` gained `counts.sky_nodes`, `counts.sky_edges`, `counts.sky_layer`, `counts.sky_by_kind` and `bytes.sky_layer` |
| 13 | `stats.repos_settled`: dirty in-tree repositories whose every dirtying path was a modified source file of one of them, so no package of theirs was re-queried and their changed files reach their consumers by label |
| 14 | `stats.repos_dirty_by_consumer`: external repositories dirtied because an indexed repository their rules depend on moved, read from the index's consumer table |
| 15 | `stats.repos_dirty_by_loader`: external repositories dirtied because a workspace-closure `.bzl` (or `MODULE.bazel`) that defines them loads from a repository whose `.bzl` files may have moved |
| 16 | `inspect verify` compares main-repository rule targets on both sides, which is all `affected` reports: `theirs_total` no longer counts the source and generated files or external targets Bazel's `rdeps` names, which are counted in the new `ignored_files` and `ignored_external` instead. `precision_pct` is now `agreed_total / ours_total`, the share of what the index reported that Bazel also reports (it was `theirs_total / ours_total`, which could pass 100%). `failures[].missing` is sorted and gained `round_trip` and `via`: the misses Bazel connects to the sample only through an external repository, and the external targets on those paths. `escalations` (`{label, rule_class, code, missing}`) lists the toolchain-class samples that depend in the index on nothing but toolchain-class rules: any change that reaches one moves a toolchain-class row, which a run answers with `//...` under `CAMBI-W023`, so their misses no longer count as unsound. A toolchain-class sample that depends on anything else is a failure like any other |
| 17 | `cambi profile`: a commit that fails no longer ends the profile. Its step carries `failed` (the error as a diagnostic) and `index: {"kind": "failed"}`, the next commit is indexed cold (`cold_after_failure`), three failures in a row stop the walk (`stopped`), and `from_failed` says when the cold index moved off the range's first commit. Also `repository`; `cold.targets` and `cold.tests`; per step `subject` and `global_cause`; `code` and `help` on `summary.global_reasons[]`; and in `summary`, `global_rebuild` and `requery` (the two halves of `rebuild`), `incremental_affected` and `incremental_tests` (`mean`, `p50`, `p90`), `affected_share`, `tests_share`, `global_share`, `index_spliced`, `index_unchanged`, `failed` and `rebuilt_after_failure`. The jsonl `row` event gains `step`, the raw step, beside its display strings. Existing fields keep their meaning: a global step is still `affected: 0` with `global: true` |
| 18 | `stats.consumer_leaves_seeded`: leaves of external repositories seeded because the consumer table's leaf map (index format 14) says their transitive deps reach a package of an indexed repository that moved, or does not know the leaf; a consumer seeded this way is still counted in `repos_dirty_by_consumer`, but only its reaching leaves are seeded rather than all of them |
| 19 | `stats.closure_leaves_seeded`: leaves seeded because their transitive deps closure runs through an external repository that moved (index format 16), and for no other reason: envoy's `@grpc//:grpc++` when an input of `@c-ares` moves |
| 20 | What `global` covers, not the shape: a run whose index is short of packages Bazel could not load, and that `baseline.publish_partial` does not name, is global (exit 2, stdout `//...`) with `CAMBI-W006` (the index is partial) or `CAMBI-W028` (this run carried the packages forward) as its cause, naming them in `global_reason`. Such a run used to answer from the partial index with `global: false`. `--allow-partial` on `affected` and `explain` answers from it as before. `doctor` and `status` values change with it: the `bazel` check fails under `CAMBI-E015` for a Bazel that cannot read the workspace's mode, and its `detail` is the version, not the binary, whenever it carries a finding; a `WORKSPACE`-only tree with no pinned Bazel grades the `workspace` check warn and puts `CAMBI-W016` in `status`'s `workspace.diagnostics`; and a comment-only `MODULE.bazel` beside a `WORKSPACE` (the stub Bazel writes) is not a marker, so such a tree's `workspace.mode` is `workspace`, not `ambiguous` |
| 21 | `cambi profile`: `summary.affected_share` and `summary.tests_share` are shares of `index.targets` and `index.tests`, the index the walk ends with (the repository the report's `targets` line describes); they were shares of `cold.targets` and `cold.tests`, the oldest commit's, which read "200% of 1" on a window that began where the repository was small. No field is added or removed |

Also at 9, on the same terms: `cambi profile --format json`, the document
behind what `profile` prints. As of 17 it holds `repository`, `from` and
`head` (the range walked); `index` (the final index: `commit`, `targets`,
`tests`, `packages`, `nodes`, `edges`, `external_repos`, `bytes_on_disk`,
`compression`, `load_ms`, `bazel_version`); `cold` (the cold build's
`commit`, `wall_ms`, `targets` and `tests`); one `commits[]` entry per step
(`commit`, `parent`, `date`, `subject`, `changed_files`, `dirty_packages`,
`affected`, `tests`, `wall_ms`, `global`, `global_reason` and
`global_cause` on a global step, `engine_cold_rebuild`, `failed` on a
commit that did not build, and `index`, what the step did to the stored
index: `{"kind": "identical"}`, `{"kind": "spliced", "rows_rewritten": n}`,
`{"kind": "rebuilt", "reason": ...}` or `{"kind": "failed"}`); `summary`
(`mean_ms`, `p50_ms`, `p90_ms`, `max_ms` over every step; `incremental` and
`rebuild`, the same four plus `commits` over the steps answered
incrementally and those a cold build answered, with `global_rebuild` and
`requery` the two halves of `rebuild`; `commits`, `globals`,
`global_share`, `rebuild_every_commits` when there was a global,
`global_reasons`, `engine_cold_rebuilds`, `mean_affected`,
`incremental_affected` and `incremental_tests`, `affected_share`,
`tests_share`, `index_spliced`, `index_unchanged`, `zero_dirty_fraction`,
`failed` and `rebuilt_after_failure`); and `from_failed` and `stopped` when
the walk had to move its cold index or stop. The contract table's rows 9
and 17 say which fields came when.

The version is shared, so a bump for one document moves all. The affected
report did not change shape between 1 and 2; it did at 3, and
`--max-depth n` (on `affected` and `inspect rdeps --transitive`) truncates the
walk at `n` hops, which is what the per-target `depth` makes meaningful.

## Configuration

`cambium.toml` at the repository root, checked in, every field optional. It
says what the graph is (Bazel flags, invalidators, ignore globs, the
`.bazelrc` allowlist, the store, the limits) and is hashed into every index it
produces, so an index built under a different config cannot be spliced onto
and `cambi index build` mints a fresh lineage instead.

Beside it, gitignored, `cambium.local.toml`: what is true of this machine, not
of the graph. Only the five keys already outside the config hash may
appear (`bazel.binary`, `bazel.startup_flags`, `baseline.cache_dir`,
`baseline.compress_indexes`, `baseline.publish_partial`); any other key is
`CAMBI-E014`, because an override that could move the hash would let two
checkouts of one commit publish indexes that claim to be interchangeable and
are not.

Precedence is flags and environment variables, then `cambium.local.toml`, then
`cambium.toml`, then the defaults. `cambi config explain` prints every
effective leaf value with the layer that decided it:

```console
$ cambi config explain
bazel.binary               = "bazelisk"  (/src/repo/cambium.local.toml)
baseline.cache_dir         = "/fast/ssd/cambium"  ($CAMBIUM_CACHE_DIR)
baseline.retention_days    = 30  (default)
limits.max_dirty_packages  = 900  (/src/repo/cambium.toml)
```

Two keys to know first: `[invalidators].paths` and `.globs` are the paths
whose change means "everything is affected", except that most default paths
are read by a model of their own first (`MODULE.bazel`, its lock and the
`WORKSPACE` files by the external-repository stage, `.bazelrc` by the rc
screen), which names what moved and is global only when it cannot; `config
explain` says under its table which entries are which. And
`limits.max_dirty_packages` bounds the only part of a run whose cost grows
with the diff; past it the run asks Bazel for the whole repository once and
reports `stats.cold_rebuild` with `cold_rebuild_reason`. The answer is exact
either way: a cold rebuild is a slower route to the same affected set, not a
surrender to `//...`.

Every section and default is in [docs/CLI.md](docs/CLI.md#configuration)
(`cambi config explain` prints them), and the store's syntax for each
backend is under [The store](docs/CLI.md#the-store) there.

## Benchmarks

`cambium-bench` runs cambium, bazel-diff, Tango and target-determinator on
the same *subject*: an
open-source Bazel monorepo from `subject_repos.toml`, or a generated graph
named `synth:<preset>` emitted as a real workspace with a real history. Its
own binary, it drives `cambi` the way a user does, by running it, so the
benchmark measures the binary that ships.

There is one workload, the *walk*: a first-parent walk from the start of a
history window to the pin, cold-building an index at the start and carrying
it one step per commit forward, the shape a CI incremental run has. Each
step is scored against the reverse-dependency oracle for recall (did a tool
miss anything) and tightness (how much wider than the truth), at the
per-step wall CI pays. cambium is one row in those tables; the oracle is
the only ground truth, and the peers are the explanation layer, not the
yardstick. Two tiers share the walk: `correctness` (no peers, the oracle on
a sample) is what every engine change pays for, and `full` (the oracle on
every step, the peers on a stratified sample) is what
[docs/RESULTS.md](docs/RESULTS.md) reports.

`cambium-bench run` is the whole thing from flags (per subject the fetch, the
cold build, the walk and the report); with no verb, on a terminal, it
composes the same plan on a screen. `cambium-bench pull-results` copies a
finished run's results off the machine that ran it, and `cambium-bench
replay` reruns the engine's static stages over every recorded step
offline, in milliseconds each, against the answers the run recorded; the
diffs it reads are written where the run happened, by `cambium-bench
replay --write-paths`.

[docs/METHODOLOGY.md](docs/METHODOLOGY.md) defines the words and says what each
measurement can and cannot claim; [docs/RESULTS.md](docs/RESULTS.md) is the
numbers; [docs/OPERATIONS.md](docs/OPERATIONS.md) is how to run the five real
subjects at full size, which does not fit on a laptop.

## Layout

```
crates/
  index/           CSR graph, interning, on-disk format, mmap loader, splice
  cambium-proto/   vendored build.proto, streamed_proto reader, rule hashing
  bazel/           spawning bazel: query, repo mapping, workspace detection
  diff/            git diff --name-status parsing and revision plumbing
  classify/        path -> package, invalidator rules, dirty-set computation
  engine/          a run: load, dirty, query, splice, direct set, walk
  synth/           synthetic graph generator, mutations, Bazel repo emitter
  bench/           the benchmark: subjects, tools, oracles, the walk, report
  cli/             the `cambi` binary
  bench-cli/       the `cambium-bench` binary, which drives `cambi`
  testkit/         the fixtures the crates' tests share; never shipped
```

`index` and `synth` do not depend on `bazel`. `engine` is the only crate that
depends on everything, `cli` is thin, and `bench-cli` thinner still: the
harness's front door, and the harness measures `cambi` by running it.

`scripts/package-release.sh` builds and packages `cambi` for one target
(`--target`, default the host) the way a release does, into `dist/`;
`.github/workflows/release.yml` runs it for the four release targets on a
`v*` tag, and `scripts/install.sh --from-dir dist` installs from its output.

## Testing

```console
$ cargo test --workspace                     # unit, integration, oracles; no Bazel
$ CAMBIUM_E2E=1 cargo test -p cambium-engine --test e2e_bazel -- --ignored   # needs Bazel
$ cargo test -p cambium-bench --test smoke_real_bazel -- --ignored            # needs bazelisk
$ CAMBIUM_SOUNDNESS_SIZE=medium CAMBIUM_SOUNDNESS_SEEDS=50 \
    cargo test --release -p cambium-engine --test soundness      # nightly
```

`cargo test --workspace` needs no Bazel: every engine a test opens points at a
Bazel binary that does not exist, and the rows come from the synthetic source
or a scanner over a temporary tree. Those fixtures (a scratch directory, a git
repository, an engine over one, a package querier that also serves `.bzl`
text, a synthetic history with an index already published) live in
`crates/testkit` (`cambium-testkit`), a dev-dependency of every crate with
tests and nothing else. Tests needing a real Bazel, a network fetch, or a
subject checkout are `#[ignore]`d, so a plain run reports them as ignored
rather than as passes that ran nothing; each is opted into with `-- --ignored`
and says what it is missing when run without it (`CAMBIUM_REQUIRE_TOOLS=1`
turns a missing tool from a skip into a failure, which the nightly job sets).

Four oracles, in increasing order of cost:

1. **The engine against a naive reference.** `crates/synth` generates a graph
   shaped like a real monorepo, mutates it, and computes the affected set the
   expensive way: hash every target with the transitive closure of its inputs
   at both revisions and diff. The engine's answer must contain that set. It
   runs over hundreds of revisions with no Bazel in the loop.
2. **A spliced index against a full rebuild.** After every mutation the
   incrementally updated index must describe the same graph, edge for edge, as
   one built from scratch.
3. **Hand-written golden scenarios against real Bazel**, the cases the
   original design listed: modified source, globbed add and delete, rename, BUILD
   attribute edit, added and deleted targets, a `.bzl` loaded by two packages,
   `exports_files` across packages, a `select` branch change, a package split,
   a global invalidator, an ignored path. Each is also checked against a full
   rebuild, so the expectations cannot drift.
4. **`cambi inspect verify`**, for a sample of targets: the computed reverse
   closure must contain every main-repository rule target `bazel query
   rdeps(//..., target)` reports. This is the soundness oracle, and it runs
   against production indexes too, reporting precision as the share of the
   index's answer that Bazel also reports.

Oracles three and four are opt-in because they need Bazel; they are green
against Bazel 7.4.1, at 100% precision on the emitted workspaces.

## Deviations from the original design

The design document this was built from is not in the repository; where
the code departs from it, it does so in three places, each for a reason
found by testing:

- **`--proto:flatten_selects=false`**, where the design called for `true`.
  Flattening reports a `select` attribute as the union over its branches, so
  editing one branch to a value another branch already names leaves the rule
  hash unchanged and the target silently reported as unaffected (an
  under-approximation). `rule_input` carries the union either way, so
  disabling flattening keeps the edges identical and gives the hash its
  per-branch detail.
- **The cold build queries `//...:*`, not `//...`**. The latter is
  shorthand for `//...:all`, which matches rules only, so a cold index would
  describe file labels differently from an incrementally updated one.
- **No separate `files` CSR**, which the design permits. Only rules hold
  forward edges, so every in-edge of a file node comes from a rule: the
  reverse row *is* the consumer list, and a second copy would be redundant.

One smaller one, not affecting behaviour: the index header stores the
commit as a hex string rather than a fixed 20 bytes, so sha256 repositories
and synthetic revision ids both fit.

One deferral that looks like a soundness question:
`Packages::bzl_closure_hash` exists in the format but is written as zero. The
design uses it for one branch of the direct-set rule (a package whose `.bzl`
load closure changed has every rule in it direct, regardless of its own hash),
and that branch is load-bearing: a `.bzl` edit changing a rule implementation
without changing any BUILD attribute leaves every rule proto byte-identical,
so nothing would be reported. cambium takes the branch on **load-closure
membership** instead: the index stores, per `.bzl` file, the packages whose
BUILD file transitively loads it (`crates/index/src/loads.rs`), exactly the
packages whose closure hash would have moved. A hash says only *that*
something moved, the stored closure says *which packages*, which is what the
direct set asks. The `edit bzl` end-to-end scenario asserts every downstream
target is reported *because of the load closure* rather than a rule hash, so
if coverage moves to the other branch, the test says so.

Scanning `load()` statements at index time also removed the last Bazel call
from the dirty-set step. It used to be `rbuildfiles`, which needs Sky Query
(`--universe_scope=//...`) and therefore loads every external repository: on a
machine that cannot fetch one it exits 3 with a partial answer, and that
partial answer can be missing exactly the package that loads the changed file,
so the run had to invalidate everything. That fired on 5 of 150 `bazel/bazel`
commits. A `load` oracle over the static closure lives at
`crates/bench/src/oracles/load.rs`; the walk's rebuild oracle at the pin
already catches a closure that drifted, and wiring the `load` oracle into the
per-step checks is a follow-up.

## Which document

For someone using `cambi`:

- this README;
- [docs/CLI.md](docs/CLI.md), every verb, flag and output rule;
- [docs/CI.md](docs/CI.md), running it in a pipeline, with the workflows in
  [docs/ci/](docs/ci/);
- [docs/STORE.md](docs/STORE.md), the index store;
- [docs/DIAGNOSTICS.md](docs/DIAGNOSTICS.md), every `CAMBI-` code and what
  to do about it;
- [docs/OVER_REPORTING.md](docs/OVER_REPORTING.md), where answers are wider
  than they need to be, which `CAMBI-W004` points at;
- the summary table at the top of [docs/RESULTS.md](docs/RESULTS.md) and
  its legend.

For working on cambium, and written for its maintainers (they name
benchmark machines, issue numbers, commits and dates freely):
[docs/METHODOLOGY.md](docs/METHODOLOGY.md) (what the benchmark measures and
may claim), [docs/OPERATIONS.md](docs/OPERATIONS.md) (running it at full
size), [docs/ROADMAP.md](docs/ROADMAP.md), [docs/ENGINE_COSTS.md](docs/ENGINE_COSTS.md),
[docs/SKYFRAME_PROTOTYPES.md](docs/SKYFRAME_PROTOTYPES.md),
[docs/XLA_MAIN_REPO_MISSES.md](docs/XLA_MAIN_REPO_MISSES.md), the
Findings section of docs/RESULTS.md, and
[docs/salvage/2026-09-18/](docs/salvage/2026-09-18/), the scratch files,
uncommitted edits, workflow journals and review findings of a 2026-09-18
roadmap session, copied out before its scratch directory was lost and kept
as found (records, not instructions).

## License

MIT. See [LICENSE](LICENSE).

One exception: `crates/cambium-proto/proto/build.proto` is vendored from Bazel
and stays under its own Apache-2.0 header.
