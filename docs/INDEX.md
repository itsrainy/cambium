# The graph index

What cambium stores about a repository, why it is stored as compressed sparse
rows, how that differs from the graph Bazel keeps in its own head, and why
none of it amounts to reimplementing Starlark.

This is the data structure. [ENGINE_COSTS.md](ENGINE_COSTS.md) is what it costs
to build and walk; [STORE.md](STORE.md) is how the files are named, compressed
and shared.

## What is in it

An index is one file describing the target graph of the main repository at one
commit. Its nodes are labels — every target a `bazel query //...:*` reported,
plus every label those targets reference. Its edges are dependencies, pointing
from a target to what it consumes.

Seven node kinds (`crates/index/src/data.rs`): `Rule`, `SourceFile`,
`GeneratedFile`, `PackageGroup`, `EnvironmentGroup`, and two that only exist
because an index is compared against other generations of itself —

- **`Unknown`**, a label some rule references that no queried package defines.
  Every label into an external repository is one of these: a node with no
  outgoing edges, because nothing ever queried that repository.
- **`Tombstone`**, a label that existed in an earlier generation and no longer
  does. Ids are never reused, so the row stays, emptied.

Per node the index keeps a kind, a package id, a rule class id, `is_test`, a
flag byte, and a 32-byte canonical rule hash. It does **not** keep attribute
values. The hash stands in for them: the rule proto is canonicalised — sorted
attributes, positional fields cleared, defaults dropped — and hashed, so an
edit that could change a target's outputs changes the hash and an edit that
moves a line does not (`crates/cambium-proto/src/hash.rs`). Comparing two
generations is then 32 bytes per node, not a structural diff of attribute
trees.

Alongside the graph sit the tables that make a *package* the unit of
replacement — package name, its node list, the hash of its BUILD file — and
the static `load()` closure, the external-repository attribution and the path
maps. Those are the subject of the last section.

## Why compressed sparse rows

Edges are two `u32` arrays. `offsets` has one entry per node plus a terminator;
`edges` is every edge in the graph, laid out so that node `u`'s row is
`edges[offsets[u]..offsets[u+1]]`. A node id *is* its index. Both directions
are stored: `fwd_*` (what a target depends on) and `rev_*` (what depends on
it).

That buys five things, and cambium uses all five.

**A row is a slice, at any node, with no search.** The reverse walk is a BFS
over `rev`, and every step is one bounds check and a slice. The walk is
O(answer + its in-edges) and never touches the rest of the graph — which is
what "cost proportional to the size of the answer" means in practice.

**The file is the data structure.** Every column is a flat array of primitives,
so loading an index is an mmap and a handful of slice casts — no
deserialisation, no pointer fixups, no allocation proportional to the graph.
A fetched baseline is never deserialised at all; `affected` walks the mmap.
(rkyv archives primitives as little-endian `rend` wrappers, which on a
little-endian target are layout-identical to the native type; big-endian hosts
are rejected at compile time rather than silently paying a conversion.)

**Four bytes an edge, and nothing else.** Forward and reverse edges plus their
offsets are 22% of a `large` index — a graph with a few million nodes fits in
a few hundred megabytes, which is what makes shipping the graph through an
object store a reasonable thing to do at all. The `u32` ceiling is enforced:
`prefix_sum` panics rather than wrapping, because a wrapped offset array is an
index whose rows point at other rows' edges.

**Reverse edges are a counting sort away.** `Csr::reverse_of` is two passes —
count in-degrees, prefix-sum, scatter — and because the scatter walks sources
in increasing order, every reverse row comes out sorted for free. There is a
parallel variant for large graphs whose atomic scatter loses that ordering and
sorts each row afterwards.

**Rows are sorted and deduplicated, always.** That is the invariant the rest of
the crate is built on: it is what makes two index generations comparable
row-for-row, what lets `finish_overlay` diff a replaced forward row against the
one it replaced to derive reverse-edge corrections, and what a future
merge-based incremental reverse rebuild would rely on. It is also why
`sort_rows_parallel` may sort but must never drop: offsets are untouched, so
removing an edge would leave a row pointing past its own end.

