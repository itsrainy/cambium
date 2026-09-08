//! Helpers shared by the engine's integration tests.
//!
//! Every test binary in `tests/` is compiled on its own, so anything shared
//! has to live in a module each one includes. Each binary uses a different
//! part of this, hence the blanket `dead_code` allowance: a helper is not dead
//! because *this* binary does not call it.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use cambium_engine::query::Rows;
use cambium_engine::{Config, Engine, EngineError, PackageQuerier};
use cambium_index::{NodeKind, PackageUpdate, TargetRow};

/// Runs git in `dir` and returns its trimmed stdout, panicking with stderr on
/// failure — a failed `git init` inside a fixture is a broken test, not a
/// condition worth propagating.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Creates `root` and makes it a git repository with an identity, so commits
/// work on a machine with no global git config (which CI is).
pub fn git_repo(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "t"]);
    // [`synthetic_engine`] keeps its cache inside the repository, so it has to
    // be invisible to `git add -A` — otherwise a fetched index lands in the
    // next commit and moves the tree, which is precisely what the tree-keying
    // tests measure. `.git/info/exclude` rather than a `.gitignore`, because
    // that file would itself be in the tree.
    std::fs::write(
        root.join(".git/info/exclude"),
        CACHE_DIR_NAME.to_string() + "/\n",
    )
    .unwrap();
}

/// Where [`synthetic_engine`] puts its cache, relative to the repository root.
const CACHE_DIR_NAME: &str = "_cambium_cache";

/// Writes a file, creating its parent directories.
pub fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, text).unwrap();
}

/// `git add -A && git commit`, returning the new commit's full sha.
///
/// Empty commits are allowed: the tree-keying tests want two commits with
/// identical content, which is the whole case they are about.
pub fn commit(root: &Path, message: &str) -> String {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", message]);
    git(root, &["rev-parse", "HEAD"])
}

/// An engine over `root` with a Bazel that cannot run.
///
/// Every test here supplies its own rows through a [`PackageQuerier`], so the
/// binary path must never be reached; pointing it at a path that does not
/// exist turns "the test accidentally shelled out to Bazel" from a slow
/// success into a loud failure. The cache is put under `root` so the fixture's
/// tempdir owns it, rather than the shared default cache directory —
/// [`git_repo`] excludes it from the repository so it cannot reach a tree.
pub fn synthetic_engine(root: &Path, cfg: Config) -> Engine {
    Engine::with_parts(
        cfg,
        cambium_diff::Repo::new(root.to_path_buf()),
        cambium_bazel::Bazel::at(root.to_path_buf(), Some("/nonexistent/bazel")),
    )
    .with_cache_dir(root.join(CACHE_DIR_NAME))
}

/// A querier that reads a toy BUILD syntax out of a directory: one rule per
/// line, `name dep dep ...`.
///
/// It stands in for Bazel wherever a test cares *which checkout* the rows came
/// from — it reads whatever directory it is pointed at, which is exactly the
/// substitution the revision-workspace bug was about — and wherever a test
/// needs the package set to follow real BUILD files appearing and disappearing
/// across commits.
pub struct DirQuerier {
    pub root: PathBuf,
}

impl DirQuerier {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Every directory under the root holding a `BUILD` or `BUILD.bazel`,
    /// which is Bazel's own rule for what a package is.
    pub fn packages(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return out;
        };
        for e in entries.flatten() {
            if self.build_file(&e.file_name().to_string_lossy()).is_some() {
                out.push(e.file_name().to_string_lossy().into_owned());
            }
        }
        out.sort();
        out
    }

    /// The BUILD file Bazel would read for `pkg`. `BUILD` wins over
    /// `BUILD.bazel`, as it does in Bazel itself.
    fn build_file(&self, pkg: &str) -> Option<PathBuf> {
        let dir = self.root.join(pkg);
        [dir.join("BUILD"), dir.join("BUILD.bazel")]
            .into_iter()
            .find(|p| p.is_file())
    }

    pub fn update(&self, pkg: &str) -> PackageUpdate {
        let text = self
            .build_file(pkg)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
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
    fn query(&self, packages: &[String]) -> Result<Rows, EngineError> {
        Ok(packages
            .iter()
            .map(|p| self.update(p))
            .collect::<Vec<_>>()
            .into())
    }

    fn universe(&self) -> Result<Rows, EngineError> {
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
