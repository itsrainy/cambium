# Operating the benchmark suite

How to actually run the expensive subjects, and the things that have bitten
somebody already. The *words* are [METHODOLOGY.md](METHODOLOGY.md), the
numbers are [RESULTS.md](RESULTS.md), and what is in flight or left to do
lives in the issue tracker, not in a file here.

Nothing here is needed to run the suite on a laptop. `cambium-bench changes
synth:small` emits its own subject and needs no setup at all; a real subject
needs `tools/bazel-diff/fetch.sh`, `tools/tango/fetch.sh` and `cambium-bench
subjects fetch <id>` first. This file is about the five real subjects at full
size, which do not fit on a laptop.

## The box

- `i-03c826487f1ab30ac`, c7i.4xlarge, us-east-2, 500 GB gp3, **stopped**.
  Volume bills ~$45/month while stopped. Key `~/.ssh/cambium-corpus.pem`,
  security group `cambium-corpus`. The AWS names are all `cambium-corpus` and
  will stay that way; only the words in this repository were swept. The
  instance carries both `cambium=bench` and the old tag value in the finder's
  filter.
- The SG allows SSH from one IP, the current one. When it changes, `aws ec2
  revoke-security-group-ingress` the old CIDR and
  `authorize-security-group-ingress` the new one.
- `tools/ec2/launch.sh {launch,sync,ssh,start,stop,terminate}`. The repo is
  private: `sync` tars the working tree over ssh and runs
  `tools/ec2/finish-bootstrap.sh` there, which rebuilds both cambium binaries, bazel-diff-rust
  and tango (`~/finish-bootstrap.log` ends with `BOOTSTRAP DONE`). Every
  hard-coded value in the two scripts is a variable at the top with the
  box's default; see the table below.
- **The mirrors on the box are still under `~/.cache/cambium/corpus/`** (61 GB,
  all five subjects fetched and built). Do not move them: the Bazel
  `--output_base` directories under there have absolute paths baked in, and
  moving the tree costs a full re-fetch and re-analysis. Set
  `CAMBIUM_SUBJECTS_DIR=$HOME/.cache/cambium/corpus` instead — the driver
  does.
- **Build both binaries.** The harness is `cambium-bench` and it measures
  cambium by running `cambi`, so a run needs both: `cargo build --release -p
  cambium-cli -p cambium-bench-cli`. `tools/bench/run-published.sh` and
  `precheck.sh` do it and export `CAMBI` (the `cambi` under measurement) and
  `CAMBIUM_BENCH` (the harness itself), each overridable in the environment;
  a hand-run `cambium-bench` needs `--cambi <path>`, `$CAMBI`, or a `cambi` on
  `PATH`, and says so at once rather than an hour in.
- **Precheck first, and with `REDERIVE=1`.** `tools/bench/precheck.sh` runs
  `cambium-bench pull-results` per subject — the index at the pin, the
  recorded results, `bases/*.bin`, `heads/*.bin`, and the diff of every change
  set, which the box's own `cambium-bench replay --write-paths` derives over
  the mirror — into `bench-precheck/`, then `cambium-bench replay --all`.
  With `REDERIVE=1` and a local mirror it also runs `cambium-bench replay
  <subject> --head-check`, which re-derives the externals tables from the
  head tree's text and compares them against the index the rebuild oracle
  cold-built at the pin: the rebuild oracle's externals half, offline. A
  subject whose published run predates the head archive has none and is
  silently skipped.
- **Smoke first.** `SMOKE=1 tools/bench/run-published.sh` does every
  subject's fetch and build, a three-commit history walk and one change set
  with every tool and both oracles, into `bench-results-smoke/`, in about
  fifteen minutes. Run it after every sync, before the real driver.
