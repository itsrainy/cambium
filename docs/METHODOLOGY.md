# How cambium is benchmarked

This page defines the words the benchmark uses and what each measurement
can and cannot claim. The numbers themselves live in [RESULTS.md](RESULTS.md).
Everything here is driven by `cambium-bench` (`crates/bench-cli`, over
`crates/bench`), which measures `cambi` by running it; real subjects are
described by `subject_repos.toml`, synthetic ones by the presets
`cambium-bench subjects list` prints.

## Concepts

**Subject.** A Bazel repository a benchmark points at. There are two kinds,
and the walk accepts either.

- *Real*: an open-source repository from `subject_repos.toml`, with a URL,
  a pinned commit, a history window (first-parent commits before the pin
  the walk steps through), and a Bazel version: the subject's own
  `.bazelversion`, or a manifest pin only when it has none (apollo, a
  WORKSPACE-only repository, runs under 7.4.1 since Bazel 9 can't load
  it). A subject is never pinned twice for another Bazel major; Bazel
  7/8/9 coverage of cambium itself is CI's synthetic version matrix.
  Today: `bazel` and `envoy` on demand, `sourcegraph`, `apollo` and `xla`
  weekly, `tensorflow` (`skip = true`) monthly or by hand
  (`.github/workflows/bench-weekly.yml`).
- *Synthetic*: a generated graph from a preset, `synth:<preset>`
  (`synth:medium` is 4 551 packages, 50 167 rule targets, 250 507 index
  nodes counting source files), emitted as a real Bazel workspace with git
  history so the same commands run as on a real subject: cheap,
  deterministic, scales past any real subject, and says nothing about real
  Starlark, which real subjects are for.

