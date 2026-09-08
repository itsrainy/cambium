//! The reverse-dependency oracle.
//!
//! bazel-diff is a *comparison*, not a ground truth: where the two tools
//! disagree, neither is authoritative. Bazel itself is. So for every
//! incremental change set the suite takes **every** changed path that belongs
//! to a package at head, names it as a source label, and asks Bazel one
//! question:
//!
//! ```text
//! kind(rule, rdeps(//..., set(//pkg:a.cc //pkg:defs.bzl //other:BUILD.bazel …)))
//! ```
//!
//! Every rule in that answer must be in cambium's affected set. Extra targets
//! on cambium's side are over-approximation (allowed, and measured as
//! precision); a rule Bazel reaches that cambium does not is unsoundness, and
//! fails the change set.
//!
//! ## Why the whole change set and not a sample
//!
//! The oracle used to sample k members of cambium's *direct* set and ask for
//! their `rdeps`. That is a check of cambium's reverse walk given the seeds it
//! chose, and it is blind to the interesting half of the problem: a path
//! cambium never seeded at all. `tools/build_defs/repo/utils.bzl` is listed in
//! the `srcs` of fifteen filegroups in `bazel/bazel`; when a `.bzl` only ever
//! went to the load closure, none of those filegroups was direct, none was in
//! cambium's answer, and — because the roots were drawn from cambium's own
//! direct set — no query the oracle made could have named them. Seeding from
//! the *changed paths* instead makes the oracle independent of cambium's
//! seeding decisions, which is the only way it can catch one being wrong.
//!
//! ## Batching
//!
//! `rdeps(//..., set(a b c))` is the union of the individual answers, so the
//! labels go out in batches and the number of Bazel invocations is
//! `ceil(n / batch)` rather than `n`. A batch is capped both by label count
//! ([`MAX_BATCH_LABELS`]) and by the byte length of the expression
//! ([`MAX_EXPR_BYTES`]), because the expression travels on the command line and
//! a change set that renames a thousand files would otherwise overrun
//! `ARG_MAX`. When a batch fails outright it is bisected down to the single
//! label that Bazel would not evaluate, so one bad label costs its own answer
//! and not its neighbours'.
//!
//! ## Deleted files
//!
//! A file deleted between base and head cannot be named at head: `//pkg:gone.cc`
//! is not a target there and the query would fail. Those paths are taken from
//! the **base** index instead — its `file -> consumers` rows say which rules
//! read the file before it went away — and each such consumer must be in
//! cambium's answer, or its package must be (a rule deleted along with its
//! source is not in anyone's answer at head, and asking for the label itself
//! would fail a change set for being right). The base index is not always in
//! hand; when it is not, the deleted half is skipped and
//! [`RdepsResult::deleted_checked`] says so.
//!
//! ## The escape hatch
//!
//! `--oracle-sample N` caps how many source labels are queried, seeded and
//! sorted so a failure reproduces from the run's seed. It exists for subjects
//! whose change sets are enormous; `0`, the default, is the full oracle and is
//! what every published number must come from.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;
use cambium_bazel::Bazel;

use crate::tools::bazel_query;
use cambium_classify::PackageSet;
use cambium_index::IndexRef;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

/// Most source labels in one `rdeps` query.
pub const MAX_BATCH_LABELS: usize = 500;

/// Most bytes of query expression in one invocation. macOS caps a process's
/// whole argument block at 256 KiB and Bazel's own flags need room, so this
/// leaves an order of magnitude of headroom rather than sailing close.
pub const MAX_EXPR_BYTES: usize = 96 * 1024;

