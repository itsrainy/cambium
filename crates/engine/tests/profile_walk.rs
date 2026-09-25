//! `cambi profile`'s walk over a synthetic git history: a cold build at the
//! start of the range, one step per commit, the head index on disk, and a
//! summary whose counts agree with the steps. Bazel is stubbed out; the rows
//! come from the synthetic graphs the emitter kept per commit.

use std::collections::{HashMap, HashSet};

use cambium_engine::profile::{profile, IndexWork, Progress};
use cambium_engine::{Config, Engine, PackageQuerier, Revisions, SynthQuerier};
use cambium_synth::{ChangeSpec, GraphSpec, SynthGraph};

struct Snapshots(HashMap<String, SynthGraph>);

impl Revisions for Snapshots {
    fn querier(&self, sha: &str) -> cambium_engine::Result<Box<dyn PackageQuerier + '_>> {
        Ok(Box::new(SynthQuerier::new(
            self.0
                .get(sha)
                .unwrap_or_else(|| panic!("no graph for {sha}")),
        )))
    }
}

/// The same snapshots, with some commits that Bazel cannot query: the old
/// commit whose fetch has moved, or whose toolchain is gone.
struct Failing<'a> {
    inner: &'a Snapshots,
    broken: HashSet<String>,
}

impl Revisions for Failing<'_> {
    fn querier(&self, sha: &str) -> cambium_engine::Result<Box<dyn PackageQuerier + '_>> {
        if self.broken.contains(sha) {
            return Err(cambium_engine::EngineError::BazelQuery {
                what: format!("//... at {sha}"),
                code: Some(7),
                stderr: "ERROR: no such package '@@broken//': fetch failed".into(),
            });
        }
        self.inner.querier(sha)
    }
}

fn history(revisions: u64) -> (tempfile::TempDir, Engine, Vec<String>, Snapshots) {
    let dir = tempfile::tempdir().unwrap();
    let graph = cambium_synth::generate(&GraphSpec::preset("tiny").unwrap());
    let change_spec = ChangeSpec {
        seed: 11,
        ..ChangeSpec::preset("typical-pr").unwrap()
    };
    let repo = cambium_synth::emit_git_history(graph, &change_spec, revisions, dir.path(), true)
        .expect("emitting a synthetic repository");
    let cfg = Config::load(dir.path()).unwrap();
    let engine = Engine::with_parts(
        cfg,
        cambium_diff::Repo::new(dir.path().to_path_buf()),
        cambium_bazel::Bazel::at(dir.path().to_path_buf(), Some("/nonexistent/bazel")),
    )
    .with_cache_dir(dir.path().join(".cambium-cache"));
    let snapshots = Snapshots(repo.commits.iter().cloned().zip(repo.graphs).collect());
    (dir, engine, repo.commits, snapshots)
}

