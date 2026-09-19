//! A repository rule that says which directory of this tree it is, and the
//! probe that decides whether to believe it.
//!
//! xla declares `@tsl` with `tf_vendored(name = "tsl", path =
//! "third_party/tsl")` inside a `module_extension` implementation. The
//! directory is a repository boundary, so `//...` stops there and every
//! change under it used to be global. The `path =` attribute says where the
//! repository is, but the rule behind it is arbitrary Starlark, so the claim
//! is checked before it is used: `bazel query '@tsl//...' --output=package`
//! has to list exactly the BUILD-bearing directories under `third_party/tsl`.
//! Confirmed, a change there is an input edit of one repository; refused, it
//! stays global and says why.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::diag::Code;
use cambium_engine::externals::FixedText;
use cambium_engine::AffectedReport;
use cambium_index::{IndexData, PackageUpdate, TargetRow};
use cambium_engine::PackageQuerier;
use cambium_testkit::{engine, req, Files, Scanning};

const DIR: &str = "third_party/tsl";

/// The extension whose implementation lambda holds the call.
const REPO_BZL: &str = "def _tsl_impl(mctx):\n\
     \x20   tf_vendored(name = \"tsl\", path = \"third_party/tsl\")\n\
     tsl_extension = module_extension(implementation = _tsl_impl)\n";

const MODULE_BAZEL: &str = "module(name = \"xla\")\n\
     tsl_extension = use_extension(\"//third_party:repo.bzl\", \"tsl_extension\")\n\
     use_repo(tsl_extension, \"tsl\")\n";

fn rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "lib" => vec![TargetRow::rule(
            "//lib:lib",
            "demo_lib",
            vec!["@tsl//tsl/platform:dso_loader".into()],
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

/// The tree the boundary walk and the C2 comparison read: `third_party/tsl`
/// is a repository of its own with two packages in it.
fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("MODULE.bazel"), MODULE_BAZEL).unwrap();
    std::fs::create_dir_all(root.join("third_party/tsl/tsl/platform")).unwrap();
    std::fs::write(root.join(DIR).join("REPO.bazel"), "# the marker\n").unwrap();
    std::fs::write(root.join(DIR).join("BUILD"), "").unwrap();
    std::fs::write(root.join(DIR).join("tsl/platform/BUILD"), "").unwrap();
    dir
}

/// The tree's packages as the probe would print them, root first.
const PROBED: &[&str] = &["", "tsl/platform"];

fn scanning(root: &std::path::Path, packages: &[&str], load_error: bool) -> Scanning {
    let mut m = BTreeMap::new();
    m.insert("MODULE.bazel".to_string(), MODULE_BAZEL.to_string());
    m.insert("third_party/repo.bzl".to_string(), REPO_BZL.to_string());
    m.insert("app/BUILD".to_string(), String::new());
    m.insert("lib/BUILD".to_string(), String::new());
    Scanning::new(&["app", "lib"], rows)
        .with_files(Files(m))
        .with_workspace(root)
        .with_repo_probe("tsl", packages, load_error)
}

fn run(index: &IndexData, root: &std::path::Path, packages: &[&str]) -> AffectedReport {
    let changes = vec![Change::new(Status::Modified, "third_party/tsl/tsl/platform/x.cc")];
    engine()
        .compute_with_texts(
            index.view(),
            &changes,
            &scanning(root, packages, false),
            &req(),
            &FixedText(BTreeMap::new()),
        )
        .unwrap()
        .report
}

#[test]
fn a_claim_bazels_own_package_list_agrees_with_maps_the_directory() {
    let dir = tree();
    let q = scanning(dir.path(), PROBED, false);
    let e = engine();
    let index = e.full_build_with("c0", &q).unwrap();
    // The probe ran, for the repository the claim names.
    assert_eq!(q.probed(), vec!["tsl".to_string()]);
    let v = index.view();
    let repo = (0..v.num_repos()).find(|&r| v.repo_name(r) == "tsl").unwrap();
    let origin = v.repo_origin(repo);
    assert_eq!(origin.dir, "third_party/tsl/");
    assert!(origin.confirmed, "{origin:?}");
    assert_eq!(
        v.repo_path_maps(repo)
            .iter()
            .map(|m| m.from.clone())
            .collect::<Vec<_>>(),
        vec!["third_party/tsl/".to_string()]
    );

    // And so a change under the boundary moves the repository it is, rather
    // than the whole world.
    let r = run(&index, dir.path(), PROBED);
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    assert_eq!(r.stats.dirty_packages, 0, "{:?}", r.dirty_packages);
    assert_eq!(r.direct, vec!["//lib:lib".to_string()]);
    assert_eq!(
        r.affected_labels(),
        vec!["//app:app".to_string(), "//lib:lib".to_string()]
    );
}

#[test]
fn a_claim_bazel_disagrees_with_is_refused_and_the_directory_stays_global() {
    let dir = tree();
    // Bazel says the repository has a package the directory does not: the
    // rule put the repository somewhere else, or read `path` as something
    // other than the repository's root.
    let probed = &["", "tsl/platform", "elsewhere"];
    let q = scanning(dir.path(), probed, false);
    let e = engine();
    let index = e.full_build_with("c0", &q).unwrap();
    let v = index.view();
    let repo = (0..v.num_repos()).find(|&r| v.repo_name(r) == "tsl");
    assert!(
        repo.is_none_or(|r| !v.repo_origin(r).confirmed && v.repo_path_maps(r).is_empty()),
        "an unconfirmed claim minted a mapping"
    );
    let codes: Vec<Code> = q.diagnostics().unwrap().drain().iter().map(|d| d.code).collect();
    assert!(
        codes.contains(&Code::W026RepoOriginUnconfirmed),
        "{codes:?}"
    );

    let r = run(&index, dir.path(), probed);
    assert!(r.global, "{:?}", r.direct);
}

#[test]
fn a_repository_that_did_not_load_is_not_indexed_and_says_so() {
    let dir = tree();
    // C1: the package list is there but a package of the repository failed to
    // load, so the list is an undercount and cannot confirm anything.
    let q = scanning(dir.path(), PROBED, true);
    let e = engine();
    let index = e.full_build_with("c0", &q).unwrap();
    let v = index.view();
    let repo = (0..v.num_repos()).find(|&r| v.repo_name(r) == "tsl");
    assert!(repo.is_none_or(|r| !v.repo_origin(r).confirmed));
    let codes: Vec<Code> = q.diagnostics().unwrap().drain().iter().map(|d| d.code).collect();
    assert!(codes.contains(&Code::W025RepoNotIndexed), "{codes:?}");
}

#[test]
fn a_repository_over_the_cap_keeps_the_wide_answer() {
    let dir = tree();
    let q = scanning(dir.path(), PROBED, false);
    let cfg = cambium_engine::Config {
        externals: cambium_engine::config::Externals {
            index_max_packages: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let e = cambium_testkit::engine_with(cfg);
    let index = e.full_build_with("c0", &q).unwrap();
    let v = index.view();
    let repo = (0..v.num_repos()).find(|&r| v.repo_name(r) == "tsl");
    assert!(repo.is_none_or(|r| !v.repo_origin(r).confirmed));
    let codes: Vec<Code> = q.diagnostics().unwrap().drain().iter().map(|d| d.code).collect();
    assert!(codes.contains(&Code::W025RepoNotIndexed), "{codes:?}");
}
