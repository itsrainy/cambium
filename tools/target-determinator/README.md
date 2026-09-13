# target-determinator

[bazel-contrib/target-determinator][td] as a third foil next to bazel-diff and
Tango, so the same `(base, head)` change sets are answered by a fourth
implementation of "which targets changed".

Unlike `tools/tango`, nothing is grafted in and nothing is patched: upstream
already ships the binary the comparison wants
(`target-determinator/target-determinator.go`), which takes one positional
`<before-revision>`, runs from a checkout that is already at head, and prints
affected labels to stdout one per line.

## Layout and build

    tools/target-determinator/fetch.sh    # clone pinned upstream, generate protos, build

`fetch.sh` clones `bazel-contrib/target-determinator` at a pinned commit into
`$DIR/src` (`$DIR = $CAMBIUM_TARGET_DETERMINATOR_DIR`, default
`~/.cache/cambium/target-determinator`), records the tag and commit in
`$DIR/VERSION`, and builds `$DIR/bin/target-determinator` with `go build`.

Pinned at **v0.34.0** = `d4b6125546979713431e63b5c3e65810fa989446`.

Bazel is needed even though the build is `go build`. The Go bindings for
Bazel's `analysis_v2` and `build` protos are generated rather than checked in
(`third_party/protobuf/bazel/*/` holds only a `dummy.go` and a `BUILD.bazel`),
so a bare `go build` fails with `undefined: analysis.ConfiguredTarget`.
Upstream's answer is a `copy_proto_output` rule per proto, which writes the
generated `.pb.go` back into the source tree; `fetch.sh` runs all three, then
shuts the Bazel server down (the proto build is the only thing it is there
for) and builds the binary with Go. The checkout is reset and cleaned first,
so rerunning is idempotent even though the generated files land in the tree.

`go build` leaves the version string unstamped (`target-determinator
0.0.0-unstamped`, since upstream stamps it through Bazel's workspace status),
which is why `$DIR/VERSION` is the identity the comparison records, exactly as
for Tango.

## Usage

    target-determinator -working-directory <path> -bazel <bazel-binary> \
        [-bazel-startup-opts X]... [-targets '//...'] [-nocache_results] <before-revision>

| flag | meaning |
| --- | --- |
| `-working-directory` | the checkout to query. Must already be at the "after" revision (default `.`) |
| `<before-revision>` (positional) | any git rev; the "before" side |
| `-bazel` | bazel/bazelisk binary (default `bazel` on `$PATH`) |
| `-bazel-startup-opts` | one Bazel *startup* option; repeat the flag per option |
| `-bazel-opts` | one option for `build` and `cquery`; repeat the flag per option |
| `-targets` | the pattern to consider (default `//...`) |
| `-enforce-clean` | `enforce-clean`, `allow-ignored` (default), or `allow-dirty` |
| `-analysis-cache-clear-strategy` | `skip` (default), `shutdown`, `discard` |
| `-before-query-error-behavior` | `ignore-and-build-all` (default) or `fatal` |
| `-cache-dir` / `-nocache_results` | where cquery results are cached, and a switch to use no cache at all |
| `-delete-cached-worktree` | delete a worktree it created rather than reusing it |
| `-filter-incompatible-targets` | drop incompatible targets from the answer (default true) |
| `-verbose` | add ` Changes: ...` after each label, explaining why |
| `-version` | print the version and exit |

Output is plain text, one label per line, in the spelling Bazel's cquery used:

    //pkg:t
    //other:u

## How the harness runs it, and why

`crates/bench/src/tools/target_determinator.rs` is the adapter. Four choices
there are worth naming, because each of them is a place the tool's defaults and
the comparison's needs disagree.

* **Its own head worktree.** target-determinator checks the "before" revision
  out *in the working directory it was given* whenever that directory is clean,
  and puts it back afterwards. Pointed at cambium's own checkout that would
  move the harness's head out from under the run and throw away the Bazel
  analysis cache twice a change set. So the adapter hands it a worktree of its
  own under the run's scratch (`<scratch>/target-determinator-head`), pinned at
  head, and keeps it between change sets so the checkout and the Bazel server
  in it are paid for once rather than per change set.
* **`-before-query-error-behavior fatal`.** The default,
  `ignore-and-build-all`, answers a failed "before" query by reporting
  *everything* as affected. That is the right default for a CI pipeline and the
  wrong one for a comparison: it would enter the results as a real, enormous
  answer instead of as the tool failure it is. `fatal` makes it a non-zero
  exit, which the harness records the way it records any other foil's failure
  on a change set.
* **`-nocache_results`.** The cross-run cquery cache would make each change
  set's cost depend on which change sets ran before it, and its key does not
  include the environment (see upstream's README). Every change set is
  therefore measured cold, and `ToolRun::warm_wall` is `None`: unlike
  bazel-diff, whose base-revision hashing can be separated out and priced as a
  warm CI job, target-determinator's run is one opaque invocation with no
  half to leave out.
* **`-cache-dir` under the run's scratch.** Not `$HOME/.cache`: whatever the
  tool does keep (its own worktrees, when it decides it needs one) belongs with
  the rest of the run's litter, where a killed run leaves it somewhere
  findable.

## Caveats

* **No distance.** target-determinator reports a flat set of labels, so
  `ToolRun::depths` is `None` and no depth agreement is scored against
  cambium's index. Tango is the only foil that reports one.
* **Startup options and relative paths.** Upstream warns that
  `-bazel-startup-opts` values such as `--bazelrc` should use paths relative to
  the repository, because the tool may query from a checkout in a temporary
  directory. The harness passes absolute paths (they are absolute everywhere
  else in the run) and drops `--output_base`, as every foil does. With the
  adapter's own head worktree the tool does not relocate, so this has not
  bitten; a subject whose `.bazelrc` is reached through a relative import is
  the case to watch.
* **cquery, not query.** The tool analyses the graph rather than loading it, so
  its universe is the configured one: `-filter-incompatible-targets` is left at
  its default, and targets excluded by a platform constraint are not in the
  answer. cambium and bazel-diff work from the loading phase, so a
  configuration-only difference is a real disagreement between the two shapes
  and not a harness artefact. It also means the subject has to *analyse* on
  the box, not just load: toolchains resolve during analysis. On a Mac with no
  JDK, bazel's own repository fails at `@rules_java//toolchains:local_jdk`
  before a single target is hashed, and the harness records that as a tool
  failure for the change set. The benchmark box installs a JDK for this
  reason (see `tools/ec2/bootstrap.sh`); cambium and bazel-diff never notice.
* **Verbose output.** `-verbose` is off. The parser tolerates it anyway (it
  takes the first token of each line), but the explanations are large and the
  harness has no use for them.

[td]: https://github.com/bazel-contrib/target-determinator