/// What the oracle found.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RdepsResult {
    /// The source labels the query asked about, sorted.
    pub roots: Vec<String>,
    /// True when `--oracle-sample` capped the roots below the change set.
    #[serde(default)]
    pub sampled: bool,
    /// How many Bazel invocations the roots cost.
    #[serde(default)]
    pub batches: usize,
    /// The main-repository rules Bazel reached from the roots.
    ///
    /// Not serialised: on a large subject this is most of the repository, and
    /// what a reader needs from a stored result is the sizes and the failures,
    /// both of which are here.
    #[serde(default, skip_serializing)]
    pub oracle_set: BTreeSet<String>,
    /// `oracle_set.len()`, which survives serialisation.
    #[serde(default)]
    pub oracle_labels: usize,
    /// `|oracle_set ∩ cambium's affected set|`. The numerator of recall.
    #[serde(default)]
    pub matched: usize,
    /// Rules the oracle reached that cambium's answer does not contain, plus
    /// any unsatisfied consumer of a deleted file. Unsoundness; empty means
    /// pass.
    pub missing: Vec<String>,
    /// The `@repo//...` half of Bazel's answer.
    ///
    /// `rdeps(//..., …)` takes `//...` as the *universe expression*, and Bazel
    /// expands a universe to its transitive closure — so the answer contains
    /// external targets that depend on a root, and on envoy that is most of it
    /// (157 of the 173 labels on one change set, all `@quiche//...`). cambium
    /// indexes `//...` by construction and can never contain them, so counting
    /// them as unsoundness would measure the vocabulary difference the
    /// `bazeldiff_extra_external` class already names on the tool side. They
    /// are kept here so the number is visible rather than silently dropped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_external: Vec<String>,
    /// The subset of [`RdepsResult::missing`] that Bazel reaches only by
    /// leaving `//...` and coming back.
    ///
    /// This is a real cambium miss and it fails the change set; the field says
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
    /// climbs back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_route: Vec<String>,
    /// Whether the deleted half of the change set could be checked at all: it
    /// needs the base index, which the caller does not always have.
    #[serde(default)]
    pub deleted_checked: bool,
    /// Deleted paths that had a package at base and a label the base index
    /// knew.
    #[serde(default)]
    pub deleted_paths: usize,
    /// Rules that consumed a deleted file at base. Each must be in cambium's
    /// answer, or its package must be.
    #[serde(default)]
    pub deleted_consumers: usize,
    /// The consumers that were neither. A subset of
    /// [`RdepsResult::missing`], repeated so a failure says which half it
    /// came from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted_missing: Vec<String>,
    /// Changed paths that belong to no package at head, and so have no label
    /// to ask about. Not a failure: a file outside every package cannot be a
    /// rule's input.
    #[serde(default)]
    pub unowned_paths: usize,
    /// Labels whose batch failed even after bisection, and were skipped.
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

impl RdepsResult {
    pub fn ok(&self) -> bool {
        self.missing.is_empty()
    }

    /// Fraction of the oracle's answer cambium reproduces. 1.0 is required.
    ///
    /// Over the main repository only, for the reason
    /// [`RdepsResult::missing_external`] gives, and over the queried half
    /// only: a deleted file's consumers are satisfied by their package as well
    /// as by their label, which is a containment question and not a ratio. An
    /// unsatisfied one still fails the change set through
    /// [`RdepsResult::missing`].
    pub fn recall(&self) -> f64 {
        if self.oracle_labels == 0 {
            return 1.0;
        }
        self.matched as f64 / self.oracle_labels as f64
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
    /// Cap on how many source labels are queried. 0 — the default — is the
    /// full change set, which is what the methodology specifies.
    pub sample: usize,
    pub seed: u64,
    /// Most labels in one Bazel invocation.
    pub max_labels: usize,
    /// Most bytes of query expression in one Bazel invocation.
    pub max_expr_bytes: usize,
}

impl Default for RdepsOracle {
    fn default() -> Self {
        Self {
            sample: 0,
            seed: 0,
            max_labels: MAX_BATCH_LABELS,
            max_expr_bytes: MAX_EXPR_BYTES,
        }
    }
}

/// What the oracle needs to know about one change set, beyond the Bazel handle.
pub struct OracleInputs<'a> {
    /// The index at head; its package list decides which changed paths have a
    /// label, and the workspace on disk decides which still exist.
    pub head: IndexRef<'a>,
    /// The index at base, for the deleted half. `None` skips it.
    pub base: Option<IndexRef<'a>>,
    /// Every path git reports changed between base and head.
    pub changed: &'a BTreeSet<String>,
    /// cambium's affected set: rule labels, the thing under test.
    pub cambium: &'a BTreeSet<String>,
    /// The packages cambium marked dirty for this change set.
    ///
    /// Only the deleted half reads it, and it is the *whole* answer for a
    /// package deleted outright: every rule in it is tombstoned, so cambium
    /// rightly emits no label carrying that package, and the package name
    /// survives nowhere else in the report. Without this the escape hatch in
    /// [`deleted_consumers`] — "the rule, or its package" — is unsatisfiable
    /// by construction for exactly the case it was written for.
    pub dirty_packages: &'a BTreeSet<String>,
}