#[test]
fn walks_the_last_n_commits_and_describes_the_head_index() {
    let (dir, engine, commits, snapshots) = history(6);
    let out = dir.path().join("head.cmbx");
    let mut seen = Vec::new();
    let p = profile(&engine, &snapshots, "HEAD", 4, &out, &mut |e| seen.push(e)).unwrap();

    // Four commits back from HEAD: the cold build is at the fifth-from-last
    // commit, and the walk covers the four after it.
    let n = commits.len();
    let (head, from) = (&commits[n - 1], &commits[n - 5]);
    assert_eq!(&p.head, head);
    assert_eq!(&p.from, from);
    assert_eq!(&p.cold.commit, from);
    assert_eq!(p.commits.len(), 4);
    for (i, step) in p.commits.iter().enumerate() {
        assert_eq!(step.parent, commits[n - 5 + i]);
        assert_eq!(step.commit, commits[n - 4 + i]);
        assert!(step.changed_files > 0, "step {i} changed nothing");
        assert_eq!(step.date.len(), "MM-DD HH:MM".len(), "{:?}", step.date);
        // Nothing here goes global, so a step either spliced rows into the
        // index or changed nothing the graph models.
        match &step.index {
            IndexWork::Spliced { rows_rewritten } => assert!(*rows_rewritten > 0),
            IndexWork::Identical => {}
            other => panic!("step {i} rebuilt the index: {other:?}"),
        }
    }
    assert!(
        p.commits
            .iter()
            .any(|s| matches!(s.index, IndexWork::Spliced { .. })),
        "a BUILD-file commit splices rows: {:?}",
        p.commits.iter().map(|s| &s.index).collect::<Vec<_>>()
    );
    assert_eq!(p.summary.commits, 4);
    assert_eq!(p.summary.globals, 0);
    assert_eq!(p.summary.rebuild_every_commits, None);

    // The head index is on disk in the form the engine writes, and the
    // summary describes that file.
    let f = cambium_index::IndexFile::open(&out).unwrap();
    assert_eq!(f.view().commit, head);
    assert_eq!(&p.index.commit, head);
    assert_eq!(p.index.targets, f.view().counts().rules);
    assert_eq!(
        p.index.bytes_on_disk,
        std::fs::metadata(&out).unwrap().len()
    );
    assert!(
        p.index.compression.starts_with("zstd-"),
        "{}",
        p.index.compression
    );

    // Progress opened with the range, named the cold build and every step in
    // order, and closed with the walk.
    assert!(matches!(seen.first(), Some(Progress::Range { commits, .. }) if *commits == 4));
    assert!(seen
        .iter()
        .any(|e| matches!(e, Progress::ColdBuild { commit } if commit == from)));
    assert!(seen
        .iter()
        .any(|e| matches!(e, Progress::ColdBuilt { targets, .. } if *targets > 0)));
    assert!(
        matches!(seen.last(), Some(Progress::Walked { commits, globals, .. })
        if *commits == 4 && *globals == 0)
    );
    let rows: Vec<usize> = seen
        .iter()
        .filter_map(|e| match e {
            Progress::Stepped { index, step, .. } => {
                assert!(!step.date.is_empty());
                Some(*index)
            }
            _ => None,
        })
        .collect();
    assert_eq!(rows, [1, 2, 3, 4]);
    let steps: Vec<usize> = seen
        .iter()
        .filter_map(|e| match e {
            Progress::Step { index, of, .. } => {
                assert_eq!(*of, 4);
                Some(*index)
            }
            _ => None,
        })
        .collect();
    assert_eq!(steps, [1, 2, 3, 4]);

    // The document round-trips.
    let text = serde_json::to_string(&p).unwrap();
    let back: cambium_engine::Profile = serde_json::from_str(&text).unwrap();
    assert_eq!(back.summary.commits, 4);
}

#[test]
fn a_short_history_is_walked_as_far_as_it_goes() {
    let (dir, engine, commits, snapshots) = history(2);
    let out = dir.path().join("head.cmbx");
    let p = profile(&engine, &snapshots, "HEAD", 50, &out, &mut |_| {}).unwrap();
    assert_eq!(p.from, commits[0]);
    assert_eq!(p.commits.len(), commits.len() - 1);

    // One commit asked for at the root: the cold build alone.
    let p = profile(&engine, &snapshots, &commits[0], 10, &out, &mut |_| {}).unwrap();
    assert_eq!(p.from, commits[0]);
    assert!(p.commits.is_empty());
    assert_eq!(p.summary.commits, 0);
}

