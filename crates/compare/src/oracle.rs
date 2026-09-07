//! The sampled `rdeps` oracle.
//!
//! bazel-diff is a *comparison*, not a ground truth: where the two tools
//! disagree, neither is authoritative. Bazel itself is. So for every pair the
//! suite samples k members of cambium's direct set, asks Bazel
//! `rdeps(//..., <root>)` for each, and requires cambium's own reverse walk
//! from the same roots to be a **superset** of the union. Extra targets on
//! cambium's side are over-approximation (allowed, and measured as precision);
//! a target Bazel reaches that cambium does not is unsoundness, and fails.
//!
//! Two details that are load-bearing:
//!
//! - **The sample is seeded** (`rand_chacha`, a stable algorithm across
//!   releases unlike `rand`'s thread rng), so a nightly failure can be
//!   reproduced exactly from the run's seed.
//! - **Roots are batched.** One Bazel invocation per root is the obvious
//!   implementation and is unusably slow at k = 100 on a repository the size
//!   of Envoy — each call re-walks the universe. `rdeps(//..., a + b + c)` is
//!   the union of the individual answers, which is exactly what the oracle
//!   needs, so roots go out in chunks and k costs `k / CHUNK` invocations.
//!   Chunks stay small because one bad root (a label Bazel refuses to
//!   evaluate) takes its whole chunk down.

use std::collections::BTreeSet;

use anyhow::{Context as _, Result};
use cambium_bazel::Bazel;
use cambium_index::{IndexRef, WalkOptions};
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

/// How many roots go into one `rdeps` query.
pub const DEFAULT_CHUNK: usize = 25;

/// What the oracle found.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OracleResult {
    /// The sampled direct-set members the queries started from.
    pub roots: Vec<String>,
    /// The union of `rdeps(//..., root)` over the roots, as Bazel reports it.
    pub oracle_set: BTreeSet<String>,
    /// cambium's reverse closure over the head index from the same roots.
    pub cambium_set_from_roots: BTreeSet<String>,
    /// `oracle_set - cambium_set_from_roots`, restricted to the main
    /// repository: unsoundness. Empty means pass.
    pub missing: Vec<String>,
    /// The `@repo//...` half of that difference.
    ///
    /// `rdeps(//..., root)` takes `//...` as the *universe expression*, and
    /// Bazel expands a universe to its transitive closure — so the answer
    /// contains external targets that depend on the root, and on envoy that is
    /// most of it (157 of the 173 labels on one pair, all `@quiche//...`).
    /// cambium indexes `//...` by construction and can never contain them, so
    /// counting them as unsoundness measures the vocabulary difference the
    /// `bazeldiff_extra_external` class already names on the producer side.
    /// They are kept here so the number is visible rather than silently
    /// dropped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_external: Vec<String>,
    /// The subset of [`OracleResult::missing`] that Bazel reaches only by
    /// leaving `//...` and coming back.
    ///
    /// This is a real cambium miss and it fails the pair; the field says
    /// *why*, so a failure that is the known external-route limitation is
    /// distinguishable from one that is a walk bug. envoy has two instances at
    /// the pin:
    ///
    /// ```text
    /// somepath(//distribution/dockerhub:readme_json, //changelogs:changelogs)
    ///   //distribution/dockerhub:readme_json
    ///   @envoy_repo//:project
    ///   //changelogs:changelogs
    /// ```
    ///
    /// and `//source/common/quic:*` -> `@quiche//:*` ->
    /// `@envoy//source/common/quic/platform:*` (envoy ships quiche's `BUILD`
    /// file and points its `label_flag`s back at main-repo platform impls).
    /// cambium stops at the first external node, so the reverse walk never
    /// climbs back. Indexing external repositories is the precise answer; an
    /// invalidator is the sound one where the changed path is nameable
    /// (`changelogs/**` covers the first case), and there is no cheap
    /// invalidator for the second.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_route: Vec<String>,
    /// Roots whose query failed (a broken package, say) and were skipped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_roots: Vec<String>,
    /// At least one batch came back from a `--keep_going` query that exited 3:
    /// Bazel could not load everything (an external repository that will not
    /// fetch on this machine, say) but answered for what did load. The oracle
    /// is a *lower bound* on what cambium must contain, so a partial answer is
    /// still sound to check against — it just proves less.
    #[serde(default)]
    pub partial: bool,
    /// How many batches came back partial.
    #[serde(default)]
    pub partial_batches: usize,
}

impl OracleResult {
    pub fn ok(&self) -> bool {
        self.missing.is_empty()
    }

