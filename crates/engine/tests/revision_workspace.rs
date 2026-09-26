//! Which checkout Bazel is run in.
//!
//! `bazel query` reads the workspace directory, so a run that needs rows for a
//! revision the working tree is not on has to get a checkout of that revision
//! first. Left unfixed, `affected` on a CI worker (checked out at head, no base
//! index in the store) built its "base" index from head's BUILD files: a rule
//! added at head appeared in both indexes with the same hash and was never
//! reported.
//!
//! Real git, no Bazel: the querier the engine builds is asked *where* it would
//! run, and a BUILD-file-reading stand-in is pointed at that same directory —
//! which is exactly the substitution the bug was about.

use std::path::{Path, PathBuf};

use cambium_engine::{Config, Engine};
use cambium_testkit::{commit_all, init_repo, synthetic_engine, write, DirQuerier};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    engine: Engine,
    base: String,
    head: String,
}

/// Two commits: base has `//lib:a`, head adds `//lib:b`. The working tree is
/// left at head, which is what a CI worker running `affected` has.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    init_repo(&root);
    write(&root, "MODULE.bazel", "");

    write(&root, "lib/BUILD", "a\n");
    let base = commit_all(&root, "base");

    write(&root, "lib/BUILD", "a\nb a\n");
    let head = commit_all(&root, "head adds b");

    let engine = synthetic_engine(&root, Config::default());
    Fixture {
        _dir: dir,
        root,
        engine,
        base,
        head,
    }
}

fn head_of(dir: &Path) -> String {
    cambium_diff::Repo::new(dir).rev_parse("HEAD").unwrap()
}

#[test]
fn a_revision_the_tree_is_not_on_gets_its_own_worktree() {
    let f = fixture();
    assert!(f.engine.needs_revision_workspace(&f.base));

    let path;
    {
        let q = f.engine.bazel_querier(&f.base).unwrap();
        assert!(
            q.uses_worktree(),
            "base is not what the tree is checked out at"
        );
        path = q.workspace_path().to_path_buf();
        assert_ne!(path, f.root, "it must not be the working tree");
        assert_eq!(
            head_of(&path),
            f.base,
            "the querier's workspace must be checked out at base"
        );
        assert_eq!(
            std::fs::read_to_string(path.join("lib/BUILD")).unwrap(),
            "a\n",
            "and so must the BUILD files bazel would read"
        );
    }
    assert!(
        !path.exists(),
        "the worktree must be removed with the querier"
    );
    // ...and it is unregistered, not merely deleted.
    let listed = cambium_diff::Repo::new(&f.root)
        .git(&["worktree", "list"])
        .unwrap();
    assert!(
        !listed.contains(&path.display().to_string()),
        "worktree still registered: {listed}"
    );
}

#[test]
fn the_revision_the_tree_is_already_on_costs_nothing() {
    let f = fixture();
    assert!(!f.engine.needs_revision_workspace(&f.head));
    let q = f.engine.bazel_querier(&f.head).unwrap();
    assert!(!q.uses_worktree(), "no second checkout for head");
    assert_eq!(q.workspace_path(), f.root);
    assert!(
        !f.root.join("_cache/worktrees").exists(),
        "nothing should have been created"
    );
}

#[test]
fn a_dirty_working_tree_at_head_stays_on_the_fast_path() {
    let f = fixture();
    // `cambi affected` on a developer's checkout is meant to see the edits in
    // front of it, so an uncommitted change must not push HEAD into a worktree
    // (which would index the commit and miss them).
    std::fs::write(f.root.join("lib/BUILD"), "a\nb a\nc\n").unwrap();
    assert!(cambium_diff::Repo::new(&f.root).is_dirty());
    assert!(!f.engine.needs_revision_workspace(&f.head));
    let q = f.engine.bazel_querier(&f.head).unwrap();
    assert!(!q.uses_worktree());
    assert_eq!(q.workspace_path(), f.root);
}

#[test]
fn an_untracked_config_is_mirrored_into_the_worktree() {
    let f = fixture();
    std::fs::write(f.root.join("cambium.toml"), "# generated\n").unwrap();
    let q = f.engine.bazel_querier(&f.base).unwrap();
    assert!(q.workspace_path().join("cambium.toml").exists());
}

