//! `docs/CORPUS.md`, generated from the run's JSON (spec §11).
//!
//! The document is written by hand *around* generated blocks: each repository
//! owns a region delimited by `<!-- corpus:<id>:begin -->` and
//! `<!-- corpus:<id>:end -->`, and a run replaces the text between its own
//! markers and nothing else. That way prose a person wrote — caveats about a
//! repository, why a pin was chosen — survives every regeneration, and two
//! repositories' nightly jobs can update the same file without fighting.
//!
//! Everything here is a pure function of the result structs, so the rendering
//! is unit-testable and a formatting change never requires a corpus run.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context as _, Result};
use cambium_corpus::ReplayResult;
use serde::{Deserialize, Serialize};

use crate::threeway::{CompareResult, WallStats};

/// `bench legacy`'s headline numbers for this repository's scale, if the run
/// produced any.
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
}

/// One row of the top-level table.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TopRow {
    pub repo: String,
    pub pin: String,
    pub bazel_versions: Vec<String>,
    pub targets: u32,
    pub cold_build_s: f64,
    pub replay_p50_ms: u64,
    pub replay_p90_ms: u64,
    pub pairs: usize,
    pub failures: usize,
    pub min_recall: f64,
}

fn ms(v: u64) -> String {
    if v >= 10_000 {
        format!("{:.1} s", v as f64 / 1000.0)
    } else {
        format!("{v} ms")
    }
}

fn pct(v: f64) -> String {
    format!("{:.1}%", v * 100.0)
}

