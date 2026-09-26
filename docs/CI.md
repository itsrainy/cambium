# cambi in CI

How to run `cambi affected` in a pull-request pipeline: how the index gets
from one runner to the next, the two jobs that keep it there and read it,
the exit codes a job branches on, and what goes wrong and how it heals.
[docs/ci/](ci/) has the two GitHub Actions workflows this page describes,
ready to copy; [the generic script](#any-other-ci) is the same thing for
any CI.

The workflows are under `docs/ci/`, not `.github/workflows/examples/`,
because they are templates for your repository: this repository does not
run `cambi` on itself, and a directory under `.github/workflows/` reads as
its own CI (GitHub runs only the files directly in `.github/workflows/`,
but a reader looking for this repository's CI would find them there).

## What CI needs

- **One `cambi` version in every job.** An index is only spliced onto by
  the `cambi` that could have written it (see [Lineage
  breaks](#lineage-breaks)), so the post-merge job and the pull-request
  job install the same pinned version.
- **History.** The pull-request job needs the commit its change lands on,
  and the first-parent history behind it for the ancestor walk
  ([Clone depth](#clone-depth)).
- **A store both jobs reach**: the post-merge job writes it, pull-request
  jobs only read it ([Choosing a store](#choosing-a-store)).
- **Two jobs.** A post-merge job on the default branch keeps an index at
  its tip; a pull-request job asks what the change affects and tests
  that.

## How the index moves between runners

An index is one file: the target graph and Bazel's loading-phase graph at
one commit. A job publishes it to the store and a later job, on another
machine, fetches it.

- **Store keys are tree hashes.** An index lives at
  `indexes/<lineage>/<tree>.bin`, where `<tree>` is the commit's git tree
  hash (`git rev-parse <sha>^{tree}`). Two commits with the same content
  share it, so a squash merge, a rebase or a merge commit whose tree
  matches finds the index built for its twin. [docs/STORE.md](STORE.md)
  has the layout.
- **A lineage** is the chain of indexes that started from one cold build.
  An incremental update carries its lineage forward; a cold build (the
  first one, or one after a [break](#lineage-breaks)) mints a new one.
  Every index also carries the hash of the `cambium.toml` that built it
  (`cambi config hash`), and a run only splices onto an index whose hash
  matches its own.
- **The ancestor walk.** `cambi affected BASE..HEAD` looks for an index at
  `BASE`. When there is none it walks back up to
  `[baseline].max_ancestor_walk` first-parent commits (20 by default),
  takes the nearest index it can use, and replays it forward to `BASE` in
  one update. When nothing within the walk can be used it builds `BASE`
  cold, or, under `--no-full-build`, exits 4. A replay or cold build that
  has to query Bazel runs in a second worktree at that revision, with its
  own Bazel server and output base, which is the expensive case; a replay
  over commits that dirtied no package queries nothing.
- **The cache key**, when the store is a directory carried by
  `actions/cache`, is a different thing from the store key: the cache
  entry holds the whole store directory, saved by the post-merge job under
  `cambium-store-<sha>-<attempt>` and restored by the prefix
  `cambium-store-`, which takes the newest entry. The ancestor walk inside
  the store does the rest.
- **`--publish-base`** makes `affected` publish the base index it had to
  compute, so the next run on the same base finds it. That is a write, so
  it belongs only on a job trusted to write the store ([Who may
  write](#who-may-write)), never on a pull-request job. With
  `actions/cache` it would also go nowhere useful: what a pull-request run
  saves is visible to later runs of that pull request only.

## The post-merge job

On every push to the default branch:

```console
$ cambi index update HEAD --publish
```

That is the update `<parent>..<merge>` with the lookup starting at the
merge commit itself: a rerun finds the index its first attempt published
and republishes the same key, a push of several commits is replayed in one
pass, and on an empty store it builds the merge commit cold. The spelled
out form, `cambi index update HEAD^..HEAD --publish`, publishes the same
key when the parent's lineage is in the store.

What it costs depends on the merge. On a clone of `bazelbuild/rules_shell`
(71 targets) on a laptop, a merge that changed one test script took 0.7 s
(no package dirtied, so no Bazel query), and the first run on an empty
store, a cold `bazel query //...:*`, took 9.0 s.

**The first run.** The workflow's first run on an empty store is the cold
build; nothing else is needed. From a machine that can write the store,
two other ways seed it:

```console
$ cambi profile --commits 30 --out ./cambium-index.bin   # also measures the last 30 commits
$ cambi index publish ./cambium-index.bin
$ cambi index build --publish                            # the cold build alone
```

A profile ends at `HEAD`, so run it on the default branch's tip. With the
`actions/cache` store the store lives in the cache, which only a workflow
can write, so its first run is the way.

**Weekly, a fresh lineage.** Splicing leaves tombstones for deleted
targets in the lineage; a scheduled `cambi index build --publish` starts a
clean one. The next runs find it by tree like any other index.

**Retention.** On a directory store, `cambi index gc` deletes every index
file and pointer older than `[baseline].retention_days`, by modification
time and whatever its lineage. A publish rewrites the file, so the index
the job just published is never old enough to go. `gc` is implemented for
the `dir` backend only; an S3 or GCS store gets a lifecycle rule instead.

The workflow: [docs/ci/cambium-index.yml](ci/cambium-index.yml).

## The pull-request job

On `pull_request`, GitHub checks out a merge commit of the change onto the
base branch: `HEAD` is that commit and `HEAD^1` the base branch tip it
merged onto. The job asks what `HEAD^1..HEAD` affects:

```sh
for try in 1 2 3; do
  rc=0
  cambi affected HEAD^1..HEAD --tests-only --no-full-build --format labels \
    > targets.txt || rc=$?
  [ "$rc" = 5 ] || break
  sleep 20
done
case $rc in
  0) if [ -s targets.txt ]; then bazel test --target_pattern_file=targets.txt
     else echo "no affected tests"; fi ;;
  2) bazel test --target_pattern_file=targets.txt ;; # global: the file holds //...
  4|5) bazel test //... ;; # no index within reach, or the store stayed down
  *) exit "$rc" ;;
esac
```

`--format labels` prints one label per line and nothing else, which
`bazel test --target_pattern_file` reads as it is. The `|| rc=$?` keeps a
`set -e` shell (a GitHub Actions `run:` step is one) from stopping before
the `case` reads the code. What each exit code means here:

| exit | meaning | the job |
|---:|---|---|
| 0 | an answer; the file lists the affected tests, and may be empty | test the file; an empty file is skipped, because `bazel test` with no targets fails ("No test targets were found") |
| 2 | the change reached everything (a `.bazelrc` flag, a toolchain, an invalidator; `CAMBI-W004`, `CAMBI-W023`, `CAMBI-W013` and the rest say which) | the file holds `//...`; test it |
| 4 | no usable index within the ancestor walk, and `--no-full-build` said not to build one | test `//...`, or drop `--no-full-build` to build the base index cold on the runner (exact, and it pays a cold query and a second Bazel server) |
| 5 | the store is there and could not be read | retry; if it stays down, test `//...` |
| 1, 3 | an error, or Bazel failed | fail the job |

Two details the script relies on:

- **A usage error clap rejects exits 2 too.** A mistyped flag prints
  clap's usage text and exits 2 with nothing on stdout. Running the file
  on exit 2, rather than `//...`, makes that a failure: the file is empty,
  and `bazel test --target_pattern_file` on an empty file exits non-zero.
- **Exit 5 happens only where a store can tell an outage from a miss**: a
  directory store whose path is a file or refused (`CAMBI-E011`), an HTTP
  store answering `5xx` or `401` or not answering (`CAMBI-E010`), and a
  command store whose fetch exits with something other than its
  `not_found_exit` (`CAMBI-E010`). The shipped `aws s3 cp` and `gsutil cp`
  templates exit 1 for every failure, so with them an outage reads as a
  miss and ends as exit 4, never 5; [docs/STORE.md](STORE.md#misses-and-outages)
  has a fetch wrapper that tells the two apart.

The workflow: [docs/ci/affected-tests.yml](ci/affected-tests.yml).

### Clone depth

`actions/checkout` fetches one commit by default, and that is not enough:

| checkout | what `cambi affected HEAD^1..HEAD` does |
|---|---|
| `fetch-depth: 1` (the default) | `HEAD^1` is not in the clone: `CAMBI-E003`, exit 1 |
| `fetch-depth: 2` | `HEAD^1` is there; the run finds an index only if the post-merge job indexed `HEAD^1` itself, and otherwise exits 4 (`none within 0 ancestors`), since the clone has no history for the walk |
| `fetch-depth: 2`, then `git fetch --no-tags --deepen=20 origin` | the walk has 20 commits behind `HEAD^1`: an index the post-merge job left a few merges back is found and replayed forward |
| `fetch-depth: 0` (the workflow) | everything |

The other spelling of the base is the branch by name. A shallow clone
fetched by bare name can leave the branch only in `FETCH_HEAD`, which is
why `CAMBI-E003`'s help names a refspec:

```console
$ git fetch --no-tags --depth=20 origin main:refs/remotes/origin/main
$ cambi affected origin/main..HEAD --tests-only --no-full-build --format labels
```

Prefer `HEAD^1` on a merge commit: `origin/main` fetched during the job
can already be ahead of the commit GitHub merged onto, and the range then
starts from a different commit than the one the change was merged with.
With no range at all, `cambi affected` compares from the merge base of
`[git].default_branch` with `HEAD`, using `origin/<branch>` when the clone
has no local branch; in a shallow clone that merge base may be missing
(`CAMBI-E013`, whose help says to deepen).

## Lineage breaks

A break is every stored index becoming unusable for a run at once. Three
things cause one:

- **A `cambium.toml` edit to anything but `[baseline]`, `[git]` and
  `[limits]`.** Those three say where and how an index is found; the rest
  says what is in it, and is hashed.
- **A `cambi` upgrade that changes the rule hash or the index content
  revision** (same effect, `CAMBI-W003`), or **the file format**, which
  makes every stored file unreadable to the new binary: the run skips
  each with a log line (`skipping an index this build cannot read`), and
  `CAMBI-W018` is what `status`, `doctor` and `--dry-run` say about an
  index file of an older format they open.
- **Nothing in the store within the ancestor walk**, which is the same
  outcome with nothing refused: a first run, or a default branch that ran
  more than `max_ancestor_walk` merges without a post-merge job.

What the pull-request job sees: a `CAMBI-W003` warning per stored index it
skipped, then, under `--no-full-build`, `CAMBI-E006` and exit 4, and the
job tests `//...`. Without the flag the run builds the base cold instead.
`cambi status` on the same checkout also shows `CAMBI-W019`, naming the
store's lineage that this configuration cannot use. A pull request that
edits `cambium.toml` itself always sees this, because its own
configuration is not the one main's indexes were built with, and testing
`//...` is the right answer for it.

How it heals: once the edit (or the upgrade) is on the default branch, its
post-merge run refuses the old lineage the same way, builds the merge
commit cold and publishes a new lineage; pull requests based on that
commit or later find it. The old lineage is skipped until `gc` or a
lifecycle rule removes it. To upgrade `cambi`, change the pinned version
in both workflows in one commit and expect that commit's post-merge run to
be a cold build, and pull requests opened against older commits to test
`//...` until they are rebased or the merge ref moves.

## Choosing a store

### A directory on `actions/cache` (the workflows)

```toml
[baseline]
store = { kind = "dir", path = ".cambium-store" }
retention_days = 3
```

A relative `path` is read from the workspace root, so `.cambium-store` is
the directory `actions/cache` saves and restores. No credentials, and a
pull request from a fork can restore what the default branch saved. The
limits are GitHub's:

- A repository gets 10 GB of cache in all, and an entry nobody has read
  for 7 days is evicted.
- Keys are immutable, so every post-merge run saves a new entry holding
  the whole store. One entry's size is roughly the indexes younger than
  `retention_days`: merges per day, times `retention_days`, times one
  index. `ls -l .cambium-store/indexes/*/` after a run says what one index
  weighs (12 KiB for rules_shell's 71 targets; docs/STORE.md's
  compression table has larger repositories). Keep `retention_days` to a
  few days on this route, and let eviction drop the old entries.
- An entry a pull-request run saves is visible only to later runs of
  that pull request, which is one more reason the pull-request job does
  not save.

### S3 or GCS through a command store

The `command` store runs your CLI. Use the fetch wrapper from
[docs/STORE.md](STORE.md#misses-and-outages), which gives an outage exit 5
rather than reading it as a miss. The workflows change in three places:

- the cache steps go;
- the post-merge job gets `permissions: id-token: write` and an
  `aws-actions/configure-aws-credentials@v4` step assuming a role that may
  `s3:GetObject`, `s3:PutObject` and `s3:ListBucket` on the prefix;
- the pull-request job assumes a read-only role, `s3:GetObject` and
  `s3:ListBucket`. The list permission is not optional: without it the
  listing fails, the run fetches the lineage-less keys blind, a publisher
  that can list writes those only under `lineage_less_copies` (below), and
  every run is a miss.

Retention is a bucket lifecycle rule on the prefix; `cambi index gc` has
no S3 or GCS implementation. The S3 and GCS commands on this page and in
STORE.md are the ones not run in checking this guide.

### HTTP, read-only, for forks

A pull-request job can read over plain HTTP (a public endpoint in front of
the bucket, say) through a second config file. HTTP cannot list, so it
finds only the lineage-less copies, which a publisher that can list writes
only when asked, in the post-merge job's `cambium.toml`:

```toml
[baseline]
lineage_less_copies = true
```

The pull-request job then runs with its own file, which must match
`cambium.toml` in every section but `[baseline]`:

```toml
# ci/cambium.pr.toml
[baseline]
store = { kind = "http", url = "https://indexes.example.com/cambium" }
```

```console
$ test "$(cambi config hash)" = "$(cambi --config ci/cambium.pr.toml config hash)"
$ cambi --config ci/cambium.pr.toml affected HEAD^1..HEAD --tests-only --no-full-build --format labels
```

The first line fails the job when the two files drift apart, which would
otherwise be a silent miss on every run. `headers` are used as written, so
an endpoint a fork can read is one that needs no secret. With
`--config`, the `cambium.local.toml` read is the one beside that file.

## Credentials a fork will not have

A `pull_request` run from a fork gets no secrets and no OIDC token.

- **`actions/cache`**: nothing to hold; the fork's run restores the
  default branch's store.
- **S3 or GCS**: the fetch fails. With the plain templates that is a miss,
  exit 4; with the wrapper it is exit 5, the retries fail too, and the
  job tests `//...` either way. Dropping `--no-full-build` for forks
  builds the base cold on the runner instead, which gives the exact answer
  at the cost of a cold query.
- **HTTP**: works when the endpoint is public, as above.

## Who may write

The store is trusted input. An index is memory-mapped and cast in place
without validation, which is what makes loading one free
([docs/STORE.md](STORE.md#compression-at-rest)), so whoever can write the
store can hand every later run crafted bytes. Publish only from the
post-merge job on the default branch; keep pull-request jobs read-only,
without `--publish-base`; never give a fork's run write credentials.
`cambi doctor` checks a round trip by writing a probe object, so it is a
check for the post-merge job, not the pull-request one.

## Two post-merge jobs at once

From [docs/STORE.md](STORE.md#writes-are-idempotent-and-concurrent-writers-are-safe):
the index for one tree is the same bytes whoever builds it, a directory
store renames a complete file into place, and an S3 or GCS put replaces a
whole object, so two jobs publishing the same key cannot corrupt it. Two
jobs that both build cold mint two lineages for one tree; a reader takes
whichever it finds with its configuration. The workflow serialises its
runs (`concurrency` with `cancel-in-progress: false`) anyway, because with
`actions/cache` two runs that overlap each save an entry and the next
restore takes only the newer: the older run's index is then missing from
the store, which costs a replay, not a wrong answer.

## What a cold runner pays

`cambi profile` and the benchmark measure from one Bazel server kept warm
across commits, with the index in memory and no store in play. A CI runner
also pays:

- installing `cambi` and Bazel (bazelisk downloads the pinned version);
- fetching the index from the store and decompressing it;
- starting a Bazel server, and fetching the external repositories the
  dirty packages' query needs, when the change dirtied a package at all;
  a change to file contents alone dirties none and runs no Bazel;
- on a miss, a replay or cold build in a second worktree with its own
  server and output base;
- with a command store, one `list_cmd` call per commit the ancestor walk
  examines.

On rules_shell on a laptop, one pull request that edited a `BUILD` file
took 0.7 s with Bazel's server warm and 6.1 s on a fresh output base, the
query alone 0.3 s against 5.6 s. The fresh run still had bazelisk's
download and Bazel's repository cache from earlier runs, which a fresh
runner does not.

## Any other CI

The pull-request job, where `BASE` is the commit the change lands on (the
first parent of a merge commit, or the target branch's tip):

```sh
#!/bin/sh
# Pull request: test what BASE..HEAD affects. BASE is the commit the change lands on.
for try in 1 2 3; do
  rc=0; cambi affected "$BASE..HEAD" --tests-only --no-full-build --format labels > targets.txt || rc=$?
  [ "$rc" = 5 ] || break; sleep 20
done
case $rc in
  0) [ ! -s targets.txt ] || exec bazel test --target_pattern_file=targets.txt ;;
  2) exec bazel test --target_pattern_file=targets.txt ;;  # global: the file holds //...
  4|5) exec bazel test //... ;;                             # no index in reach, or no store
  *) exit "$rc" ;;
esac
```

The post-merge job, on the default branch after each merge:

```console
$ cambi index update HEAD --publish && cambi index gc   # gc: a directory store only
```

## How this guide was checked

Every `cambi`, `git` and `bazel` command above was run against a clone of
`bazelbuild/rules_shell` (Bazel 9.1.1) with a bare repository standing in
for GitHub: merge refs made the way GitHub makes them, a directory store,
and `actions/cache` played by copying the store directory between clones.
The HTTP reader was checked against a local static file server over a
store published with `lineage_less_copies`. Both workflows' `run:` blocks
were run with each exit code (0 with and without tests, 2, 4 and 5). Not
run: the S3 and GCS commands, the install from a release (none has been
made yet), and the GitHub-only steps themselves (`actions/cache`, OIDC).
