//! The three-way comparison itself.
//!
//! For one `(base, head)` change set the driver produces three answers and compares
//! them:
//!
//! 1. **cambium**, twice — once with the base index resolved from scratch
//!    (`cold`) and once with it in the store (`warm`) — because the whole
//!    argument for cambium is the warm number, and quoting it without the cold
//!    one would be dishonest.
//! 2. **each tool** ([`crate::tools::Tool`]), typically bazel-diff.
//! 3. **the oracle** ([`crate::oracles::rdeps`]), one `bazel query rdeps` over
//!    every changed file.
//!
//! cambium vs tool is a *symmetric difference*, classified label by label
//! (see [`crate::classify`]). cambium vs oracle is one-sided and absolute:
//! cambium must contain everything Bazel reaches. A change set passes when no label
//! landed in `cambium_missing` or `unclassified` and the oracle found nothing
//! missing. Precision (`|tool ∩ cambium| / |cambium|`) is informational —
//! cambium deliberately over-approximates — and is reported so a regression in
//! over-approximation is visible.
//!
//! Results are written per repository to
//! `bench-results/<id>/compare-<pin>[-<bazel_version>].json` after **every**
//! change set, and finished change sets are recorded in the subject's state file, so a run
//! killed at change set 137 of 200 resumes there.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::changes::ChangeSet;
use crate::checkout::Checkout;
use crate::state::State;
use anyhow::{Context as _, Result};
use cambium_bazel::Bazel;
use cambium_engine::Engine;
use serde::{Deserialize, Serialize};

use crate::classify::{classify, BazelFacts, Class, Facts, Side};
use crate::oracles::load::{LoadOracleResult, LoadOracleSummary};
use crate::oracles::rdeps::{OracleInputs, RdepsOracle, RdepsResult};
use crate::tools::{BaseWorktree, RunContext, Tool};
use crate::util::percentile;

/// How many example labels of each class are kept in the JSON. The counts are
/// exact; only the lists are capped, because a change set can disagree on tens of
/// thousands of labels and the file has to stay readable.
pub const DEFAULT_EXAMPLES: usize = 20;

#[derive(Clone, Debug)]
pub struct ChangesOptions {
    pub repo_id: String,
    pub pin: String,
    /// Where `changes-<pin>.json` goes: `bench-results/<subject>/changes/`.
    pub results_dir: PathBuf,
    /// Where `state.json` goes: `bench-results/<subject>/`. See
    /// [`crate::history::HistoryOptions::state_dir`].
    pub state_dir: PathBuf,
    /// Bazel version this run pinned, for the file name and the JSON.
    pub bazel_version: Option<String>,
    /// Run the reverse-dependency oracle on every incremental change set.
    /// `--oracle` without `rdeps` turns it off.
    pub rdeps_oracle: bool,
    /// Cap on the source labels the rdeps oracle queries per change set.
    /// 0 — the default — is the full change set, which is the oracle the
    /// methodology specifies; a positive value is the documented escape hatch
    /// for a subject whose change sets are too big to query whole.
    pub oracle_k: usize,
    /// Run the `.bzl` load-closure oracle on every change set that touches a
    /// `.bzl`. `--oracle` without `load` turns it off.
    pub load_oracle: bool,
    pub seed: u64,
    /// Where tools may write hash files and raw output.
    pub scratch: PathBuf,
    /// Skip change sets already recorded in the state file.
    pub resume: bool,
    pub examples: usize,
    /// How many change sets Tango runs on — the first this many, in the change
    /// sets' own order — or `None` for all of them.
    ///
    /// Tango costs 30-500 s a change set against cambium's fractions of a second, so
    /// on a laptop it *is* the run. Capping it keeps a timing sample worth
    /// quoting while bazel-diff, cambium and the oracle still see every change set.
    /// A change set Tango did not run on carries no `tango` row at all: it is a change set
    /// with fewer tools, not a change set where a tool disagreed.
    pub tango_change_sets: Option<usize>,
}

/// Makes a path absolute without requiring it to exist.
///
/// Every path in a run is handed to a *child process* — bazel-diff, tango-cli,
/// Bazel — that runs with its own working directory (the subject's worktree, a
/// scratch worktree), so a relative `--results-dir bench-results` resolves
/// somewhere else in each of them. `canonicalize` is used where it can be
/// (it also resolves the macOS `/var` -> `/private/var` symlink, which keeps
/// paths comparable across processes) and falls back to joining the current
/// directory for a path that does not exist yet.
pub fn absolute(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    if p.is_absolute() {
        return p.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p),
        Err(_) => p.to_path_buf(),
    }
}

impl ChangesOptions {
    pub fn new(repo_id: impl Into<String>, pin: impl Into<String>, results_dir: PathBuf) -> Self {
        let repo_id = repo_id.into();
        Self {
            scratch: absolute(&std::env::temp_dir().join(format!("cambium-compare/{repo_id}"))),
            repo_id,
            pin: pin.into(),
            state_dir: absolute(results_dir.parent().unwrap_or(&results_dir)),
            results_dir: absolute(&results_dir),
            bazel_version: None,
            rdeps_oracle: true,
            oracle_k: 0,
            load_oracle: true,
            seed: 0,
            resume: true,
            examples: DEFAULT_EXAMPLES,
            tango_change_sets: None,
        }
    }

    /// Forces `results_dir` and `scratch` absolute. Called at the top of
    /// [`compare_change_sets`] and [`compare_change_set_with`], so a caller that built the
    /// options by hand from a relative CLI flag cannot hand a tool a path
    /// that means something different in the child's working directory.
    pub fn make_absolute(&mut self) {
        self.results_dir = absolute(&self.results_dir);
        self.state_dir = absolute(&self.state_dir);
        self.scratch = absolute(&self.scratch);
    }

    /// `compare-<pin>.json`, or `compare-<pin>-<bazel_version>.json` when a
    /// Bazel was pinned — the two runs answer different questions and must not
    /// overwrite each other.
    pub fn output_path(&self) -> PathBuf {
        Self::path(&self.results_dir, &self.pin, self.bazel_version.as_deref())
    }