fn wall(w: &WallStats) -> String {
    if w.samples == 0 {
        "—".to_string()
    } else {
        format!("{} / {}", ms(w.p50_ms), ms(w.p90_ms))
    }
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
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

/// The markdown for one repository: everything a reader needs to judge the
/// claim, in the order they will ask for it — what was measured, whether it
/// was sound, then how fast.
pub fn render_repo_section(
    replay: Option<&ReplayResult>,
    compares: &[CompareResult],
    bench: Option<&BenchLegacy>,
) -> String {
    let mut s = String::new();
    let repo = replay
        .map(|r| r.repo.clone())
        .or_else(|| compares.first().map(|c| c.repo.clone()))
        .unwrap_or_default();
    let pin = replay
        .map(|r| r.pin.clone())
        .or_else(|| compares.first().map(|c| c.pin.clone()))
        .unwrap_or_default();
    let versions: BTreeSet<String> = compares
        .iter()
        .filter_map(|c| c.bazel_version.clone())
        .chain(replay.map(|r| r.bazel_version.clone()))
        .filter(|v| !v.is_empty())
        .collect();

    s.push_str(&format!("## {repo}\n\n"));
    s.push_str(&format!("- pin: `{}`\n", short(&pin)));
    s.push_str(&format!(
        "- Bazel: {}\n",
        if versions.is_empty() {
            "unrecorded".to_string()
        } else {
            versions
                .iter()
                .map(|v| format!("`{v}`"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    s.push_str(&format!("- generated: {}\n", today()));
    if let Some(r) = replay {
        s.push_str(&format!("- tool: `{}`\n", r.tool_sha));
    }
    s.push('\n');

    // ---- cold build and replay ----
    if let Some(r) = replay {
        let c = &r.cold;
        s.push_str("### Cold build\n\n");
        s.push_str("| targets | edges | packages | index bytes | wall |\n");
        s.push_str("|---:|---:|---:|---:|---:|\n");
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n\n",
            c.nodes,
            c.edges,
            c.packages,
            c.index_bytes,
            ms(c.wall_ms)
        ));

        let u = &r.summary;
        s.push_str(&format!(
            "### Replay ({} commits, `{}..{}`)\n\n",
            u.commits,
            short(&r.from),
            short(&r.to)
        ));
        s.push_str("| p50 | p90 | p99 | max | zero-dirty | largest dirty set | globals |\n");
        s.push_str("|---:|---:|---:|---:|---:|---:|---:|\n");
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} pkgs (`{}`) | {} |\n\n",
            ms(u.p50_total_ms),
            ms(u.p90_total_ms),
            ms(u.p99_total_ms),
            ms(u.max_total_ms),
            pct(u.zero_dirty_fraction),
            u.largest_dirty_packages,
            short(&u.largest_dirty_commit),
            u.globals
        ));
        if let Some(o) = &r.oracle {
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
    }

    // ---- the three-way ----
    if !compares.is_empty() {
        s.push_str("### Three-way comparison\n\n");
        s.push_str(
            "| Bazel | producer | pairs | pass | precision | min recall | \
             producer cold p50/p90 | producer warm p50/p90 | cambium cold p50/p90 | \
             cambium warm p50/p90 |\n",
        );
        s.push_str("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|\n");
        for c in compares {
            let ver = c.bazel_version.clone().unwrap_or_else(|| "default".into());
            let names: BTreeSet<&str> = c
                .pairs
                .iter()
                .flat_map(|p| p.producers.iter().map(|pr| pr.name.as_str()))
                .collect();
            for name in names {
                s.push_str(&format!(
                    "| `{ver}` | {name} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                    c.summary.pairs,
                    c.summary.passed,
                    c.summary
                        .mean_precision
                        .get(name)
                        .map(|p| pct(*p))
                        .unwrap_or_else(|| "—".into()),
                    pct(c.summary.min_recall),
                    c.summary
                        .producer_cold
                        .get(name)
                        .map(wall)
                        .unwrap_or_else(|| "—".into()),
                    c.summary
                        .producer_warm
                        .get(name)
                        .map(wall)
                        .unwrap_or_else(|| "—".into()),
                    wall(&c.summary.cambium_cold),
                    wall(&c.summary.cambium_warm),
                ));
            }
        }
        s.push('\n');

        s.push_str("Disagreements by class (all pairs, all producers):\n\n");
        s.push_str("| class | count | fails the run |\n|---|---:|---|\n");
        let mut totals: std::collections::BTreeMap<String, usize> = Default::default();
        for c in compares {
            for (k, v) in &c.summary.by_class {
                *totals.entry(k.clone()).or_default() += v;
            }
        }
        for (class, n) in &totals {
            let fails = crate::classify::Class::ALL
                .iter()
                .any(|c| c.name() == class.as_str() && c.is_failure());
            s.push_str(&format!(
                "| `{class}` | {n} | {} |\n",
                if fails { "yes" } else { "no" }
            ));
        }
        s.push('\n');
        // Two numbers that belong to the producers rather than to a class:
        // how often the producer's own distance agrees with cambium's depth,
        // and how many file-target rows had to be dropped to compare the two
        // over one universe. Both are noise until they move.
        let mut depth: std::collections::BTreeMap<String, (f64, usize)> = Default::default();
        let mut dropped: std::collections::BTreeMap<String, usize> = Default::default();
        for c in compares {
            for pair in &c.pairs {
                for pr in &pair.producers {
                    *dropped.entry(pr.name.clone()).or_default() += pr.dropped_file_targets;
                    if let Some(d) = pr.depth_agreement {
                        let e = depth.entry(pr.name.clone()).or_default();
                        e.0 += d;
                        e.1 += 1;
                    }
                }
            }
        }
        for (name, n) in &dropped {
            let agree = match depth.get(name) {
                Some((sum, count)) if *count > 0 => {
                    format!(
                        "depth agrees on {} of shared labels ({count} pairs)",
                        pct(sum / *count as f64)
                    )
                }
                _ => "no distances reported".to_string(),
            };
            s.push_str(&format!(
                "- `{name}`: {agree}; {n} file-target rows dropped to match cambium's universe.\n"
            ));
        }
        s.push('\n');
    }

    // ---- bench legacy ----
    if let Some(b) = bench {
        s.push_str("### bazel-diff on adjacent pairs (`bench legacy`)\n\n");
        s.push_str("| preset | targets | bazel-diff cold | bazel-diff warm | cambium cold | cambium warm | warm speedup | agree |\n");
        s.push_str("|---|---:|---:|---:|---:|---:|---:|---|\n");
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {:.0}x | {} |\n\n",
            b.preset,
            b.targets,
            ms(b.legacy_cold_ms),
            ms(b.legacy_warm_ms),
            ms(b.cambium_cold_ms),
            ms(b.cambium_warm_ms),
            b.speedup(),
            if b.agreed { "yes" } else { "no" }
        ));
    }

    s
}

