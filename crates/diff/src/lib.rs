//! Git plumbing: what changed between two revisions, and which ancestors
//! to try when looking for one that has an index.
//!
//! We shell out to `git` rather than linking a library: the operations are few,
//! the output formats are stable, and it keeps cambium honest about running in
//! whatever checkout CI produced (worktrees, shallow clones, alternates).

use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(test)]
mod testrepo;
mod worktree;

pub use worktree::Worktree;

/// Why a git operation gave no answer.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// git could not be started (not installed, not on `PATH`, or the
    /// directory to run it in is gone). [`Worktree::create`] also reports
    /// the filesystem errors it meets preparing the directory this way.
    #[error("failed to run git: {0}")]
    Spawn(#[source] std::io::Error),
    /// git ran and refused. `stderr` is git's own account, kept whole so a
    /// caller can show the reader the line that explains it (a revision
    /// this clone does not have, a shallow history that stops short).
    #[error("git {args} failed with {status}: {stderr}")]
    Failed {
        args: String,
        status: String,
        stderr: String,
    },
    /// git answered in bytes that are not UTF-8. Every answer is read as
    /// text, paths included, so a non-UTF-8 file name in a diff lands here.
    #[error("git output was not valid UTF-8")]
    NotUtf8,
    /// A `--name-status` record cut short: a status with fewer paths after
    /// it than its letter promises.
    #[error("unrecognised diff status {0:?}")]
    BadStatus(String),
}

pub(crate) type Result<T> = std::result::Result<T, GitError>;

/// What happened to one path between two revisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Added,
    Modified,
    Deleted,
    /// Mode or type change (symlink <-> file). Treated as a modification of the
    /// content, which is the conservative reading.
    TypeChanged,
}

/// One path and what happened to it between two revisions: the unit the
/// classifier and the engine reason about. A rename arrives as two of
/// these, so nothing downstream has to know renames exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub status: Status,
    pub path: String,
}

impl Change {
    pub fn new(status: Status, path: impl Into<String>) -> Self {
        Self {
            status,
            path: path.into(),
        }
    }
}

/// A commit as a reader would see it in a row: the object id, the committer
/// date in `MM-DD HH:MM` (in the committer's recorded zone), and the first
/// line of the message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitMeta {
    pub sha: String,
    pub date: String,
    pub subject: String,
}

