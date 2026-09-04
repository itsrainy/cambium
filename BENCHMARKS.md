# Benchmarks

Measured numbers, and what they say about whether the design holds.

Everything below is from one machine — an **Intel Xeon @ 2.10 GHz, 4 cores,
16 GiB**, a shared cloud VM rather than a build fleet worker. The spec's
absolute targets assume 16 cores, so treat these as a lower bound on what the
design delivers, and as a baseline for regression gating rather than as
headline performance numbers. The raw results behind them are checked in
under `docs/benchmarks/`, and double as a baseline for `bench compare`.
Reproduce with:

```console
$ cargo build --release -p cambium-cli
$ ./target/release/cambium bench run --preset small,medium,large --runs 5 \
    --out bench-results/mine.json --markdown bench-results/mine.md
$ ./target/release/cambium bench compare \
    --baseline docs/benchmarks/inmemory-small-medium.json \
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
| `build_index` | 3.8 ms | 192 ms | 4.50 s |
| `load_index`, mmap only | 0.01 ms | 0.01 ms | 0.01 ms |
| `load_index`, + label map | 0.21 ms | 8.7 ms | 279 ms |
| `classify` | 1.1 ms | 31 ms | 63 ms |
| `parse_proto` | 8.1 ms | 239 ms | — |
| `rule_hash` | 4.7 ms | 141 ms | — |
| `splice`, 8 dirty packages (materialised) | 0.46 ms | 16 ms | 308 ms |
| `splice`, 42 dirty packages (delta overlay) | — | — | 32 ms |
| `reverse_rebuild` | 0.06 ms | 3.0 ms | 82 ms |
| `walk`, 10 starting points | 0.00 ms | 0.01 ms | 11 ms |
| `walk`, hub starting point | — | — | 34 ms |
| `e2e_memory`, `typical-pr` (overlay) | 3.9 ms | 22 ms | 207 ms |
| `e2e_materialize`, `typical-pr` | — | 31 ms | 541 ms |

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
| `splice` + `reverse_rebuild` on `large` | < 300 ms | 32 ms (overlay) / 308 + 82 ms (materialised) | ✅ on the path that matters |
| `load_index` on `large`, warm | < 20 ms | 0.01 ms | ✅ the label map is separate and off the walk path |
| `build_index` on `xl` | < 60 s, < 6 GiB | ~18 s, ~7 GiB (extrapolated) | time ✅, memory over |
| `e2e_memory` `typical-pr` on `xl` | < 1 s | ~0.8 s (extrapolated) | ✅ with the overlay |

On four cores. The two targets the splice used to miss are met now that the
`affected` path no longer materialises an index; `build_index` peak memory is
the one still over, and it is a cold-build cost paid once per lineage.

## Where the time actually goes

End-to-end on `large`, for a typical pull request: ten changed files, one of
them a `.bzl` that 42 packages load.

| stage | delta overlay | materialised | proportional to |
|---|---:|---:|---|
| dirty set | 40 | 40 | changed files (plus one `rbuildfiles` call) |
| query | 4 | 4 | targets in dirty packages — synthetic here; a real Bazel query dominates |
| splice | **24** | **~1200** | the change / **the whole graph** |
| direct set | 1 | 1 | dirty targets and changed files |
| walk | 45 | 40 | the answer (280k targets) |
| **total** | **207 ms** | **541 ms** | |

The splice was 52% of a run and is now 12%. Measured on its own, over 42 dirty
packages of a 2.5M-node index:

```
from_view                 8 ms     borrow the base, copy only the small tables
from_view + apply        25 ms     intern, stamp metadata, stage edges
finish_overlay            7 ms     diff replaced rows into reverse corrections
                     -------
delta overlay            32 ms

