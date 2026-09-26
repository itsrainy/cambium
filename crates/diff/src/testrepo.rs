//! A throwaway repository with two commits, for the tests in this crate.
//!
//! Built with plain `git` rather than through [`crate::Repo`], so a test of
//! a `Repo` method is not also relying on the methods that set it up.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Two commits on `main`: `first` adds `lib/a.cc` ("one"); `second` changes
/// it to "two" and adds `lib/b.cc` ("new").
pub(crate) struct TwoCommits {
    /// Holds the directory for the life of the test.
    pub dir: tempfile::TempDir,
    pub root: PathBuf,
    pub first: String,
    pub second: String,
}

impl TwoCommits {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join("lib")).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "t@example.com"]);
        git(&root, &["config", "user.name", "t"]);
        // A developer's global config must not decide whether the fixture
        // can commit.
        git(&root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("lib/a.cc"), "one").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "one"]);
        let first = git(&root, &["rev-parse", "HEAD"]);
        std::fs::write(root.join("lib/a.cc"), "two").unwrap();
        std::fs::write(root.join("lib/b.cc"), "new").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "two"]);
        let second = git(&root, &["rev-parse", "HEAD"]);
        Self {
            dir,
            root,
            first,
            second,
        }
    }

    /// Runs git in the repository and returns its trimmed stdout.
    pub fn git(&self, args: &[&str]) -> String {
        git(&self.root, args)
    }
}

/// Runs git in `dir`, failing the test with git's stderr if it refuses.
pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}
