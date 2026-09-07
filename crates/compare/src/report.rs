//! `docs/CORPUS.md`, generated from the run's JSON (spec §11).
//!
//! The document is written by hand *around* generated blocks: each repository
//! owns a region delimited by `<!-- corpus:<id>:begin -->` and
//! `<!-- corpus:<id>:end -->`, and a run replaces the text between its own
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
//! is unit-testable and a formatting change never requires a corpus run. It is
//! also written to survive *partial* data: a repository with a replay and no
//! comparison, a comparison whose producers all failed, a result file written
//! by an older binary that has no `bazel_version_used`. Every such hole
//! renders as `n/a` or a one-line note, never as a panic or a missing table.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context as _, Result};
use cambium_corpus::replay::CommitRecord;
use cambium_corpus::ReplayResult;
use serde::{Deserialize, Serialize};

use crate::threeway::{CompareResult, WallStats};

/// `bench legacy`'s headline numbers at one graph size.
///
/// Declared here rather than imported from `cambium-bench` so the reporting
/// layer does not depend on the benchmark harness; the CLI deserialises the
/// bench JSON into this.
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

/// One repository's results: what `corpus-results/<id>/` held, plus the one
/// manifest fact the JSON does not carry, its id.
#[derive(Clone, Debug, Default)]
pub struct RepoResults {
    pub id: String,
    pub replay: Option<ReplayResult>,
    pub compares: Vec<CompareResult>,
}