    pub fn path(results_dir: &Path, pin: &str, bazel_version: Option<&str>) -> PathBuf {
        results_dir.join(crate::util::versioned_name(
            &format!("changes-{pin}"),
            bazel_version,
            "json",
        ))
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
    /// Why, when `global`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_reason: Option<String>,
    /// Wall time with the base index resolved from scratch.
    pub cold_wall_ms: u64,
    /// Wall time with the base index already in the store.
    pub warm_wall_ms: u64,
    pub dirty_packages: usize,
    pub queried_targets: usize,
    pub changed_files: usize,
    /// The engine re-indexed the whole repository on this change set
    /// (`stats.cold_rebuild`): the dirty set was too large to splice, so the
    /// warm number here is a cold number wearing a warm label. The change set is
    /// still incremental — it has a head index and a classified answer — so it
    /// stays in every statistic, but it is counted separately
    /// ([`ChangesSummary::cold_rebuild_change_sets`]) because a mean that hides them
    /// is a mean of two different operations.
    #[serde(default)]
    pub cold_rebuild: bool,
    /// How many rule targets `//...` names at head. Recorded only on a
    /// `global` change set, where it *is* cambium's answer: the report quotes it
    /// against the tool's precise set size, so a global's cost to the user
    /// is a number and not a shrug. 0 when the universe could not be counted.
    #[serde(default)]
    pub universe: usize,
}

impl CambiumRun {
    /// How many targets cambium told the user to build: the universe on a
    /// global change set, the affected set otherwise.
    pub fn answer_size(&self) -> usize {
        if self.global {
            self.universe
        } else {
            self.affected
        }
    }
}

/// cambium against one tool, on one change set.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolComparison {
    pub name: String,
    pub version: String,
    pub labels: usize,
    pub cold_wall_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_wall_ms: Option<u64>,
    /// `|tool ∩ cambium| / |cambium|`.
    pub precision: f64,
    /// Of the labels both tools reported, the fraction whose depth agrees with
    /// cambium's. `None` when the tool reports no distances (bazel-diff
    /// without `--depEdgesFile`) or when the two share no labels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth_agreement: Option<f64>,
    /// File-target rows dropped so the two tools are compared over the same
    /// universe (see [`crate::tools::ToolRun::restrict_to_rules`]).
    #[serde(default)]
    pub dropped_file_targets: usize,
    pub cambium_only: usize,
    pub tool_only: usize,
    /// Exact counts, keyed by class name.
    pub counts: BTreeMap<String, usize>,
    /// A capped sample of the disagreements, failures first.
    pub examples: Vec<Disagreement>,
    /// Why cambium reached the labels the tool did not report, counted by
    /// `--explain` reason kind (`rule_hash`, `file src/.../BUILD`,
    /// `deleted_dep //x:y`, ...). Without this a `unclassified` count is a
    /// number with no way in: the classifier says "nothing explains these",
    /// and this says what cambium itself thought it was doing.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cambium_only_reasons: BTreeMap<String, usize>,
    /// Whether this tool's comparison agreed. `false` when a gating class
    /// fired, and `false` when [`ToolComparison::error`] is set: a tool that
    /// could not run has not agreed with anything.
    pub pass: bool,
    /// Set when the tool itself failed. Its timing and precision are left out
    /// of every aggregate, and the change set fails: an absent tool must not
    /// read as a passing three-way comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Default for ToolComparison {
    /// An empty comparison: no labels, no timings, no disagreements, and
    /// passing. The four real constructions differ from it in a handful of
    /// fields each, and spelling all fourteen out at every one of them is how
    /// the global branch came to record a failed tool as `pass: true`.
    fn default() -> Self {
        Self {
            name: String::new(),
            version: String::new(),
            labels: 0,
            cold_wall_ms: 0,
            warm_wall_ms: None,
            precision: 0.0,
            depth_agreement: None,
            dropped_file_targets: 0,
            cambium_only: 0,
            tool_only: 0,
            counts: BTreeMap::new(),
            examples: Vec::new(),
            cambium_only_reasons: BTreeMap::new(),
            pass: true,
            error: None,
        }
    }
}

impl ToolComparison {
    /// A passing comparison against `name`, to be filled in.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            ..Self::default()
        }
    }

    /// The tool could not run at all.
    ///
    /// A comparison that did not happen is not a comparison that agreed, so
    /// this fails the change set. Passing it is how the 2026-09-08 published
    /// run came to render sourcegraph without bazel-diff (40/40 `aspect query`
    /// failures) and apollo without Tango (40/40 stale worktrees) as though
    /// both were three-way comparisons: the summary tables simply omit a tool
    /// that reported nothing, so the absence is invisible in the output.
    pub fn failed(name: impl Into<String>, version: impl Into<String>, err: &anyhow::Error) -> Self {
        Self {
            // `{:#}` and not `{}`: anyhow's `Display` prints only the outermost
            // context, so `hashing <sha> in <dir>` would arrive without the
            // exit status and stderr tail the tool attached underneath it.
            error: Some(format!("{err:#}")),
            pass: false,
            ..Self::new(name, version)
        }
    }
}

/// Everything learned about one change set.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeSetResult {
    pub schema: u32,
    pub base: String,
    pub head: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub cambium: CambiumRun,
    pub tools: Vec<ToolComparison>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle: Option<RdepsResult>,
    /// What Bazel's `rbuildfiles` said about the `.bzl` files in this change set's
    /// change set, against the static load closure cambium answered from.
    /// `None` when nothing in the diff was a `.bzl`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_oracle: Option<LoadOracleResult>,
    /// Recall against the oracle; must be 1.0.
    pub recall: f64,
    /// cambium answered `//...`. The tools and the universe query still
    /// ran — what a global costs the user is the whole reason to record one —
    /// but nothing was classified, so the change set is excluded from the precision,
    /// class and recall aggregates and from the *incremental* wall-clock
    /// series. It is included in the all-change sets series and in
    /// [`GlobalStats`]. See [`summarise`].
    #[serde(default)]
    pub global: bool,
    pub pass: bool,
}

impl ChangeSetResult {
    pub fn key(&self) -> String {
        State::change_set_key(&self.base, &self.head)
    }
}

/// Wall-clock distribution over the change sets of a run.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct WallStats {
    pub samples: usize,
    pub p50_ms: u64,
    pub p90_ms: u64,
}

/// What the globally invalidated change sets of a run cost.
///
/// Kept apart from every other number in the summary because a global is not a
/// disagreement and not a failure — cambium's `//...` is a superset of any
/// answer — but it *is* work the user pays for, and a timings table drawn only
/// over the change sets cambium is fast on would be advocacy rather than
/// measurement.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GlobalStats {
    pub change_sets: usize,
    /// Mean size of cambium's answer on those change sets: the `//...` universe.
    pub mean_cambium_labels: f64,
    /// Mean size of the *precise* answer each tool found there. The gap
    /// between this and `mean_cambium_labels` is the over-approximation a
    /// global hands the user.
    pub mean_tool_labels: BTreeMap<String, f64>,
    pub cambium_cold: WallStats,
    pub cambium_warm: WallStats,
    pub tool_cold: BTreeMap<String, WallStats>,
    pub tool_warm: BTreeMap<String, WallStats>,
}

