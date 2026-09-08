# The baseline store

Where published indexes live, how they are named, and what the next change to
this layout should be.

## Layout

```
indexes/<lineage>/<tree>.bin       the index, keyed by git tree hash
commits/<lineage>/<sha>            a pointer: the tree hash, as text
```

Lineage first. Node ids are only comparable within a lineage, so a lineage is
the unit you list, garbage-collect and retire; putting it in the path means a
store can be swept one lineage at a time with a prefix, and an abandoned
lineage is one `rm -r` or one `aws s3 rm --recursive`.

**Tree hash, not commit sha.** Two commits with identical content have the same
tree whatever their history, so keying by tree means a rebased branch, a
squash-merge, a cherry-pick or an amended message all find the index built for
their twin. (Uber's Tango keys its cache the same way, for the same reason.)
The client resolves `git rev-parse <sha>^{tree}` before asking the store, and
the ancestor walk resolves each ancestor's tree and stops at the first hit —
which now also hits on rebased history, where no ancestor *sha* would have
matched. The index payload was already sha-independent (see below), so this is
a key-layout change only; the provenance trailer still records which commit
wrote the file.

The `commits/<lineage>/<sha>` pointer exists for tooling that has a sha and no
repository to resolve it in — a shallow clone, a service without a checkout.
It holds the tree hash and nothing else. A run that can resolve the tree never
reads it.

Stores that cannot be listed — a plain HTTP bucket behind a CDN, say — also get
lineage-less copies of both:

```
indexes/by-tree/<tree>.bin
commits/by-commit/<sha>
```

because a run looking for a baseline knows the revision but not yet the
lineage, and cannot discover it without either listing or a fetch. Everything
else keys off the canonical path. Keys from the earlier sha-addressed layout
(`indexes/<lineage>/<sha>.bin`, `indexes/by-commit/<sha>.bin`) are still read
so an upgrade does not orphan a warm store; they are no longer written.

`cambi baseline list` prints keys; `cambi baseline gc` deletes those older
than `retention_days`, walking the tree.

## What makes an index incompatible

Three separate things, and they are not interchangeable:

- **`FORMAT_VERSION`** (`crates/index/src/format.rs`) versions the *file
  layout*. A bump makes older files unreadable — `index inspect` can still name
  their version, nothing else can touch them. Bump it only when the bytes move.
  It went to **4** when the payload gained the static `.bzl` load closure — a
  `.bzl` path table plus three flat sections over it (direct loads, the
  inverted `bzl -> packages` closure, and the workspace closure), which is what
  replaced the `rbuildfiles` call. An index written before that carries no
  closure and a run over one would have to treat every `.bzl` change as global,
  so the layout bump is also what stops that answer being given silently.
  It went to **5** when the payload gained the *external-repository input map*
  (`crates/index/src/externals.rs`): a table of apparent repository names with
  their canonical spellings, a table of main-repo path patterns with the
  repositories built from each, and a row per workspace-closure `.bzl` naming
  the repositories that `.bzl` defines. Together they are what lets an edit to
  `.github/config.yml`, to `api/**` or to `MODULE.bazel` name the repositories
  it moved instead of invalidating the world. The same argument applies as at
  4: an index written before the bump has no map, and a run over one cannot
  tell "this file feeds no repository" from "we never looked".
  It went to **6** when each repository gained its *path mapping*
  (`repo_map_*`): the entries of `cambium_index::externals::PathMap`, grouped
  by repository, saying where a main-repo directory or file lands inside it —
  `api/` is the root of `@envoy_api`, so `api/envoy/x.proto` is
  `envoy/x.proto` in the repository. That is what tells an edit to a file the
  repository is *built from* apart from an edit to a `.bzl` the repository
  *exports*, and so stops one `.proto` under `api/` re-querying every package
  whose load closure reaches `@envoy_api//bazel:api_build_system.bzl`. The
  mapping is derived only from shapes that admit no guessing —
  `local_path_override`, `local_repository` / `new_local_repository`, and a
  `repository_ctx.symlink` whose source is a literal main-repo path or label —
  and a repository with no entries keeps the old, wider answer.
  It went to **7** when each input-map pattern gained a *target-only* flag
  (`repo_input_target_only`): set when every derivation of the pattern was a
  label embedded in text the repository rule writes — envoy's `bazel/repo.bzl`
  generates a BUILD file listing `@envoy//changelogs` in a `genrule`'s `tools`
  — rather than a label the rule resolves. Such a file is a dependency of the
  repository's targets and cannot move a `.bzl` in it, so an edit to it marks
  the label consumers direct and re-queries nothing; without the flag, every
  changelog entry re-queried the eleven hundred packages whose closure reaches
  `@envoy_repo//:compiler.bzl`. As at 4, 5 and 6,
  an empty table in an older index would read as "no mapping is needed" rather
  than "we never looked", which is the unsound direction, so the layout bump is
  what stops that answer being given silently.
- **`config_hash`**, stamped into the index header from
  `cambium_engine::Config::hash`, versions the *meaning of the rows*. An index
  whose `config_hash` differs from the running config is refused for splicing
  and the run mints a new lineage; the file stays readable. This is the knob for
  anything that changes what a hash or an edge means without moving the layout.
- **`lineage`**, a random 16 bytes minted per full build, is what makes node
  ids comparable. It is a consequence of the other two, never bumped by hand.

`Config::hash` therefore covers more than `cambium.toml`: it mixes in
[`cambium_proto::RULE_HASH_VERSION`], the version of the canonical rule
hashing in `crates/cambium-proto/src/hash.rs`. Dropping `generator_location`
from the canonical rule (a macro's BUILD line, which shifts whenever a line is
inserted above it, and which bazel-diff and Tango both ignore) took that
constant to **2**. Every index published by an older cambium has the old
`config_hash`, so it can never be spliced against rows hashed the new way —
the run says so and rebuilds. Any future change to `canonical_rule` bumps the
same constant.

## Writes are idempotent, and concurrent writers are safe

An index file is a 64-byte prefix, a payload, and a trailer. The **payload is a
pure function of the graph** — provenance (which commit, and when it was built)
lives in the trailer, skipped by the serialiser rather than merely unset. Two
workers that computed the same index at the same commit therefore write
byte-identical files.

Given that, concurrent publishing is safe rather than merely tolerated:

- The **directory** backend writes a temp file in the destination directory and
  renames it into place. Rename is atomic on POSIX, so a reader sees either the
  previous object or a complete new one, never a partial write. Each writer's
  temp name carries a process id *and* a per-call counter, so two threads in one
  process do not rename each other's file away. (There is a test for that; the
  first version of it, with only a pid, failed.)
