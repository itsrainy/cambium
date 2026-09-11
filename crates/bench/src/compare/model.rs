//! What a comparison *is*: the options it takes and the result it writes.
//!
//! Every type here is serialised into
//! `bench-results/<subject>/changes/changes-<pin>.json` and read back by the
//! report renderer, so a field added or renamed here is a
//! [`crate::RESULT_SCHEMA`] bump. Nothing in this file runs a tool or a Bazel.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::changes::{ChangeSetKind, StressCategory};
use crate::classify::{Class, Side};
use crate::oracles::load::{LoadOracleResult, LoadOracleSummary};
use crate::oracles::rdeps::RdepsResult;
use crate::state::State;

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
    /// Where to report progress, for `cambium-bench watch`. `None` compares
    /// exactly as before and writes no progress file.
    pub progress: Option<std::sync::Arc<crate::progress::Reporter>>,
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
            progress: None,
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
    /// Whole-process wall time with the base index resolved from scratch:
    /// what a user waits for, `cambi` starting and serialising its answer
    /// included.
    pub cold_wall_ms: u64,
    /// The same, with the base index already in the store. This is the number
    /// a CI job pays.
    pub warm_wall_ms: u64,
    /// The engine's own `timings_ms.total` from the cold run's document: the
    /// wall time above minus everything the process paid around the
    /// computation. Both are recorded because neither alone is honest — one
    /// hides the process, the other charges the engine for it. 0 in results
    /// written before the harness ran `cambi` as a child process.
    #[serde(default)]
    pub cold_engine_ms: u64,
    /// The warm run's `timings_ms.total`.
    #[serde(default)]
    pub warm_engine_ms: u64,
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
    pub counts: BTreeMap<Class, usize>,
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
    pub fn failed(
        name: impl Into<String>,
        version: impl Into<String>,
        err: &anyhow::Error,
    ) -> Self {
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
    pub kind: ChangeSetKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<StressCategory>,
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
    pub by_class: BTreeMap<Class, usize>,
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
    /// `cambi --version` of the binary this run measured, verbatim — the
    /// version and the short sha it was built from. The harness runs `cambi`
    /// as a child process, so a result names the binary that produced it
    /// rather than the crate the harness was linked against. Absent from
    /// results written before the harness shelled out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cambi_version: Option<String>,
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
        self.summary = super::summary::summarise(&self.change_sets);
    }
}

// ------------------------------------------------------------------- the math
