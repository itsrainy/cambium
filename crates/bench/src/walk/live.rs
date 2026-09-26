//! The live checks for a real subject: the `rdeps` oracle and the peers,
//! over a Bazel and a checkout.
//!
//! This is the half of a walk that the synthetic test cannot run. For one
//! step it asks Bazel the reverse-dependency question the oracle is built on,
//! then runs each peer over the same `(parent, head)` pair and scores it
//! against that oracle for recall and tightness. cambium is scored by the
//! driver from the same oracle; the peers survive here only as the
//! explanation layer, their label sets written beside the step for the
//! `labels` verb.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;
use cambium_engine::Engine;
use cambium_index::IndexFile;

use crate::checkout::Checkout;
use crate::oracles::rdeps::{OracleInputs, RdepsOracle, RdepsResult};
use crate::tools::{BaseWorktree, RunContext, Tool, ToolRun};
use crate::util::absolute;

use super::driver::{LiveChecks, StepContext};
use super::model::ToolStep;
use super::score;

/// The live checks over a real subject's Bazel and checkout.
pub struct BazelChecks<'a> {
    pub engine: &'a Engine,
    pub checkout: &'a Checkout,
    /// The peers, cambium excluded: cambium is the thing being measured.
    pub tools: &'a [&'a dyn Tool],
    /// Where the labels and the peers' scratch go.
    pub results_dir: &'a Path,
    pub scratch: &'a Path,
    /// The rdeps oracle's per-change-set root cap; 0 is the full oracle.
    pub oracle_k: usize,
    pub seed: u64,
}

impl BazelChecks<'_> {
    /// The base index for the oracle's deleted half, or `None`.
    fn base_index(&self, base: &str) -> Option<IndexFile> {
        let rev = self
            .engine
            .repo()
            .rev_parse(base)
            .unwrap_or_else(|_| base.to_string());
        let path = self.engine.fetch_baseline(&rev).ok().flatten()?;
        IndexFile::open(&path).ok()
    }
}

impl LiveChecks for BazelChecks<'_> {
    fn oracle(&self, ctx: &StepContext<'_>) -> Result<Option<RdepsResult>> {
        if !ctx.want_oracle {
            return Ok(None);
        }
        // A global step has no head index and cambium answered `//...`; the
        // oracle would be vacuous (every rule is inside "everything") and has
        // nothing to seed from.
        let Some(index_path) = ctx.head_index else {
            return Ok(None);
        };
        let index = IndexFile::open(index_path)?;
        let (dir, parent, head) = (&self.checkout.dir, &ctx.step.parent, &ctx.step.head);
        let changed = changed_paths(dir, parent, head, None)?;
        let base = self.base_index(parent);
        // For the marker files' directory listings (renames count as both,
        // since `diff` without `-M` reports them that way); without it a
        // listing is matched by any changed child, which only widens the
        // oracle.
        let added_or_deleted = changed_paths(dir, parent, head, Some("AD")).ok();
        let output_base = crate::oracles::repo_inputs::output_base(self.engine.bazel());
        let out = RdepsOracle::new(self.oracle_k, self.seed).run(
            self.engine.bazel(),
            &OracleInputs {
                head: index.view(),
                base: base.as_ref().map(|f| f.view()),
                changed: &changed,
                cambium: ctx.cambium_set,
                // cambium's own dirty set: the only spelling a wholly-deleted
                // package has, since every rule in it is tombstoned.
                dirty_packages: ctx.dirty_packages,
                output_base: output_base.as_deref(),
                added_or_deleted: added_or_deleted.as_ref(),
            },
        )?;
        Ok(Some(out))
    }

    fn peers(&self, ctx: &StepContext<'_>, oracle: Option<&RdepsResult>) -> Vec<ToolStep> {
        if !ctx.want_peers || self.tools.is_empty() {
            return Vec::new();
        }
        let base_wt = match BaseWorktree::create(&self.checkout.dir, &ctx.step.parent, self.scratch)
        {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "no base worktree; peers skipped this step"
                );
                return self.tools.iter().map(|p| failed(*p)).collect();
            }
        };
        let ctx_run = run_context(self.engine, self.scratch, base_wt.path());
        let index = ctx.head_index.and_then(|p| IndexFile::open(p).ok());
        let labels_dir =
            crate::labels::change_set_dir(self.results_dir, &ctx.step.parent, &ctx.step.head);
        let _ = std::fs::create_dir_all(&labels_dir);

        self.tools
            .iter()
            .map(|p| {
                match p.affected(
                    &self.checkout.dir,
                    &ctx.step.parent,
                    &ctx.step.head,
                    &ctx_run,
                ) {
                    Ok(mut run) => {
                        if let Some(i) = &index {
                            run.restrict_to_rules(i.view());
                        }
                        let _ = crate::labels::write_set(&labels_dir, p.name(), &run.labels);
                        step_of(*p, &run, oracle)
                    }
                    Err(e) => {
                        tracing::warn!(
                            tool = p.name(),
                            error = format!("{e:#}"),
                            "peer failed on this step"
                        );
                        failed(*p)
                    }
                }
            })
            .collect()
    }
}

