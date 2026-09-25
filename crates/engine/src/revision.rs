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
//! its own `Bazel` handle pointed at it, removed when the run is done. Three
//! details make it safe and affordable:
//!
//! - **Its own output base.** Sharing one across workspace directories poisons
//!   the external repository tree: repository rules write absolute paths into
//!   it (symlinks back into the workspace that fetched them), the fetch marker
//!   does not mention the workspace, so Bazel never refetches — and once the
//!   first workspace is deleted every one of those symlinks dangles, failing
//!   *every* later query with a repository-mapping error. See
//!   [`derive_startup_flags`].
//! - **Its server stops with it.** Each output base is a Bazel server of its
//!   own, holding a gigabyte or more, and Bazel keeps an idle one for three
//!   hours. A process that answers several revisions — the benchmark's
//!   change-set workload, one `cambi` per change set — left one server per
//!   revision behind, and forty envoy change sets were enough to run a 32 GB
//!   machine out of memory. [`RevisionQuerier`] shuts its server down when it
//!   drops; the output base itself, and what was fetched into it, stays.
//! - **Nothing is done when nothing needs doing.** `baseline update` and
//!   the history workload checks each commit out before calling the engine, so their
//!   revision is the one on disk and they must keep paying nothing: no
//!   worktree, no second Bazel server. [`Engine::needs_revision_workspace`]
//!   compares trees and says no.

use std::path::{Path, PathBuf};

use cambium_bazel::Bazel;
use cambium_diff::Worktree;

