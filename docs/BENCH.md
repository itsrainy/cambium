# Benchmarks

Every number here comes from `cambi bench`. The synthetic generator
(`cambi synth describe`) is deterministic from a preset and a seed, so a stage
measured on one machine can be compared with the same stage on another — and
`cambi bench compare` refuses to compare across machines when the fingerprints
differ.

```console
$ cambi bench run    --preset small,medium [--stage walk,splice] [--markdown m.md]
$ cambi bench sweep  --stage walk --param targets --values 1e4,1e5,1e6 --plot walk.svg
$ cambi bench legacy --preset small --changes typical-pr
$ cambi bench compare --baseline a.json --current b.json [--threshold 15]
$ cambi bench bazel  --repo <emitted workspace> --base <sha> [--legacy]
```

Results are JSON carrying `schema` (the same contract version as the engine's
reports), the tool sha, a machine fingerprint, and one record per stage with
`median_ms`, `p90_ms`, `peak_rss_mb`, a stage-specific `extra` map of numbers
and a `notes` map of strings. Saved runs live in `docs/benchmarks/`.

## In-memory stages

These run without Bazel, which is what lets them be measured at sizes no real
repository reaches.

| stage | what it measures |
|---|---|
| `build_index` | `SynthGraph` rows to a finished index; targets/s and serialised size |
| `load_index` | mmap plus building the lazy label map |
| `classify` | changed path to package, and the dirty set |
| `parse_proto` | `streamed_proto` bytes to index rows, including the rule hash |
| `rule_hash` | canonicalisation and blake3 over rule protos alone |
| `splice` | replacing a few packages' rows in a loaded index |
| `reverse_rebuild` | forward CSR to reverse CSR, serial and parallel |
| `walk` / `walk_hub` | the reverse walk, from a scattered direct set or from the biggest hub |
| `e2e_memory` | the whole pipeline minus Bazel, over a delta overlay |
| `e2e_materialize` | the same, materialising a full head index instead |

## `legacy_bazel_diff`: like-for-like against bazel-diff

`bazel-diff` is the tool cambium replaces, so the headline comparison has to
put both on the same graph, the same change, and the same amount of I/O.

```console
$ tools/bazel-diff/fetch.sh                 # builds bazel-diff-rust, once
$ cambi bench legacy --preset small --changes typical-pr
```

What the stage does:

1. generates the graph for the preset and emits it as a real workspace
   (`base/`), then applies the change preset and emits the result (`head/`).
   Two directories rather than one repository with two commits: bazel-diff
   hashes source-file *content* off disk, and two checkouts are the cheapest
   way to give it both revisions at once;
2. writes the `streamed_proto` dump `bazel query` would produce for each
   revision — the generator already emits exactly those bytes;
3. writes an executable `bazel` shim that answers the four subcommands
   bazel-diff issues: `version` (a Bazel 7.4.1 `Build label:` line), `mod`
   (exit 1, i.e. not bzlmod), `info`, and `query`, which copies the fixture
   for the workspace it was pointed at (`-w`, else `$PWD`) to `--output_file`
   or to stdout. This is the same replay technique bazel-diff's own perf gate
   uses, and it takes the Bazel server out of the measurement so what is left
   is each tool's own work;
4. runs `generate-hashes` at base and head and then `get-impacted-targets`
   (**cold**), then repeats with the base hashes already on disk (**warm**);
5. runs cambium's in-memory pipeline on the same `Commit` against the base
   index, over `--runs` iterations;
6. compares the two label sets, restricted to rule labels — bazel-diff's hash
   files also cover source files and external targets, which cambium's
   affected set never names.

The sets must agree, with one legal asymmetry: cambium's index keeps
`select()` branches that a *configured* Bazel query resolves away, so cambium
may report more labels than bazel-diff. That direction is over-approximation
and is only reported. The other direction — a label bazel-diff calls impacted
and cambium does not — is unsoundness, and fails the stage with the count and
the first ten labels.

Reported in `extra`: `bazel_diff_cold_ms`, `bazel_diff_warm_ms`, `cambium_ms`,
`cambium_p90_ms`, `speedup_vs_cold`, `speedup_vs_warm`, `agreed`,
`cambium_only`, `bazel_diff_only`, `bazel_diff_impacted`, `cambium_affected`,
`changed_files`, `targets`. In `notes`: `bazel_diff_version` (the `<commit>
<tag>` the fetch script records in `VERSION` next to `bin/`),
`bazel_diff_binary` and `change_preset`. The stage's own `median_ms` is
cambium's, so a sweep of it plots the thing being sold.

The binary is found at `--bazel-diff <path>`, else `$CAMBIUM_BAZEL_DIFF`, else
`$CAMBIUM_BAZEL_DIFF_DIR/bin/bazel-diff-rust`, else
`~/.cache/cambium/bazel-diff/bin/bazel-diff-rust`. Without it, the stage errors
and `crates/bench/tests/legacy_bazel_diff.rs` skips with a message.

### The headline chart

```console
$ cambi bench sweep --stage legacy_bazel_diff --param targets \
    --values 1e4,1e5,1e6,1e7 --base-preset medium --runs 3
```

writes `bench-legacy.svg` (the default plot path for this stage; `--plot`
overrides it) with three labelled series on shared log-log axes — bazel-diff
cold, bazel-diff warm, and cambium — plus the dashed linear reference line.
`harness::plot_svg_series` draws any number of series; `plot_svg` is the
one-series case every other sweep uses. A saved run of the first two points is
in `docs/bench-legacy.svg` and `docs/benchmarks/sweep-legacy.json`; the upper
end of the sweep needs a machine with room for a workspace of that many source
files, since both tools must see the files on disk.

Measured here (macOS x86_64, 8 cores; bazel-diff v46.1.0, commit `27ce213b`),
`typical-pr`, seed 0:

| targets | bazel-diff cold | bazel-diff warm | cambium | agreed | disagreements |
|---:|---:|---:|---:|---:|---|
| 2,007 (`small`) | 1075 ms | 386 ms | 10.1 ms | 1,638 | none |
| 10,000 | 7,593 ms | 1,678 ms | 8.0 ms | 7,526 | none |
| 100,000 | 76,868 ms | 37,315 ms | 66.9 ms | 66,298 | none |

### A note on the fixture

The dump has to carry `instantiation_stack` frames naming the `.bzl` that
instantiated each rule (`cambium_synth::proto` emits them; cambium's canonical
rule hash strips the field). Without them bazel-diff has nothing to seed a
rule's digest from and cannot see a `.bzl` edit at all, so it silently
under-reports and the comparison stops being like-for-like — which is exactly
what this stage first measured before the frames were added.

## Regression gating

`cambi bench compare` reports every stage that moved by more than the
threshold, improvements included, and exits non-zero when anything slowed
down. Compare only against a baseline from the same machine fingerprint.
