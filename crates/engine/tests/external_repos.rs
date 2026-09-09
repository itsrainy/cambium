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
//! plus a `MODULE.bazel.lock` and a legacy `WORKSPACE` that move on their own,
//! and the escalations that must survive: a toolchain-registering module, a
//! lockfile shape the parser does not know, and a `WORKSPACE` statement it
//! cannot classify.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::{FixedText, WorkspaceText};
use cambium_engine::query::Rows;
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
    // The repository rule proper: three labels it resolves, all inside the
    // declaration, and one call that names the repository they feed.
    put(
        "bazel/repo.bzl",
        "def _impl(ctx):\n\
         \x20   ctx.file(\"BUILD\", \"\")\n\
         \n\
         _gen = repository_rule(\n\
         \x20   implementation = _impl,\n\
         \x20   attrs = {\n\
         \x20       \"config\": attr.label(default = \"@demo//:config.yml\"),\n\
         \x20       \"tpl\": attr.label(default = \"@demo//tpl:helper.bzl\"),\n\
         \x20       \"data\": attr.label(default = \"@demo//data\"),\n\
         \x20   },\n\
         )\n\
         \n\
         def r():\n\
         \x20   _gen(name = \"generated\")\n",
    );
    put("app/BUILD", "load(\"//bazel:defs.bzl\", \"r\")\n");
    put("lib/BUILD", "load(\"//bazel:defs.bzl\", \"r\")\n");
    // Issue #4: a BUILD file that loads a dependency's `.bzl` *directly*,
    // with no main-repo `.bzl` in between. Its rules can change when `fmt`
    // does, even though nothing in it names a `@fmt` label.
    put("direct/BUILD", "load(\"@fmt//:defs.bzl\", \"f\")\n");
    // Both packages reach a `.bzl` inside `@generated` through this file, so
    // rule (2) has somewhere to land.
    put(
        "bazel/defs.bzl",
        "load(\"@generated//:defs.bzl\", \"g\")\nR = 1\n",
    );
    put("bazel/BUILD", "");
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
        // The shape `bazel/bazel` uses all over `tools/`: the very `.bzl` that
        // the workspace closure reaches is *also* listed as a source of a
        // filegroup. Editing it must dirty this filegroup, on top of whatever
        // the repository it defines does elsewhere.
        "bazel" => vec![
            TargetRow::source_file("//bazel:repo.bzl"),
            TargetRow::rule(
                "//bazel:bzl_srcs",
                "filegroup",
                vec!["//bazel:repo.bzl".into()],
            ),
            TargetRow::rule(
                "//bazel:embedded_tools",
                "filegroup",
                vec!["//bazel:bzl_srcs".into()],
            ),
        ],
        "direct" => vec![TargetRow::rule("//direct:d", "demo_lib", Vec::new())],
        _ => Vec::new(),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

const PACKAGES: &[&str] = &["app", "lib", "bazel", "direct"];

struct Scanning(Files);

impl PackageQuerier for Scanning {
    fn query(&self, packages: &[String]) -> Result<Rows, EngineError> {
        Ok(packages.iter().map(|p| rows(p)).collect::<Vec<_>>().into())
    }