impl RdepsOracle {
    pub fn new(sample: usize, seed: u64) -> Self {
        Self {
            sample,
            seed,
            ..Self::default()
        }
    }

    /// Runs the oracle over one change set.
    pub fn run(&self, bazel: &Bazel, inputs: &OracleInputs<'_>) -> Result<RdepsResult> {
        let mut result = RdepsResult::default();
        let packages = PackageSet::from_index(inputs.head);
        let ws = bazel.workspace();

        let mut deleted: Vec<&String> = Vec::new();
        let mut roots: Vec<String> = Vec::new();
        for path in inputs.changed {
            if !exists_at_head(ws, path) {
                deleted.push(path);
                continue;
            }
            match packages.owner_of(path) {
                Some(pkg) => roots.push(source_label(pkg, path)),
                None => result.unowned_paths += 1,
            }
        }
        roots.sort();
        roots.dedup();
        let all = roots.len();
        if self.sample > 0 && all > self.sample {
            roots = sample(&roots, self.sample, self.seed);
            result.sampled = true;
        }
        result.roots = roots.clone();

        let mut answer = BTreeSet::new();
        for batch in batches(&roots, self.max_labels, self.max_expr_bytes) {
            self.run_batch(bazel, batch, &mut answer, &mut result);
        }

        let (main, external): (BTreeSet<String>, BTreeSet<String>) =
            answer.into_iter().partition(|l| !is_external(l));
        result.oracle_labels = main.len();
        result.matched = main.intersection(inputs.cambium).count();
        result.missing = main.difference(inputs.cambium).cloned().collect();
        result.missing_external = external
            .into_iter()
            .filter(|l| !inputs.cambium.contains(l))
            .collect();
        result.oracle_set = main;
        result.external_route = external_routes(bazel, &result.roots, &result.missing);

        if let Some(base) = inputs.base {
            result.deleted_checked = true;
            let deleted_missing = deleted_consumers(
                base,
                &deleted,
                inputs.cambium,
                inputs.dirty_packages,
                &mut result,
            );
            // They gate the change set exactly as a queried miss does.
            result.missing.extend(deleted_missing.iter().cloned());
            result.deleted_missing = deleted_missing;
            result.missing.sort();
            result.missing.dedup();
        }
        Ok(result)
    }

    fn run_batch(
        &self,
        bazel: &Bazel,
        batch: &[String],
        answer: &mut BTreeSet<String>,
        result: &mut RdepsResult,
    ) {
        result.batches += 1;
        match query_rdeps(bazel, batch) {
            Ok(Batch { labels, partial }) => {
                answer.extend(labels);
                if partial {
                    result.partial = true;
                    result.partial_batches += 1;
                }
            }
            Err(e) if batch.len() > 1 => {
                // One label Bazel will not evaluate must not cost its
                // neighbours their answer, and finding it by bisection costs
                // log(n) extra invocations rather than n.
                tracing::warn!(
                    labels = batch.len(),
                    error = format!("{e:#}"),
                    "rdeps batch failed; bisecting"
                );
                let (a, b) = batch.split_at(batch.len() / 2);
                self.run_batch(bazel, a, answer, result);
                self.run_batch(bazel, b, answer, result);
            }
            Err(e) => {
                tracing::warn!(
                    root = ?batch.first(),
                    error = format!("{e:#}"),
                    "rdeps query failed for a single label; skipping it"
                );
                result.skipped_roots.extend(batch.iter().cloned());
            }
        }
    }
}

/// Does the path still exist in the head workspace?
///
/// git's diff says a path changed, not whether it survived; the head checkout
/// is on disk and is the cheapest authority. A path that is gone gets the
/// base-index treatment instead of a label Bazel would reject.
fn exists_at_head(workspace: &Path, path: &str) -> bool {
    workspace.join(path).exists()
}

