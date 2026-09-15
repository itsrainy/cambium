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

## skyframe-index

`crates/bench/src/skyframe/skyindex/`. An index whose node kinds are
Skyframe's own loading-phase keys, populated from a filtered `bazel dump
--skyframe=deps` rather than from `bazel query //...`, and answered by a
generic dirty-then-reverse-walk over those kinds. There is no classification
of the change anywhere on the answer path: the changed paths become Skyframe
keys, the walk follows stored reverse edges, and every rule it reaches is the
answer.

```
cambium-bench skyframe --subject bazel --strategy skyframe-index --steps 10
cambium-bench skyframe --subject bazel --strategy skyframe-index-requery --steps 10
```

### What prepare does, at the parent

1. `bazel query //...:*` as a proto stream. It is the load that fills
   Skyframe, and it is also where the targets come from (first finding below).
2. `bazel dump --skyframe=deps
   --skykey_filter='^(PACKAGE|GLOBS|FILE|DIRECTORY_LISTING|BZL_LOAD)'`, parsed
   into nodes plus a reverse-dependency CSR and written to
   `scratch/skyindex.bin` as zstd-compressed JSON.

The filter is a prefix set rather than an exact list of kinds. `FILE` has to
be in it because a `PACKAGE` reaches its `BUILD` file through a `FILE` node
and not straight through `FILE_STATE`, and the chain breaks in half without
it; `PACKAGE` also covers `PACKAGE_LOOKUP`, which is what carries sub-package
carving.

Eight kinds are kept: `FILE_STATE`, `FILE`, `DIRECTORY_LISTING_STATE`,
`DIRECTORY_LISTING`, `GLOBS`, `PACKAGE_LOOKUP`, `PACKAGE`, `BZL_LOAD`.
Everything else the dump offers is dropped, and dropping it is not a
convenience. `PRECOMPUTED:*`, `IGNORED_SUBDIRECTORIES:@@`,
`STARLARK_BUILTINS` and `REPOSITORY_MAPPING` are singletons that every package
in the repository depends on, so a walk that kept them would put every package
one reverse hop from every other and answer `//...` to everything.

Absolute paths are stripped: a rooted key (`FILE_STATE:[/abs/root]/[rel]`)
becomes a root id plus the relative path, root 0 being the workspace and
stored as the empty string, so the file names no path of the machine that
built it. Roots that are not the workspace (the output base, the repository
cache) are kept verbatim, since nothing in a diff can name them and they are
never a seed.

### What answer does, at head

The changed paths become the dirty seed with no case analysis: a
`FILE_STATE:<path>` for every path, and for a path that was added, deleted or
renamed, a `DIRECTORY_LISTING_STATE` for the directory holding it and for
every ancestor directory, because an add may have created the whole chain and
a delete may have emptied it. The walk is a breadth-first pass over reverse
edges. Every `PACKAGE` or `PACKAGE_LOOKUP` node reached names a dirty package;
every rule filed under a dirty package, every rule naming a changed file, and
everything that depends on one of those transitively, is the answer.

`PACKAGE_LOOKUP` counts alongside `PACKAGE` because a package created between
the two revisions has no `PACKAGE` node in an index built at the parent, while
the lookup that found nothing there does have one and carries the same name.

`skyframe-index-requery` differs only after the dirty packages are known: it
re-queries them at head (`bazel query 'set(//a:* ...)'`) and replaces their
rules before the rule-level closure, the way the engine does.

### The rows

`cambium-bench skyframe compare docs/benchmarks/skyframe-*.json`, on the
`bazel` subject (Bazel 9.2.0, seed 0):

| strategy | subject | steps | prepare p50 ms | answer p50 ms | answer p50 | recall | extra p50 | state p50 bytes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| cambi | bazel | 3 | 40250 | 1850 | 751 | 3/3 | 0 | 669636 |
| skyframe-index | bazel | 10 | 2335 | 61 | 939 | 10/10 | 0 | 945415 |
| skyframe-index-requery | bazel | 10 | 3592 | 74 | 939 | 10/10 | 0 | 949193 |

Both variants are sound on all ten steps, and nine of the ten reproduce the
oracle exactly. The tenth is the `build_add_delete` step, which answers 3285
(plain) or 3279 (requery) targets over an oracle of 761; the control answers
1342 over 761 on that same step.

