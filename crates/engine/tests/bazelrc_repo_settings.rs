//! A `.bazelrc` line that resolves an external repository's build setting to
//! a target of this tree is an input of that repository.
//!
//! This is the whole of the round trip at envoy's bzlmod pin: `@quiche` is a
//! registry `bazel_dep` whose overlay BUILD file lives in the registry and
//! names nothing of this tree, and the only edge back into the main
//! repository is 15 `build --@quiche//:X=//source/common/quic/...` lines.
//! Neither `bazel query` nor the rdeps oracle can see it: the flag is read
//! while the options are parsed, and only `cquery` prints the edge.
//!
//! Recorded as a target-only input, so the answer is rule (1) plus the
//! repository fixpoint — wide by `@quiche`, which is the sound direction
//! until the flag's readers are known.
//!
//! `bazelrc_flags.rs` beside this is the other half of the same file: what a
//! `.bazelrc` *edit* reaches. This one is about an rc file that does not move
//! while the target it names does.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::{FixedText, WorkspaceText};
use cambium_engine::Code;
use cambium_index::{IndexData, PackageUpdate, TargetRow};
use cambium_testkit::{base, engine, req, Files, Scanning};

/// `quiche` is a plain `bazel_dep`: nothing in the module file or in any
/// `.bzl` of the closure says a thing about this tree.
const MODULE: &str = "module(name = \"demo\", version = \"1\")\n\
     bazel_dep(name = \"quiche\", version = \"1.0\")\n";

/// Three settings and three things that are not one: a flag of the main
/// repository under its own apparent name, a value that is not a label, and a
/// repository flag on a command that is not `build` or `common`.
const BAZELRC: &str = "# envoy's shape, one line per flag\n\
     build --@quiche//:base_impl=//platform:base\n\
     common:asan --@quiche//:tls_impl=@demo//platform:tls\n\
     build --@demo//:own_flag=//platform:base\n\
     build --@quiche//:count=3\n\
     test --@quiche//:test_only=//platform:untouched\n";

fn files(rc: &str) -> Files {
    let mut m = BTreeMap::new();
    m.insert("MODULE.bazel".to_string(), MODULE.to_string());
    m.insert(".bazelrc".to_string(), rc.to_string());
    m.insert("app/BUILD".to_string(), String::new());
    m.insert("lib/BUILD".to_string(), String::new());
    m.insert("platform/BUILD".to_string(), String::new());
    Files(m)
}

/// ```text
/// //app:app -> //lib:lib
/// //lib:lib -> @quiche//:thing
/// //platform:base -> platform/base.cc
/// ```
fn rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "app" => vec![TargetRow::rule(
            "//app:app",
            "demo_lib",
            vec!["//lib:lib".into()],
        )],
        "lib" => vec![TargetRow::rule(
            "//lib:lib",
            "demo_lib",
            vec!["@quiche//:thing".into()],
        )],
        "platform" => vec![
            TargetRow::source_file("//platform:base.cc"),
            TargetRow::source_file("//platform:tls.cc"),
            TargetRow::source_file("//platform:untouched.cc"),
            TargetRow::rule(
                "//platform:base",
                "demo_lib",
                vec!["//platform:base.cc".into()],
            ),
            TargetRow::rule(
                "//platform:tls",
                "demo_lib",
                vec!["//platform:tls.cc".into()],
            ),
            TargetRow::rule(
                "//platform:untouched",
                "demo_lib",
                vec!["//platform:untouched.cc".into()],
            ),
        ],
        _ => Vec::new(),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

const PACKAGES: &[&str] = &["app", "lib", "platform"];

const REPO_MAPPING: &str = "{\"\":\"\",\"demo\":\"\",\"quiche\":\"quiche+\"}";

fn scanning_with(rc: &str) -> Scanning {
    Scanning::new(PACKAGES, rows)
        .with_files(files(rc))
        .with_repo_mapping(REPO_MAPPING)
}

fn scanning() -> Scanning {
    scanning_with(BAZELRC)
}

