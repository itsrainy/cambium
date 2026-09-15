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
## skyframe-analysis

The prototype for the `Escalated` list. cambium is exact at the unconfigured
target graph, and the cases it cannot answer incrementally are the ones that
live below it: toolchain resolution, platform selection, configuration. Each
of those ends the same way, with `cold_rebuild_reason` set and `//...` queried
once, and each escalates for the same reason: the reverse walk has no edge to
follow, because `bazel query` does not have one either.

Bazel does have those edges, in the analysis phase, and `bazel dump` will
print them. So the strategy is: run analysis once at the parent, dump the
Skyframe graph filtered to the analysis-phase kinds plus the loading-phase
kinds that reach them, reverse it, store it, and answer at head with a reverse
walk.

Two variants:

- `skyframe-analysis` runs `bazel build --nobuild --keep_going
  --noexpand_test_suites //...` and one `bazel dump --skyframe=deps` in
  `prepare`, and answers from the stored graph. Analysis is paid once.
- `skyframe-analysis-warm` does the same at the parent, then re-runs analysis
  at head on the same warm server, dumps again, and seeds the head graph with
  every key that appeared or vanished as well as with the changed paths. It
  measures what a warm-server analysis diff costs per step.

Code is `crates/bench/src/skyframe/analysis/`: `dump.rs` parses, `graph.rs`
stores and walks, `mod.rs` is the strategy. Nothing in `crates/index` or
`crates/engine` changed. `crates/bench/examples/analysis_walk.rs` is the same
parser and walk off the harness, which is what the escalation experiments
below use.

### The key shapes, on Bazel 9.2.0

`--skyframe=count` on the `bazel` subject after a `//...` analysis names 50
kinds. The ones this tier keeps, and what their keys look like:

| kind | key |
| --- | --- |
| `CONFIGURED_TARGET` | `ConfiguredTargetKey{label=//pkg:name, config=BuildConfigurationKey[<64 hex>]}` |
| `CONFIGURED_TARGET` (source file) | the same with `config=null` |
| `ASPECT` | `SimpleAspectKey{baseConfiguredTargetKey=ConfiguredTargetKey{…}, aspectDescriptor=@@repo//pkg:f.bzl%name}` |
| `TOOLCHAIN_RESOLUTION` | `ToolchainContextKey{configurationKey=BuildConfigurationKey[…], toolchainTypes=[ToolchainTypeRequirement[…]], execConstraintLabels=[], …}` |
| `SINGLE_TOOLCHAIN_RESOLUTION` | the same, one toolchain type |
| `REGISTERED_TOOLCHAINS` | `RegisteredToolchainsValue.Key{configurationKey=…}` |
| `PLATFORM` | `Key[label=//:default_host_platform, flagAliasMappings={…}]` |
| `BUILD_CONFIGURATION` | `BuildConfigurationKey[<64 hex>]` |

`PACKAGE`, `PACKAGE_LOOKUP`, `BZL_LOAD`, `FILE`, `FILE_STATE`,
`DIRECTORY_LISTING`, `DIRECTORY_LISTING_STATE` and `GLOBS` are as
SKYFRAME_PROTOTYPES.md already records them, with one addition: a
main-repository key carries no repository prefix at all (`PACKAGE:src/foo`,
`BZL_LOAD:KeyForBuild{label=//tools:defs.bzl, …}`), where an external one is
`@@repo+//pkg`.

### Two things the dump teaches

**Nothing depends on a `FILE_STATE`.** The chain is
`PACKAGE`/`BZL_LOAD`/`GLOBS` -> `FILE` -> `FILE_STATE`. A filter that keeps
`FILE_STATE` and drops `FILE`, which is the obvious filter to write, makes
every file state a reverse-walk dead end and the answer to a changed `BUILD`
file is empty. `FILE` is in the filter for that reason.

**No analysis node reads the bytes of an ordinary source file.** There is no
edge from a `.java` to anything: the package names the target, and the rule's
`srcs` name the file, so what carries a content edit is the *source file
target*, `CONFIGURED_TARGET:ConfiguredTargetKey{label=//pkg:Foo.java,
config=null}`, whose dependents are exactly the rules that consume it. The
seeder therefore has four routes and uses all of them: the source file target
(content edits), the `FILE` and `FILE_STATE` nodes (`BUILD` and `.bzl` edits),
the `BZL_LOAD` key for a `.bzl`, and the containing directory's listing (adds
and deletes).

### The table

Ten steps on the `bazel` subject (Bazel 9.2.0, seed 0), in a subjects
directory nothing else on the machine shared, so no other run could move the
checkout or the output base underneath it. The `cambi` row is the same ten
steps, measured in the same run; the three-step control that was already
committed is kept for continuity.

