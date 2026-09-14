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
use crate::classify::BazelFacts;
use crate::oracles::rdeps::{OracleInputs, RdepsOracle, RdepsResult};
use crate::tools::{BaseWorktree, Tool, ToolRun};

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
        let changed = BazelFacts::changed_paths(&self.checkout.dir, &ctx.step.parent, &ctx.step.head)?;
        let base = self.base_index(&ctx.step.parent);
        // The dirty packages are cambium's own; the oracle only reads them to
        // credit a deleted file's package. An empty set is honest here.
        let dirty: BTreeSet<String> = BTreeSet::new();
        let out = RdepsOracle::new(self.oracle_k, self.seed).run(
            self.engine.bazel(),
            &OracleInputs {
                head: index.view(),
                base: base.as_ref().map(|f| f.view()),
                changed: &changed,
                cambium: ctx.cambium_set,
                dirty_packages: &dirty,
            },
        )?;
        Ok(Some(out))
    }

    fn peers(&self, ctx: &StepContext<'_>, oracle: Option<&RdepsResult>) -> Vec<ToolStep> {
        if !ctx.want_peers || self.tools.is_empty() {
            return Vec::new();
        }
        let base_wt = match BaseWorktree::create(&self.checkout.dir, &ctx.step.parent, self.scratch) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "no base worktree; peers skipped this step");
                return self.tools.iter().map(|p| failed(p, &e)).collect();
            }
        };
        let ctx_run = crate::compare::run_context(self.engine, self.scratch, base_wt.path());
        let index = ctx.head_index.and_then(|p| IndexFile::open(p).ok());
        let labels_dir =
            crate::labels::change_set_dir(self.results_dir, &ctx.step.parent, &ctx.step.head);
        let _ = std::fs::create_dir_all(&labels_dir);

        self.tools
            .iter()
            .map(|p| {
                match p.affected(&self.checkout.dir, &ctx.step.parent, &ctx.step.head, &ctx_run) {
                    Ok(mut run) => {
                        if let Some(i) = &index {
                            run.restrict_to_rules(i.view());
                        }
                        let _ = crate::labels::write_set(&labels_dir, p.name(), &run.labels);
                        step_of(p, &run, oracle)
                    }
                    Err(e) => {
                        tracing::warn!(tool = p.name(), error = format!("{e:#}"), "peer failed on this step");
                        failed(p, &e)
                    }
                }
            })
            .collect()
    }
}

fn step_of(p: &&dyn Tool, run: &ToolRun, oracle: Option<&RdepsResult>) -> ToolStep {
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

fn failed(p: &&dyn Tool, _e: &anyhow::Error) -> ToolStep {
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