The prepare column is not a like-for-like comparison with the control row and
should not be read as one. The control's three steps were measured on a
shared subjects directory that other work was warming and cooling; these ten
ran on a private `--subjects-dir` whose Bazel server stayed up across steps,
so the first step paid 22 s for the universe query and the rest paid 0.5 to
3 s. The dump itself is cheap either way: 0.5 to 1.7 s for a 5.8 to 20.4 MB
text, and 0.12 to 0.78 s to parse that text into 17k to 50k nodes and 25k to
68k edges.

### Findings

- **The dump has no `TARGET` keys on 9.2.** A `bazel query //...` leaves
  `PACKAGE`, `PACKAGE_LOOKUP`, `GLOBS`, `FILE`, `FILE_STATE`,
  `DIRECTORY_LISTING`, `DIRECTORY_LISTING_STATE` and `BZL_LOAD`, and nothing
  target-shaped. Rule labels, the package each belongs to, and the labels each
  depends on all have to come from somewhere else. This prototype takes them
  from the `//...:*` proto stream, which is the same stream a cold cambium
  index is built from, so the rule half of the index is not a Skyframe idea at
  all.
- **There is no `FILE_STATE` for an ordinary source file.** The loading phase
  stats `BUILD`, `.bzl` and directories; it never stats a `.java` or a `.cc`,
  and a content-only edit of one changes no loading-phase key. On the ten
  steps here, 1 to 143 of the changed paths per step named no node in the
  graph. A walk over the dump alone answers nothing for those steps, so the
  rule layer carries an explicit rule to source-path edge and the seed is
  matched against it. That edge is the whole answer on nine of the ten steps:
  `answer.dirty_packages` is 0 on all of them.
- **Sub-package carving is encoded, and the generic walk gets it.** `GLOBS`
  nodes depend on the `PACKAGE_LOOKUP` of candidate sub-directories (144 of
  556 glob nodes on this subject do), and `PACKAGE_LOOKUP` depends on the
  `FILE` of the `BUILD` and `BUILD.bazel` it probed for (1610 of 1611 do). So
  adding `a/b/BUILD` dirties `FILE_STATE:a/b/BUILD` -> `FILE` ->
  `PACKAGE_LOOKUP:a/b` -> `GLOBS:a` -> `PACKAGE:a`, with no rule about BUILD
  files written anywhere. Note the lookup depends on its *own* directory's
  build files, not on any ancestor's; the parent is reached from the glob
  side, not from an ancestor chain.
- **Glob dirtying is encoded.** `GLOBS` depends on `DIRECTORY_LISTING`, which
  depends on `DIRECTORY_LISTING_STATE`, so adding or deleting a file in a
  globbed package reaches the package. This is also why the seed carries
  listing keys for adds and deletes and not for content changes.
- **`MODULE.bazel` is a dead end in these kinds.** `FILE:MODULE.bazel` exists
  and depends on its `FILE_STATE`, and nothing among the eight kept kinds
  depends on `FILE:MODULE.bazel`. The chain out of it runs through repository
  and module-resolution kinds the loading-tier filter does not keep, and the
  external `BZL_LOAD` nodes it eventually reaches depend on `FILE` nodes under
  the output base rather than under the workspace. So of the README's
  modelled cases, the `bazel_dep` / override / lockfile case and the
  repository-rule-reads-the-main-repo case both still need code: a wider
  filter and a second tier, not a bigger walk.
- **`select()` needs no code here**, because the branches arrive as
  `rule_input` in the proto stream, the same place cambium reads them.
- **The re-query buys almost nothing on this subject and costs 2.6 s.** On the
  one step that dirties packages at all it re-queried 10 packages in 2586 ms
  and moved the answer from 3285 to 3279. That is the honest result, and the
  reason is that both variants seed *every* rule in a dirty package. What
  narrows the control's answer to 1342 on that step is comparing rule hashes
  either side of the change, which is exactly the hand-written work this
  prototype was built without. Package-granular dirtying plus a transitive
  rule closure over a repository whose core packages feed everything is what
  the 2524 extra targets are.
- **`--skyframe=rdeps` prints direct reverse dependencies only**, not a
  closure, so it is not a shortcut past building the CSR.

### What is still open

The prepare cost here is a universe query plus a dump, which is strictly more
than the control's cold index build does, and the dump half of it is the
cheap half. A version worth pursuing would drop the universe query for the
rule layer and take targets from an analysis-tier dump instead
(`skyframe-analysis`), leaving `skyframe-index` responsible only for the
loading-phase kinds it actually models well: `.bzl` closures, glob dirtying
and sub-package carving.
