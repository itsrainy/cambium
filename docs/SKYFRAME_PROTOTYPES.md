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

## skyframe-oracle, measured

`skyframe-oracle` is the first of the four to land. It keeps one warm server
on the harness's output base and reads the answer out of the difference
between two `bazel dump --skyframe=deps` dumps: one taken at the step's
parent, one at its head.

Run it with:

```
cambium-bench skyframe --subject bazel --strategy skyframe-oracle --steps 10
cambium-bench skyframe compare docs/benchmarks/skyframe-*.json
```

The strategy is `crates/bench/src/skyframe/oracle.rs`; the dump parser it
leans on, with its unit tests over captured dump fragments, is
`crates/bench/src/skyframe/dump.rs`. Two loads are implemented and
`CAMBIUM_SKYFRAME_LOAD` picks between them. The default and the one measured
here is `loading`, which warms the server with `bazel query 'kind(rule,
//...)'` and takes the query's own output as the rule universe at head. The
other, `analysis`, warms it with `bazel build --nobuild --keep_going //...`
so the graph also holds `CONFIGURED_TARGET` nodes. `--keep_going` is not
optional on this subject: `//...` at these commits holds targets that fail
analysis, and without it the build aborts and the dump comes back empty.

Two answers are computed from every pair of dumps and both are recorded per
step. *package-diff* is the rules of every main-repository package whose
`PACKAGE` node is new, gone, or has a different dependency set. *rdeps* is the
transitive reverse closure of the head dump, seeded from the `FILE_STATE` keys
of the step's changed paths and from every node the diff named. The answer the
harness scores is the union.

### The table

Ten steps on the `bazel` subject (Bazel 9.2.0, seed 0), against the `cambi`
control's three steps at the same seed. The control's three steps are a subset
of these ten, so the rows are about the same commits.

| strategy | subject | steps | prepare p50 ms | answer p50 ms | answer p50 | recall | extra p50 | state p50 bytes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| cambi | bazel | 3 | 40250 | 1850 | 751 | 3/3 | 0 | 669636 |
| skyframe-oracle | bazel | 10 | 1342 | 1636 | 0 | 1/10 | 0 | 10947561 |

Per step, with the dump diff's own numbers:

| step | tags | changed nodes | dirty packages | answer | oracle | missed | extra | prepare ms | answer ms |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | adjacent | 1 | 0 | 3096 | 16 | 10 | 3090 | 14895 | 6742 |
| 2 | bzl, adjacent | 0 | 0 | 0 | 371 | 371 | 0 | 2763 | 1309 |
| 3 | adjacent | 0 | 0 | 0 | 1873 | 1873 | 0 | 1254 | 1572 |
| 4 | adjacent | 0 | 0 | 0 | 0 | 0 | 0 | 4450 | 1636 |
| 5 | adjacent | 0 | 0 | 0 | 1376 | 1376 | 0 | 1342 | 2558 |
| 6 | adjacent | 0 | 0 | 0 | 1977 | 1977 | 0 | 2384 | 1902 |
| 7 | adjacent | 0 | 0 | 0 | 939 | 939 | 0 | 948 | 1153 |
| 8 | adjacent | 0 | 0 | 0 | 998 | 998 | 0 | 996 | 1237 |
| 9 | adjacent | 0 | 0 | 0 | 751 | 751 | 0 | 1999 | 2836 |
| 10 | build_add_delete, adjacent | 6555 | 5 | 87 | 761 | 758 | 84 | 1125 | 1966 |

The result file is
[benchmarks/skyframe-oracle-bazel.json](benchmarks/skyframe-oracle-bazel.json).
It is large (about 750 KB) because nine of the ten steps record a `missed`
list, and those lists are the evidence for everything below.

### Findings

Recall is 1/10, and the one sound step is step 4, whose oracle is empty. The
answer is not a cheaper, more exact oracle than `bazel query rdeps`. It is not
an oracle at all at the loading tier. What the measurement establishes:

- **The loading-tier Skyframe graph does not see a source file change.** On
  eight of the ten steps the two dumps are byte-for-byte the same graph: zero
  changed nodes, zero dirty packages, an empty answer. A change to a `.java`
  or `.cc` file dirties no node in a loading-only graph, because the graph
  holds a `FILE_STATE` node for a `BUILD` file, for a `.bzl` file and for a
  directory, and none for an ordinary source. The package's `GLOBS` node
  matched the file by name and the name did not change, so nothing is
  invalidated. There is no widening of the answer that fixes this; the
  information is not in the graph.
- **The same is true of a `.bzl` change.** Step 2 is tagged `bzl` and still
  diffs to zero nodes, because the dump is taken after the *new* load: the
  server has already re-evaluated, and both dumps describe a consistent graph
  at their own revision. The dump is a snapshot of a settled graph, not a
  record of what was invalidated on the way there.
- **Loading-phase edges are not target edges.** Even where a `PACKAGE` node
  does change (step 10, five dirty packages from a `BUILD` file added and one
  deleted), the answer reaches only the rules *in* those packages: 87 of an
  oracle of 761. The rules in other packages that depend on them are not
  reachable, because `PACKAGE` nodes depend on files and `.bzl` loads and
  never on one another. The `rdeps` oracle walks the target graph, which at
  the loading tier does not exist.
- **The one wide answer came from an unrelated node.** Step 1's 3096 targets
  (against an oracle of 16) come from a single changed node,
  `BZL_LOAD:.../bazel_features+/private/util.bzl`, an external repository file
  that has nothing to do with the step. Seeding the reverse walk from it
  reaches most of the `PACKAGE` nodes in the graph, and so most of the 3911
  rules in the universe. Widening reached 3090 targets the oracle did not
  want and *still* missed 10 that it did.
- **`--skyframe=rdeps` returns direct reverse dependencies only, not a
  closure.** Asked for the reverse dependencies of a source file's
  configured target, Bazel 9.2 prints the two configured targets that name it
  directly and stops. It is not a substitute for `rdeps(//..., ...)`, so the
  transitive walk in `dump.rs` does the work instead. Recorded here so the
  next prototype does not re-try it.
- **The server does stay warm, and the dump is cheap.** All ten steps ran
  against server pid 50257: `bazel info server_pid` is identical across every
  `prepare` and `answer`, and the harness's output base is what keeps it that
  way. On a warm server the `dump` itself is 750 ms at the median and the load
  ahead of it 332 ms, against the control's 40 s cold index build. The cost
  that is real is state: 10.9 MB of dump at the median against the index's
  669 KB, and the dump grows with the graph rather than with the answer.
- **Analysis tier is where the target graph is, and it is not free.** A
  `CONFIGURED_TARGET` node carries the edges the oracle walks, and a source
  file is a configured target of its own, so `configured_target_label` and
  the reverse closure in `dump.rs` are written for it and unit-tested against
  a captured analysis-tier fragment. Measured by hand on this subject, one
  `build --nobuild --keep_going //...` is about 48 s and the resulting
  filtered dump for a *subset* (`//src/main/cpp/...`) is 76 MB across 1.17 M
  lines. Whether that pays for itself at `//...` scale is the `skyframe-index`
  and `skyframe-analysis` question, not this one.

The short answer to the branch's question: no. A dump diff over a loading-only
graph is cheap and warm, and it is blind to exactly the changes a CI job asks
about.
