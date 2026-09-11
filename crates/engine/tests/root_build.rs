//! The root `BUILD` file is an ordinary BUILD file.
//!
//! It was a default invalidator, on the grounds that it can carry repo-wide
//! `package()` settings. It cannot: a `package()` call applies to its own
//! package and no other, and repository-wide defaults are what `REPO.bazel`'s
//! `repo()` function is for — which is now the entry that carries the reason.
//! Everything else a root BUILD file does is an ordinary target in the root
//! package: `exports_files` makes `//:LICENSE` a source the graph has edges
//! to, and a `package_group` is a target whose consumers the graph knows.
//!
//! So an edit to it must dirty the root package and nothing else, and reach
//! only what depends on the root package's targets.

use cambium_diff::{Change, Status};
use cambium_index::{IndexData, PackageUpdate, TargetRow};
use cambium_testkit::{base, engine, req, Scanning};

/// ```text
/// //:LICENSE, //:root_group     (the root package)
/// //app:app -> //:LICENSE
/// //lib:lib -> //app:app
/// //far:far                     (nothing to do with the root package)
/// ```
fn rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "" => vec![
            TargetRow::source_file("//:LICENSE"),
            TargetRow::rule("//:root_group", "package_group", Vec::new()),
        ],
        "app" => vec![TargetRow::rule(
            "//app:app",
            "demo_lib",
            vec!["//:LICENSE".into()],
        )],
        "lib" => vec![TargetRow::rule(
            "//lib:lib",
            "demo_lib",
            vec!["//app:app".into()],
        )],
        "far" => vec![TargetRow::rule("//far:far", "demo_lib", Vec::new())],
        _ => Vec::new(),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

const PACKAGES: &[&str] = &["", "app", "lib", "far"];

fn synth() -> Scanning {
    Scanning::new(PACKAGES, rows)
}

fn run(index: &IndexData, path: &str) -> cambium_engine::AffectedReport {
    engine()
        .compute(
            index.view(),
            &[Change::new(Status::Modified, path)],
            &synth(),
            &req(),
        )
        .unwrap()
        .report
}

#[test]
fn a_root_build_edit_dirties_the_root_package_only() {
    let (_e, index) = base(&synth());
    let r = run(&index, "BUILD");
    assert!(
        !r.global,
        "the root BUILD file used to be a default invalidator: {:?}",
        r.global_reason
    );
    assert_eq!(r.dirty_packages, vec![String::new()]);
    // `//far:far` has nothing to do with the root package and must not be
    // reported; the re-query returned the same rows, so nothing moved at all.
    assert!(
        !r.affected_labels().iter().any(|l| l == "//far:far"),
        "{:?}",
        r.affected_labels()
    );
}

#[test]
fn the_same_holds_for_a_root_build_bazel() {
    let (_e, index) = base(&synth());
    let r = run(&index, "BUILD.bazel");
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.dirty_packages, vec![String::new()]);
}

/// The reason that used to be attached to the root BUILD file belongs to
/// `REPO.bazel`, which really does set defaults for every package.
#[test]
fn repo_bazel_is_still_an_invalidator() {
    let (_e, index) = base(&synth());
    let r = run(&index, "REPO.bazel");
    assert!(r.global);
}
