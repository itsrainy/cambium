//! `inspect verify` against a canned Bazel: what it compares, what it
//! leaves out, and how it names what it finds.
//!
//! The canned answers are shaped like the ones rules_shell gets: Bazel's
//! `rdeps` names stardoc outputs and a target in `@bazel_tools` beside the
//! rules, one test reaches the library only through
//! `@bazel_tools//tools/test:collect_coverage`, and a `toolchain_type` has
//! rdeps through toolchain resolution that no query edge in the index
//! carries.

use std::process::Command;
use std::sync::Arc;

use cambium_bazel::{Bazel, BazelError, BazelRunner, Captured, Exit};
use cambium_engine::diag::Diagnostics;
use cambium_engine::inspect;
use cambium_index::{new_lineage, IndexBuilder, IndexData, PackageUpdate, TargetRow};

const COVERAGE: &str = "@bazel_tools//tools/test:collect_coverage";

/// ```text
///   //lib:x <- //tests:a, //tests:c, //docs:gen
///   //tests:b -> @bazel_tools//tools/test:collect_coverage (-> //lib:x in Bazel only)
///   //tc:type (toolchain_type), required by //tests:a in Bazel only
/// ```
fn index() -> IndexData {
    fn pkg(name: &str, targets: Vec<TargetRow>) -> PackageUpdate {
        PackageUpdate {
            package: name.to_string(),
            build_hash: *blake3::hash(name.as_bytes()).as_bytes(),
            bzl_hash: [0; 32],
            targets,
        }
    }
    let mut b = IndexBuilder::new(new_lineage());
    b.set_bazel_version("test");
    b.apply_packages(&[
        pkg(
            "lib",
            vec![TargetRow::rule("//lib:x", "sh_library", vec![])],
        ),
        pkg(
            "tests",
            vec![
                TargetRow::rule(
                    "//tests:a",
                    "sh_test",
                    vec!["//lib:x".into(), COVERAGE.into()],
                ),
                TargetRow::rule("//tests:b", "sh_test", vec![COVERAGE.into()]),
                TargetRow::rule("//tests:c", "sh_test", vec!["//lib:x".into()]),
            ],
        ),
        pkg(
            "tc",
            vec![TargetRow::rule("//tc:type", "toolchain_type", vec![])],
        ),
        pkg(
            "docs",
            vec![TargetRow::rule(
                "//docs:gen",
                "genrule",
                vec!["//lib:x".into()],
            )],
        ),
    ]);
    b.finish("cafe")
}

/// Answers each query by its expression, and refuses any it does not know.
#[derive(Debug)]
struct Router;

impl Router {
    fn answer(expr: &str) -> Option<&'static str> {
        Some(match expr {
            "rdeps(//..., //lib:x)" => {
                "sh_library rule //lib:x\n\
                 sh_test rule //tests:a\n\
                 sh_test rule //tests:b\n\
                 sh_binary rule @bazel_tools//tools/test:collect_coverage\n\
                 genrule rule //docs:gen\n\
                 generated file //docs:gen.md\n"
            }
            "rdeps(//..., //tc:type)" => "toolchain_type rule //tc:type\nsh_test rule //tests:a\n",
            "rdeps(//..., //tests:a)" => "sh_test rule //tests:a\n",
            "rdeps(//..., //tests:b)" => "sh_test rule //tests:b\n",
            "rdeps(//..., //tests:c)" => "sh_test rule //tests:c\n",
            "rdeps(//..., //docs:gen)" => "genrule rule //docs:gen\ngenerated file //docs:gen.md\n",
            "allpaths(set(//tests:b), //lib:x)" => {
                "digraph mygraph {\n  node [shape=box];\n  \"//tests:b\"\n  \
                 \"//tests:b\" -> \"@bazel_tools//tools/test:collect_coverage\"\n  \
                 \"@bazel_tools//tools/test:collect_coverage\"\n  \
                 \"@bazel_tools//tools/test:collect_coverage\" -> \"//lib:x\"\n  \
                 \"//lib:x\"\n}\n"
            }
            _ => return None,
        })
    }
}

