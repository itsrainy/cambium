//! An edit to a file a repository is *built from* is not an edit to a `.bzl`
//! that repository *exports*.
//!
//! `@mod` is `api/` under another name, and `//app`'s load closure reaches
//! `@mod//bazel:defs.bzl`. Before the path mapping, an edit to any file under
//! `api/` re-queried every package with that load — on envoy, all twelve
//! hundred of them, which is a cold rebuild. `api/envoy/x.proto` translated
//! into the repository is `envoy/x.proto`, which nothing loads, so only the
//! label consumers of `@mod` are affected.
//!
//! The three cases that must keep the wide answer are here too: an edit that
//! *is* the loaded `.bzl`, a `MODULE.bazel` version bump, and a repository
//! whose mapping could not be derived.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::{FixedText, WorkspaceText};
use cambium_engine::query::Universe;
use cambium_engine::{AffectedRequest, Config, Engine, EngineError, PackageQuerier};
use cambium_index::{FileText, IndexData, PackageUpdate, TargetRow};

/// `mod` is `api/`, by `local_path_override`; `opaque` is defined by an
/// extension whose `.bzl` names `//conf:settings.yml` as an input, and no
/// static shape says where that file lands inside it.
const MODULE_BASE: &str = "module(name = \"demo\", version = \"1\")\n\
     bazel_dep(name = \"mod\", version = \"1.0\")\n\
     local_path_override(module_name = \"mod\", path = \"api\")\n\
     gen = use_extension(\"//bazel:ext.bzl\", \"gen\")\n\
     use_repo(gen, \"opaque\")\n";

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
    put(
        "bazel/ext.bzl",
        "CONF = attr.label(default = \"@demo//conf:settings.yml\")\n",
    );
    // `//app` loads a `.bzl` out of `@mod`, transitively; `//lib` only names a
    // target in it. Those are rules (2) and (1).
    put("app/BUILD", "load(\"//bazel:defs.bzl\", \"r\")\n");
    put("bazel/defs.bzl", "load(\"@mod//bazel:defs.bzl\", \"r\")\n");
    put("lib/BUILD", "");
    put("conf/BUILD", "");
    Files(m)
}

fn rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "app" => vec![TargetRow::rule("//app:app", "demo_lib", vec![])],
        "lib" => vec![TargetRow::rule(
            "//lib:lib",
            "demo_lib",
            vec!["@mod//:thing".into()],
        )],
        "conf" => vec![TargetRow::rule(
            "//conf:conf",
            "demo_lib",
            vec!["@opaque//:thing".into()],
        )],
        _ => Vec::new(),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

const PACKAGES: &[&str] = &["app", "lib", "conf"];

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

    fn repo_mapping(&self) -> cambium_index::RepoMapping {
        cambium_index::RepoMapping::from_dump(
            "{\"\":\"\",\"demo\":\"\",\"mod\":\"mod+\",\"opaque\":\"opaque+\"}",
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

fn base() -> IndexData {
    engine().full_build_with("c0", &Scanning(files())).unwrap()
}

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
    paths: &[&str],
    texts: &dyn WorkspaceText,
) -> cambium_engine::AffectedReport {
    let changes: Vec<Change> = paths
        .iter()
        .map(|p| Change::new(Status::Modified, *p))
        .collect();
    engine()
        .compute_with_texts(index.view(), &changes, &Scanning(files()), &req(), texts)
        .unwrap()
        .report
}

#[test]
fn the_index_records_where_a_local_path_override_lands() {
    let index = base();
    let v = index.view();
    let mod_id = (0..v.num_repos())
        .find(|&r| v.repo_name(r) == "mod")
        .expect("the overridden module is a repository");
    let maps = v.repo_path_maps(mod_id);
    assert_eq!(maps.len(), 1, "{maps:?}");
    assert_eq!(maps[0].from, "api/");
    assert_eq!(maps[0].to, "");
    assert_eq!(
        maps[0].translate("api/envoy/x.proto").as_deref(),
        Some("envoy/x.proto")
    );
    // The extension-defined repository has no derivable mapping, which is what
    // keeps the old behaviour for it.
    let opaque = (0..v.num_repos())
        .find(|&r| v.repo_name(r) == "opaque")
        .expect("the extension's repository");
    assert!(v.repo_path_maps(opaque).is_empty());
}

#[test]
fn an_input_edit_under_a_mapped_repository_reaches_only_its_label_consumers() {
    let index = base();
    let r = run(
        &index,
        &["api/envoy/config/v3/x.proto"],
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    assert_eq!(r.stats.repos_label_only, 1);
    // Rule (1): `//lib:lib` names `@mod//:thing`. Rule (2) did not fire, so
    // `//app`, whose load closure reaches `@mod//bazel:defs.bzl`, is neither
    // dirty nor affected.
    assert_eq!(r.direct, vec!["//lib:lib".to_string()]);
    assert_eq!(r.affected_labels(), vec!["//lib:lib".to_string()]);
    assert_eq!(r.stats.dirty_packages, 0, "{:?}", r.dirty_packages);
}

#[test]
fn an_input_edit_that_is_the_loaded_bzl_still_dirties_its_packages() {
    let index = base();
    let r = run(
        &index,
        &["api/bazel/defs.bzl"],
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_label_only, 0);
    assert!(
        r.dirty_packages.contains(&"app".to_string()),
        "{:?}",
        r.dirty_packages
    );
}

#[test]
fn a_module_version_bump_still_dirties_the_loading_packages() {
    let index = base();
    let head = MODULE_BASE.replace(
        "bazel_dep(name = \"mod\", version = \"1.0\")",
        "bazel_dep(name = \"mod\", version = \"2.0\")",
    );
    let r = run(&index, &["MODULE.bazel"], &texts(MODULE_BASE, &head));
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_module, 1);
    assert_eq!(r.stats.repos_label_only, 0);
    assert!(
        r.dirty_packages.contains(&"app".to_string()),
        "any file in a bumped module may have moved: {:?}",
        r.dirty_packages
    );
}

/// A version bump *and* an input edit in one commit: the bump keeps rule (2).
#[test]
fn a_bump_beats_an_input_edit_in_the_same_commit() {
    let index = base();
    let head = MODULE_BASE.replace(
        "bazel_dep(name = \"mod\", version = \"1.0\")",
        "bazel_dep(name = \"mod\", version = \"2.0\")",
    );
    let r = run(
        &index,
        &["MODULE.bazel", "api/envoy/x.proto"],
        &texts(MODULE_BASE, &head),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_label_only, 0);
    assert!(
        r.dirty_packages.contains(&"app".to_string()),
        "{:?}",
        r.dirty_packages
    );
}

/// `@opaque` has no derivable mapping, so an edit to a file it is declared to
/// read keeps the whole-closure answer.
#[test]
fn a_repository_with_no_derivable_mapping_keeps_the_old_behaviour() {
    let index = base();
    let r = run(
        &index,
        &["conf/settings.yml"],
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    assert_eq!(r.stats.repos_label_only, 0);
    assert_eq!(r.direct, vec!["//conf:conf".to_string()]);
}
