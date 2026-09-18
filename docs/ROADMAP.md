# Roadmap to done

Written 2026-09-17, the day the Skyframe loading layer landed. This is the
plan for finishing the engine, and the definition of "finished" it works
towards. Issue numbers are this repository's.

## What done means

cambium is built for CI: a cold machine, no warm Bazel server, a persistent
index carried between runs. Done is:

1. **Sound on everything the query graph can express.** Every history step
   and change set on every subject either reproduces the rdeps oracle or
   fails the walk with a named reason. No silent miss.
2. **Escalation is explicit and finite.** Every `//...` answer carries a
   diagnostic code, the list of codes is closed, and each one is either
   fixable-and-scheduled or declined in writing.
3. **Cost is change-proportional on the box.** Per-step wall on the box
   scales with dirty packages and not with the repository; a cold index is
   paid once per lineage break.
4. **The benchmark is the gate.** A change merges only after the walk on
   the box passes at the same soundness as before, and the rebuild oracle
   reports no unexplained row.
5. **A CI operator can trust the output without reading the code.** The
   diagnostics and the docs are enough to decide whether a `//...` answer
   was reasonable.

Not in scope, declined rather than deferred: analysis-phase edges
(toolchain resolution, aspects, `select()` configuration), repository-rule
inputs no label names, symlinked directories inside a package, and external
repository contents changing at a fixed version. Every peer tool shares
these, a cold `bazel query` shares most of them, and the skyframe-analysis
prototype measured the price of chasing them (docs/SKYFRAME_PROTOTYPES.md).

## Where we are

