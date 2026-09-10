//! The three-way comparison itself.
//!
//! For one `(base, head)` change set the driver produces three answers and compares
//! them:
//!
//! 1. **cambium**, twice — once with the base index resolved from scratch
//!    (`cold`) and once with it in the store (`warm`) — because the whole
//!    argument for cambium is the warm number, and quoting it without the cold
//!    one would be dishonest.
//! 2. **each tool** ([`crate::tools::Tool`]), typically bazel-diff.
//! 3. **the oracle** ([`crate::oracles::rdeps`]), one `bazel query rdeps` over
//!    every changed file.
//!
//! cambium vs tool is a *symmetric difference*, classified label by label
//! (see [`crate::classify`]). cambium vs oracle is one-sided and absolute:
//! cambium must contain everything Bazel reaches. A change set passes when no label
//! landed in `cambium_missing` or `unclassified` and the oracle found nothing
//! missing. Precision (`|tool ∩ cambium| / |cambium|`) is informational —
//! cambium deliberately over-approximates — and is reported so a regression in
//! over-approximation is visible.
//!
//! Results are written per repository to
//! `bench-results/<id>/compare-<pin>[-<bazel_version>].json` after **every**
//! change set, and finished change sets are recorded in the subject's state file, so a run
//! killed at change set 137 of 200 resumes there.

//!
//! The module is three files: [`model`] is what a comparison is (every type
//! in the result JSON), [`summary`] is the pure arithmetic that rolls the
//! change sets up, and [`driver`] is the only part that runs a Bazel.

pub mod driver;
pub mod model;
pub mod summary;