    /// Fraction of the oracle's answer cambium reproduces. 1.0 is required.
    ///
    /// Over the main repository only, for the reason
    /// [`OracleResult::missing_external`] gives.
    pub fn recall(&self) -> f64 {
        let theirs: BTreeSet<String> = self
            .oracle_set
            .iter()
            .filter(|l| !is_external(l))
            .cloned()
            .collect();
        recall(&self.cambium_set_from_roots, &theirs)
    }
}

/// `|ours ∩ theirs| / |theirs|`, and 1.0 when there is nothing to recall — an
/// empty oracle is not evidence of unsoundness.
pub fn recall(ours: &BTreeSet<String>, theirs: &BTreeSet<String>) -> f64 {
    if theirs.is_empty() {
        return 1.0;
    }
    ours.intersection(theirs).count() as f64 / theirs.len() as f64
}

/// The oracle's configuration.
#[derive(Clone, Debug)]
pub struct RdepsOracle {
    /// How many roots to sample. 0 disables the oracle.
    pub k: usize,
    pub seed: u64,
    /// Roots per Bazel invocation.
    pub chunk: usize,
}

impl Default for RdepsOracle {
    fn default() -> Self {
        Self {
            k: 100,
            seed: 0,
            chunk: DEFAULT_CHUNK,
        }
    }
}

impl RdepsOracle {
    pub fn new(k: usize, seed: u64) -> Self {
        Self {
            k,
            seed,
            chunk: DEFAULT_CHUNK,
        }
    }

    /// Runs the oracle over the head index. `direct` is cambium's depth-0 set.
    pub fn run(
        &self,
        bazel: &Bazel,
        head: IndexRef<'_>,
        direct: &[String],
    ) -> Result<OracleResult> {
        let roots = sample(direct, self.k, self.seed);
        let mut result = OracleResult {
            roots: roots.clone(),
            ..Default::default()
        };
        if roots.is_empty() {
            return Ok(result);
        }
        for batch in roots.chunks(self.chunk.max(1)) {
            match query_rdeps(bazel, batch) {
                Ok(Batch { labels, partial }) => {
                    result.oracle_set.extend(labels);
                    if partial {
                        result.partial = true;
                        result.partial_batches += 1;
                    }
                }
                Err(e) => {
                    // One unqueryable root must not lose the whole pair's
                    // oracle; record what was dropped so a suspiciously small
                    // oracle set is explainable.
                    tracing::warn!(roots = ?batch, error = format!("{e:#}"), "rdeps query failed; skipping the batch");
                    result
                        .skipped_roots
                        .extend(batch.iter().map(String::to_string));
                }
            }
        }
        result.cambium_set_from_roots = walk_from_roots(head, &roots);
        let (missing, external): (Vec<String>, Vec<String>) = result
            .oracle_set
            .difference(&result.cambium_set_from_roots)
            .cloned()
            .partition(|l| !is_external(l));
        result.missing = missing;
        result.missing_external = external;
        result.external_route = external_routes(bazel, &roots, &result.missing);
        Ok(result)
    }
}

/// A label outside the main repository, in either spelling Bazel uses.
pub fn is_external(label: &str) -> bool {
    label.starts_with('@') || label.starts_with("//external:")
}

/// How many missing labels the external-route probe is willing to ask Bazel
/// about. A failing pair's missing set is tens of labels and one `somepath` on
/// envoy is seconds; the probe is diagnosis, not a gate, so it is capped.
pub const MAX_ROUTE_PROBES: usize = 25;

/// Which of `missing` Bazel reaches only by leaving `//...`.
///
/// One `somepath(label, r1 + r2 + …)` per label: Bazel returns *a* shortest
/// path, and if that path contains an `@repo//...` node then the only way from
/// this target down to a root is through an external repository, which is
/// exactly the edge cambium's `//...`-only index does not carry. A path with
/// no external node means something else is wrong and the label is left
/// unexplained — the safe direction, since either way it fails the pair.
fn external_routes(bazel: &Bazel, roots: &[String], missing: &[String]) -> Vec<String> {
    let union = roots.join(" + ");
    let mut out = Vec::new();
    for label in missing.iter().take(MAX_ROUTE_PROBES) {
        let expr = format!("somepath({label}, {union})");
        let Ok(o) = bazel
            .command("query")
            .args([
                expr.as_str(),
                "--output=label",
                "--order_output=no",
                "--keep_going",
                "--noshow_progress",
                "--ui_event_filters=-info",
            ])
            .output()
        else {
            continue;
        };
        if !o.status.success() && o.status.code() != Some(3) {
            continue;
        }
        if parse_labels(&String::from_utf8_lossy(&o.stdout))
            .iter()
            .any(|l| is_external(l))
        {
            out.push(label.clone());
        }
    }
    out
}

