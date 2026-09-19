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
| rebuild oracle | loading-layer rows now named and classified in `walk/rebuild-<pin>.json`, and an unexplained row fails the walk; the 40-step local bazel walk and box run A decide the residual (#24) |
| known gaps without a subject step | declined cases (#27). Autoload (#25) and toolchain rule class (#26) are no longer gaps: both have a rule on this branch and a code of their own (`CAMBI-W024`, `CAMBI-W023`), each with synthetic tests. Neither has a subject step that exercises it, so the rules are tested and unmeasured on a real history |
| over-reporting | inventoried in docs/OVER_REPORTING.md; the `.bazelrc` screen is the dominant global |

## The plan, in order

The box is the gate, not the workbench. A full five-subject run is seven
to eight hours of instance time, and the last one cost a day because it
was three runs chained (a restart after an abort, a queued control walk,
a cache purge before shutdown). So the phases below are batched into
**three box runs total**, and each has a local gate that must pass before
the box is started.

**The local gate, before any box run:**

- `cargo test --workspace`, clippy clean.
- The offline replay precheck over the stored indexes and recorded
  change sets in `bench-precheck/` (docs/OPERATIONS.md). This is what
  would have caught the `//external` guard abort before it cost a rerun.
- `cambi profile` on the bazel subject (five commits, local), plus the
  one subject the phase is about, at the steps its issue names. For #23
  that is xla at the six named pairs; for #22 it is envoy at three of the
  32 and xla at the four.
- A 40-step local walk on the bazel subject with the oracle every step.

**Box run rules:**

- `SMOKE=1` first, then the full run in one invocation, never a chained
  second run. If a subject aborts, the box stops, the abort is fixed
  locally with the precheck, and the run is rescheduled as a whole.
- Only the subjects the phase can change. A phase that touches the
  loading layer runs all five; a phase that touches one stage runs the
  subjects that exercise it.
- No purge on the box. Peer caches are deleted by wiping the volume at
  the end, or not at all; a 303 GB `rm` is not a shutdown step.
- Stop is chained to the run script's exit, and the console is checked.

### Phase 1: make the oracle say what it sees (#24), local only

Cheapest work with the highest information return. The rebuild oracle
counts missing loading-layer rows and prints a number.

- Make it list missing and extra rows by kind, package and key, to a
  sidecar file next to the walk result.
- Classify rows for files or packages that stopped existing between the
  pin and the step as expected, and stop counting them.
- Decide what is left: either a non-zero count fails the walk, or the
  allowed class is documented in docs/METHODOLOGY.md.
- Rerun xla on the local machine at the six named pairs with the
  current binary (b8c8cbc is not in the box run). This tells us whether
  the ifrt/ir cluster in #23 is already gone without a box run.

Exit: the bazel local walk reports zero unexplained missing rows, or the
remaining ones are named and filed. Other subjects are read from the
existing box results; the sidecar can be produced from the stored walk
JSON where it carries enough, and otherwise waits for box run A.

### Phase 2: the xla main-repo misses (#23), local only

The plan this section opened with (the `--keep_going` retirement bug fixed in
b8c8cbc, and a `cc_ir_header` chain at `704cb1bae17e..927b467078fd` whose
missing link was assumed to be a generated-file edge) predates the
measurement and is superseded by it. Both candidates were wrong.

Measured 2026-09-19, on the box, at the six named pairs
(docs/salvage/2026-09-18/xla-six-pairs-20260919.md). What it found:

- All six pairs are unsound. Every oracle query exited 3 under
  `--keep_going`, so each oracle answer is a lower bound: it can disprove
  soundness and cannot prove it, and a cleaner run can only find more.
- The `cc_ir_header` chain is not the cause. Its targets
  (`eigen_unary_32_ll_lib`, `_extract_bc`, `vector_ops` under
  `//xla/codegen/intrinsic/cpp`) are manual-tagged, and they are in the
  answer once `--include-manual` is passed, which is what the harness
  passes (`crates/bench/src/tools/cambi.rs`). Measuring these pairs by hand
  without that flag invents a 75-label miss that is not there.
- The dominant cause is the index, not the reverse walk. Most missing
  labels sit in packages the base index never loaded, so the engine could
  not have named them; `cambi explain` on representative misses says
  uniformly that no changed input reached them.
- Why the index was partial is answered
  (docs/salvage/2026-09-18/xla-partial-index-cause.md): the packages it
  lacks are exactly the consumers of `@llvm-project`'s tblgen rules, so one
  repository that did not fetch explains all of them. The same tree loaded
  those packages for the rdeps oracle on the same box the same day. It was a
  transient fetch failure that was then published and reused as the base for
  all six pairs, which is what `publish_partial` being a blanket boolean
  allows.

The load-failure fix on this branch stands on its own terms: a package that
fails to load under `--keep_going` used to come back empty and tombstone
every row it owned, and it now keeps the rows the index has (`CAMBI-W028`).
That is a real fault with tests, but the six pairs did not measure it, and a
walk is what would show it. Unmeasured.

Genuinely unexplained, and the only work left that a partial index does not
account for:

- The 32 labels at `704cb1bae17e..927b467078fd` that sit in packages the
  answer does cover: the triton and tiled lit-test packages, plus
  `//xla/service/cpu:ir_emitter2` and
  `//xla/backends/cpu/codegen:fusion_compiler`. These are the only misses at
  that pair a missing package cannot explain, so they are the candidates for
  a walk defect.
- Steps 46, 234, 256 and 262 of the walk record, 96 labels between them. The
  record has ten steps with main-repo misses, not the six #23 names, and
  these four are outside the tombstone mechanism and unexamined.

Exit: not met. #23 does not close on this. The numbers above describe the
index that was available rather than the engine, so the six pairs need
asking again against an index whose only load errors are the two expected
`tools/toolchains/win_1803` packages, and the four unexamined steps and the
32 in-package labels need reading. Standing policy while the engine is under
active change: every run builds a fresh index, and cached indexes are not
reused.

### Phase 3: toolchain rule-class escalation (#26), local only

Small and self-contained; done before the externals work because it
closes a real hole with one rule.

- When the splice reports a rule added, changed or tombstoned whose class
  is `toolchain`, `platform` or `toolchain_type`, or a `register_*` call
  moved, escalate with a new `CAMBI-W0xx` code.
- Drop the two default invalidator globs once the rule exists; they were
  the proxy for it.
- Synthetic test for each of the three classes.

Landed as `CAMBI-W023`, wider than the bullets above in three places the
review found, each with its own synthetic test:

- `constraint_setting` and `constraint_value` join the three classes. A
  setting's default or a value's parent moving changes what every platform
  matches with no platform or toolchain proto moving, and the walk from the
  constraint rule reaches only the declarations that name it.
- A `register_*` call in a `*.MODULE.bazel` segment the root `include()`s
  (Bazel 7.2 and later) is a call in the module file: the stage follows the
  root's include list at both revisions and diffs the file with its segments
  merged in. Before that, a segment was an ordinary source file and a
  registration moving inside one had an empty answer.
- A declaration in an external repository is not a row of `//...`, so the
  rule-class check cannot see it move. A repository this commit moved that a
  registration pattern at head offers rules from escalates instead, unless
  the only route to it was a path a target the repository rule *writes*
  names.

The rule-class half is decided after the query, over the rows it read back,
so it is the one verdict `affected --dry-run` cannot reach; the plan, the
README and docs/CLI.md say so rather than promising the plan is final.

Exit: met, both halves. The globs are gone from the default config, and
`cambi profile 948b8c70e28 --commits 5` on the local bazel mirror answers
the same walk before and after: 0 globals either way, no global reason, no
rebuilt commit, and every one of the five steps reporting the same affected
count, the same test count and the same index verdict (8 rows rewritten, 1,
unchanged, unchanged, 18). The before run is the pre-escalation code with
the two globs live; the after run is this branch with them dropped. Both
read 4,104 targets at `8aea3daca0`. The `CAMBI-W023` escalation therefore
costs nothing the globs were not already costing on this subject, and the
five commits in the window move nothing the rule fires on, so the window is
evidence that the drop is not a regression rather than evidence that the
rule is exercised. The subject's generated `cambium.toml` lists the two
dropped globs explicitly, which is why the before run could keep them
without a second binary; the next bench provisioning regenerates the file
from `Config::default()` and drops them, which changes the subject's config
hash and starts a fresh lineage (a cost the walk pays at its start anyway).

### Phase 4: autoload providers under a Bazel that autoloads (#25), local only

- The externals stage already knows which module a lock, a `MODULE.bazel` or
  a `WORKSPACE` diff moved. When that module is an autoload provider
  (`rules_python`, `rules_java`, `rules_shell`, `protobuf`, `rules_android`;
  not `rules_cc`, which no Bazel autoloads from) and the workspace's Bazel
  autoloads (8 at its default, or a 7.4 with
  `--incompatible_autoload_externally` set in `.bazelrc`; Bazel 9 disabled
  autoloads and reads the flag as a no-op), escalate with a diagnostic.
- Precise variant, if cheap: seed every package whose rule classes include
  a native rule that module autoloads. Only if the rule-class table is
  already in the index; otherwise the escalation is enough.
- One synthetic test per route, and a bazel-subject step that bumps a
  provider under a Bazel that autoloads if the history has one (that
  subject's `rules_cc` bumps are toolchain moves, `CAMBI-W014`, and its
  recent steps pin Bazel 9).
- Left for a phase of its own: the routes that name a provider without a
  workspace-file diff, an input path under a `local_path_override` or a
  nested repository and a workspace-closure `.bzl` that defines the
  repository. Under a Bazel that autoloads those moves are global for the
  same reason and are not escalated yet.

Exit: a provider bump under a Bazel that autoloads answers `//...` with a
code.

### Box run A: phases 1 through 4 together

All five subjects, 300 steps, one invocation, after the local gate on the
batched branch. Expected: bazel, sourcegraph, apollo unchanged; xla down
to the four external-route steps; envoy unchanged at 32. The rebuild
oracle's sidecar from Phase 1 is the new thing this run produces. This is
also the run RESULTS.md is regenerated from, which retires the pre-layer
numbers. Seven to eight hours, then stop.

### Phase 5: in-tree external repositories (#22)

The one piece of real design left, and the one that decides whether
envoy-shaped monorepos can use this in CI. Scope is deliberately narrow:
repositories whose sources are files of this tree. Declared-only externals
stay as declarations.

1. **Identify them.** From the externals stage's existing facts:
   `local_path_override`, `new_local_repository`, `local_repository`, and
   repository rules whose `path` or `build_file` names a tree path (`@tsl`
   in xla, `third_party/xla` in tensorflow). envoy's `@quiche` is that
   shape only before b6e7e12e: at the pin it is a registry `bazel_dep`
   whose overlay BUILD is hosted in the envoy registry, with no tree path
   at all, and the only route back into this tree is 18 `.bazelrc`
   `--@quiche//:flag=//...` lines. So identification reads the BUILD text
   a repository is built with and the `.bazelrc` too, not just the call's
   attributes.
2. **Index them.** Extend the universe from `//...` to `//... + @repo//...`
   for each identified repository. Their rule and file nodes join the
   index with their real labels; the loading layer already carries their
   `PACKAGE` nodes under a different root and today drops them, so stop
   dropping keys whose root is an identified repository.

   Soundness does not wait for this. The first stage lands sound by
   width with the universe unchanged: every main-repo label a repository
   is built from becomes an input of it, so a change that reaches one
   dirties that repository whole and the consumers come back through the
   reverse rows that exist today. That is what meets the exit below;
   indexing the repositories is what makes the answers precise.
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
5. **Cost.** Measure cold index and per-step cost on envoy locally first
   (30 commits with `cambi profile`); the universe grows by the in-tree
   repositories' packages, which is bounded by the tree.

Exit locally: envoy sound at three of its 32 steps and xla at its four,
profile cost within budget.

### Box run B: phase 5, and phase 6 if it is ready

envoy and xla only, 300 steps, unless the change touched the loading
layer's key filter, in which case all five. About four hours. Phase 6 is
a separate stage with its own local gate, and if it lands on the same
branch before run B starts it rides along; otherwise it waits for run C.

### Phase 6: the `.bazelrc` screen

Not an under-report, but the dominant global on the subjects (17 of
envoy's 18, 6 of xla's 7) and the thing a CI operator will notice first.
Twelve of envoy's seventeen are its root `.bazelrc`; the other five are
nested workspaces' rc files (`api/`, `mobile/`, `bazel/tests/external/`)
that Bazel never reads for this workspace at all.

- Widen the loading-irrelevant allowlist from evidence: collect every flag
  that fired `CAMBI-W004` across the five histories and classify each as
  loading-relevant or not, with the reasoning recorded per flag.
- Model `--config` scoping: a flag under `build:ci` only matters if the
  run's configs include `ci`. The run knows its configs.
- `--registry` changes are a lock-file question, not a global; route them
  to the externals stage.

Exit locally: the global count drops on the envoy and xla profiles with
no change to the affected sets. Confirmed at 300 steps in box run B or C.

**Landed 2026-09-18**, all three parts, on `phase-6-bazelrc-screen`. The
flag table and what it assumes are in docs/OVER_REPORTING.md under
`CAMBI-W004`, and in `crates/engine/src/bazelrc.rs` as a comment per entry
on `ALLOWLIST` (cleared) and, above it, the reason each refused flag is
kept off that list. A flag is refused by falling off the allowlist: the
screen reads no deny list at runtime. `EVIDENCE` in the same file records
the verdict for every flag that fired, refused ones included, and a test
(`the_allowlist_agrees_with_the_evidence`) holds the list and the table to
each other. `--config` scoping reads the configs in effect off the rc
chain at both ends of the range, plus the new `[bazelrc].configs` for the
ones a repository's own builds pass; the default is empty, which is what
cambium's index query passes. A changed `--registry` goes to the externals
stage and clears when `MODULE.bazel.lock` moved with it, and is
`CAMBI-W014` rather than `CAMBI-W004` when it did not.

Measured offline over each subject's own 300-step window, screen only, no
Bazel and no index. The table is the survey the interrupted session ran
against its own build of the screen, so re-derive it before the box run (`cargo test -p cambium-engine --test bazelrc_diff --
--ignored --nocapture`, with `CAMBIUM_BAZELRC_ENTRY` / `_FILE` / `_PIN` /
`_WINDOW`). "Before" is the `CAMBI-W004` count of run A's walk
(`bench-results-box/.../walk/walk-<pin>.json`):

| subject and rc file | edits in the window | before | after |
|---|---:|---:|---:|
| envoy `.bazelrc` | 13 | 12 | 4, plus 1 `CAMBI-W014` |
| xla `tensorflow.bazelrc` | 5 | 5 | 0 |
| xla `.bazelrc` | 1 | 1 | 0 |
| bazel `.bazelrc` | 1 | 1 | 0 |

envoy's four survivors are two `common --enable_bzlmod` (steps 293 and
296) and two `common --incompatible_enable_proto_toolchain_resolution`
(80 and 235), all loading-relevant under both classifications. Of the six
registry bumps, five carry a root `MODULE.bazel.lock` and clear through
the externals stage; step 262 carries only `MODULE.bazel`, so it stays
global with the registry named, as `CAMBI-W014`. apollo and sourcegraph
have no local mirror to survey: apollo's two `tools/bazel.rc build
--cxxopt` globals (steps 40 and 63) are on the cleared table, and its
step 8 `try-import %workspace%/.custom.bazelrc` stays global because the
import rule is written down and not implemented.

A step that was **not** global before cannot answer differently now: the
screen only ever clears more, every flag it now clears was a global
before rather than a seed into the direct set, and an allowlisted flag
drops the same rc change out of the diff that it dropped before. What
needs the box is the answers on the steps that are no longer global,
which is box run B or C.

### Box run C: the closing run

All five subjects, once, from main, after everything above is merged.
This is the run the external write-up quotes. If run B already covered
every phase, run C is the nightly's first run instead of a separate one.

### Phase 0, running throughout: the benchmark

Everything above is gated by the walk, so the walk's own debts come first
when they block a phase.

- #14, one walk with two tiers and the oracle as the only ground truth.
  Drop the synthetic verbs. This is what makes "a box run" one command.
- A cost budget per subject, written down: cold index seconds and
  per-step p50 and p90 on the box. Phase 5 needs it to say whether the
  universe growth is acceptable.
- A scheduled run, not a nightly: weekly at most, or per merge to main
  when a phase closes. CI on GitHub is blocked on billing; when it is
  unblocked, the local gate moves there and the box stays for the
  300-step runs only.
- The precheck corpus in `bench-precheck/` kept current: every box run's
  recorded change sets and stored indexes are added, so the next
  precheck replays more of what the box would.
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

| phase | issues | size | box run | why this order |
|---|---|---:|---|---|
| 0 | #14, RESULTS.md | ongoing | none | gates everything |
| 1 | #24 | days | A | cheapest, most information |
| 2 | #23 | days | A | may already be half done |
| 3 | #26 | day | A | one rule, closes a hole |
| 4 | #25 | day or two | A | one rule, closes a hole |
| 5 | #22 | weeks | B | the design piece; decides envoy-shaped repos |
| 6 | bazelrc | week | B or C | dominant global, operator-visible |

Three box runs: A after phases 1 to 4 (all five subjects, ~8 h), B after
phase 5 (envoy and xla, ~4 h), C once from main at the end (~8 h). About
twenty instance hours for the whole roadmap, against the twenty-four the
last single run cost.

The order is by information per hour until Phase 5, which is the only
phase whose outcome is uncertain. If Phase 5's cost measurement fails the
budget, the fallback is an explicit, documented invalidator per in-tree
repository, which is sound and over-reports, and the engine is still done
by the definition above.
