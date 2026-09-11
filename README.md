# cambium

Incremental changed-target computation for large Bazel monorepos.

Given a repository at two commits, cambium reports the build and test targets
whose outputs could differ between them — at a cost proportional to the size of
the change and the size of the answer, not the size of the repository.

The project is cambium; the binary it installs is `cambi`.

```console
$ cambi affected origin/main..HEAD --tests-only
//app:main_test
//lib/core:core_test
```

It never under-approximates. Over-approximation is acceptable and measured;
missing an affected target is a bug, and the test suite is built around
proving it does not happen.

## How it works

The design rests on one invariant:

> The outgoing edges and rule attributes of a target are a pure function of its
> `BUILD` file, the `.bzl` files that file transitively loads, the set of paths
> under the package that are not in a sub-package, and the repository-global
> inputs (`MODULE.bazel`, lockfiles, `.bazelrc`, toolchains).

So a graph index built at one commit stays valid at another for every package
whose inputs did not change. A run is:

1. **Load** a baseline index for `base` — from the artifact store, or by
   bringing a first-parent ancestor's index forward, or (once per lineage) by
   building one from scratch.
2. **Classify** `git diff --name-status` into dirty packages (their rows must
   be re-queried), directly-changed files (their consumers are affected but the
   graph is unchanged), and global invalidators (everything is affected).
3. **Query** only the dirty packages: `bazel query 'set(//a:* //b:* ...)'`.
   `//pkg:*` loads that package without walking its dependencies.
4. **Splice** the freshly queried rows into the index, replacing the rows those
   packages own and leaving the rest untouched.
5. **Derive the direct set**: targets whose own definition changed, consumers
   of changed files, consumers of deleted targets, and every rule in a package
   whose `.bzl` load closure moved.
6. **Walk** the reverse edges from the direct set.

| step | cost |
|---|---|
| load index | O(1) mmap, plus an O(labels) map only where a label lookup is needed |
| dirty set | O(changed files × path depth), plus O(consumers) per changed `.bzl` from the stored load closure — no Bazel call |
| query | O(targets in dirty packages) of Bazel loading time |
| splice | O(changed edges) into an overlay, or O(edges) memcpy when a new index is written |
| direct set | O(dirty targets + changed files) |
| walk | O(affected + their in-edges) |

The splice comes in two forms, and which one a command uses follows from what
it owes at the end:

- **`affected` takes the delta overlay.** It answers a question and throws the
  graph away, so there is no reason to write a whole new index for it. The
  overlay holds only the rows the dirty packages replaced and the reverse-edge
  corrections they imply, and reads through to the mmapped baseline for
  everything else — so the step is proportional to the change rather than to
  the repository. This is the default, not a flag.
- **`index update` materialises.** Its output is the thing that gets
  published, so it has to be a complete index.

