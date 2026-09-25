# cambium

Incremental changed-target computation for large Bazel monorepos.

Given a repository at two commits, cambium reports the build and test targets
whose outputs could differ, proportional to the change and the answer, not the
repository.

The project is cambium; the binary it installs is `cambi`.

```console
$ cambi affected origin/main..HEAD --tests-only
//app:main_test
//lib/core:core_test
```

It never under-approximates: missing an affected target is a bug, and the test
suite is built around proving it does not happen. Over-approximation is
acceptable and measured.

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
- `.bazelversion`, `REPO.bazel`, and the `[invalidators]` paths and globs (no
  glob by default).
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

## Install

```console
$ cargo install --path crates/cli    # or: cargo build --release
$ cambi --version
```

The crate is `cambium-cli`, the binary `cambi`. The benchmark harness is a
second binary, `cambium-bench`, from `cambium-bench-cli`; nothing in it is
part of the contract below.

No `protoc` needed: the vendored `build.proto` is compiled by `protox`, a
pure-Rust protobuf compiler. Bazel is needed at runtime, not to build.

## Five minutes

`cambi` has nine verbs: `init`, `status`, `doctor`, `affected`, `explain`,
`profile`, `index`, `inspect`, `config`. [docs/CLI.md](docs/CLI.md) is the reference for
all of them, their flags and their output rules; this is the path through.

```console
$ cambi init --store-dir /var/cache/cambium
```

Writes `cambium.toml` and nothing else (no index, no Bazel, no credentials),
detecting `.bazelversion`, WORKSPACE versus bzlmod, the `.bazelrc`
`try-import`s and the default branch, and asking only for the store.

```console
$ cambi doctor
```

Grades what a run needs: Bazel found and its version supported, the workspace
mode unambiguous, the config parsed, the store answering both a fetch and a
push of a probe object. Exit 0 if a run can work here, 1 otherwise. Every
finding is a diagnostic with a code and a `help:` line.

```console
$ cambi index build --publish
```

One cold `bazel query //...:*`, minting a lineage and uploading the first
index; everything after is incremental. `cambi status` says where the checkout
is and which index a run would load.

```console
$ cambi affected origin/main..HEAD --tests-only
$ bazel test $(cambi affected origin/main --tests-only --format query)
```

The range is spelled as `git diff` spells it; a single ref means `<ref>..HEAD`,
no range means the merge base of `[git].default_branch` with `HEAD`.
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

### Keeping indexes warm

A post-merge job on the main branch keeps the store populated, so pull-request
runs almost always find their merge base directly:

```console
$ cambi index update <parent-sha>..<merge-sha> --publish
```

Otherwise a run walks back up to `max_ancestor_walk` first-parent ancestors,
takes the nearest index, and replays it forward to the base in one update;
failing that, it builds cold. `cambi affected --publish-base` publishes that
computed base index for the next run on the same base, and only the base,
because a pull-request head is not on the main line and nothing will ask for
it again.

Weekly, or whenever `cambium.toml` changes, mint a fresh lineage and drop
accumulated tombstones:

```console
$ cambi index build --publish && cambi index gc
```

[docs/STORE.md](docs/STORE.md) is the store: key layout, compression at rest,
what makes an index incompatible, why concurrent publishers are safe, and the
exact S3 and GCS command templates.

## The JSON contract

cambium's JSON is parsed by things that are not cambium, so field names,
shapes and exit codes are a versioned contract. Every document carries the
version it was written against:

```json
{ "schema": 10, "base": "...", "head": "...", ... }
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
| 1 | error |
| 2 | global invalidation: everything is affected; stdout is `//...` |
| 3 | Bazel failed |
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
| `affected` | \[object] | `direct` plus its reverse closure, sorted by `label`; each entry is `{label, depth, test}`, where `depth` is the number of reverse-dependency hops from the change (0 for the direct set; a hop through a deleted dependency counts) |
| `tests` | \[string] | the subset of `affected` that is a test |
| `depth_histogram` | \[int], optional | how many affected targets sit at each depth, `depth_histogram[d]` for depth `d` from 0 to the deepest reached; a depth nothing reached is a zero rather than a gap. Omitted when empty, which is what a global run and an empty answer both are |
| `timings_ms` | object | `load`, `dirty`, `query`, `splice`, `direct`, `walk`, `total` |
| `stats` | object | `base_nodes`, `head_nodes`, `head_edges`, `dirty_packages`, `queried_targets`, `direct_targets`, `affected_targets`, `changed_files`, `ignored_files`, `rbuildfiles_called`, `bzl_closure_hits`, `bazelrc_safe_diffs`, `cold_rebuild`, `repos_dirty_by_input`, `repos_dirty_by_module`, `repos_dirty_by_bzl`, `repos_dirty_by_target`, `repos_label_only`, `external_direct_targets`, `sky_seeds`, `sky_unknown_seeds`, `sky_visited`, `sky_new_packages`, `bazelrc_flag_nodes` |
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
  "schema": 10,
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
  "index":     { "size_bytes": 81234567, "packages": 4120, "targets": 38211,
                 "nodes": 120394, "edges": 488120, "external_repos": 212,
                 "built_at_unix": 1789000000, "format_version": 8,
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
  "schema": 10,
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
  "schema": 10,
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
  "schema": 10,
  "plan": { "base": "e5ac543...", "head": "ca7fa29...",
            "index": { "revision": "e5ac543...", "lineage": "f901cd...", "distance": 3,
                       "age_seconds": 7200, "packages": 4120, "targets": 38211 },
            "changed_files": 12, "dirty_packages": 4,
            "mode": "splice", "diagnostics": [] }
}
```

`mode` is `splice`, `cold_rebuild`, `global` or `cold_build`. `index` is absent
when nothing in the store is usable here (a cold build), `dirty_packages` when
there is no index to classify the diff against: "unknown until queried", never
guessed. `global_reason` is present when the plan can already
tell the answer is `//...`, which is also exit code 2.

