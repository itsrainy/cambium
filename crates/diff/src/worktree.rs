//! A second checkout of the repository, pinned at one revision.
//!
//! `git worktree add --detach` rather than a clone: it shares the object
//! store, so adding one costs a checkout rather than a copy of the history,
//! which matters when the repository is TensorFlow.
//!
//! Two callers need this and must not have two implementations of it: the
//! comparison harness, which hands bazel-diff-shaped tools a checkout at base,
//! and the engine, which needs Bazel to *read BUILD files at a revision the
//! working tree is not on* (see `cambium_engine::revision`).

use std::path::{Path, PathBuf};

use crate::{GitError, Repo};

type Result<T> = std::result::Result<T, GitError>;

/// A detached worktree of `owner`, removed when it goes out of scope.
#[derive(Debug)]
pub struct Worktree {
    path: PathBuf,
    /// The worktree (or mirror) the new one was registered against; `git
    /// worktree remove` has to run from there.
    owner: PathBuf,
    /// Set when the directory already existed and we only moved it, so `Drop`
    /// does not delete something the caller owns.
    borrowed: bool,
}

impl Worktree {
    /// Adds a worktree of `owner` at `sha`, in `path`.
    ///
    /// If `path` already holds a worktree it is reused: it is forced back onto
    /// `sha` and left in place when this handle drops. Reuse matters when
    /// consecutive runs share a revision, and re-adding a worktree per run
    /// would dominate.
    ///
    /// `--force` on the checkout, never a plain one: a plain checkout keeps
    /// local modifications, and on a case-insensitive filesystem a worktree
    /// acquires them without anyone editing anything (two paths that differ
    /// only in case collide, and git leaves one of them reported as deleted).
    pub fn create(owner: &Path, sha: &str, path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(GitError::Spawn)?;
        }
        if path.join(".git").exists() {
            git(owner, &["worktree", "prune"]).ok();
            git(path, &["checkout", "--quiet", "--force", "--detach", sha])?;
            return Ok(Self {
                path: path.to_path_buf(),
                owner: owner.to_path_buf(),
                borrowed: true,
            });
        }
        // A stale directory with no `.git` would make `worktree add` fail.
        if path.exists() {
            std::fs::remove_dir_all(path).map_err(GitError::Spawn)?;
        }
        git(owner, &["worktree", "prune"]).ok();
        git(
            owner,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                &path.display().to_string(),
                sha,
            ],
        )?;
        Ok(Self {
            path: path.to_path_buf(),
            owner: owner.to_path_buf(),
            borrowed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn owner(&self) -> &Path {
        &self.owner
    }

    /// True when this handle will leave the directory behind on drop.
    pub fn borrowed(&self) -> bool {
        self.borrowed
    }

    /// Moves this worktree to another revision.
    pub fn checkout(&self, sha: &str) -> Result<()> {
        git(
            &self.path,
            &["checkout", "--quiet", "--force", "--detach", sha],
        )?;
        Ok(())
    }

    /// Unregisters and deletes the worktree. `Drop` calls this and ignores
    /// failures; call it directly to see them.
    pub fn remove(&self) -> Result<()> {
        git(
            &self.owner,
            &[
                "worktree",
                "remove",
                "--force",
                &self.path.display().to_string(),
            ],
        )?;
        Ok(())
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        if self.borrowed {
            return;
        }
        if let Err(e) = self.remove() {
            tracing::warn!(path = %self.path.display(), error = %e, "could not remove the worktree");
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    Repo::new(dir).git(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_and_removes_a_detached_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let g = |args: &[&str]| {
            Repo::new(&root).git(args).unwrap();
        };
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "t@example.com"]);
        g(&["config", "user.name", "t"]);
        std::fs::write(root.join("a"), "one").unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-qm", "one"]);
        let first = Repo::new(&root).rev_parse("HEAD").unwrap();
        std::fs::write(root.join("a"), "two").unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-qm", "two"]);

        let wt_path = dir.path().join("wt").join(&first);
        {
            let wt = Worktree::create(&root, &first, &wt_path).unwrap();
            assert_eq!(std::fs::read(wt.path().join("a")).unwrap(), b"one");
            assert_eq!(Repo::new(wt.path()).rev_parse("HEAD").unwrap(), first);
            // The owning checkout is untouched.
            assert_eq!(std::fs::read(root.join("a")).unwrap(), b"two");
        }
        assert!(!wt_path.exists(), "the worktree should be gone");
    }
}
