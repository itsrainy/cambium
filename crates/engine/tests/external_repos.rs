//! An edit that only matters through an external repository invalidates the
//! targets that depend on that repository, not the world.
//!
//! Three shapes, all driven with no Bazel process (the engine's handle points
//! at `/nonexistent/bazel`, so shelling out would fail the test rather than
//! pass it quietly):
//!
//! - a main-repo file a repository rule reads by label (`config.yml`,
//!   `data/**`) — the `@envoy_repo` shape;
//! - a `MODULE.bazel` dependency bump;
//! - a `.bzl` that defines a repository rule.
//!
//! and the two escalations that must survive: a toolchain-registering module,
//! and a legacy `WORKSPACE`.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::{FixedText, WorkspaceText};
use cambium_engine::query::Universe;
use cambium_engine::{AffectedRequest, Config, Engine, EngineError, PackageQuerier};
use cambium_index::{FileText, IndexData, PackageUpdate, TargetRow};

/// The base `MODULE.bazel`.
///
/// `gen` is a repository the main repository defines itself, in
/// `//bazel:ext.bzl`; `fmt` is an ordinary dependency; `rules_python`
/// registers toolchains.
const MODULE_BASE: &str = "module(name = \"demo\", version = \"1\")\n\
     bazel_dep(name = \"fmt\", version = \"1.0\")\n\
     bazel_dep(name = \"rules_python\", version = \"1.0\")\n\
     gen = use_extension(\"//bazel:ext.bzl\", \"gen\")\n\
     use_repo(gen, \"generated\")\n";

struct Files(BTreeMap<String, String>);

impl FileText for Files {
    fn text(&self, path: &str) -> Option<String> {
        self.0.get(path).cloned()
    }
}

fn files() -> Files {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: &str| m.insert(k.to_string(), v.to_string());
    put("MODULE.bazel", MODULE_BASE);
    // The repository rule's implementation, reached from MODULE.bazel through
    // `use_extension` and one `load()`. Both of its main-repo labels are
    // inputs of `@generated`.
    put("bazel/ext.bzl", "load(\"//bazel:repo.bzl\", \"r\")\n");
    put(
        "bazel/repo.bzl",
        "CONFIG = attr.label(default = \"@demo//:config.yml\")\n\
         DATA = \"@demo//data\"\n",
    );
    put("app/BUILD", "load(\"//bazel:defs.bzl\", \"r\")\n");
    put("lib/BUILD", "load(\"//bazel:defs.bzl\", \"r\")\n");
    put("bazel/defs.bzl", "R = 1\n");
    Files(m)
}

/// ```text
/// //app:app -> //lib:lib, @@fmt+//:fmt
/// //lib:lib -> @generated//:thing
/// ```
fn rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "app" => vec![TargetRow::rule(
            "//app:app",
            "demo_lib",
            vec!["//lib:lib".into(), "@@fmt+//:fmt".into()],
        )],
        "lib" => vec![TargetRow::rule(
            "//lib:lib",
            "demo_lib",
            vec!["@generated//:thing".into()],
        )],
        _ => Vec::new(),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

const PACKAGES: &[&str] = &["app", "lib"];

struct Scanning(Files);

impl PackageQuerier for Scanning {
    fn query(&self, packages: &[String]) -> Result<Vec<PackageUpdate>, EngineError> {
        Ok(packages.iter().map(|p| rows(p)).collect())
    }

    fn universe(&self) -> Result<Universe, EngineError> {
        Ok(PACKAGES.iter().map(|p| rows(p)).collect::<Vec<_>>().into())
    }

    fn load_source(&self) -> Option<&dyn FileText> {
        Some(&self.0)
    }

    /// What `bazel mod dump_repo_mapping ""` prints for this workspace: the
    /// root module's own name maps to the empty canonical name (so `@demo//`
    /// is the main repository), and `fmt` carries the `+` suffix Bazel 8 and 9
    /// give a canonical name.
    fn repo_mapping(&self) -> cambium_index::RepoMapping {
        cambium_index::RepoMapping::from_dump(
            "{\"\":\"\",\"demo\":\"\",\"fmt\":\"fmt+\",\"generated\":\"generated+\"}",
        )
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
    }
}

