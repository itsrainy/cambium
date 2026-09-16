# What the engine costs

Where an `affected` run spends its time, and where an index spends its bytes.

Both are engine numbers, not benchmark results: they came from the engine's
in-memory stage timings and `cambi inspect stats` over *generated* graphs,
with no Bazel and no repository, existing to explain the design rather than
score it. What the tools do on real repositories is
[RESULTS.md](RESULTS.md); the methodology is [METHODOLOGY.md](METHODOLOGY.md).

> The in-harness micro-benchmark verbs (`stages`, `sweep`, `regress`) and
> the `synth build-index` helper that produced these numbers were retired
> with issue #14, when the harness collapsed to the single walk workload.
> The design explanation below (where a run spends its time, where an index
> spends its bytes) is still true and useful, so these numbers stand as a
> historical snapshot. A criterion bench could regenerate the stage timings
> later if a cost claim needs fresh ones; nothing in the harness produces or
> gates them today.

Everything below is from one machine (an **Intel Xeon @ 2.10 GHz, 4 cores,
16 GiB**), a shared cloud VM rather than a build fleet worker. The spec's
absolute targets assume 16 cores, so treat these as a lower bound. The raw
result files that produced these tables remain checked in under
`benchmarks/`, each recording the preset, the spec hash of the generated
graph, the tool's git sha and a machine fingerprint; they are a frozen
record now, no longer regenerated or gated against.

## Where the time actually goes

End-to-end on `large`, for a typical pull request: ten changed files, one of
them a `.bzl` that 42 packages load.

| stage | delta overlay | materialised | proportional to |
|---|---:|---:|---|
| dirty set | 40 | 40 | changed files, and the loading layer the walk touches |
| query | 4 | 4 | targets in dirty packages, synthetic here; a real Bazel query dominates |
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

- **The builder borrows its base.** It used to copy the label table, node
  columns and forward CSR up front. Now existing entries are read straight
  from the mmapped index and only what the splice touches is stored, so a
  splice is proportional to the change until something asks for a whole index.
- **Labels are found by binary search.** The index carries node ids ordered by
  label (4 bytes per node), so resolving the couple of thousand labels a
  dirty query mentions costs microseconds instead of the ~280 ms an O(labels)
  hash map build costs at a few million nodes.

Which finalizer runs is the caller's choice, and the choice is not
interchangeable:

- **`affected` takes the overlay.** It answers a question and throws the
  graph away; nobody reads the index it would have built. A fetched index is
  not even deserialised: the run walks the mmap.
- **`cambi index update` materialises.** Its output *is* the published
  artifact, and it is where "a spliced index equals a full rebuild" gets
  exercised, which two of the four correctness oracles check. Making the
  shared path lazy to speed up `affected` would take the published artifact
  with it, along with the oracle that would have caught the mistake.

The two must answer identically, enforced rather than assumed: every
scenario in the property tests and every revision of the soundness sweep
computes the answer both ways and requires the affected, direct and test
sets, and the `explain` paths, to match.

## What the loading-phase dump costs

Which packages a changed path moved is answered by a reverse walk over the
Skyframe loading layer the index carries (`crates/index/src/sky`), and that
layer is `bazel dump --skyframe=deps` taken on the server that just loaded the
repository. Two dumps a lineage step: one after the cold build's `//...:*`
query, one after the splice query, on that run's own server.

Measured on the `bazel` subject (Bazel 9.2.0, 572 packages loaded), with the
filter the engine uses (`^(PACKAGE|GLOB|FILE|DIRECTORY_LISTING|BZL_LOAD|IGNORED_)`):

| | wall | stdout | blocks |
|---|---:|---:|---:|
| the universe query that fills Skyframe | 7.7 s | | |
| the dump after it | **0.37 s** | **8.2 MB** | 17741 |

The dump is 4.6% of the load it reads, and it is text this machine parses
rather than work Bazel does twice: the graph is already in the server, and
`dump` walks it. The incremental dump is smaller in proportion to what that
run loaded, because a server only holds the packages it was asked for, which
is why the second dump needs no package filter.

What it costs to *keep* is one more section of the index. On the same
subject, `cambi inspect stats` over the head index the walk's rebuild oracle
built:

| | nodes | edges | bytes |
|---|---:|---:|---:|
| target graph | 24805 | 82217 | 3.8 MiB |
| loading layer | 16070 | 27030 | 1.1 MiB |

That is 22% of the uncompressed index, and it compresses with the rest (the
file is 777 KiB on disk). Per kind the layer is 4620 `FILE`, 4563
`FILE_STATE`, 1520 `PACKAGE_LOOKUP`, 1491 `DIRECTORY_LISTING` and as many
`DIRECTORY_LISTING_STATE`, 1255 `BZL_LOAD`, 573 `PACKAGE`, 556 `GLOB` and one
ignore node.