```
cambium-bench skyframe compare docs/benchmarks/skyframe-*.json
```

| strategy | subject | steps | prepare p50 ms | answer p50 ms | answer p50 | recall | extra p50 | state p50 bytes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| cambi | bazel | 3 | 40250 | 1850 | 751 | 3/3 | 0 | 669636 |
| cambi | bazel | 10 | 39893 | 607 | 998 | 10/10 | 0 | 668180 |
| skyframe-analysis | bazel | 10 | 49908 | 446 | 1912 | 1/10 | 44 | 4571077 |
| skyframe-analysis-warm | bazel | 10 | 35215 | 8693 | 1912 | 1/10 | 44 | 4567287 |

Results are
[benchmarks/skyframe-analysis-bazel.json](benchmarks/skyframe-analysis-bazel.json),
[benchmarks/skyframe-analysis-warm-bazel.json](benchmarks/skyframe-analysis-warm-bazel.json)
and [benchmarks/skyframe-cambi-bazel-10.json](benchmarks/skyframe-cambi-bazel-10.json).

### Cost

Per step, at the parent, median over the ten:

| | |
| --- | ---: |
| `build --nobuild --keep_going --noexpand_test_suites //...`, cold server | 35.4 s |
| the same, warm server, one commit later | 1.3 s to 15.0 s |
| `dump --skyframe=deps` with the filter | 4.8 s |
| the dump on disk | 285 MB |
| parsing it into the reversed graph | 4.9 s |
| the stored graph | 4.57 MB |
| the reverse walk itself | 0.45 s |

The dump is the whole cost: 285 MB of text per revision, five seconds to
print and five to parse, against a `cambi` index of 0.67 MB. Analysis itself
is cheap once a server is warm (the warm variant re-analyses at head in one
to fifteen seconds), and the reverse walk is the fastest answer in the table,
faster than `cambi affected`. What the warm variant buys with its extra
analysis is nothing: its answers are the same size as the cold variant's on
nine of ten steps, so the eight extra seconds of wall time bought no
precision at all. `--skyframe=deps` prints keys, not values, and on eight of
the ten steps not one key appeared or vanished between the two commits.

### Configuration checksums

`CONFIGURED_TARGET` keys reduce to `(label, config checksum)`. On the `bazel`
subject there are **24 distinct checksums** and 10183 configured labels, of
which **8178 are analysed under more than one configuration**. The tier drops
the checksum when it answers, which costs nothing here: two configurations of
one target are one label.

Stability has two very different answers.

- **Across commits**: total. The warm variant records `keys_appeared` and
  `keys_vanished` per step, and on eight of the ten steps both are zero, with
  the same 24 checksums either side. The two that move are the steps that add
  or delete targets, and even there it is 94 and 211 keys out of 186 000.
- **Across a flag**: none. Re-analysing with `--define=cambium_probe=1`
  replaced 23 of the 24 checksums (one survives), reconfigured all 100 208
  targets and cost 26.6 s. So a stored graph keyed on `(label, config)` is
  invalidated wholesale by any flag that moves the configuration.

There is no normalisation that fixes this, because the checksum is the
configuration: the only part of the key stable under a flag change is the
label, which is what the answer already reduces to. A tier that wanted to
survive flag changes would have to key on the label and treat the
configuration as an attribute, and then it could no longer say which
configuration was affected, which was the point of having the tier.

One flag went the other way and is worth recording: adding
`--incompatible_disallow_empty_glob` changed **no** checksum at all, and the
warm server reconfigured 736 targets in 2.5 s. The flag is loading-phase
Starlark semantics, not configuration. The engine escalates it (it is not on
the `crates/engine/src/bazelrc.rs` allowlist, and no `--incompatible_*` flag
is), so the correct answer here is far narrower than the global one. But the
dump cannot find it either: every key is identical either side, and a
key-level diff sees nothing. Only values changed, and `--skyframe=deps` does
not print values.

### The three escalation experiments

Three synthetic steps in the subject worktree at the pin, each an edit whose
kind appears in README.md's `Escalated` list, each asked of the tier with
`crates/bench/examples/analysis_walk.rs` against a graph built on the clean
tree. The universe is 4103 rule targets (`bazel query 'kind(rule, //...)'`);
analysis named 4175 main-repository labels.

| experiment | edit | analysis answer | what the engine does |
| --- | --- | ---: | --- |
| E1 | a line appended to `tools/jdk/BUILD` | 707 of 4175 (17%) | escalates when `tools/toolchains/**` is an invalidator; on this subject `invalidators = []`, so it answers through the loading graph |
| E2 | `common --incompatible_disallow_empty_glob` appended to `.bazelrc` | 4123 of 4175 (99%) | escalates: no `--incompatible_*` flag is on the allowlist |
| E3 | `bazel_skylib` bumped from 1.9.0 down to 1.8.2 in `MODULE.bazel` | 4123 of 4175 (99%) | modelled: the `MODULE.bazel` diff names the repositories that moved |

