//! The dirty stage over the Skyframe loading layer, with no Bazel anywhere.
//!
//! This replaces the static-`.bzl`-closure tests, which replaced the
//! `rbuildfiles` tests before them. The engine's `Bazel` handle points at
//! `/nonexistent/bazel`, so a run that tried to shell out would fail the test
//! rather than pass it quietly; the layer comes from the dump the in-memory
//! tree would have produced (`cambium_testkit::sky`), whose shapes are the
//! ones `crates/index/src/sky/fixtures/*.dump` hold verbatim from real Bazel.
//!
//! What is checked here is the *stage*: that the walk's answer reaches the
//! dirty set, the direct set and the report. What the walk itself answers over
//! real dumps is checked in `cambium_index::sky`.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::AffectedRequest;
use cambium_index::{PackageUpdate, TargetRow};
use cambium_testkit::{base, engine, req, Files, Scanning};

/// An in-memory checkout.
///
/// ```text
/// //app:app  --deps-->  //lib:lib
/// app/BUILD    loads //bazel:defs.bzl  ->  :common.bzl
/// lib/BUILD    loads //bazel:other.bzl
/// MODULE.bazel use_extension("//bazel:ext.bzl")
/// bazel/dead.bzl  loaded by nobody
/// ```
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

/// Rows for a package. Fixed, because this test is about the dirty stage and
/// not about what a query returns.
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

/// A querier that can read `tree`, and so produces an index with a layer.
fn scanning(tree: Files) -> Scanning {
    Scanning::new(PACKAGES, rows).with_files(tree)
}

/// A querier with no tree behind it: the index it builds has no layer, which
/// the engine must treat as "we cannot tell" rather than "nothing moved".
fn blind() -> Scanning {
    Scanning::new(PACKAGES, rows)
}

fn changed(path: &str) -> Vec<Change> {
    vec![Change::new(Status::Modified, path)]
}

#[test]
fn the_index_stores_the_layer_it_dumped() {
    let (_e, index) = base(&scanning(files()));
    let v = index.view();
    assert!(v.sky.present);
    assert!(v.sky.len() > 20, "{} nodes", v.sky.len());
    // Every kind the dirty stage walks through is there.
    use cambium_index::sky::Kind;
    for kind in [
        Kind::FileState,
        Kind::File,
        Kind::DirectoryListingState,
        Kind::DirectoryListing,
        Kind::Glob,
        Kind::PackageLookup,
        Kind::Package,
        Kind::BzlLoad,
        Kind::Ignored,
    ] {
        assert!(
            v.sky.counts().iter().any(|(k, n)| *k == kind && *n > 0),
            "no {} nodes",
            kind.name()
        );
    }
}

#[test]
fn a_transitively_loaded_bzl_dirties_exactly_the_packages_behind_it() {
    let (e, index) = base(&scanning(files()));
    let out = e
        .compute(
            index.view(),
            &changed("bazel/common.bzl"),
            &scanning(files()),
            &req(),
        )
        .unwrap();
    assert!(!out.report.global);
    // `bazel/common.bzl` is loaded by `bazel/defs.bzl`, which `app/BUILD`
    // loads. The chain is FILE -> BZL_LOAD -> BZL_LOAD -> PACKAGE, and no
    // step of it is a rule in the engine.
    assert!(
        out.report.dirty_packages.contains(&"app".to_string()),
        "{:?}",
        out.report.dirty_packages
    );
    assert!(!out.report.dirty_packages.contains(&"lib".to_string()));
    assert_eq!(out.report.stats.bzl_closure_hits, 1);
    assert!(
        !out.report.stats.rbuildfiles_called,
        "the layer answers this; Bazel is never asked"
    );
    // Every rule in the loading package is direct — a `.bzl` edit leaves the
    // rule protos byte-identical — and the walk carries it upward.
    assert_eq!(out.report.direct, vec!["//app:app".to_string()]);
}

#[test]
fn a_bzl_a_dependency_loads_reaches_the_dependent_through_the_walk() {
    let (e, index) = base(&scanning(files()));
    let out = e
        .compute(
            index.view(),
            &changed("bazel/other.bzl"),
            &scanning(files()),
            &req(),
        )
        .unwrap();
    assert!(out.report.dirty_packages.contains(&"lib".to_string()));
    assert_eq!(out.report.direct, vec!["//lib:lib".to_string()]);
    assert_eq!(
        out.report.affected_labels(),
        vec!["//app:app".to_string(), "//lib:lib".to_string()]
    );
}

/// A `.bzl` the module system loads can define repository rules, whose output
/// is not in the rule layer at all. The layer says which files those are, in
/// the `BZL_LOAD` key Bazel writes, so this needs no stored flag and no scan
/// of the head checkout.
#[test]
fn a_bzl_in_the_workspace_closure_is_global() {
    let (e, index) = base(&scanning(files()));
    let out = e
        .compute(
            index.view(),
            &changed("bazel/ext.bzl"),
            &scanning(files()),
            &req(),
        )
        .expect("a workspace-level loader is not an error");
    assert!(out.report.global, "must over-approximate, not miss");
    let reason = out.report.global_reason.as_deref().unwrap();
    assert!(reason.contains("bazel/ext.bzl"), "{reason}");
    assert!(reason.contains("MODULE.bazel/WORKSPACE"), "{reason}");
}

/// A `.bzl` no `load()` names has a `BZL_LOAD` node with nothing above it, so
/// the reverse walk stops there. Nothing is assumed: the node is in the layer
/// because Bazel had one.
#[test]
fn a_bzl_nothing_loads_dirties_nothing() {
    let (e, index) = base(&scanning(files()));
    let out = e
        .compute(
            index.view(),
            &changed("bazel/dead.bzl"),
            &scanning(files()),
            &req(),
        )
        .unwrap();
    assert!(!out.report.global, "a dead .bzl is not a global change");
    assert!(
        out.report.dirty_packages.is_empty(),
        "{:?}",
        out.report.dirty_packages
    );
    assert!(out.report.affected.is_empty());
    assert_eq!(out.report.stats.bzl_closure_hits, 0);
}

