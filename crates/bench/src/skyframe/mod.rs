//! A measurement harness for the Skyframe prototypes.
//!
//! Four prototypes are planned, each on its own branch, each computing the
//! changed targets by leaning on Bazel's Skyframe graph in a different way
//! (docs/SKYFRAME_PROTOTYPES.md). They are only comparable if they are asked
//! the same question on the same steps and scored the same way, so the
//! question, the steps and the score live here on main and a prototype adds
//! one [`Strategy`].
//!
//! The shape of a strategy is two calls per step:
//!
//! - [`Strategy::prepare`] at the step's parent: whatever one-time work the
//!   approach needs before it can answer incrementally (a cold index build, a
//!   warm server load, a dump).
//! - [`Strategy::answer`] at the step's head: the incremental question, which
//!   is the one a CI job asks.
//!
//! The harness checks the worktree out at the parent before `prepare` and at
//! the head before `answer`, times both, and scores the answer against the
//! `rdeps` oracle ([`crate::oracles::rdeps`]) that every other workload in
//! this crate is scored against. Nothing here parses a Skyframe dump; that is
//! each strategy's own business.
//!
//! This is deliberately not the walk ([`crate::walk`]). A walk carries one
//! index forward over a window and is the product's own measurement; these
//! steps are independent of one another, so a strategy that cannot carry
//! state between steps is measurable at all.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use cambium_bazel::Bazel;
use serde::{Deserialize, Serialize};

use crate::walk::steps::{Step, Tag};

pub mod analysis;
pub mod runner;

pub use analysis::AnalysisStrategy;
pub use runner::{compare_files, run_skyframe, Measured, SkyframeRequest};

/// How many steps a run measures when nobody says.
pub const DEFAULT_STEPS: usize = 10;

/// At least this many steps of every tag the window holds.
pub const PER_TAG: usize = 1;

/// One path the step changed, as `git diff --name-status` reports it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChangedPath {
    /// `A`, `M`, `D`, `R100`, and so on: git's own status letter, verbatim.
    pub status: String,
    /// The path at head, or the path that was deleted.
    pub path: String,
}

impl ChangedPath {
    pub fn is_delete(&self) -> bool {
        self.status.starts_with('D')
    }
}

/// `git diff --name-status parent..head`, sorted by path.
///
/// A rename is reported as its destination path plus, because the source is
/// gone at head, a `D` for the source: a strategy that keys off deletions
/// sees the same thing whether git detected the rename or not.
pub fn changed_paths(worktree: &Path, parent: &str, head: &str) -> Result<Vec<ChangedPath>> {
    let out = crate::tools::git(
        worktree,
        &["diff", "--name-status", &format!("{parent}..{head}")],
    )?;
    let mut paths = Vec::new();
    for line in out.lines() {
        let mut fields = line.split('\t');
        let (Some(status), Some(first)) = (fields.next(), fields.next()) else {
            continue;
        };
        let status = status.trim().to_string();
        match fields.next() {
            // A rename or copy: `R100<tab>old<tab>new`.
            Some(second) => {
                paths.push(ChangedPath {
                    status: status.clone(),
                    path: second.to_string(),
                });
                if status.starts_with('R') {
                    paths.push(ChangedPath {
                        status: "D".to_string(),
                        path: first.to_string(),
                    });
                }
            }
            None => paths.push(ChangedPath {
                status,
                path: first.to_string(),
            }),
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Everything a strategy is handed for one step.
///
/// The worktree is already at the parent when `prepare` is called and at the
/// head when `answer` is called; a strategy that needs the other end must
/// make its own worktree under [`StepContext::scratch`] rather than moving
/// this one, which the harness owns.
pub struct StepContext<'a> {
    /// The subject's manifest id, for logging and scratch names.
    pub subject: &'a str,
    /// The subject's worktree, absolute. Also the Bazel workspace.
    pub worktree: &'a Path,
    /// The step: `parent`, `head` and the walk's tags.
    pub step: &'a Step,
    /// `git diff --name-status parent..head`.
    pub changed: &'a [ChangedPath],
    /// The engine's Bazel wrapper for this subject: the binary, the startup
    /// flags that pin the run to its own output base, and the bazelisk
    /// environment. Use it rather than spawning `bazel` directly, or the
    /// prototype measures a different server than the control does.
    pub bazel: &'a Bazel,
    /// The bazelisk environment ([`crate::checkout::bazelisk_env`]), for a
    /// strategy that must spawn something other than Bazel itself.
    pub env: &'a [(String, String)],
    /// A per-step directory a strategy may write anything into. It exists;
    /// nothing else writes to it and nothing deletes it between the two
    /// calls of a step.
    pub scratch: &'a Path,
}

impl StepContext<'_> {
    pub fn parent(&self) -> &str {
        &self.step.parent
    }

    pub fn head(&self) -> &str {
        &self.step.head
    }

    /// The changed paths as the oracle wants them: paths only, sorted.
    pub fn changed_set(&self) -> BTreeSet<String> {
        self.changed.iter().map(|c| c.path.clone()).collect()
    }
}

/// What the one-time work at the parent cost.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Prepared {
    #[serde(with = "millis")]
    pub wall: Duration,
    /// Bytes of state the strategy carries into `answer`: an index file, a
    /// stored dump, whatever it is. A warm server holds its state in memory
    /// and reports 0.
    pub state_bytes: u64,
    /// Anything the strategy wants in the result file. Free-form on purpose.
    pub notes: BTreeMap<String, String>,
}

