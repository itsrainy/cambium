//! The walk over a synthetic git history: real git, synthetic Bazel, no
//! live checks. What this proves is the walk's loop -- cold build, tagged
//! steps, the carried index, resume, and the rebuild oracle at the pin --
//! over the same `compute` a real subject drives with a real Bazel. The
//! `rdeps` oracle and the peers need a Bazel and are exercised by the real
//! smoke test; here [`NoLiveChecks`] stands in and the rebuild oracle is the
//! proof.

use std::cell::Cell;

use cambium_bench::walk::{self, NoLiveChecks, Tier, WalkOptions};
use cambium_bench::walk::{EngineDriver, RowSource};
use cambium_engine::{Config, Engine, PackageQuerier, SynthQuerier};
use cambium_synth::{ChangeSpec, GraphSpec, SynthGraph};
use cambium_testkit::synthetic_engine;

struct SynthDriver {
    root: std::path::PathBuf,
    commits: Vec<String>,
    graphs: Vec<SynthGraph>,
    /// A commit whose tree holds loose paths (files no target names) that a
    /// cold `//...` walk lists and no refresh dump ever mentions.
    loose: Option<(String, Vec<String>)>,
    /// A commit whose querier has no loading layer to dump at all.
    layerless: Option<String>,
    /// A commit at which every package a dirty-package query names fails to
    /// load, as xla's did at an LLVM integrate whose patches no longer
    /// applied.
    failing: Option<String>,
}

impl RowSource for SynthDriver {
    fn querier(&self, rev: &str) -> anyhow::Result<Box<dyn PackageQuerier + '_>> {
        let i = self
            .commits
            .iter()
            .position(|c| c == rev)
            .ok_or_else(|| anyhow::anyhow!("unknown revision {rev}"))?;
        if self.layerless.as_deref() == Some(rev) {
            return Ok(Box::new(Layerless(SynthQuerier::new(&self.graphs[i]))));
        }
        if self.failing.as_deref() == Some(rev) {
            return Ok(Box::new(Failing(SynthQuerier::new(&self.graphs[i]))));
        }
        match &self.loose {
            Some((at, paths)) if at == rev => {
                Ok(Box::new(HoleQuerier::new(&self.graphs[i], paths.clone())))
            }
            _ => Ok(Box::new(SynthQuerier::new(&self.graphs[i]))),
        }
    }

    /// The fixture emits a real git history, so the rebuild oracle's rows
    /// are classified against real trees, as on a subject.
    fn paths(&self, rev: &str) -> anyhow::Result<Option<std::collections::BTreeSet<String>>> {
        walk::tree_paths(&self.root, rev).map(Some)
    }
}

/// A querier whose cold dump lists the loose paths and whose refresh dumps
/// never do.
///
/// That is what a real refresh does for a directory no re-queried package's
/// glob descends into (a cold `//...` walk lists every directory, a refresh
/// only what its packages reached), and it is a hole when the loose path is
/// a file inside a package the refresh did re-query. Which of the two it
/// is, the walk's rebuild oracle has to tell.
struct HoleQuerier<'a> {
    cold: SynthQuerier<'a>,
    warm: SynthQuerier<'a>,
    /// A querier is used for one operation; it is a refresh unless the
    /// universe was asked for.
    refreshing: Cell<bool>,
}

impl<'a> HoleQuerier<'a> {
    fn new(graph: &'a SynthGraph, loose: Vec<String>) -> Self {
        HoleQuerier {
            cold: SynthQuerier::with_loose_paths(graph, loose),
            warm: SynthQuerier::new(graph),
            refreshing: Cell::new(true),
        }
    }
}

