//! What a walk *is*: the per-step record, the whole result, and the summary
//! that rolls the steps up.
//!
//! Every type here is serialised into `bench-results/<subject>/walk/walk-
//! <pin>.json` and read back by the report and the replay, so a field added
//! or renamed is a [`crate::RESULT_SCHEMA`] bump. Nothing here runs a tool.
//!
//! The three questions every number answers, for every tool alike: did it
//! miss anything (recall against the rdeps oracle), how much wider than the
//! truth was it (tightness against the oracle), what did it cost (per-step
//! wall in the shape CI pays). cambium is a row in the per-tool tables, never
//! the denominator: the oracle is ground truth for it too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::env::Environment;
use crate::oracles::rdeps::RdepsResult;
use crate::util::percentile;

use super::rebuild_rows::RowCounts;
use super::sampling::{Coverage, Sample};
use super::steps::Tag;
use super::tier::{Schedule, Tier};

/// One tool's answer on one step, scored against the oracle.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolStep {
    pub name: String,
    pub version: String,
    /// How many labels the tool named, restricted to the rule universe.
    pub labels: usize,
    /// Whole-process wall for this step. A peer is stateless, so this is a
    /// full run; cambium's is the incremental step.
    pub wall_ms: u64,
    /// `|answer ∩ oracle| / |oracle|`: what the tool did not miss. `None`
    /// when the oracle did not run on this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_vs_oracle: Option<f64>,
    /// `|oracle| / |answer|`: how tight the answer is around the truth. 1.0
    /// is exact; below 1.0 is over-approximation. `None` when the oracle did
    /// not run or the answer was empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tightness_vs_oracle: Option<f64>,
    /// The tool could not run: its wall and sizes are absent and the step
    /// does not pass.
    #[serde(default)]
    pub failed: bool,
}

/// One step of the walk: what changed, what cambium did, what it cost, and
/// what every tool and the oracle said.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StepRecord {
    pub parent: String,
    pub head: String,
    pub tags: Vec<Tag>,
    pub changed_files: usize,
    pub dirty_packages: usize,
    pub queried_targets: usize,
    /// The engine's own per-stage timings, from the document.
    pub timings: StepTimings,
    /// Whole-process wall for cambium's incremental step.
    pub wall_ms: u64,
    /// The index for the parent was carried from the previous step's output,
    /// not built. False on the first step after a cold rebuild.
    pub carried_index: bool,
    /// This step ran a mid-walk rebuild oracle.
    #[serde(default)]
    pub rebuild_step: bool,
    /// cambium answered `//...`.
    pub global: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_reason: Option<String>,
    pub index: IndexCounts,
    pub cambium: CambiumAnswer,
    /// The rdeps oracle, when it ran on this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle: Option<RdepsResult>,
    /// Every tool that ran, cambium first. Scored against the oracle above.
    pub tools: Vec<ToolStep>,
    /// Whether the step is sound: the oracle found nothing cambium missed,
    /// and every tool that was asked to run did.
    pub pass: bool,
}

impl StepRecord {
    pub fn key(&self) -> String {
        crate::state::State::change_set_key(&self.parent, &self.head)
    }

    /// cambium's own recall against the oracle, or 1.0 when the oracle did
    /// not run (it proved nothing, it did not fail).
    pub fn cambium_recall(&self) -> f64 {
        self.oracle.as_ref().map(|o| o.recall()).unwrap_or(1.0)
    }
}

/// The engine's per-stage timings, from the `affected` document.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StepTimings {
    pub dirty: u64,
    pub query: u64,
    pub splice: u64,
    pub direct: u64,
    pub walk: u64,
    /// The engine's own `timings_ms.total`: the wall minus process start and
    /// serialisation.
    pub engine_total: u64,
}

/// The head index the step produced.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexCounts {
    pub nodes: u32,
    pub edges: u64,
    /// Lineage, hex: a fresh lineage means a cold rebuild happened here.
    #[serde(default)]
    pub lineage: String,
}

/// What cambium said, by size.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CambiumAnswer {
    pub direct: usize,
    pub affected: usize,
    pub tests: usize,
    /// The engine re-indexed `//...` because the dirty set overflowed: the
    /// answer is exact and the lineage survives, but the wall is a cold one.
    #[serde(default)]
    pub cold_rebuild: bool,
}

/// Wall-clock distribution.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WallStats {
    pub samples: usize,
    pub p50_ms: u64,
    pub p90_ms: u64,
    pub max_ms: u64,
}