impl RepoResults {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            replay: None,
            compares: Vec::new(),
        }
    }

    pub fn with_replay(mut self, r: Option<ReplayResult>) -> Self {
        self.replay = r;
        self
    }

    pub fn with_compares(mut self, c: Vec<CompareResult>) -> Self {
        self.compares = c;
        self
    }

    /// The profile these results came from, when any of them says. Results
    /// written before profiles existed say nothing, and were full runs.
    pub fn profile(&self) -> &str {
        if let Some(p) = self
            .compares
            .iter()
            .map(|c| c.profile.as_str())
            .chain(self.replay.iter().map(|r| r.profile.as_str()))
            .find(|p| !p.is_empty())
        {
            return p;
        }
        "full"
    }

    pub fn is_empty(&self) -> bool {
        self.replay.is_none() && self.compares.is_empty()
    }

    fn pin(&self) -> String {
        self.replay
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
        if let Some(r) = &self.replay {
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
        if let Some(r) = &self.replay {
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
        self.replay.as_ref().map(|r| &r.commits[..]).unwrap_or(&[])
    }

    /// Global commits and why, most common first. From the replay when it ran
    /// any commits, else from the comparison pairs, which see the same
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
                for p in self.pairs(c).filter(|p| p.global) {
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

    fn pairs<'a>(
        &self,
        c: &'a CompareResult,
    ) -> impl Iterator<Item = &'a crate::threeway::PairComparison> {
        c.pairs.iter()
    }

    fn pairs_total(&self) -> usize {
        self.compares.iter().map(|c| c.summary.pairs).sum()
    }

    fn failures(&self) -> usize {
        self.compares
            .iter()
            .map(|c| c.summary.pairs - c.summary.passed)
            .sum()
    }

    fn min_recall(&self) -> Option<f64> {
        self.compares
            .iter()
            .filter(|c| c.summary.pairs > 0)
            .map(|c| c.summary.min_recall)
            .fold(None, |acc, r| Some(acc.map_or(r, |a: f64| a.min(r))))
    }

    /// Every producer named anywhere in this repository's comparisons.
    fn producers(&self) -> Vec<String> {
        let mut names: BTreeSet<String> = BTreeSet::new();
        for c in &self.compares {
            for p in &c.pairs {
                for pr in &p.producers {
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

    fn producer_cold_p50(&self, name: &str) -> Option<u64> {
        let vals: Vec<u64> = self
            .compares
            .iter()
            .filter_map(|c| c.summary.producer_cold.get(name))
            .filter(|w| w.samples > 0)
            .map(|w| w.p50_ms)
            .collect();
        vals.iter().copied().max()
    }

    fn cambium_warm_p50(&self) -> Option<u64> {
        let vals: Vec<u64> = self
            .compares
            .iter()
            .map(|c| &c.summary.cambium_warm)
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
    pub replay_p50_ms: u64,
    pub replay_p90_ms: u64,
    pub replay_commits: usize,
    pub globals: usize,
    pub global_reasons: Vec<(String, usize)>,
    pub pairs: usize,
    pub failures: usize,
    pub min_recall: Option<f64>,
    /// Mean precision per producer.
    pub precision: BTreeMap<String, f64>,
    /// Producer cold p50 per producer, against cambium's warm p50.
    pub producer_cold_p50: BTreeMap<String, u64>,
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
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
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

fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() as f64 * q) as usize).min(sorted.len() - 1)]
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
pub fn top_row(r: &RepoResults) -> TopRow {
    let non_global = latency(r.commits().iter().filter(|c| !c.global).map(|c| c.total_ms));
    let (globals, reasons) = r.globals();
    let mut row = TopRow {
        repo: r.id.clone(),
        pin: r.pin(),
        bazel_used: r.bazel_used(),
        replay_p50_ms: non_global.p50,
        replay_p90_ms: non_global.p90,
        replay_commits: non_global.n,
        globals,
        global_reasons: reasons.into_iter().take(3).collect(),
        pairs: r.pairs_total(),
        failures: r.failures(),
        min_recall: r.min_recall(),
        cambium_warm_p50: r.cambium_warm_p50(),
        ..Default::default()
    };
    if let Some(rep) = &r.replay {
        row.targets = rep.cold.nodes;
        row.cold_build_s = rep.cold.wall_ms as f64 / 1000.0;
    }
    for name in r.producers() {
        if let Some(p) = r.mean_precision(&name) {
            row.precision.insert(name.clone(), p);
        }
        if let Some(c) = r.producer_cold_p50(&name) {
            row.producer_cold_p50.insert(name, c);
        }
    }
    row
}

/// The table at the top of the document: one line per repository that has
/// results at all.
pub fn render_top_table(repos: &[RepoResults]) -> String {
    let rows: Vec<TopRow> = repos
        .iter()
        .filter(|r| !r.is_empty())
        .map(top_row)
        .collect();
    if rows.is_empty() {
        return "No repository in `corpus-results/` has results yet.".to_string();
    }
    let mut s = String::new();
    s.push_str(
        "| repo | pin | Bazel | targets | cold build | replay p50/p90 | globals | pairs | \
         pass/fail | min recall | precision | producer cold p50 vs cambium warm p50 |\n",
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
            &r.producer_cold_p50
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
            ms(r.replay_p50_ms),
            ms(r.replay_p90_ms),
            globals,
            r.pairs,
            format_args!("{} / {}", r.pairs - r.failures, r.failures),
            opt_pct(r.min_recall),
            precision,
            speed,
        ));
    }
    s.push_str(
        "\nReplay p50/p90 are over **non-global** commits (the ones an updater \
         actually walks); the per-repo sections give both distributions. \
         `pass/fail` counts compared pairs. Precision against a producer is \
         informational; recall against the `rdeps` oracle is the gate.\n",
    );
    s
}

// ---------------------------------------------------------- per-repo section

/// The markdown for one repository: everything a reader needs to judge the
/// claim, in the order they will ask for it — what was measured, whether it
/// was sound, then how fast.
pub fn render_repo_section(r: &RepoResults, bench: Option<&BenchLegacy>) -> String {
    // No heading of its own: the document hand-writes `## <id>` above each
    // marked region, and that heading is what the top table links to. A
    // generated duplicate would give the anchor a `-1` suffix and break the
    // link, so the section starts at the facts.
    let mut s = String::new();
    s.push_str(&format!("- pin: `{}`\n", short(&r.pin())));
    s.push_str(&format!("- Bazel: {}\n", r.bazel_used()));
    s.push_str(&format!("- generated: {}\n", today()));
    if r.profile() != "full" {
        // Loudly, and before the tables: quick numbers are a laptop's slice of
        // the corpus, not the published run.
        s.push_str(&format!(
            "- profile: **{}** — a bounded local slice, not the published \
             numbers\n",
            r.profile()
        ));
    }
    if let Some(rep) = &r.replay {
        s.push_str(&format!("- tool: `{}`\n", rep.tool_sha));
    }
    if r.is_empty() {
        s.push_str(
            "\nNo results yet: neither `replay-*.json` nor `compare-*.json` \
                    was found for this repository.\n",
        );
        return s;
    }
    s.push('\n');

    if let Some(rep) = &r.replay {
        s.push_str(&render_replay(rep));
    } else {
        s.push_str(
            "_No replay for this repository yet; the cold build and \
                    updater-latency tables below are omitted._\n\n",
        );
    }

    if !r.compares.is_empty() {
        s.push_str(&render_compare(r));
    } else {
        s.push_str(
            "_No three-way comparison for this repository yet: no \
                    `compare-*.json`, so there are no precision, recall or \
                    disagreement numbers._\n\n",
        );
    }

    if let Some(b) = bench {
        s.push_str("#### bazel-diff on a synthetic graph of this size (`bench legacy`)\n\n");
        s.push_str(&render_bench_table(std::slice::from_ref(b)));
    }
    s
}

fn render_replay(rep: &ReplayResult) -> String {
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
        "#### Replay ({} commits, `{}..{}`)\n\n",
        u.commits,
        short(&rep.from),
        short(&rep.to)
    ));
    if !rep.done {
        s.push_str(
            "_This replay has not finished; the numbers below cover the \
                    commits it has replayed so far._\n\n",
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

fn render_compare(r: &RepoResults) -> String {
    let mut s = String::new();
    let compares = &r.compares;

    // ---- soundness first ----
    s.push_str("#### Three-way comparison\n\n");
    let pairs = r.pairs_total();
    let globals: usize = compares.iter().map(|c| c.summary.globals).sum();
    s.push_str(&format!(
        "{pairs} pairs compared, {} passed, {} failed ({globals} of them global, \
         which pass by construction). Worst oracle recall: **{}**.\n\n",
        pairs - r.failures(),
        r.failures(),
        opt_pct(r.min_recall()),
    ));

    s.push_str("| Bazel | producer | pairs | passed | precision | min recall |\n");
    s.push_str("|---|---|---:|---:|---:|---:|\n");
    for c in compares {
        let ver = c
            .bazel_version_used
            .as_deref()
            .map(tidy_version)
            .or_else(|| c.bazel_version.clone())
            .unwrap_or_else(|| "n/a".into());
        let names = producer_names(c);
        if names.is_empty() {
            s.push_str(&format!(
                "| `{ver}` | _none ran_ | {} | {} | n/a | {} |\n",
                c.summary.pairs,
                c.summary.passed,
                pct(c.summary.min_recall)
            ));
        }
        for name in names {
            s.push_str(&format!(
                "| `{ver}` | {name} | {} | {} | {} | {} |\n",
                c.summary.pairs,
                c.summary.passed,
                c.summary
                    .mean_precision
                    .get(&name)
                    .map(|p| pct(*p))
                    .unwrap_or_else(|| "n/a".into()),
                pct(c.summary.min_recall),
            ));
        }
    }
    s.push('\n');

    // ---- timings ----
    s.push_str("#### Timings (p50 / p90 over the compared pairs)\n\n");
    s.push_str("| Bazel | tool | pairs | cold | warm |\n|---|---|---:|---:|---:|\n");
    for c in compares {
        let ver = c
            .bazel_version_used
            .as_deref()
            .map(tidy_version)
            .or_else(|| c.bazel_version.clone())
            .unwrap_or_else(|| "n/a".into());
        for name in producer_names(c) {
            // Per producer, because Tango is capped under the quick profile:
            // a p50 over 10 pairs and one over 50 are different claims and the
            // table has to say which is which.
            let ran = c
                .summary
                .producer_pairs
                .get(&name)
                .copied()
                .or_else(|| c.summary.producer_cold.get(&name).map(|w| w.samples))
                .unwrap_or(0);
            s.push_str(&format!(
                "| `{ver}` | {name} | {ran} | {} | {} |\n",
                opt_wall(c.summary.producer_cold.get(&name)),
                opt_wall(c.summary.producer_warm.get(&name)),
            ));
        }
        s.push_str(&format!(
            "| `{ver}` | **cambium** | {} | {} | {} |\n",
            c.summary.cambium_cold.samples,
            wall(&c.summary.cambium_cold),
            wall(&c.summary.cambium_warm),
        ));
    }
    s.push_str(
        "\ncambium's *cold* is a full index build from `bazel query //...`; its \
         *warm* is an update against the published baseline, which is what a \
         CI job actually pays. bazel-diff's warm reuses the base revision's \
         hashes; Tango has no warm mode, so its cell is `n/a`. *pairs* is how \
         many pairs each tool's distribution is taken over: under the `quick` \
         profile Tango runs on the first 10 pairs only, while cambium and \
         bazel-diff run on all of them.\n\n",
    );

    // ---- classes ----
    s.push_str("#### Disagreements by class\n\n");
    let mut totals: BTreeMap<String, usize> = BTreeMap::new();
    for c in compares {
        for (k, v) in &c.summary.by_class {
            *totals.entry(k.clone()).or_default() += v;
        }
    }
    s.push_str("| class | labels | fails the run | meaning |\n|---|---:|---|---|\n");
    for class in crate::classify::Class::ALL {
        let n = totals.get(class.name()).copied().unwrap_or(0);
        s.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            class.name(),
            num(n as u64),
            if class.is_failure() { "**yes**" } else { "no" },
            class.meaning(),
        ));
    }
    for (class, n) in &totals {
        if !crate::classify::Class::ALL
            .iter()
            .any(|c| c.name() == class.as_str())
        {
            s.push_str(&format!(
                "| `{class}` | {n} | unknown | a class this binary does not know; \
                 the results were written by a different version |\n"
            ));
        }
    }
    s.push_str("\nCounts are labels, summed over every pair and producer.\n\n");

    // ---- why cambium reported the extras ----
    let mut reasons: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut depth: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    let mut dropped: BTreeMap<String, usize> = BTreeMap::new();
    for c in compares {
        for pair in &c.pairs {
            for pr in &pair.producers {
                let e = reasons.entry(pr.name.clone()).or_default();
                for (k, v) in &pr.cambium_only_reasons {
                    *e.entry(k.clone()).or_default() += v;
                }
                *dropped.entry(pr.name.clone()).or_default() += pr.dropped_file_targets;
                if let Some(d) = pr.depth_agreement {
                    let slot = depth.entry(pr.name.clone()).or_default();
                    slot.0 += d;
                    slot.1 += 1;
                }
            }
        }
    }
    if reasons.values().any(|m| !m.is_empty()) {
        s.push_str("#### Why cambium reported the labels the producer did not\n\n");
        s.push_str("| producer | cambium's reason | labels |\n|---|---|---:|\n");
        for (name, m) in &reasons {
            let mut v: Vec<(&String, &usize)> = m.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            for (why, n) in v {
                s.push_str(&format!("| {name} | {why} | {} |\n", num(*n as u64)));
            }
        }
        s.push_str(
            "\nThese are cambium's *own* explanations for the extra labels \
             (`affected --explain`), not the classifier's: they say what made \
             cambium reach the target, where the class table says why the \
             disagreement is benign.\n\n",
        );
    }

    // Two numbers that belong to the producers rather than to a class.
    if !dropped.is_empty() {
        for (name, n) in &dropped {
            let agree = match depth.get(name) {
                Some((sum, count)) if *count > 0 => format!(
                    "depth agrees on {} of shared labels ({count} {})",
                    pct(sum / *count as f64),
                    if *count == 1 { "pair" } else { "pairs" }
                ),
                _ => "no distances reported".to_string(),
            };
            s.push_str(&format!(
                "- `{name}`: {agree}; {} file-target rows dropped to match \
                 cambium's universe.\n",
                num(*n as u64)
            ));
        }
        s.push('\n');
    }

    // ---- the oracle ----
    let mut with_oracle = 0usize;
    let mut roots = 0usize;
    let mut partial_pairs = 0usize;
    let mut partial_batches = 0usize;
    let mut skipped_roots = 0usize;
    let mut missing = 0usize;
    let mut missing_external = 0usize;
    let mut external_route = 0usize;
    for c in compares {
        for p in &c.pairs {
            if let Some(o) = &p.oracle {
                with_oracle += 1;
                roots += o.roots.len();
                skipped_roots += o.skipped_roots.len();
                missing += o.missing.len();
                missing_external += o.missing_external.len();
                external_route += o.external_route.len();
                if o.partial {
                    partial_pairs += 1;
                    partial_batches += o.partial_batches;
                }
            }
        }
    }
    s.push_str("#### `rdeps` oracle\n\n");
    if with_oracle == 0 {
        s.push_str("No pair ran the oracle (`--oracle-sample 0`, or every pair was global).\n\n");
    } else {
        s.push_str("| pairs with an oracle | roots sampled | roots skipped | partial pairs | partial batches | missing labels | of those, external route | external labels not counted |\n");
        s.push_str("|---:|---:|---:|---:|---:|---:|---:|---:|\n");
        s.push_str(&format!(
            "| {with_oracle} | {roots} | {skipped_roots} | {partial_pairs} | {partial_batches} | {missing} | {external_route} | {missing_external} |\n\n"
        ));
        s.push_str(
            "A *partial* batch is a `--keep_going` query that exited 3: Bazel \
             answered for what loaded. The oracle is a lower bound on what \
             cambium must contain, so a partial answer is still sound to check \
             against — it just proves less. `missing labels` is the count that \
             gates the run, and must be zero. *external route* is the subset \
             `somepath` shows Bazel reaching only by leaving `//...` and coming \
             back (see [Externals and soundness](#externals-and-soundness)); it \
             is a label on a failure, not an excuse for one. The last column is \
             the `@repo//...` half of the difference, which cambium does not \
             index by construction and which recall therefore ignores.\n\n",
        );
    }

    // ---- the .bzl load-closure oracle ----
    let mut bzl = crate::bzloracle::BzlOracleSummary::default();
    for c in compares {
        for p in &c.pairs {
            if let Some(o) = &p.bzl_oracle {
                bzl.add(o);
            }
        }
    }
    s.push_str("#### `.bzl` load-closure oracle\n\n");
    if bzl.is_empty() {
        s.push_str("No pair changed a `.bzl` file.\n\n");
    } else {
        s.push_str(
            "| pairs with a `.bzl` change | `.bzl` files checked | partial answers | packages cambium missed |\n",
        );
        s.push_str("|---:|---:|---:|---:|\n");
        s.push_str(&format!(
            "| {} | {} | {} | {} |\n\n",
            bzl.pairs, bzl.checked, bzl.partial, bzl.missing_packages
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
             repository and the closure loads none.\n\n",
        );
    }

    s
}

fn producer_names(c: &CompareResult) -> Vec<String> {
    let names: BTreeSet<String> = c
        .pairs
        .iter()
        .flat_map(|p| p.producers.iter().map(|pr| pr.name.clone()))
        .collect();
    names.into_iter().collect()
}

// --------------------------------------------------------- synthetic scaling

/// The `bench legacy` sweep: the headline number, on graphs no real repository
/// reaches, next to the chart the same sweep plots.
pub fn render_bench_section(rows: &[BenchLegacy], chart: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str(
        "These are *synthetic* graphs (`cambi bench sweep --stage \
         legacy_bazel_diff`), not the corpus above: cambium and bazel-diff on \
         the same generated workspace, the same change, and a `bazel` shim that \
         replays a recorded query, so what is measured is each tool's own work \
         rather than a Bazel server. See [BENCH.md](BENCH.md#legacy_bazel_diff-like-for-like-against-bazel-diff).\n\n",
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

pub fn begin_marker(repo_id: &str) -> String {
    format!("<!-- corpus:{repo_id}:begin -->")
}

pub fn end_marker(repo_id: &str) -> String {
    format!("<!-- corpus:{repo_id}:end -->")
}

/// Replaces `repo_id`'s region of `doc` with `section`, appending the region
/// (markers and all) when it is not there yet.
///
/// A begin marker with no end marker is treated as absent rather than as a
/// licence to eat the rest of the file.
pub fn replace_section(doc: &str, repo_id: &str, section: &str) -> String {
    let (begin, end) = (begin_marker(repo_id), end_marker(repo_id));
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
pub fn update_doc(path: &Path, repo_id: &str, section: &str) -> Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let updated = replace_section(&existing, repo_id, section);
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
    use crate::threeway::{CambiumRun, CompareSummary, PairComparison, ProducerComparison};
    use cambium_corpus::replay::{ColdBuild, Summary};

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

    /// A replay with two cheap non-global commits and one very cheap global
    /// one, so the two latency rows must differ.
    fn replay(version_used: &str) -> ReplayResult {
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
        let mut r = ReplayResult {
            schema: 3,
            repo: "bazel".into(),
            pin: "0123456789abcdef0123456789abcdef01234567".into(),
            from: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            to: "0123456789abcdef0123456789abcdef01234567".into(),
            bazel_version: String::new(),
            profile: "full".into(),
            bazel_version_used: version_used.to_string(),
            tool_sha: "deadbee".into(),
            started_unix: 0,
            cold: cold(),
            commits,
            rebuild: None,
            oracle: None,
            summary: Summary::default(),
            done: true,
        };
        r.summarise();
        r
    }

    fn compare() -> CompareResult {
        let mut counts = BTreeMap::new();
        counts.insert("cambium_extra_select".to_string(), 3);
        let mut why = BTreeMap::new();
        why.insert("rule definition changed".to_string(), 233);
        let pair = PairComparison {
            schema: 3,
            base: "b".into(),
            head: "h".into(),
            kind: "adjacent".into(),
            category: None,
            cambium: CambiumRun {
                cold_wall_ms: 4200,
                warm_wall_ms: 180,
                ..Default::default()
            },
            producers: vec![ProducerComparison {
                name: "bazel-diff-rust".into(),
                version: "v46.1.0".into(),
                labels: 12,
                cold_wall_ms: 90_000,
                warm_wall_ms: Some(45_000),
                precision: 0.9,
                depth_agreement: Some(0.8),
                dropped_file_targets: 4,
                cambium_only: 3,
                producer_only: 0,
                counts,
                examples: Vec::new(),
                cambium_only_reasons: why,
                pass: true,
                error: None,
            }],
            bzl_oracle: None,
            oracle: Some(crate::oracle::OracleResult {
                roots: vec!["//a:a".into(), "//b:b".into()],
                oracle_set: Default::default(),
                cambium_set_from_roots: Default::default(),
                missing: Vec::new(),
                missing_external: Vec::new(),
                external_route: Vec::new(),
                skipped_roots: Vec::new(),
                partial: true,
                partial_batches: 2,
            }),
            recall: 1.0,
            global: false,
            pass: true,
        };
        let mut c = CompareResult {
            schema: 3,
            repo: "bazel".into(),
            pin: "0123456789abcdef0123456789abcdef01234567".into(),
            bazel_version: Some("7.x".into()),
            bazel_version_used: Some("bazel 7.4.1".into()),
            seed: 0,
            oracle_k: 100,
            profile: "full".into(),
            pairs: vec![pair],
            summary: CompareSummary::default(),
        };
        c.summarise();
        c
    }

    fn bazel_results() -> RepoResults {
        RepoResults::new("bazel")
            .with_replay(Some(replay("bazel 9.0.0")))
            .with_compares(vec![compare()])
    }

    #[test]
    fn a_repo_section_reports_the_pin_the_versions_and_the_classes() {
        let s = render_repo_section(&bazel_results(), None);
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
        let s = render_repo_section(&bazel_results(), None);
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
        let s = render_repo_section(&bazel_results(), None);
        assert!(s.contains("No pair changed a `.bzl` file."), "{s}");

        let mut r = bazel_results();
        for c in &mut r.compares {
            c.pairs[0].bzl_oracle = Some(crate::bzloracle::BzlOracleResult {
                checked: vec!["bazel/defs.bzl".into(), "bazel/other.bzl".into()],
                partial: true,
                bazel_packages: ["app".to_string()].into_iter().collect(),
                ..Default::default()
            });
        }
        let s = render_repo_section(&r, None);
        assert!(s.contains("`.bzl` load-closure oracle"), "{s}");
        assert!(s.contains("| 1 | 2 | 1 | 0 |"), "{s}");
    }

    #[test]
    fn the_section_reports_the_oracles_partial_batches() {
        let s = render_repo_section(&bazel_results(), None);
        assert!(s.contains("| 1 | 2 | 0 | 1 | 2 | 0 |"), "{s}");
        assert!(s.contains("rule definition changed | 233"), "{s}");
    }

    #[test]
    fn a_repo_with_only_a_replay_still_renders() {
        let r = RepoResults::new("envoy").with_replay(Some(replay("")));
        let s = render_repo_section(&r, None);
        assert!(s.contains("#### Cold build"), "{s}");
        assert!(s.contains("No three-way comparison"), "{s}");
        assert!(
            s.contains("- Bazel: n/a"),
            "an unrecorded version is n/a: {s}"
        );
    }

    #[test]
    fn a_repo_with_only_a_comparison_still_renders() {
        let r = RepoResults::new("bazel").with_compares(vec![compare()]);
        let s = render_repo_section(&r, None);
        assert!(s.contains("No replay for this repository yet"), "{s}");
        assert!(s.contains("#### Three-way comparison"), "{s}");
    }

    #[test]
    fn a_repo_with_no_results_says_so_rather_than_panicking() {
        let r = RepoResults::new("xla");
        let s = render_repo_section(&r, None);
        assert!(s.contains("No results yet"), "{s}");
        assert_eq!(
            render_top_table(std::slice::from_ref(&r)),
            "No repository in `corpus-results/` has results yet."
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
        assert_eq!(row.pairs, 1);
        assert_eq!(row.failures, 0);
        assert_eq!(row.bazel_used, "7.4.1, 9.0.0");
        assert_eq!(row.replay_p50_ms, 80, "non-global commits only");
        assert_eq!(row.globals, 1);
        let t = render_top_table(&[bazel_results()]);
        assert!(t.contains("[bazel](#bazel)"), "{t}");
        assert!(t.contains("`0123456789ab`"), "{t}");
        assert!(t.contains("24,519"), "{t}");
        assert!(t.contains("bazel-diff-rust 90.0%"), "{t}");
        assert!(
            t.contains("bazel-diff-rust 90.0 s vs 180 ms (500×)"),
            "producer cold against cambium warm: {t}"
        );
        assert!(
            t.contains("1× `tools/build_defs"),
            "the top reasons ride along: {t}"
        );
    }

    #[test]
    fn a_section_replaces_only_its_own_markers() {
        let doc = "# Corpus\n\nprose\n\n\
                   <!-- corpus:bazel:begin -->\nold bazel\n<!-- corpus:bazel:end -->\n\n\
                   <!-- corpus:envoy:begin -->\nenvoy\n<!-- corpus:envoy:end -->\n";
        let out = replace_section(doc, "bazel", "new bazel");
        assert!(out.contains("new bazel"));
        assert!(!out.contains("old bazel"));
        assert!(out.contains("envoy"), "the neighbour survives: {out}");
        assert!(out.starts_with("# Corpus\n\nprose\n"), "{out}");
        assert_eq!(out.matches("<!-- corpus:bazel:begin -->").count(), 1);
    }

    #[test]
    fn the_blank_line_after_a_region_survives() {
        let doc = "<!-- corpus:bazel:begin -->\nold\n<!-- corpus:bazel:end -->\n\n## Findings\n";
        let out = replace_section(doc, "bazel", "new");
        assert!(
            out.ends_with("<!-- corpus:bazel:end -->\n\n## Findings\n"),
            "the heading below keeps its blank line: {out:?}"
        );
    }

    #[test]
    fn a_hand_written_findings_section_survives_a_rerun() {
        let doc = "## Findings\n\nThe bzl class dominates.\n\n\
                   <!-- corpus:bazel:begin -->\nold\n<!-- corpus:bazel:end -->\n";
        let out = replace_section(doc, "bazel", "new");
        assert!(out.contains("The bzl class dominates."), "{out}");
    }

    #[test]
    fn an_absent_section_is_appended_with_its_markers() {
        let out = replace_section("# Corpus\n", "bazel", "hello");
        assert_eq!(
            out,
            "# Corpus\n\n<!-- corpus:bazel:begin -->\nhello\n<!-- corpus:bazel:end -->\n"
        );
        // Idempotent: rendering the same section again changes nothing.
        assert_eq!(replace_section(&out, "bazel", "hello"), out);
    }

    #[test]
    fn a_begin_marker_without_an_end_does_not_eat_the_file() {
        let doc = "# Corpus\n<!-- corpus:bazel:begin -->\nhalf written\n";
        let out = replace_section(doc, "bazel", "new");
        assert!(out.contains("half written"), "{out}");
        assert!(out.ends_with("<!-- corpus:bazel:end -->\n"), "{out}");
    }

    #[test]
    fn updating_a_missing_doc_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docs/CORPUS.md");
        update_doc(&path, "bazel", "body").unwrap();
        let s = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            s,
            "<!-- corpus:bazel:begin -->\nbody\n<!-- corpus:bazel:end -->\n"
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
}
