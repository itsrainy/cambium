//! A repository defined through a load from another repository moves when
//! that other repository does.
//!
//! xla's shape. `@llvm-raw` is an archive fetched with `patches` that live in
//! this tree (`third_party/llvm/build.patch`), and the main repository never
//! names it: its rules consume `@llvm-project`, which the module extension
//! `third_party/extensions/llvm.bzl` defines by calling `llvm_configure`,
//! loaded from `@llvm-raw//utils/bazel:configure.bzl`. An edit to the patch
//! dirties `@llvm-raw`, which has no label in the graph and no package loading
//! from it; Bazel refetches it, re-runs `llvm_configure` and rebuilds every
//! consumer of `@llvm-project`. Box run B's xla walk answered 0 direct and 0
//! affected on three such steps (13, 29 and 181).
//!
//! Driven with no Bazel process, as the other externals tests are.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::FixedText;
use cambium_index::{IndexData, PackageUpdate, TargetRow};
use cambium_testkit::{base, engine, req, Files, Scanning};

/// `raw` is fetched with a patch from this tree; `gen` is defined by
/// `//ext:ext.bzl` through a rule loaded from `@raw`; `gen2` is defined by
/// `//ext:ext2.bzl` through a rule loaded from `@gen`, one link further;
/// `other` is defined by a file that loads from nothing that moves; `tgen`
/// comes from an extension `MODULE.bazel` takes from the dependency `toolsdep`
/// directly.
const MODULE_BAZEL: &str = "module(name = \"demo\", version = \"1\")\n\
     bazel_dep(name = \"toolsdep\", version = \"1.0\")\n\
     raw_ext = use_extension(\"//third_party/raw:workspace.bzl\", \"raw_ext\")\n\
     use_repo(raw_ext, \"raw\")\n\
     gen_ext = use_extension(\"//ext:ext.bzl\", \"gen_ext\")\n\
     use_repo(gen_ext, \"gen\")\n\
     gen2_ext = use_extension(\"//ext:ext2.bzl\", \"gen2_ext\")\n\
     use_repo(gen2_ext, \"gen2\")\n\
     other_ext = use_extension(\"//ext:other.bzl\", \"other_ext\")\n\
     use_repo(other_ext, \"other\")\n\
     t = use_extension(\"@toolsdep//:ext.bzl\", \"t\")\n\
     use_repo(t, \"tgen\")\n";

fn files() -> Files {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: &str| m.insert(k.to_string(), v.to_string());
    put("MODULE.bazel", MODULE_BAZEL);
    // `@llvm-raw`: an archive whose patch is a file of this tree, so the
    // patch is a recorded input of `raw` and nothing else.
    put(
        "third_party/raw/workspace.bzl",
        "load(\"@bazel_tools//tools/build_defs/repo:http.bzl\", \"http_archive\")\n\
         \n\
         def _raw_impl(mctx):\n\
         \x20   http_archive(\n\
         \x20       name = \"raw\",\n\
         \x20       urls = [\"https://example.invalid/raw.tar.gz\"],\n\
         \x20       patches = [\"//third_party/raw:build.patch\"],\n\
         \x20   )\n\
         \n\
         raw_ext = module_extension(implementation = _raw_impl)\n",
    );
    put("third_party/raw/BUILD", "");
    // `third_party/extensions/llvm.bzl`: the rule that makes `gen` is `@raw`'s
    // code, reached only through this load.
    put(
        "ext/ext.bzl",
        "load(\"@raw//x:configure.bzl\", \"configure\")\n\
         \n\
         def _gen_impl(mctx):\n\
         \x20   configure(name = \"gen\")\n\
         \n\
         gen_ext = module_extension(implementation = _gen_impl)\n",
    );
    put(
        "ext/ext2.bzl",
        "load(\"@gen//:configure.bzl\", \"configure\")\n\
         \n\
         def _gen2_impl(mctx):\n\
         \x20   configure(name = \"gen2\")\n\
         \n\
         gen2_ext = module_extension(implementation = _gen2_impl)\n",
    );
    put(
        "ext/other.bzl",
        "load(\"@bazel_tools//tools/build_defs/repo:http.bzl\", \"http_archive\")\n\
         \n\
         def _other_impl(mctx):\n\
         \x20   http_archive(name = \"other\", urls = [\"https://example.invalid/o.tar.gz\"])\n\
         \n\
         other_ext = module_extension(implementation = _other_impl)\n",
    );
    put("ext/BUILD", "");
    for p in ["app", "app2", "app3", "app4"] {
        put(&format!("{p}/BUILD"), "");
    }
    Files(m)
}

/// ```text
/// //app:main  -> @gen//:lib
/// //app2:main -> @gen2//:lib
/// //app3:main -> @other//:lib
/// //app4:main -> @tgen//:lib
/// ```
fn rows(pkg: &str) -> PackageUpdate {
    let dep = match pkg {
        "app" => "@gen//:lib",
        "app2" => "@gen2//:lib",
        "app3" => "@other//:lib",
        "app4" => "@tgen//:lib",
        _ => {
            return PackageUpdate {
                package: pkg.to_string(),
                ..Default::default()
            }
        }
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets: vec![TargetRow::rule(
            format!("//{pkg}:main"),
            "demo_lib",
            vec![dep.into()],
        )],
        ..Default::default()
    }
}