/// The incremental answer at head.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Answer {
    /// The changed targets, as labels.
    pub targets: BTreeSet<String>,
    #[serde(with = "millis")]
    pub wall: Duration,
    pub notes: BTreeMap<String, String>,
}

/// One approach to computing the changed targets.
pub trait Strategy {
    fn name(&self) -> &str;

    /// The one-time work at the step's parent, with the worktree there.
    fn prepare(&mut self, cx: &StepContext<'_>) -> Result<Prepared>;

    /// The incremental question at the step's head, with the worktree there.
    fn answer(&mut self, cx: &StepContext<'_>) -> Result<Answer>;
}

/// One step, measured.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StepMeasurement {
    pub subject: String,
    pub parent: String,
    pub head: String,
    pub tags: Vec<Tag>,
    pub prepare_wall_ms: u64,
    pub answer_wall_ms: u64,
    /// `answer.targets.len()`.
    pub answer_size: usize,
    /// How many rules the `rdeps` oracle reached.
    pub oracle_size: usize,
    /// Oracle targets the answer does not contain. Empty is a pass; anything
    /// here is unsoundness.
    pub missed: Vec<String>,
    /// Answer targets the oracle did not reach.
    pub extra: usize,
    pub state_bytes: u64,
    pub notes: BTreeMap<String, String>,
}

impl StepMeasurement {
    pub fn sound(&self) -> bool {
        self.missed.is_empty()
    }
}

/// One strategy over one subject.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunResult {
    pub strategy: String,
    pub subject: String,
    /// The Bazel the steps ran under, as `bazel --version` reports it.
    pub bazel_version: String,
    /// Unix seconds.
    pub started: u64,
    pub steps: Vec<StepMeasurement>,
}

impl RunResult {
    /// `<results>/skyframe/<strategy>-<subject>.json`.
    pub fn path(results_root: &Path, strategy: &str, subject: &str) -> PathBuf {
        results_root
            .join("skyframe")
            .join(format!("{strategy}-{subject}.json"))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn read(path: &Path) -> Result<Self> {
        let data = std::fs::read(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(serde_json::from_slice(&data)?)
    }
}

/// `Duration` as whole milliseconds, which is the only resolution any of
/// these numbers deserve.
mod millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_millis() as u64)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(d)?))
    }
}

/// Scores one answer against the oracle: what was missed, and how much wider
/// than the truth the answer was.
///
/// The oracle is a *lower bound* on a correct answer (it is seeded from the
/// changed source files), so `extra` is not by itself a fault; it is what the
/// comparison table reports so two strategies with the same recall are
/// distinguishable.
pub fn score(answer: &BTreeSet<String>, oracle: &BTreeSet<String>) -> (Vec<String>, usize) {
    let missed: Vec<String> = oracle.difference(answer).cloned().collect();
    let extra = answer.difference(oracle).count();
    (missed, extra)
}

