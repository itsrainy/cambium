//! A main-repo file that only a repository declared inside an in-tree
//! repository reads (issue #28).
//!
//! envoy's WORKSPACE era at `142e91ef2e97`: `WORKSPACE` loads
//! `//bazel:api_repositories.bzl`, which calls `api_dependencies()` from
//! `@envoy_api//bazel:repositories.bzl`, the in-tree `api/`. That function
//! declares `com_envoyproxy_protoc_gen_validate` with `patches =
//! ["@envoy//bazel:pgv.patch"]`. An edit to the patch alone answered 0
//! labels, with no warning, against an oracle of 6550: the load walk stopped
//! at the external label, so the patch was the input of nothing and the
//! changed file was "not referenced by any rule".
//!
//! Driven with no Bazel process. The tree is on disk because the boundary
//! walk and the repository probe's package comparison read it there.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::FixedText;
use cambium_engine::AffectedReport;
use cambium_index::{IndexData, PackageUpdate, TargetRow};
use cambium_testkit::{engine, req, Files, Scanning};

/// The pre-migration `MODULE.bazel` envoy carries beside its `WORKSPACE`:
/// Bazel does not read it in this mode, but it is where the claim that
/// `@api` is `api/` comes from, and the probe checks that claim.
const MODULE_BAZEL: &str = "module(name = \"demo\")\n\
     bazel_dep(name = \"api\", version = \"1.0\")\n\
     local_path_override(module_name = \"api\", path = \"api\")\n";

const BAZELRC: &str = "common --noenable_bzlmod\ncommon --enable_workspace\n";

const WORKSPACE: &str = "workspace(name = \"demo\")\n\
     \n\
     local_repository(name = \"api\", path = \"api\")\n\
     \n\
     load(\"//bazel:api_repositories.bzl\", \"demo_api_dependencies\")\n\
     \n\
     demo_api_dependencies()\n";

const API_REPOSITORIES: &str = "load(\"@api//bazel:repositories.bzl\", \"api_dependencies\")\n\
     \n\
     def demo_api_dependencies():\n\
     \x20   api_dependencies()\n";

const API_ARCHIVE: &str =
    "load(\"@bazel_tools//tools/build_defs/repo:http.bzl\", \"http_archive\")\n\
     \n\
     def api_archive(name, **kwargs):\n\
     \x20   http_archive(name = name, **kwargs)\n";

const API_DEPS: &str = "load(\":archive.bzl\", \"api_archive\")\n\
     \n\
     def api_dependencies():\n\
     \x20   api_archive(\n\
     \x20       name = \"pgv\",\n\
     \x20       patches = [\"@demo//bazel:pgv.patch\"],\n\
     \x20   )\n\
     \x20   api_archive(\n\
     \x20       name = \"own\",\n\
     \x20       patches = [\"//bazel:own.patch\"],\n\
     \x20   )\n";

fn text_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("MODULE.bazel", MODULE_BAZEL),
        (".bazelrc", BAZELRC),
        ("WORKSPACE", WORKSPACE),
        ("bazel/BUILD", ""),
        ("bazel/api_repositories.bzl", API_REPOSITORIES),
        ("bazel/pgv.patch", "--- a\n+++ b\n"),
        ("app/BUILD", ""),
        ("other/BUILD", ""),
        ("api/WORKSPACE", "workspace(name = \"api\")\n"),
        ("api/bazel/BUILD", ""),
        ("api/bazel/archive.bzl", API_ARCHIVE),
        ("api/bazel/repositories.bzl", API_DEPS),
        ("api/bazel/own.patch", "--- a\n+++ b\n"),
    ]
}

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (p, t) in text_files() {
        let path = dir.path().join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, t).unwrap();
    }
    dir
}

/// ```text
/// //app:main   -> @pgv//:lib   (declared in api/bazel/repositories.bzl)
/// //other:main -> @own//:lib   (likewise, patched by api/bazel/own.patch)
/// ```
fn rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "app" => vec![TargetRow::rule(
            "//app:main",
            "demo_lib",
            vec!["@pgv//:lib".into()],
        )],
        "other" => vec![TargetRow::rule(
            "//other:main",
            "demo_lib",
            vec!["@own//:lib".into()],
        )],
        _ => Vec::new(),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

/// What `bazel query '//external:*' --output=build` prints for the in-tree
/// repository: in the WORKSPACE world this is where the claim the probe
/// checked lands (`map_nested_repositories`).
const EXTERNAL_RULES: &str = r#"# /ws/WORKSPACE:3:17
local_repository(
  name = "api",
  path = "api",
)
"#;

fn bare(root: &std::path::Path) -> Scanning {
    let m: BTreeMap<String, String> = text_files()
        .into_iter()
        .map(|(p, t)| (p.to_string(), t.to_string()))
        .collect();
    Scanning::new(&["app", "bazel", "other", "@api//bazel"], rows)
        .with_files(Files(m))
        .with_workspace(root)
        .with_repo_probe("api", &["bazel"], false)
}

fn scanning(root: &std::path::Path) -> Scanning {
    bare(root).with_external_rules(EXTERNAL_RULES)
}

