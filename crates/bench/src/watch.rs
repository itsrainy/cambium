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

use crate::compare::{ChangeSetResult, ChangesResult};
use crate::history::HistoryResult;
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
            || e.path().join("history").is_dir()
            || e.path().join("changes").is_dir();
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
        row.step = match (s.history.as_ref().map(|h| h.done), s.compared.is_empty()) {
            (Some(true), false) => "done".into(),
            _ => "idle".into(),
        };
        row.elapsed_secs = s
            .steps
            .get("changes.started")
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

    // The workload's own numbers, from whichever result file the subject's
    // current workload writes. An idle subject shows its comparison when it
    // has one — that is where recall and the pass count live — and its history
    // otherwise, so a finished subject's row is not blank.
    let history = read_history(&dir, &row.pin);
    let changes = read_changes(&dir, &row.pin);
    let show_changes = match row.step.as_str() {
        "history" => false,
        "changes" => true,
        _ => changes.is_some(),
    };
    if let Some(h) = history.filter(|_| !show_changes) {
        if row.workload.is_empty() {
            row.workload = "history".into();
        }
        row.globals = Some(h.summary.globals);
        row.recorded = Some(h.summary.commits);
        row.p50_ms = (h.summary.commits > 0).then_some(h.summary.p50_total_ms);
        if row.of == 0 {
            row.of = h.commits.len();
            row.index = h.commits.len();
        }
    }
    if let Some(c) = &changes {
        if show_changes {
            if row.workload.is_empty() {
                row.workload = "changes".into();
            }
            row.globals = Some(c.summary.globals);
            row.recorded = Some(c.summary.change_sets);
            row.passed = Some(c.summary.passed);
            row.compared = Some(c.summary.change_sets);
            row.min_recall = (c.summary.change_sets > 0).then_some(c.summary.min_recall);
            row.p50_ms = change_set_p50(&c.change_sets);
            if row.of == 0 {
                row.of = c.change_sets.len();
                row.index = c.change_sets.len();
            }
        }
        // The expanded view is the comparison's, whichever workload is
        // running: it is the table someone opens a row to read.
        row.change_sets = c.change_sets.iter().map(change_set_row).collect();
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
fn change_set_p50(change_sets: &[ChangeSetResult]) -> Option<u64> {
    if change_sets.is_empty() {
        return None;
    }
    let mut totals: Vec<u64> = change_sets.iter().map(change_set_ms).collect();
    totals.sort_unstable();
    Some(percentile(&totals, 0.5))
}

fn change_set_ms(c: &ChangeSetResult) -> u64 {
    let tools: u64 = c
        .tools
        .iter()
        .map(|t| t.cold_wall_ms + t.warm_wall_ms.unwrap_or(0))
        .sum();
    c.cambium.cold_wall_ms + c.cambium.warm_wall_ms + tools
}

fn change_set_row(c: &ChangeSetResult) -> ChangeSetRow {
    let mut tool_ms: Vec<(String, u64)> = vec![(
        crate::tools::cambi::NAME.to_string(),
        c.cambium.warm_wall_ms,
    )];
    tool_ms.extend(c.tools.iter().map(|t| (t.name.clone(), t.cold_wall_ms)));
    let mut classes: Vec<(String, usize)> = c
        .tools
        .iter()
        .flat_map(|t| t.counts.iter())
        .filter(|(_, n)| **n > 0)
        .fold(
            std::collections::BTreeMap::<String, usize>::new(),
            |mut acc, (class, n)| {
                *acc.entry(class.name().to_string()).or_default() += n;
                acc
            },
        )
        .into_iter()
        .collect();
    classes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ChangeSetRow {
        kind: c.kind.name().to_string(),
        change_set: format!("{}..{}", short(&c.base), short(&c.head)),
        changed_files: c.cambium.changed_files,
        dirty_packages: c.cambium.dirty_packages,
        tool_ms,
        classes,
        global: c.global,
        pass: c.pass,
    }
}

fn render_diagnostic(d: &cambium_engine::diag::Diagnostic) -> String {
    match &d.help {
        Some(h) => format!("{} {} (help: {h})", d.code, d.message),
        None => format!("{} {}", d.code, d.message),
    }
}

/// The newest readable `history-*.json` under `<subject>/history/`, preferring
/// the one for `pin`.
fn read_history(subject_dir: &Path, pin: &str) -> Option<HistoryResult> {
    let dir = subject_dir.join("history");
    let path = newest(&dir, "history-", pin)?;
    match HistoryResult::read(&path) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = format!("{e:#}"), "unreadable history result");
            None
        }
    }
}