impl BazelRunner for Router {
    fn stream(
        &self,
        _cmd: &mut Command,
        _on_stdout: &mut dyn FnMut(&mut dyn std::io::Read),
        _on_stderr: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Captured, BazelError> {
        unreachable!("verify runs whole queries")
    }

    fn output(&self, cmd: &mut Command) -> std::io::Result<Captured> {
        let expr = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .find(|a| a.starts_with("rdeps(") || a.starts_with("allpaths("))
            .unwrap_or_default();
        let answer = Self::answer(&expr);
        Ok(Captured {
            exit: Exit {
                code: Some(if answer.is_some() { 0 } else { 2 }),
                success: answer.is_some(),
            },
            stdout: answer.unwrap_or_default().as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }
}

fn run() -> inspect::VerifyResult {
    let data = index();
    let bazel = Bazel::at(std::env::temp_dir(), Some("bazel")).with_runner(Arc::new(Router));
    let diags = Diagnostics::new();
    let r = inspect::verify(&bazel, data.view(), 100, 0, &diags).unwrap();
    assert!(diags.drain().is_empty(), "every query was answered");
    r
}

#[test]
fn files_and_externals_are_counted_not_compared() {
    let r = run();
    assert_eq!(r.checked, 6);
    // //docs:gen.md twice; collect_coverage once.
    assert_eq!(r.ignored_files, 2);
    assert_eq!(r.ignored_external, 1);
    // //docs:gen and //lib:x, whose Bazel answers name a generated file, are
    // not failures on its account.
    let failing: Vec<&str> = r.failures.iter().map(|f| f.label.as_str()).collect();
    assert_eq!(failing, vec!["//lib:x"]);
    assert_eq!(r.failures[0].missing, vec!["//tests:b"]);
}

#[test]
fn a_round_trip_through_an_external_stays_unsound_and_is_named() {
    let r = run();
    assert_eq!(r.unsound, 1);
    assert!(!r.is_sound());
    let f = &r.failures[0];
    assert_eq!(f.round_trip, vec!["//tests:b"]);
    assert_eq!(f.via, vec![COVERAGE]);
}

#[test]
fn a_toolchain_type_escalates_rather_than_failing() {
    let r = run();
    assert_eq!(r.escalations.len(), 1);
    let e = &r.escalations[0];
    assert_eq!(e.label, "//tc:type");
    assert_eq!(e.rule_class, "toolchain_type");
    assert_eq!(e.code, "CAMBI-W023");
    assert_eq!(e.missing, vec!["//tests:a"]);
}

#[test]
fn precision_is_the_share_of_ours_that_bazel_reports() {
    let r = run();
    // Ours: //lib:x -> {x, a, c, gen}, and each other sample itself.
    assert_eq!(r.ours_total, 9);
    // Theirs, rules in the main repository only: //lib:x -> {x, a, b, gen},
    // //tc:type -> {type, a}, the rest themselves.
    assert_eq!(r.theirs_total, 10);
    // //tests:c is the over-approximation; //tests:b and the toolchain
    // resolution edge are Bazel's alone.
    assert_eq!(r.agreed_total, 8);
    assert!((r.precision_pct - 800.0 / 9.0).abs() < 1e-9);
    assert!(r.precision_pct <= 100.0);
}

#[test]
fn the_document_carries_the_new_fields() {
    let v = serde_json::to_value(run()).unwrap();
    for field in [
        "schema",
        "checked",
        "unsound",
        "ours_total",
        "theirs_total",
        "agreed_total",
        "precision_pct",
        "ignored_files",
        "ignored_external",
        "failures",
        "escalations",
    ] {
        assert!(v.get(field).is_some(), "{field} is missing: {v}");
    }
    assert_eq!(v["schema"], cambium_engine::report::SCHEMA_VERSION);
    for field in ["label", "missing", "round_trip", "via"] {
        assert!(v["failures"][0].get(field).is_some(), "failures[].{field}");
    }
}
