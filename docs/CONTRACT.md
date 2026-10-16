# The JSON contract

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

## Exit codes

| code | meaning |
|---:|---|
| 0 | success |
| 1 | error, or a usage error: one `cambi` diagnoses (`CAMBI-E013`), or a command line the argument parser rejects (an unknown flag or verb, a missing value), which prints the parser's message on stderr and nothing on stdout |
| 2 | global invalidation: everything is affected; stdout is `//...`. Also a run whose index is short of packages Bazel could not load that `baseline.publish_partial` does not name (`CAMBI-W006`, or `CAMBI-W028` for packages the run carried): the answer cannot be trusted to include what the change reaches through them; `--allow-partial` answers from the index instead |
| 3 | Bazel failed, or no Bazel workspace was found (`CAMBI-E004`), or the Bazel binary could not be started (`CAMBI-E015`) |
| 4 | no index available and `--no-full-build` was given |
| 5 | the store is there and could not be read (as against holding no index, which is 4): a CI job can retry this one. A directory store whose directory does not exist holds no index; the `aws s3 cp`/`gsutil cp` templates cannot tell an outage from a miss ([docs/STORE.md](STORE.md#misses-and-outages)) |

## Diagnostics

Every warning and error `cambi` reports carries a stable code (`CAMBI-W004`,
`CAMBI-E009`), a message, sometimes a location, and a `help:` line naming the
flag, file or key to change. The human form follows `rustc` and goes to stderr; under
`--format json` the objects fill the document's `diagnostics` array, under
`--format jsonl` each is its own event. Numbers are assigned once and never
reused.
[docs/DIAGNOSTICS.md](DIAGNOSTICS.md) has every code, when it fires and
what to do about it.

## `cambi affected --format json`

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
| `diagnostics` | \[object], optional | the actionable warnings this run raised, each `{code, level, message, location?, help?}`; omitted when empty. See [docs/DIAGNOSTICS.md](DIAGNOSTICS.md) |
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

## `cambi status --format json`

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

## `cambi doctor --format json`

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

## `cambi explain --format json`

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

## `cambi affected --dry-run --format json`

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

## Contract versions

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