- The **HTTP** and **command** backends do whole-object writes. S3 and GCS
  object PUTs are atomic and last-writer-wins, and since the writers are writing
  the same bytes, "last" does not matter.

What is *not* safe, in any backend, is two writers publishing **different**
content under the same key — but that cannot happen for a fixed tree, because
the payload is determined by the graph, and the graph is determined by the
tree. Two commits with the same tree write the same payload under the same
key; only the provenance trailer differs, and it records whichever commit
wrote last.

## S3 and GCS

There is no S3-specific backend, and there does not need to be one: the
`command` backend covers it exactly, with no credential handling of our own and
no AWS SDK in the dependency tree. `{name}` is substituted with the store key
(`indexes/<lineage>/<tree>.bin`, or a `commits/...` pointer) and `{path}` with the local file.

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

Notes that matter in practice:

- **`fetch_cmd` must tolerate a missing object.** A non-zero exit is read as
  "not present", which is an ordinary outcome — the ancestor walk asks for many
  keys that do not exist. `aws s3 cp` exits 1 for a missing key, which is
  exactly right. If the command wrote anything to stderr, the miss is logged at
  `warn` rather than `debug`, because a silent fall back to a cold build is how
  an expired credential hides.
- **`not_found_exit` names the exit status that means "no such object"**, when
  the command has one. With it set, every *other* non-zero exit is a store
  error and the run fails instead of quietly rebuilding from scratch; without
  it, any non-zero exit is a miss. `aws s3 cp` and `gsutil cp` both use 1, so
  `not_found_exit = 1` is the right setting for both templates above.
