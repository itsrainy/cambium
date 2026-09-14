//! What `cambium-bench watch` shows: the matrix, read off the files.
//!
//! Two pure halves, because the point of issue #1 is a view that can be
//! checked without a terminal:
//!
//! - [`snapshot`] reads `bench-results/` — the progress file, the state file
//!   and whichever result files are there — and answers with a plain data
//!   structure. It opens nothing else: no IPC, no log scraping, no handle on
//!   the running process, which is what lets it work on a finished run and on
//!   results copied off another machine.
//! - [`render`] turns that structure into lines of text, and
//!   [`render_change_sets`] into the per-change-set table a row expands into.
//!   The TUI draws exactly these lines, `--once` prints them, and the tests
//!   compare them against fixture result files.
//!
//! Nothing here writes. The harness ([`crate::progress`]) is the only writer.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::walk::{StepRecord, WalkResult};
use crate::progress::Progress;
use crate::state::State;
use crate::util::{duration, parse_rfc3339, percentile, rfc3339_now};

/// The snapshot document's version, carried by `--once --format json`.
pub const WATCH_SCHEMA: u32 = 1;

/// Every subject under one results directory, as of one poll.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema: u32,
    pub results_dir: PathBuf,
    /// When this snapshot was taken, RFC 3339.
    pub at: String,
    pub subjects: Vec<SubjectSnapshot>,
}

/// One row of the matrix.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SubjectSnapshot {
    pub subject: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pin: String,
    /// `history`, `changes`, `subjects`, or empty when only results are there.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workload: String,
    /// The step's name: `fetch`, `build`, `history`, `changes`, `done`,
    /// `failed`, `idle` for a subject with results and no progress file, or
    /// `stale` for a finished or idle subject another engine build wrote.
    pub step: String,
    pub index: usize,
    pub of: usize,
    /// The commit or `base..head` being worked on.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub current: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_elapsed_secs: Option<u64>,
    /// Since the run started on this subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_secs: Option<u64>,
    /// Remaining items times the p50 of the ones that are done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<u64>,
    /// The p50 the ETA is taken from: a commit's whole-process wall time for
    /// the history, a change set's measured total for the comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_ms: Option<u64>,
    /// Globally invalidated commits or change sets so far, over how many are
    /// recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub globals: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded: Option<usize>,
    /// The worst recall any change set has reached; `None` until one has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_recall: Option<f64>,
    /// Change sets that passed the three-way comparison, over how many ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compared: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The diagnostics the last `cambi` document carried, one rendered line
    /// each: `CAMBI-W004 .bazelrc changed and the run went global`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
    /// The rows `enter` expands into. Carried in the JSON too: a snapshot is
    /// what another tool reads instead of parsing the result files itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub change_sets: Vec<ChangeSetRow>,
}

/// One change set of one subject, for the expanded view.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeSetRow {
    pub kind: String,
    pub change_set: String,
    pub changed_files: usize,
    pub dirty_packages: usize,
    /// Wall milliseconds per tool, cambium included.
    pub tool_ms: Vec<(String, u64)>,
    /// The disagreement classes this change set produced, largest first.
    pub classes: Vec<(String, usize)>,
    pub global: bool,
    pub pass: bool,
}

/// Reads the results directory. `subjects` empty means every subject
/// directory there, in name order.
///
/// A subject directory with nothing readable in it is still a row: "no
/// results yet" is the answer someone starting a run wants to see, not an
/// empty screen.
pub fn snapshot(results_dir: &Path, subjects: &[String]) -> Snapshot {
    snapshot_with(results_dir, subjects, &BTreeMap::new())
}

/// [`snapshot`], knowing the engine id this build records for each subject
/// (`engines`, by subject id). A finished, failed or idle row whose state file
/// another engine build wrote is `stale`: its numbers are real, but they are a
/// different experiment's, and a watcher must not read them as this run's.
pub fn snapshot_with(
    results_dir: &Path,
    subjects: &[String],
    engines: &BTreeMap<String, String>,
) -> Snapshot {
    let names = match subjects.is_empty() {
        false => subjects.to_vec(),
        true => subject_dirs(results_dir),
    };
    Snapshot {
        schema: WATCH_SCHEMA,
        results_dir: results_dir.to_path_buf(),
        at: rfc3339_now(),
        subjects: names
            .iter()
            .map(|n| read_subject(results_dir, n, engines.get(n)))
            .collect(),
    }
}

