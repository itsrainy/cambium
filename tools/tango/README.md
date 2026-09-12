# tango-cli

A standalone, non-interactive command-line driver for [Uber Tango][tango]
("Target Analyzer in Go"), so Tango can be compared against cambium on the same
inputs as `bazel-diff`.

Upstream Tango ships no `cmd/`: the only entry points are `example/main.go`
(a YARPC server driven by `example/tango-config.yaml`),
`example/client/client.go`, and `example/cmd/query-bench/main.go`. It's
built to clone a *remote* repository, check out two revisions itself, and
serve changed targets over RPC. `cmd/tango-cli/main.go` here is a new
`main` package driving the same internals directly against a *local*
workspace.

## Layout and build

    tools/tango/fetch.sh          # clone pinned tango, graft the driver in, build

`fetch.sh` clones `uber/tango` at a pinned commit into `$DIR/src`
(`$DIR = $CAMBIUM_TANGO_DIR`, default `~/.cache/cambium/tango`), records the
tag and commit in `$DIR/VERSION`, copies `cmd/tango-cli/*.go` into
`$DIR/src/cmd/tango-cli/` (*inside* the module, so it can import Tango's
`internal/...` packages), and runs `go build -o $DIR/bin/tango-cli
./cmd/tango-cli`.

Pinned at **v0.0.46** = `005b0dec444d956cda42ebb62cce227362783948`.

One patch to upstream is applied, `0001-keep-going-partial-graph.patch`:
Tango's Bazel client fails a query on any non-zero exit, and `bazel query
--keep_going` exits **3** when it finished but could not load part of the
graph. On a real repository that's routine: `bazel/bazel` at its pin
references a GraalVM distribution with no macOS x86_64 build and an RBE
config that shells out to Docker, and Bazel has already streamed the proto
for everything that did load, the same universe bazel-diff (`-k`) and
cambium's own indexer see. The patch accepts exit 3 when the stream parsed,
warns, and uses the partial graph. It also stops the target hasher erroring
on a dep naming one of those unloadable repositories
(`@rbe_ubuntu2404//config:platform`), registering it as an opaque external
node with an empty hash: the repository is missing at both revisions, so
the node is constant and can neither manufacture nor hide a difference
between the targets that did load. Everything else builds unchanged:
`go.sum` is complete and the generated protos in `tangopb/` are checked in.
`fetch.sh` resets the checkout before applying `tools/tango/*.patch`, so
rerunning it is idempotent.

## Usage

    tango-cli -workspace <path> -base <sha> -head <sha> -bazel <bazel-binary> \
        [-startup-options "..."] [-extra-args "..."] [-exclude-external] -o out.json

| flag | meaning |
| --- | --- |
| `-workspace` | local git repo holding the Bazel workspace (default `.`) |
| `-base` / `-head` | any git revs; `base` is the "before" graph |
| `-bazel` | bazel/bazelisk binary. Required, without it Tango downloads bazelisk from GitHub into `~/Library/Caches/tango/bin` |
| `-startup-options` | Bazel *startup* options, placed before `query` (e.g. `--output_base=... --bazelrc=...`) |
| `-extra-args` | extra `bazel query` args, placed after `query` |
| `-exclude-external` | drop targets in external repositories |
| `-o` | output file; stdout if omitted |
| `-max-distance` | limit reverse-dep BFS (default `-1`, unlimited) |
| `-scratch` | where the temporary worktrees go (default: an OS temp dir, removed on exit) |
| `-query-timeout`, `-v`, `-stream-bazel-logs` | timeout, debug logging, live bazel stderr |

Both `-startup-options` and `-extra-args` are split shell-style (single
quotes, double quotes, backslash escapes), so paths with spaces survive.

Output is a JSON array sorted by `(distance, label)`:

    [{"label": "//pkg:t", "distance": 0, "change_type": "changed"}]

`change_type` is `new`, `deleted`, or `changed`.

## How it works, and why this shape

