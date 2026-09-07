# cambium

Incremental changed-target computation for large Bazel monorepos.

Given a repository at two commits, cambium reports the build and test targets
whose outputs could differ between them — at a cost proportional to the size of
the change and the size of the answer, not the size of the repository.

The project is cambium; the binary it installs is `cambi`.

```console
$ cambi affected --base origin/main --head HEAD --tests-only
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
| splice | O(edges) memcpy |
| direct set | O(dirty targets + changed files) |
| walk | O(affected + their in-edges) |

Only the splice touches the whole index, and only because v1 writes a complete
new one. It is ~52% of a run on a 500k-target graph. The fix — a delta overlay
for the `affected` path, leaving `baseline update` materialising, since its
output is what gets published — is described in
[BENCHMARKS.md](BENCHMARKS.md#where-the-time-actually-goes).

## Install

```console
$ cargo install --path crates/cli    # or: cargo build --release
$ cambi --version
```

The crate is `cambium-cli`; the binary it installs is `cambi`.

No `protoc` needed — the vendored `build.proto` is compiled by `protox`, a
pure-Rust protobuf compiler. Bazel itself is needed at runtime, not to build.

## Usage

```console
$ cambi affected --base <sha> --head <sha> [--tests-only] [--include-manual]
                   [--output text|json|query] [--explain] [--max-depth n]