It buys the deletion of three models of Bazel's loading phase and the
`.bzl`-closure witness that re-read every Starlark file of the head checkout
whenever the stored closure came up short.

What it costs a run, measured as the same 40-step correctness walk on the
`bazel` subject at one seed, before and after (the cold index build once, then
one materialised step per commit, each of which takes the second dump):

| | before | after |
|---|---:|---:|
| step p50 | 1533 ms | 1865 ms |
| step p90 | 2448 ms | 2925 ms |
| cold index | 46.4 s | 54.8 s |

About 330 ms a step and eight seconds of cold build, for an answer of the same
size (tightness p50 and p90 are 1.0 either way) and the same recall.

Costs elsewhere:

- The sorted-label index adds ~470 ms to a cold build at `large` (4.0 s → 4.5 s)
  and 4% to the index on disk. A cold build happens once per lineage; the
  binary search it buys happens on every run.
- The materialised splice got *faster* too (449 ms → 308 ms), because the base
  is no longer copied twice: once into the builder, once into the output.

Two smaller things, both found by measuring rather than reading:

- The direct-set stage was rebuilding a label map over the finished index
  (340 ms at `large`) to resolve changed file paths to node ids. It now
  resolves them through the builder's interner, before the index is finalised:
  340 ms → 1 ms.
- `FixedBitSet::grow` reallocates to exactly the requested size, so growing it
  one node at a time made a cold build quadratic. It now doubles.

### What is left

Of the 207 ms an `affected` run now costs on `large`, the walk is 45 ms (the
answer is 280k targets, that is the work), the dirty set is 40 ms, the
splice is 32 ms. The remainder is the small tables `IndexBuilder::from_view`
still copies: package-to-nodes lists, and the package-name and rule-class
interners. At 45k packages that is tens of milliseconds and the same
treatment would remove it, worth doing once something needs the last 20%.

## What an index spends its bytes on

`cambi inspect stats` accounts for every byte in the file. The two indexes
below were built straight from the generators, with no Bazel needed, and
`cambi inspect stats <index>.bin` produced the accounting:

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

**Labels are 33.0% of a `large` index and 34.1% of an `xl` one** (the table
plus the sort index that `id_of` binary-searches), which is 4 bytes a node and
belongs in the same total. That is under the 40% at which it would be worth
storing labels as `(package_id, relative_name)` instead of whole strings, so
they are still whole strings.

This saving is the thing not being taken. Every label is `//package:name`,
and the node columns already carry each node's package id, so the
`//package:` prefix is stored twice and dropping it from the label table
would cost nothing extra. `cambi inspect stats` reports the projection:

```
labels:            85.3 MiB   33.0% of the file (table + sort index)
  packed           47.0 MiB   18.2% would remain, storing (package, name)
  saving           38.3 MiB   14.8% of the file, 64 labels not packable
```

15% of the file, for a change touching the format, the builder, the
overlay, every lookup path, and the two finalizers, while the external API
goes on returning full labels, so every caller-facing path grows a
concatenation. Not at 33%.

**The threshold is about ten bytes of package path away.** These graphs
have short paths (`d3/d17/p842`, averaging 16 bytes of `//package:` prefix).
A repository whose packages look like `src/main/java/com/example/service`
has labels 10-15 bytes longer, and labels are then over 40%:

| | avg `//package:` prefix | label share | crosses 40% at |
|---|---:|---:|---|
| `large` | 16.0 B | 33.0% | +12.8 B of package path |
| `xl` | 17.3 B | 34.1% | +10.9 B of package path |

The extrapolation is arithmetic: a byte added to a package path lands once in
each of its nodes' labels and once in the package name table, and nowhere else,
and `label_share_grows_with_package_path_length_as_predicted` in
`crates/engine/tests/index_queries.rs` checks that the file actually behaves
that way rather than taking the arithmetic on trust. So: run `cambi inspect stats` on
an index of your own repository. If the label line is over 40% there, the
projection tells you what packing would give back before anyone writes it.

One thing the accounting turned up on the way: the sort index was 3.7% of the
file and was landing in `framing/padding`, which is why that row now reads
390 bytes instead of 9.6 MiB. Nothing changed in the format; the reported
framing was 3.7% of unattributed bytes.

`rule_hashes` at ~29% is the other large component: 32 bytes per node
including the 80% of nodes that are source files and carry a zero hash. A
bigger saving than label packing, and a much smaller change; not on this
list, so not done, but the first thing to look at if an index needs to get
smaller.
