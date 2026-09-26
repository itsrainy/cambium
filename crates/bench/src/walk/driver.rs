//! Running one walk: the cold build, the forward steps, the oracle and peers
//! where the schedule calls for them, and the rebuild oracle at the pin.
//!
//! The cambium side is driven through [`crate::walk::Driver`], the same
//! trait the offline synthetic test uses, so the walk's loop — resume, the
//! carried index, the per-step record — is exercised with no Bazel and no
//! product binary. The live checks (the `rdeps` oracle and the peers) need a
//! real Bazel and a checkout, so they are a separate [`LiveChecks`] the real
//! subject supplies and the synthetic test leaves out.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context as _, Result};
use cambium_engine::Engine;
use cambium_index::IndexFile;

use super::cambi::{cold_build, Driver};
use crate::oracles::rdeps::RdepsResult;
use crate::oracles::rebuild::{compare_by_label_with_sky, GraphDiff, SkyDiff};
use crate::state::{HistoryState, State};
use crate::util::now_unix;

use super::model::{
    CambiumAnswer, IndexCounts, RebuildOracle, StepRecord, StepTimings, ToolStep, WalkResult,
    WalkSummary,
};
use super::rebuild_rows::{self, RebuildRows};
use super::sampling::Sample;
use super::steps::Step;
use super::tier::Schedule;

/// What the live checks read for one step, once cambium has answered it.
pub struct StepContext<'a> {
    pub step: &'a Step,
    /// The head index cambium wrote for this step; `None` on a global.
    pub head_index: Option<&'a Path>,
    /// cambium's affected set, restricted to what it reports on.
    pub cambium_set: &'a BTreeSet<String>,
    /// The packages cambium marked dirty for this step.
    ///
    /// The `rdeps` oracle's deleted half needs it: when a change set deletes a
    /// whole package, every rule in it is tombstoned and the package's name
    /// survives nowhere in the affected set, so this is the only spelling that
    /// can account for its consumers.
    pub dirty_packages: &'a BTreeSet<String>,
    /// Run the `rdeps` oracle on this step.
    pub want_oracle: bool,
    /// Run the peers on this step.
    pub want_peers: bool,
}

/// The oracle and the peers over one step. A real subject implements this
/// with a Bazel and a checkout; the synthetic walk passes `None` and gets a
/// walk with no live checks, proved by the rebuild oracle alone.
pub trait LiveChecks {
    /// The `rdeps` oracle at this step, or `None` when it was not wanted or
    /// could not run.
    fn oracle(&self, ctx: &StepContext<'_>) -> Result<Option<RdepsResult>>;

    /// Each peer over this step, scored against `oracle`. cambium is not a
    /// peer here: the driver scores it itself from the same oracle.
    fn peers(&self, ctx: &StepContext<'_>, oracle: Option<&RdepsResult>) -> Vec<ToolStep>;

    /// The output base each tool wrote to, for the environment record.
    fn output_bases(&self) -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::new()
    }
}

/// A walk that runs no live checks: cambium and the rebuild oracle only.
pub struct NoLiveChecks;

impl LiveChecks for NoLiveChecks {
    fn oracle(&self, _: &StepContext<'_>) -> Result<Option<RdepsResult>> {
        Ok(None)
    }
    fn peers(&self, _: &StepContext<'_>, _: Option<&RdepsResult>) -> Vec<ToolStep> {
        Vec::new()
    }
}

/// Everything one walk needs beyond the cambium driver and the live checks.
#[derive(Clone)]
pub struct WalkOptions {
    pub repo_id: String,
    pub pin: String,
    /// Where `walk-<pin>[-<ver>].json` goes: `bench-results/<subject>/walk/`.
    pub results_dir: PathBuf,
    /// Where `state.json` goes: `bench-results/<subject>/`.
    pub state_dir: PathBuf,
    pub bazel_version: String,
    pub schedule: Schedule,
    pub oracle_sample: Sample,
    pub peer_sample: Sample,
    pub environment: crate::env::Environment,
    /// Publish every index so the next step's base is in the store and a
    /// resume can find it.
    pub publish: bool,
    pub resume: bool,
    /// How many differences the rebuild oracle lists.
    pub diff_cap: usize,
    pub progress: Option<std::sync::Arc<crate::progress::Reporter>>,
    /// This is a `--only` walk: a handful of named steps rather than the
    /// first-parent range, each one cold-built at its own parent. Its record
    /// goes beside the full walk's under its own name, so a gate run never
    /// overwrites a published one.
    pub only: bool,
}

impl WalkOptions {
    fn result_path(&self) -> PathBuf {
        WalkResult::path(
            &self.results_dir,
            &self.result_pin(),
            Some(&self.bazel_version),
        )
    }

