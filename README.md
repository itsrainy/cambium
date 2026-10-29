# cambium

cambium is a Bazel changed-targets and graph analysis tool, similar to
bazel-diff, Tango and target-determinator. Its goal is to go from a set of
file changes to the targets they affected in time proportional to the size
of the change, not the size of the build graph.

## Trying it

The command is `cambi`. The first thing to run on your repository is
`cambi profile`, which replays your recent history and tells you what
cambium would cost and save there. It needs no config and leaves your
checkout alone:

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

That's 30 commits of cockroach. Most only edit source files, which changes
nothing in the build graph, so they're answered from the index in under
half a second. The ones that change a `BUILD` file re-query just the
packages they touched, in 4 to 8 seconds. The `go.mod` bump reaches
almost everything, and took about a minute.

After that, ask what a branch affects, and why:

```console
$ cambi affected origin/main..HEAD --tests-only --format labels
//app:main_test
//lib/core:core_test
$ cambi explain //app:main_test origin/main..HEAD
```

`explain` prints the shortest path from that target back to the change.
[docs/CI.md](docs/CI.md) covers running it in a pipeline.

## Performance on large monorepos

I ran it on five open-source Bazel repositories, walking about 300 commits
of each one's history (74 for apollo) with one index carried forward a
commit at a time, the way a CI job would.

Here's how quickly it answers, as the median time per commit:

| repository | targets | cambium | bazel-diff | Tango |
|---|--:|--:|--:|--:|
| bazel | 24,805 | 96 ms | 7.0 s | 19.1 s |
| envoy | 34,097 | 3.1 s | 9.1 s | 106 s |
| sourcegraph | 70,450 | 459 ms | 12.8 s | 57.2 s |
| apollo | 25,973 | 274 ms | 13.4 s | 27.3 s |
| xla | 35,050 | 146 ms | 24.8 s | 218 s |

And here's how precise it is. Every answer was checked against Bazel's own
`rdeps` query: cambium never missed a target Bazel named. The table is the
median of Bazel's count over the tool's count, where 1.0 is exact and lower
means extra targets reported:

| repository | cambium | bazel-diff | Tango |
|---|--:|--:|--:|
| bazel | 1.000 | 1.000 | 1.000 |
| envoy | 0.986 | 1.002 | 0.899 |
| sourcegraph | 1.000 | 1.000 | 1.000 |
| apollo | 0.969 | 1.000 | 0.784 |
| xla | 1.000 | 1.000 | 1.000 |

Caveats: this is one 16-core Linux machine with Bazel's server kept warm,
so a fresh CI runner pays more (fetching the index, starting Bazel). The
peers ran once per commit on a sample of 40 commits from the same range.
target-determinator isn't benchmarked yet. The slowest 10% of cambium's
steps went up to 17 s. [docs/RESULTS.md](docs/RESULTS.md) has every number
and where each one falls short.

## Installing

There's no release yet, so you'll build from source. You'll need Rust
1.98+, a C compiler, `git`, and `bazel` or `bazelisk`. Linux and macOS only.

```console
$ git clone https://github.com/itsrainy/cambium && cd cambium
$ cargo install --path crates/cli --locked
```

Then, in your Bazel repository:

```console
$ cambi init --store-dir .cambium-store     # writes cambium.toml
$ cambi doctor                              # checks Bazel, config and store
```

[docs/INSTALL.md](docs/INSTALL.md) has the details.

## Design philosophy

cambium gives up strict correctness for speed, on purpose, and always errs
on the side of reporting too much. Running a few extra tests is cheap;
skipping one that a change broke is not. So it never tries to be exact
when exact would be slow: when it can't cheaply tell whether a target was
affected, it says it was, and when it can't answer at all, it says
"everything" (`//...`). The precision table above is what that costs.

Speed comes from not looking at what didn't change. A target's dependencies
are determined entirely by its `BUILD` file, the `.bzl` files it loads,
which files exist in its package, and a handful of repository-wide inputs
(`MODULE.bazel`, lockfiles, `.bazelrc`). Starlark can't read the clock or
the network during loading, so if none of those inputs changed, the
package's part of the graph didn't either. So a run goes:

1. Load the index from the most recent commit that has one.
2. Turn `git diff` into the set of packages whose inputs moved, using
   Bazel's own loading graph (Skyframe), saved alongside the index.
3. Re-query just those packages with real Bazel and splice the results in.
4. Walk reverse dependencies from everything that changed.

Some changes can't be answered incrementally, like a toolchain or platform
definition moving. Those fall back to re-indexing the whole repository or
answer "everything" (`//...`).

A missed target is a bug. There are a few known places it can still
happen, mostly at the edges of the main repository (an external repository
that won't load on the machine running cambium, or a repository input Bazel
doesn't record). [docs/DESIGN.md](docs/DESIGN.md) goes through every case,
including those gaps.

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