pub use driver::{compare_change_set, compare_change_set_with, compare_change_sets, run_context};
pub use model::{
    absolute, CambiumRun, ChangeSetResult, ChangesOptions, ChangesResult, ChangesSummary,
    Disagreement, GlobalStats, ToolComparison, WallStats, DEFAULT_EXAMPLES,
};
pub use summary::{
    classify_all, classify_difference, examples_of, precision, summarise, wall_stats,
};

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    use super::driver::{fresh_result, resumable};
    use super::*;
    use crate::changes::ChangeSetKind;
    use crate::classify::Class;
    use crate::classify::Facts;

    fn set(labels: &[&str]) -> BTreeSet<String> {
        labels.iter().map(|s| s.to_string()).collect()
    }

    #[derive(Default)]
    struct NoFacts;
    impl Facts for NoFacts {
        fn is_external(&self, l: &str) -> bool {
            l.starts_with('@')
        }
        fn reachable_via_select(&self, l: &str) -> bool {
            l.ends_with(":selected")
        }
        fn reachable_only_via_implicit(&self, _: &str) -> bool {
            false
        }
        fn visibility_only_change(&self, _: &str) -> bool {
            false
        }
        fn bzl_closure_only(&self, _: &str) -> bool {
            false
        }
        fn reached_via_external_repo(&self, l: &str) -> bool {
            l.ends_with(":from_external_repo")
        }
        fn changed_path_without_consumer(&self, _: &str) -> bool {
            false
        }
        fn build_file_changed(&self, _: &str) -> bool {
            false
        }
        fn reached_via_deleted_dep(&self, l: &str) -> bool {
            l.ends_with(":orphaned")
        }
        fn reached_via_build_file(&self, l: &str) -> bool {
            l.ends_with(":globbed")
        }
        fn rule_unchanged_modulo_visibility(&self, _: &str) -> bool {
            false
        }
        fn no_changed_source_inputs(&self, _: &str) -> bool {
            false
        }
        fn no_affected_deps(&self, _: &str) -> bool {
            false
        }
        fn has_external_input(&self, _: &str) -> bool {
            false
        }
        fn workspace_path_only_change(&self, _: &str) -> bool {
            false
        }
    }

    #[test]
    fn precision_is_the_tools_share_of_cambiums_answer() {
        let cambium = set(&["//a:a", "//b:b", "//c:c", "//d:d"]);
        assert_eq!(precision(&set(&["//a:a", "//b:b"]), &cambium), 0.5);
        assert_eq!(precision(&cambium, &cambium), 1.0);
        assert_eq!(
            precision(&set(&["//z:z"]), &cambium),
            0.0,
            "labels the tool alone reports do not raise precision"
        );
    }

    #[test]
    fn an_empty_cambium_answer_counts_as_agreement() {
        assert_eq!(precision(&BTreeSet::new(), &BTreeSet::new()), 1.0);
    }

    #[test]
    fn the_symmetric_difference_is_classified_from_both_sides() {
        let cambium = set(&["//a:a", "//p:selected", "//shared:s"]);
        let tool = set(&["//shared:s", "@ext//x:y", "//missed:m"]);
        let (counts, examples) = classify_difference(&cambium, &tool, &NoFacts, 10);
        assert_eq!(counts[&Class::CambiumExtraSelect], 1);
        assert_eq!(counts[&Class::Unclassified], 1, "//a:a has no explanation");
        assert_eq!(counts[&Class::BazeldiffExtraExternal], 1);
        assert_eq!(counts[&Class::CambiumMissing], 1);
        assert!(!counts.contains_key(&Class::CambiumExtraVisibility));
        assert_eq!(
            examples.iter().filter(|d| d.class.is_failure()).count(),
            2,
            "failures come first: {examples:?}"
        );
        assert!(examples[0].class.is_failure() && examples[1].class.is_failure());
    }

    #[test]
    fn a_tool_that_could_not_run_has_not_agreed_with_anything() {
        let err = anyhow::anyhow!("exit 1: stderr tail").context("hashing abc123 in /tmp/base");
        let c = ToolComparison::failed("bazel-diff-rust", "v46.1.0", &err);
        assert!(!c.pass, "a comparison that did not happen does not pass");
        // The whole chain, not just anyhow's outermost context: without the
        // alternate flag the exit status the tool attached never arrives.
        let e = c.error.unwrap();
        assert!(e.contains("hashing abc123"), "{e}");
        assert!(e.contains("exit 1"), "{e}");
        assert_eq!(c.name, "bazel-diff-rust");
        assert_eq!(c.version, "v46.1.0");
        // And a tool that did run starts from the passing default.
        assert!(ToolComparison::new("t", "v").pass);
    }

    #[test]
    fn the_example_list_is_capped_but_the_counts_are_not() {
        let cambium: BTreeSet<String> = (0..50).map(|i| format!("//a:t{i}")).collect();
        let (counts, examples) = classify_difference(&cambium, &BTreeSet::new(), &NoFacts, 3);
        assert_eq!(counts[&Class::Unclassified], 50);
        assert_eq!(examples.len(), 3);
    }

    fn change_set_comparison(
        name: &str,
        precision: f64,
        cold: u64,
        warm: u64,
        pass: bool,
    ) -> ChangeSetResult {
        let mut counts = BTreeMap::new();
        counts.insert(Class::CambiumExtraSelect, 2);
        if !pass {
            counts.insert(Class::CambiumMissing, 1);
        }
        ChangeSetResult {
            schema: crate::RESULT_SCHEMA,
            base: format!("b{name}"),
            head: format!("h{name}"),
            kind: ChangeSetKind::Adjacent,
            category: None,
            cambium: CambiumRun {
                cold_wall_ms: cold * 10,
                warm_wall_ms: warm,
                ..Default::default()
            },
            tools: vec![ToolComparison {
                name: "bazel-diff-rust".into(),
                version: "v46".into(),
                labels: 10,
                cold_wall_ms: cold,
                warm_wall_ms: Some(warm * 2),
                precision,
                depth_agreement: None,
                dropped_file_targets: 0,
                cambium_only: 2,
                tool_only: 0,
                counts,
                examples: Vec::new(),
                cambium_only_reasons: BTreeMap::new(),
                pass,
                error: None,
            }],
            oracle: None,
            load_oracle: None,
            recall: if pass { 1.0 } else { 0.75 },
            global: false,
            pass,
        }
    }

    #[test]
    fn the_summary_counts_classes_averages_precision_and_takes_the_worst_recall() {
        let change_sets = vec![
            change_set_comparison("1", 1.0, 100, 10, true),
            change_set_comparison("2", 0.5, 300, 30, false),
        ];
        let s = summarise(&change_sets);
        assert_eq!(s.change_sets, 2);
        assert_eq!(s.passed, 1);
        assert!(!s.pass, "one failing change set fails the run");
        assert_eq!(s.by_class[&Class::CambiumExtraSelect], 4);
        assert_eq!(s.by_class[&Class::CambiumMissing], 1);
        assert_eq!(
            s.by_class[&Class::BazeldiffExtraExternal],
            0,
            "every class is listed"
        );
        assert_eq!(s.mean_precision["bazel-diff-rust"], 0.75);
        assert_eq!(s.min_recall, 0.75);
        assert_eq!(
            s.tool_cold["bazel-diff-rust"],
            WallStats {
                samples: 2,
                p50_ms: 300,
                p90_ms: 300
            }
        );
        assert_eq!(s.cambium_warm.p50_ms, 30);
        assert_eq!(s.cambium_cold.p50_ms, 3000);
    }

    #[test]
    fn a_tool_that_could_not_run_fails_its_change_set() {
        // The absence has to be loud. A tool that errored contributes no
        // labels, so every aggregate simply omits it and the summary renders
        // as though the remaining tools were the whole comparison — which is
        // how the 2026-09-08 run published sourcegraph with no bazel-diff and
        // apollo with no Tango, both reading as clean.
        let mut broken = change_set_comparison("1", 1.0, 100, 10, true);
        broken.tools[0].error = Some("aspect query: ^D".into());
        broken.tools[0].pass = false;
        broken.pass = broken.tools.iter().all(|t| t.pass);
        let s = summarise(&[broken]);
        assert_eq!(s.passed, 0);
        assert!(!s.pass, "a comparison that did not happen is not a pass");
    }

    #[test]
    fn a_tool_that_failed_does_not_pollute_the_averages() {
        let mut change_sets = vec![change_set_comparison("1", 1.0, 100, 10, true)];
        let mut broken = change_set_comparison("2", 0.0, 0, 0, true);
        broken.tools[0].error = Some("boom".into());
        broken.tools[0].counts.clear();
        change_sets.push(broken);
        let s = summarise(&change_sets);
        assert_eq!(s.mean_precision["bazel-diff-rust"], 1.0);
        assert_eq!(s.tool_cold["bazel-diff-rust"].samples, 1);
    }

    /// The same change set with a `tango` row bolted on, as an uncapped run
    /// would produce.
    fn with_tango(mut p: ChangeSetResult) -> ChangeSetResult {
        let mut t = p.tools[0].clone();
        t.name = crate::tools::tango::NAME.to_string();
        t.cold_wall_ms = 300_000;
        t.warm_wall_ms = None;
        p.tools.push(t);
        p
    }

    #[test]
    fn a_change_set_tango_was_capped_out_of_is_not_a_disagreement() {
        // Two change sets, Tango on the first only: what `--quick` produces.
        let capped = summarise(&[
            with_tango(change_set_comparison("1", 1.0, 100, 10, true)),
            change_set_comparison("2", 1.0, 100, 10, true),
        ]);
        assert!(capped.pass, "a change_set Tango skipped still passes");
        assert_eq!((capped.change_sets, capped.passed), (2, 2));
        assert_eq!(
            capped.tool_change_sets[crate::tools::tango::NAME],
            1,
            "the summary says how many change_sets Tango's numbers come from"
        );
        assert_eq!(capped.tool_change_sets["bazel-diff-rust"], 2);
        assert_eq!(capped.tool_cold[crate::tools::tango::NAME].samples, 1);
        // The change sets Tango did run on are scored exactly as they would be with
        // no cap at all.
        let uncapped = summarise(&[
            with_tango(change_set_comparison("1", 1.0, 100, 10, true)),
            with_tango(change_set_comparison("2", 1.0, 100, 10, true)),
        ]);
        assert_eq!(
            capped.mean_precision[crate::tools::tango::NAME],
            uncapped.mean_precision[crate::tools::tango::NAME]
        );
        assert_eq!(capped.by_class, uncapped_minus_tango(&uncapped));
    }

    /// The class totals `uncapped` would have had with one fewer Tango row.
    fn uncapped_minus_tango(uncapped: &ChangesSummary) -> BTreeMap<Class, usize> {
        let mut totals = uncapped.by_class.clone();
        *totals.get_mut(&Class::CambiumExtraSelect).unwrap() -= 2;
        totals
    }

    /// A globally invalidated change set as the driver now records one: cambium
    /// answered `//...` (24 000 rule targets) and the tool still ran, took
    /// 999 ms and found 40 targets precisely.
    fn global_change_set(name: &str) -> ChangeSetResult {
        let mut p = change_set_comparison(name, 1.0, 999, 999, true);
        p.tools[0].labels = 40;
        p.tools[0].counts.clear();
        p.tools[0].cambium_only = 0;
        p.cambium.global = true;
        p.cambium.global_reason = Some("MODULE.bazel".into());
        p.cambium.universe = 24_000;
        p.global = true;
        p
    }

    #[test]
    fn a_global_change_set_is_counted_apart_and_leaves_the_soundness_numbers_alone() {
        let s = summarise(&[
            change_set_comparison("1", 0.5, 100, 10, true),
            global_change_set("2"),
        ]);
        assert_eq!((s.change_sets, s.passed, s.globals), (2, 2, 1));
        assert!(s.pass, "answering //... is not a failure");
        assert_eq!(
            s.mean_precision["bazel-diff-rust"], 0.5,
            "the global change_set contributes no precision"
        );
        assert_eq!(
            s.by_class[&Class::CambiumExtraSelect],
            2,
            "and no classified labels"
        );
        assert_eq!(
            s.tool_change_sets["bazel-diff-rust"], 1,
            "the incremental tool count skips it"
        );
    }

    /// The exact shape the 2026-09-08 run published: every change set global,
    /// every tool erroring, and the summary tables silent about it.
    #[test]
    fn a_global_change_set_whose_tool_failed_does_not_read_as_a_clean_run() {
        let mut g = global_change_set("2");
        let err = anyhow::anyhow!("aspect query: unexpected EOF on stdin");
        g.tools = vec![ToolComparison::failed("bazel-diff-rust", "v46.1.0", &err)];
        g.pass = g.tools.iter().all(|t| t.pass);
        assert!(!g.pass, "a comparison that did not happen is not agreement");

        let s = summarise(&[g]);
        assert_eq!((s.change_sets, s.passed, s.globals), (1, 0, 1));
        assert!(!s.pass, "and the run says so");
        // The aggregates still skip the tool -- there is no timing to take
        // from a run that produced none -- which is exactly why the `pass`
        // flag has to carry the failure instead.
        assert!(s.global_stats.tool_cold.is_empty());
        assert!(s.all_tool_change_sets.is_empty());
    }

    #[test]
    fn a_global_change_set_still_pays_for_its_tools_and_records_what_they_found() {
        let g = global_change_set("2");
        assert_eq!(
            g.tools.len(),
            1,
            "the tool ran: a global's cost is the thing being measured"
        );
        assert_eq!((g.tools[0].labels, g.cambium.universe), (40, 24_000));
        assert!(g.tools[0].counts.is_empty(), "nothing was classified");

        let s = summarise(&[change_set_comparison("1", 0.5, 100, 10, true), g]);
        let gs = &s.global_stats;
        assert_eq!(gs.change_sets, 1);
        assert_eq!(gs.mean_cambium_labels, 24_000.0);
        assert_eq!(gs.mean_tool_labels["bazel-diff-rust"], 40.0);
        assert_eq!(gs.tool_cold["bazel-diff-rust"].p50_ms, 999);
        assert_eq!(gs.cambium_cold.p50_ms, 9_990, "999 * 10, as the fixture");
        assert_eq!(gs.cambium_warm.p50_ms, 999);
    }

    #[test]
    fn the_all_change_sets_walls_include_the_globals_and_the_incremental_walls_do_not() {
        let s = summarise(&[
            change_set_comparison("1", 0.5, 100, 10, true),
            global_change_set("2"),
        ]);
        assert_eq!(
            (s.cambium_cold.samples, s.cambium_warm.samples),
            (1, 1),
            "the incremental series is over the walked change_set only"
        );
        assert_eq!(
            (s.all_cambium_cold.samples, s.all_cambium_warm.samples),
            (2, 2),
            "the all-change_sets series hides nothing"
        );
        assert_eq!(s.tool_cold["bazel-diff-rust"].samples, 1);
        assert_eq!(s.all_tool_cold["bazel-diff-rust"].samples, 2);
        assert_eq!(s.all_tool_change_sets["bazel-diff-rust"], 2);
        // The global is the slow change set, so it can only move the p90 up.
        assert!(
            s.all_cambium_warm.p90_ms > s.cambium_warm.p90_ms,
            "including the globals must not flatter cambium: {s:?}"
        );
    }

    #[test]
    fn a_cold_rebuild_change_set_is_incremental_but_counted() {
        let mut rebuilt = change_set_comparison("2", 1.0, 100, 10, true);
        rebuilt.cambium.cold_rebuild = true;
        let s = summarise(&[change_set_comparison("1", 1.0, 100, 10, true), rebuilt]);
        assert_eq!(s.cold_rebuild_change_sets, 1);
        assert_eq!(
            s.cambium_warm.samples, 2,
            "it is not a global: it stays in every statistic"
        );
        assert_eq!(s.globals, 0);
    }

    #[test]
    fn a_run_with_no_globals_carries_no_global_statistics() {
        let s = summarise(&[change_set_comparison("1", 1.0, 100, 10, true)]);
        assert!(s.global_stats.is_empty());
        assert_eq!(s.global_stats, GlobalStats::default());
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            !json.contains("global_stats"),
            "an empty table is left out of the JSON: {json}"
        );
    }

    /// `kind`, `category` and the class counts became enums; the JSON they
    /// produce must not have. A result file on the box is what the report and
    /// the next resume read, and the strings are also what a person greps for.
    #[test]
    fn the_typed_kind_category_and_classes_are_the_same_json_as_the_strings() {
        let p = ChangeSetResult {
            schema: crate::RESULT_SCHEMA,
            base: "b".into(),
            head: "h".into(),
            kind: ChangeSetKind::Stress,
            category: Some(crate::changes::StressCategory::BuildAddDelete),
            cambium: CambiumRun::default(),
            tools: vec![ToolComparison {
                counts: [(Class::CambiumExtraSelect, 3), (Class::CambiumMissing, 1)]
                    .into_iter()
                    .collect(),
                ..ToolComparison::new("bazel-diff-rust", "v46.1.0")
            }],
            oracle: None,
            load_oracle: None,
            recall: 1.0,
            global: false,
            pass: true,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains(r#""kind":"stress""#), "{json}");
        assert!(json.contains(r#""category":"build_add_delete""#), "{json}");
        assert!(json.contains(r#""cambium_extra_select":3"#), "{json}");
        assert!(json.contains(r#""cambium_missing":1"#), "{json}");

        let back: ChangeSetResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, ChangeSetKind::Stress);
        assert_eq!(back.category, p.category);
        assert_eq!(back.tools[0].counts[&Class::CambiumExtraSelect], 3);

        // And a change set with no category still leaves the key out.
        let mut plain = p.clone();
        plain.category = None;
        plain.kind = ChangeSetKind::Pr;
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("category"), "{json}");
        assert!(json.contains(r#""kind":"pr""#), "{json}");
    }

    /// Every field added since the first `changes-*.json` was written has a
    /// serde default, so the report can still read what is on the box.
    #[test]
    fn a_summary_written_before_the_all_change_sets_series_still_parses() {
        let old = r#"{
            "change_sets": 3, "passed": 3, "by_class": {}, "mean_precision": {},
            "min_recall": 1.0, "tool_cold": {}, "tool_warm": {},
            "cambium_cold": {"samples": 3, "p50_ms": 10, "p90_ms": 20},
            "cambium_warm": {"samples": 3, "p50_ms": 5, "p90_ms": 8},
            "pass": true
        }"#;
        let s: ChangesSummary = serde_json::from_str(old).unwrap();
        assert_eq!(
            (s.change_sets, s.globals, s.cold_rebuild_change_sets),
            (3, 0, 0)
        );
        assert_eq!(s.all_cambium_cold, WallStats::default());
        assert!(s.all_tool_change_sets.is_empty());
        assert!(s.global_stats.is_empty());

        let change_set = r#"{
            "schema": 3, "base": "b", "head": "h", "kind": "adjacent",
            "cambium": {"direct": 1, "affected": 2, "tests": 0, "global": false,
                        "cold_wall_ms": 9, "warm_wall_ms": 4, "dirty_packages": 1,
                        "queried_targets": 2, "changed_files": 1},
            "tools": [], "recall": 1.0, "pass": true
        }"#;
        let p: ChangeSetResult = serde_json::from_str(change_set).unwrap();
        assert!(!p.cambium.cold_rebuild);
        assert_eq!(p.cambium.universe, 0);
        assert_eq!(p.cambium.answer_size(), 2);
    }

    #[test]
    fn options_are_forced_absolute() {
        let opts = ChangesOptions::new("bazel", "abc", PathBuf::from("bench-results/bazel"));
        assert!(opts.results_dir.is_absolute(), "{opts:?}");
        assert!(opts.scratch.is_absolute(), "{opts:?}");
        let mut hand_built = ChangesOptions {
            results_dir: "bench-results/bazel".into(),
            scratch: "bench-results/bazel/scratch".into(),
            ..opts
        };
        hand_built.make_absolute();
        assert!(hand_built.results_dir.is_absolute());
        assert!(hand_built.scratch.is_absolute());
        assert!(hand_built.output_path().is_absolute());
    }

    #[test]
    fn percentiles_are_taken_over_the_sorted_walls() {
        assert_eq!(wall_stats(vec![]), WallStats::default());
        let s = wall_stats(vec![50, 10, 30, 20, 40, 60, 70, 80, 90, 100]);
        assert_eq!((s.samples, s.p50_ms, s.p90_ms), (10, 60, 100));
    }

    #[test]
    fn change_sets_compared_by_another_cambium_are_not_resumed() {
        let opts = ChangesOptions::new("bazel", "abc", PathBuf::from("/r/bazel"));
        let mut prev = fresh_result(&opts, "f8/0123456789abcdef");
        assert!(resumable(&prev, &opts, "f8/0123456789abcdef"));
        prev.engine = "f6/0123456789abcdef".into();
        assert!(!resumable(&prev, &opts, "f8/0123456789abcdef"));
        // Files written before the field existed carry no engine at all.
        prev.engine.clear();
        assert!(!resumable(&prev, &opts, "f8/0123456789abcdef"));
    }

    #[test]
    fn the_output_file_names_the_bazel_version_only_in_the_matrix() {
        let dir = Path::new("/r");
        assert_eq!(
            ChangesOptions::path(dir, "abc", None),
            PathBuf::from("/r/changes-abc.json")
        );
        assert_eq!(
            ChangesOptions::path(dir, "abc", Some("7.4.1")),
            PathBuf::from("/r/changes-abc-7.4.1.json")
        );
        assert_eq!(
            ChangesOptions::path(dir, "abc", Some("")),
            PathBuf::from("/r/changes-abc.json")
        );
    }

    #[test]
    fn a_result_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = ChangesResult {
            schema: crate::RESULT_SCHEMA,
            repo: "bazel".into(),
            pin: "abc".into(),
            bazel_version: Some("7.4.1".into()),
            bazel_version_used: Some("bazel 7.4.1".into()),
            seed: 7,
            oracle_k: 0,
            engine: String::new(),
            tango_change_sets: None,
            change_sets: vec![change_set_comparison("1", 1.0, 100, 10, true)],
            summary: ChangesSummary::default(),
        };
        r.summarise();
        let path = dir.path().join("changes-abc.json");
        r.write(&path).unwrap();
        let back = ChangesResult::read(&path).unwrap();
        assert_eq!(back.schema, crate::RESULT_SCHEMA);
        assert_eq!(back.change_sets.len(), 1);
        assert!(back.summary.pass);
    }
}
