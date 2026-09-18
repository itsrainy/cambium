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
  *Landed, in a narrower form than the prototype measured.* The rule layer
  still comes from `bazel query`; what the dump supplies is a second layer
  underneath it, which answers which packages a changed path moved
  (`crates/index/src/sky`, `docs/STORE.md` format 11). The prototype's own
  findings carried over: a source file's contents reach consumers through the
  rule layer and nothing else, and the singleton key kinds have to be dropped
  or every package sits one reverse hop from every other. Two of its choices
  did not: the ignore node is kept (it is the one singleton a changed path
  reaches, and keeping it is what makes a `.bazelignore` edit mean what it
  says), and keys of other repositories are dropped rather than stored.
- **skyframe-analysis**: an opt-in `CONFIGURED_TARGET` tier built from an
  analysis-phase dump, for the dependencies that only exist after analysis.
- **warm-query**: `bazel query rdeps` against a live server, the cheapest thing
  that could possibly work and the one every other strategy has to beat on wall
  time.

## What the four measured, and what became of them

Each prototype ran ten seeded steps on the `bazel` subject (Bazel 9.2.0, seed
0) against the control row above. The branches were closed once the loading
layer landed (PR #20); the numbers and findings are kept here because they are
what decided it. The full write-ups are on the closed PRs (#15 skycache, #16
skyframe-oracle, #17 skyframe-index, #18 warm-query, #19 skyframe-analysis).

| strategy | prepare p50 ms | answer p50 ms | recall | state p50 bytes | outcome |
| --- | ---: | ---: | ---: | ---: | --- |
| skyframe-oracle | | | 1/10 | | dropped |
| skyframe-index | 2335 | 61 | 10/10 | 945415 | landed as the loading layer |
| skyframe-analysis | 49908 | 446 | 1/10 | 4.57 MB | dropped |
| warm-query | 477 | 2066 | 10/10 | 0 | dropped (dev machines only) |
| skycache (spike) | | | | | infeasible |

- **skycache**: not reachable from a released Bazel. `upload` mode crashes on a
  missing `LongVersionGetter`, the fingerprint store is in-memory only, and the
  frontier dumps are Skyfocus state. `dump_upload_manifest_only` lists 12.5k
  analysis keys without fingerprints, so a node that stayed and changed is
  invisible.
- **skyframe-oracle**: a loading dump is a snapshot, not an invalidation log.
  It carries no `FILE_STATE` for ordinary sources, `PACKAGE` nodes never
  depend on each other, and `--skyframe=rdeps` is direct only, so diffing two
  dumps names almost nothing a source edit reaches.
- **skyframe-index**: sound on all ten steps, nine exact, 61 ms answers. Glob
  and sub-package rules fell out of `GLOBS` and `PACKAGE_LOOKUP` edges with no
  code. The rule layer still had to come from the query proto (a source file's
  contents reach consumers through rules and nothing else), `MODULE.bazel` has
  no edges, and re-querying bought nothing. This is what the loading layer is.
- **skyframe-analysis**: 285 MB per revision, recall 1/10. Implicit outputs,
  `target_compatible_with` targets and failed analyses have no
  `CONFIGURED_TARGET` node. Only toolchain edits have real edges; a flag or a
  `bazel_dep` bump moves no key, and 23 of 24 config checksums change on any
  flag. The one case it wins (a toolchain `BUILD` edit reaching 707 of 4175
  targets where the engine escalates) is not worth the tier.
- **warm-query**: sound on all ten steps and ties the index warm (1.4-2.1 s),
  but a cold answer costs 11 s (32 s worst) and re-loads the universe, which
  puts it on developer machines and out of CI. `--keep_going` is mandatory on
  this subject, and seeding `rbuildfiles` instead of the changed `.bzl` loses
  352 targets.

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
