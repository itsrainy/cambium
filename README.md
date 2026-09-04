# cambium

Incremental changed-target computation for large Bazel monorepos.

Given a repository at two commits, cambium reports the build and test targets
whose outputs could differ between them — at a cost proportional to the size of
the change and the size of the answer, not the size of the repository.

```console
$ cambium affected --base origin/main --head HEAD --tests-only
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
| dirty set | O(changed files × path depth), plus one `rbuildfiles` call if a `.bzl` changed |
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
```

No `protoc` needed — the vendored `build.proto` is compiled by `protox`, a
pure-Rust protobuf compiler. Bazel itself is needed at runtime, not to build.

## Usage

```console
$ cambium affected --base <sha> --head <sha> [--tests-only] [--include-manual]
                   [--output text|json|query] [--explain]
$ cambium baseline build  [--commit <sha>] [--publish]
$ cambium baseline update --from <sha> --to <sha> [--publish]
$ cambium baseline gc | list
$ cambium index inspect <file> [--label //x:y] [--package p] [--rdeps //x:y]
$ cambium index verify  <file> --against-bazel
$ cambium bench run|sweep|compare|bazel
$ cambium synth emit-bazel|describe|presets
$ cambium config
```

`--output query` emits a `set(...)` expression you can hand straight to Bazel:

```console
$ bazel test $(cambium affected --base origin/main --tests-only --output query)
```

`--explain` prints, for each affected target, the shortest path back to the
change and why that starting point was direct — the answer to "why is this
test running?".

Exit codes: `0` success, `1` error, `2` global invalidation (stdout is
`//...`), `3` Bazel failure, `4` no baseline and `--no-full-build` was given.

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

[ignore]                     # changes that cannot affect the build graph
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
produced it may no longer hold — `cambium baseline build` mints a fresh
lineage instead.

Store backends: `dir` (a local directory), `http` (GET/PUT against a base
URL), and `command`, whose argument templates get `{name}` and `{path}`
substituted so `gsutil`, `aws s3`, `curl` or an internal CLI can be plugged in
without touching Rust.

### Keeping baselines warm

A post-merge job on the main branch keeps the store populated, so pull-request
runs almost always find their merge base directly:

```console
$ cambium baseline update --from <parent-sha> --to <merge-sha> --publish
```

Weekly, or whenever `cambium.toml` changes, mint a fresh lineage and drop the
accumulated tombstones:

```console
$ cambium baseline build --publish && cambium baseline gc
```

## Layout

```
crates/
  index/           CSR graph, interning, on-disk format, mmap loader, splice
  cambium-proto/   vendored build.proto, streamed_proto reader, rule hashing
  bazel/           spawning bazel: query, rbuildfiles, workspace detection
  diff/            git diff --name-status parsing and revision plumbing
  classify/        path -> package, invalidator rules, dirty-set computation
  engine/          a run: load, dirty, query, splice, direct set, walk
  synth/           synthetic graph generator, mutations, Bazel repo emitter
  bench/           stage benchmarks, sweeps, regression gating
  cli/             the `cambium` binary
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
4. **`cambium index verify --against-bazel`** — for a sample of targets, our
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
and nothing would be reported. cambium takes that branch on `rbuildfiles`
membership instead — the packages `rbuildfiles` names are exactly the packages
whose closure hash would have moved — so the behaviour is the spec's, and the
stored hash would be a second, redundant answer to a question already answered.
Computing it statically needs a Starlark load-graph index, which the spec lists
as a v2 candidate and which would let us skip the `rbuildfiles` call on cold
Bazel servers.

The `edit bzl` end-to-end scenario pins this: it edits a constant nothing uses,
asserts the whole downstream closure is reported, and asserts that every one of
those targets is reported *because of the load closure* rather than a rule hash
— so if the coverage ever moves to the other branch, the test says so.

## License

MIT. See [LICENSE](LICENSE).

One exception, as you would expect: `crates/cambium-proto/proto/build.proto` is
vendored from Bazel and stays under its own Apache-2.0 header.
