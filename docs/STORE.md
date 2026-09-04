# The baseline store

Where published indexes live, how they are named, and what the next change to
this layout should be.

## Layout

```
indexes/<lineage>/<commit>.bin
```

Lineage first. Node ids are only comparable within a lineage, so a lineage is
the unit you list, garbage-collect and retire; putting it in the path means a
store can be swept one lineage at a time with a prefix, and an abandoned
lineage is one `rm -r` or one `aws s3 rm --recursive`.

Stores that cannot be listed — a plain HTTP bucket behind a CDN, say — also get
a second key:

```
indexes/by-commit/<commit>.bin
```

because a run looking for a baseline knows the commit but not yet the lineage,
and cannot discover it without either listing or a fetch. Everything else keys
off the canonical path.

`cambi baseline list` prints keys; `cambi baseline gc` deletes those older
than `retention_days`, walking the tree.

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
content under the same key — but that cannot happen for a fixed commit, because
the payload is determined by the graph, and the graph is determined by the
commit.

## S3 and GCS

There is no S3-specific backend, and there does not need to be one: the
`command` backend covers it exactly, with no credential handling of our own and
no AWS SDK in the dependency tree. `{name}` is substituted with the store key
(`indexes/<lineage>/<commit>.bin`) and `{path}` with the local file.

```toml
[baseline]
store = { kind = "command", \
  fetch_cmd = ["aws", "s3", "cp", "--quiet", "s3://my-bucket/cambium/{name}", "{path}"], \
  push_cmd  = ["aws", "s3", "cp", "--quiet", "{path}", "s3://my-bucket/cambium/{name}"], \
  list_cmd  = ["aws", "s3", "ls", "--recursive", "s3://my-bucket/cambium/"] }
```

GCS is the same shape:

```toml
store = { kind = "command", \
  fetch_cmd = ["gsutil", "-q", "cp", "gs://my-bucket/cambium/{name}", "{path}"], \
  push_cmd  = ["gsutil", "-q", "cp", "{path}", "gs://my-bucket/cambium/{name}"], \
  list_cmd  = ["gsutil", "ls", "-r", "gs://my-bucket/cambium/"] }
```

Notes that matter in practice:

- **`fetch_cmd` must tolerate a missing object.** A non-zero exit is read as
  "not present", which is an ordinary outcome — the ancestor walk asks for many
  keys that do not exist. `aws s3 cp` exits 1 for a missing key, which is
  exactly right.
- **`list_cmd` output is read one key per line**, and only the part after the
  last `/` is kept for matching, so the verbose `aws s3 ls` format works.
  Prefer `--recursive`: the layout is nested.
- Credentials are the command's business, not cambium's. Instance roles,
  `AWS_PROFILE`, workload identity — whatever the CLI already does.
- If you want server-side encryption, a storage class, or a retention policy,
  put the flags in the template.

An `s3` backend would buy a slightly shorter config and cost an SDK, its
credential chain, and its release cadence. If one is ever added, it should be
behind a feature flag, and these templates should keep working.

## The next change: content-addressed blobs

Not implemented, deliberately, but the format is already arranged for it.

Today every commit gets its own file, even when the graph did not change. On a
busy main branch most commits touch no BUILD file and no globbed path, so most
published indexes are byte-identical to their parent's — and the payload is
identical, by construction. A repository publishing an index per merge stores
the same 260 MB over and over.

The non-breaking next step is a pointer and a blob:

```
blobs/<blake3 of payload>.bin        the index itself, written once
indexes/<lineage>/<commit>           a small pointer file: the blob hash,
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
- **The key layout does not move.** `indexes/<lineage>/<commit>` stays the
  thing a run asks for; only what is behind it changes, so a fetcher can be
  taught to follow a pointer while still handling a direct `.bin`.
- **Garbage collection stays lineage-scoped**: sweep pointers by age as now,
  then delete unreferenced blobs.

The reason to wait is that it adds a round trip to every fetch and a
reachability problem to every delete, and neither is worth paying before the
duplication is measured. `cambi baseline list` plus the payload hashes will
say how much is actually duplicated in a given repository; do that first.
