# The xla main-repo misses (issue #23)

Written 2026-09-19, on the Phase 2 branch. Issue #23 listed six history steps
of the 300-step xla walk where the oracle named main-repo targets cambium did
not. It offered two candidate mechanisms and confirmed neither. This is what
the recorded evidence says.

## One mechanism for the six, and four steps it does not cover

The six steps the issue lists are one fault, and it is not the one the issue
suspected. They are not the whole of the walk's main-repo misses: the same
record has ten such steps, and the four it does not list are unexplained
(below).

### The `cc_ir_header` chain is not the miss

The worst step, `704cb1bae17e..927b467078fd`, changes three source files and
misses 132 labels. The issue read that as the `cc_ir_header` macro in
`xla/codegen/intrinsic/cpp/cc_to_llvm_ir.bzl`: a `cc_library`, a custom
`_extract_object_files` rule, two genrules, a generated `.bc` and a generated
header. Generated-file edges were the suspect, because the index gives a
generated file one forward edge, to the rule that generates it.

The step's own label lists say otherwise. In
`bench-results-box/extracted/cambium/bench-results/xla/walk/labels/704cb1bae17e..927b467078fd/`
cambium and the oracle agree on every target of that chain, all twenty of
them, the two seeds included:

```
//xla/codegen/intrinsic/cpp:vector_ops
//xla/codegen/intrinsic/cpp:tanh_ll_lib
//xla/codegen/intrinsic/cpp:tanh_ll_extract_bc
//xla/codegen/intrinsic/cpp:tanh_ll_gen_object_and_header
//xla/codegen/intrinsic/cpp:tanh_ll
...
```

The walk crosses the `cc_library`, the custom rule, both genrules and the
generated header without losing anything. The generated-file edge works.

### The missing link is one package that stopped existing in the index

The 132 missing labels fall into whole packages that cambium reports nothing
at all for, plus everything above them:

| package | cambium | oracle |
|---|---:|---:|
| `//xla/codegen/emitters/transforms` | 0 | 1 |
| `//xla/codegen/emitters/ir` | 0 | 1 |
| `//xla/backends/cpu/codegen/emitters` | 0 | 2 |
| `//xla/backends/gpu/codegen/triton` | 15 | 21 |

The frontier is a single edge. `//xla/codegen/emitters/transforms:passes` is a
plain `cc_library` whose `deps` name
`//xla/codegen/intrinsic/cpp:_intrinsic_declarations`, which cambium reached;
`passes` itself it did not, and everything downstream of `passes`
(`:fusion_compiler`, `:cpu_fusion_emitters`, `:ir_emitter2`, the 111 lit
tests) went with it. The step re-queried nothing (`dirty_packages: 0`,
`queried_targets: 0`): the answer came entirely from the carried index, so the
row for `passes` was already gone before the step began.

That is the fault `b8c8cbc` half fixed. Under `--keep_going` a package Bazel
could not load comes back from the query with no targets, which reads exactly
like a package whose `BUILD` file was deleted. `b8c8cbc` stopped such a
package from retiring its `PACKAGE` node in the loading layer, so it can be
dirtied again later. It left the other half standing: the empty update still
reaches `apply_packages`, which tombstones every target the package owned and
clears every forward row they carried. Nothing re-queries a package whose
`BUILD` did not change, so those rows stay gone for the rest of the lineage.

The six are indices 185, 186, 187, 189, 190 and 194 of 300. That is one
window, but the window's edges are not evidence of anything: the sound steps
inside and beside it (188, 192, 193) re-queried nothing at all
(`dirty_packages: 0`, `queried_targets: 0`), so no `BUILD` edit brought any row
back there. Their soundness is a fact about which seeds those steps walk from,
not about the index. What does hold up is the packages: at each of the six the
index reports zero targets for a package the oracle reaches, and the frontier
is an edge into it. 283 of the 284 steps with an oracle ran against a partial
`rdeps` query, so xla does have load errors on this history, and they are what
emptied those packages.

### The four steps this does not explain, and what does

Added 2026-09-20, from box run A's record.

`summary.soundness` of
`bench-results-box/extracted-boxa/bench-results/xla/walk/walk-076fe93fad26cb2555e3bb86defa6a772d11cf35.json`
reports `steps_sound 292`, `steps_missing 4`. The six above are gone; these
four are what is left:

| step | pair | roots | missing | external_route |
|---:|---|---|---:|---:|
| 46 | `2c72f4acb3b4..f0791dddea19` | `//xla/tsl/profiler/utils:xplane_schema.{cc,h}` | 4 | 4 |
| 234 | `aa908163c7ae..0a1ec466320a` | `//xla/tsl/profiler/utils:xplane_schema.{cc,h}` | 4 | 4 |
| 256 | `d0abca510ed7..b2d442937e5a` | `//xla/tsl/platform:macros.h` | 48 | 25 |
| 262 | `4052d4ca9439..1725bd8f8409` | `//xla/tsl/platform:macros.h` | 48 | 25 |

