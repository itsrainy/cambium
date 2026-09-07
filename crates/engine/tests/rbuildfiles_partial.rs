//! When Sky Query finishes with errors (`--keep_going`, exit 3), the
//! `rbuildfiles` answer may be missing exactly the package that loads the
//! changed `.bzl`. The engine must not trust it: the sound answer is a global
//! invalidation, never a quiet under-approximation. Found on the `bazel`
//! corpus replay, where external repositories that cannot be fetched on macOS
//! made every `.bzl` commit fail instead of degrade.

use cambium_bazel::BazelError;
use cambium_diff::{Change, Status};
use cambium_engine::{AffectedRequest, Config, Engine, EngineError, PackageQuerier, SynthQuerier};
use cambium_index::PackageUpdate;
use cambium_synth::{generate, GraphSpec};

struct PartialRbuildfiles<'a>(SynthQuerier<'a>);

impl PackageQuerier for PartialRbuildfiles<'_> {
    fn query(&self, packages: &[String]) -> Result<Vec<PackageUpdate>, EngineError> {
        self.0.query(packages)
    }
    fn universe(&self) -> Result<Vec<PackageUpdate>, EngineError> {
        self.0.universe()
    }
    fn rbuildfiles(&self, _: &[String]) -> Result<cambium_bazel::Rbuildfiles, EngineError> {
        Err(EngineError::Bazel(BazelError::Partial {
            args: "rbuildfiles(...)".into(),
            stderr: "ERROR: some external repository failed to fetch".into(),
        }))
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

#[test]
fn a_partial_rbuildfiles_answer_becomes_a_global_invalidation() {
    let g = generate(&GraphSpec::preset("tiny").unwrap().with_seed(1));
    let e = engine();
    let index = e.full_build_with("c0", &SynthQuerier::new(&g)).unwrap();
    let bzl = g.bzl.first().expect("preset has a .bzl file").path.clone();
    let changes = vec![Change::new(Status::Modified, bzl)];
    let req = AffectedRequest {
        base: "base".into(),
        head: "head".into(),
        include_manual: true,
        explain: false,
        no_full_build: true,
        materialize: false,
        publish_base: false,
        max_depth: None,
    };
    let out = e
        .compute(
            index.view(),
            &changes,
            &PartialRbuildfiles(SynthQuerier::new(&g)),
            &req,
        )
        .expect("a partial rbuildfiles answer is not an error");
    assert!(out.report.global, "must over-approximate, not fail or miss");
    assert!(out
        .report
        .global_reason
        .as_deref()
        .unwrap()
        .contains("rbuildfiles"));
    assert!(out.report.stats.rbuildfiles_called);

    // And a hard failure is still a hard failure: only exit 3 degrades.
    struct Failing<'a>(SynthQuerier<'a>);
    impl PackageQuerier for Failing<'_> {
        fn query(&self, p: &[String]) -> Result<Vec<PackageUpdate>, EngineError> {
            self.0.query(p)
        }
        fn universe(&self) -> Result<Vec<PackageUpdate>, EngineError> {
            self.0.universe()
        }
        fn rbuildfiles(&self, _: &[String]) -> Result<cambium_bazel::Rbuildfiles, EngineError> {
            Err(EngineError::Bazel(BazelError::Failed {
                args: "rbuildfiles(...)".into(),
                status: "exit status: 1".into(),
                stderr: String::new(),
            }))
        }
    }
    assert!(e
        .compute(
            index.view(),
            &changes,
            &Failing(SynthQuerier::new(&g)),
            &req
        )
        .is_err());
}
