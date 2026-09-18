//! `docs/RESULTS.md`, generated from a run's walk JSON.
//!
//! The document is written by hand *around* generated blocks: each subject
//! owns a region delimited by `<!-- bench:<id>:begin -->` and
//! `<!-- bench:<id>:end -->`, and a run replaces the text between its own
//! markers and nothing else. Prose a person wrote (a caveat, why a pin was
//! chosen, the Findings section) survives every regeneration, and two
//! subjects' nightly jobs can update the same file without fighting.
//!
//! One region is not a subject: `top`, the cross-subject table. It is
//! rendered from every subject's walk at once, so a per-subject job rewrites
//! it wholesale rather than dropping its neighbours' rows.
//!
//! Everything here is a pure function of [`crate::walk::WalkResult`], so the
//! rendering is unit-testable and a formatting change never needs a run. It
//! survives partial data: a subject with no walk renders a one-line note, not
//! a panic.

use std::path::Path;

use anyhow::{Context as _, Result};

use crate::walk::WalkResult;

/// One subject's results: the walk `bench-results/<id>/walk/` held, plus its
/// manifest id.
#[derive(Clone, Debug, Default)]
pub struct SubjectResults {
    pub id: String,
    pub walk: Option<WalkResult>,
}

impl SubjectResults {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            walk: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.walk.is_none()
    }

    /// The newest walk under `bench-results/<id>/walk/`. More than one can be
    /// present (a subject whose `bazel_version` changed leaves the older file
    /// behind); the newest by name wins.
    pub fn load(dir: &Path, id: &str) -> Result<Self> {
        let mut walks = json_files(&dir.join("walk"), "walk-");
        walks.sort();
        Ok(Self {
            id: id.to_string(),
            walk: walks.last().map(|p| WalkResult::read(p)).transpose()?,
        })
    }

    /// Every manifest entry with a results directory under `parent`, in
    /// manifest order, plus any stray directory the manifest no longer names.
    /// A renamed or removed entry still has numbers worth showing.
    pub fn collect(
        manifest: &crate::manifest::Manifest,
        parent: &Path,
        _subjects: &crate::checkout::SubjectsDir,
    ) -> Result<Vec<Self>> {
        let mut out = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for entry in &manifest.repos {
            seen.insert(entry.id.clone());
            let dir = parent.join(&entry.id);
            if !dir.is_dir() {
                continue;
            }
            let r = Self::load(&dir, &entry.id)?;
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
}

/// The `<prefix>*.json` files in `dir`, if it is one. A missing directory is
/// a subject that has not run the workload, not an error.
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

/// One row of the cross-subject table.
pub struct TopRow {
    pub repo: String,
    pub pin: String,
    pub bazel_used: String,
    /// Rule nodes in the pin's index, from the last step.
    pub targets: u32,
    pub index_build_s: f64,
    pub walk_p50_ms: u64,
    pub walk_p90_ms: u64,
    pub rebuild_steps: usize,
    pub worst_recall: f64,
    /// cambium's tightness p50, when the oracle ran.
    pub tightness_p50: Option<f64>,
    /// The peers' wall p50 over the steps they ran on.
    pub peer_wall_p50: std::collections::BTreeMap<String, u64>,
    pub pass: bool,
}

pub fn top_row(r: &SubjectResults) -> TopRow {
    let w = r.walk.as_ref();
    let s = w.map(|w| &w.summary);
    TopRow {
        repo: r.id.clone(),
        pin: w.map(|w| w.pin.clone()).unwrap_or_default(),
        bazel_used: w
            .map(|w| tidy_version(&w.environment.bazel_version_used))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "n/a".into()),
        targets: w
            .and_then(|w| w.steps.last())
            .map(|st| st.index.nodes)
            .unwrap_or(0),
        index_build_s: w.map(|w| w.index_build_ms as f64 / 1000.0).unwrap_or(0.0),
        walk_p50_ms: s.map(|s| s.cost.cambium_walk.p50_ms).unwrap_or(0),
        walk_p90_ms: s.map(|s| s.cost.cambium_walk.p90_ms).unwrap_or(0),
        rebuild_steps: s.map(|s| s.cost.rebuild_steps).unwrap_or(0),
        worst_recall: s.map(|s| s.soundness.worst_recall).unwrap_or(1.0),
        tightness_p50: s
            .and_then(|s| s.tightness.get(crate::tools::cambi::NAME))
            .map(|t| t.p50),
        peer_wall_p50: s
            .map(|s| {
                s.cost
                    .peer_wall
                    .iter()
                    .map(|(k, v)| (k.clone(), v.p50_ms))
                    .collect()
            })
            .unwrap_or_default(),
        pass: s.map(|s| s.pass).unwrap_or(false),
    }
}