fn run(index: &IndexData, root: &std::path::Path, path: &str) -> AffectedReport {
    engine()
        .compute_with_texts(
            index.view(),
            &[Change::new(Status::Modified, path)],
            &scanning(root),
            &req(),
            &FixedText(BTreeMap::new()),
        )
        .unwrap()
        .report
}

#[test]
fn the_cold_index_reads_the_in_tree_repository_rules() {
    let dir = tree();
    let index = engine()
        .full_build_with("c0", &scanning(dir.path()))
        .unwrap();
    let v = index.view();
    let repo = (0..v.num_repos())
        .find(|&r| v.repo_name(r) == "api")
        .expect("@api is in the table");
    assert!(v.repo_origin(repo).confirmed, "{:?}", v.repo_origin(repo));
    // The in-tree file is in the workspace closure, under its tree path.
    let id = v
        .bzl_id("api/bazel/repositories.bzl")
        .expect("the closure followed the load into api/");
    assert!(v.bzl_in_workspace(id));
    let named: Vec<&str> = (0..v.num_repos()).map(|r| v.repo_name(r)).collect();
    assert!(named.contains(&"pgv"), "{named:?}");
    assert!(named.contains(&"own"), "{named:?}");
}

/// The gate's shape: an edit to the patch alone reaches the consumers of the
/// repository it patches, and only those.
#[test]
fn an_edit_to_a_patch_named_from_an_in_tree_repository_reaches_its_consumers() {
    let dir = tree();
    let index = engine()
        .full_build_with("c0", &scanning(dir.path()))
        .unwrap();
    let r = run(&index, dir.path(), "bazel/pgv.patch");
    assert!(!r.global, "{:?}", r.global_reason);
    let affected = r.affected_labels();
    assert_eq!(affected, vec!["//app:main".to_string()], "{:?}", r.stats);
}

/// `//bazel:own.patch` written in `api/bazel/repositories.bzl` is
/// `api/bazel/own.patch`: the edit is under the in-tree repository's
/// directory, and reaches the repository the patch belongs to.
#[test]
fn a_patch_the_in_tree_repository_names_by_its_own_root_is_its_file() {
    let dir = tree();
    let index = engine()
        .full_build_with("c0", &scanning(dir.path()))
        .unwrap();
    let r = run(&index, dir.path(), "api/bazel/own.patch");
    assert!(!r.global, "{:?}", r.global_reason);
    assert!(
        r.affected_labels().contains(&"//other:main".to_string()),
        "{:?}",
        r.affected_labels()
    );
}

/// An edit to the in-tree file that declares the repositories still moves
/// everything it declares: following the load must not narrow it.
#[test]
fn an_edit_to_the_in_tree_declarations_still_moves_what_they_declare() {
    let dir = tree();
    let index = engine()
        .full_build_with("c0", &scanning(dir.path()))
        .unwrap();
    let r = run(&index, dir.path(), "api/bazel/repositories.bzl");
    assert!(!r.global, "{:?}", r.global_reason);
    let affected = r.affected_labels();
    assert!(affected.contains(&"//app:main".to_string()), "{affected:?}");
    assert!(
        affected.contains(&"//other:main".to_string()),
        "{affected:?}"
    );
}

/// A probe answer that never lands on a candidate (no `//external:*` answer
/// here) maps nothing, and the closure does not follow the load either: what
/// a splice reads back as confirmed is what the cold build followed, so the
/// two cannot disagree about which files the closure holds.
#[test]
fn a_confirmation_that_maps_nothing_is_not_followed() {
    let dir = tree();
    let index = engine().full_build_with("c0", &bare(dir.path())).unwrap();
    let v = index.view();
    let repo = (0..v.num_repos())
        .find(|&r| v.repo_name(r) == "api")
        .expect("@api is in the table");
    assert!(!v.repo_origin(repo).confirmed, "{:?}", v.repo_origin(repo));
    assert!(
        v.bzl_id("api/bazel/repositories.bzl")
            .is_none_or(|id| !v.bzl_in_workspace(id)),
        "the closure stopped at the external label"
    );
}

/// A splice reads the in-tree repository back from the confirmed origin the
/// base recorded, so the closure it writes follows the same load and the
/// patch stays an input of the lineage.
#[test]
fn a_splice_keeps_following_the_load() {
    let dir = tree();
    let q = scanning(dir.path());
    let index = engine().full_build_with("c0", &q).unwrap();
    let out = engine()
        .compute_with_texts(
            index.view(),
            &[Change::new(Status::Modified, "bazel/api_repositories.bzl")],
            &q,
            &cambium_engine::AffectedRequest {
                materialize: true,
                ..req()
            },
            &FixedText(BTreeMap::new()),
        )
        .unwrap();
    let spliced = out.index.expect("not globally invalidated");
    let v = spliced.view();
    let id = v
        .bzl_id("api/bazel/repositories.bzl")
        .expect("the splice's closure still follows the load");
    assert!(v.bzl_in_workspace(id));
    let r = run(&spliced, dir.path(), "bazel/pgv.patch");
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.affected_labels(), vec!["//app:main".to_string()]);
}