/// `//pkg:rel`, the source-file target a workspace-relative path names.
pub fn source_label(package: &str, path: &str) -> String {
    let rel = match package.is_empty() {
        true => path,
        false => path
            .strip_prefix(package)
            .and_then(|r| r.strip_prefix('/'))
            .unwrap_or(path),
    };
    format!("//{package}:{rel}")
}

/// The package half of `//pkg:name`, for the deleted-file containment check.
pub fn package_of_label(label: &str) -> Option<&str> {
    label.strip_prefix("//")?.split_once(':').map(|(p, _)| p)
}

/// Consumers of the deleted paths that cambium's answer does not account for.
///
/// A rule that read a file which no longer exists must show up somewhere in
/// cambium's answer — either as itself, or, when the rule was deleted along
/// with its source and so exists at neither end, as its package. Requiring the
/// label alone would fail a change set for being right about a target that is
/// gone.
///
/// "Its package" means the affected set *or* `dirty_packages`. A package
/// deleted in its entirety only ever has the latter: cambium tombstones every
/// rule in it and correctly reports none of them, so reading packages off the
/// affected labels alone would fail every wholly-deleted package. That was the
/// whole of sourcegraph's and apollo's change-set failures on the 2026-09-08
/// published run.
fn deleted_consumers(
    base: IndexRef<'_>,
    deleted: &[&String],
    cambium: &BTreeSet<String>,
    dirty_packages: &BTreeSet<String>,
    result: &mut RdepsResult,
) -> Vec<String> {
    let packages = PackageSet::from_index(base);
    // Both spellings of "cambium accounted for this package": a surviving
    // affected target inside it, or the package itself in the dirty set. A
    // package deleted outright has only the second — all its rules are
    // tombstoned and none can appear in the affected set.
    let cambium_packages: BTreeSet<&str> = cambium
        .iter()
        .filter_map(|l| package_of_label(l))
        .chain(dirty_packages.iter().map(String::as_str))
        .collect();
    let mut consumers = BTreeSet::new();
    for path in deleted {
        let Some(pkg) = packages.owner_of(path) else {
            continue;
        };
        let label = source_label(pkg, path);
        let Some(id) = base.id_of(&label) else {
            continue;
        };
        result.deleted_paths += 1;
        for &c in base.file_consumers(id) {
            if base.is_rule(c) && !base.is_tombstone(c) {
                consumers.insert(base.label(c).to_string());
            }
        }
    }
    result.deleted_consumers = consumers.len();
    consumers
        .into_iter()
        .filter(|l| {
            !cambium.contains(l)
                && !package_of_label(l).is_some_and(|p| cambium_packages.contains(p))
        })
        .collect()
}

/// A label outside the main repository, in either spelling Bazel uses.
pub fn is_external(label: &str) -> bool {
    label.starts_with('@') || label.starts_with("//external:")
}

/// How many missing labels the external-route probe is willing to ask Bazel
/// about. A failing change set's missing set is tens of labels and one
/// `somepath` on envoy is seconds; the probe is diagnosis, not a gate, so it is
/// capped.
pub const MAX_ROUTE_PROBES: usize = 25;

/// How many roots the probe's `somepath` target set carries. The full oracle's
/// root list can be thousands of labels; a probe is a hint about *why* a label
/// is missing, so it takes the first batch's worth and no more.
pub const MAX_ROUTE_ROOTS: usize = MAX_BATCH_LABELS;

/// Which of `missing` Bazel reaches only by leaving `//...`.
///
/// One `somepath(label, set(r1 r2 …))` per label: Bazel returns *a* shortest
/// path, and if that path contains an `@repo//...` node then the only way from
/// this target down to a root is through an external repository, which is
/// exactly the edge cambium's `//...`-only index does not carry. A path with
/// no external node means something else is wrong and the label is left
/// unexplained — the safe direction, since either way it fails the change set.
fn external_routes(bazel: &Bazel, roots: &[String], missing: &[String]) -> Vec<String> {
    if roots.is_empty() || missing.is_empty() {
        return Vec::new();
    }
    let head = &roots[..roots.len().min(MAX_ROUTE_ROOTS)];
    let union = label_set(head);
    let mut out = Vec::new();
    for label in missing.iter().take(MAX_ROUTE_PROBES) {
        // A probe that will not run says nothing either way, and the caller
        // leaves the label unexplained -- the safe direction, since it fails
        // the change set whichever way this lands.
        let expr = format!("somepath({label}, {union})");
        let Ok((path, _partial)) = bazel_query::labels(bazel, &expr, "somepath") else {
            continue;
        };
        if path.iter().any(|l| is_external(l)) {
            out.push(label.clone());
        }
    }
    out
}