fn base() -> (Engine, IndexData) {
    let e = engine();
    let index = e.full_build_with("c0", &Scanning(files())).unwrap();
    (e, index)
}

/// `MODULE.bazel` unchanged between the two revisions unless a test says so.
fn texts(base_module: &str, head_module: &str) -> FixedText {
    let mut m = BTreeMap::new();
    m.insert(
        ("base".to_string(), "MODULE.bazel".to_string()),
        base_module.to_string(),
    );
    m.insert(
        ("head".to_string(), "MODULE.bazel".to_string()),
        head_module.to_string(),
    );
    FixedText(m)
}

fn run(
    index: &IndexData,
    changes: &[Change],
    texts: &dyn WorkspaceText,
) -> cambium_engine::AffectedReport {
    let e = engine();
    e.compute_with_texts(index.view(), changes, &Scanning(files()), &req(), texts)
        .unwrap()
        .report
}

fn changed(paths: &[&str]) -> Vec<Change> {
    paths
        .iter()
        .map(|p| Change::new(Status::Modified, *p))
        .collect()
}

fn affected(r: &cambium_engine::AffectedReport) -> Vec<String> {
    r.affected_labels()
}

#[test]
fn the_index_stores_what_feeds_each_repository() {
    let (_e, index) = base();
    let v = index.view();
    let repos: Vec<&str> = (0..v.num_repos()).map(|r| v.repo_name(r)).collect();
    assert_eq!(repos, ["generated"]);
    let named = |p: &str| -> Vec<&str> {
        v.repos_for_path(p)
            .into_iter()
            .map(|r| v.repo_name(r))
            .collect()
    };
    assert_eq!(named("config.yml"), ["generated"]);
    assert_eq!(
        named("data/thing.txt"),
        ["generated"],
        "a package label is a prefix"
    );
    assert!(named("app/main.cc").is_empty());
    // The repository rule's own `.bzl` files name the repository they define,
    // which is what stops a change to them from being global.
    let repo_bzl = v.bzl_id("bazel/repo.bzl").unwrap();
    assert!(v.bzl_in_workspace(repo_bzl));
    assert_eq!(
        v.bzl_repos_of(repo_bzl)
            .iter()
            .map(|&r| v.repo_name(r))
            .collect::<Vec<_>>(),
        ["generated"]
    );
}