#[test]
fn an_index_without_a_layer_escalates() {
    let e = engine();
    let index = e.full_build_with("c0", &blind()).unwrap();
    assert!(!index.view().sky.present);
    let out = e
        .compute(index.view(), &changed("bazel/common.bzl"), &blind(), &req())
        .unwrap();
    assert!(
        out.report.global,
        "an empty table means 'we never looked', not 'nothing moved'"
    );
    assert!(out
        .report
        .global_reason
        .as_deref()
        .unwrap()
        .contains("no skyframe loading layer"));
}

/// A content-only edit to an ordinary source moves no loading-phase node, so
/// the run re-queries nothing and reaches its consumers through the rule
/// layer's file rows alone.
#[test]
fn a_source_content_change_re_queries_nothing() {
    let mut tree = files();
    tree.0.insert("lib/a.cc".into(), "int a;\n".into());
    let (e, index) = base(&scanning(tree.clone()));
    let out = e
        .compute(index.view(), &changed("lib/a.cc"), &scanning(tree), &req())
        .unwrap();
    assert!(!out.report.global);
    assert!(
        out.report.dirty_packages.is_empty(),
        "{:?}",
        out.report.dirty_packages
    );
}

/// A new `BUILD` file carving a sub-package out of its parent dirties both,
/// through `PACKAGE:lib -> PACKAGE_LOOKUP:lib/sub`. The parent is the half
/// that used to need a rule of its own.
#[test]
fn a_new_sub_package_dirties_the_parent_and_itself() {
    let mut tree = files();
    tree.0.insert("lib/sub/x.cc".into(), String::new());
    let (e, index) = base(&scanning(tree.clone()));

    let mut head = tree.clone();
    head.0.insert("lib/sub/BUILD".into(), String::new());
    let out = e
        .compute(
            index.view(),
            &[Change::new(Status::Added, "lib/sub/BUILD")],
            &scanning(head),
            &req(),
        )
        .unwrap();
    assert!(!out.report.global);
    assert!(
        out.report.dirty_packages.contains(&"lib".to_string()),
        "the parent lost a file to the new package: {:?}",
        out.report.dirty_packages
    );
    assert!(
        out.report.dirty_packages.contains(&"lib/sub".to_string()),
        "and the new package is queried for the first time: {:?}",
        out.report.dirty_packages
    );
}

/// The other direction: a deleted `BUILD` hands its files back to the parent.
/// It is the same seed and the same edge, which is why neither is a case.
#[test]
fn a_deleted_build_file_dirties_the_parent_and_tombstones_the_package() {
    let mut tree = files();
    tree.0.insert("lib/sub/BUILD".into(), String::new());
    tree.0.insert("lib/sub/x.cc".into(), String::new());
    let querier = Scanning::new(&["app", "bazel", "lib", "lib/sub"], rows).with_files(tree.clone());
    let (e, index) = base(&querier);

    let mut head = tree.clone();
    head.0.remove("lib/sub/BUILD");
    let out = e
        .compute(
            index.view(),
            &[Change::new(Status::Deleted, "lib/sub/BUILD")],
            &Scanning::new(&["app", "bazel", "lib"], rows).with_files(head),
            &req(),
        )
        .unwrap();
    assert!(!out.report.global);
    assert!(
        out.report.dirty_packages.contains(&"lib".to_string()),
        "{:?}",
        out.report.dirty_packages
    );
}

/// A file added where a `glob` can see it dirties the package, through the
/// directory listing Bazel read.
#[test]
fn a_file_added_to_a_globbed_directory_dirties_its_package() {
    let (e, index) = base(&scanning(files()));
    let mut head = files();
    head.0.insert("lib/new.cc".into(), String::new());
    let out = e
        .compute(
            index.view(),
            &[Change::new(Status::Added, "lib/new.cc")],
            &scanning(head),
            &req(),
        )
        .unwrap();
    assert!(!out.report.global);
    assert!(
        out.report.dirty_packages.contains(&"lib".to_string()),
        "{:?}",
        out.report.dirty_packages
    );
}

/// The materialised path refreshes the layer from the same server the splice
/// query ran on, so the published index describes head rather than base.
#[test]
fn a_splice_refreshes_the_layer_of_the_packages_it_requeried() {
    let (e, index) = base(&scanning(files()));
    let before = index.view().sky.len();

    // At head, `lib/BUILD` also loads `//bazel:common.bzl`.
    let mut head = files();
    head.0.insert(
        "lib/BUILD".into(),
        "load(\"//bazel:other.bzl\", \"lib_rule\")\nload(\"//bazel:common.bzl\", \"c\")\n".into(),
    );
    let out = e
        .compute(
            index.view(),
            &[Change::new(Status::Modified, "lib/BUILD")],
            &scanning(head),
            &AffectedRequest {
                materialize: true,
                ..req()
            },
        )
        .unwrap();
    let head_index = out.index.expect("materialised");
    let v = head_index.view();
    assert!(v.sky.present);
    assert!(v.sky.len() >= before);

    // The refreshed row is what a later run walks: editing `common.bzl` now
    // reaches `lib` as well as `app`.
    let w = v
        .sky
        .walk(&cambium_index::sky::seeds([("bazel/common.bzl", false)]));
    assert!(w.packages.contains("lib"), "{w:?}");
    assert!(w.packages.contains("app"), "{w:?}");
}
