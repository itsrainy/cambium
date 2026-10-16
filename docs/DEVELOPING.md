# Working on cambium

## Layout

```
crates/
  index/           CSR graph, interning, on-disk format, mmap loader, splice
  cambium-proto/   vendored build.proto, streamed_proto reader, rule hashing
  bazel/           spawning bazel: query, repo mapping, workspace detection
  diff/            git diff --name-status parsing and revision plumbing
  classify/        path -> package, invalidator rules, dirty-set computation
  engine/          a run: load, dirty, query, splice, direct set, walk
  synth/           synthetic graph generator, mutations, Bazel repo emitter
  bench/           the benchmark: subjects, tools, oracles, the walk, report
  cli/             the `cambi` binary
  bench-cli/       the `cambium-bench` binary, which drives `cambi`
  testkit/         the fixtures the crates' tests share; never shipped
```

`index` and `synth` do not depend on `bazel`. `engine` is the only crate that
depends on everything, `cli` is thin, and `bench-cli` thinner still: the
harness's front door, and the harness measures `cambi` by running it.

`scripts/package-release.sh` builds and packages `cambi` for one target
(`--target`, default the host) the way a release does, into `dist/`;
`.github/workflows/release.yml` runs it for the four release targets on a
`v*` tag, and `scripts/install.sh --from-dir dist` installs from its output.

## Testing

```console
$ cargo test --workspace                     # unit, integration, oracles; no Bazel
$ CAMBIUM_E2E=1 cargo test -p cambium-engine --test e2e_bazel -- --ignored   # needs Bazel
$ cargo test -p cambium-bench --test smoke_real_bazel -- --ignored            # needs bazelisk
$ CAMBIUM_SOUNDNESS_SIZE=medium CAMBIUM_SOUNDNESS_SEEDS=50 \
    cargo test --release -p cambium-engine --test soundness      # weekly
```

`cargo test --workspace` needs no Bazel: every engine a test opens points at a
Bazel binary that does not exist, and the rows come from the synthetic source
or a scanner over a temporary tree. Those fixtures (a scratch directory, a git
repository, an engine over one, a package querier that also serves `.bzl`
text, a synthetic history with an index already published) live in
`crates/testkit` (`cambium-testkit`), a dev-dependency of every crate with
tests and nothing else. Tests needing a real Bazel, a network fetch, or a
subject checkout are `#[ignore]`d, so a plain run reports them as ignored
rather than as passes that ran nothing; each is opted into with `-- --ignored`
and says what it is missing when run without it (`CAMBIUM_REQUIRE_TOOLS=1`
turns a missing tool from a skip into a failure, which the weekly CI job sets).

Four oracles, in increasing order of cost:

1. **The engine against a naive reference.** `crates/synth` generates a graph
   shaped like a real monorepo, mutates it, and computes the affected set the
   expensive way: hash every target with the transitive closure of its inputs
   at both revisions and diff. The engine's answer must contain that set. It
   runs over hundreds of revisions with no Bazel in the loop.
2. **A spliced index against a full rebuild.** After every mutation the
   incrementally updated index must describe the same graph, edge for edge, as
   one built from scratch.
3. **Hand-written golden scenarios against real Bazel**, the cases the
   original design listed: modified source, globbed add and delete, rename, BUILD
   attribute edit, added and deleted targets, a `.bzl` loaded by two packages,
   `exports_files` across packages, a `select` branch change, a package split,
   a global invalidator, an ignored path. Each is also checked against a full
   rebuild, so the expectations cannot drift.
4. **`cambi inspect verify`**, for a sample of targets: the computed reverse
   closure must contain every main-repository rule target `bazel query
   rdeps(//..., target)` reports. This is the soundness oracle, and it runs
   against production indexes too, reporting precision as the share of the
   index's answer that Bazel also reports.

Oracles three and four are opt-in because they need Bazel; they are green
against Bazel 7.4.1, at 100% precision on the emitted workspaces.

## The benchmark harness

`cambium-bench` runs cambium, bazel-diff, Tango and target-determinator on
the same *subject*: an
open-source Bazel monorepo from `subject_repos.toml`, or a generated graph
named `synth:<preset>` emitted as a real workspace with a real history. Its
own binary, it drives `cambi` the way a user does, by running it, so the
benchmark measures the binary that ships.

There is one workload, the *walk*: a first-parent walk from the start of a
history window to the pin, cold-building an index at the start and carrying
it one step per commit forward, the shape a CI incremental run has. Each
step is scored against the reverse-dependency oracle for recall (did a tool
miss anything) and tightness (how much wider than the truth), at the
per-step wall CI pays. cambium is one row in those tables; the oracle is
the only ground truth, and the peers are the explanation layer, not the
yardstick. Two tiers share the walk: `correctness` (no peers, the oracle on
a sample) is what every engine change pays for, and `full` (the oracle on
every step, the peers on a stratified sample) is what
[docs/RESULTS.md](RESULTS.md) reports.

`cambium-bench run` is the whole thing from flags (per subject the fetch, the
cold build, the walk and the report); with no verb, on a terminal, it
composes the same plan on a screen. `cambium-bench pull-results` copies a
finished run's results off the machine that ran it, and `cambium-bench
replay` reruns the engine's static stages over every recorded step
offline, in milliseconds each, against the answers the run recorded; the
diffs it reads are written where the run happened, by `cambium-bench
replay --write-paths`.

[docs/METHODOLOGY.md](METHODOLOGY.md) defines the words and says what each
measurement can and cannot claim; [docs/RESULTS.md](RESULTS.md) is the
numbers; [docs/OPERATIONS.md](OPERATIONS.md) is how to run the five real
subjects at full size, which does not fit on a laptop.

## Which document

For someone using `cambi`:

- the [README](../README.md) and [INSTALL.md](INSTALL.md);
- [docs/CLI.md](CLI.md), every verb, flag and output rule;
- [docs/CI.md](CI.md), running it in a pipeline, with the workflows in
  [docs/ci/](ci/);
- [docs/STORE.md](STORE.md), the index store;
- [docs/DIAGNOSTICS.md](DIAGNOSTICS.md), every `CAMBI-` code and what
  to do about it;
- [docs/OVER_REPORTING.md](OVER_REPORTING.md), where answers are wider
  than they need to be, which `CAMBI-W004` points at;
- the summary table at the top of [docs/RESULTS.md](RESULTS.md) and
  its legend.

For working on cambium, and written for its maintainers (they name
benchmark machines, issue numbers, commits and dates freely):
[docs/METHODOLOGY.md](METHODOLOGY.md) (what the benchmark measures and
may claim), [docs/OPERATIONS.md](OPERATIONS.md) (running it at full
size), [docs/ROADMAP.md](ROADMAP.md), [docs/ENGINE_COSTS.md](ENGINE_COSTS.md),
[docs/SKYFRAME_PROTOTYPES.md](SKYFRAME_PROTOTYPES.md),
[docs/XLA_MAIN_REPO_MISSES.md](XLA_MAIN_REPO_MISSES.md), the
Findings section of docs/RESULTS.md, and the two investigation notes
under [docs/notes/](notes/) that those documents cite.
