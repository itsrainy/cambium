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

use cambium_engine::{Config, Engine, PackageQuerier};
use cambium_index::{NodeKind, PackageUpdate, TargetRow};

/// A `PackageQuerier` that reads a toy BUILD syntax out of a directory.
///
/// One rule per line, `name dep dep ...`, so a test can assert which revision's
/// BUILD files reached the index by naming the rules that came back.
struct DirQuerier {
    root: PathBuf,
}

impl DirQuerier {
    fn packages(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return out;
        };
        for e in entries.flatten() {
            if e.path().join("BUILD").exists() {
                out.push(e.file_name().to_string_lossy().into_owned());
            }
        }
        out.sort();
        out
    }

    fn update(&self, pkg: &str) -> PackageUpdate {
        let text = std::fs::read_to_string(self.root.join(pkg).join("BUILD")).unwrap_or_default();
        let mut targets = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let mut words = line.split_whitespace();
            let name = words.next().unwrap();
            let inputs: Vec<String> = words.map(|d| d.to_string()).collect();
            targets.push(TargetRow {
                label: format!("//{pkg}:{name}"),
                kind: NodeKind::Rule,
                rule_class: Some("toy_library".into()),
                rule_hash: *blake3::hash(line.as_bytes()).as_bytes(),
                is_test: false,
                manual: false,
                inputs,
                generating_rule: None,
            });
        }
        PackageUpdate {
            package: pkg.to_string(),
            build_hash: *blake3::hash(text.as_bytes()).as_bytes(),
            bzl_hash: [0; 32],
            targets,
        }
    }
}

impl PackageQuerier for DirQuerier {
    fn query(&self, packages: &[String]) -> cambium_engine::Result<Vec<PackageUpdate>> {
        Ok(packages.iter().map(|p| self.update(p)).collect())
    }

    fn universe(&self) -> cambium_engine::Result<cambium_engine::query::Universe> {
        Ok(self
            .packages()
            .iter()
            .map(|p| self.update(p))
            .collect::<Vec<_>>()
            .into())
    }

    fn workspace(&self) -> Option<&Path> {
        Some(&self.root)
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    engine: Engine,
    base: String,
    head: String,
}

fn git(root: &Path, args: &[&str]) {
    cambium_diff::Repo::new(root)
        .git(args)
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
}

/// Two commits: base has `//lib:a`, head adds `//lib:b`. The working tree is
/// left at head, which is what a CI worker running `affected` has.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::write(root.join("MODULE.bazel"), "").unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "t"]);

    std::fs::write(root.join("lib/BUILD"), "a\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "base"]);
    let base = cambium_diff::Repo::new(&root).rev_parse("HEAD").unwrap();

    std::fs::write(root.join("lib/BUILD"), "a\nb a\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "head adds b"]);
    let head = cambium_diff::Repo::new(&root).rev_parse("HEAD").unwrap();

    let engine = Engine::with_parts(
        Config::default(),
        cambium_diff::Repo::new(root.clone()),
        cambium_bazel::Bazel::at(root.clone(), Some("/nonexistent/bazel")),
    )
    .with_cache_dir(dir.path().join("_cache"));

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
