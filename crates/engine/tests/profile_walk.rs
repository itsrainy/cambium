//! `cambi profile`'s walk over a synthetic git history: a cold build at the
//! start of the range, one step per commit, the head index on disk, and a
//! summary whose counts agree with the steps. Bazel is stubbed out; the rows
//! come from the synthetic graphs the emitter kept per commit.

use std::collections::HashMap;

use cambium_engine::profile::{profile, Progress};
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
    }
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

    // Progress named the cold build and every step, in order.
    assert!(matches!(seen.first(), Some(Progress::ColdBuild { commit }) if commit == from));
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
