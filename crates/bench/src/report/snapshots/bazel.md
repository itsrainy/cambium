- pin: `0123456789ab`
- Bazel: 7.4.1, 9.0.0
- generated: <today>
- tool: `deadbee`

#### Cold build

| targets | edges | packages | index bytes | wall |
|---:|---:|---:|---:|---:|
| 24,519 | 53,242 | 524 | 3,754,856 | 19.5 s |

#### History (3 commits, `aaaaaaaaaaaa..0123456789ab`)

| over | commits | p50 | p90 | p99 | max |
|---|---:|---:|---:|---:|---:|
| all commits | 3 | 40 ms | 80 ms | 80 ms | 80 ms |
| non-global commits | 2 | 80 ms | 80 ms | 80 ms | 80 ms |

A *global* commit touches an invalidator, so cambium answers `//...` without walking: it is fast for the wrong reason, and including it would flatter the updater. Both rows are given so the difference is visible.

- zero-dirty commits (a diff that dirtied no package): **0.0%**
- largest dirty set: **2 packages** at `222222222222`
- `rbuildfiles` calls: 0 of 3 commits
- commits where a changed `.bzl` dirtied packages through the static load closure: 0 of 3
- mean affected targets: 10

Global commits: **1**

| reason | commits |
|---|---:|
| `tools/build_defs/repo/git.bzl` matches an invalidator | 1 |

#### Three-way comparison

2 change sets compared, 2 passed, 0 failed (1 of them global, which pass by construction: `//...` contains every answer. The tools still ran on them — see **Globals** below). Worst oracle recall: **100.0%**.

| Bazel | tool | change sets | passed | precision | min recall |
|---|---|---:|---:|---:|---:|
| `7.4.1` | bazel-diff-rust | 2 | 2 | 90.0% | 100.0% |

#### Timings (p50 / p90)

| Bazel | tool | over | change sets | cold | warm |
|---|---|---|---:|---:|---:|
| `7.4.1` | bazel-diff-rust | all change sets | 2 | 100.0 s / 100.0 s | 50.0 s / 50.0 s |
| `7.4.1` | bazel-diff-rust | incremental | 1 | 90.0 s / 90.0 s | 45.0 s / 45.0 s |
| `7.4.1` | **cambium** | all change sets | 2 | 4200 ms / 4200 ms | 180 ms / 180 ms |
| `7.4.1` | **cambium** | incremental | 1 | 4200 ms / 4200 ms | 180 ms / 180 ms |

**Quote the `all change sets` rows.** They cover every change set compared, including the 1 where cambium fell back to `//...` and did no incremental work at all — which is what a user running cambium over this history actually waits for. The `incremental` rows drop those; they are the rows that belong with the precision and class numbers above, because those are computed over exactly the same change sets, but read on their own they flatter cambium — an average over the change sets it is fast on. 0 of the incremental change sets re-indexed the whole repository (the dirty set was too large to splice), so their *warm* is a cold number under a warm label.

cambium's *cold* is a full index build from `bazel query //...`; its *warm* is an update against the published baseline, which is what a CI job actually pays. bazel-diff's warm reuses the base revision's hashes; Tango has no warm mode, so its cell is `n/a`. *change sets* is how many change sets each tool's distribution is taken over: Tango is often capped (it costs 30-500 s a change set) while cambium and bazel-diff run on all of them.

#### Globals

| Bazel | change sets | cambium `//...` | tool | its precise set | tool cold p50 | cambium cold/warm p50 |
|---|---:|---:|---|---:|---:|---:|
| `7.4.1` | 1 | 24,000 | bazel-diff-rust | 40 | 100.0 s | 30 ms / 30 ms |

Means over the global change sets. *cambium `//...`* is the number of rule targets `//...` names at head — the answer cambium gave — and *its precise set* is what the tool found on the same change_set. The ratio between them is the over-approximation a global hands the user; the wall columns say what each tool charged to produce its answer. These change_sets are excluded from precision, the class counts and recall (there is nothing to be wrong about inside `//...`) and included in the `all change sets` timings above.

