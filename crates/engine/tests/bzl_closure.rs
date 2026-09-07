//! A changed `.bzl` is resolved from the index's static load closure, with no
//! Bazel process anywhere.
//!
//! This replaces the `rbuildfiles` tests. The querier here has no Bazel in it
//! at all — the engine's `Bazel` handle points at `/nonexistent/bazel`, so any
//! attempt to shell out would fail the test rather than pass it quietly — and
//! the trait no longer has an `rbuildfiles` method to call.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::query::Universe;
use cambium_engine::{AffectedRequest, Config, Engine, EngineError, PackageQuerier};
use cambium_index::{FileText, IndexData, PackageUpdate, TargetRow};

/// An in-memory checkout.
///
/// ```text
/// //app:app  --deps-->  //lib:lib
/// app/BUILD    loads //bazel:defs.bzl  ->  :common.bzl
/// lib/BUILD    loads //bazel:other.bzl
/// MODULE.bazel use_extension("//bazel:ext.bzl")
/// bazel/dead.bzl  loaded by nobody
/// ```
struct Files(BTreeMap<String, String>);

impl FileText for Files {
    fn text(&self, path: &str) -> Option<String> {
        self.0.get(path).cloned()
    }
}

fn files() -> Files {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: &str| m.insert(k.to_string(), v.to_string());
    put(
        "MODULE.bazel",
        "ext = use_extension(\"//bazel:ext.bzl\", \"ext\")\n",
    );
    put("app/BUILD", "load(\"//bazel:defs.bzl\", \"lib_rule\")\n");
    put("lib/BUILD", "load(\"//bazel:other.bzl\", \"lib_rule\")\n");
    put("bazel/BUILD", "exports_files([\"defs.bzl\"])\n");
    put("bazel/defs.bzl", "load(\":common.bzl\", \"c\")\n");
    put("bazel/common.bzl", "C = 1\n");
    put("bazel/other.bzl", "O = 1\n");
    put("bazel/ext.bzl", "E = 1\n");
    put("bazel/dead.bzl", "D = 1\n");
    Files(m)
}

/// Rows for a package. Fixed, because this test is about the closure and not
/// about what a query returns.
fn rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "app" => vec![TargetRow::rule(
            "//app:app",
            "synth_lib",
            vec!["//lib:lib".into()],
        )],
        "lib" => vec![TargetRow::rule("//lib:lib", "synth_lib", Vec::new())],
        _ => Vec::new(),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

const PACKAGES: &[&str] = &["app", "bazel", "lib"];

/// A querier that can read the tree, and so produces an index with a closure.
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
}

/// A querier with no filesystem behind it: the index it builds has no closure,
/// which the engine must treat as "we do not know" rather than "nothing".
struct Blind;

impl PackageQuerier for Blind {
    fn query(&self, packages: &[String]) -> Result<Vec<PackageUpdate>, EngineError> {
        Ok(packages.iter().map(|p| rows(p)).collect())
    }

    fn universe(&self) -> Result<Universe, EngineError> {
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
    }
}

fn base() -> (Engine, IndexData) {
    let e = engine();
    let index = e.full_build_with("c0", &Scanning(files())).unwrap();
    (e, index)
}

fn changed(path: &str) -> Vec<Change> {
    vec![Change::new(Status::Modified, path)]
}

#[test]
fn the_index_stores_the_closure_it_scanned() {
    let (_e, index) = base();
    let v = index.view();
    assert!(v.bzl_closure);
    let paths: Vec<&str> = (0..v.num_bzl()).map(|i| v.bzl_path(i)).collect();
    assert_eq!(
        paths,
        vec![
            "bazel/common.bzl",
            "bazel/defs.bzl",
            "bazel/ext.bzl",
            "bazel/other.bzl"
        ],
        "dead.bzl is loaded by nobody, so it is not in the closure at all"
    );
    let defs = v.bzl_id("bazel/defs.bzl").unwrap();
    let common = v.bzl_id("bazel/common.bzl").unwrap();
    assert_eq!(v.bzl_loads(defs), [common], "direct loads are stored");
    let consumers: Vec<&str> = v
        .bzl_consumers(common)
        .iter()
        .map(|&p| v.package_name(p))
        .collect();
    assert_eq!(consumers, ["app"], "the closure is transitive and inverted");
    assert!(v.bzl_in_workspace(v.bzl_id("bazel/ext.bzl").unwrap()));
    assert!(!v.bzl_in_workspace(common));
}