/// A deterministic sample of at most `k` labels, sorted so the JSON is stable.
pub fn sample(direct: &[String], k: usize, seed: u64) -> Vec<String> {
    let mut all: Vec<String> = direct.to_vec();
    all.sort();
    all.dedup();
    if k == 0 {
        return Vec::new();
    }
    if all.len() <= k {
        return all;
    }
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut idx: Vec<usize> = (0..all.len()).collect();
    idx.shuffle(&mut rng);
    let mut chosen: Vec<usize> = idx.into_iter().take(k).collect();
    chosen.sort_unstable();
    chosen.into_iter().map(|i| all[i].clone()).collect()
}

/// `kind(rule, rdeps(//..., a + b + c))`: the union of the roots' reverse
/// closures, restricted to rules.
///
/// The `kind(rule, ...)` wrapper is not cosmetic. Bazel's `rdeps` answer
/// includes *file* targets — the generated `//lib:lib.out` of a genrule, the
/// `//pkg:BUILD.bazel` of a package — while cambium's walk emits rules only
/// (`cambium_index::walk` filters on `NodeKind::Rule`). Without the wrapper
/// every genrule output in the sample reads as a target Bazel reached and
/// cambium missed, which is the one thing this oracle is supposed to detect,
/// and the failure would be pure vocabulary. The same restriction is applied
/// to the producers' answers (`ProducerRun::restrict_to_rules`), so all three
/// tools are compared over one universe.
pub fn rdeps_expression(roots: &[String]) -> String {
    let union = roots.join(" + ");
    format!("kind(rule, rdeps(//..., {union}))")
}

/// cambium's reverse closure from `roots`, over the head index. Roots the
/// index does not know are skipped: they cannot contribute rdeps here, and
/// their absence shows up as `missing` if Bazel disagrees.
pub fn walk_from_roots(head: IndexRef<'_>, roots: &[String]) -> BTreeSet<String> {
    let ids: Vec<u32> = roots.iter().filter_map(|l| head.id_of(l)).collect();
    if ids.is_empty() {
        return BTreeSet::new();
    }
    let r = cambium_index::walk::walk(
        head,
        &ids,
        &WalkOptions {
            // Bazel's `rdeps` does not know about `manual`, so neither may the
            // set we compare against it.
            include_manual: true,
            explain: false,
            max_depth: None,
        },
    );
    cambium_index::walk::labels_sorted(head, &r.affected)
        .into_iter()
        .collect()
}

/// One batch's answer, and whether Bazel had to give up on part of the graph.
struct Batch {
    labels: BTreeSet<String>,
    partial: bool,
}

/// Runs one batched `rdeps` query.
///
/// **Exit 3 is accepted.** Under `--keep_going` Bazel exits 3 when it finished
/// but hit errors — on a real repository that is routine: `bazel/bazel` at
/// these pins references external repositories that cannot be fetched on macOS
/// (a GraalVM distribution with no macos-x64 build, an RBE config that needs
/// Docker), so the transitive closure of `//...` cannot be completed. Bazel
/// still prints the reverse dependencies of everything that *did* load. Since
/// the oracle only ever asserts "cambium must contain all of this", a smaller
/// answer weakens the check but never makes it wrong, so the batch is kept and
/// flagged [`OracleResult::partial`]. Any other non-zero exit is a real
/// failure and takes the batch out.
fn query_rdeps(bazel: &Bazel, roots: &[String]) -> Result<Batch> {
    let expr = rdeps_expression(roots);
    let out = bazel
        .command("query")
        .args([
            expr.as_str(),
            "--output=label",
            // No `--noimplicit_deps`: the index carries implicit edges (see
            // `cambium_bazel::QUERY_FLAGS`), and the oracle has to ask the same
            // question or the two sides are not comparable. Asking it with the
            // implicit edges in is also the stronger check — it is the version
            // that catches a main-repo tool reaching its consumers.
            "--order_output=no",
            "--keep_going",
            "--noshow_progress",
            "--ui_event_filters=-info",
        ])
        .output()
        .context("spawning bazel query rdeps")?;
    let partial = out.status.code() == Some(3);
    anyhow::ensure!(
        out.status.success() || partial,
        "bazel query {} failed with {}\n{}",
        crate::producer::tail(&expr, 1),
        out.status,
        crate::producer::tail(&String::from_utf8_lossy(&out.stderr), 20)
    );
    let labels = parse_labels(&String::from_utf8_lossy(&out.stdout));
    if partial {
        tracing::warn!(
            roots = roots.len(),
            labels = labels.len(),
            "the rdeps query exited 3 under --keep_going; the oracle answer for this batch is a lower bound"
        );
    }
    Ok(Batch { labels, partial })
}