impl PackageQuerier for HoleQuerier<'_> {
    fn query(
        &self,
        packages: &[String],
    ) -> Result<cambium_engine::query::Rows, cambium_engine::EngineError> {
        self.refreshing.set(true);
        self.warm.query(packages)
    }

    fn universe(&self) -> Result<cambium_engine::query::Rows, cambium_engine::EngineError> {
        self.refreshing.set(false);
        self.cold.universe()
    }

    fn version(&self) -> String {
        self.cold.version()
    }

    fn load_source(&self) -> Option<&dyn cambium_index::FileText> {
        self.cold.load_source()
    }

    fn starlark_files(&self) -> Option<Vec<String>> {
        self.cold.starlark_files()
    }

    fn can_dump_loading_layer(&self) -> bool {
        true
    }

    fn dump_loading_layer(
        &self,
        filter: &str,
    ) -> Result<(String, cambium_index::sky::Paths), cambium_engine::EngineError> {
        if self.refreshing.get() {
            self.warm.dump_loading_layer(filter)
        } else {
            self.cold.dump_loading_layer(filter)
        }
    }
}

/// A synthetic querier with no Bazel server to dump a loading layer from.
struct Layerless<'a>(SynthQuerier<'a>);

impl PackageQuerier for Layerless<'_> {
    fn query(
        &self,
        packages: &[String],
    ) -> Result<cambium_engine::query::Rows, cambium_engine::EngineError> {
        self.0.query(packages)
    }

    fn universe(&self) -> Result<cambium_engine::query::Rows, cambium_engine::EngineError> {
        self.0.universe()
    }

    fn version(&self) -> String {
        self.0.version()
    }

    fn load_source(&self) -> Option<&dyn cambium_index::FileText> {
        self.0.load_source()
    }

    fn starlark_files(&self) -> Option<Vec<String>> {
        self.0.starlark_files()
    }

    fn can_dump_loading_layer(&self) -> bool {
        false
    }
}

/// A synthetic querier whose dirty-package queries fail to load every
/// package they name: the rows come back unloaded and the answer partial,
/// the way `BazelQuerier` answers a `Skipping` line.
struct Failing<'a>(SynthQuerier<'a>);

impl PackageQuerier for Failing<'_> {
    fn query(
        &self,
        packages: &[String],
    ) -> Result<cambium_engine::query::Rows, cambium_engine::EngineError> {
        let rows = self.0.query(packages)?;
        let stderr: String = rows
            .updates
            .iter()
            .map(|u| {
                format!(
                    "ERROR: Skipping '//{p}:*': error loading package '{p}': boom\n",
                    p = u.package
                )
            })
            .collect();
        Ok(cambium_engine::query::Rows {
            updates: Vec::new(),
            partial: Some(cambium_bazel::parse_load_errors(&stderr)),
            unloaded: rows
                .updates
                .into_iter()
                .map(|mut u| {
                    u.targets.clear();
                    u
                })
                .collect(),
        })
    }

    fn universe(&self) -> Result<cambium_engine::query::Rows, cambium_engine::EngineError> {
        self.0.universe()
    }

    fn version(&self) -> String {
        self.0.version()
    }

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
    ) -> Result<(String, cambium_index::sky::Paths), cambium_engine::EngineError> {
        self.0.dump_loading_layer(filter)
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
            loose: None,
            layerless: None,
            failing: None,
        },
    }
}

/// Appends a commit that adds `paths` (empty files no target names) to the
/// fixture's history, with the graph unchanged, and makes it the commit
/// whose refresh dumps never mention them. Returns its sha.
fn append_loose_commit(f: &mut Fixture, paths: &[&str]) -> String {
    let root = &f.driver.root;
    for p in paths {
        let file = root.join(p);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "notes\n").unwrap();
    }
    let sha = cambium_testkit::commit_all(root, "loose files");
    let graph = f.driver.graphs.last().unwrap().clone();
    f.driver.graphs.push(graph);
    f.driver.commits.push(sha.clone());
    f.driver.loose = Some((sha.clone(), paths.iter().map(|p| p.to_string()).collect()));
    sha
}