#[test]
fn a_transitively_loaded_bzl_dirties_exactly_its_consumers() {
    let (e, index) = base();
    let out = e
        .compute(
            index.view(),
            &changed("bazel/common.bzl"),
            &Scanning(files()),
            &req(),
        )
        .unwrap();
    assert!(!out.report.global);
    assert_eq!(out.report.dirty_packages, vec!["app".to_string()]);
    assert_eq!(out.report.stats.bzl_closure_hits, 1);
    assert!(
        !out.report.stats.rbuildfiles_called,
        "the closure answers this; Bazel is never asked"
    );
    // Every rule in the loading package is direct — a `.bzl` edit leaves the
    // rule protos byte-identical — and the walk carries it upward.
    assert_eq!(out.report.direct, vec!["//app:app".to_string()]);
}

#[test]
fn a_bzl_a_dependency_loads_reaches_the_dependent_through_the_walk() {
    let (e, index) = base();
    let out = e
        .compute(
            index.view(),
            &changed("bazel/other.bzl"),
            &Scanning(files()),
            &req(),
        )
        .unwrap();
    assert_eq!(out.report.dirty_packages, vec!["lib".to_string()]);
    assert_eq!(out.report.direct, vec!["//lib:lib".to_string()]);
    assert_eq!(
        out.report.affected_labels(),
        vec!["//app:app".to_string(), "//lib:lib".to_string()]
    );
}

#[test]
fn a_bzl_in_the_workspace_closure_is_global() {
    let (e, index) = base();
    let out = e
        .compute(
            index.view(),
            &changed("bazel/ext.bzl"),
            &Scanning(files()),
            &req(),
        )
        .expect("a workspace-level loader is not an error");
    assert!(out.report.global, "must over-approximate, not miss");
    let reason = out.report.global_reason.as_deref().unwrap();
    assert!(reason.contains("bazel/ext.bzl"), "{reason}");
    assert!(reason.contains("MODULE.bazel/WORKSPACE"), "{reason}");
}

#[test]
fn a_bzl_nothing_loads_dirties_nothing() {
    let (e, index) = base();
    let out = e
        .compute(
            index.view(),
            &changed("bazel/dead.bzl"),
            &Scanning(files()),
            &req(),
        )
        .unwrap();
    assert!(!out.report.global, "a dead .bzl is not a global change");
    assert!(out.report.dirty_packages.is_empty());
    assert!(out.report.affected.is_empty());
    assert_eq!(out.report.stats.bzl_closure_hits, 0);
}

#[test]
fn an_index_without_a_closure_escalates() {
    let e = engine();
    let index = e.full_build_with("c0", &Blind).unwrap();
    assert!(!index.view().bzl_closure);
    let out = e
        .compute(index.view(), &changed("bazel/common.bzl"), &Blind, &req())
        .unwrap();
    assert!(
        out.report.global,
        "an empty table means 'we never looked', not 'nothing loads it'"
    );
    assert!(out
        .report
        .global_reason
        .as_deref()
        .unwrap()
        .contains("no static .bzl load closure"));
}

#[test]
fn a_splice_refreshes_the_closure_of_the_packages_it_requeried() {
    let (e, index) = base();
    // At head, //app also loads //bazel:other.bzl.
    let mut head = files();
    head.0.insert(
        "app/BUILD".into(),
        "load(\"//bazel:defs.bzl\", \"lib_rule\")\nload(\"//bazel:other.bzl\", \"o\")\n".into(),
    );
    let out = e
        .compute(
            index.view(),
            &[Change::new(Status::Modified, "app/BUILD")],
            &Scanning(head),
            &AffectedRequest {
                materialize: true,
                ..req()
            },
        )
        .unwrap();
    let head_index = out.index.expect("materialised");
    let v = head_index.view();
    let other = v.bzl_id("bazel/other.bzl").unwrap();
    let consumers: Vec<&str> = v
        .bzl_consumers(other)
        .iter()
        .map(|&p| v.package_name(p))
        .collect();
    assert_eq!(consumers, ["app", "lib"]);
    // And the unchanged half of the closure survived the splice.
    let common = v.bzl_id("bazel/common.bzl").unwrap();
    let consumers: Vec<&str> = v
        .bzl_consumers(common)
        .iter()
        .map(|&p| v.package_name(p))
        .collect();
    assert_eq!(consumers, ["app"]);
}
