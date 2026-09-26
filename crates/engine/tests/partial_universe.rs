//! A cold build over a repository Bazel cannot fully load.
//!
//! `bazel query //...` under `--keep_going` exits 3 when some package fails to
//! load, and the engine accepts that answer — the incremental path already
//! does, and it only makes sense because a cold build drops the same packages.
//! What the cold build owes in exchange is honesty: the index it produces is
//! flagged partial and carries the count and a sample of the packages that
//! failed, and a splice on top of it stays flagged.
//!
//! Found on the `apollo` subject, whose deps live in `WORKSPACE`: under
//! Bazel 9, which no longer reads it, 770 of 771 packages fail to load.

use cambium_bazel::LoadErrors;
use cambium_engine::query::Rows;
use cambium_engine::{AffectedRequest, Config, Engine, EngineError, PackageQuerier, SynthQuerier};
use cambium_index::IndexBuilder;
use cambium_synth::{generate, GraphSpec};
use cambium_testkit::engine;

/// A querier whose universe query "exited 3": the same rows, plus the load
/// errors Bazel printed for the packages it could not load.
struct PartialUniverse<'a>(SynthQuerier<'a>, LoadErrors);

impl PackageQuerier for PartialUniverse<'_> {
    fn query(&self, packages: &[String]) -> Result<Rows, EngineError> {
        self.0.query(packages)
    }
    fn universe(&self) -> Result<Rows, EngineError> {
        Ok(Rows {
            updates: self.0.universe()?.updates,
            partial: Some(self.1.clone()),
            unloaded: Vec::new(),
        })
    }
    // The rest is the synthetic tree's own, so an index built through this
    // has the loading layer and closure a run needs to splice onto it.
    fn load_source(&self) -> Option<&dyn cambium_index::FileText> {
        self.0.load_source()
    }
    fn starlark_files(&self) -> Option<Vec<String>> {
        self.0.starlark_files()
    }
    fn can_dump_loading_layer(&self) -> bool {
        self.0.can_dump_loading_layer()
    }
    fn dump_loading_layer(
        &self,
        filter: &str,
    ) -> Result<(String, cambium_index::sky::Paths), EngineError> {
        self.0.dump_loading_layer(filter)
    }
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

/// A querier whose *incremental* query "exited 3": the rows it could load,
/// plus the load errors for the packages it could not.
///
/// This is the transient-failure shape — an unreachable registry, a flaky
/// network — which is exactly when it matters: the packages that failed to
/// load come back as empty updates and are tombstoned, and the index built on
/// them is missing them for good.
struct PartialQuery<'a>(SynthQuerier<'a>, LoadErrors);

impl PackageQuerier for PartialQuery<'_> {
    fn query(&self, packages: &[String]) -> Result<Rows, EngineError> {
        Ok(Rows {
            updates: self.0.query(packages)?.updates,
            partial: Some(self.1.clone()),
            unloaded: Vec::new(),
        })
    }
    fn universe(&self) -> Result<Rows, EngineError> {
        self.0.universe()
    }
}

fn incremental_head(e: &Engine, partial: bool) -> cambium_index::IndexData {
    use cambium_synth::Mutation;

    let before = generate(&GraphSpec {
        packages: 20,
        seed: 7,
        ..Default::default()
    });
    let base = e
        .full_build_with("base", &SynthQuerier::new(&before))
        .unwrap();
    let t = (0..before.targets.len() as u32)
        .find(|&i| !before.targets[i as usize].srcs.is_empty())
        .unwrap();
    let slot = before.targets[t as usize].srcs[0];
    let mut after = before.clone();
    let changes = cambium_synth::apply_one(&mut after, &Mutation::ModifySource { target: t, slot });

    let req = AffectedRequest {
        materialize: true,
        no_full_build: true,
        ..AffectedRequest::new("base", "head")
    };
    let q = SynthQuerier::new(&after);
    let out = if partial {
        e.compute(base.view(), &changes, &PartialQuery(q, load_errors()), &req)
    } else {
        e.compute(base.view(), &changes, &q, &req)
    };
    out.unwrap().index.expect("a spliced head index")
}