fn texts() -> FixedText {
    let mut m = BTreeMap::new();
    for rev in ["base", "head"] {
        m.insert(
            (rev.to_string(), "MODULE.bazel".to_string()),
            MODULE.to_string(),
        );
    }
    FixedText(m)
}

fn changed(paths: &[&str]) -> Vec<Change> {
    paths
        .iter()
        .map(|p| Change::new(Status::Modified, *p))
        .collect()
}

fn run(index: &IndexData, q: &Scanning, paths: &[&str]) -> cambium_engine::AffectedReport {
    let e = engine();
    let t: &dyn WorkspaceText = &texts();
    e.compute_with_texts(index.view(), &changed(paths), q, &req(), t)
        .unwrap()
        .report
}

/// The pin's shape: the file behind `//platform:base` moves, the flag line
/// makes that target an input of `@quiche`, and the repository fixpoint
/// carries it to the repository's consumers.
#[test]
fn a_target_a_bazelrc_line_names_moves_the_repository_it_configures() {
    let q = scanning();
    let (_e, index) = base(&q);
    let r = run(&index, &q, &["platform/base.cc"]);
    assert!(!r.global, "{:?}", r.global_reason);
    // Nothing in the diff named a repository: the walk did.
    assert_eq!(r.stats.repos_dirty_by_input, 0);
    assert_eq!(r.stats.repos_dirty_by_target, 1);
    let a = r.affected_labels();
    assert!(a.contains(&"//lib:lib".to_string()), "{a:?}");
    assert!(a.contains(&"//app:app".to_string()), "and above it: {a:?}");
    assert!(
        r.direct.contains(&"//lib:lib".to_string()),
        "the repository's label consumer is direct: {:?}",
        r.direct
    );
}

/// The same line under a `--config`, and the main repository under its own
/// apparent name on the value side. A config-scoped line is read until the
/// screen that models config scoping lands: wide, not wrong.
#[test]
fn a_config_scoped_line_and_an_apparent_main_repo_value_are_read_too() {
    let q = scanning();
    let (_e, index) = base(&q);
    let r = run(&index, &q, &["platform/tls.cc"]);
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_target, 1);
    assert!(
        r.affected_labels().contains(&"//app:app".to_string()),
        "{:?}",
        r.affected_labels()
    );
}

/// A target no line names reaches no repository. Without this the test above
/// would pass on any wide answer at all.
#[test]
fn a_target_no_line_names_moves_no_repository() {
    let q = scanning();
    let (_e, index) = base(&q);
    let r = run(&index, &q, &["platform/untouched.cc"]);
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_target, 0);
    assert_eq!(r.stats.repos_dirty_by_input, 0);
    assert!(
        !r.affected_labels().contains(&"//lib:lib".to_string()),
        "a `test` line is not a build's configuration: {:?}",
        r.affected_labels()
    );
}

/// With no such line the same edit reaches nothing, which is what the
/// bazelrc is buying.
#[test]
fn without_the_line_the_same_edit_reaches_no_repository() {
    let q = scanning_with("build --config=none\n");
    let (_e, index) = base(&q);
    let r = run(&index, &q, &["platform/base.cc"]);
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_target, 0);
    assert!(
        !r.affected_labels().contains(&"//lib:lib".to_string()),
        "{:?}",
        r.affected_labels()
    );
}

/// The reading is actionable: the answer is wide by a whole repository, and
/// the message says which flag bought it.
#[test]
fn each_line_read_is_reported_once_with_its_flag() {
    let q = scanning();
    let _ = base(&q);
    let d = q.diags().drain();
    let read: Vec<&cambium_engine::Diagnostic> = d
        .iter()
        .filter(|d| d.code == Code::W027BuildSettingCrossesRepository)
        .collect();
    assert_eq!(read.len(), 2, "{d:?}");
    assert!(read[0].message.contains("@quiche//:base_impl"), "{read:?}");
    assert!(read[0].message.contains("@quiche"), "{read:?}");
    assert!(read[0].help.is_some(), "{read:?}");
}
