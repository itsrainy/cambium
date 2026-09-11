//! `docs/RESULTS.md`, generated from the run's JSON.
//!
//! The document is written by hand *around* generated blocks: each repository
//! owns a region delimited by `<!-- bench:<id>:begin -->` and
//! `<!-- bench:<id>:end -->`, and a run replaces the text between its own
//! markers and nothing else. That way prose a person wrote — caveats about a
//! repository, why a pin was chosen, the Findings section — survives every
//! regeneration, and two repositories' nightly jobs can update the same file
//! without fighting.
//!
//! Two regions are not a repository: `top` (the cross-repo table) and `bench`
//! (the synthetic headline). They are rendered from every repository's results
//! at once, so a per-repo job still rewrites them wholesale rather than
//! dropping its neighbours' rows.
//!
//! Everything here is a pure function of the result structs, so the rendering
//! is unit-testable and a formatting change never requires a benchmark run. It is
//! also written to survive *partial* data: a repository with a history and no
//! comparison, a comparison whose tools all failed, a result file written
//! by an older binary that has no `bazel_version_used`. Every such hole
//! renders as `n/a` or a one-line note, never as a panic or a missing table.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::harness::BenchReport;
use crate::history::CommitRecord;
use crate::history::HistoryResult;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::compare::{ChangesResult, WallStats};
use crate::util::percentile;

mod compare;

pub use compare::{
    ClassTotal, CompareSection, DepthAgreement, GlobalRow, ReasonRow, Scope, SoundnessRow,
    TimingRow, ToolNote,
};

/// `bench legacy`'s headline numbers at one graph size.
///
/// The reporting shape, not the measured one: a [`StageResult`] is a stage,
/// a preset and a bag of `extra` numbers, and this is the handful of them the
/// synthetic section prints. [`BenchLegacy::from_report`] does the projection.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BenchLegacy {
    /// The synthetic preset or repository scale the numbers describe.
    pub preset: String,
    pub targets: u64,
    /// bazel-diff: both revisions hashed, then diffed.
    pub legacy_cold_ms: u64,
    /// bazel-diff: one revision hashed against a stored baseline.
    pub legacy_warm_ms: u64,
    pub cambium_cold_ms: u64,
    pub cambium_warm_ms: u64,
    /// cambium's p90 over the stage's runs, when the sweep recorded one.
    #[serde(default)]
    pub cambium_p90_ms: u64,
    /// Labels both tools called impacted.
    #[serde(default)]
    pub agreed_labels: u64,
    /// Labels only one tool called impacted. `cambium_only` is legal
    /// over-approximation; `legacy_only` is unsoundness.
    #[serde(default)]
    pub cambium_only: u64,
    #[serde(default)]
    pub legacy_only: u64,
    /// Whether the two tools agreed on the label set (up to `select`).
    pub agreed: bool,
}

impl BenchLegacy {
    /// The `legacy_bazel_diff` stages of one bench report, as report rows.
    ///
    /// The numbers live in two places for historical reasons: a sweep records
    /// its size in `param` and its timing in `median_ms`/`p90_ms`, while a
    /// plain run records both in `extra`. `extra` wins where it is present.
    pub fn from_report(report: &BenchReport) -> Vec<Self> {
        report
            .results
            .iter()
            .filter(|r| r.stage == crate::legacy::STAGE)
            .map(|r| {
                let n = |k: &str| r.extra.get(k).copied().unwrap_or(0.0).round() as u64;
                let or_else = |k: &str, fallback: u64| match n(k) {
                    0 => fallback,
                    v => v,
                };
                let cambium_only = n("cambium_only");
                let legacy_only = n("bazel_diff_only");
                Self {
                    preset: r.preset.clone(),
                    targets: or_else("targets", r.param.unwrap_or(0)),
                    legacy_cold_ms: n("bazel_diff_cold_ms"),
                    legacy_warm_ms: n("bazel_diff_warm_ms"),
                    cambium_cold_ms: 0,
                    cambium_warm_ms: or_else("cambium_ms", r.median_ms.round() as u64),
                    cambium_p90_ms: or_else("cambium_p90_ms", r.p90_ms.round() as u64),
                    agreed_labels: n("agreed"),
                    cambium_only,
                    legacy_only,
                    agreed: cambium_only == 0 && legacy_only == 0,
                }
            })
            .collect()
    }

    /// The saved `bench sweep --stage legacy_bazel_diff` run at `path`.
    ///
    /// Missing or unparseable is not an error: the synthetic section then says
    /// the sweep has not been recorded, which is true and better than failing
    /// a benchmark report over a benchmark file.
    pub fn read(path: &Path) -> Vec<Self> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        match serde_json::from_str::<BenchReport>(&text) {
            Ok(r) => Self::from_report(&r),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "ignoring an unreadable bench file");
                Vec::new()
            }
        }
    }

    fn speedup(&self) -> f64 {
        if self.cambium_warm_ms == 0 {
            return f64::INFINITY;
        }
        self.legacy_warm_ms as f64 / self.cambium_warm_ms as f64
    }

    fn cold_speedup(&self) -> f64 {
        if self.cambium_warm_ms == 0 {
            return f64::INFINITY;
        }
        self.legacy_cold_ms as f64 / self.cambium_warm_ms as f64
    }
}

/// The `<prefix>*.json` files in `dir`, if it is one. A missing directory is
/// a subject that has not run that workload, not an error.
fn json_files(dir: &Path, prefix: &str) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".json"))
        })
        .collect()
}

/// One repository's results: what `bench-results/<id>/` held, plus the one
/// manifest fact the JSON does not carry, its id.
#[derive(Clone, Debug, Default)]
pub struct SubjectResults {
    pub id: String,
    pub history: Option<HistoryResult>,
    pub compares: Vec<ChangesResult>,
}