#[test]
fn an_incremental_query_that_exits_three_yields_a_partial_index() {
    // Before this, the incremental path threw the load errors away: the
    // packages were tombstoned and the resulting head index claimed to be
    // complete. `baseline update` would then publish an index permanently
    // missing them, with nothing to say so.
    let index = incremental_head(&engine(), true);
    assert!(index.view().partial);
    assert_eq!(index.view().packages_in_error, 2);
    assert_eq!(
        index.view().error_packages,
        [
            "modules/planning".to_string(),
            "third_party/bazel".to_string()
        ]
    );
}

#[test]
fn a_complete_incremental_query_leaves_the_index_unflagged() {
    assert!(!incremental_head(&engine(), false).view().partial);
}

#[test]
fn publishing_a_partial_index_is_refused_unless_it_is_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.baseline.store = cambium_engine::config::StoreConfig::Dir {
        path: dir.path().join("store"),
    };
    let e = Engine::with_parts(
        cfg,
        cambium_diff::Repo::new(dir.path().to_path_buf()),
        cambium_bazel::Bazel::at(dir.path().to_path_buf(), Some("/nonexistent/bazel")),
    )
    .with_cache_dir(dir.path().join("cache"));

    let mut index = incremental_head(&e, true);
    index.header.config_hash = e.config().hash();
    let path = dir.path().join("head.bin");
    cambium_index::format::write_index(&path, &index).unwrap();

    let err = e
        .publish_index(&path, &index.header.lineage, "head")
        .expect_err("a partial head index must not be published silently");
    assert!(
        matches!(
            err,
            EngineError::PartialIndex {
                packages_in_error: 2,
                ..
            }
        ),
        "{err}"
    );

    // The escape hatch: a repository that genuinely cannot load everything —
    // apollo under Bazel 9 — has to be able to say so once and get on with it.
    let publish_partial = |e: Engine, allow: bool| {
        let mut cfg = e.config().clone();
        cfg.baseline.publish_partial = cambium_engine::config::PartialPolicy::Blanket(allow);
        e.with_config(cfg)
    };
    let e = publish_partial(e, true);
    let keys = e
        .publish_index(&path, &index.header.lineage, "head")
        .expect("publish_partial makes it explicit and allowed");
    assert!(!keys.is_empty());

    // The list form draws the line the boolean cannot: naming one of the two
    // failing packages still refuses, because the other one is not an absence
    // this subject expects. This is the shape of the xla index of 2026-09-18,
    // which was short seventeen packages an absent `@llvm-project` took with
    // it and published anyway under a blanket `true`.
    let expect = |e: Engine, names: &[&str]| {
        let mut cfg = e.config().clone();
        cfg.baseline.publish_partial = cambium_engine::config::PartialPolicy::Expected(
            names.iter().map(|s| s.to_string()).collect(),
        );
        e.with_config(cfg)
    };
    let e = expect(e, &["//modules/planning"]);
    let err = e
        .publish_index(&path, &index.header.lineage, "head3")
        .expect_err("third_party/bazel is not on the list");
    match &err {
        EngineError::PartialIndexUnexpected { packages, .. } => {
            assert_eq!(packages, &vec!["third_party/bazel".to_string()], "{err}")
        }
        _ => panic!("{err}"),
    }

    // Naming both, one of them by prefix, clears it.
    let e = expect(e, &["//modules/...", "//third_party/bazel"]);
    assert!(!e
        .publish_index(&path, &index.header.lineage, "head4")
        .expect("both failures are expected here")
        .is_empty());

    // And a complete index was never in question.
    let mut good = incremental_head(&e, false);
    good.header.config_hash = e.config().hash();
    let e = publish_partial(e, false);
    let path = dir.path().join("good.bin");
    cambium_index::format::write_index(&path, &good).unwrap();
    e.publish_index(&path, &good.header.lineage, "head2")
        .unwrap();
}

// ------------------------------------------- packages declared deleted