use crate::config::CONFIG_FILE;
use crate::error::{EngineError, Result};
use crate::query::{BazelQuerier, PackageQuerier, Rows};

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

    /// Moves the checkout to another revision. The Bazel handle from
    /// [`Self::bazel`] keeps working: it is the directory that names the
    /// server, not the commit in it.
    pub fn checkout(&self, sha: &str) -> Result<()> {
        Ok(self.worktree.checkout(sha)?)
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
/// the same revision finds its external repositories already fetched.
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
/// config is routinely untracked (the benchmark generates it into the subject
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
/// removes the directory only once nothing is querying it any more, and the
/// `Drop` impl stops the worktree's Bazel server before either goes.
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

impl Drop for RevisionQuerier {
    fn drop(&mut self) {
        // Only a worktree's server is ours to stop. Without one, the querier
        // runs in the caller's own workspace, whose server the caller (or the
        // user's next build) is still using.
        if self._workspace.is_some() {
            self.querier.bazel.shutdown();
        }
    }
}

impl PackageQuerier for RevisionQuerier {
    fn query(&self, packages: &[String]) -> std::result::Result<Rows, EngineError> {
        self.querier.query(packages)
    }

    fn universe(&self) -> std::result::Result<Rows, EngineError> {
        self.querier.universe()
    }

    /// Delegated, not defaulted. The default in
    /// [`crate::query::PackageQuerier`] answers `universe()`, which for a
    /// wrapper around a real Bazel silently drops the repositories and sends
    /// the query out as `//...:*` alone: the widened universe is written into
    /// the index's `repo_indexed` column while none of its rows are there.
    fn universe_with(&self, repos: &[String]) -> std::result::Result<Rows, EngineError> {
        self.querier.universe_with(repos)
    }

    fn load_source(&self) -> Option<&dyn cambium_index::FileText> {
        self.querier.load_source()
    }

    fn repo_mapping(&self) -> cambium_index::RepoMapping {
        self.querier.repo_mapping()
    }

    fn version(&self) -> String {
        self.querier.version()
    }

    fn workspace(&self) -> Option<&Path> {
        self.querier.workspace()
    }

    fn external_rules(&self) -> Option<String> {
        self.querier.external_rules()
    }

    fn probe_repos(&self, repos: &[String]) -> Vec<cambium_bazel::RepoProbe> {
        self.querier.probe_repos(repos)
    }

    /// Delegated for the same reason `universe_with` is: the default answers
    /// an empty table, and a cold build through this wrapper (every one the
    /// CLI and the bench make) then stored no consumer for any indexed
    /// repository while the querier underneath could have named them.
    fn consumer_repos(
        &self,
        indexed: &[String],
    ) -> std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    > {
        self.querier.consumer_repos(indexed)
    }

    /// Delegated, or every consumer's leaf map reads "not known" and every
    /// leaf is seeded, which is the fault class `consumer_repos` had: a
    /// silent default answering for the querier underneath.
    fn consumer_leaf_packages(
        &self,
        repo: &str,
        leaves: &[String],
    ) -> Option<
        std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    > {
        self.querier.consumer_leaf_packages(repo, leaves)
    }

    fn can_dump_loading_layer(&self) -> bool {
        self.querier.can_dump_loading_layer()
    }

    fn dump_loading_layer(
        &self,
        filter: &str,
    ) -> std::result::Result<(String, cambium_index::sky::Paths), EngineError> {
        self.querier.dump_loading_layer(filter)
    }

    fn reload_packages(&self, repos: &[String]) -> bool {
        self.querier.reload_packages(repos)
    }

    fn reload_named(&self, packages: &[String]) -> bool {
        self.querier.reload_named(packages)
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

    /// Records the subcommand of every Bazel invocation, in order.
    #[derive(Debug, Default)]
    struct Recorder(std::sync::Mutex<Vec<String>>);

    impl cambium_bazel::BazelRunner for Recorder {
        fn stream(
            &self,
            _cmd: &mut std::process::Command,
            _on_stdout: &mut dyn FnMut(&mut dyn std::io::Read),
            _on_stderr: &mut (dyn FnMut(&str) + Send),
        ) -> std::result::Result<cambium_bazel::Captured, cambium_bazel::BazelError> {
            unreachable!("nothing is queried")
        }

        fn output(
            &self,
            cmd: &mut std::process::Command,
        ) -> std::io::Result<cambium_bazel::Captured> {
            let args: Vec<String> = cmd
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            self.0.lock().unwrap().push(args.join(" "));
            Ok(cambium_bazel::Captured {
                exit: cambium_bazel::Exit {
                    code: Some(0),
                    success: true,
                },
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    fn querier_with(
        recorder: &std::sync::Arc<Recorder>,
        ws: Option<RevisionWorkspace>,
    ) -> RevisionQuerier {
        let dir = ws
            .as_ref()
            .map(|w| w.path().to_path_buf())
            .unwrap_or_else(std::env::temp_dir);
        let bazel = Bazel::at(dir, Some("bazel"))
            .with_startup_flags(vec!["--output_base=/fast/ob-0123456789ab".into()])
            .with_runner(recorder.clone());
        RevisionQuerier::new(
            BazelQuerier {
                bazel,
                opts: cambium_bazel::ConvertOptions::default(),
                files: crate::query::no_files(),
                diags: crate::diag::Diagnostics::new(),
            },
            ws,
        )
    }

    /// Every consumer-table question reaches the Bazel underneath. The
    /// trait's defaults answer an empty table and no leaf map, and a wrapper
    /// that leaves either to the default stores exactly that while the querier
    /// it wraps could have answered: `consumer_repos` did once (9858de7), and
    /// every cold build the CLI and the bench made had no table.
    #[test]
    fn the_consumer_questions_are_delegated() {
        let recorder = std::sync::Arc::new(Recorder::default());
        let q = querier_with(&recorder, None);
        let _ = q.consumer_repos(&["envoy_api".to_string()]);
        let leaves = q.consumer_leaf_packages("envoy_api", &["@grpc//:grpc++".to_string()]);
        // An empty graph is an answer, and it knows no leaf; the default
        // would have been no answer at all.
        assert_eq!(leaves, Some(std::collections::BTreeMap::new()));
        let asked = recorder.0.lock().unwrap().clone();
        assert!(
            asked.iter().any(|a| a.contains("query") && a.contains("rdeps(//...")),
            "{asked:?}"
        );
        assert!(
            asked
                .iter()
                .any(|a| a.contains("allpaths(set(\"@grpc//:grpc++\"), @envoy_api//...:*)")),
            "{asked:?}"
        );
    }

    /// A worktree's server is stopped when its querier goes, against the
    /// worktree's own output base; the caller's server is left alone.
    #[test]
    fn a_worktree_querier_stops_its_bazel_server_and_no_other() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        cambium_testkit::init_repo(&root);
        cambium_testkit::write(&root, "lib/BUILD", "a\n");
        let sha = cambium_testkit::commit_all(&root, "base");
        let repo = cambium_diff::Repo::new(&root);

        let recorder = std::sync::Arc::new(Recorder::default());
        let ws = RevisionWorkspace::create(&repo, &sha, &dir.path().join("worktrees")).unwrap();
        drop(querier_with(&recorder, Some(ws)));
        assert_eq!(
            *recorder.0.lock().unwrap(),
            ["--output_base=/fast/ob-0123456789ab shutdown"]
        );

        let recorder = std::sync::Arc::new(Recorder::default());
        drop(querier_with(&recorder, None));
        assert!(recorder.0.lock().unwrap().is_empty());
    }
}