- **`list_cmd` output is read one key per line**, and the key kept is the whole
  path from its first `indexes/` or `commits/` segment onward — the store's own
  URL or bucket prefix is stripped, and lines that are directory headers or
  blank are ignored. That is what makes the verbose `aws s3 ls --recursive`
  format (`<date> <time> <size> <key>`) and `gsutil ls -r`'s full `gs://` URLs
  both work. Use `--recursive`/`-r`: the layout is nested, and a listing that
  stops at the top level finds nothing.

  Listing matters because it decides what gets published: against a store that
  can list, cambium keeps only the lineage-addressed keys and lets the walk
  find them, and against one that cannot it writes extra lineage-less copies to
  fetch blind. A listing that returned bare basenames would satisfy neither —
  nothing would ever be found.
- Fetched indexes land in the local cache directory — `<tmp>/cambium-cache` by
  default, `baseline.cache_dir` in `cambium.toml`, or `--cache-dir` on the
  command line. Each fetch is written under a temporary name and renamed into
  place, so a concurrent reader never opens a half-transferred index, and an
  interrupted fetch leaves nothing behind to be mistaken for one.
- Credentials are the command's business, not cambium's. Instance roles,
  `AWS_PROFILE`, workload identity — whatever the CLI already does.
- If you want server-side encryption, a storage class, or a retention policy,
  put the flags in the template.

An `s3` backend would buy a slightly shorter config and cost an SDK, its
credential chain, and its release cadence. If one is ever added, it should be
behind a feature flag, and these templates should keep working.

## The next change: content-addressed blobs

Not implemented, deliberately, but the format is already arranged for it.

Today every distinct tree gets its own file, even when the graph did not
change. Tree keying already folds the no-op cases — an empty commit, a rebase,
a squash — but on a busy main branch most commits touch no BUILD file and no
globbed path, so most published indexes are still byte-identical to their
parent's — and the payload is identical, by construction. A repository
publishing an index per merge stores the same 260 MB over and over.

The non-breaking next step is a pointer and a blob:

```
blobs/<blake3 of payload>.bin        the index itself, written once
indexes/<lineage>/<tree>             a small pointer file: the blob hash,
                                     plus the provenance for that commit
```

Publishing becomes: hash the payload, `PUT` the blob if it is not already
there, then `PUT` the pointer. Fetching becomes: read the pointer, fetch the
blob (cache it locally by hash — the same blob will be asked for again), and
read provenance from the pointer rather than from the file's trailer.

Why it is a clean change:

- **The payload is already stable.** That is what the trailer split bought, and
  `identical_graphs_at_different_commits_have_identical_payloads` pins it.
- **The provenance is already separate**, so a pointer can carry it without the
  blob knowing which commits reference it.
- **The key layout does not move.** `indexes/<lineage>/<tree>` stays the
  thing a run asks for; only what is behind it changes, so a fetcher can be
  taught to follow a pointer while still handling a direct `.bin` — and the
  `commits/` pointers are already that shape.
- **Garbage collection stays lineage-scoped**: sweep pointers by age as now,
  then delete unreferenced blobs.

The reason to wait is that it adds a round trip to every fetch and a
reachability problem to every delete, and neither is worth paying before the
duplication is measured. `cambi baseline list` plus the payload hashes will
say how much is actually duplicated in a given repository; do that first.