impl GlobalStats {
    pub fn is_empty(&self) -> bool {
        self.change_sets == 0
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ChangesSummary {
    pub change_sets: usize,
    pub passed: usize,
    /// ChangeSets on which cambium answered `//...`. They pass by construction and
    /// contribute to no precision, class or recall statistic — a full
    /// of `MODULE.bazel` bumps cannot flatter (or ruin) the soundness numbers
    /// — but their wall times *are* counted, in the `all_*` series and in
    /// [`ChangesSummary::global_stats`].
    #[serde(default)]
    pub globals: usize,
    /// ChangeSets where the engine gave up on splicing and re-indexed the whole
    /// repository (`stats.cold_rebuild`). They are ordinary incremental change sets
    /// in every aggregate; the count is here so a warm p50 that drifted can be
    /// read against how many of its samples were secretly cold.
    #[serde(default)]
    pub cold_rebuild_change_sets: usize,
    /// Total labels in each disagreement class, over every change set and tool.
    pub by_class: BTreeMap<String, usize>,
    /// Mean precision per tool.
    pub mean_precision: BTreeMap<String, f64>,
    /// The worst recall any change set achieved. Anything below 1.0 fails the run.
    pub min_recall: f64,
    /// The `.bzl` load-closure oracle, rolled up. `missing_packages` must be 0.
    #[serde(default, skip_serializing_if = "LoadOracleSummary::is_empty")]
    pub load_oracle: LoadOracleSummary,
    /// Cold and warm wall distributions per tool, over the **incremental**
    /// change sets: the ones cambium answered with a walk. Every precision, class
    /// and recall number in this summary is taken over the same set, so these
    /// are the walls that go with them.
    pub tool_cold: BTreeMap<String, WallStats>,
    pub tool_warm: BTreeMap<String, WallStats>,
    pub cambium_cold: WallStats,
    pub cambium_warm: WallStats,
    /// How many non-global change sets each tool actually ran on. Below
    /// `change_sets` when a tool was capped (Tango usually is) or
    /// failed on some change sets, and it is what every per-tool number above is
    /// an average over.
    #[serde(default)]
    pub tool_change_sets: BTreeMap<String, usize>,
    /// The same four distributions over **every** compared change set, globals
    /// included. This is the honest headline: it is what a user running
    /// cambium over this repository's history actually waits for, including
    /// the commits where cambium has nothing clever to say. The report quotes
    /// it first.
    #[serde(default)]
    pub all_tool_cold: BTreeMap<String, WallStats>,
    #[serde(default)]
    pub all_tool_warm: BTreeMap<String, WallStats>,
    #[serde(default)]
    pub all_cambium_cold: WallStats,
    #[serde(default)]
    pub all_cambium_warm: WallStats,
    /// How many change sets of any kind each tool ran on.
    #[serde(default)]
    pub all_tool_change_sets: BTreeMap<String, usize>,
    /// The globals on their own: what cambium's `//...` costs against the
    /// precise answer the tools found.
    #[serde(default, skip_serializing_if = "GlobalStats::is_empty")]
    pub global_stats: GlobalStats,
    pub pass: bool,
}

/// The comparison run for one repository at one pin.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangesResult {
    pub schema: u32,
    pub repo: String,
    pub pin: String,
    /// The `--bazel-version` override, if any: a request (`7.x`), not what
    /// bazelisk resolved it to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bazel_version: Option<String>,
    /// What `bazel --version` actually reported, e.g. `bazel 7.4.1`. Absent
    /// from results written before this field existed; the report says `n/a`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bazel_version_used: Option<String>,
    pub seed: u64,
    pub oracle_k: usize,
    /// `State::engine_id` of the build that wrote this; a resume by any other
    /// build starts over. `state.json` already starts over on its own, but the
    /// change sets live in this file, and the first published run's re-run
    /// resumed every one of them from the previous engine's answers.
    #[serde(default)]
    pub engine: String,
    /// How many change sets Tango was asked to run on, when it was capped.
    /// Tango costs 30-500 s a change set, so a run that quotes its p50 has to
    /// say how many change sets that p50 is taken over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tango_change_sets: Option<usize>,
    pub change_sets: Vec<ChangeSetResult>,
    pub summary: ChangesSummary,
}

impl ChangesResult {
    pub fn read(path: &Path) -> Result<Self> {
        crate::util::read_result(path)
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
        self.summary = summarise(&self.change_sets);
    }
}

// ------------------------------------------------------------------- the math

/// `|tool ∩ cambium| / |cambium|`: how much of what cambium reported the
/// tool also reported.
///
/// An empty cambium answer is defined as precision 1.0 — the two tools agreed
/// that nothing is affected, which is the common case on a source-only commit,
/// and calling it 0.0 would drag every mean down for the least interesting
/// change sets in the run.
pub fn precision(tool: &BTreeSet<String>, cambium: &BTreeSet<String>) -> f64 {
    if cambium.is_empty() {
        return 1.0;
    }
    tool.intersection(cambium).count() as f64 / cambium.len() as f64
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

/// Rolls the per-change set results up. Pure, so the arithmetic is testable without
/// a repository.
///
/// Two wall-clock series come out of this, and the difference between them is
/// the point:
///
/// - the **incremental** series (`cambium_cold`, `tool_cold`, ...) covers
///   the change sets cambium answered with a walk. It is the series that belongs
///   next to precision and the class counts, because those are computed over
///   exactly those change sets.
/// - the **all-change sets** series (`all_*`) covers every change set compared, including
///   the ones where cambium fell back to `//...`. Nothing is excluded from it,
///   which is why it is the number the report quotes first.
///
/// Soundness is unchanged: a global change set enters no precision mean, no class
/// total and no recall, because there is nothing there to be right or wrong
/// about — `//...` contains everything by construction.
pub fn summarise(change_sets: &[ChangeSetResult]) -> ChangesSummary {
    let mut s = ChangesSummary {
        change_sets: change_sets.len(),
        min_recall: 1.0,
        ..Default::default()
    };
    for c in Class::ALL {
        s.by_class.insert(c.name().to_string(), 0);
    }
    let mut precisions: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut cold: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut warm: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut all_cold: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut all_warm: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut g_cold: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut g_warm: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut g_labels: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut g_cambium: Vec<usize> = Vec::new();
    for p in change_sets {
        if p.pass {
            s.passed += 1;
        }
        if p.cambium.cold_rebuild {
            s.cold_rebuild_change_sets += 1;
        }
        // Every change set's walls, whatever cambium did with it.
        for pr in p.tools.iter().filter(|pr| pr.error.is_none()) {
            *s.all_tool_change_sets.entry(pr.name.clone()).or_default() += 1;
            all_cold
                .entry(pr.name.clone())
                .or_default()
                .push(pr.cold_wall_ms);
            if let Some(w) = pr.warm_wall_ms {
                all_warm.entry(pr.name.clone()).or_default().push(w);
            }
        }
        if p.global {
            s.globals += 1;
            g_cambium.push(p.cambium.answer_size());
            for pr in p.tools.iter().filter(|pr| pr.error.is_none()) {
                g_labels.entry(pr.name.clone()).or_default().push(pr.labels);
                g_cold
                    .entry(pr.name.clone())
                    .or_default()
                    .push(pr.cold_wall_ms);
                if let Some(w) = pr.warm_wall_ms {
                    g_warm.entry(pr.name.clone()).or_default().push(w);
                }
            }
            continue;
        }
        s.min_recall = s.min_recall.min(p.recall);
        if let Some(o) = &p.load_oracle {
            s.load_oracle.add(o);
            if !o.missing_packages.is_empty() {
                *s.by_class
                    .entry(Class::CambiumMissingBzlClosure.name().to_string())
                    .or_default() += o.missing_packages.len();
            }
        }
        for pr in &p.tools {
            if pr.error.is_some() {
                continue;
            }
            for (class, n) in &pr.counts {
                *s.by_class.entry(class.clone()).or_default() += n;
            }
            *s.tool_change_sets.entry(pr.name.clone()).or_default() += 1;
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
    s.tool_cold = cold.into_iter().map(|(k, v)| (k, wall_stats(v))).collect();
    s.tool_warm = warm.into_iter().map(|(k, v)| (k, wall_stats(v))).collect();
    let timed = || change_sets.iter().filter(|p| !p.global);
    s.cambium_cold = wall_stats(timed().map(|p| p.cambium.cold_wall_ms).collect());
    s.cambium_warm = wall_stats(timed().map(|p| p.cambium.warm_wall_ms).collect());
    s.all_tool_cold = all_cold
        .into_iter()
        .map(|(k, v)| (k, wall_stats(v)))
        .collect();
    s.all_tool_warm = all_warm
        .into_iter()
        .map(|(k, v)| (k, wall_stats(v)))
        .collect();
    s.all_cambium_cold = wall_stats(change_sets.iter().map(|p| p.cambium.cold_wall_ms).collect());
    s.all_cambium_warm = wall_stats(change_sets.iter().map(|p| p.cambium.warm_wall_ms).collect());
    s.global_stats = GlobalStats {
        change_sets: s.globals,
        mean_cambium_labels: mean(g_cambium.iter().map(|n| *n as f64)),
        mean_tool_labels: g_labels
            .iter()
            .map(|(k, v)| (k.clone(), mean(v.iter().map(|n| *n as f64))))
            .collect(),
        cambium_cold: wall_stats(
            change_sets
                .iter()
                .filter(|p| p.global)
                .map(|p| p.cambium.cold_wall_ms)
                .collect(),
        ),
        cambium_warm: wall_stats(
            change_sets
                .iter()
                .filter(|p| p.global)
                .map(|p| p.cambium.warm_wall_ms)
                .collect(),
        ),
        tool_cold: g_cold
            .into_iter()
            .map(|(k, v)| (k, wall_stats(v)))
            .collect(),
        tool_warm: g_warm
            .into_iter()
            .map(|(k, v)| (k, wall_stats(v)))
            .collect(),
    };
    s.pass = s.passed == s.change_sets && s.min_recall >= 1.0;
    s
}

/// The arithmetic mean, and 0.0 over nothing.
fn mean(v: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = v.fold((0.0, 0usize), |(s, n), x| (s + x, n + 1));
    if n == 0 {
        0.0
    } else {
        sum / n as f64
    }
}

/// Classifies the symmetric difference of the two label sets.
///
/// Both directions in one pass, so a caller cannot accidentally classify one
/// side with the other side's rules.
pub fn classify_difference(
    cambium: &BTreeSet<String>,
    tool: &BTreeSet<String>,
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
    for label in cambium.difference(tool) {
        push(label, Side::Cambium);
    }
    for label in tool.difference(cambium) {
        push(label, Side::Tool);
    }
    // Failures first: the examples that survive the cap are the ones a human
    // has to look at.
    found.sort_by_key(|d| (!d.class.is_failure(), d.class, d.label.clone()));
    found.truncate(examples);
    (counts, found)
}

// ----------------------------------------------------------------- the driver

/// A Bazel rooted at another worktree, carrying this run's startup flags and
/// environment.
///
/// Everything but `--output_base`: that one is per workspace directory, and
/// lending it to a second workspace corrupts repository rules that symlink
/// into the workspace they were fetched from. See
/// [`RunContext::tool_startup_flags`].
fn bazel_at(engine: &Engine, workspace: &Path) -> Bazel {
    let startup: Vec<String> = engine
        .bazel
        .startup_flags()
        .iter()
        .filter(|f| !f.starts_with("--output_base"))
        .cloned()
        .collect();
    Bazel::at(
        workspace.to_path_buf(),
        Some(&engine.bazel.binary().display().to_string()),
    )
    .with_startup_flags(startup)
    .with_env(engine.bazel.env().to_vec())
}

/// The run context tools are handed for one change set.
pub fn run_context(engine: &Engine, scratch: &Path, base_worktree: &Path) -> RunContext {
    RunContext {
        bazel_binary: engine.bazel.binary().to_path_buf(),
        startup_flags: engine.bazel.startup_flags().to_vec(),
        env: engine.bazel.env().to_vec(),
        scratch: absolute(scratch),
        base_worktree: absolute(base_worktree),
    }
}

/// How many of `labels` cambium reached for each `--explain` reason.
fn reason_histogram<'a>(
    labels: impl IntoIterator<Item = &'a String>,
    explanations: &[cambium_engine::report::Explanation],
) -> BTreeMap<String, usize> {
    let wanted: BTreeSet<&str> = labels.into_iter().map(String::as_str).collect();
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    for e in explanations {
        if wanted.contains(e.target.as_str()) {
            *out.entry(e.reason.describe()).or_default() += 1;
        }
    }
    out
}

/// The labels themselves plus every label on their `--explain` path: the set
/// the classifier will ask Bazel about, so it can be fetched in one batch.
fn with_paths<'a>(
    labels: impl IntoIterator<Item = &'a String>,
    explanations: &[cambium_engine::report::Explanation],
) -> Vec<String> {
    let wanted: BTreeSet<&str> = labels.into_iter().map(String::as_str).collect();
    let mut out: BTreeSet<String> = wanted.iter().map(|s| s.to_string()).collect();
    for e in explanations {
        if wanted.contains(e.target.as_str()) {
            out.extend(e.path.iter().cloned());
        }
    }
    out.into_iter().collect()
}

/// One change set, with defaults for scratch and the example cap.
pub fn compare_change_set(
    engine: &Engine,
    checkout: &Checkout,
    change_set: &ChangeSet,
    tools: &[&dyn Tool],
    oracle_k: usize,
    seed: u64,
) -> Result<ChangeSetResult> {
    let opts = ChangesOptions {
        oracle_k,
        seed,
        ..ChangesOptions::new(
            checkout.id.clone(),
            change_set.head.clone(),
            checkout.dir.join("bench-results"),
        )
    };
    compare_change_set_with(engine, checkout, change_set, tools, &opts)
}

/// One change set, with the run's options.
pub fn compare_change_set_with(
    engine: &Engine,
    checkout: &Checkout,
    change_set: &ChangeSet,
    tools: &[&dyn Tool],
    opts: &ChangesOptions,
) -> Result<ChangeSetResult> {
    let mut owned = opts.clone();
    owned.make_absolute();
    let opts = &owned;
    std::fs::create_dir_all(&opts.scratch)?;

    // ---- cambium, cold then warm ----
    //
    // The first run publishes whatever base index it had to compute, so the
    // second one finds it in the store: that is precisely the difference the
    // two numbers are meant to show.
    checkout.checkout(&change_set.head)?;
    let run =
        crate::tools::cambium::Cambium::new(engine).run(&change_set.base, &change_set.head)?;
    let (cold_wall_ms, warm_wall_ms) = (run.cold.as_millis() as u64, run.warm.as_millis() as u64);
    let out = run.out;

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
        global_reason: report.global_reason.clone(),
        cold_wall_ms,
        warm_wall_ms,
        dirty_packages: report.stats.dirty_packages,
        queried_targets: report.stats.queried_targets,
        changed_files: report.stats.changed_files,
        cold_rebuild: report.stats.cold_rebuild,
        universe: 0,
    };
    // A globally invalidated change set (MODULE.bazel moved, a `.bazelrc` changed,
    // anything on the invalidator list) has no head index and no affected set:
    // cambium's answer is `//...`. There is nothing to classify — every label
    // any tool could name is trivially inside "everything" — and nothing
    // an oracle could disprove, so the change set enters no precision, class or
    // recall aggregate, is marked passing, and the run carries on.
    //
    // The tools **do** run. Skipping them was the comfortable choice and
    // the dishonest one: it left the timing rows drawn over precisely the
    // change sets cambium is fast on, so the published speedup was an average over a
    // hand-picked half of the history. Here each tool pays its usual two
    // whole-repository query passes and the change set records what it cost, what
    // precise answer it found, and — from one `kind(rule, //...)` query — how
    // many targets cambium's `//...` is asking the user to build instead.
    //
    // The two oracles do not run, and cannot:
    //
    // - the rdeps oracle asks which rules the changed files reach and requires
    //   cambium's answer to contain them; a global answer is `//...`, which
    //   contains every rule by construction, so the check is vacuous and there
    //   is no head index to seed it from anyway.
    // - the `.bzl` load-closure oracle checks `rbuildfiles` against the dirty
    //   packages cambium named, and a global names none — running it would
    //   report every package as missed, which is the opposite of the truth.
    let Some(index) = out.index.as_ref() else {
        let reason = report
            .global_reason
            .clone()
            .unwrap_or_else(|| "globally invalidated".to_string());
        tracing::info!(
            %reason,
            tools = tools.len(),
            "the change set is globally invalidated: cambium answers //..., and the tools run anyway so the cost of that is recorded"
        );
        // A failure here must not take the run down: the universe is a
        // reporting number, not a gate, and a repository whose `//...` will
        // not fully load still has a change set worth timing.
        let universe = match crate::oracles::rdeps::rule_universe(&engine.bazel) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "could not count the rule universe; the global change set records 0"
                );
                BTreeSet::new()
            }
        };
        // Only when there is a tool to hand it to: a comparison run with
        // none configured must not pay for a worktree nobody reads.
        let base_wt = match tools.is_empty() {
            true => None,
            false => Some(BaseWorktree::create(
                &checkout.dir,
                &change_set.base,
                &opts.scratch,
            )?),
        };
        let ctx = run_context(
            engine,
            &opts.scratch,
            base_wt.as_ref().map_or(&checkout.dir, |w| w.path()),
        );
        let results: Vec<ToolComparison> = tools
            .iter()
            .map(
                |p| match p.affected(&checkout.dir, &change_set.base, &change_set.head, &ctx) {
                    Ok(mut run) => {
                        run.restrict_to_rule_set(&universe);
                        ToolComparison {
                            labels: run.labels.len(),
                            cold_wall_ms: run.cold_wall.as_millis() as u64,
                            warm_wall_ms: run.warm_wall.map(|d| d.as_millis() as u64),
                            // Every label the tool named is inside `//...`, so
                            // there is nothing for cambium to have missed. It is
                            // recorded as 1.0 for the reader's sake and consumed
                            // by nothing: `summarise` takes no precision from a
                            // global change set.
                            precision: 1.0,
                            dropped_file_targets: run.dropped_file_targets,
                            ..ToolComparison::new(p.name(), p.version())
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            tool = p.name(),
                            error = format!("{e:#}"),
                            "tool failed on this global change set"
                        );
                        ToolComparison::failed(p.name(), p.version(), &e)
                    }
                },
            )
            .collect();
        // cambium cannot be unsound on a global change set -- it answered
        // `//...` -- but a tool that could not run still fails it, exactly as
        // on an incremental one. Otherwise a subject whose every change set is
        // global reports a clean run against tools that never produced an
        // answer.
        let pass = results.iter().all(|t| t.pass);
        return Ok(ChangeSetResult {
            schema: crate::RESULT_SCHEMA,
            base: change_set.base.clone(),
            head: change_set.head.clone(),
            kind: change_set.kind.name().to_string(),
            category: change_set.category.clone(),
            cambium: CambiumRun {
                global: true,
                global_reason: Some(reason),
                universe: universe.len(),
                ..cambium
            },
            tools: results,
            oracle: None,
            load_oracle: None,
            recall: 1.0,
            global: true,
            pass,
        });
    };

    // ---- the facts a classification needs ----
    let base_wt = BaseWorktree::create(&checkout.dir, &change_set.base, &opts.scratch)?;
    let changed = BazelFacts::changed_paths(&checkout.dir, &change_set.base, &change_set.head)?;
    use cambium_engine::report::DirectReason;
    let reasoned = |f: fn(&DirectReason) -> bool| -> Vec<String> {
        report
            .explanations
            .iter()
            .flatten()
            .filter(|e| f(&e.reason))
            .map(|e| e.target.clone())
            .collect()
    };
    let paths: Vec<(String, Vec<String>)> = report
        .explanations
        .iter()
        .flatten()
        .map(|e| (e.target.clone(), e.path.clone()))
        .collect();
    let deleted_dep = reasoned(|r| matches!(r, DirectReason::DeletedDep { .. }));
    let external_repo = reasoned(|r| matches!(r, DirectReason::ExternalRepo { .. }));
    let from_build_file = reasoned(|r| match r {
        DirectReason::File { path } => cambium_classify::is_build_file(path),
        _ => false,
    });
    // ---- the oracle ----
    //
    // Before the tools, not after: its answer is one of the classifier's
    // facts. A cambium-side extra that no benign predicate explains is
    // `tool_missing` rather than `unclassified` when Bazel's own `rdeps`
    // reached the label too, and that verdict has to be available while the
    // difference is being classified.
    //
    // The base index is wanted only for the deleted half of the change set
    // (see `crate::oracles::rdeps`): a file that is gone at head has no label
    // to ask Bazel about, and the base index's `file -> consumers` rows are the
    // only record of who read it. The run above published it, so this is a
    // store read; when it is not there the oracle says so and checks the rest.
    let dirty_packages: BTreeSet<String> = report.dirty_packages.iter().cloned().collect();
    let oracle = match opts.rdeps_oracle {
        true => {
            let base_index = base_index_for_oracle(engine, &change_set.base);
            Some(
                RdepsOracle::new(opts.oracle_k, opts.seed)
                    .run(
                        &engine.bazel,
                        &OracleInputs {
                            head: index.view(),
                            base: base_index.as_ref().map(|f| f.view()),
                            changed: &changed,
                            cambium: &cambium_set,
                            dirty_packages: &dirty_packages,
                        },
                    )
                    .with_context(|| format!("rdeps oracle at {}", change_set.head))?,
            )
        }
        false => None,
    };
    let oracle_recall = oracle.as_ref().map(|o| o.recall()).unwrap_or(1.0);

    let facts = BazelFacts::new(
        &engine.bazel,
        index.view(),
        direct.iter().cloned(),
        changed.iter().cloned(),
    )
    .with_paths(paths)
    .with_affected(cambium_set.iter().cloned())
    .with_deleted_dep(deleted_dep)
    .with_external_repo(external_repo)
    .with_build_file(from_build_file)
    .with_oracle(oracle.iter().flat_map(|o| o.oracle_set.iter().cloned()))
    .with_base(bazel_at(engine, base_wt.path()));

    // ---- the tools ----
    let ctx = run_context(engine, &opts.scratch, base_wt.path());
    let mut results = Vec::new();
    for p in tools {
        let cmp = match p.affected(&checkout.dir, &change_set.base, &change_set.head, &ctx) {
            Ok(mut run) => {
                // Both tools report source-file targets (`//pkg:BUILD.bazel`
                // at distance 0); cambium's walk emits rules only. Comparing
                // the two vocabularies would file every such row as
                // `cambium_missing`, so the answer is restricted to the
                // universe cambium reports on and the drop is counted.
                run.restrict_to_rules(index.view());
                // One batched Bazel query for every label in the symmetric
                // difference, before the classifier asks about them one by
                // one; see `BazelFacts::prefetch`.
                facts.prefetch(with_paths(
                    cambium_set.symmetric_difference(&run.labels),
                    report.explanations.as_deref().unwrap_or_default(),
                ));
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
                ToolComparison {
                    labels: run.labels.len(),
                    cold_wall_ms: run.cold_wall.as_millis() as u64,
                    warm_wall_ms: run.warm_wall.map(|d| d.as_millis() as u64),
                    precision: precision(&run.labels, &cambium_set),
                    depth_agreement: run
                        .depths
                        .as_ref()
                        .and_then(|d| crate::tools::tango::depth_agreement(&cambium_depths, d)),
                    dropped_file_targets: run.dropped_file_targets,
                    cambium_only: cambium_set.difference(&run.labels).count(),
                    tool_only: run.labels.difference(&cambium_set).count(),
                    counts,
                    examples,
                    cambium_only_reasons: reason_histogram(
                        cambium_set.difference(&run.labels),
                        report.explanations.as_deref().unwrap_or_default(),
                    ),
                    pass: failures == 0,
                    ..ToolComparison::new(p.name(), p.version())
                }
            }
            Err(e) => {
                tracing::warn!(
                    tool = p.name(),
                    error = format!("{e:#}"),
                    "tool failed on this change set"
                );
                ToolComparison::failed(p.name(), p.version(), &e)
            }
        };
        results.push(cmp);
    }

    // ---- the `.bzl` load-closure oracle ----
    //
    // Only on change sets that changed a `.bzl`, which is where the static closure
    // is the thing under test. `engine.bazel` is the head checkout, which is
    // the tree `rbuildfiles` has to be asked about.
    let load_oracle = match opts.load_oracle {
        true => crate::oracles::load::run(
            &engine.bazel,
            &changed,
            &report.dirty_packages.iter().cloned().collect(),
        )
        .with_context(|| format!("bzl load-closure oracle at {}", change_set.head))?,
        false => None,
    };
    if let Some(o) = &load_oracle {
        if !o.ok() {
            tracing::error!(
                missing = ?o.missing_packages,
                checked = ?o.checked,
                "rbuildfiles names packages the static load closure did not dirty"
            );
        }
    }

    let pass = results.iter().all(|p| p.pass)
        && oracle.as_ref().map_or(true, |o| o.ok())
        && load_oracle.as_ref().map_or(true, |o| o.ok());

    Ok(ChangeSetResult {
        schema: crate::RESULT_SCHEMA,
        base: change_set.base.clone(),
        head: change_set.head.clone(),
        kind: change_set.kind.name().to_string(),
        category: change_set.category.clone(),
        cambium,
        tools: results,
        oracle,
        load_oracle,
        recall: oracle_recall,
        global: false,
        pass,
    })
}

