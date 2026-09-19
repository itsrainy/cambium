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
| Invalidator path or glob | a changed path in `[invalidators].paths` (`MODULE.bazel`, `.bazelrc`, `.bazelversion`, `REPO.bazel`, ...) or a configured glob; no glob is on by default | `CAMBI-W013` | `crates/classify/src/lib.rs` (`is_invalidator`), `crates/engine/src/run.rs` |
| `.bazelrc` screen | a native flag outside the loading-irrelevant allowlist (`--enable_bzlmod`, `--incompatible_enable_proto_toolchain_resolution`, `--repo_env`, ...) on a line the index query reads (`common`, `always`, `query`, under no config or one in effect), a `--config=x` the query reads that no rc file defines, an `import` line moved whose target cannot be read, a startup flag, an rc file added, removed or unreadable | `CAMBI-W004` | `crates/engine/src/run.rs` (`screen_bazelrc`), `crates/engine/src/bazelrc.rs` |
| `--registry` without its lock's diff | an rc line the query reads changed `--registry` and the externals stage read no `MODULE.bazel.lock` diff (the lock did not move, or it moved beside a changed `MODULE.bazel`, whose diff names only the modules whose declaration moved), with bzlmod live | `CAMBI-W014` | `crates/engine/src/run.rs` (`registry_change`) |
| Starlark build setting unresolvable | a `.bazelrc` sets `--@repo//:flag` and the setting is outside the index, read by a transition, or not a parseable label | `CAMBI-W015` | `crates/engine/src/flags.rs` |
| No loading layer in the base index | an index built without Bazel behind it, or one predating format 11 | `CAMBI-W008` | `crates/engine/src/run.rs` (`loading_layer_walk`) |
| Workspace-level `.bzl` whose repositories cannot be named | a changed `.bzl` reached through `KeyForBzlmod` / `KeyForWorkspace` with no attribution | `CAMBI-W009` | `crates/engine/src/run.rs` (`workspace_bzl`), `crates/engine/src/externals.rs` |
| `WORKSPACE` statement not localised | a statement the shape reader cannot place, including any reordering; the file added or removed | `CAMBI-W014` | `crates/engine/src/externals.rs`, `crates/index/src/workspace.rs` |
| `MODULE.bazel` or lockfile not localised | an unparsed statement, lock added or removed, unknown lock key, format version change | `CAMBI-W014` | `crates/engine/src/externals.rs`, `crates/index/src/lockfile.rs` |
| Toolchain declaration | a `toolchain`, `platform`, `toolchain_type`, `constraint_setting` or `constraint_value` rule the splice added, changed or tombstoned, in any package; decided after the query, so a dry run cannot reach it | `CAMBI-W023` | `crates/engine/src/run.rs` (`toolchain_rules`), `crates/index/src/vocabulary.rs` (`TOOLCHAIN_RULE_CLASSES`) |
| Toolchain registration | `register_toolchains`, `register_execution_platforms` in `MODULE.bazel`, in a `*.MODULE.bazel` segment the root `include()`s, or in `WORKSPACE` | `CAMBI-W023` | `crates/engine/src/externals.rs`, `crates/index/src/externals/module_diff.rs`, `crates/index/src/workspace.rs` |
| Registered repository moved | any route dirtied an external repository that a registration pattern at head offers rules from, except one dirtied only through a path a target the repository rule writes names | `CAMBI-W023` | `crates/engine/src/externals.rs` (`registered_repository_moved`), `crates/index/src/vocabulary.rs` (`registration_repo`) |
| Toolchain module moved | any module on `TOOLCHAIN_MODULES` bumped, overridden or re-pinned, except under a Bazel that autoloads, where a module that is also a provider (`rules_python`, `rules_java`, `rules_shell`) answers under `CAMBI-W024` instead (the row below) | `CAMBI-W014` | `crates/engine/src/externals.rs`, `crates/index/src/vocabulary.rs` |
| Autoload provider moved | `rules_python`, `rules_java`, `rules_shell`, `protobuf` or `rules_android` bumped, overridden, re-resolved in the lock or redeclared in `WORKSPACE`, under a Bazel that autoloads (8 at its default, or 7.4 with `--incompatible_autoload_externally` set; an unknown version counts as 8, and Bazel 9 autoloads nothing) | `CAMBI-W024` | `crates/engine/src/externals.rs` (`autoload_at`), `crates/index/src/vocabulary.rs` (`AUTOLOAD_PROVIDERS`) |
| Nested repository nothing maps | a changed file under a directory with its own `WORKSPACE` or `MODULE.bazel` that no path map names and `.bazelignore` does not exclude | `CAMBI-W014` | `crates/engine/src/run.rs`, `crates/engine/src/externals.rs` (`repo_rooted_at`) |

