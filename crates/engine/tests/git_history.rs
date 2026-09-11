//! The engine driven by real `git diff` output over an emitted synthetic
//! repository. Bazel is still stubbed out — this is about the git and
//! classification path being wired to the graph path correctly.

use std::collections::BTreeSet;

use cambium_engine::{AffectedRequest, Config, Engine, SynthQuerier};
use cambium_synth::{reference::reference_affected, ChangeSpec, GraphSpec};

#[test]
fn walks_a_real_git_history() {
    let dir = tempfile::tempdir().unwrap();
    let graph = cambium_synth::generate(&GraphSpec::preset("tiny").unwrap());
    let change_spec = ChangeSpec {
        seed: 7,
        ..ChangeSpec::preset("typical-pr").unwrap()
    };
    let repo = cambium_synth::emit_git_history(graph, &change_spec, 6, dir.path(), true)
        .expect("emitting a synthetic repository");
    assert!(
        repo.commits.len() > 1,
        "history should have several commits"
    );

    // The emitted workspace carries its own cambium.toml; load it, so the test
    // exercises the same invalidator rules a user of the emitter would get.
    let cfg = Config::load(dir.path()).unwrap();
    let engine = Engine::with_parts(
        cfg,
        cambium_diff::Repo::new(dir.path().to_path_buf()),
        cambium_bazel::Bazel::at(dir.path().to_path_buf(), Some("/nonexistent/bazel")),
    );

    let mut index = engine
        .full_build_with(&repo.commits[0], &SynthQuerier::new(&repo.graphs[0]))
        .unwrap();

    for i in 1..repo.commits.len() {
        let (base, head) = (&repo.commits[i - 1], &repo.commits[i]);
        let changes = engine.repo().diff(base, head).unwrap();
        assert!(!changes.is_empty(), "commit {i} changed nothing");

        let req = AffectedRequest {
            base: base.clone(),
            head: head.clone(),
            include_manual: true,
            explain: false,
            no_full_build: true,
            materialize: true,
            publish_base: false,
            max_depth: None,
            allow_partial: false,
        };
        let out = engine
            .compute(
                index.view(),
                &changes,
                &SynthQuerier::new(&repo.graphs[i]),
                &req,
            )
            .unwrap_or_else(|e| panic!("commit {i}: {e}"));

        let ours: BTreeSet<String> = out.report.affected_labels().into_iter().collect();
        let theirs = reference_affected(&repo.graphs[i - 1], &repo.graphs[i]);
        let missing: Vec<&String> = theirs.difference(&ours).collect();
        assert!(
            missing.is_empty(),
            "commit {i} missed {} targets: {:?}",
            missing.len(),
            missing.iter().take(5).collect::<Vec<_>>()
        );
        assert_eq!(ours, theirs, "commit {i}");
        index = out.index.expect("no global invalidation expected");
    }
}