fn read_changes(subject_dir: &Path, pin: &str) -> Option<ChangesResult> {
    let dir = subject_dir.join("changes");
    let path = newest(&dir, "changes-", pin)?;
    match ChangesResult::read(&path) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = format!("{e:#}"), "unreadable changes result");
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
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::*;
    use crate::changes::ChangeSetKind;
    use crate::classify::Class;
    use crate::compare::{CambiumRun, ChangesOptions, ToolComparison};
    use crate::history::{ColdBuild, CommitRecord, Summary};
    use crate::progress::{Reporter, Step, ToolProgress};

    /// A results tree as a run leaves one. Built from the real result types
    /// rather than from checked-in JSON, so a field that moves cannot leave a
    /// stale fixture passing.
    fn fixture(root: &Path) {
        // A finished subject: a history walked and every change set compared.
        let bazel = root.join("bazel");
        history_file(&bazel, "pin1", 50, 1);
        changes_file(&bazel, "pin1", 50, 50, 1.0);
        let mut p = progress("bazel", "pin1", "changes", Step::Done, 50, 50);
        p.current = String::new();
        p.started_at = crate::util::rfc3339(1_000_000);
        p.step_started_at = crate::util::rfc3339(1_001_400);
        p.write(&bazel).unwrap();

        // A subject mid-history, with a tool running and something to say.
        let envoy = root.join("envoy");
        history_file(&envoy, "pin2", 12, 4);
        let mut p = progress("envoy", "pin2", "history", Step::History, 12, 50);
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

        // And a directory that is not a subject at all.
        std::fs::create_dir_all(root.join("scratch")).unwrap();
    }

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

    fn cold(commit: &str) -> ColdBuild {
        ColdBuild {
            commit: commit.into(),
            wall_ms: 60_000,
            index_bytes: 1024,
            nodes: 10,
            edges: 20,
            packages: 3,
            rules: 7,
            tests: 1,
            lineage: "abcdef".into(),
            partial: Default::default(),
            stats: serde_json::json!({}),
        }
    }

    fn history_file(subject_dir: &Path, pin: &str, commits: usize, globals: usize) {
        let records: Vec<CommitRecord> = (0..commits)
            .map(|i| CommitRecord {
                commit: format!("{i:040}"),
                parent: format!("{:040}", i.saturating_sub(1)),
                changed_files: 3,
                ignored_files: 0,
                dirty_packages: 2,
                queried_targets: 9,
                dirty_ms: 1,
                query_ms: 2,
                splice_ms: 3,
                direct_ms: 4,
                walk_ms: 5,
                total_ms: 2_000,
                engine_total_ms: 1_800,
                direct: 1,
                affected: 4,
                tests: 1,
                rbuildfiles_called: false,
                bzl_closure_hits: 0,
                global: i < globals,
                global_reason: None,
                cold_rebuild: false,
                engine_cold_rebuild: false,
                index_nodes: 10,
                index_edges: 20,
            })
            .collect();
        let mut r = crate::history::HistoryResult {
            schema: crate::RESULT_SCHEMA,
            repo: "r".into(),
            pin: pin.into(),
            from: format!("{:040}", 0),
            to: pin.into(),
            bazel_version: String::new(),
            bazel_version_used: "bazel 7.4.1".into(),
            tool_sha: "abc".into(),
            cambi_version: None,
            engine: String::new(),
            started_unix: 0,
            cold: cold(pin),
            commits: records,
            rebuild: None,
            oracle: None,
            head_index: None,
            summary: Summary::default(),
            done: false,
        };
        r.summarise();
        r.write(&crate::history::HistoryResult::path(
            &subject_dir.join("history"),
            pin,
        ))
        .unwrap();
    }

    fn changes_file(subject_dir: &Path, pin: &str, compared: usize, passed: usize, recall: f64) {
        let change_sets: Vec<ChangeSetResult> = (0..compared)
            .map(|i| {
                let pass = i < passed;
                let mut counts = BTreeMap::new();
                counts.insert(Class::CambiumExtraSelect, 2);
                if !pass {
                    counts.insert(Class::CambiumMissing, 1);
                }
                ChangeSetResult {
                    schema: crate::RESULT_SCHEMA,
                    base: format!("base{i:036}"),
                    head: format!("head{i:036}"),
                    kind: ChangeSetKind::Pr,
                    category: None,
                    cambium: CambiumRun {
                        changed_files: 7,
                        dirty_packages: 3,
                        affected: 40,
                        cold_wall_ms: 900,
                        warm_wall_ms: 400,
                        ..Default::default()
                    },
                    tools: vec![ToolComparison {
                        labels: 38,
                        cold_wall_ms: 11_200,
                        counts,
                        pass,
                        ..ToolComparison::new("bazel-diff-rust", "v46.1.0")
                    }],
                    oracle: None,
                    load_oracle: None,
                    recall: if pass { 1.0 } else { recall },
                    global: false,
                    pass,
                }
            })
            .collect();
        let mut r = ChangesResult {
            schema: crate::RESULT_SCHEMA,
            repo: "r".into(),
            pin: pin.into(),
            bazel_version: None,
            bazel_version_used: None,
            seed: 0,
            oracle_k: 0,
            engine: String::new(),
            cambi_version: None,
            tango_change_sets: None,
            change_sets,
            summary: Default::default(),
        };
        r.summarise();
        r.write(&ChangesOptions::path(
            &subject_dir.join("changes"),
            pin,
            None,
        ))
        .unwrap();
    }

    #[test]
    fn the_table_is_one_row_per_subject_with_the_footers_under_it() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let s = snapshot(dir.path(), &[]);
        assert_eq!(
            s.subjects
                .iter()
                .map(|r| r.subject.as_str())
                .collect::<Vec<_>>(),
            ["apollo", "bazel", "envoy"],
            "scratch/ is not a subject"
        );
        let text = render(&s).join("\n");

        // A finished subject: its elapsed is what the run took, not how long
        // ago it was, and its ETA is nothing.
        assert!(text.contains("bazel    done      changes 50/50"), "{text}");
        assert!(
            text.contains("23m20s"),
            "the finished run's own duration\n{text}"
        );
        // Every number the header promises.
        assert!(text.contains("50/50"), "{text}");
        assert!(text.contains("100.0%"), "min recall\n{text}");
        // A running subject: the commit, the tool on it, and an ETA from the
        // p50 of the commits that are done (38 left at 2 s).
        assert!(text.contains("envoy    history   history 12/50"), "{text}");
        assert!(text.contains("c3f9a1b cambium"), "{text}");
        let envoy = s.subjects.iter().find(|r| r.subject == "envoy").unwrap();
        assert_eq!(envoy.p50_ms, Some(2_000));
        assert_eq!(envoy.eta_secs, Some(76));
        assert_eq!(envoy.globals, Some(4));
        // A subject with nothing but a fetch is still a row.
        assert!(text.contains("apollo   idle"), "{text}");

        // The footers, once each, naming the subject.
        assert!(
            text.contains("errors\n  envoy    bazel query exit 3"),
            "{text}"
        );
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
        assert_eq!(s.subjects[1].step, "idle", "a subject with no results yet");
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
    fn a_row_expands_into_its_change_sets() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let s = snapshot(dir.path(), &["bazel".into()]);
        let rows = &s.subjects[0].change_sets;
        assert_eq!(rows.len(), 50);
        let text = render_change_sets(&s.subjects[0]).join("\n");
        assert!(text.contains("bazel — 50 change sets"), "{text}");
        assert!(text.contains("pr         base000..head000"), "{text}");
        assert!(text.contains("cambium 400 bazel-diff-rust 11200"), "{text}");
        assert!(text.contains("cambium_extra_select=2"), "{text}");

        // A subject with no comparison has an empty table rather than none.
        let s = snapshot(dir.path(), &["apollo".into()]);
        let text = render_change_sets(&s.subjects[0]).join("\n");
        assert!(text.contains("(no change sets compared yet)"), "{text}");
    }

    /// The whole point of reading files: a tree copied off the benchmark box
    /// renders without the run, and a failed step keeps its error.
    #[test]
    fn a_copied_tree_renders_and_a_failure_is_visible() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let copy = dir.path().join("copy");
        std::fs::create_dir_all(&copy).unwrap();
        let from = dir.path().join("envoy");
        let to = copy.join("envoy");
        copy_tree(&from, &to);
        let r = Reporter::new(&to, "changes", "envoy", "pin2", Step::Changes, None);
        r.item(7, "base..head");
        r.failed("tango: worktree is locked");

        let s = snapshot(&copy, &[]);
        assert_eq!(s.subjects.len(), 1);
        let row = &s.subjects[0];
        assert_eq!(row.step, "failed");
        assert!(row.eta_secs.is_none(), "a failed run has no ETA");
        let text = render(&s).join("\n");
        assert!(text.contains("tango: worktree is locked"), "{text}");
    }

    /// Results another engine build wrote are not this run's, however
    /// finished they look.
    #[test]
    fn a_row_another_build_wrote_is_stale_not_done() {
        let dir = tempfile::tempdir().unwrap();
        let apollo = dir.path().join("apollo");
        history_file(&apollo, "pin3", 5, 0);
        changes_file(&apollo, "pin3", 5, 5, 1.0);
        let mut st = State::fresh("apollo", "pin3");
        st.engine = "f8/old".into();
        st.history = Some(crate::state::HistoryState {
            from: "a".into(),
            to: "pin3".into(),
            bazel_version: String::new(),
            last_completed: None,
            done: true,
        });
        st.compared.insert("a..b".into());
        st.save(&apollo).unwrap();
        let names = vec!["apollo".to_string()];

        assert_eq!(snapshot(dir.path(), &names).subjects[0].step, "done");
        let other = BTreeMap::from([("apollo".to_string(), "f8/new".to_string())]);
        let row = &snapshot_with(dir.path(), &names, &other).subjects[0];
        assert_eq!(row.step, "stale");
        assert!(row.eta_secs.is_none());
        let same = BTreeMap::from([("apollo".to_string(), "f8/old".to_string())]);
        assert_eq!(
            snapshot_with(dir.path(), &names, &same).subjects[0].step,
            "done"
        );
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

    /// An unreadable result file is a row with no numbers, not a panic and not
    /// a refusal: the file may be from another schema, and the run it belongs
    /// to is still worth following.
    #[test]
    fn a_result_file_from_another_schema_costs_its_numbers_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let subject = dir.path().join("bazel");
        std::fs::create_dir_all(subject.join("changes")).unwrap();
        std::fs::write(
            subject.join("changes/changes-pin1.json"),
            r#"{"schema": 1, "repo": "bazel"}"#,
        )
        .unwrap();
        progress("bazel", "pin1", "changes", Step::Changes, 3, 10)
            .write(&subject)
            .unwrap();
        let s = snapshot(dir.path(), &[]);
        assert_eq!(s.subjects.len(), 1);
        assert_eq!(s.subjects[0].step, "changes");
        assert!(s.subjects[0].min_recall.is_none());
        assert!(render(&s).iter().any(|l| l.contains("changes 3/10")));
    }
}