/// The table at the top of the document: one line per repository.
pub fn render_top_table(rows: &[TopRow]) -> String {
    let mut s = String::new();
    s.push_str("| repo | pin | Bazel | targets | cold build | replay p50/p90 | pairs | failures | min recall |\n");
    s.push_str("|---|---|---|---:|---:|---:|---:|---:|---:|\n");
    for r in rows {
        s.push_str(&format!(
            "| [{}](#{}) | `{}` | {} | {} | {:.0} s | {} / {} | {} | {} | {} |\n",
            r.repo,
            r.repo.to_lowercase(),
            short(&r.pin),
            if r.bazel_versions.is_empty() {
                "—".to_string()
            } else {
                r.bazel_versions.join(", ")
            },
            r.targets,
            r.cold_build_s,
            ms(r.replay_p50_ms),
            ms(r.replay_p90_ms),
            r.pairs,
            r.failures,
            pct(r.min_recall),
        ));
    }
    s
}

/// A row for the top table, derived from a repository's results.
pub fn top_row(replay: Option<&ReplayResult>, compares: &[CompareResult]) -> TopRow {
    let mut row = TopRow {
        min_recall: 1.0,
        ..Default::default()
    };
    if let Some(r) = replay {
        row.repo = r.repo.clone();
        row.pin = r.pin.clone();
        row.targets = r.cold.nodes;
        row.cold_build_s = r.cold.wall_ms as f64 / 1000.0;
        row.replay_p50_ms = r.summary.p50_total_ms;
        row.replay_p90_ms = r.summary.p90_total_ms;
        if !r.bazel_version.is_empty() {
            row.bazel_versions.push(r.bazel_version.clone());
        }
    }
    for c in compares {
        if row.repo.is_empty() {
            row.repo = c.repo.clone();
            row.pin = c.pin.clone();
        }
        if let Some(v) = c.bazel_version.clone() {
            if !v.is_empty() && !row.bazel_versions.contains(&v) {
                row.bazel_versions.push(v);
            }
        }
        row.pairs += c.summary.pairs;
        row.failures += c.summary.pairs - c.summary.passed;
        row.min_recall = row.min_recall.min(c.summary.min_recall);
    }
    row
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
            out.push_str(doc[e + end.len()..].trim_start_matches('\n'));
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

    fn compare() -> CompareResult {
        let mut counts = std::collections::BTreeMap::new();
        counts.insert("cambium_extra_select".to_string(), 3);
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
                cambium_only_reasons: Default::default(),
                pass: true,
                error: None,
            }],
            oracle: None,
            recall: 1.0,
            global: false,
            pass: true,
        };
        let mut c = CompareResult {
            schema: 3,
            repo: "bazel".into(),
            pin: "0123456789abcdef".into(),
            bazel_version: Some("7.4.1".into()),
            seed: 0,
            oracle_k: 100,
            pairs: vec![pair],
            summary: CompareSummary::default(),
        };
        c.summarise();
        c
    }

    #[test]
    fn a_repo_section_reports_the_pin_the_versions_and_the_classes() {
        let s = render_repo_section(None, &[compare()], None);
        assert!(s.starts_with("## bazel\n"), "{s}");
        assert!(s.contains("`0123456789ab`"), "the pin is abbreviated: {s}");
        assert!(s.contains("`7.4.1`"), "{s}");
        assert!(s.contains("bazel-diff-rust"), "{s}");
        assert!(s.contains("| `cambium_extra_select` | 3 | no |"), "{s}");
        assert!(
            s.contains("| `cambium_missing` | 0 | yes |"),
            "a failing class is listed even at zero: {s}"
        );
        assert!(s.contains("45.0 s"), "walls are humanised: {s}");
        assert!(s.contains("100.0%"), "recall is a percentage: {s}");
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
            agreed: true,
        };
        let s = render_repo_section(None, &[], Some(&bench));
        assert!(s.contains("1000x"), "{s}");
        assert!(s.contains("| yes |"), "{s}");
    }

    #[test]
    fn the_top_table_links_each_repo_to_its_section() {
        let row = top_row(None, &[compare()]);
        assert_eq!(row.pairs, 1);
        assert_eq!(row.failures, 0);
        assert_eq!(row.bazel_versions, vec!["7.4.1"]);
        let t = render_top_table(&[row]);
        assert!(t.contains("[bazel](#bazel)"), "{t}");
        assert!(t.contains("`0123456789ab`"), "{t}");
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
