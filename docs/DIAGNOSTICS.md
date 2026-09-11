# `cambi` diagnostics

Every warning and error `cambi` reports carries a stable code. The number is
assigned once and never reused: a retired code keeps its row and is marked
retired rather than being handed to something else, because callers grep for
these strings in stderr and in JSON.

Human form follows `rustc` — `warning[CAMBI-W004]: <message>`, a `  --> path:line`
line when there is a location, and a `  = help:` line when there is something
to do. Under `--format json` the same diagnostic is an object
(`{"code", "level", "message", "location", "help"}`); on `cambi affected` the
objects are in the document's `diagnostics` array, and under `--format jsonl`
each is its own `{"event": "diagnostic", ...}` line. Warnings never change the
exit code.

The rule for what earns a code: anything a person or an agent might need to act
on. A log line (`--log info`) is not a diagnostic.

## Warnings

| code | when it fires | what to do |
|---|---|---|
| `CAMBI-W001` | a deprecated spelling was used and still worked: `cambi baseline <verb>` for `cambi index <verb>`, the old `cambi index <query>` for `cambi inspect <query>`, `cambi index inspect` for `cambi index show` / `cambi inspect target` / `cambi inspect package`, bare `cambi config` for `cambi config show`, `--output` for `--format`, or `text` for `human` | type what the message names; the old spelling still works for this release and is removed in the next. One line per invocation, on stderr, and `--quiet` silences it |
| `CAMBI-W002` | `cambium.toml` sets a key this `cambi` no longer honours | delete the key; the message names the one that replaced it, if any |
| `CAMBI-W003` | a stored index was skipped because its `config_hash` is not this workspace's | expected right after a `cambium.toml` edit; mint a new lineage with `cambi index build --publish`. If it repeats, the store holds indexes from a different configuration |
| `CAMBI-W004` | a `.bazelrc` edit changed something that could move the graph, so the run answered `//...` | if the flag cannot affect loading or analysis, add it to `[bazelrc].allowlist_extra` |
| `CAMBI-W005` | some dirty packages did not load under `--keep_going` and were tombstoned, as a cold build would drop them | usually a package that genuinely cannot load on this machine. If it should load, fix the load error; consumers are reported as affected either way |
| `CAMBI-W006` | `bazel query //...` finished with errors, so the index is partial | fix the load errors, or accept the partial index deliberately with `--allow-partial` / `baseline.publish_partial` |
| `CAMBI-W007` | a store fetch command failed with something on stderr and the artifact was read as absent | check the credentials and the bucket in `[baseline].store`: a miss read this way turns into a cold build with no other explanation |
| `CAMBI-W008` | the base index carries no static `.bzl` load closure, so a changed `.bzl` could not be resolved and the run went global | rebuild the lineage with `cambi index build` from a checkout the row source can read |
| `CAMBI-W009` | a changed `.bzl` is loaded outside any package (from `WORKSPACE` or the module closure), so the run went global | expected for a workspace-level `.bzl`; if the file is not really in the workspace closure, rebuild the index so the closure is current |
| `CAMBI-W010` | a changed `.bzl` is loaded at head but has no node in the stored load closure | the stored closure is incomplete; the witness followed its loaders, so the answer is safe. Rebuild the lineage to make it cheap again |
| `CAMBI-W011` | the base index this run computed could not be published | the answer is unaffected; the next run recomputes the base. Check that the store is writable |
| `CAMBI-W012` | the dirty set exceeded `limits.max_dirty_packages`, so the run re-indexed the whole repository at head | still exact, just slower. Raise `limits.max_dirty_packages`, or set it to 0 to never cold-rebuild |
| `CAMBI-W013` | a changed path matched an invalidator, so everything is affected | if the path cannot move the graph, remove it from `[invalidators].paths` / `globs`, or add it to `[ignore].globs` |
| `CAMBI-W014` | the external-repository stage could not account for a change and escalated to global | the message names the file; `[externals].enabled = false` turns the stage off if its precision is not wanted |
| `CAMBI-W015` | a Starlark build setting a `.bazelrc` edit changed could not be resolved against the index, so the run went global | the message says why (an external repository, a `.bzl` that reads the setting, an unparsed label). Move the setting into the main repository, or add the flag to `[bazelrc].allowlist_extra` if it cannot affect analysis |
| `CAMBI-W016` | `MODULE.bazel` and a `WORKSPACE` file are both present and there is no `.bazelversion`, so which one Bazel honours depends on whichever Bazel runs — and the two describe different graphs | pin the version in `.bazelversion`, or delete the file the build does not use. `cambi status` names the inert one once the pin decides it |
| `CAMBI-W017` | the Bazel that answered `--version` is older than the oldest major cambium is tested against | upgrade Bazel, or pin a tested version in `.bazelversion`. An untested version is not refused, only unverified |
| `CAMBI-W018` | a stored index is in an older on-disk format than this `cambi` reads, so it cannot be spliced onto | mint a fresh lineage with `cambi index build --publish`; the old blobs age out under `[baseline].retention_days`, or go now with `cambi index gc` |
| `CAMBI-W019` | the store holds a lineage whose `config_hash` is not this workspace's, or that an engine with a different rule-hash version or index content revision wrote | expected right after a `cambium.toml` edit or a `cambi` upgrade; the stale lineage is skipped rather than used. `cambi index gc` removes it now |
| `CAMBI-W020` | `cambi init` found a `.gitignore` that does not mention `cambium.local.toml` | add a `cambium.local.toml` line to it: the file holds this machine's paths and binaries, and committing one makes them everybody's. `init` says so and does not edit `.gitignore`, which belongs to the repository |

