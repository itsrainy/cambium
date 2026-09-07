//! Running Bazel against a revision the working tree is not checked out at.
//!
//! `bazel query` reads BUILD files from the workspace directory, and nothing
//! else. So a run that needs rows for a revision other than the one on disk —
//! `affected` with no base index in the store, which either replays an ancestor
//! forward or cold-builds the base while the checkout sits at head — cannot use
//! the workspace it was opened on. Doing so produced base indexes containing
//! *head's* rules: a target added at head appeared in the base index with the
//! same rule hash at both ends, and was then reported as unaffected. That is a
//! missed target, the one failure mode the whole design exists to prevent.
//!
//! The fix is a second checkout: `git worktree add --detach` at the revision,
//! its own `Bazel` handle pointed at it, removed when the run is done. Two
//! details make it safe and affordable:
//!
//! - **Its own output base.** Sharing one across workspace directories poisons
//!   the external repository tree: repository rules write absolute paths into
//!   it (symlinks back into the workspace that fetched them), the fetch marker
//!   does not mention the workspace, so Bazel never refetches — and once the
//!   first workspace is deleted every one of those symlinks dangles, failing
//!   *every* later query with a repository-mapping error. See
//!   [`derive_startup_flags`].
//! - **Nothing is done when nothing needs doing.** `baseline update` and
//!   `corpus replay` check each commit out before calling the engine, so their
//!   revision is the one on disk and they must keep paying nothing: no
//!   worktree, no second Bazel server. [`Engine::needs_revision_workspace`]
//!   compares trees and says no.

use std::path::{Path, PathBuf};

use cambium_bazel::Bazel;
use cambium_diff::Worktree;
use cambium_index::PackageUpdate;

use crate::config::CONFIG_FILE;
use crate::error::{EngineError, Result};
use crate::query::{BazelQuerier, PackageQuerier};

/// How many characters of a sha name a derived output base.
const SHORT_SHA: usize = 12;

/// A detached checkout at one revision, plus the Bazel that reads it.
#[derive(Debug)]
pub struct RevisionWorkspace {
    worktree: Worktree,
}

impl RevisionWorkspace {
    /// Adds a worktree of `repo` at `sha` under `dir`, one directory per sha,
    /// and copies in the untracked config files the engine needs.
    pub fn create(repo: &cambium_diff::Repo, sha: &str, dir: &Path) -> Result<Self> {
        let path = dir.join(sha);
        let worktree = repo.worktree_at(sha, &path)?;
        mirror_untracked_config(repo.root(), worktree.path())?;
        tracing::info!(
            %sha,
            path = %worktree.path().display(),
            "the working tree is not at this revision; querying bazel in a detached worktree"
        );
        Ok(Self { worktree })
    }

    pub fn path(&self) -> &Path {
        self.worktree.path()
    }

    /// A Bazel handle for this checkout: same binary, same query flags, same
    /// environment as `template`, and a startup flag list whose `--output_base`
    /// has been moved aside.
    pub fn bazel(&self, template: &Bazel, sha: &str) -> Bazel {
        Bazel::at(
            self.worktree.path().to_path_buf(),
            template.binary().to_str(),
        )
        .with_extra_query_flags(template.extra_query_flags().to_vec())
        .with_startup_flags(derive_startup_flags(template.startup_flags(), sha))
        .with_env(template.env().to_vec())
    }
}

/// The startup flags for a worktree's Bazel: the run's flags with
/// `--output_base=P` rewritten to `P-<short sha>`.
///
/// An output base is only safe to share between invocations in the same
/// workspace *directory*; two workspaces pointed at one base corrupt each
/// other's external repositories. Rewriting rather than dropping the flag is
/// deliberate: it keeps the base on whatever filesystem the operator chose (CI
/// images routinely put it on the fast disk, and letting Bazel derive one would
/// silently move it to `$HOME`), and it is a *stable* function of the revision,
/// so the worktree can be removed while the output base stays — a later run at
/// the same revision reconnects to a warm server instead of re-analysing.
///
/// With no `--output_base` in the flags, nothing is added: Bazel derives one
/// from the workspace path, and the worktree path is already unique per sha.
pub fn derive_startup_flags(flags: &[String], sha: &str) -> Vec<String> {
    let short = &sha[..sha.len().min(SHORT_SHA)];
    flags
        .iter()
        .map(|f| match f.strip_prefix("--output_base=") {
            Some(base) => format!("--output_base={}-{short}", base.trim_end_matches('/')),
            None => f.clone(),
        })
        .collect()
}

