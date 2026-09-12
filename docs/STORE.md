# The index store

Where published indexes live, how they are named, and the planned next
change to this layout.

## Layout

```
indexes/<lineage>/<tree>.bin       the index, keyed by git tree hash
commits/<lineage>/<sha>            a pointer: the tree hash, as text
```

Node ids are only comparable within a lineage, so it's the unit you list,
garbage-collect and retire; putting it first in the path lets a store be
swept one lineage at a time by prefix, and an abandoned lineage is one
`rm -r` or `aws s3 rm --recursive`.

**Tree hash, not commit sha.** Two commits with identical content share a
tree regardless of history, so a rebased branch, squash-merge, cherry-pick or
amended message all find the index built for their twin. The client resolves
`git rev-parse <sha>^{tree}` before asking the store; the ancestor walk stops
at the first tree hit, including on rebased history where no ancestor *sha*
would match. The payload was already sha-independent (see below), so this is
a key-layout change only; the provenance trailer still records which commit
wrote the file.

The `commits/<lineage>/<sha>` pointer serves tooling with a sha and no
repository to resolve it in (a shallow clone, a service with no checkout);
it holds the tree hash and nothing else, and a run that can resolve the
tree never reads it.

Stores that cannot be listed (a plain HTTP bucket behind a CDN, say) also get
lineage-less copies of both:

```
indexes/by-tree/<tree>.bin
commits/by-commit/<sha>
```

because a run looking for a base index knows the revision but not the
lineage, and can't discover it without listing or a blind fetch. Everything
else keys off the canonical path. Keys from the earlier sha-addressed
layout (`indexes/<lineage>/<sha>.bin`, `indexes/by-commit/<sha>.bin`) are
still read, so upgrades don't orphan a warm store, but are no longer
written.

`cambi index list` prints keys; `cambi index gc` deletes those older than
`retention_days`.

## Compression at rest

Index files are written as a single zstd frame, in the store and the local
cache alike. The key and on-disk name are unchanged, and the header check,
tree keys and `FORMAT_VERSION` all describe the *decompressed* bytes:
compression is outside the format, so toggling it or its level bumps
nothing.

**Old raw indexes stay readable**: the reader checks the first four bytes,
decompressing on zstd's magic (`28 B5 2F FD`) and otherwise memory-mapping
and parsing in place as before, so a store warmed before this change keeps
serving with nothing to republish.

The level is **3**, measured on the corpus indexes (this machine, release
build, single-threaded; times are for one whole index):

| subject | raw | zstd 3 | zstd 9 | zstd 19 |
|---|---|---|---|---|
| apollo | 4.21 MB | 1.11 MB (3.8x, 19 ms) | 1.03 MB (4.1x, 54 ms) | 0.86 MB (4.9x, 1.21 s) |
| bazel | 4.04 MB | 0.67 MB (6.0x, 11 ms) | 0.61 MB (6.7x, 38 ms) | 0.48 MB (8.4x, 1.12 s) |
| envoy | 7.31 MB | 1.21 MB (6.1x, 19 ms) | 1.13 MB (6.5x, 68 ms) | 0.90 MB (8.1x, 2.57 s) |
| sourcegraph | 8.98 MB | 2.21 MB (4.1x, 45 ms) | 2.08 MB (4.3x, 112 ms) | 1.63 MB (5.5x, 3.89 s) |
| xla | 7.78 MB | 1.41 MB (5.5x, 61 ms) | 1.31 MB (6.0x, 139 ms) | 1.03 MB (7.5x, 3.10 s) |

About 4-6x for tens of milliseconds. Level 9 buys another 5-11% for two to
three times the time, cheap and defensible if egress starts mattering. Level
19 buys 20-38% for sixty to a hundred times the time (1.1 to 3.9 s) on an
index a publish already paid minutes of Bazel for: real latency on every
`cambi index update`, worse as the index grows. Decompression costs 5-16 ms
at any level, so a fetch pays essentially nothing and the level only trades
write latency for bytes.

The frame carries zstd's content checksum. The rkyv archive inside is cast
in place and never validated (what makes loading O(1)), and one flipped bit
in a compressed stream corrupts everything after it, so the checksum
catches a damaged file before parsing; a blob that won't decompress is
skipped by the ancestor walk like an unreadable format version, one key out
of many.

`baseline.compress_indexes = false` writes the raw form instead. It is
outside the config hash, since it says how the file is stored, not what the
graph is, so flipping it neither retires a warm store nor mints a lineage.

## What makes an index incompatible

Three separate things, not interchangeable:

- **`FORMAT_VERSION`** (`crates/index/src/format.rs`) versions the *file
  layout*. A bump makes older files unreadable (`index show` can still name
  their version); bump only when the bytes move.
  **4**: the static `.bzl` load closure (path table, direct loads, the
  inverted `bzl -> packages` closure, workspace closure), replacing
  `rbuildfiles`; an older index lacks it and treats every `.bzl` change as
  global.
  **5**: the *external-repository input map*
  (`crates/index/src/externals.rs`), apparent repository names with
  canonical spellings, main-repo path patterns mapped to the repositories
  built from each, and a row per workspace-closure `.bzl` naming what it
  defines. Lets an edit to `.github/config.yml`, `api/**` or `MODULE.bazel`
  name the repositories it moved rather than invalidate the world.
  **6**: each repository's *path mapping* (`repo_map_*`, entries of
  `cambium_index::externals::PathMap`), saying where a main-repo directory
  or file lands inside it (e.g. `api/` maps into `@envoy_api`), so an edit
  to a file a repository is *built from* is distinguished from one to a
  `.bzl` it *exports*. Derived only from unambiguous shapes
  (`local_path_override`, `local_repository` / `new_local_repository`, a
  `repository_ctx.symlink` with a literal main-repo source); an entry-less
  repository keeps the old, wider answer.
  **7**: the external labels each BUILD file loads directly (`pkg_ext_*`),
  since `bzl_ext` recorded such labels only for `.bzl` files, dropping a
  label from a BUILD file that loads `@rules_foo//:defs.bzl` directly
  (issue #4); `pkg_ext` is the same table per *package*.
  **8**: a *target-only* flag per input-map pattern
  (`repo_input_target_only`), set when every derivation was a label
  embedded in generated text rather than resolved by the rule, so marking
  it direct stopped the eleven hundred packages under
  `@envoy_repo//:compiler.bzl` re-querying on every changelog edit (issue
  #3).
  At each bump, an empty table in an older index would otherwise misread as
  "no mapping needed" rather than "never looked".

  The doc comment on `FORMAT_VERSION` is authoritative if this summary
  disagrees with it.
- **`config_hash`**, stamped into the index header from
  `cambium_engine::Config::hash`, versions the *meaning of the rows*: what a
  hash or edge means, without moving the layout. An index whose
  `config_hash` differs from the running config is refused for splicing and
  mints a new lineage; the file stays readable. Only what decides an
  index's *content* is hashed. `[baseline]` (store, `max_ancestor_walk`,
  `retention_days`, cache directory) and `[limits]` say where and how an
  index is found, not what's in it, so changing them retires nothing.
- **`lineage`**, a random 16 bytes minted per full build, is what makes node
  ids comparable; a consequence of the other two, never bumped by hand.

`Config::hash` also mixes in [`cambium_proto::RULE_HASH_VERSION`]
(`crates/cambium-proto/src/hash.rs`). Dropping `generator_location` (a
macro's BUILD line that shifts whenever a line is inserted above it, which
bazel-diff and Tango both ignore) took that constant to **2**; it now stands
at **4** (per-version history on its own doc comment). An index from an
older cambium carries the old `config_hash`, can never splice against rows
hashed the new way, and rebuilds instead. Any future change to
`canonical_rule` bumps the same constant.

## Writes are idempotent, and concurrent writers are safe

An index file is a 64-byte prefix, a payload, and a trailer. The **payload is
a pure function of the graph**: provenance (which commit, and when it was
built) lives in the trailer, skipped by the serialiser rather than merely
unset. Two workers computing the same index at the same commit write
byte-identical files, so concurrent publishing is safe, not merely tolerated:

- The **directory** backend writes a temp file and renames it into place.
  Rename is atomic on POSIX, so a reader sees the previous object or a
  complete new one, never a partial write; each writer's temp name carries a
  process id *and* a per-call counter, so two threads in one process don't
  rename each other's file away (an earlier pid-only version failed this
  under test).
- The **HTTP** and **command** backends do whole-object writes: S3 and GCS
  object PUTs are atomic and last-writer-wins, and since writers write the
  same bytes, "last" doesn't matter.

What is *not* safe, in any backend, is two writers publishing **different**
content under the same key, but a fixed tree always yields the same
payload, so that can't happen; only the provenance trailer differs,
recording whichever commit wrote last.

## S3 and GCS

No S3-specific backend exists or is needed: `command` covers it exactly,
with no credential handling of our own and no AWS SDK in the dependency
tree. `{name}` substitutes the store key (`indexes/<lineage>/<tree>.bin`, or
a `commits/...` pointer) and `{path}` the local file.