## Errors

An error ends the run; the exit code is in the README's table.

| code | when it fires | what to do |
|---|---|---|
| `CAMBI-E001` | an internal failure with no more specific code: io, protobuf, index or classification | check the message; a bug report should carry `--log debug` output |
| `CAMBI-E002` | `cambium.toml` could not be read or does not parse | fix the file the message names, or point `--config` elsewhere |
| `CAMBI-E003` | git failed | the message carries git's own error; check that the revisions in the range exist in this checkout |
| `CAMBI-E004` | Bazel failed, or no Bazel workspace was found | run the command from inside the workspace, or pass `-C`; check `--bazel-binary` |
| `CAMBI-E005` | a `bazel query` failed | the message carries Bazel's stderr; the query has to succeed before `cambi` can answer |
| `CAMBI-E006` | no baseline index is available for the base and `--no-full-build` was given | run `cambi index build --publish`, or drop `--no-full-build` and let the run build one |
| `CAMBI-E007` | the index on hand was built with a different `cambium.toml`, so splicing onto it could disagree with a full build | mint a new lineage: `cambi index build --publish` |
| `CAMBI-E008` | no baseline store is configured | set `[baseline].store` in `cambium.toml` |
| `CAMBI-E009` | refusing to publish an index Bazel could not fully load | fix the load errors, or publish it deliberately with `--allow-partial` / `baseline.publish_partial = true` |
| `CAMBI-E010` | the store backend failed | check that the store in `[baseline].store` is reachable and the credentials are current; exit code 5, which a CI job can retry |
| `CAMBI-E011` | an io error against the store | check the path and the permissions on `[baseline].store`; exit code 5 |
| `CAMBI-E012` | the store backend does not support the operation (a listing, a gc) | use a backend that does, or skip the operation |
| `CAMBI-E013` | the command line does not make sense — a format a verb has no rendering for, for instance | the message names the flag; `cambi <verb> --help` lists what it takes |
| `CAMBI-E014` | `cambium.local.toml` sets a key that is part of the config hash, so it decides what the graph is rather than what this machine is | move the key the message names into `cambium.toml`, where it is checked in and hashed. Only `bazel.binary`, `bazel.startup_flags`, `baseline.cache_dir`, `baseline.compress_indexes` and `baseline.publish_partial` may appear in the local file |
| `CAMBI-E015` | the Bazel binary could not be found on `$PATH`, or ran and printed no version | install Bazel or bazelisk, or name the binary with `--bazel-binary`, `[bazel].binary`, or `bazel.binary` in `cambium.local.toml` |
| `CAMBI-E016` | `cambi doctor` published a probe object to the store and could not read it back | a run publishes indexes through the same calls, so this is the check that matters for `--publish`: confirm the store in `[baseline].store` is writable with the credentials in this environment |