/// Envoy's stderr for a dirty package its own `--deleted_packages` names, as
/// `bench history envoy` met it at 75994f65 ("test: Improve handling of repo
/// source dir (#46740)", which touches `mobile/test/common/integration/BUILD`).
const DELETED_PACKAGE_STDERR: &str = concat!(
    "ERROR: Skipping '//mobile/test/common/integration:*': no such package \
     'mobile/test/common/integration': Package is considered deleted due to \
     --deleted_packages\n",
    "WARNING: --keep_going specified, ignoring errors.\n",
);

fn outcome(stderr: &str) -> cambium_bazel::QueryOutcome {
    let facts = cambium_bazel::parse_stderr_facts(stderr);
    cambium_bazel::QueryOutcome {
        exit_code: Some(3),
        success: false,
        stderr_tail: stderr.to_string(),
        targets_read: 0,
        load_errors: facts.load_errors,
        deleted_packages: facts.deleted_packages,
    }
}

/// A querier whose queries "exited 3" with the given stderr, judged by the
/// same rule the real `BazelQuerier` applies to a real one.
struct StderrQuery<'a>(SynthQuerier<'a>, &'static str);

impl PackageQuerier for StderrQuery<'_> {
    fn query(&self, packages: &[String]) -> Result<Rows, EngineError> {
        Ok(Rows {
            updates: self.0.query(packages)?.updates,
            partial: cambium_engine::query::partial_of(&outcome(self.1)),
            unloaded: Vec::new(),
        })
    }
    fn universe(&self) -> Result<Rows, EngineError> {
        Ok(Rows {
            updates: self.0.universe()?.updates,
            partial: cambium_engine::query::partial_of(&outcome(self.1)),
            unloaded: Vec::new(),
        })
    }
}

#[test]
fn a_package_the_invocation_declared_deleted_is_not_a_partial_answer() {
    // A cold build never names such a package — `//...` skips it and exits 0 —
    // so its index is complete without it. The incremental path asks for it by
    // name, because a file under it changed, and gets exit 3; before this the
    // spliced index was flagged partial with *zero* error lines, and `baseline
    // update` then refused to publish an index that was missing nothing.
    let o = outcome(DELETED_PACKAGE_STDERR);
    assert_eq!(o.deleted_packages, ["mobile/test/common/integration"]);
    assert_eq!(o.load_errors.count, 0);
    assert!(cambium_engine::query::partial_of(&o).is_none());
}

#[test]
fn a_real_load_failure_alongside_a_deleted_package_is_still_partial() {
    let stderr =
        format!("{DELETED_PACKAGE_STDERR}ERROR: error loading package 'modules/planning': boom\n");
    let e = cambium_engine::query::partial_of(&outcome(&stderr))
        .expect("a genuine load failure is still partial");
    assert_eq!(e.count, 1);
    assert_eq!(e.packages, ["modules/planning".to_string()]);
}

#[test]
fn an_unexplained_exit_three_is_still_partial() {
    // No recognised error line and no deleted-package declaration is a stderr
    // shape this does not know; calling it complete would under-report.
    assert_eq!(
        cambium_engine::query::partial_of(&outcome("ERROR: something we have never seen\n")),
        Some(cambium_bazel::LoadErrors::default())
    );
}

fn spliced_head(e: &Engine, stderr: &'static str) -> cambium_index::IndexData {
    use cambium_synth::Mutation;

    let before = generate(&GraphSpec {
        packages: 20,
        seed: 7,
        ..Default::default()
    });
    let base = e
        .full_build_with("base", &SynthQuerier::new(&before))
        .unwrap();
    let t = (0..before.targets.len() as u32)
        .find(|&i| !before.targets[i as usize].srcs.is_empty())
        .unwrap();
    let slot = before.targets[t as usize].srcs[0];
    let mut after = before.clone();
    let changes = cambium_synth::apply_one(&mut after, &Mutation::ModifySource { target: t, slot });
    let req = AffectedRequest {
        materialize: true,
        no_full_build: true,
        ..AffectedRequest::new("base", "head")
    };
    e.compute(
        base.view(),
        &changes,
        &StderrQuery(SynthQuerier::new(&after), stderr),
        &req,
    )
    .unwrap()
    .index
    .expect("a spliced head index")
}

