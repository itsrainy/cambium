//! `limits.max_dirty_packages`: when a diff dirties more packages than a
//! per-package re-query is worth, the run re-indexes `//...` at head instead.
//!
//! The property that matters is that this is a *cost* decision. The limit it
//! replaced (`max_changed_files`) turned a large diff into a global
//! invalidation, which throws the answer away; a cold rebuild keeps it. So
//! every test here builds the same scenario twice — once under a limit that
//! cannot fire, once under a limit that always does — and requires the two
//! answers to be identical.

use cambium_engine::{AffectedRequest, Config, Engine, SynthQuerier};
use cambium_index::IndexData;
use cambium_synth::{commit, generate, ChangeSpec, GraphSpec, SynthGraph};

fn engine(max_dirty_packages: usize) -> Engine {
    let mut cfg = Config::default();
    cfg.limits.max_dirty_packages = max_dirty_packages;
    let dir = std::env::temp_dir().join("cambium-tests");
    let _ = std::fs::create_dir_all(&dir);
    Engine::with_parts(
        cfg,
        cambium_diff::Repo::new(dir.clone()),
        cambium_bazel::Bazel::at(dir, Some("/nonexistent/bazel")),
    )
}

fn request(materialize: bool) -> AffectedRequest {
    AffectedRequest {
        base: "base".into(),
        head: "head".into(),
        include_manual: true,
        explain: false,
        no_full_build: true,
        materialize,
        publish_base: false,
        max_depth: None,
        allow_partial: false,
    }
}

/// A revision that dirties several packages, so that a limit can sit on either
/// side of it.
fn scenario() -> (SynthGraph, SynthGraph, IndexData, Vec<cambium_diff::Change>) {
    let e = engine(0);
    let before = generate(&GraphSpec::preset("small").unwrap().with_seed(7));
    let base = e
        .full_build_with("c0", &SynthQuerier::new(&before))
        .expect("full build");
    let mut after = before.clone();
    let c = commit(
        &mut after,
        &ChangeSpec {
            seed: 0,
            ..ChangeSpec::preset("refactor").unwrap()
        },
        0,
    );
    assert!(!c.changes.is_empty(), "the preset produced no change");
    (before, after, base, c.changes)
}

fn run(
    e: &Engine,
    base: &IndexData,
    after: &SynthGraph,
    changes: &[cambium_diff::Change],
) -> cambium_engine::RunOutcome {
    e.compute(
        base.view(),
        changes,
        &SynthQuerier::new(after),
        &request(true),
    )
    .expect("run")
}

/// Under the limit: the ordinary incremental path, querying only what is dirty.
#[test]
fn a_dirty_set_under_the_limit_is_spliced_incrementally() {
    let (_, after, base, changes) = scenario();
    let e = engine(500);
    let out = run(&e, &base, &after, &changes);
    assert!(
        !out.report.stats.cold_rebuild,
        "the limit must not have fired"
    );
    assert!(out.report.cold_rebuild_reason.is_none());
    assert!(!out.report.global);
    assert!(
        (100..500).contains(&out.report.stats.dirty_packages),
        "the scenario has to straddle the two limits these tests use: {}",
        out.report.stats.dirty_packages
    );
    // Proof that only the dirty packages were asked for: the same revision
    // under a limit that fires queries strictly more.
    let cold = run(&engine(100), &base, &after, &changes);
    assert!(
        out.report.stats.queried_targets < cold.report.stats.queried_targets,
        "an incremental run queried {} targets; the whole repository is {}",
        out.report.stats.queried_targets,
        cold.report.stats.queried_targets
    );
}

/// Over the limit: `//...` at head, and the same answer.
#[test]
fn a_dirty_set_over_the_limit_cold_rebuilds_and_answers_the_same() {
    let (_, after, base, changes) = scenario();
    let incremental = run(&engine(500), &base, &after, &changes);
    let cold = run(&engine(100), &base, &after, &changes);
    assert!(!incremental.report.stats.cold_rebuild);

    assert!(cold.report.stats.cold_rebuild);
    assert_eq!(
        cold.report.cold_rebuild_reason.as_deref(),
        Some(
            format!(
                "dirty packages {} exceed limits.max_dirty_packages = 100",
                cold.report.stats.dirty_packages
            )
            .as_str()
        )
    );
    // A cold rebuild is not a global invalidation: the answer is enumerated.
    assert!(!cold.report.global);
    assert!(cold.report.global_reason.is_none());
    assert!(!cold.report.affected.is_empty());

    assert_eq!(
        cold.report.affected, incremental.report.affected,
        "the cold rebuild must produce exactly the incremental affected set"
    );
    assert_eq!(cold.report.direct, incremental.report.direct);
    assert_eq!(cold.report.tests, incremental.report.tests);
    assert_eq!(
        cold.report.dirty_packages, incremental.report.dirty_packages,
        "the dirty set is what triggered the rebuild; it is still reported"
    );

    // It really did ask for the whole repository, and the index it spliced is
    // the same graph the incremental splice produced.
    assert!(cold.report.stats.queried_targets > incremental.report.stats.queried_targets);
    let (a, b) = (
        cold.index.expect("materialised"),
        incremental.index.expect("materialised"),
    );
    assert_eq!(a.header.counts.nodes, b.header.counts.nodes);
    assert_eq!(a.header.counts.edges, b.header.counts.edges);
}

/// The delta overlay takes the same route, and both routes still agree.
#[test]
fn the_overlay_cold_rebuilds_too() {
    let (_, after, base, changes) = scenario();
    let e = engine(100);
    let overlay = e
        .compute(
            base.view(),
            &changes,
            &SynthQuerier::new(&after),
            &request(false),
        )
        .expect("overlay run");
    assert!(overlay.index.is_none());
    assert!(overlay.report.stats.cold_rebuild);
    let materialised = run(&e, &base, &after, &changes);
    assert_eq!(overlay.report.affected, materialised.report.affected);
    assert_eq!(overlay.report.direct, materialised.report.direct);
}

/// A diff of ten thousand source files under one package is not a reason to
/// rebuild anything: this is the `apollo` commit the old `max_changed_files`
/// limit went global on.
#[test]
fn a_huge_source_only_diff_neither_rebuilds_nor_goes_global() {
    let e = engine(500);
    let g = generate(&GraphSpec::preset("small").unwrap().with_seed(3));
    let base = e.full_build_with("c0", &SynthQuerier::new(&g)).unwrap();
    let changes: Vec<cambium_diff::Change> = (0..10_146)
        .map(|i| {
            cambium_diff::Change::new(
                cambium_diff::Status::Modified,
                format!("no/such/package/f{i}.cc"),
            )
        })
        .collect();
    let out = e
        .compute(
            base.view(),
            &changes,
            &SynthQuerier::new(&g),
            &request(true),
        )
        .expect("run");
    assert!(!out.report.global, "10146 files is not a reason to give up");
    assert!(!out.report.stats.cold_rebuild);
    assert_eq!(out.report.stats.changed_files, 10_146);
    assert!(out.report.affected.is_empty());
}