/// `git diff --no-renames --name-only [--diff-filter=<filter>] base..head` in
/// `repo`.
///
/// Without `--no-renames` git's default rename detection lists only the new
/// path of a moved file, and the old one reaches neither the deleted half of
/// the oracle nor a marker that recorded it.
fn changed_paths(
    repo: &Path,
    base: &str,
    head: &str,
    filter: Option<&str>,
) -> Result<BTreeSet<String>> {
    let filter = filter.map(|f| format!("--diff-filter={f}"));
    let range = format!("{base}..{head}");
    let mut args = vec!["diff", "--no-renames", "--name-only"];
    args.extend(filter.as_deref());
    args.push(&range);
    let out = crate::tools::git(repo, &args)?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

fn step_of(p: &dyn Tool, run: &ToolRun, oracle: Option<&RdepsResult>) -> ToolStep {
    let (recall, tightness) = score(&run.labels, oracle);
    ToolStep {
        name: p.name().to_string(),
        version: p.version(),
        labels: run.labels.len(),
        wall_ms: run.cold_wall.as_millis() as u64,
        recall_vs_oracle: recall,
        tightness_vs_oracle: tightness,
        failed: false,
    }
}

fn failed(p: &dyn Tool) -> ToolStep {
    ToolStep {
        name: p.name().to_string(),
        version: p.version(),
        labels: 0,
        wall_ms: 0,
        recall_vs_oracle: None,
        tightness_vs_oracle: None,
        failed: true,
    }
}

/// The run context a peer is handed for one step: this run's Bazel, its
/// startup flags and environment, the scratch directory, and the base
/// worktree the peer diffs against.
pub fn run_context(engine: &Engine, scratch: &Path, base_worktree: &Path) -> RunContext {
    RunContext {
        bazel_binary: engine.bazel().binary().to_path_buf(),
        startup_flags: engine.bazel().startup_flags().to_vec(),
        env: engine.bazel().env().to_vec(),
        scratch: absolute(scratch),
        base_worktree: absolute(base_worktree),
    }
}

#[cfg(test)]
mod tests {
    use cambium_testkit::{commit_all, init_repo, write};

    use super::*;

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// A rename reaches the oracle as both of its paths, and only an add or a
    /// delete reaches the directory-listing half.
    #[test]
    fn a_rename_is_changed_at_both_paths_and_edits_are_not_added_or_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        write(repo, "a/old.txt", "moved\n");
        write(repo, "b/kept.txt", "one\n");
        let base = commit_all(repo, "base");
        std::fs::rename(repo.join("a/old.txt"), repo.join("a/new.txt")).unwrap();
        write(repo, "b/kept.txt", "two\n");
        let head = commit_all(repo, "head");

        assert_eq!(
            changed_paths(repo, &base, &head, None).unwrap(),
            set(&["a/new.txt", "a/old.txt", "b/kept.txt"])
        );
        assert_eq!(
            changed_paths(repo, &base, &head, Some("AD")).unwrap(),
            set(&["a/new.txt", "a/old.txt"])
        );
    }
}