/// One label per line, blank lines dropped.
pub fn parse_labels(out: &str) -> BTreeSet<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cambium_index::{IndexBuilder, PackageUpdate, TargetRow};

    fn labels(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("//p:t{i}")).collect()
    }

    #[test]
    fn the_sample_is_seeded_and_capped() {
        let all = labels(50);
        let a = sample(&all, 5, 7);
        assert_eq!(a.len(), 5);
        assert_eq!(a, sample(&all, 5, 7), "the same seed picks the same roots");
        assert_ne!(a, sample(&all, 5, 8));
        assert!(a.windows(2).all(|w| w[0] < w[1]), "sorted: {a:?}");
        assert_eq!(sample(&all, 100, 7).len(), 50, "k above the population");
        assert!(sample(&all, 0, 7).is_empty(), "k = 0 disables the oracle");
    }

    #[test]
    fn duplicate_roots_are_sampled_once() {
        let all = vec!["//a:a".to_string(), "//a:a".to_string()];
        assert_eq!(sample(&all, 5, 0), vec!["//a:a"]);
    }

    #[test]
    fn roots_batch_into_one_union_query() {
        assert_eq!(
            rdeps_expression(&["//a:a".into(), "//b:b".into()]),
            "kind(rule, rdeps(//..., //a:a + //b:b))"
        );
    }

    #[test]
    fn labels_parse_from_query_output() {
        let got = parse_labels("//a:a\n\n  //b:b \n");
        assert_eq!(got.into_iter().collect::<Vec<_>>(), vec!["//a:a", "//b:b"]);
    }

    #[test]
    fn external_labels_are_split_off_and_left_out_of_recall() {
        let mut r = OracleResult {
            oracle_set: parse_labels("//lib:a\n//lib:b\n@quiche//:x\n//external:y\n"),
            cambium_set_from_roots: parse_labels("//lib:a\n"),
            ..Default::default()
        };
        let (missing, external): (Vec<String>, Vec<String>) = r
            .oracle_set
            .difference(&r.cambium_set_from_roots)
            .cloned()
            .partition(|l| !is_external(l));
        r.missing = missing;
        r.missing_external = external;
        assert_eq!(r.missing, vec!["//lib:b"]);
        assert_eq!(r.missing_external, vec!["//external:y", "@quiche//:x"]);
        // Recall is over `//...` only: one of two main-repo labels.
        assert_eq!(r.recall(), 0.5);
        assert!(!r.ok(), "a main-repo miss still fails");
        // With the main-repo miss gone the external half does not fail it.
        r.missing.clear();
        assert!(r.ok());
    }

    #[test]
    fn recall_is_one_when_the_oracle_is_empty_and_a_fraction_otherwise() {
        let ours: BTreeSet<String> = ["//a:a".to_string()].into_iter().collect();
        assert_eq!(recall(&ours, &BTreeSet::new()), 1.0);
        let theirs: BTreeSet<String> = ["//a:a".to_string(), "//b:b".to_string()]
            .into_iter()
            .collect();
        assert_eq!(recall(&ours, &theirs), 0.5);
        assert_eq!(recall(&theirs, &theirs), 1.0);
    }

    fn index() -> cambium_index::IndexData {
        let mut b = IndexBuilder::new(cambium_index::new_lineage());
        b.apply_packages(&[PackageUpdate {
            package: "lib".into(),
            targets: vec![
                TargetRow::rule("//lib:core", "cc_library", vec!["//lib:a.cc".into()]),
                TargetRow::rule("//lib:mid", "cc_library", vec!["//lib:core".into()]),
                TargetRow::rule("//lib:top", "cc_library", vec!["//lib:mid".into()]),
                TargetRow::source_file("//lib:a.cc"),
            ],
            ..Default::default()
        }]);
        b.finish("c")
    }

    #[test]
    fn the_cambium_side_walks_the_whole_reverse_closure_from_the_roots() {
        let idx = index();
        let got = walk_from_roots(idx.view(), &["//lib:core".into(), "//nope:nope".into()]);
        assert_eq!(
            got.into_iter().collect::<Vec<_>>(),
            vec!["//lib:core", "//lib:mid", "//lib:top"],
            "unknown roots are skipped, known ones bring their rdeps"
        );
        assert!(walk_from_roots(idx.view(), &["//nope:nope".into()]).is_empty());
    }

    #[test]
    fn missing_targets_are_the_ones_bazel_reaches_and_cambium_does_not() {
        let idx = index();
        let mut r = OracleResult {
            roots: vec!["//lib:core".into()],
            oracle_set: parse_labels("//lib:core\n//lib:mid\n//lib:top\n//other:x\n"),
            cambium_set_from_roots: walk_from_roots(idx.view(), &["//lib:core".into()]),
            ..Default::default()
        };
        r.missing = r
            .oracle_set
            .difference(&r.cambium_set_from_roots)
            .cloned()
            .collect();
        assert_eq!(r.missing, vec!["//other:x"]);
        assert!(!r.ok());
        assert_eq!(r.recall(), 0.75);
    }
}
