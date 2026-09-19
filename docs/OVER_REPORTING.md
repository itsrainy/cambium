# Where the answer is a superset

Every place the engine answers more than a cold `bazel query rdeps` would, on
purpose or as a side effect. Each row says what fires it, what it answers, and
whether a diagnostic names it. Under-reports are not here: each known one is
an open issue (#22 through #27, filed 2026-09-17).

Two rules the design leans on, so they are stated once: over-attribution is
safe and under-attribution is not, and a whole-repository answer is always
sound. Everything below is one of those two, at some scale.

## Whole-repository answers (`//...`)

| mechanism | fires on | diagnostic | where |
|---|---|---|---|
| Invalidator path or glob | a changed path in `[invalidators]`, the root `BUILD`, `tools/toolchains/**`, `platforms/**` | `CAMBI-W013` | `crates/classify/src/lib.rs` (`is_invalidator`), `crates/engine/src/run.rs` |
| `.bazelrc` screen | a native flag outside the loading-irrelevant allowlist (`--registry`, `--incompatible_*`, `--define`, `--repo_env`, ...), any `--config`-scoped flag, an `import` line moved, a startup flag, an rc file added, removed or unreadable | `CAMBI-W004` | `crates/engine/src/run.rs` (`screen_bazelrc`), `crates/engine/src/bazelrc.rs` |
| Starlark build setting unresolvable | a `.bazelrc` sets `--@repo//:flag` and the setting is outside the index, read by a transition, or not a parseable label | `CAMBI-W015` | `crates/engine/src/flags.rs` |
| No loading layer in the base index | an index built without Bazel behind it, or one predating format 11 | `CAMBI-W008` | `crates/engine/src/run.rs` (`loading_layer_walk`) |
| Workspace-level `.bzl` whose repositories cannot be named | a changed `.bzl` reached through `KeyForBzlmod` / `KeyForWorkspace` with no attribution | `CAMBI-W009` | `crates/engine/src/run.rs` (`workspace_bzl`), `crates/engine/src/externals.rs` |
| `WORKSPACE` statement not localised | a statement the shape reader cannot place, including any reordering; the file added or removed | `CAMBI-W014` | `crates/engine/src/externals.rs`, `crates/index/src/workspace.rs` |
| `MODULE.bazel` or lockfile not localised | an unparsed statement, lock added or removed, unknown lock key, format version change | `CAMBI-W014` | `crates/engine/src/externals.rs`, `crates/index/src/lockfile.rs` |
| Toolchain registration | `register_toolchains`, `register_execution_platforms`, or any module on `TOOLCHAIN_MODULES` | `CAMBI-W014` | `crates/engine/src/externals.rs`, `crates/index/src/vocabulary.rs` |
| Nested repository nothing maps | a changed file under a directory with its own `WORKSPACE` or `MODULE.bazel` that no path map names and `.bazelignore` does not exclude | `CAMBI-W014` | `crates/engine/src/run.rs`, `crates/engine/src/externals.rs` (`repo_rooted_at`) |

The `.bazelrc` screen is the dominant cause on the subjects: 13 of envoy's 18
history globals and 6 of xla's 7 (docs/RESULTS.md). The rest fire rarely; the
invalidator lists of every subject are now empty.

## Cost only, answer exact

| mechanism | fires on | diagnostic | where |
|---|---|---|---|
| `max_dirty_packages` overflow | more than 500 dirty packages (default) | `CAMBI-W012` | `crates/engine/src/run.rs` (`query`), `crates/engine/src/config.rs` |

The run re-queries `//...` and splices all of it, and the layer refresh dumps
the whole repository. 37 to 39 of envoy's 300 history commits take this path,
driven by `api/bazel/api_build_system.bzl`; apollo 3 to 4; xla 0 to 1.

## Wide but bounded answers

| mechanism | fires on | what it answers | where |
|---|---|---|---|
| `.bzl` edit marks every rule in every loading package direct | any `.bzl` reached through `BZL_LOAD` edges | every rule in each package that loads it, whatever its hash | `crates/engine/src/direct.rs` (`BzlClosure`), `crates/index/src/sky/mod.rs` |
| External repository declaration moved | a `MODULE.bazel` bump, lock re-pin, workspace `.bzl` edit, repository-rule input edit | every `@repo//...` leaf in the index for that repository, plus every package whose load closure names one | `crates/engine/src/externals.rs` (`reach`), `crates/engine/src/direct.rs` |
| Repository BUILD names an affected main-repo target | a change reaching a target named in the BUILD file an overlay repository is built with (`build_file = <main-repo label>`), or in the BUILD text a repository rule writes | that repository's whole leaf set, through the repository target fixpoint below | `crates/index/src/externals/inputs.rs` (target-only inputs), `crates/engine/src/direct.rs` (`close_over_repository_targets`) |
| Repository-rule attribution fallback | a `name =` the analysis cannot evaluate, a symbol outside the closure, an unparsed statement | every repository under the seeded subtree, or every repository the main repo defines | `crates/index/src/externals/mod.rs` (the per-occurrence ladder) |
| Repository target fixpoint | an affected main-repo target a repository rule is built around | that repository's whole leaf set, to a fixpoint (a 900k-target repo once seeded 3819 repositories from one target) | `crates/engine/src/direct.rs` (`close_over_repository_targets`), logged at info above 8 repositories |
| Nested repository mapped wholesale | any file under a mapped boundary such as `third_party/xla` | the whole mapped repository dirty | `crates/engine/src/run.rs`, `crates/engine/src/externals.rs` (`dirty_by_input`, `input_can_be_a_loaded_bzl` answers true to every unanswerable question) |
| `--keep_going` tombstone | a dirty package fails to load at head | every consumer of its targets, as a cold build would; `CAMBI-W005` | `crates/engine/src/query.rs`, `crates/engine/src/direct.rs` |
| `.bazelignore` edit | any change to the file | every package Bazel consulted the ignore list for, in practice all of them; the one singleton key the layer keeps | `crates/index/src/sky/mod.rs` (`Kind::Ignored`) |
| Package the layer cannot name | a `BUILD` file naming a package with no `PACKAGE` node in the base layer | that whole package queried and spliced | `crates/engine/src/run.rs` (`packages_the_layer_cannot_name`) |
| Deleted-`BUILD` survivors | `BUILD` deleted while `BUILD.bazel` survives, or a rename | the package kept and re-queried rather than tombstoned | `crates/engine/src/run.rs` (`keep_surviving_packages`) |
| Deleted packages in `dirty_packages` | any package the diff deleted | the name reported dirty though never queried; how the rdeps oracle credits its deleted consumers | `crates/engine/src/run.rs`, `crates/bench/src/oracles/rdeps.rs` |
| `BUILD` as a glob input | a modified `BUILD` in a `glob(["**"])` package | every rule consuming the `BUILD` as a source | `crates/classify/src/lib.rs` |
| Layer rows a cold build would not write | any splice: a refresh replaces rows and never removes a node | inert extra nodes and edges, counted as `extra` by the rebuild oracle and named row by row in `walk/rebuild-<pin>.json` (`deleted_in_window` when the path is gone at the pin, `other` otherwise) | `crates/bench/src/oracles/rebuild.rs`, `crates/bench/src/walk/rebuild_rows.rs` |

On envoy's 40 change sets the benchmark's classifier attributes cambium's
extras as: external repository reach 65 226 labels, `.bzl` closure 10 605,
`select()` branches 8 789, implicit attributes 1 704 (docs/RESULTS.md).

## Inherited from the unconfigured graph

| mechanism | what it answers | where classified |
|---|---|---|
| Every `select()` branch kept | all branches, not the configured one | `cambium_extra_select`, docs/METHODOLOGY.md |
| Implicit and private attribute edges walked | targets reached only through `$`-prefixed attributes, which `--noimplicit_deps` peers drop | `cambium_extra_implicit` |
| Rule hash ignores visibility and tags | a visibility-only edit surfaces only through a package re-query | `cambium_extra_visibility` |

These are the price of answering from the query graph rather than from
analysis, and the skyframe-analysis prototype (docs/SKYFRAME_PROTOTYPES.md)
is the measurement that says the price is worth paying.