/// The base index, for the oracle's deleted half, or `None`.
///
/// Best effort by design: the oracle is a check, and a check that cannot be
/// made is reported ([`crate::oracles::rdeps::RdepsResult::deleted_checked`]),
/// not raised. The index is in the store because the run above published it;
/// a store that will not read, or a base whose index was never published,
/// costs the deleted half of one change set and nothing else.
fn base_index_for_oracle(engine: &Engine, base: &str) -> Option<cambium_index::IndexFile> {
    let rev = engine
        .repo
        .rev_parse(base)
        .unwrap_or_else(|_| base.to_string());
    let path = match engine.fetch_baseline(&rev) {
        Ok(p) => p?,
        Err(e) => {
            tracing::warn!(%base, error = format!("{e:#}"), "no base index for the oracle's deleted half");
            return None;
        }
    };
    match cambium_index::IndexFile::open(&path) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!(%base, error = format!("{e:#}"), "could not open the base index for the oracle");
            None
        }
    }
}

/// A previous file continues this run only when the same cambium wrote it:
/// the engine id names the index format and the engine configuration, and a
/// change set answered under another one is a different experiment.
fn resumable(prev: &ChangesResult, opts: &ChangesOptions, engine_id: &str) -> bool {
    prev.repo == opts.repo_id && prev.pin == opts.pin && prev.engine == engine_id
}

