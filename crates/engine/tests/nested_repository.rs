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
use cambium_engine::AffectedReport;
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
    b.set_repo_boundaries(vec![cambium_index::Boundary {
        dir: BOUNDARY.to_string(),
        bazelignored: false,
    }]);
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

// ------------------------------------------------ a boundary .bazelignore names

/// envoy's shape: `api/` is `.bazelignore`d, carries a `MODULE.bazel` of its
/// own, and is the repository `@envoy_api` half the tree is built from. The
/// tree is loaded in the WORKSPACE world, where the directory is named by a
/// repository rule's own attribute rather than by `path`.
const ENVOY_EXTERNAL_RULES: &str = r#"# /ws/bazel/api_binding.bzl:20:19
_default_envoy_api(
  name = "envoy_api",
  generator_name = "envoy_api",
  envoy_root = "@envoy//:VERSION.txt",
  reldir = "api",
)
"#;

fn envoy_rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "lib" => vec![TargetRow::rule(
            "//lib:lib",
            "demo_lib",
            vec!["@envoy_api//envoy/service/ext_proc/v3:pkg".into()],
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

/// The tree on disk that the boundary walk reads: two ignored directories
/// that are repositories of their own, one of which a repository rule names.
fn envoy_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join(".bazelignore"), "api\ndocs\n").unwrap();
    for d in ["api", "docs"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
        std::fs::write(root.join(d).join("MODULE.bazel"), "").unwrap();
    }
    std::fs::write(root.join("WORKSPACE"), "").unwrap();
    dir
}

fn envoy_scanning(root: &std::path::Path) -> Scanning {
    let mut m = BTreeMap::new();
    m.insert("WORKSPACE".to_string(), String::new());
    // The tree is loaded in the WORKSPACE world even though `MODULE.bazel`
    // exists, which is what envoy's own bazelrc says.
    m.insert(
        ".bazelrc".to_string(),
        "common --noenable_bzlmod --enable_workspace\n".to_string(),
    );
    m.insert("app/BUILD".to_string(), String::new());
    m.insert("lib/BUILD".to_string(), String::new());
    Scanning::new(&["app", "lib"], envoy_rows)
        .with_files(Files(m))
        .with_workspace(root)
        .with_external_rules(ENVOY_EXTERNAL_RULES)
}

fn envoy_run(index: &IndexData, root: &std::path::Path, paths: &[&str]) -> AffectedReport {
    let changes: Vec<Change> = paths
        .iter()
        .map(|p| Change::new(Status::Modified, *p))
        .collect();
    engine()
        .compute_with_texts(
            index.view(),
            &changes,
            &envoy_scanning(root),
            &req(),
            &FixedText(BTreeMap::new()),
        )
        .unwrap()
        .report
}

#[test]
fn a_change_in_an_ignored_boundary_a_rule_names_moves_the_repository_it_is() {
    let dir = envoy_tree();
    let (_e, index) = base(&envoy_scanning(dir.path()));
    // The walk recorded both ignored repositories, and said they are ignored.
    assert_eq!(
        index.view().repo_boundaries().collect::<Vec<_>>(),
        vec!["api", "docs"]
    );
    assert!(index.view().boundary_is_bazelignored("api"));

    let r = envoy_run(
        &index,
        dir.path(),
        &["api/envoy/service/ext_proc/v3/external_processor.proto"],
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.dirty_packages, 0, "{:?}", r.dirty_packages);
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    assert_eq!(r.direct, vec!["//lib:lib".to_string()]);
    assert_eq!(
        r.affected_labels(),
        vec!["//app:app".to_string(), "//lib:lib".to_string()]
    );
}

#[test]
fn a_change_in_an_ignored_boundary_nothing_names_is_ignored_rather_than_global() {
    let dir = envoy_tree();
    let (_e, index) = base(&envoy_scanning(dir.path()));
    let r = envoy_run(&index, dir.path(), &["docs/root/intro/arch_overview.rst"]);
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.ignored_files, 1);
    assert!(r.direct.is_empty(), "{:?}", r.direct);
}
