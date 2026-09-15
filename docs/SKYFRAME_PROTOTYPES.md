# Skyframe prototypes: how they are measured

Four prototypes are planned, each computing "which targets could this change
affect" by leaning on Bazel's own Skyframe graph rather than on a static
index. Each lands on its own branch. The harness that measures them lands on
main (`crates/bench/src/skyframe/`), so every branch is measured on identical
steps with identical metrics and the results can be put in one table.

Vocabulary is [METHODOLOGY.md](METHODOLOGY.md)'s: a *subject* is a Bazel
repository the measurement points at, an *oracle* is an independent check, an
*index* is what cambium derives and carries forward.

## What the harness measures

A *strategy* is one approach, and it answers two calls per step:

- `prepare`, with the worktree at the step's parent commit: whatever one-time
  work the approach needs before it can answer incrementally (a cold index
  build, a warm server load, a dump).
- `answer`, with the worktree at the step's head commit: the incremental
  question, which is the one a CI job actually asks.

The harness owns everything else. It checks the worktree out at each end, times
both calls, and scores the answer against the `rdeps` oracle
(`crates/bench/src/oracles/rdeps.rs`) that every other workload in the crate is
scored against: Bazel's own reverse-dependency closure over the step's changed
source files, at head. Every strategy is scored the same way, including the
ones that build no index at all.

Per step the result file records: subject, parent, head, the walk's tags,
prepare wall, answer wall, answer size, oracle size, the oracle targets the
answer missed (any at all is unsoundness), how many answer targets the oracle
did not reach, the bytes of state `prepare` left behind, and whatever notes the
strategy wanted to keep. Results go to
`bench-results/skyframe/<strategy>-<subject>.json`, rewritten after every step
so a killed run keeps what it measured.

The steps are the walk's own: `walk::steps` tags every first-parent step in the
subject's window, and `walk::sample` draws `--steps N` (default 10) of them at
a fixed seed with at least one step per tag the window holds. Two strategies on
a subject at the same seed therefore see the same (parent, head) pairs, in the
same order, under the same Bazel. The chosen pairs are printed before the first
step. Steps are independent of one another, which is the one place this differs
from the walk: nothing is carried between them, so a strategy that cannot carry
state is measurable at all.

`cambi` is the built-in strategy and the control row: `cambi index build` at
the parent, `cambi affected parent..head` at the head. It measures nothing new,
and that is the point, since it is the number a prototype has to beat.

## Running it

```
cambium-bench skyframe --subject bazel --strategy cambi --steps 3
cambium-bench skyframe compare bench-results/skyframe/*.json
```

`compare` prints one markdown table: strategy, subject, steps, median prepare
wall, median answer wall, median answer size, recall (steps with nothing missed
over steps), median extra, median state bytes.

The control row on the `bazel` subject (Bazel 9.2.0, three steps, seed 0) is
committed as [benchmarks/skyframe-cambi-bazel.json](benchmarks/skyframe-cambi-bazel.json);
`bench-results/` itself is not tracked. It reads:

| strategy | subject | steps | prepare p50 ms | answer p50 ms | answer p50 | recall | extra p50 | state p50 bytes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| cambi | bazel | 3 | 40250 | 1850 | 751 | 3/3 | 0 | 669636 |

Two of the three steps reproduce the oracle exactly; the third (a step that
adds or deletes a BUILD file, 152 changed paths) answers 1342 targets over an
oracle of 761.

## The four planned strategies

- **skyframe-oracle**: diff two `bazel dump --skyframe=deps` dumps taken on a
  warm server either side of the change, as a ground truth for what Bazel
  itself considers dirty.
- **skyframe-index**: populate the index from a filtered `bazel dump
  --skyframe=deps` and shape it like that dump, instead of from `bazel query`.
- **skyframe-analysis**: an opt-in `CONFIGURED_TARGET` tier built from an
  analysis-phase dump, for the dependencies that only exist after analysis.
- **warm-query**: `bazel query rdeps` against a live server, the cheapest thing
  that could possibly work and the one every other strategy has to beat on wall
  time.

