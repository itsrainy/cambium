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

## warm-query: no index at all

`crates/bench/src/skyframe/warm.rs`. There is no index, no dump and no state
on disk. `prepare` warms a Bazel server at the parent with `bazel query
//...`; `answer` asks that same server, now checked out at head, the oracle's
own question:

```text
kind(rule, rdeps(//..., set(//pkg:a.cc //pkg:defs.bzl ...)))
```

The label mapping and the batching are the oracle's
(`crates/bench/src/oracles/rdeps.rs`), reused rather than re-derived. The one
thing the strategy cannot borrow is the oracle's package list, which comes
from an index: it reads packages off the head worktree instead, by walking
each changed path's ancestors for a `BUILD` or `BUILD.bazel` file.

A path deleted at head cannot be named as a source label, so, in order: the
surviving ancestor package's `//pkg:*` (sound and wider, since the package is
a superset of the file's own reverse dependencies); then, for a `.bzl` or
`BUILD` file whose package is gone too, the packages `rbuildfiles` names;
then nothing, counted in `answer.unnamed_deletes`. That last case is the one
place this design is weaker than an index, which answers it from the parent.

Every step records the packages the second query re-loaded, read off the
`Loading: N packages loaded` progress line, since that number, not the wall,
is what says whether Skyframe's invalidation is change-proportional. The
suite's shared bazelrc passes `--noshow_progress`, so the query passes
`--show_progress --show_loading_progress` to turn it back on; without them
every step records 0 and the run reads like a perfect server.

Variants are read off the strategy name, so each is its own result file:
`warm-query-cold`, `-scope`, `-infer`, `-ordered`, `-rbuildfiles`,
`-nokeepgoing`.

### Results

Ten steps, seed 0, `bazel` subject, Bazel 9.2.0, a private `--subjects-dir`.
`cambium-bench skyframe compare docs/benchmarks/skyframe-*.json
docs/benchmarks/warm-query*.json`:

| strategy | subject | steps | prepare p50 ms | answer p50 ms | answer p50 | recall | extra p50 | state p50 bytes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| cambi | bazel | 3 | 40250 | 1850 | 751 | 3/3 | 0 | 669636 |
| warm-query | bazel | 10 | 477 | 2066 | 939 | 10/10 | 0 | 0 |
| warm-query-cold | bazel | 10 | 730 | 10980 | 939 | 10/10 | 0 | 0 |
| warm-query-infer | bazel | 10 | 1558 | 1440 | 939 | 10/10 | 0 | 0 |
| warm-query-ordered | bazel | 10 | 364 | 2345 | 939 | 10/10 | 0 | 0 |
| warm-query-rbuildfiles | bazel | 10 | 396 | 2119 | 939 | 9/10 | 0 | 0 |
| warm-query-scope | bazel | 10 | 1421 | 1368 | 939 | 10/10 | 0 | 0 |

The `cambi` row is three steps and the rest are ten, so the two samples
overlap rather than coincide; the medians are comparable, the step counts are
not.

### Findings

- **Warm answer wall is the control's, near enough.** 2066 ms p50 against the
  control's 1850 ms, over a wider sample. The spread is 1.2 s to 5.7 s once
  the server is warm, plus one 131 s outlier: the first answer of the run,
  where the checkout to head invalidated 936 packages on a server that had
  only just finished its first `//...`. Every later step pays a fraction of
  that. So on wall time this beats nothing and loses to nothing, and the
  choice between it and an index is not a speed choice.
- **Skyframe's invalidation really is change-proportional.** Packages
  re-loaded per step, warm: 0, 0, 0, 0, 0, 1, 33, 212, 213, 936, against a
  universe of about 4100 targets in roughly 1070 packages. Six of ten steps
  re-load nothing at all and still answer correctly, which is the strongest
  result here. The 936 is the first step of the run, the 212 and 213 are
  steps whose change reaches a `.bzl` in the load closure.
- **Cold is five times the wall and a full re-load, every time.** With
  `bazel shutdown` before the answer: 10.9 s p50, worst 32.7 s, and 993 to
  1070 packages re-loaded on every step, which is the whole universe. So the
  design fits a dev machine, where a server is already up, and not CI, where
  each job would pay the cold number and then throw the graph away. That is
  the one number that decides where this sits.
- **Recall is 10/10 and tightness is exact, by construction.** Nine of ten
  steps reproduce the oracle target for target (extra 0), which is what has
  to happen: this is the oracle's own query. The tenth is the
  `build_add_delete` step, 144 deleted paths, where the deleted-path policy
  seeds 144 `//pkg:*` wildcards and answers 2647 targets over an oracle of
  761. The over-approximation is the policy's, not Bazel's, and it is the
  safe direction.
- **A first run scored 8/10, and the cause was the harness, not the
  strategy.** That run used the default `~/.cache/cambium/subjects` while
  other prototype runs were using it too. Its answer for one step was missing
  `//:verify_module_bazel_lock`, a target that exists at neither end of that
  step: it is declared only at `8b43b175147`, which is not an ancestor of the
  step's head. Another run had moved the shared worktree under the query.
  Skyframe runs on one machine need a private `--subjects-dir` each, and the
  `answer.same_server` note (true on every step of every run here) is what
  says the server at least was not restarted underneath.
- **`--universe_scope=//...` is the best flag.** 1368 ms p50 against 2066,
  and 645 packages re-loaded across the whole run against 1394. Sky Query
  mode evaluates the universe once for the server rather than as part of the
  `rdeps` expression. `--infer_universe_scope` is nearly as good (1440 ms,
  677 packages) and needs no literal, but its spread is worse (up to 7.5 s).
  If this design is pursued, `--universe_scope` is the shape to pursue.
- **`--order_output=no` is worth having.** Ordered output costs 2345 ms
  against 2066, and re-loads twice the packages (1394 against 645 for the
  scoped variant), since the sort forces more of the graph.
- **`--keep_going` is not optional on this subject.** Without it the first
  step exits 7 and the run stops: `no such target
  '@@+bazel_build_deps+debian_java_deps//:core-jars-0.8.14'`. The subject
  cannot load the whole of its `//...` on macOS, which is why every query in
  this suite accepts exit 3 as a lower bound. There is no
  `warm-query-nokeepgoing` result file because the run produced no steps.
- **`rbuildfiles` first is unsound, so do not.** 9/10, and the missed step is
  the `bzl` one: `rbuildfiles(<changed .bzl>)` named no main-repo package at
  all, so the file's own reverse dependencies were never seeded and 352
  targets went missing. A `.bzl` loaded only by other `.bzl` files has no
  `BUILD` that `rbuildfiles` will name. It stays as the second-chance route
  for a deleted file, where there is nothing better, and never as a
  replacement for naming the file itself.
- **State is genuinely zero, and that is the trade.** A warm server holds its
  graph in memory, so `state_bytes` is 0 against the control's 669 KB, and
  nothing is carried between machines, between jobs, or into the past. The
  price is the cold row: without a server, every answer re-loads the world.