    /// The name the record is filed under: the pin, or the pin marked as a
    /// `--only` run.
    pub fn result_pin(&self) -> String {
        if self.only {
            format!("{}-only", self.pin)
        } else {
            self.pin.clone()
        }
    }
}

/// The walk. `commits` is the first-parent range oldest-first with the pin
/// last; `steps` is one shorter, each `(commits[i-1], commits[i])`.
pub fn walk(
    engine: &Engine,
    driver: &dyn Driver,
    live: &dyn LiveChecks,
    commits: &[String],
    steps: &[Step],
    opts: &WalkOptions,
) -> Result<WalkResult> {
    anyhow::ensure!(commits.len() >= 2, "a walk needs at least two commits");
    anyhow::ensure!(
        steps.len() + 1 == commits.len(),
        "a {}-commit range has {} steps, not {}",
        commits.len(),
        commits.len() - 1,
        steps.len()
    );
    let from = &commits[0];
    let to = &commits[commits.len() - 1];
    let stem = format!("index-{}", opts.pin);
    let (mut w, start_at) = match Runner::open(engine, driver, live, opts, from, to, &stem)? {
        Opened::Finished(prev) => return Ok(prev),
        Opened::Ready(w, start_at) => (w, start_at),
    };
    w.begin(steps.len(), from, to)?;
    for (i, step) in steps.iter().enumerate().skip(start_at) {
        w.step(i + 1, steps.len(), step)?;
    }
    w.rebuild_oracle(to)?;
    w.finish()
}

/// A walk of named steps only.
///
/// The gate for an engine change is a handful of oracle steps, not the whole
/// first-parent range, and those steps do not neighbour each other: step 27 of
/// envoy and step 154 have five score commits between them. So every step here
/// is cold-built at its own parent and answered forward once, which is the
/// same question the full walk asks of that step without the six hundred that
/// carried the index to it. Nothing is carried between steps.
///
/// `--resume` continues a record this same set of steps wrote: the steps
/// already in it are kept and the walk restarts at the first that is missing.
/// A gate run is hours of cold builds, and re-running the ones that finished
/// because the last one died is what `--restart` is for.
pub fn walk_only(
    engine: &Engine,
    driver: &dyn Driver,
    live: &dyn LiveChecks,
    steps: &[Step],
    opts: &WalkOptions,
) -> Result<WalkResult> {
    anyhow::ensure!(!steps.is_empty(), "a walk needs at least one step");
    let from = &steps[0].parent;
    let to = &steps[steps.len() - 1].head;
    let stem = format!("index-{}-only", opts.pin);
    let (mut w, start_at) = match Runner::open(engine, driver, live, opts, from, to, &stem)? {
        Opened::Finished(prev) => return Ok(prev),
        Opened::Ready(w, start_at) => (w, start_at),
    };
    w.begin(steps.len(), from, to)?;
    // A resume past the last step takes none and only finishes the record.
    for (i, step) in steps.iter().enumerate().skip(start_at) {
        // `start` already cold-built at the first step's parent; a resumed
        // walk starts at a step nothing has built for.
        if i > 0 || start_at > 0 {
            w.driver.checkout(&step.parent)?;
            cold_build(w.driver, &step.parent, &w.index_path, w.opts.publish)
                .with_context(|| format!("cold build at {}", step.parent))?;
        }
        w.step(i + 1, steps.len(), step)?;
    }
    w.finish()
}

enum Resume {
    Done(WalkResult),
    /// A record to continue, and the index of the first step it is missing.
    ///
    /// A `--only` walk cold-builds at every step's own parent, so the index
    /// the last completed step left in the store is not the base the next one
    /// wants; `resume` does not look for it, and an answer either way is one
    /// such a walk can continue from.
    From(WalkResult, usize),
    Fresh,
}

/// A walk ready to take its steps from the one named, or the record of one
/// that had already finished.
///
/// There is one of these per walk and it is moved once, so the size gap
/// between the variants costs nothing worth a box.
#[allow(clippy::large_enum_variant)]
enum Opened<'a> {
    Finished(WalkResult),
    Ready(Runner<'a>, usize),
}

fn resume(
    engine: &Engine,
    from: &str,
    to: &str,
    opts: &WalkOptions,
    engine_id: &str,
) -> Result<Resume> {
    if !opts.resume {
        return Ok(Resume::Fresh);
    }
    let path = opts.result_path();
    if !path.exists() {
        return Ok(Resume::Fresh);
    }
    let prev = WalkResult::read(&path)
        .map_err(|e| anyhow::anyhow!("cannot resume {}: {e:#}; pass --restart", path.display()))?;
    anyhow::ensure!(
        prev.from == from && prev.to == to,
        "cannot resume {}: it walked {}..{} and this walk is {from}..{to}; pass --restart",
        path.display(),
        prev.from,
        prev.to
    );
    anyhow::ensure!(
        prev.environment.engine_id == engine_id,
        "cannot resume {}: {} recorded it and this build is {engine_id}; pass --restart",
        path.display(),
        prev.environment.engine_id
    );
    if prev.done {
        return Ok(Resume::Done(prev));
    }
    let last = prev
        .steps
        .last()
        .map(|s| s.head.clone())
        .unwrap_or_else(|| from.to_string());
    let done = prev.steps.len();
    // A `--only` walk needs no stored index to go on with: it cold-builds at
    // the next step's own parent whatever the store holds.
    if opts.only {
        tracing::info!(%last, done, "resuming a walk of named steps");
        return Ok(Resume::From(prev, done));
    }
    match engine.fetch_baseline(&last)? {
        Some(_) => {
            tracing::info!(%last, done, "resuming walk");
            Ok(Resume::From(prev, done))
        }
        None => anyhow::bail!(
            "cannot resume {}: the store no longer holds the index for {last}; pass --restart",
            path.display()
        ),
    }
}

struct Runner<'a> {
    engine: &'a Engine,
    driver: &'a dyn Driver,
    live: &'a dyn LiveChecks,
    opts: &'a WalkOptions,
    path: PathBuf,
    index_path: PathBuf,
    result: WalkResult,
    state: State,
}