#### Disagreements by class

| class | labels | fails the run | meaning |
|---|---:|---|---|
| `cambium_extra_select` | 3 | no | cambium keeps `select()` branches a configured Bazel query resolves away |
| `cambium_extra_external_repo` | 0 | no | cambium reached it from an external repository whose contents moved; the tool resolves the repository and sees its targets unchanged |
| `cambium_extra_implicit` | 0 | no | reached only through an implicit (`$`-prefixed) attribute edge the tool does not walk |
| `cambium_extra_visibility` | 0 | no | the two rules differ only in `visibility`/`tags`, which cambium's hash ignores |
| `cambium_extra_bzl` | 0 | no | the target's own rule is byte-identical, but a `.bzl` its package loads changed |
| `cambium_extra_deleted` | 0 | no | a dependency of this target was deleted between base and head |
| `cambium_extra_build_glob` | 0 | no | reached from a changed `BUILD` file that a `glob()`bing filegroup consumes as a source |
| `bazeldiff_extra_content` | 0 | no | a changed path with no consumer in cambium's head index (content the graph does not name) |
| `bazeldiff_extra_external` | 0 | no | confined to `@external//...` labels, which cambium does not index and the tools canonicalise differently |
| `bazeldiff_extra_external_content` | 0 | no | an unchanged `//...` rule that depends on something external; how much of the external graph loads varies between the tool's runs |
| `bazeldiff_extra_workspace_path` | 0 | no | the rule is identical at both revisions once each checkout's own absolute workspace root is normalised away |
| `bazeldiff_extra_line_shift` | 0 | no | an unchanged rule whose `BUILD` file changed around it: only its line number moved, and cambium's rule hash excludes `generator_location` |
| `tool_extra_source_directory` | 0 | no | the target consumes a directory as a source file (`//pkg:.`) and a changed path lies under it; the tool hashes the directory's contents, Bazel's graph has no such edge and the reverse-dependency oracle did not reach the label |
| `tool_missing` | 0 | no | the tool missed a target the reverse-dependency oracle confirms is affected: the tool's miss, not cambium's extra |
| `cambium_missing` | 0 | **yes** | **fails**: the oracle or a tool reached something cambium did not — unsoundness |
| `cambium_missing_bzl_closure` | 0 | **yes** | **fails**: `rbuildfiles` names a loading package the static `.bzl` closure did not dirty |
| `unclassified` | 0 | **yes** | **fails**: no predicate explains the disagreement; needs a new class or a fix |

Counts are labels, summed over every change set and tool.

#### Why cambium reported the labels the tool did not

| tool | cambium's reason | labels |
|---|---|---:|
| bazel-diff-rust | rule definition changed | 233 |

These are cambium's *own* explanations for the extra labels (`affected --explain`), not the classifier's: they say what made cambium reach the target, where the class table says why the disagreement is benign.

- `bazel-diff-rust`: depth agrees on 80.0% of shared labels (1 change set); 4 file-target rows dropped to match cambium's universe.

#### `rdeps` oracle

| change sets with an oracle | source labels queried | labels skipped | partial change sets | partial batches | missing labels | of those, deleted-file consumers | of those, external route | external labels not counted |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 2 | 0 | 1 | 2 | 0 | 0 | 0 | 0 |

A *partial* batch is a `--keep_going` query that exited 3: Bazel answered for what loaded. The oracle is a lower bound on what cambium must contain, so a partial answer is still sound to check against — it just proves less. `missing labels` is the count that gates the run, and must be zero. *external route* is the subset `somepath` shows Bazel reaching only by leaving `//...` and coming back (see [Externals and soundness](#externals-and-soundness)); it is a label on a failure, not an excuse for one. *deleted-file consumers* is the other half of the check: a file that is gone at head has no label to query, so its consumers come from the base index and must appear in cambium's answer by label or by package. The last column is the `@repo//...` half of the difference, which cambium does not index by construction and which recall therefore ignores.

#### `.bzl` load-closure oracle

No change set changed a `.bzl` file.