A fifth was considered and dropped: **skycache**, persisting Bazel's serialized
analysis frontier and diffing two commits' uploads. The spike is written up
under [skycache](#skycache-a-spike-and-why-there-is-no-fifth-strategy) below.
It is not implementable against a released Bazel.

## Adding one

1. Implement `Strategy` for a type in `crates/bench/src/skyframe/`. `name`
   returns the strategy's name, `prepare` does the work at the parent, `answer`
   does the work at head.
2. Add an arm to `open` in `crates/bench/src/skyframe/runner.rs` and the name to
   `strategies()`.
3. Run it, and run `cambi` on the same subject and seed if that result file is
   not already there. Compare the two.

What a strategy is handed is `StepContext`:

- `worktree`: the subject's checkout, absolute, and also the Bazel workspace.
  The harness owns which revision it is at (parent for `prepare`, head for
  `answer`); a strategy that needs the other end makes its own worktree under
  `scratch` rather than moving this one.
- `step`: parent sha, head sha, and the walk's tags for the step.
- `changed`: `git diff --name-status parent..head`. A rename appears as its
  destination plus a `D` for the source, so a strategy keying off deletions
  sees the same thing whether git detected the rename or not.
- `bazel`: the engine's Bazel wrapper, carrying the binary, the startup flags
  that pin the run to its own output base, and the bazelisk environment. Use
  it rather than spawning `bazel` directly, or the prototype measures a
  different server than the control does.
- `env`: the bazelisk environment on its own, for spawning something that is
  not Bazel.
- `scratch`: a per-step directory. It exists, nothing else writes to it, and it
  survives between the two calls of a step.

Two things worth knowing. The harness builds its own index at head for the
oracle (the oracle needs a package list to turn a changed path into a source
label), so a strategy neither has to produce one nor may assume its own is the
one being read. And the oracle's deleted half needs an index at the parent: it
runs when some strategy published one and is skipped, recorded, when none did.

Facts a dump-parsing strategy can rely on, on Bazel 8.7 and 9.2:
`bazel dump --skyframe=deps|rdeps|count` accepts `--skykey_filter=<regex>`.
Keys look like `PACKAGE:src/foo`, `FILE_STATE:[/abs/root]/[rel/path]`,
`BZL_LOAD:KeyForBuild{label=@@repo+//pkg:file.bzl, isBuildPrelude=false}`,
`GLOBS:<GlobsKey packageRoot = /abs, packageIdentifier = pkg, globRequests =
[...]>` and `DIRECTORY_LISTING:[/abs]/[pkg]`, with dependencies listed under
indented "Group N:" lines. The harness itself parses none of that.

## skycache: a spike, and why there is no fifth strategy

Bazel 9 carries "remote analysis caching" (Skycache): it selects a frontier of
Skyframe nodes around a set of active directories, serializes them, and
content-addresses each one by a fingerprint of the key plus its transitive
source inputs. If that upload were reachable from outside Bazel, two commits'
uploads could be diffed to name the changed analysis nodes, and a second
machine could hydrate instead of re-analyzing. This section records a spike
against that idea, run on the `bazel` subject (Bazel 9.2.0, subject checkout at
0fc10749b38, a scratch worktree, a private output base).

The verdict is that it is not reachable, for three independent reasons. No
`skycache` strategy is implemented.

### The flags that exist

Everything in `RemoteAnalysisCachingOptions` is
`OptionDocumentationCategory.UNDOCUMENTED`, so `bazel help build` lists none of
it except `--serialized_frontier_profile`. The flags are accepted anyway:

- `--experimental_remote_analysis_cache_mode` (default `off`). The enum is
  `UPLOAD, DUMP_UPLOAD_MANIFEST_ONLY, DOWNLOAD, ASYNC_BIDI, ASYNC_UPLOAD, BIDI,
  OFF`.
- `--serialized_frontier_profile=<path>`: "Dump a profile of serialized
  frontier bytes. Specifies the output path." The only documented one.
- `--remote_analysis_json_log=<path>`: "If set, a JSON file is written to this
  location that contains a detailed log of the behavior of remote analysis
  caching."
- `--experimental_active_directories`: "Active directories for Skyfocus and
  remote analysis caching. Specify as comma-separated workspace root-relative
  paths. This is a stateful flag." A leading `-` excludes a path.
- `--experimental_frontier_violation_check` (`strict`, `warn`,
  `disabled_for_testing`) and `--experimental_frontier_violation_verbose`.
- `--experimental_skycache_analysis_only`, `--experimental_skycache_minimize_memory`,
  `--experimental_analysis_cache_bail_on_missing_fingerprint`,
  `--experimental_analysis_cache_key_distinguisher_for_testing`.
- From `RemoteAnalysisCachingServicesOptions`:
  `--experimental_analysis_cache_service` (a locator for an
  AnalysisCacheService instance), `--experimental_remote_analysis_write_proxy`,
  `--experimental_remote_analysis_cache_storage` (`RAM`, `HDD`, `BOTH`),
  `--remote_analysis_debug_entries`, and a set of concurrency and batch knobs.
- `bazel dump` has `--skycache_cache` and `--skycache_fingerprint=<hex>`, and
  `--skyframe=` accepts `active_directories` and
  `active_directories_frontier_deps`.

### What a PROJECT.scl must contain

`ProjectFunction` parses a top-level `project` dict. For Skycache only one key
matters:

```
project = {
    "active_directories": {"default": ["src/main/java/com/google/devtools/build/lib/skyframe"]},
}
```

A non-empty `active_directories` must contain the `default` key
(`ProjectFunction.java:395`); a bare list of strings is accepted and wrapped as
`default`. The other keys (`configs`, `default_config`, `enforcement_policy`,
`always_allowed_configs`) belong to `--scl_config` and are not needed here.
The file is picked up from the innermost package of the requested targets, and
Bazel says so: "Reading project settings from
//src/main/java/com/google/devtools/build/lib/skyframe:PROJECT.scl."

PROJECT.scl is not actually required. `--experimental_active_directories`
alone sets the same matcher and takes precedence over the file. With neither,
Bazel reports "No active directories were found. Falling back on full
serialization."

### Blocker 1: the upload crashes, and the missing piece is not a flag

```
bazel build --nobuild //src/main/java/com/google/devtools/build/lib/skyframe:skyframe_cluster \
  --experimental_remote_analysis_cache_mode=upload \
  --serialized_frontier_profile=<path>
```

gets through frontier selection and then dies:

```
INFO: Found 12547 active or frontier keys in 21.07 ms
FATAL: bazel crashed due to an internal error. Printing stack trace:
java.lang.NullPointerException: missing versionGetter
	at ...analysis.FrontierSerializer.serializeAndUploadFrontier(FrontierSerializer.java:151)
	at ...buildtool.BuildTool.serializeValues(BuildTool.java:1831)
```

No profile file is written; the crash is before the serializer runs.

`FrontierSerializer` needs a `LongVersionGetter`, which maps a path to the
version at which it last changed (its MTSV), and which the upload uses to build
the invalidation metadata that makes a cache entry checkable on a later commit.
`BuildTool` passes `env.getVersionGetter()`. Searching the whole subject tree
for an implementation of `com.google.devtools.build.lib.versioning.LongVersionGetter`
finds none: the only thing that can supply one is
`LongVersionGetterTestInjection.injectVersionGetterForTesting`, and
`FrontierSerializer` reaches for it only when `isInTest()`. The real
implementation is a source-control snapshot service that the open-source tree
does not carry, so `--experimental_remote_analysis_cache_mode=upload` is
unreachable from a released Bazel by any combination of flags.

### Blocker 2: the store is a field in the server's heap

`SerializationModule.getAnalysisCachingServicesSupplier` returns
`InMemoryRemoteAnalysisCachingServicesSupplier`, whose entire store is

```java
private static final ListenableFuture<FingerprintValueStore>
    // TODO: b/358347099 - use a persistent store
    WRAPPED_STORE_INSTANCE = immediateFuture(new InMemoryFingerprintValueStore());
```

There is no directory-backed store and no gRPC store in the open-source binary.
`RemoteAnalysisCachingServicesOptions` (the class holding
`--experimental_analysis_cache_service` and the write proxy) is annotated
`@ScOnly` and the supplier interface is `@SkybridgeInterface`: these are the
seams where Google's internal service component is injected, and the
open-source side of the seam is the in-memory stub. So even a working upload
would leave nothing on disk for a second machine or a second run to read.
`--experimental_remote_analysis_cache_storage=HDD` names a storage type the
open-source store does not implement.

Download confirms the same seam from the other side:

```
WARNING: --experimental_remote_analysis_cache_mode=DOWNLOAD was requested but
--experimental_analysis_cache_service was not specified. Falling back on local
evaluation.
```

The build then analyzes locally and succeeds, so there is no download timing to
report: without a service there is no hydration to measure.

What the upload would write, if it ran, is more than values. `DumpCommand`'s
`--skycache_fingerprint=<hex>` path reads an entry from the store and calls
`GraphDumper.collectEdgesForSkyValue` plus
`GraphDumper.collectInvalidationGraph(entry.dependencyFingerprint(), ...)`, so
each entry carries a dependency fingerprint and the edges are recoverable. That
is the shape a diff would want. It is only reachable through the same in-memory
store, from inside the same server process that wrote it.

The fingerprinter itself is fine: `FingerprintValueService.NONPROD_FINGERPRINTER`
is `murmur3_128` over the serialized bytes, which is deterministic and carries
nothing machine-local. Content addressing is not the problem.

### Blocker 3: the frontier dumps are Skyfocus, and they are machine-local

`bazel dump --skyframe=active_directories` and
`--skyframe=active_directories_frontier_deps` do not read the Skycache
frontier. `DumpCommand` routes both to
`env.getSkyframeExecutor().getSkyfocusState()`, which stays empty unless
`--experimental_enable_skyfocus` (default false) was passed on a build. After a
Skycache upload attempt both dumps print only the header warning and no rows.

After a build with `--experimental_enable_skyfocus
--experimental_active_directories=...`, they do print. The shape is file-level
Skyframe keys, one per line, no edges:

```
FILE_STATE:[/abs/output_base]/[MODULE.bazel.lock]
FILE_STATE:[/abs/workspace]/[src/main/java/com/google/devtools/build/lib/skyframe]
DIRECTORY_LISTING_STATE:[<absolute root>]/[/Users/<user>/Library/Caches/bazel/_bazel_<user>/cache/repos/v1/contents/0c82fb.../cc/toolchains/actions]
PACKAGE_LOOKUP:@@remoteapis+//build/bazel/semver
FILE:[/abs/output_base/external/bazel_tools]/[tools/osx/xcode_configure.bzl]
```

These are not content-addressed and not machine-independent: most rows are
absolute paths into the output base or the user's repository contents cache,
including per-fetch UUID path segments that differ between machines and between
fetches on one machine. They also name files, not analysis nodes, so diffing
two of them names changed inputs rather than changed configured targets, which
is the question the harness asks.

### The one thing that is reachable

`--experimental_remote_analysis_cache_mode=dump_upload_manifest_only` returns
from `FrontierSerializer` before it touches the version getter or the store, so
it is the only Skycache mode that runs to completion on a released Bazel. It
writes the selection to stdout, one key per line, prefixed by its role:

```
ACTIVE: CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/...:foo, config=BuildConfigurationKey[f3f51bf1...]}
FRONTIER_CANDIDATE: ASPECT:SimpleAspectKey{baseConfiguredTargetKey=ConfiguredTargetKey{label=//src/main/protobuf:action_cache_proto, config=BuildConfigurationKey[f3f51bf1...]}, aspectDescriptor=@@protobuf+//bazel/private:java_proto_library.bzl%java_proto_aspect}
```

On `//src/main/java/com/google/devtools/build/lib/skyframe:skyframe_cluster`
with the PROJECT.scl above it emits 12547 lines, 1054 `ACTIVE:` and 11493
`FRONTIER_CANDIDATE:`, in about 12 seconds on a warm server.

This is machine-independent (labels and a configuration checksum, no absolute
paths) and it is analysis-level, which is more than either dump gives. It is
still not what the question needs. It is the membership of the frontier, not a
fingerprint per node: diffing two commits' manifests names the nodes that
entered or left the frontier, and says nothing about a node that stayed in the
frontier and changed value. That is the common case, and it is precisely the
case a changed-analysis-node report exists to catch. A strategy built on it
would answer with an empty or near-empty set on most steps and be recorded as
unsound rather than as a useful comparison, so it is not worth a row in the
table.

### What would unblock it

In rough order of how much would have to change:

1. A `LongVersionGetter` an open-source Bazel can construct. A git-backed one
   (commit ordinal per path) or even an mtime-backed one would satisfy the
   serializer's contract for a single-writer CI setting. Without this nothing
   else matters, because the upload never runs.
2. A persistent `FingerprintValueStore` in the open-source binary, which the
   source already names as wanted (`TODO: b/358347099 - use a persistent
   store`). A directory-backed store behind
   `--experimental_remote_analysis_cache_storage=HDD` plus a path flag would
   make uploads readable from outside the server and across machines, and
   `bazel dump --skycache_fingerprint` would then work against a store on disk.
3. Failing either, an external service implementing the Skycache gRPC surface
   that `--experimental_analysis_cache_service` and
   `--experimental_remote_analysis_write_proxy` point at. This is the seam
   Google's own deployment uses. It is the largest piece of work of the three
   and the proto is only half of it.

A smaller change that would make the manifest route viable on its own: have
`DUMP_UPLOAD_MANIFEST_ONLY` emit each selected key's `FrontierNodeVersion` or
its serialized-value fingerprint alongside the key. That is a fingerprint the
serializer already computes, it needs no store and no version getter, and it
would turn the manifest from a membership list into exactly the
content-addressed, diffable, machine-independent per-node record this question
asks for.

### Reproducing

```
git worktree add <scratch>/bazelwt HEAD          # in the bazel subject
cat > <scratch>/bazelwt/src/main/java/com/google/devtools/build/lib/skyframe/PROJECT.scl <<'EOF'
project = {
    "active_directories": {"default": ["src/main/java/com/google/devtools/build/lib/skyframe"]},
}
EOF
bazel --output_base=<scratch>/ob build --nobuild \
  //src/main/java/com/google/devtools/build/lib/skyframe:skyframe_cluster \
  --experimental_remote_analysis_cache_mode=dump_upload_manifest_only > manifest.txt
```

Swap the mode for `upload` to see the crash, or `download` to see the
fallback warning.