impl<'a> Runner<'a> {
    /// The setup a full walk and a `--only` walk share: the state file, the
    /// index path (`<index_stem>[-<ver>].bin` beside it), and either the
    /// record `--resume` continues or a cold build at `from` that starts a
    /// fresh one.
    fn open(
        engine: &'a Engine,
        driver: &'a dyn Driver,
        live: &'a dyn LiveChecks,
        opts: &'a WalkOptions,
        from: &str,
        to: &str,
        index_stem: &str,
    ) -> Result<Opened<'a>> {
        let engine_id = State::engine_id(&engine.config().hash());
        let state = State::load_for(&opts.state_dir, &opts.repo_id, &opts.pin, &engine_id)?;
        std::fs::create_dir_all(&opts.state_dir)?;
        let index_path = opts.state_dir.join(crate::util::versioned_name(
            index_stem,
            Some(&opts.bazel_version),
            "bin",
        ));
        let (result, start_at) = match resume(engine, from, to, opts, &engine_id)? {
            Resume::Done(prev) => return Ok(Opened::Finished(prev)),
            Resume::From(prev, at) => (prev, at),
            Resume::Fresh => start(driver, from, to, &index_path, opts, &engine_id)?,
        };
        let runner = Runner {
            engine,
            driver,
            live,
            opts,
            path: opts.result_path(),
            index_path,
            result,
            state,
        };
        Ok(Opened::Ready(runner, start_at))
    }

    fn progress(&self) -> Option<&crate::progress::Reporter> {
        self.opts.progress.as_deref()
    }

    fn begin(&mut self, steps: usize, from: &str, to: &str) -> Result<()> {
        if let Some(p) = self.progress() {
            p.step(crate::progress::Step::Walk);
            p.total(steps);
            if let Some(last) = self.result.steps.last() {
                p.item(self.result.steps.len(), &last.head);
            }
        }
        self.state.stamp_start("walk");
        self.state.history = Some(HistoryState {
            from: from.to_string(),
            to: to.to_string(),
            bazel_version: self.opts.bazel_version.clone(),
            last_completed: self.result.steps.last().map(|s| s.head.clone()),
            done: false,
        });
        self.state.save(&self.opts.state_dir)
    }

    /// One step forward: cambium's answer, the live checks the schedule wants,
    /// and the record written before the next.
    fn step(&mut self, i: usize, of: usize, step: &Step) -> Result<()> {
        if let Some(p) = self.progress() {
            p.item(i, &step.head);
            p.tool(Some(crate::tools::cambi::NAME));
        }
        self.driver.checkout(&step.head)?;
        let run = match self.driver.update(
            &step.parent,
            &step.head,
            &self.index_path,
            self.opts.publish,
        ) {
            Ok(s) => s,
            Err(e) => {
                if let Some(p) = self.progress() {
                    p.failed(&format!("{e:#}"));
                }
                return Err(e);
            }
        };
        let mut wall_ms = run.wall.as_millis() as u64;
        let r = &run.report;
        // The step carried the previous step's index unless the range
        // invalidated everything and a fresh lineage had to be cold-built.
        let mut carried_index = true;
        if !run.wrote_index && !r.global {
            let cold = cold_build(self.driver, &step.head, &self.index_path, self.opts.publish)?;
            wall_ms += cold.wall_ms;
            carried_index = false;
        }

        // The set beside the JSON, under the shared layout.
        if !r.global {
            let labels_dir =
                crate::labels::change_set_dir(&self.opts.results_dir, &step.parent, &step.head);
            std::fs::create_dir_all(&labels_dir)?;
            crate::labels::write_set(&labels_dir, crate::tools::cambi::NAME, &run.labels)?;
        }

        let (nodes, edges, lineage) = if run.wrote_index || !r.global {
            let f = IndexFile::open(&self.index_path)?;
            let v = f.view();
            (
                v.counts().nodes,
                v.counts().edges,
                cambium_engine::inspect::hex(&v.lineage),
            )
        } else {
            (0, 0, String::new())
        };

        // The live checks, scheduled per step. cambium's oracle scoring is
        // done here from the same oracle the peers were scored with.
        let want_oracle = self.opts.oracle_sample.contains(i - 1);
        let want_peers = self.opts.peer_sample.contains(i - 1);
        let head_index = (run.wrote_index).then_some(self.index_path.as_path());
        let dirty_packages: BTreeSet<String> = r.dirty_packages.iter().cloned().collect();
        let ctx = StepContext {
            step,
            head_index,
            cambium_set: &run.labels,
            dirty_packages: &dirty_packages,
            want_oracle,
            want_peers,
        };
        let oracle = self.live.oracle(&ctx)?;
        if let Some(o) = &oracle {
            let labels_dir =
                crate::labels::change_set_dir(&self.opts.results_dir, &step.parent, &step.head);
            std::fs::create_dir_all(&labels_dir)?;
            crate::labels::write_set(&labels_dir, crate::labels::ORACLE, &o.oracle_set)?;
        }

        let mut tools = vec![score_cambium(&run.labels, wall_ms, oracle.as_ref())];
        tools.extend(self.live.peers(&ctx, oracle.as_ref()));

        let cambium_ok = oracle.as_ref().is_none_or(|o| o.ok());
        let peers_ran = tools.iter().skip(1).all(|t| !t.failed);
        let rebuild_step = self.opts.schedule.rebuilds_at(i);
        let pass = cambium_ok && peers_ran;

        let rec = StepRecord {
            parent: step.parent.clone(),
            head: step.head.clone(),
            tags: step.tags.clone(),
            changed_files: r.stats.changed_files,
            dirty_packages: r.stats.dirty_packages,
            queried_targets: r.stats.queried_targets,
            timings: StepTimings {
                dirty: r.timings_ms.dirty,
                query: r.timings_ms.query,
                splice: r.timings_ms.splice,
                direct: r.timings_ms.direct,
                walk: r.timings_ms.walk,
                engine_total: r.timings_ms.total,
            },
            wall_ms,
            carried_index,
            rebuild_step,
            global: r.global,
            global_reason: r.global_reason.clone(),
            index: IndexCounts {
                nodes,
                edges,
                lineage,
            },
            cambium: CambiumAnswer {
                direct: r.direct.len(),
                affected: r.affected.len(),
                tests: r.tests.len(),
                cold_rebuild: r.stats.cold_rebuild,
            },
            oracle,
            tools,
            pass,
        };
        if let Some(p) = self.progress() {
            p.diagnostics(&run.report.diagnostics);
            p.tool(None);
        }
        // Pushed before the mid-walk oracle so that, when this step itself
        // cold-rebuilt, the rows are classified against its head.
        self.result.steps.push(rec);
        if rebuild_step {
            self.mid_walk_rebuild(step)?;
        }
        self.result.summarise();
        self.result.write(&self.path)?;
        if let Some(rs) = self.state.history.as_mut() {
            rs.last_completed = Some(step.head.clone());
        }
        self.state.stamp("walk", "updated");
        self.state.save(&self.opts.state_dir)?;
        tracing::info!(step = i, of, head = %step.head, ms = wall_ms, "stepped");
        Ok(())
    }

    /// The tree at `rev`, or `None` with a warning: a driver that cannot
    /// list its tree leaves every missing layer row unexplained, and a FAIL
    /// from that cause has to be diagnosable from the log.
    fn tree(&self, rev: &str) -> Option<BTreeSet<String>> {
        match self.driver.paths(rev) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(%rev, error = format!("{e:#}"), "cannot list the tree; every missing loading-layer row will be unexplained");
                None
            }
        }
    }

    /// The commit the loading-layer rows are classified against: the head
    /// of the last cold rebuild inside the window (a fresh lineage resets
    /// the layer), else the window start. Both are in the walk record, so a
    /// resumed walk classifies identically.
    fn classification_base(&self) -> String {
        self.result
            .steps
            .iter()
            .rev()
            .find(|s| !s.carried_index)
            .map(|s| s.head.clone())
            .unwrap_or_else(|| self.result.from.clone())
    }

    /// Every loading-layer row of the diff, classified.
    fn layer_rows(
        &self,
        sky: &SkyDiff,
        at: &str,
        applied: cambium_index::IndexRef<'_>,
        rebuilt: cambium_index::IndexRef<'_>,
    ) -> RebuildRows {
        let from = self.classification_base();
        let from_tree = self.tree(&from);
        let to_tree = self.tree(at);
        let applied_paths = rebuild_rows::applied_paths(applied.sky);
        let errors = rebuild_rows::ErrorPackages::of(applied, rebuilt);
        let (missing, extra) = rebuild_rows::classify(
            sky,
            from_tree.as_ref(),
            to_tree.as_ref(),
            &applied_paths,
            &errors,
        );
        RebuildRows::new(
            &self.opts.repo_id,
            &self.opts.pin,
            &from,
            at,
            &self.opts.bazel_version,
            missing,
            extra,
        )
    }

    /// The rebuild oracle's reference: a cold build at `rev` on a server
    /// with an empty Skyframe graph, written to `out` and never published.
    ///
    /// # Why the server is restarted first
    ///
    /// `cambi index build` in the walk's checkout makes no worktree (the
    /// checkout is already at `rev`), so it runs on the checkout's output
    /// base and on the one server that answered every step of the walk, and
    /// every `rdeps(//..., ...)` query the oracle asked. A `bazel dump
    /// --skyframe` prints every done node that server holds, not what the
    /// build at `rev` evaluated. So the reference carried the walk's history:
    /// on envoy at `7ba51266` it had 33017 nodes against the walked 26062,
    /// and its 306 unexplained rows and 13449 "boundary probes" were rows
    /// the walk had left there (patches deleted mid-window, `WORKSPACE`
    /// probes under directories absent at the pin, packages deleted inside
    /// the window, a few files live at the pin that only a dependency
    /// traversal reaches, and root `WORKSPACE` probes from some command
    /// other than the `//...:*` load). On xla at `076fe93f` its 314 are
    /// expected to be the same kind (patch and `build_file` inputs that
    /// fetches on the walk's server wrote, and packages only a traversal
    /// reaches), though that was read from the rows and not reproduced. The
    /// applied lineage's own base is built by the engine on a fresh server in
    /// a detached worktree with its own output base
    /// (`crates/engine/src/revision.rs`).
    ///
    /// Two ways to get a fresh graph. Building through a detached worktree
    /// on an output base of its own, as the lineage base is built, gives the
    /// same provenance but fetches every external repository (gigabytes for
    /// envoy) into a new output base for each revision it builds, since that
    /// base is kept per revision. Stopping the checkout's server gives the
    /// same empty graph for the price of one server start: Bazel keeps the
    /// Skyframe graph in memory, while the fetched repositories stay on disk
    /// and are reused through their marker files. This does the second. It rests on nothing persisting the graph across a restart:
    /// Skyframe serialization (a remote analysis cache) would, and neither
    /// the bench's rc file and startup flags nor the rc files of envoy, xla
    /// and bazel at their pins configure one. A subject that did would need
    /// the worktree route.
    ///
    /// The cost falls on whatever runs next on that server. At the pin
    /// nothing does. Mid-walk (`--rebuild-every`) the rebuild itself pays a
    /// full cold load instead of an incremental one over the warm graph, and
    /// the steps after it run on a server whose history starts at that
    /// rebuild, not at the walk's first cold build; the step record says so
    /// through `rebuild_step`. A `--rebuild-every` walk's steps therefore do
    /// not run on the same server history as a walk without it.
    fn reference_build(&self, rev: &str, out: &Path) -> Result<()> {
        self.driver
            .restart_server()
            .with_context(|| format!("restarting the bazel server before the rebuild at {rev}"))?;
        let _ = cold_build(self.driver, rev, out, false)?;
        Ok(())
    }

    /// The comparison both rebuild oracles make: a reference build at `rev`
    /// into `fresh` ([`Self::reference_build`]), diffed node by node against
    /// the walk's carried index, with the loading-layer rows classified.
    /// `fresh` is left for the caller, which archives or deletes it.
    fn rebuild_at(&self, rev: &str, fresh: &Path) -> Result<(GraphDiff, SkyDiff, RebuildRows)> {
        self.reference_build(rev, fresh)?;
        let applied = IndexFile::open(&self.index_path)?;
        let rebuilt = IndexFile::open(fresh)?;
        let (diff, sky) =
            compare_by_label_with_sky(applied.view(), rebuilt.view(), self.opts.diff_cap);
        let rows = self.layer_rows(&sky, rev, applied.view(), rebuilt.view());
        Ok((diff, sky, rows))
    }

    /// A mid-walk rebuild oracle: cold-build at the step's head on a fresh
    /// server and compare, without publishing (that would overwrite the
    /// walk's own carried index). The rows go beside the step's label sets.
    fn mid_walk_rebuild(&mut self, step: &Step) -> Result<()> {
        let head = step.head.as_str();
        let fresh = self.index_path.with_extension("rebuild.bin");
        let (diff, sky, rows) = self.rebuild_at(head, &fresh)?;
        let dir = crate::labels::change_set_dir(&self.opts.results_dir, &step.parent, head);
        rows.write(&dir.join(rebuild_rows::MID_WALK_FILE))?;
        let unexplained = rows.counts.missing_unexplained;
        if sky.present.is_some() || !diff.holds_with_sky_missing(unexplained) {
            tracing::error!(%head, summary = %diff.summary(), unexplained, "mid-walk rebuild oracle diverged");
        }
        let _ = std::fs::remove_file(&fresh);
        Ok(())
    }

    /// The rebuild oracle at the pin: the applied index must equal a cold
    /// build on a fresh server ([`Self::reference_build`]), and that index is
    /// archived for `replay --head-check`.
    ///
    /// The loading-layer half is judged on its unexplained rows
    /// ([`rebuild_rows`]): a row a refresh could never have written is
    /// listed in the sidecar and does not fail the walk.
    fn rebuild_oracle(&mut self, to: &str) -> Result<()> {
        if let Some(p) = self.progress() {
            p.item(self.result.steps.len(), "rebuild oracle");
        }
        let fresh_path = self.index_path.with_extension("rebuild.bin");
        let (diff, sky, rows) = self.rebuild_at(to, &fresh_path)?;
        let rows_path = RebuildRows::path(
            &self.opts.results_dir,
            &self.opts.pin,
            Some(&self.opts.bazel_version),
        );
        rows.write(&rows_path)?;
        let unexplained = rows.counts.missing_unexplained;
        let ok = sky.present.is_none() && diff.holds_with_sky_missing(unexplained);
        self.result.head_index =
            super::cambi::archive_head_index(self.engine, &self.opts.state_dir, to, &fresh_path);
        self.result.rebuild_oracle = Some(RebuildOracle {
            ok,
            summary: if ok {
                String::new()
            } else {
                format!(
                    "{}; loading layer unexplained: {unexplained}",
                    diff.summary()
                )
            },
            rows: rows_path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned()),
            sky: rows.counts,
            rule_hash_mismatch: diff.rule_hash_mismatch.iter().take(64).cloned().collect(),
        });
        let _ = std::fs::remove_file(&fresh_path);
        Ok(())
    }

    fn finish(mut self) -> Result<WalkResult> {
        self.result.environment.output_bases = self.live.output_bases();
        self.result.done = true;
        self.result.summarise();
        self.result.write(&self.path)?;
        if let Some(rs) = self.state.history.as_mut() {
            rs.done = true;
        }
        self.state.stamp("walk", "done");
        self.state.save(&self.opts.state_dir)?;
        if let Some(p) = self.progress() {
            p.done();
        }
        Ok(self.result)
    }
}

