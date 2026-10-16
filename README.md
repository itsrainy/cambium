# cambium

**Which Bazel targets did this change affect, answered in about the time it
takes to look at the change.**

If you run CI on a big Bazel monorepo, you've probably wanted to test only
what a pull request could actually affect, rather than `bazel test //...`
every time. Tools for this exist (bazel-diff, Tango, target-determinator),
and they work by looking at the whole build graph at both commits and
comparing. That's reliable, but on a large repository it can take tens of
seconds to a few minutes per commit, and that cost grows with the
repository, not with the change.

cambium tries a different approach. It keeps an index of the build graph,
and when a commit comes in, it asks Bazel only about the packages that
commit could have touched, patches those into the index, and walks the
reverse dependencies from there. The goal is a cost that follows the size
of the change and the size of the answer, not the size of the repository.

The command is `cambi`:

```console
$ cambi affected origin/main..HEAD --tests-only --format labels
//app:main_test
//lib/core:core_test
```

The rule I care most about: **it should never miss an affected target.**
Reporting a few extra targets is fine (and measured); leaving one out is a
bug. When cambium can't be sure, it says "everything" (`//...`) rather than
guessing.

> **Status:** early and still changing. There's no release yet, so you'll be
> building from source. Linux and macOS only.

## How well does it work?

To find out, I ran it on five open-source Bazel repositories: bazel, envoy,
sourcegraph, apollo and xla. For each, it walked about 300 commits of
history (74 for apollo), carrying one index forward a commit at a time,
the way a CI job would. Every step's answer was checked against Bazel's own
`rdeps` query.

- **Nothing missed.** On every step the check could run on, cambium's
  answer included every target Bazel named.
- **Usually tight.** At the median step the answer was exactly Bazel's on
  bazel, sourcegraph and xla, and a few percent wider on envoy and apollo.
- **Fast.** A typical step took between 96 ms and 3.1 s depending on the
  repository, compared with 7 to 25 s for bazel-diff and 19 to 218 s for
  Tango on the same steps. The slowest 10% of steps went up to 17 s.

Some honest caveats: these numbers come from one 16-core Linux machine with
Bazel's server kept warm between steps, so a fresh CI runner will pay more
(fetching the index, starting Bazel). The `rdeps` check has blind spots of
its own, too. [docs/RESULTS.md](docs/RESULTS.md) has every number and
where each one falls short.

## How it works, briefly

The whole thing leans on one observation about Bazel's loading phase: a
target's dependencies are determined entirely by its `BUILD` file, the
`.bzl` files it loads, which files exist in its package, and a handful of
repository-wide inputs (`MODULE.bazel`, lockfiles, `.bazelrc`). Starlark
can't read the clock or the network during loading, so if none of those
inputs changed, the package's part of the graph didn't change either.

So a run goes like this:

1. Load the index from the most recent commit that has one.
2. Turn `git diff` into the set of packages whose inputs moved. cambium
   gets this from Bazel's own loading graph (Skyframe), saved alongside the
   index, rather than guessing from paths.
3. Re-query just those packages with real Bazel and splice the results in.
4. Walk reverse dependencies from everything that changed.

Some changes can't be answered incrementally, like a toolchain or platform
definition moving or certain `.bazelrc` flags changing. Those either fall
back to re-indexing the whole repository (still exact, just slower) or
answer `//...`. [docs/DESIGN.md](docs/DESIGN.md) goes through every case,
including the known gaps where an answer can still miss something today.

## Trying it out

You'll need Rust 1.98+, a C compiler, `git`, and `bazel` or `bazelisk`.

```console
$ git clone https://github.com/itsrainy/cambium && cd cambium
$ cargo install --path crates/cli --locked
```

Then, in your Bazel repository:

```console
$ cambi init --store-dir .cambium-store     # writes cambium.toml
$ cambi doctor                              # checks Bazel, config and store
```

`cambi profile` is a good first thing to run. It replays your last 30
commits and tells you what cambium would cost and save on your repository.
`--out` keeps the index it builds so you can use it as your first one:

```console
$ cambi profile --commits 30 --out ./cambium-index.bin
$ cambi index publish ./cambium-index.bin
```

After that you can ask what a branch affects, and why:

```console
$ cambi affected origin/main..HEAD --tests-only
$ cambi explain //app:main_test origin/main..HEAD
```

`explain` prints the shortest path from that target back to the change. If
a run went global, `explain` with no label tells you what caused it and
how to fix it.

## In CI

The setup is two jobs. One runs after each merge to your main branch and
keeps an index at its tip (`cambi index update HEAD --publish`). The other
runs on each pull request and tests what it affects:

```sh
rc=0; cambi affected origin/main --tests-only --no-full-build --format labels > targets.txt || rc=$?
case $rc in
  0) [ ! -s targets.txt ] || bazel test --target_pattern_file=targets.txt ;;
  2) bazel test --target_pattern_file=targets.txt ;;  # everything affected; the file holds //...
  4|5) bazel test //... ;;                             # no index found, or the store is unreachable
  *) exit "$rc" ;;
esac
```

[docs/CI.md](docs/CI.md) walks through the full setup, with GitHub Actions
workflows you can copy from [docs/ci/](docs/ci/).

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
