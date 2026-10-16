# How cambium works

What an answer rests on, where that stops holding, and the choices the
implementation made along the way.

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
[docs/ENGINE_COSTS.md](ENGINE_COSTS.md) has the time and the bytes.

Which packages a changed path moved is not cambium's own reading of that
invariant. The index carries Bazel's loading-phase graph alongside the target
graph, taken from `bazel dump --skyframe=deps` on the server that just loaded
the repository: the file states, directory listings, `glob` nodes, package
lookups, packages and `BZL_LOAD` nodes, with the edges between them
(`crates/index/src/sky`). A changed path becomes the Skyframe keys it moved,
the reverse walk follows those edges, and every `PACKAGE` or `PACKAGE_LOOKUP`
node it reaches is a dirty package. The invariant above says what makes that
sound; the layer is where the edges come from.

## Where the invariant stops

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
  verdict a dry run cannot reach (see `--dry-run` in [CLI.md](CLI.md)).
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
  [docs/METHODOLOGY.md](METHODOLOGY.md#disagreement-taxonomy).
- **Analysis-phase edges in general**: aspects' private attributes,
  transitions, exec-platform choice. cambium is exact at the unconfigured
  target graph, which over-approximates the configured one except for
  aspect-private attributes, which `bazel query` cannot see. That gap is the
  same on a cold build and an incremental one.

## Known gaps

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

## Design choices

Three places where the implementation does something other than the
obvious thing, each for a reason found by testing:

- **`--proto:flatten_selects=false`**, not `true`.
  Flattening reports a `select` attribute as the union over its branches, so
  editing one branch to a value another branch already names leaves the rule
  hash unchanged and the target silently reported as unaffected (an
  under-approximation). `rule_input` carries the union either way, so
  disabling flattening keeps the edges identical and gives the hash its
  per-branch detail.
- **The cold build queries `//...:*`, not `//...`**. The latter is
  shorthand for `//...:all`, which matches rules only, so a cold index would
  describe file labels differently from an incrementally updated one.
- **No separate `files` CSR**. Only rules hold
  forward edges, so every in-edge of a file node comes from a rule: the
  reverse row *is* the consumer list, and a second copy would be redundant.

One smaller one, not affecting behaviour: the index header stores the
commit as a hex string rather than a fixed 20 bytes, so sha256 repositories
and synthetic revision ids both fit.

One deferral that looks like a soundness question:
`Packages::bzl_closure_hash` exists in the format but is written as zero. It
was meant for one branch of the direct-set rule (a package whose `.bzl`
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