impl SubjectResults {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            history: None,
            compares: Vec::new(),
        }
    }

    pub fn with_history(mut self, r: Option<HistoryResult>) -> Self {
        self.history = r;
        self
    }

    pub fn with_compares(mut self, c: Vec<ChangesResult>) -> Self {
        self.compares = c;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.history.is_none() && self.compares.is_empty()
    }

    /// Everything `bench-results/<id>/` holds.
    ///
    /// The two workloads write into the directory that produced them,
    /// `<id>/history/` and `<id>/changes/`, so this looks in both and nowhere
    /// else. More than one history can be present -- a subject whose
    /// `bazel_version` changed leaves the older file behind -- and the newest
    /// by name wins for the history tables, while every change-set result is
    /// rendered.
    pub fn load(dir: &Path, id: &str) -> Result<Self> {
        let mut histories = json_files(&dir.join("history"), "history-");
        let mut compares = json_files(&dir.join("changes"), "changes-");
        histories.sort();
        compares.sort();
        Ok(Self::new(id)
            .with_history(
                histories
                    .last()
                    .map(|p| HistoryResult::read(p))
                    .transpose()?,
            )
            .with_compares(
                compares
                    .iter()
                    .map(|p| ChangesResult::read(p))
                    .collect::<Result<Vec<_>>>()?,
            ))
    }

    /// Every manifest entry that has a results directory under `parent`, in
    /// manifest order, plus any stray directory the manifest no longer names.
    ///
    /// The strays are deliberate: a renamed or removed entry still has numbers
    /// worth showing, and silently dropping a subject's rows from
    /// `docs/RESULTS.md` is worse than an unexplained row.
    ///
    /// `subjects` is consulted only to fill in the Bazel version of a history
    /// written before `bazel_version_used` existed: with no override, that run
    /// took whatever the worktree's `.bazelversion` asked bazelisk for, and
    /// that file is the honest record.
    pub fn collect(
        manifest: &crate::manifest::Manifest,
        parent: &Path,
        subjects: &crate::checkout::SubjectsDir,
    ) -> Result<Vec<Self>> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for entry in &manifest.repos {
            seen.insert(entry.id.clone());
            let dir = parent.join(&entry.id);
            if !dir.is_dir() {
                continue;
            }
            let mut r = Self::load(&dir, &entry.id)?;
            if let Some(h) = r.history.as_mut() {
                if h.bazel_version_used.is_empty() && h.bazel_version.is_empty() {
                    let path = subjects.worktree(&entry.id).join(".bazelversion");
                    if let Ok(v) = std::fs::read_to_string(path) {
                        h.bazel_version_used = v.trim().to_string();
                    }
                }
            }
            if !r.is_empty() {
                out.push(r);
            }
        }
        let mut strays: Vec<std::path::PathBuf> = std::fs::read_dir(parent)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        strays.sort();
        for dir in strays {
            let Some(id) = dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if seen.contains(id) {
                continue;
            }
            let r = Self::load(&dir, id)?;
            if !r.is_empty() {
                out.push(r);
            }
        }
        Ok(out)
    }

    fn pin(&self) -> String {
        self.history
            .as_ref()
            .map(|r| r.pin.clone())
            .or_else(|| self.compares.first().map(|c| c.pin.clone()))
            .unwrap_or_default()
    }

    /// What `bazel --version` said, over every result file this repository
    /// produced. Older files do not carry it; a run under a `--bazel-version`
    /// override at least knows what it asked for, and says so.
    fn bazel_used(&self) -> String {
        let mut used: BTreeSet<String> = BTreeSet::new();
        if let Some(r) = &self.history {
            if !r.bazel_version_used.is_empty() {
                used.insert(tidy_version(&r.bazel_version_used));
            }
        }
        for c in &self.compares {
            if let Some(v) = c.bazel_version_used.as_deref().filter(|v| !v.is_empty()) {
                used.insert(tidy_version(v));
            }
        }
        if !used.is_empty() {
            return used.into_iter().collect::<Vec<_>>().join(", ");
        }
        let mut asked: BTreeSet<String> = BTreeSet::new();
        if let Some(r) = &self.history {
            if !r.bazel_version.is_empty() {
                asked.insert(r.bazel_version.clone());
            }
        }
        for c in &self.compares {
            if let Some(v) = c.bazel_version.as_deref().filter(|v| !v.is_empty()) {
                asked.insert(v.to_string());
            }
        }
        match asked.is_empty() {
            true => "n/a".to_string(),
            false => format!(
                "{} (requested)",
                asked.into_iter().collect::<Vec<_>>().join(", ")
            ),
        }
    }

    fn commits(&self) -> &[CommitRecord] {
        self.history.as_ref().map(|r| &r.commits[..]).unwrap_or(&[])
    }

    /// Global commits and why, most common first. From the history when it ran
    /// any commits, else from the comparison change sets, which see the same
    /// invalidators over a different (sampled) slice of history.
    fn globals(&self) -> (usize, Vec<(String, usize)>) {
        let mut h: BTreeMap<String, usize> = BTreeMap::new();
        let mut n = 0;
        if !self.commits().is_empty() {
            for c in self.commits().iter().filter(|c| c.global) {
                n += 1;
                *h.entry(reason(c.global_reason.as_deref())).or_default() += 1;
            }
        } else {
            for c in &self.compares {
                for p in c.change_sets.iter().filter(|p| p.global) {
                    n += 1;
                    *h.entry(reason(p.cambium.global_reason.as_deref()))
                        .or_default() += 1;
                }
            }
        }
        let mut v: Vec<(String, usize)> = h.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        (n, v)
    }

    fn change_sets_total(&self) -> usize {
        self.compares.iter().map(|c| c.summary.change_sets).sum()
    }

    fn failures(&self) -> usize {
        self.compares
            .iter()
            .map(|c| c.summary.change_sets - c.summary.passed)
            .sum()
    }

    fn min_recall(&self) -> Option<f64> {
        self.compares
            .iter()
            .filter(|c| c.summary.change_sets > 0)
            .map(|c| c.summary.min_recall)
            .fold(None, |acc, r| Some(acc.map_or(r, |a: f64| a.min(r))))
    }

    /// Every tool named anywhere in this repository's comparisons.
    fn tools(&self) -> Vec<String> {
        let mut names: BTreeSet<String> = BTreeSet::new();
        for c in &self.compares {
            for p in &c.change_sets {
                for pr in &p.tools {
                    names.insert(pr.name.clone());
                }
            }
        }
        names.into_iter().collect()
    }

    fn mean_precision(&self, name: &str) -> Option<f64> {
        let vals: Vec<f64> = self
            .compares
            .iter()
            .filter_map(|c| c.summary.mean_precision.get(name).copied())
            .collect();
        (!vals.is_empty()).then(|| vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Over **all** compared change sets, globals included — the headline number, so
    /// the one the top table carries. Results written before that series
    /// existed fall back to the incremental one.
    fn tool_cold_p50(&self, name: &str) -> Option<u64> {
        let vals: Vec<u64> = self
            .compares
            .iter()
            .filter_map(|c| {
                c.summary
                    .all_tool_cold
                    .get(name)
                    .filter(|w| w.samples > 0)
                    .or_else(|| c.summary.tool_cold.get(name))
            })
            .filter(|w| w.samples > 0)
            .map(|w| w.p50_ms)
            .collect();
        vals.iter().copied().max()
    }

    fn cambium_warm_p50(&self) -> Option<u64> {
        let vals: Vec<u64> = self
            .compares
            .iter()
            .map(|c| {
                let all = &c.summary.all_cambium_warm;
                if all.samples > 0 {
                    all
                } else {
                    &c.summary.cambium_warm
                }
            })
            .filter(|w| w.samples > 0)
            .map(|w| w.p50_ms)
            .collect();
        vals.iter().copied().max()
    }
}

/// A `global_reason` shortened to the part that differs between commits: the
/// invalidator's own text, without the path that matched it, so the histogram
/// buckets by *cause* rather than by file.
fn reason(r: Option<&str>) -> String {
    let r = r.unwrap_or("unrecorded").trim();
    match r.split_once(" matches an invalidator") {
        Some((path, _)) => format!("`{path}` matches an invalidator"),
        None => r.to_string(),
    }
}

/// `bazel 7.4.1` → `7.4.1`: the report's column already says it is Bazel.
fn tidy_version(v: &str) -> String {
    v.trim()
        .strip_prefix("bazel ")
        .unwrap_or(v.trim())
        .to_string()
}

/// One row of the top-level table.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TopRow {
    pub repo: String,
    pub pin: String,
    /// What `bazel --version` reported, or `n/a`.
    pub bazel_used: String,
    pub targets: u32,
    pub cold_build_s: f64,
    /// Over non-global commits only — the commits an updater actually works
    /// on; a global answers `//...` without walking anything.
    pub history_p50_ms: u64,
    pub history_p90_ms: u64,
    pub history_commits: usize,
    pub globals: usize,
    pub global_reasons: Vec<(String, usize)>,
    pub change_sets: usize,
    pub failures: usize,
    pub min_recall: Option<f64>,
    /// Mean precision per tool.
    pub precision: BTreeMap<String, f64>,
    /// Tool cold p50 per tool, against cambium's warm p50.
    pub tool_cold_p50: BTreeMap<String, u64>,
    pub cambium_warm_p50: Option<u64>,
}

fn ms(v: u64) -> String {
    if v >= 10_000 {
        format!("{:.1} s", v as f64 / 1000.0)
    } else {
        format!("{v} ms")
    }
}

fn opt_ms(v: Option<u64>) -> String {
    v.map(ms).unwrap_or_else(|| "n/a".into())
}

fn pct(v: f64) -> String {
    format!("{:.1}%", v * 100.0)
}

fn opt_pct(v: Option<f64>) -> String {
    v.map(pct).unwrap_or_else(|| "n/a".into())
}

fn wall(w: &WallStats) -> String {
    if w.samples == 0 {
        "n/a".to_string()
    } else {
        format!("{} / {}", ms(w.p50_ms), ms(w.p90_ms))
    }
}

fn opt_wall(w: Option<&WallStats>) -> String {
    w.map(wall).unwrap_or_else(|| "n/a".into())
}

fn short(sha: &str) -> String {
    if sha.is_empty() {
        return "n/a".to_string();
    }
    sha.chars().take(12).collect()
}