| | status |
|---|---|
| bazel, sourcegraph, apollo | 300 steps each, recall 1.0, PASS |
| envoy | 32 of 300 unsound, all the `@quiche` external round trip (#22) |
| xla | 10 of 300 unsound: 4 external round trip (#22), 6 main-repo (#23) |
| rebuild oracle | "layer rows missing" non-zero on every subject, never named (#24) |
| known gaps without a subject step | autoload (#25), toolchain rule class (#26), declined cases (#27) |
| over-reporting | inventoried in docs/OVER_REPORTING.md; the `.bazelrc` screen is the dominant global |

## The plan, in order

Each phase ends with a box run and a merge to main. Nothing starts on the
next phase until the previous one's box run is green or its failure is
understood and filed.

### Phase 1: make the oracle say what it sees (#24)

Cheapest work with the highest information return. The rebuild oracle
counts missing loading-layer rows and prints a number.

- Make it list missing and extra rows by kind, package and key, to a
  sidecar file next to the walk result.
- Classify rows for files or packages that stopped existing between the
  pin and the step as expected, and stop counting them.
- Decide what is left: either a non-zero count fails the walk, or the
  allowed class is documented in docs/METHODOLOGY.md.
- Rerun xla on current main (b8c8cbc is not in the box run). This tells
  us whether the ifrt/ir cluster in #23 is already gone.

Exit: every subject reports zero unexplained missing rows, or the
remaining ones are named and filed.

### Phase 2: the xla main-repo misses (#23)

Whatever Phase 1 leaves. Two candidates:

- The `--keep_going` retirement bug, fixed in b8c8cbc. If the rerun
  clears the ifrt/ir, mhlo_to_hlo and mosaic misses, close that half.
- The `cc_ir_header` chain at `704cb1bae17e..927b467078fd`: two source
  files reach 132 lit-test targets through a `cc_library`, a custom rule
  and two genrules. Reproduce with `cambi` at that pair against the rdeps
  query, find the missing link (generated-file edges are the suspect),
  fix, add the shape to the synthetic corpus.

Exit: xla history unsound steps are exactly the four external-route
steps.

### Phase 3: toolchain rule-class escalation (#26)

Small and self-contained; done before the externals work because it
closes a real hole with one rule.

- When the splice reports a rule added, changed or tombstoned whose class
  is `toolchain`, `platform` or `toolchain_type`, or a `register_*` call
  moved, escalate with a new `CAMBI-W0xx` code.
- Drop the two default invalidator globs once the rule exists; they were
  the proxy for it.
- Synthetic test for each of the three classes.

Exit: the globs are gone from the default config and the walk is
unchanged on every subject.

### Phase 4: autoload providers on Bazel 8+ (#25)

- The externals stage already knows which module a lock or `MODULE.bazel`
  diff moved. When that module is an autoload provider (`rules_cc`,
  `rules_python`, `rules_java`, `rules_shell`, `protobuf`) and the
  workspace's Bazel is 8 or later, escalate with a diagnostic.
- Precise variant, if cheap: seed every package whose rule classes include
  a native rule that module autoloads. Only if the rule-class table is
  already in the index; otherwise the escalation is enough.
- One synthetic test, and a bazel-subject step that bumps `rules_cc` if
  the history has one.

Exit: a provider bump on Bazel 8+ answers `//...` with a code.

### Phase 5: in-tree external repositories (#22)

The one piece of real design left, and the one that decides whether
envoy-shaped monorepos can use this in CI. Scope is deliberately narrow:
repositories whose sources are files of this tree. Declared-only externals
stay as declarations.

1. **Identify them.** From the externals stage's existing facts:
   `local_path_override`, `new_local_repository`, `local_repository`, and
   repository rules whose `path` or `build_file` names a tree path
   (`@quiche` in envoy, `@tsl` in xla, `third_party/xla` in tensorflow).
2. **Index them.** Extend the universe from `//...` to `//... + @repo//...`
   for each identified repository. Their rule and file nodes join the
   index with their real labels; the loading layer already carries their
   `PACKAGE` nodes under a different root and today drops them, so stop
   dropping keys whose root is an identified repository.
3. **Walk them.** A changed tree path that is a source of `@repo//pkg` is
   seeded as that label; the reverse walk then crosses back into `//...`
   through the edges that were leaves before. The externals stage's
   "declaration moved, every leaf dirty" rule no longer applies to these
   repositories, which removes the largest over-report class on envoy
   (65 226 labels across 40 change sets) at the same time.
4. **Gate it.** The rdeps oracle's `external_route` probe already names
   the round trip; it should now pass on envoy's 32 steps and xla's 4.
   Keep the probe, because a declared-only repository can still round
   trip and must still fail.
5. **Cost.** Measure cold index and per-step cost on envoy and xla on the
   box before and after. The universe grows by the in-tree repositories'
   packages, which is bounded by the tree.

Exit: envoy and xla PASS at 300 steps, and the cold and per-step cost
columns are within the budget set in Phase 0 below.

### Phase 6: the `.bazelrc` screen

Not an under-report, but the dominant global on the subjects (13 of
envoy's 18, 6 of xla's 7) and the thing a CI operator will notice first.

- Widen the loading-irrelevant allowlist from evidence: collect every flag
  that fired `CAMBI-W004` across the five histories and classify each as
  loading-relevant or not, with the reasoning recorded per flag.
- Model `--config` scoping: a flag under `build:ci` only matters if the
  run's configs include `ci`. The run knows its configs.
- `--registry` changes are a lock-file question, not a global; route them
  to the externals stage.

Exit: the history global count drops on envoy and xla with no change to
soundness.

### Phase 0, running throughout: the benchmark

Everything above is gated by the walk, so the walk's own debts come first
when they block a phase.

- #14, one walk with two tiers and the oracle as the only ground truth.
  Drop the synthetic verbs. This is what makes "a box run" one command.
- A cost budget per subject, written down: cold index seconds and
  per-step p50 and p90 on the box. Phase 5 needs it to say whether the
  universe growth is acceptable.
- Nightly on the box against main, results tarred back, RESULTS.md
  regenerated. CI on GitHub is blocked on billing; the box is the CI until
  that changes.
- Regenerate RESULTS.md for the layer now; it still describes the
  pre-layer engine.

## After the engine is done

The remaining work is product, and it is where the next decision is:

- **Diagnostics for an operator.** Every `CAMBI-W` code gets a one-line
  "what to do about it" in docs/DIAGNOSTICS.md, and `cambi` prints it.
- **Lineage in CI.** How the index moves between runners: an artifact
  store, the cache key, what happens on a lineage break. The store format
  is done; the operational story is not written.
- **One external-facing write-up.** What the tool answers, what it
  declines, and the box numbers, for someone deciding whether to run it.

## Order and sizing

| phase | issues | size | why this order |
|---|---|---:|---|
| 0 | #14, RESULTS.md | ongoing | gates everything |
| 1 | #24 | days | cheapest, most information |
| 2 | #23 | days | may already be half done |
| 3 | #26 | day | one rule, closes a hole |
| 4 | #25 | day or two | one rule, closes a hole |
| 5 | #22 | weeks | the design piece; decides envoy-shaped repos |
| 6 | bazelrc | week | dominant global, operator-visible |

The order is by information per hour until Phase 5, which is the only
phase whose outcome is uncertain. If Phase 5's cost measurement fails the
budget, the fallback is an explicit, documented invalidator per in-tree
repository, which is sound and over-reports, and the engine is still done
by the definition above.