/// The cross-subject table: one row per subject, the question an engine change
/// reads first.
pub fn render_top_table(repos: &[SubjectResults]) -> String {
    use std::fmt::Write as _;
    let rows: Vec<TopRow> = repos.iter().map(top_row).collect();
    // The peer columns are the union of every subject's peers.
    let mut peers: Vec<String> = rows
        .iter()
        .flat_map(|r| r.peer_wall_p50.keys().cloned())
        .collect();
    peers.sort();
    peers.dedup();

    let mut s = String::new();
    let _ = writeln!(s, "## Subjects\n");
    let _ = write!(
        s,
        "| subject | pin | bazel | targets | index build | walk p50 | walk p90 | rebuilds | worst recall | tightness p50 |"
    );
    for p in &peers {
        let _ = write!(s, " {p} p50 |");
    }
    let _ = writeln!(s);
    let _ = write!(s, "|---|---|---|--:|--:|--:|--:|--:|--:|--:|");
    for _ in &peers {
        let _ = write!(s, "--:|");
    }
    let _ = writeln!(s);
    for r in &rows {
        let short = &r.pin[..r.pin.len().min(12)];
        let tight = r
            .tightness_p50
            .map(|t| format!("{t:.3}"))
            .unwrap_or_else(|| "n/a".into());
        let _ = write!(
            s,
            "| {} | {} | {} | {} | {:.1} s | {} ms | {} ms | {} | {:.4}{} | {} |",
            r.repo,
            short,
            r.bazel_used,
            r.targets,
            r.index_build_s,
            r.walk_p50_ms,
            r.walk_p90_ms,
            r.rebuild_steps,
            r.worst_recall,
            if r.pass { "" } else { " ⚠" },
            tight,
        );
        for p in &peers {
            match r.peer_wall_p50.get(p) {
                Some(ms) => {
                    let _ = write!(s, " {ms} ms |");
                }
                None => {
                    let _ = write!(s, " — |");
                }
            }
        }
        let _ = writeln!(s);
    }
    s
}

/// One subject's section: its walk, or a note that it has none.
pub fn render_subject_section(r: &SubjectResults) -> String {
    match &r.walk {
        Some(w) => crate::walk::report::render(w),
        None => format!("### {}\n\n_No walk recorded yet._\n", r.id),
    }
}

/// `bazel 7.4.1` -> `7.4.1`; anything else is left alone.
fn tidy_version(v: &str) -> String {
    v.trim().strip_prefix("bazel ").unwrap_or(v.trim()).to_string()
}

// ------------------------------------------------------------- the regions

pub fn begin_marker(subject_id: &str) -> String {
    format!("<!-- bench:{subject_id}:begin -->")
}

pub fn end_marker(subject_id: &str) -> String {
    format!("<!-- bench:{subject_id}:end -->")
}