/// cambium as a tool row, scored against the oracle like any other.
fn score_cambium(
    labels: &BTreeSet<String>,
    wall_ms: u64,
    oracle: Option<&RdepsResult>,
) -> ToolStep {
    let (recall, tightness) = score(labels, oracle);
    ToolStep {
        name: crate::tools::cambi::NAME.to_string(),
        version: String::new(),
        labels: labels.len(),
        wall_ms,
        recall_vs_oracle: recall,
        tightness_vs_oracle: tightness,
        failed: false,
    }
}

/// Recall and tightness of an answer against the oracle's main-repo set.
///
/// Recall is `|answer ∩ oracle| / |oracle|`; tightness is `|oracle| /
/// |answer|`. Both `None` when the oracle did not run; tightness `None` on an
/// empty answer, which has no ratio.
///
/// Both ratios are over the main repository on both sides. The oracle's own
/// set is already partitioned that way (`RdepsResult::missing_external` holds
/// the rest), and the answer is partitioned here with the same
/// [`crate::oracles::rdeps::is_external`] test, so an `@R//` rule in the
/// answer is neither credited as a match nor charged as an extra. Without the
/// partition a tool that answers over more than `//...` reads as loose in
/// exact proportion to how much of the other vocabulary it carries, which
/// measures the vocabulary difference and not the answer.
pub fn score(
    labels: &BTreeSet<String>,
    oracle: Option<&RdepsResult>,
) -> (Option<f64>, Option<f64>) {
    let Some(o) = oracle else {
        return (None, None);
    };
    if o.oracle_labels == 0 {
        return (Some(1.0), None);
    }
    let main = labels
        .iter()
        .filter(|l| !crate::oracles::rdeps::is_external(l))
        .count();
    let matched = o.oracle_set.intersection(labels).count();
    let recall = matched as f64 / o.oracle_labels as f64;
    let tightness = if main == 0 {
        None
    } else {
        Some(o.oracle_labels as f64 / main as f64)
    };
    (Some(recall), tightness)
}

