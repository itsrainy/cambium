//! A repository whose definition the index never read still moves when the
//! code that defines it does.
//!
//! envoy's WORKSPACE-era shape, box run F steps 191 and 194
//! (`c99b0017689c..381b3de6a96e`, the `io_bazel_rules_go` to `rules_go`
//! rename). `WORKSPACE` loads `//bazel:api_repositories.bzl`, whose
//! `envoy_api_dependencies()` calls `api_dependencies()`, loaded from
//! `@envoy_api//bazel:repositories.bzl`. That file lives at
//! `api/bazel/repositories.bzl`, inside the in-tree repository `@envoy_api`,
//! and it is the only place `com_github_chrusty_protoc_gen_jsonschema` is
//! declared. The static load walk stops at the external label, so the index's
//! repository table never names the repository, and no `.bzl` row says which
//! file defines it. The step added a `repo_mapping` to that declaration, so
//! the repository's `BUILD` files load a different `rules_go`; Bazel refetches
//! it and rebuilds `//tools/protojsonschema:protoc_gen_jsonschema_proto_plugin`,
//! whose only dependency is `@com_github_chrusty_protoc_gen_jsonschema//cmd/
//! protoc-gen-jsonschema`. cambium answered 14627 labels without it.
//!
//! Driven with no Bazel process, as the other externals tests are.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::FixedText;
use cambium_index::externals::PathMap;
use cambium_index::{IndexBuilder, IndexData, PackageUpdate, TargetRow};
use cambium_testkit::{base, engine, req, Files, Scanning};

const WORKSPACE: &str = "workspace(name = \"demo\")\n\
     \n\
     local_repository(name = \"api\", path = \"api\")\n\
     \n\
     load(\"//bazel:api_repositories.bzl\", \"demo_api_dependencies\")\n\
     \n\
     demo_api_dependencies()\n\
     \n\
     load(\"//bazel:repositories.bzl\", \"demo_dependencies\")\n\
     \n\
     demo_dependencies()\n";

fn files() -> Files {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: &str| m.insert(k.to_string(), v.to_string());
    put("WORKSPACE", WORKSPACE);
    // `bazel/api_repositories.bzl`: every repository it defines comes from a
    // function in a file this tree's load walk never reads.
    put(
        "bazel/api_repositories.bzl",
        "load(\"@api//bazel:repositories.bzl\", \"api_dependencies\")\n\
         \n\
         def demo_api_dependencies():\n\
         \x20   api_dependencies()\n",
    );
    // `bazel/repositories.bzl`: a repository declared in a file the walk
    // reads, with a literal name, so the table knows it and knows who
    // defines it.
    put(
        "bazel/repositories.bzl",
        "load(\"@bazel_tools//tools/build_defs/repo:http.bzl\", \"http_archive\")\n\
         \n\
         def demo_dependencies():\n\
         \x20   http_archive(name = \"known\", urls = [\"https://example.invalid/k.tar.gz\"])\n",
    );
    put("bazel/BUILD", "");
    // The in-tree repository's own files, which the index does not read.
    put("api/WORKSPACE", "workspace(name = \"api\")\n");
    put("api/bazel/BUILD", "");
    put(
        "api/bazel/repositories.bzl",
        "load(\"@bazel_tools//tools/build_defs/repo:http.bzl\", \"http_archive\")\n\
         \n\
         def api_dependencies():\n\
         \x20   http_archive(name = \"jsonschema\", urls = [\"https://example.invalid/j.tar.gz\"])\n",
    );
    for p in ["tools", "app", "cc"] {
        put(&format!("{p}/BUILD"), "");
    }
    Files(m)
}

/// ```text
/// //tools:plugin -> @jsonschema//cmd:gen     (a repository no read file declares)
/// //tools:schema -> //tools:plugin
/// //app:main     -> @known//:lib             (declared in bazel/repositories.bzl)
/// //cc:lib       -> @bazel_tools//tools:tool (Bazel's own, never the tree's)
/// ```
fn rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "tools" => vec![
            TargetRow::rule(
                "//tools:plugin",
                "proto_plugin",
                vec!["@jsonschema//cmd:gen".into()],
            ),
            TargetRow::rule("//tools:schema", "demo_lib", vec!["//tools:plugin".into()]),
        ],
        "app" => vec![TargetRow::rule(
            "//app:main",
            "demo_lib",
            vec!["@known//:lib".into()],
        )],
        "cc" => vec![TargetRow::rule(
            "//cc:lib",
            "demo_lib",
            vec!["@bazel_tools//tools:tool".into()],
        )],
        _ => Vec::new(),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

const PACKAGES: &[&str] = &["app", "bazel", "cc", "tools"];

fn scanning() -> Scanning {
    Scanning::new(PACKAGES, rows).with_files(files())
}