from_view + apply + finish     1216 ms    materialise a complete new index
```

Two things made that possible:

- **The builder borrows its base.** It used to copy the label table, the node
  columns and the forward CSR up front. Now existing entries are read straight
  from the mmapped index and only what the splice touches is stored, so a
  splice is proportional to the change until something asks for a whole index.
- **Labels are found by binary search.** The index carries node ids ordered by
  label (4 bytes per node), so resolving the couple of thousand labels a dirty
  query mentions costs microseconds instead of the ~280 ms an O(labels) hash
  map build costs at a few million nodes.

Which finalizer runs is the caller's choice, and the choice is not
interchangeable:

- **`affected` takes the overlay.** It answers a question and throws the graph
  away; nobody reads the index it would have built. A fetched baseline is not
  even deserialised — the run walks the mmap.
- **`baseline update` materialises.** Its output *is* the published artifact,
  and it is where "a spliced index equals a full rebuild" gets exercised, which
  two of the four correctness oracles check. Making the shared path lazy to
  speed up `affected` would take the published artifact with it, along with the
  oracle that would have caught the mistake.

The two must answer identically, and that is enforced rather than assumed:
every scenario in the property tests and every revision of the soundness sweep
computes the answer both ways and requires the affected, direct and test sets —
and the `--explain` paths — to match.

Costs elsewhere, for honesty:

- The sorted-label index adds ~470 ms to a cold build at `large` (4.0 s → 4.5 s)
  and 4% to the index on disk. A cold build happens once per lineage; the
  binary search it buys happens on every run.
- The materialised splice got *faster* too (449 ms → 308 ms), because the base
  is no longer copied twice — once into the builder, once into the output.

Two smaller things, both found by measuring rather than reading:

- The direct-set stage was rebuilding a label map over the finished index
  (340 ms at `large`) to resolve changed file paths to node ids. It now
  resolves them through the builder's interner, before the index is finalised:
  340 ms → 1 ms.
- `FixedBitSet::grow` reallocates to exactly the requested size, so growing it
  one node at a time made a cold build quadratic. It now doubles.

### What is left

Of the 207 ms an `affected` run now costs on `large`, the walk is 45 ms (the
answer is 280k targets — that is the work), the dirty set is 40 ms, and the
splice is 32 ms. The remainder is the small tables `IndexBuilder::from_view`
still copies: the package-to-nodes lists, and the package-name and rule-class
interners. At 45k packages that is tens of milliseconds and the same treatment
would remove it. It is the next thing to do here, and it is worth doing only
once something needs the last 20%.

## What an index spends its bytes on

`cambium index stats` accounts for every byte in the file. Two indexes built
straight from the generators — no Bazel needed:

```console
$ cambium synth build-index --spec large --out large.bin
$ cambium index stats large.bin
```

| component | `large` (258.6 MiB) | `xl` (1.03 GiB) |
|---|---:|---:|
| label table | 29.3% | 30.5% |
| label sort index | 3.7% | 3.6% |
| rule hashes | 29.6% | 29.1% |
| node columns | 10.2% | 10.0% |
| forward edges + offsets | 11.0% | 10.8% |
| reverse edges + offsets | 11.0% | 10.8% |
| package table | 5.1% | 5.1% |
| framing/padding | 0.0% (390 B) | 0.0% (379 B) |

### Labels: 33%, and not compressed

**Labels are 33.0% of a `large` index and 34.1% of an `xl` one** — the table
plus the sort index that `id_of` binary-searches, which is 4 bytes a node and
belongs in the same total. That is under the 40% at which it would be worth
storing labels as `(package_id, relative_name)` instead of whole strings, so
they are still whole strings.

The saving is worth stating, because it is the thing not being taken. Every
label is `//package:name`, and the node columns already carry each node's
package id, so the `//package:` prefix is stored twice and dropping it from the
label table would cost nothing extra. `index stats` reports the projection:

```
labels:            85.3 MiB   33.0% of the file (table + sort index)
  packed           47.0 MiB   18.2% would remain, storing (package, name)
  saving           38.3 MiB   14.8% of the file, 64 labels not packable
```

15% of the file, for a change that touches the format, the builder, the
overlay, every lookup path, and the two finalizers — while the external API
goes on returning full labels, so every caller-facing path grows a
concatenation. Not at 33%.

The reason to report the projection rather than delete the question: **the
threshold is about ten bytes of package path away.** These graphs have short
paths (`d3/d17/p842`, averaging 16 bytes of `//package:` prefix). A repository
whose packages look like `src/main/java/com/example/service` has labels 10-15
bytes longer, and labels are then over 40%:

| | avg `//package:` prefix | label share | crosses 40% at |
|---|---:|---:|---|
| `large` | 16.0 B | 33.0% | +12.8 B of package path |
| `xl` | 17.3 B | 34.1% | +10.9 B of package path |

The extrapolation is arithmetic — a byte added to a package path lands once in
each of its nodes' labels and once in the package name table, and nowhere else
— and `label_share_grows_with_package_path_length_as_predicted` in
`crates/engine/tests/index_queries.rs` checks that the file actually behaves
that way rather than taking the arithmetic on trust. So: run `index stats` on
an index of your own repository. If the label line is over 40% there, the
projection tells you what packing would give back before anyone writes it.

One thing the accounting turned up on the way: the sort index was 3.7% of the
file and was landing in `framing/padding`, which is why that row now reads
390 bytes instead of 9.6 MiB. Nothing changed in the format; the reported
framing was 3.7% of unattributed bytes.

`rule_hashes` at ~29% is the other large component, and it is 32 bytes per node
including the 80% of nodes that are source files and carry a zero hash. That is
a bigger saving than label packing and a much smaller change. It was not on
this list, so it is not done, but it is the first thing to look at if an index
needs to get smaller.

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
$ cambium bench compare --baseline docs/benchmarks/inmemory-small-medium.json \
    --current ci.json --threshold 15
 SLOWER  +22.4%  splice@medium  16.31 ms -> 19.95 ms
```

Exit status is non-zero if any stage regressed past the threshold. Improvements
are reported too, since a stage that suddenly got twice as fast usually means it
stopped doing something.