    fn universe(&self) -> Result<Rows, EngineError> {
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
        allow_partial: false,
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

/// A `.bzl` the rule resolves is named *inside the call that names the
/// repository*, so per-occurrence attribution lands it on `@generated` — and
/// because a repository rule may write anything it reads into a `.bzl` of its
/// own, the packages whose load closure reaches into that repository are
/// re-queried. Narrowing attribution must not lose that edge.
#[test]
fn a_bzl_named_inside_a_repository_rules_call_dirties_its_loaders() {
    let (_e, index) = base();
    let v = index.view();
    assert_eq!(
        v.repos_for_path("tpl/helper.bzl")
            .into_iter()
            .map(|r| v.repo_name(r))
            .collect::<Vec<_>>(),
        ["generated"]
    );
    let r = run(
        &index,
        &changed(&["tpl/helper.bzl"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    assert_eq!(r.stats.repos_label_only, 0);
    assert!(
        r.dirty_packages.contains(&"app".to_string())
            && r.dirty_packages.contains(&"lib".to_string()),
        "{:?}",
        r.dirty_packages
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
fn a_build_files_direct_external_load_is_seen_by_rule_a() {
    // Issue #4. `//direct` loads `@fmt//:defs.bzl` from its BUILD file, so a
    // `fmt` bump must re-query it: the macro it loads can now emit different
    // rules. `//app` and `//lib` load only main-repo `.bzl` files and stay put.
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
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(
        r.dirty_packages,
        vec!["direct".to_string()],
        "rule (a) must see a BUILD file's own external load"
    );
    // The re-query returned the same rows, so the label consumer is still the
    // only affected target.
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
fn a_legacy_workspace_edit_that_cannot_be_classified_is_global() {
    let (_e, index) = base();
    // An assignment at the top level: not a shape the statement diff reads.
    let r = run(
        &index,
        &changed(&["WORKSPACE"]),
        &texts_with_workspace(
            &format!("{WORKSPACE_BASE}VERSION = \"1\"\n"),
            &format!("{WORKSPACE_BASE}VERSION = \"2\"\n"),
        ),
    );
    assert!(r.global);
    assert!(r.global_reason.unwrap().contains("classify"));
}

/// A legacy `WORKSPACE` that declares the same repository the module file's
/// extension does, so the graph has somewhere to send it.
const WORKSPACE_BASE: &str = "load(\"@bazel_tools//tools/build_defs/repo:http.bzl\", \
     \"http_archive\")\n\
     http_archive(\n    name = \"generated\",\n    sha256 = \"aaa\",\n)\n\
     http_archive(\n    name = \"unused\",\n    sha256 = \"bbb\",\n)\n";

fn texts_with_workspace(base_ws: &str, head_ws: &str) -> FixedText {
    let mut m = texts(MODULE_BASE, MODULE_BASE).0;
    m.insert(
        ("base".to_string(), "WORKSPACE".to_string()),
        base_ws.to_string(),
    );
    m.insert(
        ("head".to_string(), "WORKSPACE".to_string()),
        head_ws.to_string(),
    );
    FixedText(m)
}

/// The sourcegraph shape: one `http_archive` in `WORKSPACE` moved, and only
/// the targets that depend on that repository are affected.
#[test]
fn a_legacy_workspace_declaration_names_its_repository() {
    let (_e, index) = base();
    let r = run(
        &index,
        &changed(&["WORKSPACE"]),
        &texts_with_workspace(WORKSPACE_BASE, &WORKSPACE_BASE.replace("aaa", "zzz")),
    );
    assert!(
        !r.global,
        "a WORKSPACE edit used to be global on its own: {:?}",
        r.global_reason
    );
    assert_eq!(r.direct, vec!["//lib:lib".to_string()]);
}

/// The other declaration in the same file moving reaches nothing, which is the
/// whole point of diffing the file rather than invalidating on it.
#[test]
fn an_unrelated_workspace_declaration_reaches_nothing() {
    let (_e, index) = base();
    let r = run(
        &index,
        &changed(&["WORKSPACE"]),
        &texts_with_workspace(WORKSPACE_BASE, &WORKSPACE_BASE.replace("bbb", "zzz")),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert!(r.direct.is_empty());
}

/// Toolchain registration has no edge in the graph wherever it is written.
#[test]
fn a_workspace_toolchain_registration_is_global() {
    let (_e, index) = base();
    let r = run(
        &index,
        &changed(&["WORKSPACE"]),
        &texts_with_workspace(
            &format!("{WORKSPACE_BASE}go_register_toolchains(version = \"1\")\n"),
            &format!("{WORKSPACE_BASE}go_register_toolchains(version = \"2\")\n"),
        ),
    );
    assert!(r.global);
    assert!(r.global_reason.unwrap().contains("toolchain"));
}

/// A `MODULE.bazel.lock` around one extension, whose `gen` extension
/// generates the repository `generated` with `spec` as its definition.
fn lock(spec: &str, digest: &str) -> String {
    format!(
        "{{\"lockFileVersion\": 24, \"registryFileHashes\": {{}}, \
         \"selectedYankedVersions\": {{}}, \"moduleExtensions\": {{\
         \"//bazel:ext.bzl%gen\": {{\"general\": {{\
         \"bzlTransitiveDigest\": \"{digest}\", \
         \"generatedRepoSpecs\": {{\"generated\": {{\"url\": \"{spec}\"}}}}}}}}}}}}"
    )
}

/// The same two revisions, with a lockfile on each side as well.
fn texts_with_lock(base_lock: &str, head_lock: &str) -> FixedText {
    let mut m = texts(MODULE_BASE, MODULE_BASE).0;
    m.insert(
        ("base".to_string(), "MODULE.bazel.lock".to_string()),
        base_lock.to_string(),
    );
    m.insert(
        ("head".to_string(), "MODULE.bazel.lock".to_string()),
        head_lock.to_string(),
    );
    FixedText(m)
}

/// The envoy shape: the lockfile moves on its own because an extension was
/// re-evaluated, and exactly one repository's definition came out different.
#[test]
fn a_lockfile_moving_on_its_own_names_the_repositories_that_moved() {
    let (_e, index) = base();
    let r = run(
        &index,
        &changed(&["MODULE.bazel.lock"]),
        &texts_with_lock(&lock("a", "d"), &lock("b", "d")),
    );
    assert!(
        !r.global,
        "the lone lockfile used to be an invalidator: {:?}",
        r.global_reason
    );
    assert_eq!(r.stats.repos_dirty_by_module, 1);
    // `//lib:lib` names `@generated//:thing`; `//app:app` reaches it.
    assert_eq!(r.direct, vec!["//lib:lib".to_string()]);
}

/// Only the extension's implementation digest moved: the extension was
/// re-evaluated and produced exactly the same repositories.
#[test]
fn a_lockfile_whose_digests_alone_moved_dirties_nothing() {
    let (_e, index) = base();
    let r = run(
        &index,
        &changed(&["MODULE.bazel.lock"]),
        &texts_with_lock(&lock("a", "d1"), &lock("a", "d2")),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_module, 0);
    assert!(r.direct.is_empty());
}

#[test]
fn a_lockfile_cambium_cannot_read_is_global() {
    let (_e, index) = base();
    let r = run(
        &index,
        &changed(&["MODULE.bazel.lock"]),
        &texts_with_lock(
            &lock("a", "d"),
            &lock("a", "d").replace("{\"lockFileVersion\"", "{\"novel\": 1, \"lockFileVersion\""),
        ),
    );
    assert!(r.global);
    assert!(r.global_reason.unwrap().contains("novel"));
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
    assert_eq!(
        r.direct,
        vec!["//bazel:bzl_srcs".to_string(), "//lib:lib".to_string()],
        "the repository consequence and the source-file consequence are both owed"
    );
}

/// The soundness hole the bazel subject found on `9a7ba209..cc5c15fa`.
///
/// `tools/build_defs/repo/utils.bzl` is loaded from `MODULE.bazel` (through
/// `http.bzl`) *and* listed in the `srcs` of fifteen filegroups in its own
/// package. Once the externals stage started claiming such a file as a
/// repository-defining input, and item 4 stopped sending it through
/// `rbuildfiles`, nothing left marked its ordinary source-file consumers: the
/// filegroups were `cambium_missing` against bazel-diff. Before those items
/// the edit went global, which hid the miss rather than fixing it.
#[test]
fn a_workspace_closure_bzl_still_reaches_the_filegroups_that_list_it() {
    let (_e, index) = base();
    let r = run(
        &index,
        &changed(&["bazel/repo.bzl"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    let a = affected(&r);
    assert!(
        a.contains(&"//bazel:bzl_srcs".to_string()),
        "the filegroup consuming the .bzl as a source must be affected: {a:?}"
    );
    assert!(
        a.contains(&"//bazel:embedded_tools".to_string()),
        "and everything above it: {a:?}"
    );
    // Without losing the repository consequence.
    assert!(a.contains(&"//lib:lib".to_string()), "{a:?}");
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