The `.bazelrc` screen was the dominant cause on the subjects: 17 of envoy's 18
history globals and 6 of xla's 7 in docs/RESULTS.md, before the screen learnt
which lines a query reads (the next section). Twelve of envoy's seventeen are
its root `.bazelrc`; the other five are nested workspaces' rc files, which the
screen now exempts rather than classifies. The rest fire rarely; the
invalidator lists of every subject are now empty.

The three `CAMBI-W023` rows postdate those numbers and are unmeasured on a
full walk. The last of them is the one with room to surprise: a tree whose `MODULE.bazel` registers from an
extension-defined repository now answers `//...` on any route that dirties
that repository, where it used to name the repository and stop. The tree to
check first is bazel's own, which registers this way. At `948b8c70e28` its
`MODULE.bazel` carries five registration statements, and two of them name an
extension-defined repository: `@local_config_winsdk//:all` (the
`winsdk_configure` repository rule) and `@graalvm_ce_toolchains//:gvm` (the
`bazel_build_deps` module extension). The other three (`//:all`,
`//:default_host_platform`, `//tools/res:empty_rc_toolchain`) are in the tree
itself and the rule-class check reads them as ordinary rows. So two
repositories there carry the wide answer.

## The `.bazelrc` screen (`CAMBI-W004`)

The index is `bazel query //...`, so an rc edit matters exactly when that
query would load or report something different. The screen reads the whole
rc chain (`.bazelrc`, `--bazelrc=` files inside the repository, and their
`import`/`try-import` closure) at both ends of the range and screens the
(command, config, flag) triples the query reads at each end. Four scopes
are applied before any flag is classified:

- **Command.** Bazel applies an rc line to the command it names and to the
  commands that inherit from it. `query` inherits from nothing, so only
  `common`, `always` and `query` lines reach it. A `build`, `test`, `run`,
  `coverage` or `cquery` line is a fact about builds, not about the graph
  the index holds, and is never a change to the screen. `startup` lines
  reach every command and are never classified.
- **Config.** A `command:name` line applies only when `--config=name` is in
  effect. The configs in effect for the query are `[bazelrc].configs` and
  any `--config` in `[bazel].extra_query_flags` (both empty on every subject;
  the walk passes none), plus every `--config=` a line that is itself in
  effect names, to a fixpoint, plus the five host names when
  `--enable_platform_specific_config` is in effect (the host is not
  modelled, so all of them count). A triple under any other config is
  skipped. `--config` itself is never a changed flag: it is expanded, and
  the flags it pulls in are what is screened, so the reason reads
  `common:ci --repo_env` rather than a bare `--config`. A `--config=x` in
  effect that no rc file defines for any command is unknown.
- **Import.** An import is textual inclusion. A `try-import` whose target is
  missing at an end contributes nothing there, as in Bazel; a target present
  at an end is read into that end's chain and screened like the root, and
  the reason names it. A plain `import` of a missing file, or either keyword
  naming a file outside the repository, is unknown when the line itself
  moved. The chain is read from git at both revisions, never from the
  working tree, so an untracked developer override is out of scope.
- **Startup.** `[bazel].startup_flags` decide which files are rc files at
  all: `--bazelrc=` adds one, and `--noworkspace_rc` tells Bazel not to
  read `%workspace%/.bazelrc`, nor therefore anything it imports. Under it
  the root rc is inert, and an edit to it is an ordinary source change,
  like a nested workspace's `.bazelrc`, exempt from the `.bazelrc`
  invalidator. apollo runs this way on the benchmark (its own rc carries a
  query flag its pinned Bazel refuses), so its `.bazelrc` edits reach
  neither the screen nor `CAMBI-W004`.

What is left is classified by the allowlist, whose criterion is loading:
whether the flag can change which packages load, which targets exist, or
which edges the query reports. Analysis-phase configuration (`select()`
resolution, toolchain choice, compiler command lines) is declined by design
(docs/ROADMAP.md), and flags that only have those effects are on the list.
A Starlark build setting (`--@repo//pkg:flag`) is handed to the flag
resolver (`CAMBI-W015` when it cannot be resolved); `--registry` is handed to
the externals stage as a lock-file question. Two parse subtleties remain:
repeated lines count as a change of value (`['']` to `['', '']`), and
`--nofoo` and `--foo` are distinct keys, so one boolean flip is several
changed triples. Neither changes a verdict.

