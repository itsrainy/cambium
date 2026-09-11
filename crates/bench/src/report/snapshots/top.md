| subject | pin | Bazel | targets | cold build | history p50/p90 | globals | change sets | pass/fail | min recall | precision | tool cold p50 vs cambium warm p50 |
|---|---|---|---:|---:|---:|---|---:|---:|---:|---|---|
| [bazel](#bazel) | `0123456789ab` | 7.4.1, 9.0.0 | 24,519 | 20 s | 80 ms / 80 ms | **1**<br>1× `tools/build_defs/repo/git.bzl` matches an … | 2 | 2 / 0 | 100.0% | bazel-diff-rust 90.0% | bazel-diff-rust 100.0 s vs 180 ms (556×) |

The tool-vs-cambium column is over **all** compared change sets, the ones cambium answered with `//...` included, so the ratio is not an average over the change sets cambium is fast on; each subject's section breaks it out. History p50/p90 are over **non-global** commits (the ones an updater actually walks); the per-subject sections give both distributions. `pass/fail` counts compared change sets. Precision against a tool is informational; recall against the `rdeps` oracle is the gate.
