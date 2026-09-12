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

That guarantee rests on one invariant:

> The outgoing edges and rule attributes of a target are a pure function of its
> `BUILD` file, the `.bzl` files that file transitively loads, the set of paths
> under the package that are not in a sub-package, and the repository-global
> inputs (`MODULE.bazel`, lockfiles, `.bazelrc`, toolchains).

So a graph index built at one commit stays valid at another for every package
whose inputs did not change. A run loads an index for the base, classifies
`git diff --name-status` into dirty packages and directly-changed files,
re-queries only the dirty packages (`bazel query 'set(//a:* //b:* ...)'`),
splices those rows into the index, derives the direct set, and walks the
reverse edges. Every step is proportional to the change or to the answer.
[docs/ENGINE_COSTS.md](docs/ENGINE_COSTS.md) is where the time and the bytes
actually go.

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

## Five minutes

`cambi` has nine verbs: `init`, `status`, `doctor`, `affected`, `explain`,
`profile`, `index`, `inspect`, `config`. [docs/CLI.md](docs/CLI.md) is the reference for
all of them, their flags and their output rules; this is the path through.

```console
$ cambi init --store-dir /var/cache/cambium
```

Writes `cambium.toml` and nothing else — no index, no Bazel, no credentials.
It detects `.bazelversion`, WORKSPACE versus bzlmod, the `.bazelrc`
`try-import`s and the default branch, and asks only for the store.

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

One cold `bazel query //...:*`, which mints a lineage and uploads the first
index. Everything after this is incremental. `cambi status` says where the
checkout is and which index a run would load.

```console
$ cambi affected origin/main..HEAD --tests-only
$ bazel test $(cambi affected origin/main --tests-only --format query)
```

The range is spelled as `git diff` spells it; a single ref means `<ref>..HEAD`,
and no range means the merge base of `[git].default_branch` with `HEAD`.
`--dry-run` prints the plan — which index would be loaded, how the diff
classifies, and whether the run would splice, cold-rebuild or go global —
and exits before any Bazel query.

```console
$ cambi explain //app:main_test origin/main..HEAD
```

Answers "why is this test running?": the shortest path from the target back to
the change and the direct reason at the end of it. With no label it tells the
run's own story instead — what made it global or cold, with the `help:` line
that names the fix.

`cambi inspect` asks graph questions against one index file — `deps`, `rdeps`,
`path`, `owner`, `consumers`, `targets`, `stats`, `externals`, `verify`. It is
plumbing, and it is how a surprising `explain` answer gets checked.

### Keeping indexes warm

A post-merge job on the main branch keeps the store populated, so pull-request
runs almost always find their merge base directly:

```console
$ cambi index update <parent-sha>..<merge-sha> --publish
```

When a run does not find one, it walks back up to `max_ancestor_walk`
first-parent ancestors, takes the nearest index it finds, and replays it
forward to the base in a single update. Failing that, it builds cold.
`cambi affected --publish-base` publishes that computed base index so the next
run on the same base finds it; only the base, because a pull-request head is
not on the main line and nothing will ask for it again.

Weekly, or whenever `cambium.toml` changes, mint a fresh lineage and drop the
accumulated tombstones:

```console
$ cambi index build --publish && cambi index gc
```