/// Replaces `subject_id`'s region of `doc` with `section`, appending the
/// region (markers and all) when it is not there yet.
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

    #[test]
    fn a_region_is_replaced_in_place_and_prose_survives() {
        let doc = "# Results\n\nHand-written intro.\n\n<!-- bench:envoy:begin -->\nold\n<!-- bench:envoy:end -->\n\n## Findings\n\nkeep me\n";
        let out = replace_section(doc, "envoy", "### envoy\n\nnew");
        assert!(out.contains("### envoy\n\nnew"));
        assert!(!out.contains("old"));
        assert!(out.contains("Hand-written intro."));
        assert!(out.contains("## Findings"));
        assert!(out.contains("keep me"));
    }

    #[test]
    fn a_missing_region_is_appended() {
        let out = replace_section("# Results\n", "bazel", "### bazel\n\nrows");
        assert!(out.contains("<!-- bench:bazel:begin -->"));
        assert!(out.contains("### bazel"));
        assert!(out.ends_with("<!-- bench:bazel:end -->\n"));
    }

    #[test]
    fn the_top_table_has_a_row_per_subject_and_a_column_per_peer() {
        let mut a = SubjectResults::new("envoy");
        a.walk = Some(a_walk("envoy", &["bazel-diff"]));
        let mut b = SubjectResults::new("bazel");
        b.walk = Some(a_walk("bazel", &["tango"]));
        let table = render_top_table(&[a, b]);
        assert!(table.contains("| envoy |"));
        assert!(table.contains("| bazel |"));
        // The peer columns are the union.
        assert!(table.contains("bazel-diff p50"));
        assert!(table.contains("tango p50"));
        // A subject missing a peer shows a dash, not a zero.
        assert!(table.contains(" — |"));
    }

    fn a_walk(id: &str, peers: &[&str]) -> WalkResult {
        use crate::walk::{model::*, sampling::Sample, steps::Step, Tier};
        let step = Step {
            parent: "p0".into(),
            head: "h0".into(),
            tags: vec![crate::walk::Tag::Adjacent],
        };
        let sample = Sample::all(std::slice::from_ref(&step));
        let mut tools = vec![ToolStep {
            name: "cambium".into(),
            version: "1".into(),
            labels: 10,
            wall_ms: 30,
            recall_vs_oracle: Some(1.0),
            tightness_vs_oracle: Some(0.9),
            failed: false,
        }];
        for p in peers {
            tools.push(ToolStep {
                name: (*p).into(),
                version: "1".into(),
                labels: 20,
                wall_ms: 700,
                recall_vs_oracle: Some(1.0),
                tightness_vs_oracle: Some(0.5),
                failed: false,
            });
        }
        let rec = StepRecord {
            parent: "p0".into(),
            head: "h0".into(),
            tags: vec![crate::walk::Tag::Adjacent],
            changed_files: 1,
            dirty_packages: 1,
            queried_targets: 1,
            timings: StepTimings::default(),
            wall_ms: 30,
            carried_index: true,
            rebuild_step: false,
            global: false,
            global_reason: None,
            index: IndexCounts {
                nodes: 1234,
                edges: 5,
                lineage: String::new(),
            },
            cambium: CambiumAnswer::default(),
            oracle: Some(crate::oracles::rdeps::RdepsResult {
                oracle_labels: 10,
                matched: 10,
                oracle_set: ["//a:b".to_string()].into_iter().collect(),
                ..Default::default()
            }),
            tools,
            pass: true,
        };
        let mut w = WalkResult {
            schema: crate::RESULT_SCHEMA,
            repo: id.into(),
            pin: "h0deadbeef00".into(),
            from: "p0".into(),
            to: "h0deadbeef00".into(),
            tier: Tier::Full,
            schedule: Tier::Full.defaults(),
            oracle_sample: sample.clone(),
            peer_sample: sample,
            environment: crate::env::Environment {
                machine: crate::harness::Machine::detect(),
                bazel_version_used: "bazel 7.4.1".into(),
                cambi_version: "cambi".into(),
                tool_sha: "sha".into(),
                engine_id: "eng".into(),
                config_hash: "h".into(),
                output_bases: Default::default(),
            },
            started_unix: 0,
            index_build_ms: 12000,
            steps: vec![rec],
            rebuild_oracle: Some(RebuildOracle {
                ok: true,
                summary: String::new(),
                rows: None,
                sky: Default::default(),
            }),
            head_index: None,
            summary: WalkSummary::default(),
            done: true,
        };
        w.summarise();
        w
    }
}