/// The cold index, plus what a real cold build in envoy's tree records for
/// `api/`: the boundary, and the mapping that says `@api` is that directory.
fn index() -> IndexData {
    let (_e, cold) = base(&scanning());
    let mut b = IndexBuilder::from_view(cold.view());
    b.set_repo_boundaries(vec![cambium_index::Boundary {
        dir: "api".to_string(),
        bazelignored: false,
        traversed: false,
    }]);
    let mut c = b.take_bzl_closure().expect("the cold build read the tree");
    c.inputs.repos.insert("api".to_string());
    c.inputs
        .path_maps
        .entry("api".to_string())
        .or_default()
        .insert(PathMap::exhaustive_dir("api/".to_string()));
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

/// What the index holds, which is the whole of the problem: `@jsonschema`
/// is a leaf of the graph, the repository table does not name it, and the
/// file that defines it through `@api` answers for what the table holds.
#[test]
fn the_index_cannot_name_the_repository_or_its_definer() {
    let index = index();
    let v = index.view();
    let named: Vec<&str> = (0..v.num_repos()).map(|r| v.repo_name(r)).collect();
    assert!(named.contains(&"known"), "{named:?}");
    assert!(named.contains(&"api"), "{named:?}");
    assert!(!named.contains(&"jsonschema"), "{named:?}");
    assert!(
        (0..v.num_nodes()).any(|id| v.label(id) == "@jsonschema//cmd:gen"),
        "the leaf is in the graph"
    );
    let definer = v.bzl_id("bazel/api_repositories.bzl").unwrap();
    assert!(v.bzl_in_workspace(definer));
    assert_eq!(v.bzl_external(definer), ["@@api//bazel:repositories.bzl"]);
    let defines: Vec<&str> = v
        .bzl_repos_of(definer)
        .iter()
        .map(|&r| v.repo_name(r))
        .collect();
    assert!(!defines.contains(&"jsonschema"), "{defines:?}");
}

/// The under-report. On `046c77d` this reached `@api`'s own consumers (none
/// here) and the repositories the table attributes to the definer, and not
/// the consumer of the repository the edited declaration defines.
#[test]
fn an_edit_to_an_unread_definition_reaches_the_consumers_of_what_it_defines() {
    let index = index();
    let r = run(&index, &["api/bazel/repositories.bzl"]);
    assert!(!r.global, "{:?}", r.global_reason);
    // The edit moved `@api`, whose `.bzl` the definer loads.
    assert!(r.stats.repos_dirty_by_input >= 1, "{:?}", r.stats);
    assert!(r.stats.repos_dirty_by_loader >= 1, "{:?}", r.stats);
    let affected = r.affected_labels();
    assert!(
        affected.contains(&"//tools:plugin".to_string()),
        "the consumer of the repository the edit re-declares is reached: {affected:?}"
    );
    assert!(
        affected.contains(&"//tools:schema".to_string()),
        "and everything above it: {affected:?}"
    );
    // Bazel's own repository is never one the tree defines, whatever moved.
    assert!(!affected.contains(&"//cc:lib".to_string()), "{affected:?}");
}

/// A changed workspace-level `.bzl` whose functions call into code the walk
/// did not read moves what that code may define, as well as what the table
/// attributes to the file.
#[test]
fn a_workspace_bzl_calling_unread_code_moves_the_repositories_the_table_cannot_name() {
    let index = index();
    let r = run(&index, &["bazel/api_repositories.bzl"]);
    assert!(!r.global, "{:?}", r.global_reason);
    let affected = r.affected_labels();
    assert!(
        affected.contains(&"//tools:plugin".to_string()),
        "{affected:?}"
    );
    assert!(!affected.contains(&"//cc:lib".to_string()), "{affected:?}");
}

/// A workspace-level `.bzl` that reads only literal declarations and loads
/// only from Bazel's own repository defines exactly what the table says, and
/// nothing the table cannot name moves with it.
#[test]
fn a_workspace_bzl_the_walk_read_in_full_moves_only_what_it_declares() {
    let index = index();
    let r = run(&index, &["bazel/repositories.bzl"]);
    assert!(!r.global, "{:?}", r.global_reason);
    let affected = r.affected_labels();
    assert!(affected.contains(&"//app:main".to_string()), "{affected:?}");
    assert!(
        !affected.contains(&"//tools:plugin".to_string()),
        "{affected:?}"
    );
    assert!(!affected.contains(&"//cc:lib".to_string()), "{affected:?}");
}

/// A file of this tree that nothing defines a repository through moves no
/// repository the table cannot name.
#[test]
fn an_unrelated_edit_moves_no_unnamed_repository() {
    let index = index();
    let r = run(&index, &["app/README.md"]);
    assert!(!r.global, "{:?}", r.global_reason);
    assert!(r.affected_labels().is_empty(), "{:?}", r.affected_labels());
}