/// The median of a sample, the lower of the two middles on an even count.
/// `None` for an empty sample.
pub fn median<T: Copy + Ord>(values: &[T]) -> Option<T> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort();
    Some(v[(v.len() - 1) / 2])
}

/// The markdown table `skyframe compare` prints: one row per result file.
pub fn compare(results: &[RunResult]) -> String {
    let mut out = String::from(
        "| strategy | subject | steps | prepare p50 ms | answer p50 ms | answer p50 | recall | extra p50 | state p50 bytes |\n\
         | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n",
    );
    for r in results {
        let n = r.steps.len();
        let prep: Vec<u64> = r.steps.iter().map(|s| s.prepare_wall_ms).collect();
        let ans: Vec<u64> = r.steps.iter().map(|s| s.answer_wall_ms).collect();
        let size: Vec<usize> = r.steps.iter().map(|s| s.answer_size).collect();
        let extra: Vec<usize> = r.steps.iter().map(|s| s.extra).collect();
        let state: Vec<u64> = r.steps.iter().map(|s| s.state_bytes).collect();
        let sound = r.steps.iter().filter(|s| s.sound()).count();
        let recall = if n == 0 {
            "n/a".to_string()
        } else {
            format!("{sound}/{n}")
        };
        fn cell<T: std::fmt::Display>(v: Option<T>) -> String {
            v.map(|x| x.to_string()).unwrap_or_else(|| "n/a".into())
        }
        out.push_str(&format!(
            "| {} | {} | {n} | {} | {} | {} | {recall} | {} | {} |\n",
            r.strategy,
            r.subject,
            cell(median(&prep)),
            cell(median(&ans)),
            cell(median(&size)),
            cell(median(&extra)),
            cell(median(&state)),
        ));
    }
    out
}

// ------------------------------------------------------------------ cambi

/// The control row: cambium as it ships.
///
/// `prepare` is `cambi index build` at the parent, published so the answer
/// can find it; `answer` is `cambi affected parent..head`. It measures
/// nothing new, and that is the point: it is the number every Skyframe
/// prototype has to beat, measured by the same harness on the same steps.
pub struct CambiStrategy<'a> {
    pub cambi: &'a crate::tools::cambi::Cambi,
    /// Where the index files go.
    pub index_dir: PathBuf,
}

impl CambiStrategy<'_> {
    fn index_path(&self, rev: &str) -> PathBuf {
        self.index_dir.join(format!("index-{rev}.bin"))
    }
}

impl Strategy for CambiStrategy<'_> {
    fn name(&self) -> &str {
        "cambi"
    }

    fn prepare(&mut self, cx: &StepContext<'_>) -> Result<Prepared> {
        std::fs::create_dir_all(&self.index_dir)?;
        let out = self.index_path(cx.parent());
        let wall = self
            .cambi
            .index_build(cx.worktree, cx.parent(), &out, true)
            .with_context(|| format!("cold index build at {}", cx.parent()))?;
        let state_bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        Ok(Prepared {
            wall,
            state_bytes,
            notes: BTreeMap::new(),
        })
    }

    fn answer(&mut self, cx: &StepContext<'_>) -> Result<Answer> {
        let out = self.index_path(cx.head());
        let run = self.cambi.affected(
            cx.worktree,
            &crate::tools::cambi::AffectedArgs::harness(cx.parent(), cx.head(), &out),
        )?;
        let mut notes = BTreeMap::new();
        notes.insert("engine_ms".into(), run.engine_ms().to_string());
        notes.insert("wrote_index".into(), run.index.is_some().to_string());
        Ok(Answer {
            targets: run.report.affected_labels().into_iter().collect(),
            wall: run.wall,
            notes,
        })
    }
}