#[test]
fn a_repository_rule_input_reaches_only_the_repositorys_consumers() {
    let (_e, index) = base();
    let r = run(
        &index,
        &changed(&["config.yml"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    // `//lib:lib` names `@generated//:thing`; `//app:app` reaches it by one hop.
    assert_eq!(r.direct, vec!["//lib:lib".to_string()]);
    assert_eq!(
        affected(&r),
        vec!["//app:app".to_string(), "//lib:lib".to_string()]
    );
}

#[test]
fn a_file_under_a_repository_rules_package_label_counts_too() {
    let (_e, index) = base();
    let r = run(
        &index,
        &changed(&["data/thing.txt"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.direct, vec!["//lib:lib".to_string()]);
}

#[test]
fn an_unrelated_file_reaches_no_repository() {
    let (_e, index) = base();
    let r = run(
        &index,
        &changed(&["notes.txt"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global);
    assert_eq!(r.stats.repos_dirty_by_input, 0);
    assert!(r.direct.is_empty());
}

#[test]
fn a_bazel_dep_bump_reaches_only_that_dependencys_consumers() {
    let (_e, index) = base();
    let head = MODULE_BASE.replace(
        "bazel_dep(name = \"fmt\", version = \"1.0\")",
        "bazel_dep(name = \"fmt\", version = \"2.0\")",
    );
    let r = run(
        &index,
        &changed(&["MODULE.bazel"]),
        &texts(MODULE_BASE, &head),
    );
    assert!(
        !r.global,
        "MODULE.bazel is a default invalidator, but the stage handled it: {:?}",
        r.global_reason
    );
    assert_eq!(r.stats.repos_dirty_by_module, 1);
    // Only `//app:app` names `@@fmt+//:fmt`, and nothing depends on it.
    assert_eq!(r.direct, vec!["//app:app".to_string()]);
    assert_eq!(affected(&r), vec!["//app:app".to_string()]);
}

#[test]
fn a_canonical_name_matches_the_apparent_one() {
    // `@@fmt+//:fmt` is how Bazel 8 and 9 spell it; the module names `fmt`.
    let (_e, index) = base();
    let head = MODULE_BASE.replace(
        "bazel_dep(name = \"fmt\", version = \"1.0\")",
        "bazel_dep(name = \"fmt\", version = \"9.9\")",
    );
    let r = run(
        &index,
        &changed(&["MODULE.bazel"]),
        &texts(MODULE_BASE, &head),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.direct, vec!["//app:app".to_string()]);
}

#[test]
fn a_toolchain_module_bump_is_global() {
    let (_e, index) = base();
    let head = MODULE_BASE.replace(
        "bazel_dep(name = \"rules_python\", version = \"1.0\")",
        "bazel_dep(name = \"rules_python\", version = \"2.0\")",
    );
    let r = run(
        &index,
        &changed(&["MODULE.bazel"]),
        &texts(MODULE_BASE, &head),
    );
    assert!(r.global);
    let reason = r.global_reason.unwrap();
    assert!(
        reason.contains("rules_python") && reason.contains("toolchain"),
        "{reason}"
    );
}

#[test]
fn a_legacy_workspace_edit_is_global() {
    let (_e, index) = base();
    let r = run(
        &index,
        &changed(&["WORKSPACE"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(r.global);
    assert!(r.global_reason.unwrap().contains("imperative"));
}

#[test]
fn a_lockfile_moving_on_its_own_is_global() {
    let (_e, index) = base();
    let r = run(
        &index,
        &changed(&["MODULE.bazel.lock"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    // Not the stage's doing: it leaves a lone lockfile to the invalidator
    // rules, where `MODULE.bazel.lock` is a default entry.
    assert!(r.global);
    assert!(r.global_reason.unwrap().contains("invalidator"));
}

#[test]
fn a_lockfile_moving_with_the_module_file_adds_nothing() {
    let (_e, index) = base();
    let head = MODULE_BASE.replace(
        "bazel_dep(name = \"fmt\", version = \"1.0\")",
        "bazel_dep(name = \"fmt\", version = \"9.9\")",
    );
    let r = run(
        &index,
        &changed(&["MODULE.bazel", "MODULE.bazel.lock"]),
        &texts(MODULE_BASE, &head),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.direct, vec!["//app:app".to_string()]);
}

#[test]
fn a_repository_rules_bzl_names_its_repository_instead_of_going_global() {
    let (_e, index) = base();
    let r = run(
        &index,
        &changed(&["bazel/repo.bzl"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(
        !r.global,
        "a workspace-level .bzl used to be global: {:?}",
        r.global_reason
    );
    assert_eq!(r.stats.repos_dirty_by_bzl, 1);
    assert_eq!(r.direct, vec!["//lib:lib".to_string()]);
}

#[test]
fn an_unparseable_module_file_is_global() {
    let (_e, index) = base();
    let head = format!("{MODULE_BASE}if x:\n    bazel_dep(name = \"q\")\n");
    let r = run(
        &index,
        &changed(&["MODULE.bazel"]),
        &texts(MODULE_BASE, &head),
    );
    assert!(r.global);
    assert!(r.global_reason.unwrap().contains("cannot parse"));
}

#[test]
fn turning_the_stage_off_restores_whole_world_invalidation() {
    let e = engine();
    let index = e.full_build_with("c0", &Scanning(files())).unwrap();
    let mut cfg = Config::default();
    cfg.externals.enabled = false;
    let dir = std::env::temp_dir().join("cambium-tests");
    let e = Engine::with_parts(
        cfg,
        cambium_diff::Repo::new(dir.clone()),
        cambium_bazel::Bazel::at(dir, Some("/nonexistent/bazel")),
    );
    let out = e
        .compute_with_texts(
            index.view(),
            &changed(&["MODULE.bazel"]),
            &Scanning(files()),
            &req(),
            &texts(MODULE_BASE, MODULE_BASE),
        )
        .unwrap();
    assert!(out.report.global);
    assert!(out.report.global_reason.unwrap().contains("invalidator"));
}
