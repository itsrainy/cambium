# Handoff: cambium

Session state on 2026-09-11, and nothing else: what is in flight, what to do
next, and what is known to be open. The durable material lives elsewhere —
read the pointers at the bottom before starting anything.

## In flight

Nothing is running. The benchmark box (`i-03c826487f1ab30ac`) is **stopped**;
its mirrors and store are intact under `~/.cache/cambium/corpus`, holding the
lineage the 2026-09-10 published run left there. The working tree is clean and
pushed.

Both binaries exist now, and both have to be built:

```console
$ cargo build --release -p cambium-cli -p cambium-bench-cli
```

`tools/bench/run-published.sh` and `tools/bench/precheck.sh` do that themselves
and export `CAMBI` and `CAMBIUM_BENCH`. A hand-run `cambium-bench` needs
`--cambi <path>`, `$CAMBI`, or a `cambi` on `PATH`, and says so at once rather
than an hour in.

## What to do next

1. **The box run, on the new binaries.** Nothing has measured `cambi` as a
   child process at full size yet. The order is `REDERIVE=1
   tools/bench/precheck.sh`, then `SMOKE=1 tools/bench/run-published.sh` on the
   box, then the full driver under `nohup`. Follow it with `cambium-bench
   watch` — one row per subject, polled off `bench-results/<subject>/progress.json`
   — which is also the first real exercise that view has had. Compare each
   subject's summary line against [docs/RESULTS.md](docs/RESULTS.md) before
   letting the run continue.
2. **Clear the GitHub Actions billing block.** No workflow can run until it is
   cleared — not `ci`, not `nightly`, not `bench-nightly`. Everything below
   that waits on a green nightly waits on this first.
3. **Issue #9's re-measurement.** `a723a4c` is the second attempt at the
   814k-target answer on the 900k-target repository: a file the fixpoint
   learns nothing about no longer takes the universe as its fallback unless
   something in it is unreadable, dead scopes contribute nothing, and a
   format-string label on the fallback makes no prefix. The issue comment lists
   the four re-measurements asked for. Nothing has measured it; the first
   attempt (`0e92cc8`) moved the number not at all, so measure before
   believing. If the tools-directory workspace `.bzl` is still at universe, the
   new `debug` line names the def and the callee that made it so.
4. **Close #1 and #2 after a green nightly.** The live matrix (#1) and the CLI
   surface (#2) are both landed and both have contract tests
   (`crates/bench-cli/tests/bench_cli_contract.rs`,
   `crates/cli/tests/cli_contract.rs`). They stay open until a nightly run
   proves the two binaries work somewhere that is not this laptop.

## Open items

- **The `dump_repo_mapping` identity fallback has no diagnostic.** When
  `bazel mod dump_repo_mapping` prints something unparseable, `crates/index`
  falls back to an identity mapping and says so with `tracing::warn!`
  (`crates/index/src/loads.rs`). That is a log line, not a `CAMBI-W` code, so
  it is invisible in JSON and to `cambi doctor` — and an identity mapping is
  exactly the condition under which an external label resolves to the wrong
  repository. It wants a code and a row in
  [docs/DIAGNOSTICS.md](docs/DIAGNOSTICS.md).
- **The report generator cannot print the engine-ms column.** Every change-set
  result already carries `cold_engine_ms` and `warm_engine_ms`, and every
  history commit `engine_total_ms`, but `ChangesSummary`
  (`crates/bench/src/compare/model.rs`) aggregates walls only (`tool_cold`,
  `tool_warm`). Showing both columns in RESULTS.md needs new summary fields and
  a fresh run to fill them.
- **Nothing has watched a real box run.** `cambium-bench watch` is checked
  against fixture files and has never followed five subjects over ssh for eight
  hours. Expect the first run to find something.
- **The next run's warm numbers include process start.** The harness measures
  cambium by running `cambi` (`index build`, `affected --format json`, `index
  publish`), so a wall time now carries process start and JSON serialisation.
  The published numbers predate that and quote the wall times only. Expect the
  next run's warm numbers to sit slightly above them for that reason, and say
  so when publishing them.
- **Fallbacks that remain by design.** `WORKSPACE` statements the diff cannot
  classify, toolchain modules and `register_toolchains`, `--config`-scoped and
  external-repository Starlark settings, `--registry` / `--repo_env` /
  compilation flags, `.bazelversion`, `try-import`; and the
  `limits.max_dirty_packages` overflow, which keeps the lineage and re-queries
  `//...`.
- **Known noise on a run.** envoy fails 3 change sets on recall by design
  (`external_route`, the documented round trip through `@quiche`); history
  replays of envoy and sourcegraph differ from the recorded run by a few
  packages on a handful of commits, which is the pin-versus-base drift the
  replay names rather than an engine change. Two `e2e_bazel` failures on this
  laptop predate all current work and reproduce on unmodified HEAD; the
  nightly workflow's `e2e` job is the arbiter.

## Where everything else is

- [docs/CLI.md](docs/CLI.md) — the `cambi` surface: the eight verbs, the
  configuration, the output contract, the diagnostics format, and
  `cambium-bench`'s verbs.
- [docs/METHODOLOGY.md](docs/METHODOLOGY.md) — the words the benchmark uses,
  what each measurement can claim, and how to validate an engine change
  offline before starting a run.
- [docs/OPERATIONS.md](docs/OPERATIONS.md) — the box runbook and every gotcha
  that has cost somebody hours.
- [docs/RESULTS.md](docs/RESULTS.md) — the published numbers, and the Findings
  paragraph that says what each run found.
- [docs/STORE.md](docs/STORE.md), [docs/ENGINE_COSTS.md](docs/ENGINE_COSTS.md),
  [docs/DIAGNOSTICS.md](docs/DIAGNOSTICS.md) — the store, where the time and
  bytes go, and every diagnostic code.

Commit and push after every green test run.
