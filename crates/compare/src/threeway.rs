//! The three-way comparison itself.
//!
//! For one `(base, head)` pair the driver produces three answers and compares
//! them:
//!
//! 1. **cambium**, twice — once with the base index resolved from scratch
//!    (`cold`) and once with it in the store (`warm`) — because the whole
//!    argument for cambium is the warm number, and quoting it without the cold
//!    one would be dishonest.
//! 2. **each producer** ([`crate::producer::Producer`]), typically bazel-diff.
//! 3. **the oracle** ([`crate::oracle`]), a sampled `bazel query rdeps`.
//!
//! cambium vs producer is a *symmetric difference*, classified label by label
//! (see [`crate::classify`]). cambium vs oracle is one-sided and absolute:
//! cambium must contain everything Bazel reaches. A pair passes when no label
//! landed in `cambium_missing` or `unclassified` and the oracle found nothing
//! missing. Precision (`|producer ∩ cambium| / |cambium|`) is informational —
//! cambium deliberately over-approximates — and is reported so a regression in
//! over-approximation is visible.
//!
//! Results are written per repository to
//! `corpus-results/<id>/compare-<pin>[-<bazel_version>].json` after **every**
//! pair, and finished pairs are recorded in the corpus state file, so a run
//! killed at pair 137 of 200 resumes there.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context as _, Result};
use cambium_bazel::Bazel;
use cambium_corpus::{Checkout, Pair, State};
use cambium_engine::{AffectedRequest, Engine};
use serde::{Deserialize, Serialize};

use crate::classify::{classify, BazelFacts, Class, Facts, Side};
use crate::oracle::{OracleResult, RdepsOracle};
use crate::producer::{BaseWorktree, Producer, RunContext};

/// How many example labels of each class are kept in the JSON. The counts are
/// exact; only the lists are capped, because a pair can disagree on tens of
/// thousands of labels and the file has to stay readable.
pub const DEFAULT_EXAMPLES: usize = 20;

#[derive(Clone, Debug)]
pub struct CompareOptions {
    pub repo_id: String,
    pub pin: String,
    /// `corpus-results/<id>`.
    pub results_dir: PathBuf,
    /// Bazel version this run pinned, for the file name and the JSON.
    pub bazel_version: Option<String>,
    /// Roots the oracle samples per pair; 0 disables it.
    pub oracle_k: usize,
    pub seed: u64,
    /// Where producers may write hash files and raw output.
    pub scratch: PathBuf,
    /// Skip pairs already recorded in the state file.
    pub resume: bool,
    pub examples: usize,
}

impl CompareOptions {
    pub fn new(repo_id: impl Into<String>, pin: impl Into<String>, results_dir: PathBuf) -> Self {
        let repo_id = repo_id.into();
        Self {
            scratch: std::env::temp_dir().join(format!("cambium-compare/{repo_id}")),
            repo_id,
            pin: pin.into(),
            results_dir,
            bazel_version: None,
            oracle_k: 100,
            seed: 0,
            resume: true,
            examples: DEFAULT_EXAMPLES,
        }
    }

    /// `compare-<pin>.json`, or `compare-<pin>-<bazel_version>.json` when the
    /// version matrix is in play — the two runs answer different questions and
    /// must not overwrite each other.
    pub fn output_path(&self) -> PathBuf {
        Self::path(&self.results_dir, &self.pin, self.bazel_version.as_deref())
    }

    pub fn path(results_dir: &Path, pin: &str, bazel_version: Option<&str>) -> PathBuf {
        match bazel_version.filter(|v| !v.is_empty()) {
            Some(v) => {
                results_dir.join(format!("compare-{pin}-{}.json", v.replace(['/', ' '], "_")))
            }
            None => results_dir.join(format!("compare-{pin}.json")),
        }
    }
}

/// One label the two tools disagreed about.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Disagreement {
    pub label: String,
    pub side: Side,
    pub class: Class,
}