**Tool.** Anything that answers "which targets could be affected by this
change": cambium, [bazel-diff](https://github.com/Tinder/bazel-diff) (the
Rust port, `bazel-diff-rust`), [Tango](https://github.com/uber/tango) and
[target-determinator](https://github.com/bazel-contrib/target-determinator).
cambium is one row in the tables, not the yardstick; the other three are
peers, and invocation detail is in [Tools](#tools).

**Oracle.** An independent check named by what it proves. Two run.

| oracle | question it answers | how |
|---|---|---|
| reverse-dependency (rdeps) | did a tool miss a target? | `bazel query "kind(rule, rdeps(//..., set(<every changed file's label>)))"` on the head revision, over every changed path in a head package, rooted at every label the path has: its main-repo one, and its label inside an in-tree repository whose packages the index holds (a `.proto` under envoy's `api/` has only the second; rooted at main-repo labels alone the oracle asked nothing about it and scored a miss there as recall 1.0), and at `@@R//...:*` for every external repository `R` whose recorded inputs include a changed path (below). Batched by command-line length and unioned. A tool's answer must be a superset of the result. A file *deleted* at head has no label, so its consumers come from the parent index's file rows instead and must appear by label, by package, or in `dirty_packages` (the only spelling when a whole package is deleted, since every rule in it is tombstoned). The package spelling is offered only to a consumer that is itself gone at head; one that survives in another package must appear by label however dirty its own package is. This is the only ground truth, and every tool including cambium is scored against it. `--oracle-sample N` caps the steps queried; the full tier runs it on every step. |
| rebuild | is the incremental updater exact? | the applied index at the pin (the one the walk carried step to step) is diffed node by node, edge by edge and rule hash by rule hash against an index cold-built at the pin on a fresh Bazel server (the reference build, below), and its Skyframe loading layer row by row. Any difference in the graph fails; a loading-layer row the applied index lacks fails unless the walk can explain it (below). It always runs at the pin; `--rebuild-every N` also runs it every N steps. |

*The repository-input roots.* A changed path's own label is not enough
when the path is an input of a repository rule: a `patches` entry, a
`build_file`, a lock file a rule reads. Nothing in the query graph depends
on it, so `rdeps` of it is empty and recall 1.0 on that step proves
nothing. That is how xla's `third_party/llvm/*.patch` (inputs of
`@llvm-raw`, consumed only through `@llvm-project`, which a module
extension generates from it) and envoy's WORKSPACE-era `bazel/pgv.patch`
(issue #28) passed five box runs as 0 of 0. So the oracle has a second root
source, and it does not come from cambium's tables: Bazel's own repository
marker files, `<output_base>/external/@<canonical>.marker`, which list every
file (`FILE:`), directory listing (`DIRENTS:`) and directory tree
(`DIRTREE:`) the repository rule read, and which Bazel itself consults to
decide a refetch. A repository with a changed path among them is dirty, and
so, to a fixpoint, is every repository whose marker records a file of a
dirty one (`@llvm-project`'s marker names `@llvm-raw`'s files). Each dirty
repository adds `@@R//...:*` to the roots: a refetch re-evaluates all of it
and a patch can change any of its files, so what Bazel could rebuild is
everything in `//...` that reaches any target of `R`. The markers are read
twice, before the oracle's first query (the previous step's universe
checked them at the step's base) and after the changed paths' half (whose
universe made Bazel check, and refetch where stale, every repository
`//...` reaches at head), and the two reads are unioned. In-tree
repositories (`local_repository`, `local_path_override`) record no inputs,
and their files are already rooted by label. The markers leave three routes
to a refetch unseen, and a step confined to one of them is still 0 of 0: a
repository never fetched on the walk's output base has no marker; a module
extension's file inputs are recorded with the extension, not in the markers
of the repositories it generates (xla's `pip.parse` reads
`requirements_lock_3_11.txt`, and no marker names it); and a change to a
repository's own definition (xla's `LLVM_COMMIT` bump in
`third_party/llvm/workspace.bzl`) changes the marker's rule key, not a
recorded input. A marker left on a shared output base by another revision
can also name a changed path: if the repository is not defined at head its
pattern fails and roots nothing, and if it still is, the union can root it
although Bazel would not refetch it between base and head. Snapshotting the
markers at base, before cambium's run, and rooting every repository whose
rule key or recorded digests differ after the head universe would close the
definition route and replace the union; it is not built. The step record's
`oracle.repository_inputs` names the dirty repositories, what dirtied each,
the patterns and those that `failed`, how many main-repository rules only
they reached (`labels_added`), each read's marker count, and what the reads
and the pattern queries cost.

*Blind roots.* A changed path's root can go unanswered, and the empty
answer means two different things. A file no rule names (a patch, a README)
is `no such target`: nothing depends on it, and 0 of 0 is the truth. A file
whose package did not load at head is a different case: Bazel skips the
root with the load failure as the reason (`Skipping
'//xla/mosaic:dialect/tpu/tpu_ops.cc': error loading package
'xla/mosaic': ...`), or, when the head index lacks the file's package
altogether, the oracle roots it under the nearest ancestor package the
index has (`//xla/python/ifrt:ir/x.cc` for a file of `xla/python/ifrt/ir`)
and Bazel answers `no such target` for a label that crosses a package
boundary. Either way what the path reaches is unknown, not empty. Box run F
hid four real xla misses this way (steps 41, 45, 193 and 195: the head
index had tombstoned the file's package, the oracle answered 0 of 0, the
step scored recall 1.0). A root is *blind* when Bazel skipped it for any
reason but `no such target`, or when it is unresolved and a `BUILD` file on
disk at head sits between its package and the file in a directory the
workspace's `.bazelrc` (or a `%workspace%/` file it imports) does not name
in `--deleted_packages`; the step record lists them in
`oracle.blind_roots`. Applied to run F's xla record, the second
test alone marks steps 12, 41, 45, 174, 175, 193 and 195 blind, of the 37
steps with an unresolved root.

A step with a blind root and nothing missing is *oracle-blind*: it is not a
pass (the step's `pass` is false), it is not counted as sound or as missing
(`soundness.steps_blind`, named in `soundness.blind_steps`), and it does not
fail the walk. A step with a miss is a miss whether or not it is also
blind. Scoring it as a miss would claim one nobody observed, and scoring it
as a pass is what hid run F's four; so the walk's verdict is over the steps
the oracle could see, and the report prints the blind count beside it in
the soundness line and in the subject table (`1.0000 (7 blind)`). A PASS
with blind steps is a PASS about fewer steps than the walk took, and reads
as one.

*The loading-layer rows.* The rebuild oracle also compares the Skyframe
loading layer (the `FILE`, `DIRECTORY_LISTING`, `PACKAGE`, `GLOB` and
`BZL_LOAD` nodes and reverse edges the dirty stage walks), restricted on
both sides to what a changed path in the subject can reach. Every row that
differs is written, classified, to a sidecar beside the walk record:
`bench-results/<subject>/walk/rebuild-<pin>[-<ver>].json` at the pin, and
`walk/labels/<parent>..<head>/rebuild.json` for a `--rebuild-every` step.
A row is a node (`kind`, `root`, `key`) or an edge (`from`, `to`), the
repository `path` its key names when one can be read off it, the
`package` (the one the key names, or, for a file, listing or `.bzl` row
whose key names none, the one whose directory holds the path at the
compared commit: the nearest `BUILD` or `BUILD.bazel` above it), and a
`class`:

- `created_in_window` (missing, expected): the row sits under a directory
  created between the classification base and the compared commit, and
  the applied layer holds no row at all under that directory. A cold
  `//...` build walks every directory; a refresh dumps only what
  re-querying the dirty packages made Bazel evaluate, so a directory no
  re-queried package's glob descended into has its listing, lookup and
  `BUILD` probe rows on the cold side only. Such rows are inert: every
  seed into the directory starts under it, and a `BUILD` landing there
  later is named from the diff. The applied-layer condition is what keeps
  the class from excusing a real shortfall (a refresh that covered the
  directory would have written at least its listing row). The created
  directory is a directory at the compared commit, never the row's own
  file: a file added to a directory that already existed sits under no
  created directory, its rows are exactly what the refresh of its package
  had to write, and a new `.bzl` in particular has consumers that a layer
  without its `BZL_LOAD` node would never dirty. Nor is the created
  directory ever one that holds a `BUILD` or `BUILD.bazel` at the compared
  commit: that directory is a package created inside the window, which the
  engine had to name from the diff and query, so its rows are owed and a
  hole in them is a shortfall. Refusing the class there costs a correct
  engine nothing, since a package it did query leaves at least its lookup
  and `BUILD` rows under the root. The class is a bound,
  not the exact set of directories the refreshes covered; only a per-step
  record of what was re-queried would tighten it.
- `load_error` (missing, expected): the row's package, or either end's for
  an edge, is one the cold build at the compared commit recorded as
  failing to load under `--keep_going`. Only the rebuilt index's list
  counts for a missing row. The applied index's list is the lineage's
  history, not its state: the engine carries it through every splice,
  assigns it only when it is empty and caps it at 32 names, so a package
  that failed once early in the window and has loaded fine since is still
  in it, and a hole in that package's rows would be excused on the
  strength of an error that no longer exists. A missing row can only be a
  load-error artifact when the cold build itself failed the package.
- `load_error` (extra, expected): either index's list names the row's
  package. An extra never fails, so the looser test is harmless. A
  package that failed to load has its glob inputs and `BUILD` probes on
  one side and not the other, and those rows, which name their package
  only through the tree, are the failure's and not a hole.
- `boundary_probe` (missing, expected): a `FILE` or `FILE_STATE` row for a
  `WORKSPACE`, `WORKSPACE.bazel`, `WORKSPACE.bzlmod`, `REPO.bazel`,
  `MODULE.bazel`, `BUILD` or `BUILD.bazel` that does not exist at the
  compared commit, in a directory that does: Bazel's stat of a file no one
  named. Under `--enable_workspace`, and only then, Bazel looks for a
  `WORKSPACE` file in every package directory it looks up, to find
  repository boundaries, so a server that has loaded packages in WORKSPACE
  mode holds a probe row for each of them, and a refresh that re-queries a
  few packages writes only theirs. Such rows are inert: the file's later
  creation is an added path the diff names, not an edit that has to travel
  through the row. A row for a file under a directory that exists nowhere
  is not a probe and stays unexplained. The `BUILD` and `BUILD.bazel`
  spellings are for the package lookup, which stats `BUILD.bazel` before
  `BUILD`, so a package spelled `BUILD`, or a directory that is no
  package, can hold a row for a file that is absent; apollo's run A rows,
  recorded before the class existed, include 926 `FILE_STATE` rows for
  each of `BUILD` and `BUILD.bazel` (and as many for `WORKSPACE` and
  `WORKSPACE.bazel`), not read further. The class was written (`2868471`)
  for the rows run A (16468) and run C (13755) had found missing on envoy
  and read as unexplained, on the reading that a cold dump carries such
  probes and a refresh dump does not. Runs D and E each counted 13449 of
  envoy's missing rows in it (of 13755 in run E), every one a `WORKSPACE`,
  `WORKSPACE.bazel` or `WORKSPACE.bzlmod` probe. envoy's pin runs
  `--noenable_workspace`, so those were not probes a build at the pin
  makes: they were left on the reference build's server by the walk's
  WORKSPACE-era steps (the reference build, below). A pin in bzlmod mode
  built on a fresh server was expected to put no `WORKSPACE` row in this
  class, and box run G, the reference build on a fresh server, measured
  it: envoy's rebuild oracle counted 0 missing rows, none in this class.
- `deleted_in_window` (extra, expected): the row's path is gone at the
  compared commit. A refresh cannot remove a node, so a file, package or
  `.bzl` that stopped existing leaves its row behind.
- `other` (extra): the path still exists; a dropped `load()`, a glob that
  stopped matching. Inert either way.
- `unexplained` (missing): nothing above accounts for it.

The classification base is the head of the last cold rebuild inside the
window (a fresh lineage resets the layer) or, when there was none, the
window start; both commits are in the sidecar (`from`, `at`) and in the
walk record, so a resumed walk classifies identically. The trees come from
`git ls-tree` on the subject's checkout; a driver with no tree expects
nothing, and every missing row is unexplained.

The pass rule: a non-zero `unexplained` count fails the rebuild oracle,
and the rebuild oracle's verdict is folded into the walk's `pass`, so a
walk whose applied index disagrees with a cold build at the pin fails
whatever every step said. Extras never fail. The tallies (`missing`,
`missing_expected`, `missing_unexplained`, `extra`, `extra_expected`)
travel in the walk record's `rebuild_oracle.sky`, and the report prints
all of them beside the verdict whether or not the oracle held, so a PASS
reached by excusing thousands of missing rows says so where the verdict
is read. The 40-step local walk on the bazel subject found no missing row
at all (docs/ROADMAP.md, Phase 1), so it passes under any set of classes
and tests none of them. The
classes of missing row allowed are the three marked expected above and no
others, and an allowed class is one written down here. They excuse
loading-layer rows only: a node, an edge, a rule hash or an externals row
that differs from the reference build fails the oracle whatever the rows
say.

*The reference build.* The index the rebuild oracle compares against is a
cold `cambi index build` at the pin on a fresh Skyframe graph: a Bazel
server that has evaluated nothing before that build. A dump prints every
node its server has finished, whatever asked for it. The applied index
descends from a build of that kind (after a global step the engine
cold-builds the new lineage's base in a worktree of its own, on an output
base and a server of its own), so a reference built on any other server
compares the lineage against that server's history as well as against the
pin. The descent is not pure: a step that overflows into a re-query, or
refreshes more than 500 packages or the root package, dumps the walk's
server with the full key filter and merges what it holds into the applied
layer, which can only add rows the reference lacks. Those are extras,
which never fail. The runs up to box run E built their reference on
another server; runs F and G built it on a fresh one, and G is the run
RESULTS.md carries for envoy and xla. The earlier reference ran in the
walk's checkout on the walk's output base, on the one server every step's
queries had used and every rdeps oracle query had run on, and
`rdeps(//..., ...)` loads the transitive closure of `//...` and fetches
every external repository it reaches. What those runs reported as
loading-layer rows missing from the applied index was mostly that
server's history. On envoy (box run E) the
reference build held 13755 rows the applied index did not, 13449 of them
classed `boundary_probe` and 306 unexplained, none extra, and its layer
had 1737 `FILE` rows for a `WORKSPACE` against the applied layer's 25.
(Steps here are indices into the walk record's `steps`, counted from 0;
the walk's progress output counts from 1.) envoy ran `--enable_workspace`
until step 293; 298 of the 306 are rows no build at the pin writes (inputs
of WORKSPACE-era repository rules, most of them deleted inside the window,
the oldest at step 10; `WORKSPACE` probes, which the pin's mode never
makes; probes of package directories deleted in the window), and the other
8 only a dependency traversal writes. On xla (box run B) it held 314, all
unexplained, none extra: 280 for inputs of external repositories (244 for
the patches repository fetches watch, 32 for `build_file` files, which the
fetch does not watch and which were statted through the external
repository's `BUILD.bazel` symlink when its root package loaded, and 4 for
a pip lock file the module extension reads), 30 for two main-repo packages
only dead labels name (`tsl/platform`, `tsl/profiler/protobuf`), which
only a dependency traversal visits, and 4 for the root `WORKSPACE`, whose
reader is not identified. xla's lineage base, cold-built on a fresh server
at the head of step 233, holds none of them. RESULTS.md has the rows in
more detail. With the reference on a fresh server, box run G's rebuild
oracle held at both pins: envoy 0 missing rows and 190 extra (178 for
paths deleted in the window), with no rule-hash mismatch, so run E's 11
`//tools/` hashes were the shared server's too; xla 0 missing and 39
extra. sourcegraph's and apollo's rows in RESULTS.md are still run A's,
against a reference on the walk's server, and have not been read. The
classes and the pass rule above are unchanged by this: a row the fresh
reference build holds and the applied index lacks still needs a class,
and a non-zero `unexplained` count still fails the walk.

**Workload.** What tools are run on. There is one: the walk.

- *The walk.* A first-parent walk from the start of the history window to
  the pin. cambium cold-builds an index at the start of the window, then
  takes one step per commit forward, carrying the index from the previous
  step through the store: the shape a CI incremental run has. Each step is
  a `(parent, head)` pair, tagged by its shape (exactly one of `merge` or
  `adjacent`) and by what it touches (any number of `bzl`,
  `build_add_delete`, `hub`, `invalidator`). No step is *selected* by its
  tag: the walk carries the index through every commit in the window. The
  tags are what the sampler stratifies on, and what the coverage table
  reports.

  | tag | what it marks |
  |---|---|
  | `merge` | a merge commit: one pull request's worth of change, the shape a CI job asks about |
  | `adjacent` | a single-parent commit: the same shape as a squash-merged PR |
  | `bzl` | the commit touches a `.bzl` file, so the static load closure is under test |
  | `build_add_delete` | it adds or deletes a `BUILD` file, the two cases a hash-diffing tool cannot see the whole of (at one end the target does not exist) |
  | `hub` | it touches a package holding one of the index's highest in-degree targets, so the reverse closure is as large as this subject gets |
  | `invalidator` | it touches a path the configuration names as an invalidator, the one way to make cambium answer `//...` on purpose |

  A step whose head cannot load some of the packages it dirtied (xla at an
  LLVM integrate whose patches no longer apply to the new LLVM) leaves a
  partial index: those packages keep the rows of the last query that loaded
  them. The walk publishes it to its own store all the same and walks on,
  because the steps after it are the ones that show whether the carried
  packages heal. `cambi` itself refuses such a publish by default
  (`CAMBI-E009`); the walk passes `--allow-partial` for its step indexes
  only. The step record keeps the index's partial state (`index.partial`:
  the packages still in error at that head) and, apart from it, the
  packages the index carries (`index.carried`: live rows from an older
  query, for a package the subject's `publish_partial` does not expect to
  fail). The summary counts and names the carrying steps
  (`carried_index_steps`, `carried_index_packages`) and, separately, the
  steps partial for any reason (`partial_index_steps`), because a subject
  whose cold build is partial by design (xla's
  `tools/toolchains/win_1803/...`) is partial on every step. The cold build at
  the window's start, and any cold rebuild after a global step, keep the
  default policy: the subject's `publish_partial` decides, and a subject
  whose tree does not load there stops the walk.

**Tier.** One workload, two budgets, recorded beside the tier's name in
the result so a number says which budget produced it.

- *correctness*: no peers, the rdeps oracle on a sampled set of steps
  (default 20, at least 2 per tag). What every engine change pays for, run
  beside the offline `replay` (see [Validating an engine change before a
  run](#validating-an-engine-change-before-a-run)).
- *full*: the rdeps oracle on every step, the peers on a
  category-stratified sample (default 40, at least 5 per tag), plus the
  periodic rebuild oracle. The box run.

Each flag (`--oracle-sample`, `--peer-sample`, `--per-tag`,
`--rebuild-every`, `--peers`) overrides one of the tier's defaults and is
recorded in the result.

**Run.** One benchmark run is a subject walked to its pin at one tier, and
the identity of the cambium build that ran it. The environment is stamped
into every result: the machine, the OS and arch, the Bazel version that
answered, the `cambi` version measured, the harness's own sha, the engine
id (index format plus engine configuration), the configuration hash, and
where each tool wrote (so no two tools shared a Bazel output base). Results
are stored under `bench-results/<subject>/walk/walk-<pin>.json`, schema 7,
with that identity inside, so a run from another build never resumes into
this one, and synthetic and real results sit in one tree. Old
`history-*.json` / `changes-*.json` files (schema 6) are refused, not
migrated. Window size (`--steps N`) is a walk parameter, not a mode: a
short local run and the box run differ only in size and tier.

## Measurements

Every number answers one of three questions per step, per tool, and cambium
is a row in the per-tool tables, never the denominator.

**Recall.** `|answer ∩ oracle| / |oracle|`: what the tool did not miss,
against the rdeps oracle. Anything under 1.0 fails the run. cambium's own
recall is what the soundness verdict is built from: a step where the oracle
found cambium missed something is a missing step, and any missing step
fails the run. An oracle-blind step (above) is neither: it is counted
apart and left out of the verdict.

**Tightness.** `|oracle| / |answer|`: how tight the answer is around the
truth. 1.0 is exact; below 1.0 is over-approximation. Reported per tool as
p50/p90 over the steps the oracle ran on, with cambium listed as the first
row. There is no "precision" metric any more (the old share-of-cambium's-
answer-a-peer-names number is gone): tightness against the oracle is the one
width measure, and it is defined the same way for every tool.

**Cost.** Per-step wall time, in the shape CI pays it, with two populations
kept apart because a peer's per-step wall is a full run and cambium's is an
increment:

- *cambium walk (incremental)*: cambium's per-step wall over the steps it
  answered with a splice, one increment each. A whole-process wall, the
  `cambi` child process from start to JSON serialisation; the engine's own
  `timings_ms.total` travels beside it per step, so overhead is visible.
- *each peer, full run per step*: the peer's wall over the steps it ran on.
  The peers keep no state between steps, so every step is a full run.

Beside the two populations the result also records the one cold index build
per subject (at the start of the window) and the rebuild steps (steps the
walk cold-rebuilt instead of splicing, and what they cost). There is no
cold/warm pair: the walk carries one index forward, so a step is either an
increment or a rebuild, never a fresh cold-versus-warm measurement of the
same pair.

## Fallbacks

Where cambium gave up precision or paid a cold cost. Globals still exist;
the cold/warm framing does not.

- *Global rate.* The share of steps cambium answered `//...`, with the
  reason for each (a `.bazelrc` flag not on the allowlist, a lockfile edit,
  a legacy `WORKSPACE`, an invalidator path). A global step is left out of
  the cambium-walk cost population, since its wall is a `//...` fallback,
  not an increment.
- *Engine cold-rebuild rate.* Steps where the dirty package set exceeded
  `limits.max_dirty_packages` and cambium re-indexed the whole repository
  instead of splicing: an exact answer, not a global, at cold cost, counted
  as a rebuild step with the count showing how often it happened.

## Tools

How each tool is invoked, for fairness:

- **cambium**: the walk's driver. `cambi affected <parent>..<head>` at each
  step with the subject's generated `cambium.toml` (defaults plus a
  per-subject output base and store), the index carried from the previous
  step. The only subject-specific configuration is the manifest's
  invalidator list (paths whose change invalidates everything), a soundness
  escape hatch, not tuning. `bazel` and `envoy` have none; `sourcegraph`,
  `apollo`, `xla` and `tensorflow` each name a handful, every one in
  `subject_repos.toml` with its reason.
- **bazel-diff-rust** v46.1.0: `generate-hashes` at parent and head in two
  worktrees, then `get-impacted-targets`, with `-k` (keep going) so a
  failed-to-load package doesn't abort the run. Externals are hashed as
  bazel-diff does by default.
- **Tango** v0.0.46: `tango-cli` built inside the upstream module with a
  small driver (`tools/tango`) running its library on a `(parent, head)`
  pair and printing the impacted set, patched only to pass `--keep_going`
  to its Bazel queries, for the same reason.
- **target-determinator** v0.34.0: upstream's own binary
  (`tools/target-determinator`), run from a head worktree of its own with
  the parent revision as its one positional argument. It is the only peer
  that works from the *configured* graph (one `bazel cquery` per revision,
  hashing configured targets) rather than from the loading phase, so a
  difference that is only a configuration difference is a real disagreement
  between shapes and not a harness artefact. Two of its defaults are
  overridden: a failed base query is a failure
  (`-before-query-error-behavior fatal`) rather than an answer of
  "everything is affected", and its cross-run result cache is off
  (`-nocache_results`) so a step's cost does not depend on which steps ran
  before it.

The peers each see the same checkout, the subject's Bazel version,
repository cache, and stdin closed with `CI=1` set. Labels are compared
after canonicalising external repository names, with file targets dropped
so only rule targets are compared. The peers are the explanation layer: a
disagreement between a peer and cambium is filed under the
[taxonomy](#disagreement-taxonomy) below, but the peers are never the
yardstick. Only the oracle is.

## What is not measured

- **The peers are not ground truth.** Bazel has no query for "and nothing
  else could change"; the rdeps oracle answers "did a tool miss anything",
  and the taxonomy is our explanation of each difference. A peer's answer
  is a row scored against the oracle exactly as cambium's is.
- **External repository contents.** cambium's graph stops at the main
  repository's edge. A dependency changing *by declaration* (a version
  bump in `MODULE.bazel`, a main-repo file a repository rule reads) is
  seen; a change in an external repository's contents at the same
  declared version is not. The hash-diffing peers fetch and hash
  externals, so on such a change they are right and cambium is wrong,
  filed under `cambium_missing`. None observed in the subjects' histories.
- **Round-trip edges against a cold build.** The rebuild oracle compares
  the two indexes by label and skips unknown nodes, which are the external
  leaves, so the edges a leaf carries back into the index's universe
  (`crates/engine/src/round_trip.rs`) are not held against the cold
  build's. A walk that carried a stale one is still caught where it
  matters: the rdeps oracle's answer is Bazel's whole graph, so a target
  reached only through a missing edge is a miss there.
- **Configured builds.** cambium and the loading-phase peers work on the
  unconfigured target graph, keeping `select()` branches: cambium's largest
  over-approximation class.
- **The edge of the invariant.** A toolchain implementation moving behind
  an unchanged declaration (the declarations themselves escalate, by rule
  class, wherever they live), `.bazelignore` and symlinked directories
  aren't modelled, and a bump of a module the Bazel here autoloads native
  rules from is escalated rather than followed (`CAMBI-W024`); the README's
  "Where the invariant stops" section is the authoritative list, with each
  case's status.

## Disagreement taxonomy

Every disagreement between cambium and a peer, or between cambium and the
oracle, is filed under exactly one class
(`crates/bench/src/classify/mod.rs`). The three that fail a run are marked.
The taxonomy is the explanation layer, not the yardstick.

| class | meaning |
|---|---|
| `cambium_extra_select` | cambium keeps `select()` branches a configured query resolves away |
| `cambium_extra_external_repo` | reached through an external repository whose declaration moved; cambium cannot ask whether its contents did |
| `cambium_extra_implicit` | reached only through an implicit (`$`-prefixed) attribute edge the other tool does not walk |
| `cambium_extra_visibility` | rules differ only in `visibility` or `tags`, which cambium's hash ignores |
| `cambium_extra_bzl` | the rule is unchanged but a `.bzl` its package loads changed, so the loading package is direct |
| `cambium_extra_deleted` | a dependency of the target was deleted; not an approximation |
| `cambium_extra_build_glob` | a changed `BUILD` file is a source of a `glob()`bing filegroup |
| `bazeldiff_extra_content` | bazel-diff hashes a path cambium's graph does not name |
| `bazeldiff_extra_external` | canonicalisation of external labels only |
| `bazeldiff_extra_external_content` | external load nondeterminism under `--keep_going` in bazel-diff |
| `bazeldiff_extra_workspace_path` | a rule that bakes the absolute workspace root into its attributes; a harness artifact |
| `bazeldiff_extra_line_shift` | only the rule's line number moved; bazel-diff hashes `generator_location`, cambium and Tango do not |
| `tool_extra_source_directory` | the target consumes a *directory* as a source file (`src_dir = ["."]`, rendered by Bazel as `//pkg:.`), and a changed path lies under it. Tango hashes the directory's contents on disk; Bazel has no edge from a file to its parent directory (documented untracked), so neither oracle nor cambium reaches the target. Filed only when the oracle missed the label too (otherwise `cambium_missing`). Reported, not gated: seen on every apollo step touching an `install_src_files` package |
| `tool_missing` | the peer missed a target the reverse-dependency oracle confirms is affected: the peer's miss, not cambium's extra. Reported, not gated |
| `cambium_missing` | **fails**: the oracle or a tool reports a target cambium does not |
| `cambium_missing_bzl_closure` | **fails**: `rbuildfiles` names a loading package the static `.bzl` closure did not dirty |
| `unclassified` | **fails**: no rule explains it |

## Machines

Local runs (a laptop) are development evidence only; timings aren't
comparable, since some subjects' externals can't be fetched on macOS
x86_64 and a failed fetch is retried by bazel-diff and Tango every step.
The box run comes from one machine, named in RESULTS.md, with Docker
available (some subjects generate remote-execution configurations by
running containers) and every external fetchable. The environment stamped
into each result is what lets a reader refuse to compare two runs from
different machines.

## The commands

```
cambium-bench subjects            list, fetch, or cold-build a subject
cambium-bench walk     <subject>  the walk; one tier, scored against the oracle
cambium-bench run                 the whole plan from flags
cambium-bench replay   <subject>  replay the static stages offline, against a stored index
cambium-bench pull-results        copy a run's results off the box for replay
cambium-bench labels   <subject>  the label sets a step stored
cambium-bench watch               follow a run: one row per subject
cambium-bench report              regenerate RESULTS.md from bench-results/
```

The verbs `history`, `changes`, `stages`, `sweep`, `regress` and `synth`
no longer exist. `cambi bench ...` and `cambi synth ...` are ordinary
unknown-subcommand errors: the harness and the synthetic generator are
`cambium-bench`, not `cambi`.

`cambium-bench walk <subject> --tier correctness|full` is the one workload.
It takes `--steps N` (the window; the subject's own by default),
`--peers <list>|none`, `--oracle-sample N` (0 is every step),
`--peer-sample N`, `--per-tag N`, `--rebuild-every N`, and `--resume` /
`--restart` for a walk already recorded at the pin.

`--only <parent>..<head>[,...]` walks the named steps and no others. The
steps a change is suspected on do not neighbour each other, so each named
step is cold-built at its own parent and answered forward once rather than
carrying an index through the six hundred between them; the answer for that
step is the same question the full walk asks. Revisions may be abbreviated,
a spec the window does not hold is an error rather than a skip, and the
record is filed as `walk-<pin>-only[-<bazel>].json` so a gate run never
overwrites a published walk. This is how a soundness gate costs seven
oracle steps instead of a whole history.

`cambium-bench` with no verb, on a terminal, is the launcher: it composes a
run from the manifest's subjects (real and synthetic), the peers and a size,
shows the exact command lines it will run, then switches to the watch
screen, reimplementing no workload (the lines are the ones
`tools/bench/run-published.sh` runs).

`<subject>` is a name from `subject_repos.toml` or `synth:<preset>`.

## Running it

```console
$ tools/bazel-diff/fetch.sh          # builds bazel-diff-rust at its pin, once
$ tools/tango/fetch.sh               # builds tango-cli at its pin, once
$ tools/target-determinator/fetch.sh # builds target-determinator at its pin, once
$ cambium-bench subjects fetch bazel # mirror and check out the pin
$ cambium-bench subjects build bazel # one cold `bazel query //...`
$ cambium-bench walk bazel --tier correctness --steps 50
```

The tool scripts cache their binaries under
`~/.cache/cambium/{bazel-diff,tango,target-determinator}` (override with
`$CAMBIUM_BAZEL_DIFF_DIR` / `$CAMBIUM_TANGO_DIR` /
`$CAMBIUM_TARGET_DETERMINATOR_DIR`); a run uses whichever is
installed and says so when one is missing, rather than refusing to
compare the other. Mirrors, worktrees, per-subject stores, emitted
synthetic workspaces and a *shared* Bazel `repository_cache` and
`disk_cache` live under `$CAMBIUM_SUBJECTS_DIR`, default
`~/.cache/cambium/subjects`, so subjects don't redownload each other's
externals.

The `run` verb drives the whole sequence per subject (fetch, build, walk,
report, then a final report over every subject), sized small / medium /
large: small is `--tier correctness --steps 3` into
`bench-results-smoke/`, medium is `--tier correctness --steps 50`, large is
`--tier full`. A short local walk and a box run land in the same tree and
differ only in size and tier, which are recorded in the result.

Budget per concurrent real subject: 16+ cores (Bazel's own parallelism
dominates), 64+ GB of RAM (`envoy` and `xla` load a large query result
into one server process), and 500+ GB of disk. `bazel` is the smallest
entry, so a from-scratch contributor can exercise the pipeline on a
laptop.

## Engine micro-benchmarks

The in-memory engine stage benchmarks (the `stages` / `sweep` / `regress`
verbs, and the bazel-diff-on-a-synthetic-graph "legacy" sweep) were retired
with the walk. There is no live "engine stages" workload. A criterion
bench could replace the stage timings later if a cost claim needs them; for
now the walk's per-step engine timings (`timings_ms.total` beside each
step's wall) are the only engine-cost signal the harness records.

## Validating an engine change before a run

A full run takes hours on a rented machine: too slow to be the first
signal an engine change moved an answer. Two cheaper tiers come first.

**Tier 1, offline replay plus a correctness walk.** Most of what an engine
change touches needs no Bazel: the engine decides which external
repositories moved and by which rule, which packages that dirties, and
whether the dirty set exceeds `limits.max_dirty_packages` and
cold-rebuilds. A run leaves the index at the pin behind, so all of this
replays offline, in milliseconds a step.

`tools/bench/precheck.sh` is that tier. It runs, per subject:

```console
$ cambium-bench pull-results <subject>          # the stored index, bases, heads, results, diffs
$ cambium-bench replay <subject> --all          # exit 1 on divergence, no Bazel
$ cambium-bench walk <subject> --tier correctness --steps 20 --restart   # needs Bazel
```

`WALK=0 tools/bench/precheck.sh` runs the offline replay alone (no Bazel).
`replay` links the engine in process against a stored index, with no Bazel,
checkout, or process start. The live correctness walk is the half the
synthetic tests cannot run: it fetches the subject and runs Bazel and
`cambi` against the rdeps oracle.

**Tier 2, the box run.** `tools/bench/run-published.sh` builds both
binaries, refuses a stale one, and hands the plan to `cambium-bench run` at
`--tier full`. See [OPERATIONS.md](OPERATIONS.md).

`replay` beats the pin where it can: a run never splices onto the index at
the pin, but takes the step's own parent from the store (an inert
`MODULE.bazel` read in WORKSPACE mode was invisible against the pin but
reproduced against that parent). The driver archives each parent at
`bench-results/<subject>/bases/<base>.bin`; `replay` prefers an archived
parent, naming which it used and skipping the drift guards for those steps
(`--index` overrides this).

Symmetrically: the rebuild oracle's reference index at the pin is the only
artefact stating what Bazel's graph and externals tables *are* there, and
the run discards it. Archived at
`bench-results/<subject>/heads/<pin>.bin`, `cambium-bench replay <subject>
--head-check --repo <git dir>` re-derives externals tables from the head
tree's own text and runs the oracle's `compare_by_label`. Node, edge and
rule-hash halves are empty by construction, so the externals section is
the whole signal: the one an engine change to those tables moves.
`--rederive` only checks today's derivation against an earlier engine's
tables; `--head-check` checks it against Bazel's truth, in seconds on a
laptop, and `precheck.sh` runs it per subject with a head archive when
`REDERIVE=1`.

It prints: repositories dirtied and by which rule; packages dirtied and
by which route (a `.bzl`-row hit versus `pkg_ext`, issue #4); the
dirty-package count against the limit; and a diff against the run's
record.

Blind spots, reported in its own output:

- **The answer needs Bazel.** `direct` and `affected` come from rule
  hashes after the head packages are re-queried; replay reports only
  `affected (lower bound)` and never gates on it.
- **A cold rebuild's answer needs Bazel.** Replay reports only that the
  fallback fires, not what `//...` would return.
- **A global the run reached after its query is not compared.** A
  toolchain-class declaration the splice found moved (`CAMBI-W023`) is
  decided over the rows the query read back, and replay runs no query, so
  its "not global" there is an earlier stage's answer to a later stage's
  question. Replay skips the comparison rather than raise that lead on
  every such step, and the skip is silent: the step passes with no
  divergence and no note.
- **The stored index is at the pin**, so a divergence from a recorded
  answer is a lead, not a verdict. The tight loop is `--save-baseline`
  before the change and `--baseline` after (`SAVE_BASELINE=1` /
  `BASELINE=1` for `precheck.sh`): both replay on the same index, so
  every difference is the engine's.
- **Some change sets are declined outright**: one editing `MODULE.bazel`
  (`--repo <git dir>` settles it), one editing a `.bazelrc` (needs the
  file's text at both revisions), one adding or removing a `BUILD` file
  (the engine asks the head worktree whether a deletion really removed
  the package). `--repo` also declines two shapes of pin drift large
  enough to change the externals stage's input: an undeclared dirtied
  repository, and a modified `.bzl` outside the stored load closure.
  envoy's three hundred commits straddle a Bzlmod migration and produce
  both.
- **It replays against a healthy index; a run does not.** A run's parent
  keeps whatever the previous step published, including an index a
  since-fixed cambium derived wrongly. `FORMAT_VERSION` and
  `RULE_HASH_VERSION` don't move when only the derivation changes, so a
  fix to *what a correct index contains* must bump
  `cambium_index::CONTENT_REVISION` or the precheck stays clean while the
  run keeps splicing onto the broken parent.

### `--rederive`: an index-time change the stored tables cannot show

The externals stage looks up which main-repo paths feed external
repositories in side tables (`bzl_repos`, per-repository path maps,
`target_only`, the repository set) `cambium_index::build_repo_inputs`
derived at index time from `MODULE.bazel`, `WORKSPACE` and the workspace
`.bzl` closure. A change to that derivation is invisible to a plain replay,
which reruns the stage over the old stored tables.

```console
$ cambium-bench replay envoy --all --repo ~/.cache/cambium/subjects/envoy.git --rederive
$ REDERIVE=1 tools/bench/precheck.sh
```

`--rederive` replays each step with those tables rebuilt from the step's
*own parent* (`git show <base>:MODULE.bazel` and its workspace closure). It
needs `--repo`; every outcome carries `rederived` in the JSON and a
`REDERIVED` marker, and costs a whole index rebuild per distinct parent.

It settles whether an index-time derivation change moves the externals
answer on the real subjects, and whether the current derivation still
matches what the run stored. It cannot see the package closures or set,
which are the pin's: only `bazel query` builds those. Compare a re-derived
run only against another re-derived run: `--baseline` across the two names
which side diverged, since the difference is the derivation, not the
engine. A clean precheck is not a substitute for the run; it is what makes
the run worth starting.

## Choosing what to run

A change to cambium reruns only what it can affect:

| change | run |
|---|---|
| invalidation (what goes global, what gets dirtied), the splice, or the rebuild oracle | `cambium-bench walk <subject> --tier correctness` on the subjects that exercise it |
| the walk, hashing, or the affected set | `cambium-bench walk <subject> --tier correctness --peers none` |
| a peer driver | `cambium-bench walk <subject> --tier full --peers <peer> --peer-sample 5` |
| any of the static stages, before any of the above | `tools/bench/precheck.sh` (offline, minutes, no Bazel) |
| a timing claim against the peers | the box run on the benchmark machine, and only then |

Unit tests, the synthetic soundness sweep and the real-Bazel end-to-end
tests come before any of these. The benchmark is for numbers against the
peers, not for finding out whether a change works.