/// Builds the strategy a name asks for. A prototype branch adds its arm here
/// and nothing else in this file changes.
pub fn strategies() -> &'static [&'static str] {
    &["cambi", analysis::NAME, analysis::WARM_NAME]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_score_is_what_was_missed_and_how_much_was_extra() {
        let answer = set(&["//a:a", "//b:b", "//c:c"]);
        let oracle = set(&["//a:a", "//d:d"]);
        let (missed, extra) = score(&answer, &oracle);
        assert_eq!(missed, vec!["//d:d".to_string()]);
        assert_eq!(extra, 2);

        // An answer that contains the oracle is sound, however wide.
        let (missed, extra) = score(&answer, &set(&["//a:a"]));
        assert!(missed.is_empty());
        assert_eq!(extra, 2);
    }

    #[test]
    fn medians_take_the_lower_middle() {
        assert_eq!(median::<u64>(&[]), None);
        assert_eq!(median(&[5u64]), Some(5));
        assert_eq!(median(&[3u64, 1, 2]), Some(2));
        assert_eq!(median(&[4u64, 1, 2, 3]), Some(2));
    }

    fn measurement(missed: &[&str], extra: usize) -> StepMeasurement {
        StepMeasurement {
            subject: "bazel".into(),
            parent: "p".into(),
            head: "h".into(),
            tags: vec![Tag::Adjacent],
            prepare_wall_ms: 100,
            answer_wall_ms: 10,
            answer_size: 3,
            oracle_size: 2,
            missed: missed.iter().map(|s| s.to_string()).collect(),
            extra,
            state_bytes: 4096,
            notes: BTreeMap::new(),
        }
    }

    fn result() -> RunResult {
        RunResult {
            strategy: "cambi".into(),
            subject: "bazel".into(),
            bazel_version: "9.2.0".into(),
            started: 0,
            steps: vec![measurement(&[], 1), measurement(&["//x:x"], 3)],
        }
    }

    #[test]
    fn a_run_result_round_trips_through_json() {
        let r = result();
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<RunResult>(&json).unwrap(), r);
    }

    #[test]
    fn the_comparison_table_is_one_row_per_result() {
        let table = compare(&[result()]);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3, "{table}");
        // Two steps, one of them sound.
        assert!(lines[2].contains("| cambi | bazel | 2 |"), "{table}");
        assert!(lines[2].contains("| 1/2 |"), "{table}");
        // The medians of (100,100), (10,10), (3,3), (1,3), (4096,4096).
        assert!(lines[2].contains("| 100 | 10 | 3 |"), "{table}");
        assert!(lines[2].ends_with("| 1 | 4096 |"), "{table}");
    }

    #[test]
    fn an_empty_run_says_so_rather_than_dividing_by_zero() {
        let mut r = result();
        r.steps.clear();
        let table = compare(&[r]);
        assert!(table.contains("| n/a |"), "{table}");
    }

    #[test]
    fn a_result_path_is_named_by_the_strategy_and_the_subject() {
        let p = RunResult::path(Path::new("/r"), "warm-query", "bazel");
        assert_eq!(p, Path::new("/r/skyframe/warm-query-bazel.json"));
    }

    #[test]
    fn a_rename_is_a_destination_and_a_deletion() {
        // The parser, over what `git diff --name-status` prints.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let git = |args: &[&str]| crate::tools::git(dir, args).unwrap();
        git(&["init", "--quiet", "-b", "main", "."]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a"), "one").unwrap();
        std::fs::write(dir.join("b"), "two").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "one"]);
        let parent = crate::tools::git(dir, &["rev-parse", "HEAD"]).unwrap();
        std::fs::rename(dir.join("a"), dir.join("c")).unwrap();
        std::fs::remove_file(dir.join("b")).unwrap();
        std::fs::write(dir.join("d"), "new").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "two"]);
        let head = crate::tools::git(dir, &["rev-parse", "HEAD"]).unwrap();

        let changed = changed_paths(dir, parent.trim(), head.trim()).unwrap();
        let by_path: BTreeMap<&str, &str> = changed
            .iter()
            .map(|c| (c.path.as_str(), c.status.as_str()))
            .collect();
        assert_eq!(by_path.get("b"), Some(&"D"));
        assert_eq!(by_path.get("d"), Some(&"A"));
        assert!(by_path.contains_key("c"), "{changed:?}");
        // However git reported the move, the source is gone at head.
        assert_eq!(by_path.get("a"), Some(&"D"));
        assert!(changed.iter().any(|c| c.is_delete()));
    }
}
