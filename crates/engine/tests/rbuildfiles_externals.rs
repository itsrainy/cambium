//! A changed `.bzl` whose loader is not a package BUILD file.
//!
//! `MODULE.bazel` can pull a `.bzl` in through a module extension or
//! `use_repo_rule`, and `WORKSPACE` through a plain `load()`. Such a file
//! defines repository rules: changing it changes the contents of an external
//! repository, and so every target that depends on one. None of that is in the
//! main tree's diff, so the only sound answer is a global invalidation.
//!
//! `rbuildfiles` reports that case by naming the external repository
//! (`@my_repo//:BUILD`) or `//external:WORKSPACE` instead of a main-repo
//! package — measured on 9.2.0, 8.5.1 and 7.4.1, bzlmod and WORKSPACE mode;
//! see `cambium_bazel::Rbuildfiles`. The complementary fact is that it never
//! answers *empty* for such a file, which is what lets a genuinely empty
//! answer stay local: a `.bzl` nothing loads cannot change any target.

use cambium_bazel::Rbuildfiles;
use cambium_diff::{Change, Status};
use cambium_engine::{AffectedRequest, Config, Engine, EngineError, PackageQuerier, SynthQuerier};
use cambium_index::PackageUpdate;
use cambium_synth::{generate, GraphSpec};

/// A querier whose `rbuildfiles` returns a canned answer.
struct Canned<'a>(SynthQuerier<'a>, Rbuildfiles);

impl PackageQuerier for Canned<'_> {
    fn query(&self, packages: &[String]) -> Result<Vec<PackageUpdate>, EngineError> {
        self.0.query(packages)
    }
    fn universe(&self) -> Result<Vec<PackageUpdate>, EngineError> {
        self.0.universe()
    }
    fn rbuildfiles(&self, _: &[String]) -> Result<Rbuildfiles, EngineError> {
        Ok(self.1.clone())
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

#[test]
fn a_bzl_loaded_only_by_module_bazel_is_global() {
    let g = generate(&GraphSpec::preset("tiny").unwrap().with_seed(1));
    let e = engine();
    let index = e.full_build_with("c0", &SynthQuerier::new(&g)).unwrap();
    let bzl = g.bzl.first().expect("preset has a .bzl file").path.clone();
    let changes = vec![Change::new(Status::Modified, bzl.clone())];

    // bzlmod: the module extension's `.bzl` names only the repository it
    // defines. No main-repo package to re-query — but every target that
    // depends on `@my_repo//:x` is at risk.
    let q = Canned(
        SynthQuerier::new(&g),
        Rbuildfiles {
            packages: Vec::new(),
            workspace_level: vec!["@my_repo//:BUILD".into()],
        },
    );
    let out = e
        .compute(index.view(), &changes, &q, &req())
        .expect("a workspace-level loader is not an error");
    assert!(out.report.global, "must over-approximate, not miss");
    let reason = out.report.global_reason.as_deref().unwrap();
    assert!(reason.contains(&bzl), "reason names the path: {reason}");
    assert!(reason.contains("MODULE.bazel/WORKSPACE"), "{reason}");
    assert!(reason.contains("@my_repo//:BUILD"), "{reason}");
    assert!(out.report.stats.rbuildfiles_called);

    // WORKSPACE mode: the pseudo-package, alongside real packages. The real
    // packages do not make it safe to stay local.
    let q = Canned(
        SynthQuerier::new(&g),
        Rbuildfiles {
            packages: vec![g.packages[0].path.clone()],
            workspace_level: vec!["//external:WORKSPACE".into()],
        },
    );
    let out = e.compute(index.view(), &changes, &q, &req()).unwrap();
    assert!(out.report.global);
    assert!(out
        .report
        .global_reason
        .as_deref()
        .unwrap()
        .contains("//external:WORKSPACE"));
}

#[test]
fn a_bzl_nothing_loads_dirties_nothing() {
    let g = generate(&GraphSpec::preset("tiny").unwrap().with_seed(1));
    let e = engine();
    let index = e.full_build_with("c0", &SynthQuerier::new(&g)).unwrap();
    let bzl = g.bzl.first().expect("preset has a .bzl file").path.clone();
    let changes = vec![Change::new(Status::Modified, bzl)];

    let q = Canned(SynthQuerier::new(&g), Rbuildfiles::default());
    let out = e.compute(index.view(), &changes, &q, &req()).unwrap();
    assert!(!out.report.global, "a dead .bzl is not a global change");
    assert!(out.report.dirty_packages.is_empty());
    assert!(out.report.affected.is_empty());
    assert!(out.report.stats.rbuildfiles_called);
}