Two structures beside the CSR fall out of the same "flat arrays, no maps"
discipline. Labels are a string table plus `label_sorted`, node ids ordered by
label, so a label resolves by binary search in microseconds rather than by
building an O(nodes) hash map (~280 ms at a few million nodes, on every run).
And `pkg_nodes` is itself a CSR, package id to its node ids, which is what lets
a splice replace one package's rows without touching the rest of the graph.

## How this differs from Bazel's own graph

Bazel has a target graph too. It is not this one, and the differences are all
consequences of a single split: Bazel's graph exists to *build* one state of
one workspace, and cambium's exists to be *compared against another commit*.

**Unconfigured, not configured.** What cambium indexes is the loading-phase
target graph that `bazel query` reports — one node per label. Bazel's analysis
phase works on configured targets, one node per (target, configuration), with
`select()` resolved, toolchains bound and aspects applied. Cambium never gets
there: it stores the union over every `select` branch (which is what
`rule_input` already carries, alongside the `config_setting` labels the
branches key on) and keeps `--proto:flatten_selects=false` so that the *hash*
still sees which branch was edited. So one index answers for every
configuration at once, and it over-approximates any single one.

**Materialised in both directions, not derived in one.** Bazel's target graph
is objects on the server's heap; a target's dependencies are read out of its
attribute values when something asks. Reverse dependencies are not a structure
you can index into: `rdeps` has to be handed a universe to search it in
(`rdeps(//..., //lib:base)`), and `rbuildfiles` needs Sky Query with
`--universe_scope=//...`. Either way the universe gets loaded, external
repositories included, which is why that path finishes with errors on any
machine that cannot fetch one of them — and a partial answer to "who depends on
this?" is unusable, because the package it dropped may be exactly the one that
mattered. Cambium pays for the reverse edges once, at index time, and every run
after that reads them as slices: a counting sort and 11% of the file, against a
whole-universe load per question.

**A file, not a server.** Bazel's graph lives in one process, tied to one
workspace at one state of the filesystem, and goes away when the server does.
An index is a portable artifact: it is fetched from an object store, mmapped
by a machine that may have no Bazel and no checkout, and — because the
revision lives in the file's trailer rather than in the archive — two commits
with identical graphs produce byte-identical payloads. Bazel has no notion of
"the graph at commit X" to compare with "the graph at commit Y"; that
comparison is the only thing cambium exists to do.

**Lossy on purpose.** Bazel keeps every attribute because it has to build the
thing. Cambium keeps edges, a rule class, a few bits and a hash, because all it
has to do is notice that something moved. An index is roughly two
`u32`-per-edge arrays and 32 bytes a node where Bazel holds a `Package` full of
`Target`s full of attribute maps.

**Dense ids that outlive a generation.** Bazel identifies a target by its
`Label`; ids are an implementation detail of whatever is running. Cambium's
ids are the index into every column, they are stable across a splice, and they
are never reused — hence tombstones. A deleted target keeps its row so that a
walk through a deleted dependency still has somewhere to go, and so that the
base and head of a run share one id space.

**Invalidated by the package, not by the file.** Skyframe invalidates at the
granularity of the thing that changed and re-evaluates only what depended on
it. Cambium's unit is coarser and deliberately so: a changed file dirties its
package, the package is re-queried whole (`bazel query 'set(//a:* //b:*)'`),
and the resulting rows are spliced over the old ones. Coarser is cheaper here,
because the expensive step is *starting a conversation with Bazel at all*, not
the size of the answer it gives.

**Graphs that fail to load are recorded, not fatal.** A query that finished
under `--keep_going` with packages it could not load produces an index marked
`partial`, with the error count and the first few package names in its
provenance. A cold build and an incremental update drop exactly the same
packages, so the two still agree — but a consumer comparing an index against
reality deserves to know.

## Are we reimplementing Starlark?

No, and the reason is that two different questions are being answered by two
different things.

> **Bazel answers "what are the edges?" Cambium answers "which packages do I
> have to ask Bazel about?"**

Every edge and every rule hash in an index came out of a `bazel query`. Cambium
does not evaluate Starlark, does not expand macros, does not run repository
rules, and has no opinion about what a rule produces. When a package is dirty,
Bazel re-reads it and cambium writes down what Bazel said. On that side of the
line there is no gap to close, because there is no second implementation to
diverge from.

What cambium *does* read statically is the part of Starlark that is not
computable by construction:

- **`load()`**. It is legal only as a top-level statement and its label
  argument must be a string literal — Bazel's own parser rejects anything else.
  So a lexer that understands strings and comments sees every load a file
  performs, and the transitive closure can be read off the text with no Bazel
  process and no failure mode. That is the `bzl_consumers` table: one binary
  search and one CSR row to answer "this `.bzl` changed, which packages care?",
  where the `rbuildfiles` call it replaced needed the whole universe loaded and
  fired a global invalidation on 5 of 150 commits on `bazel/bazel`.
- **The shape of `MODULE.bazel` / `WORKSPACE` and their `.bzl` closure**, to
  answer which external repositories a main-repo path feeds. That one *is* a
  real analysis — facts scanned out of a syntax tree, then named rules run to a
  fixpoint (`crates/index/src/attribution/`) — and it is still not evaluation.
  It reads `def`s, calls, `name =` arguments and string literals. A
  `name = "a" + b` is a `BinOp`, and the consumer's job is to say it cannot read
  that as a name.

### Won't there always be gaps, then?

There will always be things the static reader cannot resolve. That is designed
for, and it is why the gaps do not accumulate into wrong answers: **every
unknown widens the answer, and none of them narrows it.**

The fallbacks are uniform, at every level:

| what could not be read | what happens |
|---|---|
| a statement the parser chokes on | `Stmt::Unparsed`, its span kept, its string literals still read off the tokens; the enclosing scope takes its wide answer |
| an unparsed statement in `MODULE.bazel` | recorded in `ModuleFacts::unparsed`; a diff touching it is global |
| a load label that cannot be resolved | the `.bzl` is not localised, so the change escalates |
| a changed `.bzl` with no node in the closure | the engine re-derives the inverse of one hop of the load graph from the head checkout and follows every loader it finds — a BUILD loader dirties its package, a workspace-file loader escalates. With no checkout to witness against, the run reports `bzl_witness_unavailable` rather than claiming the check passed |
| an index with no closure at all | global — "no consumers" and "we never looked" are the same empty table, and only one of them is safe to act on, so the flag is recorded rather than inferred |
| a repository whose path mapping could not be derived | the pre-format-6 answer stands for it: every package that loads out of it is dirty, no narrowing |
| a `select`, whose branch depends on a configuration no index has | every branch's labels are edges |
| more dirty packages than `limits.max_dirty_packages` | a cold rebuild — still exact, just slower |

The worst thing any of these produces is a slow, exact run or an exit 2. The
one direction cambium is not allowed to be wrong in — an edge too few, a
package not re-queried, a target missing from the affected set — is the
direction a static reader of Starlark can only fail in if it *believes* it
understands something it does not.

Which is the actual argument against going further. A complete Starlark
evaluator would be a second implementation of Bazel's loading phase, and its
failure mode is the bad one: where the tolerant reader says "I could not read
this, take everything", an evaluator says "I read this, the answer is these
four packages" — and when it has diverged from Bazel by a version, a builtin or
a `--incompatible_` flag, that is a silently wrong answer with nothing to catch
it. Over-approximation is measured and reported; unsoundness is a bug, and the
test suite is built around proving it does not happen.

So the roadmap is not "toward full Starlark". It is toward narrowing the wide
answers one shape at a time, and the format's history is exactly that ledger:

| format | what got a narrower answer |
|---:|---|
| 4 | a changed `.bzl` — resolved from the static load closure instead of from `rbuildfiles`, which needed the whole universe |
| 5 | a `MODULE.bazel` edit, a `local_path_override` path, a workspace-closure `.bzl` — answered as "these repositories moved" rather than `//...` |
| 6 | an input-path edit to a repository whose derived path mapping shows the edit cannot be a `.bzl` the repository exports — label consumers only, no re-query |
| 7 | a changed external `.bzl` that a BUILD file loads *directly*, which the per-`.bzl` table had no row for |
| 8 | an edit to a path an external repository merely writes into a generated BUILD file, as against one its rule reads — only the repository's *targets* move, so no package's load closure is re-queried |

The same thing happens outside the format: a `.bazelrc` edit that changes a
Starlark build setting used to be global and is now resolved to the flag's own
label and its consumers, because the rc file's text says which setting moved.

Each step took a case that used to answer `//...` and gave it a bounded answer.
None of them made cambium understand Starlark better; they made it read one
more purely syntactic fact, and know what to do when it cannot read it.