/// What cambium said, and what it cost.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CambiumRun {
    pub direct: usize,
    pub affected: usize,
    pub tests: usize,
    pub global: bool,
    /// Wall time with the base index resolved from scratch.
    pub cold_wall_ms: u64,
    /// Wall time with the base index already in the store.
    pub warm_wall_ms: u64,
    pub dirty_packages: usize,
    pub queried_targets: usize,
    pub changed_files: usize,
}

/// cambium against one producer, on one pair.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProducerComparison {
    pub name: String,
    pub version: String,
    pub labels: usize,
    pub cold_wall_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_wall_ms: Option<u64>,
    /// `|producer ∩ cambium| / |cambium|`.
    pub precision: f64,
    /// Of the labels both tools reported, the fraction whose depth agrees with
    /// cambium's. `None` when the producer reports no distances (bazel-diff
    /// without `--depEdgesFile`) or when the two share no labels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth_agreement: Option<f64>,
    /// File-target rows dropped so the two tools are compared over the same
    /// universe (see [`crate::producer::ProducerRun::restrict_to_rules`]).
    #[serde(default)]
    pub dropped_file_targets: usize,
    pub cambium_only: usize,
    pub producer_only: usize,
    /// Exact counts, keyed by class name.
    pub counts: BTreeMap<String, usize>,
    /// A capped sample of the disagreements, failures first.
    pub examples: Vec<Disagreement>,
    pub pass: bool,
    /// Set when the producer itself failed; the pair is then not counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Everything learned about one pair.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairComparison {
    pub schema: u32,
    pub base: String,
    pub head: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub cambium: CambiumRun,
    pub producers: Vec<ProducerComparison>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle: Option<OracleResult>,
    /// Recall against the oracle; must be 1.0.
    pub recall: f64,
    pub pass: bool,
}

impl PairComparison {
    pub fn key(&self) -> String {
        State::pair_key(&self.base, &self.head)
    }
}

/// Wall-clock distribution over the pairs of a run.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct WallStats {
    pub samples: usize,
    pub p50_ms: u64,
    pub p90_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CompareSummary {
    pub pairs: usize,
    pub passed: usize,
    /// Total labels in each disagreement class, over every pair and producer.
    pub by_class: BTreeMap<String, usize>,
    /// Mean precision per producer.
    pub mean_precision: BTreeMap<String, f64>,
    /// The worst recall any pair achieved. Anything below 1.0 fails the run.
    pub min_recall: f64,
    /// Cold and warm wall distributions per producer.
    pub producer_cold: BTreeMap<String, WallStats>,
    pub producer_warm: BTreeMap<String, WallStats>,
    pub cambium_cold: WallStats,
    pub cambium_warm: WallStats,
    pub pass: bool,
}

/// The comparison run for one repository at one pin.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompareResult {
    pub schema: u32,
    pub repo: String,
    pub pin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bazel_version: Option<String>,
    pub seed: u64,
    pub oracle_k: usize,
    pub pairs: Vec<PairComparison>,
    pub summary: CompareSummary,
}

impl CompareResult {
    pub fn read(path: &Path) -> Result<Self> {
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn summarise(&mut self) {
        self.summary = summarise(&self.pairs);
    }
}

// ------------------------------------------------------------------- the math

/// `|producer ∩ cambium| / |cambium|`: how much of what cambium reported the
/// producer also reported.
///
/// An empty cambium answer is defined as precision 1.0 — the two tools agreed
/// that nothing is affected, which is the common case on a source-only commit,
/// and calling it 0.0 would drag every mean down for the least interesting
/// pairs in the corpus.
pub fn precision(producer: &BTreeSet<String>, cambium: &BTreeSet<String>) -> f64 {
    if cambium.is_empty() {
        return 1.0;
    }
    producer.intersection(cambium).count() as f64 / cambium.len() as f64
}

fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() as f64 * q) as usize).min(sorted.len() - 1)]
}

