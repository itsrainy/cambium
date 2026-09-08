# Operating the benchmark suite

How to actually run the expensive subjects, and the things that have bitten
somebody already. The *words* are [METHODOLOGY.md](METHODOLOGY.md), the
numbers are [RESULTS.md](RESULTS.md), and what is left to do in the current
session is [HANDOFF.md](../HANDOFF.md).

Nothing here is needed to run the suite on a laptop. `cambi bench changes
synth:small` emits its own subject and needs no setup at all; a real subject
needs `tools/bazel-diff/fetch.sh`, `tools/tango/fetch.sh` and `cambi bench
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
- The driver from the last run is at `~/run-published.sh` on the box: one
  subject at a time, cheapest first (bazel, sourcegraph, apollo, envoy, xla),
  `fetch` / `build` / `history --oracle rebuild` / `changes --tools
  cambium,bazel-diff,tango --oracle rdeps,load` / `report`, then a final
  `bench report`. A failing subject does not stop the rest. Run it under
  `nohup` with a log and watch it with a Monitor; grep the log for
  `^=== `, `FAILED`, `PUBLISHED RUN DONE`, `panic`, `recall`, `unclassified`,
  `cambium_missing`, `No space left`.
- Timings from the last run, for planning: bazel ~23 min, sourcegraph ~57 min,
  apollo ~32 min, envoy ~2 h 34 m (Tango is ~3 min per change set there),
  xla ~3 h. Fetches and cold builds are ~free now that the caches exist.
- When it is done: tar `bench-results/` back, regenerate and commit
  `docs/RESULTS.md`, and `launch.sh stop`.

## Gotchas

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