/// The subject directories under `results_dir`, in name order. A directory
/// with neither a state file, a progress file nor a workload directory is not
/// one (`scratch/`, `bases/`, a stray tarball).
fn subject_dirs(results_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(results_dir) else {
        return out;
    };
    for e in rd.flatten() {
        if !e.path().is_dir() {
            continue;
        }
        let looks_like_one = Progress::path(&e.path()).is_file()
            || State::path(&e.path()).is_file()
            || e.path().join("walk").is_dir();
        if looks_like_one {
            if let Some(name) = e.file_name().to_str() {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    out
}

fn read_subject(results_dir: &Path, subject: &str, engine: Option<&String>) -> SubjectSnapshot {
    let dir = crate::subject_dir(results_dir, subject);
    let progress = Progress::read(&dir);
    let state = State::peek(&dir);
    let pin = progress
        .as_ref()
        .map(|p| p.pin.clone())
        .or_else(|| state.as_ref().map(|s| s.pin.clone()))
        .unwrap_or_default();

    let mut row = SubjectSnapshot {
        subject: subject.to_string(),
        pin,
        step: "idle".into(),
        ..Default::default()
    };

    if let Some(p) = &progress {
        row.workload = p.workload.clone();
        row.step = p.step.name().to_string();
        row.index = p.index;
        row.of = p.of;
        row.current = p.current.clone();
        row.last_error = p.last_error.clone();
        row.diagnostics = p.diagnostics.iter().map(render_diagnostic).collect();
        row.tool = p.tool.as_ref().map(|t| t.name.clone());
        row.tool_elapsed_secs = p.tool.as_ref().and_then(|t| since(&t.started_at));
        // A finished run's elapsed is what it took, not how long ago it was:
        // `step_started_at` is the moment it reached `done`.
        row.elapsed_secs = match p.step.finished() {
            true => elapsed_between(&p.started_at, &p.step_started_at),
            false => since(&p.started_at),
        };
    } else if let Some(s) = &state {
        // No progress file: an older run, or one whose subject was only ever
        // fetched. The state file's own stamps are what is left.
        row.step = match s.history.as_ref().map(|h| h.done) {
            Some(true) => "done".into(),
            _ => "idle".into(),
        };
        row.elapsed_secs = s
            .steps
            .get("walk.started")
            .or_else(|| s.steps.get("history.started"))
            .and_then(|t| since(t));
    }

    if let (Some(now), Some(s)) = (engine, &state) {
        if !s.engine.is_empty()
            && s.engine != *now
            && matches!(row.step.as_str(), "done" | "failed" | "idle")
        {
            row.step = "stale".into();
        }
    }

    // The walk's own numbers, from `walk/walk-<pin>.json`.
    if let Some(w) = read_walk(&dir, &row.pin) {
        if row.workload.is_empty() {
            row.workload = "walk".into();
        }
        let sum = &w.summary;
        row.globals = Some(sum.globals);
        row.recorded = Some(sum.steps);
        row.passed = Some(
            w.steps.iter().filter(|st| st.pass).count(),
        );
        row.compared = Some(sum.steps_with_oracle);
        row.min_recall = (sum.steps_with_oracle > 0).then_some(sum.soundness.worst_recall);
        row.p50_ms = (sum.steps > 0).then_some(sum.cost.cambium_walk.p50_ms);
        if row.of == 0 {
            row.of = w.steps.len();
            row.index = w.steps.len();
        }
        // The expanded view is the per-step table someone opens a row to read.
        row.change_sets = w.steps.iter().map(step_row).collect();
    }
    row.eta_secs = eta(&row);
    row
}

/// Remaining items times the p50 of the ones that are done.
///
/// `None` once the run is over, and `None` while nothing has finished: an ETA
/// extrapolated from no samples is a guess dressed as a measurement.
fn eta(row: &SubjectSnapshot) -> Option<u64> {
    if matches!(row.step.as_str(), "done" | "failed" | "idle" | "stale") {
        return None;
    }
    let p50 = row.p50_ms.filter(|ms| *ms > 0)?;
    let remaining = row.of.checked_sub(row.index)?;
    Some(remaining as u64 * p50 / 1000)
}

/// The p50 of what a compared change set cost: every tool's walls plus
/// cambium's two, which is the work one change set of the loop does.
fn step_p50(steps: &[StepRecord]) -> Option<u64> {
    if steps.is_empty() {
        return None;
    }
    let mut totals: Vec<u64> = steps.iter().map(|s| s.wall_ms).collect();
    totals.sort_unstable();
    Some(percentile(&totals, 0.5))
}

fn step_row(st: &StepRecord) -> ChangeSetRow {
    let tool_ms: Vec<(String, u64)> =
        st.tools.iter().map(|t| (t.name.clone(), t.wall_ms)).collect();
    // The per-step "classes" line now carries the recall of each tool that the
    // oracle scored, low first, since a recall below 1.0 is the thing to see.
    let mut recalls: Vec<(String, usize)> = st
        .tools
        .iter()
        .filter_map(|t| t.recall_vs_oracle.map(|r| (t.name.clone(), (r * 1000.0) as usize)))
        .collect();
    recalls.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    let tags = st
        .tags
        .iter()
        .map(|t| t.name())
        .collect::<Vec<_>>()
        .join(",");
    ChangeSetRow {
        kind: tags,
        change_set: format!("{}..{}", short(&st.parent), short(&st.head)),
        changed_files: st.changed_files,
        dirty_packages: st.dirty_packages,
        tool_ms,
        classes: recalls,
        global: st.global,
        pass: st.pass,
    }
}

fn render_diagnostic(d: &cambium_engine::diag::Diagnostic) -> String {
    match &d.help {
        Some(h) => format!("{} {} (help: {h})", d.code, d.message),
        None => format!("{} {}", d.code, d.message),
    }
}

/// The newest readable `walk-*.json` under `<subject>/walk/`, preferring the
/// one for `pin`.
fn read_walk(subject_dir: &Path, pin: &str) -> Option<WalkResult> {
    let dir = subject_dir.join("walk");
    let path = newest(&dir, "walk-", pin)?;
    match WalkResult::read(&path) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = format!("{e:#}"), "unreadable walk result");
            None
        }
    }
}