/// Thousands separators, because six-digit target counts are the whole point.
fn num(v: u64) -> String {
    let s = v.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Markdown tables are one row per line, so a multi-line cell is `<br>`.
fn lines(cells: &[String]) -> String {
    if cells.is_empty() {
        "n/a".to_string()
    } else {
        cells.join("<br>")
    }
}

/// Today, as `YYYY-MM-DD`, from the system clock. Rendered into the section so
/// a stale table is visible as stale.
fn today() -> String {
    let secs = crate::util::now_unix();
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days + 719_468);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard's `civil_from_days`, so the report needs no date crate.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// p50/p90/p99/max over a set of commits' total update wall.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Lat {
    n: usize,
    p50: u64,
    p90: u64,
    p99: u64,
    max: u64,
}

fn latency(commits: impl Iterator<Item = u64>) -> Lat {
    let mut v: Vec<u64> = commits.collect();
    v.sort_unstable();
    Lat {
        n: v.len(),
        p50: percentile(&v, 0.5),
        p90: percentile(&v, 0.9),
        p99: percentile(&v, 0.99),
        max: v.last().copied().unwrap_or(0),
    }
}

fn lat_row(label: &str, l: Lat) -> String {
    if l.n == 0 {
        return format!("| {label} | 0 | n/a | n/a | n/a | n/a |\n");
    }
    format!(
        "| {label} | {} | {} | {} | {} | {} |\n",
        l.n,
        ms(l.p50),
        ms(l.p90),
        ms(l.p99),
        ms(l.max)
    )
}

// ---------------------------------------------------------------- top table

/// A row for the top table, derived from a repository's results.
pub fn top_row(r: &SubjectResults) -> TopRow {
    let non_global = latency(r.commits().iter().filter(|c| !c.global).map(|c| c.total_ms));
    let (globals, reasons) = r.globals();
    let mut row = TopRow {
        repo: r.id.clone(),
        pin: r.pin(),
        bazel_used: r.bazel_used(),
        history_p50_ms: non_global.p50,
        history_p90_ms: non_global.p90,
        history_commits: non_global.n,
        globals,
        global_reasons: reasons.into_iter().take(3).collect(),
        change_sets: r.change_sets_total(),
        failures: r.failures(),
        min_recall: r.min_recall(),
        cambium_warm_p50: r.cambium_warm_p50(),
        ..Default::default()
    };
    if let Some(rep) = &r.history {
        row.targets = rep.cold.nodes;
        row.cold_build_s = rep.cold.wall_ms as f64 / 1000.0;
    }
    for name in r.tools() {
        if let Some(p) = r.mean_precision(&name) {
            row.precision.insert(name.clone(), p);
        }
        if let Some(c) = r.tool_cold_p50(&name) {
            row.tool_cold_p50.insert(name, c);
        }
    }
    row
}

/// The table at the top of the document: one line per repository that has
/// results at all.
pub fn render_top_table(repos: &[SubjectResults]) -> String {
    let rows: Vec<TopRow> = repos
        .iter()
        .filter(|r| !r.is_empty())
        .map(top_row)
        .collect();
    if rows.is_empty() {
        return "No subject in `bench-results/` has results yet.".to_string();
    }
    let mut s = String::new();
    s.push_str(
        "| subject | pin | Bazel | targets | cold build | history p50/p90 | globals | change sets | \
         pass/fail | min recall | precision | tool cold p50 vs cambium warm p50 |\n",
    );
    s.push_str("|---|---|---|---:|---:|---:|---|---:|---:|---:|---|---|\n");
    for r in &rows {
        let globals = if r.globals == 0 {
            "0".to_string()
        } else {
            let mut cells = vec![format!("**{}**", r.globals)];
            cells.extend(
                r.global_reasons
                    .iter()
                    .map(|(why, n)| format!("{n}× {}", truncate(why, 44))),
            );
            lines(&cells)
        };
        let precision = lines(
            &r.precision
                .iter()
                .map(|(n, p)| format!("{n} {}", pct(*p)))
                .collect::<Vec<_>>(),
        );
        let speed = lines(
            &r.tool_cold_p50
                .iter()
                .map(|(n, c)| {
                    let warm = r.cambium_warm_p50.unwrap_or(0);
                    let x = if warm == 0 {
                        "n/a".to_string()
                    } else {
                        format!("{:.0}×", *c as f64 / warm as f64)
                    };
                    format!("{n} {} vs {} ({x})", ms(*c), opt_ms(r.cambium_warm_p50))
                })
                .collect::<Vec<_>>(),
        );
        s.push_str(&format!(
            "| [{}](#{}) | `{}` | {} | {} | {} | {} / {} | {} | {} | {} | {} | {} | {} |\n",
            r.repo,
            r.repo.to_lowercase(),
            short(&r.pin),
            r.bazel_used,
            if r.targets == 0 {
                "n/a".to_string()
            } else {
                num(r.targets as u64)
            },
            if r.cold_build_s == 0.0 {
                "n/a".to_string()
            } else {
                format!("{:.0} s", r.cold_build_s)
            },
            ms(r.history_p50_ms),
            ms(r.history_p90_ms),
            globals,
            r.change_sets,
            format_args!("{} / {}", r.change_sets - r.failures, r.failures),
            opt_pct(r.min_recall),
            precision,
            speed,
        ));
    }
    s.push_str(
        "\nThe tool-vs-cambium column is over **all** compared change sets, the \
         ones cambium answered with `//...` included, so the ratio is not an \
         average over the change sets cambium is fast on; each subject's section \
         breaks it out. History p50/p90 are over **non-global** commits (the ones an updater \
         actually walks); the per-subject sections give both distributions. \
         `pass/fail` counts compared change sets. Precision against a tool is \
         informational; recall against the `rdeps` oracle is the gate.\n",
    );
    s
}

// ---------------------------------------------------------- per-repo section

/// The markdown for one repository: everything a reader needs to judge the
/// claim, in the order they will ask for it — what was measured, whether it
/// was sound, then how fast.
pub fn render_subject_section(r: &SubjectResults, bench: Option<&BenchLegacy>) -> String {
    // No heading of its own: the document hand-writes `## <id>` above each
    // marked region, and that heading is what the top table links to. A
    // generated duplicate would give the anchor a `-1` suffix and break the
    // link, so the section starts at the facts.
    let mut s = String::new();
    s.push_str(&format!("- pin: `{}`\n", short(&r.pin())));
    s.push_str(&format!("- Bazel: {}\n", r.bazel_used()));
    s.push_str(&format!("- generated: {}\n", today()));
    if let Some(rep) = &r.history {
        s.push_str(&format!("- tool: `{}`\n", rep.tool_sha));
    }
    if r.is_empty() {
        s.push_str(
            "\nNo results yet: neither `history-*.json` nor `changes-*.json` \
                    was found for this subject.\n",
        );
        return s;
    }
    s.push('\n');

    if let Some(rep) = &r.history {
        s.push_str(&render_history(rep));
    } else {
        s.push_str(
            "_No history for this subject yet; the cold build and \
                    updater-latency tables below are omitted._\n\n",
        );
    }

    if !r.compares.is_empty() {
        s.push_str(&render_compare(r));
    } else {
        s.push_str(
            "_No comparison for this subject yet: no \
                    `changes-*.json`, so there are no precision, recall or \
                    disagreement numbers._\n\n",
        );
    }

    if let Some(b) = bench {
        s.push_str("#### bazel-diff on a synthetic graph of this size (`bench legacy`)\n\n");
        s.push_str(&render_bench_table(std::slice::from_ref(b)));
    }
    s
}

fn render_history(rep: &HistoryResult) -> String {
    let mut s = String::new();
    let c = &rep.cold;
    s.push_str("#### Cold build\n\n");
    if c.partial.partial {
        // Said before the numbers, not after: every count in this section is
        // of a repository minus the packages Bazel refused to load.
        s.push_str(&format!(
            "> **Loaded with {} packages in error.** `bazel query //...` finished \
             under `--keep_going` with load errors, so the graph below omits them; \
             the incremental path omits the same packages, which is why the two \
             still agree.{}\n\n",
            c.partial.packages_in_error,
            if c.partial.packages.is_empty() {
                String::new()
            } else {
                format!(" First: `{}`.", c.partial.packages.join("`, `"))
            }
        ));
    }
    s.push_str("| targets | edges | packages | index bytes | wall |\n");
    s.push_str("|---:|---:|---:|---:|---:|\n");
    s.push_str(&format!(
        "| {} | {} | {} | {} | {} |\n\n",
        num(c.nodes as u64),
        num(c.edges),
        num(c.packages as u64),
        num(c.index_bytes),
        ms(c.wall_ms)
    ));

    let u = &rep.summary;
    s.push_str(&format!(
        "#### History ({} commits, `{}..{}`)\n\n",
        u.commits,
        short(&rep.from),
        short(&rep.to)
    ));
    if !rep.done {
        s.push_str(
            "_This history has not finished; the numbers below cover the \
                    commits it has applied so far._\n\n",
        );
    }
    s.push_str("| over | commits | p50 | p90 | p99 | max |\n|---|---:|---:|---:|---:|---:|\n");
    s.push_str(&lat_row(
        "all commits",
        latency(rep.commits.iter().map(|c| c.total_ms)),
    ));
    s.push_str(&lat_row(
        "non-global commits",
        latency(rep.commits.iter().filter(|c| !c.global).map(|c| c.total_ms)),
    ));
    s.push('\n');
    s.push_str(
        "A *global* commit touches an invalidator, so cambium answers `//...` \
         without walking: it is fast for the wrong reason, and including it \
         would flatter the updater. Both rows are given so the difference is \
         visible.\n\n",
    );
    s.push_str(&format!(
        "- zero-dirty commits (a diff that dirtied no package): **{}**\n",
        pct(u.zero_dirty_fraction)
    ));
    s.push_str(&format!(
        "- largest dirty set: **{} packages** at `{}`\n",
        u.largest_dirty_packages,
        short(&u.largest_dirty_commit)
    ));
    s.push_str(&format!(
        "- `rbuildfiles` calls: {} of {} commits\n",
        u.rbuildfiles_calls, u.commits
    ));
    s.push_str(&format!(
        "- commits where a changed `.bzl` dirtied packages through the static \
         load closure: {} of {}\n",
        u.bzl_closure_commits, u.commits
    ));
    s.push_str(&format!(
        "- mean affected targets: {:.0}\n\n",
        u.mean_affected
    ));

    let mut h: BTreeMap<String, usize> = BTreeMap::new();
    for c in rep.commits.iter().filter(|c| c.global) {
        *h.entry(reason(c.global_reason.as_deref())).or_default() += 1;
    }
    s.push_str(&format!("Global commits: **{}**", u.globals));
    if h.is_empty() {
        s.push_str(" — no commit in this range hit an invalidator.\n\n");
    } else {
        s.push_str("\n\n| reason | commits |\n|---|---:|\n");
        let mut v: Vec<(String, usize)> = h.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (why, n) in v {
            s.push_str(&format!("| {why} | {n} |\n"));
        }
        s.push('\n');
    }

    if let Some(o) = &rep.oracle {
        s.push_str(&format!(
            "Incremental-equals-full oracle at the pin: **{}**{}\n\n",
            if o.ok { "pass" } else { "FAIL" },
            if o.ok {
                String::new()
            } else {
                format!(" — {}", o.diff.summary())
            }
        ));
    }
    s
}

/// The Bazel a comparison ran under: what it reported, else what was asked
/// for, else `n/a`.
fn compare_bazel(c: &ChangesResult) -> String {
    c.bazel_version_used
        .as_deref()
        .map(tidy_version)
        .or_else(|| c.bazel_version.clone())
        .unwrap_or_else(|| "n/a".into())
}

/// What the globally invalidated change sets cost.
///
/// cambium answers `//...` on them, which is sound — it is a superset of any
/// answer — and useless: the user is told to rebuild the repository. This
/// table is the price of that, side by side with the precise set the tools
/// found on the same change sets, so "cambium goes global on N% of commits" is a
/// sentence with a number after it rather than an asterisk.
fn render_globals(rows: &[GlobalRow]) -> String {
    let mut s = String::from("#### Globals\n\n");
    if rows.is_empty() {
        s.push_str("No compared change set was globally invalidated.\n\n");
        return s;
    }
    s.push_str(
        "| Bazel | change sets | cambium `//...` | tool | its precise set | \
         tool cold p50 | cambium cold/warm p50 |\n\
         |---|---:|---:|---|---:|---:|---:|\n",
    );
    for g in rows {
        let cambium = format!(
            "{} / {}",
            ms(g.cambium_cold_p50_ms),
            ms(g.cambium_warm_p50_ms)
        );
        match (&g.tool, g.tool_labels) {
            (Some(name), Some(labels)) => s.push_str(&format!(
                "| `{}` | {} | {} | {name} | {} | {} | {cambium} |\n",
                g.bazel,
                g.change_sets,
                num(g.cambium_labels),
                num(labels),
                opt_ms(g.tool_cold_p50_ms),
            )),
            _ => s.push_str(&format!(
                "| `{}` | {} | {} | _none ran_ | n/a | n/a | {cambium} |\n",
                g.bazel,
                g.change_sets,
                num(g.cambium_labels),
            )),
        }
    }
    s.push_str(
        "\nMeans over the global change sets. *cambium `//...`* is the number of \
         rule targets `//...` names at head — the answer cambium gave — and \
         *its precise set* is what the tool found on the same change_set. The \
         ratio between them is the over-approximation a global hands the user; \
         the wall columns say what each tool charged to produce its answer. \
         These change_sets are excluded from precision, the class counts and recall \
         (there is nothing to be wrong about inside `//...`) and included in \
         the `all change sets` timings above.\n\n",
    );
    s
}

/// The comparison section: the numbers in [`CompareSection`], as prose and
/// tables.
fn render_compare(r: &SubjectResults) -> String {
    render_compare_section(&CompareSection::compute(r))
}

fn render_compare_section(c: &CompareSection) -> String {
    let mut s = String::new();

    // ---- soundness first ----
    s.push_str("#### Three-way comparison\n\n");
    s.push_str(&format!(
        "{} change sets compared, {} passed, {} failed ({} of them \
         global, which pass by construction: `//...` contains every answer. \
         The tools still ran on them — see **Globals** below). Worst \
         oracle recall: **{}**.\n\n",
        c.change_sets,
        c.passed,
        c.failed,
        c.globals,
        opt_pct(c.min_recall),
    ));
    s.push_str("| Bazel | tool | change sets | passed | precision | min recall |\n");
    s.push_str("|---|---|---:|---:|---:|---:|\n");
    for row in &c.soundness {
        match &row.tool {
            None => s.push_str(&format!(
                "| `{}` | _none ran_ | {} | {} | n/a | {} |\n",
                row.bazel,
                row.change_sets,
                row.passed,
                pct(row.min_recall)
            )),
            Some(name) => s.push_str(&format!(
                "| `{}` | {name} | {} | {} | {} | {} |\n",
                row.bazel,
                row.change_sets,
                row.passed,
                opt_pct(row.precision),
                pct(row.min_recall),
            )),
        }
    }
    s.push('\n');

    // ---- timings ----
    s.push_str("#### Timings (p50 / p90)\n\n");
    s.push_str(
        "| Bazel | tool | over | change sets | cold | warm | engine cold | engine warm |\n\
         |---|---|---|---:|---:|---:|---:|---:|\n",
    );
    for row in &c.timings {
        let tool = match &row.tool {
            Some(name) => name.clone(),
            None => "**cambium**".to_string(),
        };
        s.push_str(&format!(
            "| `{}` | {tool} | {} | {} | {} | {} | {} | {} |\n",
            row.bazel,
            row.scope.name(),
            row.change_sets,
            opt_wall(row.cold.as_ref()),
            opt_wall(row.warm.as_ref()),
            opt_wall(row.cold_engine.as_ref()),
            opt_wall(row.warm_engine.as_ref()),
        ));
    }
    s.push_str(&format!(
        "\n**Quote the `{}` rows.** They cover every change set compared, \
         including the {} where cambium fell back to `//...` and did no \
         incremental work at all — which is what a user running cambium over \
         this history actually waits for. The `{}` rows drop \
         those; they are the rows that belong with the precision and class \
         numbers above, because those are computed over exactly the same \
         change sets, but read on their own they flatter cambium — an average \
         over the change sets it is fast on. {} of the incremental \
         change sets \
         re-indexed the whole repository (the dirty set was too large to \
         splice), so their *warm* is a cold number under a warm label.\n\n",
        Scope::All.name(),
        c.globals,
        Scope::Incremental.name(),
        c.cold_rebuild_change_sets,
    ));
    s.push_str(
        "cambium's *cold* is a full index build from `bazel query //...`; its \
         *warm* is an update against the published baseline, which is what a \
         CI job actually pays. bazel-diff's warm reuses the base revision's \
         hashes; Tango has no warm mode, so its cell is `n/a`. *change sets* is \
         how many change sets each tool's distribution is taken over: Tango is \
         often capped (it costs 30-500 s a change set) while cambium and \
         bazel-diff run on all of them.\n\n",
    );
    s.push_str(
        "*cold* and *warm* are whole-process walls: for cambium that includes \
         `cambi` starting and serialising its JSON answer. *engine cold* and \
         *engine warm* are the engine's own `timings_ms.total` from the same \
         runs, the wall without the process around it; they are `n/a` for the \
         other tools, which report no such split, and for results recorded \
         before the harness ran `cambi` as a child process.\n\n",
    );

    // ---- globals ----
    s.push_str(&render_globals(&c.globals_table));

    // ---- classes ----
    s.push_str("#### Disagreements by class\n\n");
    s.push_str("| class | labels | fails the run | meaning |\n|---|---:|---|---|\n");
    for t in &c.classes {
        s.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            t.class.name(),
            num(t.labels as u64),
            if t.class.is_failure() {
                "**yes**"
            } else {
                "no"
            },
            t.class.meaning(),
        ));
    }
    s.push_str("\nCounts are labels, summed over every change set and tool.\n\n");

    // ---- why cambium reported the extras ----
    if c.has_reasons() {
        s.push_str("#### Why cambium reported the labels the tool did not\n\n");
        s.push_str("| tool | cambium's reason | labels |\n|---|---|---:|\n");
        for row in &c.reasons {
            s.push_str(&format!(
                "| {} | {} | {} |\n",
                row.tool,
                row.reason,
                num(row.labels as u64)
            ));
        }
        s.push_str(
            "\nThese are cambium's *own* explanations for the extra labels \
             (`affected --explain`), not the classifier's: they say what made \
             cambium reach the target, where the class table says why the \
             disagreement is benign.\n\n",
        );
    }

    // Two numbers that belong to the tools rather than to a class.
    if !c.tool_notes.is_empty() {
        for note in &c.tool_notes {
            let agree = match &note.depth {
                Some(d) => format!(
                    "depth agrees on {} of shared labels ({} {})",
                    pct(d.mean),
                    d.change_sets,
                    if d.change_sets == 1 {
                        "change set"
                    } else {
                        "change sets"
                    }
                ),
                None => "no distances reported".to_string(),
            };
            s.push_str(&format!(
                "- `{}`: {agree}; {} file-target rows dropped to match \
                 cambium's universe.\n",
                note.tool,
                num(note.dropped_file_targets as u64)
            ));
        }
        s.push('\n');
    }

    // ---- the oracle ----
    s.push_str("#### `rdeps` oracle\n\n");
    if c.rdeps.is_empty() {
        s.push_str("No change set ran the oracle (`--oracle` without `rdeps`, or every change set was global).\n\n");
    } else {
        let o = &c.rdeps;
        s.push_str("| change sets with an oracle | source labels queried | labels skipped | partial change sets | partial batches | missing labels | of those, deleted-file consumers | of those, external route | external labels not counted |\n");
        s.push_str("|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n");
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n\n",
            o.change_sets,
            o.roots,
            o.skipped_roots,
            o.partial_change_sets,
            o.partial_batches,
            o.missing,
            o.deleted_missing,
            o.external_route,
            o.missing_external
        ));
        s.push_str(
            "A *partial* batch is a `--keep_going` query that exited 3: Bazel \
             answered for what loaded. The oracle is a lower bound on what \
             cambium must contain, so a partial answer is still sound to check \
             against — it just proves less. `missing labels` is the count that \
             gates the run, and must be zero. *external route* is the subset \
             `somepath` shows Bazel reaching only by leaving `//...` and coming \
             back (see [Externals and soundness](#externals-and-soundness)); it \
             is a label on a failure, not an excuse for one. *deleted-file \
             consumers* is the other half of the check: a file that is gone at \
             head has no label to query, so its consumers come from the base \
             index and must appear in cambium's answer by label or by package. \
             The last column is the `@repo//...` half of the difference, which \
             cambium does not index by construction and which recall therefore \
             ignores.\n\n",
        );
    }

    // ---- the .bzl load-closure oracle ----
    s.push_str("#### `.bzl` load-closure oracle\n\n");
    if c.load.is_empty() {
        s.push_str("No change set changed a `.bzl` file.\n\n");
    } else {
        let bzl = &c.load;
        s.push_str(
            "| change sets with a `.bzl` change | `.bzl` files checked | partial answers | repository-evaluation answers | packages cambium missed |\n",
        );
        s.push_str("|---:|---:|---:|---:|---:|\n");
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n\n",
            bzl.change_sets,
            bzl.checked,
            bzl.partial,
            bzl.repository_evaluation,
            bzl.missing_packages
        ));
        s.push_str(
            "cambium resolves a changed `.bzl` from the static load closure it \
             stores in the index, with no Bazel call. This checks that answer \
             against `rbuildfiles`, the call it replaced: cambium's dirty set \
             must contain every main-repo package Bazel names. The last column \
             gates the run and must be zero; a non-zero entry files those \
             packages under `cambium_missing_bzl_closure`. A *partial* answer \
             is a Sky Query that exited 3 — the very failure mode the static \
             closure removes, since `rbuildfiles` loads every external \
             subject and the closure loads none. A *repository-evaluation* \
             answer is one where Bazel named a package whose `BUILD` file has \
             no `load()` in it at all, or where the changed `.bzl` is loaded \
             by no `BUILD` file in the head tree, only from the workspace \
             files and the `.bzl` files they load: `rbuildfiles` walks \
             Skyframe's file nodes, so a change to a repository-declaring \
             `.bzl` reaches every package through the repository mapping \
             rather than through a load edge, and containment against a load \
             closure is not a question that answer can settle. Those change \
             sets are counted here and checked by the `rdeps` oracle \
             instead.\n\n",
        );
    }

    s
}

