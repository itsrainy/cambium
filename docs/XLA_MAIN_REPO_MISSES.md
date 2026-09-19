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

### The four steps this does not explain

`summary.soundness` of
`bench-results-box/extracted/cambium/bench-results/xla/walk/walk-076fe93fad26cb2555e3bb86defa6a772d11cf35.json`
reports `steps_sound 274`, `steps_missing 10`. Six of the ten are above. The
other four are main-repo misses too and no reading here covers them:

| step | pair | missing |
|---:|---|---:|
| 46 | `2c72f4acb3b4..f0791dddea19` | 4 |
| 234 | `aa908163c7ae..0a1ec466320a` | 4 |
| 256 | `d0abca510ed7..b2d442937e5a` | 48 |
| 262 | `4052d4ca9439..1725bd8f8409` | 48 |

Steps 46 and 234 miss the same four labels under `//xla/tsl/profiler/...`;
steps 256 and 262 miss the same 48, scattered over some twenty packages
(`//third_party/tsl/tsl/platform:dso_loader`,
`//xla/backends/cpu/codegen:cpu_features` and the rest), which is not the shape
an emptied package makes. All four re-queried nothing. They are open, and
issue #23 does not close with them unexamined.

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

The run reports the carry-forward with `CAMBI-W023` and the rows it stands on
are as old as the last query that loaded the package.

The run also stops treating such a package as one it re-queried, so the `.bzl`
closure rescan and the loading-layer refresh leave what they hold for it
alone. Undoing the carry-forward one layer down would cost the same targets by
a different route.

## Where that leaves the six pairs

The mechanism is a property of a walk, not of a pair: it needs a step whose
query fails to load a package, and the damage shows up on later steps that
re-query nothing. A cold index built at either end of a pair never reproduces
it, so `cambi index build <base>` followed by `cambi affected` cannot measure
these six. The 300-step walk is the measurement, and it is box run A.

| pair | issue's count | verdict here |
|---|---:|---|
| `1ffb35c3f694..ac12f341f5b6` | 1 | same mechanism, `//xla/codegen/emitters/ir` emptied |
| `ac12f341f5b6..ec00cb04918b` | 39 | same mechanism, `ifrt/ir`, `mhlo_to_hlo`, `mosaic` emptied |
| `ec00cb04918b..c9fc88060ec4` | 71 | same mechanism, same three packages |
| `704cb1bae17e..927b467078fd` | 132 | same mechanism, `emitters/transforms` and two others emptied; not the `cc_ir_header` chain |
| `927b467078fd..dc85a4033ee0` | 24 | same mechanism, `ifrt/ir` emptied |
| `67157cf882d3..80c647bcaae0` | 1 | same mechanism, `//xla/codegen/emitters/ir` emptied |

The four steps above are outside this table and outside the mechanism.

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
