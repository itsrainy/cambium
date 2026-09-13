//! A subdirectory of the main repository that is a Bazel repository of its
//! own.
//!
//! `bazel query //...` stops at tensorflow's `third_party/xla`, which carries
//! a `WORKSPACE` and is the repository `@xla`, so the index has no package
//! under it. What the engine must not do with a change there is read it as a
//! main-repo path: that mints packages Bazel will not load, and the run that
//! queries them reports nothing affected. What it must do instead is move the
//! repository the directory is, or say it cannot.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::FixedText;
use cambium_index::externals::PathMap;
use cambium_index::{IndexBuilder, IndexData, PackageUpdate, TargetRow};
use cambium_testkit::{base, engine, req, Files, Scanning};

const BOUNDARY: &str = "third_party/xla";

fn files() -> Files {
    let mut m = BTreeMap::new();
    m.insert("app/BUILD".to_string(), String::new());
    m.insert("lib/BUILD".to_string(), String::new());
    Files(m)
}

fn rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        // `//lib:lib` names a target in the nested repository, which is the
        // edge the reverse walk follows.
        "lib" => vec![TargetRow::rule(
            "//lib:lib",
            "demo_lib",
            vec!["@xla//:thing".into()],
        )],
        "app" => vec![TargetRow::rule(
            "//app:app",
            "demo_lib",
            vec!["//lib:lib".into()],
        )],
        _ => Vec::new(),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

fn scanning() -> Scanning {
    Scanning::new(&["app", "lib"], rows).with_files(files())
}

/// The cold index, plus the two things a real cold build in tensorflow's tree
/// would have recorded: the boundary, and (when `mapped`) the mapping that
/// says `@xla` *is* that directory.
fn index(mapped: bool) -> IndexData {
    let (_e, cold) = base(&scanning());
    let mut b = IndexBuilder::from_view(cold.view());
    b.set_repo_boundaries(vec![BOUNDARY.to_string()]);
    let mut c = b.take_bzl_closure().expect("the cold build read the tree");
    if mapped {
        c.inputs.repos.insert("xla".to_string());
        c.inputs
            .path_maps
            .entry("xla".to_string())
            .or_default()
            .insert(PathMap::exhaustive_dir(format!("{BOUNDARY}/")));
    }
    b.set_bzl_closure(c);
    b.finish("c0")
}

fn run(index: &IndexData, paths: &[&str]) -> cambium_engine::AffectedReport {
    let changes: Vec<Change> = paths
        .iter()
        .map(|p| Change::new(Status::Modified, *p))
        .collect();
    engine()
        .compute_with_texts(
            index.view(),
            &changes,
            &scanning(),
            &req(),
            &FixedText(BTreeMap::new()),
        )
        .unwrap()
        .report
}

#[test]
fn a_change_in_a_mapped_nested_repository_moves_the_repository_it_is() {
    let index = index(true);
    let r = run(
        &index,
        &[
            "third_party/xla/service/BUILD",
            "third_party/xla/service/gpu.cc",
        ],
    );
    assert!(!r.global, "{:?}", r.global_reason);
    // No package was minted from either path, so nothing unloadable is
    // queried and nothing is tombstoned.
    assert_eq!(r.stats.dirty_packages, 0, "{:?}", r.dirty_packages);
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    // Rule (1): the leaf `@xla//:thing` is seeded, and the walk goes up.
    assert_eq!(r.direct, vec!["//lib:lib".to_string()]);
    assert_eq!(
        r.affected_labels(),
        vec!["//app:app".to_string(), "//lib:lib".to_string()]
    );
}

#[test]
fn a_change_in_an_unmapped_nested_repository_is_global_and_says_which_directory() {
    let index = index(false);
    let r = run(&index, &["third_party/xla/service/gpu.cc"]);
    assert!(r.global);
    let why = r.global_reason.unwrap_or_default();
    assert!(why.contains(BOUNDARY), "{why}");
    assert!(why.contains("Bazel repository of its own"), "{why}");
}

#[test]
fn a_change_outside_every_boundary_is_unaffected_by_one() {
    let index = index(true);
    let r = run(&index, &["lib/a.cc"]);
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_input, 0);
}