/// The file in `dir` named `<prefix><pin>...json` if there is one, else the
/// most recently modified `<prefix>*.json`.
///
/// A subject carries one result file per Bazel version and keeps the files of
/// earlier pins, and a watcher that picked the wrong one would show a finished
/// run while another is walking.
fn newest(dir: &Path, prefix: &str, pin: &str) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let path = e.path();
        let name = path.file_name()?.to_str()?.to_string();
        if !name.starts_with(prefix) || !name.ends_with(".json") {
            continue;
        }
        if !pin.is_empty() && name.starts_with(&format!("{prefix}{pin}")) {
            return Some(path);
        }
        let t = e
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().map_or(true, |(bt, _)| t > *bt) {
            best = Some((t, path));
        }
    }
    best.map(|(_, p)| p)
}

fn since(stamp: &str) -> Option<u64> {
    let then = parse_rfc3339(stamp)?;
    Some(crate::util::now_unix().saturating_sub(then))
}

fn elapsed_between(from: &str, to: &str) -> Option<u64> {
    Some(parse_rfc3339(to)?.saturating_sub(parse_rfc3339(from)?))
}

fn short(rev: &str) -> &str {
    &rev[..rev.len().min(7)]
}

// --------------------------------------------------------------- the drawing

/// The one screen, as lines of text.
///
/// The TUI draws these and `--once` prints them, so what a test compares is
/// what both show. Plain text with no escape codes: colour is the TUI's
/// business and `--once` has none by the output rules in docs/CLI.md.
pub fn render(s: &Snapshot) -> Vec<String> {
    let mut out = Vec::new();
    let w = s
        .subjects
        .iter()
        .map(|r| r.subject.len())
        .chain(std::iter::once("subject".len()))
        .max()
        .unwrap_or(7);
    out.push(format!(
        "cambium-bench watch   {}   {}",
        s.results_dir.display(),
        s.at
    ));
    out.push(String::new());
    out.push(format!(
        "{:<w$}  {:<8}  {:<14}  {:<24}  {:>8}  {:>8}  {:>9}  {:>7}  {:>9}",
        "subject",
        "step",
        "progress",
        "current",
        "elapsed",
        "eta",
        "globals",
        "recall",
        "pass",
        w = w
    ));
    for r in &s.subjects {
        out.push(format!(
            "{:<w$}  {:<8}  {:<14}  {:<24}  {:>8}  {:>8}  {:>9}  {:>7}  {:>9}",
            r.subject,
            r.step,
            progress_cell(r),
            current_cell(r),
            r.elapsed_secs.map(duration).unwrap_or_else(dash),
            r.eta_secs.map(duration).unwrap_or_else(dash),
            globals_cell(r),
            recall_cell(r),
            pass_cell(r),
            w = w
        ));
    }
    let errors: Vec<&SubjectSnapshot> = s
        .subjects
        .iter()
        .filter(|r| r.last_error.is_some())
        .collect();
    if !errors.is_empty() {
        out.push(String::new());
        out.push("errors".to_string());
        for r in errors {
            out.push(format!(
                "  {:<w$}  {}",
                r.subject,
                r.last_error.as_deref().unwrap_or_default(),
                w = w
            ));
        }
    }
    let noisy: Vec<&SubjectSnapshot> = s
        .subjects
        .iter()
        .filter(|r| !r.diagnostics.is_empty())
        .collect();
    if !noisy.is_empty() {
        out.push(String::new());
        out.push("diagnostics".to_string());
        for r in noisy {
            for d in &r.diagnostics {
                out.push(format!("  {:<w$}  {d}", r.subject, w = w));
            }
        }
    }
    out
}