fn tool_names(c: &ChangesResult) -> Vec<String> {
    let names: BTreeSet<String> = c
        .change_sets
        .iter()
        .flat_map(|p| p.tools.iter().map(|pr| pr.name.clone()))
        .collect();
    names.into_iter().collect()
}

// --------------------------------------------------------- synthetic scaling

/// The `bench legacy` sweep: the headline number, on graphs no real repository
/// reaches, next to the chart the same sweep plots.
pub fn render_bench_section(rows: &[BenchLegacy], chart: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str(
        "These are *synthetic* graphs (`cambium-bench sweep --stage \
         legacy_bazel_diff`), not the real subjects above: cambium and bazel-diff on \
         the same generated workspace, the same change, and a `bazel` shim that \
         replays a recorded query, so what is measured is each tool's own work \
         rather than a Bazel server. See [METHODOLOGY.md](METHODOLOGY.md#engine-stages).\n\n",
    );
    if rows.is_empty() {
        s.push_str("_No `bench legacy` sweep recorded yet._\n");
        return s;
    }
    s.push_str(&render_bench_table(rows));
    s.push_str(
        "\ncambium's column is its warm, in-memory update against the base \
         index — the operation bazel-diff's warm column is also doing. \
         `agreed` is the label sets matching up to `select()`; any label \
         bazel-diff reports and cambium does not fails the stage outright, so \
         a row exists only because that count was zero.\n",
    );
    if let Some(c) = chart {
        s.push_str(&format!("\n![bazel-diff vs cambium, log-log]({c})\n"));
    }
    s
}

