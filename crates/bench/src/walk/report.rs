//! Rendering a walk to markdown: soundness, tightness per tool, cost per
//! population, coverage per tag, the environment, and the taxonomy as the
//! explanation layer.
//!
//! Pure over [`WalkResult`], so the table a run prints is checked without a
//! Bazel. The top line is the one an engine change reads first: pin, Bazel,
//! index build, walk p50/p90, rebuild steps, worst recall, tightness p50, and
//! the peers' wall on the sample. cambium is a tightness row like any peer;
//! the oracle is what every row is measured against.

use std::fmt::Write as _;

use super::model::{Tightness, WalkResult, WallStats};
use super::steps::Tag;

/// The whole per-subject section.
pub fn render(r: &WalkResult) -> String {
    let mut out = String::new();
    let short = &r.pin[..r.pin.len().min(12)];
    let _ = writeln!(out, "### {} — {} ({})", r.repo, short, r.tier.name());
    let _ = writeln!(out);
    let e = &r.environment;
    let _ = writeln!(
        out,
        "Environment: {} · {} · cambi {} · tool {} · engine {}",
        e.machine.fingerprint(),
        if e.bazel_version_used.is_empty() {
            "bazel n/a"
        } else {
            &e.bazel_version_used
        },
        e.cambi_version,
        e.tool_sha,
        &e.engine_id[..e.engine_id.len().min(12)],
    );
    let _ = writeln!(out);
    render_soundness(&mut out, r);
    render_tightness(&mut out, r);
    render_cost(&mut out, r);
    render_coverage(&mut out, r);
    out
}

fn render_soundness(out: &mut String, r: &WalkResult) {
    let s = &r.summary;
    let _ = writeln!(out, "**Soundness**");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{} steps, {} with the oracle. {} sound, {} missing; worst recall {:.4}{}.",
        s.steps,
        s.steps_with_oracle,
        s.soundness.steps_sound,
        s.soundness.steps_missing,
        s.soundness.worst_recall,
        if s.soundness.worst_step.is_empty() {
            String::new()
        } else {
            format!(" at {}", short_key(&s.soundness.worst_step))
        },
    );
    if let Some(o) = &r.rebuild_oracle {
        let _ = write!(
            out,
            "Rebuild oracle at the pin: {}.",
            if o.ok { "held" } else { &o.summary }
        );
        if let Some(file) = &o.rows {
            let c = &o.sky;
            if o.ok {
                let _ = write!(
                    out,
                    " Loading layer: {} extra rows ({} for paths deleted in the window or a \
                     package that failed to load); rows in walk/{file}.",
                    c.extra, c.extra_expected
                );
            } else {
                let _ = write!(
                    out,
                    " Loading layer: {} missing rows, {} expected (created in the window or a \
                     package that failed to load), {} unexplained; rows in walk/{file}.",
                    c.missing, c.missing_expected, c.missing_unexplained
                );
            }
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(
        out,
        "Globals: {}. Engine cold rebuilds: {}.",
        s.globals, s.engine_cold_rebuilds
    );
    let _ = writeln!(out);
}

fn render_tightness(out: &mut String, r: &WalkResult) {
    let s = &r.summary;
    if s.tightness.is_empty() {
        return;
    }
    let _ = writeln!(out, "**Tightness against the oracle** (1.0 is exact)");
    let _ = writeln!(out);
    let _ = writeln!(out, "| tool | p50 | p90 | steps |");
    let _ = writeln!(out, "|---|--:|--:|--:|");
    // cambium first, then the peers alphabetically.
    let mut names: Vec<&String> = s.tightness.keys().collect();
    names.sort_by_key(|n| (n.as_str() != crate::tools::cambi::NAME, (*n).clone()));
    for n in names {
        let Tightness { p50, p90, steps } = &s.tightness[n];
        let _ = writeln!(out, "| {n} | {p50:.3} | {p90:.3} | {steps} |");
    }
    let _ = writeln!(out);
}

fn render_cost(out: &mut String, r: &WalkResult) {
    let c = &r.summary.cost;
    let _ = writeln!(out, "**Cost** (the two populations kept apart)");
    let _ = writeln!(out);
    let _ = writeln!(out, "Index build: {} ms.", c.index_build_ms);
    let _ = writeln!(out, "| population | p50 | p90 | max | samples |");
    let _ = writeln!(out, "|---|--:|--:|--:|--:|");
    row(out, "cambium walk (incremental)", &c.cambium_walk);
    if c.rebuild_steps > 0 {
        row(out, "cambium rebuild steps", &c.rebuild_wall);
    }
    for (name, w) in &c.peer_wall {
        row(out, &format!("{name} (full run per step)"), w);
    }
    let _ = writeln!(out);
}

fn row(out: &mut String, label: &str, w: &WallStats) {
    let _ = writeln!(
        out,
        "| {label} | {} ms | {} ms | {} ms | {} |",
        w.p50_ms, w.p90_ms, w.max_ms, w.samples
    );
}

fn render_coverage(out: &mut String, r: &WalkResult) {
    let _ = writeln!(out, "**Coverage per tag**");
    let _ = writeln!(out);
    let _ = writeln!(out, "| tag | present | sampled | wanted | deficit |");
    let _ = writeln!(out, "|---|--:|--:|--:|--:|");
    for t in Tag::ALL {
        if let Some(c) = r.summary.coverage.get(&t) {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} |",
                t.name(),
                c.present,
                c.sampled,
                c.wanted,
                c.deficit()
            );
        }
    }
    let _ = writeln!(out);
}