fn fresh_result(opts: &ChangesOptions, engine_id: &str) -> ChangesResult {
    ChangesResult {
        schema: crate::RESULT_SCHEMA,
        repo: opts.repo_id.clone(),
        pin: opts.pin.clone(),
        bazel_version: opts.bazel_version.clone(),
        bazel_version_used: None,
        seed: opts.seed,
        oracle_k: opts.oracle_k,
        engine: engine_id.to_string(),
        tango_change_sets: opts.tango_change_sets,
        change_sets: Vec::new(),
        summary: ChangesSummary::default(),
    }
}

/// Every change set, resumable, writing after each one.
///
/// A change set that errors outright (a tool that cannot run at all, a base
/// revision Bazel refuses to load) stops the run rather than being skipped: a
/// comparison that silently drops the hard change sets is worse than no comparison.
pub fn compare_change_sets(
    engine: &Engine,
    checkout: &Checkout,
    change_sets: &[ChangeSet],
    tools: &[&dyn Tool],
    opts: &ChangesOptions,
) -> Result<ChangesResult> {
    let mut owned = opts.clone();
    owned.make_absolute();
    let opts = &owned;
    let path = opts.output_path();
    let mut state = State::load_for(
        &opts.state_dir,
        &opts.repo_id,
        &opts.pin,
        &State::engine_id(&engine.cfg.hash()),
    )?;
    let engine_id = State::engine_id(&engine.cfg.hash());
    let mut result = match (opts.resume, ChangesResult::read(&path)) {
        (true, Ok(prev)) if resumable(&prev, opts, &engine_id) => prev,
        (true, Ok(prev)) if prev.repo == opts.repo_id && prev.pin == opts.pin => {
            tracing::warn!(
                was = %prev.engine,
                now = %engine_id,
                "change sets were compared by a different cambium; starting over"
            );
            fresh_result(opts, &engine_id)
        }
        _ => fresh_result(opts, &engine_id),
    };
    // Recorded once per run, and re-recorded on a resume whose earlier half
    // predates the field: the report's numbers are only worth reading if they
    // name the Bazel that actually answered.
    if result.bazel_version_used.is_none() {
        let v = engine.bazel.version();
        let v = v.lines().next().unwrap_or("").trim().to_string();
        if !v.is_empty() {
            result.bazel_version_used = Some(v);
        }
    }
    let done: BTreeSet<String> = result.change_sets.iter().map(|p| p.key()).collect();

    for (i, change_set) in change_sets.iter().enumerate() {
        let key = change_set.key();
        if opts.resume && done.contains(&key) {
            tracing::debug!(%key, "already compared");
            continue;
        }
        // The cap is on the *change set index*, not on how many Tango has managed so
        // far, so which change sets it runs on is the same set whether the run
        // finished in one go or resumed at change set 30.
        let running: Vec<&dyn Tool> = tools
            .iter()
            .copied()
            .filter(|p| {
                p.name() != crate::tools::tango::NAME
                    || opts.tango_change_sets.map_or(true, |n| i < n)
            })
            .collect();
        tracing::info!(
            change_set = i + 1,
            of = change_sets.len(),
            %key,
            kind = change_set.kind.name(),
            tools = running.len(),
            "comparing"
        );
        let cmp = compare_change_set_with(engine, checkout, change_set, &running, opts)?;
        if !cmp.pass {
            tracing::warn!(%key, "change set failed the three-way comparison");
        }
        result.change_sets.push(cmp);
        result.summarise();
        result.write(&path)?;
        state.compared.insert(key);
        state.save(&opts.state_dir)?;
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
        fn reached_via_external_repo(&self, l: &str) -> bool {
            l.ends_with(":from_external_repo")
        }
        fn changed_path_without_consumer(&self, _: &str) -> bool {
            false
        }
        fn build_file_changed(&self, _: &str) -> bool {
            false
        }
        fn reached_via_deleted_dep(&self, l: &str) -> bool {
            l.ends_with(":orphaned")
        }
        fn reached_via_build_file(&self, l: &str) -> bool {
            l.ends_with(":globbed")
        }
        fn rule_unchanged_modulo_visibility(&self, _: &str) -> bool {
            false
        }
        fn no_changed_source_inputs(&self, _: &str) -> bool {
            false
        }
        fn no_affected_deps(&self, _: &str) -> bool {
            false
        }
        fn has_external_input(&self, _: &str) -> bool {
            false
        }
        fn workspace_path_only_change(&self, _: &str) -> bool {
            false
        }
    }

    #[test]
    fn precision_is_the_tools_share_of_cambiums_answer() {
        let cambium = set(&["//a:a", "//b:b", "//c:c", "//d:d"]);
        assert_eq!(precision(&set(&["//a:a", "//b:b"]), &cambium), 0.5);
        assert_eq!(precision(&cambium, &cambium), 1.0);
        assert_eq!(
            precision(&set(&["//z:z"]), &cambium),
            0.0,
            "labels the tool alone reports do not raise precision"
        );
    }

    #[test]
    fn an_empty_cambium_answer_counts_as_agreement() {
        assert_eq!(precision(&BTreeSet::new(), &BTreeSet::new()), 1.0);
    }

    #[test]
    fn the_symmetric_difference_is_classified_from_both_sides() {
        let cambium = set(&["//a:a", "//p:selected", "//shared:s"]);
        let tool = set(&["//shared:s", "@ext//x:y", "//missed:m"]);
        let (counts, examples) = classify_difference(&cambium, &tool, &NoFacts, 10);
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
    fn a_tool_that_could_not_run_has_not_agreed_with_anything() {
        let err = anyhow::anyhow!("exit 1: stderr tail").context("hashing abc123 in /tmp/base");
        let c = ToolComparison::failed("bazel-diff-rust", "v46.1.0", &err);
        assert!(!c.pass, "a comparison that did not happen does not pass");
        // The whole chain, not just anyhow's outermost context: without the
        // alternate flag the exit status the tool attached never arrives.
        let e = c.error.unwrap();
        assert!(e.contains("hashing abc123"), "{e}");
        assert!(e.contains("exit 1"), "{e}");
        assert_eq!(c.name, "bazel-diff-rust");
        assert_eq!(c.version, "v46.1.0");
        // And a tool that did run starts from the passing default.
        assert!(ToolComparison::new("t", "v").pass);
    }

    #[test]
    fn the_example_list_is_capped_but_the_counts_are_not() {
        let cambium: BTreeSet<String> = (0..50).map(|i| format!("//a:t{i}")).collect();
        let (counts, examples) = classify_difference(&cambium, &BTreeSet::new(), &NoFacts, 3);
        assert_eq!(counts["unclassified"], 50);
        assert_eq!(examples.len(), 3);
    }

    fn change_set_comparison(
        name: &str,
        precision: f64,
        cold: u64,
        warm: u64,
        pass: bool,
    ) -> ChangeSetResult {
        let mut counts = BTreeMap::new();
        counts.insert("cambium_extra_select".to_string(), 2);
        if !pass {
            counts.insert("cambium_missing".to_string(), 1);
        }
        ChangeSetResult {
            schema: crate::RESULT_SCHEMA,
            base: format!("b{name}"),
            head: format!("h{name}"),
            kind: "adjacent".into(),
            category: None,
            cambium: CambiumRun {
                cold_wall_ms: cold * 10,
                warm_wall_ms: warm,
                ..Default::default()
            },
            tools: vec![ToolComparison {
                name: "bazel-diff-rust".into(),
                version: "v46".into(),
                labels: 10,
                cold_wall_ms: cold,
                warm_wall_ms: Some(warm * 2),
                precision,
                depth_agreement: None,
                dropped_file_targets: 0,
                cambium_only: 2,
                tool_only: 0,
                counts,
                examples: Vec::new(),
                cambium_only_reasons: BTreeMap::new(),
                pass,
                error: None,
            }],
            oracle: None,
            load_oracle: None,
            recall: if pass { 1.0 } else { 0.75 },
            global: false,
            pass,
        }
    }

    #[test]
    fn the_summary_counts_classes_averages_precision_and_takes_the_worst_recall() {
        let change_sets = vec![
            change_set_comparison("1", 1.0, 100, 10, true),
            change_set_comparison("2", 0.5, 300, 30, false),
        ];
        let s = summarise(&change_sets);
        assert_eq!(s.change_sets, 2);
        assert_eq!(s.passed, 1);
        assert!(!s.pass, "one failing change set fails the run");
        assert_eq!(s.by_class["cambium_extra_select"], 4);
        assert_eq!(s.by_class["cambium_missing"], 1);
        assert_eq!(
            s.by_class["bazeldiff_extra_external"], 0,
            "every class is listed"
        );
        assert_eq!(s.mean_precision["bazel-diff-rust"], 0.75);
        assert_eq!(s.min_recall, 0.75);
        assert_eq!(
            s.tool_cold["bazel-diff-rust"],
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
    fn a_tool_that_could_not_run_fails_its_change_set() {
        // The absence has to be loud. A tool that errored contributes no
        // labels, so every aggregate simply omits it and the summary renders
        // as though the remaining tools were the whole comparison — which is
        // how the 2026-09-08 run published sourcegraph with no bazel-diff and
        // apollo with no Tango, both reading as clean.
        let mut broken = change_set_comparison("1", 1.0, 100, 10, true);
        broken.tools[0].error = Some("aspect query: ^D".into());
        broken.tools[0].pass = false;
        broken.pass = broken.tools.iter().all(|t| t.pass);
        let s = summarise(&[broken]);
        assert_eq!(s.passed, 0);
        assert!(!s.pass, "a comparison that did not happen is not a pass");
    }

    #[test]
    fn a_tool_that_failed_does_not_pollute_the_averages() {
        let mut change_sets = vec![change_set_comparison("1", 1.0, 100, 10, true)];
        let mut broken = change_set_comparison("2", 0.0, 0, 0, true);
        broken.tools[0].error = Some("boom".into());
        broken.tools[0].counts.clear();
        change_sets.push(broken);
        let s = summarise(&change_sets);
        assert_eq!(s.mean_precision["bazel-diff-rust"], 1.0);
        assert_eq!(s.tool_cold["bazel-diff-rust"].samples, 1);
    }

    /// The same change set with a `tango` row bolted on, as an uncapped run
    /// would produce.
    fn with_tango(mut p: ChangeSetResult) -> ChangeSetResult {
        let mut t = p.tools[0].clone();
        t.name = crate::tools::tango::NAME.to_string();
        t.cold_wall_ms = 300_000;
        t.warm_wall_ms = None;
        p.tools.push(t);
        p
    }

    #[test]
    fn a_change_set_tango_was_capped_out_of_is_not_a_disagreement() {
        // Two change sets, Tango on the first only: what `--quick` produces.
        let capped = summarise(&[
            with_tango(change_set_comparison("1", 1.0, 100, 10, true)),
            change_set_comparison("2", 1.0, 100, 10, true),
        ]);
        assert!(capped.pass, "a change_set Tango skipped still passes");
        assert_eq!((capped.change_sets, capped.passed), (2, 2));
        assert_eq!(
            capped.tool_change_sets[crate::tools::tango::NAME],
            1,
            "the summary says how many change_sets Tango's numbers come from"
        );
        assert_eq!(capped.tool_change_sets["bazel-diff-rust"], 2);
        assert_eq!(capped.tool_cold[crate::tools::tango::NAME].samples, 1);
        // The change sets Tango did run on are scored exactly as they would be with
        // no cap at all.
        let uncapped = summarise(&[
            with_tango(change_set_comparison("1", 1.0, 100, 10, true)),
            with_tango(change_set_comparison("2", 1.0, 100, 10, true)),
        ]);
        assert_eq!(
            capped.mean_precision[crate::tools::tango::NAME],
            uncapped.mean_precision[crate::tools::tango::NAME]
        );
        assert_eq!(capped.by_class, uncapped_minus_tango(&uncapped));
    }

    /// The class totals `uncapped` would have had with one fewer Tango row.
    fn uncapped_minus_tango(uncapped: &ChangesSummary) -> BTreeMap<String, usize> {
        let mut totals = uncapped.by_class.clone();
        *totals.get_mut("cambium_extra_select").unwrap() -= 2;
        totals
    }

    /// A globally invalidated change set as the driver now records one: cambium
    /// answered `//...` (24 000 rule targets) and the tool still ran, took
    /// 999 ms and found 40 targets precisely.
    fn global_change_set(name: &str) -> ChangeSetResult {
        let mut p = change_set_comparison(name, 1.0, 999, 999, true);
        p.tools[0].labels = 40;
        p.tools[0].counts.clear();
        p.tools[0].cambium_only = 0;
        p.cambium.global = true;
        p.cambium.global_reason = Some("MODULE.bazel".into());
        p.cambium.universe = 24_000;
        p.global = true;
        p
    }

    #[test]
    fn a_global_change_set_is_counted_apart_and_leaves_the_soundness_numbers_alone() {
        let s = summarise(&[
            change_set_comparison("1", 0.5, 100, 10, true),
            global_change_set("2"),
        ]);
        assert_eq!((s.change_sets, s.passed, s.globals), (2, 2, 1));
        assert!(s.pass, "answering //... is not a failure");
        assert_eq!(
            s.mean_precision["bazel-diff-rust"], 0.5,
            "the global change_set contributes no precision"
        );
        assert_eq!(
            s.by_class["cambium_extra_select"], 2,
            "and no classified labels"
        );
        assert_eq!(
            s.tool_change_sets["bazel-diff-rust"], 1,
            "the incremental tool count skips it"
        );
    }

    #[test]
    fn a_global_change_set_still_pays_for_its_tools_and_records_what_they_found() {
        let g = global_change_set("2");
        assert_eq!(
            g.tools.len(),
            1,
            "the tool ran: a global's cost is the thing being measured"
        );
        assert_eq!((g.tools[0].labels, g.cambium.universe), (40, 24_000));
        assert!(g.tools[0].counts.is_empty(), "nothing was classified");

        let s = summarise(&[change_set_comparison("1", 0.5, 100, 10, true), g]);
        let gs = &s.global_stats;
        assert_eq!(gs.change_sets, 1);
        assert_eq!(gs.mean_cambium_labels, 24_000.0);
        assert_eq!(gs.mean_tool_labels["bazel-diff-rust"], 40.0);
        assert_eq!(gs.tool_cold["bazel-diff-rust"].p50_ms, 999);
        assert_eq!(gs.cambium_cold.p50_ms, 9_990, "999 * 10, as the fixture");
        assert_eq!(gs.cambium_warm.p50_ms, 999);
    }

    #[test]
    fn the_all_change_sets_walls_include_the_globals_and_the_incremental_walls_do_not() {
        let s = summarise(&[
            change_set_comparison("1", 0.5, 100, 10, true),
            global_change_set("2"),
        ]);
        assert_eq!(
            (s.cambium_cold.samples, s.cambium_warm.samples),
            (1, 1),
            "the incremental series is over the walked change_set only"
        );
        assert_eq!(
            (s.all_cambium_cold.samples, s.all_cambium_warm.samples),
            (2, 2),
            "the all-change_sets series hides nothing"
        );
        assert_eq!(s.tool_cold["bazel-diff-rust"].samples, 1);
        assert_eq!(s.all_tool_cold["bazel-diff-rust"].samples, 2);
        assert_eq!(s.all_tool_change_sets["bazel-diff-rust"], 2);
        // The global is the slow change set, so it can only move the p90 up.
        assert!(
            s.all_cambium_warm.p90_ms > s.cambium_warm.p90_ms,
            "including the globals must not flatter cambium: {s:?}"
        );
    }

    #[test]
    fn a_cold_rebuild_change_set_is_incremental_but_counted() {
        let mut rebuilt = change_set_comparison("2", 1.0, 100, 10, true);
        rebuilt.cambium.cold_rebuild = true;
        let s = summarise(&[change_set_comparison("1", 1.0, 100, 10, true), rebuilt]);
        assert_eq!(s.cold_rebuild_change_sets, 1);
        assert_eq!(
            s.cambium_warm.samples, 2,
            "it is not a global: it stays in every statistic"
        );
        assert_eq!(s.globals, 0);
    }

    #[test]
    fn a_run_with_no_globals_carries_no_global_statistics() {
        let s = summarise(&[change_set_comparison("1", 1.0, 100, 10, true)]);
        assert!(s.global_stats.is_empty());
        assert_eq!(s.global_stats, GlobalStats::default());
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            !json.contains("global_stats"),
            "an empty table is left out of the JSON: {json}"
        );
    }

    /// Every field added since the first `changes-*.json` was written has a
    /// serde default, so the report can still read what is on the box.
    #[test]
    fn a_summary_written_before_the_all_change_sets_series_still_parses() {
        let old = r#"{
            "change_sets": 3, "passed": 3, "by_class": {}, "mean_precision": {},
            "min_recall": 1.0, "tool_cold": {}, "tool_warm": {},
            "cambium_cold": {"samples": 3, "p50_ms": 10, "p90_ms": 20},
            "cambium_warm": {"samples": 3, "p50_ms": 5, "p90_ms": 8},
            "pass": true
        }"#;
        let s: ChangesSummary = serde_json::from_str(old).unwrap();
        assert_eq!(
            (s.change_sets, s.globals, s.cold_rebuild_change_sets),
            (3, 0, 0)
        );
        assert_eq!(s.all_cambium_cold, WallStats::default());
        assert!(s.all_tool_change_sets.is_empty());
        assert!(s.global_stats.is_empty());

        let change_set = r#"{
            "schema": 3, "base": "b", "head": "h", "kind": "adjacent",
            "cambium": {"direct": 1, "affected": 2, "tests": 0, "global": false,
                        "cold_wall_ms": 9, "warm_wall_ms": 4, "dirty_packages": 1,
                        "queried_targets": 2, "changed_files": 1},
            "tools": [], "recall": 1.0, "pass": true
        }"#;
        let p: ChangeSetResult = serde_json::from_str(change_set).unwrap();
        assert!(!p.cambium.cold_rebuild);
        assert_eq!(p.cambium.universe, 0);
        assert_eq!(p.cambium.answer_size(), 2);
    }

    #[test]
    fn options_are_forced_absolute() {
        let opts = ChangesOptions::new("bazel", "abc", PathBuf::from("bench-results/bazel"));
        assert!(opts.results_dir.is_absolute(), "{opts:?}");
        assert!(opts.scratch.is_absolute(), "{opts:?}");
        let mut hand_built = ChangesOptions {
            results_dir: "bench-results/bazel".into(),
            scratch: "bench-results/bazel/scratch".into(),
            ..opts
        };
        hand_built.make_absolute();
        assert!(hand_built.results_dir.is_absolute());
        assert!(hand_built.scratch.is_absolute());
        assert!(hand_built.output_path().is_absolute());
    }

    #[test]
    fn percentiles_are_taken_over_the_sorted_walls() {
        assert_eq!(wall_stats(vec![]), WallStats::default());
        let s = wall_stats(vec![50, 10, 30, 20, 40, 60, 70, 80, 90, 100]);
        assert_eq!((s.samples, s.p50_ms, s.p90_ms), (10, 60, 100));
    }

    #[test]
    fn change_sets_compared_by_another_cambium_are_not_resumed() {
        let opts = ChangesOptions::new("bazel", "abc", PathBuf::from("/r/bazel"));
        let mut prev = fresh_result(&opts, "f8/0123456789abcdef");
        assert!(resumable(&prev, &opts, "f8/0123456789abcdef"));
        prev.engine = "f6/0123456789abcdef".into();
        assert!(!resumable(&prev, &opts, "f8/0123456789abcdef"));
        // Files written before the field existed carry no engine at all.
        prev.engine.clear();
        assert!(!resumable(&prev, &opts, "f8/0123456789abcdef"));
    }

    #[test]
    fn the_output_file_names_the_bazel_version_only_in_the_matrix() {
        let dir = Path::new("/r");
        assert_eq!(
            ChangesOptions::path(dir, "abc", None),
            PathBuf::from("/r/changes-abc.json")
        );
        assert_eq!(
            ChangesOptions::path(dir, "abc", Some("7.4.1")),
            PathBuf::from("/r/changes-abc-7.4.1.json")
        );
        assert_eq!(
            ChangesOptions::path(dir, "abc", Some("")),
            PathBuf::from("/r/changes-abc.json")
        );
    }

    #[test]
    fn a_result_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = ChangesResult {
            schema: crate::RESULT_SCHEMA,
            repo: "bazel".into(),
            pin: "abc".into(),
            bazel_version: Some("7.4.1".into()),
            bazel_version_used: Some("bazel 7.4.1".into()),
            seed: 7,
            oracle_k: 0,
            engine: String::new(),
            tango_change_sets: None,
            change_sets: vec![change_set_comparison("1", 1.0, 100, 10, true)],
            summary: ChangesSummary::default(),
        };
        r.summarise();
        let path = dir.path().join("changes-abc.json");
        r.write(&path).unwrap();
        let back = ChangesResult::read(&path).unwrap();
        assert_eq!(back.schema, crate::RESULT_SCHEMA);
        assert_eq!(back.change_sets.len(), 1);
        assert!(back.summary.pass);
    }
}
