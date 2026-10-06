//! A repository declared in a `.bzl` of an in-tree repository names a file
//! of the main repository as its input (issue #28).
//!
//! envoy's WORKSPACE era: `WORKSPACE` loads `//bazel:api_repositories.bzl`,
//! which loads `api_dependencies` from `@envoy_api//bazel:repositories.bzl`.
//! That file lives at `api/bazel/repositories.bzl` (`@envoy_api` is the
//! in-tree `api/`), and it declares `com_envoyproxy_protoc_gen_validate` with
//! `patches = ["@envoy//bazel:pgv.patch"]`. The closure walk stopped at the
//! external label, so the patch was the input of nothing: an edit to it
//! answered 0 against an oracle of 6550. Once Bazel has confirmed that the
//! repository is exactly that directory, its files are files of this tree,
//! and the walk follows the load and reads them in the repository's own
//! context, where `//x` means `api/x`.

mod support;

use cambium_index::externals::{build_repo_inputs, read_module_facts, Pattern};
use cambium_index::{LoadGraph, RepoMapping};
use support::{set, Files};

const WORKSPACE: &str = "workspace(name = \"demo\")\n\
     \n\
     local_repository(name = \"api\", path = \"api\")\n\
     \n\
     load(\"//bazel:api_repositories.bzl\", \"demo_api_dependencies\")\n\
     \n\
     demo_api_dependencies()\n";

fn files() -> Files {
    Files(vec![
        ("WORKSPACE", WORKSPACE),
        (
            "bazel/api_repositories.bzl",
            "load(\"@api//bazel:repositories.bzl\", \"api_dependencies\")\n\
             \n\
             def demo_api_dependencies():\n\
             \x20   api_dependencies()\n",
        ),
        ("api/WORKSPACE", "workspace(name = \"api\")\n"),
        (
            // A helper of the in-tree repository's own, loaded relative to
            // its file and by its `//` label: both are `api/bazel/...`.
            "api/bazel/archive.bzl",
            "load(\"@bazel_tools//tools/build_defs/repo:http.bzl\", \"http_archive\")\n\
             \n\
             def api_archive(name, **kwargs):\n\
             \x20   http_archive(name = name, **kwargs)\n",
        ),
        (
            "api/bazel/repositories.bzl",
            "load(\":archive.bzl\", \"api_archive\")\n\
             load(\"//bazel:more.bzl\", \"more_dependencies\")\n\
             \n\
             def api_dependencies():\n\
             \x20   api_archive(\n\
             \x20       name = \"pgv\",\n\
             \x20       patches = [\"@demo//bazel:pgv.patch\"],\n\
             \x20   )\n\
             \x20   api_archive(\n\
             \x20       name = \"rooted\",\n\
             \x20       patches = [\"@//bazel:rooted.patch\"],\n\
             \x20   )\n\
             \x20   more_dependencies()\n",
        ),
        (
            "api/bazel/more.bzl",
            "load(\":archive.bzl\", \"api_archive\")\n\
             \n\
             def more_dependencies():\n\
             \x20   api_archive(\n\
             \x20       name = \"own\",\n\
             \x20       patches = [\"//bazel:own.patch\"],\n\
             \x20   )\n",
        ),
    ])
}

fn derive(m: &RepoMapping) -> (LoadGraph, Vec<String>, cambium_index::externals::RepoInputs) {
    let files = files();
    let mut g = LoadGraph::new();
    let ws = g.workspace_closure(&files, m);
    let facts = read_module_facts(&files);
    let ri = build_repo_inputs(&files, m, &g, &ws, &facts, &["api".to_string()]);
    let paths = ws.iter().map(|&id| g.path(id).to_string()).collect();
    (g, paths, ri)
}

fn mapping() -> RepoMapping {
    RepoMapping::identity().with_main_alias("demo")
}

/// Without the confirmation the walk stops where it always did, and the
/// patch is the input of nothing: the shape of the miss.
#[test]
fn an_unconfirmed_in_tree_repository_is_not_entered() {
    let (_, ws, ri) = derive(&mapping());
    assert_eq!(ws, vec!["bazel/api_repositories.bzl".to_string()]);
    assert!(ri.repos_for("bazel/pgv.patch").is_empty());
}

#[test]
fn a_confirmed_in_tree_repository_is_read_in_its_own_context() {
    let (g, ws, ri) = derive(&mapping().with_in_tree("api", "api/"));
    // The load into `@api` is followed, and the files there resolve their
    // own `:x` and `//x` labels under `api/`.
    for p in [
        "bazel/api_repositories.bzl",
        "api/bazel/repositories.bzl",
        "api/bazel/more.bzl",
        "api/bazel/archive.bzl",
    ] {
        assert!(ws.contains(&p.to_string()), "{p} in {ws:?}");
    }
    assert!(!ws.iter().any(|p| p == "bazel/more.bzl"), "{ws:?}");
    // The external label is still recorded beside the edge.
    let definer = g.id_of("bazel/api_repositories.bzl").unwrap();
    assert_eq!(g.external(definer), ["@@api//bazel:repositories.bzl"]);

    // The main repository's own name, and `@//`, mean the main root from
    // inside the in-tree repository.
    assert_eq!(ri.repos_for("bazel/pgv.patch"), set(&["pgv"]));
    assert_eq!(ri.repos_for("bazel/rooted.patch"), set(&["rooted"]));
    // `//bazel:own.patch` in an `@api` file is `api/bazel/own.patch`, and
    // not the main repository's `bazel/own.patch`.
    // (`@api` itself is there too, through its `path = "api"` prefix.)
    assert_eq!(ri.repos_for("api/bazel/own.patch"), set(&["api", "own"]));
    assert!(ri.repos_for("bazel/own.patch").is_empty());
    // A label of the in-tree repository gets no `//pkg:name` label pattern:
    // that would name a main-repo target that does not exist.
    assert!(!ri
        .patterns
        .keys()
        .any(|p| matches!(p, Pattern::Label(l) if l == "//bazel:own.patch")));
    // The files of the in-tree repository answer for what they declare.
    assert_eq!(
        ri.bzl_repos["api/bazel/more.bzl"],
        set(&["own"]),
        "{:?}",
        ri.bzl_repos
    );
    assert!(ri.bzl_repos["api/bazel/repositories.bzl"].contains("pgv"));
    // And the main-repo file that calls into them is no longer read as
    // unattributable: it defines what the function it calls defines.
    assert_eq!(
        ri.bzl_repos["bazel/api_repositories.bzl"],
        set(&["own", "pgv", "rooted"])
    );
}

/// A main-repo `.bzl` naming a file of the in-tree repository by its label
/// names a file of this tree.
#[test]
fn an_in_tree_label_written_in_the_main_repository_is_a_tree_file() {
    let m = mapping().with_in_tree("api", "api");
    let pats = cambium_index::externals::main_repo_patterns("@api//bazel:x.patch", &m);
    assert_eq!(pats, vec![Pattern::Exact("api/bazel/x.patch".to_string())]);
    let unmapped = cambium_index::externals::main_repo_patterns("@api//bazel:x.patch", &mapping());
    assert!(unmapped.is_empty());
}