Both paths run the same direct-set derivation and the same walk over a
`GraphView`, so they cannot disagree about the answer — only about what is left
on disk afterwards. Materialising was once the only option, and at ~52% of a
run on a 500k-target graph it was the single most expensive step;
[docs/ENGINE_COSTS.md](docs/ENGINE_COSTS.md#where-the-time-actually-goes) has the numbers for
both.

## Install

```console
$ cargo install --path crates/cli    # or: cargo build --release
$ cambi --version
```

The crate is `cambium-cli`; the binary it installs is `cambi`. The benchmark
harness is a second binary, `cambium-bench`, from `cambium-bench-cli`; nothing
in it is part of the contract below.

No `protoc` needed — the vendored `build.proto` is compiled by `protox`, a
pure-Rust protobuf compiler. Bazel itself is needed at runtime, not to build.

## Usage

```console
$ cambi                            # no verb: cambi status
$ cambi init      [--store-dir PATH | --store-s3 BUCKET/PREFIX |
                    --store-gcs BUCKET/PREFIX | --store-http URL | --store-none]
                   [--default-branch BRANCH] [--yes]
$ cambi status    [--format human|json]
$ cambi doctor    [--format human|json]
$ cambi affected [<base>..<head>] [--tests-only] [--include-manual]
                   [--format human|json|jsonl|query|null] [--max-depth n]
                   [--no-full-build] [--publish-base] [--out <index file>] [--dry-run]
$ cambi explain [<label>] [<base>..<head>]   # the same flags as affected
$ cambi index build  [--commit <sha>] [--publish] [--out <file>]
$ cambi index update --from <sha> [--to <sha>] [--publish] [--out <file>]
                   [--no-rebuild-on-global]
$ cambi index publish <file>
$ cambi index show    <file>
$ cambi index gc [--keep <artifact>] | list
$ cambi inspect target    <file> //x:y
$ cambi inspect package   <file> <package>
$ cambi inspect rdeps     <file> //x:y  [--transitive] [--include-manual] [--tests-only] [--max-depth n]
$ cambi inspect deps      <file> //x:y  [--transitive] [--include-manual] [--tests-only] [--max-depth n]
$ cambi inspect path      <file> //from:a //to:b
$ cambi inspect consumers <file> <path>...  [--transitive] [--include-manual] [--tests-only] [--max-depth n]
$ cambi inspect owner     <file> <path>...
$ cambi inspect targets   <file> <package> [--kind rule]
$ cambi inspect stats     <file>
$ cambi inspect verify    <file> [--sample n] [--seed n]
$ cambium-bench subjects list|fetch|build [<subject>]
$ cambium-bench history  <subject> [--commits n] [--oracle rebuild|none]
$ cambium-bench changes  <subject> [--change-sets n] [--kind pr,adjacent,stress]
                   [--tools cambium,bazel-diff,tango] [--oracle rdeps,load]
$ cambium-bench run [--subject <s>]... [--steps fetch,build,history,changes,report]
                   [--smoke] [--restart-changes] [--restart-history] [--require-sha <sha>]
$ cambium-bench stages|sweep <subject>
$ cambium-bench regress <a.json> <b.json>
$ cambium-bench replay  <subject> (--all | --change-set <a>..<b> | --paths <p>)
                   [--repo <git dir>] [--rederive] [--head-check] [--write-paths [<file>]]
$ cambium-bench pull-results [<subject>]... [--cache <dir>] [--host <h>] [--key <pem>]
$ cambium-bench labels  <subject> --change-set <a>..<b> [--tool t] [--class c] [--side s] [--diff]
$ cambium-bench watch   [--results-dir <dir>] [--once]
$ cambium-bench report  [<subject>]
$ cambium-bench synth emit-bazel --spec <preset> --out <dir> [--changes <preset>]
                   [--revisions n] [--seed n] [--force]
$ cambium-bench synth describe    [--spec <preset>] [--seed n]
$ cambium-bench synth build-index [--spec <preset>] --out <file> [--seed n]
$ cambium-bench synth presets
$ cambi config show|explain|hash
```

`index build` and `index update` need `--out <file>` to say where the
index goes; only a run that also `--publish`es may omit it, and that run
writes through a temporary file it deletes once the upload is done. An index
is routinely gigabytes, so there is no default path.

Every subcommand also accepts these global options, declared once on `cambi`
itself: `-C, --workspace <dir>` (defaults to the current directory),
`--config <cambium.toml>`, `--bazel-binary <binary>` (overrides config and
`$BAZEL_REAL`), `--log <level>` (`error`, `warn`, `info`, `debug`, `trace`;
also settable via `$CAMBIUM_LOG`, with an explicit `--log` winning),
`--format <human|json|jsonl|query|null>`, `--quiet` (nothing on stderr on
success) and `--color <auto|always|never>` (never under `$NO_COLOR`, and only
on a TTY under `auto`).

`--format` replaces the per-command `--output`, which stays as a hidden alias,
as does `text` for the value `human`, so existing invocations keep working.
`human` is the default. `jsonl` streams one `{"event": ...}` line at a time —
a diagnostic event per diagnostic, then the result — so a watcher can follow a
long run. `null` prints nothing and only sets the exit code. `query` is
`affected` only; asking any other verb for it is a usage error.

`--format query` emits a `set(...)` expression you can hand straight to Bazel:

```console
$ bazel test $(cambi affected origin/main --tests-only --format query)
```

`cambi explain` answers "why is this test running?". With a label it prints
the shortest path from that target back to the change and the direct reason at
the end of it; with no label it tells the run's own story — the cause that made
it global or cold, with the `help:` line that names the fix, or the numbers
that say why the answer was the size it was. It runs the same computation
`affected` does and takes the same range and the same flags. `--explain` on
`affected` is the old spelling: it still works for a release and says so
(`CAMBI-W001`).

`cambi affected --dry-run` prints the plan and exits before any Bazel query:
which index would be loaded (revision, lineage, age, distance), how the diff
classifies (changed files, dirty packages, whether an invalidator, externals or
`.bazelrc` cause makes it global), and what the run would therefore do —
`splice`, `cold rebuild`, `global` or `cold build`. Exit 0, or 2 when the plan
already knows the answer is `//...`.

### Querying an index

The `inspect` subcommands answer the same questions against an index that
already exists, in text or JSON (`--format json`, carrying `schema`):

```console
$ cambi inspect consumers idx.bin lib/core/parser.cc --transitive
lib/core/parser.cc	//lib/core:core
lib/core/parser.cc	//app:main
lib/core/parser.cc	//app:main_test

$ cambi inspect path idx.bin //app:main_test //lib/core:core
//app:main_test
  -> //app:main
  -> //lib/core:core
```

`rdeps` and `deps` are one hop by default and transitive with `--transitive`;
the transitive form includes the subject itself, matching
`bazel query rdeps`. `path` exits non-zero when there is no path, and says so
if one exists the other way round. `stats` reports counts, the in-degree
distribution, and per-component byte sizes — which is how you find out what an
index is actually spending its bytes on. It also projects what labels would
cost stored as `(package_id, relative_name)`, which is the measurement behind
[not doing that](docs/ENGINE_COSTS.md#labels-33-and-not-compressed); run it on an index
of your own repository, where package paths are longer than the generators'
and the answer may well come out differently.

To get an index to point it at without going near Bazel:

```console
$ cambium-bench synth build-index --spec large --out large.bin
$ cambi inspect stats large.bin
```

They share the engine's walk and the same package-ownership lookup the dirty
set uses, so an `inspect` answer and an `affected` answer cannot disagree.

## The JSON contract

Everything cambium prints as JSON is meant to be parsed by something that is
not cambium, so the field names, their shapes, and the exit codes are a
versioned contract. Every JSON document carries the version it was written
against:

```json
{ "schema": 9, "base": "...", "head": "...", ... }
```

Bump `SCHEMA_VERSION` (`crates/engine/src/report.rs`) whenever a consumer could
notice the difference — a renamed, removed or retyped field, an added field, or
an exit code changing meaning. `crates/engine/tests/json_contract.rs` pins the
affected report's shape and fails with instructions if it moves without a bump;
`crates/cli/src/main.rs` pins the exit codes.

### Exit codes

| code | meaning |
|---:|---|
| 0 | success |
| 1 | error |
| 2 | global invalidation — everything is affected; stdout is `//...` |
| 3 | Bazel failed |
| 4 | no baseline available and `--no-full-build` was given |
| 5 | the store could not be reached (as against holding no index, which is 4): a CI job can retry this one |

### Diagnostics

Every warning and error `cambi` reports carries a stable code — `CAMBI-W004`,
`CAMBI-E009` — a message, sometimes a location, and a `help:` line naming the
flag, file or key to change. The human form follows `rustc` and goes to
stderr; under `--format json` the objects are in the document's `diagnostics`
array, and under `--format jsonl` each is its own event. The numbers are
assigned once and never reused. Every code, when it fires and what to do about
it is in [docs/DIAGNOSTICS.md](docs/DIAGNOSTICS.md).

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
| `timings_ms` | object | `load`, `dirty`, `query`, `splice`, `direct`, `walk`, `total` |
| `stats` | object | `base_nodes`, `head_nodes`, `head_edges`, `dirty_packages`, `queried_targets`, `direct_targets`, `affected_targets`, `changed_files`, `ignored_files`, `rbuildfiles_called`, `bzl_closure_hits`, `bazelrc_safe_diffs`, `cold_rebuild`, `repos_dirty_by_input`, `repos_dirty_by_module`, `repos_dirty_by_bzl`, `repos_dirty_by_target`, `repos_label_only`, `external_direct_targets`, `bzl_witnessed_dead`, `bzl_witnessed_loaded`, `bzl_witness_packages`, `bzl_witness_unavailable`, `bazelrc_flag_nodes` |
| `diagnostics` | \[object], optional | the actionable warnings this run raised, each `{code, level, message, location?, help?}`; omitted when empty. See [docs/DIAGNOSTICS.md](docs/DIAGNOSTICS.md) |
| `explanations` | \[object], optional | with the deprecated `--explain` (and within `cambi explain`): `target`, `path` (target → direct member), `direct`, `reason` |

`reason` is tagged by `kind`: `new`, `rule_hash`, `bzl_closure` (with
`package`), `file` (with `path`), `deleted_dep` (with `label`), `external_repo` (with
`label`), `external_repo_target` (with `repo` and `via`), `starlark_flag` (with
`label`).

Optional fields are omitted rather than null, so absence is meaningful. A
document with no `schema` key predates the field and is version 1.

`--tests-only` applies to `--format json` too: `affected` is restricted to the
entries with `test: true` (`direct` and every other field are unchanged, so a
label a change touched directly but that is not itself a test can still
appear there). This does not change the document's shape, so it is not a
`SCHEMA_VERSION` bump — a consumer that already reads `affected` sees a
subset of what it would otherwise.

`cambi inspect ... --format json` and `cambium-bench` results carry `schema`
on the same terms.

### `cambi status --format json`

One document, one field per probe, and nothing computed that the human form
does not also show:

```json
{
  "schema": 9,
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
probes `status` does not run at all (`bazel`, `round_trip`), which are not in
this document.

### `cambi doctor --format json`

One graded check per probe. `level` is `ok`, `warn` or `fail`; anything that is
not `ok` carries the diagnostic that graded it, and the exit code is 1 if any
check failed and 0 otherwise — a `warn` is a run that works:

```json
{
  "schema": 9,
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
round trip is the check that matters before `--publish`, because a store that
answers a read and refuses a write looks healthy until a run tries to use it.
The probe object is left behind: no backend here can delete a key.

### `cambi explain --format json`

One document for both scopes: the run's fields always, and the label's only
when a label was asked about.

```json
{
  "schema": 9,
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
| `reason` | object, optional | why that member was direct — the same `kind`-tagged shapes `affected`'s `explanations` use |
| `why_not` | string, optional | with a label that is not affected: the one line the human form prints under it |
| `summary` | object | `changed_files`, `dirty_packages`, `direct_targets`, `affected_targets`, `base_nodes`, `cold_rebuild` |

A label that exists in neither index is a usage error (`CAMBI-E013`), not an
answer. A label that exists and is not affected is an answer, and exits 0.

### `cambi affected --dry-run --format json`

```json
{
  "schema": 9,
  "plan": { "base": "e5ac543...", "head": "ca7fa29...",
            "index": { "revision": "e5ac543...", "lineage": "f901cd...", "distance": 3,
                       "age_seconds": 7200, "packages": 4120, "targets": 38211 },
            "changed_files": 12, "dirty_packages": 4,
            "mode": "splice", "diagnostics": [] }
}
```

`mode` is `splice`, `cold_rebuild`, `global` or `cold_build`. `index` is absent
when nothing in the store is usable here (a cold build), and `dirty_packages`
is absent when there is no index to classify the diff against — "unknown until
queried", never guessed. `global_reason` is present when the plan can already
tell the answer is `//...`, which is also exit code 2.

### Contract versions

| version | change |
|---:|---|
| 1 | the first published contract |
| 2 | `index stats` gained `bytes.label_sorted`, `bytes.label_total_share` and `bytes.label_packing` |
| 3 | `affected` became `[{label, depth, test}]`; `stats.rbuildfiles_called`; `index stats` gained `in_degree.top` (the hub list) |
| 4 | `stats.bzl_closure_hits`; `stats.rbuildfiles_called` is now always `false` — a changed `.bzl` is resolved from the index's static load closure instead of from Bazel. `index stats` gained `counts.bzl_files`, `counts.bzl_workspace_files` and `bytes.bzl_closure` |
| 5 | `stats.bazelrc_safe_diffs` (changed bazelrc files whose every changed flag was on the loading-irrelevant allowlist, and which therefore invalidated nothing); `stats.cold_rebuild` and the optional `cold_rebuild_reason` (the dirty set exceeded `limits.max_dirty_packages`, so the run re-indexed at head — still exact, not global; `limits.max_changed_files`, which used to make that case global, is gone); `stats.repos_dirty_by_input`, `stats.repos_dirty_by_module`, `stats.repos_dirty_by_bzl` and `stats.external_direct_targets`; `--explain` gained the direct reason `{"kind": "external_repo", "label": ...}` — the target names something in an external repository whose contents moved (a `MODULE.bazel` edit, or a main-repo file the repository is built from) |
| 6 | `stats.repos_label_only`: of the repositories an input-path edit dirtied, how many the repository's derived path mapping showed the edit could not be a `.bzl` loaded from — those repositories contribute their label consumers to the direct set, and their loading packages are not re-queried |
| 7 | `stats.bzl_witnessed_dead`, `stats.bzl_witnessed_loaded`, `stats.bzl_witness_packages` and `stats.bzl_witness_unavailable`: what the engine did about a changed `.bzl` the base index's load closure had no node for. `stats.bazelrc_flag_nodes` and the `bazelrc_flags` list: a `.bazelrc` edit that changes a Starlark build setting is no longer automatically global, and `--explain` gained the direct reason `{"kind": "starlark_flag", "label": ...}` |
| 8 | `stats.repos_dirty_by_target`: external repositories whose *generated* targets name a main-repo rule target the reverse walk reached — the `@envoy_repo` shape, where the BUILD file the repository rule writes lists `@envoy//changelogs` in a `genrule`'s `tools`. Such a repository is found after the walk rather than from the diff, so the walk runs again from its label consumers, and `--explain` gained the direct reason `{"kind": "external_repo_target", "repo": ..., "via": ...}`. Only the repository's *targets* move this way: a repository rule runs during loading and cannot read a rule target's output, so no `.bzl` it exports can have changed and no package's load closure is re-queried |

| 9 | `cambi status --format json` and `cambi doctor --format json`, new documents (fields above): a new document needs no bump, because the version says what the *existing* documents mean and a reader that has never seen `status` cannot be broken by its arrival. The same holds for `explain`, below. Also at 9: `diagnostics` on the affected report: the actionable warnings a run raised, each `{code, level, message, location?, help?}` with a stable `CAMBI-W` code, omitted when empty. Purely additive — a run with nothing to say prints what version 8 printed. Also at 9: `cambi explain --format json` and `cambi affected --dry-run --format json`, two more new documents on the same terms. The CLI gained exit code 5 (the store could not be reached) and the global `--format`, with `--output` kept as an alias |

The version is shared by every document, so a bump made for one of them moves
all of them. The affected report did not change shape between 1 and 2; it did
at 3, and `--max-depth n` (on `affected` and `index rdeps --transitive`)
truncates the walk at `n` hops, which is what the per-target `depth` makes
meaningful.

### Configuration

`cambium.toml` at the repository root. Every field is optional; the defaults
below are what you get with no file at all. `cambi init` writes one, asking
only what it cannot detect — the store — and editing only the keys it was asked
about on a file that already exists, with its comments and its key order
preserved. It never runs Bazel, never builds an index, and never asks for or
stores a credential: the store backends are a directory, an HTTP base, or a
command template that drives `aws s3` or `gsutil` under whatever credentials
the environment already has, and that is the whole of what `cambi` knows about
authentication.

Beside it, `cambium.local.toml`: what is true of this machine and not of the
graph. Only the five keys that are already outside the config hash may appear —
`bazel.binary`, `bazel.startup_flags`, `baseline.cache_dir`,
`baseline.compress_indexes`, `baseline.publish_partial` — and any other key is
`CAMBI-E014` naming the key and the file it belongs in, because an override
that could move the hash would let two checkouts of one commit publish indexes
that claim to be interchangeable and are not. Keep it out of git; `cambi init`
says so if `.gitignore` does not mention it (and does not edit `.gitignore`).

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

```toml
[bazel]
binary = ""                  # else $BAZEL_REAL, $BAZEL, bazelisk, bazel
extra_query_flags = []

[git]
default_branch = "main"      # `cambi affected` with no range compares the
                             # merge base of this branch with HEAD. Not part
                             # of the config hash: it says which revisions a
                             # run compares, not what the graph is.

[invalidators]               # any change here means "everything is affected"
paths = ["MODULE.bazel", "MODULE.bazel.lock", "WORKSPACE", "WORKSPACE.bazel",
         "WORKSPACE.bzlmod", "WORKSPACE.bzlmod.lock", ".bazelrc",
         ".bazelversion", "REPO.bazel"]
globs = ["tools/toolchains/**", "platforms/**"]

[bazelrc]                    # a changed bazelrc is read rather than assumed:
                             # if every flag it changed is known not to affect
                             # loading or analysis (--remote_*, --jobs,
                             # --test_output, BES, sandboxing, UI...) the run
                             # is not invalidated. An unknown flag, a startup
                             # line, an import, or the file appearing or
                             # vanishing is still global. This extends that
                             # allowlist; entries may end in `*`.
allowlist_extra = []
[externals]                  # map MODULE.bazel edits and repository-rule
enabled = true               # inputs onto the repositories they move, rather
                             # than invalidating everything. Turn it off for a
                             # repository whose repository rules read main-repo
                             # files by computed path rather than by label,
                             # which no static scan can see.

[ignore]                     # changes that cannot affect the build graph;
                             # a path some rule consumes as a source is an
                             # input anyway, whatever the glob says
globs = ["**/*.md", "docs/**", ".github/**"]

[output]
exclude_tags = ["manual"]
test_rule_suffixes = ["_test"]

[baseline]
store = { kind = "dir", path = "/var/cache/cambium" }
max_ancestor_walk = 20
retention_days = 30

[limits]
max_dirty_packages = 500     # past this many dirty packages, re-index `//...`
                             # at head instead of splicing; 0 disables it
```

`max_dirty_packages` bounds the only part of a run whose cost grows with the
diff: the per-package re-query. Past it the run asks Bazel for the whole
repository once, splices all of it onto the baseline, and reports
`stats.cold_rebuild` with `cold_rebuild_reason`. The answer is exact either
way — a cold rebuild is a slower route to the same affected set, not a
surrender to `//...`. (`limits.max_changed_files`, which *did* surrender, is
gone; a config that still sets it loads with a warning and the key ignored.)

The config is hashed into every index it produces. An index built under a
different config cannot be spliced onto, because the invalidator rules that
produced it may no longer hold — `cambi index build` mints a fresh
lineage instead.

Store backends: `dir` (a local directory), `http` (GET/PUT against a base
URL), and `command`, whose argument templates get `{name}` and `{path}`
substituted so `aws s3`, `gsutil`, `curl` or an internal CLI can be plugged in
without touching Rust. Keys are `indexes/<lineage>/<tree>.bin` — the git
*tree* hash, so a rebased or squash-merged commit finds the index its twin
published — with a `commits/<lineage>/<sha>` pointer for callers that only
have a sha. Writes are idempotent, and concurrent publishers are safe — see
[docs/STORE.md](docs/STORE.md), which also has the exact S3 and GCS templates
and the design note for content-addressed blobs.

### Keeping baselines warm

A post-merge job on the main branch keeps the store populated, so pull-request
runs almost always find their merge base directly:

```console
$ cambi index update --from <parent-sha> --to <merge-sha> --publish
```

When a run does not find one, it walks back up to `max_ancestor_walk`
first-parent ancestors, takes the nearest index it finds, and replays it
forward to the base in a single update — the dirty set over the whole range is
the union, so re-querying a package once per commit would buy nothing. Failing
that, it builds cold.

`cambi affected --publish-base` publishes that computed base index under the
base revision, so the next run on the same base finds it instead of replaying
again. Only the base: the head-side index of an `affected` run is never
published, because a pull-request head is not on the main line and nothing will
ask for it again.

Weekly, or whenever `cambium.toml` changes, mint a fresh lineage and drop the
accumulated tombstones:

```console
$ cambi index build --publish && cambi index gc
```

## Benchmarks

`cambium-bench` runs cambium, bazel-diff and Tango on the same *subject* — an
open-source Bazel monorepo from `subject_repos.toml` (bazel, envoy,
sourcegraph, apollo, xla, tensorflow), or a generated graph named
`synth:<preset>` and emitted as a real workspace with a real history. The same
commands run on either.

The harness is its own binary (`crates/bench-cli`) and drives `cambi` the way a
user does, by running it: the benchmark measures the binary that ships, with
the same flags, the same store and the same exit codes.

Two workloads: `cambium-bench history` applies a subject's last N first-parent commits
one at a time and ends by diffing the result against a cold rebuild, and
`cambium-bench changes` runs every tool over `(base, head)` change sets shaped like the
question a CI job asks, scoring recall against a `bazel query rdeps` oracle and
filing every disagreement under a named class. `cambium-bench stages` and `cambium-bench sweep`
time each in-memory stage at sizes no real repository reaches.

[docs/METHODOLOGY.md](docs/METHODOLOGY.md) defines the words and says what each
measurement can and cannot claim; [docs/RESULTS.md](docs/RESULTS.md) is the
numbers; [docs/ENGINE_COSTS.md](docs/ENGINE_COSTS.md) is where an `affected`
run spends its time and where an index spends its bytes; and
[docs/OPERATIONS.md](docs/OPERATIONS.md) is how to run the five real subjects
at full size, which does not fit on a laptop.

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
  bench/           the benchmark: subjects, tools, oracles, workloads, report
  cli/             the `cambi` binary
  bench-cli/       the `cambium-bench` binary, which drives `cambi`
```

`index` and `synth` do not depend on `bazel`. `engine` is the only crate that
depends on everything, and `cli` is thin. `bench-cli` is thinner still: it is
the harness's front door, and the harness measures `cambi` by running it.

## Testing

```console
$ cargo test --workspace                     # unit, integration, oracles
$ CAMBIUM_E2E=1 cargo test -p cambium-engine --test e2e_bazel -- --ignored   # needs Bazel
$ CAMBIUM_SOUNDNESS_SIZE=medium CAMBIUM_SOUNDNESS_SEEDS=50 \
    cargo test --release -p cambium-engine --test soundness      # nightly
```

The correctness argument rests on four oracles, in increasing order of how
much they cost to run:

1. **The engine against a naive reference.** `crates/synth` generates a graph
   with the shape of a real monorepo, mutates it, and computes the affected set
   the expensive way — hash every target together with the transitive closure
   of its inputs at both revisions and diff. The engine's answer must contain
   that set. It runs over hundreds of revisions with no Bazel in the loop.
2. **A spliced index against a full rebuild.** After every mutation the
   incrementally updated index must describe the same graph, edge for edge, as
   one built from scratch.
3. **Hand-written golden scenarios against real Bazel** — the cases from the
   spec's list: modified source, globbed add and delete, rename, BUILD
   attribute edit, added and deleted targets, a `.bzl` loaded by two packages,
   `exports_files` across packages, a `select` branch change, a package split,
   a global invalidator, an ignored path. Each is also checked against a full
   rebuild, so the expectations cannot drift.
4. **`cambi inspect verify`** — for a sample of targets, our
   reverse closure must contain everything `bazel query rdeps(//..., target)`
   reports. This is the soundness oracle, and it runs against production
   indexes too, reporting precision as a percentage.

Oracles three and four are opt-in because they need Bazel; they are green
against Bazel 7.4.1, at 100% precision on the emitted workspaces.

## Deviations from the design spec

Three, each for a reason found by testing:

- **`--proto:flatten_selects=false`**, where the spec calls for `true` (7.1).
  Flattening reports a `select` attribute as the union over its branches, so
  editing one branch to a value another branch already names leaves the rule
  hash unchanged and the target is silently reported as unaffected —
  an under-approximation. `rule_input` carries the union either way, so
  disabling flattening keeps the edges identical and gives the hash the
  per-branch detail it needs.
- **The cold build queries `//...:*`, not `//...`** (6.9). The latter is
  shorthand for `//...:all`, which matches rules only, so a cold index would
  describe file labels differently from an incrementally updated one.
- **No separate `files` CSR** (5.3, which permits this). Only rules hold
  forward edges, so every in-edge of a file node comes from a rule: the
  reverse row *is* the consumer list, and a second copy would be redundant.

Two smaller ones, neither affecting behaviour: the index header stores the
commit as a hex string rather than a fixed 20 bytes, so that sha256
repositories and synthetic revision ids both fit; and `cambium-bench sweep --plot`
writes its log-log chart with a small hand-rolled SVG writer rather than
pulling in a plotting crate and its font stack for two dozen line segments.

And one deferral, which is worth being precise about because it looks like a
soundness question: `Packages::bzl_closure_hash` exists in the format but is
written as zero.

The hash exists in the spec to drive one branch of the direct-set rule — when a
package's `.bzl` load closure changed, every rule in it is direct regardless of
its own hash. That branch is load-bearing, not a nicety: a `.bzl` edit that
changes a rule implementation without changing any BUILD attribute leaves every
rule proto in the loading packages byte-identical, so every rule hash matches
and nothing would be reported. cambium takes that branch on **load-closure
membership** instead: the index stores, per `.bzl` file, the packages whose
BUILD file transitively loads it (`crates/index/src/loads.rs`), and those are
exactly the packages whose closure hash would have moved. A hash could only say
*that* something moved; the stored closure says *which packages*, which is the
question the direct set actually asks, so the hash stays a second, redundant
answer and is written as zero.

The closure is computed by scanning `load()` statements at index time, which is
also what removed the last Bazel call from the dirty-set step. It used to be
`rbuildfiles`, which needs Sky Query (`--universe_scope=//...`) and therefore
loads every external repository: on a machine that cannot fetch one, it exits 3
with a partial answer, and a partial answer there can be missing exactly the
package that loads the changed file — so the run had to invalidate everything.
That fired on 5 of 150 `bazel/bazel` commits. The benchmark still makes the
call, as the `load` oracle over the static closure
(`crates/bench/src/oracles/load.rs`).

The `edit bzl` end-to-end scenario pins this: it edits a constant nothing uses,
asserts the whole downstream closure is reported, and asserts that every one of
those targets is reported *because of the load closure* rather than a rule hash
— so if the coverage ever moves to the other branch, the test says so.

## License

MIT. See [LICENSE](LICENSE).

One exception, as you would expect: `crates/cambium-proto/proto/build.proto` is
vendored from Bazel and stays under its own Apache-2.0 header.