#[test]
fn a_commit_that_fails_is_a_failed_step_and_the_next_is_indexed_cold() {
    let (dir, engine, commits, snapshots) = history(6);
    let out = dir.path().join("head.cmbx");
    let n = commits.len();
    // Walk the last four; the third step's commit does not build.
    let broken = commits[n - 2].clone();
    let revs = Failing {
        inner: &snapshots,
        broken: [broken.clone()].into(),
    };
    let mut seen = Vec::new();
    let p = profile(&engine, &revs, "HEAD", 4, &out, &mut |e| seen.push(e)).unwrap();
    assert_eq!(p.commits.len(), 4);
    let failed = &p.commits[2];
    assert_eq!(failed.commit, broken);
    let err = failed.failed.as_ref().expect("the step records its error");
    assert_eq!(err.code.as_str(), "CAMBI-E005");
    assert!(err.message.contains("fetch failed"), "{}", err.message);
    assert_eq!(failed.index, IndexWork::Failed);
    // The next commit has no index at its parent, so it is indexed cold and
    // counted as a rebuild with its reason.
    let next = &p.commits[3];
    assert!(next.cold_after_failure);
    assert!(next.failed.is_none());
    assert!(matches!(&next.index, IndexWork::Rebuilt { reason } if reason.contains("failed")));
    assert!(seen
        .iter()
        .any(|e| matches!(e, Progress::Recover { commit, .. } if commit == &commits[n - 1])));
    assert_eq!(p.summary.failed, 1);
    assert_eq!(p.summary.rebuilt_after_failure, 1);
    assert_eq!(p.summary.commits, 4);
    assert_eq!(p.summary.rebuild.commits, 1);
    assert_eq!(p.summary.incremental.commits, 2);
    assert!(p.stopped.is_none());
    // The walk still ends at the head with an index on disk.
    assert_eq!(p.index.commit, commits[n - 1]);
    let text = serde_json::to_string(&p).unwrap();
    let back: cambium_engine::Profile = serde_json::from_str(&text).unwrap();
    assert_eq!(back.summary.failed, 1);
}

#[test]
fn a_cold_build_that_fails_moves_to_the_next_commit() {
    let (dir, engine, commits, snapshots) = history(6);
    let out = dir.path().join("head.cmbx");
    let n = commits.len();
    let revs = Failing {
        inner: &snapshots,
        broken: [commits[n - 5].clone()].into(),
    };
    let p = profile(&engine, &revs, "HEAD", 4, &out, &mut |_| {}).unwrap();
    assert_eq!(p.from, commits[n - 5]);
    assert!(p.from_failed.is_some());
    assert_eq!(p.cold.commit, commits[n - 4]);
    assert!(p.cold.targets > 0);
    assert!(p.commits[0].cold_after_failure);
    assert_eq!(p.commits.len(), 4);
    assert_eq!(p.summary.failed, 0);
}

#[test]
fn consecutive_failures_stop_the_walk_with_a_report_for_what_was_walked() {
    let (dir, engine, commits, snapshots) = history(8);
    let out = dir.path().join("head.cmbx");
    let n = commits.len();
    // Six steps; the last four commits do not build.
    let revs = Failing {
        inner: &snapshots,
        broken: commits[n - 4..].iter().cloned().collect(),
    };
    let p = profile(&engine, &revs, "HEAD", 6, &out, &mut |_| {}).unwrap();
    assert_eq!(
        p.commits.len(),
        2 + cambium_engine::profile::MAX_CONSECUTIVE_FAILURES
    );
    assert!(p.stopped.as_deref().unwrap().contains("in a row failed"));
    assert_eq!(p.summary.failed, 3);
    // The index kept is the last one that built.
    assert_eq!(p.index.commit, commits[n - 5]);

    // Nothing builds at all: the first failure is the error.
    let all = Failing {
        inner: &snapshots,
        broken: commits.iter().cloned().collect(),
    };
    let err = profile(&engine, &all, "HEAD", 2, &out, &mut |_| {}).unwrap_err();
    assert!(matches!(
        err,
        cambium_engine::EngineError::BazelQuery { .. }
    ));
}

#[test]
fn the_shares_are_of_the_cold_index() {
    let (dir, engine, _, snapshots) = history(6);
    let out = dir.path().join("head.cmbx");
    let p = profile(&engine, &snapshots, "HEAD", 4, &out, &mut |_| {}).unwrap();
    assert!(p.cold.targets > 0);
    let s = &p.summary;
    assert_eq!(
        s.incremental_affected.p50 as f64 / p.cold.targets as f64,
        s.affected_share
    );
    assert!(!p.repository.is_empty());
    assert!(p.commits.iter().all(|c| !c.subject.is_empty()));
    assert_eq!(s.index_spliced + s.index_unchanged, s.commits);
}