[docs/STORE.md](docs/STORE.md) is the store: the key layout, compression at
rest, what makes an index incompatible, why concurrent publishers are safe,
and the exact S3 and GCS command templates.

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
| 4 | no index available and `--no-full-build` was given |
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
| 2 | `inspect stats` gained `bytes.label_sorted`, `bytes.label_total_share` and `bytes.label_packing` |
| 3 | `affected` became `[{label, depth, test}]`; `stats.rbuildfiles_called`; `inspect stats` gained `in_degree.top` (the hub list) |
| 4 | `stats.bzl_closure_hits`; `stats.rbuildfiles_called` is now always `false` — a changed `.bzl` is resolved from the index's static load closure instead of from Bazel. `inspect stats` gained `counts.bzl_files`, `counts.bzl_workspace_files` and `bytes.bzl_closure` |
| 5 | `stats.bazelrc_safe_diffs` (changed bazelrc files whose every changed flag was on the loading-irrelevant allowlist, and which therefore invalidated nothing); `stats.cold_rebuild` and the optional `cold_rebuild_reason` (the dirty set exceeded `limits.max_dirty_packages`, so the run re-indexed at head — still exact, not global; `limits.max_changed_files`, which used to make that case global, is gone); `stats.repos_dirty_by_input`, `stats.repos_dirty_by_module`, `stats.repos_dirty_by_bzl` and `stats.external_direct_targets`; `--explain` gained the direct reason `{"kind": "external_repo", "label": ...}` — the target names something in an external repository whose contents moved (a `MODULE.bazel` edit, or a main-repo file the repository is built from) |
| 6 | `stats.repos_label_only`: of the repositories an input-path edit dirtied, how many the repository's derived path mapping showed the edit could not be a `.bzl` loaded from — those repositories contribute their label consumers to the direct set, and their loading packages are not re-queried |
| 7 | `stats.bzl_witnessed_dead`, `stats.bzl_witnessed_loaded`, `stats.bzl_witness_packages` and `stats.bzl_witness_unavailable`: what the engine did about a changed `.bzl` the base index's load closure had no node for. `stats.bazelrc_flag_nodes` and the `bazelrc_flags` list: a `.bazelrc` edit that changes a Starlark build setting is no longer automatically global, and `--explain` gained the direct reason `{"kind": "starlark_flag", "label": ...}` |
| 8 | `stats.repos_dirty_by_target`: external repositories whose *generated* targets name a main-repo rule target the reverse walk reached — the `@envoy_repo` shape, where the BUILD file the repository rule writes lists `@envoy//changelogs` in a `genrule`'s `tools`. Such a repository is found after the walk rather than from the diff, so the walk runs again from its label consumers, and `--explain` gained the direct reason `{"kind": "external_repo_target", "repo": ..., "via": ...}`. Only the repository's *targets* move this way: a repository rule runs during loading and cannot read a rule target's output, so no `.bzl` it exports can have changed and no package's load closure is re-queried |
| 9 | `cambi status --format json` and `cambi doctor --format json`, new documents (fields above): a new document needs no bump, because the version says what the *existing* documents mean and a reader that has never seen `status` cannot be broken by its arrival. The same holds for `explain`, below. Also at 9: `diagnostics` on the affected report: the actionable warnings a run raised, each `{code, level, message, location?, help?}` with a stable `CAMBI-W` code, omitted when empty. Purely additive — a run with nothing to say prints what version 8 printed. Also at 9: `cambi explain --format json` and `cambi affected --dry-run --format json`, two more new documents on the same terms. The CLI gained exit code 5 (the store could not be reached) and the global `--format`, with `--output` kept as an alias |