/// A package alive at the window start and at the pin.
fn a_surviving_package(f: &Fixture) -> String {
    let first = &f.driver.graphs[0];
    let last = f.driver.graphs.last().unwrap();
    first
        .packages
        .iter()
        .zip(&last.packages)
        .find(|(a, b)| a.alive && b.alive && a.path == b.path)
        .map(|(a, _)| a.path.clone())
        .expect("a package that survives the window")
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
            ..Default::default()
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
            ctx: &walk::AnsweredStep<'_>,
        ) -> anyhow::Result<Option<cambium_bench::oracles::rdeps::RdepsResult>> {
            self.seen.lock().unwrap().push(ctx.dirty_packages.clone());
            Ok(None)
        }
        fn peers(
            &self,
            _: &walk::AnsweredStep<'_>,
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

/// What a walk asked its driver, in order.
struct Recording<'a> {
    inner: EngineDriver<'a>,
    events: std::sync::Mutex<Vec<String>>,
}

impl walk::Driver for Recording<'_> {
    fn checkout(&self, rev: &str) -> anyhow::Result<()> {
        self.inner.checkout(rev)
    }
    fn build(
        &self,
        rev: &str,
        out: &std::path::Path,
        publish: bool,
    ) -> anyhow::Result<std::time::Duration> {
        self.events
            .lock()
            .unwrap()
            .push(format!("build {rev} publish={publish}"));
        self.inner.build(rev, out, publish)
    }
    fn update(
        &self,
        base: &str,
        head: &str,
        out: &std::path::Path,
        publish: bool,
    ) -> anyhow::Result<walk::Update> {
        self.events.lock().unwrap().push(format!("update {head}"));
        self.inner.update(base, head, out, publish)
    }
    fn paths(&self, rev: &str) -> anyhow::Result<Option<std::collections::BTreeSet<String>>> {
        self.inner.paths(rev)
    }
    fn restart_server(&self) -> anyhow::Result<()> {
        self.events.lock().unwrap().push("restart".to_string());
        Ok(())
    }
}

/// The rebuild oracle's reference build runs on a fresh server, at the pin
/// and mid-walk, and the walk's own builds keep the server they have.
///
/// The reference used to be built on the server that answered every step
/// and every oracle query of the walk, and its dump carried all of it: rows
/// no build at the pin writes, read as the applied layer's shortfall.
#[test]
fn every_reference_build_and_only_those_starts_on_a_fresh_server() {
    let f = fixture(6, "typical-pr");
    let (commits, s) = steps(&f);
    let driver = Recording {
        inner: f.engine_driver(),
        events: Default::default(),
    };
    let mut o = opts(&f);
    o.schedule.rebuild_every = Some(2);
    let r = walk::walk(&f.engine, &driver, &NoLiveChecks, &commits, &s, &o).unwrap();
    assert!(r.rebuild_oracle.as_ref().is_some_and(|ro| ro.ok));

    let events = driver.events.into_inner().unwrap();
    let restarts: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| *e == "restart")
        .map(|(i, _)| i)
        .collect();
    let mid_walk: Vec<&str> = r
        .steps
        .iter()
        .filter(|st| st.rebuild_step)
        .map(|st| st.head.as_str())
        .collect();
    assert!(
        !mid_walk.is_empty(),
        "the schedule asked for mid-walk rebuilds"
    );
    assert_eq!(restarts.len(), mid_walk.len() + 1, "{events:#?}");
    // Each restart is followed by the reference build it was for, which is
    // never published.
    let mut expected: Vec<String> = mid_walk
        .iter()
        .map(|h| format!("build {h} publish=false"))
        .collect();
    expected.push(format!("build {} publish=false", o.pin));
    let got: Vec<String> = restarts.iter().map(|i| events[i + 1].clone()).collect();
    assert_eq!(got, expected, "{events:#?}");
    // The walk's own cold build at the window start is not preceded by one.
    assert_eq!(events[0], format!("build {} publish=true", commits[0]));
    // The pin's reference build is the last thing the walk asked for.
    assert_eq!(events.last(), expected.last());
}