/// A repository we run git commands in.
#[derive(Clone, Debug)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// A repository rooted at `root`. Nothing is checked here: the first
    /// command run in it is what finds out whether it is one.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Finds the repository root containing `start`.
    pub fn discover(start: &Path) -> Result<Self> {
        let top = run_line(start, &["rev-parse", "--show-toplevel"])?;
        Ok(Self::new(PathBuf::from(top)))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a revision to a full object id.
    pub fn rev_parse(&self, rev: &str) -> Result<String> {
        self.peel(rev, "commit")
    }

    /// The tree object a revision points at.
    ///
    /// Two commits with identical content have identical trees whatever their
    /// shas, which is what lets the index store key indexes by tree: a
    /// rebased or squash-merged commit finds the index built for its
    /// pre-rebase twin.
    pub fn tree_of(&self, rev: &str) -> Result<String> {
        self.peel(rev, "tree")
    }

    /// The id of the `kind` object `rev` peels to (`rev^{kind}`), which
    /// fails rather than guessing when `rev` names nothing of that kind.
    fn peel(&self, rev: &str, kind: &str) -> Result<String> {
        run_line(
            &self.root,
            &["rev-parse", "--verify", &format!("{rev}^{{{kind}}}")],
        )
    }

    /// First-parent history of `rev`, oldest first, at most `max` commits
    /// (including `rev`).
    pub fn first_parent_log(&self, rev: &str, max: usize) -> Result<Vec<String>> {
        let mut log = self.first_parent_walk(rev, max)?;
        log.reverse();
        Ok(log)
    }

    /// The same history with the date and subject of each commit, for a
    /// caller that reports a commit to a person rather than only walking it.
    ///
    /// One `git log` rather than two calls per commit: a thirty-commit
    /// profile would otherwise pay sixty process starts to print sixty
    /// fields. The date is the committer date as `MM-DD HH:MM`, the precision
    /// a row has space for, shown in the zone the committer recorded (what
    /// `--date=format:` does), not converted to the reader's. Two rows from
    /// committers in different zones are therefore not directly comparable.
    pub fn first_parent_log_meta(&self, rev: &str, max: usize) -> Result<Vec<CommitMeta>> {
        let out = run(
            &self.root,
            &[
                "log",
                "--first-parent",
                &format!("--max-count={max}"),
                "--date=format:%m-%d %H:%M",
                "--format=%H%x1f%cd%x1f%s",
                rev,
            ],
        )?;
        let mut v: Vec<CommitMeta> = out
            .lines()
            .filter(|l| !l.is_empty())
            .map(|line| {
                let mut f = line.split('\u{1f}');
                CommitMeta {
                    sha: f.next().unwrap_or_default().to_string(),
                    date: f.next().unwrap_or_default().to_string(),
                    subject: f.next().unwrap_or_default().to_string(),
                }
            })
            .collect();
        v.reverse();
        Ok(v)
    }

    /// Parents of a commit, in order.
    pub fn parents(&self, rev: &str) -> Result<Vec<String>> {
        let out = run(&self.root, &["rev-list", "--parents", "-n", "1", rev])?;
        Ok(out
            .split_whitespace()
            .skip(1)
            .map(|s| s.to_string())
            .collect())
    }

    /// True when the working tree has uncommitted changes to tracked files.
    ///
    /// A dirty tree is not an error: `cambi affected` on a developer's
    /// checkout is meant to see the edits. The answer is reported, not acted
    /// on: `cambi status` tells the reader a run will index the edits as they
    /// are. The engine's choice of checkout compares trees and ignores dirt on
    /// purpose (see `Engine::needs_revision_workspace`).
    ///
    /// A `git status` that fails reads as clean, not as an error, because the
    /// flag only annotates a report and a probe should not fail over it.
    pub fn is_dirty(&self) -> bool {
        match run(
            &self.root,
            &["status", "--porcelain", "--untracked-files=no"],
        ) {
            Ok(out) => !out.trim().is_empty(),
            Err(_) => false,
        }
    }

    /// Adds a detached worktree of this repository at `rev`, in `path`.
    pub fn worktree_at(&self, rev: &str, path: &Path) -> Result<Worktree> {
        Worktree::create(&self.root, rev, path)
    }

    /// Runs an arbitrary git command in the repository and returns stdout.
    pub fn git(&self, args: &[&str]) -> Result<String> {
        run(&self.root, args)
    }

    /// First-parent ancestors of `rev`, nearest first, excluding `rev` itself:
    /// the order to try them in when looking for the nearest one with an
    /// index.
    pub fn first_parent_ancestors(&self, rev: &str, max: usize) -> Result<Vec<String>> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let walk = self.first_parent_walk(rev, max + 1)?;
        Ok(walk.into_iter().skip(1).collect())
    }

    /// `rev` and its first-parent ancestors, nearest first, at most `count`
    /// commits: the one walk behind both the oldest-first log and the
    /// ancestor search.
    fn first_parent_walk(&self, rev: &str, count: usize) -> Result<Vec<String>> {
        let out = run(
            &self.root,
            &[
                "rev-list",
                "--first-parent",
                &format!("--max-count={count}"),
                rev,
            ],
        )?;
        Ok(out.lines().map(str::to_string).collect())
    }

    /// The best common ancestor of `a` and `b`: what "the changes on this
    /// branch" are measured from when no base is given. Fails when there is
    /// none in this clone, which on CI usually means the history is shallow.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<String> {
        run_line(&self.root, &["merge-base", a, b])
    }

    /// `git diff --name-status -M -z base head`, with renames and copies
    /// expanded into plain adds and deletes.
    pub fn diff(&self, base: &str, head: &str) -> Result<Vec<Change>> {
        let out = run(
            &self.root,
            &["diff", "--name-status", "-M", "-z", base, head],
        )?;
        parse_name_status_z(&out)
    }

    /// Reads a blob at a revision. `None` if the path does not exist there.
    ///
    /// Any git failure reads as `None`, so a revision git cannot resolve
    /// looks the same as an absent path; callers that must tell the two
    /// apart resolve the revision first.
    pub fn read_at(&self, rev: &str, path: &str) -> Option<Vec<u8>> {
        run_bytes(&self.root, &["show", &format!("{rev}:{path}")]).ok()
    }
}

/// Parses the NUL-separated form of `--name-status`.
///
/// Rename and copy records carry two paths (`R100\0old\0new`). A rename
/// becomes a delete of the old path and an add of the new one; a copy
/// leaves its source in place, so it becomes only the add. Either way the
/// dirty-set rules see one shape for every change: a path that appeared,
/// changed or went away.
fn parse_name_status_z(out: &str) -> Result<Vec<Change>> {
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    let mut changes = Vec::new();
    while let Some(status) = fields.next() {
        let code = status.as_bytes()[0];
        match code {
            b'R' | b'C' => {
                let from = fields.next().ok_or(GitError::BadStatus(status.into()))?;
                let to = fields.next().ok_or(GitError::BadStatus(status.into()))?;
                if code == b'R' {
                    changes.push(Change::new(Status::Deleted, from));
                }
                changes.push(Change::new(Status::Added, to));
            }
            _ => {
                let path = fields.next().ok_or(GitError::BadStatus(status.into()))?;
                let s = match code {
                    b'A' => Status::Added,
                    b'M' => Status::Modified,
                    b'D' => Status::Deleted,
                    b'T' => Status::TypeChanged,
                    // U (unmerged), X (unknown), B (broken pairing): be
                    // conservative and treat as a modification.
                    _ => Status::Modified,
                };
                changes.push(Change::new(s, path));
            }
        }
    }
    Ok(changes)
}