fn start(
    driver: &dyn Driver,
    from: &str,
    to: &str,
    index_path: &Path,
    opts: &WalkOptions,
    engine_id: &str,
) -> Result<(WalkResult, usize)> {
    let t = Instant::now();
    let cold = cold_build(driver, from, index_path, opts.publish)
        .with_context(|| format!("cold build at {from}"))?;
    let index_build_ms = if cold.wall_ms > 0 {
        cold.wall_ms
    } else {
        t.elapsed().as_millis() as u64
    };
    let mut environment = opts.environment.clone();
    environment.engine_id = engine_id.to_string();
    let result = WalkResult {
        schema: crate::RESULT_SCHEMA,
        repo: opts.repo_id.clone(),
        pin: opts.pin.clone(),
        from: from.to_string(),
        to: to.to_string(),
        tier: opts.schedule.tier,
        schedule: opts.schedule,
        oracle_sample: opts.oracle_sample.clone(),
        peer_sample: opts.peer_sample.clone(),
        environment,
        started_unix: now_unix(),
        index_build_ms,
        steps: Vec::new(),
        rebuild_oracle: None,
        head_index: None,
        summary: WalkSummary::default(),
        done: false,
    };
    result.write(&opts.result_path())?;
    Ok((result, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn oracle(labels: &[&str]) -> RdepsResult {
        let mut r = RdepsResult::default();
        r.oracle_set = set(labels);
        r.oracle_labels = r.oracle_set.len();
        r
    }

    #[test]
    fn tightness_is_over_the_main_repository_on_both_sides() {
        let o = oracle(&["//a:a", "//b:b"]);

        let (recall, tightness) = score(&set(&["//a:a", "//b:b"]), Some(&o));
        assert_eq!(recall, Some(1.0));
        assert_eq!(tightness, Some(1.0), "an exact main-repo answer is tight");

        // The same answer plus the repository rows an indexed external brings.
        // They are not in the oracle's main-repo set and must not be charged
        // as extras, or a tool reads as looser the more vocabulary it carries.
        let (recall, tightness) = score(
            &set(&[
                "//a:a",
                "//b:b",
                "@quiche//:x",
                "@quiche//:y",
                "//external:z",
            ]),
            Some(&o),
        );
        assert_eq!(recall, Some(1.0));
        assert_eq!(
            tightness,
            Some(1.0),
            "external rows are neither matched nor charged"
        );

        // A genuine main-repo extra still costs tightness.
        let (_, tightness) = score(&set(&["//a:a", "//b:b", "//c:c"]), Some(&o));
        assert_eq!(tightness, Some(2.0 / 3.0));
    }

    #[test]
    fn an_answer_with_nothing_of_the_main_repository_in_it_has_no_ratio() {
        let o = oracle(&["//a:a"]);
        assert_eq!(score(&set(&[]), Some(&o)), (Some(0.0), None));
        assert_eq!(
            score(&set(&["@quiche//:x"]), Some(&o)),
            (Some(0.0), None),
            "no main-repo labels is the same empty ratio as no labels"
        );
        assert_eq!(score(&set(&["//a:a"]), None), (None, None));
    }
}