Tango's own flow (`core/repomanager` + `core/workspace`) clones from a
remote URL into a pooled workspace and `git checkout`s each revision in
place. Pointing that at a local path would work, but it serialises on a
single checkout, mutates whatever directory it's given, and drags in the
repo-manager pool, config provider, and metrics scope for no benefit.
Instead the driver creates its own two checkouts and calls the layers
underneath:

1. `git worktree add --detach` for **both** `base` and `head` into the scratch
   dir. The user's checkout is never touched, and each revision gets its own
   Bazel workspace path (and so its own output base, unless `-startup-options`
   pins one).
2. Per revision: `bazel.NewBazelClient` + `git.New` +
   `graphrunner.NewNativeGraphRunner(...).Compute(workspace.NewWorkspace{...})`.
   This is verbatim the server's graph path: the same query, the same
   `git ls-tree` source hashes, the same `targethasher.FromProto`.
3. `mapper.ResultToTargetGraph` → `entity.GetTargetGraphResponse` chunk →
   `tgbdiff.EncodeChunks` → `tgb.NewReader`. This is the blob the server caches
   and serves.
4. `tgbdiff.Compare(before, after, Options{MaxDistance, SeedAttrs: nil})`,
   the production comparison (`controller.compareTargetGraphsTGB`), which
   returns `{Label, ChangeType, Distance}` directly.

The query is therefore exactly Tango's, built in `graphrunner/native.go`:

    bazel <startup-options> query <extra-args> --order_output=no --proto:locations \
          --noproto:default_values --output=streamed_proto \
          'deps(//...:all-targets)'

with `//external:all-targets + ` prepended only when bzlmod is *disabled*.
`bzlmod_enabled` is detected per checkout from the presence of `MODULE.bazel`
(or `MODULE.bazel.lock`), matching `RepositoryConfig.BzlmodEnabled` (which
upstream defaults to true).

The environment (including `USE_BAZEL_VERSION`) is passed through. Tango's
`bazel.Params.EnvVarsMap` *replaces* rather than extends the child
environment when non-empty, so the driver hands it the full `os.Environ()`.

## Caveats

* **`distance` semantics.** `internal/targetdiff/compare.go` computes it. A
  changed target is a *seed* (distance 0) when it's new, deleted, a changed
  source file, has a changed dependency *set* (not just changed dep hashes),
  has a changed source-file dependency, or its own compared attributes
  differ. All other changed targets get their distance from a multi-source
  BFS over reverse dependencies from the seeds; a changed target no seed can
  reach gets `-1`. So distance 0 is "changed in its own right", not merely
  "in the git diff", and a `//pkg:BUILD.bazel` source-file target appears at
  distance 0 alongside the rules it defines.
* **Attributes.** `RepositoryConfig.SeedAttributes` is left unset, so *every*
  string attribute counts as evidence of a direct change, including
  bookkeeping attributes like `generator_location`, which shifts when
  unrelated lines move within a BUILD file. Uber runs production with an
  allowlist here; if a comparison shows Tango over-reporting distance 0,
  this is the first knob.
* **External labels.** `-exclude-external` uses Tango's own `External` bit
  from the encoded graph (falling back to an `@`/`//external:` label prefix
  if a label cannot be located), not a string match. Labels are printed
  exactly as Bazel's query proto reports them, so an external target reads
  `@ext//:x`; bazel-diff and cambium may canonicalise external labels
  differently (`@@ext~//:x`), which matters when diffing tools' outputs.
* **Bazel version.** Tested against Bazel 9.2.0 via bazelisk with bzlmod.
  Under bzlmod the `//external:all-targets` term is (correctly) never
  emitted; the WORKSPACE path is untested here because WORKSPACE is gone in
  Bazel 9.
* **`AllTargetsFiles`** (the "this file changed, so everything changed"
  trigger) and the disk/cache layers are not wired up: this driver always
  computes both graphs from scratch.
* The driver does not fetch: `-base`/`-head` must already be resolvable in
  the local repo.

[tango]: https://github.com/uber/tango