/// A deterministic sample of at most `k` labels, sorted so the JSON is stable.
pub fn sample(labels: &[String], k: usize, seed: u64) -> Vec<String> {
    let mut all: Vec<String> = labels.to_vec();
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

/// Splits the roots into query-sized batches: at most `max_labels` each, and
/// at most `max_bytes` of expression each. A single label longer than the byte
/// cap still gets its own batch — dropping it would silently weaken the
/// oracle, and Bazel will say if it is really too long.
pub fn batches(roots: &[String], max_labels: usize, max_bytes: usize) -> Vec<&[String]> {
    let max_labels = max_labels.max(1);
    let mut out = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for i in 0..roots.len() {
        let cost = roots[i].len() + 1;
        let full = i > start && (i - start >= max_labels || bytes + cost > max_bytes);
        if full {
            out.push(&roots[start..i]);
            start = i;
            bytes = 0;
        }
        bytes += cost;
    }
    if start < roots.len() {
        out.push(&roots[start..]);
    }
    out
}

/// `set(a b c)`: Bazel's literal set, which needs no operator precedence and
/// no quoting of the individual labels.
pub fn label_set(labels: &[String]) -> String {
    format!("set({})", labels.join(" "))
}

/// `kind(rule, rdeps(//..., set(a b c)))`: every rule that reaches any of the
/// changed files.
///
/// The `kind(rule, ...)` wrapper is not cosmetic. Bazel's `rdeps` answer
/// includes *file* targets — the generated `//lib:lib.out` of a genrule, the
/// `//pkg:BUILD.bazel` of a package, and the roots themselves — while cambium's
/// walk emits rules only (`cambium_index::walk` filters on `NodeKind::Rule`).
/// Without the wrapper every changed file in the change set reads as a target
/// Bazel reached and cambium missed, which is the one thing this oracle is
/// supposed to detect, and the failure would be pure vocabulary. The same
/// restriction is applied to the other tools' answers
/// (`ToolRun::restrict_to_rules`), so all three are compared over one
/// universe.
pub fn rdeps_expression(roots: &[String]) -> String {
    format!("kind(rule, rdeps(//..., {}))", label_set(roots))
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
/// flagged [`RdepsResult::partial`]. Any other non-zero exit is a real
/// failure and sends the batch to the bisection in
/// [`RdepsOracle::run_batch`].
fn query_rdeps(bazel: &Bazel, roots: &[String]) -> Result<Batch> {
    let expr = rdeps_expression(roots);
    let (labels, partial) = bazel_query::labels(bazel, &expr, &crate::tools::tail(&expr, 1))?;
    if partial {
        tracing::warn!(
            roots = roots.len(),
            labels = labels.len(),
            "the rdeps query exited 3 under --keep_going; the oracle answer for this batch is a lower bound"
        );
    }
    Ok(Batch { labels, partial })
}

/// Every rule target `//...` names, at whatever revision the workspace is on.
///
/// This is the *size of cambium's answer* on a globally invalidated change set:
/// it says `//...`, and `//...` is this set. Without the number the report can
/// only say "cambium went global", which hides the thing a reader actually
/// wants — bazel-diff found M targets, cambium is asking the user to build N,
/// and N/M is what a global costs.
///
/// It doubles as the rule filter for that change set's other tools
/// ([`crate::tools::ToolRun::restrict_to_rule_set`]), so the one query
/// serves both and the two sides stay in one vocabulary.
///
/// Exit 3 is accepted for the reason [`query_rdeps`] gives: a partial universe
/// under `--keep_going` under-counts N, which understates cambium's
/// over-approximation rather than flattering it.
pub fn rule_universe(bazel: &Bazel) -> Result<BTreeSet<String>> {
    let (labels, partial) = bazel_query::labels(bazel, "kind(rule, //...)", "kind(rule, //...)")?;
    if partial {
        tracing::warn!(
            labels = labels.len(),
            "the universe query exited 3 under --keep_going; the global answer size is a lower bound"
        );
    }
    Ok(labels)
}


#[cfg(test)]
mod tests {
    use super::*;
    use cambium_index::{IndexBuilder, PackageUpdate, TargetRow};

    fn labels(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("//p:t{i}")).collect()
    }

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_sample_is_seeded_and_capped() {
        let all = labels(50);
        let a = sample(&all, 5, 7);
        assert_eq!(a.len(), 5);
        assert_eq!(a, sample(&all, 5, 7), "the same seed picks the same labels");
        assert_ne!(a, sample(&all, 5, 8));
        assert!(a.windows(2).all(|w| w[0] < w[1]), "sorted: {a:?}");
        assert_eq!(sample(&all, 100, 7).len(), 50, "k above the population");
        assert!(sample(&all, 0, 7).is_empty(), "k = 0 samples nothing");
    }

    #[test]
    fn a_path_becomes_the_source_label_of_its_package() {
        assert_eq!(source_label("lib", "lib/a.cc"), "//lib:a.cc");
        assert_eq!(source_label("lib", "lib/sub/a.cc"), "//lib:sub/a.cc");
        assert_eq!(source_label("", "a.cc"), "//:a.cc");
        assert_eq!(package_of_label("//lib:sub/a.cc"), Some("lib"));
        assert_eq!(package_of_label("//:a"), Some(""));
        assert_eq!(package_of_label("@ext//:a"), None);
    }

    #[test]
    fn roots_go_out_as_a_literal_set() {
        assert_eq!(
            rdeps_expression(&["//a:a.cc".into(), "//b:b.cc".into()]),
            "kind(rule, rdeps(//..., set(//a:a.cc //b:b.cc)))"
        );
    }

    #[test]
    fn batching_respects_both_the_count_and_the_byte_cap() {
        let all = labels(10);
        assert_eq!(batches(&all, 100, 1 << 20).len(), 1, "one batch when small");
        let by_count = batches(&all, 3, 1 << 20);
        assert_eq!(by_count.len(), 4);
        assert_eq!(by_count[0].len(), 3);
        assert_eq!(by_count[3].len(), 1);
        // Every label is in exactly one batch, in order.
        let flat: Vec<String> = by_count.iter().flat_map(|b| b.to_vec()).collect();
        assert_eq!(flat, all);
        // "//p:t0" is 6 bytes, so 15 bytes holds two of them and not three.
        let by_bytes = batches(&all, 100, 15);
        assert!(by_bytes.iter().all(|b| b.len() == 2), "{by_bytes:?}");
        assert!(batches(&[], 10, 10).is_empty());
        // A label alone over the cap still gets asked about.
        assert_eq!(batches(&labels(1), 10, 1).len(), 1);
    }

    #[test]
    fn recall_is_one_when_the_oracle_is_empty_and_a_fraction_otherwise() {
        let ours: BTreeSet<String> = ["//a:a".to_string()].into_iter().collect();
        assert_eq!(recall(&ours, &BTreeSet::new()), 1.0);
        let theirs = set(&["//a:a", "//b:b"]);
        assert_eq!(recall(&ours, &theirs), 0.5);
        assert_eq!(recall(&theirs, &theirs), 1.0);
        let r = RdepsResult {
            oracle_labels: 4,
            matched: 3,
            ..Default::default()
        };
        assert_eq!(r.recall(), 0.75);
        assert_eq!(RdepsResult::default().recall(), 1.0);
    }

    // ---- the stub Bazel ----
    //
    // `Bazel::at` runs whatever binary it is given, so a script that ignores
    // its arguments and prints a fixed answer is a complete Bazel as far as
    // the oracle is concerned. That is enough to drive `run` end to end: what
    // is under test here is which labels are asked about and how the answer is
    // scored, not Bazel.
    #[cfg(unix)]
    fn stub_bazel(dir: &Path, answer: &str) -> Bazel {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("stub-bazel");
        std::fs::write(&bin, format!("#!/bin/sh\ncat <<'EOF'\n{answer}\nEOF\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Bazel::at(dir.to_path_buf(), Some(bin.to_str().unwrap()))
    }

    /// The scenario of 11ae8e0: `defs/defs.bzl` is listed in the `srcs` of a
    /// filegroup in its own package. Bazel's `rdeps` names the filegroup; if
    /// cambium's answer has only the packages that *load* the file, the
    /// filegroup is missed.
    fn bzl_filegroup_index() -> cambium_index::IndexData {
        let mut b = IndexBuilder::new(cambium_index::new_lineage());
        b.apply_packages(&[
            PackageUpdate {
                package: "defs".into(),
                targets: vec![
                    TargetRow::rule(
                        "//defs:bzl_srcs",
                        "filegroup",
                        vec!["//defs:defs.bzl".into()],
                    ),
                    TargetRow::source_file("//defs:defs.bzl"),
                ],
                ..Default::default()
            },
            PackageUpdate {
                package: "app".into(),
                targets: vec![TargetRow::rule("//app:app", "genrule", vec![])],
                ..Default::default()
            },
        ]);
        b.finish("head")
    }

    #[cfg(unix)]
    #[test]
    fn a_filegroup_over_a_changed_bzl_is_missed_when_cambium_leaves_it_out() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("defs")).unwrap();
        std::fs::write(ws.join("defs/defs.bzl"), "def f():\n    pass\n").unwrap();
        // Bazel reaches the filegroup from the changed `.bzl`; the loading
        // package's rule is reached through the load, which `rdeps` over a
        // source label does not see, so the oracle names only the filegroup.
        let bazel = stub_bazel(ws, "//defs:bzl_srcs");
        let idx = bzl_filegroup_index();
        let changed = set(&["defs/defs.bzl"]);

        // cambium answers with the loading package only: the miss.
        let loads_only = set(&["//app:app"]);
        let r = RdepsOracle::default()
            .run(
                &bazel,
                &OracleInputs {
                    head: idx.view(),
                    base: None,
                    changed: &changed,
                    cambium: &loads_only,
                    dirty_packages: &BTreeSet::new(),
                },
            )
            .unwrap();
        assert_eq!(
            r.roots,
            vec!["//defs:defs.bzl"],
            "the changed path is the root"
        );
        assert_eq!(r.missing, vec!["//defs:bzl_srcs"]);
        assert_eq!(r.recall(), 0.0);
        assert!(
            !r.ok(),
            "a filegroup Bazel reaches and cambium does not fails"
        );

        // With the file's consumers in the answer, the same oracle passes.
        let with_consumers = set(&["//app:app", "//defs:bzl_srcs"]);
        let r = RdepsOracle::default()
            .run(
                &bazel,
                &OracleInputs {
                    head: idx.view(),
                    base: None,
                    changed: &changed,
                    cambium: &with_consumers,
                    dirty_packages: &BTreeSet::new(),
                },
            )
            .unwrap();
        assert!(r.missing.is_empty());
        assert_eq!(r.recall(), 1.0);
        assert!(r.ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_path_in_no_package_is_counted_and_not_queried() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("docs")).unwrap();
        std::fs::write(ws.join("docs/README.md"), "hi\n").unwrap();
        let bazel = stub_bazel(ws, "");
        let idx = bzl_filegroup_index();
        let changed = set(&["docs/README.md"]);
        let r = RdepsOracle::default()
            .run(
                &bazel,
                &OracleInputs {
                    head: idx.view(),
                    base: None,
                    changed: &changed,
                    cambium: &BTreeSet::new(),
                    dirty_packages: &BTreeSet::new(),
                },
            )
            .unwrap();
        assert!(r.roots.is_empty());
        assert_eq!(r.unowned_paths, 1);
        assert_eq!(r.batches, 0, "no roots, no query");
        assert!(r.ok());
    }

    #[cfg(unix)]
    #[test]
    fn external_labels_are_split_off_and_left_out_of_recall() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("defs")).unwrap();
        std::fs::write(ws.join("defs/defs.bzl"), "x\n").unwrap();
        let bazel = stub_bazel(ws, "//defs:bzl_srcs\n@quiche//:x\n//external:y");
        let idx = bzl_filegroup_index();
        let r = RdepsOracle::default()
            .run(
                &bazel,
                &OracleInputs {
                    head: idx.view(),
                    base: None,
                    changed: &set(&["defs/defs.bzl"]),
                    cambium: &set(&["//defs:bzl_srcs"]),
                    dirty_packages: &BTreeSet::new(),
                },
            )
            .unwrap();
        assert!(r.missing.is_empty(), "{:?}", r.missing);
        assert_eq!(r.missing_external, vec!["//external:y", "@quiche//:x"]);
        assert_eq!(r.recall(), 1.0, "the external half is not counted");
        assert!(r.ok());
    }

    fn base_index_with_a_consumed_source() -> cambium_index::IndexData {
        let mut b = IndexBuilder::new(cambium_index::new_lineage());
        b.apply_packages(&[PackageUpdate {
            package: "lib".into(),
            targets: vec![
                TargetRow::rule("//lib:core", "cc_library", vec!["//lib:gone.cc".into()]),
                TargetRow::source_file("//lib:gone.cc"),
            ],
            ..Default::default()
        }]);
        b.finish("base")
    }

    #[cfg(unix)]
    #[test]
    fn a_deleted_files_consumers_come_from_the_base_index() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        // `lib/gone.cc` is not on disk: that is what makes it deleted.
        let bazel = stub_bazel(ws, "");
        let head = bzl_filegroup_index();
        let base = base_index_with_a_consumed_source();
        let changed = set(&["lib/gone.cc"]);
        let run = |cambium: &BTreeSet<String>, dirty: &BTreeSet<String>| {
            RdepsOracle::default()
                .run(
                    &bazel,
                    &OracleInputs {
                        head: head.view(),
                        base: Some(base.view()),
                        changed: &changed,
                        cambium,
                        dirty_packages: dirty,
                    },
                )
                .unwrap()
        };
        let none = BTreeSet::new();

        let r = run(&none, &none);
        assert!(r.roots.is_empty(), "a deleted path cannot be named at head");
        assert!(r.deleted_checked);
        assert_eq!(r.deleted_paths, 1);
        assert_eq!(r.deleted_consumers, 1);
        assert_eq!(r.deleted_missing, vec!["//lib:core"]);
        assert_eq!(r.missing, vec!["//lib:core"]);
        assert!(!r.ok());

        // The label itself satisfies it.
        assert!(run(&set(&["//lib:core"]), &none).ok());
        // So does anything else in its package: the rule may have been deleted
        // with its source, and then it is in nobody's answer at head.
        assert!(run(&set(&["//lib:other"]), &none).ok());
        // And so does the dirty package alone, with an empty affected set.
        //
        // This is the wholly-deleted package: every rule in `lib` is
        // tombstoned, so cambium rightly reports no label carrying the
        // package and `dirty_packages` is the only place its name survives.
        // Before the dirty set was consulted this case could not be satisfied
        // at all, and it failed every such change set on the 2026-09-08
        // published run — `//internal/search/smartsearch` on sourcegraph,
        // `//modules/drivers/lidar/vanjeelidar` on apollo.
        assert!(run(&none, &set(&["lib"])).ok());
        // A different package does not satisfy it.
        assert!(!run(&none, &set(&["other"])).ok());
        // And with no base index the half is skipped rather than failed.
        let r = RdepsOracle::default()
            .run(
                &bazel,
                &OracleInputs {
                    head: head.view(),
                    base: None,
                    changed: &changed,
                    cambium: &BTreeSet::new(),
                    dirty_packages: &BTreeSet::new(),
                },
            )
            .unwrap();
        assert!(!r.deleted_checked);
        assert!(r.ok());
    }

    #[cfg(unix)]
    #[test]
    fn the_sample_escape_hatch_caps_the_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("defs")).unwrap();
        let mut changed = BTreeSet::new();
        for i in 0..10 {
            std::fs::write(ws.join(format!("defs/f{i}.bzl")), "x\n").unwrap();
            changed.insert(format!("defs/f{i}.bzl"));
        }
        let bazel = stub_bazel(ws, "");
        let idx = bzl_filegroup_index();
        let inputs = OracleInputs {
            head: idx.view(),
            base: None,
            changed: &changed,
            cambium: &changed,
            dirty_packages: &BTreeSet::new(),
        };
        let full = RdepsOracle::default().run(&bazel, &inputs).unwrap();
        assert_eq!(full.roots.len(), 10);
        assert!(!full.sampled);
        let capped = RdepsOracle::new(3, 7).run(&bazel, &inputs).unwrap();
        assert_eq!(capped.roots.len(), 3);
        assert!(capped.sampled);
    }
}