/// `parent..head` -> `parent12..head12`.
fn short_key(key: &str) -> String {
    match key.split_once("..") {
        Some((a, b)) => format!("{}..{}", &a[..a.len().min(12)], &b[..b.len().min(12)]),
        None => key.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::*;
    use super::super::sampling::Sample;
    use super::super::steps::{Step, Tag};
    use super::super::tier::Tier;
    use super::*;
    use crate::env::Environment;
    use crate::harness::Machine;

    fn a_walk() -> WalkResult {
        let steps: Vec<Step> = vec![Step {
            parent: "aaaaaaaaaaaa0".into(),
            head: "bbbbbbbbbbbb0".into(),
            tags: vec![Tag::Adjacent, Tag::Bzl],
        }];
        let sample = Sample::all(&steps);
        let step = StepRecord {
            parent: steps[0].parent.clone(),
            head: steps[0].head.clone(),
            tags: steps[0].tags.clone(),
            changed_files: 1,
            dirty_packages: 2,
            queried_targets: 3,
            timings: StepTimings::default(),
            wall_ms: 42,
            carried_index: true,
            rebuild_step: false,
            global: false,
            global_reason: None,
            index: IndexCounts::default(),
            cambium: CambiumAnswer::default(),
            oracle: Some(crate::oracles::rdeps::RdepsResult {
                oracle_labels: 10,
                matched: 10,
                oracle_set: ["//a:b".to_string()].into_iter().collect(),
                ..Default::default()
            }),
            tools: vec![
                ToolStep {
                    name: "cambium".into(),
                    version: "1".into(),
                    labels: 12,
                    wall_ms: 42,
                    recall_vs_oracle: Some(1.0),
                    tightness_vs_oracle: Some(10.0 / 12.0),
                    failed: false,
                },
                ToolStep {
                    name: "bazel-diff".into(),
                    version: "2".into(),
                    labels: 20,
                    wall_ms: 800,
                    recall_vs_oracle: Some(1.0),
                    tightness_vs_oracle: Some(0.5),
                    failed: false,
                },
            ],
            pass: true,
        };
        let mut r = WalkResult {
            schema: crate::RESULT_SCHEMA,
            repo: "envoy".into(),
            pin: "bbbbbbbbbbbb0".into(),
            from: "aaaaaaaaaaaa0".into(),
            to: "bbbbbbbbbbbb0".into(),
            tier: Tier::Full,
            schedule: Tier::Full.defaults(),
            oracle_sample: sample.clone(),
            peer_sample: sample,
            environment: Environment {
                machine: Machine::detect(),
                bazel_version_used: "bazel 7.4.1".into(),
                cambi_version: "cambi 0.1 (abc)".into(),
                tool_sha: "def".into(),
                engine_id: "0123456789abcdef".into(),
                config_hash: "hash".into(),
                output_bases: Default::default(),
            },
            started_unix: 0,
            index_build_ms: 5000,
            steps: vec![step],
            rebuild_oracle: Some(RebuildOracle {
                ok: true,
                summary: String::new(),
                rows: Some("rebuild-bbbbbbbbbbbb0-7.4.1.json".into()),
                sky: crate::walk::RowCounts {
                    extra: 4,
                    extra_expected: 3,
                    ..Default::default()
                },
            }),
            head_index: None,
            summary: WalkSummary::default(),
            done: true,
        };
        r.summarise();
        r
    }

    #[test]
    fn the_section_names_every_tool_and_the_oracle_is_the_yardstick() {
        let md = render(&a_walk());
        assert!(md.contains("### envoy"));
        assert!(md.contains("Tightness against the oracle"));
        // cambium is a row, listed first, and a peer is a row too.
        let cambium = md.find("| cambium |").expect("cambium row");
        let peer = md.find("| bazel-diff |").expect("peer row");
        assert!(cambium < peer, "cambium is the first tightness row");
        assert!(md.contains("Index build: 5000 ms"));
        assert!(md.contains("full run per step"));
        assert!(md.contains("Rebuild oracle at the pin: held"));
        assert!(md.contains("4 extra rows (3 for paths deleted"), "{md}");
        assert!(md.contains("rows in walk/rebuild-bbbbbbbbbbbb0-7.4.1.json"), "{md}");
        assert!(md.contains("bazel 7.4.1"));
    }

    /// A failing oracle names the sidecar and how many rows nothing explains.
    #[test]
    fn a_failing_rebuild_oracle_names_its_rows_and_the_unexplained_count() {
        let mut r = a_walk();
        r.rebuild_oracle = Some(RebuildOracle {
            ok: false,
            summary: "loading layer rows missing: 5 (extra: 0); loading layer unexplained: 2".into(),
            rows: Some("rebuild-bbbbbbbbbbbb0-7.4.1.json".into()),
            sky: crate::walk::RowCounts {
                missing: 5,
                missing_expected: 3,
                missing_unexplained: 2,
                ..Default::default()
            },
        });
        r.summarise();
        assert!(!r.summary.pass);
        let md = render(&r);
        assert!(md.contains("Rebuild oracle at the pin: loading layer rows missing: 5"), "{md}");
        assert!(
            md.contains("Loading layer: 5 missing rows, 3 expected (created in the window or a package that failed to load), 2 unexplained; rows in walk/rebuild-bbbbbbbbbbbb0-7.4.1.json."),
            "{md}"
        );
    }
}