/// The cost of the walk, in the shape CI pays it: the two populations kept
/// apart, because a peer's per-step wall is a full run and cambium's is an
/// increment.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cost {
    /// cambium's incremental steps.
    pub cambium_walk: WallStats,
    /// The steps that rebuilt instead of splicing, and what they cost.
    pub rebuild_steps: usize,
    pub rebuild_wall: WallStats,
    /// The one cold index build per subject, at the start of the window.
    pub index_build_ms: u64,
    /// Each peer's wall over the steps it ran on: a full run per step.
    pub peer_wall: BTreeMap<String, WallStats>,
}

/// How each tool did against the oracle, over the steps the oracle ran on.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Tightness {
    pub p50: f64,
    pub p90: f64,
    /// Steps this tool was scored on.
    pub steps: usize,
}

/// The soundness verdict for the run.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Soundness {
    /// Steps the oracle ran on and cambium contained everything.
    pub steps_sound: usize,
    /// Steps the oracle found cambium missed something on.
    pub steps_missing: usize,
    /// The worst recall any step achieved; below 1.0 fails the run.
    pub worst_recall: f64,
    /// The step that achieved it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worst_step: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WalkSummary {
    pub steps: usize,
    pub steps_with_oracle: usize,
    pub steps_with_peers: usize,
    /// Per-tag coverage of the whole walk: present, sampled for the oracle,
    /// and wanted.
    pub coverage: BTreeMap<Tag, Coverage>,
    pub soundness: Soundness,
    /// Tightness p50/p90 per tool, cambium included as a row.
    pub tightness: BTreeMap<String, Tightness>,
    pub cost: Cost,
    /// Steps cambium answered `//...`.
    pub globals: usize,
    /// Steps the engine answered with a whole-repository re-query. Not
    /// globals: the answer was exact.
    pub engine_cold_rebuilds: usize,
    pub pass: bool,
}

impl Default for WalkSummary {
    fn default() -> Self {
        Self {
            steps: 0,
            steps_with_oracle: 0,
            steps_with_peers: 0,
            coverage: BTreeMap::new(),
            soundness: Soundness {
                worst_recall: 1.0,
                ..Default::default()
            },
            tightness: BTreeMap::new(),
            cost: Cost::default(),
            globals: 0,
            engine_cold_rebuilds: 0,
            pass: true,
        }
    }
}

/// The rebuild oracle at the pin: applied index versus a fresh cold build.
///
/// `ok` counts the loading layer by its unexplained missing rows
/// ([`super::rebuild_rows`]), and it is folded into [`WalkSummary::pass`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RebuildOracle {
    pub ok: bool,
    /// A human summary of the diff; empty when it held.
    pub summary: String,
    /// The sidecar beside this file that lists every loading-layer row,
    /// classified: `rebuild-<pin>[-<ver>].json` under `walk/`. Absent from
    /// records written before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<String>,
    /// The sidecar's tallies.
    #[serde(default)]
    pub sky: RowCounts,
}

/// One subject's walk at one pin.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalkResult {
    pub schema: u32,
    pub repo: String,
    pub pin: String,
    pub from: String,
    pub to: String,
    pub tier: Tier,
    /// The schedule the tier resolved to, flags applied.
    pub schedule: Schedule,
    /// The sample the oracle ran on (or every step, on the full tier).
    pub oracle_sample: Sample,
    /// The sample the peers ran on.
    pub peer_sample: Sample,
    pub environment: Environment,
    pub started_unix: u64,
    /// The cold build at the start of the window.
    pub index_build_ms: u64,
    pub steps: Vec<StepRecord>,
    /// The rebuild oracle at the pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebuild_oracle: Option<RebuildOracle>,
    /// Where the pin's freshly built index was archived, for `replay
    /// --head-check`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_index: Option<PathBuf>,
    pub summary: WalkSummary,
    pub done: bool,
}

impl WalkResult {
    pub fn path(walk_dir: &Path, pin: &str, version: Option<&str>) -> PathBuf {
        walk_dir.join(crate::util::versioned_name(
            &format!("walk-{pin}"),
            version,
            "json",
        ))
    }

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

    /// Rolls the steps up, and folds the pin's rebuild oracle into `pass`: a
    /// walk whose applied index disagrees with a cold build at the pin did
    /// not pass, whatever every step said. Per step the oracle is still
    /// `None` and `pass` is the steps' alone.
    pub fn summarise(&mut self) {
        self.summary = summarise(
            &self.steps,
            &self.oracle_sample,
            &self.peer_sample,
            self.index_build_ms,
        );
        self.summary.pass = self.summary.pass && self.rebuild_oracle.as_ref().is_none_or(|o| o.ok);
    }
}