- The driver is `tools/bench/run-published.sh`: it builds both binaries,
  refuses to start unless they are built from the tree (`cambium-bench run
  --require-sha`), and hands the plan to `cambium-bench run` — one subject at
  a time, cheapest first (bazel, sourcegraph, apollo, envoy, xla), `fetch` /
  `build` / `history --oracle rebuild` / `changes --tools
  cambium,bazel-diff,tango --oracle rdeps,load` / `report`, then a final
  `cambium-bench report`. The plan is `crates/bench/src/plan.rs`, the same one
  the launcher screen composes; `cambium-bench run --dry-run` prints it. A
  failing step does not stop the rest, and a failed fetch or build skips the
  rest of that subject. Run it under `nohup` with a log; follow it with
  `cambium-bench watch` (below) and grep the log for `^=== `, `FAILED`,
  `PUBLISHED RUN DONE`, `panic`, `recall`, `unclassified`, `cambium_missing`,
  `No space left`.
  Compare each subject's summary line against `docs/RESULTS.md` as it lands,
  before letting the run continue to the next: the cheap subjects finish
  first, and a regression they show is hours cheaper to stop there than after
  xla.
- **Following it: `cambium-bench watch`.** One row per subject, polled off
  `bench-results/<subject>/progress.json` once a second — the step, progress,
  the commit or change set in hand and the tool running on it, elapsed, an ETA
  from the p50 of what is done, and the globals, worst recall and pass count so
  far — with each subject's last error and the diagnostics its last `cambi`
  document carried in the footer. `enter` opens the selected subject's change
  sets, `q` quits. It reads files and nothing else, so it follows a run over
  ssh on the box, a finished run, and an `rsync`ed copy on a laptop.
  `cambium-bench watch --once` prints the same table for a log or a cron mail;
  `--once --format json` is the snapshot document for a script. Run
  `cambium-bench` with no arguments on a terminal for the launcher, which
  composes a run and then shows this screen.
- Timings from the last run, for planning: bazel ~23 min, sourcegraph ~57 min,
  apollo ~32 min, envoy ~2 h 34 m (Tango is ~3 min per change set there),
  xla ~3 h. Fetches and cold builds are ~free now that the caches exist.
- When it is done: tar `bench-results/` back, regenerate and commit
  `docs/RESULTS.md`, and `launch.sh stop`.

## Environment

Every knob the harness and its scripts read. A per-run choice is a flag of
`cambium-bench` with the variable as its fallback (clap `env`), so a script
and a person at a terminal spell it the same way; the library itself reads
none of them. Unset means the default.

