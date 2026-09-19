//! The walk over a synthetic git history: real git, synthetic Bazel, no
//! live checks. What this proves is the walk's loop -- cold build, tagged
//! steps, the carried index, resume, and the rebuild oracle at the pin --
//! over the same `compute` a real subject drives with a real Bazel. The
//! `rdeps` oracle and the peers need a Bazel and are exercised by the real
//! smoke test; here [`NoLiveChecks`] stands in and the rebuild oracle is the
//! proof.

use cambium_bench::walk::{self, NoLiveChecks, Tier, WalkOptions};
use cambium_bench::walk::{EngineDriver, RowSource};
use cambium_engine::{Config, Engine, PackageQuerier, SynthQuerier};
use cambium_synth::{ChangeSpec, GraphSpec, SynthGraph};
use cambium_testkit::synthetic_engine;

struct SynthDriver {
    root: std::path::PathBuf,
    commits: Vec<String>,
    graphs: Vec<SynthGraph>,
}

impl RowSource for SynthDriver {
    fn querier(&self, rev: &str) -> anyhow::Result<Box<dyn PackageQuerier + '_>> {
        let i = self
            .commits
            .iter()
            .position(|c| c == rev)
            .ok_or_else(|| anyhow::anyhow!("unknown revision {rev}"))?;
        Ok(Box::new(SynthQuerier::new(&self.graphs[i])))
    }

    /// The fixture emits a real git history, so the rebuild oracle's rows
    /// are classified against real trees, as on a subject.
    fn paths(&self, rev: &str) -> anyhow::Result<Option<std::collections::BTreeSet<String>>> {
        walk::tree_paths(&self.root, rev).map(Some)
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    engine: Engine,
    driver: SynthDriver,
    results: std::path::PathBuf,
}

impl Fixture {
    fn engine_driver(&self) -> EngineDriver<'_> {
        EngineDriver {
            engine: &self.engine,
            rows: &self.driver,
        }
    }
}

fn fixture(revisions: u64, preset: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    let graph = cambium_synth::generate(&GraphSpec::preset("tiny").unwrap());
    let change = ChangeSpec {
        seed: 11,
        ..ChangeSpec::preset(preset).unwrap()
    };
    let repo = cambium_synth::emit_git_history(graph, &change, revisions, &root, true).unwrap();
    let mut cfg = Config::load(&root).unwrap();
    cfg.baseline.store = cambium_engine::config::StoreConfig::Dir {
        path: dir.path().join("store"),
    };
    let engine = synthetic_engine(&root, cfg).with_cache_dir(dir.path().join("cache"));
    Fixture {
        results: dir.path().join("results"),
        _dir: dir,
        engine,
        driver: SynthDriver {
            root,
            commits: repo.commits,
            graphs: repo.graphs,
        },
    }
}

fn opts(f: &Fixture) -> WalkOptions {
    let schedule = Tier::Correctness.defaults();
    WalkOptions {
        repo_id: "synth".into(),
        pin: f.driver.commits.last().unwrap().clone(),
        results_dir: f.results.join("walk"),
        state_dir: f.results.clone(),
        bazel_version: "synth".into(),
        schedule,
        // No live checks run, so neither sample matters; empty is honest.
        oracle_sample: walk::Sample::default(),
        peer_sample: walk::Sample::default(),
        environment: cambium_bench::env::Environment {
            machine: cambium_bench::harness::Machine::detect(),
            bazel_version_used: String::new(),
            cambi_version: String::new(),
            tool_sha: String::new(),
            engine_id: String::new(),
            config_hash: String::new(),
            output_bases: Default::default(),
        },
        publish: true,
        resume: false,
        diff_cap: 20,
        progress: None,
        only: false,
    }
}

fn steps(f: &Fixture) -> (Vec<String>, Vec<walk::Step>) {
    let commits = f.driver.commits.clone();
    let steps = walk::tag_steps(f.engine.repo(), &commits, &walk::Probes::default()).unwrap();
    (commits, steps)
}

/// The live checks see cambium's dirty packages, not an empty set.
///
/// The `rdeps` oracle's deleted half credits a deleted file's consumer to its
/// package when the rule is gone at head, and for a wholly-deleted package
/// that is the only spelling left: every rule in it is tombstoned, so no
/// affected label carries the package name. The driver used to hand the live
/// checks an empty dirty set, which made that escape hatch unreachable and
/// failed every change set that deleted a package outright.
#[test]
fn the_live_checks_get_the_steps_dirty_packages() {
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<BTreeSet<String>>>,
    }

    impl walk::LiveChecks for Recorder {
        fn oracle(
            &self,
            ctx: &walk::StepContext<'_>,
        ) -> anyhow::Result<Option<cambium_bench::oracles::rdeps::RdepsResult>> {
            self.seen.lock().unwrap().push(ctx.dirty_packages.clone());
            Ok(None)
        }
        fn peers(
            &self,
            _: &walk::StepContext<'_>,
            _: Option<&cambium_bench::oracles::rdeps::RdepsResult>,
        ) -> Vec<cambium_bench::walk::model::ToolStep> {
            Vec::new()
        }
    }

    let f = fixture(6, "typical-pr");
    let (commits, s) = steps(&f);
    let rec = Recorder::default();
    let r = walk::walk(&f.engine, &f.engine_driver(), &rec, &commits, &s, &opts(&f)).unwrap();
    assert!(r.done);
    let seen = rec.seen.lock().unwrap();
    assert_eq!(
        seen.len(),
        r.steps.len(),
        "the oracle hook ran on every step"
    );
    // The record's count and the set the live checks saw are the same fact.
    for (st, got) in r.steps.iter().zip(seen.iter()) {
        assert_eq!(st.dirty_packages, got.len(), "{}..{}", st.parent, st.head);
    }
    assert!(
        seen.iter().any(|d| !d.is_empty()),
        "no step passed a dirty package through"
    );
}

