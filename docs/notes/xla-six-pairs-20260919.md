# The six xla pairs, measured on the box, 2026-09-19

Issue #23 says cambium under-reports at six xla pairs. They had never been
measured since the loading layer landed. Two local attempts died (the first
to a bazel server killed by load average 193, the second to a walk flag that
does not exist: `cambium-bench walk` has no way to restrict itself to named
pairs). This run was done on the c7i.4xlarge, idle, against branch
salvage/wf-20260918 at 82c9a26, and took about 35 minutes.

Logs and label lists (not tracked, `bench-results*/` is ignored):
`bench-results-box/archives/xla-pairs-20260919/`, `xlalogs.tgz` sha256
`fd5c88187ee424781d506c959017e2d7b9504daf067f7fe885ca9b39b722c8fa`,
verified identical on both machines.

## Method

Mirrors `crates/bench/src/oracles/rdeps.rs` and `tools/bazel_query.rs` rather
than inventing a query: changed paths from `git diff --name-only BASE..HEAD`,
each mapped to its source label, one
`bazel query 'kind(rule, rdeps(//..., set(<root>)))' --keep_going` per root,
union of roots as the oracle answer. Cambium's answer is
`cambi affected BASE..HEAD --include-manual`, because `--include-manual` is
what the harness itself passes (`crates/bench/src/tools/cambi.rs:37`).
Without that flag every pair shows a spurious 75-label miss, which is a trap
for anyone measuring these by hand.

## Result

| pair | oracle rules | cambi | missing | verdict |
|---|---:|---:|---:|---|
| 704cb1bae17e..927b467078fd | 5438 | 5306 | 132 | unsound |
| ec00cb04918b..c9fc88060ec4 | 8002 | 7931 | 71 | unsound |
| ac12f341f5b6..ec00cb04918b | 7786 | 7747 | 39 | unsound |
| 927b467078fd..dc85a4033ee0 | 5552 | 5528 | 24 | unsound |
| 1ffb35c3f694..ac12f341f5b6 | 5362 | 5361 | 1 | unsound |
| 67157cf882d3..80c647bcaae0 | 5270 | 5269 | 1 | unsound |

Every bazel query exited 3 under `--keep_going`, so every oracle answer here
is a lower bound. A lower bound can disprove soundness and cannot prove it.
All six were disproved, so the caveat does not soften the verdict, but it does
mean a later clean run can only find more, never fewer.

## What the misses actually are

The cached base index for every pair is the same lineage (`b411bea59b`) and it
is a PARTIAL index: bazel failed to load 21 or more packages into it, among
them `xla/python/ifrt/ir`, `xla/hlo/translate/mhlo_to_hlo`, `xla/mosaic`,
`xla/codegen/emitters/ir`, and the cpu and gpu codegen emitter packages.

Splitting each pair's misses by whether they sit in a package cambium reported
at all:

| pair | in a package wholly absent from the answer | in a reported package |
|---|---:|---:|
| 704cb..927b | 100 | 32 |
| ec00..c9fc | 68 | 3 |
| ac12..ec00 | 38 | 1 |
| 927b..dc85 | 23 | 1 |
| 1ffb..ac12 | 1 | 0 |
| 67157..80c6 | 1 | 0 |

So the dominant cause is not a reverse walk that loses an edge. It is that the
engine cannot report a target that never entered the index. `cambi explain` on
the representative misses agrees, saying uniformly that no changed input
reached them.

That reframes both clusters:

- Cluster (a), the ifrt/ir, mhlo_to_hlo and mosaic family, was thought to be
  fixed by b8c8cbc. b8c8cbc IS an ancestor of the build measured here, and the
  labels are still missed, but every one of them lies in a package the index
  could not load. This measurement neither confirms nor clears b8c8cbc; it
  says the question cannot be asked against a partial index.
- Cluster (b), the cc_ir_header chain at 704cb1bae17e..927b467078fd, is the
  surprise. The `//xla/codegen/intrinsic/cpp` chain targets
  (`eigen_unary_32_ll_lib`, `_extract_bc`, `vector_ops`) ARE in cambium's
  direct set once `--include-manual` is passed. They are manual-tagged. The
  generated-file edge was the suspect and it is not the cause of these misses.

## What is worth digging into

The 32 misses at 704cb..927b that sit in packages cambium DID report. Those
are the only ones here that a partial index does not explain, and so the only
ones that could still be a genuine walk defect. They are the triton and tiled
lit-test packages plus `//xla/service/cpu:ir_emitter2` and
`//xla/backends/cpu/codegen:fusion_compiler`.

Everything else needs a clean full index of xla (no load errors) before it can
be asked again. Until that exists, #23's numbers describe the index that was
available, not the engine.
