# Benchmarks

Measured numbers, and what they say about whether the design holds.

Everything below is from one machine — an **Intel Xeon @ 2.10 GHz, 4 cores,
16 GiB**, a shared cloud VM rather than a build fleet worker. The spec's
absolute targets assume 16 cores, so treat these as a lower bound on what the
design delivers, and as a baseline for regression gating rather than as
headline performance numbers. Raw results are in `bench-results/`. Reproduce
with:

```console
$ cargo build --release -p cambium-cli
$ ./target/release/cambium bench run --preset small,medium,large --runs 5 \
    --out bench-results/mine.json --markdown bench-results/mine.md
$ ./target/release/cambium bench compare --baseline bench-results/known-good.json \
    --current bench-results/mine.json --threshold 15
```

Every result records the preset, the spec hash of the generated graph, the
tool's git sha and a machine fingerprint; `bench compare` refuses to compare
runs from different machines rather than reporting noise as a regression.

## Presets

| preset | targets | index nodes | index edges | index on disk |
|---|---:|---:|---:|---:|
| `small` | 2 007 | 10.3k | 19.1k | 1.0 MB |
| `medium` | 50 176 | 251k | 495k | 25.3 MB |
| `large` | 501 759 | 2.5M | 5.0M | 261 MB |
| `xl` | 2 000 000 | ~10M | ~20M | ~1 GB |

Index nodes exceed the target count because source files are nodes too. Shape
variants (`-hubby`, `-deep`, `-flat`, `-wide-packages`) keep the size and change
the topology; `cambium synth presets` lists them all.

## In-memory stages

Median of 5 runs (3 at `large`).

| stage | small | medium | large |
|---|---:|---:|---:|
| `build_index` | 3.8 ms | 192 ms | 4.03 s |
| `load_index`, mmap only | 0.01 ms | 0.01 ms | 0.01 ms |
| `load_index`, + label map | 0.21 ms | 8.7 ms | 279 ms |
| `classify` | 1.1 ms | 31 ms | 63 ms |
| `parse_proto` | 8.1 ms | 239 ms | — |
| `rule_hash` | 4.7 ms | 141 ms | — |
| `splice`, 8 dirty packages | 0.46 ms | 16 ms | 449 ms |
| `reverse_rebuild` | 0.06 ms | 3.0 ms | 82 ms |
| `walk`, 10 starting points | 0.00 ms | 0.01 ms | 11 ms |
| `walk`, hub starting point | — | — | 34 ms |
| `e2e_memory`, `typical-pr` | 3.9 ms | 37 ms | 822 ms |

Throughputs at `large`: 623k targets/s indexed, 757k paths/s classified, 61M
edges/s for the reverse rebuild. At `medium`, 150 MB/s of streamed proto and
355k rule hashes/s. Peak RSS: 13 MiB, 194 MiB and 1.84 GiB for the three sizes.

`parse_proto` and `rule_hash` are not run at `large`: they materialise the
whole proto stream in memory, which is a multiple of the index size. They are
parse-rate measurements, and the parse rate does not depend on the size of the
repository.

The `walk` row is the one number that needs reading carefully. It is not
proportional to the graph, it is proportional to the answer, and the answer
depends entirely on where you start. The 11 ms at `large` enumerates 121 000
affected targets — a quarter of the graph — at about 90 ns each. The 0.01 ms at
`medium` enumerates 337.

### Against the spec's absolute targets

| target | spec | measured | |
|---|---|---|---|
| `walk` on `large`, non-hub start | < 5 ms | 11 ms, 121k affected | see above |
| `walk` on `large`, hub start | < 50 ms, ~40% affected | 34 ms, 50% affected | ✅ |
| `splice` + `reverse_rebuild` on `large` | < 300 ms | 449 + 82 ms | ❌ see below |
| `load_index` on `large`, warm | < 20 ms | 0.01 ms | ✅ the label map is separate and off the walk path |
| `build_index` on `xl` | < 60 s, < 6 GiB | ~16 s, ~7 GiB (extrapolated) | time ✅, memory over |
| `e2e_memory` `typical-pr` on `xl` | < 1 s | ~3 s (extrapolated) | ❌ dominated by the splice |

On four cores, with the splice accounting for both misses.

## Where the time actually goes

End-to-end on `large`, for a typical pull request: ten changed files, one of
them a `.bzl` that 42 packages load.