/// A directory created inside the window that no re-queried package's glob
/// descends into has its rows on the cold side only. They reach the sidecar
/// as `created_in_window`, and the walk passes with them: the whole path
/// from the diff through the classifier to `RebuildOracle.ok`, not the
/// classifier alone.
#[test]
fn a_directory_no_refresh_had_reason_to_dump_is_expected_and_the_walk_passes() {
    let mut f = fixture(4, "typical-pr");
    let pin = append_loose_commit(&mut f, &["loose/notes.txt"]);
    let (commits, s) = steps(&f);
    let o = opts(&f);
    assert_eq!(o.pin, pin);
    let r = walk::walk(
        &f.engine,
        &f.engine_driver(),
        &NoLiveChecks,
        &commits,
        &s,
        &o,
    )
    .unwrap();
    let ro = r.rebuild_oracle.as_ref().expect("the rebuild oracle ran");
    assert!(ro.ok, "{}", ro.summary);
    assert!(r.summary.pass);
    assert!(
        ro.sky.missing > 0,
        "the cold layer listed the loose directory: {:?}",
        ro.sky
    );
    assert_eq!(ro.sky.missing_expected, ro.sky.missing, "{:?}", ro.sky);
    assert_eq!(ro.sky.missing_unexplained, 0);
    let sidecar = walk::RebuildRows::path(&f.results.join("walk"), &pin, Some("synth"));
    let rows = walk::RebuildRows::read(&sidecar).unwrap();
    assert_eq!(rows.counts, ro.sky);
    for row in &rows.missing {
        assert_eq!(row.class, walk::RowClass::CreatedInWindow, "{row:?}");
        assert!(
            row.path
                .as_deref()
                .is_some_and(|p| p == "loose" || p.starts_with("loose/")),
            "{row:?}"
        );
    }
    // The report says how many rows were excused, on a held oracle too.
    let md = cambium_bench::walk::report::render(&r);
    assert!(
        md.contains(&format!(
            "Rebuild oracle at the pin: held. Loading layer: {} missing rows ({} expected",
            ro.sky.missing, ro.sky.missing
        )),
        "{md}"
    );
}

/// A file added inside the window to a package directory the applied layer
/// knows, which the refresh of that package never dumped, is a hole: the
/// row is unexplained, the oracle does not hold, and the walk fails.
#[test]
fn a_hole_under_a_package_the_refresh_covered_fails_the_walk() {
    let mut f = fixture(4, "typical-pr");
    let pkg = a_surviving_package(&f);
    let readme = format!("{pkg}/readme.txt");
    let pin = append_loose_commit(&mut f, &[&readme]);
    let (commits, s) = steps(&f);
    let o = opts(&f);
    let r = walk::walk(
        &f.engine,
        &f.engine_driver(),
        &NoLiveChecks,
        &commits,
        &s,
        &o,
    )
    .unwrap();
    let ro = r.rebuild_oracle.as_ref().expect("the rebuild oracle ran");
    assert!(!ro.ok, "{:?}", ro.sky);
    assert!(!r.summary.pass);
    assert!(ro.sky.missing_unexplained > 0, "{:?}", ro.sky);
    assert!(
        ro.summary.contains(&format!(
            "loading layer unexplained: {}",
            ro.sky.missing_unexplained
        )),
        "{}",
        ro.summary
    );
    let sidecar = walk::RebuildRows::path(&f.results.join("walk"), &pin, Some("synth"));
    let rows = walk::RebuildRows::read(&sidecar).unwrap();
    let unexplained: Vec<&walk::Row> = rows
        .missing
        .iter()
        .filter(|r| r.class == walk::RowClass::Unexplained)
        .collect();
    assert_eq!(unexplained.len(), ro.sky.missing_unexplained);
    // The file's own rows, and its edge to the package directory (located
    // by the directory, its `to` end); all placed in the package.
    assert!(
        unexplained.iter().all(|r| {
            let p = r.path.as_deref();
            (p == Some(readme.as_str()) || p == Some(pkg.as_str()))
                && r.package.as_deref() == Some(pkg.as_str())
        }),
        "{unexplained:?}"
    );
    assert!(
        unexplained
            .iter()
            .any(|r| r.path.as_deref() == Some(readme.as_str())),
        "{unexplained:?}"
    );
    // Sorted unexplained-first, so the first row is one that fails the walk.
    assert_eq!(rows.missing[0].class, walk::RowClass::Unexplained);
}