```toml
[baseline]
store = { kind = "command", \
  fetch_cmd = ["aws", "s3", "cp", "--quiet", "s3://my-bucket/cambium/{name}", "{path}"], \
  push_cmd  = ["aws", "s3", "cp", "--quiet", "{path}", "s3://my-bucket/cambium/{name}"], \
  list_cmd  = ["aws", "s3", "ls", "--recursive", "s3://my-bucket/cambium/"], \
  not_found_exit = 1 }
```

GCS is the same shape:

```toml
store = { kind = "command", \
  fetch_cmd = ["gsutil", "-q", "cp", "gs://my-bucket/cambium/{name}", "{path}"], \
  push_cmd  = ["gsutil", "-q", "cp", "{path}", "gs://my-bucket/cambium/{name}"], \
  list_cmd  = ["gsutil", "ls", "-r", "gs://my-bucket/cambium/"], \
  not_found_exit = 1 }
```

In practice:

- **`fetch_cmd` must tolerate a missing object**: a non-zero exit reads as
  "not present" (the ancestor walk asks for many missing keys). `aws s3 cp`
  exits 1 for a missing key, exactly right; stderr output logs the miss at
  `warn` rather than `debug`, since a silent fallback to a cold build is how
  an expired credential hides.
- **`not_found_exit` names the exit status meaning "no such object"**. With
  it set, every other non-zero exit is a store error and the run fails
  instead of quietly rebuilding; without it, any non-zero exit is a miss.
  `aws s3 cp` and `gsutil cp` both use 1.
- **`list_cmd` output is read one key per line**, keeping the whole path
  from its first `indexes/` or `commits/` segment onward, with the store's
  own URL or bucket prefix stripped and directory-header or blank lines
  ignored, matching both the verbose `aws s3 ls --recursive` format
  (`<date> <time> <size> <key>`) and `gsutil ls -r`'s full `gs://` URLs. Use
  `--recursive`/`-r`: a top-level-only listing finds nothing in this nested
  layout. A listable store keeps only lineage-addressed keys, which the
  walk then finds; one that can't list gets extra lineage-less copies to
  fetch blind, since bare basenames satisfy neither.
- Fetched indexes land in the local cache directory (`<tmp>/cambium-cache`
  by default, `baseline.cache_dir`, or `--cache-dir`), each fetch written
  under a temporary name and renamed into place, so an interrupted fetch
  leaves nothing behind to be mistaken for a complete one.
- Credentials are the command's business: instance roles, `AWS_PROFILE`,
  workload identity, whatever the CLI does.
- For encryption, a storage class, or a retention policy, put the flags in
  the template.

An `s3` backend would buy a slightly shorter config at the cost of an SDK,
its credential chain, and its release cadence; if added, keep it behind a
feature flag so these templates still work.

## The next change: content-addressed blobs

Not implemented, deliberately, but the format is already arranged for it.

Every distinct tree gets its own file today, even when the graph didn't
change. Tree keying folds the no-op cases (an empty commit, a rebase, a
squash), but on a busy main branch most commits touch no BUILD file and no
globbed path, so most published indexes are byte-identical to their
parent's. A repository publishing an index per merge stores the same 260 MB
over and over.

The non-breaking next step is a pointer and a blob:

```
blobs/<blake3 of payload>.bin        the index itself, written once
indexes/<lineage>/<tree>             a small pointer file: the blob hash,
                                     plus the provenance for that commit
```

Publishing becomes: hash the payload, `PUT` the blob if not already there,
then `PUT` the pointer. Fetching becomes: read the pointer, fetch the blob
(cached locally by hash, since it'll be asked for again), and read
provenance from the pointer rather than the trailer. It's clean because:

- **The payload is already stable** (the trailer split bought that);
  `identical_graphs_at_different_commits_have_identical_payloads` pins it.
- **The provenance is already separate**, so a pointer can carry it without
  the blob knowing which commits reference it.
- **The key layout doesn't move**: `indexes/<lineage>/<tree>` stays the
  thing a run asks for, so a fetcher learns to follow a pointer while still
  handling a direct `.bin`; the `commits/` pointers are already that shape.
- **Garbage collection stays lineage-scoped**: sweep pointers by age as now,
  then delete unreferenced blobs.

The reason to wait: a round trip on every fetch and a reachability problem
on every delete, neither worth paying before `cambi index list` plus the
payload hashes measure how much is actually duplicated.