#[test]
fn a_splice_over_a_declared_deleted_package_publishes() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.baseline.store = cambium_engine::config::StoreConfig::Dir {
        path: dir.path().join("store"),
    };
    let e = Engine::with_parts(
        cfg,
        cambium_diff::Repo::new(dir.path().to_path_buf()),
        cambium_bazel::Bazel::at(dir.path().to_path_buf(), Some("/nonexistent/bazel")),
    )
    .with_cache_dir(dir.path().join("cache"));

    let mut index = spliced_head(&e, DELETED_PACKAGE_STDERR);
    assert!(
        !index.view().partial,
        "a package Bazel says is deleted leaves nothing missing"
    );
    index.header.config_hash = e.config().hash();
    let path = dir.path().join("head.bin");
    cambium_index::format::write_index(&path, &index).unwrap();
    e.publish_index(&path, &index.header.lineage, "head")
        .expect("nothing is missing, so nothing stops the publish");
}

// ------------------------------------------- answering from a partial index

/// A WORKSPACE-only tree under Bazel 9: the cold `//...` loads a fraction of
/// the packages, and a run spliced onto that index answers about the rest as
/// if they did not exist. The stranger's run said "0 affected targets" where
/// Bazel 7 found 92 tests, with exit 0. As an answer, it is everything,
/// under `CAMBI-W006` naming the packages the index is short of.
#[test]
fn an_answer_from_a_cold_partial_index_is_everything() {
    use cambium_synth::Mutation;

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
    let t = (0..g.targets.len() as u32)
        .find(|&i| !g.targets[i as usize].srcs.is_empty())
        .unwrap();
    let slot = g.targets[t as usize].srcs[0];
    let mut after = g.clone();
    let changes = cambium_synth::apply_one(&mut after, &Mutation::ModifySource { target: t, slot });
    let req = AffectedRequest::new("base", "head");
    let run = |e: &Engine, req: &AffectedRequest| {
        let out = e
            .compute(base.view(), &changes, &SynthQuerier::new(&after), req)
            .unwrap();
        e.trust_partial(req, out)
    };

    let out = run(&e, &req);
    assert!(out.report.global, "the answer is everything");
    let reason = out.report.global_reason.clone().unwrap();
    assert!(
        reason.contains("modules/planning") && reason.contains("third_party/bazel"),
        "{reason}"
    );
    let cause = e
        .diagnostics()
        .drain()
        .into_iter()
        .find(|d| cambium_engine::GlobalReason::explains(d, Some(&reason)))
        .expect("the cause is said");
    assert_eq!(cause.code, cambium_engine::Code::W006PartialQuery);
    assert!(cause.help.unwrap().contains("--allow-partial"));

    // `--allow-partial` answers from it.
    let allowed = AffectedRequest {
        allow_partial: true,
        ..req.clone()
    };
    assert!(!run(&e, &allowed).report.global);

    // Naming one of the two is not enough; naming both is.
    let with_policy = |p: cambium_engine::config::PartialPolicy| {
        let mut cfg = Config::default();
        cfg.baseline.publish_partial = p;
        cambium_testkit::engine_with(cfg)
    };
    let expect = |names: &[&str]| {
        with_policy(cambium_engine::config::PartialPolicy::Expected(
            names.iter().map(|s| s.to_string()).collect(),
        ))
    };
    let e = expect(&["//modules/planning"]);
    let out = run(&e, &req);
    assert!(out.report.global);
    let reason = out.report.global_reason.unwrap();
    assert!(
        reason.contains("third_party/bazel") && !reason.contains("modules/planning"),
        "only the unexpected one is named: {reason}"
    );
    let e = expect(&["//modules/...", "//third_party/bazel"]);
    assert!(!run(&e, &req).report.global);

    // The blanket `true` publishes such an index, and expects nothing.
    let e = with_policy(cambium_engine::config::PartialPolicy::Blanket(true));
    assert!(run(&e, &req).report.global);

    // Nothing changed: nothing is affected, whatever the index lacks.
    let out = e
        .compute(base.view(), &[], &SynthQuerier::new(&g), &req)
        .unwrap();
    assert!(!e.trust_partial(&req, out).report.global);
}