#[test]
fn a_walk_runs_and_the_rebuild_oracle_holds() {
    let f = fixture(8, "typical-pr");
    let (commits, s) = steps(&f);
    let r = walk::walk(
        &f.engine,
        &f.engine_driver(),
        &NoLiveChecks,
        &commits,
        &s,
        &opts(&f),
    )
    .unwrap();

    assert!(r.done);
    assert_eq!(r.steps.len(), commits.len() - 1);
    assert_eq!(r.tier, Tier::Correctness);
    let ro = r.rebuild_oracle.as_ref().expect("the rebuild oracle ran");
    assert!(
        ro.ok,
        "applied index differs from a rebuild: {}",
        ro.summary
    );
    assert!(r.head_index.is_some(), "the pin's index was archived");
    assert!(r.summary.pass);

    // The loading-layer rows went to the sidecar beside the walk record,
    // classified against the fixture's own git trees.
    let sidecar = walk::RebuildRows::path(&f.results.join("walk"), &opts(&f).pin, Some("synth"));
    assert!(sidecar.exists(), "no sidecar at {}", sidecar.display());
    assert_eq!(
        ro.rows.as_deref(),
        sidecar.file_name().and_then(|n| n.to_str()),
        "the record names the sidecar"
    );
    let rows = walk::RebuildRows::read(&sidecar).unwrap();
    assert_eq!(rows.at, opts(&f).pin);
    assert_eq!(rows.from, commits[0], "no cold rebuild inside the window");
    assert_eq!(rows.counts, ro.sky);
    assert_eq!(ro.sky.missing_unexplained, 0, "{:?}", rows.missing);
    assert!(!rows.truncated);
    for row in &rows.extra {
        assert_eq!(
            row.class,
            walk::RowClass::DeletedInWindow,
            "an extra row a refresh could not remove: {row:?}"
        );
    }
    // Every step scored cambium as a tool row, even with no oracle.
    assert!(r
        .steps
        .iter()
        .all(|st| st.tools.first().map(|t| t.name.as_str()) == Some("cambium")));
    assert!(r.steps.iter().any(|st| st.dirty_packages > 0));
    // The first step's parent is the cold-build commit; each step carries on.
    assert_eq!(r.steps[0].parent, commits[0]);
    assert!(r.steps.iter().all(|st| st.parent != st.head));

    // Every step's index was published, so a resume could find it.
    for c in &commits {
        assert!(
            f.engine.fetch_baseline(c).unwrap().is_some(),
            "no index for {c}"
        );
    }
    let on_disk = walk::WalkResult::read(&walk::WalkResult::path(
        &f.results.join("walk"),
        &opts(&f).pin,
        Some("synth"),
    ))
    .unwrap();
    assert_eq!(on_disk.steps.len(), r.steps.len());
    assert!(on_disk.done);
}

#[test]
fn a_walk_resumes_from_its_last_published_index() {
    let f = fixture(6, "typical-pr");
    let (commits, s) = steps(&f);
    let full = walk::walk(
        &f.engine,
        &f.engine_driver(),
        &NoLiveChecks,
        &commits,
        &s,
        &opts(&f),
    )
    .unwrap();
    assert!(full.done);

    // Truncate to three steps and mark it unfinished, then resume.
    let path = walk::WalkResult::path(&f.results.join("walk"), &opts(&f).pin, Some("synth"));
    let mut truncated = walk::WalkResult::read(&path).unwrap();
    truncated.steps.truncate(3);
    truncated.done = false;
    truncated.rebuild_oracle = None;
    truncated.write(&path).unwrap();
    let sidecar = walk::RebuildRows::path(&f.results.join("walk"), &opts(&f).pin, Some("synth"));
    std::fs::remove_file(&sidecar).unwrap();

    let mut o = opts(&f);
    o.resume = true;
    let resumed = walk::walk(
        &f.engine,
        &f.engine_driver(),
        &NoLiveChecks,
        &commits,
        &s,
        &o,
    )
    .unwrap();
    assert_eq!(resumed.steps.len(), commits.len() - 1);
    assert_eq!(resumed.steps[2].head, truncated.steps[2].head);
    let ro = resumed.rebuild_oracle.unwrap();
    assert!(ro.ok, "a resumed walk still equals a rebuild: {}", ro.summary);
    assert!(sidecar.exists(), "the resumed walk rewrote the sidecar");
    assert_eq!(walk::RebuildRows::read(&sidecar).unwrap().counts, ro.sky);
}

#[test]
fn schema_seven_refuses_a_schema_six_walk_file() {
    let f = fixture(4, "typical-pr");
    let (commits, s) = steps(&f);
    walk::walk(
        &f.engine,
        &f.engine_driver(),
        &NoLiveChecks,
        &commits,
        &s,
        &opts(&f),
    )
    .unwrap();
    let path = walk::WalkResult::path(&f.results.join("walk"), &opts(&f).pin, Some("synth"));
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    doc["schema"] = serde_json::json!(6);
    std::fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();
    let err = walk::WalkResult::read(&path).unwrap_err().to_string();
    assert!(err.contains("schema") || err.contains("version"), "{err}");
}