### Contract versions

| version | change |
|---:|---|
| 1 | the first published contract |
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

Also at 9, on the same terms: `cambi profile --format json`, the document
behind what `profile` prints: `index` (targets, tests, packages,
external repositories, `bytes_on_disk`, `compression`, `load_ms`), `cold`
(the cold build's commit and `wall_ms`), one `commits[]` entry per step
(`changed_files`, `dirty_packages`, `affected`, `tests`, `wall_ms`, `global`,
`global_reason`, `engine_cold_rebuild`, `date` and `index`, the last being
what the step did to the stored index: `{"kind": "identical"}`,
`{"kind": "spliced", "rows_rewritten": n}` or `{"kind": "rebuilt", "reason":
...}`) and `summary` (`mean_ms`, `p50_ms`,
`p90_ms`, `max_ms` over every step; `incremental` and `rebuild`, the same
four plus `commits` over the steps answered incrementally and the steps a
cold build answered; `globals`, `rebuild_every_commits`, `global_reasons`,
`engine_cold_rebuilds`, `mean_affected`, `zero_dirty_fraction`).

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
whose change means "everything is affected", and `limits.max_dirty_packages`
bounds the only part of a run whose cost grows with the diff; past it the run
asks Bazel for the whole repository once and reports `stats.cold_rebuild` with
`cold_rebuild_reason`. The answer is exact either way: a cold rebuild is a
slower route to the same affected set, not a surrender to `//...`.

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
every step, the peers on a stratified sample) is the box run.

`cambium-bench run` is the whole thing from flags (per subject the fetch, the
cold build, the walk and the report), as `tools/bench/run-published.sh`
runs it on the benchmark box; with no verb, on a terminal, it composes the same
plan on a screen. Before that box is started,
`cambium-bench pull-results` copies a finished run's results off it and
`cambium-bench replay` reruns the engine's static stages over every recorded
step offline, in milliseconds each, against the answers the run
recorded; the diffs it reads are written on the box by `cambium-bench replay
--write-paths`.

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
corpus checkout are `#[ignore]`d, so a plain run reports them as ignored
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
3. **Hand-written golden scenarios against real Bazel**, the cases from the
   spec's list: modified source, globbed add and delete, rename, BUILD
   attribute edit, added and deleted targets, a `.bzl` loaded by two packages,
   `exports_files` across packages, a `select` branch change, a package split,
   a global invalidator, an ignored path. Each is also checked against a full
   rebuild, so the expectations cannot drift.
4. **`cambi inspect verify`**, for a sample of targets: the computed reverse
   closure must contain everything `bazel query rdeps(//..., target)` reports.
   This is the soundness oracle, and it runs against production indexes too,
   reporting precision as a percentage.

Oracles three and four are opt-in because they need Bazel; they are green
against Bazel 7.4.1, at 100% precision on the emitted workspaces.

## Deviations from the design spec

Three, each for a reason found by testing:

- **`--proto:flatten_selects=false`**, where the spec calls for `true` (7.1).
  Flattening reports a `select` attribute as the union over its branches, so
  editing one branch to a value another branch already names leaves the rule
  hash unchanged and the target silently reported as unaffected (an
  under-approximation). `rule_input` carries the union either way, so
  disabling flattening keeps the edges identical and gives the hash its
  per-branch detail.
- **The cold build queries `//...:*`, not `//...`** (6.9). The latter is
  shorthand for `//...:all`, which matches rules only, so a cold index would
  describe file labels differently from an incrementally updated one.
- **No separate `files` CSR** (5.3, which permits this). Only rules hold
  forward edges, so every in-edge of a file node comes from a rule: the
  reverse row *is* the consumer list, and a second copy would be redundant.

One smaller one, not affecting behaviour: the index header stores the
commit as a hex string rather than a fixed 20 bytes, so sha256 repositories
and synthetic revision ids both fit.

One deferral that looks like a soundness question:
`Packages::bzl_closure_hash` exists in the format but is written as zero. The
spec uses it for one branch of the direct-set rule (a package whose `.bzl`
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

## License

MIT. See [LICENSE](LICENSE).

One exception: `crates/cambium-proto/proto/build.proto` is vendored from Bazel
and stays under its own Apache-2.0 header.
