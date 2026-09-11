//! How a run comes by its base index: the store, an ancestor replayed forward,
//! or a cold build — and what `--publish-base` does about it.
//!
//! Real git, synthetic Bazel: the ancestor walk is git plumbing plus a replay
//! through the same `compute` a normal run uses, and neither needs Bazel to be
//! exercised.

use cambium_engine::{AffectedRequest, BaselineOrigin, Config, Engine, SynthQuerier};
use cambium_index::IndexFile;
use cambium_synth::{ChangeSpec, GraphSpec, SynthGraph};

struct Fixture {
    _dir: tempfile::TempDir,
    engine: Engine,
    commits: Vec<String>,
    graphs: Vec<SynthGraph>,
}

fn fixture(revisions: u64) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let graph = cambium_synth::generate(&GraphSpec::preset("tiny").unwrap());
    let change = ChangeSpec {
        seed: 5,
        ..ChangeSpec::preset("typical-pr").unwrap()
    };
    let repo = cambium_synth::emit_git_history(graph, &change, revisions, &root, true)
        .expect("emitting a synthetic repository");

    let mut cfg = Config::load(&root).unwrap();
    cfg.baseline.store = cambium_engine::config::StoreConfig::Dir {
        path: root.join("_store"),
    };
    let engine = Engine::with_parts(
        cfg,
        cambium_diff::Repo::new(root.clone()),
        cambium_bazel::Bazel::at(root.clone(), Some("/nonexistent/bazel")),
    )
    .with_cache_dir(root.join("_cache"));

    Fixture {
        _dir: dir,
        engine,
        commits: repo.commits,
        graphs: repo.graphs,
    }
}

impl Fixture {
    fn request(&self, base: &str, head: &str) -> AffectedRequest {
        AffectedRequest {
            base: base.to_string(),
            head: head.to_string(),
            include_manual: true,
            explain: false,
            no_full_build: true,
            materialize: true,
            publish_base: false,
            max_depth: None,
            allow_partial: false,
        }
    }

    /// Publishes an index built from the graph at `i`, as a post-merge job would.
    fn publish_at(&self, i: usize) {
        let index = self
            .engine
            .full_build_with(&self.commits[i], &SynthQuerier::new(&self.graphs[i]))
            .unwrap();
        self.engine.publish_base_index(&index).unwrap();
    }

    /// The index blobs in the store. Each publish also writes a
    /// `commits/<lineage>/<sha>` pointer; those are not counted here.
    fn store_keys(&self) -> Vec<String> {
        self.engine
            .store()
            .unwrap()
            .list()
            .unwrap()
            .into_iter()
            .filter(|k| k.starts_with("indexes/"))
            .collect()
    }
}

#[test]
fn a_published_baseline_is_used_as_is() {
    let f = fixture(3);
    f.publish_at(0);
    let req = f.request(&f.commits[0], &f.commits[1]);
    let (_index, origin) = f
        .engine
        .resolve_baseline_with(&f.commits[0], &req, &SynthQuerier::new(&f.graphs[0]))
        .unwrap();
    assert_eq!(origin, BaselineOrigin::Fetched);
}

/// A run has to be able to say which base it read, or an offline replay has
/// only the index at the pin — a different graph — to work with. A fetched
/// baseline names the file the store served; a computed one is the index
/// itself, because that is the only copy there is.
#[test]
fn a_baseline_says_which_index_it_is_so_a_run_can_archive_it() {
    let f = fixture(3);
    f.publish_at(0);
    let req = f.request(&f.commits[0], &f.commits[1]);
    let (fetched, _) = f
        .engine
        .resolve_baseline_with(&f.commits[0], &req, &SynthQuerier::new(&f.graphs[0]))
        .unwrap();
    match fetched.into_base_index() {
        cambium_engine::BaseIndex::Mapped(p) => {
            assert_eq!(IndexFile::open(&p).unwrap().view().commit, f.commits[0]);
        }
        other => panic!("a fetched baseline is a file: {other:?}"),
    }

    let base = f.commits[2].clone();
    let req = f.request(&base, &base);
    let (replayed, _) = f
        .engine
        .resolve_baseline_with(&base, &req, &SynthQuerier::new(&f.graphs[2]))
        .unwrap();
    match replayed.into_base_index() {
        cambium_engine::BaseIndex::Owned(d) => assert_eq!(d.view().commit, base),
        other => panic!("a computed baseline is the index itself: {other:?}"),
    }
}

#[test]
fn a_missing_baseline_is_replayed_forward_from_an_ancestor() {
    let f = fixture(3);
    // Only the root commit has an index; the run wants one two commits later.
    f.publish_at(0);
    let base = f.commits[2].clone();
    let req = f.request(&base, &base);

    let (index, origin) = f
        .engine
        .resolve_baseline_with(&base, &req, &SynthQuerier::new(&f.graphs[2]))
        .unwrap();

    assert_eq!(
        origin,
        BaselineOrigin::Replayed {
            from: f.commits[0].clone()
        },
        "it should walk back to the nearest ancestor that has one"
    );
    assert_eq!(
        index.view().commit,
        base,
        "the replayed index describes the base commit"
    );

    // Replaying forward has to produce the same graph a build at that commit
    // would: this is the ancestor walk's whole claim.
    let fresh = f
        .engine
        .full_build_with(&base, &SynthQuerier::new(&f.graphs[2]))
        .unwrap();
    assert_eq!(
        edges(index.view()),
        edges(fresh.view()),
        "the replayed index disagrees with a build at the same commit"
    );
    assert_eq!(
        index.view().lineage,
        IndexFile::open(&f.engine.fetch_baseline(&f.commits[0]).unwrap().unwrap())
            .unwrap()
            .view()
            .lineage,
        "a replay stays in the ancestor's lineage"
    );
}

