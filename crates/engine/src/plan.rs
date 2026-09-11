//! What a run would do, worked out before it does it.
//!
//! `cambi affected --dry-run` is the one line a developer wants before a
//! two-minute wait: which index would be loaded, what the diff classifies as,
//! and whether the answer would be a splice, a cold rebuild or `//...`.
//!
//! Everything here runs against git and the store and never against Bazel. The
//! stages are the run's own — the position probe, the bazelrc screen, the
//! external-repository analysis, the classifier's dirty set — so the plan
//! cannot say one thing and the run then do another. What genuinely needs
//! Bazel is not guessed at: it is reported as unknown until queried.

use cambium_classify::{Matchers, PackageSet};
use serde::{Deserialize, Serialize};

use crate::diag::Diagnostic;
use crate::engine::{AffectedRequest, Engine};
use crate::error::Result;
use crate::probe;

/// What the run would end up doing.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanMode {
    /// An index is in reach and the dirty packages are few enough to re-query.
    Splice,
    /// An index is in reach, but the dirty set is over
    /// `limits.max_dirty_packages`, so the run re-queries `//...` and splices
    /// all of it. Still exact; only the cost differs.
    ColdRebuild,
    /// Everything is affected, and the run knows it already.
    Global,
    /// No usable index anywhere in the store: `bazel query //...` from cold.
    ColdBuild,
}

impl std::fmt::Display for PlanMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PlanMode::Splice => "splice",
            PlanMode::ColdRebuild => "cold rebuild",
            PlanMode::Global => "global",
            PlanMode::ColdBuild => "cold build",
        })
    }
}

/// The index the run would load.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannedIndex {
    /// The revision it was built for.
    pub revision: String,
    pub lineage: String,
    /// How far back from the run's base it is, in first-parent commits.
    pub distance: Option<usize>,
    /// Seconds since it was built, when the header records a time.
    pub age_seconds: Option<u64>,
    pub packages: u32,
    pub targets: u32,
}

/// The whole plan, as `--dry-run` prints it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunPlan {
    pub base: String,
    pub head: String,
    /// Absent means a cold build: nothing in the store is usable here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<PlannedIndex>,
    pub changed_files: usize,
    /// `None` is "unknown until queried": without an index there is nothing to
    /// classify the diff against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty_packages: Option<usize>,
    /// Set when the plan can already tell that the answer is `//...`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_reason: Option<String>,
    pub mode: PlanMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
}

/// A row source that has no rows: the plan classifies a diff and never queries.
///
/// The `.bzl` stage takes a querier so it can witness a file the stored closure
/// has no node for against a checkout; there is none here, and the stage
/// already knows how to say so (`stats.bzl_witness_unavailable`) rather than
/// guess.
struct NoQuerier;

impl crate::query::PackageQuerier for NoQuerier {
    fn query(&self, _packages: &[String]) -> Result<crate::query::Rows> {
        Err(crate::error::EngineError::BazelQuery {
            what: "a dry run makes no query".into(),
            code: None,
            stderr: String::new(),
        })
    }

    fn universe(&self) -> Result<crate::query::Rows> {
        self.query(&[])
    }
}

impl Engine {
    /// What `affected` would do for this request, without doing it.
    pub fn plan(&self, req: &AffectedRequest) -> Result<RunPlan> {
        let base = self
            .repo
            .rev_parse(&req.base)
            .unwrap_or_else(|_| req.base.clone());
        let head = self
            .repo
            .rev_parse(&req.head)
            .unwrap_or_else(|_| req.head.clone());
        let req = AffectedRequest {
            base: base.clone(),
            head: head.clone(),
            ..req.clone()
        };

        // The index a run would load is the one for the *base* of the range,
        // not for wherever the working tree is sitting.
        let position = probe::position_at(
            &self.repo,
            &base,
            self.store.as_deref(),
            self.cache_dir(),
            self.cfg.hash(),
            self.cfg.baseline.max_ancestor_walk,
        );
        let mut diagnostics = position.diagnostics.clone();

        let changes = self.repo.diff(&base, &head)?;
        let mut plan = RunPlan {
            base,
            head,
            index: None,
            changed_files: changes.len(),
            dirty_packages: None,
            global_reason: None,
            mode: PlanMode::ColdBuild,
            diagnostics: Vec::new(),
        };

        // The bazelrc screen needs no index: it reads the two texts out of git
        // and decides whether the edit can reach loading at all.
        let screened = self.screen_bazelrc(&changes, &req);
        plan.global_reason = screened.global_reason;

        let Some(path) = position.index_path.as_deref() else {
            // Nothing to splice onto, so the run indexes the repository and
            // the diff's classification is beside the point.
            plan.diagnostics = drained(&self.diagnostics, diagnostics);
            return Ok(plan);
        };
        let (probe_index, index_diagnostics) = probe::index_at(path);
        diagnostics.extend(index_diagnostics);
        plan.index = probe_index.map(|i| PlannedIndex {
            revision: i.commit,
            lineage: i.lineage,
            distance: position.distance,
            age_seconds: (i.built_at_unix > 0).then(|| now_unix().saturating_sub(i.built_at_unix)),
            packages: i.packages,
            targets: i.targets,
        });
        if plan.index.is_none() {
            plan.diagnostics = drained(&self.diagnostics, diagnostics);
            return Ok(plan);
        }
        if plan.global_reason.is_some() {
            plan.mode = PlanMode::Global;
            plan.diagnostics = drained(&self.diagnostics, diagnostics);
            return Ok(plan);
        }

        let file = cambium_index::IndexFile::open(path)?;
        let view = file.view();

        // The same two stages the run puts in front of the classifier, in the
        // same order: external repositories first, so a diff they account for
        // does not also fire the `MODULE.bazel` invalidator.
        let mut ext = if self.cfg.externals.enabled {
            crate::externals::analyse(
                view,
                &changes,
                &req.base,
                &req.head,
                &crate::externals::GitText(&self.repo),
            )
        } else {
            crate::externals::Externals::default()
        };
        let mut matchers = Matchers::compile(&self.cfg.rules())?;
        matchers.exempt(ext.handled.clone());
        let mut packages = PackageSet::from_index(view);
        let mut dirty = cambium_classify::dirty_set(
            &screened.changes,
            &mut packages,
            &matchers,
            &cambium_classify::IndexConsumers(view),
        );
        let mut stats = crate::report::Stats::default();
        self.resolve_changed_bzl(view, &NoQuerier, &mut dirty, &mut ext, &mut stats);
        if let Some(reason) = ext.global.take() {
            dirty.global = true;
            dirty.global_reason = Some(reason);
        }

        plan.dirty_packages = Some(dirty.dirty.len());
        if dirty.global {
            plan.mode = PlanMode::Global;
            plan.global_reason = dirty.global_reason;
        } else {
            let limit = self.cfg.limits.max_dirty_packages;
            plan.mode = if limit > 0 && dirty.dirty.len() > limit {
                PlanMode::ColdRebuild
            } else {
                PlanMode::Splice
            };
        }
        plan.diagnostics = drained(&self.diagnostics, diagnostics);
        Ok(plan)
    }
}

/// The probes' diagnostics plus whatever the stages emitted into the engine's
/// own sink, each one once.
fn drained(sink: &crate::diag::Diagnostics, mut have: Vec<Diagnostic>) -> Vec<Diagnostic> {
    for d in sink.drain() {
        if !have.contains(&d) {
            have.push(d);
        }
    }
    have
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
