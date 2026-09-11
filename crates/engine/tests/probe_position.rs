//! The probe that answers "warm or cold?", over a real git history and a real
//! directory store.
//!
//! Bazel never runs: the index is built from the synthetic row source, so this
//! exercises the part that has actually been wrong before — the ancestor walk
//! and the tree keys it fetches by — on a machine with no Bazel installed.

use cambium_engine::probe::{self, Plan};
use cambium_engine::store::DirStore;
use cambium_engine::{Config, Engine, SynthQuerier};
use cambium_synth::{ChangeSpec, GraphSpec};

struct Fixture {
    _dir: tempfile::TempDir,
    store_dir: tempfile::TempDir,
    repo: cambium_diff::Repo,
    commits: Vec<String>,
    cfg: Config,
    cache: tempfile::TempDir,
    /// Index into `commits` of the revision whose index was published.
    published: usize,
}

/// A five-commit synthetic repository with an index published for one of them.
fn fixture(publish_nth_from_head: usize) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let graph = cambium_synth::generate(&GraphSpec::preset("tiny").unwrap());
    let spec = ChangeSpec {
        seed: 11,
        ..ChangeSpec::preset("typical-pr").unwrap()
    };
    let emitted =
        cambium_synth::emit_git_history(graph, &spec, 5, dir.path(), true).expect("a git history");
    let cfg = Config::load(dir.path()).unwrap();
    let engine = Engine::with_parts(
        cfg.clone(),
        cambium_diff::Repo::new(dir.path().to_path_buf()),
        cambium_bazel::Bazel::at(dir.path().to_path_buf(), Some("/nonexistent/bazel")),
    )
    .with_cache_dir(cache.path().to_path_buf());

    // `emit_git_history` returns commits oldest-first.
    let commits = emitted.commits.clone();
    let nth = commits.len() - 1 - publish_nth_from_head;
    let index = engine
        .full_build_with(&commits[nth], &SynthQuerier::new(&emitted.graphs[nth]))
        .unwrap();
    let path = cache.path().join("published.cmbx");
    cambium_index::format::write_index(&path, &index).unwrap();
    let store = DirStore::new(store_dir.path().to_path_buf());
    let repo = cambium_diff::Repo::new(dir.path().to_path_buf());
    let tree = repo.tree_of(&commits[nth]).unwrap();
    cambium_engine::BaselineCache::new(&store, cache.path(), cfg.hash())
        .publish(&path, &index.header.lineage, &commits[nth], &tree)
        .expect("publishing into a directory store");

    Fixture {
        _dir: dir,
        store_dir,
        repo,
        commits,
        cfg,
        cache,
        published: nth,
    }
}

impl Fixture {
    fn probe(&self) -> probe::PositionProbe {
        let store = DirStore::new(self.store_dir.path().to_path_buf());
        probe::position(
            &self.repo,
            Some(&store),
            self.cache.path(),
            self.cfg.hash(),
            self.cfg.baseline.max_ancestor_walk,
        )
    }
}

#[test]
fn an_index_for_head_itself_is_warm_at_distance_zero() {
    let f = fixture(0);
    let p = f.probe();
    assert!(p.has_index, "{p:?}");
    assert_eq!(p.plan, Plan::Warm);
    assert_eq!(p.distance, Some(0));
    assert_eq!(p.nearest.as_deref(), p.head.as_deref());
    assert!(p.index_path.is_some(), "the fetched file is named");
    assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
}

/// The line `cambi status` exists to print: HEAD has nothing, an ancestor does,
/// and the distance is how many commits back it is.
#[test]
fn an_ancestors_index_is_a_splice_at_the_distance_back_to_it() {
    let f = fixture(3);
    let p = f.probe();
    assert!(!p.has_index);
    assert_eq!(p.plan, Plan::Splice);
    assert_eq!(p.distance, Some(3));
    assert_eq!(
        p.nearest.as_deref(),
        Some(f.commits[f.published].as_str()),
        "the walk stops at the first ancestor that has one"
    );
    let index = probe::index_at(p.index_path.as_ref().unwrap()).0.unwrap();
    assert_eq!(index.commit, f.commits[f.published]);
    assert!(index.packages > 0 && index.targets > 0, "{index:?}");
}

#[test]
fn an_empty_store_is_a_cold_build_and_not_a_failure() {
    let f = fixture(0);
    let empty = tempfile::tempdir().unwrap();
    let store = DirStore::new(empty.path().to_path_buf());
    let p = probe::position(
        &f.repo,
        Some(&store),
        f.cache.path(),
        f.cfg.hash(),
        f.cfg.baseline.max_ancestor_walk,
    );
    assert_eq!(p.plan, Plan::Cold);
    assert!(p.nearest.is_none() && p.distance.is_none());
    assert!(p.index_path.is_none());
    assert!(p.diagnostics.is_empty(), "an empty store is not a problem");
}

/// A store holding only indexes from another configuration is the same as an
/// empty one — and says so, once, with the code the store layout documents.
#[test]
fn a_lineage_from_another_configuration_is_skipped_and_named() {
    let f = fixture(0);
    let store = DirStore::new(f.store_dir.path().to_path_buf());
    let mut other = f.cfg.clone();
    other.invalidators.globs.push("something/else/**".into());
    let p = probe::position(
        &f.repo,
        Some(&store),
        f.cache.path(),
        other.hash(),
        other.baseline.max_ancestor_walk,
    );
    assert_eq!(
        p.plan,
        Plan::Cold,
        "an incompatible index is not a baseline"
    );
    let mismatch: Vec<_> = p
        .diagnostics
        .iter()
        .filter(|d| d.code == cambium_engine::Code::W003IndexConfigMismatch)
        .collect();
    assert_eq!(mismatch.len(), 1, "{:?}", p.diagnostics);

    // And the lineage probe turns that into the conflict `status` reports.
    let store_probe = probe::store(&other, Some(&store));
    let l = probe::lineage(&store_probe, &p, None);
    assert_eq!(l.conflicting.len(), 1, "{l:?}");
    assert_eq!(
        l.diagnostics[0].code,
        cambium_engine::Code::W019LineageConflict
    );
}
