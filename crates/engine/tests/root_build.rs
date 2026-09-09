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
use cambium_engine::query::Rows;
use cambium_engine::{AffectedRequest, Config, Engine, EngineError, PackageQuerier};
use cambium_index::{IndexData, PackageUpdate, TargetRow};

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

struct Synth;

impl PackageQuerier for Synth {
    fn query(&self, packages: &[String]) -> Result<Rows, EngineError> {
        Ok(packages.iter().map(|p| rows(p)).collect::<Vec<_>>().into())
    }

    fn universe(&self) -> Result<Rows, EngineError> {
        Ok(PACKAGES.iter().map(|p| rows(p)).collect::<Vec<_>>().into())
    }
}

fn engine() -> Engine {
    let dir = std::env::temp_dir().join("cambium-tests");
    let _ = std::fs::create_dir_all(&dir);
    Engine::with_parts(
        Config::default(),
        cambium_diff::Repo::new(dir.clone()),
        cambium_bazel::Bazel::at(dir, Some("/nonexistent/bazel")),
    )
}

fn req() -> AffectedRequest {
    AffectedRequest {
        base: "base".into(),
        head: "head".into(),
        include_manual: true,
        explain: false,
        no_full_build: true,
        materialize: false,
        publish_base: false,
        max_depth: None,
        allow_partial: false,
    }
}

fn base() -> IndexData {
    engine().full_build_with("c0", &Synth).unwrap()
}

fn run(index: &IndexData, path: &str) -> cambium_engine::AffectedReport {
    engine()
        .compute(
            index.view(),
            &[Change::new(Status::Modified, path)],
            &Synth,
            &req(),
        )
        .unwrap()
        .report
}

#[test]
fn a_root_build_edit_dirties_the_root_package_only() {
    let index = base();
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
    let index = base();
    let r = run(&index, "BUILD.bazel");
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.dirty_packages, vec![String::new()]);
}

/// The reason that used to be attached to the root BUILD file belongs to
/// `REPO.bazel`, which really does set defaults for every package.
#[test]
fn repo_bazel_is_still_an_invalidator() {
    let index = base();
    let r = run(&index, "REPO.bazel");
    assert!(r.global);
}