| variable | read by | default | meaning |
|---|---|---|---|
| `CAMBI` | `cambium-bench --cambi` | `cambi` on `PATH` | the product binary under measurement |
| `CAMBIUM_SUBJECTS_DIR` | `--subjects-dir`; the scripts | `~/.cache/cambium/subjects` | mirrors, worktrees, stores, output bases (the box keeps `~/.cache/cambium/corpus`) |
| `CAMBIUM_BAZEL_DIFF` | `--bazel-diff` | `<bazel-diff dir>/bin/bazel-diff-rust` | the bazel-diff Rust binary |
| `CAMBIUM_BAZEL_DIFF_JAR` | `--bazel-diff-jar` | `<bazel-diff dir>/bazel-diff_deploy.jar` | the Kotlin jar, for `--tools bazel-diff-kotlin` |
| `CAMBIUM_BAZEL_DIFF_DIR` | `--bazel-diff-dir`; `tools/bazel-diff/fetch.sh` | `~/.cache/cambium/bazel-diff` | where `fetch.sh` installed |
| `CAMBIUM_TANGO` | `--tango` | `<tango dir>/bin/tango-cli` | the Tango driver |
| `CAMBIUM_TANGO_DIR` | `--tango-dir`; `tools/tango/fetch.sh` | `~/.cache/cambium/tango` | where `fetch.sh` installed |
| `CAMBIUM_REDERIVE_DUMP` | `replay --rederive-dump` | off | keep each re-derived index at `<dir>/rederived-<base>.bin` |
| `CAMBIUM_LOG` | `--log` | `warn` | log level |
| `AWS_REGION` | `pull-results --region`; `launch.sh` | `us-east-2` | where the box is |
| `CAMBIUM_REQUIRE_TOOLS` | the real-tool tests | unset | `1` turns a "tool not installed" skip into a failure (CI's nightly job) |
| `CAMBIUM_SMOKE_SCRATCH` | `crates/bench/tests/smoke_real_bazel.rs` | a fresh temp dir | keep the smoke test's subjects between runs |
| `SUBJECTS` | `run-published.sh`, `precheck.sh` | the five subjects | which subjects, space-separated |
| `STEPS` | `run-published.sh` | `fetch build history changes report` | which steps |
| `SMOKE` | `run-published.sh` | `0` | `1` is the fifteen-minute pass into `bench-results-smoke/` |
| `RESTART_CHANGES`, `RESTART_HISTORY` | `run-published.sh` | `0` | `--restart` on the workload |
| `NO_BUILD` | `run-published.sh` | `0` | `1` skips the cargo build (the sha preflight still runs) |
| `CAMBIUM_REPO` | `run-published.sh`, `finish-bootstrap.sh` | `~/cambium` | the checkout on the box |
| `CAMBIUM_BENCH` | `run-published.sh`, `precheck.sh` | `target/release/cambium-bench` | the harness binary |
| `CARGO_ENV`, `GO_BIN`, `BAZELISK` | `run-published.sh`, `finish-bootstrap.sh` | `~/.cargo/env`, `/usr/local/go/bin`, `/usr/local/bin/bazelisk` | the box's toolchains |
| `BOOTSTRAP_LOG` | `run-published.sh`, `launch.sh` | `~/finish-bootstrap.log` | where `finish-bootstrap.sh` wrote; the driver refuses to run unless it ends in `BOOTSTRAP DONE` |
| `CACHE` | `precheck.sh` | `bench-precheck/` | where `pull-results` puts the pulled trees |
| `LOCAL_MIRRORS` | `precheck.sh` | `$CAMBIUM_SUBJECTS_DIR` | local mirrors to pass as `--repo` |
| `REDERIVE`, `BASELINE`, `SAVE_BASELINE` | `precheck.sh` | `0` | `--rederive` and the head check; `--baseline` / `--save-baseline` against `<cache>/<subject>/baseline.json` |
| `PEM` | `precheck.sh`, `launch.sh` | `~/.ssh/cambium-corpus.pem` | the ssh key |
| `BOX_HOST` | `precheck.sh` | asked of EC2 | the box's address |
| `BOX_REPO`, `BOX_SUBJECTS` | `precheck.sh`, `launch.sh` | `cambium`, `.cache/cambium/corpus` | the checkout and the mirrors on the box, relative to its home |
| `PATHS_FROM_BOX` | `precheck.sh` | `1` | derive the change-set diffs on the box |
| `SIZE`, `DISK` | `launch.sh` | `c7i.4xlarge`, `500` | the instance |
| `KEY`, `SG_NAME`, `TAG_FILTER`, `INSTANCE_NAME`, `AMI_OWNER`, `AMI_NAME`, `BOX_USER`, `POLL_SECS` | `launch.sh` | see the script | the AWS names, and how often `sync` polls the build |
| `TANGO_REPO`, `TANGO_TAG`, `TANGO_COMMIT`, `GO` | `tools/tango/fetch.sh` | the pinned tag | which Tango, and which Go builds it |

## Gotchas

- The externals tables live *in the index*. Measuring an attribution change
  against a store that already holds an index at the same config hash reads
  the old tables back and looks like a post-fix measurement — issue #9's
  first run did exactly that. `[baseline]` is outside `Config::hash()`, so
  redirect `store.path` and `--cache-dir` to force a cold build without
  changing the hash, or check `cambi inspect externals` against the change you
  expect before trusting the number.
- `changes-<pin>.json` keeps per-class counts and at most 20 example labels
  per tool, which is not enough to explain a change set where a tool named 990
  targets cambium lacked. The sets themselves are written whole beside it, one
  directory per change set:
  `bench-results/<subject>/changes/labels/<base>..<head>/` holds
  `cambium.txt.zst`, one `<tool>.txt.zst` each, `oracle.txt.zst` when the rdeps
  oracle ran, and `classes.tsv.zst` — `tool`, `side`, `class`, `label` for
  *every* disagreement, not just the examples. Read them with `cambium-bench
  labels <subject> --change-set <base>..<head>` (abbreviated revisions are
  fine), narrowed with `--tool`, `--class` and `--side`, or `--diff` for the
  only-tool/only-cambium/both counts per tool. It needs no Bazel and no
  checkout, so it works on results tarred off the box. Globally invalidated
  change sets store nothing: cambium answered `//...` and nothing was compared.
- The job tmp dir (`$CLAUDE_JOB_DIR/tmp`) was wiped mid-session once; keep
  driver scripts and logs under the repo (ignored) or on the box. Note the
  engine's own index cache sits in `/tmp/cambium-cache/` on the box, which is
  the same kind of place.
- Tango, bazel-diff and cambium must never share an `--output_base`.
- macOS x86_64 cannot fetch some subjects' externals (toolchains_llvm,
  GraalVM, Docker-generated RBE configs): local tool timings are inflated and
  not publishable; bazelisk for the suite is `~/.cache/cambium/bin/bazelisk`.
