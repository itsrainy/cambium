# The index store

Where published indexes live, how to point `cambi` at them, how they are
named, what a miss is as against an outage, and the planned next change to
this layout.

## Configuring a store

The store is `[baseline].store` in `cambium.toml`, one of three kinds.
`cambi init` writes it for you from one flag: `--store-dir PATH`,
`--store-s3 BUCKET/PREFIX`, `--store-gcs BUCKET/PREFIX`, `--store-http URL`,
or `--store-none` (no store: every run builds its own index).

A directory, local or a shared mount:

```toml
[baseline]
store = { kind = "dir", path = ".cambium-store" }
```

A relative `path` is read from the workspace root (the directory holding
`MODULE.bazel` or `WORKSPACE`), not from wherever `cambi` was started, so
`cambi -C <workspace>` and a run from a subdirectory reach the same store.
`init --store-dir` writes the path as given. The directory need not exist:
the first `--publish` creates it.

A plain HTTP base URL, read with `GET` and written with `PUT`, the key
appended after a `/`:

```toml
[baseline]
store = { kind = "http", url = "https://indexes.example.com/cambium", headers = ["Authorization: Bearer <token>"] }
```

`headers` is optional, one `Name: value` string per header. The values are
used as written (no environment expansion), so a secret here is a secret
checked in; for a credential, use the command kind with a tool that reads
it from the environment. HTTP cannot list, which has a consequence for
mixed setups: see [Readers that cannot list](#readers-that-cannot-list).

A command template, for S3, GCS or anything with a CLI: see
[S3 and GCS](#s3-and-gcs) for the full shape and what `not_found_exit`
buys.

The whole `[baseline]` section is outside the config hash, so changing the
store (or pointing a CI job at another one with `--config`) retires no
index.

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
`retention_days` (see [Garbage collection](#garbage-collection)).

### Readers that cannot list

A publisher writes the lineage-less copies only when *it* cannot list. A
reader that cannot list (the `http` kind) can only fetch those copies. So a
setup where one side lists and the other does not finds nothing: an `aws
s3` (or `gsutil`, or directory) publisher writes `indexes/<lineage>/...`
only, and an `http` reader in front of the same bucket asks for
`indexes/by-tree/<tree>.bin`, misses every time, and builds cold (exit 4
under `--no-full-build`).

Set this on the publisher to make that setup work:

```toml
[baseline]
lineage_less_copies = true
```

It makes a publish to a store that can list also write
`indexes/by-tree/<tree>.bin` and `commits/by-commit/<sha>`. Each by-tree
copy holds whichever lineage published that tree last; a reader still
refuses one built with another configuration. The key is outside the config
hash, like the rest of `[baseline]`, so turning it on retires nothing, and
the copies appear from the next publish on. A store where every reader can
list does not need it.

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

The level is **3**, measured on the benchmark subjects' indexes (one
machine, release build, single-threaded; times are for one whole index):

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
  label from a BUILD file that loads `@rules_foo//:defs.bzl` directly;
  `pkg_ext` is the same table per *package*.
  **8**: a *target-only* flag per input-map pattern
  (`repo_input_target_only`), set when every derivation was a label
  embedded in generated text rather than resolved by the rule, so marking
  it direct stopped the eleven hundred packages under
  `@envoy_repo//:compiler.bzl` re-querying on every changelog edit.
  **9**: the repository boundaries of the main repository's own tree
  (`repo_boundary_*`), the directories below the root carrying a
  `WORKSPACE`, `WORKSPACE.bazel`, `MODULE.bazel` or `REPO.bazel` of their
  own. `//...` stops at them, so no package under one is ever in the index,
  and without the list a change there was read as a main-repo path and
  minted packages Bazel will not load (tensorflow's `third_party/xla`,
  which is the repository `@xla`).
  **10**: a flag per boundary (`repo_boundary_ignored`) saying whether
  `.bazelignore` names the directory. A `.bazelignore`d directory carrying a
  repository marker is now a boundary too (envoy ignores `api/`, which is
  `@envoy_api`), and the flag is what lets the engine tell a boundary Bazel
  reads but no repository rule claims (a hole in the graph, so everything is
  affected) from one Bazel never reads (nothing can depend on it, so the
  change is ignored).
  **11**: the *Skyframe loading layer* (`sky_*`; `crates/index/src/sky`),
  Bazel's own loading-phase graph read back from `bazel dump
  --skyframe=deps` on the server that just loaded the repository, stored as
  a node table ordered by `(kind, root, key)` plus a CSR of reverse edges.
  It answers "which packages did this changed path move" from the edges
  Bazel recorded, and three restatements of that question went with the
  bump: the classifier's sub-package carving rule, its glob rule, and the
  package half of the static `.bzl` load closure. Keys of other
  repositories are dropped (they are sinks for this walk, and keeping them
  put the machine's output base in the payload), except `BZL_LOAD`, because
  an external `.bzl` can be a file of this tree.
  **12**: what each external repository *is* as far as this tree is
  concerned (`repo_origin_kind`, `repo_origin_dir`,
  `repo_origin_evidence`, `repo_confirmed`, `repo_indexed`): the
  repositories whose sources are files of this tree, with the evidence for
  the claim and whether Bazel's own package list confirmed it; and per
  repository boundary of the tree, whether the cold query returned a
  main-repo package under it (`repo_boundary_traversed`), since the marker
  a directory carries does not settle that (Bazel 8.7 loads packages under
  xla's `third_party/tsl/REPO.bazel`).
  **13**: the *external-consumer table* (`repo_consumer_*`): per indexed
  repository, the external repositories whose rules depend on its targets
  and the packages of it they depend on, asked of Bazel once at cold build.
  It routes a change under envoy's `api/` to the tests that link
  `@grpc//:grpc`, whose xDS client consumes the API's protos; that leaf has
  no outgoing edges, so without the table there was no path back.
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
with no credential handling in `cambi` and no AWS SDK in the dependency
tree. `{name}` substitutes the store key (`indexes/<lineage>/<tree>.bin`, or
a `commits/...` pointer) and `{path}` the local file. Each template is a
list of arguments run directly, with no shell, from the directory `cambi`
runs in.

```toml
[baseline.store]
kind = "command"
fetch_cmd = ["aws", "s3", "cp", "--only-show-errors", "s3://my-bucket/cambium/{name}", "{path}"]
push_cmd = ["aws", "s3", "cp", "--only-show-errors", "{path}", "s3://my-bucket/cambium/{name}"]
list_cmd = ["aws", "s3", "ls", "--recursive", "s3://my-bucket/cambium/"]
not_found_exit = 1
```

GCS is the same shape:

```toml
[baseline.store]
kind = "command"
fetch_cmd = ["gsutil", "-q", "cp", "gs://my-bucket/cambium/{name}", "{path}"]
push_cmd = ["gsutil", "-q", "cp", "{path}", "gs://my-bucket/cambium/{name}"]
list_cmd = ["gsutil", "ls", "-r", "gs://my-bucket/cambium/"]
not_found_exit = 1
```

`cambi init --store-s3` and `--store-gcs` write the same shape without
`--only-show-errors` or `-q`: the tools' progress then goes to stdout,
which `cambi` does not read, and their errors to stderr as above. Read
[Misses and outages](#misses-and-outages) before relying on them in CI:
with these templates an outage reads as a miss.

In practice:

- **`fetch_cmd` must tolerate a missing object**: the ancestor walk asks for
  keys that do not exist, and a miss is an ordinary answer. A failed fetch
  that printed something on stderr is also reported as `CAMBI-W007`, with
  that text, once per run (the rest are logged at `--log warn`).
- **Keep the tool's errors on stderr**, because that text is the only sign
  a cold build came from a failing fetch. `aws s3 cp --quiet` prints nothing
  at all when it fails (aws-cli attaches no result printer under
  `--quiet`), so with it an outage is a cold build with nothing said, or
  exit 4 under `--no-full-build` with only `CAMBI-E006`. Use
  `--only-show-errors`, which hides the progress lines and keeps the
  errors, as the templates above do. `gsutil -q` only lowers the log level
  to warnings; the message it exits with still goes to stderr.
- **A real miss stays quiet when the listing works**: a store that lists
  is only asked for keys it listed. `aws s3 ls` exits 1 on a prefix that
  holds nothing, though, so a store nobody has published to reads as one
  that cannot list, and the blind fetches' `(404)` is said once as
  `CAMBI-W007`: the same notice a `dir` store gives for a directory that
  does not exist yet.
- **`not_found_exit` names the exit status meaning "no such object"**. With
  it set, every other non-zero exit is a store error (`CAMBI-E010`, exit 5)
  and the run fails instead of quietly rebuilding; without it, any non-zero
  exit is a miss. It only helps when the tool gives a miss an exit status of
  its own, which `aws s3 cp` and `gsutil cp` do not (below).
- **`list_cmd` output is read one key per line**, keeping the whole path
  from its first `indexes/` or `commits/` segment onward, with the store's
  own URL or bucket prefix stripped and directory-header or blank lines
  ignored, matching both the verbose `aws s3 ls --recursive` format
  (`<date> <time> <size> <key>`) and `gsutil ls -r`'s full `gs://` URLs. Use
  `--recursive`/`-r`: a top-level-only listing finds nothing in this nested
  layout. A listable store keeps only lineage-addressed keys, which the
  walk then finds; one that can't list gets extra lineage-less copies to
  fetch blind, since bare basenames satisfy neither. A reader's credentials
  need the list permission (`s3:ListBucket`) as well as the read one.
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

## Misses and outages

A store that holds no index for a revision is a *miss*: the run builds cold,
or exits 4 under `--no-full-build`. A store that is there and cannot be read
is an *outage*: the run fails with exit 5, which a CI job can retry. What
each backend reads as which:

| backend | miss (cold build, or exit 4) | outage (exit 5) |
|---|---|---|
| `dir` | the key's file is absent; the root directory does not exist (said once as `CAMBI-W007`: a store nobody has published to has no directory yet) | any other I/O failure on the root or a key, such as a file where a directory should be (`/dev/null/nope`), a permission refused, or a root that is a symlink to nothing, such as a cache mount that did not come up (`CAMBI-E011`) |
| `http` | `404`; also `403`, because S3 and GCS answer `403` for a missing key to a caller that may not list (logged at `--log debug` only) | any other status, `5xx` and `401` included; a refused connection, a DNS failure, or 60 seconds without a byte (`CAMBI-E010`) |
| `command` | the exit status `not_found_exit` names, or any non-zero exit when it is unset (`CAMBI-W007`, once per run, when the command printed on stderr; nothing when it printed nothing) | any other exit status or a signal when `not_found_exit` is set; a command that cannot be started (`CAMBI-E010`) |

Two consequences for the shipped templates and for HTTP:

- **`aws s3 cp` and `gsutil cp` exit 1 for every failure**, so
  `not_found_exit = 1` cannot tell a miss from an outage. In aws-cli, any
  exception during an `s3` transfer (the `HeadObject` that answers `404`
  for a missing key, a `403`, a refused connection) is recorded as one
  failed task, and any failed task makes the return code 1; its [return
  codes](https://docs.aws.amazon.com/cli/latest/userguide/cli-usage-returncodes.html)
  give 1 as "one or more Amazon S3 transfer operations failed". gsutil's
  top level exits 1 for every exception it handles. With these templates an
  S3 or GCS outage, an expired credential or a wrong bucket reads as a
  miss: a cold build, or exit 4 under `--no-full-build`, never exit 5, so
  a retry-on-5 loop never fires. What says so is `CAMBI-W007` carrying the
  tool's message, and only while the template keeps errors on stderr: with
  `aws s3 cp --quiet` the outage says nothing at all.
- **An `http` credential that is wrong reads as a miss** when the server
  answers `403`, for the same reason: a `403` is what a missing key looks
  like to a reader that may not list.

To get exit 5 for an S3 outage, make the fetch tell the two apart before it
copies. `aws s3api head-object` asks for the key alone and prints the
status it got (`(404)` for a missing key, given `s3:ListBucket`; without
that permission S3 answers `403` for a missing key too). Exit 3 for a miss
and anything else for the rest:

```toml
[baseline.store]
kind = "command"
fetch_cmd = ["sh", "-c", '''
k="cambium/$1"
if e=$(aws s3api head-object --bucket my-bucket --key "$k" 2>&1 >/dev/null); then
  exec aws s3 cp --only-show-errors "s3://my-bucket/$k" "$2"
fi
case $e in *"(404)"*) exit 3 ;; esac
printf '%s\n' "$e" >&2
exit 1
''', "sh", "{name}", "{path}"]
push_cmd = ["aws", "s3", "cp", "--only-show-errors", "{path}", "s3://my-bucket/cambium/{name}"]
list_cmd = ["aws", "s3", "ls", "--recursive", "s3://my-bucket/cambium/"]
not_found_exit = 3
```

A miss then exits 3 and says nothing; a `403`, a refused connection or a
missing credential exits 1, which `not_found_exit = 3` turns into
`CAMBI-E010` and exit 5. The same shape works for GCS with `gsutil stat`
in place of `head-object`, matching the message your `gsutil` prints for a
missing object. The `list_cmd` is the other half: when it fails, the run
reads the store as one that cannot list and fetches the lineage-less keys
blind. In a real outage those fetches fail too, and the wrapper turns that
into exit 5; a listing that fails on its own (a reader without
`s3:ListBucket`) ends as a miss, because a listing publisher does not write
the lineage-less keys.

## Garbage collection

`cambi index gc` is implemented by the `dir` backend only; for S3 or GCS,
give the prefix a lifecycle rule (expire objects some days after creation)
instead.

On a directory store it is by age and blind to lineage: every index file
and `commits/` pointer whose modification time is older than
`[baseline].retention_days` (30 by default) is deleted, whatever lineage it
belongs to, and everything younger is kept. `--keep <key>` keeps one key
whatever its age. It does not look for the newest index of the live
lineage, so a lineage nobody has published to for `retention_days` goes
entirely, and a run then builds cold once and mints a new one.

Republishing refreshes the age: a publish writes a new file and renames it
into place, even when the bytes are identical, so an index a post-merge job
keeps publishing (`cambi index update HEAD --publish` on an unchanged tree
republishes the same key) is never old enough to collect. The lineage-less
copies age the same way. `cambi doctor`'s probe objects under `.probe/` are
not index keys, and `gc` leaves them.

## The next change: content-addressed blobs

Not implemented, deliberately, but the format is already arranged for it.

Every distinct tree gets its own file today, even when the graph didn't
change. Tree keying folds the no-op cases (an empty commit, a rebase, a
squash), but on a busy main branch most commits touch no BUILD file and no
globbed path, so most published indexes are byte-identical to their
parent's. A repository publishing an index per merge stores the same bytes
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