/// Copies an untracked `cambium.toml` into the worktree.
///
/// A fresh `git worktree add` only has what git tracks, and cambium's own
/// config is routinely untracked (the corpus generates it into the repository
/// root and excludes it). Two things go wrong without it: the engine reads no
/// config in that tree, and — where the root package globs its directory, as
/// `bazel/bazel`'s `filegroup(name = "srcs", srcs = glob(["*"]))` does — the
/// two trees disagree about a file that has nothing to do with the commit.
///
/// A tracked config is left alone; the checkout already has the right version.
fn mirror_untracked_config(repo_root: &Path, dest: &Path) -> Result<()> {
    let src = repo_root.join(CONFIG_FILE);
    if !src.exists() {
        return Ok(());
    }
    let repo = cambium_diff::Repo::new(repo_root);
    if repo
        .git(&["ls-files", "--error-unmatch", CONFIG_FILE])
        .is_ok()
    {
        return Ok(());
    }
    std::fs::copy(&src, dest.join(CONFIG_FILE))?;
    Ok(())
}

/// The row source for one revision: a `BazelQuerier`, plus the worktree it
/// reads when that revision is not the one on disk.
///
/// The worktree field is declared *after* the querier so that drop order
/// removes the directory only once nothing is querying it any more.
pub struct RevisionQuerier {
    querier: BazelQuerier,
    _workspace: Option<RevisionWorkspace>,
}

impl RevisionQuerier {
    pub fn new(querier: BazelQuerier, workspace: Option<RevisionWorkspace>) -> Self {
        Self {
            querier,
            _workspace: workspace,
        }
    }

    /// The directory Bazel is run in. The tests assert this is a checkout of
    /// the revision that was asked for.
    pub fn workspace_path(&self) -> &Path {
        self.querier.bazel.workspace()
    }

    /// Whether this querier had to make a worktree.
    pub fn uses_worktree(&self) -> bool {
        self._workspace.is_some()
    }
}

impl PackageQuerier for RevisionQuerier {
    fn query(&self, packages: &[String]) -> std::result::Result<Vec<PackageUpdate>, EngineError> {
        self.querier.query(packages)
    }

    fn universe(&self) -> std::result::Result<crate::query::Universe, EngineError> {
        self.querier.universe()
    }

    fn rbuildfiles(
        &self,
        bzl_paths: &[String],
    ) -> std::result::Result<cambium_bazel::Rbuildfiles, EngineError> {
        self.querier.rbuildfiles(bzl_paths)
    }

    fn version(&self) -> String {
        self.querier.version()
    }

    fn workspace(&self) -> Option<&Path> {
        self.querier.workspace()
    }
}

/// Where a run keeps its per-revision worktrees.
pub fn worktrees_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("worktrees")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_output_base_moves_aside_per_revision() {
        let flags = vec![
            "--bazelrc=/etc/rc".to_string(),
            "--output_base=/fast/ob".to_string(),
        ];
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            derive_startup_flags(&flags, sha),
            vec![
                "--bazelrc=/etc/rc".to_string(),
                "--output_base=/fast/ob-0123456789ab".to_string()
            ]
        );
        // Stable: the same revision derives the same base, so its server can
        // be reused after the worktree is gone.
        assert_eq!(
            derive_startup_flags(&flags, sha),
            derive_startup_flags(&flags, sha)
        );
        // A trailing slash does not produce `ob/-sha`.
        assert_eq!(
            derive_startup_flags(&["--output_base=/fast/ob/".to_string()], sha),
            vec!["--output_base=/fast/ob-0123456789ab".to_string()]
        );
    }

    #[test]
    fn without_an_output_base_flag_nothing_is_invented() {
        let flags = vec!["--bazelrc=/etc/rc".to_string()];
        assert_eq!(derive_startup_flags(&flags, "abc"), flags);
        assert!(derive_startup_flags(&[], "abc").is_empty());
    }
}
