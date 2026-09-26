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

use crate::{run, GitError, Result};

/// A detached worktree of `owner`, removed when it goes out of scope.
#[derive(Debug)]
pub struct Worktree {
    path: PathBuf,
    /// The worktree (or mirror) the new one was registered against; `git
    /// worktree remove` has to run from there.
    owner: PathBuf,
    /// Set when `Drop` must leave the directory alone: either it was already
    /// a worktree when `create` was called, so it belongs to whoever made it,
    /// or the caller asked to [`keep`](Worktree::keep) it.
    keep_on_drop: bool,
}

impl Worktree {
    /// Adds a worktree of `owner` at `rev`, in `path`.
    ///
    /// If `path` already holds a worktree it is reused: it is forced back onto
    /// `rev`, losing local changes to tracked files, and left in place when
    /// this handle drops. Reuse matters when consecutive runs share a
    /// revision, and re-adding a worktree per run would dominate.
    pub fn create(owner: &Path, rev: &str, path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(GitError::Spawn)?;
        }
        if path.join(".git").exists() {
            run(owner, &["worktree", "prune"]).ok();
            force_checkout(path, rev)?;
            return Ok(Self {
                path: path.to_path_buf(),
                owner: owner.to_path_buf(),
                keep_on_drop: true,
            });
        }
        // A stale directory with no `.git` would make `worktree add` fail.
        if path.exists() {
            std::fs::remove_dir_all(path).map_err(GitError::Spawn)?;
        }
        run(owner, &["worktree", "prune"]).ok();
        run(
            owner,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                &path.display().to_string(),
                rev,
            ],
        )?;
        Ok(Self {
            path: path.to_path_buf(),
            owner: owner.to_path_buf(),
            keep_on_drop: false,
        })
    }

    /// The directory holding the checkout at the revision asked for.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The checkout this worktree was added from, where untracked files the
    /// worktree does not get (local Bazel config, say) can be copied from.
    pub fn owner(&self) -> &Path {
        &self.owner
    }

    /// Leaves the directory registered and on disk when this handle drops,
    /// for a caller that wants the next run to reuse it.
    ///
    /// A worktree of a repository the size of tensorflow costs a full checkout
    /// to add and a cold Bazel server to use, so a caller that runs a tool
    /// once per change set pays both per change set unless it keeps the one it
    /// made. [`Worktree::create`] reuses a directory that is already a
    /// worktree, so keeping it is all the next run needs.
    pub fn keep(mut self) {
        self.keep_on_drop = true;
    }

    /// Moves this worktree to another revision, losing local changes to
    /// tracked files.
    pub fn checkout(&self, rev: &str) -> Result<()> {
        force_checkout(&self.path, rev)
    }

    /// Unregisters and deletes the worktree. `Drop` calls this and ignores
    /// failures; call it directly to see them.
    pub fn remove(&self) -> Result<()> {
        run(
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
        if self.keep_on_drop {
            return;
        }
        if let Err(e) = self.remove() {
            tracing::warn!(path = %self.path.display(), error = %e, "could not remove the worktree");
        }
    }
}

/// Puts the worktree in `dir` on `rev`, discarding local changes to
/// tracked files.
///
/// `--force`, never a plain checkout: a plain one keeps local
/// modifications, and on a case-insensitive filesystem a worktree acquires
/// them without anyone editing anything (two paths that differ only in
/// case collide, and git leaves one of them reported as deleted).
fn force_checkout(dir: &Path, rev: &str) -> Result<()> {
    run(dir, &["checkout", "--quiet", "--force", "--detach", rev])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testrepo::TwoCommits;

    #[test]
    fn adds_and_removes_a_detached_worktree() {
        let f = TwoCommits::new();
        let wt_path = f.dir.path().join("wt").join(&f.first);
        {
            let wt = Worktree::create(&f.root, &f.first, &wt_path).unwrap();
            assert_eq!(std::fs::read(wt.path().join("lib/a.cc")).unwrap(), b"one");
            assert_eq!(
                crate::testrepo::git(wt.path(), &["rev-parse", "HEAD"]),
                f.first
            );
            // The owning checkout is untouched.
            assert_eq!(std::fs::read(f.root.join("lib/a.cc")).unwrap(), b"two");
        }
        assert!(!wt_path.exists(), "the worktree should be gone");
    }
}