It is one cause, not two, and it is not a walk mechanism at all. The oracle's
own `external_route` probe already named it on every step: every label it
asked about is one Bazel reaches only by leaving `//...` and coming back.

The route is `@tsl`. `third_party/tsl` carries a `REPO.bazel` marker and is
also the repository the `tsl_extension` of `MODULE.bazel` declares, so Bazel
loads those BUILD files twice: once as main-repo packages under
`//third_party/tsl/...`, once as `@tsl//...`. The `@tsl` copies name the main
repository back, as `@xla//xla/...`, which under bzlmod is the root module.
So `//xla/tsl/platform:macros` has a consumer at
`@tsl//tsl/profiler/lib:traceme`, and that has a consumer at
`//xla/service:lockable`. `rdeps(//..., ...)` expands its universe to the
transitive closure, so Bazel walks that; the index is `//...` by construction
and stops at the first node outside it, so the reverse walk never climbs back.
The `//third_party/tsl` twin is reached and reported, which is where the same
steps' extra labels come from: step 256 answers 14 `//third_party/tsl/tsl/profiler/lib`
targets the oracle does not name, the twins of the `@tsl` targets the route
actually runs through.

The two pairs differ only in their seeds. 46 and 234 walk from
`xplane_schema`, whose `@tsl` consumers are four; 256 and 262 walk from
`macros.h`, whose `@tsl` consumers are 48. Reading them as two faults was
reading two seeds.

All 48 are now confirmed by Bazel rather than by reading BUILD files: with the
probe cap raised (below) the rerun returns `external_route 48`,
`route_unprobed 0`, nothing unexplained. The paths are what the BUILD files
said they would be, for instance

```text
somepath(//xla/service:lockable, set(//xla/tsl/platform:macros.h))
  //xla/service:lockable
  @tsl//tsl/profiler/lib:traceme
  //xla/tsl/platform:macros
  //xla/tsl/platform:macros.h
```

and for the ones a hop further out, `//xla/tsl/cuda:cudart` through
`:cudart_stub` to `@tsl//tsl/platform:load_library`.

### It reproduces on a fresh index

`cambium-bench walk xla --tier correctness --peers none --oracle-sample 0
--only <the four pairs>` cold-builds an index at each pair's own parent and
answers forward once. All four come back label for label the same as box run
A: missing 4, 4, 48, 48, `external_route` 4, 4, 25, 25, worst recall 0.9802,
`engine_cold_rebuilds 0`, no global. A fresh index at the parent cannot carry
the emptied-package mechanism of the six above, and these four are unmoved by
it, so the cause is the vocabulary of the index and not the history it walked.

Run again with the probe cap at 200, the same four steps report
`external_route` 4, 4, 48, 48 and `route_unprobed` 0 on every one. Every label
of every one of these steps is an external route, with no residue.

### Why they were filed as unexplained

`external_routes` probed at most `MAX_ROUTE_PROBES` labels, 25, and recorded
nothing about the ones it skipped. On steps 256 and 262 that reads as 48
missing and 25 explained, and the 23 the probe never asked about are
indistinguishable in the record from 23 it asked about and could not explain.

Two things changed. `route_unprobed` (`crates/bench/src/oracles/rdeps.rs`) now
carries the count, so unexplained is
`missing - external_route - route_unprobed` and a truncated probe cannot be
read as a finding. And the cap itself was wrong: it was set against a reading
of `somepath` cost that was the first query on a cold server, not the steady
state. Measured on xla at `1725bd8f8409` against a warm output base, the first
probe takes 12.7 s and the next three 0.25, 0.37 and 0.43 s, so the 23 skipped
labels were worth about eight seconds on a step that had already failed. The
cap is 200 now, which is a guard against a pathological missing set rather
than a budget.

### What closes them

Phase 5 of docs/ROADMAP.md, stage 2: index the in-tree external repositories,
`@tsl` among them, so the universe is `//... + @repo//...` and the reverse
walk carries the edge. Nothing narrower closes them: the edge is real, and the
index does not have a node at either end of it. Until that lands these four
steps fail the walk, which is the correct answer for an under-report.

## The fix