E1 is the one the tier wins. A toolchain `BUILD` edit has real edges here:
`FILE` -> `PACKAGE` -> the toolchain's `CONFIGURED_TARGET` -> the
`SINGLE_TOOLCHAIN_RESOLUTION` and `TOOLCHAIN_RESOLUTION` nodes that named it
-> the configured targets that resolved through them. 707 targets instead of
a `//...` rebuild, from a graph that already exists.

E2 and E3 are the ones it loses, and the reason is the same for both and is
not a precision problem. **A flag has no node in the graph, and neither does a
module version.** The 4123 both experiments answer is not the tier finding a
dependency; it is `.bazelrc` and `MODULE.bazel` being ordinary source files in
`//:bazel-srcs` and `//:dist-srcs`, whose reverse closure is most of the
repository. Delete those filegroups and the answer would be zero, which is
unsound, rather than 4123, which is right by accident. The tier cannot answer
a question about the invocation, because the invocation is not in the graph
it dumped; it is the thing that produced the graph.

### Findings

1. **The analysis graph has the toolchain and platform edges, and they work.**
   `TOOLCHAIN_RESOLUTION`, `SINGLE_TOOLCHAIN_RESOLUTION`,
   `REGISTERED_TOOLCHAINS` and `PLATFORM` are all reachable by a reverse walk
   from a changed `BUILD` file, and a toolchain edit answers 707 targets where
   the engine escalates to 4103. This is the one case on the `Escalated` list
   that an analysis tier genuinely answers.
2. **Flags and module versions are not in the graph at all**, so the two
   remaining escalation families are untouched. A dump records the graph a set
   of flags produced; it cannot record what a different set would produce.
3. **A key-level dump diff is blind to value changes.** `--skyframe=deps`
   prints keys and their dependency keys. `--incompatible_disallow_empty_glob`
   reconfigured 736 targets without changing one key. Any strategy that diffs
   two dumps by key, this one's warm variant included, cannot see that.
4. **Recall is 1/10, and the misses are structural, not a walk bug.** Over the
   ten steps the tier missed 36 distinct labels, in three classes, none of
   which a better walk would reach:
   - **Implicit output files** (`X.jar`, `X_deploy.jar`, `X-src.jar`: 20 of
     the 36). The query graph has a target per implicit output; the configured
     graph folds them into the rule, so there is no node to reach.
   - **Platform-incompatible targets.** `//third_party/def_parser:def_parser_test`
     is `target_compatible_with = ["@platforms//os:windows"]`, and
     `build //...` on macOS skips it, so it is never configured. The
     loading-phase graph is configuration-independent and has it. A tier built
     from analysis is therefore **specific to the machine that built it**,
     which is a much stronger constraint than an index has.
   - **Targets whose analysis failed.** On this subject a GraalVM distribution
     will not fetch for macOS, so `//scripts/packages/fedora:bazel` and its
     neighbours never get a node. `--keep_going` keeps the run alive and
     silently narrows the graph.
5. **`--noexpand_test_suites` is mandatory.** Without it Bazel expands a
   `test_suite` into its members and the suite has no configured target, which
   cost six extra missed labels a step on the first run.
6. **The state is seven times the size of a `cambi` index** (4.57 MB against
   0.67 MB) and comes from a 285 MB dump, for an answer that is nearly twice
   as wide (1912 against 998 at the median) and unsound. The walk itself is
   the fastest thing in the table at 0.45 s, which is the one number worth
   carrying forward.
7. **The dump has to be streamed.** 285 MB of text per revision, and the
   parser interns keys as it reads because almost all of that text is repeated
   key spellings: 186 605 distinct nodes and 1 450 872 edges out of it.
8. **A server does not survive repeated full dumps.** Analysing `//...` and
   dumping 285 MB at several revisions in one server kills it partway through
   a later dump ("Server terminated abruptly"). `prepare` shuts the server
   down first, which also makes its number an honest cold cost.

The conclusion for the tier as designed: it answers exactly one of the three
escalation families, at seven times the state and with a soundness gap that
is a property of the analysis phase rather than of the implementation. If an
analysis tier is worth having, the version worth building is a narrow one:
`TOOLCHAIN_RESOLUTION` and `PLATFORM` edges only, folded into the existing
index as extra edges out of the toolchain and platform packages, so that a
toolchain edit stops being a global. Nothing else here pays for itself.