/// An applied index with a layer against a cold build with none is not a
/// layer with zero unexplained rows: the oracle does not hold, whatever the
/// classifier counted.
#[test]
fn a_cold_build_with_no_layer_fails_the_oracle_whatever_the_rows_say() {
    let mut f = fixture(3, "typical-pr");
    f.driver.layerless = f.driver.commits.last().cloned();
    let (commits, s) = steps(&f);
    let o = opts(&f);
    let r = walk::walk(
        &f.engine,
        &f.engine_driver(),
        &NoLiveChecks,
        &commits,
        &s,
        &o,
    )
    .unwrap();
    let ro = r.rebuild_oracle.as_ref().expect("the rebuild oracle ran");
    assert!(!ro.ok, "{:?}", ro.sky);
    assert!(!r.summary.pass);
    assert_eq!(
        ro.sky.missing_unexplained, 0,
        "no rows to classify: {:?}",
        ro.sky
    );
    assert!(
        ro.summary.contains("loading layer rows missing: 1"),
        "{}",
        ro.summary
    );
}

/// `--only` answers the named steps and nothing else, and answers them the
/// same way the full walk does.
///
/// The two steps here do not neighbour each other, which is the whole point:
/// a gate names the steps a change is suspected on, and those are scattered
/// through the history. Each one is cold-built at its own parent instead of
/// carrying an index through everything between, so the comparison against
/// the full walk's own records is the proof that the shortcut asks the same
/// question.
#[test]
fn only_answers_the_named_steps_the_way_the_full_walk_does() {
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
    assert!(
        s.len() >= 4,
        "the fixture needs steps between the named two"
    );

    let named = vec![s[1].clone(), s[3].clone()];
    let mut o = opts(&f);
    o.only = true;
    let only = walk::walk_only(&f.engine, &f.engine_driver(), &NoLiveChecks, &named, &o).unwrap();

    assert!(only.done);
    assert_eq!(only.steps.len(), 2, "the named steps and no others");
    assert_eq!(only.from, named[0].parent);
    assert_eq!(only.to, named[1].head);
    for (rec, step) in only.steps.iter().zip(&named) {
        assert_eq!(rec.parent, step.parent);
        assert_eq!(rec.head, step.head);
        let same = full
            .steps
            .iter()
            .find(|r| r.head == step.head)
            .expect("the full walk answered this step too");
        assert_eq!(
            rec.cambium.affected, same.cambium.affected,
            "step {}..{} answered differently when it was named alone",
            step.parent, step.head
        );
        assert_eq!(rec.global, same.global);
        assert_eq!(rec.dirty_packages, same.dirty_packages);
    }

    // The record is filed beside the full walk's, not over it.
    let mine = walk::WalkResult::path(
        &f.results.join("walk"),
        &format!("{}-only", o.pin),
        Some("synth"),
    );
    assert!(mine.exists(), "no record at {}", mine.display());
    let theirs = walk::WalkResult::path(&f.results.join("walk"), &o.pin, Some("synth"));
    assert!(theirs.exists(), "the full walk's record was overwritten");
    assert_eq!(
        walk::WalkResult::read(&theirs).unwrap().steps.len(),
        s.len()
    );
}