/// The per-change-set table one row expands into.
pub fn render_change_sets(r: &SubjectSnapshot) -> Vec<String> {
    let mut out = vec![format!(
        "{} — {} change sets",
        r.subject,
        r.change_sets.len()
    )];
    out.push(String::new());
    out.push(format!(
        "{:<9}  {:<20}  {:>5}  {:>5}  {:<30}  {:<5}  {}",
        "kind", "change set", "files", "dirty", "tools (ms)", "pass", "classes"
    ));
    for c in &r.change_sets {
        let tools = c
            .tool_ms
            .iter()
            .map(|(n, ms)| format!("{n} {ms}"))
            .collect::<Vec<_>>()
            .join(" ");
        let classes = match c.classes.is_empty() {
            true => "none".to_string(),
            false => c
                .classes
                .iter()
                .map(|(n, k)| format!("{n}={k}"))
                .collect::<Vec<_>>()
                .join(" "),
        };
        out.push(format!(
            "{:<9}  {:<20}  {:>5}  {:>5}  {:<30}  {:<5}  {}",
            match c.global {
                true => "global",
                false => c.kind.as_str(),
            },
            c.change_set,
            c.changed_files,
            c.dirty_packages,
            tools,
            match c.pass {
                true => "yes",
                false => "NO",
            },
            classes
        ));
    }
    if r.change_sets.is_empty() {
        out.push("  (no change sets compared yet)".to_string());
    }
    out
}

fn dash() -> String {
    "-".to_string()
}

fn progress_cell(r: &SubjectSnapshot) -> String {
    match r.of {
        0 => dash(),
        of => format!(
            "{} {}/{of}",
            match r.workload.is_empty() {
                true => "items",
                false => r.workload.as_str(),
            },
            r.index
        ),
    }
}