/// Runs a git command whose answer is one line, and returns that line.
fn run_line(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(run(dir, args)?.trim().to_string())
}

/// Runs git in `dir` and returns its stdout as text.
pub(crate) fn run(dir: &Path, args: &[&str]) -> Result<String> {
    String::from_utf8(run_bytes(dir, args)?).map_err(|_| GitError::NotUtf8)
}

/// Runs git in `dir` and returns its stdout as it came: the one place this
/// crate starts git, and the form for output that is file content rather
/// than text git wrote.
fn run_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(GitError::Spawn)?;
    if !out.status.success() {
        return Err(GitError::Failed {
            args: args.join(" "),
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testrepo::TwoCommits;

    #[test]
    fn parses_simple_statuses() {
        let out = "M\0a/BUILD\0A\0b/x.cc\0D\0c/y.cc\0";
        assert_eq!(
            parse_name_status_z(out).unwrap(),
            vec![
                Change::new(Status::Modified, "a/BUILD"),
                Change::new(Status::Added, "b/x.cc"),
                Change::new(Status::Deleted, "c/y.cc"),
            ]
        );
    }

    #[test]
    fn expands_renames_into_delete_plus_add() {
        let out = "R100\0old/x.cc\0new/x.cc\0";
        assert_eq!(
            parse_name_status_z(out).unwrap(),
            vec![
                Change::new(Status::Deleted, "old/x.cc"),
                Change::new(Status::Added, "new/x.cc"),
            ]
        );
    }

    #[test]
    fn copies_only_add_the_destination() {
        let out = "C75\0src/x.cc\0dst/x.cc\0";
        assert_eq!(
            parse_name_status_z(out).unwrap(),
            vec![Change::new(Status::Added, "dst/x.cc")]
        );
    }

    #[test]
    fn handles_paths_with_spaces_and_quotes() {
        let out = "M\0a dir/with \"quotes\".cc\0";
        assert_eq!(
            parse_name_status_z(out).unwrap(),
            vec![Change::new(Status::Modified, "a dir/with \"quotes\".cc")]
        );
    }

    #[test]
    fn diff_lists_each_path_that_changed() {
        let f = TwoCommits::new();
        let repo = Repo::new(&f.root);
        assert_eq!(
            repo.diff(&f.first, &f.second).unwrap(),
            vec![
                Change::new(Status::Modified, "lib/a.cc"),
                Change::new(Status::Added, "lib/b.cc"),
            ]
        );
    }

    #[test]
    fn rev_parse_gives_the_commit_id() {
        let f = TwoCommits::new();
        assert_eq!(Repo::new(&f.root).rev_parse("HEAD").unwrap(), f.second);
    }

    #[test]
    fn tree_of_gives_the_tree_not_the_commit() {
        let f = TwoCommits::new();
        let tree = Repo::new(&f.root).tree_of("HEAD").unwrap();
        assert_eq!(tree, f.git(&["rev-parse", "HEAD^{tree}"]));
        assert_ne!(tree, f.second);
    }

    #[test]
    fn first_parent_log_is_oldest_first_and_includes_rev() {
        let f = TwoCommits::new();
        let repo = Repo::new(&f.root);
        assert_eq!(
            repo.first_parent_log("HEAD", 10).unwrap(),
            vec![f.first.clone(), f.second.clone()]
        );
        assert_eq!(repo.first_parent_log("HEAD", 1).unwrap(), vec![f.second]);
    }

    #[test]
    fn first_parent_log_meta_carries_what_a_row_prints() {
        let f = TwoCommits::new();
        let meta = Repo::new(&f.root)
            .first_parent_log_meta("HEAD", 10)
            .unwrap();
        assert_eq!(
            meta.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(),
            vec!["one", "two"]
        );
        assert_eq!(meta[0].sha, f.first);
        assert_eq!(meta[1].sha, f.second);
        assert_eq!(meta[0].date.len(), "MM-DD HH:MM".len(), "{:?}", meta[0]);
    }

    #[test]
    fn first_parent_ancestors_start_at_the_parent() {
        let f = TwoCommits::new();
        assert_eq!(
            Repo::new(&f.root)
                .first_parent_ancestors("HEAD", 5)
                .unwrap(),
            vec![f.first]
        );
    }

    #[test]
    fn parents_lists_the_parent_commit() {
        let f = TwoCommits::new();
        assert_eq!(Repo::new(&f.root).parents("HEAD").unwrap(), vec![f.first]);
    }

    #[test]
    fn read_at_reads_a_file_as_it_was_and_misses_an_absent_one() {
        let f = TwoCommits::new();
        let repo = Repo::new(&f.root);
        assert_eq!(repo.read_at(&f.first, "lib/a.cc").unwrap(), b"one");
        assert!(repo.read_at(&f.first, "lib/b.cc").is_none());
    }
}