/// p50/p90 over a set of wall times.
pub fn wall_stats(mut ms: Vec<u64>) -> WallStats {
    ms.sort_unstable();
    WallStats {
        samples: ms.len(),
        p50_ms: percentile(&ms, 0.5),
        p90_ms: percentile(&ms, 0.9),
    }
}

/// Rolls the per-pair results up. Pure, so the arithmetic is testable without
/// a repository.
pub fn summarise(pairs: &[PairComparison]) -> CompareSummary {
    let mut s = CompareSummary {
        pairs: pairs.len(),
        min_recall: 1.0,
        ..Default::default()
    };
    for c in Class::ALL {
        s.by_class.insert(c.name().to_string(), 0);
    }
    let mut precisions: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut cold: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut warm: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for p in pairs {
        if p.pass {
            s.passed += 1;
        }
        s.min_recall = s.min_recall.min(p.recall);
        for pr in &p.producers {
            if pr.error.is_some() {
                continue;
            }
            for (class, n) in &pr.counts {
                *s.by_class.entry(class.clone()).or_default() += n;
            }
            precisions
                .entry(pr.name.clone())
                .or_default()
                .push(pr.precision);
            cold.entry(pr.name.clone())
                .or_default()
                .push(pr.cold_wall_ms);
            if let Some(w) = pr.warm_wall_ms {
                warm.entry(pr.name.clone()).or_default().push(w);
            }
        }
    }
    s.mean_precision = precisions
        .into_iter()
        .map(|(k, v)| {
            let mean = if v.is_empty() {
                1.0
            } else {
                v.iter().sum::<f64>() / v.len() as f64
            };
            (k, mean)
        })
        .collect();
    s.producer_cold = cold.into_iter().map(|(k, v)| (k, wall_stats(v))).collect();
    s.producer_warm = warm.into_iter().map(|(k, v)| (k, wall_stats(v))).collect();
    s.cambium_cold = wall_stats(pairs.iter().map(|p| p.cambium.cold_wall_ms).collect());
    s.cambium_warm = wall_stats(pairs.iter().map(|p| p.cambium.warm_wall_ms).collect());
    s.pass = s.passed == s.pairs && s.min_recall >= 1.0;
    s
}

/// Classifies the symmetric difference of the two label sets.
///
/// Both directions in one pass, so a caller cannot accidentally classify one
/// side with the other side's rules.
pub fn classify_difference(
    cambium: &BTreeSet<String>,
    producer: &BTreeSet<String>,
    facts: &dyn Facts,
    examples: usize,
) -> (BTreeMap<String, usize>, Vec<Disagreement>) {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut found: Vec<Disagreement> = Vec::new();
    let mut push = |label: &str, side: Side| {
        let class = classify(label, side, facts);
        *counts.entry(class.name().to_string()).or_default() += 1;
        found.push(Disagreement {
            label: label.to_string(),
            side,
            class,
        });
    };
    for label in cambium.difference(producer) {
        push(label, Side::Cambium);
    }
    for label in producer.difference(cambium) {
        push(label, Side::Producer);
    }
    // Failures first: the examples that survive the cap are the ones a human
    // has to look at.
    found.sort_by_key(|d| (!d.class.is_failure(), d.class, d.label.clone()));
    found.truncate(examples);
    (counts, found)
}

// ----------------------------------------------------------------- the driver

/// A Bazel rooted at another worktree, carrying this run's startup flags and
/// environment so it joins the same server.
fn bazel_at(engine: &Engine, workspace: &Path) -> Bazel {
    Bazel::at(
        workspace.to_path_buf(),
        Some(&engine.bazel.binary().display().to_string()),
    )
    .with_startup_flags(engine.bazel.startup_flags().to_vec())
    .with_env(engine.bazel.env().to_vec())
}

