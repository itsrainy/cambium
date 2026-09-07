//! A cold build over a repository Bazel cannot fully load.
//!
//! `bazel query //...` under `--keep_going` exits 3 when some package fails to
//! load, and the engine accepts that answer — the incremental path already
//! does, and it only makes sense because a cold build drops the same packages.
//! What the cold build owes in exchange is honesty: the index it produces is
//! flagged partial and carries the count and a sample of the packages that
//! failed, and a splice on top of it stays flagged.
//!
//! Found on the `apollo` corpus repo, whose deps live in `WORKSPACE`: under
//! Bazel 9, which no longer reads it, 770 of 771 packages fail to load.

use cambium_bazel::LoadErrors;
use cambium_engine::query::Universe;
use cambium_engine::{Config, Engine, EngineError, PackageQuerier, SynthQuerier};
use cambium_index::{IndexBuilder, PackageUpdate};
use cambium_synth::{generate, GraphSpec};

/// A querier whose universe query "exited 3": the same rows, plus the load
/// errors Bazel printed for the packages it could not load.
struct PartialUniverse<'a>(SynthQuerier<'a>, LoadErrors);

impl PackageQuerier for PartialUniverse<'_> {
    fn query(&self, packages: &[String]) -> Result<Vec<PackageUpdate>, EngineError> {
        self.0.query(packages)
    }
    fn universe(&self) -> Result<Universe, EngineError> {
        Ok(Universe {
            updates: self.0.universe()?.updates,
            partial: Some(self.1.clone()),
        })
    }
    fn rbuildfiles(&self, p: &[String]) -> Result<cambium_bazel::Rbuildfiles, EngineError> {
        self.0.rbuildfiles(p)
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

fn load_errors() -> LoadErrors {
    cambium_bazel::parse_load_errors(concat!(
        "ERROR: error loading package 'modules/planning': boom\n",
        "ERROR: package contains errors: third_party/bazel: name 'py_library' is not defined\n",
        "WARNING: --keep_going specified, ignoring errors.\n",
    ))
}

#[test]
fn a_universe_query_that_exits_three_yields_a_partial_index() {
    let g = generate(&GraphSpec {
        packages: 20,
        seed: 7,
        ..Default::default()
    });
    let e = engine();
    let index = e
        .full_build_with(
            "head",
            &PartialUniverse(SynthQuerier::new(&g), load_errors()),
        )
        .unwrap();

    let v = index.view();
    assert!(v.partial, "the index must record that it is incomplete");
    assert_eq!(v.packages_in_error, 2);
    assert_eq!(
        v.error_packages,
        [
            "modules/planning".to_string(),
            "third_party/bazel".to_string()
        ]
    );
    // The rows Bazel did manage to load are still there: a partial index is a
    // usable index, not an error.
    assert!(v.num_nodes() > 0);
}

#[test]
fn a_complete_universe_query_leaves_the_index_unflagged() {
    let g = generate(&GraphSpec {
        packages: 20,
        seed: 7,
        ..Default::default()
    });
    let index = engine()
        .full_build_with("head", &SynthQuerier::new(&g))
        .unwrap();
    assert!(!index.view().partial);
    assert_eq!(index.view().packages_in_error, 0);
}

#[test]
fn a_splice_onto_a_partial_base_stays_partial() {
    let g = generate(&GraphSpec {
        packages: 20,
        seed: 7,
        ..Default::default()
    });
    let e = engine();
    let base = e
        .full_build_with(
            "base",
            &PartialUniverse(SynthQuerier::new(&g), load_errors()),
        )
        .unwrap();

    let mut b = IndexBuilder::from_view(base.view());
    b.apply_packages(&[]);
    let spliced = b.finish("head");
    assert!(
        spliced.view().partial,
        "an update over a base that is missing packages is still missing them"
    );
    assert_eq!(spliced.view().packages_in_error, 2);
}
