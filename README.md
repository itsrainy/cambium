# cambium

**TL;DR:** cambium tells you which Bazel targets a set of file changes
affected, in time that follows the size of the change rather than the size
of your build graph. To try it on your own repository:

```console
$ git clone https://github.com/itsrainy/cambium && cd cambium
$ cargo install --path crates/cli --locked
$ cd /path/to/your/bazel/repo && cambi profile
```

`cambi profile` replays your last 30 commits and reports what cambium would
cost there. It needs no config and leaves your checkout alone. The rest of
this README is why it exists, how it works and how it compares.

A common need for any sufficiently large build graph is determining which
build targets are affected by a given set of file changes. I just put up a
PR: which targets should I build, and which tests should I run?

Bazel itself is not particularly well-suited to answer this question. It
is built to work forwards: you name the targets you want, and it loads
their packages, analyzes them, and runs only the actions whose inputs
changed. Going the other way, from changed files back to the targets that
depend on them, means asking `bazel query 'rdeps(//..., set(<files>))'`.
To find every reverse dependency, `rdeps` has to load the whole universe
you give it, so it costs as much as loading the repository. And it only
ever sees one commit: it can tell you who depends on a file today, but
not that a `BUILD` file or a `.bzl` macro changed what a target is.

This has led to the proliferation of tools to fill this gap. Uber's Tango,
bazel-diff and target-determinator are three examples of open-source tools
intended to answer this question, and there are certainly many custom
home-rolled implementations living inside private monorepos.

The way most of these tools work is simple: have Bazel dump the entire
build graph at point A and at point B, then diff the two graphs, usually by
hashing each target together with all of its inputs. Any target whose
inputs changed is a changed target.

And this works great! It produces an exactly correct set of the targets
Bazel will need to reevaluate. Unfortunately, the cost of this process
grows with the size of your build graph, not the size of your change. A
one-line fix pays for loading and hashing the whole repository, twice.
On a large repository that's tens of seconds to a few minutes per commit.

## A different approach

cambium tries to make the cost follow the change instead. It keeps an
index of the build graph, and when a commit comes in, it asks Bazel only
about the packages that commit could have touched, splices those into the
index, and walks the reverse dependencies from there.

That works because of a property of Bazel's loading phase. A target's
dependencies are determined entirely by its `BUILD` file, the `.bzl` files
it loads, which files exist in its package, and a handful of
repository-wide inputs (`MODULE.bazel`, lockfiles, `.bazelrc`). Starlark
can't read the clock or the network while loading, so if none of those
inputs changed, that package's part of the graph didn't change either.
Most commits only edit source files, which change nothing in the graph at
all.

The price is exactness. cambium always errs on the side of reporting too
much: when it can't cheaply tell whether a target was affected, it says it
was, and when it can't answer at all (a toolchain definition moved, say),
it says "everything" (`//...`). Running a few extra tests is cheap;
skipping one that a change broke is not, so a missed target is a bug.
[docs/DESIGN.md](docs/DESIGN.md) goes through every case, including the
known places where one can still happen.

## How it compares

I ran cambium, bazel-diff and Tango on five open-source Bazel
repositories, walking about 300 commits of each one's history (74 for
apollo) with one index carried forward a commit at a time, the way a CI
job would. The median time per commit:

| repository | targets | cambium | bazel-diff | Tango |
|---|--:|--:|--:|--:|
| bazel | 24,805 | 96 ms | 7.0 s | 19.1 s |
| envoy | 34,097 | 3.1 s | 9.1 s | 106 s |
| sourcegraph | 70,450 | 459 ms | 12.8 s | 57.2 s |
| apollo | 25,973 | 274 ms | 13.4 s | 27.3 s |
| xla | 35,050 | 146 ms | 24.8 s | 218 s |

Every answer was checked against Bazel's own `rdeps` query, and cambium
never missed a target Bazel named. How much extra it reported, as the
median of Bazel's count over the tool's count (1.0 is exact, lower means
extra targets):

| repository | cambium | bazel-diff | Tango |
|---|--:|--:|--:|
| bazel | 1.000 | 1.000 | 1.000 |
| envoy | 0.986 | 1.002 | 0.899 |
| sourcegraph | 1.000 | 1.000 | 1.000 |
| apollo | 0.969 | 1.000 | 0.784 |
| xla | 1.000 | 1.000 | 1.000 |

