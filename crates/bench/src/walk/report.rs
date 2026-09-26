//! Rendering a walk to markdown: soundness, tightness per tool, cost per
//! population, coverage per tag, the environment, and the taxonomy as the
//! explanation layer.
//!
//! Pure over [`WalkResult`], so the table a run prints is checked without a
//! Bazel. The cross-subject line an engine change reads first is not here but
//! in [`crate::report::render_top_table`]. cambium is a tightness row like
//! any peer; the oracle is what every row is measured against.

use std::fmt::Write as _;

use crate::util::short;

use super::model::{Tightness, WalkResult, WallStats};
use super::steps::Tag;

/// The whole per-subject section.
pub fn render(r: &WalkResult) -> String {
    let mut out = String::new();
    let pin = short(&r.pin, 12);
    let _ = writeln!(out, "### {} — {} ({})", r.repo, pin, r.tier.name());
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
        short(&e.engine_id, 12),
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
    if s.oracle_partial_steps > 0 {
        let _ = writeln!(
            out,
            "The oracle's answer was a lower bound on {} of {} steps: Bazel could not resolve {}, \
             and under `--keep_going` every target that depends on an unresolvable one leaves the \
             `rdeps` universe, so recall is measured against what was left.",
            s.oracle_partial_steps,
            s.steps_with_oracle,
            listed(&s.oracle_unresolved, 8, |l| format!("`{l}`")),
        );
    }
    if s.oracle_repository_steps > 0 {
        let _ = writeln!(
            out,
            "Repository-rule inputs rooted the oracle on {} of {} steps: Bazel's marker files \
             named a changed path among the inputs of {}, and rooting the oracle at each of \
             them as `@@R//...:*` reached {} main-repository rules that the changed paths' own \
             labels did not.",
            s.oracle_repository_steps,
            s.steps_with_oracle,
            listed(&s.oracle_repositories, 8, |l| format!("`@@{l}`")),
            s.oracle_repository_labels,
        );
    }
    if s.oracle_repository_idle_steps > 0 {
        let _ = writeln!(
            out,
            "Steps on which the marker files named a changed path but the repository roots \
             added no rule: {} (a pattern that did not resolve at head, or one that reached \
             nothing the changed paths' labels had not). They are as vacuous as they were \
             without the markers; `oracle.repository_inputs` on each says which.",
            s.oracle_repository_idle_steps,
        );
    }
    if let Some(o) = &r.rebuild_oracle {
        let _ = write!(
            out,
            "Rebuild oracle at the pin: {}.",
            if o.ok { "held" } else { &o.summary }
        );
        // The same tallies whether or not the oracle held: a PASS reached by
        // excusing thousands of missing rows has to say so where the verdict
        // is read, not only in the sidecar.
        if let Some(file) = &o.rows {
            let c = &o.sky;
            let _ = write!(
                out,
                " Loading layer: {} missing rows ({} expected: created in the window, a probe \
                 for a boundary or build file that does not exist, or a package that failed to \
                 load; {} unexplained), {} extra ({} for paths deleted in the window or a \
                 package that failed to load); rows in walk/{file}.",
                c.missing, c.missing_expected, c.missing_unexplained, c.extra, c.extra_expected
            );
        }
        if !o.rule_hash_mismatch.is_empty() {
            let _ = write!(
                out,
                " Rule hashes differ from the cold rebuild for {}: {}.",
                o.rule_hash_mismatch.len(),
                listed(&o.rule_hash_mismatch, 6, |l| format!("`{l}`")),
            );
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
    if c.oracle_wall.samples > 0 {
        row(out, "rdeps oracle", &c.oracle_wall);
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

/// The first `n` of `items` through `fmt`, comma-separated, then ` and M
/// more` when there were more than `n`.
fn listed<'a, I>(items: I, n: usize, fmt: impl Fn(&str) -> String) -> String
where
    I: IntoIterator<Item = &'a String>,
    I::IntoIter: ExactSizeIterator,
{
    let items = items.into_iter();
    let total = items.len();
    let shown = items.take(n).map(|l| fmt(l)).collect::<Vec<_>>().join(", ");
    if total > n {
        format!("{shown} and {} more", total - n)
    } else {
        shown
    }
}

/// `parent..head` -> `parent12..head12`.
fn short_key(key: &str) -> String {
    match key.split_once("..") {
        Some((a, b)) => format!("{}..{}", short(a, 12), short(b, 12)),
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

    #[test]
    fn a_long_list_shows_its_first_n_and_counts_the_rest() {
        let v: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let tick = |l: &str| format!("`{l}`");
        assert_eq!(listed(&v, 8, tick), "`a`, `b`, `c`");
        assert_eq!(listed(&v, 3, tick), "`a`, `b`, `c`");
        assert_eq!(listed(&v, 2, tick), "`a`, `b` and 1 more");
        assert_eq!(listed(&Vec::new(), 2, tick), "");
    }

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
                ..Default::default()
            },
            started_unix: 0,
            index_build_ms: 5000,
            steps: vec![step],
            rebuild_oracle: Some(RebuildOracle {
                ok: true,
                summary: String::new(),
                rows: Some("rebuild-bbbbbbbbbbbb0-7.4.1.json".into()),
                sky: crate::walk::RowCounts {
                    missing: 6712,
                    missing_expected: 6712,
                    missing_unexplained: 0,
                    extra: 4,
                    extra_expected: 3,
                },
                rule_hash_mismatch: Vec::new(),
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
        // A held oracle still says how many missing rows it excused.
        assert!(
            md.contains("6712 missing rows (6712 expected: created in the window, a probe for a boundary or build file that does not exist, or a package that failed to load; 0 unexplained), 4 extra (3 for paths deleted"),
            "{md}"
        );
        assert!(
            md.contains("rows in walk/rebuild-bbbbbbbbbbbb0-7.4.1.json"),
            "{md}"
        );
        assert!(md.contains("bazel 7.4.1"));
    }

    /// The repository-input sentences: the rooted steps with what they
    /// added, and the steps whose patterns added nothing, apart.
    #[test]
    fn the_repository_roots_are_reported_and_an_idle_step_is_not_counted_as_rooted() {
        use crate::oracles::rdeps::RepositoryRoots;
        use crate::oracles::repo_inputs::DirtyRepository;
        let mut r = a_walk();
        let rooted = RepositoryRoots {
            repositories: vec![DirtyRepository {
                name: "+third_party_ext+llvm-raw".into(),
                via: vec!["third_party/llvm/build.patch".into()],
            }],
            roots: vec!["@@+third_party_ext+llvm-raw//...:*".into()],
            labels_added: 9559,
            ..Default::default()
        };
        let idle = RepositoryRoots {
            repositories: vec![DirtyRepository {
                name: "base_pip3".into(),
                via: vec!["tools/base/requirements.txt".into()],
            }],
            roots: vec!["@@base_pip3//...:*".into()],
            failed: vec!["@@base_pip3//...:*".into()],
            ..Default::default()
        };
        let mut second = r.steps[0].clone();
        second.head = "cccccccccccc0".into();
        r.steps[0]
            .oracle
            .get_or_insert_with(Default::default)
            .repository_inputs = Some(rooted);
        second
            .oracle
            .get_or_insert_with(Default::default)
            .repository_inputs = Some(idle);
        r.steps.push(second);
        r.summarise();
        let md = render(&r);
        assert!(
            md.contains("Repository-rule inputs rooted the oracle on 1 of 2 steps: Bazel's marker files named a changed path among the inputs of `@@+third_party_ext+llvm-raw`, and rooting the oracle at each of them as `@@R//...:*` reached 9559 main-repository rules that the changed paths' own labels did not."),
            "{md}"
        );
        assert!(
            md.contains("Steps on which the marker files named a changed path but the repository roots added no rule: 1 ("),
            "{md}"
        );
        assert!(!md.contains("`@@base_pip3`"), "{md}");
    }

    /// A failing oracle names the sidecar and how many rows nothing explains.
    #[test]
    fn a_failing_rebuild_oracle_names_its_rows_and_the_unexplained_count() {
        let mut r = a_walk();
        r.rebuild_oracle = Some(RebuildOracle {
            ok: false,
            summary: "loading layer rows missing: 5 (extra: 0); loading layer unexplained: 2"
                .into(),
            rows: Some("rebuild-bbbbbbbbbbbb0-7.4.1.json".into()),
            sky: crate::walk::RowCounts {
                missing: 5,
                missing_expected: 3,
                missing_unexplained: 2,
                ..Default::default()
            },
            rule_hash_mismatch: Vec::new(),
        });
        r.summarise();
        assert!(!r.summary.pass);
        let md = render(&r);
        assert!(
            md.contains("Rebuild oracle at the pin: loading layer rows missing: 5"),
            "{md}"
        );
        assert!(
            md.contains("Loading layer: 5 missing rows (3 expected: created in the window, a probe for a boundary or build file that does not exist, or a package that failed to load; 2 unexplained), 0 extra (0 for paths deleted in the window or a package that failed to load); rows in walk/rebuild-bbbbbbbbbbbb0-7.4.1.json."),
            "{md}"
        );
    }
}