const PACKAGES: &[&str] = &["app", "app2", "app3", "app4", "ext", "third_party/raw"];

/// Extension-generated repositories carry the extension in their canonical
/// name, as Bazel 8 writes it: `+raw_ext+raw`, whose last segment is the
/// apparent name.
const REPO_MAPPING: &str = "{\"\":\"\",\"demo\":\"\",\"toolsdep\":\"toolsdep+\",\
     \"raw\":\"+raw_ext+raw\",\"gen\":\"+gen_ext+gen\",\"gen2\":\"+gen2_ext+gen2\",\
     \"other\":\"+other_ext+other\",\"tgen\":\"toolsdep++t+tgen\"}";

fn scanning() -> Scanning {
    Scanning::new(PACKAGES, rows)
        .with_files(files())
        .with_repo_mapping(REPO_MAPPING)
}

fn texts(head_module: &str) -> FixedText {
    let mut m = BTreeMap::new();
    m.insert(
        ("base".to_string(), "MODULE.bazel".to_string()),
        MODULE_BAZEL.to_string(),
    );
    m.insert(
        ("head".to_string(), "MODULE.bazel".to_string()),
        head_module.to_string(),
    );
    // Bazel 7: no autoload question to answer for the version bump below.
    m.insert(
        ("base".to_string(), ".bazelversion".to_string()),
        "7.4.1\n".to_string(),
    );
    FixedText(m)
}

fn run(index: &IndexData, path: &str, head_module: &str) -> cambium_engine::AffectedReport {
    let changes = vec![Change::new(Status::Modified, path)];
    engine()
        .compute_with_texts(
            index.view(),
            &changes,
            &scanning(),
            &req(),
            &texts(head_module),
        )
        .unwrap()
        .report
}

/// What the index records, which is everything the engine needs: the patch
/// is an input of `raw` alone, `raw` has no label in the graph, and the file
/// that defines `gen` stopped its load walk at `@raw`.
#[test]
fn the_index_records_the_load_that_defines_the_repository() {
    let (_e, index) = base(&scanning());
    let v = index.view();
    let named: Vec<&str> = v
        .repos_for_path("third_party/raw/build.patch")
        .into_iter()
        .map(|r| v.repo_name(r))
        .collect();
    assert_eq!(named, ["raw"]);
    assert!(
        (0..v.num_nodes()).all(|id| !v.label(id).contains("raw//")),
        "no label of `@raw` is in the graph"
    );
    let ext = v.bzl_id("ext/ext.bzl").unwrap();
    assert!(v.bzl_in_workspace(ext));
    assert_eq!(v.bzl_external(ext), ["@@+raw_ext+raw//x:configure.bzl"]);
    let defines: Vec<&str> = v
        .bzl_repos_of(ext)
        .iter()
        .map(|&r| v.repo_name(r))
        .collect();
    assert_eq!(defines, ["gen"]);
}

/// The under-report itself. On `c096403` this answered nothing: direct and
/// affected both empty, and not global.
#[test]
fn an_input_of_a_repository_reaches_the_consumers_of_what_it_defines() {
    let (_e, index) = base(&scanning());
    let r = run(&index, "third_party/raw/build.patch", MODULE_BAZEL);
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    let affected = r.affected_labels();
    assert!(
        affected.contains(&"//app:main".to_string()),
        "the consumer of `@gen` is reached: {affected:?}"
    );
    // Transitively: `gen2` is defined through a load from `gen`.
    assert!(
        affected.contains(&"//app2:main".to_string()),
        "the consumer of `@gen2` is reached: {affected:?}"
    );
    assert_eq!(r.stats.repos_dirty_by_loader, 2);
    // A repository no moved repository's code defines stays clean, and so
    // does a consumer of an unrelated extension.
    assert!(
        !affected.contains(&"//app3:main".to_string()),
        "{affected:?}"
    );
    assert!(
        !affected.contains(&"//app4:main".to_string()),
        "{affected:?}"
    );
}

/// The same route from `MODULE.bazel` itself: an extension taken from a
/// dependency directly. A version bump replaces the dependency, the
/// extension's code with it, and so what the extension generates.
#[test]
fn a_dependency_bump_reaches_the_consumers_of_its_extensions_repositories() {
    let (_e, index) = base(&scanning());
    let head = MODULE_BAZEL.replace(
        "bazel_dep(name = \"toolsdep\", version = \"1.0\")",
        "bazel_dep(name = \"toolsdep\", version = \"2.0\")",
    );
    let r = run(&index, "MODULE.bazel", &head);
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_module, 1);
    assert_eq!(r.stats.repos_dirty_by_loader, 1);
    let affected = r.affected_labels();
    assert_eq!(affected, vec!["//app4:main".to_string()]);
}

/// A file of this tree that nothing defines a repository through moves no
/// repository by this route.
#[test]
fn an_unrelated_edit_moves_nothing_through_a_load() {
    let (_e, index) = base(&scanning());
    let r = run(&index, "app/README.md", MODULE_BAZEL);
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_loader, 0);
}