- A killed change-set run leaves tango worktrees registered on the subject
  mirror; tango-cli recovers them itself on the next run (and says so on
  stderr when it does), `git -C <mirror>.git worktree prune` is the manual
  fix. An `is not a working tree` line is *not* that failure — see the apollo
  item above.
- Results written by another cambium build (format version or config hash)
  are started over, not resumed; that identity is in every state file. You
  will see `WARN skipping an index this build cannot read` for the same
  reason, which is benign.
- apollo: WORKSPACE-only, pinned to Bazel 7.4.1, history capped at 74
  commits because a deeper revision does not load. Its rebuild oracle is
  `partial` with 1 package in error — `@local_config_cuda`, no CUDA on the
  box. Expected.
- xla loads in ~25 s and indexes 455 packages against 449 BUILD files on
  disk, so that is a *complete* load, not a truncated one. The manifest's
  "~1 h loading, large fetch" note is about analysis with externals, which a
  loading-phase `bazel query` never pays. Its 4 `PARTIAL` packages are the
  Windows `tools/toolchains/win_1803/py3{8,9}` toolchains failing on Linux.
- zsh does not word-split `$VAR`; use explicit arrays in ad-hoc shell.
- A subject whose pin is partial on the box for a constant reason (apollo's
  `@local_config_cuda`, xla's Windows toolchains) needs `publish_partial =
  true` in `subject_repos.toml`, or `cambium-bench subjects build` refuses to publish
  the index and every later step of that subject fails.
- The pin index is not the index a change set ran against. envoy's pin is past
  its bzlmod migration; the bases the run spliced onto were WORKSPACE-mode
  trees with an inert `MODULE.bazel`, and a replay against the pin cannot see
  what the derivation did there. When a change set's `dirty_packages` looks
  wrong, copy the index the run loaded (`loaded baseline index ... path=` in
  the log, under `/tmp/cambium-cache/`) and replay with `--index` pointed at
  it; `cambi inspect externals <file> --repo <name>` diffs the repository
  tables of the two. `CAMBIUM_REDERIVE_DUMP=<dir>` on a `--rederive` replay
  keeps each re-derived index on disk for the same comparison, and the corpus
  attribution test (`cargo test -p cambium-index corpus_attribution --
  --ignored --nocapture`, with `CAMBIUM_CORPUS_DIR`, `_PATHS`, `_ALIAS` and
  `_INDEX`) answers "what does the derivation say about this file in this
  tree" in twenty seconds, against a stored index's mapping when given one.
- `launch.sh sync` once reported `BUILD FAILED` on a healthy build: `set -x`
  echoes the `trap 'echo "BOOTSTRAP FAILED"'` line into the log. The greps are
  anchored now; if it recurs, read the log's last line.
- Two `cargo test` runs at once share `$TMPDIR/cambium-tests` and the
  store-layout test fails on the race. Run one at a time.

## Known noise

What a healthy run reports that is not a regression. Anything not on this
list is.

- envoy fails 3 change sets on recall by design: `external_route`, the
  documented round trip through `@quiche` (METHODOLOGY.md, RESULTS.md). The
  diagnosis lists at most 25 of a set's missing labels as probed
  (`MAX_ROUTE_PROBES`); the rest of that set are the same quic targets past
  the cap, not a second cause.
- History replays of envoy and sourcegraph differ from the recorded run by a
  few packages on a handful of commits. That is the pin-versus-base drift the
  replay names, not an engine change; change-set replays are exact, and one
  that is not is a finding.
- `WARN skipping an index this build cannot read`, and a results directory
  another cambium build wrote being started over rather than resumed: see the
  gotcha above, benign.
