//! cambium as a [`Tool`].
//!
//! In a comparison cambium is one of three tools, not the thing being served,
//! and this is where that is made literal: the same trait bazel-diff and Tango
//! implement, answering the same question from the same checkout under the
//! same Bazel.
//!
//! It has one thing the other two do not, which is why [`Cambium::run`] exists
//! alongside the trait method. cambium's answer carries more than a label set
//! — the head index the classifier needs, the `--explain` reasons, the dirty
//! package count, whether the change went global — and the change-set driver
//! ([`crate::compare`]) consumes all of it. `Tool::affected` is that same run
//! with everything but the labels and the two wall times thrown away, which is
//! exactly what a foil is compared on.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use cambium_engine::{AffectedRequest, Engine, RunOutcome};

use crate::tools::{RunContext, Tool, ToolRun};

/// The name cambium is recorded under, in `--tools` and in every result file.
pub const NAME: &str = "cambium";

/// cambium, driven through an [`Engine`] already configured for the subject.
pub struct Cambium<'a> {
    pub engine: &'a Engine,
}

/// One `(base, head)` run of cambium, cold then warm.
pub struct CambiumRunOutput {
    /// The warm run's output: the report, and the head index when there is one.
    pub out: RunOutcome,
    /// Wall time with the base index not in the store — it is built with one
    /// `bazel query //...`.
    pub cold: Duration,
    /// Wall time with the base index in the store: only the changed packages
    /// are queried. This is what a CI job actually pays.
    pub warm: Duration,
}

impl<'a> Cambium<'a> {
    pub fn new(engine: &'a Engine) -> Self {
        Self { engine }
    }

    /// Runs `cambi affected` twice over the same revisions.
    ///
    /// The first run publishes whatever base index it had to compute, so the
    /// second finds it in the store: that is precisely the difference the two
    /// numbers are meant to show.
    pub fn run(&self, base: &str, head: &str) -> Result<CambiumRunOutput> {
        let req = AffectedRequest {
            include_manual: true,
            materialize: true,
            publish_base: true,
            // The classifier needs the per-target reason to explain the
            // largest cambium-side class on a deletion commit
            // (`cambium_extra_deleted`).
            explain: true,
            ..AffectedRequest::new(base.to_string(), head.to_string())
        };
        let t = Instant::now();
        let first = self
            .engine
            .affected(&req)
            .with_context(|| format!("cambium affected {base}..{head}"))?;
        let cold = t.elapsed();
        let t = Instant::now();
        let out = self.engine.affected(&req)?;
        let warm = t.elapsed();
        drop(first);
        Ok(CambiumRunOutput { out, cold, warm })
    }
}

impl Tool for Cambium<'_> {
    fn name(&self) -> &str {
        NAME
    }

    fn version(&self) -> String {
        format!("cambium {}", env!("CARGO_PKG_VERSION"))
    }

    /// `ws` and the base worktree are ignored: the engine already knows which
    /// checkout it is pointed at, and moving it is the driver's job.
    fn affected(&self, _ws: &Path, base: &str, head: &str, _ctx: &RunContext) -> Result<ToolRun> {
        let r = self.run(base, head)?;
        let labels: BTreeSet<String> = r.out.report.affected_labels().into_iter().collect();
        Ok(ToolRun {
            labels,
            cold_wall: r.cold,
            warm_wall: Some(r.warm),
            depths: Some(
                r.out
                    .report
                    .affected
                    .iter()
                    .map(|t| (t.label.clone(), t.depth))
                    .collect(),
            ),
            ..ToolRun::default()
        })
    }
}
