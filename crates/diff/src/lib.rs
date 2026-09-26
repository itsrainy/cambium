//! Git plumbing: what changed between two revisions, and which ancestors
//! to try when looking for one that has an index.
//!
//! We shell out to `git` rather than linking a library: the operations are few,
//! the output formats are stable, and it keeps cambium honest about running in
//! whatever checkout CI produced (worktrees, shallow clones, alternates).

use std::path::{Path, PathBuf};
use std::process::Command;

pub mod worktree;

pub use worktree::Worktree;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("failed to run git: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("git {args} failed with {status}: {stderr}")]
    Failed {
        args: String,
        status: String,
        stderr: String,
    },
    #[error("git output was not valid UTF-8")]
    NotUtf8,
    #[error("unrecognised diff status {0:?}")]
    BadStatus(String),
}

type Result<T> = std::result::Result<T, GitError>;

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

/// A commit as a reader would see it in a row: the object id, the commit
/// date in `MM-DD HH:MM`, and the first line of the message.
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
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Finds the repository root containing `start`.
    pub fn discover(start: &Path) -> Result<Self> {
        let out = run(start, &["rev-parse", "--show-toplevel"])?;
        Ok(Self::new(PathBuf::from(out.trim())))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a revision to a full object id.
    pub fn rev_parse(&self, rev: &str) -> Result<String> {
        Ok(run(
            &self.root,
            &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
        )?
        .trim()
        .to_string())
    }

    /// The tree object a revision points at.
    ///
    /// Two commits with identical content have identical trees whatever their
    /// shas, which is what lets the index store key indexes by tree: a
    /// rebased or squash-merged commit finds the index built for its
    /// pre-rebase twin.
    pub fn tree_of(&self, rev: &str) -> Result<String> {
        Ok(run(
            &self.root,
            &["rev-parse", "--verify", &format!("{rev}^{{tree}}")],
        )?
        .trim()
        .to_string())
    }

    /// First-parent history of `rev`, oldest first, at most `max` commits
    /// (including `rev`).
    pub fn first_parent_log(&self, rev: &str, max: usize) -> Result<Vec<String>> {
        let out = run(
            &self.root,
            &[
                "rev-list",
                "--first-parent",
                &format!("--max-count={max}"),
                rev,
            ],
        )?;
        let mut v: Vec<String> = out.lines().map(|s| s.to_string()).collect();
        v.reverse();
        Ok(v)
    }

    /// The same history with the date and subject of each commit, for a
    /// caller that reports a commit to a person rather than only walking it.
    ///
    /// One `git log` rather than two calls per commit: a thirty-commit
    /// profile would otherwise pay sixty process starts to print sixty
    /// fields. The date is the commit date in the local zone, `MM-DD HH:MM`,
    /// which is the precision a row has space for.
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
    /// checkout is meant to see the edits. It only means the tree is not
    /// exactly `HEAD`, which the engine takes into account when it decides
    /// whether a revision can be read from the tree it already has.
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
        let out = run(
            &self.root,
            &[
                "rev-list",
                "--first-parent",
                &format!("--max-count={}", max + 1),
                rev,
            ],
        )?;
        Ok(out.lines().skip(1).map(|s| s.to_string()).collect())
    }

    pub fn merge_base(&self, a: &str, b: &str) -> Result<String> {
        Ok(run(&self.root, &["merge-base", a, b])?.trim().to_string())
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
    pub fn read_at(&self, rev: &str, path: &str) -> Option<Vec<u8>> {
        let out = Command::new("git")
            .current_dir(&self.root)
            .args(["show", &format!("{rev}:{path}")])
            .output()
            .ok()?;
        out.status.success().then_some(out.stdout)
    }
}

/// Parses the NUL-separated form of `--name-status`.
///
/// Rename and copy records carry two paths (`R100\0old\0new`). A rename
/// becomes a delete of the old path and an add of the new one; a copy
/// leaves its source in place, so it becomes only the add. Either way the
/// dirty-set rules see one shape for every change: a path that appeared,
/// changed or went away.
pub fn parse_name_status_z(out: &str) -> Result<Vec<Change>> {
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

fn run(dir: &Path, args: &[&str]) -> Result<String> {
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
    String::from_utf8(out.stdout).map_err(|_| GitError::NotUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn drives_a_real_repository() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let st = Command::new("git")
                .current_dir(root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                st.status.success(),
                "git {args:?}: {:?}",
                String::from_utf8_lossy(&st.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::write(root.join("lib/a.cc"), "one").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "one"]);
        std::fs::write(root.join("lib/a.cc"), "two").unwrap();
        std::fs::write(root.join("lib/b.cc"), "new").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "two"]);

        let repo = Repo::new(root);
        let changes = repo.diff("HEAD~1", "HEAD").unwrap();
        assert_eq!(
            changes,
            vec![
                Change::new(Status::Modified, "lib/a.cc"),
                Change::new(Status::Added, "lib/b.cc"),
            ]
        );
        let head = repo.rev_parse("HEAD").unwrap();
        assert_eq!(head.len(), 40);
        let anc = repo.first_parent_ancestors("HEAD", 5).unwrap();
        assert_eq!(anc.len(), 1);
        let tree = repo.tree_of("HEAD").unwrap();
        assert_eq!(tree.len(), 40);
        assert_ne!(tree, head);
        assert_eq!(repo.first_parent_log("HEAD", 10).unwrap().len(), 2);
        // Oldest first, like the log it mirrors, and carrying what a row
        // prints about a commit.
        let meta = repo.first_parent_log_meta("HEAD", 10).unwrap();
        assert_eq!(
            meta.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(),
            vec!["one", "two"]
        );
        assert_eq!(meta[1].sha, head);
        assert_eq!(meta[0].date.len(), "MM-DD HH:MM".len(), "{:?}", meta[0]);
        assert_eq!(repo.parents("HEAD").unwrap(), vec![anc[0].clone()]);
        assert_eq!(repo.read_at("HEAD~1", "lib/a.cc").unwrap(), b"one");
        assert!(repo.read_at("HEAD~1", "lib/b.cc").is_none());
    }
}
