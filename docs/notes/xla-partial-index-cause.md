# Why the xla index was partial, 2026-09-19

The six pairs of issue #23 were measured on 2026-09-19 and all six were
unsound, but most missing labels sat in packages the index never held (see
xla-six-pairs-20260919.md). This is the answer to why those packages were
missing. It is not an engine defect, and it is not the tree's fault.

## The proximate cause, proven by set equality

`cambi inspect stats` on the base index (lineage `b411bea59b`) lists 19 error
packages: the two `tools/toolchains/win_1803/py3{8,9}` and 17 xla packages.

Grepping the tree for `load("@llvm-project//mlir:tblgen.bzl", ...)` returns
exactly 16 packages, and they are exactly 16 of those 17. The 17th,
`xla/mlir_hlo/tests`, depends on `//xla/mlir_hlo`, which is one of the 16.

So every missing package is a consumer of `@llvm-project`'s tblgen rules, and
the single cause is that `@llvm-project` was not available when that index was
built. `@llvm-project` comes from `MODULE.bazel:217-218` through
`third_party/extensions/llvm.bzl`, which reads `@llvm-raw`, which is
`third_party/llvm/workspace.bzl`: a pinned LLVM monorepo tarball of roughly
200 MB plus patches. The pin moves across xla revisions, so a history walk
re-fetches it repeatedly.

## The control that rules out the alternatives

On the same box, the same day, with the same bazelisk, the rdeps oracle loaded
those packages without trouble: `ac12f341f5b6..ec00cb04918b.oracle` carries 28
labels in `//xla/mosaic`, `//xla/mlir_hlo` and `//xla/python/ifrt/ir`, while
cambium's answer for the same pair carries none. The oracle used a fresh
per-pair output base and its stderr shows live downloads. Cambium used the
shared long-lived output base and an index built two days earlier.

The tree is therefore loadable at these revisions. What failed was one index
build's environment, and the broken index was then cached and reused as the
base for all six pairs.

## The cambium defect this exposes

`subject_repos.toml` sets `publish_partial = true` for xla, with a comment
saying the pin is partial by exactly the two `win_1803` packages. The flag is
a blanket boolean, so the harness published an index that was missing 17 more
packages than the comment allows for, and every later run reused it. A
transient fetch failure became a persistent under-report, silently, which is
the one failure mode this project exists to prevent.

`--noshow_progress` in the shared rc hid the download and its failure, and
`--lockfile_mode=off` means the module extensions are re-evaluated on every
run rather than served from `MODULE.bazel.lock`, which maximises exposure to
the fetch. Both are contributing factors, not the mechanism.

## What is NOT the cause

- Running `configure.py` is not required. XLA's own build_from_source says it
  "can be used to adjust" settings; what it writes (`xla_configure.bazelrc`)
  is compiler and CUDA settings that affect analysis and execution, not
  loading. Do not run it against the shared mirror.
- `HERMETIC_PYTHON_VERSION` is not required; it defaults to 3.11.
- The `//:requirements.txt` failure is an upstream xla bzlmod defect:
  `third_party/extensions/python_version.bzl` hardcodes
  `REQUIREMENTS = "//:requirements.txt"` behind a TODO, where the
  WORKSPACE-era path computed the real lock file. No flag fixes it, and its
  blast radius is `//third_party/py` alone.
- The `tsl/platform` and `tsl/profiler/protobuf` errors are a universe
  enumeration artifact. `third_party/tsl` is the root of the `@tsl`
  repository, so `//...` walks into it and reads its BUILD files as
  main-repo packages, where its own `//tsl/platform` labels do not resolve.
  Any tool running `bazel query //...` on this tree hits it. The same class
  covers `third_party/rdma_core` and `tools/toolchains/ios`, WORKSPACE-era
  repositories left undeclared under bzlmod.
- The `win_1803` tracebacks are `py_runtime_pair` from `@bazel_tools`, which
  now fails unconditionally at load time on every host. Expected.

## What to change

1. Make `publish_partial` an allowlist of expected-failing package paths
   rather than a boolean (`crates/bench/src/manifest.rs:60-67`), and refuse to
   publish when the error set is not a subset of it. For xla the allowlist is
   the two `win_1803` packages. This alone turns a silent under-report into a
   loud failure.
2. Record the bazel stderr beside the index file, so the next investigation
   reads the cause instead of inferring it from a set intersection.
3. Verify `@llvm-project` resolves once per revision before indexing, and hard
   fail rather than letting `//...` degrade. Cost is network only, and the
   shared repository cache absorbs repeats.
4. Reconsider `--lockfile_mode=off` for xla; redirect the lockfile rather than
   disabling it, so extension results are reproducible per revision.
5. Consider excluding `third_party/tsl/...` from the query universe for BOTH
   the engine and the oracle. It removes the noise that makes every oracle
   query exit 3, which is what makes the answers lower bounds rather than
   exact. Applied to only one side it would invalidate the comparison.

Until a fresh index exists whose error set is exactly the two `win_1803`
packages, issue #23's numbers describe the index, not the engine.