A requested package that bazel named in a load error keeps the rows the index
has for it, whether it came back with no targets at all or with the few
`--keep_going` created before the error (a truncated update is the same
package half read, and applying it tombstones everything the error swallowed).
Three cases stay tombstoned: a package whose `BUILD` is gone was deleted; a
package this invocation declared deleted (`--deleted_packages`) does not exist
in this configuration, however many other packages failed in the same batch,
which is why the test is membership in the error list and not the presence of
a `BUILD` file (envoy declares plenty of packages deleted whose BUILD files
are on disk); and a package that loaded and is simply empty now was emptied by
an edit. Past 32 error names bazel's stderr is counted and not recorded, and
there the shape decides instead: no targets plus a `BUILD` file on disk.

One case carries the other way. A package dirtied by an edit to its own
`BUILD` file, which then failed to load, is tombstoned rather than carried:
the rows the index has describe it before the edit, so answering from them
would report the targets the edit changed as unaffected. Stale beats gone
everywhere except there, where wide beats short.

The run reports the carry-forward with `CAMBI-W028` and the rows it stands on
are as old as the last query that loaded the package.

The run also stops treating such a package as one it re-queried, so the `.bzl`
closure rescan and the loading-layer refresh leave what they hold for it
alone. Undoing the carry-forward one layer down would cost the same targets by
a different route.

## Where that leaves the six pairs

The mechanism is a property of a walk, not of a pair: it needs a step whose
query fails to load a package, and the damage shows up on later steps that
re-query nothing. A cold index built at either end of a pair does not reproduce
that mechanism, so the 300-step walk is its measurement, and that is box run A.

A pair-by-pair run measures something else, and one was done on 2026-09-19
(docs/salvage/2026-09-18/xla-six-pairs-20260919.md): `cambi index build <base>`
then `cambi affected <base>..<head> --include-manual`, scored against a
`bazel query rdeps` oracle, on all six pairs. Every pair came back unsound, so
the route does measure these pairs; it just does not measure this mechanism.
What it found was a different fault. The base index those runs loaded was short
seventeen packages because `@llvm-project` was absent when it was built, so the
engine held no rows for them either to carry forward or to tombstone
(docs/salvage/2026-09-18/xla-partial-index-cause.md). A freshly built index of
the same revision, the same day, had an error set of exactly the two expected
`tools/toolchains/win_1803` packages.

Two faults therefore sit over the same packages, and any measurement has to say
which one it is looking at. This document's mechanism needs a walk. The
partial-index fault needs only a stale index, which is why every run now builds
a fresh one. `--include-manual` matters to both: without it every pair shows a
spurious 75-label miss, and the `cc_ir_header` chain is answered in full once
it is passed, which is why that chain is no longer a suspect.

| pair | issue's count | verdict here |
|---|---:|---|
| `1ffb35c3f694..ac12f341f5b6` | 1 | same mechanism, `//xla/codegen/emitters/ir` emptied |
| `ac12f341f5b6..ec00cb04918b` | 39 | same mechanism, `ifrt/ir`, `mhlo_to_hlo`, `mosaic` emptied |
| `ec00cb04918b..c9fc88060ec4` | 71 | same mechanism, same three packages |
| `704cb1bae17e..927b467078fd` | 132 | same mechanism, `emitters/transforms` and two others emptied; not the `cc_ir_header` chain |
| `927b467078fd..dc85a4033ee0` | 24 | same mechanism, `ifrt/ir` emptied |
| `67157cf882d3..80c647bcaae0` | 1 | same mechanism, `//xla/codegen/emitters/ir` emptied |

These counts are the same six the 2026-09-19 pair run measured, label for
label. The two readings agree on what happened and differ on why: a package
whose load failed lost its rows, and "emptied" is that seen from the walk
record, while the absent `@llvm-project` is what made the load fail in the
first place. Neither reading makes the other wrong, and the fix in this
document (carry the rows a failed load would have cost) is worth having
whatever emptied the package, because a fresh index cannot help a walk that
loses a package halfway through.

The four steps above are outside this table and outside the mechanism:
they are the `@tsl` route, and Phase 5 is what closes them.

None of the six is a generated-file edge, and none of them is closed by
`b8c8cbc` on its own: that commit lets the package be dirtied again, it does
not keep the rows in the meantime.

## The bazel walk

The 40-step local walk on the bazel subject at pin
`948b8c70e281c2fe42aa5468dad543c7af9f0ccc` reports worst recall 1.0 over 40
steps with the oracle on every one, and its rebuild oracle reports
`missing 0`, `missing_expected 0`, `missing_unexplained 0`. There is no
unexplained missing row to name, so Phase 1's exit gate holds and nothing on
this branch is waiting on a classification gap. The one caveat is the oracle's
own: many steps logged that the `rdeps` query exited 3 under `--keep_going`,
so on those steps recall is measured against a lower bound.