#[test]
fn a_cold_base_index_is_built_from_the_base_tree_not_from_head() {
    let f = fixture();
    // The bug, exactly: HEAD is at head, the store has nothing, so the base
    // index is built cold. Built in the working tree it would contain
    // `//lib:b` — a rule that does not exist at base — with the same hash at
    // both ends of the range, so the run would report nothing.
    let q = f.engine.bazel_querier(&f.base).unwrap();
    let dq = DirQuerier {
        root: q.workspace_path().to_path_buf(),
    };
    let base_index = f.engine.full_build_with(&f.base, &dq).unwrap();
    let v = base_index.view();
    assert!(v.id_of("//lib:a").is_some(), "base has //lib:a");
    assert!(
        v.id_of("//lib:b").is_none(),
        "//lib:b is added at head; a base index that has it is unsound"
    );

    // The head side, by contrast, is read straight from the working tree.
    let qh = f.engine.bazel_querier(&f.head).unwrap();
    let dqh = DirQuerier {
        root: qh.workspace_path().to_path_buf(),
    };
    let head_index = f.engine.full_build_with(&f.head, &dqh).unwrap();
    assert!(head_index.view().id_of("//lib:b").is_some());
}

/// `cambi profile` moves one worktree across history, and bazelisk starts
/// whichever Bazel each commit's `.bazelversion` names on the worktree's one
/// output base. A `bazel shutdown` reaches only a server of the version the
/// client is, so the server a walk leaves running is stopped only if the
/// shutdown is run under the version that started it. Before this, the drop
/// ran one shutdown under the last commit's version; a walk whose last query
/// ran under an older `.bazelversion` (rules_shell: 8.2.1 queried, 9.1.1 at
/// head) left that server running on a worktree that was then deleted, where
/// nothing could reach it again.
///
/// The stub Bazel logs each command with the `.bazelversion` it ran under;
/// every version that ran a server command must end with a shutdown.
#[test]
fn a_profile_worktree_stops_every_server_it_started() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    init_repo(&root);
    write(&root, "MODULE.bazel", "");
    write(&root, "lib/BUILD", "a\n");
    write(&root, ".bazelversion", "8.2.1\n");
    let old = commit_all(&root, "8.2.1");
    write(&root, ".bazelversion", "9.1.1\n");
    let mid = commit_all(&root, "9.1.1");
    write(&root, "README", "no query needed here\n");
    let head = commit_all(&root, "a file no package owns");

    let log = dir.path().join("bazel.log");
    let stub = dir.path().join("bazel");
    cambium_testkit::executable_script(
        &stub,
        &format!(
            "echo \"$(cat .bazelversion 2>/dev/null | tr -d '\\n') $*\" >> '{}'\nexit 0",
            log.display()
        ),
    );
    let engine = Engine::with_parts(
        Config::default(),
        cambium_diff::Repo::new(root.clone()),
        cambium_bazel::Bazel::at(root.clone(), Some(stub.to_str().unwrap())),
    )
    .with_cache_dir(dir.path().join("cache"));

    use cambium_engine::Revisions as _;
    let revs = cambium_engine::WorktreeRevisions::create(&engine, &head).unwrap();
    // The walk: a query at the old version, one at the new, then a commit
    // that asks Bazel nothing, and the walk ends.
    let _ = revs.querier(&old).unwrap().query(&["lib".to_string()]);
    let _ = revs.querier(&mid).unwrap().query(&["lib".to_string()]);
    let _ = revs.querier(&old).unwrap().query(&["lib".to_string()]);
    drop(revs.querier(&head).unwrap());
    drop(revs);

    let text = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<(&str, &str)> = text
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(_, cmd)| !cmd.starts_with("--"))
        .collect();
    let versions: std::collections::BTreeSet<&str> = lines.iter().map(|(v, _)| *v).collect();
    assert!(versions.len() >= 2, "both versions ran something:\n{text}");
    for v in versions {
        let last = lines.iter().rev().find(|(w, _)| *w == v).unwrap();
        assert!(
            last.1.starts_with("shutdown"),
            "the {v} server was left running; its last command was `{}`:\n{text}",
            last.1
        );
    }
}
