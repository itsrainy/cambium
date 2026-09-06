//! The replay over a synthetic git history: real git, synthetic Bazel.
//!
//! What this proves is the harness — cold build, forward walk, publish,
//! resume, the incremental-equals-full oracle, pair selection — over the same
//! `compute` a corpus run drives with a real Bazel.

use std::path::Path;

use cambium_corpus::pairs::{self, PairKind, PairOptions};
use cambium_corpus::replay::{replay, Driver, ReplayOptions, ReplayResult};
use cambium_corpus::State;
use cambium_engine::{Config, Engine, PackageQuerier, SynthQuerier};
use cambium_synth::{ChangeSpec, GraphSpec, SynthGraph};

struct SynthDriver {
    commits: Vec<String>,
    graphs: Vec<SynthGraph>,
}

impl Driver for SynthDriver {
    fn checkout(&self, _rev: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn querier(&self, rev: &str) -> anyhow::Result<Box<dyn PackageQuerier + '_>> {
        let i = self
            .commits
            .iter()
            .position(|c| c == rev)
            .ok_or_else(|| anyhow::anyhow!("unknown revision {rev}"))?;
        Ok(Box::new(SynthQuerier::new(&self.graphs[i])))
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    engine: Engine,
    driver: SynthDriver,
    results: std::path::PathBuf,
}

fn fixture(revisions: u64, change_preset: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    let graph = cambium_synth::generate(&GraphSpec::preset("tiny").unwrap());
    let change = ChangeSpec {
        seed: 11,
        ..ChangeSpec::preset(change_preset).unwrap()
    };
    let repo = cambium_synth::emit_git_history(graph, &change, revisions, &root, true).unwrap();
    let mut cfg = Config::load(&root).unwrap();
    cfg.baseline.store = cambium_engine::config::StoreConfig::Dir {
        path: dir.path().join("store"),
    };
    let engine = Engine::with_parts(
        cfg,
        cambium_diff::Repo::new(root.clone()),
        cambium_bazel::Bazel::at(root.clone(), Some("/nonexistent/bazel")),
    )
    .with_cache_dir(dir.path().join("cache"));
    Fixture {
        results: dir.path().join("results"),
        _dir: dir,
        root,
        engine,
        driver: SynthDriver {
            commits: repo.commits,
            graphs: repo.graphs,
        },
    }
}

fn opts(f: &Fixture, resume: bool) -> ReplayOptions {
    ReplayOptions {
        repo_id: "synth".into(),
        pin: f.driver.commits.last().unwrap().clone(),
        results_dir: f.results.clone(),
        publish: true,
        resume,
        bazel_version: "synth".into(),
        diff_cap: 20,
    }
}

#[test]
fn replays_a_history_and_the_oracle_holds() {
    let f = fixture(8, "typical-pr");
    let commits = f.driver.commits.clone();
    let r = replay(&f.engine, &f.driver, &commits, &opts(&f, false)).unwrap();

    assert!(r.done);
    assert_eq!(r.commits.len(), commits.len() - 1);
    assert_eq!(r.cold.commit, commits[0]);
    assert!(r.cold.nodes > 0 && r.cold.index_bytes > 0);
    assert!(r.cold.stats["counts"]["rules"].as_u64().unwrap() > 0);
    let oracle = r.oracle.as_ref().expect("oracle ran");
    assert!(
        oracle.ok,
        "replayed index differs from a rebuild: {} {:?}",
        oracle.diff.summary(),
        oracle.diff
    );
    assert_eq!(r.rebuild.as_ref().unwrap().commit, *commits.last().unwrap());
    assert_eq!(r.summary.commits, r.commits.len());
    assert!(r.commits.iter().all(|c| c.parent != c.commit));
    assert!(r.commits.iter().any(|c| c.dirty_packages > 0));

    // Every commit's index was published, keyed by tree.
    for c in &commits {
        assert!(
            f.engine.fetch_baseline(c).unwrap().is_some(),
            "no published index for {c}"
        );
    }
    // The results file is the same document the function returned.
    let on_disk =
        ReplayResult::read(&ReplayResult::path(&f.results, &opts(&f, false).pin)).unwrap();
    assert_eq!(on_disk.commits.len(), r.commits.len());
    assert!(on_disk.done);
    let st = State::load(&f.results, "synth", &opts(&f, false).pin).unwrap();
    assert!(st.replay.as_ref().unwrap().done);
}

#[test]
fn a_replay_resumes_from_its_last_published_index() {
    let f = fixture(6, "typical-pr");
    let commits = f.driver.commits.clone();
    // Run the first half only, as a killed run would have.
    let half = &commits[..4];
    let o = opts(&f, false);
    let partial = replay(&f.engine, &f.driver, half, &o).unwrap();
    assert!(partial.done);

    // Now a full-range run resumes: its results file says the range ended at
    // `half.last()`, which differs, so it must start over — that is the
    // conservative reading, and it must still be correct.
    let full = replay(&f.engine, &f.driver, &commits, &opts(&f, true)).unwrap();
    assert_eq!(full.commits.len(), commits.len() - 1);
    assert!(full.oracle.unwrap().ok);

    // A genuinely interrupted run: truncate the results file to three commits
    // and mark it unfinished, then resume. The remaining commits are replayed
    // from the published index of the third, not from the cold build.
    let path = ReplayResult::path(&f.results, &o.pin);
    let mut truncated = ReplayResult::read(&path).unwrap();
    truncated.commits.truncate(3);
    truncated.done = false;
    truncated.oracle = None;
    truncated.write(&path).unwrap();
    let resumed = replay(&f.engine, &f.driver, &commits, &opts(&f, true)).unwrap();
    assert_eq!(resumed.commits.len(), commits.len() - 1);
    assert_eq!(resumed.commits[2].commit, truncated.commits[2].commit);
    assert!(
        resumed.oracle.unwrap().ok,
        "resumed replay must still equal a rebuild"
    );
    assert_eq!(
        resumed.cold.commit, commits[0],
        "the cold build is kept from the first run"
    );
}

#[test]
fn pairs_are_deterministic_and_cover_the_range() {
    let f = fixture(10, "mixed");
    let commits = f.driver.commits.clone();
    let pin = commits.last().unwrap().clone();
    let o = PairOptions {
        repo_id: "synth".into(),
        pin: pin.clone(),
        replay_range: commits.len() - 1,
        seed: 3,
        cap: 2,
        hub_packages: vec![f.driver.graphs[0].package_path(0).to_string()],
        invalidator_paths: vec!["MODULE.bazel".into()],
        invalidator_globs: vec!["rules/**".into()],
    };
    let adjacent = pairs::adjacent(&f.engine.repo, &pin, o.replay_range).unwrap();
    assert_eq!(adjacent.len(), commits.len() - 1);
    assert_eq!(adjacent[0].base, commits[0]);
    assert_eq!(adjacent.last().unwrap().head, pin);

    // A linear history has no merges.
    assert!(pairs::pr_shaped(&f.engine.repo, &pin, o.replay_range)
        .unwrap()
        .is_empty());

    let stress = pairs::stress(&f.engine.repo, &o).unwrap();
    let again = pairs::stress(&f.engine.repo, &o).unwrap();
    assert_eq!(stress, again, "stress selection is deterministic");
    for p in &stress {
        assert!(p.category.is_some());
        assert_eq!(f.engine.repo.parents(&p.head).unwrap()[0], p.base);
    }
    let per_category = |name: &str| {
        stress
            .iter()
            .filter(|p| p.category.as_deref() == Some(name))
            .count()
    };
    assert!(per_category("bzl") <= 2 && per_category("build_add_delete") <= 2);
    // `mixed` edits .bzl files and adds packages, so both categories fire.
    assert!(per_category("bzl") > 0, "{stress:?}");

    let set = pairs::select(&f.engine.repo, &[PairKind::Adjacent, PairKind::Stress], &o).unwrap();
    assert_eq!(set.pairs.len(), adjacent.len() + stress.len());
    let path = f.results.join("pairs.json");
    set.write(&path).unwrap();
    assert_eq!(
        cambium_corpus::PairSet::read(&path).unwrap().pairs,
        set.pairs
    );
    let _ = Path::new(&f.root);
}

/// A merge commit in the history makes a PR-shaped pair whose base is the
/// merge base of its parents.
#[test]
fn pr_pairs_come_from_merge_commits() {
    let f = fixture(3, "typical-pr");
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&f.root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let main_tip = git(&["rev-parse", "HEAD"]);
    let base = git(&["rev-parse", "HEAD~2"]);
    git(&["checkout", "-q", "-b", "pr", &base]);
    std::fs::write(f.root.join("README.md"), "pr work\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "pr"]);
    git(&["checkout", "-q", "main"]);
    git(&["merge", "-q", "--no-ff", "-m", "merge pr", "pr"]);
    let merge = git(&["rev-parse", "HEAD"]);

    let prs = pairs::pr_shaped(&f.engine.repo, &merge, 10).unwrap();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].head, merge);
    assert_eq!(
        prs[0].base, base,
        "merge-base of {main_tip} and the pr branch"
    );
    assert_eq!(prs[0].kind, PairKind::Pr);
}