/// `--only` with `--resume` continues the record it left, rather than
/// cold-building the first named step again over it. A gate run is hours of
/// cold builds and the CLI tells the operator to pass `--resume` after one
/// dies.
#[test]
fn only_resumes_the_steps_it_has_not_answered() {
    let f = fixture(6, "typical-pr");
    let (_, s) = steps(&f);
    assert!(s.len() >= 4);
    let named = vec![s[1].clone(), s[3].clone()];
    let mut o = opts(&f);
    o.only = true;
    walk::walk_only(&f.engine, &f.engine_driver(), &NoLiveChecks, &named, &o).unwrap();

    // The shape a died run leaves: the first step answered, the second not.
    let path = walk::WalkResult::path(
        &f.results.join("walk"),
        &format!("{}-only", o.pin),
        Some("synth"),
    );
    let mut partial = walk::WalkResult::read(&path).unwrap();
    partial.steps.truncate(1);
    partial.done = false;
    // A mark nothing but a re-run of that step can erase.
    partial.steps[0].dirty_packages = 4242;
    partial.write(&path).unwrap();

    o.resume = true;
    let resumed =
        walk::walk_only(&f.engine, &f.engine_driver(), &NoLiveChecks, &named, &o).unwrap();
    assert!(resumed.done);
    assert_eq!(resumed.steps.len(), 2);
    assert_eq!(
        resumed.steps[0].dirty_packages, 4242,
        "the finished step was answered again instead of resumed"
    );
    assert_eq!(resumed.steps[1].head, named[1].head);
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
    assert!(
        ro.ok,
        "a resumed walk still equals a rebuild: {}",
        ro.summary
    );
    assert!(sidecar.exists(), "the resumed walk rewrote the sidecar");
    assert_eq!(walk::RebuildRows::read(&sidecar).unwrap().counts, ro.sky);
}

/// A step whose head cannot load the packages it dirtied leaves a partial
/// index. The walk publishes it to its own store, records the partial state
/// on the step, counts it in the summary, and walks on: the default publish
/// policy used to stop the walk here with CAMBI-E009.
#[test]
fn a_step_whose_index_carries_unloaded_packages_is_walked_and_published() {
    // Which step re-queries something, from a walk over the same history.
    let probe = fixture(6, "typical-pr");
    let (commits, s) = steps(&probe);
    let full = walk::walk(
        &probe.engine,
        &probe.engine_driver(),
        &NoLiveChecks,
        &commits,
        &s,
        &opts(&probe),
    )
    .unwrap();
    let at = full
        .steps
        .iter()
        .position(|st| st.dirty_packages > 0 && !st.global)
        .expect("a step that re-queried packages");

    // The same history, fresh store, with that step's head broken.
    let mut f = fixture(6, "typical-pr");
    let (commits, s) = steps(&f);
    let broken = s[at].head.clone();
    f.driver.failing = Some(broken.clone());
    let r = walk::walk(
        &f.engine,
        &f.engine_driver(),
        &NoLiveChecks,
        &commits,
        &s,
        &opts(&f),
    )
    .expect("a partial step index does not stop the walk");
    assert!(r.done);
    assert_eq!(r.steps.len(), commits.len() - 1);
    let st = &r.steps[at];
    assert_eq!(st.head, broken);
    assert!(st.index.partial.partial, "{:?}", st.index);
    assert!(!st.index.partial.packages.is_empty());
    assert!(r.summary.partial_index_steps >= 1);
    assert!(!r.summary.partial_index_packages.is_empty());
    assert!(
        f.engine.fetch_baseline(&broken).unwrap().is_some(),
        "the partial index was published to the walk's store"
    );
    let text = walk::report::render(&r);
    assert!(
        text.contains("Steps whose index carried packages that failed to load"),
        "{text}"
    );
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
    // The walk record is read through the crate's one schema gate, which
    // names both versions.
    let err = walk::WalkResult::read(&path).unwrap_err().to_string();
    assert!(
        err.contains(&format!(
            "is schema 6 and this cambium reads {}",
            cambium_bench::RESULT_SCHEMA
        )),
        "{err}"
    );
}