This was one 16-core Linux machine with Bazel's server kept warm, so a
fresh CI runner pays more (fetching the index, starting Bazel). The peers
ran once per commit on a sample of 40 commits from the same range, and
target-determinator isn't benchmarked yet. The slowest 10% of cambium's
steps went up to 17 s. [docs/RESULTS.md](docs/RESULTS.md) has every number
and where each one falls short.

## Trying it

There's no release yet, so you'll build from source. You'll need Rust
1.98+, a C compiler, `git`, and `bazel` or `bazelisk`. Linux and macOS only.

```console
$ git clone https://github.com/itsrainy/cambium && cd cambium
$ cargo install --path crates/cli --locked
```

The command is `cambi`. A good first thing to run on your repository is
`cambi profile`, which replays your recent history and tells you what
cambium would cost there. It needs no config and leaves your checkout
alone. Here it is on 30 commits of cockroach:

```console
$ cambi profile --commits 30 b8b50a4ab8
repository  cockroach   at b8b50a4ab8, last 30 first-parent commits
targets     14,082      932 tests, 1,567 packages, 1,220 external repositories
full index  1 min 50 s  at 11100921a1 (14,076 targets, 930 tests); 1.9 MiB on disk (zstd-3), loads in 19 ms
per commit  4.6 s       mean of 30 incremental, p50 421 ms, p90 5.8 s, max 58.9 s; 4.6 s mean over all 30 with rebuilds
affected    543         targets on a typical commit (p50), 4% of 14,082 (manual included); mean 868, p90 1,413
tests       346         tests on a typical commit (p50), 37% of 932; mean 291, p90 602
global      0/30        no commit invalidated the whole graph
rebuilds    0/30        the whole graph was never rebuilt

    commit                                time affected  tests index
    b2b88b1 06-01 14:27 changefeedc…    443 ms      202    126 = unchanged
    3eb7ff1 06-01 15:16 build/roach…    367 ms        0      0 = unchanged
    14bf541 06-01 19:11 sql,util/ad…     8.2 s    1,763    707 ~     7 rewritten
    ff2117a 06-01 19:11 opt: genera…    361 ms      556    352 = unchanged
    afd1b1a 06-01 21:19 kvserver: a…     5.6 s      688    401 ~     1 rewritten
    94b5e9e 06-02 02:43 go.mod: bum…    58.9 s   11,329    929 = unchanged
    b8b50a4 06-02 08:58 kvserver: d…    382 ms      465    328 = unchanged
    …
```

Most of those commits only edit source files, so they're answered from the
index in under half a second. The ones that change a `BUILD` file re-query
just the packages they touched, in 4 to 8 seconds. The `go.mod` bump
reaches almost everything, and took about a minute.

To use it day to day, set it up in your repository and ask what a branch
affects, and why:

```console
$ cambi init --store-dir .cambium-store     # writes cambium.toml
$ cambi doctor                              # checks Bazel, config and store
$ cambi affected origin/main..HEAD --tests-only --format labels
//app:main_test
//lib/core:core_test
$ cambi explain //app:main_test origin/main..HEAD
```

`explain` prints the shortest path from that target back to the change.
[docs/CI.md](docs/CI.md) covers running it in a pipeline.

## More docs

- [docs/CLI.md](docs/CLI.md): every command, flag and config key
- [docs/CI.md](docs/CI.md): running it in a pipeline
- [docs/STORE.md](docs/STORE.md): where indexes live (a directory, S3, GCS, HTTP)
- [docs/DESIGN.md](docs/DESIGN.md): how it works, in detail, and where it doesn't yet
- [docs/CONTRACT.md](docs/CONTRACT.md): the JSON output and exit codes, for scripts
- [docs/DIAGNOSTICS.md](docs/DIAGNOSTICS.md): what each `CAMBI-` warning means
- [docs/INSTALL.md](docs/INSTALL.md): install details and requirements
- [docs/DEVELOPING.md](docs/DEVELOPING.md): working on cambium itself

## License

MIT, see [LICENSE](LICENSE). The vendored
`crates/cambium-proto/proto/build.proto` comes from Bazel and keeps its
Apache-2.0 license.