fn current_cell(r: &SubjectSnapshot) -> String {
    let current = match r.current.is_empty() {
        true => dash(),
        false => short(&r.current).to_string(),
    };
    match (&r.tool, r.tool_elapsed_secs) {
        (Some(t), Some(secs)) => format!("{current} {t} {}", duration(secs)),
        (Some(t), None) => format!("{current} {t}"),
        _ => current,
    }
}

fn globals_cell(r: &SubjectSnapshot) -> String {
    match (r.globals, r.recorded) {
        (Some(g), Some(n)) => format!("{g}/{n}"),
        (Some(g), None) => g.to_string(),
        _ => dash(),
    }
}

fn recall_cell(r: &SubjectSnapshot) -> String {
    match r.min_recall {
        Some(v) => format!("{:.1}%", v * 100.0),
        None => dash(),
    }
}

fn pass_cell(r: &SubjectSnapshot) -> String {
    match (r.passed, r.compared) {
        (Some(p), Some(n)) => format!("{p}/{n}"),
        _ => dash(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::progress::{Reporter, Step, ToolProgress};
    use crate::walk::model::*;
    use crate::walk::{sampling::Sample, steps::Step as WalkStep, Tag, Tier};

    fn progress(
        subject: &str,
        pin: &str,
        workload: &str,
        step: Step,
        index: usize,
        of: usize,
    ) -> Progress {
        Progress {
            schema: crate::progress::PROGRESS_SCHEMA,
            workload: workload.into(),
            subject: subject.into(),
            pin: pin.into(),
            step,
            index,
            of,
            started_at: crate::util::rfc3339_now(),
            step_started_at: crate::util::rfc3339_now(),
            current: String::new(),
            tool: None,
            last_error: None,
            cambi_version: Some("cambi 0.1.0 (abc1234)".into()),
            diagnostics: Vec::new(),
        }
    }

    /// A walk result with `steps` steps, `globals` of them global, and the
    /// last `steps - passed` failing (a missing recall).
    fn walk_file(subject_dir: &Path, pin: &str, steps: usize, globals: usize, passed: usize) {
        let mut recs = Vec::new();
        for i in 0..steps {
            let global = i < globals;
            let pass = i < passed;
            let oracle = crate::oracles::rdeps::RdepsResult {
                oracle_labels: 10,
                matched: if pass { 10 } else { 8 },
                missing: if pass { vec![] } else { vec!["//x:y".into()] },
                oracle_set: ["//a:b".to_string()].into_iter().collect(),
                ..Default::default()
            };
            recs.push(StepRecord {
                parent: format!("base{i:036}"),
                head: format!("head{i:036}"),
                tags: vec![Tag::Adjacent],
                changed_files: 7,
                dirty_packages: 3,
                queried_targets: 9,
                timings: StepTimings::default(),
                wall_ms: 2_000,
                carried_index: true,
                rebuild_step: false,
                global,
                global_reason: None,
                index: IndexCounts { nodes: 10, edges: 20, lineage: String::new() },
                cambium: CambiumAnswer { direct: 1, affected: 40, tests: 1, cold_rebuild: false },
                oracle: (!global).then(|| oracle),
                tools: vec![
                    ToolStep {
                        name: "cambium".into(),
                        version: "cambi".into(),
                        labels: 40,
                        wall_ms: 2_000,
                        recall_vs_oracle: (!global).then_some(if pass { 1.0 } else { 0.8 }),
                        tightness_vs_oracle: (!global).then_some(0.25),
                        failed: false,
                    },
                    ToolStep {
                        name: "bazel-diff".into(),
                        version: "v46".into(),
                        labels: 38,
                        wall_ms: 11_200,
                        recall_vs_oracle: (!global).then_some(1.0),
                        tightness_vs_oracle: (!global).then_some(0.5),
                        failed: false,
                    },
                ],
                pass,
            });
        }
        let steps_vec: Vec<WalkStep> = recs
            .iter()
            .map(|r| WalkStep { parent: r.parent.clone(), head: r.head.clone(), tags: r.tags.clone() })
            .collect();
        let sample = Sample::all(&steps_vec);
        let mut r = WalkResult {
            schema: crate::RESULT_SCHEMA,
            repo: "r".into(),
            pin: pin.into(),
            from: format!("{:040}", 0),
            to: pin.into(),
            tier: Tier::Full,
            schedule: Tier::Full.defaults(),
            oracle_sample: sample.clone(),
            peer_sample: sample,
            environment: crate::env::Environment {
                machine: crate::harness::Machine::detect(),
                bazel_version_used: "bazel 7.4.1".into(),
                cambi_version: "cambi".into(),
                tool_sha: "abc".into(),
                engine_id: String::new(),
                config_hash: String::new(),
                output_bases: Default::default(),
            },
            started_unix: 0,
            index_build_ms: 60_000,
            steps: recs,
            rebuild_oracle: None,
            head_index: None,
            summary: WalkSummary::default(),
            done: false,
        };
        r.summarise();
        r.write(&WalkResult::path(&subject_dir.join("walk"), pin, None)).unwrap();
    }

    /// A results tree as a run leaves one.
    fn fixture(root: &Path) {
        // A finished subject: every step walked and passing.
        let bazel = root.join("bazel");
        walk_file(&bazel, "pin1", 50, 1, 50);
        let mut p = progress("bazel", "pin1", "walk", Step::Done, 50, 50);
        p.started_at = crate::util::rfc3339(1_000_000);
        p.step_started_at = crate::util::rfc3339(1_001_400);
        p.write(&bazel).unwrap();

        // A subject mid-walk, with a tool running and something to say.
        let envoy = root.join("envoy");
        walk_file(&envoy, "pin2", 12, 4, 12);
        let mut p = progress("envoy", "pin2", "walk", Step::Walk, 12, 50);
        p.current = "c3f9a1b2c3d4e5f6".into();
        p.tool = Some(ToolProgress {
            name: "cambium".into(),
            started_at: crate::util::rfc3339_now(),
        });
        p.last_error = Some("bazel query exit 3 (partial universe, accepted)".into());
        p.diagnostics = vec![cambium_engine::diag::Diagnostic::warning(
            cambium_engine::diag::Code::W004BazelrcGlobal,
            ".bazelrc changed and the run went global",
        )];
        p.write(&envoy).unwrap();

        // A subject that has been fetched and nothing more.
        let apollo = root.join("apollo");
        std::fs::create_dir_all(&apollo).unwrap();
        let mut st = State::fresh("apollo", "pin3");
        st.fetched = true;
        st.save(&apollo).unwrap();

        std::fs::create_dir_all(root.join("scratch")).unwrap();
    }

    #[test]
    fn the_table_is_one_row_per_subject_with_the_footers_under_it() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let s = snapshot(dir.path(), &[]);
        assert_eq!(
            s.subjects.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>(),
            ["apollo", "bazel", "envoy"],
            "scratch/ is not a subject"
        );
        let text = render(&s).join("
");
        assert!(text.contains("bazel    done      walk 50/50"), "{text}");
        assert!(text.contains("23m20s"), "the finished run's own duration\n{text}");
        assert!(text.contains("100.0%"), "worst recall\n{text}");
        assert!(text.contains("envoy    walk      walk 12/50"), "{text}");
        assert!(text.contains("c3f9a1b cambium"), "{text}");
        let envoy = s.subjects.iter().find(|r| r.subject == "envoy").unwrap();
        assert_eq!(envoy.p50_ms, Some(2_000));
        assert_eq!(envoy.eta_secs, Some(76));
        assert_eq!(envoy.globals, Some(4));
        assert!(text.contains("apollo   idle"), "{text}");
        assert!(text.contains("errors\n  envoy    bazel query exit 3"), "{text}");
        assert!(
            text.contains("CAMBI-W004 .bazelrc changed and the run went global"),
            "{text}"
        );
    }

    #[test]
    fn a_named_subject_is_the_only_row_and_a_missing_one_is_still_one() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let s = snapshot(dir.path(), &["envoy".into(), "xla".into()]);
        assert_eq!(s.subjects.len(), 2);
        assert_eq!(s.subjects[1].subject, "xla");
        assert_eq!(s.subjects[1].step, "idle");
        assert!(s.subjects[1].eta_secs.is_none());
    }

    #[test]
    fn the_snapshot_is_a_document_another_tool_can_read() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let s = snapshot(dir.path(), &[]);
        let json = serde_json::to_string(&s).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema, WATCH_SCHEMA);
        assert_eq!(back.subjects.len(), 3);
        assert!(crate::util::parse_rfc3339(&back.at).is_some());
    }

    #[test]
    fn a_row_expands_into_its_steps() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let s = snapshot(dir.path(), &["bazel".into()]);
        let rows = &s.subjects[0].change_sets;
        assert_eq!(rows.len(), 50);
        let text = render_change_sets(&s.subjects[0]).join("\n");
        assert!(text.contains("bazel — 50"), "{text}");
        assert!(text.contains("base000..head000"), "{text}");
        assert!(text.contains("cambium 2000 bazel-diff 11200"), "{text}");

        let s = snapshot(dir.path(), &["apollo".into()]);
        let text = render_change_sets(&s.subjects[0]).join("\n");
        assert!(text.contains("(no change sets compared yet)"), "{text}");
    }

    #[test]
    fn a_copied_tree_renders_and_a_failure_is_visible() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let copy = dir.path().join("copy");
        std::fs::create_dir_all(&copy).unwrap();
        let from = dir.path().join("envoy");
        let to = copy.join("envoy");
        copy_tree(&from, &to);
        let r = Reporter::new(&to, "walk", "envoy", "pin2", Step::Walk, None);
        r.item(7, "base..head");
        r.failed("tango: worktree is locked");

        let s = snapshot(&copy, &[]);
        assert_eq!(s.subjects.len(), 1);
        let row = &s.subjects[0];
        assert_eq!(row.step, "failed");
        assert!(row.eta_secs.is_none());
        let text = render(&s).join("\n");
        assert!(text.contains("tango: worktree is locked"), "{text}");
    }

    #[test]
    fn a_row_another_build_wrote_is_stale_not_done() {
        let dir = tempfile::tempdir().unwrap();
        let apollo = dir.path().join("apollo");
        walk_file(&apollo, "pin3", 5, 0, 5);
        let mut st = State::fresh("apollo", "pin3");
        st.engine = "f8/old".into();
        st.history = Some(crate::state::HistoryState {
            from: "a".into(),
            to: "pin3".into(),
            bazel_version: String::new(),
            last_completed: None,
            done: true,
        });
        st.save(&apollo).unwrap();
        let names = vec!["apollo".to_string()];

        assert_eq!(snapshot(dir.path(), &names).subjects[0].step, "done");
        let other = std::collections::BTreeMap::from([("apollo".to_string(), "f8/new".to_string())]);
        let row = &snapshot_with(dir.path(), &names, &other).subjects[0];
        assert_eq!(row.step, "stale");
        assert!(row.eta_secs.is_none());
        let same = std::collections::BTreeMap::from([("apollo".to_string(), "f8/old".to_string())]);
        assert_eq!(snapshot_with(dir.path(), &names, &same).subjects[0].step, "done");
    }

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap().flatten() {
            let dest = to.join(e.file_name());
            if e.path().is_dir() {
                copy_tree(&e.path(), &dest);
            } else {
                std::fs::copy(e.path(), dest).unwrap();
            }
        }
    }

    #[test]
    fn a_result_file_from_another_schema_costs_its_numbers_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let subject = dir.path().join("bazel");
        std::fs::create_dir_all(subject.join("walk")).unwrap();
        std::fs::write(
            subject.join("walk/walk-pin1.json"),
            r#"{"schema": 1, "repo": "bazel"}"#,
        )
        .unwrap();
        progress("bazel", "pin1", "walk", Step::Walk, 3, 10)
            .write(&subject)
            .unwrap();
        let s = snapshot(dir.path(), &[]);
        assert_eq!(s.subjects.len(), 1);
        assert_eq!(s.subjects[0].step, "walk");
        assert!(s.subjects[0].min_recall.is_none());
        assert!(render(&s).iter().any(|l| l.contains("walk 3/10")));
    }
}