$ cambi baseline build  [--commit <sha>] [--publish]
$ cambi baseline update --from <sha> --to <sha> [--publish]
$ cambi baseline gc | list
$ cambi index inspect   <file> [--label //x:y] [--package p]
$ cambi index rdeps     <file> //x:y  [--transitive] [--tests-only] [--max-depth n]
$ cambi index deps      <file> //x:y  [--transitive]
$ cambi index path      <file> //from:a //to:b
$ cambi index consumers <file> <path>...  [--transitive]
$ cambi index owner     <file> <path>...
$ cambi index targets   <file> <package> [--kind rule]
$ cambi index stats     <file>
$ cambi index verify    <file> --against-bazel
$ cambi bench run|sweep|legacy|compare|bazel
$ cambi corpus fetch|build|replay|pairs|compare|report|all <id>
                   [--bazel-version v] [--manifest corpus.toml] [--corpus-dir d]
$ cambi corpus compare <id> [--pairs f.json] [--kind adjacent|pr|stress] [--cap n]
                   [--oracle-sample k] [--bazel-diff rust|kotlin|none] [--no-tango]
                   [--producers bazel-diff-rust,tango]
$ cambi synth emit-bazel|describe|presets
$ cambi config
```

`--output query` emits a `set(...)` expression you can hand straight to Bazel:

```console
$ bazel test $(cambi affected --base origin/main --tests-only --output query)
```

`--explain` prints, for each affected target, the shortest path back to the
change and why that starting point was direct — the answer to "why is this
test running?".

### Querying an index

The `index` subcommands answer the same questions against an index that
already exists, in text or JSON (`--output json`, carrying `schema`):

```console
$ cambi index consumers idx.bin lib/core/parser.cc --transitive
lib/core/parser.cc	//lib/core:core
lib/core/parser.cc	//app:main
lib/core/parser.cc	//app:main_test

$ cambi index path idx.bin //app:main_test //lib/core:core
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
[not doing that](BENCHMARKS.md#labels-33-and-not-compressed); run it on an index
of your own repository, where package paths are longer than the generators'
and the answer may well come out differently.

To get an index to point it at without going near Bazel:

```console
$ cambi synth build-index --spec large --out large.bin
$ cambi index stats large.bin
```

They share the engine's walk and the same package-ownership lookup the dirty
set uses, so an `index` answer and an `affected` answer cannot disagree.

## The JSON contract

Everything cambium prints as JSON is meant to be parsed by something that is
not cambium, so the field names, their shapes, and the exit codes are a
versioned contract. Every JSON document carries the version it was written
against:

```json
{ "schema": 4, "base": "...", "head": "...", ... }
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

### `cambi affected --output json`

| field | type | meaning |
|---|---|---|
| `schema` | int | contract version |
| `base`, `head` | string | resolved revisions |
| `global` | bool | everything is affected; `affected` and `tests` are empty and the answer is `//...` |
| `global_reason` | string, optional | why, when `global` |
| `dirty_packages` | \[string] | packages re-queried, without the leading `//` |
| `direct` | \[string] | labels whose own definition or inputs changed |
| `affected` | \[object] | `direct` plus its reverse closure, sorted by `label`; each entry is `{label, depth, test}`, where `depth` is the number of reverse-dependency hops from the change (0 for the direct set; a hop through a deleted dependency counts) |
| `tests` | \[string] | the subset of `affected` that is a test |
| `timings_ms` | object | `load`, `dirty`, `query`, `splice`, `direct`, `walk`, `total` |
| `stats` | object | `base_nodes`, `head_nodes`, `head_edges`, `dirty_packages`, `queried_targets`, `direct_targets`, `affected_targets`, `changed_files`, `ignored_files`, `rbuildfiles_called`, `bzl_closure_hits` |
| `explanations` | \[object], optional | with `--explain`: `target`, `path` (target → direct member), `direct`, `reason` |

`reason` is tagged by `kind`: `new`, `rule_hash`, `bzl_closure` (with
`package`), `file` (with `path`), `deleted_dep` (with `label`).

Optional fields are omitted rather than null, so absence is meaningful. A
document with no `schema` key predates the field and is version 1.

`cambi index ... --output json` and `cambi bench` results carry `schema`
on the same terms.

### Contract versions

| version | change |
|---:|---|
| 1 | the first published contract |
| 2 | `index stats` gained `bytes.label_sorted`, `bytes.label_total_share` and `bytes.label_packing` |
| 3 | `affected` became `[{label, depth, test}]`; `stats.rbuildfiles_called`; `index stats` gained `in_degree.top` (the hub list) |
| 4 | `stats.bzl_closure_hits`; `stats.rbuildfiles_called` is now always `false` — a changed `.bzl` is resolved from the index's static load closure instead of from Bazel. `index stats` gained `counts.bzl_files`, `counts.bzl_workspace_files` and `bytes.bzl_closure` |

The version is shared by every document, so a bump made for one of them moves
all of them. The affected report did not change shape between 1 and 2; it did
at 3, and `--max-depth n` (on `affected` and `index rdeps --transitive`)
truncates the walk at `n` hops, which is what the per-target `depth` makes
meaningful.

### Configuration

`cambium.toml` at the repository root. Every field is optional; the defaults
below are what you get with no file at all.

```toml
[bazel]
binary = ""                  # else $BAZEL_REAL, $BAZEL, bazelisk, bazel
extra_query_flags = []

[invalidators]               # any change here means "everything is affected"
paths = ["MODULE.bazel", "MODULE.bazel.lock", "WORKSPACE", "WORKSPACE.bazel",
         "WORKSPACE.bzlmod", "WORKSPACE.bzlmod.lock", ".bazelrc",
         ".bazelversion", "BUILD", "BUILD.bazel"]
globs = ["tools/toolchains/**", "platforms/**", "third_party/**/*.bzl"]

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
max_changed_files = 5000     # larger diffs fall back to global invalidation
```

The config is hashed into every index it produces. An index built under a
different config cannot be spliced onto, because the invalidator rules that
produced it may no longer hold — `cambi baseline build` mints a fresh
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
$ cambi baseline update --from <parent-sha> --to <merge-sha> --publish
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
$ cambi baseline build --publish && cambi baseline gc
```

## Benchmarks

`cambi bench run` and `cambi bench sweep` measure each stage of the pipeline
against synthetic graphs at sizes no real repository reaches, and
`cambi bench legacy` puts cambium and bazel-diff on the identical graph and
change — a `bazel` shim replays the graph dump the generator already produces,
so the measured time is each tool's own work rather than a Bazel server's, and
the two label sets have to agree. See [docs/BENCH.md](docs/BENCH.md).

## Validation against real repositories

`cambi bench legacy` is a like-for-like comparison against bazel-diff, but it
runs on a synthetic graph. The corpus suite (`cambi corpus ...`,
`crates/corpus`, `crates/compare`) runs the same kind of three-way comparison
— cambium vs bazel-diff vs Tango vs a from-scratch `rdeps` oracle — against
real Bazel monorepos (bazel, envoy, sourcegraph, apollo, xla, tensorflow), plus
an updater replay over each repository's real commit history. See
[docs/CORPUS.md](docs/CORPUS.md) for what it proves, how to run it locally,
the disagreement classes, and the current results.

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
  bench/           stage benchmarks, sweeps, regression gating, bazel-diff comparison
  cli/             the `cambi` binary
```

`index` and `synth` do not depend on `bazel`. `engine` is the only crate that
depends on everything, and `cli` is thin.

## Testing

```console
$ cargo test --workspace                     # unit, integration, oracles
$ CAMBIUM_E2E=1 cargo test -p cambium-engine --test e2e_bazel   # needs Bazel
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
4. **`cambi index verify --against-bazel`** — for a sample of targets, our
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
repositories and synthetic revision ids both fit; and `bench sweep --plot`
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
That fired on 5 of 150 `bazel/bazel` commits in the corpus. The comparison
suite still makes the call, as an oracle over the static closure
(`crates/compare/src/bzloracle.rs`).

The `edit bzl` end-to-end scenario pins this: it edits a constant nothing uses,
asserts the whole downstream closure is reported, and asserts that every one of
those targets is reported *because of the load closure* rather than a rule hash
— so if the coverage ever moves to the other branch, the test says so.

## License

MIT. See [LICENSE](LICENSE).

One exception, as you would expect: `crates/cambium-proto/proto/build.proto` is
vendored from Bazel and stays under its own Apache-2.0 header.
