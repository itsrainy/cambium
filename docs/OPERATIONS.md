# Operating the benchmark suite

How to actually run the expensive subjects, and the things that have bitten
somebody already. The *words* are [METHODOLOGY.md](METHODOLOGY.md), the
numbers are [RESULTS.md](RESULTS.md), and what is left to do in the current
session is [HANDOFF.md](../HANDOFF.md).

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
- The SG allows SSH from Rainy's current IP only. When it changes, `aws ec2
  revoke-security-group-ingress` the old CIDR and
  `authorize-security-group-ingress` the new one.
- `tools/ec2/launch.sh {launch,sync,ssh,start,stop,terminate}`. The repo is
  private: `sync` tars the working tree over ssh and rebuilds cambi,
  bazel-diff-rust and tango there (`~/finish-bootstrap.log` ends with
  `BOOTSTRAP DONE`).
- **The mirrors on the box are still under `~/.cache/cambium/corpus/`** (61 GB,
  all five subjects fetched and built). Do not move them: the Bazel
  `--output_base` directories under there have absolute paths baked in, and
  moving the tree costs a full re-fetch and re-analysis. Set
  `CAMBIUM_SUBJECTS_DIR=$HOME/.cache/cambium/corpus` instead — the driver
  does.
- **Precheck first, and with `REDERIVE=1`.** `tools/bench/precheck.sh` pulls
  each subject's `bases/*.bin` and `heads/*.bin` alongside the index and the
  results, replays every change set, and — with `REDERIVE=1` and a local
  mirror — also runs `cambium-bench replay <subject> --head-check`, which
  re-derives the externals tables from the head tree's text and compares them
  against the index the rebuild oracle cold-built at the pin. That is the
  rebuild oracle's externals half, offline: an engine change to the externals
  tables is checkable without Bazel or the box. A subject whose published run
  predates the head archive has none and is silently skipped.
- **Smoke first.** `SMOKE=1 tools/bench/run-published.sh` does every
  subject's fetch and build, a three-commit history walk and one change set
  with every tool and both oracles, into `bench-results-smoke/`, in about
  fifteen minutes. Run it after every sync, before the real driver: two of
  the 2026-09-10 confirmation runs lost hours to a refusal it would have
  shown in the first minutes.
- The driver from the last run is at `~/run-published.sh` on the box: one
  subject at a time, cheapest first (bazel, sourcegraph, apollo, envoy, xla),
  `fetch` / `build` / `history --oracle rebuild` / `changes --tools
  cambium,bazel-diff,tango --oracle rdeps,load` / `report`, then a final
  `cambium-bench report`. A failing subject does not stop the rest. Run it under
  `nohup` with a log and watch it with a Monitor; grep the log for
  `^=== `, `FAILED`, `PUBLISHED RUN DONE`, `panic`, `recall`, `unclassified`,
  `cambium_missing`, `No space left`.
- Timings from the last run, for planning: bazel ~23 min, sourcegraph ~57 min,
  apollo ~32 min, envoy ~2 h 34 m (Tango is ~3 min per change set there),
  xla ~3 h. Fetches and cold builds are ~free now that the caches exist.
- When it is done: tar `bench-results/` back, regenerate and commit
  `docs/RESULTS.md`, and `launch.sh stop`.

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
  tables of the two.
- `launch.sh sync` once reported `BUILD FAILED` on a healthy build: `set -x`
  echoes the `trap 'echo "BOOTSTRAP FAILED"'` line into the log. The greps are
  anchored now; if it recurs, read the log's last line.
- Two `cargo test` runs at once share `$TMPDIR/cambium-tests` and the
  store-layout test fails on the race. Run one at a time.