Every flag that fired `CAMBI-W004` across the five subject histories was
classified twice against the Bazel 8.7.0 and 9.2.0 sources and `bazel help
<command> --long`, and the two classifications reconciled. The table is also
`EVIDENCE` in `crates/engine/src/bazelrc.rs`, where a test holds the allowlist
to it.

| flag | fired on | verdict | reasoning |
|---|---|---|---|
| `--registry` | envoy 293, 295, and six pin bumps in the 300-step history | routed | RepositoryOptions, accepted by `bazel query`. Selects the module registries resolution reads, so a list or pin change can change resolved module versions, patches and MODULE.bazel contents. Loading-relevant, and the resolved state is `MODULE.bazel.lock` (since 7.2 keyed on a hash per registry file read). When the lock moved in the same diff and the externals stage localised it, its diff names the modules whose registry metadata moved and the rc line adds nothing; with bzlmod off the line is inert; otherwise the stage escalates (`CAMBI-W014`) |
| `--enable_bzlmod`, `--enable_workspace` | envoy 293 (added), 296 (removed) | relevant | Tags loading_and_analysis, accepted by `bazel query` in 8.7. They select the external dependency system: which repositories exist, canonical names, the repository mapping, whether WORKSPACE or MODULE.bazel is evaluated |
| `--incompatible_enable_proto_toolchain_resolution` | envoy 80, 235 | relevant | A StarlarkSemantics option (BuildLanguageOptions), accepted by `bazel query`; part of every `.bzl` load key, and protobuf's `toolchain_helpers.bzl` reads it at `.bzl` top level to add or drop the legacy `_proto_compiler` attributes on proto rules, which are query-visible edges. The 235 instance was a duplicated line with the same value, which the multiset diff cannot tell from a change |
| `--incompatible_default_to_explicit_init_py` | envoy 235 | irrelevant | Tags affects_outputs; the Python fragment reads it at analysis to decide whether `__init__.py` files are synthesised into runfiles. Not accepted by `bazel query`; a no-op in 9.2 |
| `--per_file_copt` | envoy 235 | irrelevant | CppOptions, tags action_command_lines; C++ compile command lines only |
| `--define` | envoy 235, 295 | irrelevant | CoreOptions; `bazel query` accepts it only because QueryCommand carries CoreOptions for `--action_env`, and never evaluates `select()`, `config_setting` or Make variables, its only consumers. cambium walks every `select()` branch, so no package, target or edge depends on the value; which branch is built is analysis-phase configuration, declined |
| `--features`, `--host_features` | xla 228 | irrelevant | CoreOptions; the effective feature set of a configured target, computed at analysis for the toolchain's feature configuration (for `-external_include_paths`, C++ compile flags). No package, target or edge changes |
| `--dynamic_mode` | xla 233 | irrelevant | CppOptions; read by `cc_binary`/`cc_test` through the cpp fragment to choose static versus dynamic linking. The loading_and_analysis tag is Bazel's coarse configured-graph bucket; not accepted by `bazel query` |
| `--java_language_version`, `--tool_java_language_version` | bazel 50 | irrelevant | JavaOptions; java_toolchain selection through config_settings on the java fragment at analysis. Not accepted by `bazel query` |
| `--cxxopt` | apollo (two steps) | irrelevant | CppOptions, tags action_command_lines; compile command lines only, and under `build`, which never reaches a query |
| `--test_tag_filters` | envoy 51 | irrelevant | LoadingOptions, but included only by build and print_action; filters which already-loaded tests run. Already allowlisted; every instance was config-scoped too |
| `--repo_env` | xla 239 | relevant | CommonCommandOptions, so every command reads it; folded into the repository environment that `repository_ctx.os.environ`/`getenv` and module extensions observe, and a changed value re-runs the repository rule. `HERMETIC_NCCL_VERSION` picks which NCCL redistribution xla downloads. `--action_env=NAME=VALUE` reaches the repository environment the same way and is not allowlisted either |
| `--config` (`rocm`, `workspace`) | xla 50, 208 | routed | An rc expansion, not a flag: its relevance is that of the expanded section (`common:workspace --noenable_bzlmod --enable_workspace` makes `--config=workspace` the bzlmod switch). Expanded at each end and classified as the flags it carries; both instances sit under `common:rocm_ci*`, which the query never selects |
| `import`, `try-import` | apollo 8 (`try-import %workspace%/.custom.bazelrc` added, tracked at neither revision), envoy's `bazel/tests/external/.bazelrc` | routed | Not a flag: an import edit is loading-relevant exactly when the imported file's triples are. `try-import` is textual inclusion Bazel skips when the file is missing, so a target absent at both ends contributes no line at either and the edit is harmless; a target present at an end is read into that end's chain and screened like the root. A plain `import` of a missing file fails Bazel, and a target outside the repository cannot be read, so either is unknown when the line itself moved. The chain is read from git at both revisions, never from the working tree, so an untracked developer override is out of scope |
| `--@repo//pkg:setting` (`@quiche//:*`, `@grpc//third_party:*`, `@protobuf//:*`, `@envoy//bazel:{crypto,ssl,enable_bzlmod}`, `@rules_ml_toolchain//common:*`, ...) | envoy 80, 235, 293, 295, 296; xla 233 | routed | Build settings: configuration visible only at analysis, unreadable by BUILD files, macros, repository rules or module extensions, so no package's content or edges depend on the value. Handed to the flag resolver rather than allowlisted: a main-repository setting is seeded through its `config_setting` edges, and an external one stays `CAMBI-W015`, soundly, because `bazel query` loads the flag's package while parsing the option, so a renamed or removed external setting changes what loads and can fail the query. Under a config that is not in effect the triple is skipped |

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
| Repository-rule attribution fallback | a `name =` the analysis cannot evaluate, a symbol outside the closure, an unparsed statement | every repository under the seeded subtree, or every repository the main repo defines | `crates/index/src/externals/mod.rs` (the per-occurrence ladder) |
| Repository target fixpoint | an affected main-repo target a repository rule is built around | that repository's whole leaf set, to a fixpoint (a 900k-target repo once seeded 3819 repositories from one target) | `crates/engine/src/direct.rs` (`close_over_repository_targets`), logged at info above 8 repositories |
| Nested repository mapped wholesale | any file under a mapped boundary such as `third_party/xla` | the whole mapped repository dirty | `crates/engine/src/run.rs`, `crates/engine/src/externals.rs` (`dirty_by_input`, `input_can_be_a_loaded_bzl` answers true to every unanswerable question) |
| `--keep_going` tombstone | a dirty package that does not exist at head: its `BUILD` file is gone, or this configuration declares it deleted | every consumer of its targets, as a cold build would; `CAMBI-W005` | `crates/engine/src/query.rs`, `crates/engine/src/direct.rs` |
| `--keep_going` tombstone, own `BUILD` edited | a dirty package whose own `BUILD` file this change edited fails to load | every consumer of its targets; the alternative would answer from rows that predate the edit; `CAMBI-W005` | `crates/engine/src/run.rs` (`query`) |
| `.bazelignore` edit | any change to the file | every package Bazel consulted the ignore list for, in practice all of them; the one singleton key the layer keeps | `crates/index/src/sky/mod.rs` (`Kind::Ignored`) |
| Package the layer cannot name | a `BUILD` file naming a package with no `PACKAGE` node in the base layer | that whole package queried and spliced | `crates/engine/src/run.rs` (`packages_the_layer_cannot_name`) |
| Deleted-`BUILD` survivors | `BUILD` deleted while `BUILD.bazel` survives, or a rename | the package kept and re-queried rather than tombstoned | `crates/engine/src/run.rs` (`keep_surviving_packages`) |
| Deleted packages in `dirty_packages` | any package the diff deleted | the name reported dirty though never queried; how the rdeps oracle credits its deleted consumers | `crates/engine/src/run.rs`, `crates/bench/src/oracles/rdeps.rs` |
| `BUILD` as a glob input | a modified `BUILD` in a `glob(["**"])` package | every rule consuming the `BUILD` as a source | `crates/classify/src/lib.rs` |
| Layer rows a cold build would not write | any splice: a refresh replaces rows and never removes a node | inert extra nodes and edges, counted as `extra` by the rebuild oracle and named row by row in `walk/rebuild-<pin>.json` (`deleted_in_window` when the path is gone at the pin, `other` otherwise) | `crates/bench/src/oracles/rebuild.rs`, `crates/bench/src/walk/rebuild_rows.rs` |

One `--keep_going` case is not on this page at all, and is here so that a
triage reading it does not go looking for the wide answer. A dirty package
bazel named in a load error, whose `BUILD` file this change did not edit,
keeps the rows the index already has (`CAMBI-W028`, docs/XLA_MAIN_REPO_MISSES.md).
That answer is stale, not wide: its consumers are not forced direct, and the
rows it answers from are as old as the last query that loaded the package.

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