/// p50/p90/max over a set of walls.
pub fn wall_stats(mut ms: Vec<u64>) -> WallStats {
    ms.sort_unstable();
    WallStats {
        samples: ms.len(),
        p50_ms: percentile(&ms, 0.5),
        p90_ms: percentile(&ms, 0.9),
        max_ms: ms.last().copied().unwrap_or(0),
    }
}

/// p50/p90 over a set of ratios.
fn ratio_stats(mut v: Vec<f64>) -> (f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let at = |q: f64| {
        let idx = ((v.len() as f64 - 1.0) * q).round() as usize;
        v[idx.min(v.len() - 1)]
    };
    (at(0.5), at(0.9))
}

/// Rolls the steps up. Pure, so the arithmetic is tested without a Bazel.
///
/// Tightness and recall are taken over the steps the oracle ran on, per tool.
/// cambium is a tool row like any other: its answer is scored against the
/// same oracle, so a table cannot silently make cambium the yardstick it is
/// being measured against.
pub fn summarise(
    steps: &[StepRecord],
    oracle_sample: &Sample,
    peer_sample: &Sample,
    index_build_ms: u64,
) -> WalkSummary {
    let mut s = WalkSummary {
        steps: steps.len(),
        coverage: oracle_sample.coverage.clone(),
        ..Default::default()
    };
    s.steps_with_oracle = steps.iter().filter(|st| st.oracle.is_some()).count();
    s.steps_with_peers = peer_sample.len().min(steps.len());
    s.globals = steps.iter().filter(|st| st.global).count();
    s.engine_cold_rebuilds = steps.iter().filter(|st| st.cambium.cold_rebuild).count();

    // Soundness, from cambium's recall against the oracle.
    for st in steps.iter().filter(|st| st.oracle.is_some()) {
        let recall = st.cambium_recall();
        if recall < s.soundness.worst_recall {
            s.soundness.worst_recall = recall;
            s.soundness.worst_step = st.key();
        }
        if st.oracle.as_ref().is_some_and(|o| o.ok()) {
            s.soundness.steps_sound += 1;
        } else {
            s.soundness.steps_missing += 1;
        }
    }

    // Tightness per tool, over the steps that tool was scored on.
    let mut tight: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut walls: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut cambium_walk: Vec<u64> = Vec::new();
    let mut rebuild_wall: Vec<u64> = Vec::new();
    for st in steps {
        if st.rebuild_step {
            rebuild_wall.push(st.wall_ms);
        }
        if !st.global {
            cambium_walk.push(st.wall_ms);
        }
        for t in &st.tools {
            if t.failed {
                continue;
            }
            if let Some(tt) = t.tightness_vs_oracle {
                tight.entry(t.name.clone()).or_default().push(tt);
            }
            if t.name != crate::tools::cambi::NAME {
                walls.entry(t.name.clone()).or_default().push(t.wall_ms);
            }
        }
    }
    for (name, v) in tight {
        let steps = v.len();
        let (p50, p90) = ratio_stats(v);
        s.tightness.insert(name, Tightness { p50, p90, steps });
    }

    s.cost = Cost {
        cambium_walk: wall_stats(cambium_walk),
        rebuild_steps: rebuild_wall.len(),
        rebuild_wall: wall_stats(rebuild_wall),
        index_build_ms,
        peer_wall: walls.into_iter().map(|(k, v)| (k, wall_stats(v))).collect(),
    };

    s.pass = s.soundness.steps_missing == 0
        && s.soundness.worst_recall >= 1.0
        && steps.iter().all(|st| st.pass);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, labels: usize, recall: Option<f64>, tight: Option<f64>) -> ToolStep {
        ToolStep {
            name: name.into(),
            version: "v".into(),
            labels,
            wall_ms: 100,
            recall_vs_oracle: recall,
            tightness_vs_oracle: tight,
            failed: false,
        }
    }

    fn step(head: &str, oracle_ok: bool, cambium_recall: f64) -> StepRecord {
        let oracle = RdepsResult {
            oracle_labels: 10,
            matched: (10.0 * cambium_recall) as usize,
            missing: if oracle_ok {
                vec![]
            } else {
                vec!["//x:y".into()]
            },
            ..Default::default()
        };
        StepRecord {
            parent: "p".into(),
            head: head.into(),
            tags: vec![Tag::Adjacent],
            changed_files: 1,
            dirty_packages: 1,
            queried_targets: 1,
            timings: StepTimings::default(),
            wall_ms: 200,
            carried_index: true,
            rebuild_step: false,
            global: false,
            global_reason: None,
            index: IndexCounts::default(),
            cambium: CambiumAnswer::default(),
            oracle: Some(oracle),
            tools: vec![
                tool("cambium", 12, Some(cambium_recall), Some(10.0 / 12.0)),
                tool("bazel-diff", 20, Some(1.0), Some(0.5)),
            ],
            pass: oracle_ok,
        }
    }

    #[test]
    fn cambium_is_a_tightness_row_not_the_denominator() {
        let steps = vec![step("h1", true, 1.0), step("h2", true, 1.0)];
        let sample = Sample::all(
            &steps
                .iter()
                .map(|s| super::super::steps::Step {
                    parent: s.parent.clone(),
                    head: s.head.clone(),
                    tags: s.tags.clone(),
                })
                .collect::<Vec<_>>(),
        );
        let sum = summarise(&steps, &sample, &Sample::default(), 5000);
        assert!(sum.tightness.contains_key("cambium"));
        assert!(sum.tightness.contains_key("bazel-diff"));
        assert_eq!(sum.tightness["bazel-diff"].steps, 2);
        assert_eq!(sum.cost.index_build_ms, 5000);
        assert!(sum.pass);
    }

    fn a_result(steps: Vec<StepRecord>, rebuild_oracle: Option<RebuildOracle>) -> WalkResult {
        let sample = Sample::all(&steps.iter().map(|s| super::super::steps::Step {
            parent: s.parent.clone(),
            head: s.head.clone(),
            tags: s.tags.clone(),
        }).collect::<Vec<_>>());
        WalkResult {
            schema: crate::RESULT_SCHEMA,
            repo: "r".into(),
            pin: "h2".into(),
            from: "p".into(),
            to: "h2".into(),
            tier: Tier::Correctness,
            schedule: Tier::Correctness.defaults(),
            oracle_sample: sample,
            peer_sample: Sample::default(),
            environment: Environment {
                machine: crate::harness::Machine::detect(),
                bazel_version_used: String::new(),
                cambi_version: String::new(),
                tool_sha: String::new(),
                engine_id: String::new(),
                config_hash: String::new(),
                output_bases: Default::default(),
            },
            started_unix: 0,
            index_build_ms: 0,
            steps,
            rebuild_oracle,
            head_index: None,
            summary: WalkSummary::default(),
            done: false,
        }
    }

    /// The pin's verdict lands in `pass`; a walk still in progress, with no
    /// verdict yet, is judged by its steps alone.
    #[test]
    fn a_failed_rebuild_oracle_fails_the_walk_and_none_leaves_it_alone() {
        let steps = vec![step("h1", true, 1.0), step("h2", true, 1.0)];
        let mut r = a_result(steps.clone(), None);
        r.summarise();
        assert!(r.summary.pass);
        r.rebuild_oracle = Some(RebuildOracle {
            ok: true,
            summary: String::new(),
            rows: Some("rebuild-h2.json".into()),
            sky: RowCounts::default(),
        });
        r.summarise();
        assert!(r.summary.pass);
        r.rebuild_oracle = Some(RebuildOracle {
            ok: false,
            summary: "loading layer unexplained: 3".into(),
            rows: Some("rebuild-h2.json".into()),
            sky: RowCounts {
                missing: 3,
                missing_unexplained: 3,
                ..Default::default()
            },
        });
        r.summarise();
        assert!(!r.summary.pass);
        assert_eq!(r.summary.soundness.steps_missing, 0, "the steps were sound");
    }

    #[test]
    fn a_missing_step_fails_the_run_and_names_the_worst() {
        let steps = vec![step("h1", true, 1.0), step("h2", false, 0.8)];
        let sample = Sample::all(
            &steps
                .iter()
                .map(|s| super::super::steps::Step {
                    parent: s.parent.clone(),
                    head: s.head.clone(),
                    tags: s.tags.clone(),
                })
                .collect::<Vec<_>>(),
        );
        let sum = summarise(&steps, &sample, &Sample::default(), 0);
        assert!(!sum.pass);
        assert_eq!(sum.soundness.steps_missing, 1);
        assert!(sum.soundness.worst_recall < 1.0);
        assert!(sum.soundness.worst_step.contains("h2"));
    }
}
