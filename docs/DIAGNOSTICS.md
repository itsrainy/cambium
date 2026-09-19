# `cambi` diagnostics

Every warning and error carries a stable code, assigned once and never reused; retired codes stay, marked retired. Human form follows `rustc`: `warning[CAMBI-W004]: <message>`, `  --> path:line` if located, `  = help:` if fixable. Under `--format json` it's `{"code", "level", "message", "location", "help"}` in the `diagnostics` array; under `jsonl`, one `{"event": "diagnostic", ...}` line each. Warnings never change the exit code; a log line (`--log info`) is never a diagnostic.

## Warnings

| code | when it fires | what to do |
|---|---|---|
| `CAMBI-W001` | a deprecated spelling still worked: `baseline` for `index`, old `index <query>` for `inspect <query>`, `index inspect` for `index show`/`inspect target`/`inspect package`, bare `config` for `config show`, `--output` for `--format`, `text` for `human` | Switch spelling; removed next release. One line per invocation on stderr; `--quiet` silences it |
| `CAMBI-W002` | `cambium.toml` sets a key `cambi` no longer honours | Delete it; message names its replacement, if any |
| `CAMBI-W003` | a stored index was skipped: `config_hash` doesn't match this workspace | Expected after a config edit; `cambi index build --publish` mints a lineage. Repeats mean the store holds another configuration's indexes |
| `CAMBI-W004` | a `.bazelrc` edit could move the graph; run answered `//...` | Allowlist it in `[bazelrc].allowlist_extra` if it can't affect loading or analysis |
| `CAMBI-W005` | dirty packages failed to load under `--keep_going` and were tombstoned, as a cold build would | Usually broken on this machine; fix if it should load. Consumers count as affected regardless |
| `CAMBI-W006` | `bazel query //...` finished with errors: index is partial | Fix load errors, or accept partiality (`--allow-partial` / `baseline.publish_partial`) |
| `CAMBI-W007` | a store fetch failed with stderr output and was read as absent | Check credentials and the bucket in `[baseline].store` |
| `CAMBI-W008` | the base index carries no Skyframe loading layer, so which packages the change moved can't be answered; run went global | `cambi index build` against a Bazel that can run `dump --skyframe=deps` |
| `CAMBI-W009` | a changed `.bzl` is one the module system loads (`WORKSPACE` or the module closure) and the index can't name the repositories it defines; run went global | Expected for a workspace-level `.bzl`; else rebuild the index |
| `CAMBI-W010` | retired with the static `.bzl` load closure's witness. The loading layer's `BZL_LOAD` edges are Bazel's own, so there's no stored closure left to be incomplete | Nothing; the code is not reused |
| `CAMBI-W011` | the base index this run computed couldn't be published | Answer unaffected; check store's writable |
| `CAMBI-W012` | dirty set exceeded `limits.max_dirty_packages`; run re-indexed at head | Still exact, just slower. Raise the limit, or 0 to never cold-rebuild |
| `CAMBI-W013` | a changed path matched an invalidator, so everything is affected | If it can't move the graph, drop it from `[invalidators]`, or add to `[ignore].globs` |
| `CAMBI-W014` | the external-repository stage couldn't account for a change, escalating to global | `[externals].enabled = false` disables the stage if precision isn't wanted |
| `CAMBI-W015` | a Starlark build setting a `.bazelrc` edit changed couldn't resolve against the index; run went global | Message says why. Move the setting in-repo, or allowlist it |
| `CAMBI-W016` | `MODULE.bazel` and `WORKSPACE` both present with no `.bazelversion`: which Bazel honours depends on whichever runs, describing different graphs | Pin the version or delete the unused file; `cambi status` names the inert one once pinned |
| `CAMBI-W017` | the Bazel that answered `--version` is older than the oldest major cambium is tested against | Upgrade or pin a tested version; untested is unverified, not refused |
| `CAMBI-W018` | a stored index predates this `cambi`'s on-disk format, so it can't be spliced onto | Mint a fresh lineage (`cambi index build --publish`); old blobs age out via `[baseline].retention_days`, or `gc` now |
| `CAMBI-W019` | the store holds a lineage with a mismatched `config_hash`, or from an engine with a different rule-hash or content revision | Expected after a config edit or upgrade; skipped. `cambi index gc` removes it |
| `CAMBI-W020` | `cambi init` found a `.gitignore` not mentioning `cambium.local.toml` | Add the line: it holds machine-local paths and binaries, and committing them makes them everybody's |
| `CAMBI-W021` | `cambi inspect verify` couldn't run `rdeps` for one sampled target, skipped it; `checked` counts only compared targets | Run `bazel query 'rdeps(//..., <label>)'` by hand to see why; other samples' verdict stands |
| `CAMBI-W022` | `bazel mod dump_repo_mapping` stdout wasn't a JSON object, so identity mapping stood in for the real one; a renamed `bazel_dep` can then name the wrong repository | Run `bazel mod dump_repo_mapping ""`, check stdout is one JSON object (a wrapper script printing to stdout is the usual cause); rebuild once fixed |
| `CAMBI-W024` | a module Bazel autoloads native rules from (`rules_python`, `rules_java`, `rules_shell`, `protobuf`, `rules_android`) moved in `MODULE.bazel`, its lock or `WORKSPACE`, and the Bazel here autoloads: 8 at its default, or 7.4 with `--incompatible_autoload_externally` set in `.bazelrc` (an unknown version counts as 8; Bazel 9 disabled autoloads and reads the flag as a no-op); run answered `//...` | Expected: a bare `py_binary` or `proto_library` is that module's rule and no BUILD file loads it, so no edge names its consumers. If the Bazel here is not 8 and nothing says so, pin it in `.bazelversion`; `--incompatible_autoload_externally=` (empty) on an unconditional line of `.bazelrc` turns autoload off, after which a toolchain module still answers `//...` under `CAMBI-W014` |

## Errors

An error ends the run; exit codes are in the README's table.

| code | when it fires | what to do |
|---|---|---|
| `CAMBI-E001` | internal failure with no specific code: io, protobuf, index or classification | Check the message; bug reports should carry `--log debug` output |
| `CAMBI-E002` | `cambium.toml` couldn't be read or doesn't parse | Fix the file, or point `--config` elsewhere |
| `CAMBI-E003` | git failed | Carries git's error; check the revisions exist here |
| `CAMBI-E004` | Bazel failed, or no workspace was found | Run inside the workspace, or pass `-C`; check `--bazel-binary` |
| `CAMBI-E005` | a `bazel query` failed | Carries Bazel's stderr; the query must succeed first |
| `CAMBI-E006` | no baseline index is available for the base and `--no-full-build` was given | `cambi index build --publish`, or drop `--no-full-build` |
| `CAMBI-E007` | the index on hand used a different `cambium.toml`; splicing could disagree with a full build | Mint a new lineage: `cambi index build --publish` |
| `CAMBI-E008` | no baseline store is configured | Set `[baseline].store` in `cambium.toml` |
| `CAMBI-E009` | refusing to publish an index Bazel couldn't fully load | Fix the load errors, or publish with `--allow-partial` / `baseline.publish_partial = true` |
| `CAMBI-E010` | the store backend failed | Check `[baseline].store` is reachable, credentials current; exit 5, retryable |
| `CAMBI-E011` | an io error against the store | Check path/permissions on `[baseline].store`; exit 5 |
| `CAMBI-E012` | the store backend doesn't support the operation (listing, gc) | Use a backend that does, or skip it |
| `CAMBI-E013` | the command line doesn't make sense, e.g. a format a verb can't render | Message names the flag; `cambi <verb> --help` lists what it takes |
| `CAMBI-E014` | `cambium.local.toml` sets a key that's part of the config hash, deciding the graph, not the machine | Move it into `cambium.toml`; only `bazel.binary`, `bazel.startup_flags`, `baseline.cache_dir`, `baseline.compress_indexes`, `baseline.publish_partial` may appear locally |
| `CAMBI-E015` | the Bazel binary wasn't found on `$PATH`, or printed no version | Install Bazel/bazelisk, or name it via `--bazel-binary` / `[bazel].binary` / `bazel.binary` |
| `CAMBI-E016` | `cambi doctor` published a probe object to the store and couldn't read it back | Same calls `--publish` uses; confirm `[baseline].store` writable |
| `CAMBI-E017` | a `cambi inspect` query named a label, package or node kind the index lacks | `cambi inspect targets <package>` lists a package's targets; `cambi inspect owner <path>` says which package owns a path |
| `CAMBI-E018` | `cambi index update` found the range invalidates everything (reason in the message) and `--no-rebuild-on-global` blocked a fresh lineage | Drop the flag, or pick a splice-able range |