| stage | ms | proportional to |
|---|---:|---|
| dirty set | 42 | changed files (plus one `rbuildfiles` call) |
| query | 4 | targets in dirty packages — synthetic here; a real Bazel query is the dominant cost |
| splice | 425 | **the whole graph** |
| direct set | 2 | dirty targets and changed files |
| walk | 40 | the answer (280k targets) |
| **total** | **822** | |

Everything except the splice sums to ~88 ms, and each of those stages is
proportional to the change or to the answer, which is the property the whole
design exists to get. The splice is proportional to the graph and is 52% of
the run.

It is not the CSR memcpy the spec budgeted for. v1 produces a *complete new
index* at head, so `IndexBuilder::from_view` copies the label bytes (~110 MB at
this size), the per-node hashes (80 MB) and the edges (40 MB), and rebuilds the
interner's hash table over 2.5M labels. The edge memcpy the spec estimated at
"tens of milliseconds" is indeed tens of milliseconds; the rest is the cost of
materialising everything else.

The fix is the one the spec already anticipates (section 15, *delta overlays*):
a pull-request run does not need a new index at all. It needs the base reverse
CSR plus a small in-memory delta for the dirty packages, and only
`baseline update` needs to write a spliced index. That would take this run from
~820 ms to under 100 ms. It is deliberately not in v1, because the splice is
what makes the incremental index equal to a full rebuild — the property the
correctness oracles check on every commit — and it was worth having that before
optimising it away.

Two smaller things, both found by measuring rather than reading:

- The direct-set stage was rebuilding a label map over the finished index
  (340 ms at `large`) to resolve changed file paths to node ids. It now
  resolves them through the builder's interner, before the index is finalised:
  340 ms → 2 ms.
- `FixedBitSet::grow` reallocates to exactly the requested size, so growing it
  one node at a time made a cold build quadratic. It now doubles.

## Scaling

`bench sweep` varies one parameter and plots the result on log-log axes, where
linear scaling is a line parallel to the dashed reference.

![walk scaling](docs/sweep-walk.svg)

```
walk from a hub    10k targets    0.27 ms      6.1k affected (60%)
                   50k            2.06 ms     29.5k affected (60%)
                  200k           11.39 ms    119.7k affected (60%)
                    1M          116.86 ms    587.3k affected (60%)
```

The sweep starts from the highest in-degree target, so the affected fraction
stays at 60% as the graph grows and the curve is comparable across sizes.
100x the graph costs 433x the time: linear in the answer, with a constant that
degrades as the visited set outgrows cache. Sweeping `walk` from arbitrary
starting points instead produces a scatter rather than a curve, because the
cost tracks the size of the answer and not the size of the graph — which is the
point.

![splice scaling](docs/sweep-splice.svg)

```
splice, 8 dirty     99k edges      4.94 ms
packages           495k           30.15 ms
                  1.98M          157.79 ms
                  9.91M         1028.45 ms
```

100x the edges costs 208x the time — linear in the graph, as designed, and
independent of how many packages are dirty.

## Against the approach this replaces

`cambium bench bazel --repo <emitted> --base <sha> --head <sha> --legacy` runs
both paths on the same commits with a real Bazel: the incremental run, and the
"dump the whole graph at both commits and diff every target's hash" baseline it
replaces. Both sides pay the same Bazel loading costs where they need them; the
difference is that the incremental run only asks Bazel about the dirty
packages.

On the emitted `tiny` workspace the numbers are dominated by Bazel server
startup and are not informative. Run it on `medium` or larger, on a worker with
a warm Bazel server, to get a ratio meaningful for your own repository:

```console
$ cambium synth emit-bazel --spec medium --out /tmp/synth --revisions 5
$ cambium bench bazel --repo /tmp/synth --base <first> --head <last> --legacy
```

## Reproducing and gating

CI runs `small` and `medium` on every pull request and `large` nightly. Because
results carry a machine fingerprint, gating works by comparing against a
baseline captured on the same runner class:

```console
$ cambium bench compare --baseline known-good.json --current ci.json --threshold 15
 SLOWER  +22.4%  splice@medium  16.31 ms -> 19.95 ms
```

Exit status is non-zero if any stage regressed past the threshold. Improvements
are reported too, since a stage that suddenly got twice as fast usually means it
stopped doing something.