/// The run context producers are handed for one pair.
pub fn run_context(engine: &Engine, scratch: &Path, base_worktree: &Path) -> RunContext {
    RunContext {
        bazel_binary: engine.bazel.binary().to_path_buf(),
        startup_flags: engine.bazel.startup_flags().to_vec(),
        env: engine.bazel.env().to_vec(),
        scratch: scratch.to_path_buf(),
        base_worktree: base_worktree.to_path_buf(),
    }
}

/// One pair, with defaults for scratch and the example cap.
pub fn compare_pair(
    engine: &Engine,
    checkout: &Checkout,
    pair: &Pair,
    producers: &[&dyn Producer],
    oracle_k: usize,
    seed: u64,
) -> Result<PairComparison> {
    let opts = CompareOptions {
        oracle_k,
        seed,
        ..CompareOptions::new(
            checkout.id.clone(),
            pair.head.clone(),
            checkout.dir.join("corpus-results"),
        )
    };
    compare_pair_with(engine, checkout, pair, producers, &opts)
}

/// One pair, with the run's options.
pub fn compare_pair_with(
    engine: &Engine,
    checkout: &Checkout,
    pair: &Pair,
    producers: &[&dyn Producer],
    opts: &CompareOptions,
) -> Result<PairComparison> {
    std::fs::create_dir_all(&opts.scratch)?;

    // ---- cambium, cold then warm ----
    //
    // The first run publishes whatever base index it had to compute, so the
    // second one finds it in the store: that is precisely the difference the
    // two numbers are meant to show.
    checkout.checkout(&pair.head)?;
    let req = AffectedRequest {
        include_manual: true,
        materialize: true,
        publish_base: true,
        ..AffectedRequest::new(pair.base.clone(), pair.head.clone())
    };
    let t = Instant::now();
    let cold = engine
        .affected(&req)
        .with_context(|| format!("cambium affected {}..{}", pair.base, pair.head))?;
    let cold_wall_ms = t.elapsed().as_millis() as u64;
    let t = Instant::now();
    let out = engine.affected(&req)?;
    let warm_wall_ms = t.elapsed().as_millis() as u64;
    drop(cold);

    let report = &out.report;
    let cambium_set: BTreeSet<String> = report.affected_labels().into_iter().collect();
    let cambium_depths: BTreeMap<String, u32> = report
        .affected
        .iter()
        .map(|t| (t.label.clone(), t.depth))
        .collect();
    let direct: Vec<String> = report.direct.clone();
    let cambium = CambiumRun {
        direct: report.direct.len(),
        affected: report.affected.len(),
        tests: report.tests.len(),
        global: report.global,
        cold_wall_ms,
        warm_wall_ms,
        dirty_packages: report.stats.dirty_packages,
        queried_targets: report.stats.queried_targets,
        changed_files: report.stats.changed_files,
    };
    let index = out
        .index
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no head index: the pair was globally invalidated"))?;

    // ---- the facts a classification needs ----
    let base_wt = BaseWorktree::create(&checkout.dir, &pair.base, &opts.scratch)?;
    let changed = BazelFacts::changed_paths(&checkout.dir, &pair.base, &pair.head)?;
    let facts = BazelFacts::new(
        &engine.bazel,
        index.view(),
        direct.iter().cloned(),
        changed.iter().cloned(),
    )
    .with_base(bazel_at(engine, base_wt.path()));

    // ---- the producers ----
    let ctx = run_context(engine, &opts.scratch, base_wt.path());
    let mut results = Vec::new();
    for p in producers {
        let cmp = match p.affected(&checkout.dir, &pair.base, &pair.head, &ctx) {
            Ok(mut run) => {
                // Both producers report source-file targets (`//pkg:BUILD.bazel`
                // at distance 0); cambium's walk emits rules only. Comparing
                // the two vocabularies would file every such row as
                // `cambium_missing`, so the answer is restricted to the
                // universe cambium reports on and the drop is counted.
                run.restrict_to_rules(index.view());
                let (counts, examples) =
                    classify_difference(&cambium_set, &run.labels, &facts, opts.examples);
                let failures: usize = counts
                    .iter()
                    .filter(|(k, _)| {
                        Class::ALL
                            .iter()
                            .any(|c| c.name() == k.as_str() && c.is_failure())
                    })
                    .map(|(_, n)| n)
                    .sum();
                ProducerComparison {
                    name: p.name().to_string(),
                    version: p.version(),
                    labels: run.labels.len(),
                    cold_wall_ms: run.cold_wall.as_millis() as u64,
                    warm_wall_ms: run.warm_wall.map(|d| d.as_millis() as u64),
                    precision: precision(&run.labels, &cambium_set),
                    depth_agreement: run
                        .depths
                        .as_ref()
                        .and_then(|d| crate::tango::depth_agreement(&cambium_depths, d)),
                    dropped_file_targets: run.dropped_file_targets,
                    cambium_only: cambium_set.difference(&run.labels).count(),
                    producer_only: run.labels.difference(&cambium_set).count(),
                    counts,
                    examples,
                    pass: failures == 0,
                    error: None,
                }
            }
            Err(e) => {
                tracing::warn!(producer = p.name(), error = %e, "producer failed on this pair");
                ProducerComparison {
                    name: p.name().to_string(),
                    version: p.version(),
                    labels: 0,
                    cold_wall_ms: 0,
                    warm_wall_ms: None,
                    precision: 0.0,
                    depth_agreement: None,
                    dropped_file_targets: 0,
                    cambium_only: 0,
                    producer_only: 0,
                    counts: BTreeMap::new(),
                    examples: Vec::new(),
                    pass: true,
                    error: Some(e.to_string()),
                }
            }
        };
        results.push(cmp);
    }

    // ---- the oracle ----
    let oracle = if opts.oracle_k > 0 {
        Some(
            RdepsOracle::new(opts.oracle_k, opts.seed)
                .run(&engine.bazel, index.view(), &direct)
                .with_context(|| format!("rdeps oracle at {}", pair.head))?,
        )
    } else {
        None
    };
    let oracle_recall = oracle.as_ref().map(|o| o.recall()).unwrap_or(1.0);
    let pass = results.iter().all(|p| p.pass) && oracle.as_ref().map_or(true, |o| o.ok());

    Ok(PairComparison {
        schema: cambium_engine::report::SCHEMA_VERSION,
        base: pair.base.clone(),
        head: pair.head.clone(),
        kind: pair.kind.name().to_string(),
        category: pair.category.clone(),
        cambium,
        producers: results,
        oracle,
        recall: oracle_recall,
        pass,
    })
}