Also at 9, on the same terms: `cambi profile --format json`, the document
behind the sentences `profile` prints — `index` (targets, tests, packages,
external repositories, `bytes_on_disk`, `compression`, `load_ms`), `cold`
(the cold build's commit and `wall_ms`), one `commits[]` entry per step
(`changed_files`, `dirty_packages`, `affected`, `tests`, `wall_ms`, `global`,
`global_reason`, `engine_cold_rebuild`) and `summary` (`mean_ms`, `p50_ms`,
`p90_ms`, `max_ms` over every step; `incremental` and `rebuild`, the same
four plus `commits` over the steps answered incrementally and the steps a
cold build answered; `globals`, `rebuild_every_commits`, `global_reasons`,
`engine_cold_rebuilds`, `mean_affected`, `zero_dirty_fraction`).

The version is shared by every document, so a bump made for one of them moves
all of them. The affected report did not change shape between 1 and 2; it did
at 3, and `--max-depth n` (on `affected` and `inspect rdeps --transitive`)
truncates the walk at `n` hops, which is what the per-target `depth` makes
meaningful.

## Configuration

`cambium.toml` at the repository root, checked in, every field optional. It
says what the graph is — Bazel flags, invalidators, ignore globs, the
`.bazelrc` allowlist, the store, the limits — and is hashed into every index
it produces, so an index built under a different config cannot be spliced
onto and `cambi index build` mints a fresh lineage instead.

Beside it, gitignored, `cambium.local.toml`: what is true of this machine and
not of the graph. Only the five keys already outside the config hash may
appear — `bazel.binary`, `bazel.startup_flags`, `baseline.cache_dir`,
`baseline.compress_indexes`, `baseline.publish_partial` — and any other key is
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

Two keys are worth knowing before reading the full list:
`[invalidators].paths` and `.globs` are the paths whose change means
"everything is affected", and `limits.max_dirty_packages` bounds the only part
of a run whose cost grows with the diff — past it the run asks Bazel for the
whole repository once and reports `stats.cold_rebuild` with
`cold_rebuild_reason`. The answer is exact either way: a cold rebuild is a
slower route to the same affected set, not a surrender to `//...`.

Every section, every default and every store backend is in
[docs/CLI.md](docs/CLI.md#configuration).

## Benchmarks

`cambium-bench` runs cambium, bazel-diff and Tango on the same *subject* — an
open-source Bazel monorepo from `subject_repos.toml`, or a generated graph
named `synth:<preset>` emitted as a real workspace with a real history. It is
its own binary and drives `cambi` the way a user does, by running it, so the
benchmark measures the binary that ships.

`cambium-bench run` is the whole thing from flags — per subject the fetch,
the cold build, both workloads and the report — as `tools/bench/run-published.sh`
runs it on the benchmark box; with no verb, on a terminal, `cambium-bench`
composes the same plan on a screen. Before that box is started,
`cambium-bench pull-results` copies a finished run's results off it and
`cambium-bench replay` reruns the engine's static stages over every recorded
change set offline, in milliseconds each, against the answers the run
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
  bench/           the benchmark: subjects, tools, oracles, workloads, report
  cli/             the `cambi` binary
  bench-cli/       the `cambium-bench` binary, which drives `cambi`
  testkit/         the fixtures the crates' tests share; never shipped
```

`index` and `synth` do not depend on `bazel`. `engine` is the only crate that
depends on everything, and `cli` is thin. `bench-cli` is thinner still: it is
the harness's front door, and the harness measures `cambi` by running it.

## Testing

```console
$ cargo test --workspace                     # unit, integration, oracles; no Bazel
$ CAMBIUM_E2E=1 cargo test -p cambium-engine --test e2e_bazel -- --ignored   # needs Bazel
$ cargo test -p cambium-bench --test smoke_real_bazel -- --ignored            # needs bazelisk
$ CAMBIUM_SOUNDNESS_SIZE=medium CAMBIUM_SOUNDNESS_SEEDS=50 \
    cargo test --release -p cambium-engine --test soundness      # nightly
```

`cargo test --workspace` runs nothing that needs Bazel: every engine a test
opens points at a Bazel binary that does not exist, and the rows come from
the synthetic source or from a scanner over a temporary tree. Those fixtures
— a scratch directory, a git repository, an engine over one, a package
querier that also serves `.bzl` text, a synthetic history with an index
already published — live in `crates/testkit` (`cambium-testkit`), a
dev-dependency of every crate with tests and nothing else. The tests that
need a real Bazel, a network fetch, or a corpus checkout are `#[ignore]`d, so
a plain run reports them as ignored rather than as passes that ran nothing;
each is opted into with `-- --ignored` and says what it is missing when run
without it (`CAMBIUM_REQUIRE_TOOLS=1` turns a missing tool from a skip into
a failure, which is what the nightly job sets).

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
written as zero. The spec uses it to drive one branch of the direct-set rule —
when a package's `.bzl` load closure changed, every rule in it is direct
regardless of its own hash — and that branch is load-bearing: a `.bzl` edit
that changes a rule implementation without changing any BUILD attribute leaves
every rule proto byte-identical, so nothing would be reported. cambium takes
the branch on **load-closure membership** instead. The index stores, per `.bzl`
file, the packages whose BUILD file transitively loads it
(`crates/index/src/loads.rs`), and those are exactly the packages whose closure
hash would have moved — a hash could only say *that* something moved, where the
stored closure says *which packages*, which is the question the direct set
actually asks. The `edit bzl` end-to-end scenario pins it: it asserts every
downstream target is reported *because of the load closure* rather than a rule
hash, so if the coverage ever moves to the other branch, the test says so.

Computing the closure by scanning `load()` statements at index time is also
what removed the last Bazel call from the dirty-set step. It used to be
`rbuildfiles`, which needs Sky Query (`--universe_scope=//...`) and therefore
loads every external repository: on a machine that cannot fetch one it exits 3
with a partial answer, and a partial answer there can be missing exactly the
package that loads the changed file — so the run had to invalidate everything.
That fired on 5 of 150 `bazel/bazel` commits. The benchmark still makes the
call, as the `load` oracle over the static closure
(`crates/bench/src/oracles/load.rs`).

## License

MIT. See [LICENSE](LICENSE).

One exception, as you would expect: `crates/cambium-proto/proto/build.proto` is
vendored from Bazel and stays under its own Apache-2.0 header.