fn render_bench_table(rows: &[BenchLegacy]) -> String {
    let mut s = String::new();
    s.push_str(
        "| targets | bazel-diff cold | bazel-diff warm | cambium | cambium p90 | \
         vs cold | vs warm | agreed labels |\n",
    );
    s.push_str("|---:|---:|---:|---:|---:|---:|---:|---|\n");
    let mut rows: Vec<&BenchLegacy> = rows.iter().collect();
    rows.sort_by_key(|r| r.targets);
    for b in rows {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {:.0}× | {:.0}× | {} |\n",
            num(b.targets),
            ms(b.legacy_cold_ms),
            ms(b.legacy_warm_ms),
            ms(b.cambium_warm_ms),
            if b.cambium_p90_ms == 0 {
                "n/a".to_string()
            } else {
                ms(b.cambium_p90_ms)
            },
            b.cold_speedup(),
            b.speedup(),
            if b.agreed {
                format!("{} — none missed", num(b.agreed_labels))
            } else {
                format!(
                    "**{} cambium-only, {} bazel-diff-only**",
                    num(b.cambium_only),
                    num(b.legacy_only)
                )
            },
        ));
    }
    s
}

pub fn begin_marker(subject_id: &str) -> String {
    format!("<!-- bench:{subject_id}:begin -->")
}

pub fn end_marker(subject_id: &str) -> String {
    format!("<!-- bench:{subject_id}:end -->")
}

/// Replaces `subject_id`'s region of `doc` with `section`, appending the region
/// (markers and all) when it is not there yet.
///
/// A begin marker with no end marker is treated as absent rather than as a
/// licence to eat the rest of the file.
pub fn replace_section(doc: &str, subject_id: &str, section: &str) -> String {
    let (begin, end) = (begin_marker(subject_id), end_marker(subject_id));
    let block = format!("{begin}\n{}\n{end}\n", section.trim_end());
    match (doc.find(&begin), doc.find(&end)) {
        (Some(b), Some(e)) if e > b => {
            let mut out = String::with_capacity(doc.len() + block.len());
            out.push_str(&doc[..b]);
            out.push_str(&block);
            // Exactly one newline: the block already ends the marker's own
            // line, and eating the rest would close up the blank line before
            // whatever hand-written heading follows.
            let rest = &doc[e + end.len()..];
            out.push_str(rest.strip_prefix('\n').unwrap_or(rest));
            out
        }
        _ => {
            let mut out = doc.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&block);
            out
        }
    }
}