/// Every pair, resumable, writing after each one.
///
/// A pair that errors outright (a producer that cannot run at all, a base
/// revision Bazel refuses to load) stops the run rather than being skipped: a
/// comparison that silently drops the hard pairs is worse than no comparison.
pub fn compare_pairs(
    engine: &Engine,
    checkout: &Checkout,
    pairs: &[Pair],
    producers: &[&dyn Producer],
    opts: &CompareOptions,
) -> Result<CompareResult> {
    let path = opts.output_path();
    let mut state = State::load(&opts.results_dir, &opts.repo_id, &opts.pin)?;
    let mut result = match (opts.resume, CompareResult::read(&path)) {
        (true, Ok(prev)) if prev.repo == opts.repo_id && prev.pin == opts.pin => prev,
        _ => CompareResult {
            schema: cambium_engine::report::SCHEMA_VERSION,
            repo: opts.repo_id.clone(),
            pin: opts.pin.clone(),
            bazel_version: opts.bazel_version.clone(),
            seed: opts.seed,
            oracle_k: opts.oracle_k,
            pairs: Vec::new(),
            summary: CompareSummary::default(),
        },
    };
    let done: BTreeSet<String> = result.pairs.iter().map(|p| p.key()).collect();

    for (i, pair) in pairs.iter().enumerate() {
        let key = pair.key();
        if opts.resume && done.contains(&key) {
            tracing::debug!(%key, "already compared");
            continue;
        }
        tracing::info!(pair = i + 1, of = pairs.len(), %key, kind = pair.kind.name(), "comparing");
        let cmp = compare_pair_with(engine, checkout, pair, producers, opts)?;
        if !cmp.pass {
            tracing::warn!(%key, "pair failed the three-way comparison");
        }
        result.pairs.push(cmp);
        result.summarise();
        result.write(&path)?;
        state.compared.insert(key);
        state.save(&opts.results_dir)?;
    }
    result.summarise();
    result.write(&path)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(labels: &[&str]) -> BTreeSet<String> {
        labels.iter().map(|s| s.to_string()).collect()
    }

    #[derive(Default)]
    struct NoFacts;
    impl Facts for NoFacts {
        fn is_external(&self, l: &str) -> bool {
            l.starts_with('@')
        }
        fn reachable_via_select(&self, l: &str) -> bool {
            l.ends_with(":selected")
        }
        fn reachable_only_via_implicit(&self, _: &str) -> bool {
            false
        }
        fn visibility_only_change(&self, _: &str) -> bool {
            false
        }
        fn bzl_closure_only(&self, _: &str) -> bool {
            false
        }
        fn changed_path_without_consumer(&self, _: &str) -> bool {
            false
        }
    }

    #[test]
    fn precision_is_the_producers_share_of_cambiums_answer() {
        let cambium = set(&["//a:a", "//b:b", "//c:c", "//d:d"]);
        assert_eq!(precision(&set(&["//a:a", "//b:b"]), &cambium), 0.5);
        assert_eq!(precision(&cambium, &cambium), 1.0);
        assert_eq!(
            precision(&set(&["//z:z"]), &cambium),
            0.0,
            "labels the producer alone reports do not raise precision"
        );
    }

    #[test]
    fn an_empty_cambium_answer_counts_as_agreement() {
        assert_eq!(precision(&BTreeSet::new(), &BTreeSet::new()), 1.0);
    }

    #[test]
    fn the_symmetric_difference_is_classified_from_both_sides() {
        let cambium = set(&["//a:a", "//p:selected", "//shared:s"]);
        let producer = set(&["//shared:s", "@ext//x:y", "//missed:m"]);
        let (counts, examples) = classify_difference(&cambium, &producer, &NoFacts, 10);
        assert_eq!(counts["cambium_extra_select"], 1);
        assert_eq!(counts["unclassified"], 1, "//a:a has no explanation");
        assert_eq!(counts["bazeldiff_extra_external"], 1);
        assert_eq!(counts["cambium_missing"], 1);
        assert!(!counts.contains_key("cambium_extra_visibility"));
        assert_eq!(
            examples.iter().filter(|d| d.class.is_failure()).count(),
            2,
            "failures come first: {examples:?}"
        );
        assert!(examples[0].class.is_failure() && examples[1].class.is_failure());
    }

    #[test]
    fn the_example_list_is_capped_but_the_counts_are_not() {
        let cambium: BTreeSet<String> = (0..50).map(|i| format!("//a:t{i}")).collect();
        let (counts, examples) = classify_difference(&cambium, &BTreeSet::new(), &NoFacts, 3);
        assert_eq!(counts["unclassified"], 50);
        assert_eq!(examples.len(), 3);
    }

    fn pair_comparison(
        name: &str,
        precision: f64,
        cold: u64,
        warm: u64,
        pass: bool,
    ) -> PairComparison {
        let mut counts = BTreeMap::new();
        counts.insert("cambium_extra_select".to_string(), 2);
        if !pass {
            counts.insert("cambium_missing".to_string(), 1);
        }
        PairComparison {
            schema: 3,
            base: format!("b{name}"),
            head: format!("h{name}"),
            kind: "adjacent".into(),
            category: None,
            cambium: CambiumRun {
                cold_wall_ms: cold * 10,
                warm_wall_ms: warm,
                ..Default::default()
            },
            producers: vec![ProducerComparison {
                name: "bazel-diff-rust".into(),
                version: "v46".into(),
                labels: 10,
                cold_wall_ms: cold,
                warm_wall_ms: Some(warm * 2),
                precision,
                depth_agreement: None,
                dropped_file_targets: 0,
                cambium_only: 2,
                producer_only: 0,
                counts,
                examples: Vec::new(),
                pass,
                error: None,
            }],
            oracle: None,
            recall: if pass { 1.0 } else { 0.75 },
            pass,
        }
    }

    #[test]
    fn the_summary_counts_classes_averages_precision_and_takes_the_worst_recall() {
        let pairs = vec![
            pair_comparison("1", 1.0, 100, 10, true),
            pair_comparison("2", 0.5, 300, 30, false),
        ];
        let s = summarise(&pairs);
        assert_eq!(s.pairs, 2);
        assert_eq!(s.passed, 1);
        assert!(!s.pass, "one failing pair fails the run");
        assert_eq!(s.by_class["cambium_extra_select"], 4);
        assert_eq!(s.by_class["cambium_missing"], 1);
        assert_eq!(
            s.by_class["bazeldiff_extra_external"], 0,
            "every class is listed"
        );
        assert_eq!(s.mean_precision["bazel-diff-rust"], 0.75);
        assert_eq!(s.min_recall, 0.75);
        assert_eq!(
            s.producer_cold["bazel-diff-rust"],
            WallStats {
                samples: 2,
                p50_ms: 300,
                p90_ms: 300
            }
        );
        assert_eq!(s.cambium_warm.p50_ms, 30);
        assert_eq!(s.cambium_cold.p50_ms, 3000);
    }

    #[test]
    fn a_producer_that_failed_does_not_pollute_the_averages() {
        let mut pairs = vec![pair_comparison("1", 1.0, 100, 10, true)];
        let mut broken = pair_comparison("2", 0.0, 0, 0, true);
        broken.producers[0].error = Some("boom".into());
        broken.producers[0].counts.clear();
        pairs.push(broken);
        let s = summarise(&pairs);
        assert_eq!(s.mean_precision["bazel-diff-rust"], 1.0);
        assert_eq!(s.producer_cold["bazel-diff-rust"].samples, 1);
    }

    #[test]
    fn percentiles_are_taken_over_the_sorted_walls() {
        assert_eq!(wall_stats(vec![]), WallStats::default());
        let s = wall_stats(vec![50, 10, 30, 20, 40, 60, 70, 80, 90, 100]);
        assert_eq!((s.samples, s.p50_ms, s.p90_ms), (10, 60, 100));
    }

    #[test]
    fn the_output_file_names_the_bazel_version_only_in_the_matrix() {
        let dir = Path::new("/r");
        assert_eq!(
            CompareOptions::path(dir, "abc", None),
            PathBuf::from("/r/compare-abc.json")
        );
        assert_eq!(
            CompareOptions::path(dir, "abc", Some("7.4.1")),
            PathBuf::from("/r/compare-abc-7.4.1.json")
        );
        assert_eq!(
            CompareOptions::path(dir, "abc", Some("")),
            PathBuf::from("/r/compare-abc.json")
        );
    }

    #[test]
    fn a_result_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = CompareResult {
            schema: cambium_engine::report::SCHEMA_VERSION,
            repo: "bazel".into(),
            pin: "abc".into(),
            bazel_version: Some("7.4.1".into()),
            seed: 7,
            oracle_k: 100,
            pairs: vec![pair_comparison("1", 1.0, 100, 10, true)],
            summary: CompareSummary::default(),
        };
        r.summarise();
        let path = dir.path().join("compare-abc.json");
        r.write(&path).unwrap();
        let back = CompareResult::read(&path).unwrap();
        assert_eq!(back.schema, cambium_engine::report::SCHEMA_VERSION);
        assert_eq!(back.pairs.len(), 1);
        assert!(back.summary.pass);
    }
}