#[test]
fn the_walk_stops_after_max_ancestor_walk() {
    let mut f = fixture(4);
    f.publish_at(0);
    let mut cfg = f.engine.config().clone();
    cfg.baseline.max_ancestor_walk = 1;
    f.engine = f.engine.with_config(cfg);
    let base = f.commits[4].clone();
    let req = f.request(&base, &base);

    // The only index is four commits back, further than the walk may look, and
    // a cold build is refused.
    let err = f
        .engine
        .resolve_baseline_with(&base, &req, &SynthQuerier::new(&f.graphs[4]))
        .unwrap_err();
    assert_eq!(err.exit_code(), 4, "exit 4 means no baseline: {err}");
}

#[test]
fn no_baseline_at_all_falls_back_to_a_cold_build() {
    let f = fixture(2);
    let base = f.commits[1].clone();
    let req = AffectedRequest {
        no_full_build: false,
        ..f.request(&base, &base)
    };
    let (index, origin) = f
        .engine
        .resolve_baseline_with(&base, &req, &SynthQuerier::new(&f.graphs[1]))
        .unwrap();
    assert_eq!(origin, BaselineOrigin::ColdBuild);
    assert_eq!(index.view().commit, base);
}

#[test]
fn publish_base_publishes_the_replayed_index_under_the_base_commit() {
    let f = fixture(3);
    f.publish_at(0);
    let before = f.store_keys();
    assert_eq!(before.len(), 1, "only the ancestor is published");

    let base = f.commits[2].clone();
    let req = AffectedRequest {
        publish_base: true,
        ..f.request(&base, &base)
    };
    let (index, origin) = f
        .engine
        .resolve_baseline_with(&base, &req, &SynthQuerier::new(&f.graphs[2]))
        .unwrap();
    assert!(matches!(origin, BaselineOrigin::Replayed { .. }));
    f.engine.publish_baseline(&index, false).unwrap();

    let after = f.store_keys();
    assert_eq!(after.len(), 2, "the replayed base should now be published");
    let key = cambium_index::index_key(&index.view().lineage, &f.engine.tree_of(&base));
    assert!(after.contains(&key), "expected {key} in {after:?}");

    // ...and the next run finds it rather than replaying again.
    let (_, origin) = f
        .engine
        .resolve_baseline_with(&base, &req, &SynthQuerier::new(&f.graphs[2]))
        .unwrap();
    assert_eq!(origin, BaselineOrigin::Fetched);
}

#[test]
fn affected_never_publishes_the_head_index() {
    let f = fixture(3);
    f.publish_at(0);

    let base = f.commits[1].clone();
    let head = f.commits[2].clone();
    let req = AffectedRequest {
        publish_base: true,
        ..f.request(&base, &head)
    };

    // Resolve the base (replayed from commit 0, so it gets published), then run
    // the head side exactly as `affected` does.
    let (base_index, origin) = f
        .engine
        .resolve_baseline_with(&base, &req, &SynthQuerier::new(&f.graphs[1]))
        .unwrap();
    assert!(matches!(origin, BaselineOrigin::Replayed { .. }));
    f.engine.publish_baseline(&base_index, false).unwrap();

    let changes = f.engine.repo().diff(&base, &head).unwrap();
    let out = f
        .engine
        .compute(
            base_index.view(),
            &changes,
            &SynthQuerier::new(&f.graphs[2]),
            &req,
        )
        .unwrap();
    assert!(out.index.is_some(), "the run produced a head index");

    let keys = f.store_keys();
    let head_key = cambium_index::index_key(
        &out.index.as_ref().unwrap().header.lineage,
        &f.engine.tree_of(&head),
    );
    assert!(
        !keys.contains(&head_key),
        "affected must not publish the head index: {keys:?}"
    );
    assert_eq!(
        keys.len(),
        2,
        "only the ancestor and the computed base: {keys:?}"
    );
}

#[test]
fn publish_base_does_nothing_when_the_baseline_was_already_published() {
    let f = fixture(2);
    f.publish_at(0);
    let before = f.store_keys();

    let req = AffectedRequest {
        publish_base: true,
        ..f.request(&f.commits[0], &f.commits[1])
    };
    let (_, origin) = f
        .engine
        .resolve_baseline_with(&f.commits[0], &req, &SynthQuerier::new(&f.graphs[0]))
        .unwrap();
    assert_eq!(origin, BaselineOrigin::Fetched);
    // `affected` skips the publish for a fetched baseline; nothing changed.
    assert_eq!(f.store_keys(), before);
}

fn edges(v: cambium_index::IndexRef<'_>) -> std::collections::BTreeSet<(String, String)> {
    let mut out = std::collections::BTreeSet::new();
    for id in 0..v.num_nodes() {
        if v.is_tombstone(id) {
            continue;
        }
        for &d in v.forward.row(id) {
            out.insert((v.label(id).to_string(), v.label(d).to_string()));
        }
    }
    out
}