/// [`replace_section`] against a file, creating it when missing.
pub fn update_doc(path: &Path, subject_id: &str, section: &str) -> Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let updated = replace_section(&existing, subject_id, section);
    if updated == existing {
        return Ok(());
    }
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, updated).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::Class;
    use crate::compare::{CambiumRun, ChangeSetResult, ChangesSummary, ToolComparison};
    use crate::history::{ColdBuild, Summary};

    fn cold() -> ColdBuild {
        ColdBuild {
            commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            partial: Default::default(),
            wall_ms: 19_540,
            index_bytes: 3_754_856,
            nodes: 24_519,
            edges: 53_242,
            packages: 524,
            rules: 3_901,
            tests: 991,
            lineage: "l".into(),
            stats: serde_json::Value::Null,
        }
    }

    fn commit(sha: &str, total_ms: u64, global: bool, why: Option<&str>) -> CommitRecord {
        CommitRecord {
            engine_total_ms: 0,
            commit: sha.into(),
            parent: "p".into(),
            changed_files: 3,
            ignored_files: 0,
            dirty_packages: if global { 0 } else { 2 },
            queried_targets: 0,
            dirty_ms: 0,
            query_ms: 0,
            splice_ms: 0,
            direct_ms: 0,
            walk_ms: 0,
            total_ms,
            direct: 1,
            affected: 10,
            tests: 2,
            rbuildfiles_called: false,
            bzl_closure_hits: 0,
            global,
            global_reason: why.map(str::to_string),
            engine_cold_rebuild: false,
            cold_rebuild: false,
            index_nodes: 24_519,
            index_edges: 53_242,
        }
    }

    /// A history with two cheap non-global commits and one very cheap global
    /// one, so the two latency rows must differ.
    fn history(version_used: &str) -> HistoryResult {
        let commits = vec![
            commit("1111111111111111111111111111111111111111", 40, false, None),
            commit("2222222222222222222222222222222222222222", 80, false, None),
            commit(
                "3333333333333333333333333333333333333333",
                2,
                true,
                Some("tools/build_defs/repo/git.bzl matches an invalidator"),
            ),
        ];
        let mut r = HistoryResult {
            cambi_version: None,
            schema: crate::RESULT_SCHEMA,
            repo: "bazel".into(),
            pin: "0123456789abcdef0123456789abcdef01234567".into(),
            from: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            to: "0123456789abcdef0123456789abcdef01234567".into(),
            bazel_version: String::new(),
            bazel_version_used: version_used.to_string(),
            tool_sha: "deadbee".into(),
            engine: String::new(),
            started_unix: 0,
            cold: cold(),
            commits,
            rebuild: None,
            oracle: None,
            head_index: None,
            summary: Summary::default(),
            done: true,
        };
        r.summarise();
        r
    }

    fn compare() -> ChangesResult {
        let mut counts = BTreeMap::new();
        counts.insert(Class::CambiumExtraSelect, 3);
        let mut why = BTreeMap::new();
        why.insert("rule definition changed".to_string(), 233);
        let change_set = ChangeSetResult {
            schema: crate::RESULT_SCHEMA,
            base: "b".into(),
            head: "h".into(),
            kind: crate::changes::ChangeSetKind::Adjacent,
            category: None,
            cambium: CambiumRun {
                cold_wall_ms: 4200,
                warm_wall_ms: 180,
                cold_engine_ms: 3900,
                warm_engine_ms: 95,
                ..Default::default()
            },
            tools: vec![ToolComparison {
                name: "bazel-diff-rust".into(),
                version: "v46.1.0".into(),
                labels: 12,
                cold_wall_ms: 90_000,
                warm_wall_ms: Some(45_000),
                precision: 0.9,
                depth_agreement: Some(0.8),
                dropped_file_targets: 4,
                cambium_only: 3,
                tool_only: 0,
                counts,
                examples: Vec::new(),
                cambium_only_reasons: why,
                pass: true,
                error: None,
            }],
            load_oracle: None,
            oracle: Some(crate::oracles::rdeps::RdepsResult {
                roots: vec!["//a:a.cc".into(), "//b:b.cc".into()],
                partial: true,
                partial_batches: 2,
                ..Default::default()
            }),
            recall: 1.0,
            global: false,
            pass: true,
        };
        let mut c = ChangesResult {
            cambi_version: None,
            schema: crate::RESULT_SCHEMA,
            repo: "bazel".into(),
            pin: "0123456789abcdef0123456789abcdef01234567".into(),
            bazel_version: Some("7.x".into()),
            bazel_version_used: Some("bazel 7.4.1".into()),
            seed: 0,
            oracle_k: 0,
            engine: String::new(),
            tango_change_sets: None,
            change_sets: vec![change_set, global_change_set()],
            summary: ChangesSummary::default(),
        };
        c.summarise();
        c
    }

    /// A globally invalidated change set as the driver records one: cambium answered
    /// `//...` — 24 000 rule targets — in 30 ms, and bazel-diff still ran, took
    /// 100 s and found 40 targets precisely.
    fn global_change_set() -> ChangeSetResult {
        ChangeSetResult {
            schema: crate::RESULT_SCHEMA,
            base: "gb".into(),
            head: "gh".into(),
            kind: crate::changes::ChangeSetKind::Adjacent,
            category: None,
            cambium: CambiumRun {
                global: true,
                global_reason: Some("`MODULE.bazel` matches an invalidator".into()),
                universe: 24_000,
                cold_wall_ms: 30,
                warm_wall_ms: 30,
                cold_engine_ms: 12,
                warm_engine_ms: 12,
                ..Default::default()
            },
            tools: vec![ToolComparison {
                name: "bazel-diff-rust".into(),
                version: "v46.1.0".into(),
                labels: 40,
                cold_wall_ms: 100_000,
                warm_wall_ms: Some(50_000),
                precision: 1.0,
                depth_agreement: None,
                dropped_file_targets: 0,
                cambium_only: 0,
                tool_only: 0,
                counts: BTreeMap::new(),
                examples: Vec::new(),
                cambium_only_reasons: BTreeMap::new(),
                pass: true,
                error: None,
            }],
            load_oracle: None,
            oracle: None,
            recall: 1.0,
            global: true,
            pass: true,
        }
    }

    fn bazel_results() -> SubjectResults {
        SubjectResults::new("bazel")
            .with_history(Some(history("bazel 9.0.0")))
            .with_compares(vec![compare()])
    }

    /// The whole path a `cambium-bench report` walks, with no CLI in it: write
    /// what a run writes, discover it, render it, and update the document.
    #[test]
    fn results_are_discovered_from_disk_and_rendered_into_the_doc() {
        let dir = tempfile::tempdir().unwrap();
        let subject = dir.path().join("bench-results/bazel");
        let c = compare();
        c.write(&subject.join("changes").join("changes-0123456789ab.json"))
            .unwrap();
        history("bazel 9.0.0")
            .write(&subject.join("history").join("history-0123456789ab.json"))
            .unwrap();
        // Not a result file, and not in a workload directory: neither must be
        // picked up.
        std::fs::write(subject.join("state.json"), "{}").unwrap();
        std::fs::write(subject.join("changes/notes.txt"), "hi").unwrap();

        let r = SubjectResults::load(&subject, "bazel").unwrap();
        assert!(r.history.is_some());
        assert_eq!(r.compares.len(), 1);
        assert_eq!(r.compares[0].change_sets.len(), 2);

        let doc = dir.path().join("RESULTS.md");
        std::fs::write(&doc, "# Results\n\nProse a person wrote.\n").unwrap();
        update_doc(&doc, &r.id, &render_subject_section(&r, None)).unwrap();
        update_doc(&doc, "top", &render_top_table(std::slice::from_ref(&r))).unwrap();
        let text = std::fs::read_to_string(&doc).unwrap();
        assert!(text.contains("Prose a person wrote."), "{text}");
        assert!(text.contains("<!-- bench:bazel:begin -->"), "{text}");
        assert!(text.contains("<!-- bench:top:begin -->"), "{text}");
        assert!(text.contains("`cambium_extra_select`"), "{text}");

        // Idempotent: a second report over the same results is a no-op, so a
        // nightly job that changes nothing leaves no diff to commit.
        let before = text.clone();
        update_doc(&doc, &r.id, &render_subject_section(&r, None)).unwrap();
        update_doc(&doc, "top", &render_top_table(std::slice::from_ref(&r))).unwrap();
        assert_eq!(std::fs::read_to_string(&doc).unwrap(), before);
    }

    /// A subject with no results directory at all, and one the manifest has
    /// forgotten: neither may take the report down, and the stray's numbers
    /// are still worth showing.
    #[test]
    fn collect_skips_the_absent_and_keeps_the_stray() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("bench-results");
        compare()
            .write(&parent.join("gone/changes/changes-0123456789ab.json"))
            .unwrap();
        std::fs::create_dir_all(parent.join("empty")).unwrap();
        let manifest: crate::manifest::Manifest = toml::from_str(&format!(
            "[[repo]]\nid = \"bazel\"\nurl = \"u\"\npin = \"{}\"\n",
            "a".repeat(40)
        ))
        .unwrap();
        let subjects = crate::checkout::SubjectsDir::new(dir.path().join("subjects"));
        let all = SubjectResults::collect(&manifest, &parent, &subjects).unwrap();
        let ids: Vec<&str> = all.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            ["gone"],
            "the manifest entry has no results, the stray does"
        );
    }

    #[test]
    fn the_legacy_rows_are_projected_off_the_stage_results() {
        use crate::harness::StageResult;
        let stage = |name: &str, extra: &[(&str, f64)]| StageResult {
            stage: name.into(),
            preset: "small".into(),
            spec_hash: "abc".into(),
            param: Some(10_000),
            runs: 3,
            median_ms: 12.4,
            p90_ms: 20.6,
            peak_rss_mb: 10.0,
            extra: extra.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            notes: BTreeMap::new(),
        };
        let report = BenchReport::new(vec![
            stage("walk", &[]),
            stage(crate::legacy::STAGE, &[("bazel_diff_warm_ms", 9000.0)]),
            stage(
                crate::legacy::STAGE,
                &[
                    ("targets", 50_000.0),
                    ("cambium_ms", 7.0),
                    ("cambium_p90_ms", 9.0),
                    ("bazel_diff_cold_ms", 120_000.0),
                    ("bazel_diff_warm_ms", 60_000.0),
                    ("agreed", 4321.0),
                    ("cambium_only", 3.0),
                ],
            ),
        ]);
        let rows = BenchLegacy::from_report(&report);
        assert_eq!(rows.len(), 2, "only the legacy stage");

        // A sweep records its size and timing at the top level; `extra` is
        // empty, and the fallbacks round.
        assert_eq!(rows[0].targets, 10_000);
        assert_eq!(rows[0].cambium_warm_ms, 12);
        assert_eq!(rows[0].cambium_p90_ms, 21);
        assert!(rows[0].agreed, "no disagreement recorded is agreement");

        // A plain run records both in `extra`, which wins.
        assert_eq!(rows[1].targets, 50_000);
        assert_eq!(rows[1].cambium_warm_ms, 7);
        assert_eq!(rows[1].legacy_cold_ms, 120_000);
        assert_eq!(rows[1].agreed_labels, 4321);
        assert!(!rows[1].agreed, "cambium reported three the tool did not");
    }

    #[test]
    fn a_missing_or_unreadable_bench_file_is_no_rows_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(BenchLegacy::read(&dir.path().join("nope.json")).is_empty());
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "not json").unwrap();
        assert!(BenchLegacy::read(&bad).is_empty());
    }

    #[test]
    fn a_repo_section_reports_the_pin_the_versions_and_the_classes() {
        let s = render_subject_section(&bazel_results(), None);
        assert!(s.starts_with("- pin: `0123456789ab`"), "{s}");
        assert!(s.contains("`0123456789ab`"), "the pin is abbreviated: {s}");
        assert!(s.contains("bazel-diff-rust"), "{s}");
        assert!(s.contains("| `cambium_extra_select` | 3 | no |"), "{s}");
        assert!(
            s.contains("| `cambium_missing` | 0 | **yes** |"),
            "a failing class is listed even at zero: {s}"
        );
        assert!(s.contains("45.0 s"), "walls are humanised: {s}");
        assert!(s.contains("100.0%"), "recall is a percentage: {s}");
        assert!(
            s.contains("unsoundness"),
            "the class table carries its meaning column: {s}"
        );
    }

    #[test]
    fn the_section_gives_both_latency_distributions() {
        let s = render_subject_section(&bazel_results(), None);
        assert!(s.contains("| all commits | 3 | 40 ms |"), "{s}");
        assert!(s.contains("| non-global commits | 2 | 80 ms |"), "{s}");
        assert!(
            s.contains("`tools/build_defs/repo/git.bzl` matches an invalidator | 1 |"),
            "the global histogram is bucketed by reason: {s}"
        );
        assert!(s.contains("largest dirty set: **2 packages**"), "{s}");
    }

    #[test]
    fn the_section_reports_the_bzl_load_closure_oracle() {
        // No `.bzl` in the fixture's change sets.
        let s = render_subject_section(&bazel_results(), None);
        assert!(s.contains("No change set changed a `.bzl` file."), "{s}");

        let mut r = bazel_results();
        for c in &mut r.compares {
            c.change_sets[0].load_oracle = Some(crate::oracles::load::LoadOracleResult {
                checked: vec!["bazel/defs.bzl".into(), "bazel/other.bzl".into()],
                partial: true,
                bazel_packages: ["app".to_string()].into_iter().collect(),
                ..Default::default()
            });
        }
        let s = render_subject_section(&r, None);
        assert!(s.contains("`.bzl` load-closure oracle"), "{s}");
        assert!(s.contains("| 1 | 2 | 1 | 0 |"), "{s}");
    }

    #[test]
    fn a_package_the_load_closure_missed_lands_in_the_class_table() {
        // The one class no label is filed under: it is counted per missing
        // package, straight off the oracle, and it fails the run.
        let mut r = bazel_results();
        for c in &mut r.compares {
            c.change_sets[0].load_oracle = Some(crate::oracles::load::LoadOracleResult {
                checked: vec!["bazel/defs.bzl".into()],
                bazel_packages: ["app".to_string(), "lib".to_string()].into_iter().collect(),
                missing_packages: ["lib".to_string()].into_iter().collect(),
                ..Default::default()
            });
            c.summarise();
        }
        let s = render_subject_section(&r, None);
        assert!(
            s.contains("| `cambium_missing_bzl_closure` | 1 | **yes** |"),
            "the missing package is counted under a class this binary knows: {s}"
        );
        assert!(
            !s.contains("a class this binary does not know"),
            "and not under the unknown-class fallback: {s}"
        );
    }

    #[test]
    fn the_section_reports_the_oracles_partial_batches() {
        let s = render_subject_section(&bazel_results(), None);
        assert!(s.contains("| 1 | 2 | 0 | 1 | 2 | 0 | 0 |"), "{s}");
        assert!(s.contains("rule definition changed | 233"), "{s}");
    }

    #[test]
    fn the_timings_table_gives_both_populations_and_says_which_to_quote() {
        let s = render_subject_section(&bazel_results(), None);
        assert!(
            s.contains("| `7.4.1` | **cambium** | all change sets | 2 |"),
            "the all-change-sets row covers the global too: {s}"
        );
        assert!(
            s.contains("| `7.4.1` | **cambium** | incremental | 1 |"),
            "the incremental row drops it: {s}"
        );
        assert!(
            s.contains("| `7.4.1` | bazel-diff-rust | all change sets | 2 |"),
            "and so does the tool, which ran on the global: {s}"
        );
        assert!(
            s.contains("| `7.4.1` | bazel-diff-rust | incremental | 1 |"),
            "{s}"
        );
        assert!(
            s.contains("**Quote the `all change sets` rows.**"),
            "the prose names the published number: {s}"
        );
        assert!(
            s.contains("1 where cambium fell back to `//...`"),
            "and says how many change sets that is: {s}"
        );
    }

    #[test]
    fn the_globals_table_prices_the_over_approximation() {
        let s = render_subject_section(&bazel_results(), None);
        assert!(s.contains("#### Globals"), "{s}");
        assert!(
            s.contains("| `7.4.1` | 1 | 24,000 | bazel-diff-rust | 40 | 100.0 s | 30 ms / 30 ms |"),
            "cambium's //... against the precise set, with both walls: {s}"
        );

        // A run with no global change set says so rather than printing an empty
        // table.
        let mut r = bazel_results();
        for c in &mut r.compares {
            c.change_sets.retain(|p| !p.global);
            c.summarise();
        }
        let s = render_subject_section(&r, None);
        assert!(
            s.contains("No compared change set was globally invalidated."),
            "{s}"
        );
        assert!(
            s.contains("| `7.4.1` | **cambium** | all change sets | 1 |"),
            "{s}"
        );
    }

    #[test]
    fn a_subject_with_only_a_history_still_renders() {
        let r = SubjectResults::new("envoy").with_history(Some(history("")));
        let s = render_subject_section(&r, None);
        assert!(s.contains("#### Cold build"), "{s}");
        assert!(s.contains("No comparison for this subject"), "{s}");
        assert!(
            s.contains("- Bazel: n/a"),
            "an unrecorded version is n/a: {s}"
        );
    }

    #[test]
    fn a_subject_with_only_a_comparison_still_renders() {
        let r = SubjectResults::new("bazel").with_compares(vec![compare()]);
        let s = render_subject_section(&r, None);
        assert!(s.contains("No history for this subject yet"), "{s}");
        assert!(s.contains("#### Three-way comparison"), "{s}");
    }

    #[test]
    fn a_subject_with_no_results_says_so_rather_than_panicking() {
        let r = SubjectResults::new("xla");
        let s = render_subject_section(&r, None);
        assert!(s.contains("No results yet"), "{s}");
        assert_eq!(
            render_top_table(std::slice::from_ref(&r)),
            "No subject in `bench-results/` has results yet."
        );
    }

    #[test]
    fn the_bench_row_shows_the_warm_speedup() {
        let bench = BenchLegacy {
            preset: "large".into(),
            targets: 1_000_000,
            legacy_cold_ms: 600_000,
            legacy_warm_ms: 300_000,
            cambium_cold_ms: 60_000,
            cambium_warm_ms: 300,
            cambium_p90_ms: 400,
            agreed_labels: 12,
            cambium_only: 0,
            legacy_only: 0,
            agreed: true,
        };
        let s = render_bench_section(&[bench], Some("bench-legacy.svg"));
        assert!(s.contains("1,000,000"), "{s}");
        assert!(s.contains("1000×"), "{s}");
        assert!(s.contains("2000×"), "the cold speedup is there too: {s}");
        assert!(s.contains("12 — none missed"), "{s}");
        assert!(
            s.contains("![bazel-diff vs cambium, log-log](bench-legacy.svg)"),
            "{s}"
        );
        assert!(render_bench_section(&[], None).contains("No `bench legacy` sweep"));
    }

    #[test]
    fn the_top_table_links_each_repo_to_its_section() {
        let row = top_row(&bazel_results());
        assert_eq!(
            row.change_sets, 2,
            "the global change set is a compared change set"
        );
        assert_eq!(row.failures, 0);
        assert_eq!(row.bazel_used, "7.4.1, 9.0.0");
        assert_eq!(row.history_p50_ms, 80, "non-global commits only");
        assert_eq!(row.globals, 1);
        let t = render_top_table(&[bazel_results()]);
        assert!(t.contains("[bazel](#bazel)"), "{t}");
        assert!(t.contains("`0123456789ab`"), "{t}");
        assert!(t.contains("24,519"), "{t}");
        assert!(t.contains("bazel-diff-rust 90.0%"), "{t}");
        assert!(
            t.contains("bazel-diff-rust 100.0 s vs 180 ms (556×)"),
            "tool cold against cambium warm, over all change sets: {t}"
        );
        assert!(
            t.contains("1× `tools/build_defs"),
            "the top reasons ride along: {t}"
        );
    }

    #[test]
    fn a_section_replaces_only_its_own_markers() {
        let doc = "# Results\n\nprose\n\n\
                   <!-- bench:bazel:begin -->\nold bazel\n<!-- bench:bazel:end -->\n\n\
                   <!-- bench:envoy:begin -->\nenvoy\n<!-- bench:envoy:end -->\n";
        let out = replace_section(doc, "bazel", "new bazel");
        assert!(out.contains("new bazel"));
        assert!(!out.contains("old bazel"));
        assert!(out.contains("envoy"), "the neighbour survives: {out}");
        assert!(out.starts_with("# Results\n\nprose\n"), "{out}");
        assert_eq!(out.matches("<!-- bench:bazel:begin -->").count(), 1);
    }

    #[test]
    fn the_blank_line_after_a_region_survives() {
        let doc = "<!-- bench:bazel:begin -->\nold\n<!-- bench:bazel:end -->\n\n## Findings\n";
        let out = replace_section(doc, "bazel", "new");
        assert!(
            out.ends_with("<!-- bench:bazel:end -->\n\n## Findings\n"),
            "the heading below keeps its blank line: {out:?}"
        );
    }

    #[test]
    fn a_hand_written_findings_section_survives_a_rerun() {
        let doc = "## Findings\n\nThe bzl class dominates.\n\n\
                   <!-- bench:bazel:begin -->\nold\n<!-- bench:bazel:end -->\n";
        let out = replace_section(doc, "bazel", "new");
        assert!(out.contains("The bzl class dominates."), "{out}");
    }

    #[test]
    fn an_absent_section_is_appended_with_its_markers() {
        let out = replace_section("# Results\n", "bazel", "hello");
        assert_eq!(
            out,
            "# Results\n\n<!-- bench:bazel:begin -->\nhello\n<!-- bench:bazel:end -->\n"
        );
        // Idempotent: rendering the same section again changes nothing.
        assert_eq!(replace_section(&out, "bazel", "hello"), out);
    }

    #[test]
    fn a_begin_marker_without_an_end_does_not_eat_the_file() {
        let doc = "# Results\n<!-- bench:bazel:begin -->\nhalf written\n";
        let out = replace_section(doc, "bazel", "new");
        assert!(out.contains("half written"), "{out}");
        assert!(out.ends_with("<!-- bench:bazel:end -->\n"), "{out}");
    }

    #[test]
    fn updating_a_missing_doc_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docs/RESULTS.md");
        update_doc(&path, "bazel", "body").unwrap();
        let s = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            s,
            "<!-- bench:bazel:begin -->\nbody\n<!-- bench:bazel:end -->\n"
        );
        update_doc(&path, "envoy", "other").unwrap();
        let s = std::fs::read_to_string(&path).unwrap();
        assert!(s.contains("body") && s.contains("other"), "{s}");
    }

    #[test]
    fn the_generated_date_is_todays() {
        let d = today();
        assert_eq!(d.len(), 10, "{d}");
        assert!(d.starts_with("20"), "{d}");
        assert_eq!(civil_from_days(719_468 + 20_000), (2024, 10, 4));
    }

    /// The rendered Markdown is pinned byte for byte, the date line aside:
    /// the section is generated into a document people read and diff, so a
    /// refactor of the numbers behind it must not move a character of it.
    fn rendered(r: &SubjectResults) -> String {
        render_subject_section(r, None)
            .lines()
            .map(|l| {
                if l.starts_with("- generated: ") {
                    "- generated: <today>".to_string()
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    #[test]
    fn the_section_renders_exactly_as_it_did() {
        assert_eq!(
            rendered(&bazel_results()),
            include_str!("report/snapshots/bazel.md")
        );
        let mut r = bazel_results();
        for c in &mut r.compares {
            c.change_sets[0].load_oracle = Some(crate::oracles::load::LoadOracleResult {
                checked: vec!["bazel/defs.bzl".into()],
                bazel_packages: ["app".to_string(), "lib".to_string()].into_iter().collect(),
                missing_packages: ["lib".to_string()].into_iter().collect(),
                ..Default::default()
            });
            c.summarise();
        }
        assert_eq!(rendered(&r), include_str!("report/snapshots/bazel-load.md"));
        let mut r = bazel_results();
        for c in &mut r.compares {
            c.change_sets.retain(|p| !p.global);
            c.summarise();
        }
        assert_eq!(
            rendered(&r),
            include_str!("report/snapshots/bazel-no-global.md")
        );
        assert_eq!(
            render_top_table(&[bazel_results(), SubjectResults::new("xla")]),
            include_str!("report/snapshots/top.md")
        );
    }

    /// The numbers the section prints, as the typed model carries them.
    #[test]
    fn the_comparison_section_is_computed_before_it_is_rendered() {
        let c = CompareSection::compute(&bazel_results());
        assert_eq!((c.change_sets, c.passed, c.failed, c.globals), (2, 2, 0, 1));
        assert_eq!(c.min_recall, Some(1.0));
        assert_eq!(c.soundness.len(), 1);
        assert_eq!(c.soundness[0].tool.as_deref(), Some("bazel-diff-rust"));
        assert_eq!(c.soundness[0].precision, Some(0.9));

        // The tool over both scopes, then cambium over both.
        let rows: Vec<(Option<&str>, Scope, usize)> = c
            .timings
            .iter()
            .map(|t| (t.tool.as_deref(), t.scope, t.change_sets))
            .collect();
        assert_eq!(
            rows,
            [
                (Some("bazel-diff-rust"), Scope::All, 2),
                (Some("bazel-diff-rust"), Scope::Incremental, 1),
                (None, Scope::All, 2),
                (None, Scope::Incremental, 1),
            ]
        );
        assert_eq!(c.timings[1].cold.as_ref().map(|w| w.p50_ms), Some(90_000));
        assert_eq!(c.cold_rebuild_change_sets, 0);

        assert_eq!(c.globals_table.len(), 1);
        let g = &c.globals_table[0];
        assert_eq!(
            (g.change_sets, g.cambium_labels, g.tool_labels),
            (1, 24_000, Some(40))
        );
        assert_eq!(g.tool_cold_p50_ms, Some(100_000));
        assert_eq!((g.cambium_cold_p50_ms, g.cambium_warm_p50_ms), (30, 30));

        assert_eq!(
            c.classes.len(),
            Class::ALL.len(),
            "every class, at zero if it never fired"
        );
        let select = c
            .classes
            .iter()
            .find(|t| t.class == Class::CambiumExtraSelect)
            .unwrap();
        assert_eq!(select.labels, 3);
        assert_eq!(
            c.reasons,
            [ReasonRow {
                tool: "bazel-diff-rust".into(),
                reason: "rule definition changed".into(),
                labels: 233
            }]
        );
        assert_eq!(c.tool_notes.len(), 1);
        assert_eq!(c.tool_notes[0].dropped_file_targets, 4);
        assert_eq!(
            c.tool_notes[0].depth,
            Some(DepthAgreement {
                mean: 0.8,
                change_sets: 1
            })
        );

        assert_eq!(c.rdeps.change_sets, 1);
        assert_eq!(c.rdeps.roots, 2);
        assert_eq!(
            (c.rdeps.partial_change_sets, c.rdeps.partial_batches),
            (1, 2)
        );
        assert_eq!(c.rdeps.missing, 0);
        assert!(c.load.is_empty());
    }

    /// The reason rows come per tool in name order and most labels first
    /// within a tool; a tool with no distances has no depth note.
    #[test]
    fn reasons_are_ordered_and_a_tool_without_distances_has_no_depth() {
        let mut r = bazel_results();
        let mut tango = r.compares[0].change_sets[0].tools[0].clone();
        tango.name = "tango".into();
        tango.depth_agreement = None;
        tango.cambium_only_reasons.clear();
        tango.cambium_only_reasons.insert("a".into(), 1);
        tango.cambium_only_reasons.insert("b".into(), 5);
        tango.cambium_only_reasons.insert("c".into(), 5);
        r.compares[0].change_sets[0].tools.push(tango);
        r.compares[0].summarise();
        let c = CompareSection::compute(&r);
        let rows: Vec<(&str, &str, usize)> = c
            .reasons
            .iter()
            .map(|x| (x.tool.as_str(), x.reason.as_str(), x.labels))
            .collect();
        assert_eq!(
            rows,
            [
                ("bazel-diff-rust", "rule definition changed", 233),
                ("tango", "b", 5),
                ("tango", "c", 5),
                ("tango", "a", 1),
            ]
        );
        let tango = c.tool_notes.iter().find(|n| n.tool == "tango").unwrap();
        assert!(tango.depth.is_none());
        assert!(c.rdeps.roots == 2 && !c.rdeps.is_empty());
    }
}
